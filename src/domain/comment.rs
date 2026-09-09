use serde::{Deserialize, Serialize};

use super::confidence::Confidence;

/// A publishable finding. All six fields are required before it may go out.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Comment {
    pub target: CommentTarget,
    /// The problem. The fix goes in `suggestion`.
    pub body: String,
    pub suggestion: String,
    pub confidence: Confidence,
    /// The 0-100 integer the model gave. Stored and published verbatim.
    pub confidence_score: u8,
    pub trace_id: String,
}

/// File plus line range. `line` is `None` for a file-level comment.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CommentTarget {
    pub path: String,
    pub line: Option<u32>,
    pub end_line: Option<u32>,
}
