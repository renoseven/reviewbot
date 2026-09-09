//! Parse and validate `reviewbot.toml`, resolve the
//! model -> provider -> protocol chain, compute the fingerprint, and say
//! where credentials come from.
//!
//! Nothing here reaches the network and nothing here holds a credential
//! beyond the caller's own `Secret`.

pub mod file;
pub mod fingerprint;
pub mod paths;
pub mod retry;
pub mod secret;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use globset::Glob;

pub use file::{
    Config, Model, ParamKind, ParamSpec, PlatformEntry, PlatformKind, Provider, ReviewSettings,
    SecuritySettings, ToolEntry, TriageSettings, builtin_kind,
};
pub use retry::Backoff;
pub use secret::{Secret, SecretSource};

/// Wire protocols this binary can speak. `protocol` resolves the same list;
/// it is named here so `config` never has to look up at the adapter layer.
pub const KNOWN_PROTOCOLS: [&str; 1] = ["openai"];

/// Responses API `reasoning.effort`. Omitted on the wire when the model
/// entry leaves `reasoning_effort` unset, so the vendor default applies.
pub const KNOWN_REASONING_EFFORTS: [&str; 7] =
    ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Names `tool` compiles in. A `[[tool]]` entry may not collide with them.
pub const BUILTIN_TOOL_NAMES: [&str; 10] = [
    "submit_comment",
    "submit_summary",
    "stat_repo_file",
    "read_repo_file",
    "list_repo_files",
    "search_repo",
    "stat_worktree_file",
    "read_worktree_file",
    "list_worktree_files",
    "search_worktree",
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
const BUILTIN_PLACEHOLDERS: [&str; 1] = ["worktree"];

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no config file at {path} (set --config to point somewhere else)")]
    Missing { path: PathBuf },
    #[error("cannot read config at {path}: {source}")]
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot parse config at {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("duplicate {table} name {name:?}")]
    DuplicateName { table: &'static str, name: String },
    #[error("duplicate platform host {host:?}")]
    DuplicateHost { host: String },
    #[error(
        "platform host {host:?} is not a builtin host, so it needs an explicit kind (gitlab or github)"
    )]
    UnknownHost { host: String },
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
        "model {model:?} has context_window {context_window} but needs more than max_output_tokens {max_output_tokens} plus {skeleton} tokens of prompt skeleton"
    )]
    ContextTooSmall {
        model: String,
        context_window: u32,
        max_output_tokens: u32,
        skeleton: u32,
    },
    #[error(
        "model {model:?} has context_window {context_window}, and holding {allowance} tokens back for {rounds} rounds of tool output leaves {left} for the diff; lower [review].max_tool_rounds or [review].max_tool_output_bytes"
    )]
    ToolAllowanceTooLarge {
        model: String,
        context_window: u32,
        rounds: u32,
        allowance: u32,
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
    #[error("{field} is empty; give an environment variable name or a path")]
    SecretEmpty { field: String },
    #[error("{field} looks like the credential itself; use an environment variable name or a path")]
    SecretInline { field: String },
    #[error("{field} points at {path}, which is inside the repository under review")]
    SecretInsideRepo { field: String, path: PathBuf },
    #[error("cannot read {field} from {origin}: {reason}")]
    SecretUnreadable {
        field: String,
        origin: String,
        reason: String,
    },
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
    pub out_dir: Option<PathBuf>,
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
            out_dir: None,
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

/// A validated config plus the command line that goes with it.
#[derive(Clone, Debug)]
pub struct Settings {
    pub config: Config,
    pub config_path: PathBuf,
    pub options: RunOptions,
}

impl Settings {
    /// Read and validate the file. `config_path` is the only path source;
    /// `None` means the XDG default, never the current directory.
    pub fn load(config_path: Option<&Path>, options: RunOptions) -> Result<Self, ConfigError> {
        let path = config_path
            .map(paths::absolute)
            .unwrap_or_else(paths::default_config_path);
        if !path.exists() {
            return Err(ConfigError::Missing { path });
        }
        let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Unreadable {
            path: path.clone(),
            source,
        })?;
        let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
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
            && self.config_path.starts_with(paths::absolute(worktree))
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

    pub fn fingerprint(&self) -> String {
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
        SecretSource::parse(&field, &provider.api_key)?
            .read(&field, self.options.worktree.as_deref())
    }

    /// Paths reviewbot writes to this run, which are always denied for reads.
    pub fn written_paths(&self) -> Vec<PathBuf> {
        let mut written = vec![paths::absolute(&self.options.runs_dir)];
        if let Some(out_dir) = &self.options.out_dir {
            written.push(paths::absolute(out_dir));
        }
        written
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
            if !hosts.insert(platform.host.as_str()) {
                return Err(ConfigError::DuplicateHost {
                    host: platform.host.clone(),
                });
            }
            if platform.resolved_kind().is_none() {
                return Err(ConfigError::UnknownHost {
                    host: platform.host.clone(),
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
            if provider.budget < 0.0 && provider.budget != -1.0 {
                return Err(ConfigError::InvalidBudget {
                    provider: provider.name.clone(),
                    value: provider.budget,
                });
            }
            if provider.budget == -1.0 {
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
            if model.context_window <= floor {
                return Err(ConfigError::ContextTooSmall {
                    model: model.name.clone(),
                    context_window: model.context_window,
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

    /// Every sizing number is required. None of them has an answer that holds
    /// across models and repositories — the round ceiling and the per-round
    /// output cap are bounded by the model's window, the chunk size and the
    /// read ceiling by how big this project's files get, the two listing caps
    /// by how many paths a listing is worth. A builtin default would make
    /// that call silently, and the wrong call is quiet: reviews that stop
    /// half way, or a window that has no room left for the diff.
    fn check_sizes(&self) -> Result<(), ConfigError> {
        let required = [
            (
                "[review].max_tool_rounds",
                u64::from(self.review.max_tool_rounds),
            ),
            (
                "[review].max_files_listed",
                u64::from(self.review.max_files_listed),
            ),
            (
                "[review].max_search_hits",
                u64::from(self.review.max_search_hits),
            ),
            (
                "[triage].max_chunk_tokens",
                u64::from(self.triage.max_chunk_tokens),
            ),
            ("[triage].skip_over_bytes", self.triage.skip_over_bytes),
            ("[review].max_read_bytes", self.review.max_read_bytes),
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
            if BUILTIN_TOOL_NAMES.contains(&tool.name.as_str()) {
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
                &format!("platform.{}.api_token", platform.host),
                &platform.api_token,
            )?;
        }
        Ok(())
    }

    pub fn provider(&self, name: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.name == name)
    }

    pub fn platform(&self, host: &str) -> Option<&PlatformEntry> {
        self.platforms.iter().find(|p| p.host == host)
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
max_tool_rounds = 12
max_files_listed = 200
max_search_hits = 50
max_read_bytes = 262144
max_tool_output_bytes = 32768

[triage]
max_chunk_tokens = 24000
skip_over_bytes = 262144

[security]
allow_extensions = ["rs", "toml"]

[[provider]]
name = "deepseek"
protocol = "openai"
base_url = "https://api.deepseek.com"
api_key = "DEEPSEEK_API_KEY"
currency = "CNY"
budget = 10.0

[[model]]
name = "deepseek-v4-flash"
provider = "deepseek"
input_per_1m = 2.0
output_per_1m = 3.0
context_window = 131072
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
budget = 1.0
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
input_per_1m = 4.0
output_per_1m = 12.0
context_window = 131072
max_output_tokens = 8192
"#,
        );
        assert!(matches!(
            parse(&text).validate(),
            Err(ConfigError::DuplicateName { table: "model", .. })
        ));
    }

    #[test]
    fn unknown_host_needs_an_explicit_kind_even_when_base_url_looks_like_gitlab() {
        let mut text = MINIMAL.to_string();
        text.push_str(
            r#"
[[platform]]
host = "git.example.com"
base_url = "https://git.example.com/api/v4"
api_token = "GITLAB_TOKEN"
"#,
        );
        let error = parse(&text).validate().expect_err("must fail");
        assert!(
            matches!(&error, ConfigError::UnknownHost { host } if host == "git.example.com"),
            "got {error}"
        );
    }

    #[test]
    fn builtin_hosts_resolve_without_a_kind() {
        let mut text = MINIMAL.to_string();
        text.push_str(
            r#"
[[platform]]
host = "gitlab.com"
base_url = "https://gitlab.com/api/v4"
api_token = "GITLAB_TOKEN"

[[platform]]
host = "github.com"
base_url = "https://api.github.com"
api_token = "GITHUB_TOKEN"
"#,
        );
        let config = parse(&text);
        config.validate().expect("valid");
        assert_eq!(
            config.platform("gitlab.com").unwrap().resolved_kind(),
            Some(PlatformKind::Gitlab)
        );
        assert_eq!(
            config.platform("github.com").unwrap().resolved_kind(),
            Some(PlatformKind::Github)
        );
    }

    #[test]
    fn negative_budgets_other_than_minus_one_fail() {
        let text = MINIMAL.replace("budget = 10.0", "budget = -2.0");
        assert!(matches!(
            parse(&text).validate(),
            Err(ConfigError::InvalidBudget { .. })
        ));
        let unlimited = MINIMAL.replace("budget = 10.0", "budget = -1.0");
        parse(&unlimited).validate().expect("-1 is unlimited");
        let nothing = MINIMAL.replace("budget = 10.0", "budget = 0.0");
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
            ("[review].max_tool_rounds", "max_tool_rounds = 12"),
            ("[review].max_files_listed", "max_files_listed = 200"),
            ("[review].max_search_hits", "max_search_hits = 50"),
            ("[triage].max_chunk_tokens", "max_chunk_tokens = 24000"),
            ("[triage].skip_over_bytes", "skip_over_bytes = 262144"),
            ("[review].max_read_bytes", "max_read_bytes = 262144"),
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
        let text = include_str!("../../examples/reviewbot.toml");
        parse(text)
            .validate()
            .expect("the shipped example is a complete config");
    }

    #[test]
    fn context_window_must_leave_room_for_output_and_skeleton() {
        let text = MINIMAL.replace("context_window = 131072", "context_window = 4096");
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
name = "search_repo"
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
input_per_1m = 4.0
output_per_1m = 12.0
context_window = 131072
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
