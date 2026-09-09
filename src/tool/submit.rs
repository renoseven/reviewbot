//! How a finding is delivered. The model already speaks function calls for
//! checkers and file reads; the comments use the same channel, so reviewbot
//! never has to parse JSON out of a chat message.

use serde_json::{Value, json};

use super::{Origin, Tool, ToolError, ToolOutput};

/// A whole number from 0 to 100 as the model wrote it, a quoted one
/// included. Models that speak function calls stringify integers often
/// enough that refusing `"45"` buys a whole extra round trip and the same 45
/// back — the schema says integer and the vendor does not enforce it.
///
/// Nothing is rounded, clamped or guessed: `"45.7"`, `"high"` and `120` are
/// all still refused. A score is the one number reviewbot never invents, and
/// reading `"45"` as 45 invents nothing.
pub fn whole_score(value: Option<&Value>) -> Option<u8> {
    let score = match value? {
        Value::Number(number) => number.as_u64()?,
        Value::String(text) => text.trim().parse::<u64>().ok()?,
        _ => return None,
    };
    (score <= 100).then_some(score as u8)
}

pub struct SubmitComment;

impl SubmitComment {
    pub const NAME: &'static str = "submit_comment";

    pub fn description_text() -> &'static str {
        "Submit one finding for this file. Call once per finding; several \
         calls in one round are fine. If there is no locatable defect, do \
         not call this tool — reply with a short message instead. body is \
         the problem; suggestion is the fix. path may be omitted (this \
         file). Stop after this round; do not wait for confirmation."
    }

    pub fn parameters_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Repository-relative path of this file as it appears in the diff. Omit to use the file under review."
                },
                "line": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "First commentable line of the finding."
                },
                "end_line": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Last commentable line of the finding; omit if it is a single line."
                },
                "body": {
                    "type": "string",
                    "description": "The problem only: what is wrong, why it is true, and what goes wrong. Do not put the fix here."
                },
                "suggestion": {
                    "type": "string",
                    "description": "How to fix it. Do not restate the problem. Never an empty string."
                },
                "confidence_score": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 100,
                    "description": "Integer 0–100: how sure you are that a reader should act on this."
                },
                "evidence": {
                    "type": "object",
                    "properties": {
                        "diff_lines": {
                            "type": "array",
                            "items": { "type": "integer", "minimum": 0 },
                            "description": "At least one line from this file's diff that the finding rests on."
                        },
                        "external_files": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Other files the finding used; empty if none."
                        },
                        "tool_quote": {
                            "type": "object",
                            "properties": {
                                "tool": { "type": "string" },
                                "text": { "type": "string" },
                                "note": { "type": "string" }
                            }
                        }
                    },
                    "required": ["diff_lines"]
                }
            },
            "required": ["body", "suggestion", "confidence_score", "evidence"]
        })
    }

    /// The chunk already knows which file this is. An omitted or empty path
    /// means "this file", which is what the description promises.
    pub fn with_default_path(mut arguments: Value, path: &str) -> Value {
        let Some(object) = arguments.as_object_mut() else {
            return arguments;
        };
        let missing = match object.get("path") {
            None | Some(Value::Null) => true,
            Some(Value::String(text)) if text.trim().is_empty() => true,
            _ => false,
        };
        if missing {
            object.insert("path".to_string(), Value::String(path.to_string()));
        }
        arguments
    }
}

impl Tool for SubmitComment {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        Self::description_text()
    }

    fn parameters(&self) -> Value {
        Self::parameters_schema()
    }

    fn origin(&self) -> Origin {
        Origin::Builtin
    }

    fn available_when_concluding(&self) -> bool {
        true
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        if is_blank_finding(arguments) {
            return Ok(ToolOutput::new(
                "no finding recorded. Do not call submit_comment unless you have a locatable defect."
                    .to_string(),
            ));
        }
        validate(arguments)?;
        Ok(ToolOutput::new("recorded".to_string()).with_submission(arguments.clone()))
    }
}

/// The overall score, delivered the same way a finding is. `merge` used to
/// read this out of the chat body, which meant parsing JSON a model had
/// written as prose: it arrived wrapped in a markdown fence often enough
/// that reviewbot carried a fence stripper for it alone. A function call has
/// a schema the vendor enforces, so none of that is needed.
///
/// No `Tool` impl, unlike `SubmitComment`: that trait is what the review
/// loop drives a `Registry` through, and this one is called straight from
/// `merge` on the single round that offers it. A second `execute` nobody
/// calls would only drift from `read`.
pub struct SubmitSummary;

impl SubmitSummary {
    pub const NAME: &'static str = "submit_summary";

    pub fn description_text() -> &'static str {
        "Submit the overall verdict for this review. Call exactly once, \
         after weighing the findings you were given. overall_score is how \
         much a reader should act on this review as a whole; summary is a \
         few sentences on the state of the change. This is the only way to \
         answer -- text in the reply is not read."
    }

    pub fn parameters_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "overall_score": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 100,
                    "description": "Integer 0-100 for the review as a whole."
                },
                "summary": {
                    "type": "string",
                    "description": "A few sentences on the state of this change."
                }
            },
            "required": ["overall_score", "summary"]
        })
    }

    /// The score and the summary as the model wrote them. `None` for the
    /// summary when it is blank: an empty string is not a summary, and the
    /// report says "no summary" rather than printing nothing.
    pub fn read(arguments: &Value) -> Result<(u8, Option<String>), ToolError> {
        let Some(object) = arguments.as_object() else {
            return Err(summary_invalid("arguments must be a JSON object"));
        };
        // Never invented: no default score exists, and 0 is a real verdict
        // that has to stay tellable apart from "not scored".
        let Some(score) = whole_score(object.get("overall_score")) else {
            return Err(summary_invalid(
                "overall_score must be an integer between 0 and 100",
            ));
        };
        let summary = object
            .get("summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        Ok((score, summary))
    }
}

fn summary_invalid(reason: &str) -> ToolError {
    ToolError::InvalidArguments {
        tool: SubmitSummary::NAME.to_string(),
        reason: reason.to_string(),
    }
}

fn validate(arguments: &Value) -> Result<(), ToolError> {
    let Some(object) = arguments.as_object() else {
        return Err(invalid("arguments must be a JSON object"));
    };
    nonempty_string(object.get("path"), "path")?;
    nonempty_string(object.get("body"), "body")?;
    nonempty_string(object.get("suggestion"), "suggestion")?;
    if whole_score(object.get("confidence_score")).is_none() {
        return Err(invalid(
            "confidence_score must be an integer between 0 and 100",
        ));
    }
    let Some(lines) = object
        .get("evidence")
        .and_then(Value::as_object)
        .and_then(|evidence| evidence.get("diff_lines"))
        .and_then(Value::as_array)
    else {
        return Err(invalid("evidence.diff_lines is required"));
    };
    if lines.is_empty() {
        return Err(invalid("evidence.diff_lines must name at least one line"));
    }
    Ok(())
}

fn is_blank_finding(arguments: &Value) -> bool {
    let Some(object) = arguments.as_object() else {
        return false;
    };
    blank(object.get("body")) && blank(object.get("suggestion"))
}

fn blank(value: Option<&Value>) -> bool {
    match value.and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() => false,
        _ => true,
    }
}

fn nonempty_string(value: Option<&Value>, field: &str) -> Result<(), ToolError> {
    if blank(value) {
        Err(invalid(&format!(
            "{field} must be a non-empty string. \
             If you have no locatable defect, do not call submit_comment."
        )))
    } else {
        Ok(())
    }
}

fn invalid(reason: &str) -> ToolError {
    ToolError::InvalidArguments {
        tool: SubmitComment::NAME.to_string(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_args() -> Value {
        json!({
            "path": "src/parse.c",
            "line": 11,
            "body": "the index is a constant 5",
            "suggestion": "bound the index",
            "confidence_score": 92,
            "evidence": { "diff_lines": [11] }
        })
    }

    /// The habit that made this necessary: a model that speaks function
    /// calls writes the integer as a string, and a re-ask gets the same
    /// number back one round trip later.
    #[test]
    fn a_quoted_whole_number_is_that_number() {
        assert_eq!(whole_score(Some(&json!("45"))), Some(45));
        assert_eq!(whole_score(Some(&json!(" 45 "))), Some(45));
        assert_eq!(whole_score(Some(&json!(0))), Some(0));
        assert_eq!(whole_score(Some(&json!(100))), Some(100));
    }

    #[test]
    fn nothing_else_is_read_as_a_score() {
        for bad in [
            json!("45.7"),
            json!(45.7),
            json!("high"),
            json!(101),
            json!(-1),
            json!(null),
        ] {
            assert_eq!(whole_score(Some(&bad)), None, "{bad}");
        }
        assert_eq!(whole_score(None), None);
    }

    #[test]
    fn a_quoted_score_reaches_both_submissions() {
        let (score, _) =
            SubmitSummary::read(&json!({"overall_score": "45", "summary": "s"})).expect("accepted");
        assert_eq!(score, 45);
        let mut args = ok_args();
        args["confidence_score"] = json!("92");
        assert!(SubmitComment.execute(&args).is_ok());
    }

    #[test]
    fn a_complete_comment_is_recorded() {
        let output = SubmitComment.execute(&ok_args()).expect("accepted");
        assert_eq!(output.text, "recorded");
        assert_eq!(output.submission.as_ref(), Some(&ok_args()));
    }

    #[test]
    fn a_missing_suggestion_is_refused_so_the_model_can_retry() {
        let mut args = ok_args();
        args.as_object_mut().unwrap().remove("suggestion");
        let error = SubmitComment.execute(&args).expect_err("refused");
        assert!(error.to_string().contains("suggestion"), "{error}");
        assert!(
            error.to_string().contains("do not call submit_comment"),
            "{error}"
        );
    }

    #[test]
    fn a_blank_call_is_no_finding_not_an_error() {
        let output = SubmitComment
            .execute(&json!({"path": "src/main.rs"}))
            .expect("empty call is not a defect");
        assert!(output.submission.is_none());
        assert!(output.text.contains("no finding"), "{}", output.text);
    }

    #[test]
    fn a_summary_call_carries_the_score_and_the_text() {
        let (score, summary) = SubmitSummary::read(&json!({
            "overall_score": 40,
            "summary": "one real finding"
        }))
        .expect("accepted");
        assert_eq!(score, 40);
        assert_eq!(summary.as_deref(), Some("one real finding"));
    }

    /// 0 is a verdict, not a missing value, and the report has to be able to
    /// say "0 out of 100" rather than "not scored".
    #[test]
    fn a_score_of_zero_is_a_score() {
        let (score, _) =
            SubmitSummary::read(&json!({"overall_score": 0, "summary": "bad"})).expect("accepted");
        assert_eq!(score, 0);
    }

    #[test]
    fn a_score_that_is_not_an_integer_zero_to_a_hundred_is_refused() {
        for bad in [
            json!({"summary": "s"}),
            json!({"overall_score": 101, "summary": "s"}),
            json!({"overall_score": "high", "summary": "s"}),
        ] {
            let error = SubmitSummary::read(&bad).expect_err("refused");
            assert!(error.to_string().contains("overall_score"), "{error}");
        }
    }

    #[test]
    fn a_blank_summary_is_no_summary_rather_than_an_empty_one() {
        let (_, summary) =
            SubmitSummary::read(&json!({"overall_score": 90, "summary": "   "})).expect("accepted");
        assert!(summary.is_none());
    }

    #[test]
    fn an_omitted_path_becomes_the_file_under_review() {
        let mut args = ok_args();
        args.as_object_mut().unwrap().remove("path");
        let filled = SubmitComment::with_default_path(args, "src/main.rs");
        let output = SubmitComment.execute(&filled).expect("accepted");
        assert_eq!(output.submission.unwrap()["path"], "src/main.rs");
    }
}
