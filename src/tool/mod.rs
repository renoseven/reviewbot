//! What the model may call. External commands and builtins are two
//! implementations of one trait behind one registry, so nothing downstream
//! has to ask which kind it is looking at.
//!
//! One of the three extension points.

pub mod builtin;
pub mod command;
pub mod registry;
pub mod submit;

use serde::{Deserialize, Serialize};

use crate::record::ContextFile;

use crate::config::Settings;

pub use builtin::{
    BuiltinSpec, ListRepoFiles, ListWorktreeFiles, ReadRepoFile, ReadWorktreeFile, RepoContext,
    SearchRepo, SearchWorktree, StatRepoFile, StatWorktreeFile, ToolLimits, WorktreeContext,
    builtin_specs,
};
pub use command::{CommandContext, CommandTool};
pub use registry::Registry;
pub use submit::{SubmitComment, SubmitSummary, whole_score};

/// One row of `tool list`: every builtin and every `[[tool]]` entry, plus
/// whether this invocation would actually register it.
#[derive(Clone, Debug, Serialize)]
pub struct ToolListing {
    pub name: String,
    pub origin: Origin,
    pub description: String,
    pub parameters: serde_json::Value,
    pub registered: bool,
    /// Why it would not register, when it would not. `None` when it would.
    pub reason: Option<String>,
}

/// Builtins first, then `[[tool]]`. A missing worktree leaves disk tools
/// and every config command unregistered; `search_repo` stays visible as a
/// capability-gated builtin even when it would not register.
pub fn inventory(settings: &Settings) -> Vec<ToolListing> {
    let has_worktree = settings.options.worktree.is_some();
    let mut rows = Vec::new();
    rows.push(ToolListing {
        name: SubmitComment::NAME.to_string(),
        origin: Origin::Builtin,
        description: SubmitComment::description_text().to_string(),
        parameters: SubmitComment::parameters_schema(),
        registered: true,
        reason: None,
    });
    // Listed although no review round advertises it: this is the only place
    // the whole set of tools is visible, and a tool the model can call that
    // does not appear here is a gap in exactly the wrong direction.
    rows.push(ToolListing {
        name: SubmitSummary::NAME.to_string(),
        origin: Origin::Builtin,
        description: SubmitSummary::description_text().to_string(),
        parameters: SubmitSummary::parameters_schema(),
        registered: true,
        reason: Some("merge stage only".to_string()),
    });
    for spec in builtin_specs(ToolLimits::from_config(&settings.config)) {
        let (registered, reason) = builtin_registration(&spec, has_worktree);
        rows.push(ToolListing {
            name: spec.name.to_string(),
            origin: Origin::Builtin,
            description: spec.description,
            parameters: spec.parameters,
            registered,
            reason,
        });
    }
    for entry in &settings.config.tools {
        let (registered, reason) = config_registration(entry, has_worktree);
        rows.push(ToolListing {
            name: entry.name.clone(),
            origin: Origin::Config,
            description: entry.description.clone(),
            parameters: CommandTool::parameters_for(entry),
            registered,
            reason,
        });
    }
    rows
}

fn builtin_registration(spec: &BuiltinSpec, has_worktree: bool) -> (bool, Option<String>) {
    if spec.capability_gate == Some("code_search") {
        return (false, Some("needs code_search".to_string()));
    }
    if spec.requires_worktree && !has_worktree {
        return (false, Some("no worktree".to_string()));
    }
    if !spec.requires_worktree && spec.capability_gate.is_none() {
        // Repo tools need a URL / platform. `tool list` has neither.
        return (false, Some("no URL".to_string()));
    }
    (true, None)
}

fn config_registration(
    entry: &crate::config::ToolEntry,
    has_worktree: bool,
) -> (bool, Option<String>) {
    if !has_worktree {
        return (false, Some("no worktree".to_string()));
    }
    if entry.requires_worktree && !has_worktree {
        return (false, Some("requires_worktree".to_string()));
    }
    (true, None)
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
    #[error("{tool} is not implemented yet")]
    NotImplemented { tool: String },
}

impl ToolError {
    /// A command that timed out or was killed may have hit a hiccup worth one
    /// more try. A non-zero exit is a result, and a missing binary will stay
    /// missing, so neither is retried.
    pub fn is_retryable(&self) -> bool {
        matches!(self, ToolError::Timeout { .. } | ToolError::Killed { .. })
    }
}

/// Where a tool came from, for `tool list`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Builtin,
    Config,
}

/// What the model is told about a tool. Generated from the registry, so the
/// list in the prompt and the `tools` field of the request share a source.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub origin: Origin,
    /// Stays in the request after investigation tools have been withdrawn.
    pub concluding: bool,
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
}

impl ToolOutput {
    pub fn new(text: String) -> Self {
        Self {
            text,
            omitted_bytes: 0,
            context_file: None,
            submission: None,
        }
    }

    pub fn clipped(text: String, omitted_bytes: usize) -> Self {
        Self {
            text,
            omitted_bytes,
            context_file: None,
            submission: None,
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

    /// Has to describe what the tool really does. The model has no other way
    /// to notice an overstatement.
    fn description(&self) -> &str;

    /// JSON Schema for the arguments.
    fn parameters(&self) -> serde_json::Value;

    fn origin(&self) -> Origin;

    /// Preconditions checked at startup. An unmet one means the tool is not
    /// registered at all, rather than registered and always failing.
    fn requires_worktree(&self) -> bool {
        false
    }

    fn requires_build(&self) -> bool {
        false
    }

    /// How findings are delivered stays callable after the investigation
    /// tools have been taken away.
    fn available_when_concluding(&self) -> bool {
        false
    }

    fn execute(&self, arguments: &serde_json::Value) -> Result<ToolOutput, ToolError>;

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters(),
            origin: self.origin(),
            concluding: self.available_when_concluding(),
        }
    }
}
