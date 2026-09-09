//! Checks, not enforcement.
//!
//! Everything here is a function the caller chooses to call. The call site
//! for path checks is fixed at `tool`: every read the model can see arrives
//! through a builtin tool, so checking there covers all of them. This is an
//! implementation guarantee held up by tests and review, not by the type
//! system.

pub mod path;
pub mod redact;
pub mod subprocess;

pub use crate::common::{Truncated, truncate};
pub use path::{PathPolicy, PathRejection};
pub use redact::Redactor;
pub use subprocess::{EnvPolicy, Limits};
