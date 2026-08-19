pub mod claude;
pub mod codex;
pub mod gemini;
pub mod opencode;
pub mod pi;

use crate::model::{Agent, ParsedSession};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    fs::File,
    fs::Metadata,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// Candidates and non-fatal discovery failures found during one agent scan.
#[derive(Debug, Default)]
pub struct Discovery {
    pub candidates: Vec<SessionCandidate>,
    pub failures: Vec<ParserFailure>,
}

impl Discovery {
    pub fn push_failure(&mut self, locator: impl Into<String>, error: anyhow::Error) {
        self.failures.push(ParserFailure {
            locator: locator.into(),
            error,
        });
    }
}

#[derive(Debug)]
pub struct ParserFailure {
    pub locator: String,
    pub error: anyhow::Error,
}

#[derive(Debug, thiserror::Error)]
#[error("unsupported native format: {0}")]
pub struct UnsupportedFormat(pub String);

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
    pub fn include_file(&mut self, path: &Path) -> Result<()> {
        let metadata = path.metadata()?;
        self.fingerprint.push_str(&format!(
            ":{}:{}",
            metadata.len(),
            modified_ns(&metadata).unwrap_or_default()
        ));
        Ok(())
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
    fn discover(&self, root: &Path) -> Result<Discovery>;
    /// Parse one candidate. `None` means the candidate is intentionally filtered.
    fn parse(&self, candidate: &SessionCandidate, root: &Path) -> Result<Option<ParsedSession>>;

    /// Parse a batch of candidates. Implementations may override this to share
    /// expensive read-only state, such as a database connection, across rows.
    /// Individual parse failures are retained so callers can preserve the
    /// historical best-effort ingest behavior.
    fn parse_many(
        &self,
        candidates: &[SessionCandidate],
        root: &Path,
    ) -> Vec<(SessionCandidate, Result<Option<ParsedSession>>)> {
        candidates
            .iter()
            .cloned()
            .map(|candidate| {
                let parsed = self.parse(&candidate, root);
                (candidate, parsed)
            })
            .collect()
    }

    fn parse_all(&self, root: &Path) -> Result<Vec<ParsedSession>> {
        let mut sessions = Vec::new();
        let discovery = self.discover(root)?;
        if let Some(failure) = discovery.failures.into_iter().next() {
            return Err(failure.error).with_context(|| {
                format!("failed to discover native session at {}", failure.locator)
            });
        }
        for (candidate, parsed) in self.parse_many(&discovery.candidates, root) {
            if let Some(mut parsed) = parsed? {
                parsed.session.fingerprint = candidate.fingerprint;
                sessions.push(parsed);
            }
        }
        Ok(sessions)
    }
}

pub(crate) fn read_json_lines(path: &Path) -> Result<Vec<Value>> {
    let file = File::open(path)
        .with_context(|| format!("failed to open JSONL source {}", path.display()))?;
    let mut records = Vec::new();
    for (line_index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.with_context(|| {
            format!(
                "failed to read JSONL source {} at line {}",
                path.display(),
                line_index + 1
            )
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str(&line).with_context(|| {
            format!(
                "invalid JSON in {} at line {}",
                path.display(),
                line_index + 1
            )
        })?;
        records.push(record);
    }
    Ok(records)
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
