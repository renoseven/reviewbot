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

use crate::domain::Stage;

/// What a finished stage proved, kept as numbers until a consumer needs
/// words. Both terminal shapes ask this type for those words, so a new screen
/// cannot quietly give a count a second name.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Input {
        files: usize,
    },
    Triage {
        chunks: usize,
        skipped: usize,
    },
    Review {
        chunks: usize,
        unreviewed: usize,
    },
    Merge {
        comments: usize,
        overall: Option<u8>,
    },
    Report,
    Publish {
        posted: usize,
        already_there: usize,
        asked: bool,
    },
}

impl Outcome {
    /// The sentence belongs to the fact rather than either renderer. Keeping
    /// it here makes the retained pipe history and the changing terminal
    /// block disagree only if they were handed different events.
    pub fn sentence(&self) -> String {
        match self {
            Self::Input { files } => format!("{files} {}", count(*files, "file", "files")),
            Self::Triage { chunks, skipped } => format!(
                "{chunks} {}, {skipped} {} skipped",
                count(*chunks, "chunk", "chunks"),
                count(*skipped, "file", "files")
            ),
            Self::Review { chunks, unreviewed } => {
                let reviewed = format!("{chunks} {} reviewed", count(*chunks, "chunk", "chunks"));
                match unreviewed {
                    0 => reviewed,
                    unreviewed => format!(
                        "{reviewed}, {unreviewed} {} unreviewed",
                        count(*unreviewed, "file", "files")
                    ),
                }
            }
            Self::Merge { comments, overall } => format!(
                "{comments} {}, {}",
                count(*comments, "comment", "comments"),
                match overall {
                    Some(score) => format!("overall {score} / 100"),
                    None => "not scored".to_string(),
                }
            ),
            Self::Report => "report.md and summary.json written".to_string(),
            Self::Publish {
                posted,
                already_there,
                asked,
            } => match asked {
                true => format!(
                    "{posted} {} published, {already_there} already on the change",
                    count(*posted, "comment", "comments")
                ),
                false => "nothing posted: this run was not asked to publish".to_string(),
            },
        }
    }
}

fn count(value: usize, singular: &'static str, plural: &'static str) -> &'static str {
    match value {
        1 => singular,
        _ => plural,
    }
}

/// One thing that became true during a run, in the vocabulary of the stage
/// it happened in.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Before the run has an id. What happens first is working out which
    /// change this is — for a URL that means fetching the merge request and its
    /// diff, which is seconds of a watcher having nothing else to say — and then
    /// taking the run directory the id names. Said so that those seconds are
    /// accounted for rather than looking like a run that has not started.
    Opening,
    /// The run has a directory and an id, and has spent nothing yet.
    /// `input` names what is being reviewed the way the run's own record
    /// does: the platform locator, or where the diff came from.
    RunStarted {
        run_id: String,
        run_dir: PathBuf,
        model: String,
        input: String,
        /// The checkout named by the caller. `None` means the worktree is the
        /// run's own directory, opened empty and filled only as needed.
        worktree: Option<PathBuf>,
    },
    /// The preamble both `triage` and `review` need is being assembled: the
    /// list of files this change touches, and the digest of the repository's
    /// layout — which is a tree fetch, and on a large project several seconds
    /// of it. It belongs to neither stage, so the checklist has no row for it
    /// and a watcher would otherwise have nothing to say for those seconds.
    Preparing,
    /// A stage is about to run, or about to be skipped. Both are said, so a
    /// watcher can show all six without knowing which of them cost anything.
    StageStarted { stage: Stage },
    /// The result stays typed here because prose is a renderer's concern.
    /// `from_checkpoint` says an earlier attempt established it, which a
    /// watcher must not mistake for work this process did.
    StageFinished {
        stage: Stage,
        outcome: Outcome,
        from_checkpoint: bool,
    },
    /// The review stage has moved on to a file. `index` / `of` are that
    /// file's place in the plan, counted from 1 — including files an earlier
    /// attempt already finished, so a re-entered run does not look like it
    /// started over.
    ///
    /// A file too big for one request is reviewed in several pieces, so `piece`
    /// says which one this is and `pieces` how many there are — 1 of 1 for a
    /// file that fitted. What a watcher shows is files: pieces are how the diff
    /// had to be cut, not progress through the change.
    Chunk {
        index: usize,
        of: usize,
        path: String,
        piece: usize,
        pieces: usize,
    },
    /// One round of the tool loop inside the current file. `of` is the ceiling
    /// this run worked out, not a forecast: most files end far short of it.
    /// Never emitted for the concluding turn, which is not a round of the loop
    /// — saying so was how a screen came to show `round 13/12`.
    Round { round: u32, of: u32 },
    /// The loop is over and the model is being asked to conclude: either it
    /// used every round, or the conversation reached the window. `why` is the
    /// same sentence the trace and the report carry.
    Concluding { why: String },
    /// A tool the model asked for, as the call goes out. Its answer goes to
    /// the model and to the trace, not here.
    Tool { name: String },
    /// The tool call has returned. `ms` is the same elapsed measurement kept
    /// in the trace, so display timing cannot disagree with the run record.
    ToolDone { name: String, ms: u64 },
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

#[cfg(test)]
mod tests {
    use super::Outcome;

    #[test]
    fn input_sentence_names_files() {
        assert_eq!(Outcome::Input { files: 12 }.sentence(), "12 files");
    }

    #[test]
    fn triage_sentence_pluralizes_each_count() {
        assert_eq!(
            Outcome::Triage {
                chunks: 4,
                skipped: 1,
            }
            .sentence(),
            "4 chunks, 1 file skipped"
        );
    }

    #[test]
    fn review_sentence_omits_an_empty_unreviewed_count() {
        assert_eq!(
            Outcome::Review {
                chunks: 3,
                unreviewed: 0,
            }
            .sentence(),
            "3 chunks reviewed"
        );
        assert_eq!(
            Outcome::Review {
                chunks: 3,
                unreviewed: 2,
            }
            .sentence(),
            "3 chunks reviewed, 2 files unreviewed"
        );
    }

    #[test]
    fn merge_sentence_keeps_scores_optional() {
        assert_eq!(
            Outcome::Merge {
                comments: 4,
                overall: Some(54),
            }
            .sentence(),
            "4 comments, overall 54 / 100"
        );
        assert_eq!(
            Outcome::Merge {
                comments: 4,
                overall: None,
            }
            .sentence(),
            "4 comments, not scored"
        );
    }

    #[test]
    fn report_sentence_names_both_artifacts() {
        assert_eq!(
            Outcome::Report.sentence(),
            "report.md and summary.json written"
        );
    }

    #[test]
    fn publish_sentence_distinguishes_intent() {
        assert_eq!(
            Outcome::Publish {
                posted: 2,
                already_there: 1,
                asked: true,
            }
            .sentence(),
            "2 comments published, 1 already on the change"
        );
        assert_eq!(
            Outcome::Publish {
                posted: 0,
                already_there: 0,
                asked: false,
            }
            .sentence(),
            "nothing posted: this run was not asked to publish"
        );
    }
}
