//! Parse and validate `reviewbot.toml`, resolve the
//! model -> provider -> protocol chain, compute the fingerprint, and say
//! where credentials come from.
//!
//! Nothing here reaches the network. Credential *values* live in `common`;
//! this module only stores the pointer the TOML named.

pub mod file;
pub mod fingerprint;
pub mod paths;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use globset::Glob;

use crate::common::{Secret, SecretSource};

pub use crate::common::Backoff;
pub use fingerprint::Fingerprint;

pub use file::{
    Config, LogLevel, LogSettings, Model, ParamKind, ParamSpec, PlatformEntry, PlatformKind,
    Provider, ReviewSettings, SecuritySettings, ToolEntry, TriageSettings,
};

/// The shipped example, which is also what `config init` writes. Complete
/// enough to parse and validate; credentials still have to be pointed at.
pub const EXAMPLE_CONFIG: &str = include_str!("example.toml");

/// Wire protocols this binary can speak. `protocol` resolves the same list;
/// it is named here so `config` never has to look up at the adapter layer.
pub const KNOWN_PROTOCOLS: [&str; 1] = ["openai"];

/// Responses API `reasoning.effort`. Omitted on the wire when the model
/// entry leaves `reasoning_effort` unset, so the vendor default applies.
pub const KNOWN_REASONING_EFFORTS: [&str; 7] =
    ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Names `tool` compiles in. A `[[tool]]` entry may not collide with them,
/// which makes this list a security boundary rather than documentation: a name
/// missing from it is a builtin a config entry could shadow. A test asserts it
/// equals what a fully equipped run registers, because it has drifted before.
pub const RESERVED_TOOL_NAMES: [&str; 7] = [
    "submit_comment",
    "finish_review",
    "submit_summary",
    "list_files",
    "stat_file",
    "read_file",
    "search_code",
];

/// What the startup check assumes the prompt body and the tool schemas
/// occupy. It runs before there is a registry to measure, so it has to be a
/// number; `triage` then measures the assembled instructions and the schemas
/// for real (`review::prompt_tokens`) and reserves the larger of the two.
///
/// The larger, not the measured, because this is a promise already made: the
/// startup check let the model through on the grounds that this much was
/// spent, and a later measurement coming in under it must not hand that
/// window back. Set from the shipped prompt with the builtin tools
/// registered, which measures a little over four thousand — it was 2048 while
/// nothing measured the real thing, and understated it by half.
pub const PROMPT_SKELETON_TOKENS: u32 = 4_096;

/// Placeholders `[[tool]].args` may use without declaring them in `params`.
/// One run, one worktree, so there is one path worth naming.
const BUILTIN_PLACEHOLDERS: [&str; 1] = ["worktree"];

/// Read just enough configuration to choose the startup log filter.
///
/// The real load follows during dispatch and reports any error with its path
/// and context. Startup logging therefore stays lenient and uses `info` when
/// the file is absent, unreadable, or malformed.
pub fn log_level(config_path: Option<&Path>) -> tracing::Level {
    let path = config_path
        .map(paths::expand_user)
        .unwrap_or_else(paths::default_config_path);
    let level = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<Config>(&text).ok())
        .map(|config| config.log.level)
        .unwrap_or_default();
    match level {
        LogLevel::Error => tracing::Level::ERROR,
        LogLevel::Warn => tracing::Level::WARN,
        LogLevel::Info => tracing::Level::INFO,
        LogLevel::Debug => tracing::Level::DEBUG,
        LogLevel::Trace => tracing::Level::TRACE,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no config file at {path} (set --config to point somewhere else)")]
    Missing { path: PathBuf },
    #[error("will not overwrite {path}; pass --config to choose another path")]
    AlreadyExists { path: PathBuf },
    #[error("cannot read config at {path}: {source}")]
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot write config at {path}: {source}")]
    Unwritable {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The parser's own error is boxed: it is a hundred bytes of span and
    /// message, and left inline it makes every `Result<_, Error>` in the crate
    /// wide enough for clippy to object. Nothing suppresses that lint here —
    /// there is not one suppression in this repository, on purpose — so the
    /// type is small instead.
    #[error("cannot parse config at {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("duplicate {table} name {name:?}")]
    DuplicateName { table: &'static str, name: String },
    #[error("duplicate platform host {host:?}")]
    DuplicateHost { host: String },
    #[error("platform base_url {url:?} is not gitlab.com or api.github.com")]
    UnknownPlatform { url: String },
    #[error("model {model:?} refers to provider {provider:?}, which is not configured")]
    UnknownProvider { model: String, provider: String },
    #[error("provider {provider:?} speaks protocol {protocol:?}; known protocols: {known}")]
    UnknownProtocol {
        provider: String,
        protocol: String,
        known: String,
    },
    #[error(
        "provider {provider:?} has budget {value}; only -1 (unlimited), 0 or a positive amount are allowed"
    )]
    InvalidBudget { provider: String, value: f64 },
    #[error("model {model:?} has reasoning_effort {value:?}; allowed values: {allowed}")]
    InvalidReasoningEffort {
        model: String,
        value: String,
        allowed: String,
    },
    #[error(
        "model {model:?} has context_window_tokens {context_window_tokens} but needs more than max_output_tokens {max_output_tokens} plus {skeleton} tokens of prompt skeleton"
    )]
    ContextTooSmall {
        model: String,
        context_window_tokens: u32,
        max_output_tokens: u32,
        skeleton: u32,
    },
    #[error(
        "model {model:?} has context_window_tokens {context_window_tokens}, and what it must always carry takes {reserved} of that, leaving {left} — not enough to review anything; raise the window by choosing another [[model]], or lower that model's max_output_tokens"
    )]
    WindowTooSmall {
        model: String,
        context_window_tokens: u32,
        reserved: u32,
        left: u32,
    },
    #[error(
        "{field} must be set to a value above zero; there is no builtin default because the right value depends on the model and the repository"
    )]
    MissingSize { field: &'static str },
    #[error("no models are configured")]
    NoModels,
    #[error("more than one model is marked default: {names}")]
    MultipleDefaults { names: String },
    #[error("no model selected; pass --model with one of: {options}")]
    NoModelSelected { options: String },
    #[error("unknown model {requested:?}; configured models: {options}")]
    UnknownModel { requested: String, options: String },
    #[error(
        "[security].allow_extensions is empty, so no file could be read; \
         list the extensions this repository's review may read (there is no builtin list)"
    )]
    NoAllowedExtensions,
    #[error("[security].allow_extensions has {value:?}; write extensions bare, as in \"rs\"")]
    MalformedExtension { value: String },
    #[error("invalid glob {pattern:?} in {field}: {reason}")]
    InvalidGlob {
        field: &'static str,
        pattern: String,
        reason: String,
    },
    #[error("tool {tool:?} collides with the builtin tool of the same name")]
    ToolNameCollidesWithBuiltin { tool: String },
    #[error("tool {tool:?} has bin {bin}, which must be an absolute path")]
    ToolBinNotAbsolute { tool: String, bin: PathBuf },
    #[error("tool {tool:?} parameter {param:?} is a bare string; give it a pattern or an enum")]
    ToolBareStringParam { tool: String, param: String },
    #[error("tool {tool:?} uses placeholder {{{placeholder}}} which is not declared in params")]
    ToolUndeclaredPlaceholder { tool: String, placeholder: String },
    #[error(
        "tool {tool:?} sets requires_build, which needs [security].allow_build_tools and a sandbox"
    )]
    ToolBuildNotAllowed { tool: String },
    #[error(transparent)]
    Secret(#[from] crate::common::SecretError),
}

/// The model entry chosen for this run, together with the provider it hangs
/// off. Everything downstream reads prices, currency and budget from here.
#[derive(Clone, Copy, Debug)]
pub struct Selection<'a> {
    pub model: &'a Model,
    pub provider: &'a Provider,
    pub reason: SelectionReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionReason {
    CommandLine,
    ConfigDefault,
    SoleEntry,
}

impl SelectionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SelectionReason::CommandLine => "--model",
            SelectionReason::ConfigDefault => "default = true",
            SelectionReason::SoleEntry => "the only configured model",
        }
    }
}

/// The parts of the command line that are facts about this invocation or
/// this machine rather than about the project.
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub runs_dir: PathBuf,
    pub output_dir: Option<PathBuf>,
    pub model: Option<String>,
    pub worktree: Option<PathBuf>,
    pub publish: bool,
    pub run_id: Option<String>,
    pub retries: u32,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            runs_dir: paths::default_runs_dir(),
            output_dir: None,
            model: None,
            worktree: None,
            publish: false,
            run_id: None,
            retries: 2,
        }
    }
}

impl RunOptions {
    pub fn backoff(&self) -> Backoff {
        Backoff::new(self.retries)
    }
}

/// Where `--config` points, or `~/.reviewbot/config.toml`. The current
/// directory is never consulted.
pub fn resolve_config_path(config_path: Option<&Path>) -> Result<PathBuf, ConfigError> {
    match config_path {
        Some(path) => {
            let expanded = paths::expand_user(path);
            std::path::absolute(&expanded).map_err(|source| ConfigError::Unreadable {
                path: expanded,
                source,
            })
        }
        None => Ok(paths::default_config_path()),
    }
}

/// Write the shipped example to `--config`, or to `~/.reviewbot/config.toml`.
/// Refuses to overwrite: a file that is already there is left alone.
pub fn init_config(config_path: Option<&Path>) -> Result<PathBuf, ConfigError> {
    let path = resolve_config_path(config_path)?;
    if path.exists() {
        return Err(ConfigError::AlreadyExists { path });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Unwritable {
            path: path.clone(),
            source,
        })?;
    }
    std::fs::write(&path, EXAMPLE_CONFIG).map_err(|source| ConfigError::Unwritable {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// A validated config plus the command line that goes with it.
#[derive(Clone, Debug)]
pub struct Settings {
    pub config: Config,
    pub config_path: PathBuf,
    pub options: RunOptions,
}

impl Settings {
    /// Read and validate the file. `config_path` is the only path source;
    /// `None` means `~/.reviewbot/config.toml`, never the current directory.
    pub fn load(config_path: Option<&Path>, options: RunOptions) -> Result<Self, ConfigError> {
        let path = resolve_config_path(config_path)?;
        if !path.exists() {
            return Err(ConfigError::Missing { path });
        }
        let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Unreadable {
            path: path.clone(),
            source,
        })?;
        let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source: Box::new(source),
        })?;
        let settings = Settings {
            config,
            config_path: path,
            options,
        };
        settings.config.validate()?;
        settings.warn_about_worktree_config();
        Ok(settings)
    }

    /// A config inside the worktree is a real local workflow, so this only
    /// warns. In CI nobody writes that path.
    fn warn_about_worktree_config(&self) {
        if let Some(worktree) = &self.options.worktree
            && let Ok(root) = std::path::absolute(worktree)
            && self.config_path.starts_with(root)
        {
            tracing::warn!(
                config = %self.config_path.display(),
                "config file lives inside the worktree under review"
            );
        }
    }

    pub fn selection(&self) -> Result<Selection<'_>, ConfigError> {
        self.config.select_model(self.options.model.as_deref())
    }

    pub fn fingerprint(&self) -> Fingerprint {
        fingerprint::fingerprint(
            &self.config,
            self.options.model.as_deref(),
            self.options.worktree.is_some(),
        )
    }

    /// Read the credential of the selected provider. Only the provider this
    /// run actually uses has to be readable.
    pub fn selected_api_key(&self) -> Result<Secret, ConfigError> {
        let provider = self.selection()?.provider;
        let field = format!("provider.{}.api_key", provider.name);
        Ok(SecretSource::parse(&field, &provider.api_key)?
            .read(&field, self.options.worktree.as_deref())?)
    }

    /// Paths reviewbot writes to this run, which are always denied for reads.
    pub fn written_paths(&self) -> Result<Vec<PathBuf>, ConfigError> {
        let to_abs = |path: &Path| {
            std::path::absolute(path).map_err(|source| ConfigError::Unreadable {
                path: path.to_path_buf(),
                source,
            })
        };
        let mut written = vec![to_abs(&self.options.runs_dir)?];
        if let Some(output_dir) = &self.options.output_dir {
            written.push(to_abs(output_dir)?);
        }
        Ok(written)
    }
}

impl Config {
    /// Every rule that spans two tables, in one place. Nothing here touches
    /// the network or checks whether a `bin` really exists.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.check_unique_names()?;
        self.check_platforms()?;
        self.check_providers()?;
        self.check_models()?;
        self.check_globs()?;
        self.check_extensions()?;
        self.check_sizes()?;
        self.check_tools()?;
        self.check_secret_sources()?;
        Ok(())
    }

    fn check_unique_names(&self) -> Result<(), ConfigError> {
        let mut providers = BTreeSet::new();
        for provider in &self.providers {
            if !providers.insert(provider.name.as_str()) {
                return Err(ConfigError::DuplicateName {
                    table: "provider",
                    name: provider.name.clone(),
                });
            }
        }
        // `name` and `alias` share one namespace.
        let mut models = BTreeSet::new();
        for model in &self.models {
            for label in [Some(model.name.as_str()), model.alias.as_deref()]
                .into_iter()
                .flatten()
            {
                if !models.insert(label) {
                    return Err(ConfigError::DuplicateName {
                        table: "model",
                        name: label.to_string(),
                    });
                }
            }
        }
        let mut tools = BTreeSet::new();
        for tool in &self.tools {
            if !tools.insert(tool.name.as_str()) {
                return Err(ConfigError::DuplicateName {
                    table: "tool",
                    name: tool.name.clone(),
                });
            }
        }
        Ok(())
    }

    fn check_platforms(&self) -> Result<(), ConfigError> {
        let mut hosts = BTreeSet::new();
        for platform in &self.platforms {
            let Some(host) = platform.host() else {
                return Err(ConfigError::UnknownPlatform {
                    url: platform.base_url.clone(),
                });
            };
            if !hosts.insert(host) {
                return Err(ConfigError::DuplicateHost {
                    host: host.to_string(),
                });
            }
        }
        Ok(())
    }

    fn check_providers(&self) -> Result<(), ConfigError> {
        for provider in &self.providers {
            if !KNOWN_PROTOCOLS.contains(&provider.protocol.as_str()) {
                return Err(ConfigError::UnknownProtocol {
                    provider: provider.name.clone(),
                    protocol: provider.protocol.clone(),
                    known: KNOWN_PROTOCOLS.join(", "),
                });
            }
            if provider.budget_per_run < 0.0 && provider.budget_per_run != -1.0 {
                return Err(ConfigError::InvalidBudget {
                    provider: provider.name.clone(),
                    value: provider.budget_per_run,
                });
            }
            if provider.budget_per_run == -1.0 {
                tracing::debug!(provider = %provider.name, "no budget ceiling for this run");
            }
        }
        Ok(())
    }

    fn check_models(&self) -> Result<(), ConfigError> {
        let defaults: Vec<&str> = self
            .models
            .iter()
            .filter(|m| m.default)
            .map(|m| m.name.as_str())
            .collect();
        if defaults.len() > 1 {
            return Err(ConfigError::MultipleDefaults {
                names: defaults.join(", "),
            });
        }
        for model in &self.models {
            if self.provider(&model.provider).is_none() {
                return Err(ConfigError::UnknownProvider {
                    model: model.name.clone(),
                    provider: model.provider.clone(),
                });
            }
            let floor = model
                .max_output_tokens
                .saturating_add(PROMPT_SKELETON_TOKENS);
            if model.context_window_tokens <= floor {
                return Err(ConfigError::ContextTooSmall {
                    model: model.name.clone(),
                    context_window_tokens: model.context_window_tokens,
                    max_output_tokens: model.max_output_tokens,
                    skeleton: PROMPT_SKELETON_TOKENS,
                });
            }
            if let Some(effort) = &model.reasoning_effort
                && !KNOWN_REASONING_EFFORTS.contains(&effort.as_str())
            {
                return Err(ConfigError::InvalidReasoningEffort {
                    model: model.name.clone(),
                    value: effort.clone(),
                    allowed: KNOWN_REASONING_EFFORTS.join(", "),
                });
            }
        }
        Ok(())
    }

    fn check_globs(&self) -> Result<(), ConfigError> {
        let groups: [(&'static str, &Vec<String>); 2] = [
            ("[triage].skip_paths", &self.triage.skip_paths),
            ("[security].deny_paths", &self.security.deny_paths),
        ];
        for (field, patterns) in groups {
            for pattern in patterns {
                Glob::new(pattern).map_err(|e| ConfigError::InvalidGlob {
                    field,
                    pattern: pattern.clone(),
                    reason: e.to_string(),
                })?;
            }
        }
        Ok(())
    }

    /// An empty whitelist rejects every path, so the run would look like it
    /// works right up to the first file read. Say so at startup instead.
    fn check_extensions(&self) -> Result<(), ConfigError> {
        if self.security.allow_extensions.is_empty() {
            return Err(ConfigError::NoAllowedExtensions);
        }
        for extension in &self.security.allow_extensions {
            if extension.is_empty() || extension.starts_with('.') || extension.contains('/') {
                return Err(ConfigError::MalformedExtension {
                    value: extension.clone(),
                });
            }
        }
        Ok(())
    }

    /// Every sizing number here is required, and none of them has an answer
    /// that holds across projects: the chunk size and the read ceiling depend
    /// on how big this project's files get, the listing caps on how many paths
    /// a listing is worth reading, the per-answer cap on how much diagnostic
    /// output is worth carrying. A builtin default would make those calls
    /// silently, and the wrong call is quiet.
    ///
    /// The round ceiling used to be here and is not any more: it is not a
    /// project fact but whatever the model's window can afford once the diff
    /// has its share, so it is worked out per run rather than guessed by hand
    /// (see `stage::triage::Window`).
    fn check_sizes(&self) -> Result<(), ConfigError> {
        let required = [
            (
                "[review].max_files_per_listing",
                u64::from(self.review.max_files_per_listing),
            ),
            (
                "[review].max_hits_per_search",
                u64::from(self.review.max_hits_per_search),
            ),
            (
                "[triage].max_chunk_tokens",
                u64::from(self.triage.max_chunk_tokens),
            ),
            (
                "[triage].skip_files_over_bytes",
                self.triage.skip_files_over_bytes,
            ),
            ("[review].max_file_bytes", self.review.max_file_bytes),
            (
                "[review].max_tool_output_bytes",
                self.review.max_tool_output_bytes,
            ),
        ];
        for (field, value) in required {
            if value == 0 {
                return Err(ConfigError::MissingSize { field });
            }
        }
        Ok(())
    }

    fn check_tools(&self) -> Result<(), ConfigError> {
        for tool in &self.tools {
            if RESERVED_TOOL_NAMES.contains(&tool.name.as_str()) {
                return Err(ConfigError::ToolNameCollidesWithBuiltin {
                    tool: tool.name.clone(),
                });
            }
            if !tool.bin.is_absolute() {
                return Err(ConfigError::ToolBinNotAbsolute {
                    tool: tool.name.clone(),
                    bin: tool.bin.clone(),
                });
            }
            if tool.requires_build && !self.security.allow_build_tools {
                return Err(ConfigError::ToolBuildNotAllowed {
                    tool: tool.name.clone(),
                });
            }
            for (param, spec) in &tool.params {
                let bare = spec.kind == ParamKind::String
                    && spec.pattern.is_none()
                    && spec.choices.is_none();
                if bare {
                    return Err(ConfigError::ToolBareStringParam {
                        tool: tool.name.clone(),
                        param: param.clone(),
                    });
                }
            }
            for arg in &tool.args {
                for placeholder in placeholders(arg) {
                    let declared = tool.params.contains_key(&placeholder)
                        || BUILTIN_PLACEHOLDERS.contains(&placeholder.as_str());
                    if !declared {
                        return Err(ConfigError::ToolUndeclaredPlaceholder {
                            tool: tool.name.clone(),
                            placeholder,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn check_secret_sources(&self) -> Result<(), ConfigError> {
        for provider in &self.providers {
            SecretSource::parse(
                &format!("provider.{}.api_key", provider.name),
                &provider.api_key,
            )?;
        }
        for platform in &self.platforms {
            SecretSource::parse(
                &format!(
                    "platform.{}.api_token",
                    platform.host().unwrap_or("unknown")
                ),
                &platform.api_token,
            )?;
        }
        Ok(())
    }

    pub fn provider(&self, name: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.name == name)
    }

    pub fn platform(&self, host: &str) -> Option<&PlatformEntry> {
        self.platforms.iter().find(|p| p.host() == Some(host))
    }

    /// `--model` beats `default = true` beats the sole entry. Nothing else,
    /// and no builtin fallback model.
    pub fn select_model(&self, requested: Option<&str>) -> Result<Selection<'_>, ConfigError> {
        if self.models.is_empty() {
            return Err(ConfigError::NoModels);
        }
        let (model, reason) = match requested {
            Some(name) => {
                let model = self
                    .models
                    .iter()
                    .find(|m| m.answers_to(name))
                    .ok_or_else(|| ConfigError::UnknownModel {
                        requested: name.to_string(),
                        options: self.model_options(),
                    })?;
                (model, SelectionReason::CommandLine)
            }
            None => match self.models.iter().find(|m| m.default) {
                Some(model) => (model, SelectionReason::ConfigDefault),
                None if self.models.len() == 1 => (&self.models[0], SelectionReason::SoleEntry),
                None => {
                    return Err(ConfigError::NoModelSelected {
                        options: self.model_options(),
                    });
                }
            },
        };
        let provider =
            self.provider(&model.provider)
                .ok_or_else(|| ConfigError::UnknownProvider {
                    model: model.name.clone(),
                    provider: model.provider.clone(),
                })?;
        Ok(Selection {
            model,
            provider,
            reason,
        })
    }

    /// The `model list` table, printed whenever selection fails.
    pub fn model_options(&self) -> String {
        self.models
            .iter()
            .map(|m| match &m.alias {
                Some(alias) => format!("{} (alias {alias}, provider {})", m.name, m.provider),
                None => format!("{} (provider {})", m.name, m.provider),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The `{name}` slots inside one argv template element.
fn placeholders(arg: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = arg;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                found.push(after[..close].to_string());
                rest = &after[close + 1..];
            }
            None => break,
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        toml::from_str(text).expect("valid toml")
    }

    const MINIMAL: &str = r#"
[review]
max_files_per_listing = 200
max_hits_per_search = 50
max_file_bytes = 262144
max_tool_output_bytes = 32768

[triage]
max_chunk_tokens = 24000
skip_files_over_bytes = 262144

[security]
allow_extensions = ["rs", "toml"]

[[provider]]
name = "deepseek"
protocol = "openai"
base_url = "https://api.deepseek.com"
api_key = "DEEPSEEK_API_KEY"
currency = "CNY"
budget_per_run = 10.0

[[model]]
name = "deepseek-v4-flash"
provider = "deepseek"
input_per_1m_tokens = 2.0
output_per_1m_tokens = 3.0
context_window_tokens = 131072
max_output_tokens = 4096
"#;

    #[test]
    fn minimal_config_validates_and_selects_the_sole_model() {
        let config = parse(MINIMAL);
        config.validate().expect("valid");
        let selection = config.select_model(None).expect("selected");
        assert_eq!(selection.model.name, "deepseek-v4-flash");
        assert_eq!(selection.reason, SelectionReason::SoleEntry);
    }

    #[test]
    fn log_level_is_read_leniently_before_the_real_config_load() {
        let directory = tempfile::tempdir().expect("temp dir");
        let config = directory.path().join("reviewbot.toml");
        std::fs::write(&config, "[log]\nlevel = \"debug\"\n").expect("config");
        assert_eq!(log_level(Some(&config)), tracing::Level::DEBUG);

        let missing = directory.path().join("missing.toml");
        assert_eq!(log_level(Some(&missing)), tracing::Level::INFO);
        std::fs::write(&config, "not toml = [").expect("malformed config");
        assert_eq!(log_level(Some(&config)), tracing::Level::INFO);
    }

    #[test]
    fn log_level_does_not_change_the_config_fingerprint() {
        let mut info = parse(MINIMAL);
        info.log.level = LogLevel::Info;
        let mut trace = info.clone();
        trace.log.level = LogLevel::Trace;

        assert_eq!(
            fingerprint::fingerprint(&info, None, false),
            fingerprint::fingerprint(&trace, None, false)
        );
    }

    #[test]
    fn duplicate_provider_names_fail() {
        let mut text = MINIMAL.to_string();
        text.push_str(
            r#"
[[provider]]
name = "deepseek"
protocol = "openai"
base_url = "https://other.example.com"
api_key = "OTHER_KEY"
currency = "USD"
budget_per_run = 1.0
"#,
        );
        assert!(matches!(
            parse(&text).validate(),
            Err(ConfigError::DuplicateName {
                table: "provider",
                ..
            })
        ));
    }

    #[test]
    fn model_alias_shares_the_namespace_with_names() {
        let mut text = MINIMAL.to_string();
        text.push_str(
            r#"
[[model]]
name = "deepseek-v4-pro"
alias = "deepseek-v4-flash"
provider = "deepseek"
input_per_1m_tokens = 4.0
output_per_1m_tokens = 12.0
context_window_tokens = 131072
max_output_tokens = 8192
"#,
        );
        assert!(matches!(
            parse(&text).validate(),
            Err(ConfigError::DuplicateName { table: "model", .. })
        ));
    }

    #[test]
    fn an_unknown_api_is_refused_even_when_base_url_looks_like_gitlab() {
        let mut text = MINIMAL.to_string();
        text.push_str(
            r#"
[[platform]]
base_url = "https://git.example.com/api/v4"
api_token = "GITLAB_TOKEN"
"#,
        );
        let error = parse(&text).validate().expect_err("must fail");
        assert!(
            matches!(&error, ConfigError::UnknownPlatform { url } if url == "https://git.example.com/api/v4"),
            "got {error}"
        );
    }

    #[test]
    fn a_kind_field_is_unknown() {
        let mut text = MINIMAL.to_string();
        text.push_str(
            r#"
[[platform]]
kind = "gitlab"
base_url = "https://gitlab.com/api/v4"
api_token = "GITLAB_TOKEN"
"#,
        );
        assert!(
            toml::from_str::<Config>(&text).is_err(),
            "kind is not a config field"
        );
    }

    #[test]
    fn a_host_field_is_unknown() {
        let mut text = MINIMAL.to_string();
        text.push_str(
            r#"
[[platform]]
host = "gitlab.com"
base_url = "https://gitlab.com/api/v4"
api_token = "GITLAB_TOKEN"
"#,
        );
        assert!(
            toml::from_str::<Config>(&text).is_err(),
            "host is not a config field"
        );
    }

    #[test]
    fn known_apis_resolve() {
        let mut text = MINIMAL.to_string();
        text.push_str(
            r#"
[[platform]]
base_url = "https://gitlab.com/api/v4"
api_token = "GITLAB_TOKEN"

[[platform]]
base_url = "https://api.github.com"
api_token = "GITHUB_TOKEN"
"#,
        );
        let config = parse(&text);
        config.validate().expect("valid");
        assert_eq!(
            config.platform("gitlab.com").unwrap().kind(),
            Some(PlatformKind::Gitlab)
        );
        assert_eq!(
            config.platform("github.com").unwrap().kind(),
            Some(PlatformKind::Github)
        );
    }

    #[test]
    fn negative_budgets_other_than_minus_one_fail() {
        let text = MINIMAL.replace("budget_per_run = 10.0", "budget_per_run = -2.0");
        assert!(matches!(
            parse(&text).validate(),
            Err(ConfigError::InvalidBudget { .. })
        ));
        let unlimited = MINIMAL.replace("budget_per_run = 10.0", "budget_per_run = -1.0");
        parse(&unlimited).validate().expect("-1 is unlimited");
        let nothing = MINIMAL.replace("budget_per_run = 10.0", "budget_per_run = 0.0");
        parse(&nothing).validate().expect("0 spends nothing");
    }

    /// There is no builtin extension list, and an empty one rejects every
    /// path, so the run would look healthy until the first file read.
    #[test]
    fn an_absent_allow_extensions_is_a_config_error_rather_than_a_default() {
        let text = MINIMAL.replace(r#"allow_extensions = ["rs", "toml"]"#, "");
        assert!(matches!(
            parse(&text).validate(),
            Err(ConfigError::NoAllowedExtensions)
        ));
        let empty = MINIMAL.replace(r#"["rs", "toml"]"#, "[]");
        assert!(matches!(
            parse(&empty).validate(),
            Err(ConfigError::NoAllowedExtensions)
        ));
    }

    #[test]
    fn an_extension_is_written_bare_rather_than_with_a_dot() {
        let dotted = MINIMAL.replace(r#""rs""#, r#"".rs""#);
        assert!(matches!(
            parse(&dotted).validate(),
            Err(ConfigError::MalformedExtension { value }) if value == ".rs"
        ));
    }

    /// Every sizing number is required for the same reason the extension list
    /// is: the code cannot pick one that holds across models and repositories,
    /// and picking silently is how a run ends up half reviewed or with no room
    /// left for the diff. Omitting the field and writing a zero are the same
    /// mistake and get the same answer.
    #[test]
    fn every_sizing_number_is_required_rather_than_defaulted() {
        for (field, line) in [
            (
                "[review].max_files_per_listing",
                "max_files_per_listing = 200",
            ),
            ("[review].max_hits_per_search", "max_hits_per_search = 50"),
            ("[triage].max_chunk_tokens", "max_chunk_tokens = 24000"),
            (
                "[triage].skip_files_over_bytes",
                "skip_files_over_bytes = 262144",
            ),
            ("[review].max_file_bytes", "max_file_bytes = 262144"),
            (
                "[review].max_tool_output_bytes",
                "max_tool_output_bytes = 32768",
            ),
        ] {
            assert!(MINIMAL.contains(line), "the fixture stopped setting {line}");
            let absent = MINIMAL.replace(line, "");
            assert!(
                matches!(parse(&absent).validate(), Err(ConfigError::MissingSize { field: named }) if named == field),
                "omitting {line} should name {field}"
            );
            let name = line.split(' ').next().expect("a field name");
            let zero = MINIMAL.replace(line, &format!("{name} = 0"));
            assert!(
                matches!(parse(&zero).validate(), Err(ConfigError::MissingSize { field: named }) if named == field),
                "a zero {name} should name {field}"
            );
        }
    }

    /// The example config is the only documentation of these numbers that a
    /// reader will copy, so it has to keep parsing and validating as fields
    /// come and go. It has no builtin defaults to fall back on.
    #[test]
    fn the_example_config_sets_everything_that_is_required() {
        parse(EXAMPLE_CONFIG)
            .validate()
            .expect("the shipped example is a complete config");
        assert!(
            !EXAMPLE_CONFIG
                .lines()
                .any(|line| line.starts_with("[[tool]]")),
            "external checkers stay commented; listed means enabled"
        );
    }

    #[test]
    fn init_writes_the_example_and_refuses_to_overwrite() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("nested").join("config.toml");
        let written = init_config(Some(&path)).expect("first write");
        assert_eq!(written, std::path::absolute(&path).expect("abs"));
        assert_eq!(
            std::fs::read_to_string(&written).expect("read"),
            EXAMPLE_CONFIG
        );
        let refused = init_config(Some(&path)).expect_err("already there");
        assert!(
            matches!(refused, ConfigError::AlreadyExists { .. }),
            "{refused}"
        );
        assert_eq!(
            refused.to_string(),
            format!(
                "will not overwrite {}; pass --config to choose another path",
                std::path::absolute(&path).expect("abs").display()
            )
        );
    }

    #[test]
    fn context_window_must_leave_room_for_output_and_skeleton() {
        let text = MINIMAL.replace(
            "context_window_tokens = 131072",
            "context_window_tokens = 4096",
        );
        assert!(matches!(
            parse(&text).validate(),
            Err(ConfigError::ContextTooSmall { .. })
        ));
    }

    #[test]
    fn reasoning_effort_must_be_a_known_value() {
        let ok = MINIMAL.to_string() + "reasoning_effort = \"low\"\n";
        parse(&ok).validate().expect("low is allowed");
        let unset = parse(MINIMAL);
        unset
            .validate()
            .expect("omitting effort is the vendor default");
        assert!(unset.models[0].reasoning_effort.is_none());
        let bad = MINIMAL.to_string() + "reasoning_effort = \"turbo\"\n";
        assert!(matches!(
            parse(&bad).validate(),
            Err(ConfigError::InvalidReasoningEffort { .. })
        ));
    }

    #[test]
    fn tool_entries_reject_unknown_fields_and_bare_strings() {
        let mut text = MINIMAL.to_string();
        text.push_str(
            r#"
[[tool]]
name = "cppcheck"
description = "static analysis for C and C++"
bin = "/usr/bin/cppcheck"
args = ["--", "{path}"]
params.path = { type = "path" }
schedule = "always"
"#,
        );
        assert!(
            toml::from_str::<Config>(&text).is_err(),
            "schedule is not a field"
        );

        let mut bare = MINIMAL.to_string();
        bare.push_str(
            r#"
[[tool]]
name = "cppcheck"
description = "static analysis for C and C++"
bin = "/usr/bin/cppcheck"
args = ["{flag}"]
params.flag = { type = "string" }
"#,
        );
        assert!(matches!(
            parse(&bare).validate(),
            Err(ConfigError::ToolBareStringParam { .. })
        ));
    }

    #[test]
    fn tool_names_may_not_collide_with_builtins() {
        let mut text = MINIMAL.to_string();
        text.push_str(
            r#"
[[tool]]
name = "search_code"
description = "shadows the builtin"
bin = "/usr/bin/rg"
args = ["{query}"]
params.query = { type = "string", pattern = ".*" }
"#,
        );
        assert!(matches!(
            parse(&text).validate(),
            Err(ConfigError::ToolNameCollidesWithBuiltin { .. })
        ));
    }

    #[test]
    fn selection_needs_exactly_one_default_when_several_models_exist() {
        let mut text = MINIMAL.to_string();
        text.push_str(
            r#"
[[model]]
name = "deepseek-v4-pro"
provider = "deepseek"
input_per_1m_tokens = 4.0
output_per_1m_tokens = 12.0
context_window_tokens = 131072
max_output_tokens = 8192
"#,
        );
        let config = parse(&text);
        config.validate().expect("valid");
        let error = config.select_model(None).expect_err("ambiguous");
        assert!(matches!(error, ConfigError::NoModelSelected { .. }));
        let selected = config.select_model(Some("deepseek-v4-pro")).expect("named");
        assert_eq!(selected.reason, SelectionReason::CommandLine);
    }

    #[test]
    fn placeholders_are_extracted_per_argv_element() {
        assert_eq!(placeholders("{path}"), vec!["path".to_string()]);
        assert_eq!(placeholders("--file={path}"), vec!["path".to_string()]);
        assert!(placeholders("--quiet").is_empty());
    }
}
