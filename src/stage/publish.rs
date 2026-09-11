//! Stage 6. The MR/PR only hears from us when `--publish` was asked for, and
//! posting is idempotent: comments go out as the finding plus a visible
//! `trace_id`, with the hidden marker for dedup.
//!
//! It runs on every entry, and what decides whether anything goes out is the
//! MR's current state — the markers already on it plus `published.json` — not
//! a checkpoint of its own. A run re-entered to finish posting is the only
//! way left to finish posting, so it may not talk itself out of looking.
//!
//! The numbers in the summary comment are stage five's `Summary`, taken as
//! input rather than counted again.

use serde::{Deserialize, Serialize};

use crate::config::PlatformKind;
use crate::domain::{ChangeSet, Locator, Stage};
use crate::platform::{ChangeRef, DiffPaths, DiffRefs, OutgoingComment};
use crate::record::layout;

use super::merge::MergeOutput;
use super::report::{Summary, badge_suffix, trace_line};
use super::{StageContext, StageError};

/// Everything that goes on the MR, gathered by the caller so the stage takes
/// one argument. The changeset is here for the diff paths a comment has to be
/// anchored to; nothing in it is rendered.
pub struct PublishInput<'a> {
    pub changeset: &'a ChangeSet,
    pub merged: &'a MergeOutput,
    pub summary: &'a Summary,
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

pub struct Publish<'c, 'a> {
    context: &'c mut StageContext<'a>,
}

impl<'c, 'a> Publish<'c, 'a> {
    pub fn new(context: &'c mut StageContext<'a>) -> Self {
        Self { context }
    }

    pub fn run(&mut self, input: &PublishInput<'_>) -> Result<PublishOutput, StageError> {
        let output = Self::post(self.context, input)?;
        Self::persist_published(self.context, &output.published)?;
        self.context.complete(Stage::Publish, &output)?;
        Ok(output)
    }

    /// Post whatever the MR has not heard yet. A run that was never asked to
    /// publish returns here, before the platform is touched at all — which is
    /// what keeps re-entering a run to re-render its report an offline act.
    fn post(
        context: &StageContext<'_>,
        input: &PublishInput<'_>,
    ) -> Result<PublishOutput, StageError> {
        if !context.recorder.meta().publish {
            return Ok(PublishOutput::default());
        }
        let Some(platform) = context.adapters.platform() else {
            return Err(StageError::UnreadableInput {
                reason: "--publish needs a platform URL as input".to_string(),
            });
        };
        let (change, refs) = Self::publish_target(&input.changeset.locator)?;
        let run_id = context.recorder.meta().run_id.clone();

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
                line: comment.target.start_line,
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
                body: Self::summary_body(input.summary, &summary_mark),
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
                        platform,
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
                    Self::dispatch(context, platform, &change, &refs, &rest, &mut published)?;
                }
                if !summaries.is_empty() {
                    Self::dispatch(
                        context,
                        platform,
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

    fn publish_target(locator: &Locator) -> Result<(ChangeRef, DiffRefs), StageError> {
        let missing = |field: &str| StageError::UnreadableInput {
            reason: format!("--publish needs the change {field}"),
        };
        Ok((
            ChangeRef {
                host: locator.host.clone().ok_or_else(|| missing("host"))?,
                project: locator.project.clone().ok_or_else(|| missing("project"))?,
                number: locator.number.ok_or_else(|| missing("number"))?,
            },
            DiffRefs {
                head_sha: locator
                    .head_sha
                    .clone()
                    .ok_or_else(|| missing("head_sha"))?,
                base_sha: locator.base_sha.clone(),
                start_sha: locator.start_sha.clone(),
            },
        ))
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
        serde_json::from_slice(&bytes).map_err(|source| {
            crate::record::RecordError::Serialize {
                file: layout::PUBLISHED.to_string(),
                source,
            }
            .into()
        })
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
        if !trace.has_note(Stage::Publish, note) {
            trace.note(Stage::Publish, note);
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
        body.push_str(&summary.basics());
        body.push('\n');
        body.push_str(&summary.verdict());
        fit_body(&body, "", marker, COMMENT_BODY_LIMIT)
    }
}

/// GitHub's review-comment ceiling. GitLab is larger; one limit keeps the
/// finding and the trace id when a body would overflow.
const COMMENT_BODY_LIMIT: usize = 65_536;

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

    use crate::domain::{Comment, CommentTarget, Confidence, Severity};
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
                start_line: Some(11),
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
        trace.set_diff("@@ -10,2 +10,3 @@\n+buf[5] = 0;\n");
        trace.set_prompt(format!(
            "instructions\n\ncontext of src/parse.h:\n{SECRET_BODY}\n"
        ));
        trace.set_model_output(r#"{"comments":[{"path":"src/parse.c"}]}"#);
        trace.add_context_file(ContextFile {
            path: "src/parse.h".to_string(),
            first_line: 1,
            last_line: 1,
            body: SECRET_BODY.to_string(),
        });
        trace
    }

    /// What stage five hands over. Written out rather than rendered, because
    /// what is being tested here is that these numbers reach the MR unchanged.
    fn summary() -> Summary {
        Summary {
            run_id: "test-run".to_string(),
            model: "deepseek-v4-flash".to_string(),
            overall_score: Some(54),
            summary: Some("one finding".to_string()),
            unscored_reason: None,
            comments: Vec::new(),
            by_severity: Vec::new(),
            skipped: Vec::new(),
            unreviewed: Vec::new(),
            stopped: None,
            unproduced: Vec::new(),
            unavailable: Vec::new(),
            spent: 0.0,
            budget: Some(10.0),
            currency: "CNY".to_string(),
        }
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
        let merged = MergeOutput {
            comments: vec![comment("review-src_parse.c")],
            overall_score: Some(54),
            summary: Some("one finding".to_string()),
            ..MergeOutput::default()
        };
        let summary = summary();
        let input = PublishInput {
            changeset: &changeset,
            merged: &merged,
            summary: &summary,
        };
        let mut context = fixture.context();
        Publish::new(&mut context).run(&input).expect("posted");

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

        let summary_comment = posted
            .iter()
            .find(|body| body.contains(":summary -->"))
            .expect("the summary went out");
        assert!(
            summary_comment.contains("**Reviewbot report**"),
            "{summary_comment}"
        );
        assert!(
            summary_comment.contains("run: `test-run`"),
            "{summary_comment}"
        );
        assert!(
            summary_comment.contains("overall: 54 / 100"),
            "{summary_comment}"
        );
        assert!(!summary_comment.contains("<details>"), "{summary_comment}");
        assert!(!summary_comment.contains("## budget"), "{summary_comment}");
        assert!(!summary_comment.contains("spent "), "{summary_comment}");
    }

    #[test]
    fn publish_fails_when_the_locator_is_missing() {
        let mut fixture =
            StageFixture::new(Vec::new()).with_platform(Box::new(RecordingPlatform {
                existing: Vec::new(),
                posts: Arc::new(Mutex::new(Vec::new())),
                fail_after: usize::MAX,
                kind: PlatformKind::Gitlab,
            }));
        fixture.enable_publish();
        let changeset = ChangeSet::default();
        let merged = MergeOutput::default();
        let summary = summary();
        let input = PublishInput {
            changeset: &changeset,
            merged: &merged,
            summary: &summary,
        };
        let mut context = fixture.context();
        let error = Publish::new(&mut context)
            .run(&input)
            .expect_err("a missing locator cannot be invented");
        assert!(
            matches!(error, StageError::UnreadableInput { .. }),
            "{error}"
        );
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
        let merged = MergeOutput {
            comments: vec![comment("review-src_parse.c")],
            overall_score: Some(54),
            summary: Some("one finding".to_string()),
            ..MergeOutput::default()
        };
        let summary = summary();
        let input = PublishInput {
            changeset: &changeset,
            merged: &merged,
            summary: &summary,
        };
        let mut context = fixture.context();
        let output = Publish::new(&mut context).run(&input).expect("publish");

        assert_eq!(output.skipped_as_duplicate, 1);
        let posted = posts.lock().expect("posts");
        assert_eq!(posted.len(), 1, "only the summary is new: {posted:?}");
        assert!(posted[0].contains(":summary -->"), "{posted:?}");
    }

    /// The stage runs on every entry now, including on a run that was never
    /// asked to post. Re-entering such a run has to stay an offline act — it
    /// is how the report is re-rendered — so the platform is not asked
    /// anything at all, not even what is already on the MR.
    #[test]
    fn a_run_that_was_not_asked_to_publish_never_reaches_the_platform() {
        let mut fixture = StageFixture::new(Vec::new()).with_platform(Box::new(RefusingPlatform));
        let changeset = url_changeset();
        let merged = MergeOutput {
            comments: vec![comment("review-src_parse.c")],
            overall_score: Some(54),
            ..MergeOutput::default()
        };
        let summary = summary();
        let input = PublishInput {
            changeset: &changeset,
            merged: &merged,
            summary: &summary,
        };
        let output = {
            let mut context = fixture.context();
            Publish::new(&mut context)
                .run(&input)
                .expect("the stage still finishes")
        };

        assert!(!output.posted_to_platform);
        assert!(output.published.is_empty());
        assert_eq!(
            fixture
                .recorder()
                .read_artifact(layout::PUBLISHED)
                .expect("readable")
                .expect("written even so"),
            b"[]",
            "an empty published.json says the run posted nothing, not that it never ran"
        );
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
        let merged = MergeOutput {
            comments: vec![
                comment("review-src_parse.c"),
                Comment {
                    target: CommentTarget {
                        path: "src/other.c".to_string(),
                        start_line: Some(3),
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
        let summary = summary();
        let input = PublishInput {
            changeset: &changeset,
            merged: &merged,
            summary: &summary,
        };
        let mut context = fixture.context();
        let error = Publish::new(&mut context)
            .run(&input)
            .expect_err("second post fails");
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

        fn repo(&self) -> crate::platform::Repo {
            crate::platform::Repo::new(
                std::sync::Arc::new(NullRepo),
                crate::platform::Capabilities::default(),
            )
        }

        fn cached_body(&self, _path: &str) -> Option<String> {
            None
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

        fn bind_repo(
            &self,
            _change: &ChangeRef,
            _head_sha: &str,
        ) -> Result<(), crate::platform::PlatformError> {
            Ok(())
        }
    }

    /// A platform every call to which is a test failure. The only assertion
    /// it makes is that it is never asked anything.
    struct RefusingPlatform;

    impl crate::platform::Platform for RefusingPlatform {
        fn kind(&self) -> PlatformKind {
            PlatformKind::Gitlab
        }

        fn host(&self) -> &str {
            "gitlab.com"
        }

        fn repo(&self) -> crate::platform::Repo {
            crate::platform::Repo::new(
                std::sync::Arc::new(NullRepo),
                crate::platform::Capabilities::default(),
            )
        }

        fn cached_body(&self, _path: &str) -> Option<String> {
            None
        }

        fn head_sha(&self, _change: &ChangeRef) -> Result<String, crate::platform::PlatformError> {
            panic!("the platform was reached")
        }

        fn fetch_change(
            &self,
            _change: &ChangeRef,
        ) -> Result<crate::platform::PlatformChange, crate::platform::PlatformError> {
            panic!("the platform was reached")
        }

        fn existing_comments(
            &self,
            _change: &ChangeRef,
        ) -> Result<Vec<ExistingComment>, crate::platform::PlatformError> {
            panic!("the platform was reached")
        }

        fn post_comments(
            &self,
            _change: &ChangeRef,
            _refs: &crate::platform::DiffRefs,
            _comments: &[OutgoingComment],
        ) -> Result<Vec<ExistingComment>, crate::platform::PlatformError> {
            panic!("the platform was reached")
        }

        fn bind_repo(
            &self,
            _change: &ChangeRef,
            _head_sha: &str,
        ) -> Result<(), crate::platform::PlatformError> {
            Ok(())
        }
    }

    struct NullRepo;

    impl crate::platform::RepoSource for NullRepo {
        fn list_files(
            &self,
            _glob: &str,
        ) -> Result<crate::platform::Listing, crate::platform::PlatformError> {
            Ok(crate::platform::Listing {
                files: Vec::new(),
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

        fn size(&self, _path: &str) -> Result<u64, crate::platform::PlatformError> {
            Ok(0)
        }

        fn search(
            &self,
            _kind: crate::platform::SearchKind,
            _query: &str,
            _glob: Option<&str>,
        ) -> Result<Vec<crate::platform::SearchHit>, crate::platform::PlatformError> {
            Ok(Vec::new())
        }
    }
}
