//! The six stages. Each one ends by writing a checkpoint; none of them knows
//! what runs before or after it. The order lives only in `lib.rs`.

pub mod adapters;
pub mod context;
pub mod error;
pub mod input;
pub mod merge;
pub mod orient;
pub mod plan;
pub mod prompt;
pub mod publish;
pub mod report;
pub mod review;

#[cfg(test)]
mod fixture;

pub use adapters::{Adapters, Equipment};
pub use context::StageContext;
pub use error::StageError;
pub use input::Source;
