//! The identity of one run and everything it writes down: `run_id`, the run
//! directory, the lock, `meta.json`, atomic checkpoints, and traces.
//!
//! It does not know the order of the stages; it only records which of them
//! finished.

pub mod error;
pub mod layout;
pub mod listing;
pub mod lock;
pub mod meta;
pub mod recorder;
pub mod run_id;
pub mod storage;
pub mod trace;

pub use error::RecordError;
pub use listing::{
    DEFAULT_KEEP, ListedTrace, PruneReport, RunRow, RunShow, Runs, TraceListing, WARN_AFTER_RUNS,
};
pub use lock::DirLock;
pub use meta::{InputKind, InputRecord, Meta, Reentry, RunIdentity};
pub use recorder::Recorder;
pub use run_id::InputIdentity;
pub use storage::{LocalStorage, Storage};
pub use trace::{Check, ContextFile, PublishedView, ToolCall, Trace};
