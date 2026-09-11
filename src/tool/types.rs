use serde::{Deserialize, Serialize};

use crate::record::ContextFile;

use super::error::ToolError;
use super::signature::Signature;

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
