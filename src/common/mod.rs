//! Shared infrastructure: no functional owner of its own.
//!
//! Retry curves, path expansion, credentials, output clipping and the HTTP
//! transport are used by more than one adapter. They live here so `config` is
//! not a home for a backoff or a secret, `security` is not a home for a
//! string cut, and `platform` is not the only copy of the retry loop.

pub mod backoff;
pub mod http;
pub mod paths;
pub mod secret;
pub mod truncate;

pub use backoff::Backoff;
pub use secret::{Secret, SecretError, SecretSource};
pub use truncate::{Truncated, truncate};
