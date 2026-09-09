//! Stage 4. Collect the per chunk answers into one ordered, publishable
//! list, then ask the model once for an overall score.
//!
//! Seven steps in order: parse, drop what is out of range, align the line
//! numbers, check the quotations, deduplicate, sort and count, score. Only
//! the seventh talks to the model: the comments themselves arrived through
//! `submit_comment`, and `review` serialised the document this stage reads.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::domain::{ChangeSet, Comment, CommentTarget, Confidence, FileChange, Severity};
use crate::protocol::{InputItem, Request, Role, ToolSchema};
use crate::record::{ToolCall, Trace};
use crate::tool::{Round, SubmitSummary, whole_score};

use super::prompt::Prompts;

use super::review::{ChunkOutput, ReviewOutput};
use super::{StageContext, StageError};

pub const NUMBER: u8 = 4;
pub const NAME: &str = "merge";

/// The two badges reviewbot may put on a comment. They say what reviewbot
/// checked, never what it thinks: the number beside them is the model's.
pub const TOOL_FOUND: &str = "found by tool";
pub const QUOTE_UNVERIFIED: &str = "quote unverified";

/// How far a line may be moved to reach a commentable one. Hanging a comment
/// on the context line next to the change is a legal anchor; moving it
/// further than this would point at unrelated code. The quotation check
/// reuses it because alignment has just moved the line by up to that much.
const ALIGN_WINDOW: u32 = 3;

/// The scoring call belongs to no single comment, so it gets a trace of its
/// own rather than sharing a comment's.
const SUMMARY_TRACE_ID: &str = "merge-summary";

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct MergeOutput {
    pub comments: Vec<Comment>,
    /// reviewbot's own check of each comment's quotation, in the same order
    /// as `comments`. Not a `Comment` field: `domain` carries no logic, and
    /// a badge is a fact reviewbot verified rather than something the model
    /// said. `None` means the comment quoted no tool at all.
    #[serde(default)]
    pub badges: Vec<Option<String>>,
    /// What the model gave, stored as given. Never filled with 0.
    pub overall_score: Option<u8>,
    pub summary: Option<String>,
    /// Why there is no score, when there is none. `null` and 0 are different
    /// things and the report has to show them differently.
    pub unscored_reason: Option<String>,
    /// Chunks that gave nothing this stage could use. The money already
    /// spent on the other chunks is not thrown away with them.
    #[serde(default)]
    pub unproduced: Vec<Unproduced>,
}

/// A chunk that produced nothing, and why. Named in the report next to the
/// skipped and the unreviewed files.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Unproduced {
    pub path: String,
    pub reason: String,
}

impl MergeOutput {
    /// The badge for the comment at `index`, if it earned one.
    pub fn badge(&self, index: usize) -> Option<&str> {
        self.badges.get(index).and_then(|badge| badge.as_deref())
    }

    /// Counts per confidence band, for `summary.json` and the stdout summary.
    pub fn counts(&self) -> Vec<(Confidence, usize)> {
        Confidence::ALL
            .iter()
            .map(|band| {
                let count = self
                    .comments
                    .iter()
                    .filter(|comment| comment.confidence == *band)
                    .count();
                (*band, count)
            })
            .collect()
    }

    /// The same over the other axis. Two breakdowns rather than a grid: a
    /// reader wants "how bad is the worst of it" and "how sure is any of it",
    /// and a four-by-four table answers neither at a glance.
    pub fn severity_counts(&self) -> Vec<(Severity, usize)> {
        Severity::ALL
            .iter()
            .map(|band| {
                let count = self
                    .comments
                    .iter()
                    .filter(|comment| comment.severity == *band)
                    .count();
                (*band, count)
            })
            .collect()
    }
}

/// The document the prompt asks for. `comments` stays untyped on purpose: one
/// unreadable entry then costs one entry, not the whole reply.
#[derive(Deserialize)]
struct RawDocument {
    comments: Vec<serde_json::Value>,
}

/// One entry as the model wrote it. Every field is optional here so the
/// checks below can name what is missing instead of failing the document.
#[derive(Default, Deserialize)]
#[serde(default)]
struct RawComment {
    path: Option<String>,
    line: Option<u32>,
    end_line: Option<u32>,
    body: Option<String>,
    suggestion: Option<String>,
    /// Left as JSON: `82.5` and `"82"` have to be told apart from `82`.
    severity_score: Option<serde_json::Value>,
    /// Left as JSON: `82.5` and `"82"` have to be told apart from `82`.
    confidence_score: Option<serde_json::Value>,
    evidence: Option<RawEvidence>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawEvidence {
    diff_lines: Option<Vec<u32>>,
    external_files: Option<Vec<String>>,
    tool_quote: Option<RawQuote>,
}

/// The quotation the model claims came out of a tool. `note` is deliberately
/// absent: it is the model's own diagnosis, shown next to the quotation but
/// never part of what is checked, so this step must not be able to read it.
#[derive(Default, Deserialize)]
#[serde(default)]
struct RawQuote {
    tool: Option<String>,
    text: Option<String>,
}

/// Everything the deterministic steps need about the file one chunk was
/// about: its two line sets, the tool output the model was shown, and the
/// files this run really fetched. Gathered so those steps take one argument.
struct ChunkFile<'a> {
    path: &'a str,
    trace_id: &'a str,
    commentable_lines: &'a BTreeSet<u32>,
    changed_lines: &'a BTreeSet<u32>,
    /// As recorded, which is exactly what went into `function_call_output`.
    /// Checking against a rawer capture would fail on anything the model was
    /// never shown.
    tool_calls: &'a [ToolCall],
    fetched: &'a [String],
    /// The worktree root, so an absolute path in a quotation can be brought
    /// back to the repository relative form before the comparison.
    root: Option<&'a Path>,
}

/// A publishable comment plus what reviewbot verified about it. The badge and
/// the evidence live here rather than on `Comment` because `domain` carries
/// no logic and neither of them is something the model said.
#[derive(Clone)]
struct Finding {
    comment: Comment,
    badge: Option<String>,
    external_files: Vec<String>,
}

/// A comment that survived, with the notes its survival produced.
struct Produced {
    finding: Finding,
    notes: Vec<String>,
}

/// Where a comment ended up hanging, and what that took.
struct Alignment {
    line: Option<u32>,
    note: Option<String>,
}

/// Step 7's outcome. Three fields rather than one enum because all three go
/// into the checkpoint and two of them are `null` more often than not.
#[derive(Default)]
struct Score {
    overall_score: Option<u8>,
    summary: Option<String>,
    unscored_reason: Option<String>,
}

impl Score {
    /// No number, and the reason said out loud. Nothing on this path may
    /// fill in 0: that is a real score the model can give.
    fn unscored(reason: impl Into<String>) -> Self {
        Self {
            overall_score: None,
            summary: None,
            unscored_reason: Some(reason.into()),
        }
    }
}

/// A chunk that failed on its own. Kept apart from `StageError` so one bad
/// reply does not end a run the other chunks already paid for.
enum ChunkError {
    Unproduced(String),
    Stage(StageError),
}

impl From<StageError> for ChunkError {
    fn from(error: StageError) -> Self {
        ChunkError::Stage(error)
    }
}

impl From<crate::record::RecordError> for ChunkError {
    fn from(error: crate::record::RecordError) -> Self {
        ChunkError::Stage(error.into())
    }
}

impl From<crate::config::ConfigError> for ChunkError {
    fn from(error: crate::config::ConfigError) -> Self {
        ChunkError::Stage(error.into())
    }
}

impl From<crate::protocol::ProtocolError> for ChunkError {
    fn from(error: crate::protocol::ProtocolError) -> Self {
        ChunkError::Stage(error.into())
    }
}

pub struct Merge;

impl Merge {
    pub fn run(
        context: &mut StageContext<'_>,
        changeset: &ChangeSet,
        review: &ReviewOutput,
    ) -> Result<MergeOutput, StageError> {
        let mut findings = Vec::new();
        let mut unproduced = Vec::new();
        for chunk in &review.chunks {
            match Self::run_chunk(context, changeset, chunk) {
                Ok(found) => findings.extend(found),
                Err(ChunkError::Stage(error)) => return Err(error),
                Err(ChunkError::Unproduced(reason)) => {
                    tracing::warn!(path = %chunk.path, %reason, "chunk produced no comments");
                    unproduced.push(Unproduced {
                        path: chunk.path.clone(),
                        reason,
                    });
                }
            }
        }

        // Step 5, across chunks, because the same problem is only reported
        // twice when one file was cut into two of them.
        let merged = Self::deduplicate(&mut findings);
        for (trace_id, note) in merged {
            Self::note_on_trace(context, &trace_id, note)?;
        }

        Self::sort(&mut findings);
        let comments: Vec<Comment> = findings
            .iter()
            .map(|finding| finding.comment.clone())
            .collect();
        let badges: Vec<Option<String>> =
            findings.into_iter().map(|finding| finding.badge).collect();
        // An empty list after a finished review is a real outcome ("nothing
        // found") and still gets a score. Skip only when the run did not
        // actually finish looking: stopped, no chunks, or a chunk that never
        // produced a readable answer and left nothing else to judge.
        let unfinished = review.stopped.is_some()
            || review.chunks.is_empty()
            || (comments.is_empty() && !unproduced.is_empty());
        let score = match unfinished {
            true => Score::unscored(nothing_to_score(review, &unproduced)),
            false => Self::score(context, &comments)?,
        };
        let output = MergeOutput {
            comments,
            badges,
            overall_score: score.overall_score,
            summary: score.summary,
            unscored_reason: score.unscored_reason,
            unproduced,
        };
        tracing::info!(
            comments = output.comments.len(),
            unproduced = output.unproduced.len(),
            "merge done"
        );
        context.complete(NUMBER, NAME, &output)?;
        Ok(output)
    }

    /// One chunk, steps 1 to 4. The trace it wrote in `review` is where the
    /// notes go.
    fn run_chunk(
        context: &mut StageContext<'_>,
        changeset: &ChangeSet,
        chunk: &ChunkOutput,
    ) -> Result<Vec<Finding>, ChunkError> {
        let Some(file) = file_of(changeset, &chunk.path) else {
            return Err(ChunkError::Unproduced(format!(
                "the change set no longer has a file at {}",
                chunk.path
            )));
        };
        let mut trace = context
            .recorder
            .read_trace(&chunk.trace_id)?
            .unwrap_or_else(|| Trace::new(chunk.trace_id.clone()));
        // Merge owns the post-processing half of a trace, so running the stage
        // twice rewrites its own notes. Only its own: the account of the
        // conversation belongs to `review`, and clearing the lot used to erase
        // it, so a comment whose evidence was never really looked for read
        // exactly like one that was.
        trace.forget(NAME);

        let entries = match parse_document(&chunk.raw_output) {
            Ok(entries) => entries,
            Err(reason) => {
                // Nothing to ask the model about: this document is the one
                // `review` serialised from the tool calls it accepted, so an
                // unreadable one is a damaged checkpoint rather than a badly
                // behaved reply.
                trace.note(
                    NAME,
                    format!("the chunk document would not parse ({reason})"),
                );
                context.recorder.write_trace(&trace)?;
                return Err(ChunkError::Unproduced(format!(
                    "the submitted comments would not parse: {reason}"
                )));
            }
        };

        // Cloned out of the trace because the notes go back into the same
        // trace while these are being read.
        let tool_calls = trace.tool_calls.clone();
        let fetched: Vec<String> = trace
            .context_files
            .iter()
            .map(|file| file.path.clone())
            .collect();
        let mut notes: Vec<String> = Vec::new();
        let findings = Self::findings_from(
            &entries,
            &ChunkFile {
                path: &chunk.path,
                trace_id: &chunk.trace_id,
                commentable_lines: &file.commentable_lines,
                changed_lines: &file.changed_lines,
                tool_calls: &tool_calls,
                fetched: &fetched,
                root: context.settings.options.worktree.as_deref(),
            },
            &mut notes,
        );
        for note in notes {
            trace.note(NAME, note);
        }
        context.recorder.write_trace(&trace)?;
        Ok(findings)
    }

    /// One more line on a trace another step already wrote. Used by dedup,
    /// where the note belongs to the survivor and the survivor may have come
    /// out of a different chunk.
    fn note_on_trace(
        context: &StageContext<'_>,
        trace_id: &str,
        note: String,
    ) -> Result<(), StageError> {
        let Some(mut trace) = context.recorder.read_trace(trace_id)? else {
            return Ok(());
        };
        trace.note(NAME, note);
        context.recorder.write_trace(&trace)?;
        Ok(())
    }

    /// Steps 1 to 4 for the entries of one document, with no model and no
    /// disk in the way. Every drop is recorded; none of them is a downgrade.
    fn findings_from(
        entries: &[serde_json::Value],
        file: &ChunkFile<'_>,
        checks: &mut Vec<String>,
    ) -> Vec<Finding> {
        let mut findings = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            match Self::finding_from(entry, file) {
                Ok(produced) => {
                    for note in produced.notes {
                        checks.push(format!("comment {index}: {note}"));
                    }
                    findings.push(produced.finding);
                }
                Err(reason) => checks.push(format!("comment {index} dropped: {reason}")),
            }
        }
        findings
    }

    /// One entry, or the reason it cannot be published.
    fn finding_from(entry: &serde_json::Value, file: &ChunkFile<'_>) -> Result<Produced, String> {
        let raw: RawComment = serde_json::from_value(entry.clone())
            .map_err(|error| format!("the entry will not read as a comment: {error}"))?;

        // Step 2, out of the file. The model only ever saw this one file, so
        // an assertion about another has nothing behind it; moving it to the
        // right chunk would be inventing the context it never had.
        match raw.path.as_deref() {
            Some(path) if path == file.path => {}
            Some(path) => return Err(format!("path {path} is not this chunk's file")),
            None => return Err("no path".to_string()),
        }

        let severity_score = score_of("severity_score", raw.severity_score.as_ref())?;
        let score = score_of("confidence_score", raw.confidence_score.as_ref())?;
        let body = match raw.body {
            Some(body) if !body.trim().is_empty() => body,
            _ => return Err("no body".to_string()),
        };
        let suggestion = match raw.suggestion {
            Some(suggestion) if !suggestion.trim().is_empty() => suggestion,
            _ => return Err("no suggestion".to_string()),
        };

        let evidence = raw.evidence.unwrap_or_default();
        let diff_lines = evidence.diff_lines.unwrap_or_default();
        // Step 2, out of the change. The changed set, not the commentable
        // one: context lines are code nobody touched, and a comment that
        // leans only on those is about the code as it already was.
        if !diff_lines
            .iter()
            .any(|line| file.changed_lines.contains(line))
        {
            return Err(match diff_lines.is_empty() {
                true => "no evidence.diff_lines, so nothing ties it to this change".to_string(),
                false => format!("evidence.diff_lines {diff_lines:?} touch none of this change"),
            });
        }

        // Step 3, alignment. It moves the line and nothing else: where a
        // comment hangs and whether it is right are different questions.
        let alignment = Self::align(file.commentable_lines, raw.line, &diff_lines);
        let end_line = end_line_of(&alignment, raw.end_line, file.commentable_lines);
        let mut notes = Vec::new();
        notes.extend(alignment.note);

        // Step 4, the quotation check. It answers "did that tool really say
        // this, about this line", never "is the finding right", so all it
        // can produce is a badge. The score is not touched on either branch.
        let badge =
            evidence
                .tool_quote
                .map(|quote| match check_quote(&quote, file, alignment.line) {
                    Ok(tool) => {
                        notes.push(format!(
                        "{TOOL_FOUND}: the quotation is verbatim in {tool}'s output and points here"
                    ));
                        TOOL_FOUND.to_string()
                    }
                    Err(reason) => {
                        notes.push(format!("{QUOTE_UNVERIFIED}: {reason}"));
                        QUOTE_UNVERIFIED.to_string()
                    }
                });

        let external_files = evidence.external_files.unwrap_or_default();
        let invented: Vec<&String> = external_files
            .iter()
            .filter(|named| !file.fetched.iter().any(|path| path == *named))
            .collect();
        if !invented.is_empty() {
            notes.push(format!(
                "evidence.external_files names {}, which this run never fetched",
                invented
                    .iter()
                    .map(|path| path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        Ok(Produced {
            finding: Finding {
                comment: Comment {
                    target: CommentTarget {
                        path: file.path.to_string(),
                        line: alignment.line,
                        end_line,
                    },
                    body,
                    suggestion,
                    severity: severity_band(severity_score),
                    severity_score,
                    confidence: band(score),
                    confidence_score: score,
                    trace_id: file.trace_id.to_string(),
                },
                badge,
                external_files,
            },
            notes,
        })
    }

    /// Step 3. The commentable set is the one that decides where a comment
    /// may hang; whether it may exist at all was step 2's question.
    fn align(commentable: &BTreeSet<u32>, line: Option<u32>, diff_lines: &[u32]) -> Alignment {
        if let Some(line) = line {
            if commentable.contains(&line) {
                return Alignment {
                    line: Some(line),
                    note: None,
                };
            }
            if let Some(near) = nearest_commentable(commentable, line) {
                return Alignment {
                    line: Some(near),
                    note: Some(format!(
                        "line {line} is not commentable; moved {} to {near}",
                        offset(line, near)
                    )),
                };
            }
        }
        // What the model said it read is usually closer to the truth than the
        // line it wrote down: the first it copied, the second it counted.
        if let Some(fallback) = diff_lines
            .iter()
            .copied()
            .find(|line| commentable.contains(line))
        {
            return Alignment {
                line: Some(fallback),
                note: Some(match line {
                    Some(line) => format!(
                        "line {line} is more than {ALIGN_WINDOW} lines from any commentable line; \
                         used evidence.diff_lines {fallback} instead"
                    ),
                    None => format!("no line was given; used evidence.diff_lines {fallback}"),
                }),
            };
        }
        Alignment {
            line: None,
            note: Some(
                "no commentable line was found, so this became a file level comment".to_string(),
            ),
        }
    }

    /// Step 5. Two comments are the same finding only when they are about the
    /// same file, their ranges touch, and their bodies are the same text once
    /// line numbers and spacing are set aside. "The same kind of problem" is
    /// a judgement, and getting it wrong deletes a real finding, so it is not
    /// one this step is allowed to make.
    ///
    /// Returns the note each survivor's trace owes, since a duplicate that
    /// was dropped has to stay findable.
    fn deduplicate(findings: &mut Vec<Finding>) -> Vec<(String, String)> {
        let mut kept: Vec<Finding> = Vec::with_capacity(findings.len());
        let mut notes = Vec::new();
        for candidate in std::mem::take(findings) {
            let Some(at) = kept
                .iter()
                .position(|existing| same_finding(existing, &candidate))
            else {
                kept.push(candidate);
                continue;
            };
            let existing = kept.remove(at);
            // The worse reading of the same defect wins the comment itself,
            // certainty breaking the tie; everything the loser pointed at is
            // kept, because that is what the reader would have had to read
            // both entries for.
            let (survivor, dropped) = match weight(&candidate) > weight(&existing) {
                true => (candidate, existing),
                false => (existing, candidate),
            };
            notes.push((
                survivor.comment.trace_id.clone(),
                format!(
                    "merged a duplicate of this comment from {} line {} \
                     (severity {}, confidence {}, trace {})",
                    dropped.comment.target.path,
                    sort_line(&dropped.comment),
                    dropped.comment.severity_score,
                    dropped.comment.confidence_score,
                    dropped.comment.trace_id,
                ),
            ));
            kept.insert(at, merge_findings(survivor, dropped));
        }
        *findings = kept;
        notes
    }

    /// Step 6, the order. Worst first, then most certain: on a busy merge
    /// request the top of the list is the only part that gets read, and what
    /// belongs there is the thing that would do the most damage. Sorting by
    /// certainty alone put a sure naming quibble above an uncertain memory
    /// error, which is the wrong way round for the person deciding whether to
    /// merge.
    fn sort(findings: &mut [Finding]) {
        findings.sort_by(|left, right| {
            right
                .comment
                .severity_score
                .cmp(&left.comment.severity_score)
                .then_with(|| {
                    right
                        .comment
                        .confidence_score
                        .cmp(&left.comment.confidence_score)
                })
                .then_with(|| left.comment.target.path.cmp(&right.comment.target.path))
                .then_with(|| sort_line(&left.comment).cmp(&sort_line(&right.comment)))
        });
    }

    /// Step 7. One more call, and only now that the list is final: the score
    /// judges what is about to be published, not the diff. An empty list is
    /// a finished review that found nothing, and still belongs here.
    fn score(context: &mut StageContext<'_>, comments: &[Comment]) -> Result<Score, StageError> {
        let selection = context.settings.selection()?;
        let instructions = context.redactor.redact(&Prompts::SUMMARY.text()?);
        let findings = context.redactor.redact(&findings_json(comments));
        let request = Request {
            model: selection.model.name.clone(),
            instructions,
            input: vec![InputItem::Message {
                role: Role::User,
                content: findings,
            }],
            // From the registry, on the scoring round: this stage does not
            // write out a schema of its own, and the round is what keeps
            // `submit_comment` and the content tools off this turn.
            tools: scoring_tools(context),
            max_output_tokens: selection.model.max_output_tokens,
            reasoning_effort: selection.model.reasoning_effort.clone(),
        };
        let estimate = context.budget.estimate(
            request.estimated_input_tokens(),
            selection.model.max_output_tokens,
        );

        // Skipping the score is not the same as failing the run: the comments
        // are already final and they still go out. The exit code is the
        // review stage's business, and it does not change here.
        if let Err(error) = context.budget.check(estimate) {
            tracing::warn!("no overall score: {error}");
            return Ok(Score::unscored(format!("not scored: {error}")));
        }

        let mut trace = Trace::new(SUMMARY_TRACE_ID);
        trace.prompt = request.instructions.clone();
        let outcome = Self::ask_for_score(context, &request, &mut trace);
        context.recorder.write_trace(&trace)?;
        outcome
    }

    /// The call itself, plus the one re-ask a wrong shape earns. Two bad
    /// replies leave the score `None` with the reason written down.
    fn ask_for_score(
        context: &mut StageContext<'_>,
        request: &Request,
        trace: &mut Trace,
    ) -> Result<Score, StageError> {
        let rejected = match Self::score_once(context, request, trace) {
            Ok(score) => return Ok(score),
            Err(rejected) => rejected,
        };
        let mut reason = rejected.reason;
        let note = Prompts::RESCORE
            .fill()
            .set("reason", reason.clone())
            .render()?;
        let mut retry = request.clone();
        // A call the model made and reviewbot refused has to go back out with
        // its refusal: the protocol is stateless, so an unanswered call left
        // in the history is one the vendor cannot match up.
        if let Some(answer) = rejected.answer {
            retry.input.push(InputItem::FunctionCall {
                call_id: answer.call_id.clone(),
                name: SubmitSummary::NAME.to_string(),
                arguments: answer.arguments,
            });
            retry.input.push(InputItem::FunctionCallOutput {
                call_id: answer.call_id,
                output: context.redactor.redact(&answer.refusal),
            });
        }
        retry.input.push(InputItem::Message {
            role: Role::User,
            content: context.redactor.redact(&note),
        });
        trace.note(
            NAME,
            format!("the scoring call did not arrive ({reason}); asked once more"),
        );

        let estimate = context
            .budget
            .estimate(retry.estimated_input_tokens(), retry.max_output_tokens);
        if let Err(error) = context.budget.check(estimate) {
            return Ok(Score::unscored(format!("not scored: {error}")));
        }
        Ok(match Self::score_once(context, &retry, trace) {
            Ok(score) => score,
            Err(second) => {
                let second = second.reason;
                reason = format!("{reason}, then {second}");
                trace.note(
                    NAME,
                    format!("the second score reply would not read ({second})"),
                );
                tracing::warn!("no overall score: {reason}");
                Score::unscored(format!(
                    "not scored: the model's reply was unreadable ({reason})"
                ))
            }
        })
    }

    /// One request. A network failure that survived the protocol's own retry
    /// leaves the score missing rather than throwing away a finished list.
    fn score_once(
        context: &mut StageContext<'_>,
        request: &Request,
        trace: &mut Trace,
    ) -> Result<Score, Rejected> {
        let _span = tracing::info_span!("merge", step = "score").entered();
        let response = match context.send_and_settle(request) {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!("no overall score: {error}");
                return Ok(Score::unscored(format!("not scored: {error}")));
            }
        };
        let reply = response.output_text();
        if !trace.model_output.is_empty() {
            trace.model_output.push_str("\n\n--- re-ask ---\n\n");
        }
        trace.model_output.push_str(&reply);
        trace.usage.add(&response.usage);

        // The verdict arrives as a function call, so there is no JSON to
        // pick out of prose and no fence to strip. A model that answers in
        // text has not answered, and has left nothing of its own to send back.
        let Some((call_id, arguments)) = response
            .function_calls()
            .find(|(_, name, _)| *name == SubmitSummary::NAME)
            .map(|(call_id, _, arguments)| (call_id.to_string(), arguments.to_string()))
        else {
            return Err(Rejected::nothing_to_answer(format!(
                "it called no {}",
                SubmitSummary::NAME
            )));
        };
        let outcome = read_summary(&arguments);
        trace.tool_calls.push(ToolCall {
            name: SubmitSummary::NAME.to_string(),
            input: arguments.clone(),
            output: match &outcome {
                Ok(_) => "recorded".to_string(),
                Err(reason) => reason.clone(),
            },
            duration_ms: 0,
            succeeded: outcome.is_ok(),
        });
        let (score, summary) = outcome.map_err(|reason| Rejected {
            reason: reason.clone(),
            answer: Some(Answer {
                call_id,
                arguments,
                refusal: reason,
            }),
        })?;
        Ok(Score {
            overall_score: Some(score),
            summary: Some(summary),
            unscored_reason: None,
        })
    }
}

/// Why one scoring attempt produced no verdict, and what of the model's the
/// next attempt owes an answer to.
struct Rejected {
    reason: String,
    answer: Option<Answer>,
}

struct Answer {
    call_id: String,
    arguments: String,
    refusal: String,
}

impl Rejected {
    fn nothing_to_answer(reason: String) -> Self {
        Self {
            reason,
            answer: None,
        }
    }
}

/// The arguments as the model wrote them, read through the tool that owns
/// the schema. Malformed JSON and a bad score come back the same way: both
/// mean "this call carried no verdict", and both earn the same re-ask.
fn read_summary(arguments: &str) -> Result<(u8, String), String> {
    let parsed: serde_json::Value = serde_json::from_str(arguments)
        .map_err(|error| format!("its arguments are not JSON: {error}"))?;
    SubmitSummary::read(&parsed).map_err(|error| error.to_string())
}

/// Why scoring is skipped. "Nothing was found" is not one of these: that
/// case still asks the model. These are the runs that never finished looking.
fn nothing_to_score(review: &ReviewOutput, unproduced: &[Unproduced]) -> String {
    if let Some(reason) = &review.stopped {
        return format!("not scored: the run stopped before every chunk was reviewed ({reason})");
    }
    if review.chunks.is_empty() {
        return "not scored: no chunk was reviewed, so there was nothing to score".to_string();
    }
    if !unproduced.is_empty() {
        return format!(
            "not scored: {} of {} chunks gave no readable answer and the rest reported nothing",
            unproduced.len(),
            review.chunks.len()
        );
    }
    "not scored: no chunk was reviewed, so there was nothing to score".to_string()
}

/// The band table, endpoints included. It lives here because `domain` holds
/// no logic and the model is not asked to report the band itself.
fn band(score: u8) -> Confidence {
    match score {
        90..=100 => Confidence::Certain,
        70..=89 => Confidence::High,
        40..=69 => Confidence::Medium,
        _ => Confidence::Low,
    }
}

/// The same cut points on the other axis, so a reader learns one table rather
/// than two. The names differ because the questions do.
fn severity_band(score: u8) -> Severity {
    match score {
        90..=100 => Severity::Critical,
        70..=89 => Severity::Major,
        40..=69 => Severity::Minor,
        _ => Severity::Trivial,
    }
}

/// The numbers reviewbot never invents: missing or not a 0-100 integer and the
/// entry goes, because there is no default to fall back on. Read through the
/// same function the tools use, so a quoted number is read the same way here
/// as it is on the way in. The field is named because the note goes into the
/// trace and "which of the two" is the first thing a reader asks.
fn score_of(field: &str, value: Option<&serde_json::Value>) -> Result<u8, String> {
    let Some(value) = value else {
        return Err(format!("no {field}"));
    };
    whole_score(Some(value))
        .ok_or_else(|| format!("{field} {value} is not an integer between 0 and 100"))
}

/// What the scoring round offers, straight from the registry: one tool, the
/// one that takes a verdict. A round shows what it accepts and nothing else,
/// so this list is also the whole of what this call will be answered with.
fn scoring_tools(context: &StageContext<'_>) -> Vec<ToolSchema> {
    context
        .adapters
        .tools
        .schemas_for(Round::Scoring)
        .into_iter()
        .map(|schema| ToolSchema {
            name: schema.name,
            description: schema.description,
            parameters: schema.parameters,
        })
        .collect()
}

/// Step 1. Not a model reply: `review` builds this document out of the
/// `submit_comment` calls it accepted, so the only way it fails to parse is a
/// damaged checkpoint. Nothing to ask again — and since the score became a
/// `submit_summary` call, no stage reads JSON a model wrote as prose, so the
/// fence stripper that used to live here is gone.
fn parse_document(raw: &str) -> Result<Vec<serde_json::Value>, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let document: RawDocument = serde_json::from_str(text).map_err(|error| error.to_string())?;
    Ok(document.comments)
}

/// The file a chunk was about. Chunks are named by their new path, and a
/// file with no new side never becomes a chunk.
fn file_of<'a>(changeset: &'a ChangeSet, path: &str) -> Option<&'a FileChange> {
    changeset.files.iter().find(|file| file.new_path == path)
}

/// The closest commentable line within the window, preferring the line above
/// on a tie so the choice is reproducible.
fn nearest_commentable(commentable: &BTreeSet<u32>, line: u32) -> Option<u32> {
    (1..=ALIGN_WINDOW)
        .flat_map(|distance| [line.checked_sub(distance), line.checked_add(distance)])
        .flatten()
        .find(|candidate| commentable.contains(candidate))
}

fn offset(from: u32, to: u32) -> String {
    match to >= from {
        true => format!("+{}", to - from),
        false => format!("-{}", from - to),
    }
}

/// The model's end line, kept only when it still describes a range that can
/// carry a comment. A file level comment has no range at all.
fn end_line_of(
    alignment: &Alignment,
    end_line: Option<u32>,
    commentable: &BTreeSet<u32>,
) -> Option<u32> {
    let line = alignment.line?;
    end_line.filter(|end| *end > line && commentable.contains(end))
}

/// A file level comment sorts after the lines of the same file rather than
/// in front of them: it is the vaguest of the bunch.
fn sort_line(comment: &Comment) -> u32 {
    comment.target.line.unwrap_or(u32::MAX)
}

/// Step 4, for one quotation. Two questions, both mechanical: is this text
/// really in what the tool printed, and is it about this line. Either answer
/// being no makes the badge say so; neither of them makes the comment wrong.
fn check_quote(
    quote: &RawQuote,
    file: &ChunkFile<'_>,
    line: Option<u32>,
) -> Result<String, String> {
    let Some(tool) = quote.tool.as_deref().filter(|tool| !tool.is_empty()) else {
        return Err("evidence.tool_quote names no tool".to_string());
    };
    let Some(text) = quote.text.as_deref().filter(|text| !text.is_empty()) else {
        return Err(format!(
            "evidence.tool_quote quotes {tool} but carries no text"
        ));
    };

    let calls: Vec<&ToolCall> = file
        .tool_calls
        .iter()
        .filter(|call| call.name == tool)
        .collect();
    if calls.is_empty() {
        return Err(format!("{tool} was never called on this chunk"));
    }
    let wanted = normalize_quote(text, file.root);
    let Some(call) = calls
        .iter()
        .find(|call| normalize_quote(&call.output, file.root).contains(&wanted))
    else {
        return Err(format!(
            "the quotation is not a verbatim substring of what {tool} printed"
        ));
    };

    // Pointing. A quotation that could belong to any file, or to any line of
    // this one, is not evidence for the line the comment hangs on. Tools that
    // print no line numbers fail here, and that is the honest answer.
    if !wanted.contains(file.path) {
        return Err(format!(
            "the quotation from {tool} does not name {}",
            file.path
        ));
    }
    let Some(line) = line else {
        return Err(format!(
            "the comment is file level, so no line in {tool}'s output can confirm it"
        ));
    };
    match line_numbers(&wanted).any(|found| found.abs_diff(line) <= ALIGN_WINDOW) {
        true => Ok(call.name.clone()),
        false => Err(format!(
            "the quotation from {tool} carries no line number within {ALIGN_WINDOW} of line {line}"
        )),
    }
}

/// The only two liberties the comparison takes: runs of whitespace become one
/// space, and absolute paths under the worktree become repository relative.
/// A tool wrapping its output or being run from an absolute path should not
/// look like a fabricated quotation. Everything else is compared as written.
fn normalize_quote(text: &str, root: Option<&Path>) -> String {
    let relative = match root
        .and_then(|root| root.to_str())
        .filter(|root| !root.is_empty())
    {
        Some(root) => text.replace(&format!("{}/", root.trim_end_matches('/')), ""),
        None => text.to_string(),
    };
    relative.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every run of digits in the text, read as a line number. Crude on purpose:
/// tools disagree about `file:12:3`, `line 12` and `(12)`, and all this has
/// to decide is whether the number the comment sits on appears at all.
fn line_numbers(text: &str) -> impl Iterator<Item = u32> + '_ {
    text.split(|character: char| !character.is_ascii_digit())
        .filter(|run| !run.is_empty())
        .filter_map(|run| run.parse::<u32>().ok())
}

/// Step 5's test. Same file, ranges that touch, and the same body once the
/// things two reports of one problem may legitimately differ on are removed.
fn same_finding(left: &Finding, right: &Finding) -> bool {
    left.comment.target.path == right.comment.target.path
        && ranges_intersect(&left.comment, &right.comment)
        && normalized_body(&left.comment.body) == normalized_body(&right.comment.body)
}

/// A one line comment is the range `[line, line]`. Two file level comments on
/// one file overlap each other and nothing else.
fn ranges_intersect(left: &Comment, right: &Comment) -> bool {
    let span = |comment: &Comment| {
        comment
            .target
            .line
            .map(|line| (line, comment.target.end_line.unwrap_or(line)))
    };
    match (span(left), span(right)) {
        (Some((left_from, left_to)), Some((right_from, right_to))) => {
            left_from <= right_to && right_from <= left_to
        }
        (None, None) => true,
        _ => false,
    }
}

/// What two reports of the same problem are allowed to differ on: spacing,
/// and the line numbers they quote. Anything else and they are two findings,
/// because deciding otherwise means deleting one of them unread.
fn normalized_body(body: &str) -> String {
    let without_numbers: String = body
        .chars()
        .map(|character| match character.is_ascii_digit() {
            true => ' ',
            false => character,
        })
        .collect();
    without_numbers
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The survivor keeps its own comment and score, and inherits whatever the
/// loser had that it did not: a badge it never earned, files it never named.
fn merge_findings(survivor: Finding, dropped: Finding) -> Finding {
    let mut external_files = survivor.external_files;
    for path in dropped.external_files {
        if !external_files.contains(&path) {
            external_files.push(path);
        }
    }
    Finding {
        comment: survivor.comment,
        // "found by tool" outranks the other: one of the two really was checked.
        badge: match survivor.badge.as_deref() == Some(TOOL_FOUND) {
            true => survivor.badge,
            false => match dropped.badge.as_deref() == Some(TOOL_FOUND) {
                true => dropped.badge,
                false => survivor.badge.or(dropped.badge),
            },
        },
        external_files,
    }
}

/// Which of two readings of the same defect the merged comment keeps. Damage
/// first, certainty second — the same order the list is sorted in, so the
/// survivor of a merge is the one that would have sorted higher anyway.
fn weight(finding: &Finding) -> (u8, u8) {
    (
        finding.comment.severity_score,
        finding.comment.confidence_score,
    )
}

/// What the scoring call is given: the final list and nothing else. Not the
/// diff, which was already paid for once, and not the dropped entries, which
/// are not being published.
///
/// Both numbers travel, because the verdict is a judgement about them
/// together: one certain trivial note and one uncertain critical defect are
/// very different lists, and a scoring round shown only certainty cannot tell
/// them apart.
fn findings_json(comments: &[Comment]) -> String {
    let findings: Vec<serde_json::Value> = comments
        .iter()
        .map(|comment| {
            serde_json::json!({
                "path": comment.target.path,
                "line": comment.target.line,
                "severity": comment.severity.as_str(),
                "severity_score": comment.severity_score,
                "confidence": comment.confidence.as_str(),
                "confidence_score": comment.confidence_score,
                "body": comment.body,
                "suggestion": comment.suggestion,
            })
        })
        .collect();
    serde_json::json!({ "comments": findings }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::Limit;
    use crate::domain::Hunk;
    use crate::stage::fixture::{Reply, StageFixture};

    /// Every scoring reply is a `submit_summary` call now, so the tests say
    /// so once here rather than wrapping the same JSON seventeen times.
    fn scoring(replies: Vec<&str>) -> StageFixture {
        StageFixture::scripted(
            replies
                .into_iter()
                .map(|arguments| Reply::calls(&[(SubmitSummary::NAME, arguments)]))
                .collect(),
            Limit::Amount(10.0),
        )
    }

    /// One file with the two line sets spelled out, which is all merge reads.
    fn file(path: &str, commentable: &[u32], changed: &[u32]) -> FileChange {
        FileChange {
            old_path: path.to_string(),
            new_path: path.to_string(),
            hunks: vec![Hunk {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                text: format!("@@ -1 +1 @@\n+/* {path} */\n"),
            }],
            commentable_lines: commentable.iter().copied().collect(),
            changed_lines: changed.iter().copied().collect(),
            binary: false,
        }
    }

    fn changeset(files: Vec<FileChange>) -> ChangeSet {
        ChangeSet {
            files,
            ..ChangeSet::default()
        }
    }

    fn chunk(path: &str, raw_output: &str) -> ChunkOutput {
        ChunkOutput {
            path: path.to_string(),
            trace_id: format!("review-{}", path.replace('/', "_")),
            raw_output: raw_output.to_string(),
        }
    }

    fn review(chunks: Vec<ChunkOutput>) -> ReviewOutput {
        ReviewOutput {
            chunks,
            ..ReviewOutput::default()
        }
    }

    /// One entry with everything the contract wants, so each test can bend
    /// exactly the field it is about. Severity is fixed here because most
    /// tests are not about it; `graded` is for the ones that are.
    fn entry(path: &str, line: u32, score: &str, diff_lines: &str, body: &str) -> String {
        graded(path, line, "50", score, diff_lines, body)
    }

    fn graded(
        path: &str,
        line: u32,
        severity: &str,
        score: &str,
        diff_lines: &str,
        body: &str,
    ) -> String {
        format!(
            r#"{{"path":"{path}","line":{line},"body":"{body}","suggestion":"fix it","severity_score":{severity},"confidence_score":{score},"evidence":{{"diff_lines":{diff_lines}}}}}"#
        )
    }

    fn document(entries: &[String]) -> String {
        format!(r#"{{"comments":[{}]}}"#, entries.join(","))
    }

    /// A rerun rewrites what this stage said and leaves the rest alone. It
    /// used to clear the whole list, taking `review`'s account of the
    /// conversation with it — and a comment nobody really investigated then
    /// read exactly like one that was investigated properly.
    #[test]
    fn running_twice_replaces_this_stages_notes_and_keeps_the_other_stages() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11], &[11])]);
        let raw = document(&[entry("src/parse.c", 11, "88", "[11]", "a finding")]);
        let mut fixture = scoring(vec![
            r#"{"overall_score":40,"summary":"first pass"}"#,
            r#"{"overall_score":40,"summary":"second pass"}"#,
        ]);
        write_trace(&fixture, "review-src_parse.c");
        {
            let mut trace = fixture
                .recorder()
                .read_trace("review-src_parse.c")
                .expect("readable")
                .expect("written");
            trace.note(
                crate::stage::review::NAME,
                "the tool loop reached its ceiling of 2 rounds",
            );
            fixture.recorder().write_trace(&trace).expect("written");
        }
        let review = review(vec![chunk("src/parse.c", &raw)]);

        merge(&mut fixture, &changeset, &review);
        let after_first = notes_by(&fixture, "review-src_parse.c", NAME).len();
        merge(&mut fixture, &changeset, &review);

        assert_eq!(
            notes_by(&fixture, "review-src_parse.c", NAME).len(),
            after_first,
            "this stage's notes are rewritten, not doubled"
        );
        assert_eq!(
            notes_by(&fixture, "review-src_parse.c", crate::stage::review::NAME),
            vec!["the tool loop reached its ceiling of 2 rounds".to_string()],
            "how far the investigation got is not this stage's to erase"
        );
    }

    /// The trace `review` would have left behind, which is where this stage
    /// writes its own notes.
    fn write_trace(fixture: &StageFixture, trace_id: &str) {
        let mut trace = Trace::new(trace_id);
        trace.prompt = "the review instructions, byte for byte".to_string();
        trace.diff = "--- a/src/parse.c\n+++ b/src/parse.c\n@@ -10,2 +10,3 @@\n+int added(void);\n"
            .to_string();
        fixture
            .recorder()
            .write_trace(&trace)
            .expect("the trace is written");
    }

    /// What one stage wrote on a trace, which is the only thing a test about
    /// that stage may assert on.
    fn notes_by(fixture: &StageFixture, trace_id: &str, stage: &str) -> Vec<String> {
        fixture
            .recorder()
            .read_trace(trace_id)
            .expect("readable")
            .expect("the trace is on disk")
            .notes_by(stage)
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn checks_of(fixture: &StageFixture, trace_id: &str) -> Vec<String> {
        notes_by(fixture, trace_id, NAME)
    }

    fn merge(
        fixture: &mut StageFixture,
        changeset: &ChangeSet,
        review: &ReviewOutput,
    ) -> MergeOutput {
        let mut context = fixture.context();
        Merge::run(&mut context, changeset, review).expect("merge finishes")
    }

    #[test]
    fn a_comment_leaning_only_on_context_lines_is_dropped_and_one_on_an_added_line_is_kept() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11, 12, 13], &[11, 12])]);
        let raw = document(&[
            entry("src/parse.c", 11, "80", "[11]", "on an added line"),
            entry("src/parse.c", 10, "80", "[10,13]", "only context lines"),
            entry("src/parse.c", 11, "80", "[]", "no diff_lines at all"),
        ]);
        let mut fixture = scoring(vec![r#"{"overall_score":42,"summary":"one finding"}"#]);
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );

        assert_eq!(output.comments.len(), 1, "{:?}", output.comments);
        assert_eq!(output.comments[0].body, "on an added line");
        let checks = checks_of(&fixture, "review-src_parse.c");
        assert!(
            checks
                .iter()
                .any(|check| check.contains("comment 1 dropped") && check.contains("touch none")),
            "{checks:?}"
        );
        assert!(
            checks
                .iter()
                .any(|check| check.contains("comment 2 dropped")
                    && check.contains("no evidence.diff_lines")),
            "{checks:?}"
        );
    }

    #[test]
    fn alignment_walks_the_four_steps_and_never_touches_the_score() {
        let commentable: BTreeSet<u32> = [10, 11, 12, 13].into_iter().collect();

        let exact = Merge::align(&commentable, Some(11), &[11]);
        assert_eq!(exact.line, Some(11));
        assert!(exact.note.is_none(), "an exact hit says nothing");

        let near = Merge::align(&commentable, Some(9), &[11]);
        assert_eq!(near.line, Some(10), "the window reaches three lines");
        assert!(
            near.note.as_deref().unwrap().contains("+1"),
            "{:?}",
            near.note
        );

        let fallback = Merge::align(&commentable, Some(80), &[40, 12]);
        assert_eq!(fallback.line, Some(12), "diff_lines is the third try");
        assert!(
            fallback.note.as_deref().unwrap().contains("diff_lines"),
            "{:?}",
            fallback.note
        );

        let missing = Merge::align(&commentable, Some(80), &[40, 41]);
        assert_eq!(missing.line, None, "the fourth step is file level");
        assert!(missing.note.as_deref().unwrap().contains("file level"));

        // A pure deletion file keeps its adjacent context line in the
        // commentable set, so a deletion still has somewhere to hang; a file
        // with nothing commentable at all degrades instead of vanishing.
        assert_eq!(Merge::align(&BTreeSet::new(), Some(11), &[11]).line, None);
    }

    #[test]
    fn a_file_level_fallback_publishes_the_score_the_model_gave() {
        let changeset = changeset(vec![file("src/parse.c", &[500], &[500])]);
        let raw = document(&[entry(
            "src/parse.c",
            11,
            "63",
            "[500]",
            "far from any commentable line",
        )]);
        let mut fixture = scoring(vec![r#"{"overall_score":50,"summary":"s"}"#]);
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );

        assert_eq!(output.comments.len(), 1);
        assert_eq!(
            output.comments[0].target.line,
            Some(500),
            "diff_lines carried it"
        );
        assert_eq!(
            output.comments[0].confidence_score, 63,
            "the score is untouched"
        );
        assert_eq!(output.comments[0].confidence, Confidence::Medium);
    }

    #[test]
    fn a_comment_about_another_file_is_dropped_and_the_other_chunk_is_untouched() {
        let changeset = changeset(vec![
            file("src/parse.c", &[10, 11], &[11]),
            file("src/other.c", &[20, 21], &[21]),
        ]);
        let wrong = document(&[entry("src/other.c", 21, "90", "[21]", "wrong chunk")]);
        let right = document(&[entry("src/other.c", 21, "70", "[21]", "right chunk")]);
        let mut fixture = scoring(vec![r#"{"overall_score":66,"summary":"s"}"#]);
        write_trace(&fixture, "review-src_parse.c");
        write_trace(&fixture, "review-src_other.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![
                chunk("src/parse.c", &wrong),
                chunk("src/other.c", &right),
            ]),
        );

        assert_eq!(output.comments.len(), 1);
        assert_eq!(output.comments[0].body, "right chunk");
        let checks = checks_of(&fixture, "review-src_parse.c");
        assert!(
            checks
                .iter()
                .any(|check| check.contains("is not this chunk's file")),
            "{checks:?}"
        );
    }

    /// The verdict is a function call, so the vendor enforces its shape and
    /// there is no chat body to read JSON out of. This is what used to need
    /// a fence stripper.
    #[test]
    fn the_verdict_comes_back_as_a_tool_call_not_as_json_in_the_reply() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11], &[11])]);
        let raw = document(&[entry("src/parse.c", 11, "88", "[11]", "a finding")]);
        let mut fixture = scoring(vec![r#"{"overall_score":40,"summary":"one finding"}"#]);
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );

        assert_eq!(output.overall_score, Some(40));
        assert_eq!(output.summary.as_deref(), Some("one finding"));
        assert_eq!(fixture.sent().len(), 1, "one call, no re-ask");
        let sent = fixture.sent();
        assert!(
            sent[0]
                .tools
                .iter()
                .any(|tool| tool.name == SubmitSummary::NAME),
            "the schema goes out with the request: {:?}",
            sent[0].tools
        );
        // The call is in the trace the same way a checker call is, so a
        // reader can see what the score was computed from.
        let trace = fixture
            .recorder()
            .read_trace("merge-summary")
            .expect("readable")
            .expect("the trace is on disk");
        assert_eq!(trace.tool_calls.len(), 1);
        assert_eq!(trace.tool_calls[0].name, SubmitSummary::NAME);
        assert!(trace.tool_calls[0].succeeded);
    }

    /// A reply that talks instead of calling has not answered. The re-ask
    /// says so, and a proper call on the second turn still scores.
    #[test]
    fn a_reply_that_only_talks_earns_one_re_ask() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11], &[11])]);
        let raw = document(&[entry("src/parse.c", 11, "88", "[11]", "a finding")]);
        let mut fixture = StageFixture::scripted(
            vec![
                Reply::Message("the change looks fine to me".to_string()),
                Reply::calls(&[(
                    SubmitSummary::NAME,
                    r#"{"overall_score":70,"summary":"ok"}"#,
                )]),
            ],
            Limit::Amount(10.0),
        );
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );

        assert_eq!(output.overall_score, Some(70));
        assert_eq!(fixture.sent().len(), 2);
        let checks = checks_of(&fixture, "merge-summary");
        assert!(
            checks
                .iter()
                .any(|check| check.contains("scoring call did not arrive")),
            "{checks:?}"
        );
    }

    /// A call reviewbot refused still has to be answered before the re-ask:
    /// a stateless protocol cannot match a later reply to a dangling call.
    #[test]
    fn a_refused_call_is_answered_before_the_second_ask() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11], &[11])]);
        let raw = document(&[entry("src/parse.c", 11, "88", "[11]", "a finding")]);
        let mut fixture = scoring(vec![
            r#"{"overall_score":"high","summary":"not a number"}"#,
            r#"{"overall_score":25,"summary":"second time"}"#,
        ]);
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );

        assert_eq!(output.overall_score, Some(25));
        let second = &fixture.sent()[1].input;
        assert!(
            second.iter().any(|item| matches!(item,
                InputItem::FunctionCall { name, .. } if name == SubmitSummary::NAME)),
            "{second:?}"
        );
        assert!(
            second.iter().any(|item| matches!(item,
                InputItem::FunctionCallOutput { output, .. } if output.contains("overall_score"))),
            "the refusal goes back as that call's output: {second:?}"
        );
    }

    /// A summary of nothing but whitespace used to be accepted and stored as
    /// "no summary", so the run published a score with nothing behind it. It
    /// is refused like any other malformed verdict, which buys the re-ask.
    #[test]
    fn a_whitespace_only_summary_is_refused_and_re_asked() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11], &[11])]);
        let raw = document(&[entry("src/parse.c", 11, "88", "[11]", "a finding")]);
        let mut fixture = scoring(vec![
            r#"{"overall_score":80,"summary":"   "}"#,
            r#"{"overall_score":80,"summary":"one finding worth reading"}"#,
        ]);
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );

        assert_eq!(fixture.sent().len(), 2, "the blank summary was refused");
        assert_eq!(output.overall_score, Some(80));
        assert_eq!(output.summary.as_deref(), Some("one finding worth reading"));
    }

    /// Two blank summaries in a row leave the run unscored rather than
    /// scored-with-no-words. `None`, never 0 and never an empty string.
    #[test]
    fn a_summary_that_stays_blank_leaves_the_run_unscored() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11], &[11])]);
        let raw = document(&[entry("src/parse.c", 11, "88", "[11]", "a finding")]);
        let mut fixture = scoring(vec![
            r#"{"overall_score":80,"summary":"  "}"#,
            r#"{"overall_score":80,"summary":""}"#,
        ]);
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );

        assert_eq!(output.overall_score, None);
        assert!(output.summary.is_none());
        assert!(
            output
                .unscored_reason
                .as_deref()
                .unwrap_or_default()
                .contains("summary"),
            "{:?}",
            output.unscored_reason
        );
        assert_eq!(output.comments.len(), 1, "the findings still go out");
    }

    #[test]
    fn a_chunk_that_submitted_nothing_is_no_findings() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11], &[11])]);
        let mut fixture = scoring(vec![r#"{"overall_score":90,"summary":"unused"}"#]);
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", r#"{"comments":[]}"#)]),
        );

        assert!(output.comments.is_empty());
        assert!(output.unproduced.is_empty());
        assert_eq!(output.overall_score, Some(90));
        assert!(output.unscored_reason.is_none());
        assert_eq!(fixture.sent().len(), 1, "an empty list still gets a score");
    }

    /// A damaged checkpoint, not a badly behaved model: the document is the
    /// one `review` serialised. It costs no call and does not touch the rest.
    #[test]
    fn a_chunk_document_that_will_not_parse_is_listed_and_its_sibling_is_kept() {
        let changeset = changeset(vec![
            file("src/parse.c", &[10, 11], &[11]),
            file("src/other.c", &[20, 21], &[21]),
        ]);
        let good = document(&[entry("src/other.c", 21, "75", "[21]", "the sibling")]);
        let mut fixture = scoring(vec![r#"{"overall_score":30,"summary":"one finding"}"#]);
        write_trace(&fixture, "review-src_parse.c");
        write_trace(&fixture, "review-src_other.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![
                chunk("src/parse.c", "not json"),
                chunk("src/other.c", &good),
            ]),
        );

        assert_eq!(output.comments.len(), 1);
        assert_eq!(output.comments[0].body, "the sibling");
        assert_eq!(fixture.sent().len(), 1, "only the scoring call");
        assert_eq!(output.unproduced.len(), 1);
        assert_eq!(output.unproduced[0].path, "src/parse.c");
        assert!(
            output.unproduced[0].reason.contains("would not parse"),
            "{:?}",
            output.unproduced[0].reason
        );
    }

    #[test]
    fn one_entry_without_a_score_goes_and_its_neighbours_stay_without_a_re_ask() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11, 12], &[11, 12])]);
        let raw = format!(
            r#"{{"comments":[{},{},{}]}}"#,
            r#"{"path":"src/parse.c","line":11,"body":"no score at all","severity_score":50,"evidence":{"diff_lines":[11]}}"#,
            r#"{"path":"src/parse.c","line":12,"body":"a fractional score","severity_score":50,"confidence_score":82.5,"evidence":{"diff_lines":[12]}}"#,
            entry("src/parse.c", 11, "44", "[11]", "the neighbour"),
        );
        let mut fixture = scoring(vec![r#"{"overall_score":55,"summary":"s"}"#]);
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );

        assert_eq!(output.comments.len(), 1);
        assert_eq!(output.comments[0].body, "the neighbour");
        assert_eq!(fixture.sent().len(), 1, "only the scoring call was made");
        let checks = checks_of(&fixture, "review-src_parse.c");
        assert!(
            checks
                .iter()
                .any(|check| check.contains("comment 0 dropped")
                    && check.contains("no confidence_score")),
            "{checks:?}"
        );
        assert!(
            checks
                .iter()
                .any(|check| check.contains("comment 1 dropped") && check.contains("82.5")),
            "{checks:?}"
        );
    }

    #[test]
    fn one_entry_without_a_suggestion_goes_and_its_neighbours_stay() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11, 12], &[11, 12])]);
        let raw = format!(
            r#"{{"comments":[{},{}]}}"#,
            r#"{"path":"src/parse.c","line":11,"body":"a real problem","severity_score":50,"confidence_score":80,"evidence":{"diff_lines":[11]}}"#,
            entry("src/parse.c", 12, "44", "[12]", "the neighbour"),
        );
        let mut fixture = scoring(vec![r#"{"overall_score":55,"summary":"s"}"#]);
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );

        assert_eq!(output.comments.len(), 1);
        assert_eq!(output.comments[0].body, "the neighbour");
        assert_eq!(output.comments[0].suggestion, "fix it");
        let checks = checks_of(&fixture, "review-src_parse.c");
        assert!(
            checks
                .iter()
                .any(|check| check.contains("comment 0 dropped") && check.contains("no suggestion")),
            "{checks:?}"
        );
    }

    #[test]
    fn the_band_table_reads_the_way_it_is_written() {
        assert_eq!(band(0), Confidence::Low);
        assert_eq!(band(39), Confidence::Low);
        assert_eq!(band(40), Confidence::Medium);
        assert_eq!(band(69), Confidence::Medium);
        assert_eq!(band(70), Confidence::High);
        assert_eq!(band(89), Confidence::High);
        assert_eq!(band(90), Confidence::Certain);
        assert_eq!(band(100), Confidence::Certain);
    }

    #[test]
    fn the_list_is_ordered_by_score_then_by_path_and_line() {
        let changeset = changeset(vec![
            file("src/a.c", &[1, 2, 3], &[1, 2, 3]),
            file("src/b.c", &[1, 2, 3], &[1, 2, 3]),
        ]);
        let first = document(&[
            entry("src/a.c", 3, "80", "[3]", "a3"),
            entry("src/a.c", 1, "80", "[1]", "a1"),
            entry("src/a.c", 2, "95", "[2]", "a95"),
        ]);
        let second = document(&[entry("src/b.c", 1, "80", "[1]", "b1")]);
        let mut fixture = scoring(vec![r#"{"overall_score":20,"summary":"s"}"#]);
        write_trace(&fixture, "review-src_a.c");
        write_trace(&fixture, "review-src_b.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/a.c", &first), chunk("src/b.c", &second)]),
        );

        let order: Vec<&str> = output
            .comments
            .iter()
            .map(|comment| comment.body.as_str())
            .collect();
        assert_eq!(order, vec!["a95", "a1", "a3", "b1"]);
        let counts = output.counts();
        assert_eq!(counts[0], (Confidence::Certain, 1));
        assert_eq!(counts[1], (Confidence::High, 3));
    }

    /// The whole reason there are two numbers. Sorting on certainty alone put
    /// a sure naming quibble above an uncertain memory error, and on a busy
    /// merge request the top of the list is the only part that gets read.
    #[test]
    fn the_worst_finding_leads_even_when_a_trivial_one_is_more_certain() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11, 12], &[10, 11, 12])]);
        let raw = document(&[
            graded(
                "src/parse.c",
                10,
                "20",
                "99",
                "[10]",
                "the name reads oddly",
            ),
            graded("src/parse.c", 11, "95", "45", "[11]", "the buffer overruns"),
            graded(
                "src/parse.c",
                12,
                "95",
                "80",
                "[12]",
                "the pointer is freed twice",
            ),
        ]);
        let mut fixture = scoring(vec![r#"{"overall_score":30,"summary":"s"}"#]);
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );

        let order: Vec<&str> = output
            .comments
            .iter()
            .map(|comment| comment.body.as_str())
            .collect();
        assert_eq!(
            order,
            vec![
                "the pointer is freed twice",
                "the buffer overruns",
                "the name reads oddly",
            ],
            "worst first, certainty breaking the tie"
        );
        assert_eq!(output.comments[0].severity, Severity::Critical);
        assert_eq!(output.comments[0].confidence, Confidence::High);
        // Both numbers are the model's, kept as given on both axes.
        assert_eq!(output.comments[2].severity_score, 20);
        assert_eq!(output.comments[2].confidence_score, 99);
        assert_eq!(output.comments[2].severity, Severity::Trivial);
        assert_eq!(output.comments[2].confidence, Confidence::Certain);
    }

    /// The scoring round cannot weigh what it cannot see: a list shown only
    /// certainty reads one certain triviality and one uncertain critical
    /// defect as much the same thing.
    #[test]
    fn the_scoring_round_is_shown_both_numbers() {
        let changeset = changeset(vec![file("src/parse.c", &[11], &[11])]);
        let raw = document(&[graded(
            "src/parse.c",
            11,
            "95",
            "45",
            "[11]",
            "the buffer overruns",
        )]);
        let mut fixture = scoring(vec![r#"{"overall_score":40,"summary":"s"}"#]);
        write_trace(&fixture, "review-src_parse.c");

        merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );

        let sent = fixture.sent();
        let scoring_call = sent.last().expect("the scoring call");
        let InputItem::Message { content, .. } = &scoring_call.input[0] else {
            panic!(
                "the findings ride in a message: {:?}",
                scoring_call.input[0]
            );
        };
        assert!(content.contains(r#""severity":"critical""#), "{content}");
        assert!(content.contains(r#""severity_score":95"#), "{content}");
        assert!(content.contains(r#""confidence":"medium""#), "{content}");
        assert!(content.contains(r#""confidence_score":45"#), "{content}");
    }

    /// A file level comment cannot come out of a well formed change set —
    /// every changed line is commentable, so step 3 always finds one — but
    /// the order still has to be defined for the one built by hand.
    #[test]
    fn a_file_level_comment_sorts_behind_the_lines_of_the_same_file() {
        let comment = |line: Option<u32>, body: &str| Comment {
            target: CommentTarget {
                path: "src/a.c".to_string(),
                line,
                end_line: None,
            },
            body: body.to_string(),
            suggestion: "fix it".to_string(),
            severity: severity_band(60),
            severity_score: 60,
            confidence: band(80),
            confidence_score: 80,
            trace_id: "review-src_a.c".to_string(),
        };
        let finding = |line: Option<u32>, body: &str| Finding {
            comment: comment(line, body),
            badge: None,
            external_files: Vec::new(),
        };
        let mut findings = vec![
            finding(None, "file level"),
            finding(Some(9), "line 9"),
            finding(Some(2), "line 2"),
        ];
        Merge::sort(&mut findings);
        let order: Vec<&str> = findings
            .iter()
            .map(|finding| finding.comment.body.as_str())
            .collect();
        assert_eq!(order, vec!["line 2", "line 9", "file level"]);
    }

    #[test]
    fn the_scoring_call_is_given_the_final_list_and_nothing_else() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11], &[11])]);
        let raw = document(&[
            entry("src/parse.c", 11, "77", "[11]", "kept in the list"),
            entry("src/parse.c", 10, "99", "[10]", "dropped as out of range"),
        ]);
        let mut fixture = scoring(vec![r#"{"overall_score":61,"summary":"the state"}"#]);
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );

        assert_eq!(output.overall_score, Some(61));
        assert_eq!(output.summary.as_deref(), Some("the state"));
        assert!(output.unscored_reason.is_none());

        let sent = fixture.sent();
        assert_eq!(sent.len(), 1);
        let content = match &sent[0].input[0] {
            InputItem::Message { content, .. } => content.clone(),
            other => panic!("expected a user message, got {other:?}"),
        };
        assert!(content.contains("kept in the list"), "{content}");
        assert!(content.contains(r#""suggestion":"fix it""#), "{content}");
        assert!(
            !content.contains("dropped as out of range"),
            "a dropped comment is not scored: {content}"
        );
        assert!(
            !content.contains("@@ -"),
            "the diff is not sent a second time: {content}"
        );
        // The one tool this round advertises is the way back: nothing that
        // reads a file or runs a checker, because the list is already final.
        assert_eq!(sent[0].tools.len(), 1, "{:?}", sent[0].tools);
        assert_eq!(sent[0].tools[0].name, SubmitSummary::NAME);
    }

    #[test]
    fn an_empty_list_still_gets_a_score() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11], &[11])]);
        let mut fixture = scoring(vec![r#"{"overall_score":90,"summary":"nothing found"}"#]);
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", r#"{"comments":[]}"#)]),
        );

        assert!(output.comments.is_empty());
        assert_eq!(output.overall_score, Some(90));
        assert_eq!(output.summary.as_deref(), Some("nothing found"));
        assert!(output.unscored_reason.is_none());
        assert_eq!(fixture.sent().len(), 1);
        let content = match &fixture.sent()[0].input[0] {
            InputItem::Message { content, .. } => content.clone(),
            other => panic!("expected a user message, got {other:?}"),
        };
        assert!(content.contains(r#""comments":[]"#), "{content}");
    }

    #[test]
    fn nothing_found_and_nothing_reviewed_do_not_read_the_same() {
        let stopped = ReviewOutput {
            chunks: vec![chunk("src/parse.c", r#"{"comments":[]}"#)],
            unreviewed: vec!["src/other.c".to_string()],
            stopped: Some("budget exhausted".to_string()),
            unused_checkers: Vec::new(),
            cut_short: Vec::new(),
            unavailable: Vec::new(),
        };
        let stopped_reason = nothing_to_score(&stopped, &[]);
        assert!(stopped_reason.contains("stopped"), "{stopped_reason}");
        assert!(!stopped_reason.contains("no findings"), "{stopped_reason}");

        let clean = review(vec![chunk("src/parse.c", r#"{"comments":[]}"#)]);

        let unreadable = nothing_to_score(
            &clean,
            &[Unproduced {
                path: "src/parse.c".to_string(),
                reason: "unreadable twice".to_string(),
            }],
        );
        assert!(unreadable.contains("no readable answer"), "{unreadable}");
    }

    #[test]
    fn a_budget_that_cannot_cover_the_score_still_publishes_the_comments() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11], &[11])]);
        let raw = document(&[entry("src/parse.c", 11, "91", "[11]", "still published")]);
        let mut fixture = StageFixture::with_budget(
            vec![r#"{"overall_score":10,"summary":"never asked for"}"#],
            Limit::Nothing,
        );
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );

        assert_eq!(output.comments.len(), 1, "the comments still go out");
        assert_eq!(output.overall_score, None, "never 0 for a missing score");
        assert!(
            output
                .unscored_reason
                .as_deref()
                .unwrap()
                .contains("budget"),
            "{:?}",
            output.unscored_reason
        );
        assert!(fixture.sent().is_empty());
    }

    #[test]
    fn two_unreadable_score_replies_leave_the_score_missing_rather_than_zero() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11], &[11])]);
        let raw = document(&[entry("src/parse.c", 11, "91", "[11]", "a finding")]);
        let mut fixture = StageFixture::scripted(
            vec![
                Reply::Message("the change looks fine to me".to_string()),
                Reply::calls(&[(
                    SubmitSummary::NAME,
                    r#"{"overall_score":"high","summary":"still wrong"}"#,
                )]),
            ],
            Limit::Amount(10.0),
        );
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );

        assert_eq!(output.comments.len(), 1);
        assert_eq!(output.overall_score, None);
        assert_eq!(fixture.sent().len(), 2, "asked exactly once more");
        assert!(
            output
                .unscored_reason
                .as_deref()
                .unwrap()
                .contains("unreadable"),
            "{:?}",
            output.unscored_reason
        );
    }

    #[test]
    fn a_score_of_zero_from_the_model_is_a_real_score() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11], &[11])]);
        let raw = document(&[entry("src/parse.c", 11, "96", "[11]", "a real defect")]);
        let mut fixture = scoring(vec![r#"{"overall_score":0,"summary":"do not merge this"}"#]);
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );

        assert_eq!(output.overall_score, Some(0));
        assert!(output.unscored_reason.is_none());
    }

    /// What cppcheck printed and the model was shown, recorded by `review`
    /// exactly as it went into `function_call_output`.
    fn write_trace_with_tool_output(fixture: &StageFixture, trace_id: &str, output: &str) {
        let mut trace = Trace::new(trace_id);
        trace.prompt = "the review instructions, byte for byte".to_string();
        trace.diff = "--- a/src/parse.c\n+++ b/src/parse.c\n@@ -10,2 +10,3 @@\n+int added(void);\n"
            .to_string();
        trace.tool_calls.push(ToolCall {
            name: "cppcheck".to_string(),
            input: r#"{"path":"src/parse.c"}"#.to_string(),
            output: output.to_string(),
            duration_ms: 3,
            succeeded: true,
        });
        fixture
            .recorder()
            .write_trace(&trace)
            .expect("the trace is written");
    }

    /// One entry quoting cppcheck, so each test can bend one field of the
    /// quotation and leave the rest of the contract alone.
    fn quoting(line: u32, score: u8, quote: &str, note: &str) -> String {
        format!(
            r#"{{"path":"src/parse.c","line":{line},"body":"cppcheck found it",
               "suggestion":"fix it",
               "severity_score":50,
               "confidence_score":{score},
               "evidence":{{"diff_lines":[{line}],
               "tool_quote":{{"tool":"cppcheck","text":"{quote}","note":"{note}"}}}}}}"#
        )
    }

    /// The badge says what reviewbot checked, and the number stays the
    /// model's on every branch: the four runs here differ only in the
    /// quotation, and all four publish the comment with the same 85.
    #[test]
    fn a_quotation_earns_its_badge_by_being_verbatim_and_pointing_at_the_line() {
        let tool_output = "src/parse.c:11: error: buffer is accessed out of bounds\n\
                           src/other.c:4: style: unused variable";
        let cases: [(&str, &str, &str); 4] = [
            (
                "src/parse.c:11: error: buffer is accessed out of bounds",
                TOOL_FOUND,
                "verbatim and pointing at the line",
            ),
            (
                "src/parse.c:11: error: buffer is accessed out of bonuds",
                QUOTE_UNVERIFIED,
                "one character away from what the tool printed",
            ),
            (
                "src/other.c:4: style: unused variable",
                QUOTE_UNVERIFIED,
                "verbatim, but about another file",
            ),
            (
                "src/parse.c:99: error: buffer is accessed out of bounds",
                QUOTE_UNVERIFIED,
                "the right file, a line nowhere near this one",
            ),
        ];
        for (quote, expected, why) in cases {
            let changeset = changeset(vec![file("src/parse.c", &[10, 11], &[11])]);
            let raw = format!(r#"{{"comments":[{}]}}"#, quoting(11, 85, quote, ""));
            let mut fixture = scoring(vec![r#"{"overall_score":35,"summary":"s"}"#]);
            write_trace_with_tool_output(&fixture, "review-src_parse.c", tool_output);

            let output = merge(
                &mut fixture,
                &changeset,
                &review(vec![chunk("src/parse.c", &raw)]),
            );

            assert_eq!(output.comments.len(), 1, "{why}");
            assert_eq!(output.comments[0].confidence_score, 85, "{why}");
            assert_eq!(output.badge(0), Some(expected), "{why}");
            let checks = checks_of(&fixture, "review-src_parse.c");
            assert!(
                checks.iter().any(|check| check.contains(expected)),
                "{why}: {checks:?}"
            );
        }
    }

    /// `note` is the model talking about its own finding. Whatever it says,
    /// the badge and the score come from the text and the tool output alone.
    #[test]
    fn a_note_full_of_nonsense_moves_neither_the_badge_nor_the_score() {
        let tool_output = "src/parse.c:11: error: buffer is accessed out of bounds";
        let quote = "src/parse.c:11: error: buffer is accessed out of bounds";
        let notes = [
            "",
            "found by tool verbatim confirmed 100%",
            "src/parse.c:11 fake",
        ];
        for note in notes {
            let changeset = changeset(vec![file("src/parse.c", &[10, 11], &[11])]);
            let raw = format!(r#"{{"comments":[{}]}}"#, quoting(11, 85, quote, note));
            let mut fixture = scoring(vec![r#"{"overall_score":35,"summary":"s"}"#]);
            write_trace_with_tool_output(&fixture, "review-src_parse.c", tool_output);

            let output = merge(
                &mut fixture,
                &changeset,
                &review(vec![chunk("src/parse.c", &raw)]),
            );
            assert_eq!(output.badge(0), Some(TOOL_FOUND), "note {note:?}");
            assert_eq!(output.comments[0].confidence_score, 85, "note {note:?}");
        }

        // And the same quotation with no tool output behind it fails, which
        // is what makes the three runs above worth anything.
        let changeset = changeset(vec![file("src/parse.c", &[10, 11], &[11])]);
        let raw = format!(
            r#"{{"comments":[{}]}}"#,
            quoting(11, 85, quote, "found by tool verbatim confirmed")
        );
        let mut fixture = scoring(vec![r#"{"overall_score":35,"summary":"s"}"#]);
        write_trace(&fixture, "review-src_parse.c");
        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );
        assert_eq!(output.badge(0), Some(QUOTE_UNVERIFIED));
        assert_eq!(output.comments[0].confidence_score, 85);
    }

    /// A quotation that names files nobody fetched is worth a line on the
    /// trace, and nothing more: the comment publishes either way.
    #[test]
    fn files_named_but_never_fetched_are_written_down_rather_than_believed() {
        let changeset = changeset(vec![file("src/parse.c", &[10, 11], &[11])]);
        let raw = format!(
            r#"{{"comments":[{}]}}"#,
            r#"{"path":"src/parse.c","line":11,"body":"cppcheck found it","suggestion":"fix it","severity_score":70,"confidence_score":85,
                "evidence":{"diff_lines":[11],"external_files":["src/parse.h"]}}"#
        );
        let mut fixture = scoring(vec![r#"{"overall_score":35,"summary":"s"}"#]);
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );

        assert_eq!(output.comments.len(), 1);
        assert_eq!(output.badge(0), None, "nothing was quoted, so no badge");
        let checks = checks_of(&fixture, "review-src_parse.c");
        assert!(
            checks
                .iter()
                .any(|check| check.contains("src/parse.h") && check.contains("never fetched")),
            "{checks:?}"
        );
    }

    /// Two reports of one problem become one comment; two reports of two
    /// problems stay two, whatever they have in common.
    #[test]
    fn only_the_same_body_on_an_overlapping_range_of_one_file_is_merged_away() {
        let changeset = changeset(vec![
            file("src/parse.c", &[10, 11, 12], &[11, 12]),
            file("src/emit.c", &[20], &[20]),
        ]);
        let raw = document(&[
            // The same sentence about the same place, differing only in the
            // line number it quotes and in its spacing.
            entry("src/parse.c", 11, "70", "[11]", "buf is read past line 11"),
            entry(
                "src/parse.c",
                11,
                "88",
                "[11]",
                "buf  is read past  line 12",
            ),
            // The same place, a different problem.
            entry(
                "src/parse.c",
                11,
                "60",
                "[11]",
                "the lock is never released",
            ),
        ]);
        let elsewhere = document(&[entry(
            "src/emit.c",
            20,
            "90",
            "[20]",
            "buf is read past line 20",
        )]);
        let mut fixture = scoring(vec![r#"{"overall_score":40,"summary":"s"}"#]);
        write_trace(&fixture, "review-src_parse.c");
        write_trace(&fixture, "review-src_emit.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![
                chunk("src/parse.c", &raw),
                chunk("src/emit.c", &elsewhere),
            ]),
        );

        let published: Vec<(&str, Option<u32>, u8)> = output
            .comments
            .iter()
            .map(|comment| {
                (
                    comment.target.path.as_str(),
                    comment.target.line,
                    comment.confidence_score,
                )
            })
            .collect();
        assert_eq!(
            published,
            vec![
                // The same body on another file is another finding.
                ("src/emit.c", Some(20), 90),
                // The duplicate pair kept the higher of the two scores.
                ("src/parse.c", Some(11), 88),
                ("src/parse.c", Some(11), 60),
            ]
        );
        let checks = checks_of(&fixture, "review-src_parse.c");
        assert!(
            checks
                .iter()
                .any(|check| check.contains("merged a duplicate")),
            "{checks:?}"
        );
    }

    /// Ranges are the other half of the test: the same sentence about lines
    /// that do not touch is the same class of bug found twice, not once.
    #[test]
    fn the_same_body_on_ranges_that_do_not_touch_stays_two_comments() {
        let changeset = changeset(vec![file("src/parse.c", &[11, 40], &[11, 40])]);
        let raw = document(&[
            entry("src/parse.c", 11, "70", "[11]", "buf is read past the end"),
            entry("src/parse.c", 40, "70", "[40]", "buf is read past the end"),
        ]);
        let mut fixture = scoring(vec![r#"{"overall_score":40,"summary":"s"}"#]);
        write_trace(&fixture, "review-src_parse.c");

        let output = merge(
            &mut fixture,
            &changeset,
            &review(vec![chunk("src/parse.c", &raw)]),
        );
        assert_eq!(output.comments.len(), 2);
    }
}
