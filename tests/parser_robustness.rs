//! Property coverage for malformed and arbitrary native parser input.

use proptest::prelude::*;
use std::{
    fs,
    panic::{catch_unwind, AssertUnwindSafe},
};
use tempfile::tempdir;
use tracedb::parsers::{
    claude::ClaudeParser, codex::CodexParser, gemini::GeminiParser, opencode::OpenCodeParser,
    pi::PiParser, Parser,
};

fn assert_parser_does_not_panic<P: Parser>(parser: P, filename: &str, bytes: &[u8]) {
    let directory = tempdir().expect("create parser fixture directory");
    fs::write(directory.path().join(filename), bytes).expect("write parser fixture");
    let result = catch_unwind(AssertUnwindSafe(|| parser.parse_all(directory.path())));
    assert!(result.is_ok(), "parser panicked for arbitrary input");
}

proptest! {
    #[test]
    fn claude_parser_rejects_or_parses_arbitrary_jsonl_without_panicking(
        bytes in prop::collection::vec(any::<u8>(), 0..2048)
    ) {
        assert_parser_does_not_panic(ClaudeParser, "session.jsonl", &bytes);
    }

    #[test]
    fn codex_parser_rejects_or_parses_arbitrary_jsonl_without_panicking(
        bytes in prop::collection::vec(any::<u8>(), 0..2048)
    ) {
        assert_parser_does_not_panic(CodexParser, "rollout-fuzz.jsonl", &bytes);
    }

    #[test]
    fn gemini_parser_rejects_or_parses_arbitrary_json_without_panicking(
        bytes in prop::collection::vec(any::<u8>(), 0..2048)
    ) {
        assert_parser_does_not_panic(GeminiParser, "session-fuzz.json", &bytes);
    }

    #[test]
    fn pi_parser_rejects_or_parses_arbitrary_jsonl_without_panicking(
        bytes in prop::collection::vec(any::<u8>(), 0..2048)
    ) {
        assert_parser_does_not_panic(PiParser, "session.jsonl", &bytes);
    }
}

#[test]
fn malformed_jsonl_reports_a_locator_and_line_number() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("session.jsonl");
    fs::write(&path, b"{\"type\":\"session\"}\n{not-json\n").unwrap();
    let error = PiParser.parse_all(directory.path()).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("session.jsonl"));
    assert!(message.contains("line 2"));
}

#[test]
fn malformed_opencode_database_returns_an_error_without_panicking() {
    let directory = tempdir().unwrap();
    fs::write(directory.path().join("opencode.db"), b"not sqlite").unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        OpenCodeParser.parse_all(directory.path())
    }));
    assert!(
        result.is_ok(),
        "OpenCode parser panicked for malformed SQLite"
    );
    assert!(result.unwrap().is_err());
}
