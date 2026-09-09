//! What the model may call. One trait, one registry, one construction path.
//! There is no "builtin versus external command" anywhere downstream: running
//! an external command is one `Tool` implementation among the others, and the
//! only difference is who writes the contract — Rust for the four content
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

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::Settings;
use crate::record::ContextFile;
use crate::security::{EnvPolicy, PathPolicy};
use crate::worktree::{Abilities, Widest, WorktreeSource};

pub use availability::precondition;
pub use command::{CommandContext, CommandTool};
pub use content::{ListFiles, ReadFile, SearchCode, StatFile, ToolLimits, WorktreeContext};
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
    worktree: Arc<dyn WorktreeSource>,
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
    let context = WorktreeContext::new(Arc::clone(&worktree), paths.clone(), limits);
    registry.register(Box::new(ListFiles::new(context.clone())));
    registry.register(Box::new(StatFile::new(context.clone())));
    registry.register(Box::new(ReadFile::new(context.clone())));
    registry.register(Box::new(SearchCode::new(context)));

    for entry in &settings.config.tools {
        registry.register(Box::new(CommandTool::new(
            entry.clone(),
            CommandContext::new(
                EnvPolicy::default(),
                paths.clone(),
                Arc::clone(&worktree),
                settings.config.review.max_tool_output_bytes,
                settings.options.backoff(),
            ),
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

/// The catalog, read off the real tools rather than described a second time.
///
/// There is no run behind this command, so the tools are built over the widest
/// worktree there is — a whole checkout with a platform that can match
/// expressions — and the preconditions are what say when a run would get a
/// refusal instead. Going through `build` is the point: a row here cannot
/// disagree with what a review registers, because it is what a review
/// registers.
pub fn inventory(settings: &Settings) -> Result<Vec<ToolListing>, crate::config::ConfigError> {
    let registry = build(
        settings,
        PathPolicy::for_settings(settings)?,
        Arc::new(Widest),
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
                .needs()
                .iter()
                .map(|ability| precondition(ability).to_string())
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

    /// What this tool needs of the run's worktree. The declaration: the
    /// catalog prints it, and `unavailable` is matched against it.
    ///
    /// It never decides whether the tool is registered — the model is offered
    /// every tool on every run. Leaving a tool out instead used to look safer
    /// (what the model sees equals what it can call) but it means the set of
    /// names changes with the run, so the prompt describes tools that are not
    /// there and the model has to infer its own abilities from a list it
    /// cannot check.
    fn needs(&self) -> Abilities {
        Abilities::empty()
    }

    /// Why this run's worktree cannot answer this tool, when it cannot.
    /// Derived from `needs`, never written a second time, so the catalog's
    /// preconditions and the refusal cannot disagree.
    fn unavailable(&self) -> Option<&str> {
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
