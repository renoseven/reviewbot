//! The five stages. Each one ends by writing a checkpoint; none of them knows
//! what runs before or after it. The order lives only in `lib.rs`.

pub mod input;
pub mod merge;
pub mod orient;
pub mod prompt;
pub mod publish;
pub mod review;
pub mod triage;

#[cfg(test)]
mod fixture;

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::budget::{Budget, BudgetError};
use crate::config::{ConfigError, Secret, SecretSource, Settings};
use crate::platform::{Capabilities, ChangeRef, Platform, PlatformError};
use crate::protocol::{Protocol, ProtocolError, Request, Response};
use crate::record::{InputIdentity, InputRecord, RecordError, Recorder};
use crate::security::{EnvPolicy, PathPolicy, Redactor};
use crate::tool::{
    CommandContext, CommandTool, FinishReview, ListFiles, ReadFile, Registry, SearchCode, StatFile,
    SubmitComment, SubmitSummary, ToolError, ToolLimits, WorktreeContext,
};
use crate::worktree::{Checkout, FetchedWorktree, Search, WorktreeError, WorktreeSource};

use input::diff::DiffError;
use prompt::PromptError;

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
    #[error(transparent)]
    Prompt(#[from] PromptError),
    #[error("cannot read the input: {reason}")]
    UnreadableInput { reason: String },
    /// The first turn of a chunk does not fit. Nothing the tool loop can do
    /// about it: the chunk limit `triage` computed was wrong.
    #[error(
        "{path}: the first turn already needs {tokens} tokens of a {context_window_tokens} token window"
    )]
    ChunkTooLarge {
        path: String,
        tokens: u32,
        context_window_tokens: u32,
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
    /// This run's only source of code, and always there: the checkout the
    /// command line named, or a directory of the run's own. Shared rather
    /// than owned, because the content tools hold it for the whole run.
    pub worktree: Arc<dyn WorktreeSource>,
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

        // One worktree, whatever the input was. A checkout the command line
        // named is the whole project on the reviewed commit; without one, the
        // run opens a directory of its own and fills it from the platform.
        // The platform is an attribute of that worktree, not a second mode.
        let worktree: Arc<dyn WorktreeSource> = match &settings.options.worktree {
            Some(path) => Arc::new(Checkout::open(path.clone())?),
            None => Arc::new(FetchedWorktree::new(
                platform.as_ref().map(|platform| platform.repo_source()),
                platform
                    .as_ref()
                    .map(|platform| platform.capabilities())
                    .unwrap_or_default(),
            )),
        };

        let tools = build_tools(ToolSources {
            settings,
            paths: path_policy(settings)?,
            worktree: Arc::clone(&worktree),
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
            // Nothing to read: these commands replay checkpoints, so the
            // worktree is there in shape only and registers no tool.
            worktree: Arc::new(FetchedWorktree::new(None, Capabilities::default())),
            redactor,
        })
    }

    /// Give the run's own worktree its directory, now that the run directory
    /// exists. A checkout was already open before the tools were registered;
    /// this is the other shape catching up, and it is why the run id could be
    /// computed first.
    pub fn open_worktree(&self, run_dir: &Path) -> Result<(), StageError> {
        self.worktree.open_in(run_dir)?;
        Ok(())
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
/// Gathered into one value so registration still reads as a single decision.
struct ToolSources<'a> {
    settings: &'a Settings,
    paths: PathPolicy,
    /// This run's worktree, which is the only content source there is.
    worktree: Arc<dyn WorktreeSource>,
}

/// A tool whose preconditions are unmet is not registered, and says so once:
/// what the model can see always equals what it can really call. A tool that
/// fails is read as an answer about the repository, and that answer ends up in
/// a comment about the code.
///
/// Three preconditions, all of them properties of this run's worktree:
///
/// - **content at all.** A diff with no platform behind it leaves the worktree
///   empty for the whole run, so no content tool and no checker is registered.
/// - **a search that can be answered.** A platform with no code search leaves
///   `search_code` out rather than answering nothing: "no hits" would be read
///   as "not there".
/// - **a whole checkout.** A checker that compiles, walks history or reasons
///   across files needs the project, not the handful of files fetched so far.
fn build_tools(sources: ToolSources<'_>) -> Result<Registry, StageError> {
    let mut registry = Registry::new();
    // The three ways of handing something over, always registered: a finding,
    // "I have none", and the verdict. None of them has a precondition, and
    // `Round` keeps each off the turns it does not belong to. "I have none"
    // has to be as available as filing something, or a model with nothing to
    // file will file something.
    registry.register(Box::new(SubmitComment::new()));
    registry.register(Box::new(FinishReview::new()));
    registry.register(Box::new(SubmitSummary::new()));
    let settings = sources.settings;
    let reach = sources.worktree.reach();
    if !reach.has_content() {
        tracing::warn!(
            "this run has no code to read: no content tool and no checker is registered"
        );
        return Ok(registry);
    }

    let limits = ToolLimits::from_config(&settings.config);
    let context =
        WorktreeContext::new(Arc::clone(&sources.worktree), sources.paths.clone(), limits);
    registry.register(Box::new(ListFiles::new(context.clone())));
    registry.register(Box::new(StatFile::new(context.clone())));
    registry.register(Box::new(ReadFile::new(context.clone())));
    match reach.search {
        Search::Unavailable => {
            tracing::warn!("nothing can answer a search this run: search_code is not registered");
        }
        _ => registry.register(Box::new(SearchCode::new(context))),
    }

    for entry in &settings.config.tools {
        if entry.requires_checkout && !reach.is_checkout() {
            tracing::warn!(
                tool = %entry.name,
                "this run has no checkout, only fetched files: not registered"
            );
            continue;
        }
        registry.register(Box::new(CommandTool::new(
            entry.clone(),
            CommandContext::new(
                EnvPolicy::default(),
                sources.paths.clone(),
                Arc::clone(&sources.worktree),
                settings.config.review.max_tool_output_bytes,
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
    use std::path::PathBuf;

    use super::*;
    use crate::config::{BUILTIN_TOOL_NAMES, RunOptions};
    use crate::platform::{LineRange, Listing, RepoSource, SearchHit};
    use crate::tool::Purpose;

    /// Present or absent is the only thing these tests ask of a repository, so
    /// it does not have to answer anything.
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

    const CONFIG: &str = r#"
[review]
max_tool_rounds = 12
max_files_per_listing = 200
max_hits_per_search = 50
max_file_bytes = 262144
max_tool_output_bytes = 32768

[triage]
max_chunk_tokens = 24000
skip_files_over_bytes = 262144

[security]
allow_extensions = ["rs", "toml", "c", "h"]

[[provider]]
name = "deepseek"
protocol = "openai"
base_url = "https://api.deepseek.com"
api_key = "DEEPSEEK_API_KEY"
currency = "CNY"
budget_per_run = 10.0

[[model]]
name = "deepseek-v4-flash"
default = true
provider = "deepseek"
input_per_1m_tokens = 2.0
output_per_1m_tokens = 3.0
context_window_tokens = 131072
max_output_tokens = 4096

[[tool]]
name = "typecheck"
description = "runs the type checker over one file"
bin = "/usr/bin/env"
args = ["echo", "{path}"]

[tool.params.path]
type = "path"

[[tool]]
name = "compile"
description = "compiles the whole project"
bin = "/usr/bin/env"
args = ["echo", "{path}"]
requires_checkout = true

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

    /// The registration matrix run through `build_tools` itself: every one of
    /// its decisions is a property of this run's worktree and nothing else.
    fn registry(settings: &Settings, worktree: Arc<dyn WorktreeSource>) -> Registry {
        build_tools(ToolSources {
            settings,
            paths: path_policy(settings).expect("valid globs"),
            worktree,
        })
        .expect("built")
    }

    /// The worktree a run opens for itself, already sitting in a directory.
    fn fetched(
        run_dir: &Path,
        repository: Option<Arc<dyn RepoSource>>,
        capabilities: Capabilities,
    ) -> Arc<dyn WorktreeSource> {
        let worktree = FetchedWorktree::new(repository, capabilities);
        worktree.open_in(run_dir).expect("opened");
        Arc::new(worktree)
    }

    fn checkout(root: &Path) -> Arc<dyn WorktreeSource> {
        Arc::new(Checkout::open(root.to_path_buf()).expect("a directory"))
    }

    /// A diff with no platform behind it: the worktree exists, and it is
    /// empty for the whole run. Not registered rather than registered and
    /// always failing — a model that can see a tool will call it, and it
    /// would read the failure as an answer about the repository.
    #[test]
    fn an_empty_worktree_leaves_the_model_no_content_tools_and_no_checkers() {
        let root = tempfile::tempdir().expect("temp dir");
        let settings = settings(root.path(), None);
        let registry = registry(
            &settings,
            fetched(root.path(), None, Capabilities::default()),
        );

        assert_eq!(
            registry.names(),
            vec!["submit_comment", "finish_review", "submit_summary"],
            "the three ways of handing something over, and nothing to look at"
        );
        assert!(registry.names_with_purpose(Purpose::Content).is_empty());
        assert!(registry.names_with_purpose(Purpose::Check).is_empty());
        for name in BUILTIN_TOOL_NAMES {
            // The three ways of handing something over need nothing of the
            // worktree, so they are there even when there is nothing to read.
            let delivery = registry
                .get(name)
                .is_some_and(|tool| tool.purpose() == Purpose::Delivery);
            match delivery {
                true => continue,
                false => assert!(registry.get(name).is_none(), "{name} should not exist"),
            }
        }
    }

    /// A URL with no checkout: one group of content tools over a worktree the
    /// run fills from the platform. The names are the same ones a checkout
    /// gets — only the descriptions differ.
    #[test]
    fn a_fetched_worktree_registers_the_content_tools_under_the_same_names() {
        let root = tempfile::tempdir().expect("temp dir");
        let settings = settings(root.path(), None);

        let without_search = registry(
            &settings,
            fetched(
                root.path(),
                Some(Arc::new(StubRepo) as Arc<dyn RepoSource>),
                Capabilities::default(),
            ),
        );
        assert_eq!(
            without_search.names_with_purpose(Purpose::Content),
            vec!["list_files", "stat_file", "read_file"],
            "a platform that cannot search registers no search"
        );

        let with_search = registry(
            &settings,
            fetched(
                root.path(),
                Some(Arc::new(StubRepo) as Arc<dyn RepoSource>),
                Capabilities {
                    code_search: true,
                    regex_search: false,
                },
            ),
        );
        assert_eq!(
            with_search.names_with_purpose(Purpose::Content),
            vec!["list_files", "stat_file", "read_file", "search_code"]
        );
    }

    /// "Needs a whole checkout" is its own precondition, and a worktree of
    /// fetched files does not satisfy it: a `.c` file without its project
    /// headers earns a screen of missing includes, which is worse than not
    /// running the checker at all.
    #[test]
    fn a_checker_that_needs_the_whole_project_waits_for_a_checkout() {
        let root = tempfile::tempdir().expect("temp dir");

        let without = settings(root.path(), None);
        let fetched_only = registry(
            &without,
            fetched(
                root.path(),
                Some(Arc::new(StubRepo) as Arc<dyn RepoSource>),
                Capabilities::default(),
            ),
        );
        assert_eq!(
            fetched_only.names_with_purpose(Purpose::Check),
            vec!["typecheck"],
            "the single-file checker still runs; the compiling one does not"
        );

        let with = settings(root.path(), Some(root.path().to_path_buf()));
        let whole = registry(&with, checkout(root.path()));
        assert_eq!(
            whole.names_with_purpose(Purpose::Check),
            vec!["typecheck", "compile"]
        );
        assert!(
            whole.get("compile").is_some(),
            "adding a checker is a [[tool]] entry and nothing else"
        );
    }

    /// The reserved list is a security boundary: a `[[tool]]` entry carrying
    /// a builtin's name is refused at startup, so a name that registers
    /// without being reserved is a name config could shadow. Equality, in
    /// both directions, because this list has drifted before and will again.
    #[test]
    fn the_reserved_names_are_exactly_what_a_fully_equipped_run_registers() {
        let root = tempfile::tempdir().expect("temp dir");
        let settings = settings(root.path(), Some(root.path().to_path_buf()));
        let registry = registry(&settings, checkout(root.path()));

        // Everything the registry holds, whatever its purpose and whichever
        // round offers it: the verdict is registered like the rest and kept
        // off a review round by `Round::Scoring`, not by being left out.
        let mut registered: Vec<String> = registry
            .names()
            .into_iter()
            .filter(|name| {
                registry
                    .get(name)
                    .is_some_and(|tool| tool.purpose() != Purpose::Check)
            })
            .map(str::to_string)
            .collect();
        registered.sort();
        let mut reserved: Vec<String> = BUILTIN_TOOL_NAMES
            .iter()
            .map(|name| name.to_string())
            .collect();
        reserved.sort();
        assert_eq!(registered, reserved);
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
}
