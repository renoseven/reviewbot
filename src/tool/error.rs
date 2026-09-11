use crate::worktree::WorktreeError;

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
