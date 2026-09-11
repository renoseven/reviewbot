//! Parse and validate `reviewbot.toml`, resolve the
//! model -> provider -> protocol chain, compute the fingerprint, and say
//! where credentials come from.
//!
//! Nothing here reaches the network. Credential *values* live in `common`;
//! this module only stores the pointer the TOML named.

pub mod error;
pub mod file;
pub mod fingerprint;
pub mod paths;
pub mod settings;

pub use crate::common::Backoff;
pub use error::ConfigError;
pub use file::{
    Config, LogLevel, LogSettings, Model, ParamKind, ParamSpec, PlanSettings, PlatformEntry,
    PlatformKind, Provider, ReviewSettings, SecuritySettings, ToolEntry,
};
pub use fingerprint::Fingerprint;
pub use settings::{
    EXAMPLE_CONFIG, KNOWN_PROTOCOLS, KNOWN_REASONING_EFFORTS, PROMPT_SKELETON_TOKENS,
    RESERVED_TOOL_NAMES, RunOptions, Selection, SelectionReason, Settings, init_config, log_level,
    resolve_config_path,
};
