//! The six stages. Each one ends by writing a checkpoint; none of them knows
//! what runs before or after it. The order lives only in `lib.rs`.

pub mod input;
pub mod merge;
pub mod orient;
pub mod prompt;
pub mod publish;
pub mod report;
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
use crate::common::{Secret, SecretSource};
use crate::config::{ConfigError, Settings};
use crate::domain::Stage;
use crate::platform::Repo;
use crate::platform::{ChangeRef, Platform, PlatformError};
use crate::progress::{Event, Outcome, Progress};
use crate::protocol::{Protocol, ProtocolError, Request, Response};
use crate::record::{InputIdentity, InputRecord, RecordError, Recorder};
use crate::security::{PathPolicy, Redactor};
use crate::tool::{Registry, ToolError};
use crate::worktree::{Worktree, WorktreeError};

use input::DiffError;
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
    #[error(
        "{posted} comments posted, {failed} failed; run the same `review` command again to post the rest"
    )]
    PublishIncomplete { posted: usize, failed: usize },
}

/// What a run can reach before it has a directory of its own: the platform
/// the change comes from, the protocol the model is called over, and the
/// redactor holding this run's secrets. Built at startup and handed to every
/// stage, so tests can put fakes in the same slots.
///
/// The worktree and the tools are deliberately not here. Neither can exist
/// this early — the cache lives inside the run directory, and every tool's
/// description is written from the worktree it will read — so they are built
/// afterwards, as `Equipment`.
pub struct Adapters {
    pub platform: Option<Box<dyn Platform>>,
    pub protocol: Box<dyn Protocol>,
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

        Ok(Self {
            platform,
            protocol,
            redactor,
        })
    }

    /// The repository this run reads content from, when there is a platform
    /// to read it through. A plain diff has none, which is the single source
    /// of the `Option` the worktree carries.
    pub fn repo(&self) -> Option<Repo> {
        self.platform.as_ref().map(|platform| platform.repo())
    }

    /// Point the repository reads at the commit under review. The head sha is
    /// settled before any stage runs — this run resolved it, or read it back
    /// out of the `meta.json` an earlier attempt left — and every repository
    /// read is by that sha.
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

/// The other half of the adapters: this run's worktree and the tools built
/// over it.
///
/// Apart from `Adapters` because of when it can be built. The cache a run
/// fills for itself sits inside the run directory, so it must not be opened
/// before that directory is locked; and every tool's description, refusal and
/// availability is written from the worktree, so the tools cannot be built
/// before it either. Both facts point at the same moment, which is after the
/// lock and before the first stage.
pub struct Equipment {
    /// Shared rather than owned: the content tools and the checkers hold it
    /// for the whole run.
    pub worktree: Arc<Worktree>,
    pub tools: Registry,
}

impl Equipment {
    pub fn real(
        settings: &Settings,
        adapters: &Adapters,
        run_dir: &Path,
    ) -> Result<Self, StageError> {
        let worktree = Arc::new(Worktree::open(
            settings.options.worktree.clone(),
            adapters.repo(),
            run_dir,
        )?);
        warn_about(&worktree);
        let tools = crate::tool::build(
            settings,
            PathPolicy::for_settings(settings)?,
            Arc::clone(&worktree),
            run_dir,
        );
        Ok(Self { worktree, tools })
    }
}

/// The read boundary for this run: `[security]` plus the directories the run
/// writes to, expressed relative to the worktree when there is one.
pub fn path_policy(settings: &Settings) -> Result<PathPolicy, StageError> {
    Ok(PathPolicy::for_settings(settings)?)
}

fn hide_secret(redactor: &mut Redactor, secret: &Secret) {
    redactor.hide_value(secret.expose());
}

fn hide_platform_token(redactor: &mut Redactor, settings: &Settings, host: &str) {
    let Some(entry) = settings.config.platform(host) else {
        return;
    };
    let field = format!("platform.{}.api_token", entry.host().unwrap_or("unknown"));
    if let Ok(token) = SecretSource::parse(&field, &entry.api_token)
        .and_then(|source| source.read(&field, settings.options.worktree.as_deref()))
    {
        hide_secret(redactor, &token);
    }
}

/// Say once, for whoever is watching the run, what this worktree cannot do.
/// The same sentences the report will carry, so the terminal and the report
/// cannot disagree; the model is told the same thing by every description it
/// is given.
fn warn_about(worktree: &Worktree) {
    for went_without in worktree.went_without() {
        tracing::warn!("this run {went_without}");
    }
    if worktree.is_empty() {
        tracing::warn!(
            "pass --worktree to point at a checkout, or review a merge request URL. Every tool \
             is still offered, and each one says it cannot answer"
        );
    }
}

/// Everything a stage is allowed to reach. Assembled by the caller so no
/// stage has to build its own dependencies.
pub struct StageContext<'a> {
    pub settings: &'a Settings,
    pub adapters: &'a Adapters,
    /// Where this run reads code from. Beside `adapters` rather than inside
    /// it, because it is built later: not until the run directory exists.
    pub worktree: &'a Worktree,
    /// What the model may call, built over that worktree.
    pub tools: &'a Registry,
    pub recorder: &'a mut Recorder,
    pub budget: &'a mut Budget,
    pub redactor: &'a Redactor,
    pub paths: &'a PathPolicy,
    /// Whoever is watching this run. Shared, not owned: the same watcher
    /// hears every stage, and no stage may change what it does because of
    /// what is on the other end.
    pub progress: &'a dyn Progress,
}

impl StageContext<'_> {
    /// A stage is beginning, whether or not there is work left in it.
    pub fn stage_started(&self, stage: Stage) {
        self.progress.emit(Event::StageStarted { stage });
    }

    /// A stage is over, with the typed result the caller learned by placing
    /// it in the sequence. Said here rather than inside a stage because only
    /// `lib.rs` knows whether that result came from work or a checkpoint.
    pub fn stage_finished(&self, stage: Stage, outcome: Outcome, from_checkpoint: bool) {
        self.progress.emit(Event::StageFinished {
            stage,
            outcome,
            from_checkpoint,
        });
    }

    /// The stage's own checkpoint, when it already finished.
    pub fn completed<T: DeserializeOwned>(
        &mut self,
        stage: Stage,
    ) -> Result<Option<T>, StageError> {
        Ok(self.recorder.completed(stage)?)
    }

    /// Write the checkpoint and mark the stage done.
    pub fn complete<T: Serialize>(&mut self, stage: Stage, output: &T) -> Result<(), StageError> {
        self.recorder.complete(stage, output)?;
        Ok(())
    }

    /// Write the checkpoint without marking the stage done.
    pub fn save<T: Serialize>(&self, stage: Stage, output: &T) -> Result<(), StageError> {
        self.recorder.save(stage, output)?;
        Ok(())
    }

    /// The checkpoint as last written, whether or not the stage finished.
    pub fn saved<T: DeserializeOwned>(&self, stage: Stage) -> Result<Option<T>, StageError> {
        Ok(self.recorder.saved(stage)?)
    }

    /// The one gate every model call passes: cap this request's output to
    /// what the money on hand buys, and refuse the call when that is no
    /// longer enough to answer with. The allowance is written onto the
    /// request rather than checked against a number the request does not
    /// carry, so the vendor is held to it.
    ///
    /// `reserve` is the output allowance a caller wants kept back for a call
    /// after this one — the turn that ends a conversation properly. Zero
    /// from a caller with nothing to follow.
    pub fn authorize(&self, request: &mut Request, reserve: u32) -> Result<(), BudgetError> {
        let ceiling = request.max_output_tokens;
        let allowed = self.budget.allow(ceiling, reserve)?;
        if allowed < ceiling {
            tracing::debug!(allowed, ceiling, "output capped to what is left to spend");
        }
        request.max_output_tokens = allowed;
        Ok(())
    }

    /// One model call with the wait visible on the log: a line before the
    /// HTTP round trip, and one after with duration, tokens and spend.
    pub fn send_and_settle(&mut self, request: &Request) -> Result<Response, StageError> {
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
        // The one place money moves, so the one place worth saying it from.
        // Straight off the budget rather than accumulated by the watcher:
        // the run's own account is what the summary will print.
        self.progress.emit(Event::Spend {
            spent: self.budget.spent(),
            budget: self.budget.ceiling(),
            currency: self.budget.currency().to_string(),
        });
        // The one place money moves, so the one place it is written down.
        // A run that dies mid-chunk still knows what it has already spent
        // when it comes back.
        self.recorder.record_spend(self.budget.spent())?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::{RESERVED_TOOL_NAMES, RunOptions};
    use crate::platform::{Capabilities, LineRange, Listing, RepoSource, SearchHit};
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

        fn size(&self, _path: &str) -> Result<u64, PlatformError> {
            Ok(0)
        }

        fn search(
            &self,
            _kind: crate::platform::SearchKind,
            _query: &str,
            _glob: Option<&str>,
        ) -> Result<Vec<SearchHit>, PlatformError> {
            Ok(Vec::new())
        }
    }

    const CONFIG: &str = r#"
[review]
max_files_per_listing = 200
max_hits_per_search = 50
max_files_per_fetch = 20
max_file_bytes = 262144
max_tool_output_bytes = 32768
max_rounds = 100

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

    /// Every case below goes through the real construction path, because what
    /// is being asserted is that it decides nothing: the worktree does.
    fn registry(settings: &Settings, worktree: Worktree) -> Registry {
        crate::tool::build(
            settings,
            path_policy(settings).expect("valid globs"),
            Arc::new(worktree),
            settings.options.runs_dir.as_path(),
        )
    }

    /// The cache a run opens for itself, already sitting in its directory,
    /// with a repository behind it that answers whatever `capabilities` says.
    fn cache(run_dir: &Path, capabilities: Capabilities) -> Worktree {
        let repo = Repo::new(Arc::new(StubRepo) as Arc<dyn RepoSource>, capabilities);
        Worktree::open(None, Some(repo), run_dir).expect("a cache")
    }

    fn checkout(root: &Path) -> Worktree {
        Worktree::open(Some(root.to_path_buf()), None, root).expect("a directory")
    }

    /// The whole point of the change: what a run can check is a property of
    /// its worktree, and the set of names is not. Three very different
    /// worktrees, one list.
    #[test]
    fn every_worktree_offers_the_model_the_same_tools() {
        let root = tempfile::tempdir().expect("temp dir");
        let names = |worktree| {
            let settings = settings(root.path(), None);
            registry(&settings, worktree)
                .names()
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<String>>()
        };
        let expected = vec![
            "submit_comment",
            "finish_review",
            "submit_summary",
            "list_local_files",
            "suggest_local_read",
            "read_local_file",
            "search_local_regex",
            "list_repo_files",
            "fetch_repo_file",
            "search_repo_regex",
            "search_repo_keyword",
            "typecheck",
            "compile",
        ];

        assert_eq!(
            names(Worktree::Empty),
            expected,
            "an empty worktree withholds nothing"
        );
        assert_eq!(
            names(cache(root.path(), Capabilities::empty())),
            expected,
            "neither does a platform that cannot search"
        );
        assert_eq!(names(checkout(root.path())), expected);
    }

    /// A diff with no platform behind it: the worktree exists and is empty for
    /// the whole run. Every tool is still offered, and every one of them says
    /// what is missing and that nothing about the code follows from it.
    #[test]
    fn an_empty_worktree_answers_every_tool_with_a_reason_about_the_run() {
        let root = tempfile::tempdir().expect("temp dir");
        let settings = settings(root.path(), None);
        let registry = registry(&settings, Worktree::Empty);

        assert!(registry.usable_with_purpose(Purpose::Content).is_empty());
        assert!(registry.usable_with_purpose(Purpose::Check).is_empty());
        for name in registry.names() {
            let tool = registry.get(name).expect("just listed");
            if tool.purpose() == Purpose::Delivery {
                // Handing a finding over needs nothing of the worktree.
                assert!(tool.unavailable().is_none(), "{name}");
                continue;
            }
            let reason = tool.unavailable().expect(name);
            assert!(
                reason.contains("not about the repository")
                    || reason.contains("not evidence")
                    || reason.contains("not about the code"),
                "{name}: {reason}"
            );
            assert!(reason.contains("this run"), "{name}: {reason}");
            assert!(
                tool.description().contains("NOT AVAILABLE THIS RUN"),
                "{name}: the model has to be able to decide before calling"
            );
            let refused = registry
                .execute(name, &serde_json::json!({}))
                .expect_err(name);
            assert!(
                matches!(&refused, ToolError::Unavailable { reason, .. }
                    if reason.contains("not about the repository")
                        || reason.contains("not evidence")
                        || reason.contains("not about the code")),
                "{name}: {refused}"
            );
        }
        for name in RESERVED_TOOL_NAMES {
            assert!(registry.get(name).is_some(), "{name} should be offered");
        }
    }

    /// A platform that cannot search still offers both repository search
    /// tools, and the refusal says what an empty result would have meant if
    /// it had run: a miss here is the run's limit, not proof of absence.
    #[test]
    fn a_worktree_that_cannot_search_refuses_the_search_and_answers_the_rest() {
        let root = tempfile::tempdir().expect("temp dir");
        let settings = settings(root.path(), None);
        let registry = registry(&settings, cache(root.path(), Capabilities::empty()));

        assert_eq!(
            registry.usable_with_purpose(Purpose::Content),
            vec![
                "list_local_files",
                "suggest_local_read",
                "read_local_file",
                "search_local_regex",
                "list_repo_files",
                "fetch_repo_file",
            ]
        );
        for name in ["search_repo_regex", "search_repo_keyword"] {
            let search = registry.get(name).expect("still offered");
            let reason = search.unavailable().expect("nothing can answer one");
            assert!(reason.contains("not evidence"), "{reason}");
            assert!(search.description().contains("NOT AVAILABLE THIS RUN"));
        }
    }

    /// "Needs a whole checkout" is a fact about the worktree, not about which
    /// tools were built: a `.c` file without its project headers earns a
    /// screen of missing includes, which is worse than not running the checker
    /// at all. So the checker is offered either way and refuses without one.
    #[test]
    fn a_checker_that_needs_the_whole_project_refuses_until_it_has_one() {
        let root = tempfile::tempdir().expect("temp dir");

        let without = settings(root.path(), None);
        let cache_only = registry(&without, cache(root.path(), Capabilities::empty()));
        assert_eq!(
            cache_only.usable_with_purpose(Purpose::Check),
            vec!["typecheck"],
            "the single-file checker answers; the compiling one cannot"
        );
        let compile = cache_only.get("compile").expect("offered all the same");
        assert!(
            compile
                .unavailable()
                .is_some_and(|reason| reason.contains("whole checkout")),
            "{:?}",
            compile.unavailable()
        );

        let with = settings(root.path(), Some(root.path().to_path_buf()));
        let whole = registry(&with, checkout(root.path()));
        assert_eq!(
            whole.usable_with_purpose(Purpose::Check),
            vec!["typecheck", "compile"]
        );
        assert!(
            whole
                .get("compile")
                .is_some_and(|tool| tool.unavailable().is_none()
                    && !tool.description().contains("NOT AVAILABLE")),
            "adding a checker is a [[tool]] entry and nothing else"
        );
    }

    /// `tool list` goes through `build` on the widest shape — a Local
    /// worktree holding a repository that can answer both search engines —
    /// so every content tool and a `requires_checkout` checker print as
    /// available. That is how the catalog stays what a review really registers.
    #[test]
    fn the_catalog_prints_the_widest_shape_as_available() {
        let root = tempfile::tempdir().expect("temp dir");
        let settings = settings(root.path(), Some(root.path().to_path_buf()));
        let listing = crate::tool::inventory(&settings).expect("catalog");
        for name in [
            "list_local_files",
            "suggest_local_read",
            "read_local_file",
            "search_local_regex",
            "list_repo_files",
            "fetch_repo_file",
            "search_repo_regex",
            "search_repo_keyword",
            "compile",
        ] {
            let row = listing
                .iter()
                .find(|row| row.name == name)
                .unwrap_or_else(|| panic!("{name} missing from the catalog"));
            assert!(
                !row.description.contains("NOT AVAILABLE THIS RUN"),
                "{name}: {}",
                row.description
            );
            assert!(
                !row.preconditions.is_empty(),
                "{name} still names the condition a narrower run would miss"
            );
        }
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
        let mut reserved: Vec<String> = RESERVED_TOOL_NAMES
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
