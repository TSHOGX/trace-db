//! Privacy policy applied to normalized metadata and events.

use crate::model::{Event, ParsedSession, Session};
use regex::Regex;
use serde_json::Value;

const REDACTED: &str = "[REDACTED]";

/// Credential patterns enabled for every ingest, before user-supplied rules.
const BUILTIN_PATTERNS: &[&str] = &[
    r"(?i)\b(?:authorization\s*:\s*bearer|bearer)\s+[A-Za-z0-9._~+/=-]+",
    r"(?i)\b(?:api[_-]?key|apikey|token|password|secret)\s*[:=]\s*[^\s,;]+",
    r"\b(?:sk|ghp|github_pat|xoxb|xoxp|akia)-[A-Za-z0-9_-]+\b",
    r"(?i)\b[A-Z][A-Z0-9_]*(?:TOKEN|KEY|SECRET|PASSWORD)\s*=\s*[^\s,;]+",
];

#[derive(Clone)]
pub(crate) struct Redactor {
    patterns: Vec<Regex>,
}

impl Redactor {
    pub(crate) fn new(user_patterns: &[String]) -> anyhow::Result<Self> {
        let mut patterns = BUILTIN_PATTERNS
            .iter()
            .map(|pattern| Regex::new(pattern).expect("built-in privacy pattern must compile"))
            .collect::<Vec<_>>();
        patterns.extend(
            user_patterns
                .iter()
                .map(|pattern| Regex::new(pattern))
                .collect::<Result<Vec<_>, _>>()?,
        );
        Ok(Self { patterns })
    }

    pub(crate) fn redact_session(&self, parsed: &mut ParsedSession) {
        redact_session(&self.patterns, &mut parsed.session);
        for event in &mut parsed.events {
            redact_event(&self.patterns, event);
        }
    }

    #[cfg(test)]
    pub(crate) fn redact_text(&self, text: &str) -> String {
        redact_string(&self.patterns, text)
    }
}

fn redact_session(patterns: &[Regex], session: &mut Session) {
    redact_option(patterns, &mut session.cwd);
    redact_option(patterns, &mut session.title);
    redact_option(patterns, &mut session.model);
    redact_option(patterns, &mut session.provider);
    redact_option(patterns, &mut session.git_branch);
    redact_option(patterns, &mut session.parent_session_id);
    redact_option(patterns, &mut session.forked_from);
    redact_value(patterns, &mut session.meta);
}

fn redact_event(patterns: &[Regex], event: &mut Event) {
    event.subtype = redact_option_value(patterns, event.subtype.take());
    event.role = redact_option_value(patterns, event.role.take());
    event.name = redact_option_value(patterns, event.name.take());
    event.call_id = redact_option_value(patterns, event.call_id.take());
    event.parent_id = redact_option_value(patterns, event.parent_id.take());
    event.model = redact_option_value(patterns, event.model.take());
    event.provider = redact_option_value(patterns, event.provider.take());
    event.text = redact_string(patterns, &event.text);
    if let Some(data) = &mut event.data_json {
        redact_value(patterns, data);
    }
}

fn redact_option(patterns: &[Regex], value: &mut Option<String>) {
    if let Some(value) = value {
        *value = redact_string(patterns, value);
    }
}

fn redact_option_value(patterns: &[Regex], value: Option<String>) -> Option<String> {
    value.map(|value| redact_string(patterns, &value))
}

fn redact_value(patterns: &[Regex], value: &mut Value) {
    match value {
        Value::String(text) => *text = redact_string(patterns, text),
        Value::Array(values) => values
            .iter_mut()
            .for_each(|value| redact_value(patterns, value)),
        Value::Object(values) => values
            .values_mut()
            .for_each(|value| redact_value(patterns, value)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn redact_string(patterns: &[Regex], value: &str) -> String {
    patterns.iter().fold(value.to_owned(), |value, pattern| {
        pattern.replace_all(&value, REDACTED).into_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_redact_credentials_and_custom_rules_redact_paths() {
        let redactor = Redactor::new(&[r"/Users/[^ ]+".into()]).unwrap();
        let value = redactor.redact_text(
            "Authorization: Bearer abc token=secret123 API_TOKEN=xyz /Users/alice/private",
        );
        assert_eq!(value, "[REDACTED] [REDACTED] [REDACTED] [REDACTED]");
    }
}
