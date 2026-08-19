use crate::model::{Agent, IngestMode};
use anyhow::{bail, Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    env, fmt,
    path::{Path, PathBuf},
    str::FromStr,
};

pub const DEFAULT_WATCH_INTERVAL_SECONDS: u64 = 300;
pub const DEFAULT_WATCH_DEBOUNCE_MS: u64 = 1_000;

/// Fully resolved TraceDB configuration after applying all precedence layers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceDbConfig {
    pub config_path: PathBuf,
    pub config_file_exists: bool,
    pub database_path: PathBuf,
    pub default_agents: Vec<Agent>,
    pub capture_mode: IngestMode,
    pub exclude: Vec<String>,
    pub tokenizer: TokenizerKind,
    pub tokenizer_extension: Option<PathBuf>,
    pub output_format: OutputFormat,
    pub watch_interval_seconds: u64,
    pub watch_debounce_ms: u64,
}

/// Highest-precedence values supplied by an embedding or CLI.
#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    pub config_path: Option<PathBuf>,
    pub database_path: Option<PathBuf>,
    pub default_agents: Option<Vec<Agent>>,
    pub capture_mode: Option<IngestMode>,
    pub exclude: Option<Vec<String>>,
    pub tokenizer: Option<TokenizerKind>,
    pub tokenizer_extension: Option<PathBuf>,
    pub output_format: Option<OutputFormat>,
    pub watch_interval_seconds: Option<u64>,
    pub watch_debounce_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenizerKind {
    #[default]
    Unicode61,
    Jieba,
}

impl fmt::Display for TokenizerKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unicode61 => "unicode61",
            Self::Jieba => "jieba",
        })
    }
}

impl FromStr for TokenizerKind {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "unicode61" => Ok(Self::Unicode61),
            "jieba" => Ok(Self::Jieba),
            _ => Err(format!("unknown tokenizer: {value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Text => "text",
            Self::Json => "json",
        })
    }
}

impl FromStr for OutputFormat {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            _ => Err(format!("unknown output format: {value}")),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    database_path: Option<PathBuf>,
    default_agents: Option<Vec<Agent>>,
    capture_mode: Option<IngestMode>,
    exclude: Option<Vec<String>>,
    tokenizer: Option<TokenizerKind>,
    tokenizer_extension: Option<PathBuf>,
    output_format: Option<OutputFormat>,
    watch_interval_seconds: Option<u64>,
    watch_debounce_ms: Option<u64>,
}

impl TraceDbConfig {
    /// Load a strict TOML configuration and resolve CLI > environment > file > defaults.
    pub fn load(overrides: ConfigOverrides) -> Result<Self> {
        let env_config_path = env_path("TRACEDB_CONFIG");
        let explicit_config = overrides.config_path.is_some() || env_config_path.is_some();
        let config_path = overrides
            .config_path
            .clone()
            .or(env_config_path)
            .unwrap_or_else(default_config_path);
        let config_file_exists = config_path
            .try_exists()
            .with_context(|| format!("inspect TraceDB config {}", config_path.display()))?;
        if explicit_config && !config_file_exists {
            bail!(
                "TraceDB config file does not exist: {}",
                config_path.display()
            );
        }
        let file = if config_file_exists {
            let contents = std::fs::read_to_string(&config_path)
                .with_context(|| format!("read TraceDB config {}", config_path.display()))?;
            toml::from_str::<ConfigFile>(&contents)
                .with_context(|| format!("parse TraceDB config {}", config_path.display()))?
        } else {
            ConfigFile::default()
        };
        let config_base = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let mut resolved = Self {
            config_path,
            config_file_exists,
            database_path: default_database_path(),
            default_agents: Agent::ALL.to_vec(),
            capture_mode: IngestMode::Partial,
            exclude: Vec::new(),
            tokenizer: TokenizerKind::Unicode61,
            tokenizer_extension: None,
            output_format: OutputFormat::Text,
            watch_interval_seconds: DEFAULT_WATCH_INTERVAL_SECONDS,
            watch_debounce_ms: DEFAULT_WATCH_DEBOUNCE_MS,
        };
        resolved.apply_file(file, &config_base);
        resolved.apply_environment()?;
        resolved.apply_overrides(overrides);
        resolved.validate()?;
        Ok(resolved)
    }

    fn apply_file(&mut self, file: ConfigFile, base: &Path) {
        if let Some(path) = file.database_path {
            self.database_path = resolve_file_path(base, path);
        }
        if let Some(agents) = file.default_agents {
            self.default_agents = agents;
        }
        if let Some(mode) = file.capture_mode {
            self.capture_mode = mode;
        }
        if let Some(exclude) = file.exclude {
            self.exclude = exclude;
        }
        apply_tokenizer_layer(
            &mut self.tokenizer,
            &mut self.tokenizer_extension,
            file.tokenizer,
            file.tokenizer_extension
                .map(|path| resolve_file_path(base, path)),
        );
        if let Some(format) = file.output_format {
            self.output_format = format;
        }
        if let Some(seconds) = file.watch_interval_seconds {
            self.watch_interval_seconds = seconds;
        }
        if let Some(milliseconds) = file.watch_debounce_ms {
            self.watch_debounce_ms = milliseconds;
        }
    }

    fn apply_environment(&mut self) -> Result<()> {
        if let Some(path) = env_path("TRACEDB_PATH") {
            self.database_path = path;
        }
        if let Some(value) = env_string("TRACEDB_AGENTS")? {
            self.default_agents = parse_list(&value, "TRACEDB_AGENTS")?;
        }
        if let Some(value) = env_string("TRACEDB_CAPTURE_MODE")? {
            self.capture_mode = value.parse().map_err(anyhow::Error::msg)?;
        }
        if let Some(value) = env_string("TRACEDB_EXCLUDE")? {
            self.exclude = split_csv(&value);
        }
        let tokenizer = env_string("TRACEDB_TOKENIZER")?
            .map(|value| value.parse().map_err(anyhow::Error::msg))
            .transpose()?;
        let tokenizer_extension = env_path("TRACEDB_JIEBA_EXT");
        apply_tokenizer_layer(
            &mut self.tokenizer,
            &mut self.tokenizer_extension,
            tokenizer,
            tokenizer_extension,
        );
        if let Some(value) = env_string("TRACEDB_OUTPUT_FORMAT")? {
            self.output_format = value.parse().map_err(anyhow::Error::msg)?;
        }
        if let Some(value) = env_string("TRACEDB_WATCH_INTERVAL")? {
            self.watch_interval_seconds = parse_u64(&value, "TRACEDB_WATCH_INTERVAL")?;
        }
        if let Some(value) = env_string("TRACEDB_WATCH_DEBOUNCE")? {
            self.watch_debounce_ms = parse_u64(&value, "TRACEDB_WATCH_DEBOUNCE")?;
        }
        Ok(())
    }

    fn apply_overrides(&mut self, overrides: ConfigOverrides) {
        if let Some(path) = overrides.database_path {
            self.database_path = path;
        }
        if let Some(agents) = overrides.default_agents {
            self.default_agents = agents;
        }
        if let Some(mode) = overrides.capture_mode {
            self.capture_mode = mode;
        }
        if let Some(exclude) = overrides.exclude {
            self.exclude = exclude;
        }
        apply_tokenizer_layer(
            &mut self.tokenizer,
            &mut self.tokenizer_extension,
            overrides.tokenizer,
            overrides.tokenizer_extension,
        );
        if let Some(format) = overrides.output_format {
            self.output_format = format;
        }
        if let Some(seconds) = overrides.watch_interval_seconds {
            self.watch_interval_seconds = seconds;
        }
        if let Some(milliseconds) = overrides.watch_debounce_ms {
            self.watch_debounce_ms = milliseconds;
        }
    }

    fn validate(&mut self) -> Result<()> {
        if self.default_agents.is_empty() {
            bail!("default_agents must contain at least one agent");
        }
        let mut seen = HashSet::new();
        self.default_agents.retain(|agent| seen.insert(*agent));
        ExcludeMatcher::new(&self.exclude)?;
        match self.tokenizer {
            TokenizerKind::Unicode61 if self.tokenizer_extension.is_some() => {
                bail!("tokenizer_extension requires tokenizer = \"jieba\"")
            }
            TokenizerKind::Jieba if self.tokenizer_extension.is_none() => {
                bail!("tokenizer = \"jieba\" requires tokenizer_extension")
            }
            _ => {}
        }
        if self.watch_interval_seconds == 0 {
            bail!("watch_interval_seconds must be greater than zero");
        }
        if self.watch_debounce_ms == 0 {
            bail!("watch_debounce_ms must be greater than zero");
        }
        Ok(())
    }
}

fn apply_tokenizer_layer(
    tokenizer: &mut TokenizerKind,
    extension: &mut Option<PathBuf>,
    layer_tokenizer: Option<TokenizerKind>,
    layer_extension: Option<PathBuf>,
) {
    if let Some(value) = layer_tokenizer {
        *tokenizer = value;
        if matches!(value, TokenizerKind::Unicode61) {
            *extension = None;
        }
    }
    if let Some(path) = layer_extension {
        if layer_tokenizer.is_none() {
            *tokenizer = TokenizerKind::Jieba;
        }
        *extension = Some(path);
    }
}

fn parse_list<T>(value: &str, name: &str) -> Result<Vec<T>>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    split_csv(value)
        .into_iter()
        .map(|item| {
            item.parse::<T>()
                .map_err(|error| anyhow::anyhow!("invalid {name} value {item:?}: {error}"))
        })
        .collect()
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_u64(value: &str, name: &str) -> Result<u64> {
    value
        .parse()
        .with_context(|| format!("{name} must be a non-negative integer"))
}

fn env_string(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => bail!("{name} is not valid Unicode"),
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn resolve_file_path(base: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

/// Return the platform configuration file searched when no override is set.
pub fn default_config_path() -> PathBuf {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("trace-db").join("config.toml")
}

pub(crate) fn default_database_path() -> PathBuf {
    let base = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("trace-db").join("trace.db")
}

pub(crate) struct ExcludeMatcher(GlobSet);

impl ExcludeMatcher {
    pub(crate) fn new(patterns: &[String]) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = GlobBuilder::new(pattern)
                .literal_separator(false)
                .backslash_escape(false)
                .build()
                .with_context(|| format!("invalid exclude pattern {pattern:?}"))?;
            builder.add(glob);
        }
        Ok(Self(builder.build()?))
    }

    pub(crate) fn matches(&self, locator: &str, path: &Path) -> bool {
        self.0.is_match(locator.replace('\\', "/"))
            || self.0.is_match(path.to_string_lossy().replace('\\', "/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_layers_clear_or_imply_extensions() {
        let mut tokenizer = TokenizerKind::Jieba;
        let mut extension = Some(PathBuf::from("jieba.so"));
        apply_tokenizer_layer(
            &mut tokenizer,
            &mut extension,
            Some(TokenizerKind::Unicode61),
            None,
        );
        assert_eq!(tokenizer, TokenizerKind::Unicode61);
        assert_eq!(extension, None);

        apply_tokenizer_layer(
            &mut tokenizer,
            &mut extension,
            None,
            Some(PathBuf::from("jieba.so")),
        );
        assert_eq!(tokenizer, TokenizerKind::Jieba);
        assert_eq!(extension, Some(PathBuf::from("jieba.so")));
    }

    #[test]
    fn exclude_matcher_normalizes_path_separators() {
        let matcher = ExcludeMatcher::new(&["**/private/**".into()]).unwrap();
        assert!(matcher.matches(
            r"C:\\sessions\\private\\rollout.jsonl",
            Path::new(r"C:\\sessions\\private\\rollout.jsonl")
        ));
    }
}
