use serde_json::Value;
use std::{path::Path, process::Command};
use tempfile::tempdir;
use tracedb::{ConfigOverrides, TraceDb, TraceDbConfig};

const TRACEDB_ENV: [&str; 10] = [
    "TRACEDB_CONFIG",
    "TRACEDB_PATH",
    "TRACEDB_AGENTS",
    "TRACEDB_CAPTURE_MODE",
    "TRACEDB_EXCLUDE",
    "TRACEDB_TOKENIZER",
    "TRACEDB_JIEBA_EXT",
    "TRACEDB_OUTPUT_FORMAT",
    "TRACEDB_REDACT_PATTERNS",
    "TRACEDB_WATCH_INTERVAL",
];

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_trace-db"));
    for name in TRACEDB_ENV {
        command.env_remove(name);
    }
    command
        .env_remove("TRACEDB_WATCH_DEBOUNCE")
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env("XDG_DATA_HOME", root.join("xdg-data"));
    command
}

fn json_output(mut command: Command) -> Value {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn config_resolves_file_paths_and_all_layers_of_precedence() {
    let dir = tempdir().unwrap();
    let config_dir = dir.path().join("project-config");
    std::fs::create_dir(&config_dir).unwrap();
    let config_path = config_dir.join("trace.toml");
    std::fs::write(
        &config_path,
        r#"
database_path = "archive/trace.db"
default_agents = ["claude", "codex"]
capture_mode = "full"
exclude = ["**/from-file/**"]
tokenizer = "jieba"
tokenizer_extension = "extensions/jieba.so"
output_format = "json"
watch_interval_seconds = 120
watch_debounce_ms = 250
"#,
    )
    .unwrap();

    let mut file_command = command(dir.path());
    file_command
        .args(["config", "--json"])
        .env("TRACEDB_CONFIG", &config_path);
    let file_config = json_output(file_command);
    assert_eq!(file_config["configPath"], config_path.display().to_string());
    assert_eq!(file_config["configFileExists"], true);
    assert_eq!(
        file_config["databasePath"],
        config_dir.join("archive/trace.db").display().to_string()
    );
    assert_eq!(
        file_config["defaultAgents"],
        serde_json::json!(["claude", "codex"])
    );
    assert_eq!(file_config["captureMode"], "full");
    assert_eq!(
        file_config["exclude"],
        serde_json::json!(["**/from-file/**"])
    );
    assert_eq!(file_config["tokenizer"], "jieba");
    assert_eq!(
        file_config["tokenizerExtension"],
        config_dir.join("extensions/jieba.so").display().to_string()
    );
    assert_eq!(file_config["outputFormat"], "json");
    assert_eq!(file_config["watchIntervalSeconds"], 120);
    assert_eq!(file_config["watchDebounceMs"], 250);

    let environment_db = dir.path().join("environment.db");
    let environment_extension = dir.path().join("environment-jieba.so");
    let cli_db = dir.path().join("cli.db");
    let mut layered_command = command(dir.path());
    layered_command
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--db",
            cli_db.to_str().unwrap(),
            "--format",
            "text",
            "--tokenizer",
            "unicode61",
            "config",
            "--json",
        ])
        .env("TRACEDB_PATH", &environment_db)
        .env("TRACEDB_AGENTS", "gemini,pi,gemini")
        .env("TRACEDB_CAPTURE_MODE", "partial")
        .env("TRACEDB_EXCLUDE", "**/from-env/**")
        .env("TRACEDB_JIEBA_EXT", &environment_extension)
        .env("TRACEDB_OUTPUT_FORMAT", "json")
        .env("TRACEDB_WATCH_INTERVAL", "60")
        .env("TRACEDB_WATCH_DEBOUNCE", "125");
    let layered = json_output(layered_command);
    assert_eq!(layered["databasePath"], cli_db.display().to_string());
    assert_eq!(
        layered["defaultAgents"],
        serde_json::json!(["gemini", "pi"])
    );
    assert_eq!(layered["captureMode"], "partial");
    assert_eq!(layered["exclude"], serde_json::json!(["**/from-env/**"]));
    assert_eq!(layered["tokenizer"], "unicode61");
    assert!(layered["tokenizerExtension"].is_null());
    assert_eq!(layered["outputFormat"], "text");
    assert_eq!(layered["watchIntervalSeconds"], 60);
    assert_eq!(layered["watchDebounceMs"], 125);
}

#[test]
fn ingest_cli_overrides_environment_and_config_defaults() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
database_path = "from-config.db"
default_agents = ["claude"]
capture_mode = "partial"
exclude = ["**/rollout-good.jsonl"]
output_format = "json"
"#,
    )
    .unwrap();
    let native = dir.path().join("native");
    std::fs::create_dir(&native).unwrap();
    std::fs::write(
        native.join("rollout-good.jsonl"),
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"good\"}}\n",
    )
    .unwrap();
    std::fs::write(
        native.join("rollout-secret.jsonl"),
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"secret\"}}\n",
    )
    .unwrap();
    let environment_db = dir.path().join("from-environment.db");
    let cli_db = dir.path().join("from-cli.db");

    let mut ingest = command(dir.path());
    ingest
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--db",
            cli_db.to_str().unwrap(),
            "ingest",
            "--agent",
            "codex",
            "--mode",
            "full",
            "--exclude",
            "**/rollout-secret.jsonl",
            "--root",
            native.to_str().unwrap(),
        ])
        .env("TRACEDB_PATH", &environment_db)
        .env("TRACEDB_AGENTS", "gemini")
        .env("TRACEDB_CAPTURE_MODE", "partial")
        .env("TRACEDB_EXCLUDE", "**/rollout-good.jsonl");
    let report = json_output(ingest);
    assert_eq!(report["agents"][0]["agent"], "codex");
    assert_eq!(report["agents"][0]["discovered"], 2);
    assert_eq!(report["agents"][0]["ingested"], 1);
    assert_eq!(report["agents"][0]["skipped"], 1);
    assert!(cli_db.exists());
    assert!(!environment_db.exists());
    assert!(!dir.path().join("from-config.db").exists());

    let db = TraceDb::open_read_only(&cli_db).unwrap();
    assert_eq!(
        db.show("codex:good").unwrap().unwrap().mode.to_string(),
        "full"
    );
    assert!(db.show("codex:secret").unwrap().is_none());
}

#[test]
fn configured_output_format_applies_without_json_flag() {
    let dir = tempdir().unwrap();
    let database = dir.path().join("trace.db");
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "database_path = {:?}\noutput_format = \"json\"\n",
            database.display().to_string()
        ),
    )
    .unwrap();

    let mut stats_json = command(dir.path());
    stats_json.args(["--config", config_path.to_str().unwrap(), "stats"]);
    let stats = json_output(stats_json);
    assert_eq!(stats["totalSessions"], 0);

    let output = command(dir.path())
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--format",
            "text",
            "stats",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("db: "));
}

#[test]
fn tokenizer_extension_implies_jieba_and_unicode_override_clears_it() {
    let dir = tempdir().unwrap();
    let extension = dir.path().join("jieba.so");

    let mut implied = command(dir.path());
    implied
        .args(["config", "--json"])
        .env("TRACEDB_JIEBA_EXT", &extension);
    let implied_config = json_output(implied);
    assert_eq!(implied_config["tokenizer"], "jieba");
    assert_eq!(
        implied_config["tokenizerExtension"],
        extension.display().to_string()
    );

    let mut overridden = command(dir.path());
    overridden
        .args(["--tokenizer", "unicode61", "config", "--json"])
        .env("TRACEDB_JIEBA_EXT", &extension);
    let overridden_config = json_output(overridden);
    assert_eq!(overridden_config["tokenizer"], "unicode61");
    assert!(overridden_config["tokenizerExtension"].is_null());
}

#[test]
fn redact_patterns_support_file_environment_and_cli_precedence() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "redact_patterns = [\"file-secret\"]\n").unwrap();

    let from_file = TraceDbConfig::load(ConfigOverrides {
        config_path: Some(config_path.clone()),
        ..ConfigOverrides::default()
    })
    .unwrap();
    assert_eq!(from_file.redact_patterns, vec!["file-secret"]);

    let from_environment = command(dir.path())
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "config",
            "--json",
        ])
        .env("TRACEDB_REDACT_PATTERNS", "env-secret;env-path")
        .output()
        .unwrap();
    let environment_config: Value = serde_json::from_slice(&from_environment.stdout).unwrap();
    assert_eq!(
        environment_config["redactPatterns"],
        serde_json::json!(["env-secret", "env-path"])
    );

    let from_cli = TraceDbConfig::load(ConfigOverrides {
        config_path: Some(config_path),
        redact_patterns: Some(vec!["cli-secret".into()]),
        ..ConfigOverrides::default()
    })
    .unwrap();
    assert_eq!(from_cli.redact_patterns, vec!["cli-secret"]);
}

#[test]
fn config_is_strict_and_missing_default_file_is_normal() {
    let dir = tempdir().unwrap();
    let default_config = dir.path().join("xdg-config/trace-db/config.toml");
    let mut defaults = command(dir.path());
    defaults.args(["config", "--json"]);
    let resolved = json_output(defaults);
    assert_eq!(resolved["configPath"], default_config.display().to_string());
    assert_eq!(resolved["configFileExists"], false);
    assert!(!default_config.exists());

    let cases = [
        ("unknown = true\n", "unknown field"),
        ("default_agents = []\n", "at least one agent"),
        ("default_agents = [\"future\"]\n", "unknown variant"),
        ("exclude = [\"[unterminated\"]\n", "invalid exclude pattern"),
        (
            "redact_patterns = [\"[unterminated\"]\n",
            "invalid redact pattern",
        ),
        ("tokenizer = \"jieba\"\n", "requires tokenizer_extension"),
        ("watch_interval_seconds = 0\n", "greater than zero"),
        ("watch_debounce_ms = 0\n", "greater than zero"),
    ];
    for (index, (contents, expected)) in cases.into_iter().enumerate() {
        let path = dir.path().join(format!("invalid-{index}.toml"));
        std::fs::write(&path, contents).unwrap();
        let output = command(dir.path())
            .args(["--config", path.to_str().unwrap(), "config", "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "config should fail: {contents}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "expected {expected:?} for {contents:?}, got {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let missing = dir.path().join("missing.toml");
    let output = command(dir.path())
        .args(["--config", missing.to_str().unwrap(), "config"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not exist"));
}
