//! What the model may call. One trait, one registry, one construction path.
//! There is no "builtin versus external command" anywhere downstream: running
//! an external command is one `Tool` implementation among the others, and the
//! only difference is who writes the contract — Rust for the eight content
//! tools and the three deliveries, a `[[tool]]` entry for a command.
//!
//! One tool is one set of facts: its name, its description, the arguments it
//! declares, what it is for, which rounds it is offered on, and what it needs
//! of this run's worktree. They live together on the tool, because the model
//! only ever reads one copy and a second copy is a copy that drifts. That is
//! why `inventory` builds the real tools rather than describing them again.
//!
//! One of the three extension points.

pub mod availability;
pub mod command;
pub mod content;
pub mod registry;
pub mod signature;
pub mod submit;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::Settings;
use crate::platform::{
    Capabilities, LineRange, Listing, PlatformError, Repo, RepoSource, SearchHit, SearchKind,
};
use crate::record::ContextFile;
use crate::security::{EnvPolicy, PathPolicy};
use crate::worktree::{Worktree, WorktreeError};

pub use command::{CommandContext, CommandTool};
pub use content::{
    FetchRepoFile, ListLocalFiles, ListRepoFiles, ReadLocalFile, SearchLocalRegex,
    SearchRepoKeyword, SearchRepoRegex, SuggestLocalRead, ToolLimits,
};
pub use registry::Registry;
pub use signature::{Arguments, Parameter, Shape, Signature};
pub use submit::{FinishReview, SubmitComment, SubmitSummary, whole_score};

/// Every tool this config offers, in one registry, with no branch in sight.
///
/// What a run can check is a property of its worktree, not of which tools were
/// built, and conflating the two is what this signature exists to prevent.
/// Registration used to depend on preconditions, so the set of names changed
/// from run to run: the prompt could only speak about tools in general, the
/// model had to work out its own abilities from a list it could not check, and
/// reviewbot decided "there is no point looking at the project" on its behalf.
/// Now the names are fixed and the worktree speaks for itself, in each
/// description before a call and in the refusal after one.
pub fn build(
    settings: &Settings,
    paths: PathPolicy,
    worktree: Arc<Worktree>,
    run_dir: &Path,
) -> Registry {
    let mut registry = Registry::new();
    // The three ways of handing something over: a finding, "I have none", and
    // the verdict. None of them touches the worktree, and `Round` keeps each
    // off the turns it does not belong to. "I have none" has to be as
    // available as filing something, or a model with nothing to file will file
    // something.
    registry.register(Box::new(SubmitComment::new()));
    registry.register(Box::new(FinishReview::new()));
    registry.register(Box::new(SubmitSummary::new()));

    let limits = ToolLimits::from_config(&settings.config);
    registry.register(Box::new(ListLocalFiles::new(
        Arc::clone(&worktree),
        paths.clone(),
        limits,
    )));
    registry.register(Box::new(SuggestLocalRead::new(
        Arc::clone(&worktree),
        paths.clone(),
        limits,
    )));
    registry.register(Box::new(ReadLocalFile::new(
        Arc::clone(&worktree),
        paths.clone(),
        limits,
    )));
    registry.register(Box::new(SearchLocalRegex::new(
        Arc::clone(&worktree),
        paths.clone(),
        limits,
    )));
    registry.register(Box::new(ListRepoFiles::new(
        Arc::clone(&worktree),
        paths.clone(),
        limits,
    )));
    registry.register(Box::new(FetchRepoFile::new(
        Arc::clone(&worktree),
        paths.clone(),
        limits,
    )));
    registry.register(Box::new(SearchRepoRegex::new(
        Arc::clone(&worktree),
        paths.clone(),
        limits,
    )));
    registry.register(Box::new(SearchRepoKeyword::new(
        Arc::clone(&worktree),
        paths.clone(),
        limits,
    )));

    for entry in &settings.config.tools {
        registry.register(Box::new(CommandTool::new(
            entry.clone(),
            CommandContext::new(
                EnvPolicy::default(),
                paths.clone(),
                Arc::clone(&worktree),
                run_dir.to_path_buf(),
                settings.options.backoff(),
            ),
            limits,
        )));
    }
    registry
}

/// One row of `tool list`: the contract the model would be given.
#[derive(Clone, Debug, Serialize)]
pub struct ToolListing {
    pub name: String,
    pub purpose: Purpose,
    pub description: String,
    pub parameters: serde_json::Value,
    pub rounds: Vec<Round>,
    /// What a run's worktree has to be able to do before a call to the tool
    /// can be answered. Empty when nothing does. Never a reason for the tool
    /// to be missing: every tool is offered on every run.
    pub preconditions: Vec<String>,
}

/// A `RepoSource` with no run behind it. `tool list` still has to go through
/// `build`, and a `Repo` needs a source, so every method says there is
/// nothing to read rather than inventing an answer.
struct CatalogSource;

impl CatalogSource {
    fn no_run(operation: &'static str) -> PlatformError {
        PlatformError::Request {
            operation,
            host: "catalog".to_string(),
            reason: "there is no run behind this, so there is nothing to read".to_string(),
        }
    }
}

impl RepoSource for CatalogSource {
    fn list_files(&self, _glob: &str) -> Result<Listing, PlatformError> {
        Err(Self::no_run("listing repository files"))
    }

    fn read_file(&self, _path: &str, _lines: Option<LineRange>) -> Result<String, PlatformError> {
        Err(Self::no_run("reading a repository file"))
    }

    fn size(&self, _path: &str) -> Result<u64, PlatformError> {
        Err(Self::no_run("reading a file size"))
    }

    fn search(
        &self,
        _kind: SearchKind,
        _query: &str,
        _glob: Option<&str>,
    ) -> Result<Vec<SearchHit>, PlatformError> {
        Err(Self::no_run("searching the repository"))
    }
}

/// The catalog, read off the real tools rather than described a second time.
///
/// There is no run behind this command, so the tools are built over the widest
/// worktree there is — a whole checkout holding a repository that can answer
/// both search engines — and the preconditions are what say when a run would
/// get a refusal instead. Going through `build` is the point: a row here
/// cannot disagree with what a review registers, because it is what a review
/// registers.
pub fn inventory(settings: &Settings) -> Result<Vec<ToolListing>, crate::config::ConfigError> {
    let worktree = Worktree::Local {
        root: PathBuf::from("/catalog"),
        repo: Some(Repo::new(
            Arc::new(CatalogSource) as Arc<dyn RepoSource>,
            Capabilities::all(),
        )),
    };
    let registry = build(
        settings,
        PathPolicy::for_settings(settings)?,
        Arc::new(worktree),
        Path::new("/catalog"),
    );
    Ok(registry
        .all()
        .into_iter()
        .map(|tool| ToolListing {
            name: tool.name().to_string(),
            purpose: tool.purpose(),
            description: tool.description().to_string(),
            parameters: tool.signature().schema(),
            rounds: tool.rounds().to_vec(),
            preconditions: tool
                .precondition()
                .into_iter()
                .map(str::to_string)
                .collect(),
        })
        .collect())
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{tool}: {reason}")]
    InvalidArguments { tool: String, reason: String },
    #[error("{tool}: {reason}")]
    Rejected { tool: String, reason: String },
    #[error("{tool} timed out after {timeout_ms}ms")]
    Timeout { tool: String, timeout_ms: u64 },
    #[error("{tool} was killed before it finished ({status})")]
    Killed { tool: String, status: String },
    #[error("{tool} could not be run: {reason}")]
    Unavailable { tool: String, reason: String },
}

impl ToolError {
    /// A command that timed out or was killed may have hit a hiccup worth one
    /// more try. A non-zero exit is a result, and a missing binary will stay
    /// missing, so neither is retried.
    pub fn is_retryable(&self) -> bool {
        matches!(self, ToolError::Timeout { .. } | ToolError::Killed { .. })
    }

    /// A worktree call that is not a read of a named file: listings, searches.
    pub(crate) fn failed(tool: &str, error: WorktreeError) -> Self {
        ToolError::Unavailable {
            tool: tool.to_string(),
            reason: error.to_string(),
        }
    }

    /// A worktree call about one path. `TooBig` carries no path of its own —
    /// the tool puts it back into the wording the model reads.
    pub(crate) fn failed_read(tool: &str, path: &str, error: WorktreeError) -> Self {
        match error {
            WorktreeError::TooBig { bytes } => ToolError::Rejected {
                tool: tool.to_string(),
                reason: format!(
                    "{path} is {bytes} bytes, past max_file_bytes; \
                     no part of it can be read, because reading any part means reading all of it. \
                     Use a search to find what you need instead"
                ),
            },
            other => ToolError::Unavailable {
                tool: tool.to_string(),
                reason: format!(
                    "{other} (If the path was wrong, list the files first to see what \
                     the worktree actually has, rather than guessing again.)"
                ),
            },
        }
    }
}

/// What a tool is for. Not where it came from: "builtin or configured" is a
/// fact about this repository's source tree, and the runtime never needs it.
/// What it does need is what an answer means — a checker that could have run
/// and never was called says nobody scanned this chunk and has to leave a
/// trace, while a file read that never happened says nothing at all.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    /// Answers about the code: listings, sizes, bodies, searches.
    Content,
    /// Scans the code and reports findings of its own.
    Check,
    /// How the model hands over a finding or a verdict.
    Delivery,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Content => "content",
            Purpose::Check => "check",
            Purpose::Delivery => "delivery",
        }
    }
}

/// The three kinds of round a run has. A tool is offered on the rounds it
/// belongs to and on no others: a tool the model can see but cannot use this
/// round is an action guaranteed to fail, and a failed tool call reads to it
/// like an answer about the repository.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Round {
    /// Looking at one file's diff, with everything available.
    Investigation,
    /// The last turn of a chunk, after the investigation tools are withdrawn.
    Conclusion,
    /// The one call `merge` makes over the final list of findings.
    Scoring,
}

impl Round {
    pub fn as_str(self) -> &'static str {
        match self {
            Round::Investigation => "investigation",
            Round::Conclusion => "conclusion",
            Round::Scoring => "scoring",
        }
    }
}

/// What the model is told about a tool. Generated from the registry, so the
/// list in the prompt and the `tools` field of the request share a source.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub purpose: Purpose,
}

/// Whatever the tool produced, handed to the model verbatim apart from
/// truncation. Nothing here parses a tool's output format.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutput {
    pub text: String,
    pub omitted_bytes: usize,
    /// Set by a file read that really happened. The review loop copies it into
    /// the trace, which is how `merge` tells a file the model actually fetched
    /// from one it named in its evidence without ever asking for it.
    pub context_file: Option<ContextFile>,
    /// A finding the model just handed over. Investigation tools leave this
    /// empty; the review loop collects it without matching on a tool name.
    pub submission: Option<serde_json::Value>,
    /// Set by a call that ends this file's review without filing anything. The
    /// loop needs an end signal that is not a finding, and it must not have to
    /// recognise one by tool name or by reading a body.
    pub finished: bool,
}

impl ToolOutput {
    pub fn new(text: String) -> Self {
        Self {
            text,
            omitted_bytes: 0,
            context_file: None,
            submission: None,
            finished: false,
        }
    }

    pub fn clipped(text: String, omitted_bytes: usize) -> Self {
        Self {
            text,
            omitted_bytes,
            context_file: None,
            submission: None,
            finished: false,
        }
    }

    pub fn with_context_file(mut self, file: ContextFile) -> Self {
        self.context_file = Some(file);
        self
    }

    pub fn with_submission(mut self, finding: serde_json::Value) -> Self {
        self.submission = Some(finding);
        self
    }

    /// This call was the model saying it is done with nothing to file.
    pub fn finishing(mut self) -> Self {
        self.finished = true;
        self
    }

    /// What goes into `function_call_output`. A clip says so, otherwise the
    /// model reads a truncated file as a short one.
    pub fn for_model(&self) -> String {
        match self.omitted_bytes {
            0 => self.text.clone(),
            omitted => format!("{}\n[{omitted} more bytes not shown]", self.text),
        }
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    /// Has to describe what the tool really does, this run, with the numbers
    /// it will really enforce. The model has no other way to notice an
    /// overstatement.
    fn description(&self) -> &str;

    /// What it takes. The schema the model is shown and the check its call is
    /// put through are both derived from this one declaration.
    fn signature(&self) -> &Signature;

    fn purpose(&self) -> Purpose;

    /// The rounds this tool is offered on. What the model can see in a round
    /// is exactly what it can call in that round.
    fn rounds(&self) -> &'static [Round];

    /// Why this run's worktree cannot answer this tool, when it cannot. The
    /// same words the description was written from, so what the model is told
    /// before a call and what it is told after one cannot disagree.
    ///
    /// It never decides whether the tool is registered — the model is offered
    /// every tool on every run. Leaving a tool out instead used to look safer
    /// (what the model sees equals what it can call) but it means the set of
    /// names changes with the run, so the prompt describes tools that are not
    /// there and the model has to infer its own abilities from a list it
    /// cannot check.
    fn unavailable(&self) -> Option<&str> {
        None
    }

    /// What `tool list` prints as the condition a run's worktree has to meet.
    /// Empty when nothing does. Never a reason for the tool to be missing.
    fn precondition(&self) -> Option<&'static str> {
        None
    }

    fn offered_on(&self, round: Round) -> bool {
        self.rounds().contains(&round)
    }

    fn execute(&self, arguments: &serde_json::Value) -> Result<ToolOutput, ToolError>;

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.signature().schema(),
            purpose: self.purpose(),
        }
    }
}

impl From<ToolSchema> for crate::protocol::ToolSchema {
    fn from(schema: ToolSchema) -> Self {
        Self {
            name: schema.name,
            description: schema.description,
            parameters: schema.parameters,
        }
    }
}
