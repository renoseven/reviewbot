use std::path::PathBuf;

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
    #[error("cannot serialize the config for a fingerprint: {source}")]
    Fingerprint { source: serde_json::Error },
}
