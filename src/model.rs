//! The stable, agent-neutral data model.
//!
//! The normalized layer intentionally contains only information that is useful
//! to every supported coding agent. Agent-specific fields remain in `meta` and
//! `data_json`; in `full` mode the original native source is additionally kept
//! as a content-addressed object, so normalization can never become the source
//! of truth.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    OpenCode,
    Gemini,
    Pi,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IngestMode {
    #[default]
    Partial,
    Full,
}

impl IngestMode {
    /// A full snapshot is sticky: a later default/partial ingest must never
    /// silently throw away the only copy of a native source.
    pub fn retain_full(self, existing: Option<IngestMode>) -> IngestMode {
        if matches!(self, IngestMode::Full) || matches!(existing, Some(IngestMode::Full)) {
            IngestMode::Full
        } else {
            IngestMode::Partial
        }
    }
}

impl fmt::Display for IngestMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Partial => "partial",
            Self::Full => "full",
        })
    }
}

impl FromStr for IngestMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "partial" => Ok(Self::Partial),
            "full" => Ok(Self::Full),
            _ => Err(format!(
                "unknown ingest mode: {s} (expected partial or full)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    User,
    Assistant,
    Thinking,
    ToolCall,
    ToolResult,
    System,
    Usage,
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
    pub model: Option<String>,
    pub provider: Option<String>,
    pub usage: Option<TokenUsage>,
    pub text: String,
    /// Structured arguments/results/message content when it is useful and
    /// reasonably bounded. Full native bytes are still the lossless copy.
    pub data_json: Option<Value>,
    pub created_at_ms: Option<i64>,
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
            model: None,
            provider: None,
            usage: None,
            text: text.into(),
            data_json: None,
            created_at_ms: None,
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
