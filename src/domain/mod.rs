//! Shared vocabulary: the handful of types every layer above agrees on.
//!
//! `domain` depends on nothing and holds no logic. Behaviour lives with its
//! owner: line alignment in `stage::merge`, score bands in `stage::merge`,
//! traces in `record`.

pub mod changeset;
pub mod comment;
pub mod confidence;
pub mod severity;
pub mod stage;

pub use changeset::{ChangeSet, DEV_NULL, FileChange, Hunk, Locator, Narrative};
pub use comment::{Comment, CommentTarget};
pub use confidence::Confidence;
pub use severity::Severity;
pub use stage::Stage;
