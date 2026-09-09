//! What a run says about itself while it is still running.
//!
//! One channel with two ends and no middle: the stages emit as the work
//! happens, and whoever called `review` decides what that looks like. The
//! terminal is the first consumer — a status screen carrying the same fields
//! as the final summary, filled in as they become known — but nothing here
//! knows that, and nothing here renders.
//!
//! Not an extension point. The three traits a user extends are `platform`,
//! `protocol` and `tool`: each of them changes what a run does, and each is
//! reachable from the config. This is the other kind of trait — an output
//! port for one process's own progress, chosen by the caller in code and
//! invisible to the config. Nothing a run decides may turn on who is
//! watching, which is why `Silent` has to be a complete answer.
//!
//! Beside `tracing`, never over it. A log line is prose for whoever reads it
//! afterwards; an event is a typed fact about a run that is still going. The
//! two channels carry the same moments in different words, and neither is
//! derived from the other.

use std::path::PathBuf;

/// One thing that became true during a run, in the vocabulary of the stage
/// it happened in.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The run has a directory and an id, and has spent nothing yet.
    /// `input` names what is being reviewed the way the run's own record
    /// does: the platform locator, or where the diff came from.
    RunStarted {
        run_id: String,
        run_dir: PathBuf,
        model: String,
        input: String,
    },
    /// A stage is about to run, or about to be skipped. Both are said, so a
    /// watcher can show all six without knowing which of them cost anything.
    StageStarted { number: u8, name: &'static str },
    /// `detail` is what a finished stage leaves on the screen: the counts it
    /// is answerable for, in the same terms the final summary uses, plus —
    /// for a stage an earlier attempt already finished — that they were read
    /// back off a checkpoint rather than worked out again.
    StageFinished {
        number: u8,
        name: &'static str,
        detail: String,
    },
    /// One chunk of the review stage, counted from 1 for the reader rather
    /// than from 0 for the loop.
    Chunk {
        index: usize,
        of: usize,
        path: String,
    },
    /// One round of the tool loop inside the current chunk. `of` is the
    /// configured ceiling, not a forecast: most chunks end far short of it.
    Round { round: u32, of: u32 },
    /// A tool the model asked for, as the call goes out. Its answer goes to
    /// the model and to the trace, not here.
    Tool { name: String },
    /// What the run has spent, after a model call settled. `budget` is
    /// `None` when this run has no ceiling, which a watcher has to spell out
    /// rather than leave blank.
    Spend {
        spent: f64,
        budget: Option<f64>,
        currency: String,
    },
}

/// Where the events go. One implementation per caller: the CLI's status
/// screen, a test that records what it was told, `Silent` for everyone else.
pub trait Progress {
    /// Called from the stage doing the work, in the order things happen.
    ///
    /// `&self` because a run is serial and every stage shares the one
    /// watcher; an implementation that accumulates keeps its own interior
    /// mutability rather than forcing a mutable borrow through the whole
    /// stage context. Emitting must not fail and must not block: a watcher
    /// that cannot keep up drops what it cannot show, because no part of a
    /// review may wait on the thing displaying it.
    fn emit(&self, event: Event);
}

/// The watcher for a caller with nothing to show: tests, and the CLI until
/// the status screen lands.
pub struct Silent;

impl Progress for Silent {
    fn emit(&self, _event: Event) {}
}
