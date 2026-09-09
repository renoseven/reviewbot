//! The shape of `reviewbot.toml`. Parsing only; every rule that spans two
//! tables lives in `Config::validate`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Everything the TOML file can say. Unknown fields are rejected everywhere:
/// a misspelled key is a configuration error, not a silent default.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Logging controls diagnostics, not what the review concludes.
    // `Config` is serialized only for the run fingerprint. Keep this
    // operational setting out so changing it still re-enters the same run.
    #[serde(default, skip_serializing)]
    pub log: LogSettings,
    #[serde(default)]
    pub review: ReviewSettings,
    #[serde(default)]
    pub triage: TriageSettings,
    #[serde(default)]
    pub security: SecuritySettings,
    /// Singular section names: one `[[provider]]` block declares one
    /// provider, which is how TOML arrays of tables read. The Rust fields
    /// stay plural because they hold many.
    #[serde(default, rename = "provider")]
    pub providers: Vec<Provider>,
    #[serde(default, rename = "model")]
    pub models: Vec<Model>,
    #[serde(default, rename = "platform")]
    pub platforms: Vec<PlatformEntry>,
    #[serde(default, rename = "tool")]
    pub tools: Vec<ToolEntry>,
}

/// Diagnostics written beside the run's other artifacts.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogSettings {
    #[serde(default)]
    pub level: LogLevel,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
        }
    }
}

/// The least severe diagnostic retained in the run log.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

/// The tool loop's shape. Every number here is required: what the right value
/// is depends on the model's context window and on how big the repository is,
/// and neither is something this code can see. A builtin default would be a
/// guess made on the config author's behalf, silently. `Config::validate`
/// refuses a zero, which is also what an omitted field parses to.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSettings {
    /// Rounds, not calls: one round carries however many `function_call`s the
    /// model emitted together, so a dozen rounds is far more than a dozen
    /// reads. Costs context: `triage` reserves this times
    /// `max_tool_output_bytes` out of the window.
    ///
    #[serde(default)]
    pub max_tool_rounds: u32,
    /// How many paths one listing may show before it says how many remain.
    #[serde(default)]
    pub max_files_per_listing: u32,
    /// How many hits one search may show.
    #[serde(default)]
    pub max_hits_per_search: u32,
    /// The most of one file a read-a-file tool may fetch. It bounds the
    /// fetch rather than the answer: the platform API has no range request,
    /// so reading part of a file means downloading all of it, and a file
    /// past this cannot be read at all, in whole or in part.
    #[serde(default)]
    pub max_file_bytes: u64,
    /// What one tool answer may hand back, and what all the tool output of
    /// one round may add up to. Paired with `max_tool_rounds`: the two
    /// multiplied together are held back from the context window.
    #[serde(default)]
    pub max_tool_output_bytes: u64,
}

#[cfg(test)]
impl ReviewSettings {
    /// None of these have defaults, so every fixture that runs the loop or
    /// reads a file needs them. Kept in one place rather than per test.
    pub fn for_tests() -> Self {
        Self {
            max_tool_rounds: 12,
            max_files_per_listing: 200,
            max_hits_per_search: 50,
            max_file_bytes: 262_144,
            max_tool_output_bytes: 32_768,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TriageSettings {
    /// The working size of one review turn. Clamped down to whatever the
    /// window still has room for, never up.
    #[serde(default)]
    pub max_chunk_tokens: u32,
    #[serde(default)]
    pub skip_paths: Vec<String>,
    /// Not a number and not a guess: skipping generated files is the only
    /// reading of "generated" worth having, so this one keeps its default.
    #[serde(default = "default_true")]
    pub skip_generated: bool,
    #[serde(default)]
    pub skip_files_over_bytes: u64,
}

impl Default for TriageSettings {
    fn default() -> Self {
        Self {
            max_chunk_tokens: 0,
            skip_paths: Vec::new(),
            skip_generated: default_true(),
            skip_files_over_bytes: 0,
        }
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
impl TriageSettings {
    /// The numbers have no defaults, so a fixture that filters files needs
    /// them spelled out. Kept in one place rather than per test.
    pub fn for_tests() -> Self {
        Self {
            max_chunk_tokens: 24_000,
            skip_paths: Vec::new(),
            skip_generated: true,
            skip_files_over_bytes: 262_144,
        }
    }
}

/// The hard boundary. No command line flag may override any of this; the
/// config may only append to `deny_paths`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecuritySettings {
    #[serde(default)]
    pub deny_paths: Vec<String>,
    #[serde(default)]
    pub follow_symlinks: bool,
    /// The whitelist of extensions the read-a-file tools may touch. There is
    /// no builtin list: which extensions are safe depends on the repository,
    /// and a default would decide that for the config author silently.
    /// `Config::validate` refuses an empty one.
    #[serde(default)]
    pub allow_extensions: Vec<String>,
    #[serde(default)]
    pub allow_build_tools: bool,
}

#[cfg(test)]
impl SecuritySettings {
    /// `allow_extensions` has no default, so every fixture that reads a file
    /// needs one. Kept in one place rather than spelled out per test.
    pub fn for_tests() -> Self {
        Self {
            deny_paths: Vec::new(),
            follow_symlinks: false,
            allow_extensions: ["rs", "toml", "md", "c", "h", "py"]
                .iter()
                .map(|extension| extension.to_string())
                .collect(),
            allow_build_tools: false,
        }
    }
}

/// A vendor account: which wire protocol it speaks, where it lives, how the
/// key is fetched, and the money it may spend.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub name: String,
    pub protocol: String,
    pub base_url: String,
    pub api_key: String,
    pub currency: String,
    /// The ceiling for one run, not a monthly allowance: `-1` unlimited,
    /// `0` spend nothing, positive is the cap. Required.
    pub budget_per_run: f64,
}

/// A model entry: the real vendor model name plus its prices and limits.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub name: String,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub default: bool,
    pub provider: String,
    pub input_per_1m_tokens: f64,
    #[serde(default)]
    pub cached_input_per_1m_tokens: Option<f64>,
    pub output_per_1m_tokens: f64,
    pub context_window_tokens: u32,
    pub max_output_tokens: u32,
    /// Sent as `reasoning.effort`. Omit to leave the vendor default.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

impl Model {
    /// True when `--model` naming this entry should select it. `name` and
    /// `alias` share one namespace.
    pub fn answers_to(&self, requested: &str) -> bool {
        self.name == requested || self.alias.as_deref() == Some(requested)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlatformKind {
    Gitlab,
    Github,
}

impl PlatformKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PlatformKind::Gitlab => "gitlab",
            PlatformKind::Github => "github",
        }
    }
}

/// A code hosting instance, keyed by `host`. No `name`: nothing cross
/// references it, the input URL's host matches it on the spot.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformEntry {
    #[serde(default)]
    pub kind: Option<PlatformKind>,
    pub host: String,
    pub base_url: String,
    pub api_token: String,
}

impl PlatformEntry {
    /// Explicit `kind` wins; otherwise only the two builtin hosts resolve.
    /// Never inferred from the shape of `base_url`.
    pub fn resolved_kind(&self) -> Option<PlatformKind> {
        self.kind.or_else(|| builtin_kind(&self.host))
    }
}

/// The whole builtin host table: two rows, written down in code.
pub fn builtin_kind(host: &str) -> Option<PlatformKind> {
    match host {
        "gitlab.com" => Some(PlatformKind::Gitlab),
        "github.com" => Some(PlatformKind::Github),
        _ => None,
    }
}

/// An external command exposed to the model. Builtin tools never appear here.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolEntry {
    pub name: String,
    pub description: String,
    pub bin: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub params: BTreeMap<String, ParamSpec>,
    /// Whether this checker needs a whole checkout on disk. The run always
    /// has a worktree, so "does it need one" says nothing; what a compiler,
    /// a history walk or a cross-file analysis needs is the whole project,
    /// and a worktree holding the files fetched so far is not that.
    #[serde(default)]
    pub requires_checkout: bool,
    #[serde(default)]
    pub requires_build: bool,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    60_000
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParamSpec {
    #[serde(rename = "type")]
    pub kind: ParamKind,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default, rename = "enum")]
    pub choices: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamKind {
    Path,
    String,
    Integer,
    Number,
    Boolean,
}
