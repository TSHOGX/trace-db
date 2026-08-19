pub mod claude;
pub mod codex;
pub mod gemini;
pub mod opencode;
pub mod pi;

use crate::model::{Agent, ParsedSession};
use anyhow::Result;
use std::{
    fs::Metadata,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// Cheap metadata discovered before a native session is fully parsed.
#[derive(Debug, Clone)]
pub struct SessionCandidate {
    pub path: PathBuf,
    pub locator: String,
    pub native_id: Option<String>,
    pub fingerprint: String,
    pub updated_at_ms: Option<i64>,
    pub bytes: Option<i64>,
    pub mtime_ns: Option<i64>,
    pub parent_session_id: Option<String>,
    pub agent_type: Option<String>,
}

impl SessionCandidate {
    /// Build a metadata fingerprint for a file-backed native source.
    pub fn file(path: PathBuf) -> Result<Self> {
        let metadata = path.metadata()?;
        let mtime_ns = modified_ns(&metadata);
        Ok(Self {
            locator: path.display().to_string(),
            fingerprint: format!(
                "file-v1:{}:{}",
                metadata.len(),
                mtime_ns.unwrap_or_default()
            ),
            updated_at_ms: mtime_ns.map(|value| value / 1_000_000),
            bytes: Some(metadata.len() as i64),
            mtime_ns,
            path,
            native_id: None,
            parent_session_id: None,
            agent_type: None,
        })
    }

    /// Add a related file, such as Claude's subagent metadata sidecar, to the hint.
    pub fn include_file(&mut self, path: &Path) {
        if let Ok(metadata) = path.metadata() {
            self.fingerprint.push_str(&format!(
                ":{}:{}",
                metadata.len(),
                modified_ns(&metadata).unwrap_or_default()
            ));
        }
    }
}

fn modified_ns(metadata: &Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
}

pub trait Parser {
    /// Identify the native agent handled by this parser.
    fn agent(&self) -> Agent;
    /// Discover cheap candidates without parsing complete session contents.
    fn discover(&self, root: &Path) -> Result<Vec<SessionCandidate>>;
    /// Parse one candidate. `None` means the candidate is intentionally filtered.
    fn parse(&self, candidate: &SessionCandidate, root: &Path) -> Result<Option<ParsedSession>>;

    fn parse_all(&self, root: &Path) -> Result<Vec<ParsedSession>> {
        let mut sessions = Vec::new();
        for candidate in self.discover(root)? {
            if let Some(mut parsed) = self.parse(&candidate, root)? {
                parsed.session.fingerprint = candidate.fingerprint;
                sessions.push(parsed);
            }
        }
        Ok(sessions)
    }
}

pub fn parser(agent: Agent) -> Box<dyn Parser> {
    match agent {
        Agent::Claude => Box::new(claude::ClaudeParser),
        Agent::Codex => Box::new(codex::CodexParser),
        Agent::OpenCode => Box::new(opencode::OpenCodeParser),
        Agent::Gemini => Box::new(gemini::GeminiParser),
        Agent::Pi => Box::new(pi::PiParser),
    }
}
