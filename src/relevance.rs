//! Deterministic, labeled relevance evaluation for TraceDB search.
//!
//! The suite intentionally builds normalized sessions through the public
//! facade. It evaluates ranking, graded relevance, lineage collapse, and
//! context assembly without depending on host-native agent stores.

use crate::{
    Agent, Event, EventKind, IngestMode, ParsedSession, SearchRequest, SearchResult, Session,
    TraceDb,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, HashSet};

pub const RELEVANCE_SCHEMA_VERSION: &str = "tracedb-relevance-v1";
pub const STANDARD_RELEVANCE_QUERY_COUNT: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelevanceTag {
    Multilingual,
    MultiTerm,
    Title,
    Cwd,
    Tool,
    Error,
    Model,
    Provider,
    OldImportant,
    ParentSubagent,
    Fork,
    DistantContext,
}

impl std::fmt::Display for RelevanceTag {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Multilingual => "multilingual",
            Self::MultiTerm => "multi_term",
            Self::Title => "title",
            Self::Cwd => "cwd",
            Self::Tool => "tool",
            Self::Error => "error",
            Self::Model => "model",
            Self::Provider => "provider",
            Self::OldImportant => "old_important",
            Self::ParentSubagent => "parent_subagent",
            Self::Fork => "fork",
            Self::DistantContext => "distant_context",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelevanceLabel {
    pub session_id: String,
    /// Graded relevance from 0 through 3. Zero labels are ignored in metrics.
    pub grade: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextExpectation {
    pub ask_contains: Option<String>,
    pub outcome_contains: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelevanceCase {
    pub id: String,
    pub query: String,
    pub labels: Vec<RelevanceLabel>,
    pub tags: Vec<RelevanceTag>,
    pub expected_lineage_root: Option<String>,
    pub context: Option<ContextExpectation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelevanceMetrics {
    pub query_count: usize,
    pub recall_at_5: f64,
    pub recall_at_10: f64,
    pub mrr: f64,
    pub ndcg_at_10: f64,
    pub lineage_collapse_accuracy: f64,
    pub context_answerability: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelevanceCaseResult {
    pub id: String,
    pub query: String,
    pub tags: Vec<RelevanceTag>,
    pub returned_ids: Vec<String>,
    pub returned_lineage_roots: Vec<String>,
    pub relevant_results: usize,
    pub reciprocal_rank: f64,
    pub ndcg_at_10: f64,
    pub lineage_collapse_correct: Option<bool>,
    pub context_answerable: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelevanceReport {
    pub schema_version: String,
    pub query_count: usize,
    pub corpus_sessions: usize,
    pub metrics: RelevanceMetrics,
    pub metrics_by_tag: BTreeMap<RelevanceTag, RelevanceMetrics>,
    pub cases: Vec<RelevanceCaseResult>,
}

/// Run the standard deterministic labeled suite through the canonical facade.
pub fn evaluate_relevance() -> Result<RelevanceReport> {
    let mut database = TraceDb::open(":memory:")?;
    let corpus_sessions = build_corpus(&mut database)?;
    let cases = standard_cases();
    if cases.len() != STANDARD_RELEVANCE_QUERY_COUNT {
        anyhow::bail!("standard relevance case count changed without updating the contract");
    }
    let mut results = Vec::with_capacity(cases.len());
    for case in &cases {
        results.push(evaluate_case(&database, case)?);
    }
    let metrics = aggregate_metrics(&cases, &results, None);
    let mut metrics_by_tag = BTreeMap::new();
    let tags = cases
        .iter()
        .flat_map(|case| case.tags.iter().copied())
        .collect::<HashSet<_>>();
    for tag in tags {
        metrics_by_tag.insert(tag, aggregate_metrics(&cases, &results, Some(tag)));
    }
    Ok(RelevanceReport {
        schema_version: RELEVANCE_SCHEMA_VERSION.into(),
        query_count: cases.len(),
        corpus_sessions,
        metrics,
        metrics_by_tag,
        cases: results,
    })
}

pub fn standard_cases() -> Vec<RelevanceCase> {
    vec![
        RelevanceCase {
            id: "deploy-netlify".into(),
            query: "deploy netlify".into(),
            labels: labels(&[("codex:deploy", 3), ("codex:generic-deploy", 1)]),
            tags: vec![RelevanceTag::MultiTerm, RelevanceTag::Title],
            expected_lineage_root: None,
            context: Some(ContextExpectation {
                ask_contains: Some("Netlify".into()),
                outcome_contains: Some("deployed".into()),
            }),
        },
        RelevanceCase {
            id: "sqlite-migration".into(),
            query: "sqlite migration rollback".into(),
            labels: labels(&[("codex:sqlite", 3), ("codex:lineage-child", 2)]),
            tags: vec![RelevanceTag::MultiTerm, RelevanceTag::Tool],
            expected_lineage_root: Some("codex:lineage-parent".into()),
            context: Some(ContextExpectation {
                ask_contains: Some("sqlite".into()),
                outcome_contains: Some("migration".into()),
            }),
        },
        RelevanceCase {
            id: "chinese-deploy".into(),
            query: "部署 失败".into(),
            labels: labels(&[("codex:chinese", 3)]),
            tags: vec![RelevanceTag::Multilingual, RelevanceTag::Error],
            expected_lineage_root: None,
            context: None,
        },
        RelevanceCase {
            id: "mixed-language-timeout".into(),
            query: "réparer timeout".into(),
            labels: labels(&[("codex:mixed-language", 3)]),
            tags: vec![RelevanceTag::Multilingual, RelevanceTag::MultiTerm],
            expected_lineage_root: None,
            context: None,
        },
        RelevanceCase {
            id: "terraform-tool".into(),
            query: "terraform provider".into(),
            labels: labels(&[("codex:infra", 3)]),
            tags: vec![
                RelevanceTag::Tool,
                RelevanceTag::Cwd,
                RelevanceTag::Provider,
            ],
            expected_lineage_root: None,
            context: None,
        },
        RelevanceCase {
            id: "old-auth".into(),
            query: "authentication token rotation".into(),
            labels: labels(&[("codex:old-auth", 3), ("codex:recent-auth", 1)]),
            tags: vec![
                RelevanceTag::OldImportant,
                RelevanceTag::Model,
                RelevanceTag::Provider,
            ],
            expected_lineage_root: None,
            context: Some(ContextExpectation {
                ask_contains: Some("authentication".into()),
                outcome_contains: Some("rotated".into()),
            }),
        },
        RelevanceCase {
            id: "distant-context".into(),
            query: "bisect regression".into(),
            labels: labels(&[("codex:lineage-child", 3)]),
            tags: vec![RelevanceTag::ParentSubagent, RelevanceTag::DistantContext],
            expected_lineage_root: Some("codex:lineage-parent".into()),
            context: Some(ContextExpectation {
                ask_contains: Some("bisect".into()),
                outcome_contains: Some("resolved".into()),
            }),
        },
        RelevanceCase {
            id: "fork-lineage".into(),
            query: "config fork".into(),
            labels: labels(&[("codex:fork-child", 3)]),
            tags: vec![RelevanceTag::Fork, RelevanceTag::MultiTerm],
            expected_lineage_root: Some("codex:fork-root".into()),
            context: None,
        },
        RelevanceCase {
            id: "metadata-signals".into(),
            query: "provider model timeout".into(),
            labels: labels(&[("codex:mixed-language", 3)]),
            tags: vec![
                RelevanceTag::Model,
                RelevanceTag::Provider,
                RelevanceTag::Error,
            ],
            expected_lineage_root: None,
            context: None,
        },
    ]
}

fn labels(values: &[(&str, u8)]) -> Vec<RelevanceLabel> {
    values
        .iter()
        .map(|(session_id, grade)| RelevanceLabel {
            session_id: (*session_id).into(),
            grade: *grade,
        })
        .collect()
}

fn evaluate_case(database: &TraceDb, case: &RelevanceCase) -> Result<RelevanceCaseResult> {
    let results = database.search(SearchRequest {
        query: case.query.clone(),
        limit: 10,
        agent: None,
        cwd: None,
        since_ms: None,
    })?;
    let returned_ids = results
        .iter()
        .map(|result| result.id.clone())
        .collect::<Vec<_>>();
    let returned_lineage_roots = results
        .iter()
        .map(|result| result.lineage_root_id.clone())
        .collect::<Vec<_>>();
    let relevant_results = results
        .iter()
        .filter(|result| relevance_grade(result, case) > 0)
        .count();
    let reciprocal_rank = results
        .iter()
        .position(|result| relevance_grade(result, case) > 0)
        .map(|index| 1.0 / (index + 1) as f64)
        .unwrap_or(0.0);
    let ndcg_at_10 = ndcg(&results, case);
    let lineage_collapse_correct = case.expected_lineage_root.as_ref().map(|expected| {
        returned_lineage_roots
            .iter()
            .filter(|root| *root == expected)
            .count()
            == 1
            && returned_lineage_roots.len()
                == returned_lineage_roots.iter().collect::<HashSet<_>>().len()
    });
    let context_answerable = case.context.as_ref().map(|expectation| {
        results.iter().any(|result| {
            relevance_grade(result, case) > 0
                && expectation
                    .ask_contains
                    .as_ref()
                    .is_none_or(|needle| contains_case_insensitive(result.ask.as_deref(), needle))
                && expectation.outcome_contains.as_ref().is_none_or(|needle| {
                    contains_case_insensitive(result.outcome.as_deref(), needle)
                })
        })
    });
    Ok(RelevanceCaseResult {
        id: case.id.clone(),
        query: case.query.clone(),
        tags: case.tags.clone(),
        returned_ids,
        returned_lineage_roots,
        relevant_results,
        reciprocal_rank,
        ndcg_at_10,
        lineage_collapse_correct,
        context_answerable,
    })
}

fn relevance_grade(result: &SearchResult, case: &RelevanceCase) -> u8 {
    case.labels
        .iter()
        .filter(|label| {
            label.session_id == result.id
                || label.session_id == result.lineage_root_id
                || result.related_session_ids.contains(&label.session_id)
        })
        .map(|label| label.grade)
        .max()
        .unwrap_or(0)
}

fn ndcg(results: &[SearchResult], case: &RelevanceCase) -> f64 {
    let ideal = case
        .labels
        .iter()
        .filter(|label| label.grade > 0)
        .map(|label| gain(label.grade))
        .collect::<Vec<_>>();
    if ideal.is_empty() {
        return 0.0;
    }
    let mut ideal = ideal;
    ideal.sort_by(|left, right| right.total_cmp(left));
    let ideal_dcg = ideal
        .iter()
        .take(10)
        .enumerate()
        .map(|(index, value)| value / (index as f64 + 2.0).log2())
        .sum::<f64>();
    if ideal_dcg == 0.0 {
        return 0.0;
    }
    results
        .iter()
        .take(10)
        .enumerate()
        .map(|(index, result)| gain(relevance_grade(result, case)) / (index as f64 + 2.0).log2())
        .sum::<f64>()
        / ideal_dcg
}

fn gain(grade: u8) -> f64 {
    (2_u32.pow(u32::from(grade)) - 1) as f64
}

fn aggregate_metrics(
    cases: &[RelevanceCase],
    results: &[RelevanceCaseResult],
    tag: Option<RelevanceTag>,
) -> RelevanceMetrics {
    let selected = cases
        .iter()
        .zip(results)
        .filter(|(case, _)| tag.is_none_or(|tag| case.tags.contains(&tag)))
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return RelevanceMetrics {
            query_count: 0,
            recall_at_5: 0.0,
            recall_at_10: 0.0,
            mrr: 0.0,
            ndcg_at_10: 0.0,
            lineage_collapse_accuracy: 0.0,
            context_answerability: 0.0,
        };
    }
    let mut recall5 = 0.0;
    let mut recall10 = 0.0;
    let mut mrr = 0.0;
    let mut ndcg10 = 0.0;
    let mut lineage_total = 0;
    let mut lineage_passed = 0;
    let mut context_total = 0;
    let mut context_passed = 0;
    for (case, result) in selected.iter().copied() {
        let relevant_count = case
            .labels
            .iter()
            .filter(|label| label.grade > 0)
            .count()
            .max(1);
        let hit5 = result
            .returned_ids
            .iter()
            .take(5)
            .filter(|id| returned_id_is_relevant(id, result, case))
            .count();
        let hit10 = result
            .returned_ids
            .iter()
            .take(10)
            .filter(|id| returned_id_is_relevant(id, result, case))
            .count();
        recall5 += hit5 as f64 / relevant_count as f64;
        recall10 += hit10 as f64 / relevant_count as f64;
        mrr += result.reciprocal_rank;
        ndcg10 += result.ndcg_at_10;
        if let Some(correct) = result.lineage_collapse_correct {
            lineage_total += 1;
            lineage_passed += usize::from(correct);
        }
        if let Some(answerable) = result.context_answerable {
            context_total += 1;
            context_passed += usize::from(answerable);
        }
    }
    let count = selected.len() as f64;
    RelevanceMetrics {
        query_count: selected.len(),
        recall_at_5: recall5 / count,
        recall_at_10: recall10 / count,
        mrr: mrr / count,
        ndcg_at_10: ndcg10 / count,
        lineage_collapse_accuracy: ratio(lineage_passed, lineage_total),
        context_answerability: ratio(context_passed, context_total),
    }
}

fn returned_id_is_relevant(id: &str, result: &RelevanceCaseResult, case: &RelevanceCase) -> bool {
    let root = result
        .returned_ids
        .iter()
        .position(|returned| returned == id)
        .and_then(|index| result.returned_lineage_roots.get(index));
    case.labels.iter().any(|label| {
        label.grade > 0
            && (label.session_id == id || root.is_some_and(|root| label.session_id == *root))
    })
}

fn ratio(passed: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        passed as f64 / total as f64
    }
}

fn contains_case_insensitive(value: Option<&str>, needle: &str) -> bool {
    value
        .map(|value| value.to_lowercase().contains(&needle.to_lowercase()))
        .unwrap_or(false)
}

fn event(kind: EventKind, text: impl Into<String>) -> Event {
    Event::new(kind, text)
}

#[allow(clippy::too_many_arguments)]
fn session(
    id: &str,
    title: &str,
    cwd: &str,
    started_at_ms: i64,
    model: &str,
    provider: &str,
    parent_session_id: Option<&str>,
    forked_from: Option<&str>,
    events: Vec<Event>,
) -> ParsedSession {
    ParsedSession {
        session: Session {
            id: id.into(),
            agent: Agent::Codex,
            cwd: Some(cwd.into()),
            started_at_ms: Some(started_at_ms),
            ended_at_ms: Some(started_at_ms + 1_000),
            title: Some(title.into()),
            model: Some(model.into()),
            provider: Some(provider.into()),
            git_branch: Some("relevance".into()),
            parent_session_id: parent_session_id.map(str::to_owned),
            forked_from: forked_from.map(str::to_owned),
            meta: json!({"suite":"relevance","signals":["title","cwd","tool","error","model","provider"]}),
            fingerprint: id.into(),
            sources: Vec::new(),
        },
        events,
    }
}

fn build_corpus(database: &mut TraceDb) -> Result<usize> {
    let day = 86_400_000_i64;
    let now = 1_800_000_000_000_i64;
    let corpus = vec![
        session(
            "codex:deploy",
            "Netlify deploy",
            "/workspace/web",
            now - day,
            "gpt-5",
            "openai",
            None,
            None,
            vec![
                event(EventKind::User, "Deploy the web service to Netlify"),
                event(EventKind::ToolCall, "netlify deploy --prod"),
                event(
                    EventKind::Assistant,
                    "The web service was deployed successfully",
                ),
            ],
        ),
        session(
            "codex:generic-deploy",
            "Deployment notes",
            "/workspace/misc",
            now - 2 * day,
            "gpt-4",
            "openai",
            None,
            None,
            vec![
                event(EventKind::User, "deploy service"),
                event(EventKind::Assistant, "deployment noted"),
            ],
        ),
        session(
            "codex:sqlite",
            "SQLite migration rollback",
            "/workspace/api",
            now - 4 * day,
            "claude-4",
            "anthropic",
            None,
            None,
            vec![
                event(EventKind::User, "Migrate the SQLite schema safely"),
                event(EventKind::ToolCall, "sqlite migration rollback checkpoint"),
                event(
                    EventKind::Assistant,
                    "Migration completed with a rollback plan",
                ),
            ],
        ),
        session(
            "codex:chinese",
            "中文部署故障",
            "/workspace/cn",
            now - 3 * day,
            "gpt-5",
            "openai",
            None,
            None,
            vec![
                event(EventKind::User, "修复部署 失败问题"),
                event(EventKind::Assistant, "部署 失败已定位并修复"),
            ],
        ),
        session(
            "codex:mixed-language",
            "API timeout recovery",
            "/workspace/i18n",
            now - 5 * day,
            "gpt-5",
            "openai",
            None,
            None,
            vec![
                event(EventKind::User, "réparer API timeout error"),
                {
                    let mut e = event(EventKind::System, "timeout error from provider model");
                    e.is_error = Some(true);
                    e
                },
                event(
                    EventKind::Assistant,
                    "Réparation complete; timeout resolved",
                ),
            ],
        ),
        session(
            "codex:infra",
            "Terraform provider rollout",
            "/workspace/infra",
            now - 6 * day,
            "gpt-5",
            "azure",
            None,
            None,
            vec![
                event(EventKind::User, "configure the Terraform provider"),
                event(EventKind::ToolCall, "terraform init provider aws"),
                event(EventKind::Assistant, "Terraform provider configured"),
            ],
        ),
        session(
            "codex:old-auth",
            "Important authentication recovery",
            "/workspace/security",
            1_600_000_000_000,
            "gpt-4",
            "openai",
            None,
            None,
            vec![
                event(EventKind::User, "authentication token rotation recovery"),
                event(EventKind::ToolCall, "rotate authentication credentials"),
                event(EventKind::Assistant, "Authentication tokens rotated safely"),
            ],
        ),
        session(
            "codex:recent-auth",
            "Recent auth note",
            "/workspace/security",
            now - day,
            "gpt-5",
            "openai",
            None,
            None,
            vec![
                event(EventKind::User, "authentication token rotation"),
                event(EventKind::Assistant, "authentication reviewed"),
            ],
        ),
        session(
            "codex:lineage-parent",
            "Production incident",
            "/workspace/ops",
            now - 8 * day,
            "gpt-5",
            "openai",
            None,
            None,
            vec![
                event(EventKind::User, "Investigate the production outage"),
                event(
                    EventKind::Assistant,
                    "Production incident resolved after rollback",
                ),
            ],
        ),
        session(
            "codex:lineage-child",
            "Incident subagent",
            "/workspace/ops",
            now - 7 * day,
            "gpt-5",
            "openai",
            Some("codex:lineage-parent"),
            None,
            vec![
                event(
                    EventKind::User,
                    "bisect regression and rollback migration details",
                ),
                event(EventKind::ToolCall, "git bisect regression"),
            ],
        ),
        session(
            "codex:fork-root",
            "Configuration investigation",
            "/workspace/config",
            now - 9 * day,
            "gpt-5",
            "openai",
            None,
            None,
            vec![
                event(EventKind::User, "Investigate configuration drift"),
                event(EventKind::Assistant, "Configuration baseline recorded"),
            ],
        ),
        session(
            "codex:fork-child",
            "Configuration fork",
            "/workspace/config",
            now - 8 * day,
            "gpt-5",
            "openai",
            None,
            Some("codex:fork-root#experiment"),
            vec![
                event(EventKind::User, "config fork experiment"),
                event(EventKind::Assistant, "Configuration fork validated"),
            ],
        ),
    ];
    for parsed in corpus {
        database.ingest_session(parsed, IngestMode::Partial)?;
    }
    Ok(12)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_suite_has_required_dimensions() {
        let cases = standard_cases();
        assert_eq!(cases.len(), STANDARD_RELEVANCE_QUERY_COUNT);
        let tags = cases
            .iter()
            .flat_map(|case| case.tags.iter())
            .collect::<HashSet<_>>();
        for tag in [
            RelevanceTag::Multilingual,
            RelevanceTag::MultiTerm,
            RelevanceTag::Title,
            RelevanceTag::Cwd,
            RelevanceTag::Tool,
            RelevanceTag::Error,
            RelevanceTag::Model,
            RelevanceTag::Provider,
            RelevanceTag::OldImportant,
            RelevanceTag::ParentSubagent,
            RelevanceTag::Fork,
            RelevanceTag::DistantContext,
        ] {
            assert!(tags.contains(&tag));
        }
    }

    #[test]
    fn evaluator_returns_stable_shape_and_successful_context_checks() {
        let report = evaluate_relevance().unwrap();
        assert_eq!(report.schema_version, RELEVANCE_SCHEMA_VERSION);
        assert_eq!(report.query_count, STANDARD_RELEVANCE_QUERY_COUNT);
        assert_eq!(report.corpus_sessions, 12);
        assert!(report.metrics.recall_at_10 > 0.8);
        assert!(report.metrics.mrr > 0.6);
        assert!(report.metrics.ndcg_at_10 > 0.7);
        assert_eq!(report.metrics.lineage_collapse_accuracy, 1.0);
        assert_eq!(report.metrics.context_answerability, 1.0);
    }

    #[test]
    fn metadata_signal_fixtures_are_preserved_and_cwd_filterable() {
        let mut database = TraceDb::open(":memory:").unwrap();
        assert_eq!(build_corpus(&mut database).unwrap(), 12);

        let filtered = database
            .search(SearchRequest {
                query: "deploy".into(),
                limit: 10,
                agent: None,
                cwd: Some("/workspace/web".into()),
                since_ms: None,
            })
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "codex:deploy");
        assert_eq!(filtered[0].title.as_deref(), Some("Netlify deploy"));
        assert_eq!(filtered[0].cwd.as_deref(), Some("/workspace/web"));

        let old_auth = database.show("codex:old-auth").unwrap().unwrap();
        assert_eq!(old_auth.session.model.as_deref(), Some("gpt-4"));
        assert_eq!(old_auth.session.provider.as_deref(), Some("openai"));

        let errored = database.show("codex:mixed-language").unwrap().unwrap();
        assert!(errored
            .events
            .iter()
            .any(|event| event.is_error == Some(true)));
    }
}
