//! What the model may call. One trait, one registry, one construction path.
//!
//! One of the three extension points.

pub mod availability;
pub mod catalog;
pub mod command;
pub mod content;
pub mod error;
pub mod registry;
pub mod signature;
pub mod submit;
pub mod types;

pub use catalog::{ToolListing, inventory};
pub use command::{CommandContext, CommandTool};
pub use content::{
    FetchRepoFile, ListLocalFiles, ListRepoFiles, ReadLocalFile, SearchLocalRegex,
    SearchRepoKeyword, SearchRepoRegex, SuggestLocalRead, ToolLimits,
};
pub use error::ToolError;
pub use registry::Registry;
pub use signature::{Arguments, Parameter, Shape, Signature};
pub use submit::{FinishReview, SubmitComment, SubmitSummary, whole_score};
pub use types::{Purpose, Round, Tool, ToolOutput, ToolSchema};
