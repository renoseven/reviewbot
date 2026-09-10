//! How one comment came to be. Two views of the same `trace_id`: the
//! internal one goes into the checkpoint, the published one drops file
//! bodies. The report and the MR comment name the id; they do not fold
//! the view in.

use serde::{Deserialize, Serialize};

use crate::budget::TokenUsage;
use crate::common::truncate;
use crate::domain::Stage;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Trace {
    pub trace_id: String,
    pub tool_calls: Vec<ToolCall>,
    /// The diff hunk that triggered the comment.
    pub diff: String,
    /// Files pulled in as context. Their bodies stay internal.
    pub context_files: Vec<ContextFile>,
    /// Redacted, in full.
    pub prompt: String,
    /// Raw, not post processed.
    pub model_output: String,
    /// Chain of thought. Internal only; the published view omits it.
    #[serde(default)]
    pub reasoning: String,
    /// What each stage wrote down about this comment: how the conversation
    /// went, what was dropped, how far a line moved, whether a quotation held
    /// up. Every line says which stage wrote it, because a reader has to be
    /// able to tell "the model only looked half way" from "the line number
    /// moved by one" — those say very different things about the same comment.
    pub checks: Vec<Check>,
    pub usage: TokenUsage,
}

/// One line of a stage's account of a comment, tagged with the stage that
/// wrote it.
///
/// The tag is a field rather than a prefix on the text: rerunning a stage has
/// to clear what that stage said last time and nothing else, and matching a
/// string prefix to decide would make the tag part of the note's wording,
/// where the next edit breaks the clearing.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Check {
    pub stage: Stage,
    pub note: String,
}

impl Check {
    pub fn new(stage: Stage, note: impl Into<String>) -> Self {
        Self {
            stage,
            note: note.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolCall {
    pub name: String,
    pub input: String,
    pub output: String,
    pub duration_ms: u64,
    pub succeeded: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ContextFile {
    pub path: String,
    pub first_line: u32,
    pub last_line: u32,
    /// Internal only. The published view keeps the path and the range.
    pub body: String,
}

/// The internal view is the trace itself; naming it keeps the two views
/// symmetrical at the call sites.
pub type InternalView<'a> = &'a Trace;

/// What may leave the machine: no file bodies, tool output clipped to a
/// ceiling with a pointer back to the internal view.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PublishedView {
    pub trace_id: String,
    pub tool_calls: Vec<ToolCall>,
    pub diff: String,
    pub context_files: Vec<ContextRange>,
    pub prompt: String,
    pub model_output: String,
    pub checks: Vec<Check>,
    pub usage: TokenUsage,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ContextRange {
    pub path: String,
    pub first_line: u32,
    pub last_line: u32,
}

impl Trace {
    pub fn new(trace_id: impl Into<String>) -> Self {
        Self {
            trace_id: trace_id.into(),
            ..Self::default()
        }
    }

    pub fn internal(&self) -> InternalView<'_> {
        self
    }

    /// Write one line down, in the name of the stage writing it.
    pub fn note(&mut self, stage: Stage, note: impl Into<String>) {
        self.checks.push(Check::new(stage, note));
    }

    /// Whether this stage already said this. Used where a note would otherwise
    /// be written twice by the same stage.
    pub fn has_note(&self, stage: Stage, note: &str) -> bool {
        self.checks
            .iter()
            .any(|check| check.stage == stage && check.note == note)
    }

    /// Forget what one stage said, because it is about to say it again. Only
    /// that stage: the account of the conversation belongs to `review`, and a
    /// second `merge` used to wipe it, leaving a comment whose evidence looked
    /// like it had never been investigated at all.
    pub fn forget(&mut self, stage: Stage) {
        self.checks.retain(|check| check.stage != stage);
    }

    /// What one stage wrote, in order.
    pub fn notes_by(&self, stage: Stage) -> Vec<&str> {
        self.checks
            .iter()
            .filter(|check| check.stage == stage)
            .map(|check| check.note.as_str())
            .collect()
    }

    /// `max_tool_output_bytes` bounds each tool call's output; the prompt
    /// keeps its diff hunks but loses embedded file bodies.
    pub fn published(&self, max_tool_output_bytes: usize) -> PublishedView {
        PublishedView {
            trace_id: self.trace_id.clone(),
            tool_calls: self
                .tool_calls
                .iter()
                .map(|call| call.summarized(max_tool_output_bytes))
                .collect(),
            diff: self.diff.clone(),
            context_files: self.context_files.iter().map(ContextFile::range).collect(),
            prompt: self.prompt_without_file_bodies(),
            model_output: self.model_output.clone(),
            checks: self.checks.clone(),
            usage: self.usage,
        }
    }

    fn prompt_without_file_bodies(&self) -> String {
        let mut prompt = self.prompt.clone();
        for file in &self.context_files {
            if file.body.is_empty() {
                continue;
            }
            prompt = prompt.replace(
                file.body.as_str(),
                &format!(
                    "<{} lines {}-{}, body in the internal trace>",
                    file.path, file.first_line, file.last_line
                ),
            );
        }
        prompt
    }
}

impl ToolCall {
    fn summarized(&self, max_bytes: usize) -> ToolCall {
        let input = truncate(&self.input, max_bytes);
        let output = truncate(&self.output, max_bytes);
        let note = |text: String, omitted: usize| {
            if omitted == 0 {
                text
            } else {
                format!("{text}\n<{omitted} more bytes in the internal trace>")
            }
        };
        ToolCall {
            name: self.name.clone(),
            input: note(input.text, input.omitted_bytes),
            output: note(output.text, output.omitted_bytes),
            duration_ms: self.duration_ms,
            succeeded: self.succeeded,
        }
    }
}

impl ContextFile {
    fn range(&self) -> ContextRange {
        ContextRange {
            path: self.path.clone(),
            first_line: self.first_line,
            last_line: self.last_line,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_published_view_keeps_paths_but_drops_file_bodies() {
        let mut trace = Trace::new("t1");
        trace.prompt = "context:\nsecret body text\nend".to_string();
        trace.context_files.push(ContextFile {
            path: "src/parse.h".to_string(),
            first_line: 1,
            last_line: 3,
            body: "secret body text".to_string(),
        });

        let published = trace.published(1024);
        assert!(!published.prompt.contains("secret body text"));
        assert!(published.prompt.contains("src/parse.h lines 1-3"));
        assert_eq!(published.context_files[0].path, "src/parse.h");
        assert!(trace.internal().context_files[0].body.contains("secret"));
    }

    #[test]
    fn oversized_tool_output_is_clipped_not_dropped() {
        let mut trace = Trace::new("t1");
        trace.tool_calls.push(ToolCall {
            name: "cppcheck".to_string(),
            input: "src/parse.c".to_string(),
            output: "x".repeat(100),
            duration_ms: 12,
            succeeded: true,
        });
        let published = trace.published(10);
        assert!(published.tool_calls[0].output.starts_with(&"x".repeat(10)));
        assert!(
            published.tool_calls[0]
                .output
                .contains("90 more bytes in the internal trace")
        );
    }
}
