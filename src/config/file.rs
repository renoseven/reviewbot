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
    // Named in no fingerprint slice (`config::fingerprint`), so asking for
    // debug logging re-enters the same run with every checkpoint intact.
    // Skipped on the wire as well, so no serialized `Config` carries it.
    #[serde(default, skip_serializing)]
    pub log: LogSettings,
    #[serde(default)]
    pub review: ReviewSettings,
    #[serde(default)]
    pub plan: PlanSettings,
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

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

/// The tool loop's shape. Every number here is required: what the right value
/// is depends on the model's context window and on how big the repository is,
/// and neither is something this code can see. A builtin default would be a
/// guess made on the config author's behalf, silently. `Config::validate`
/// refuses a zero, which is also what an omitted field parses to.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSettings {
    /// How many paths one listing may show before it says how many remain.
    #[serde(default)]
    pub max_files_per_listing: u32,
    /// How many hits one search may show.
    #[serde(default)]
    pub max_hits_per_search: u32,
    /// How many paths one `fetch_repo_file` call may pull. A longer list is
    /// refused as a whole, before any request is issued.
    #[serde(default)]
    pub max_files_per_fetch: u32,
    /// The most of one file a read-a-file tool may fetch. It bounds the
    /// fetch rather than the answer: the platform API has no range request,
    /// so reading part of a file means downloading all of it, and a file
    /// past this cannot be read at all, in whole or in part.
    #[serde(default)]
    pub max_file_bytes: u64,
    /// What one tool answer may hand back. A fact about tools rather than
    /// about the window: how much diagnostic output is worth reading is the
    /// same question whatever model is asked, and the registry needs the
    /// number before there is a window to divide. **How much a whole round
    /// may add up to is not this**: that is what the window can afford after
    /// the diff has taken its share, worked out per run.
    #[serde(default)]
    pub max_tool_output_bytes: u64,
    /// How many investigation rounds one file may take. A dead-loop guard,
    /// not a window reservation: the conversation also stops when the next
    /// call no longer fits. Per file, not per run.
    #[serde(default)]
    pub max_rounds: u32,
}

#[cfg(test)]
impl ReviewSettings {
    /// None of these have defaults, so every fixture that runs the loop or
    /// reads a file needs them. Kept in one place rather than per test.
    pub fn for_tests() -> Self {
        Self {
            max_files_per_listing: 200,
            max_hits_per_search: 50,
            max_files_per_fetch: 20,
            max_file_bytes: 262_144,
            max_tool_output_bytes: 32_768,
            max_rounds: 100,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSettings {
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

impl Default for PlanSettings {
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
impl PlanSettings {
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
    /// The whitelist of extensions that may be reviewed or read. There is
    /// no builtin list: which extensions are safe depends on the repository,
    /// and a default would decide that for the config author silently.
    /// `Config::validate` refuses an empty one. `plan` skips anything
    /// outside it; the read-a-file tools refuse it again.
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

/// A code hosting instance. The input URL's host picks the row; that host
/// is read off `base_url`. No `name`, no `kind`, no `host` field.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformEntry {
    pub base_url: String,
    pub api_token: String,
}

impl PlatformEntry {
    /// The browser host this row answers, if `base_url` is a known API.
    pub fn host(&self) -> Option<&'static str> {
        known_platform(&self.base_url).map(|row| row.web_host)
    }

    pub fn kind(&self) -> Option<PlatformKind> {
        known_platform(&self.base_url).map(|row| row.kind)
    }
}

/// One public API we will talk to. The API host and the browser host differ
/// for GitHub (`api.github.com` vs `github.com`); they match for GitLab.
struct KnownPlatform {
    api_host: &'static str,
    web_host: &'static str,
    kind: PlatformKind,
}

const KNOWN_PLATFORMS: [KnownPlatform; 2] = [
    KnownPlatform {
        api_host: "gitlab.com",
        web_host: "gitlab.com",
        kind: PlatformKind::Gitlab,
    },
    KnownPlatform {
        api_host: "api.github.com",
        web_host: "github.com",
        kind: PlatformKind::Github,
    },
];

/// Only these two API hosts resolve. The path of `base_url` is not a clue.
fn known_platform(base_url: &str) -> Option<&'static KnownPlatform> {
    let url = reqwest::Url::parse(base_url).ok()?;
    let host = url.host_str()?;
    KNOWN_PLATFORMS.iter().find(|row| row.api_host == host)
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
