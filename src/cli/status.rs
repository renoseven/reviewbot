//! What the terminal knows about a run while the run is still going.
//!
//! One set of facts, two ways of showing them. A terminal gets the block in
//! `screen`, repainted on a timer because a model call blocks the run for tens
//! of seconds and a screen that only moves when something is reported is a
//! dead screen for exactly those stretches. A pipe gets appended lines: cursor
//! motion is useful only where there is a cursor, and what makes CI output
//! worth keeping is that every finished stage is still in it afterwards.
//!
//! The facts live here rather than in either renderer, so the two cannot come
//! to disagree about what the run said.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reviewbot::domain::Stage;
use reviewbot::progress::{Event, Outcome, Progress};

use super::screen::Screen;

/// A consumer of one run's progress. Shared by all six stages, so everything
/// it holds is behind a lock: the terminal's painter reads the same facts from
/// its own thread.
pub struct Status {
    state: Arc<Mutex<State>>,
    view: View,
}

enum View {
    /// A terminal, painted by `screen`'s thread rather than by `emit`.
    Terminal(Screen),
    /// Anything else: one line per thing worth keeping.
    Pipe(Mutex<Box<dyn Write + Send>>),
}

impl Status {
    /// Take the bottom of the terminal for the duration of the run, or fall
    /// back to plain lines when this terminal will not hold a block. Some
    /// things that pass for a terminal never answer the question an inline
    /// viewport has to ask, and a run with nothing at all to show would be a
    /// worse answer to that than a run whose progress simply scrolls.
    pub fn terminal(color: bool) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        match Screen::start(Arc::clone(&state), color) {
            Some(screen) => Self {
                state,
                view: View::Terminal(screen),
            },
            None => Self::pipe(Box::new(std::io::stdout())),
        }
    }

    /// What the caller already knows before the run does: which model was
    /// selected, what it was pointed at, and which checkout it may read. The
    /// run replaces all three the moment it can name itself, but until then a
    /// screen that shows them is a screen that says what is about to happen.
    pub fn about(self, input: &str, model: Option<&str>, worktree: Option<&Path>) -> Self {
        if let Ok(mut state) = self.state.lock() {
            state.input = input.to_string();
            state.model = model.unwrap_or_default().to_string();
            state.worktree = worktree.map(Path::to_path_buf);
        }
        self
    }

    pub fn pipe(writer: Box<dyn Write + Send>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            view: View::Pipe(Mutex::new(writer)),
        }
    }

    /// Give the terminal back before anything else writes to it. Consuming
    /// `self` is the point: the final summary and any failure message must not
    /// land in a block that is still being repainted, and there is no way to
    /// ask for that ordering here.
    pub fn finish(self) {
        match self.view {
            View::Terminal(screen) => screen.stop(),
            View::Pipe(_) => {}
        }
    }
}

impl Progress for Status {
    fn emit(&self, event: Event) {
        let line = match self.state.lock() {
            Ok(mut state) => state.apply(event, Instant::now()),
            Err(_) => return,
        };
        // A terminal is repainted from the facts, so there is nothing to write
        // here; a pipe only ever gains lines.
        if let (View::Pipe(writer), Some(line)) = (&self.view, line)
            && let Ok(mut writer) = writer.lock()
        {
            let _ = writer.write_all(line.as_bytes());
            let _ = writer.flush();
        }
    }
}

/// Everything either renderer might show, in the order it becomes known.
pub(super) struct State {
    pub(super) run_id: String,
    pub(super) input: String,
    pub(super) model: String,
    pub(super) worktree: Option<PathBuf>,
    pub(super) stages: Vec<StageRow>,
    /// Which piece of the current file, and how many pieces it was cut into.
    /// `None` while nothing is being reviewed; `(1, 1)` for a file that fitted
    /// in one request, which is the ordinary case and says nothing worth a row.
    pub(super) piece: Option<(usize, usize)>,
    pub(super) path: Option<String>,
    /// Files, which are not chunks: one file too big for a single request is
    /// reviewed as several. Both are counted so neither can be read as the
    /// other.
    pub(super) files_seen: usize,
    pub(super) files_total: Option<usize>,
    pub(super) spend: Option<String>,
    /// Which turn of the tool loop, and the ceiling that ends the chunk.
    pub(super) exchange: Option<(u32, u32)>,
    pub(super) waiting_since: Option<Instant>,
    pub(super) tool: Option<(String, Instant)>,
    /// Why the loop is over, once it is: the model is being asked to conclude
    /// on what it has rather than on everything it wanted.
    pub(super) concluding: bool,
    /// Since when the run has been working out which change this is. Cleared by
    /// the first stage, which is the moment there is something better to say.
    pub(super) opening: Option<Instant>,
    input_files: Option<usize>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            run_id: String::new(),
            input: String::new(),
            model: String::new(),
            worktree: None,
            // Every stage is on screen from the first frame. What is still
            // coming is as much a fact about a run as what is done.
            stages: Stage::ALL
                .into_iter()
                .map(|stage| StageRow {
                    stage,
                    step: Step::Pending,
                })
                .collect(),
            piece: None,
            path: None,
            files_seen: 0,
            files_total: None,
            spend: None,
            exchange: None,
            waiting_since: None,
            tool: None,
            concluding: false,
            // Starting from the moment there is a screen at all: the run says
            // so too, a moment later, but the frames before that would
            // otherwise show a checklist of six things not started — and then
            // take it away again when the run finally speaks.
            opening: Some(Instant::now()),
            input_files: None,
        }
    }
}

pub(super) struct StageRow {
    pub(super) stage: Stage,
    pub(super) step: Step,
}

pub(super) enum Step {
    Pending,
    Running {
        since: Instant,
    },
    Done {
        took: Duration,
        sentence: String,
        from_checkpoint: bool,
    },
}

impl State {
    /// Take one event in, and say what a pipe should keep about it. `now` is
    /// passed rather than read so the elapsed times a test asserts are the
    /// times it chose.
    pub(super) fn apply(&mut self, event: Event, now: Instant) -> Option<String> {
        match event {
            Event::RunStarted {
                run_id,
                model,
                input,
                worktree,
                ..
            } => {
                self.run_id = run_id;
                self.model = model;
                self.input = input;
                self.worktree = worktree;
                let mut line = format!("run  {}  model {}", self.input, self.model);
                if let Some(worktree) = &self.worktree {
                    line.push_str(&format!("  worktree {}", worktree.display()));
                }
                line.push('\n');
                Some(line)
            }
            // Nothing is named yet; what there is to say is that the wait has
            // started and what it is for.
            Event::Opening => {
                self.opening = Some(now);
                None
            }
            Event::StageStarted { stage } => {
                self.begin(stage, now);
                None
            }
            Event::StageFinished {
                stage,
                outcome,
                from_checkpoint,
            } => {
                self.learn(&outcome);
                let sentence = outcome.sentence();
                let line = format!(
                    "[{}/{}] {:<9} {sentence}{}\n",
                    stage.number(),
                    Stage::ALL.len(),
                    stage.name(),
                    match from_checkpoint {
                        true => ", from checkpoint",
                        false => "",
                    }
                );
                let took = self
                    .row(stage)
                    .and_then(|row| match row.step {
                        Step::Running { since } => Some(now.saturating_duration_since(since)),
                        _ => None,
                    })
                    .unwrap_or_default();
                if let Some(row) = self.row_mut(stage) {
                    row.step = Step::Done {
                        took,
                        sentence,
                        from_checkpoint,
                    };
                }
                self.forget_chunk();
                Some(line)
            }
            Event::Chunk {
                index,
                of,
                path,
                piece,
                pieces,
            } => {
                // A concluding turn belongs to the file that hit the ceiling
                // or the prose re-ask, not to the next one. Leaving the flag
                // set is how every later wait said "concluding".
                self.forget_chunk();
                self.piece = Some((piece, pieces));
                // The plan's place, not how many paths this process has
                // seen: a re-entered run skips finished files and would
                // otherwise look like it started at file 1 again.
                self.files_seen = index;
                self.path = Some(path.clone());
                Some(format!(
                    "[{}/{}] {:<9} file {}/{}  {path}{}{}\n",
                    Stage::Review.number(),
                    Stage::ALL.len(),
                    Stage::Review.name(),
                    self.files_seen,
                    self.files_total.unwrap_or(of),
                    match pieces {
                        1 => String::new(),
                        pieces => format!("  piece {piece}/{pieces}"),
                    },
                    match &self.spend {
                        Some(spend) => format!("  {spend}"),
                        None => String::new(),
                    }
                ))
            }
            Event::Round { round, of } => {
                self.concluding = false;
                self.exchange = Some((round, of));
                self.waiting_since = Some(now);
                self.tool = None;
                None
            }
            Event::Concluding { .. } => {
                self.concluding = true;
                self.exchange = None;
                self.waiting_since = Some(now);
                self.tool = None;
                None
            }
            Event::Tool { name } => {
                self.waiting_since = None;
                self.tool = Some((name, now));
                None
            }
            // Only that the wait is over: which tool answered, and how long it
            // took, are the log's business and the trace's.
            Event::ToolDone { .. } => {
                self.tool = None;
                None
            }
            Event::Spend {
                spent,
                budget,
                currency,
            } => {
                self.spend = Some(match budget {
                    Some(ceiling) => format!("{spent:.4} / {ceiling:.4} {currency}"),
                    None => format!("{spent:.4} {currency} (no ceiling)"),
                });
                None
            }
        }
    }

    fn begin(&mut self, stage: Stage, now: Instant) {
        self.opening = None;
        if let Some(row) = self.row_mut(stage) {
            row.step = Step::Running { since: now };
        }
        self.forget_chunk();
    }

    fn forget_chunk(&mut self) {
        self.piece = None;
        self.path = None;
        self.concluding = false;
        self.exchange = None;
        self.waiting_since = None;
        self.tool = None;
    }

    /// The two counts a later row needs: how many files there were, and how
    /// many of them were never going to be read.
    fn learn(&mut self, outcome: &Outcome) {
        match outcome {
            Outcome::Input { files } => self.input_files = Some(*files),
            Outcome::Plan { skipped, .. } => {
                self.files_total = self.input_files.map(|files| files.saturating_sub(*skipped));
            }
            _ => {}
        }
    }

    fn row(&self, stage: Stage) -> Option<&StageRow> {
        self.stages.iter().find(|row| row.stage == stage)
    }

    fn row_mut(&mut self, stage: Stage) -> Option<&mut StageRow> {
        self.stages.iter_mut().find(|row| row.stage == stage)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;

    /// A writer the test can read back afterwards.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// One run getting as far as the third chunk of the review stage, with a
    /// tool answered and another in flight. Shared with `screen`'s tests,
    /// which draw the block this leaves behind.
    pub(crate) fn mid_review(now: Instant) -> State {
        let mut state = State::default();
        for event in events() {
            state.apply(event, now);
        }
        state
    }

    pub(crate) fn events() -> Vec<Event> {
        vec![
            Event::RunStarted {
                run_id: "7f3a9c1e".to_string(),
                run_dir: PathBuf::from("/tmp/runs/7f3a9c1e"),
                model: "deepseek-v4-flash".to_string(),
                input: "change.diff".to_string(),
                worktree: Some(PathBuf::from("/repo")),
            },
            Event::StageStarted {
                stage: Stage::Input,
            },
            Event::StageFinished {
                stage: Stage::Input,
                outcome: Outcome::Input { files: 9 },
                from_checkpoint: false,
            },
            Event::StageStarted { stage: Stage::Plan },
            Event::StageFinished {
                stage: Stage::Plan,
                outcome: Outcome::Plan {
                    chunks: 7,
                    skipped: 2,
                },
                from_checkpoint: false,
            },
            Event::StageStarted {
                stage: Stage::Review,
            },
            Event::Chunk {
                index: 3,
                of: 7,
                path: "src/foo.c".to_string(),
                piece: 1,
                pieces: 1,
            },
            Event::Round { round: 2, of: 6 },
            Event::Spend {
                spent: 1.83,
                budget: Some(10.0),
                currency: "CNY".to_string(),
            },
            Event::Tool {
                name: "read_file".to_string(),
            },
            Event::ToolDone {
                name: "read_file".to_string(),
                ms: 18,
            },
            Event::Tool {
                name: "search_repo".to_string(),
            },
        ]
    }

    /// A pipe keeps one line for each thing that changed what a reader would
    /// conclude, and never asks a cursor to move.
    #[test]
    fn a_pipe_appends_the_run_its_files_and_its_finished_stages() {
        let shared = Shared::default();
        let status = Status::pipe(Box::new(shared.clone()));
        let now = Instant::now();
        for event in events() {
            status.emit(event);
        }
        status.emit(Event::StageFinished {
            stage: Stage::Review,
            outcome: Outcome::Review {
                chunks: 7,
                unreviewed: 0,
            },
            from_checkpoint: false,
        });
        let _ = now;
        status.finish();

        let text = String::from_utf8(shared.0.lock().expect("lock").clone()).expect("utf8");
        assert_eq!(
            text,
            "\
run  change.diff  model deepseek-v4-flash  worktree /repo
[1/6] input     9 files
[2/6] plan      7 chunks, 2 files skipped
[3/6] review    file 3/7  src/foo.c
[3/6] review    7 chunks reviewed
"
        );
        assert!(!text.contains("\x1b["), "a pipe gets no escape sequences");
    }

    /// Rounds, tools and spend change what the terminal shows without adding
    /// a line each: a chunk with six rounds and three tools apiece would
    /// otherwise bury a CI log in near-identical lines.
    #[test]
    fn the_events_inside_a_chunk_do_not_each_get_a_line() {
        let shared = Shared::default();
        let status = Status::pipe(Box::new(shared.clone()));
        for event in [
            Event::Round { round: 4, of: 6 },
            Event::Tool {
                name: "read_file".to_string(),
            },
            Event::ToolDone {
                name: "read_file".to_string(),
                ms: 3,
            },
            Event::Spend {
                spent: 0.5,
                budget: None,
                currency: "CNY".to_string(),
            },
        ] {
            status.emit(event);
        }
        status.finish();

        assert!(shared.0.lock().expect("lock").is_empty());
    }

    /// Spend is known by the time a later file starts, so it rides on that
    /// line rather than needing one of its own.
    #[test]
    fn a_file_line_carries_the_running_spend() {
        let shared = Shared::default();
        let status = Status::pipe(Box::new(shared.clone()));
        status.emit(Event::Spend {
            spent: 1.83,
            budget: Some(10.0),
            currency: "CNY".to_string(),
        });
        status.emit(Event::Chunk {
            index: 2,
            of: 4,
            path: "src/bar.c".to_string(),
            piece: 1,
            pieces: 1,
        });
        status.finish();

        let text = String::from_utf8(shared.0.lock().expect("lock").clone()).expect("utf8");
        assert_eq!(
            text,
            "[3/6] review    file 2/4  src/bar.c  1.8300 / 10.0000 CNY\n"
        );
    }

    #[test]
    fn the_facts_a_chunk_establishes_are_the_ones_a_screen_needs() {
        let now = Instant::now();
        let state = mid_review(now);

        assert_eq!(state.piece, Some((1, 1)), "one request was enough for it");
        assert_eq!(state.path.as_deref(), Some("src/foo.c"));
        assert_eq!(state.files_seen, 3);
        assert_eq!(state.files_total, Some(7), "9 files, 2 of them skipped");
        assert_eq!(state.exchange, Some((2, 6)));
        assert_eq!(state.run_id, "7f3a9c1e");
        assert_eq!(
            state.tool.as_ref().map(|(name, _)| name.as_str()),
            Some("search_repo"),
            "the tool in flight is what the wait is for"
        );
        assert_eq!(state.worktree.as_deref(), Some(Path::new("/repo")));
    }

    /// A tool coming back ends the wait it was the reason for, and the next
    /// round starts a new one. Which tool answered is the log's business.
    #[test]
    fn a_tool_coming_back_ends_the_wait_it_explained() {
        let now = Instant::now();
        let mut state = mid_review(now);
        state.apply(
            Event::ToolDone {
                name: "search_repo".to_string(),
                ms: 4,
            },
            now,
        );
        assert!(state.tool.is_none());
        assert!(state.waiting_since.is_none());

        state.apply(Event::Round { round: 3, of: 6 }, now);
        assert!(state.waiting_since.is_some());
    }

    #[test]
    fn a_new_file_is_counted_once_however_many_chunks_it_takes() {
        let now = Instant::now();
        let mut state = mid_review(now);
        state.apply(
            Event::Chunk {
                index: 4,
                of: 7,
                path: "src/baz.c".to_string(),
                piece: 1,
                pieces: 2,
            },
            now,
        );
        assert_eq!(state.files_seen, 4);

        state.apply(
            Event::Chunk {
                index: 4,
                of: 7,
                path: "src/baz.c".to_string(),
                piece: 2,
                pieces: 2,
            },
            now,
        );
        assert_eq!(
            state.files_seen, 4,
            "the same file split in two is one file"
        );
    }

    /// A concluding turn is about that file. The next file starts a new
    /// conversation; leaving the flag set made every later wait say
    /// "concluding" while the loop was still investigating.
    #[test]
    fn a_later_file_is_not_still_the_conclusion() {
        let now = Instant::now();
        let mut state = mid_review(now);
        state.apply(
            Event::Concluding {
                why: "the tool loop reached its ceiling of 6 rounds".to_string(),
            },
            now,
        );
        assert!(state.concluding);

        state.apply(
            Event::Chunk {
                index: 4,
                of: 7,
                path: "src/baz.c".to_string(),
                piece: 1,
                pieces: 1,
            },
            now,
        );
        assert!(
            !state.concluding,
            "the next file is not the concluding turn"
        );

        state.apply(Event::Round { round: 1, of: 6 }, now);
        assert!(!state.concluding);
        assert_eq!(state.exchange, Some((1, 6)));
    }

    /// A re-entered run emits only the files it still has to do. The number
    /// on screen is that file's place in the plan, not "the first one this
    /// process happened to look at".
    #[test]
    fn a_resumed_file_keeps_its_place_in_the_plan() {
        let now = Instant::now();
        let mut state = State::default();
        state.apply(
            Event::StageFinished {
                stage: Stage::Input,
                outcome: Outcome::Input { files: 65 },
                from_checkpoint: true,
            },
            now,
        );
        state.apply(
            Event::StageFinished {
                stage: Stage::Plan,
                outcome: Outcome::Plan {
                    chunks: 57,
                    skipped: 8,
                },
                from_checkpoint: true,
            },
            now,
        );
        let line = state.apply(
            Event::Chunk {
                index: 2,
                of: 57,
                path: "assets/style.css".to_string(),
                piece: 1,
                pieces: 1,
            },
            now,
        );
        assert_eq!(state.files_seen, 2);
        assert_eq!(state.files_total, Some(57));
        assert_eq!(
            line.as_deref(),
            Some("[3/6] review    file 2/57  assets/style.css\n")
        );
    }
}
