use crate::budget::BudgetError;
use crate::config::ConfigError;
use crate::platform::PlatformError;
use crate::protocol::ProtocolError;
use crate::record::RecordError;
use crate::security::PatternError;
use crate::tool::ToolError;
use crate::worktree::WorktreeError;

use super::input::DiffError;
use super::prompt::PromptError;

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
    Redact(#[from] PatternError),
    #[error(transparent)]
    Prompt(#[from] PromptError),
    #[error("cannot read the input: {reason}")]
    UnreadableInput { reason: String },
    /// The first turn of a chunk does not fit. Nothing the tool loop can do
    /// about it: the chunk limit `plan` computed was wrong.
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
    #[error("cannot encode tool schemas: {source}")]
    Schemas { source: serde_json::Error },
}
