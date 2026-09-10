use serde::{Deserialize, Serialize};

use super::confidence::Confidence;
use super::severity::Severity;

/// A publishable finding. Every field is required before it may go out.
///
/// Two numbers, because they answer two questions a reader has to weigh
/// separately: how much it matters if this is real (`severity_score`), and how
/// sure the model is that it is real (`confidence_score`). Squashed into one,
/// a certain naming quibble outranks an uncertain memory error, which is the
/// wrong way round for whoever reads the top of the list first.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Comment {
    pub target: CommentTarget,
    /// The problem. The fix goes in `suggestion`.
    pub body: String,
    pub suggestion: String,
    pub severity: Severity,
    /// The 0-100 integer the model gave. Stored and published verbatim.
    pub severity_score: u8,
    pub confidence: Confidence,
    /// The 0-100 integer the model gave. Stored and published verbatim.
    pub confidence_score: u8,
    pub trace_id: String,
}

/// File plus line range. `start_line` is `None` for a file-level comment.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CommentTarget {
    pub path: String,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
}
