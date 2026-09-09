//! The terminal's view of a run while the run still owns stdout.
//!
//! A terminal gets one growing summary-shaped block, so finishing does not
//! replace one vocabulary with another or spend rows on facts not known yet.
//! A pipe gets a sparse event log instead: cursor motion is useful only when
//! there is a cursor to move, and retaining each completed stage is what
//! makes CI output useful after the process ends.

use std::cell::RefCell;
use std::io::Write;

use reviewbot::progress::{Event, Progress};

use super::render::{SUMMARY_LABEL_WIDTH, push_summary_line};

const STAGES: u8 = 6;

/// A serial progress consumer. Both the accumulated facts and the writer use
/// interior mutability because `Progress` is shared by all six stages.
pub struct Status<W: Write> {
    writer: RefCell<W>,
    state: RefCell<State>,
    tty: bool,
    color: bool,
}

impl<W: Write> Status<W> {
    pub fn new(writer: W, tty: bool, color: bool) -> Self {
        Self {
            writer: RefCell::new(writer),
            state: RefCell::new(State::default()),
            tty,
            color: tty && color,
        }
    }

    /// Remove the temporary terminal block before the one canonical final
    /// renderer writes. Pipes retain their event history by design.
    pub fn finish(&self) {
        if !self.tty {
            return;
        }
        let mut state = self.state.borrow_mut();
        if state.drawn_rows > 0 {
            self.write(&format!("\x1b[{}A\x1b[J", state.drawn_rows));
            state.drawn_rows = 0;
        }
    }

    fn write(&self, text: &str) {
        let mut writer = self.writer.borrow_mut();
        let _ = writer.write_all(text.as_bytes());
        let _ = writer.flush();
    }

    fn redraw(&self, state: &mut State) {
        let mut out = String::new();
        if state.drawn_rows > 0 {
            out.push_str(&format!("\x1b[{}A\x1b[J", state.drawn_rows));
        }
        state.drawn_rows = state.push_block(&mut out, self.color);
        self.write(&out);
    }

    fn append(&self, line: String) {
        self.write(&line);
    }
}

impl<W: Write> Progress for Status<W> {
    fn emit(&self, event: Event) {
        let mut state = self.state.borrow_mut();
        let line = state.apply(event);
        if self.tty {
            self.redraw(&mut state);
        } else if let Some(line) = line {
            self.append(line);
        }
    }
}

#[derive(Default)]
struct State {
    run_id: String,
    model: String,
    overall: String,
    comments: String,
    skipped: String,
    unreviewed: String,
    budget: String,
    report: String,
    summary: String,
    published: String,
    stage_number: Option<u8>,
    stage: String,
    stage_detail: Option<String>,
    chunk: Option<(usize, usize, String)>,
    round: Option<(u32, u32)>,
    tool: Option<String>,
    drawn_rows: usize,
}

impl State {
    fn apply(&mut self, event: Event) -> Option<String> {
        match event {
            Event::RunStarted {
                run_id,
                run_dir,
                model,
                input,
            } => {
                self.run_id = run_id;
                self.model = model;
                self.report = run_dir.join("report.md").display().to_string();
                self.summary = run_dir.join("summary.json").display().to_string();
                Some(format!(
                    "run {}  model {}  input {}\n",
                    self.run_id, self.model, input
                ))
            }
            Event::StageStarted { number, name } => {
                self.stage_number = Some(number);
                self.stage = name.to_string();
                self.stage_detail = None;
                self.chunk = None;
                self.round = None;
                self.tool = None;
                None
            }
            Event::StageFinished {
                number,
                name,
                detail,
            } => {
                self.stage_number = Some(number);
                self.stage = name.to_string();
                self.learn_stage(name, &detail);
                self.stage_detail = Some(detail.clone());
                self.chunk = None;
                self.round = None;
                self.tool = None;
                Some(format!("[{number}/{STAGES}] {name:<9} {detail}\n"))
            }
            Event::Chunk { index, of, path } => {
                self.stage_detail = None;
                self.chunk = Some((index, of, path));
                self.round = None;
                self.tool = None;
                self.chunk_line()
            }
            Event::Round { round, of } => {
                self.round = Some((round, of));
                self.tool = None;
                None
            }
            Event::Tool { name } => {
                self.tool = Some(name);
                None
            }
            Event::Spend {
                spent,
                budget,
                currency,
            } => {
                self.budget = match budget {
                    Some(ceiling) => format!("{spent:.4} / {ceiling:.4} {currency}"),
                    None => format!("{spent:.4} {currency} (no ceiling)"),
                };
                None
            }
        }
    }

    /// Stage details deliberately use the final summary's nouns. Reading the
    /// small numeric claims here avoids coupling the terminal to checkpoints;
    /// facts the channel does not carry remain visibly unknown.
    fn learn_stage(&mut self, name: &str, detail: &str) {
        let current = detail.strip_suffix(", from checkpoint").unwrap_or(detail);
        match name {
            "triage" => {
                if let Some(count) = number_before(current, " files skipped") {
                    self.skipped = format!("{count} files");
                }
            }
            "review" => {
                self.unreviewed = number_before(current, " files unreviewed")
                    .map(|count| format!("{count} files"))
                    .unwrap_or_else(|| "0 files".to_string());
            }
            "merge" => {
                if let Some(count) = number_before(current, " comments") {
                    self.comments = count.to_string();
                }
                if let Some(score) = number_after(current, "overall ", " / 100") {
                    self.overall = format!("{score} / 100");
                } else if current.contains("not scored") {
                    self.overall = "not scored".to_string();
                }
            }
            "publish" => {
                self.published = number_before(current, " comments published")
                    .map(|count| format!("{count} comments"))
                    .unwrap_or_else(|| "0 comments".to_string());
            }
            _ => {}
        }
    }

    fn chunk_line(&self) -> Option<String> {
        let number = self.stage_number?;
        let mut facts = Vec::new();
        if let Some((index, of, path)) = &self.chunk {
            facts.push(format!("chunk {index}/{of}"));
            facts.push(path.clone());
        }
        if !self.budget.is_empty() {
            facts.push(self.budget.clone());
        }
        if facts.is_empty() {
            return None;
        }
        Some(format!(
            "[{number}/{STAGES}] {:<9} {}\n",
            self.stage,
            facts.join("  ")
        ))
    }

    fn push_block(&self, out: &mut String, color: bool) -> usize {
        let mut rows = 0;
        for (label, value) in [
            ("run_id", self.run_id.as_str()),
            ("model", self.model.as_str()),
            ("overall", self.overall.as_str()),
            ("comments", self.comments.as_str()),
            ("skipped", self.skipped.as_str()),
            ("unreviewed", self.unreviewed.as_str()),
            ("budget", self.budget.as_str()),
            ("report", self.report.as_str()),
            ("summary", self.summary.as_str()),
            ("published", self.published.as_str()),
        ] {
            if value.is_empty() {
                continue;
            }
            if color {
                out.push_str(&format!(
                    "\x1b[36m{label:<SUMMARY_LABEL_WIDTH$}\x1b[0m {value}\n"
                ));
            } else {
                push_summary_line(out, label, value);
            }
            rows += 1;
        }
        let activity = self.activity();
        if color {
            out.push_str(&format!("\x1b[1;32m{activity}\x1b[0m\n"));
        } else {
            out.push_str(&activity);
            out.push('\n');
        }
        rows + 1
    }

    fn activity(&self) -> String {
        if self.stage.is_empty() {
            return "starting".to_string();
        }
        let mut facts = vec![self.stage.clone()];
        if let Some(detail) = &self.stage_detail {
            facts.push(detail.clone());
            return facts.join("  ");
        }
        if let Some((index, of, path)) = &self.chunk {
            facts.push(format!("chunk {index}/{of}"));
            facts.push(path.clone());
        }
        if let Some((round, of)) = self.round {
            facts.push(format!("round {round}/{of}"));
        }
        if let Some(tool) = &self.tool {
            facts.push(format!("tool {tool}"));
        }
        facts.join("  ")
    }
}

fn number_before(text: &str, suffix: &str) -> Option<usize> {
    let before = text.split(suffix).next()?;
    before.split_whitespace().next_back()?.parse().ok()
}

fn number_after(text: &str, prefix: &str, suffix: &str) -> Option<usize> {
    let after = text.split_once(prefix)?.1;
    after.split_once(suffix)?.0.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn events() -> Vec<Event> {
        vec![
            Event::RunStarted {
                run_id: "7f3a9c1e".to_string(),
                run_dir: PathBuf::from("/tmp/runs/7f3a9c1e"),
                model: "deepseek-v4-flash".to_string(),
                input: "change.diff".to_string(),
            },
            Event::StageStarted {
                number: 2,
                name: "triage",
            },
            Event::StageFinished {
                number: 2,
                name: "triage",
                detail: "7 chunks, 2 files skipped".to_string(),
            },
            Event::StageStarted {
                number: 3,
                name: "review",
            },
            Event::Spend {
                spent: 0.75,
                budget: Some(10.0),
                currency: "CNY".to_string(),
            },
            Event::Chunk {
                index: 3,
                of: 7,
                path: "src/foo.c".to_string(),
            },
            Event::Round { round: 2, of: 6 },
            Event::Tool {
                name: "ripgrep".to_string(),
            },
            Event::Spend {
                spent: 1.83,
                budget: Some(10.0),
                currency: "CNY".to_string(),
            },
            Event::StageFinished {
                number: 3,
                name: "review",
                detail: "3 chunks reviewed, 2 files unreviewed".to_string(),
            },
        ]
    }

    /// Pipes preserve one record for each useful change and never emit a byte
    /// that asks a cursor to move.
    #[test]
    fn a_pipe_appends_progress_and_the_finished_stage() {
        let status = Status::new(Vec::new(), false, true);
        for event in events() {
            status.emit(event);
        }
        status.finish();

        let text = String::from_utf8(status.writer.into_inner()).expect("utf8");
        assert_eq!(
            text,
            "\
run 7f3a9c1e  model deepseek-v4-flash  input change.diff
[2/6] triage    7 chunks, 2 files skipped
[3/6] review    chunk 3/7  src/foo.c  0.7500 / 10.0000 CNY
[3/6] review    3 chunks reviewed, 2 files unreviewed
"
        );
        assert!(!text.contains("\x1b["));
    }

    /// Early in review only facts already heard from the run have rows.
    /// Rounds and tools still change the activity line, while the row count
    /// used by the next redraw follows the block's actual height.
    #[test]
    fn an_early_terminal_block_has_only_known_rows() {
        let status = Status::new(Vec::new(), true, false);
        for event in events().into_iter().take(8) {
            status.emit(event);
        }

        let written = String::from_utf8(status.writer.borrow().clone()).expect("utf8");
        let block = written
            .rsplit_once("\x1b[J")
            .map(|(_, block)| block)
            .unwrap_or(&written);
        assert_eq!(
            block,
            "\
run_id     7f3a9c1e
model      deepseek-v4-flash
skipped    2 files
budget     0.7500 / 10.0000 CNY
report     /tmp/runs/7f3a9c1e/report.md
summary    /tmp/runs/7f3a9c1e/summary.json
review  chunk 3/7  src/foo.c  round 2/6  tool ripgrep
"
        );

        status.finish();
        let written = String::from_utf8(status.writer.into_inner()).expect("utf8");
        assert!(written.ends_with("\x1b[7A\x1b[J"));
    }

    /// As stages finish the block grows by exactly the rows their details can
    /// establish. Fields absent from the channel never create dash-only rows.
    #[test]
    fn a_late_terminal_block_grows_with_known_stage_results() {
        let status = Status::new(Vec::new(), true, false);
        for event in events() {
            status.emit(event);
        }
        for event in [
            Event::StageStarted {
                number: 4,
                name: "merge",
            },
            Event::StageFinished {
                number: 4,
                name: "merge",
                detail: "4 comments, overall 54 / 100".to_string(),
            },
            Event::StageStarted {
                number: 5,
                name: "report",
            },
            Event::StageFinished {
                number: 5,
                name: "report",
                detail: "report.md and summary.json written".to_string(),
            },
            Event::StageStarted {
                number: 6,
                name: "publish",
            },
            Event::StageFinished {
                number: 6,
                name: "publish",
                detail: "nothing posted: this run was not asked to publish".to_string(),
            },
        ] {
            status.emit(event);
        }

        let written = String::from_utf8(status.writer.borrow().clone()).expect("utf8");
        let block = written.rsplit_once("\x1b[J").expect("redraw").1;
        assert_eq!(
            block,
            "\
run_id     7f3a9c1e
model      deepseek-v4-flash
overall    54 / 100
comments   4
skipped    2 files
unreviewed 2 files
budget     1.8300 / 10.0000 CNY
report     /tmp/runs/7f3a9c1e/report.md
summary    /tmp/runs/7f3a9c1e/summary.json
published  0 comments
publish  nothing posted: this run was not asked to publish
"
        );
        assert!(!block.contains("severity"));
        assert!(!block.contains("confidence"));
        assert!(!block.contains("stopped"));

        status.finish();
        let written = String::from_utf8(status.writer.into_inner()).expect("utf8");
        assert!(written.ends_with("\x1b[11A\x1b[J"));
    }

    /// Colour decorates the already-padded label and activity, so turning it
    /// off changes only escape bytes, never the in-place redraw contract.
    #[test]
    fn no_color_keeps_redraw_and_removes_only_color_sequences() {
        let plain = Status::new(Vec::new(), true, false);
        plain.emit(events().remove(0));
        let plain = String::from_utf8(plain.writer.into_inner()).expect("utf8");
        assert!(!plain.contains("\x1b[36m"));

        let colored = Status::new(Vec::new(), true, true);
        colored.emit(events().remove(0));
        let colored = String::from_utf8(colored.writer.into_inner()).expect("utf8");
        assert!(colored.contains("\x1b[36mrun_id    \x1b[0m 7f3a9c1e"));
    }
}
