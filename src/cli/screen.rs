//! The block a terminal shows while the run is still going.
//!
//! Drawn on a timer rather than on events. A single model call blocks the run
//! for tens of seconds, so a screen that repaints only when something is
//! reported is a frozen screen for exactly the stretches a person most wants
//! to know something is still happening. The thread that repaints it draws and
//! nothing else: it never touches a stage, a budget or a checkpoint, so the
//! serial rule the review pipeline lives by is untouched.
//!
//! The height is fixed. Ratatui anchors an inline viewport when the terminal
//! is created and cannot resize it afterwards (ratatui#984), so the layout
//! reserves its rows up front and leaves them blank rather than growing into
//! the caller's scrollback. `insert_before` would be the way to push finished
//! work above the block, and is deliberately not used: combined with a
//! continuous redraw it duplicates the viewport into the scrollback whenever
//! the window is resized (ratatui#2666).

use std::io::Stdout;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport};

use super::status::{State, Step};
use reviewbot::domain::Stage;

/// The tallest the block ever gets: two header rows, a blank one, one row per
/// stage, another blank one, and two rows of what is happening now.
///
/// It is a ceiling rather than the height. An inline viewport cannot be resized
/// once it is anchored (ratatui#984), so a block that wants fewer rows is given
/// a new viewport anchored at the same row — see `reanchor`. The alternative was
/// reserving the ceiling always, which left a screen's worth of blank rows under
/// a run that had not started yet.
pub(super) const MAX_HEIGHT: u16 = 2 + 1 + Stage::ALL.len() as u16 + 1 + 2;

/// How often the block is repainted. Fast enough that the spinner reads as
/// motion, slow enough that a run spends no measurable time drawing.
const TICK: Duration = Duration::from_millis(100);

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The checklist is a table, so its columns are fixed rather than stretched to
/// whatever the terminal happens to be. Right-aligning the elapsed time to the
/// far edge left it floating half a screen away from the row it belonged to.
const DETAIL_WIDTH: usize = 52;
const SEPARATOR: &str = "  ·  ";
/// `"✓ 1 input     "`: the mark, the number, the name padded. Flush left, like
/// the title above it: an indent made the rows read as a sub-list of something.
const HEAD_WIDTH: usize = 14;
const TIME_WIDTH: usize = 7;

/// The terminal, plus the thread keeping it current.
pub(super) struct Screen {
    stop: Arc<AtomicBool>,
    painter: Option<JoinHandle<()>>,
}

impl Screen {
    /// Take the bottom of the terminal and start repainting it, or say that
    /// this terminal will not give it up.
    ///
    /// The terminal is claimed here rather than on the thread so that failing
    /// is answerable: an inline viewport has to ask where the cursor is, and
    /// some terminals that pass for one never answer. The caller can then fall
    /// back to plain lines instead of leaving a run with nothing to show.
    pub(super) fn start(state: Arc<Mutex<State>>, color: bool) -> Option<Self> {
        let backend = CrosstermBackend::new(std::io::stdout());
        // Measured before the viewport exists, so the first one is already the
        // right size: the width comes from the backend, the height from the
        // block that width produces.
        let width = backend.size().map(|size| size.width).unwrap_or(80);
        let height = match state.lock() {
            Ok(state) => block(&state, width as usize, 0, color, Instant::now()).len() as u16,
            Err(_) => MAX_HEIGHT,
        };
        let options = TerminalOptions {
            viewport: Viewport::Inline(height),
        };
        let mut terminal = match Terminal::with_options(backend, options) {
            Ok(terminal) => terminal,
            Err(error) => {
                tracing::warn!(%error, "this terminal will not hold a status screen");
                return None;
            }
        };
        let _ = terminal.hide_cursor();
        let stop = Arc::new(AtomicBool::new(false));
        let painter = std::thread::Builder::new()
            .name("reviewbot-screen".to_string())
            .spawn({
                let stop = Arc::clone(&stop);
                move || paint_until_stopped(terminal, &state, &stop, color)
            })
            .ok();
        Some(Self { stop, painter })
    }

    /// Give the terminal back. Consuming `self` is the point: the final
    /// summary must not be printed into a viewport that is still being
    /// repainted, and there is no way to ask for that here.
    pub(super) fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(painter) = self.painter.take() {
            let _ = painter.join();
        }
    }
}

fn paint_until_stopped(
    mut terminal: Terminal<CrosstermBackend<Stdout>>,
    state: &Mutex<State>,
    stop: &AtomicBool,
    color: bool,
) {
    let mut tick = 0usize;
    while !stop.load(Ordering::Acquire) {
        paint(&mut terminal, state, tick, color);
        tick = tick.wrapping_add(1);
        std::thread::sleep(TICK);
    }
    // One last frame, so whatever the run ended on is what was on screen when
    // it did, and then the rows go back to the caller.
    paint(&mut terminal, state, tick, color);
    hand_back(&mut terminal);
}

/// Draw one frame, first giving it a viewport its own height.
fn paint(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &Mutex<State>,
    tick: usize,
    color: bool,
) {
    let now = Instant::now();
    let area = terminal.get_frame().area();
    let lines = {
        let Ok(state) = state.lock() else {
            return;
        };
        block(&state, area.width as usize, tick, color, now)
    };
    if area.height as usize != lines.len() {
        reanchor(terminal, lines.len() as u16);
    }
    let _ = terminal.draw(|frame| {
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Left),
            frame.area(),
        )
    });
}

/// Give the block a viewport of a different height, anchored where the old one
/// began.
///
/// Ratatui fixes an inline viewport's height when it anchors it, so a block that
/// grew or shrank needs a new one. Anchoring happens at the cursor, so the old
/// block is wiped and the cursor put back at its first row before the new
/// viewport is asked for — which is what keeps the block from walking down the
/// screen every time it changes size. A terminal that will not answer where the
/// cursor is keeps the viewport it has: a block of the wrong height still says
/// everything, and losing the screen entirely does not.
fn reanchor(terminal: &mut Terminal<CrosstermBackend<Stdout>>, height: u16) {
    hand_back(terminal);
    let backend = CrosstermBackend::new(std::io::stdout());
    let options = TerminalOptions {
        viewport: Viewport::Inline(height),
    };
    match Terminal::with_options(backend, options) {
        Ok(resized) => {
            *terminal = resized;
            let _ = terminal.hide_cursor();
        }
        Err(error) => tracing::warn!(%error, "keeping the status screen at its old height"),
    }
}

/// Wipe the block and leave the cursor where its first row was, so whatever
/// prints next starts there.
///
/// The cursor has to be moved first. `Terminal::clear` restores the cursor to
/// wherever it was when it was called, which for a painted block is somewhere
/// in the middle of it — and the summary then lands indented, under as many
/// blank lines as the block was tall.
fn hand_back<B: Backend>(terminal: &mut Terminal<B>) {
    let origin = terminal.get_frame().area().as_position();
    let _ = terminal.set_cursor_position(origin);
    let _ = terminal.clear();
    let _ = terminal.show_cursor();
    let _ = terminal.flush();
}

/// The whole layout, as a function of the facts and the width it has. Its length
/// is the block's height, which is why nothing here pads to a constant: the
/// tests read these rows back, and the painter asks the terminal for exactly
/// this many.
pub(super) fn block(
    state: &State,
    width: usize,
    tick: usize,
    color: bool,
    now: Instant,
) -> Vec<Line<'static>> {
    // Three sections, each there only when it has something to say, joined by
    // one blank row. Building it this way is what keeps the block as tall as its
    // content: nothing pads to a constant, and a section that says nothing
    // takes its separator with it.
    let checklist = match state.opening {
        // Nothing has begun, so there is no checklist: six rows of "not
        // started" under a run that has not started is a screen claiming to be
        // stuck.
        Some(_) => Vec::new(),
        None => (0..state.stages.len())
            .map(|row| stage_line(state, row, width, color, now))
            .collect(),
    };
    // The running commentary: which file, and what is being waited on. Set
    // apart from the list above because read as one block the file looked like
    // a seventh stage.
    let commentary: Vec<Line<'static>> = [
        path_line(state, width, color),
        activity_line(state, tick, color, now),
    ]
    .into_iter()
    .filter(|line| line.width() > 0)
    .collect();

    let mut lines = Vec::with_capacity(MAX_HEIGHT as usize);
    for section in [header(state, width, color), checklist, commentary] {
        if section.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(section);
    }
    lines
}

/// Which build of what, on which run, with which model, over what.
///
/// The first three are what you have to quote to ask anybody about a run
/// afterwards — the run id in particular used to appear only once the run was
/// over. The last two are the subject: what is being reviewed, and what can be
/// read while reviewing it. The worktree appears only when there is a checkout
/// to name; a run without one is not carrying a special mode worth a caption,
/// it is the ordinary way to review a diff, and what it could not reach is
/// answered where it matters, in the report's coverage note.
///
/// One row if it fits, two if it does not, and the second is empty rather than
/// padded with something to justify it.
fn header(state: &State, width: usize, color: bool) -> Vec<Line<'static>> {
    let mut who = vec![format!("reviewbot {}", env!("CARGO_PKG_VERSION"))];
    if !state.run_id.is_empty() {
        who.push(format!("run {}", state.run_id));
    }
    if !state.model.is_empty() {
        who.push(state.model.clone());
    }
    let mut what = Vec::new();
    if !state.input.is_empty() {
        what.push(state.input.clone());
    }
    if let Some(worktree) = &state.worktree {
        what.push(format!("worktree {}", worktree.display()));
    }

    let who = who.join(SEPARATOR);
    let what = what.join(SEPARATOR);
    let together = match what.is_empty() {
        true => who.clone(),
        false => format!("{who}{SEPARATOR}{what}"),
    };
    match together.chars().count() <= width {
        true => vec![Line::from(Span::styled(together, bold(color)))],
        false => vec![
            Line::from(Span::styled(who, bold(color))),
            Line::from(Span::styled(clip(&what, width), dim(color))),
        ],
    }
}

fn stage_line(state: &State, row: usize, width: usize, color: bool, now: Instant) -> Line<'static> {
    let stage = &state.stages[row];
    let (mark, mark_style, detail, elapsed) = match &stage.step {
        Step::Pending => ("·", dim(color), String::new(), None),
        Step::Running { since } => (
            "▸",
            styled(color, Color::Cyan),
            running_detail(state, row),
            Some(now.saturating_duration_since(*since)),
        ),
        Step::Done {
            took,
            sentence,
            from_checkpoint,
        } => (
            "✓",
            styled(color, Color::Green),
            match from_checkpoint {
                true => format!("{sentence}  ·  from checkpoint"),
                false => sentence.clone(),
            },
            Some(*took),
        ),
    };
    // Wide enough for the longest sentence a stage produces, and narrower when
    // the terminal is: the time column stays put either way.
    let field = DETAIL_WIDTH.min(width.saturating_sub(HEAD_WIDTH + 2 + TIME_WIDTH));
    let detail = clip(&detail, field);
    let mut spans = vec![
        Span::styled(mark.to_string(), mark_style),
        Span::raw(format!(
            " {} {:<9} ",
            stage.stage.number(),
            stage.stage.name()
        )),
        Span::styled(
            format!("{detail:<field$}"),
            dim_unless_running(color, &stage.step),
        ),
    ];
    if let Some(elapsed) = elapsed {
        // Padded by hand rather than by a second widget: one row of one
        // paragraph keeps the block a single render.
        spans.push(Span::styled(
            format!("  {:>TIME_WIDTH$}", duration(elapsed)),
            dim(color),
        ));
    }
    Line::from(spans)
}

/// The one stage with counters worth watching, counted in files.
///
/// Pieces are not a second progress bar: a file too big for one request is
/// reviewed in several, and while that is worth knowing about the file on
/// screen, it says nothing about how far through the change the run is. So the
/// count is files, and pieces appear only for a file that actually got cut.
fn running_detail(state: &State, row: usize) -> String {
    if state.stages[row].stage != Stage::Review {
        return String::new();
    }
    let mut facts = Vec::new();
    if let Some(total) = state.files_total {
        facts.push(format!("file {}/{}", state.files_seen, total));
    }
    if let Some((piece, pieces)) = state.piece.filter(|(_, pieces)| *pieces > 1) {
        facts.push(format!("piece {piece}/{pieces}"));
    }
    if let Some(spend) = &state.spend {
        facts.push(spend.clone());
    }
    facts.join("  ·  ")
}

/// The file on the table, said as something being done to it: a bare path
/// looked like another line of the checklist above rather than the work in
/// progress. Long paths lose their front, because what tells one file from
/// another is the end of it.
///
/// The round count belongs here rather than on the line below. It counts this
/// file's conversation and starts again at the next one, while the line below
/// says what the run is waiting on this second — and it kept disappearing from
/// there whenever that was a tool rather than the model.
fn path_line(state: &State, width: usize, color: bool) -> Line<'static> {
    let Some(path) = &state.path else {
        return Line::default();
    };
    let label = "reviewing  ";
    let round = match state.exchange {
        // The ceiling earns a word because reaching it ends this file early, on
        // half the evidence.
        Some((round, of)) if round >= of => Some(format!("round {round}/{of} (last)")),
        Some((round, of)) => Some(format!("round {round}/{of}")),
        None => None,
    };
    let room = width
        .saturating_sub(label.len())
        .saturating_sub(round.as_ref().map_or(0, |said| said.chars().count() + 5));
    let mut spans = vec![
        Span::styled(label.to_string(), dim(color)),
        Span::raw(clip_start(path, room)),
    ];
    if let Some(round) = round {
        let last = round.ends_with("(last)");
        spans.push(Span::styled(SEPARATOR.to_string(), dim(color)));
        spans.push(Span::styled(
            round,
            match last {
                true => styled(color, Color::Yellow),
                false => dim(color),
            },
        ));
    }
    Line::from(spans)
}

/// The row that has to move. What it says is what the run is doing, and when
/// that is waiting on somebody else, who.
///
/// One shape for all of it: a phrase, an ellipsis because it has not finished,
/// and — for the waits, where how long is the whole question — a clock. A stage
/// working needs no clock here; the checklist above it keeps one.
fn activity_line(state: &State, tick: usize, color: bool, now: Instant) -> Line<'static> {
    let (what, since) = match (
        &state.tool,
        state.waiting_since,
        state.preparing,
        state.opening,
    ) {
        (Some((tool, since)), _, _, _) => (format!("waiting for {tool}"), Some(*since)),
        (None, Some(since), _, _) => (
            // The conclusion is one more thing to wait for, said the same way.
            match state.concluding {
                true => "waiting for conclusion".to_string(),
                false => "waiting for model".to_string(),
            },
            Some(since),
        ),
        (None, None, Some(since), _) => ("reading the repository layout".to_string(), Some(since)),
        (None, None, None, Some(since)) => ("starting".to_string(), Some(since)),
        // Nothing is being waited on, so the row says what the stage that is
        // running is doing. With every stage finished there is nothing left to
        // be doing, and the final summary is about to take this block's place —
        // so it goes quiet rather than spending its last tenth of a second
        // saying `done`, which the summary underneath says better.
        (None, None, None, None) => match state
            .stages
            .iter()
            .find(|row| matches!(row.step, Step::Running { .. }))
        {
            Some(row) => (doing(row.stage).to_string(), None),
            None => return Line::default(),
        },
    };
    let frame = SPINNER[tick % SPINNER.len()];
    let mut spans = vec![
        Span::styled(format!("{frame} "), styled(color, Color::Cyan)),
        Span::styled(format!("{what}..."), bold(color)),
    ];
    if let Some(since) = since {
        // In brackets rather than after a `·`: everywhere else that separator
        // holds two facts of equal standing apart, and this is not another
        // fact — it is how long the one already on the row has been going.
        spans.push(Span::raw(format!(
            " ({})",
            duration(now.saturating_duration_since(since))
        )));
    }
    Line::from(spans)
}

/// What a stage is doing, for the row that has nothing more specific to say.
/// The stage's own name is on the checklist above; repeating it here answered
/// "where are we" twice and "what is happening" not at all.
fn doing(stage: Stage) -> &'static str {
    match stage {
        Stage::Input => "reading changes",
        Stage::Triage => "planning the review",
        Stage::Review => "reviewing",
        Stage::Merge => "merging findings",
        Stage::Report => "writing the report",
        Stage::Publish => "posting comments",
    }
}

/// Cut the end off, for text whose beginning identifies it.
fn clip(text: &str, width: usize) -> String {
    match text.chars().count() > width && width > 1 {
        true => text.chars().take(width - 1).chain(['…']).collect(),
        false => text.to_string(),
    }
}

/// Cut the front off, for a path: two files in one project differ at the end,
/// so that is the half worth keeping.
fn clip_start(text: &str, width: usize) -> String {
    let length = text.chars().count();
    match length > width && width > 1 {
        true => ['…']
            .into_iter()
            .chain(text.chars().skip(length - (width - 1)))
            .collect(),
        false => text.to_string(),
    }
}

fn duration(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..10 => format!("{:.1}s", elapsed.as_secs_f64()),
        10..60 => format!("{seconds}s"),
        60..3600 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

fn styled(color: bool, foreground: Color) -> Style {
    match color {
        true => Style::default().fg(foreground),
        false => Style::default(),
    }
}

fn bold(color: bool) -> Style {
    match color {
        true => Style::default().add_modifier(Modifier::BOLD),
        false => Style::default(),
    }
}

fn dim(color: bool) -> Style {
    match color {
        true => Style::default().add_modifier(Modifier::DIM),
        false => Style::default(),
    }
}

fn dim_unless_running(color: bool, step: &Step) -> Style {
    match step {
        Step::Running { .. } => Style::default(),
        _ => dim(color),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    const WIDTH: usize = 100;
    /// Named so that moving a row does not turn every assertion into a puzzle.
    /// These are the positions for a header that fits on one row, which is the
    /// wide case; the block is as tall as its content, so the last two rows are
    /// counted from the end.
    const WHO: usize = 0;
    const TITLE_GAP: usize = 1;
    const FIRST_STAGE: usize = 2;
    const GAP: usize = FIRST_STAGE + Stage::ALL.len();

    /// The rows the painter would ask a viewport of exactly this height to hold.
    fn rows_at(state: &State, width: usize, tick: usize, now: Instant) -> Vec<String> {
        block(state, width, tick, false, now)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn rows(state: &State, tick: usize, now: Instant) -> Vec<String> {
        rows_at(state, WIDTH, tick, now)
    }

    fn last(rows: &[String]) -> String {
        rows.last().cloned().unwrap_or_default()
    }

    fn second_last(rows: &[String]) -> String {
        rows.get(rows.len().wrapping_sub(2))
            .cloned()
            .unwrap_or_default()
    }

    /// The title says what is being reviewed and what can be read while doing
    /// it; the checklist names all six stages from the first frame, so what is
    /// still coming is as visible as what is done.
    #[test]
    fn the_block_is_exactly_the_viewport_and_names_every_stage_up_front() {
        let now = Instant::now();
        let state = crate::cli::status::tests::mid_review(now);
        let rows = rows(&state, 0, now);

        assert_eq!(
            rows.len(),
            MAX_HEIGHT as usize - 1,
            "as tall as it needs to be: one row of title, so one less than the most"
        );
        assert_eq!(
            rows[WHO],
            format!(
                "reviewbot {}  ·  run 7f3a9c1e  ·  deepseek-v4-flash  ·  change.diff  ·  worktree /repo",
                env!("CARGO_PKG_VERSION")
            ),
            "one row while it fits: the run is named, and so is what it reviews"
        );
        assert_eq!(rows[TITLE_GAP], "", "one blank row under the title");
        assert_eq!(rows[GAP], "", "the checklist ends with a blank row");
        assert!(
            rows[FIRST_STAGE].starts_with("✓ 1 input     9 files"),
            "{:?}",
            rows[FIRST_STAGE]
        );
        assert!(
            rows[FIRST_STAGE + 1].starts_with("✓ 2 triage    7 chunks, 2 files skipped"),
            "{:?}",
            rows[FIRST_STAGE + 1]
        );
        assert!(
            rows[FIRST_STAGE + 2].starts_with("▸ 3 review    file 1/7  ·  1.8300 / 10.0000 CNY"),
            "counted in files, and pieces only for a file that got cut: {:?}",
            rows[FIRST_STAGE + 2]
        );
        assert!(
            rows[FIRST_STAGE].ends_with("0.0s"),
            "each row keeps its own clock"
        );
    }

    /// The first frame, before the run can name itself: what the caller already
    /// knew, and the row that moves. Three rows, because the block is only as
    /// tall as what it has to say.
    #[test]
    fn the_frame_before_a_run_has_a_name_still_says_something() {
        let now = Instant::now();
        let mut state = State::default();
        state.input = "github.com/acme/app #1".to_string();
        state.model = "deepseek-v4-flash".to_string();
        state.apply(reviewbot::progress::Event::Opening, now);
        let rows = rows(&state, 0, now);

        assert_eq!(
            rows[WHO],
            format!(
                "reviewbot {}  ·  deepseek-v4-flash  ·  github.com/acme/app #1",
                env!("CARGO_PKG_VERSION")
            ),
            "no run id yet, because nothing can name it before the change is fetched"
        );
        assert_eq!(
            rows[rows.len() - 1],
            "⠋ starting... (0.0s)",
            "what is happening now is on the last row, whatever else is on screen"
        );
        assert_eq!(
            rows.len(),
            3,
            "a title, a blank row and the row that moves — nothing has begun, so \
             nothing else is claimed: {rows:?}"
        );
    }

    /// Too narrow for one row and it becomes two, rather than losing the half
    /// that says what is being reviewed.
    #[test]
    fn a_header_too_long_for_one_row_uses_the_second() {
        let now = Instant::now();
        let state = crate::cli::status::tests::mid_review(now);
        let narrow = rows_at(&state, 60, 0, now);

        assert_eq!(
            narrow[WHO],
            format!(
                "reviewbot {}  ·  run 7f3a9c1e  ·  deepseek-v4-flash",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert_eq!(narrow[1], "change.diff  ·  worktree /repo");
        assert_eq!(narrow[2], "", "and the blank row still follows the title");
        assert_eq!(
            narrow.len(),
            rows(&state, 0, now).len() + 1,
            "the block is one row taller for the row the title needed"
        );
    }

    #[test]
    fn a_stage_that_has_not_started_is_marked_and_says_nothing_else() {
        let now = Instant::now();
        let state = crate::cli::status::tests::mid_review(now);
        let rows = rows(&state, 0, now);

        assert_eq!(rows[FIRST_STAGE + 3], "· 4 merge");
        assert_eq!(rows[FIRST_STAGE + 4], "· 5 report");
        assert_eq!(rows[FIRST_STAGE + 5], "· 6 publish");
    }

    /// Below the checklist, separated from it, is the running commentary: the
    /// file being read and why the run is not answering. A bare path read as
    /// another checklist row, and as a verb it does not.
    #[test]
    fn the_activity_rows_are_set_apart_and_say_what_is_being_done() {
        let now = Instant::now();
        let state = crate::cli::status::tests::mid_review(now);
        let rows = rows(&state, 0, now);

        assert_eq!(rows[GAP], "", "the checklist ends here");
        assert_eq!(
            rows[rows.len() - 2],
            "reviewing  src/foo.c  ·  round 2/6",
            "the round counts this file's conversation, so it sits with the file"
        );
        assert_eq!(
            rows[rows.len() - 1],
            "⠋ waiting for search_repo... (0.0s)",
            "and the line below says only what is being waited on, and for how long"
        );
    }

    /// Between tool calls the wait belongs to the model, and the row that says
    /// so carries nothing but that and its clock.
    #[test]
    fn waiting_on_the_model_says_so_and_nothing_else() {
        let now = Instant::now();
        let mut state = crate::cli::status::tests::mid_review(now);
        state.apply(reviewbot::progress::Event::Round { round: 3, of: 12 }, now);
        let rows = rows(&state, 2, now);

        assert_eq!(rows[rows.len() - 2], "reviewing  src/foo.c  ·  round 3/12");
        assert_eq!(rows[rows.len() - 1], "⠹ waiting for model... (0.0s)");
    }

    /// The last round is worth marking: reaching the ceiling ends the file
    /// there, so whatever it says next is said on half the evidence.
    #[test]
    fn the_last_round_before_the_ceiling_says_so() {
        let now = Instant::now();
        let mut state = crate::cli::status::tests::mid_review(now);
        state.apply(reviewbot::progress::Event::Round { round: 12, of: 12 }, now);

        assert_eq!(
            second_last(&rows_at(&state, WIDTH, 0, now)),
            "reviewing  src/foo.c  ·  round 12/12 (last)"
        );
    }

    /// What prints next has to start where the block started. `Terminal::clear`
    /// alone restores the cursor to wherever the last frame left it, which put
    /// the final summary indented and under a screen's worth of blank rows.
    #[test]
    fn handing_the_terminal_back_leaves_the_cursor_where_the_block_began() {
        let now = Instant::now();
        let state = crate::cli::status::tests::mid_review(now);
        let lines = block(&state, WIDTH, 0, false, now);
        let height = lines.len() as u16;
        let mut terminal = Terminal::with_options(
            TestBackend::new(WIDTH as u16, 30),
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )
        .expect("a test terminal has no io");
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new(lines), frame.area());
            })
            .expect("draw");
        let origin = terminal.get_frame().area().as_position();
        // Where painting leaves it: somewhere inside the block.
        terminal
            .set_cursor_position((7, origin.y + 4))
            .expect("cursor");

        hand_back(&mut terminal);

        assert_eq!(terminal.get_cursor_position().expect("cursor"), origin);
        let buffer = terminal.backend().buffer().clone();
        let painted: String = (0..height)
            .flat_map(|row| (0..WIDTH as u16).map(move |column| (column, row)))
            .map(|position| buffer[position].symbol())
            .collect();
        assert!(painted.trim().is_empty(), "the block is gone: {painted:?}");
    }

    /// A stage with no sub-events of its own still says what it is doing: the
    /// checklist above already says where the run is, so repeating the stage
    /// name here answered that twice and the question this row asks not at all.
    #[test]
    fn a_stage_with_nothing_more_specific_says_what_it_is_doing() {
        let now = Instant::now();
        let mut state = crate::cli::status::tests::mid_review(now);
        // One stage finishes before the next starts, as in a run: the row
        // speaks for whichever one is running.
        state.apply(
            reviewbot::progress::Event::StageFinished {
                stage: reviewbot::domain::Stage::Review,
                outcome: reviewbot::progress::Outcome::Review {
                    chunks: 7,
                    unreviewed: 0,
                },
                from_checkpoint: false,
            },
            now,
        );
        for (stage, said) in [
            (reviewbot::domain::Stage::Merge, "⠋ merging findings..."),
            (reviewbot::domain::Stage::Report, "⠋ writing the report..."),
            (reviewbot::domain::Stage::Publish, "⠋ posting comments..."),
        ] {
            state.apply(reviewbot::progress::Event::StageStarted { stage }, now);
            assert_eq!(last(&rows(&state, 0, now)), said);
            state.apply(
                reviewbot::progress::Event::StageFinished {
                    stage,
                    outcome: reviewbot::progress::Outcome::Report,
                    from_checkpoint: false,
                },
                now,
            );
        }
    }

    /// The conclusion is one more thing the run is waiting for, said the way the
    /// other two waits are said.
    #[test]
    fn the_last_turn_is_a_wait_like_the_others() {
        let now = Instant::now();
        let mut state = crate::cli::status::tests::mid_review(now);
        state.apply(
            reviewbot::progress::Event::Concluding {
                why: "the tool loop reached its ceiling of 12 rounds".to_string(),
            },
            now,
        );

        assert_eq!(
            last(&rows(&state, 0, now)),
            "⠋ waiting for conclusion... (0.0s)"
        );
    }

    /// The seconds between `input` finishing and `triage` starting are a tree
    /// fetch that belongs to neither, and the row that moves has to account for
    /// them: the checklist has no line to mark, so it looked like a run that had
    /// stopped with one stage done.
    #[test]
    fn the_work_between_two_stages_is_still_on_screen() {
        let now = Instant::now();
        let mut state = crate::cli::status::tests::mid_review(now);
        // The moment it happens in a real run: a stage has just finished, so
        // nothing is in flight, and the next one has not started.
        state.apply(
            reviewbot::progress::Event::StageFinished {
                stage: reviewbot::domain::Stage::Input,
                outcome: reviewbot::progress::Outcome::Input { files: 9 },
                from_checkpoint: false,
            },
            now,
        );
        state.apply(reviewbot::progress::Event::Preparing, now);
        assert_eq!(
            last(&rows(&state, 0, now)),
            "⠋ reading the repository layout... (0.0s)"
        );

        state.apply(
            reviewbot::progress::Event::StageStarted {
                stage: reviewbot::domain::Stage::Triage,
            },
            now,
        );
        assert!(
            !last(&rows(&state, 0, now)).contains("layout"),
            "and it stops saying so the moment a stage does start"
        );
    }

    /// A run that is over says nothing here: the summary that replaces this
    /// block a moment later says all of it, and better.
    #[test]
    fn a_finished_run_leaves_the_moving_row_empty() {
        let now = Instant::now();
        let mut state = crate::cli::status::tests::mid_review(now);
        for stage in Stage::ALL {
            state.apply(
                reviewbot::progress::Event::StageFinished {
                    stage,
                    outcome: reviewbot::progress::Outcome::Report,
                    from_checkpoint: false,
                },
                now,
            );
        }

        // Every stage done and nothing to wait on: the block ends at the
        // checklist, with no moving row and no gap left over to hold one.
        let rows = rows(&state, 0, now);
        assert!(last(&rows).starts_with("✓ 6 publish"), "{rows:?}");
        assert!(
            !rows.iter().any(|row| row.contains('⠋')),
            "nothing is spinning: {rows:?}"
        );
    }

    /// A path too long for the row loses its front: two files in one project
    /// differ at the end.
    #[test]
    fn paths_are_shortened_where_they_carry_least() {
        let path = "daemon/src/patch/driver/loader/target.rs";
        let clipped = clip_start(path, 20);
        assert_eq!(clipped, "…er/loader/target.rs");
        assert_eq!(clipped.chars().count(), 20, "it fits the row it was given");
        assert_eq!(clip_start("short.rs", 20), "short.rs");
        assert_eq!(clip("a very long banner indeed", 10), "a very lo…");
    }

    #[test]
    fn the_spinner_advances_between_frames() {
        let now = Instant::now();
        let state = crate::cli::status::tests::mid_review(now);

        let first = last(&rows(&state, 0, now));
        let second = last(&rows(&state, 1, now));
        assert_ne!(first, second, "the one row that has to move, moves");
        assert!(first.contains('⠋') && second.contains('⠙'));
    }
}
