//! Stage 5. The Markdown report is always written; traces stay in `traces/`.
//! The MR/PR only hears from us when `--publish` was asked for, and posting
//! is idempotent: comments go out as the finding plus a visible `trace_id`,
//! with the hidden marker for dedup.
//!
//! M1 writes the report, the summary and an empty `published.json`. The
//! GitLab and GitHub calls arrive in M5.

use serde::{Deserialize, Serialize};

use crate::config::PlatformKind;
use crate::domain::{ChangeSet, Confidence, Severity};
use crate::platform::{ChangeRef, DiffPaths, DiffRefs, OutgoingComment};
use crate::record::layout;

use super::merge::MergeOutput;
use super::review::CutShort;
use super::triage::TriagePlan;
use super::{StageContext, StageError};

pub const NUMBER: u8 = 5;
pub const NAME: &str = "publish";

/// Everything the report and the posting need, gathered by the caller so the
/// stage takes one argument.
pub struct PublishInput<'a> {
    pub changeset: &'a ChangeSet,
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

/// One comment that made it out, keyed by the marker hidden in its body.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PublishedComment {
    pub trace_id: String,
    pub marker: String,
    pub url: Option<String>,
    /// A 422 forced this comment onto the file instead of a line. The
    /// model's `confidence_score` is unchanged.
    #[serde(default)]
    pub degraded_to_file: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PublishOutput {
    pub published: Vec<PublishedComment>,
    pub skipped_as_duplicate: usize,
    /// False when this run was never asked to post.
    pub posted_to_platform: bool,
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

pub struct Publish;

impl Publish {
    pub fn run(
        context: &mut StageContext<'_>,
        input: &PublishInput<'_>,
    ) -> Result<PublishOutput, StageError> {
        Self::write_artifacts(context, input)?;
        let output = Self::post(context, input, false)?;
        Self::persist_published(context, &output.published)?;
        context.complete(NUMBER, NAME, &output)?;
        Ok(output)
    }

    /// Rewrite `report.md` and `summary.json` from the checkpoints. Does
    /// not talk to a model or a platform.
    pub fn write_artifacts(
        context: &mut StageContext<'_>,
        input: &PublishInput<'_>,
    ) -> Result<(), StageError> {
        let summary = Self::summary(context, input);
        let report = Self::report(input, &summary);
        context
            .recorder
            .write_artifact(layout::REPORT, report.as_bytes())?;
        let summary_bytes = serde_json::to_vec_pretty(&summary).map_err(|source| {
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

    /// Post leftover comments. `force` is the standalone `publish` command:
    /// the user asked, so `meta.publish` being false does not skip the MR.
    pub fn post(
        context: &mut StageContext<'_>,
        input: &PublishInput<'_>,
        force: bool,
    ) -> Result<PublishOutput, StageError> {
        if !force && !context.recorder.meta().publish {
            return Ok(PublishOutput::default());
        }
        let Some(platform) = context.adapters.platform.as_ref() else {
            return Err(StageError::UnreadableInput {
                reason: "--publish needs a platform URL as input".to_string(),
            });
        };
        let change = ChangeRef {
            host: input.changeset.locator.host.clone().unwrap_or_default(),
            project: input.changeset.locator.project.clone().unwrap_or_default(),
            number: input.changeset.locator.number.unwrap_or_default(),
        };
        let refs = DiffRefs {
            head_sha: input.changeset.locator.head_sha.clone().unwrap_or_default(),
            base_sha: input.changeset.locator.base_sha.clone(),
            start_sha: input.changeset.locator.start_sha.clone(),
        };
        let run_id = context.recorder.meta().run_id.clone();
        let summary = Self::summary(context, input);

        // A marker already on the MR, or already listed in published.json,
        // is not said twice. published.json is rewritten after each success
        // so a crash mid-loop does not lose what already went out.
        let existing = platform.existing_comments(&change)?;
        let mut published = Self::load_published(context)?;
        let mut known: std::collections::BTreeSet<String> = existing
            .iter()
            .map(|posted| posted.marker.clone())
            .chain(published.iter().map(|posted| posted.marker.clone()))
            .collect();

        let mut outgoing = Vec::new();
        let mut skipped = 0usize;
        for (index, comment) in input.merged.comments.iter().enumerate() {
            let marker = marker(&run_id, &comment.trace_id);
            if !known.insert(marker.clone()) {
                skipped += 1;
                continue;
            }
            outgoing.push(OutgoingComment {
                paths: paths_for(input.changeset, &comment.target.path),
                line: comment.target.line,
                end_line: comment.target.end_line,
                body: Self::comment_body(comment, input.merged.badge(index), &marker, &run_id),
                marker,
            });
        }

        let summary_mark = summary_marker(&run_id);
        if known.insert(summary_mark.clone()) {
            outgoing.push(OutgoingComment {
                paths: DiffPaths::for_path(""),
                line: None,
                end_line: None,
                body: Self::summary_body(&summary, &summary_mark),
                marker: summary_mark,
            });
        } else {
            skipped += 1;
        }

        match platform.kind() {
            PlatformKind::Gitlab => {
                for comment in &outgoing {
                    Self::dispatch(
                        context,
                        platform.as_ref(),
                        &change,
                        &refs,
                        std::slice::from_ref(comment),
                        &mut published,
                    )?;
                }
            }
            PlatformKind::Github => {
                let (summaries, rest): (Vec<_>, Vec<_>) = outgoing
                    .into_iter()
                    .partition(|comment| comment.is_summary());
                if !rest.is_empty() {
                    Self::dispatch(
                        context,
                        platform.as_ref(),
                        &change,
                        &refs,
                        &rest,
                        &mut published,
                    )?;
                }
                if !summaries.is_empty() {
                    Self::dispatch(
                        context,
                        platform.as_ref(),
                        &change,
                        &refs,
                        &summaries,
                        &mut published,
                    )?;
                }
            }
        }

        Ok(PublishOutput {
            published,
            skipped_as_duplicate: skipped,
            posted_to_platform: true,
        })
    }

    fn dispatch(
        context: &StageContext<'_>,
        platform: &dyn crate::platform::Platform,
        change: &ChangeRef,
        refs: &DiffRefs,
        comments: &[OutgoingComment],
        published: &mut Vec<PublishedComment>,
    ) -> Result<(), StageError> {
        let posted = match platform.post_comments(change, refs, comments) {
            Ok(posted) => posted,
            Err(error) => {
                Self::persist_published(context, published)?;
                return if published.is_empty() {
                    Err(error.into())
                } else {
                    Err(StageError::PublishIncomplete {
                        posted: published.len(),
                        failed: comments.len(),
                    })
                };
            }
        };
        if posted.len() < comments.len() {
            Self::persist_published(context, published)?;
            return Err(StageError::PublishIncomplete {
                posted: published.len() + posted.len(),
                failed: comments.len() - posted.len(),
            });
        }
        for item in posted {
            if item.degraded_to_file {
                Self::note_degraded(context, &item.marker)?;
            }
            published.push(PublishedComment {
                trace_id: trace_id_from(&item.marker),
                marker: item.marker,
                url: item.url,
                degraded_to_file: item.degraded_to_file,
            });
        }
        Self::persist_published(context, published)
    }

    fn load_published(context: &StageContext<'_>) -> Result<Vec<PublishedComment>, StageError> {
        let Some(bytes) = context.recorder.read_artifact(layout::PUBLISHED)? else {
            return Ok(Vec::new());
        };
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_slice(&bytes).unwrap_or_default())
    }

    fn persist_published(
        context: &StageContext<'_>,
        published: &[PublishedComment],
    ) -> Result<(), StageError> {
        let bytes = serde_json::to_vec_pretty(published).map_err(|source| {
            crate::record::RecordError::Serialize {
                file: layout::PUBLISHED.to_string(),
                source,
            }
        })?;
        context.recorder.write_artifact(layout::PUBLISHED, &bytes)?;
        Ok(())
    }

    fn note_degraded(context: &StageContext<'_>, marker: &str) -> Result<(), StageError> {
        let trace_id = trace_id_from(marker);
        if trace_id.is_empty() || trace_id == "summary" {
            return Ok(());
        }
        let Some(mut trace) = context.recorder.read_trace(&trace_id)? else {
            return Ok(());
        };
        let note = "line is not commentable; posted as a file-level comment";
        if !trace.has_note(NAME, note) {
            trace.note(NAME, note);
            context.recorder.write_trace(&trace)?;
        }
        Ok(())
    }

    fn comment_body(
        comment: &crate::domain::Comment,
        badge: Option<&str>,
        marker: &str,
        run_id: &str,
    ) -> String {
        let finding = format!(
            "**[{} {}% / {} {}%]**{} {}\n\nsuggestion:\n{}",
            comment.severity,
            comment.severity_score,
            comment.confidence,
            comment.confidence_score,
            badge_suffix(badge),
            comment.body,
            comment.suggestion
        );
        fit_body(
            &finding,
            &format!("run: `{run_id}`\n{}", trace_line(&comment.trace_id)),
            marker,
            COMMENT_BODY_LIMIT,
        )
    }

    fn summary_body(summary: &Summary, marker: &str) -> String {
        let mut body = String::from("**Reviewbot report**\n\n");
        body.push_str(&basics(summary));
        body.push('\n');
        body.push_str(&overall_block(summary));
        body.push_str(LEGEND);
        fit_body(&body, "", marker, COMMENT_BODY_LIMIT)
    }

    fn summary(context: &StageContext<'_>, input: &PublishInput<'_>) -> Summary {
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
    fn report(input: &PublishInput<'_>, summary: &Summary) -> String {
        let mut report = String::from("# Reviewbot report\n\n");
        report.push_str(&basics(summary));
        report.push('\n');
        // Ahead of the model's paragraph, because it bounds every word of it.
        report.push_str(&coverage_bound(&summary.unavailable));
        report.push_str(&overall_block(summary));
        report.push_str(LEGEND);
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

/// GitHub's review-comment ceiling. GitLab is larger; one limit keeps the
/// finding and the trace id when a body would overflow.
const COMMENT_BODY_LIMIT: usize = 65_536;

const LEGEND: &str = "\
A finding is headed by two of the model's own judgements, published as given: \
how much it matters if it is real, then how sure the model is that it is. They \
are meant to disagree — a severe finding held with low confidence is a reason \
to look, not a reason to block. A badge next to them is reviewbot's check of a \
quotation — a fact it verified, not a judgement it made, and it never moves \
either number. Overall is the model's judgement of what this run found, not a \
code quality score: reviewbot only read the changed lines, one file at a \
time.\n";

fn basics(summary: &Summary) -> String {
    let overall = match summary.overall_score {
        Some(score) => format!("overall: {score} / 100"),
        None => "overall: not scored".to_string(),
    };
    format!(
        "run: `{}`\nmodel: `{}`\n{overall}\n",
        summary.run_id, summary.model
    )
}

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

fn overall_block(summary: &Summary) -> String {
    match summary.overall_score {
        Some(_) => match &summary.summary {
            Some(text) => format!("{text}\n\n"),
            None => String::new(),
        },
        None => {
            let reason = summary
                .unscored_reason
                .as_deref()
                .unwrap_or("no reason was recorded");
            format!("{reason}\n\nNot scored is not a score of 0.\n\n")
        }
    }
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

fn paths_for(changeset: &ChangeSet, path: &str) -> DiffPaths {
    changeset
        .files
        .iter()
        .find(|file| file.new_path == path || file.old_path == path)
        .map(|file| DiffPaths {
            old_path: file.old_path.clone(),
            new_path: file.new_path.clone(),
        })
        .unwrap_or_else(|| DiffPaths::for_path(path))
}

fn fit_body(finding: &str, extra: &str, marker: &str, limit: usize) -> String {
    let extra_block = if extra.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n{extra}")
    };
    let full = format!("{finding}{extra_block}\n{marker}");
    if full.len() <= limit {
        return full;
    }
    let pointer = "\n\nThe rest of this comment is in the run directory traces/.\n";
    let suffix = format!("{extra_block}{pointer}{marker}");
    let budget = limit.saturating_sub(suffix.len());
    format!("{}{suffix}", take_bytes(finding, budget))
}

fn take_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// What goes between the confidence and the finding. Empty when reviewbot
/// checked nothing, so a comment with no quotation reads as it always did.
fn badge_suffix(badge: Option<&str>) -> String {
    badge.map(|badge| format!(" {badge}")).unwrap_or_default()
}

/// Visible lookup key, same wording on the report and on the MR comment.
fn trace_line(trace_id: &str) -> String {
    format!("trace: `{trace_id}`")
}

/// The hidden idempotency marker. Neither platform renders HTML comments.
pub fn marker(run_id: &str, trace_id: &str) -> String {
    format!("<!-- reviewbot:{run_id}:{trace_id} -->")
}

/// The top level summary comment has no `trace_id`, so it takes a fixed one.
pub fn summary_marker(run_id: &str) -> String {
    marker(run_id, "summary")
}

fn trace_id_from(marker: &str) -> String {
    marker
        .trim_start_matches("<!-- reviewbot:")
        .trim_end_matches(" -->")
        .split(':')
        .nth(1)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::domain::{Comment, CommentTarget};
    use crate::platform::ExistingComment;
    use crate::record::{ContextFile, Trace};
    use crate::stage::StageError;
    use crate::stage::fixture::StageFixture;
    use crate::stage::merge::MergeOutput;

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
        trace.note(
            crate::stage::merge::NAME,
            "line 12 is not commentable; moved -1 to 11",
        );
        trace.context_files.push(ContextFile {
            path: "src/parse.h".to_string(),
            first_line: 1,
            last_line: 1,
            body: SECRET_BODY.to_string(),
        });
        trace
    }

    fn publish(fixture: &mut StageFixture, merged: &MergeOutput) {
        let changeset = ChangeSet::default();
        let plan = TriagePlan::default();
        let input = PublishInput {
            changeset: &changeset,
            plan: &plan,
            merged,
            unreviewed: &[],
            cut_short: &[],
            unavailable: &[],
        };
        let mut context = fixture.context();
        Publish::run(&mut context, &input).expect("the report is written");
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
        let changeset = ChangeSet::default();
        let plan = TriagePlan {
            skipped: vec![crate::stage::triage::SkippedFile {
                path: "src/gone.c".to_string(),
                reason: "the whole file was deleted".to_string(),
            }],
            ..TriagePlan::default()
        };
        let input = PublishInput {
            changeset: &changeset,
            plan: &plan,
            merged: &merged,
            unreviewed: &[],
            cut_short: &[],
            unavailable: &[],
        };
        {
            let mut context = fixture.context();
            Publish::run(&mut context, &input).expect("the report is written");
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
        publish(&mut fixture, &scored);
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
        publish(&mut fixture, &unscored);
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
        publish(&mut fixture, &merged);
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
        let changeset = ChangeSet::default();
        let plan = TriagePlan::default();
        let cut_short = [CutShort {
            path: "src/emit.c".to_string(),
            reason: "the tool loop reached its ceiling of 12 rounds".to_string(),
        }];
        let input = PublishInput {
            changeset: &changeset,
            plan: &plan,
            merged: &merged,
            unreviewed: &[],
            cut_short: &cut_short,
            unavailable: &[],
        };
        {
            let mut context = fixture.context();
            Publish::run(&mut context, &input).expect("the report is written");
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
        let changeset = ChangeSet::default();
        let plan = TriagePlan::default();
        let unavailable = ["could not read any file: it saw the diff and nothing else".to_string()];
        let input = PublishInput {
            changeset: &changeset,
            plan: &plan,
            merged: &merged,
            unreviewed: &[],
            cut_short: &[],
            unavailable: &unavailable,
        };
        {
            let mut context = fixture.context();
            Publish::run(&mut context, &input).expect("the report is written");
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
        publish(&mut fixture, &merged);
        assert!(
            !fixture.report().contains("coverage:"),
            "{}",
            fixture.report()
        );
    }

    #[test]
    fn markers_are_stable_and_readable_back() {
        let marker = marker("7f3a9c1e", "review-src_parse.c");
        assert_eq!(marker, "<!-- reviewbot:7f3a9c1e:review-src_parse.c -->");
        assert_eq!(trace_id_from(&marker), "review-src_parse.c");
        assert_eq!(
            summary_marker("7f3a9c1e"),
            "<!-- reviewbot:7f3a9c1e:summary -->"
        );
    }

    #[test]
    fn a_long_finding_is_clipped_and_the_trace_id_stays() {
        let finding = format!("**[certain 92%]** {}", "x".repeat(80_000));
        let extra = "trace: `review-src_parse.c`";
        let marker = "<!-- reviewbot:run:review-src_parse.c -->";
        let body = fit_body(&finding, extra, marker, 400);
        assert!(body.contains("trace: `review-src_parse.c`"), "{body}");
        assert!(body.contains(marker), "{body}");
        assert!(body.contains("run directory traces/"), "{body}");
        assert!(body.len() <= 400, "{}", body.len());
        assert!(!body.contains(&"x".repeat(500)), "the finding was clipped");
    }

    #[test]
    fn an_mr_comment_is_the_finding_and_the_trace_id() {
        let posts = Arc::new(Mutex::new(Vec::new()));
        let mut fixture =
            StageFixture::new(Vec::new()).with_platform(Box::new(RecordingPlatform {
                existing: Vec::new(),
                posts: Arc::clone(&posts),
                fail_after: usize::MAX,
                kind: PlatformKind::Gitlab,
            }));
        fixture.enable_publish();
        fixture
            .recorder()
            .write_trace(&internal_trace("review-src_parse.c"))
            .expect("trace");

        let changeset = url_changeset();
        let plan = TriagePlan::default();
        let merged = MergeOutput {
            comments: vec![comment("review-src_parse.c")],
            overall_score: Some(54),
            summary: Some("one finding".to_string()),
            ..MergeOutput::default()
        };
        let input = PublishInput {
            changeset: &changeset,
            plan: &plan,
            merged: &merged,
            unreviewed: &[],
            cut_short: &[],
            unavailable: &[],
        };
        let mut context = fixture.context();
        Publish::run(&mut context, &input).expect("posted");

        let posted = posts.lock().expect("posts");
        let body = posted
            .iter()
            .find(|body| body.contains("review-src_parse.c"))
            .expect("the inline comment went out");
        assert!(
            body.contains("the index is a constant 5 while buf is char[3]"),
            "{body}"
        );
        assert!(
            body.contains("suggestion:\nbound the index to buf's length"),
            "{body}"
        );
        assert!(body.contains("trace: `review-src_parse.c`"), "{body}");
        assert!(body.contains("run: `test-run`"), "{body}");
        assert!(
            body.contains("<!-- reviewbot:test-run:review-src_parse.c -->"),
            "{body}"
        );
        assert!(!body.contains("<details>"), "{body}");
        assert!(!body.contains("+buf[5] = 0;"), "{body}");
        assert!(!body.contains("instructions"), "{body}");
        assert!(!body.contains(SECRET_BODY), "{body}");
        assert!(!body.contains("spent "), "{body}");

        let summary = posted
            .iter()
            .find(|body| body.contains(":summary -->"))
            .expect("the summary went out");
        assert!(summary.contains("**Reviewbot report**"), "{summary}");
        assert!(summary.contains("run: `test-run`"), "{summary}");
        assert!(summary.contains("overall: 54 / 100"), "{summary}");
        assert!(!summary.contains("<details>"), "{summary}");
        assert!(!summary.contains("## budget"), "{summary}");
        assert!(!summary.contains("spent "), "{summary}");
    }

    #[test]
    fn an_existing_marker_is_not_posted_again() {
        let marker = marker("test-run", "review-src_parse.c");
        let posts = Arc::new(Mutex::new(Vec::new()));
        let mut fixture =
            StageFixture::new(Vec::new()).with_platform(Box::new(RecordingPlatform {
                existing: vec![ExistingComment {
                    marker: marker.clone(),
                    url: None,
                    degraded_to_file: false,
                }],
                posts: Arc::clone(&posts),
                fail_after: usize::MAX,
                kind: PlatformKind::Gitlab,
            }));
        fixture.enable_publish();
        fixture
            .recorder()
            .write_trace(&internal_trace("review-src_parse.c"))
            .expect("trace");

        let changeset = url_changeset();
        let plan = TriagePlan::default();
        let merged = MergeOutput {
            comments: vec![comment("review-src_parse.c")],
            overall_score: Some(54),
            summary: Some("one finding".to_string()),
            ..MergeOutput::default()
        };
        let input = PublishInput {
            changeset: &changeset,
            plan: &plan,
            merged: &merged,
            unreviewed: &[],
            cut_short: &[],
            unavailable: &[],
        };
        let mut context = fixture.context();
        let output = Publish::run(&mut context, &input).expect("publish");

        assert_eq!(output.skipped_as_duplicate, 1);
        let posted = posts.lock().expect("posts");
        assert_eq!(posted.len(), 1, "only the summary is new: {posted:?}");
        assert!(posted[0].contains(":summary -->"), "{posted:?}");
    }

    #[test]
    fn published_json_is_rewritten_after_each_gitlab_success() {
        let posts = Arc::new(Mutex::new(Vec::new()));
        let mut fixture =
            StageFixture::new(Vec::new()).with_platform(Box::new(RecordingPlatform {
                existing: Vec::new(),
                posts: Arc::clone(&posts),
                fail_after: 1,
                kind: PlatformKind::Gitlab,
            }));
        fixture.enable_publish();
        fixture
            .recorder()
            .write_trace(&internal_trace("review-src_parse.c"))
            .expect("trace");
        fixture
            .recorder()
            .write_trace(&internal_trace("review-other"))
            .expect("trace");

        let changeset = url_changeset();
        let plan = TriagePlan::default();
        let merged = MergeOutput {
            comments: vec![
                comment("review-src_parse.c"),
                Comment {
                    target: CommentTarget {
                        path: "src/other.c".to_string(),
                        line: Some(3),
                        end_line: None,
                    },
                    body: "another finding".to_string(),
                    suggestion: "change the other file".to_string(),
                    severity: Severity::Minor,
                    severity_score: 55,
                    confidence: Confidence::High,
                    confidence_score: 80,
                    trace_id: "review-other".to_string(),
                },
            ],
            overall_score: Some(54),
            summary: Some("two findings".to_string()),
            ..MergeOutput::default()
        };
        let input = PublishInput {
            changeset: &changeset,
            plan: &plan,
            merged: &merged,
            unreviewed: &[],
            cut_short: &[],
            unavailable: &[],
        };
        let mut context = fixture.context();
        let error = Publish::run(&mut context, &input).expect_err("second post fails");
        assert!(
            matches!(
                error,
                StageError::PublishIncomplete {
                    posted: 1,
                    failed: 1
                }
            ),
            "{error}"
        );

        let bytes = fixture
            .recorder()
            .read_artifact(layout::PUBLISHED)
            .expect("readable")
            .expect("written after the first success");
        let saved: Vec<PublishedComment> = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].trace_id, "review-src_parse.c");
        assert_eq!(posts.lock().expect("posts").len(), 1);
    }

    fn url_changeset() -> ChangeSet {
        ChangeSet {
            locator: crate::domain::Locator {
                host: Some("gitlab.com".to_string()),
                project: Some("acme/app".to_string()),
                number: Some(128),
                head_sha: Some("head".to_string()),
                base_sha: Some("base".to_string()),
                start_sha: Some("start".to_string()),
            },
            files: Vec::new(),
            narrative: Default::default(),
        }
    }

    struct RecordingPlatform {
        existing: Vec<ExistingComment>,
        posts: Arc<Mutex<Vec<String>>>,
        fail_after: usize,
        kind: PlatformKind,
    }

    impl crate::platform::Platform for RecordingPlatform {
        fn kind(&self) -> PlatformKind {
            self.kind
        }

        fn host(&self) -> &str {
            "gitlab.com"
        }

        fn capabilities(&self) -> crate::platform::Capabilities {
            crate::platform::Capabilities::default()
        }

        fn head_sha(&self, _change: &ChangeRef) -> Result<String, crate::platform::PlatformError> {
            Ok("head".to_string())
        }

        fn fetch_change(
            &self,
            _change: &ChangeRef,
        ) -> Result<crate::platform::PlatformChange, crate::platform::PlatformError> {
            Ok(crate::platform::PlatformChange {
                diff: String::new(),
                head_sha: "head".to_string(),
                base_sha: Some("base".to_string()),
                start_sha: Some("start".to_string()),
                narrative: Default::default(),
            })
        }

        fn existing_comments(
            &self,
            _change: &ChangeRef,
        ) -> Result<Vec<ExistingComment>, crate::platform::PlatformError> {
            Ok(self.existing.clone())
        }

        fn post_comments(
            &self,
            _change: &ChangeRef,
            _refs: &crate::platform::DiffRefs,
            comments: &[OutgoingComment],
        ) -> Result<Vec<ExistingComment>, crate::platform::PlatformError> {
            let mut posts = self.posts.lock().expect("posts");
            if posts.len() >= self.fail_after {
                return Err(crate::platform::PlatformError::Request {
                    operation: "posting a discussion",
                    host: "gitlab.com".to_string(),
                    reason: "forced failure".to_string(),
                });
            }
            let mut posted = Vec::new();
            for comment in comments {
                posts.push(comment.body.clone());
                posted.push(ExistingComment {
                    marker: comment.marker.clone(),
                    url: None,
                    degraded_to_file: false,
                });
            }
            Ok(posted)
        }

        fn bind_repo(&self, _change: &ChangeRef, _head_sha: &str) {}

        fn repo_source(&self) -> std::sync::Arc<dyn crate::platform::RepoSource> {
            std::sync::Arc::new(NullRepo)
        }
    }

    struct NullRepo;

    impl crate::platform::RepoSource for NullRepo {
        fn list_files(
            &self,
            _glob: &str,
        ) -> Result<crate::platform::Listing, crate::platform::PlatformError> {
            Ok(crate::platform::Listing {
                paths: Vec::new(),
                complete: true,
            })
        }

        fn read_file(
            &self,
            _path: &str,
            _lines: Option<crate::platform::LineRange>,
        ) -> Result<String, crate::platform::PlatformError> {
            Ok(String::new())
        }

        fn search(
            &self,
            _query: &str,
            _glob: Option<&str>,
        ) -> Result<Vec<crate::platform::SearchHit>, crate::platform::PlatformError> {
            Ok(Vec::new())
        }
    }
}
