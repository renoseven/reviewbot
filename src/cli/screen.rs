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

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

use super::status::{State, Step};
use reviewbot::domain::Stage;

/// Two header rows, one row per stage, then a blank one and two rows of what
/// is happening now. Every row is always there, because the viewport cannot
/// change height.
pub(super) const HEIGHT: u16 = 2 + Stage::ALL.len() as u16 + 3;

/// How often the block is repainted. Fast enough that the spinner reads as
/// motion, slow enough that a run spends no measurable time drawing.
const TICK: Duration = Duration::from_millis(100);

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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
        let options = TerminalOptions {
            viewport: Viewport::Inline(HEIGHT),
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

fn paint_until_stopped<B: Backend>(
    mut terminal: Terminal<B>,
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

fn paint<B: Backend>(terminal: &mut Terminal<B>, state: &Mutex<State>, tick: usize, color: bool) {
    let now = Instant::now();
    let Ok(state) = state.lock() else {
        return;
    };
    let _ = terminal.draw(|frame| draw(frame, &state, tick, color, now));
}

/// The whole layout, as a function of the facts and the frame it has: the
/// tests draw it into a `TestBackend` and read the rows back.
pub(super) fn draw(frame: &mut Frame, state: &State, tick: usize, color: bool, now: Instant) {
    let area = frame.area();
    let width = area.width as usize;
    let mut lines = Vec::with_capacity(HEIGHT as usize);
    lines.push(who(state, color));
    lines.push(what(state, width, color));
    for row in 0..state.stages.len() {
        lines.push(stage_line(state, row, width, color, now));
    }
    // The checklist is a list and the rows below it are a running commentary;
    // reading them as one block was the reason the file being reviewed looked
    // like a seventh stage.
    lines.push(Line::default());
    lines.push(path_line(state, width, color));
    lines.push(activity_line(state, tick, color, now));
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Left),
        frame.area(),
    );
}

/// Which build of what, on which run, with which model. All three are things
/// you have to quote to ask anybody about a run afterwards, and the run id in
/// particular used to appear only once the run was over.
fn who(state: &State, color: bool) -> Line<'static> {
    let mut facts = vec![format!("reviewbot {}", env!("CARGO_PKG_VERSION"))];
    if !state.run_id.is_empty() {
        facts.push(format!("run {}", state.run_id));
    }
    if !state.model.is_empty() {
        facts.push(state.model.clone());
    }
    Line::from(Span::styled(facts.join("  ·  "), bold(color)))
}

/// What is being reviewed, and what can be read while doing it.
///
/// The worktree appears only when there is a checkout to name. A run without
/// one is not carrying a special mode worth a caption — it is the ordinary way
/// to review a diff, and what it could not reach is answered where it matters,
/// in the report's coverage note.
fn what(state: &State, width: usize, color: bool) -> Line<'static> {
    let mut facts = Vec::new();
    if !state.input.is_empty() {
        facts.push(state.input.clone());
    }
    if let Some(worktree) = &state.worktree {
        facts.push(format!("worktree {}", home_relative(worktree)));
    }
    Line::from(Span::styled(clip(&facts.join("  ·  "), width), dim(color)))
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
    let head = format!(
        "  {mark} {} {:<9} ",
        stage.stage.number(),
        stage.stage.name()
    );
    let left = format!("{head}{detail}");
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(mark.to_string(), mark_style),
        Span::raw(format!(
            " {} {:<9} ",
            stage.stage.number(),
            stage.stage.name()
        )),
        Span::styled(detail, dim_unless_running(color, &stage.step)),
    ];
    if let Some(elapsed) = elapsed {
        let took = duration(elapsed);
        // Right-aligned by padding rather than by a second widget: one row of
        // one paragraph keeps the block a single render.
        let used = left.chars().count() + took.chars().count();
        if width > used {
            spans.push(Span::raw(" ".repeat(width - used)));
        } else {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(took, dim(color)));
    }
    Line::from(spans)
}

/// The one stage with counters worth watching. Chunks and files are counted
/// separately because they are not the same thing: a file too big for one
/// request is reviewed as several chunks.
fn running_detail(state: &State, row: usize) -> String {
    if state.stages[row].stage != Stage::Review {
        return String::new();
    }
    let mut facts = Vec::new();
    if let Some((index, of)) = state.chunk {
        facts.push(format!("chunk {index}/{of}"));
    }
    if let Some(total) = state.files_total {
        facts.push(format!("file {}/{}", state.files_seen, total));
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
fn path_line(state: &State, width: usize, color: bool) -> Line<'static> {
    let Some(path) = &state.path else {
        return Line::default();
    };
    let label = "  reviewing  ";
    Line::from(vec![
        Span::styled(label.to_string(), dim(color)),
        Span::raw(clip_start(path, width.saturating_sub(label.len()))),
    ])
}

/// The row that has to move. What it says is why the run is not answering:
/// waiting on the model, or waiting on a tool the model asked for.
fn activity_line(state: &State, tick: usize, color: bool, now: Instant) -> Line<'static> {
    let frame = SPINNER[tick % SPINNER.len()];
    let mut spans = vec![Span::styled(
        format!("  {frame} "),
        styled(color, Color::Cyan),
    )];
    if let Some((tool, since)) = &state.tool {
        spans.push(Span::styled(format!("running {tool}"), bold(color)));
        spans.push(Span::raw(format!(
            "  ·  {}",
            duration(now.saturating_duration_since(*since))
        )));
        return Line::from(spans);
    }
    if let Some(since) = state.waiting_since {
        spans.push(Span::styled(
            "waiting for the model".to_string(),
            bold(color),
        ));
        if let Some((round, of)) = state.exchange {
            // How many times this file has been round the loop of the model
            // asking for context and being answered. "exchange" left people
            // asking what was being counted; a round of a conversation with a
            // limit on it is what this is. The ceiling earns its place because
            // reaching it ends the file early, on half the evidence, so the
            // last one says so instead of looking like any other.
            let last = round >= of;
            spans.push(Span::raw("  ·  "));
            spans.push(Span::styled(
                match last {
                    true => format!("round {round}/{of} (last)"),
                    false => format!("round {round}/{of}"),
                },
                match last {
                    true => styled(color, Color::Yellow),
                    false => Style::default(),
                },
            ));
        }
        spans.push(Span::raw(format!(
            "  ·  {}",
            duration(now.saturating_duration_since(since))
        )));
        return Line::from(spans);
    }
    // Nothing is being waited on, so the row says which stage is working —
    // and, when none is, whether that is because the run has not begun or
    // because it is over. "starting" under six finished stages was a small
    // lie the last frame told for a tenth of a second.
    let running = state
        .stages
        .iter()
        .find(|row| matches!(row.step, Step::Running { .. }));
    let what = match running {
        Some(row) => row.stage.name(),
        None => match state
            .stages
            .iter()
            .any(|row| matches!(row.step, Step::Done { .. }))
        {
            true => "done",
            false => "starting",
        },
    };
    spans.push(Span::styled(what.to_string(), bold(color)));
    Line::from(spans)
}

/// `$HOME` written the way a person writes it.
fn home_relative(path: &Path) -> String {
    let shown = path.display().to_string();
    match dirs::home_dir().map(|home| home.display().to_string()) {
        Some(home) if !home.is_empty() && shown.starts_with(&home) => {
            format!("~{}", &shown[home.len()..])
        }
        _ => shown,
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

    const WIDTH: u16 = 100;
    /// Named so that moving a row does not turn every assertion into a puzzle.
    const WHO: usize = 0;
    const WHAT: usize = 1;
    const FIRST_STAGE: usize = 2;
    const GAP: usize = FIRST_STAGE + Stage::ALL.len();
    const PATH: usize = GAP + 1;
    const ACTIVITY: usize = PATH + 1;

    /// Rows as a person would read them, trailing blanks removed. Drawn
    /// through an inline viewport, which is the one production uses: a
    /// fullscreen frame would hide anything that depends on the block being
    /// anchored into a terminal that has other output above it.
    fn rows(state: &State, tick: usize, now: Instant) -> Vec<String> {
        let mut terminal = Terminal::with_options(
            TestBackend::new(WIDTH, 30),
            TerminalOptions {
                viewport: Viewport::Inline(HEIGHT),
            },
        )
        .expect("a test terminal has no io");
        terminal
            .draw(|frame| {
                assert_eq!(frame.area().height, HEIGHT, "the block is the viewport");
                draw(frame, state, tick, false, now);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..HEIGHT)
            .map(|row| {
                (0..WIDTH)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// The title says what is being reviewed and what can be read while doing
    /// it; the checklist names all six stages from the first frame, so what is
    /// still coming is as visible as what is done.
    #[test]
    fn the_block_is_exactly_the_viewport_and_names_every_stage_up_front() {
        let now = Instant::now();
        let state = crate::cli::status::tests::mid_review(now);
        let rows = rows(&state, 0, now);

        assert_eq!(rows.len(), HEIGHT as usize, "the height never moves");
        assert_eq!(
            rows[WHO],
            format!(
                "reviewbot {}  ·  run 7f3a9c1e  ·  deepseek-v4-flash",
                env!("CARGO_PKG_VERSION")
            ),
            "the run is named while it is still running, not only afterwards"
        );
        assert_eq!(rows[WHAT], "change.diff  ·  worktree /repo");
        assert!(
            rows[FIRST_STAGE].starts_with("  ✓ 1 input     9 files"),
            "{:?}",
            rows[FIRST_STAGE]
        );
        assert!(
            rows[FIRST_STAGE + 1].starts_with("  ✓ 2 triage    7 chunks, 2 files skipped"),
            "{:?}",
            rows[FIRST_STAGE + 1]
        );
        assert!(
            rows[FIRST_STAGE + 2]
                .starts_with("  ▸ 3 review    chunk 3/7  ·  file 1/7  ·  1.8300 / 10.0000 CNY"),
            "{:?}",
            rows[FIRST_STAGE + 2]
        );
        assert!(
            rows[FIRST_STAGE].ends_with("0.0s"),
            "each row keeps its own clock"
        );
    }

    #[test]
    fn a_stage_that_has_not_started_is_marked_and_says_nothing_else() {
        let now = Instant::now();
        let state = crate::cli::status::tests::mid_review(now);
        let rows = rows(&state, 0, now);

        assert_eq!(rows[FIRST_STAGE + 3], "  · 4 merge");
        assert_eq!(rows[FIRST_STAGE + 4], "  · 5 report");
        assert_eq!(rows[FIRST_STAGE + 5], "  · 6 publish");
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
        assert_eq!(rows[PATH], "  reviewing  src/foo.c");
        assert_eq!(
            rows[ACTIVITY], "  ⠋ running search_repo  ·  0.0s",
            "a tool in flight is what the wait is for"
        );
    }

    /// Between tool calls the wait belongs to the model, said as a round of a
    /// conversation with a limit on it.
    #[test]
    fn waiting_on_the_model_says_which_round_it_is() {
        let now = Instant::now();
        let mut state = crate::cli::status::tests::mid_review(now);
        state.apply(reviewbot::progress::Event::Round { round: 3, of: 12 }, now);

        assert_eq!(
            rows(&state, 2, now)[ACTIVITY],
            "  ⠹ waiting for the model  ·  round 3/12  ·  0.0s"
        );
    }

    /// The last round is worth marking: reaching the ceiling ends the file
    /// there, so whatever it says next is said on half the evidence.
    #[test]
    fn the_last_round_before_the_ceiling_says_so() {
        let now = Instant::now();
        let mut state = crate::cli::status::tests::mid_review(now);
        state.apply(reviewbot::progress::Event::Round { round: 12, of: 12 }, now);

        assert_eq!(
            rows(&state, 0, now)[ACTIVITY],
            "  ⠋ waiting for the model  ·  round 12/12 (last)  ·  0.0s"
        );
    }

    /// What prints next has to start where the block started. `Terminal::clear`
    /// alone restores the cursor to wherever the last frame left it, which put
    /// the final summary indented and under a screen's worth of blank rows.
    #[test]
    fn handing_the_terminal_back_leaves_the_cursor_where_the_block_began() {
        let now = Instant::now();
        let state = crate::cli::status::tests::mid_review(now);
        let mut terminal = Terminal::with_options(
            TestBackend::new(WIDTH, 30),
            TerminalOptions {
                viewport: Viewport::Inline(HEIGHT),
            },
        )
        .expect("a test terminal has no io");
        terminal
            .draw(|frame| draw(frame, &state, 0, false, now))
            .expect("draw");
        let origin = terminal.get_frame().area().as_position();
        // Where painting leaves it: somewhere inside the block.
        terminal
            .set_cursor_position((7, origin.y + 4))
            .expect("cursor");

        hand_back(&mut terminal);

        assert_eq!(terminal.get_cursor_position().expect("cursor"), origin);
        let buffer = terminal.backend().buffer().clone();
        let block: String = (0..HEIGHT)
            .flat_map(|row| (0..WIDTH).map(move |column| (column, row)))
            .map(|position| buffer[position].symbol())
            .collect();
        assert!(block.trim().is_empty(), "the block is gone: {block:?}");
    }

    /// A run that is over does not claim to be starting, which is what the
    /// last frame before the block is wiped used to say.
    #[test]
    fn a_finished_run_does_not_say_it_is_starting() {
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

        assert_eq!(rows(&state, 0, now)[ACTIVITY], "  ⠋ done");
    }

    /// Home is written the way a person writes it, and a path too long for the
    /// row loses its front: two files in one project differ at the end.
    #[test]
    fn paths_are_shortened_where_they_carry_least() {
        let home = dirs::home_dir().expect("home");
        assert_eq!(
            home_relative(&home.join("checkout")),
            format!("~{}checkout", std::path::MAIN_SEPARATOR)
        );
        assert_eq!(
            home_relative(std::path::Path::new("/srv/reviews/change.diff")),
            "/srv/reviews/change.diff"
        );

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

        let first = rows(&state, 0, now)[ACTIVITY].clone();
        let second = rows(&state, 1, now)[ACTIVITY].clone();
        assert_ne!(first, second, "the one row that has to move, moves");
        assert!(first.contains('⠋') && second.contains('⠙'));
    }
}
