//! Stage 5. `report.md`, `summary.json` and the `--output-dir` copies, read
//! off the checkpoints the stages before it wrote.
//!
//! Nothing here touches the network, and that is the point: re-entering a
//! finished run is the only way to get the report back, so rendering it again
//! has to be free and has to work on a machine with no reach at all. The
//! stage runs on every entry for the same reason.
//!
//! Its checkpoint is the `Summary`, because `publish` says those numbers
//! again on the MR and must not arrive at a second count of its own.

use serde::{Deserialize, Serialize};

use crate::domain::{Confidence, Severity, Stage};
use crate::record::layout;

use super::merge::MergeOutput;
use super::review::CutShort;
use super::triage::TriagePlan;
use super::{StageContext, StageError};

/// Everything the report is rendered from, gathered by the caller so the
/// stage takes one argument. No changeset and no platform: this stage reads
/// back what the run decided, it does not look the change up again.
pub struct ReportInput<'a> {
    pub plan: &'a TriagePlan,
    pub merged: &'a MergeOutput,
    pub unreviewed: &'a [String],
    /// Chunks whose investigation the loop ended early. A file the model was
    /// still reading around must not read like one it finished with.
    pub cut_short: &'a [CutShort],
    /// What this run's worktree could not do at all. A run that saw only the
    /// diff must not read like one that looked everywhere.
    pub unavailable: &'a [String],
}

/// `summary.json`: the same numbers as the report, for scripts.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Summary {
    pub run_id: String,
    pub model: String,
    pub overall_score: Option<u8>,
    pub summary: Option<String>,
    pub unscored_reason: Option<String>,
    pub comments: Vec<CountByBand>,
    /// The same findings counted the other way. Two breakdowns, because "how
    /// bad is the worst of it" and "how sure is any of it" are the two things
    /// a reader wants and neither answers the other.
    #[serde(default)]
    pub by_severity: Vec<CountBySeverity>,
    pub skipped: Vec<String>,
    pub unreviewed: Vec<String>,
    /// Chunks that gave nothing usable. Named so a thin report is not
    /// mistaken for a clean review.
    #[serde(default)]
    pub unproduced: Vec<String>,
    /// Chunks the loop stopped investigating before the model was done. Named
    /// for the same reason, and because the remedy is a config knob: a run
    /// where most files land here is asking for more rounds.
    #[serde(default)]
    pub cut_short: Vec<String>,
    /// What this run's worktree could not do, which bounds everything above
    /// it. Empty on a run that could look at whatever it liked.
    #[serde(default)]
    pub unavailable: Vec<String>,
    pub spent: f64,
    pub budget: Option<f64>,
    pub currency: String,
}

impl Summary {
    /// Which run, which model, and the model's number. The three lines both
    /// the report and the MR's summary comment open with.
    pub fn basics(&self) -> String {
        let overall = match self.overall_score {
            Some(score) => format!("overall: {score} / 100"),
            None => "overall: not scored".to_string(),
        };
        format!(
            "run: `{}`\nmodel: `{}`\n{overall}\n",
            self.run_id, self.model
        )
    }

    /// What the model said about the change — or why there is no score — and
    /// then how to read the numbers. Word for word the same in the report and
    /// on the MR, because one person reads both and they must not disagree.
    pub fn verdict(&self) -> String {
        let mut verdict = match self.overall_score {
            Some(_) => match &self.summary {
                Some(text) => format!("{text}\n\n"),
                None => String::new(),
            },
            None => {
                let reason = self
                    .unscored_reason
                    .as_deref()
                    .unwrap_or("no reason was recorded");
                format!("{reason}\n\nNot scored is not a score of 0.\n\n")
            }
        };
        verdict.push_str(LEGEND);
        verdict
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CountByBand {
    pub band: Confidence,
    pub count: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CountBySeverity {
    pub band: Severity,
    pub count: usize,
}

pub struct Report;

impl Report {
    pub fn run(
        context: &mut StageContext<'_>,
        input: &ReportInput<'_>,
    ) -> Result<Summary, StageError> {
        let summary = Self::summary(context, input);
        Self::write_artifacts(context, input, &summary)?;
        context.complete(Stage::Report, &summary)?;
        Ok(summary)
    }

    /// Rewrite `report.md` and `summary.json` from the checkpoints, and copy
    /// both out when `--output-dir` asked for them.
    fn write_artifacts(
        context: &StageContext<'_>,
        input: &ReportInput<'_>,
        summary: &Summary,
    ) -> Result<(), StageError> {
        let report = Self::report(input, summary);
        context
            .recorder
            .write_artifact(layout::REPORT, report.as_bytes())?;
        let summary_bytes = serde_json::to_vec_pretty(summary).map_err(|source| {
            crate::record::RecordError::Serialize {
                file: layout::SUMMARY.to_string(),
                source,
            }
        })?;
        context
            .recorder
            .write_artifact(layout::SUMMARY, &summary_bytes)?;
        Self::export(context, &report, &summary_bytes)?;
        Ok(())
    }

    fn summary(context: &StageContext<'_>, input: &ReportInput<'_>) -> Summary {
        let meta = context.recorder.meta();
        Summary {
            run_id: meta.run_id.clone(),
            model: meta.model.clone(),
            overall_score: input.merged.overall_score,
            summary: input.merged.summary.clone(),
            unscored_reason: input.merged.unscored_reason.clone(),
            comments: input
                .merged
                .counts()
                .into_iter()
                .map(|(band, count)| CountByBand { band, count })
                .collect(),
            by_severity: input
                .merged
                .severity_counts()
                .into_iter()
                .map(|(band, count)| CountBySeverity { band, count })
                .collect(),
            skipped: input
                .plan
                .skipped
                .iter()
                .map(|file| file.path.clone())
                .collect(),
            unreviewed: input.unreviewed.to_vec(),
            unproduced: input
                .merged
                .unproduced
                .iter()
                .map(|chunk| format!("{}: {}", chunk.path, chunk.reason))
                .collect(),
            cut_short: input
                .cut_short
                .iter()
                .map(|chunk| format!("{}: {}", chunk.path, chunk.reason))
                .collect(),
            unavailable: input.unavailable.to_vec(),
            spent: context.budget.spent(),
            budget: context.budget.ceiling(),
            currency: context.budget.currency().to_string(),
        }
    }

    /// Findings and coverage in one list. Prompt, diff hunk, model output
    /// and usage live in `traces/`; each finding names its `trace_id`.
    /// Spend stays in `summary.json` and on the CLI, not here.
    fn report(input: &ReportInput<'_>, summary: &Summary) -> String {
        let mut report = String::from("# Reviewbot report\n\n");
        report.push_str(&summary.basics());
        report.push('\n');
        // Ahead of the model's paragraph, because it bounds every word of it.
        report.push_str(&coverage_bound(&summary.unavailable));
        report.push_str(&summary.verdict());
        report.push('\n');

        let mut items = String::new();
        for (index, comment) in input.merged.comments.iter().enumerate() {
            items.push_str(&finding_item(comment, input.merged.badge(index)));
        }
        for chunk in &input.merged.unproduced {
            items.push_str(&coverage_item(&chunk.path, &chunk.reason));
        }
        for file in &input.plan.skipped {
            items.push_str(&coverage_item(&file.path, &file.reason));
        }
        for chunk in input.cut_short {
            items.push_str(&coverage_item(
                &chunk.path,
                &format!("investigation cut short: {}", chunk.reason),
            ));
        }
        for path in &summary.unreviewed {
            items.push_str(&coverage_item(path, "not reviewed"));
        }
        if items.is_empty() {
            report.push_str("None.\n");
        } else {
            report.push_str(&items);
        }
        report
    }

    /// `--output-dir` gets the two artifacts that may leave the machine. The run
    /// directory as a whole may not: it holds the internal trace view.
    fn export(
        context: &StageContext<'_>,
        report: &str,
        summary_bytes: &[u8],
    ) -> Result<(), StageError> {
        let Some(output_dir) = &context.settings.options.output_dir else {
            return Ok(());
        };
        let run_id = &context.recorder.meta().run_id;
        let io = |path: std::path::PathBuf| {
            move |source| crate::record::RecordError::Io { path, source }
        };
        std::fs::create_dir_all(output_dir).map_err(io(output_dir.clone()))?;
        let report_path = output_dir.join(layout::exported_report(run_id));
        std::fs::write(&report_path, report).map_err(io(report_path.clone()))?;
        let summary_path = output_dir.join(layout::exported_summary(run_id));
        std::fs::write(&summary_path, summary_bytes).map_err(io(summary_path.clone()))?;
        Ok(())
    }
}

const LEGEND: &str = "\
A finding is headed by two of the model's own judgements, published as given: \
how much it matters if it is real, then how sure the model is that it is. They \
are meant to disagree — a severe finding held with low confidence is a reason \
to look, not a reason to block. A badge next to them is reviewbot's check of a \
quotation — a fact it verified, not a judgement it made, and it never moves \
either number. Overall is the model's judgement of what this run found, not a \
code quality score: reviewbot only read the changed lines, one file at a \
time.\n";

/// What this run could not look at, said before anything it concluded.
///
/// A review whose worktree could answer nothing still runs, still calls the
/// model on every chunk and still produces a report — and that report is
/// indistinguishable from a thorough one: the finding list is empty either
/// way, the score is the model's either way, and the paragraph says it found
/// nothing either way. This paragraph is the difference.
fn coverage_bound(unavailable: &[String]) -> String {
    if unavailable.is_empty() {
        return String::new();
    }
    format!(
        "coverage: this run {}. That bounds everything below it — reviewing from what was \
         available is a normal way to run reviewbot, and the findings stand on their own, but \
         nothing outside it was examined, so an empty list here is not evidence that there was \
         nothing to find.\n\n",
        unavailable.join("; it also "),
    )
}

fn finding_item(comment: &crate::domain::Comment, badge: Option<&str>) -> String {
    let line = comment
        .target
        .line
        .map(|line| format!(":{line}"))
        .unwrap_or_default();
    format!(
        "## [{} {}% / {} {}%]{} `{}{}`\n\n{}\n\nsuggestion:\n{}\n\n{}\n\n",
        comment.severity,
        comment.severity_score,
        comment.confidence,
        comment.confidence_score,
        badge_suffix(badge),
        comment.target.path,
        line,
        comment.body,
        comment.suggestion,
        trace_line(&comment.trace_id)
    )
}

fn coverage_item(path: &str, reason: &str) -> String {
    format!("## `{path}`\n\n{reason}\n\n")
}

/// What goes between the confidence and the finding. Empty when reviewbot
/// checked nothing, so a comment with no quotation reads as it always did.
///
/// Shared with `publish`, which heads its MR comments the same way. A finding
/// a person reads on the MR and the same finding in the report have to be
/// recognisable as one thing, so the wording has one owner.
pub(super) fn badge_suffix(badge: Option<&str>) -> String {
    badge.map(|badge| format!(" {badge}")).unwrap_or_default()
}

/// Visible lookup key, same wording on the report and on the MR comment.
pub(super) fn trace_line(trace_id: &str) -> String {
    format!("trace: `{trace_id}`")
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::domain::{Comment, CommentTarget};
    use crate::record::{ContextFile, Trace};
    use crate::stage::fixture::StageFixture;

    const SECRET_BODY: &str = "static int callee(void) { return 1; }";

    fn comment(trace_id: &str) -> Comment {
        Comment {
            target: CommentTarget {
                path: "src/parse.c".to_string(),
                line: Some(11),
                end_line: None,
            },
            body: "the index is a constant 5 while buf is char[3]".to_string(),
            suggestion: "bound the index to buf's length".to_string(),
            severity: Severity::Major,
            severity_score: 80,
            confidence: Confidence::Certain,
            confidence_score: 92,
            trace_id: trace_id.to_string(),
        }
    }

    /// The internal trace `review` leaves behind, complete with the body of a
    /// file that was pulled in as context.
    fn internal_trace(trace_id: &str) -> Trace {
        let mut trace = Trace::new(trace_id);
        trace.diff = "@@ -10,2 +10,3 @@\n+buf[5] = 0;\n".to_string();
        trace.prompt = format!("instructions\n\ncontext of src/parse.h:\n{SECRET_BODY}\n");
        trace.model_output = r#"{"comments":[{"path":"src/parse.c"}]}"#.to_string();
        trace.note(Stage::Merge, "line 12 is not commentable; moved -1 to 11");
        trace.context_files.push(ContextFile {
            path: "src/parse.h".to_string(),
            first_line: 1,
            last_line: 1,
            body: SECRET_BODY.to_string(),
        });
        trace
    }

    fn render(fixture: &mut StageFixture, merged: &MergeOutput) {
        let plan = TriagePlan::default();
        let input = ReportInput {
            plan: &plan,
            merged,
            unreviewed: &[],
            cut_short: &[],
            unavailable: &[],
        };
        let mut context = fixture.context();
        Report::run(&mut context, &input).expect("the report is written");
    }

    #[test]
    fn the_report_is_the_findings_and_the_trace_stays_in_traces() {
        let mut fixture = StageFixture::new(Vec::new());
        fixture
            .recorder()
            .write_trace(&internal_trace("review-src_parse.c"))
            .expect("trace written");
        let merged = MergeOutput {
            comments: vec![comment("review-src_parse.c")],
            overall_score: Some(54),
            summary: Some("one certain finding".to_string()),
            unproduced: vec![crate::stage::merge::Unproduced {
                path: "src/other.c".to_string(),
                reason: "unreadable twice".to_string(),
            }],
            ..MergeOutput::default()
        };
        let plan = TriagePlan {
            skipped: vec![crate::stage::triage::SkippedFile {
                path: "src/gone.c".to_string(),
                reason: "the whole file was deleted".to_string(),
            }],
            ..TriagePlan::default()
        };
        let input = ReportInput {
            plan: &plan,
            merged: &merged,
            unreviewed: &[],
            cut_short: &[],
            unavailable: &[],
        };
        {
            let mut context = fixture.context();
            Report::run(&mut context, &input).expect("the report is written");
        }
        let report = fixture.report();

        assert!(report.starts_with("# Reviewbot report\n"), "{report}");
        assert!(report.contains("run: `test-run`"), "{report}");
        assert!(report.contains("model: `deepseek-v4-flash`"), "{report}");
        assert!(
            report.contains("## [major 80% / certain 92%] `src/parse.c:11`"),
            "{report}"
        );
        assert!(
            report.contains("the index is a constant 5 while buf is char[3]"),
            "{report}"
        );
        assert!(
            report.contains("suggestion:\nbound the index to buf's length"),
            "{report}"
        );
        assert!(report.contains("trace: `review-src_parse.c`"), "{report}");
        assert!(
            !report.contains("traces/"),
            "the report names the id, not the file path: {report}"
        );
        assert!(
            !report.contains("<details>"),
            "invocation details stay in traces/: {report}"
        );
        assert!(
            !report.contains("+buf[5] = 0;"),
            "the triggering hunk is not inlined: {report}"
        );
        assert!(
            !report.contains("instructions"),
            "the prompt is not inlined: {report}"
        );
        assert!(
            !report.contains("moved -1 to 11"),
            "checks stay in the trace file: {report}"
        );
        assert!(
            !report.contains(SECRET_BODY),
            "file bodies never leave the internal trace: {report}"
        );
        assert!(report.contains("overall: 54 / 100"), "{report}");
        assert!(report.contains("not a code quality score"), "{report}");
        assert!(
            !report.contains("## comments"),
            "findings and coverage share one list: {report}"
        );
        assert!(!report.contains("## skipped"), "{report}");
        assert!(!report.contains("## no answer"), "{report}");
        assert!(!report.contains("## budget"), "{report}");
        assert!(
            !report.contains("spent "),
            "spend is not for the report reader: {report}"
        );
        assert!(report.contains("## `src/other.c`"), "{report}");
        assert!(report.contains("unreadable twice"), "{report}");
        assert!(report.contains("## `src/gone.c`"), "{report}");
        assert!(report.contains("the whole file was deleted"), "{report}");

        let stored = fixture
            .recorder()
            .read_trace("review-src_parse.c")
            .expect("readable")
            .expect("the trace is on disk");
        assert!(
            stored.context_files[0].body.contains(SECRET_BODY),
            "the internal view still has the file body"
        );
        let published = fixture.recorder().published_trace(&stored);
        assert!(
            published
                .context_files
                .iter()
                .all(|file| file.path == "src/parse.h"),
            "{:?}",
            published.context_files
        );
        assert!(published.diff.contains("+buf[5] = 0;"));
        assert!(!published.prompt.contains(SECRET_BODY));
    }

    #[test]
    fn a_score_of_zero_does_not_read_as_not_scored() {
        let mut fixture = StageFixture::new(Vec::new());
        let scored = MergeOutput {
            comments: Vec::new(),
            overall_score: Some(0),
            summary: Some("do not merge this".to_string()),
            ..MergeOutput::default()
        };
        render(&mut fixture, &scored);
        let report = fixture.report();
        assert!(report.contains("overall: 0 / 100"), "{report}");
        assert!(!report.contains("overall: not scored"), "{report}");
    }

    #[test]
    fn an_unscored_run_gives_the_reason_instead_of_a_number() {
        let mut fixture = StageFixture::new(Vec::new());
        let unscored = MergeOutput {
            unscored_reason: Some("not scored: budget exhausted".to_string()),
            unproduced: vec![crate::stage::merge::Unproduced {
                path: "src/parse.c".to_string(),
                reason: "unreadable twice".to_string(),
            }],
            ..MergeOutput::default()
        };
        render(&mut fixture, &unscored);
        let report = fixture.report();

        assert!(report.contains("overall: not scored"), "{report}");
        assert!(report.contains("budget exhausted"), "{report}");
        assert!(
            report.contains("Not scored is not a score of 0."),
            "{report}"
        );
        assert!(report.contains("## `src/parse.c`"), "{report}");
        assert!(report.contains("unreadable twice"), "{report}");
        assert!(!report.contains("## no answer"), "{report}");
        assert!(!report.contains("## budget"), "{report}");

        let bytes = fixture
            .recorder()
            .read_artifact(layout::SUMMARY)
            .expect("readable")
            .expect("summary.json is always written");
        let summary: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert!(
            summary["overall_score"].is_null(),
            "a missing score is null, never 0: {summary}"
        );
        assert!(summary["unscored_reason"].is_string());
    }

    /// The badge sits next to the model's number without disturbing it.
    /// Unused checkers stay on the chunk's trace; they are not a report item.
    #[test]
    fn the_badge_sits_beside_the_score() {
        let mut fixture = StageFixture::new(Vec::new());
        let merged = MergeOutput {
            comments: vec![comment("review-src_parse.c")],
            badges: vec![Some(crate::stage::merge::TOOL_FOUND.to_string())],
            overall_score: Some(54),
            ..MergeOutput::default()
        };
        render(&mut fixture, &merged);
        let report = fixture.report();

        assert!(
            report.contains("## [major 80% / certain 92%] found by tool `src/parse.c:11`"),
            "{report}"
        );
        assert!(!report.contains("none called"), "{report}");
    }

    /// A file the loop stopped investigating is not a file that came back
    /// clean, and the report is where the difference has to show: the run
    /// that produced it saw two thirds of its chunks end this way and read
    /// as a quiet review.
    #[test]
    fn a_chunk_the_loop_cut_short_is_named_in_the_report_and_the_summary() {
        let mut fixture = StageFixture::new(Vec::new());
        let merged = MergeOutput {
            overall_score: Some(70),
            ..MergeOutput::default()
        };
        let plan = TriagePlan::default();
        let cut_short = [CutShort {
            path: "src/emit.c".to_string(),
            reason: "the tool loop reached its ceiling of 12 rounds".to_string(),
        }];
        let input = ReportInput {
            plan: &plan,
            merged: &merged,
            unreviewed: &[],
            cut_short: &cut_short,
            unavailable: &[],
        };
        {
            let mut context = fixture.context();
            Report::run(&mut context, &input).expect("the report is written");
        }

        let report = fixture.report();
        assert!(report.contains("## `src/emit.c`"), "{report}");
        assert!(
            report.contains(
                "investigation cut short: the tool loop reached its ceiling of 12 rounds"
            ),
            "{report}"
        );
        let bytes = fixture
            .recorder()
            .read_artifact(layout::SUMMARY)
            .expect("readable")
            .expect("summary.json is always written");
        let summary: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(
            summary["cut_short"][0],
            "src/emit.c: the tool loop reached its ceiling of 12 rounds"
        );
    }

    /// The failure this exists for, from a real run: a plain diff with no
    /// worktree behind it. Nothing could be read, the model said so in every
    /// reply, the finding list came back empty — and the report said
    /// "overall 100 / 100 ... appears safe to merge" with not one word about
    /// the run having seen only the diff. A clean review and a blind one have
    /// to be told apart by a person reading the report, so it is said before
    /// anything the model concluded.
    #[test]
    fn a_run_that_saw_only_the_diff_does_not_read_like_a_thorough_one() {
        let mut fixture = StageFixture::new(Vec::new());
        let merged = MergeOutput {
            overall_score: Some(100),
            summary: Some("Nothing worth flagging; safe to merge.".to_string()),
            ..MergeOutput::default()
        };
        let plan = TriagePlan::default();
        let unavailable = ["could not read any file: it saw the diff and nothing else".to_string()];
        let input = ReportInput {
            plan: &plan,
            merged: &merged,
            unreviewed: &[],
            cut_short: &[],
            unavailable: &unavailable,
        };
        {
            let mut context = fixture.context();
            Report::run(&mut context, &input).expect("the report is written");
        }

        let report = fixture.report();
        let coverage = report.find("coverage: this run").expect(&report);
        assert!(
            report.contains("could not read any file: it saw the diff and nothing else"),
            "{report}"
        );
        assert!(
            report.contains("not evidence that there was nothing to find"),
            "{report}"
        );
        assert!(
            coverage < report.find("safe to merge").expect(&report),
            "the bound has to be read before the verdict it bounds: {report}"
        );

        let bytes = fixture
            .recorder()
            .read_artifact(layout::SUMMARY)
            .expect("readable")
            .expect("summary.json is always written");
        let summary: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(
            summary["unavailable"][0],
            "could not read any file: it saw the diff and nothing else"
        );
    }

    /// And a run that could look at everything says nothing about coverage:
    /// the ordinary case must not pay for a caveat with nothing to caveat.
    #[test]
    fn a_run_that_could_look_anywhere_prints_no_coverage_note() {
        let mut fixture = StageFixture::new(Vec::new());
        let merged = MergeOutput {
            overall_score: Some(90),
            ..MergeOutput::default()
        };
        render(&mut fixture, &merged);
        assert!(
            !fixture.report().contains("coverage:"),
            "{}",
            fixture.report()
        );
    }
}
