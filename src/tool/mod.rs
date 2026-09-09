//! What the model may call. External commands and builtins are two
//! implementations of one trait behind one registry, so nothing downstream
//! has to ask which kind it is looking at.
//!
//! One tool is one set of facts: its name, its description, the arguments it
//! declares, what it is for, which rounds it is offered on, and what has to be
//! true before it exists at all. They live together on the tool, because the
//! model only ever reads one copy and a second copy is a copy that drifts.
//!
//! One of the three extension points.

pub mod builtin;
pub mod command;
pub mod registry;
pub mod signature;
pub mod submit;

use serde::{Deserialize, Serialize};

use crate::record::ContextFile;

use crate::config::Settings;

pub use builtin::{
    BuiltinSpec, ListFiles, ReadFile, SearchCode, StatFile, ToolLimits, WorktreeContext,
    builtin_specs,
};
pub use command::{CommandContext, CommandTool};
pub use registry::Registry;
pub use signature::{Arguments, Parameter, Shape, Signature};
pub use submit::{FinishReview, SubmitComment, SubmitSummary, whole_score};

/// One row of `tool list`: every builtin and every `[[tool]]` entry, with the
/// contract the model would be given.
#[derive(Clone, Debug, Serialize)]
pub struct ToolListing {
    pub name: String,
    pub purpose: Purpose,
    pub description: String,
    pub parameters: serde_json::Value,
    pub rounds: Vec<Round>,
    /// What has to be true of this run before the tool exists at all. Empty
    /// when nothing does.
    pub preconditions: Vec<String>,
}

/// Builtins first, then `[[tool]]`. There is no run behind this command, so
/// the content tools are described as they would be for the widest worktree
/// there is — a checkout with a platform that can match expressions — and the
/// preconditions are what say when they would be narrower or absent.
pub fn inventory(settings: &Settings) -> Vec<ToolListing> {
    let reach = crate::worktree::Reach {
        content: crate::worktree::Content::Checkout,
        search: crate::worktree::Search::Regex,
    };
    let mut rows = vec![
        ToolListing {
            name: FinishReview::NAME.to_string(),
            purpose: Purpose::Delivery,
            description: FinishReview::description_text().to_string(),
            parameters: FinishReview::new().signature().schema(),
            rounds: FinishReview::ROUNDS.to_vec(),
            preconditions: Vec::new(),
        },
        ToolListing {
            name: SubmitComment::NAME.to_string(),
            purpose: Purpose::Delivery,
            description: SubmitComment::description_text().to_string(),
            parameters: SubmitComment::new().signature().schema(),
            rounds: SubmitComment::ROUNDS.to_vec(),
            preconditions: Vec::new(),
        },
        ToolListing {
            name: SubmitSummary::NAME.to_string(),
            purpose: Purpose::Delivery,
            description: SubmitSummary::description_text().to_string(),
            parameters: SubmitSummary::new().signature().schema(),
            rounds: SubmitSummary::ROUNDS.to_vec(),
            preconditions: Vec::new(),
        },
    ];
    for spec in builtin_specs(reach, ToolLimits::from_config(&settings.config)) {
        let mut preconditions = vec!["the worktree has code to read".to_string()];
        if spec.requires_search {
            preconditions.push("the worktree can answer a search".to_string());
        }
        if spec.requires_checkout {
            preconditions.push("the worktree is a whole checkout".to_string());
        }
        rows.push(ToolListing {
            name: spec.name.to_string(),
            purpose: Purpose::Content,
            description: spec.description,
            parameters: spec.parameters,
            rounds: vec![Round::Investigation],
            preconditions,
        });
    }
    for entry in &settings.config.tools {
        let mut preconditions = vec!["the worktree has code to read".to_string()];
        if entry.requires_checkout {
            preconditions.push("the worktree is a whole checkout".to_string());
        }
        if entry.requires_build {
            preconditions.push("[security].allow_build_tools and a sandbox".to_string());
        }
        rows.push(ToolListing {
            name: entry.name.clone(),
            purpose: Purpose::Check,
            description: entry.description.clone(),
            parameters: Signature::from_entry(entry).schema(),
            rounds: vec![Round::Investigation],
            preconditions,
        });
    }
    rows
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
/// What it does need is what an answer means — a checker that was registered
/// and never called says nobody scanned this chunk and has to leave a trace,
/// while a file read that never happened says nothing at all.
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

    /// Preconditions checked at startup. An unmet one means the tool is not
    /// registered at all, rather than registered and always failing.
    ///
    /// The worktree always exists, so "does it need one" answers nothing.
    /// What a compiler, a history walk or a cross-file analysis needs is a
    /// whole checkout, and a worktree holding the files fetched so far is not
    /// that: a `.c` file without its project headers earns a screen of
    /// missing includes, which is worse than not running at all.
    fn requires_checkout(&self) -> bool {
        false
    }

    fn requires_build(&self) -> bool {
        false
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
