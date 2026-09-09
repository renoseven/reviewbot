//! The five stages. Each one ends by writing a checkpoint; none of them knows
//! what runs before or after it. The order lives only in `lib.rs`.

pub mod input;
pub mod merge;
pub mod orient;
pub mod publish;
pub mod review;
pub mod triage;

#[cfg(test)]
mod fixture;

use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::budget::{Budget, BudgetError};
use crate::config::{ConfigError, Secret, SecretSource, Settings};
use crate::platform::{Capabilities, ChangeRef, Platform, PlatformError, RepoSource};
use crate::protocol::{Protocol, ProtocolError, Request, Response};
use crate::record::{InputIdentity, InputRecord, RecordError, Recorder};
use crate::security::{EnvPolicy, PathPolicy, Redactor};
use crate::tool::{
    CommandContext, CommandTool, ListRepoFiles, ListWorktreeFiles, ReadRepoFile, ReadWorktreeFile,
    Registry, RepoContext, SearchRepo, SearchWorktree, StatRepoFile, StatWorktreeFile,
    SubmitComment, ToolError, ToolLimits, WorktreeContext,
};
use crate::worktree::{LocalWorktree, WorktreeError, WorktreeSource};

use input::diff::DiffError;

#[derive(Debug, thiserror::Error)]
pub enum StageError {
    #[error(transparent)]
    Diff(#[from] DiffError),
    #[error(transparent)]
    Platform(#[from] PlatformError),
    #[error(transparent)]
    Worktree(#[from] WorktreeError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Record(#[from] RecordError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("cannot read the input: {reason}")]
    UnreadableInput { reason: String },
    /// The first turn of a chunk does not fit. Nothing the tool loop can do
    /// about it: the chunk limit `triage` computed was wrong.
    #[error(
        "{path}: the first turn already needs {tokens} tokens of a {context_window} token window"
    )]
    ChunkTooLarge {
        path: String,
        tokens: u32,
        context_window: u32,
    },
    #[error("{posted} comments posted, {failed} failed; run `reviewbot publish` to finish")]
    PublishIncomplete { posted: usize, failed: usize },
}

/// The adapters this run talks to. Built once at startup and handed to every
/// stage, so tests can put fakes in the same slots.
pub struct Adapters {
    pub platform: Option<Box<dyn Platform>>,
    pub protocol: Box<dyn Protocol>,
    pub tools: Registry,
    /// Shared rather than owned: the three disk tools hold it for the whole
    /// run, and the stages read it through the same handle.
    pub worktree: Option<Arc<dyn WorktreeSource>>,
    /// Process redactor with this run's secrets already hidden.
    pub redactor: Redactor,
}

impl Adapters {
    /// The real ones. `host` comes from the input URL and decides which
    /// `[[platform]]` entry is used; diff input has none.
    pub fn real(settings: &Settings, host: Option<&str>) -> Result<Self, StageError> {
        let selection = settings.selection()?;
        let api_key = settings.selected_api_key()?;
        let mut redactor = Redactor::new();
        hide_secret(&mut redactor, &api_key);
        if let Some(host) = host {
            hide_platform_token(&mut redactor, settings, host);
        }

        let protocol =
            crate::protocol::resolve(selection.provider, api_key, settings.options.backoff())?;

        let platform = match host {
            Some(host) => Some(crate::platform::resolve(
                &settings.config,
                host,
                settings.options.backoff(),
            )?),
            None => None,
        };

        let worktree = match &settings.options.worktree {
            Some(path) => {
                Some(Arc::new(LocalWorktree::open(path.clone())?) as Arc<dyn WorktreeSource>)
            }
            None => None,
        };

        // Both content sources are independently there or not there, and which
        // builtin tools exist follows straight from that (§8): a URL brings the
        // repository group, `--worktree` brings the disk group, and a diff with
        // no worktree brings neither.
        let tools = build_tools(ToolSources {
            settings,
            paths: path_policy(settings)?,
            repo: platform.as_ref().map(|platform| platform.repo_source()),
            disk: worktree.clone(),
            capabilities: platform
                .as_ref()
                .map(|platform| platform.capabilities())
                .unwrap_or_default(),
        })?;

        Ok(Self {
            tools,
            platform,
            protocol,
            worktree,
            redactor,
        })
    }

    /// Platform and redactor only. Used by `publish` / `report`, which must
    /// not construct a model client.
    pub fn without_model(settings: &Settings, host: Option<&str>) -> Result<Self, StageError> {
        let mut redactor = Redactor::new();
        if let Some(host) = host {
            hide_platform_token(&mut redactor, settings, host);
        }
        let platform = match host {
            Some(host) => Some(crate::platform::resolve(
                &settings.config,
                host,
                settings.options.backoff(),
            )?),
            None => None,
        };
        Ok(Self {
            tools: Registry::new(),
            platform,
            protocol: Box::new(UnusedProtocol),
            worktree: None,
            redactor,
        })
    }

    /// Point the repository reads at the commit under review. The head sha is
    /// settled before any stage runs — this run resolved it, or `resume` read
    /// it back out of `meta.json` — and every repository read is by that sha.
    pub fn bind_repo(&self, record: &InputRecord) {
        let Some(platform) = &self.platform else {
            return;
        };
        let InputIdentity::Platform {
            host,
            project,
            number,
        } = &record.identity
        else {
            return;
        };
        platform.bind_repo(
            &ChangeRef {
                host: host.clone(),
                project: project.clone(),
                number: *number,
            },
            &record.head_sha,
        );
    }
}

/// Stands in for the model client on commands that must not call one.
struct UnusedProtocol;

impl Protocol for UnusedProtocol {
    fn name(&self) -> &'static str {
        "unused"
    }

    fn send(&self, _request: &Request) -> Result<Response, ProtocolError> {
        Err(ProtocolError::Fatal {
            protocol: "unused",
            reason: "this command does not call the model".to_string(),
            status: None,
        })
    }
}

/// The read boundary for this run: `[security]` plus the directories the run
/// writes to, expressed relative to the worktree when there is one.
pub fn path_policy(settings: &Settings) -> Result<PathPolicy, StageError> {
    PathPolicy::new(
        &settings.config.security,
        &settings.written_paths(),
        settings.options.worktree.as_deref(),
    )
    .map_err(|error| {
        StageError::Config(ConfigError::InvalidGlob {
            field: "[security].deny_paths",
            pattern: String::new(),
            reason: error.to_string(),
        })
    })
}

fn hide_secret(redactor: &mut Redactor, secret: &Secret) {
    redactor.hide_value(secret.expose());
}

fn hide_platform_token(redactor: &mut Redactor, settings: &Settings, host: &str) {
    let Some(entry) = settings.config.platform(host) else {
        return;
    };
    let field = format!("platform.{}.api_token", entry.host);
    if let Ok(token) = SecretSource::parse(&field, &entry.api_token)
        .and_then(|source| source.read(&field, settings.options.worktree.as_deref()))
    {
        hide_secret(redactor, &token);
    }
}

/// What decides which tools exist this run and what each of them may reach.
/// Gathered into one value so registration still reads as a single decision
/// rather than six arguments threaded through.
struct ToolSources<'a> {
    settings: &'a Settings,
    paths: PathPolicy,
    /// The repository through the platform API, which only a URL input has.
    repo: Option<Arc<dyn RepoSource>>,
    /// The local checkout, which only `--worktree` brings.
    disk: Option<Arc<dyn WorktreeSource>>,
    capabilities: Capabilities,
}

/// A tool whose preconditions are unmet is not registered, and says so once:
/// what the model can see always equals what it can really call. That is the
/// same rule for all three kinds here — a `[[tool]]` entry needs a worktree
/// because its cwd is the worktree root, the disk group needs one because it
/// reads the disk, and the repository group needs a platform to read through.
///
/// A search the platform cannot do is left out rather than registered and
/// answering nothing: the model would read "no hits" as "not there".
fn build_tools(sources: ToolSources<'_>) -> Result<Registry, StageError> {
    let mut registry = Registry::new();
    registry.register(Box::new(SubmitComment));
    let settings = sources.settings;
    let max_output_bytes = settings.config.review.max_tool_output_bytes;
    let limits = ToolLimits::from_config(&settings.config);

    if let Some(repo) = &sources.repo {
        let context = RepoContext::new(Arc::clone(repo), sources.paths.clone(), limits);
        registry.register(Box::new(ListRepoFiles::new(context.clone())));
        registry.register(Box::new(StatRepoFile::new(context.clone())));
        registry.register(Box::new(ReadRepoFile::new(context.clone())));
        if sources.capabilities.code_search {
            registry.register(Box::new(SearchRepo::new(context, sources.capabilities)));
        } else {
            tracing::warn!("this platform has no code search: search_repo is not registered");
        }
    }

    if let Some(disk) = &sources.disk {
        let context = WorktreeContext::new(Arc::clone(disk), sources.paths.clone(), limits);
        registry.register(Box::new(ListWorktreeFiles::new(context.clone())));
        registry.register(Box::new(StatWorktreeFile::new(context.clone())));
        registry.register(Box::new(ReadWorktreeFile::new(context.clone())));
        registry.register(Box::new(SearchWorktree::new(context)));
    }

    let Some(worktree) = sources.disk.as_ref().map(|disk| disk.root().to_path_buf()) else {
        for entry in &settings.config.tools {
            tracing::debug!(tool = %entry.name, "no worktree: this tool is not registered");
        }
        return Ok(registry);
    };
    for entry in &settings.config.tools {
        registry.register(Box::new(CommandTool::new(
            entry.clone(),
            CommandContext::new(
                EnvPolicy::default(),
                sources.paths.clone(),
                worktree.clone(),
                max_output_bytes,
                settings.options.backoff(),
            ),
        )));
    }
    Ok(registry)
}

/// Everything a stage is allowed to reach. Assembled by the caller so no
/// stage has to build its own dependencies.
pub struct StageContext<'a> {
    pub settings: &'a Settings,
    pub adapters: &'a Adapters,
    pub recorder: &'a mut Recorder,
    pub budget: &'a mut Budget,
    pub redactor: &'a Redactor,
    pub paths: &'a PathPolicy,
}

impl StageContext<'_> {
    /// The stage's own checkpoint, when it already finished.
    pub fn completed<T: DeserializeOwned>(
        &mut self,
        number: u8,
        stage: &str,
    ) -> Result<Option<T>, StageError> {
        Ok(self.recorder.completed(number, stage)?)
    }

    /// Write the checkpoint and mark the stage done.
    pub fn complete<T: Serialize>(
        &mut self,
        number: u8,
        stage: &str,
        output: &T,
    ) -> Result<(), StageError> {
        self.recorder.complete(number, stage, output)?;
        Ok(())
    }

    /// One model call with the wait visible on the log: a line before the
    /// HTTP round trip, and one after with duration, tokens and spend.
    pub fn send_and_settle(&mut self, request: &Request) -> Result<Response, ProtocolError> {
        let started = Instant::now();
        tracing::info!(
            model = %request.model,
            tools = request.tools.len(),
            "calling model"
        );
        let response = self.adapters.protocol.send(request)?;
        let cost = self.budget.settle(&response.usage);
        tracing::info!(
            duration_ms = started.elapsed().as_millis() as u64,
            input_tokens = response.usage.input_tokens,
            cached_tokens = response.usage.cached_input_tokens,
            output_tokens = response.usage.output_tokens,
            message_bytes = response.output_text().len(),
            function_calls = response.function_calls().count(),
            spent = format!("{cost:.4}"),
            currency = %self.budget.currency(),
            cumulative = format!("{:.4}", self.budget.spent()),
            "model replied"
        );
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::config::{BUILTIN_TOOL_NAMES, RunOptions};
    use crate::platform::{LineRange, Listing, SearchHit};
    use crate::tool::Origin;
    use crate::worktree::{LineRange as DiskRange, SearchHit as DiskHit};

    /// Present or absent is the only thing these tests ask of a source, so
    /// neither of them has to answer anything.
    struct StubRepo;

    impl RepoSource for StubRepo {
        fn list_files(&self, _glob: &str) -> Result<Listing, PlatformError> {
            Ok(Listing::default())
        }

        fn read_file(
            &self,
            _path: &str,
            _lines: Option<LineRange>,
        ) -> Result<String, PlatformError> {
            Ok(String::new())
        }

        fn search(
            &self,
            _query: &str,
            _glob: Option<&str>,
        ) -> Result<Vec<SearchHit>, PlatformError> {
            Ok(Vec::new())
        }
    }

    struct StubDisk {
        root: PathBuf,
    }

    impl WorktreeSource for StubDisk {
        fn root(&self) -> &Path {
            &self.root
        }

        fn head_sha(&self) -> Result<String, WorktreeError> {
            Ok("head".to_string())
        }

        fn list_files(&self, _glob: &str) -> Result<Vec<String>, WorktreeError> {
            Ok(Vec::new())
        }

        fn read_file(
            &self,
            _path: &str,
            _lines: Option<DiskRange>,
        ) -> Result<String, WorktreeError> {
            Ok(String::new())
        }

        fn search(&self, _query: &str, _glob: Option<&str>) -> Result<Vec<DiskHit>, WorktreeError> {
            Ok(Vec::new())
        }
    }

    const CONFIG: &str = r#"
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
allow_extensions = ["rs", "toml", "c", "h"]

[[provider]]
name = "deepseek"
protocol = "openai"
base_url = "https://api.deepseek.com"
api_key = "DEEPSEEK_API_KEY"
currency = "CNY"
budget = 10.0

[[model]]
name = "deepseek-v4-flash"
default = true
provider = "deepseek"
input_per_1m = 2.0
output_per_1m = 3.0
context_window = 131072
max_output_tokens = 4096

[[tool]]
name = "typecheck"
description = "runs the type checker over one file"
bin = "/usr/bin/env"
args = ["echo", "{path}"]
requires_worktree = true

[tool.params.path]
type = "path"
"#;

    fn settings(root: &Path, worktree: Option<PathBuf>) -> Settings {
        let config = root.join("reviewbot.toml");
        if !config.exists() {
            std::fs::write(&config, CONFIG).expect("write config");
        }
        Settings::load(
            Some(&config),
            RunOptions {
                runs_dir: root.join("runs"),
                worktree,
                ..RunOptions::default()
            },
        )
        .expect("valid config")
    }

    /// The registration matrix of §8 run through `build_tools` itself: the two
    /// content sources are independently there or not, and which tools exist
    /// follows from that alone.
    fn registry(
        settings: &Settings,
        repo: Option<Arc<dyn RepoSource>>,
        disk: Option<Arc<dyn WorktreeSource>>,
        capabilities: Capabilities,
    ) -> Registry {
        build_tools(ToolSources {
            settings,
            paths: path_policy(settings).expect("valid globs"),
            repo,
            disk,
            capabilities,
        })
        .expect("built")
    }

    fn stub_repo() -> Option<Arc<dyn RepoSource>> {
        Some(Arc::new(StubRepo) as Arc<dyn RepoSource>)
    }

    fn stub_disk(root: &Path) -> Option<Arc<dyn WorktreeSource>> {
        Some(Arc::new(StubDisk {
            root: root.to_path_buf(),
        }) as Arc<dyn WorktreeSource>)
    }

    /// The last cell of the table, and it is "not registered" rather than
    /// "registered and always failing": a model that can see a tool will call
    /// it, and it would read the failure as an answer about the repository.
    #[test]
    fn a_diff_without_a_worktree_leaves_the_model_no_content_tools() {
        let root = tempfile::tempdir().expect("temp dir");
        let settings = settings(root.path(), None);
        let registry = registry(&settings, None, None, Capabilities::default());
        assert_eq!(registry.names(), vec!["submit_comment"]);
        for name in BUILTIN_TOOL_NAMES {
            if name == "submit_comment" {
                assert!(registry.get(name).is_some(), "{name} is always registered");
                continue;
            }
            assert!(registry.get(name).is_none(), "{name} should not exist");
        }
    }

    /// A URL brings the repository group whether or not there is a checkout,
    /// and `search_repo` follows the instance rather than the group.
    #[test]
    fn a_url_without_a_worktree_registers_the_repository_group_only() {
        let root = tempfile::tempdir().expect("temp dir");
        let settings = settings(root.path(), None);

        let without_search = registry(&settings, stub_repo(), None, Capabilities::default());
        assert_eq!(
            without_search.names(),
            vec![
                "submit_comment",
                "list_repo_files",
                "stat_repo_file",
                "read_repo_file"
            ],
            "no code search means no search_repo, and no worktree means no disk group"
        );

        let with_search = registry(
            &settings,
            stub_repo(),
            None,
            Capabilities {
                code_search: true,
                regex_search: false,
            },
        );
        assert_eq!(
            with_search.names(),
            vec![
                "submit_comment",
                "list_repo_files",
                "stat_repo_file",
                "read_repo_file",
                "search_repo",
            ]
        );
    }

    #[test]
    fn a_diff_with_a_worktree_registers_the_disk_group_only() {
        let root = tempfile::tempdir().expect("temp dir");
        let settings = settings(root.path(), Some(root.path().to_path_buf()));
        let registry = registry(
            &settings,
            None,
            stub_disk(root.path()),
            Capabilities::default(),
        );
        assert_eq!(
            registry.names_with_origin(Origin::Builtin),
            vec![
                "submit_comment",
                "list_worktree_files",
                "stat_worktree_file",
                "read_worktree_file",
                "search_worktree"
            ]
        );
    }

    /// Both sources, and the six names are exactly the ones `config` reserves
    /// so a `[[tool]]` entry can be refused for colliding with them.
    #[test]
    fn a_url_with_a_worktree_registers_both_groups_under_the_reserved_names() {
        let root = tempfile::tempdir().expect("temp dir");
        let settings = settings(root.path(), Some(root.path().to_path_buf()));
        let registry = registry(
            &settings,
            stub_repo(),
            stub_disk(root.path()),
            Capabilities {
                code_search: true,
                regex_search: false,
            },
        );

        let mut builtin = registry.names_with_origin(Origin::Builtin);
        builtin.sort();
        let mut reserved = BUILTIN_TOOL_NAMES.to_vec();
        reserved.sort();
        // The two lists answer different questions and no longer match. What
        // is reserved is what a `[[tool]]` entry may not be called; what is
        // registered is what this round advertises. `submit_summary` is
        // reserved without registering, because the verdict belongs to
        // `merge` and a review round has no business offering it.
        assert!(
            builtin.iter().all(|name| reserved.contains(name)),
            "{builtin:?} against {reserved:?}"
        );
        let unregistered: Vec<&&str> = reserved
            .iter()
            .filter(|name| !builtin.contains(name))
            .collect();
        assert_eq!(unregistered, vec![&"submit_summary"]);
        assert_eq!(
            registry.names_with_origin(Origin::Config),
            vec!["typecheck"],
            "the configured checker registers beside the builtins, not instead of them"
        );
    }

    #[test]
    fn hide_secret_blots_out_a_value_that_matches_no_shape() {
        let mut redactor = Redactor::new();
        hide_secret(
            &mut redactor,
            &Secret::from("plain-looking-credential".to_string()),
        );
        assert_eq!(
            redactor.redact("token is plain-looking-credential here"),
            "token is <redacted:credential> here"
        );
    }

    /// Adding a checker is a `[[tool]]` entry and nothing else — no Rust,
    /// and no name this module has heard of. Without a worktree there is no
    /// cwd to run one from, so the entry is dropped rather than advertised
    /// as callable: what the model is shown always equals what it can call.
    #[test]
    fn a_config_entry_becomes_a_callable_tool_only_where_it_can_actually_run() {
        let root = tempfile::tempdir().expect("temp dir");

        let with = settings(root.path(), Some(root.path().to_path_buf()));
        let runnable = registry(&with, None, stub_disk(root.path()), Capabilities::default());
        assert_eq!(
            runnable.names_with_origin(Origin::Config),
            vec!["typecheck"]
        );
        assert!(runnable.get("typecheck").is_some());

        let without = settings(root.path(), None);
        let nowhere_to_run = registry(&without, None, None, Capabilities::default());
        assert!(nowhere_to_run.names_with_origin(Origin::Config).is_empty());
        assert!(nowhere_to_run.get("submit_comment").is_some());
    }
}
