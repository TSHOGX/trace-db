use crate::{Agent, EventKind};
use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const PER_SESSION_HIT_CAP: usize = 50;
const MAX_CANDIDATE_HITS: usize = 5_000;
const MAX_CONTEXT_SESSIONS: usize = 2_000;
type LineageEdges = HashMap<String, (Option<String>, Option<String>)>;

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
    snippet: String,
}

struct SessionCandidate {
    id: String,
    agent: Agent,
    cwd: Option<String>,
    title: Option<String>,
    started_at_ms: Option<i64>,
    ended_at_ms: Option<i64>,
    best_match: SearchMatch,
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
    let candidates = load_candidates(connection, request, &planned_query, candidate_limit)?;
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

    let edges = load_lineage_edges(connection)?;
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
    attach_context(connection, &mut collapsed, &edges)?;
    Ok(collapsed)
}

fn load_candidates(
    connection: &Connection,
    request: &SearchRequest,
    planned_query: &str,
    candidate_limit: usize,
) -> Result<Vec<Candidate>> {
    let mut filters = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(planned_query.to_owned())];
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
    let sql = format!(
        "WITH hits AS (
           SELECT e.session_id,s.agent,s.cwd,s.title,s.started_at_ms,s.ended_at_ms,
                  e.idx,e.kind,bm25(events_fts) AS score,
                  snippet(events_fts,0,'«','»','…',24) AS snippet
           FROM events_fts
           JOIN events e ON e.id=events_fts.rowid
           JOIN sessions s ON s.id=e.session_id
           WHERE events_fts MATCH ?1{extra_filters}
         ), ranked AS (
           SELECT *,row_number() OVER (PARTITION BY session_id ORDER BY score ASC) AS session_rank
           FROM hits
         )
         SELECT session_id,agent,cwd,title,started_at_ms,ended_at_ms,idx,kind,score,snippet
         FROM ranked WHERE session_rank<=?{per_session_parameter}
         ORDER BY score ASC LIMIT ?{total_parameter}"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(
        rusqlite::params_from_iter(values.iter().map(|value| value.as_ref())),
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, f64>(8)?,
                row.get::<_, String>(9)?,
            ))
        },
    )?;
    rows.map(|row| {
        let (session_id, agent, cwd, title, started, ended, idx, kind, bm25, snippet) = row?;
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
            snippet,
        })
    })
    .collect()
}

fn group_candidates(
    candidates: Vec<Candidate>,
    terms: &[String],
) -> HashMap<String, SessionCandidate> {
    let mut sessions = HashMap::new();
    for candidate in candidates {
        let covered = covered_terms(&candidate.snippet, candidate.title.as_deref(), terms);
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
                    snippet: candidate.snippet,
                },
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
    }
}

fn load_lineage_edges(connection: &Connection) -> Result<LineageEdges> {
    let mut statement =
        connection.prepare("SELECT id,parent_session_id,forked_from FROM sessions")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut edges = HashMap::new();
    for row in rows {
        let (id, parent, forked) = row?;
        edges.insert(
            id,
            (
                parent,
                forked.map(|value| value.split('#').next().unwrap_or(&value).to_owned()),
            ),
        );
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
        let Some((parent, forked)) = edges.get(&current) else {
            break;
        };
        let Some(next) = parent.as_ref().or(forked.as_ref()) else {
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
    let sql = format!(
        "SELECT s.id,
          (SELECT text FROM events WHERE session_id=s.id AND kind='user' ORDER BY idx LIMIT 1),
          (SELECT text FROM events WHERE session_id=s.id AND kind='assistant' ORDER BY idx DESC LIMIT 1)
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
        context.insert(
            id,
            (
                ask.map(|text| preview(&text, 500)),
                outcome.map(|text| preview(&text, 500)),
            ),
        );
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

fn covered_terms(text: &str, title: Option<&str>, terms: &[String]) -> HashSet<usize> {
    let haystack = format!(
        "{} {}",
        text.to_lowercase(),
        title.unwrap_or("").to_lowercase()
    );
    terms
        .iter()
        .enumerate()
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

fn preview(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_owned()
    } else {
        text.chars().take(limit).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Event, IngestMode, ParsedSession, Session};
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
    fn previews_preserve_credentials_and_whitespace() {
        assert_eq!(
            preview("Authorization:\nBearer sk-example token=abc", 500),
            "Authorization:\nBearer sk-example token=abc"
        );
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
                    title: None,
                    model: None,
                    provider: None,
                    git_branch: None,
                    parent_session_id: parent.map(str::to_owned),
                    forked_from: None,
                    meta: json!({}),
                    fingerprint: id.to_owned(),
                    sources: Vec::new(),
                },
                events,
            },
            IngestMode::Partial,
        )
        .unwrap();
    }
}
