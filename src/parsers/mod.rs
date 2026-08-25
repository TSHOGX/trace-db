pub mod claude;
pub mod codex;
pub mod gemini;
pub mod opencode;
pub mod pi;

use crate::model::{Agent, ParsedSession};
use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::Metadata,
    fs::{self, File},
    io::{BufRead, BufReader, Read},
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
    pub mode: Option<u32>,
    pub parent_session_id: Option<String>,
    pub agent_type: Option<String>,
}

impl SessionCandidate {
    /// Build a content fingerprint for a file-backed native source. Metadata
    /// alone can miss same-size rewrites or coarse-timestamp updates.
    pub fn file(path: PathBuf) -> Result<Self> {
        Self::file_with_cache(path, None)
    }

    /// Reuse a previously stored fingerprint when size and mtime are unchanged,
    /// avoiding a full-file SHA-256 on every discovery pass.
    pub fn file_with_cache(path: PathBuf, cached_fingerprint: Option<&str>) -> Result<Self> {
        let metadata = path.metadata()?;
        let mtime_ns = modified_ns_public(&metadata);
        let bytes = metadata.len();
        let fingerprint = if let Some(cached) = cached_fingerprint {
            if fingerprint_metadata_matches(cached, bytes, mtime_ns) {
                cached.to_string()
            } else {
                file_fingerprint(&path, bytes, mtime_ns)?
            }
        } else {
            file_fingerprint(&path, bytes, mtime_ns)?
        };
        Ok(Self {
            locator: path.display().to_string(),
            fingerprint,
            updated_at_ms: mtime_ns.map(|value| value / 1_000_000),
            bytes: Some(bytes as i64),
            mtime_ns,
            mode: file_mode(&metadata),
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
            ":{}:{}:{}",
            sha256_file(path)?,
            metadata.len(),
            modified_ns_public(&metadata).unwrap_or_default()
        ));
        Ok(())
    }
}

fn file_fingerprint(path: &Path, bytes: u64, mtime_ns: Option<i64>) -> Result<String> {
    Ok(format!(
        "file-v2:{}:{}:{}",
        sha256_file(path)?,
        bytes,
        mtime_ns.unwrap_or_default()
    ))
}

pub(crate) fn fingerprint_metadata_matches(
    fingerprint: &str,
    bytes: u64,
    mtime_ns: Option<i64>,
) -> bool {
    let Some(rest) = fingerprint.strip_prefix("file-v2:") else {
        return false;
    };
    let Some((_, tail)) = rest.split_once(':') else {
        return false;
    };
    let Some((stored_bytes, stored_mtime)) = tail.rsplit_once(':') else {
        return false;
    };
    stored_bytes == bytes.to_string()
        && stored_mtime.parse::<i64>().ok() == mtime_ns.or(Some(0))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("failed to fingerprint native source {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to fingerprint native source {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(unix)]
fn file_mode(metadata: &Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode())
}

#[cfg(not(unix))]
fn file_mode(_metadata: &Metadata) -> Option<u32> {
    None
}

pub(crate) fn modified_ns_public(metadata: &Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
}

pub(crate) fn file_mode_public(metadata: &Metadata) -> Option<u32> {
    file_mode(metadata)
}

pub trait Parser {
    /// Identify the native agent handled by this parser.
    fn agent(&self) -> Agent;
    /// Discover cheap candidates without parsing complete session contents.
    fn discover(&self, root: &Path) -> Result<Discovery> {
        self.discover_with_states(root, &std::collections::HashMap::new())
    }
    /// Discover candidates, reusing stored fingerprints when file metadata is unchanged.
    fn discover_with_states(
        &self,
        root: &Path,
        states: &std::collections::HashMap<String, String>,
    ) -> Result<Discovery>;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_metadata_matches_unchanged_files() {
        let fingerprint = "file-v2:deadbeef:42:1000";
        assert!(fingerprint_metadata_matches(fingerprint, 42, Some(1000)));
        assert!(!fingerprint_metadata_matches(fingerprint, 43, Some(1000)));
        assert!(!fingerprint_metadata_matches("other", 42, Some(1000)));
    }

    #[test]
    fn file_with_cache_reuses_fingerprint_when_metadata_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, b"{\"type\":\"session_meta\"}\n").unwrap();
        let first = SessionCandidate::file(path.clone()).unwrap();
        let second =
            SessionCandidate::file_with_cache(path, Some(&first.fingerprint)).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
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
