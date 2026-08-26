use crate::{Agent, EventKind};
use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const PER_SESSION_HIT_CAP: usize = 50;
const MAX_CANDIDATE_HITS: usize = 5_000;
const MAX_CONTEXT_SESSIONS: usize = 2_000;
/// Highlight delimiters and token budget for `snippet()`. Phase 2 must pass
/// these unchanged so deferring snippet generation cannot alter output text.
const SNIPPET_OPEN: &str = "«";
const SNIPPET_CLOSE: &str = "»";
const SNIPPET_ELLIPSIS: &str = "…";
const SNIPPET_TOKENS: i64 = 24;
/// Name of the scalar registered on every connection by
/// [`register_term_coverage`]. Coverage is computed inside the candidate query
/// so full event text never crosses the SQLite boundary.
const TERM_COVERAGE_FUNCTION: &str = "tracedb_term_coverage";
/// Terms beyond this count cannot be represented in the coverage bitmask.
/// Ranking degrades gracefully: extra terms simply do not contribute coverage.
const MAX_COVERAGE_TERMS: usize = 63;
type LineageEdges = HashMap<String, Option<String>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    pub query: String,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
    pub agent: Option<Agent>,
    pub cwd: Option<String>,
    pub since_ms: Option<i64>,
}

impl SearchRequest {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            limit: default_search_limit(),
            agent: None,
            cwd: None,
            since_ms: None,
        }
    }
}

fn default_search_limit() -> usize {
    20
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub id: String,
    pub lineage_root_id: String,
    pub agent: Agent,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub score: f64,
    pub score_breakdown: ScoreBreakdown,
    pub hits: i64,
    pub best_match: SearchMatch,
    pub ask: Option<String>,
    pub outcome: Option<String>,
    pub related_session_ids: Vec<String>,
    /// `events_fts.rowid` backing `best_match`, threaded from phase 1 so phase 2
    /// can fetch this result's snippet. Internal: never crosses the wire, and
    /// defaults on deserialize so the public shape is unchanged.
    #[serde(skip)]
    best_fts_rowid: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMatch {
    pub event_idx: i64,
    pub kind: EventKind,
    pub bm25: f64,
    pub snippet: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoreBreakdown {
    pub best_match: f64,
    pub hit_coverage: f64,
    pub term_coverage: f64,
    pub kind: f64,
    pub recency: f64,
    pub title: f64,
    pub lineage: f64,
}

struct Candidate {
    session_id: String,
    agent: Agent,
    cwd: Option<String>,
    title: Option<String>,
    started_at_ms: Option<i64>,
    ended_at_ms: Option<i64>,
    event_idx: i64,
    kind: EventKind,
    bm25: f64,
    /// `events_fts.rowid` of this hit, kept so phase 2 can address exactly the
    /// surviving best matches. Never serialized.
    fts_rowid: i64,
    /// Bitmask of query terms present in this event's full text, computed in
    /// SQL rather than inferred from a snippet excerpt.
    covered_mask: u64,
}

struct SessionCandidate {
    id: String,
    agent: Agent,
    cwd: Option<String>,
    title: Option<String>,
    started_at_ms: Option<i64>,
    ended_at_ms: Option<i64>,
    best_match: SearchMatch,
    best_fts_rowid: i64,
    hits: i64,
    covered_terms: HashSet<usize>,
}

pub fn search(connection: &Connection, request: &SearchRequest) -> Result<Vec<SearchResult>> {
    if request.limit == 0 || request.query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let planned_query = plan_fts_query(&request.query);
    let query_terms = plain_terms(&request.query);
    let candidate_limit = request
        .limit
        .saturating_mul(PER_SESSION_HIT_CAP)
        .clamp(500, MAX_CANDIDATE_HITS);
    let candidates = load_candidates(
        connection,
        request,
        &planned_query,
        &query_terms,
        candidate_limit,
    )?;
    let mut sessions = group_candidates(candidates, &query_terms);
    if sessions.is_empty() {
        return Ok(Vec::new());
    }

    let min_relevance = sessions
        .values()
        .map(|session| -session.best_match.bm25)
        .fold(f64::INFINITY, f64::min);
    let max_relevance = sessions
        .values()
        .map(|session| -session.best_match.bm25)
        .fold(f64::NEG_INFINITY, f64::max);
    let max_hit_coverage = sessions
        .values()
        .map(|session| (session.hits as f64).ln_1p())
        .fold(0.0, f64::max)
        .max(1.0);
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut scored = sessions
        .drain()
        .map(|(_, session)| {
            score_session(
                session,
                &query_terms,
                min_relevance,
                max_relevance,
                max_hit_coverage,
                now_ms,
            )
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| right.ended_at_ms.cmp(&left.ended_at_ms))
            .then_with(|| left.id.cmp(&right.id))
    });

    let lineage_roots = scored
        .iter()
        .map(|result| result.id.clone())
        .collect::<Vec<_>>();
    let edges = load_lineage_edges(connection, &lineage_roots)?;
    let mut collapsed = Vec::<SearchResult>::new();
    let mut roots = HashMap::<String, usize>::new();
    for mut result in scored {
        let root = lineage_root(&result.id, &edges);
        result.lineage_root_id = root.clone();
        if let Some(index) = roots.get(&root).copied() {
            let representative = &mut collapsed[index];
            representative.hits += result.hits;
            representative.score_breakdown.lineage += result.score * 0.1;
            representative.score += result.score * 0.1;
            representative.related_session_ids.push(result.id);
            representative
                .related_session_ids
                .extend(result.related_session_ids);
        } else {
            roots.insert(root, collapsed.len());
            collapsed.push(result);
        }
    }
    collapsed.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| right.ended_at_ms.cmp(&left.ended_at_ms))
            .then_with(|| left.id.cmp(&right.id))
    });
    collapsed.truncate(request.limit);
    // Phase 2: only the surviving results need snippet text. Building snippets
    // for every bounded candidate dominated search latency.
    attach_snippets(connection, &mut collapsed, &planned_query)?;
    attach_context(connection, &mut collapsed, &edges)?;
    Ok(collapsed)
}

/// Register the term-coverage scalar used by phase 1.
///
/// Coverage must agree with `title_covered_terms` exactly, including Unicode case
/// folding. SQLite's built-in `lower()` folds ASCII only, so computing coverage
/// with `instr(lower(text),term)` would silently score accented and other
/// non-ASCII terms as uncovered. Evaluating it in Rust keeps one definition of
/// "this term is present" and avoids transferring full event text to score it.
pub fn register_term_coverage(connection: &Connection) -> Result<()> {
    use rusqlite::functions::FunctionFlags;
    connection.create_scalar_function(
        TERM_COVERAGE_FUNCTION,
        2,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |context| {
            let text = context.get_raw(0).as_str_or_null()?.unwrap_or_default();
            let terms = context.get_raw(1).as_str_or_null()?.unwrap_or_default();
            if terms.is_empty() {
                return Ok(0i64);
            }
            let haystack = text.to_lowercase();
            let mut mask = 0u64;
            for (index, term) in terms.split('\u{1f}').enumerate().take(MAX_COVERAGE_TERMS) {
                if !term.is_empty() && haystack.contains(term) {
                    mask |= 1u64 << index;
                }
            }
            Ok(mask as i64)
        },
    )?;
    Ok(())
}

/// Terms are passed to SQL as one unit-separated string so the scalar keeps a
/// fixed arity regardless of query length.
fn encode_terms(terms: &[String]) -> String {
    terms
        .iter()
        .take(MAX_COVERAGE_TERMS)
        .cloned()
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

fn mask_to_terms(mask: u64) -> HashSet<usize> {
    (0..MAX_COVERAGE_TERMS)
        .filter(|index| mask & (1u64 << index) != 0)
        .collect()
}

fn load_candidates(
    connection: &Connection,
    request: &SearchRequest,
    planned_query: &str,
    query_terms: &[String],
    candidate_limit: usize,
) -> Result<Vec<Candidate>> {
    let mut filters = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(planned_query.to_owned())];
    values.push(Box::new(encode_terms(query_terms)));
    let terms_parameter = values.len();
    if let Some(agent) = request.agent {
        values.push(Box::new(agent.as_str().to_owned()));
        filters.push(format!("s.agent=?{}", values.len()));
    }
    if let Some(cwd) = &request.cwd {
        values.push(Box::new(format!("%{cwd}%")));
        filters.push(format!("s.cwd LIKE ?{}", values.len()));
    }
    if let Some(since_ms) = request.since_ms {
        values.push(Box::new(since_ms));
        filters.push(format!(
            "COALESCE(s.ended_at_ms,s.started_at_ms)>=?{}",
            values.len()
        ));
    }
    values.push(Box::new(PER_SESSION_HIT_CAP as i64));
    let per_session_parameter = values.len();
    values.push(Box::new(candidate_limit as i64));
    let total_parameter = values.len();
    let extra_filters = if filters.is_empty() {
        String::new()
    } else {
        format!(" AND {}", filters.join(" AND "))
    };
    // Phase 1 selects only what ranking consumes. `snippet()` is deliberately
    // absent: it was previously evaluated for every candidate row before the
    // window function and LIMIT, and dominated the query's cost. Term coverage
    // is computed here against full event text, which is already on the row.
    let sql = format!(
        "WITH hits AS (
           SELECT events_fts.rowid AS fts_rowid,
                  e.session_id,s.agent,s.cwd,s.title,s.started_at_ms,s.ended_at_ms,
                  e.idx,e.kind,bm25(events_fts) AS score,
                  {TERM_COVERAGE_FUNCTION}(e.text,?{terms_parameter}) AS covered_mask
           FROM events_fts
           JOIN events e ON e.id=events_fts.rowid
           JOIN sessions s ON s.id=e.session_id
           WHERE events_fts MATCH ?1{extra_filters}
         ), ranked AS (
           SELECT *,row_number() OVER (PARTITION BY session_id ORDER BY score ASC) AS session_rank
           FROM hits
         )
         SELECT fts_rowid,session_id,agent,cwd,title,started_at_ms,ended_at_ms,idx,kind,score,covered_mask
         FROM ranked WHERE session_rank<=?{per_session_parameter}
         ORDER BY score ASC LIMIT ?{total_parameter}"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(
        rusqlite::params_from_iter(values.iter().map(|value| value.as_ref())),
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, f64>(9)?,
                row.get::<_, i64>(10)?,
            ))
        },
    )?;
    rows.map(|row| {
        let (
            fts_rowid,
            session_id,
            agent,
            cwd,
            title,
            started,
            ended,
            idx,
            kind,
            bm25,
            covered_mask,
        ) = row?;
        Ok(Candidate {
            session_id,
            agent: agent.parse().map_err(anyhow::Error::msg)?,
            cwd,
            title,
            started_at_ms: started,
            ended_at_ms: ended,
            event_idx: idx,
            kind: kind.parse().map_err(anyhow::Error::msg)?,
            bm25,
            fts_rowid,
            covered_mask: covered_mask as u64,
        })
    })
    .collect()
}

/// Phase 2: fetch snippets for the surviving results only.
///
/// `snippet()` needs the `events_fts` MATCH context, so this re-issues the same
/// planned query constrained to the kept rowids. Delimiters and token budget are
/// the shared constants, so text is identical to single-phase generation.
fn attach_snippets(
    connection: &Connection,
    results: &mut [SearchResult],
    planned_query: &str,
) -> Result<()> {
    if results.is_empty() {
        return Ok(());
    }
    let rowids = results
        .iter()
        .map(|result| result.best_fts_rowid)
        .collect::<Vec<_>>();
    let placeholders = (0..rowids.len())
        .map(|index| format!("?{}", index + 6))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT events_fts.rowid,snippet(events_fts,0,?2,?3,?4,?5)
         FROM events_fts
         WHERE events_fts MATCH ?1 AND events_fts.rowid IN ({placeholders})"
    );
    let mut statement = connection.prepare(&sql)?;
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(planned_query.to_owned()),
        Box::new(SNIPPET_OPEN),
        Box::new(SNIPPET_CLOSE),
        Box::new(SNIPPET_ELLIPSIS),
        Box::new(SNIPPET_TOKENS),
    ];
    for rowid in &rowids {
        values.push(Box::new(*rowid));
    }
    let rows = statement.query_map(
        rusqlite::params_from_iter(values.iter().map(|value| value.as_ref())),
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
    )?;
    let mut snippets = HashMap::new();
    for row in rows {
        let (rowid, snippet) = row?;
        snippets.insert(rowid, snippet);
    }
    for result in results {
        if let Some(snippet) = snippets.get(&result.best_fts_rowid) {
            result.best_match.snippet = snippet.clone();
        }
    }
    Ok(())
}

fn group_candidates(
    candidates: Vec<Candidate>,
    terms: &[String],
) -> HashMap<String, SessionCandidate> {
    let mut sessions = HashMap::new();
    for candidate in candidates {
        // Coverage comes from the event's full text (computed in SQL) unioned
        // with the session title, not from a 24-token snippet excerpt.
        let mut covered = mask_to_terms(candidate.covered_mask);
        covered.extend(title_covered_terms(candidate.title.as_deref(), terms));
        sessions
            .entry(candidate.session_id.clone())
            .and_modify(|session: &mut SessionCandidate| {
                session.hits += 1;
                session.covered_terms.extend(covered.iter().copied());
            })
            .or_insert_with(|| SessionCandidate {
                id: candidate.session_id,
                agent: candidate.agent,
                cwd: candidate.cwd,
                title: candidate.title,
                started_at_ms: candidate.started_at_ms,
                ended_at_ms: candidate.ended_at_ms,
                best_match: SearchMatch {
                    event_idx: candidate.event_idx,
                    kind: candidate.kind,
                    bm25: candidate.bm25,
                    // Filled by phase 2 for surviving results only.
                    snippet: String::new(),
                },
                best_fts_rowid: candidate.fts_rowid,
                hits: 1,
                covered_terms: covered,
            });
    }
    sessions
}

fn score_session(
    session: SessionCandidate,
    terms: &[String],
    min_relevance: f64,
    max_relevance: f64,
    max_hit_coverage: f64,
    now_ms: i64,
) -> SearchResult {
    let relevance = -session.best_match.bm25;
    let best_match = if (max_relevance - min_relevance).abs() < f64::EPSILON {
        1.0
    } else {
        (relevance - min_relevance) / (max_relevance - min_relevance)
    };
    let hit_coverage = (session.hits as f64).ln_1p() / max_hit_coverage;
    let term_coverage = if terms.is_empty() {
        1.0
    } else {
        session.covered_terms.len() as f64 / terms.len() as f64
    };
    let kind = kind_bonus(session.best_match.kind);
    let age_days = session
        .ended_at_ms
        .map(|ended| (now_ms - ended).max(0) as f64 / 86_400_000.0)
        .unwrap_or(3650.0);
    let recency = (-std::f64::consts::LN_2 * age_days / 30.0).exp();
    let title = title_matches(session.title.as_deref(), terms) as u8 as f64;
    let breakdown = ScoreBreakdown {
        best_match,
        hit_coverage,
        term_coverage,
        kind,
        recency,
        title,
        lineage: 0.0,
    };
    let score = best_match
        + 0.25 * hit_coverage
        + 0.35 * term_coverage
        + 0.2 * kind
        + 0.25 * recency
        + 0.15 * title;
    SearchResult {
        id: session.id.clone(),
        lineage_root_id: session.id,
        agent: session.agent,
        cwd: session.cwd,
        title: session.title,
        started_at_ms: session.started_at_ms,
        ended_at_ms: session.ended_at_ms,
        score,
        score_breakdown: breakdown,
        hits: session.hits,
        best_match: session.best_match,
        ask: None,
        outcome: None,
        related_session_ids: Vec::new(),
        best_fts_rowid: session.best_fts_rowid,
    }
}

fn load_lineage_edges(connection: &Connection, roots: &[String]) -> Result<LineageEdges> {
    if roots.is_empty() {
        return Ok(HashMap::new());
    }
    // Search only needs lineage reachable from matched sessions. Loading the
    // entire sessions table made query latency grow with archive size even for
    // highly selective searches.
    let placeholders = (1..=roots.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "WITH RECURSIVE lineage(id) AS (
           SELECT id FROM sessions WHERE id IN ({placeholders})
           UNION
           SELECT s.parent_session_id
           FROM sessions s JOIN lineage l ON s.id=l.id
           WHERE s.parent_session_id IS NOT NULL
         )
         SELECT s.id,s.parent_session_id
         FROM sessions s JOIN lineage l ON l.id=s.id"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(roots.iter()), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    let mut edges = HashMap::new();
    for row in rows {
        let (id, parent) = row?;
        edges.insert(id, parent);
    }
    Ok(edges)
}

fn lineage_root(id: &str, edges: &LineageEdges) -> String {
    lineage_path(id, edges)
        .last()
        .cloned()
        .unwrap_or_else(|| id.to_owned())
}

fn lineage_path(id: &str, edges: &LineageEdges) -> Vec<String> {
    let mut current = id.to_owned();
    let mut path = Vec::new();
    let mut seen = HashSet::new();
    while seen.insert(current.clone()) {
        let Some(Some(next)) = edges.get(&current) else {
            break;
        };
        if !edges.contains_key(next) {
            break;
        }
        current = next.clone();
        path.push(current.clone());
    }
    path
}

fn attach_context(
    connection: &Connection,
    results: &mut [SearchResult],
    edges: &LineageEdges,
) -> Result<()> {
    if results.is_empty() {
        return Ok(());
    }
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    let mut lineage_context_ids = HashMap::<String, Vec<String>>::new();
    for result in results.iter() {
        let context_ids = lineage_context_ids.entry(result.id.clone()).or_default();
        for id in std::iter::once(&result.id).chain(result.related_session_ids.iter()) {
            for ancestor in lineage_path(id, edges) {
                if !context_ids.contains(&ancestor) {
                    context_ids.push(ancestor.clone());
                }
            }
        }
        if seen.insert(result.id.clone()) {
            ids.push(result.id.clone());
        }
        for related in &result.related_session_ids {
            if seen.insert(related.clone()) {
                ids.push(related.clone());
            }
        }
        for ancestor in context_ids.iter() {
            if ids.len() >= MAX_CONTEXT_SESSIONS {
                break;
            }
            if seen.insert(ancestor.clone()) {
                ids.push(ancestor.clone());
            }
        }
    }
    let placeholders = (1..=ids.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(",");
    // Both previews are materialized at ingest, already truncated to the shared
    // budget, so context assembly is one indexed row read per session.
    let sql = format!(
        "SELECT s.id,s.first_user_text,s.last_assistant_text
         FROM sessions s WHERE s.id IN ({placeholders})"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut context = HashMap::new();
    for row in rows {
        let (id, ask, outcome) = row?;
        context.insert(id, (ask, outcome));
    }
    for result in results {
        let own = context.get(&result.id);
        let ancestors = lineage_context_ids
            .get(&result.id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        result.ask = own
            .and_then(|(ask, _)| ask.clone())
            .or_else(|| related_context(&context, &result.related_session_ids, true));
        result.outcome = own
            .and_then(|(_, outcome)| outcome.clone())
            .or_else(|| related_context(&context, &result.related_session_ids, false));
        if result.ask.is_none() {
            result.ask = related_context(&context, ancestors, true);
        }
        if result.outcome.is_none() {
            result.outcome = related_context(&context, ancestors, false);
        }
    }
    Ok(())
}

fn related_context(
    context: &HashMap<String, (Option<String>, Option<String>)>,
    related_ids: &[String],
    ask: bool,
) -> Option<String> {
    related_ids.iter().find_map(|id| {
        context
            .get(id)
            .and_then(|(first, last)| if ask { first.clone() } else { last.clone() })
    })
}

fn plan_fts_query(query: &str) -> String {
    if has_fts_syntax(query) {
        return query.to_owned();
    }
    let terms = query.split_whitespace().collect::<Vec<_>>();
    if terms.len() <= 1 {
        return quote_fts(query.trim());
    }
    std::iter::once(quote_fts(query.trim()))
        .chain(terms.into_iter().map(quote_fts))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn has_fts_syntax(query: &str) -> bool {
    let upper = query.to_ascii_uppercase();
    query.contains(['"', '*', '(', ')', ':'])
        || [" OR ", " AND ", " NOT ", "NEAR("]
            .iter()
            .any(|operator| upper.contains(operator))
}

fn quote_fts(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn plain_terms(query: &str) -> Vec<String> {
    if has_fts_syntax(query) {
        Vec::new()
    } else {
        query
            .split_whitespace()
            .map(|term| term.to_lowercase())
            .filter(|term| !term.is_empty())
            .collect()
    }
}

/// Query terms present in a session title. Kept separate from event-text
/// coverage so the two sources can be unioned per candidate.
fn title_covered_terms(title: Option<&str>, terms: &[String]) -> HashSet<usize> {
    let Some(title) = title else {
        return HashSet::new();
    };
    let haystack = title.to_lowercase();
    terms
        .iter()
        .enumerate()
        .take(MAX_COVERAGE_TERMS)
        .filter_map(|(index, term)| haystack.contains(term).then_some(index))
        .collect()
}

fn title_matches(title: Option<&str>, terms: &[String]) -> bool {
    let Some(title) = title else { return false };
    let title = title.to_lowercase();
    !terms.is_empty() && terms.iter().all(|term| title.contains(term))
}

fn kind_bonus(kind: EventKind) -> f64 {
    match kind {
        EventKind::User => 1.0,
        EventKind::Assistant => 0.8,
        EventKind::System => 0.5,
        EventKind::Thinking => 0.4,
        EventKind::ToolCall => 0.3,
        EventKind::ToolResult | EventKind::Usage => 0.2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Event, ParsedSession, Session};
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn planner_combines_phrase_precision_with_term_recall() {
        assert_eq!(
            plan_fts_query("deploy netlify"),
            "\"deploy netlify\" OR \"deploy\" OR \"netlify\""
        );
        assert_eq!(plan_fts_query("deploy OR release"), "deploy OR release");
    }

    #[test]
    fn ranking_rewards_term_coverage_and_collapses_lineage() {
        let dir = tempdir().unwrap();
        let mut connection = crate::store::open(dir.path().join("trace.db")).unwrap();
        insert_session(
            &mut connection,
            "codex:partial",
            None,
            vec![Event::new(EventKind::User, "deploy service")],
        );
        insert_session(
            &mut connection,
            "codex:parent",
            None,
            vec![
                Event::new(EventKind::User, "deploy service to netlify"),
                Event::new(EventKind::Assistant, "netlify deployment complete"),
            ],
        );
        insert_session(
            &mut connection,
            "codex:child",
            Some("codex:parent"),
            vec![Event::new(EventKind::User, "verify netlify deploy")],
        );

        let results = search(&connection, &SearchRequest::new("deploy netlify")).unwrap();
        assert_eq!(results[0].lineage_root_id, "codex:parent");
        assert_eq!(results[0].hits, 3);
        assert!(results[0].ask.is_some());
        assert_eq!(
            results[0].outcome.as_deref(),
            Some("netlify deployment complete")
        );
        assert_eq!(results[0].related_session_ids.len(), 1);
        assert_eq!(results[0].score_breakdown.term_coverage, 1.0);
        assert!(results[0].score_breakdown.lineage > 0.0);
        assert_eq!(results[1].id, "codex:partial");
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn internal_rowid_never_reaches_the_wire_contract() {
        let dir = tempdir().unwrap();
        let mut connection = crate::store::open(dir.path().join("trace.db")).unwrap();
        insert_session(
            &mut connection,
            "codex:one",
            None,
            vec![Event::new(EventKind::User, "deploy netlify")],
        );
        let results = search(&connection, &SearchRequest::new("deploy netlify")).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].best_fts_rowid > 0, "rowid is tracked internally");

        let json = serde_json::to_value(&results[0]).unwrap();
        let object = json.as_object().unwrap();
        assert!(
            !object
                .keys()
                .any(|key| key.to_lowercase().contains("rowid")),
            "serialized result leaked an internal rowid: {:?}",
            object.keys().collect::<Vec<_>>()
        );
        assert!(object.contains_key("bestMatch"));
        // Round-trips without the skipped field present.
        let restored: SearchResult = serde_json::from_value(json).unwrap();
        assert_eq!(restored.best_fts_rowid, 0);
        assert_eq!(restored.best_match.snippet, results[0].best_match.snippet);
    }

    #[test]
    fn deferred_snippets_match_single_phase_generation() {
        let dir = tempdir().unwrap();
        let mut connection = crate::store::open(dir.path().join("trace.db")).unwrap();
        // Text long enough that the 24-token snippet window genuinely truncates,
        // so an ellipsis/delimiter difference would surface.
        insert_session(
            &mut connection,
            "codex:long",
            None,
            vec![
                Event::new(
                    EventKind::User,
                    "preamble one two three four five six seven eight nine ten \
                     eleven twelve thirteen fourteen fifteen sixteen seventeen \
                     eighteen nineteen twenty deploy netlify trailing words here \
                     that run past the snippet budget entirely",
                ),
                Event::new(EventKind::Assistant, "netlify deploy finished cleanly"),
            ],
        );
        insert_session(
            &mut connection,
            "codex:short",
            None,
            vec![Event::new(EventKind::User, "deploy netlify quickly")],
        );

        let request = SearchRequest::new("deploy netlify");
        let planned = plan_fts_query(&request.query);
        let results = search(&connection, &request).unwrap();
        assert!(!results.is_empty());

        // Recompute each result's snippet the old way: single-phase, inside the
        // candidate query, and assert byte equality.
        for result in &results {
            let expected: String = connection
                .query_row(
                    "SELECT snippet(events_fts,0,'«','»','…',24)
                     FROM events_fts
                     WHERE events_fts MATCH ?1 AND events_fts.rowid=?2",
                    rusqlite::params![&planned, result.best_fts_rowid],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                result.best_match.snippet, expected,
                "deferred snippet diverged for {}",
                result.id
            );
            assert!(!result.best_match.snippet.is_empty());
        }
    }

    #[test]
    fn term_coverage_counts_matches_outside_the_snippet_window() {
        let dir = tempdir().unwrap();
        let mut connection = crate::store::open(dir.path().join("trace.db")).unwrap();
        // "netlify" sits far past the 24-token snippet window that starts at the
        // "deploy" match, so excerpt-derived coverage used to miss it.
        let filler = (0..60)
            .map(|index| format!("filler{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        insert_session(
            &mut connection,
            "codex:distant",
            None,
            vec![Event::new(
                EventKind::User,
                format!("deploy {filler} netlify"),
            )],
        );

        let results = search(&connection, &SearchRequest::new("deploy netlify")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].score_breakdown.term_coverage, 1.0,
            "both query terms are present in the event text"
        );
    }

    #[test]
    fn term_coverage_folds_non_ascii_case_like_rust() {
        let dir = tempdir().unwrap();
        let mut connection = crate::store::open(dir.path().join("trace.db")).unwrap();
        insert_session(
            &mut connection,
            "codex:accented",
            None,
            vec![Event::new(EventKind::User, "ÉCOLE deploy notes")],
        );

        // SQLite's lower() folds ASCII only; the registered scalar must not.
        let results = search(&connection, &SearchRequest::new("école deploy")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].score_breakdown.term_coverage, 1.0);
    }

    #[test]
    fn context_falls_back_to_unmatched_lineage_ancestors() {
        let dir = tempdir().unwrap();
        let mut connection = crate::store::open(dir.path().join("trace.db")).unwrap();
        insert_session(
            &mut connection,
            "codex:ancestor",
            None,
            vec![
                Event::new(EventKind::User, "original request"),
                Event::new(EventKind::Assistant, "ancestor completed the work"),
            ],
        );
        insert_session(
            &mut connection,
            "codex:child",
            Some("codex:ancestor"),
            vec![Event::new(EventKind::User, "needle child detail")],
        );

        let results = search(&connection, &SearchRequest::new("needle")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].lineage_root_id, "codex:ancestor");
        assert_eq!(results[0].ask.as_deref(), Some("needle child detail"));
        assert_eq!(
            results[0].outcome.as_deref(),
            Some("ancestor completed the work")
        );
    }

    #[test]
    fn since_filter_includes_active_sessions_without_end_times() {
        let dir = tempdir().unwrap();
        let mut connection = crate::store::open(dir.path().join("trace.db")).unwrap();
        insert_session(
            &mut connection,
            "codex:active",
            None,
            vec![Event::new(EventKind::User, "active deployment")],
        );
        let started_at = chrono::Utc::now().timestamp_millis() - 1_000;
        connection
            .execute(
                "UPDATE sessions SET started_at_ms=?1,ended_at_ms=NULL WHERE id='codex:active'",
                [started_at],
            )
            .unwrap();

        let results = search(
            &connection,
            &SearchRequest {
                query: "deployment".into(),
                limit: 20,
                agent: None,
                cwd: None,
                since_ms: Some(started_at - 1),
            },
        )
        .unwrap();
        assert_eq!(results[0].id, "codex:active");
    }

    fn insert_session(
        connection: &mut Connection,
        id: &str,
        parent: Option<&str>,
        events: Vec<Event>,
    ) {
        crate::store::upsert(
            connection,
            ParsedSession {
                session: Session {
                    id: id.to_owned(),
                    agent: Agent::Codex,
                    cwd: Some("/workspace".into()),
                    started_at_ms: Some(chrono::Utc::now().timestamp_millis() - 1000),
                    ended_at_ms: Some(chrono::Utc::now().timestamp_millis()),
                    status: None,
                    title: None,
                    model: None,
                    provider: None,
                    git_branch: None,
                    parent_session_id: parent.map(str::to_owned),
                    parent_relation: parent.map(|_| crate::SessionRelation::Subagent),
                    fork_point_native_id: None,
                    meta: json!({}),
                    fingerprint: id.to_owned(),
                    sources: Vec::new(),
                },
                events,
            },
        )
        .unwrap();
    }
}
