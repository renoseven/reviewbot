//! Subcommand dispatch and the mapping from the library's error type to an
//! exit code. No business logic lives here.

pub mod app;
pub mod args;
mod logging;
pub mod render;
mod screen;
mod status;

pub use app::run;
