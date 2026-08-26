//! The stable, agent-neutral data model.
//!
//! The normalized layer intentionally contains only information that is useful
//! to every supported coding agent. Agent-specific fields remain in `meta` and
//! `data_json`; every ingest additionally keeps the original native source as a
//! content-addressed object, so normalization can never become the source of
//! truth.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fmt, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    OpenCode,
    Gemini,
    Pi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Completed,
    Failed,
    Interrupted,
    Abandoned,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Abandoned => "abandoned",
        }
    }
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SessionStatus {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "active" => Ok(Self::Active),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "interrupted" => Ok(Self::Interrupted),
            "abandoned" => Ok(Self::Abandoned),
            _ => Err(format!("unknown session status: {value}")),
        }
    }
}

impl Agent {
    pub const ALL: [Agent; 5] = [
        Agent::Claude,
        Agent::Codex,
        Agent::OpenCode,
        Agent::Gemini,
        Agent::Pi,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::OpenCode => "opencode",
            Agent::Gemini => "gemini",
            Agent::Pi => "pi",
        }
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Agent {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "opencode" | "open-code" => Ok(Self::OpenCode),
            "gemini" => Ok(Self::Gemini),
            "pi" => Ok(Self::Pi),
            _ => Err(format!("unknown agent: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    User,
    Assistant,
    Thinking,
    ToolCall,
    ToolResult,
    System,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventParentKind {
    /// A transcript predecessor link, typically forming a linear chain.
    PreviousEvent,
    /// A native message-parent/grouping relationship.
    MessageParent,
    /// The producer overloads the native field with multiple relationship kinds.
    NativeMixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    Tool,
    Delegation,
}

impl SpanKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Delegation => "delegation",
        }
    }
}

impl fmt::Display for SpanKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SpanKind {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "tool" => Ok(Self::Tool),
            "delegation" => Ok(Self::Delegation),
            _ => Err(format!("unknown span kind: {value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanStatus {
    Active,
    Completed,
    Failed,
    Interrupted,
    Abandoned,
}

impl SpanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Abandoned => "abandoned",
        }
    }
}

impl fmt::Display for SpanStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SpanStatus {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "active" => Ok(Self::Active),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "interrupted" => Ok(Self::Interrupted),
            "abandoned" => Ok(Self::Abandoned),
            _ => Err(format!("unknown span status: {value}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub id: String,
    pub parent_span_id: Option<String>,
    pub kind: SpanKind,
    pub name: Option<String>,
    pub native_id: Option<String>,
    pub call_id: Option<String>,
    pub status: Option<SpanStatus>,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub start_event_idx: Option<i64>,
    pub end_event_idx: Option<i64>,
    pub data_json: Option<Value>,
}

impl EventParentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreviousEvent => "previous_event",
            Self::MessageParent => "message_parent",
            Self::NativeMixed => "native_mixed",
        }
    }
}

impl fmt::Display for EventParentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventParentKind {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "previous_event" => Ok(Self::PreviousEvent),
            "message_parent" => Ok(Self::MessageParent),
            "native_mixed" => Ok(Self::NativeMixed),
            _ => Err(format!("unknown event parent kind: {value}")),
        }
    }
}

impl EventKind {
    pub const ALL: [EventKind; 7] = [
        Self::User,
        Self::Assistant,
        Self::Thinking,
        Self::ToolCall,
        Self::ToolResult,
        Self::System,
        Self::Usage,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Thinking => "thinking",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::System => "system",
            Self::Usage => "usage",
        }
    }

    pub fn searchable(self) -> bool {
        !matches!(self, Self::ToolResult | Self::Usage)
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "thinking" => Ok(Self::Thinking),
            "tool_call" => Ok(Self::ToolCall),
            "tool_result" => Ok(Self::ToolResult),
            "system" => Ok(Self::System),
            "usage" => Ok(Self::Usage),
            _ => Err(format!("unknown event kind: {s}")),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: Option<i64>,
    pub output: Option<i64>,
    pub reasoning: Option<i64>,
    pub cache_read: Option<i64>,
    pub cache_write: Option<i64>,
    pub total: Option<i64>,
}

impl TokenUsage {
    pub fn total_or_sum(&self) -> Option<i64> {
        self.total.or_else(|| {
            let values = [
                self.input,
                self.output,
                self.reasoning,
                self.cache_read,
                self.cache_write,
            ];
            let mut sum = 0;
            let mut any = false;
            for value in values.into_iter().flatten() {
                sum += value;
                any = true;
            }
            any.then_some(sum)
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub idx: i64,
    pub kind: EventKind,
    pub subtype: Option<String>,
    pub role: Option<String>,
    pub name: Option<String>,
    pub call_id: Option<String>,
    pub is_error: Option<bool>,
    pub native_id: Option<String>,
    pub parent_id: Option<String>,
    pub parent_kind: Option<EventParentKind>,
    /// First-class normalized trajectory containing this event.
    pub span_id: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub usage: Option<TokenUsage>,
    pub text: String,
    /// Structured arguments/results/message content when it is useful and
    /// reasonably bounded. Full native bytes are still the lossless copy.
    pub data_json: Option<Value>,
    pub created_at_ms: Option<i64>,
    /// Explicit event end time when the native producer provides one.
    /// This is never inferred from the next event's timestamp.
    pub ended_at_ms: Option<i64>,
}

impl Event {
    pub fn new(kind: EventKind, text: impl Into<String>) -> Self {
        Self {
            idx: 0,
            kind,
            subtype: None,
            role: None,
            name: None,
            call_id: None,
            is_error: None,
            native_id: None,
            parent_id: None,
            parent_kind: None,
            span_id: None,
            model: None,
            provider: None,
            usage: None,
            text: text.into(),
            data_json: None,
            created_at_ms: None,
            ended_at_ms: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Capture {
    /// A regular file in one of the native agent stores.
    File { path: String },
    /// A logical SQLite session bundle. The payload is a deterministic JSON
    /// envelope of typed rows and schema needed by `reconstruct`.
    Bytes { label: String, bytes: Vec<u8> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeSource {
    pub locator: String,
    pub kind: String,
    /// A path relative to the reconstructed native root. It never accepts `..`.
    pub restore_path: String,
    pub role: Option<String>,
    pub bytes: Option<i64>,
    pub mtime_ns: Option<i64>,
    pub mode: Option<u32>,
    #[serde(skip)]
    pub capture: Option<Capture>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub agent: Agent,
    pub cwd: Option<String>,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub status: Option<SessionStatus>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub git_branch: Option<String>,
    pub parent_session_id: Option<String>,
    pub forked_from: Option<String>,
    pub meta: Value,
    pub fingerprint: String,
    pub sources: Vec<NativeSource>,
}

#[derive(Debug, Clone)]
pub struct ParsedSession {
    pub session: Session,
    pub events: Vec<Event>,
}

pub fn assign_indexes(events: &mut [Event]) {
    for (idx, event) in events.iter_mut().enumerate() {
        event.idx = idx as i64;
    }
}

/// Materialize stable turn-internal trajectories without inventing sessions.
/// Tool calls/results form call spans; delegation payloads may additionally
/// fan out child spans by native `task_id`.
pub fn derive_spans(events: &mut [Event]) -> Vec<Span> {
    let mut spans = BTreeMap::<String, Span>::new();
    for event in events {
        if !matches!(event.kind, EventKind::ToolCall | EventKind::ToolResult) {
            continue;
        }
        let Some(call_id) = event.call_id.clone() else {
            continue;
        };
        let span_id = format!("call:{call_id}");
        let delegation = event.name.as_deref().is_some_and(is_delegation_tool);
        let span = spans.entry(span_id.clone()).or_insert_with(|| Span {
            id: span_id.clone(),
            parent_span_id: None,
            kind: if delegation {
                SpanKind::Delegation
            } else {
                SpanKind::Tool
            },
            name: event.name.clone(),
            native_id: event.native_id.clone(),
            call_id: Some(call_id.clone()),
            status: Some(SpanStatus::Active),
            started_at_ms: event.created_at_ms,
            ended_at_ms: None,
            start_event_idx: Some(event.idx),
            end_event_idx: None,
            data_json: None,
        });
        if delegation {
            span.kind = SpanKind::Delegation;
        }
        span.name = span.name.clone().or_else(|| event.name.clone());
        span.started_at_ms = span.started_at_ms.or(event.created_at_ms);
        span.start_event_idx = span.start_event_idx.or(Some(event.idx));
        if event.kind == EventKind::ToolResult {
            span.ended_at_ms = event.ended_at_ms.or(event.created_at_ms);
            span.end_event_idx = Some(event.idx);
            span.status = Some(if event.is_error == Some(true) {
                SpanStatus::Failed
            } else {
                SpanStatus::Completed
            });
        }
        event.span_id = Some(span_id.clone());

        if let Some(data) = event.data_json.as_ref() {
            let mut tasks = Vec::new();
            collect_task_records(data, &mut tasks);
            for (task_id, task_data) in tasks {
                let child_id = format!("task:{task_id}");
                let child = spans.entry(child_id.clone()).or_insert_with(|| Span {
                    id: child_id,
                    parent_span_id: Some(span_id.clone()),
                    kind: SpanKind::Delegation,
                    name: task_data
                        .get("name")
                        .or_else(|| task_data.get("subject"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    native_id: Some(task_id),
                    call_id: None,
                    status: task_data
                        .get("status")
                        .and_then(Value::as_str)
                        .and_then(parse_span_status),
                    started_at_ms: task_data
                        .get("started_at_ms")
                        .and_then(Value::as_i64)
                        .or(event.created_at_ms),
                    ended_at_ms: task_data.get("ended_at_ms").and_then(Value::as_i64),
                    start_event_idx: Some(event.idx),
                    end_event_idx: None,
                    data_json: Some(Value::Object(task_data.clone())),
                });
                if let Some(status) = task_data
                    .get("status")
                    .and_then(Value::as_str)
                    .and_then(parse_span_status)
                {
                    child.status = Some(status);
                    if !matches!(status, SpanStatus::Active) {
                        child.ended_at_ms = task_data
                            .get("ended_at_ms")
                            .and_then(Value::as_i64)
                            .or(event.ended_at_ms)
                            .or(event.created_at_ms);
                        child.end_event_idx = Some(event.idx);
                    }
                }
                child.data_json = Some(Value::Object(task_data.clone()));
            }
        }
    }
    spans.into_values().collect()
}

fn is_delegation_tool(name: &str) -> bool {
    matches!(
        name.rsplit([':', '/', '.'])
            .next()
            .unwrap_or(name)
            .to_ascii_lowercase()
            .as_str(),
        "task" | "workflow" | "spawn_agent" | "delegate"
    )
}

fn collect_task_records<'a>(
    value: &'a Value,
    output: &mut Vec<(String, &'a serde_json::Map<String, Value>)>,
) {
    match value {
        Value::Object(object) => {
            if let Some(task_id) = object.get("task_id").and_then(Value::as_str) {
                output.push((task_id.to_owned(), object));
            }
            for child in object.values() {
                collect_task_records(child, output);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_task_records(item, output);
            }
        }
        _ => {}
    }
}

fn parse_span_status(value: &str) -> Option<SpanStatus> {
    match value.to_ascii_lowercase().as_str() {
        "active" | "running" | "in_progress" => Some(SpanStatus::Active),
        "completed" | "complete" | "done" | "succeeded" => Some(SpanStatus::Completed),
        "failed" | "error" => Some(SpanStatus::Failed),
        "interrupted" | "cancelled" | "canceled" => Some(SpanStatus::Interrupted),
        "abandoned" => Some(SpanStatus::Abandoned),
        _ => None,
    }
}

pub fn turn_count(events: &[Event]) -> i64 {
    events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::User | EventKind::Assistant))
        .count() as i64
}

pub fn compact(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        _ => serde_json::to_string(value).unwrap_or_else(|_| String::from("<invalid-json>")),
    }
}

pub fn flatten(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|v| match v {
                Value::Object(obj) => obj
                    .get("text")
                    .or_else(|| obj.get("content"))
                    .map(compact)
                    .unwrap_or_else(|| compact(v)),
                _ => compact(v),
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(obj) => obj
            .get("text")
            .or_else(|| obj.get("content"))
            .map(compact)
            .unwrap_or_else(|| compact(value)),
        _ => compact(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spans_express_multiplexed_delegation_without_sessions() {
        let mut call = Event::new(EventKind::ToolCall, "workflow");
        call.name = Some("Workflow".into());
        call.call_id = Some("host-1".into());
        call.created_at_ms = Some(10);
        call.data_json = Some(json!({
            "delegates": [
                {"task_id":"task-a","name":"review"},
                {"task_id":"task-b","name":"test"}
            ]
        }));
        let mut result = Event::new(EventKind::ToolResult, "done");
        result.call_id = Some("host-1".into());
        result.created_at_ms = Some(20);
        let mut events = vec![call, result];
        assign_indexes(&mut events);

        let spans = derive_spans(&mut events);
        assert_eq!(events[0].span_id.as_deref(), Some("call:host-1"));
        assert_eq!(events[1].span_id.as_deref(), Some("call:host-1"));
        assert_eq!(spans.len(), 3);
        let host = spans.iter().find(|span| span.id == "call:host-1").unwrap();
        assert_eq!(host.kind, SpanKind::Delegation);
        assert_eq!(host.status, Some(SpanStatus::Completed));
        for task_id in ["task:task-a", "task:task-b"] {
            let child = spans.iter().find(|span| span.id == task_id).unwrap();
            assert_eq!(child.parent_span_id.as_deref(), Some("call:host-1"));
            assert_eq!(child.kind, SpanKind::Delegation);
        }
    }
}
