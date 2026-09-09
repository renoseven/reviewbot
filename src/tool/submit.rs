//! How a finding and a verdict are delivered. The model already speaks
//! function calls for checkers and file reads; both submissions use the same
//! channel, so reviewbot never has to parse JSON out of a chat message.

use serde_json::Value;

use super::signature::{Parameter, Shape, Signature};
use super::{Purpose, Round, Tool, ToolError, ToolOutput};

/// A whole number from 0 to 100 as the model wrote it, a quoted one included.
/// Read through the same rule the signature check uses, so a score arriving as
/// `"45"` means the same thing wherever it is read.
///
/// Nothing is rounded, clamped or guessed: `"45.7"`, `"high"` and `120` are all
/// still refused. A score is the one number reviewbot never invents, and
/// reading `"45"` as 45 invents nothing.
pub fn whole_score(value: Option<&Value>) -> Option<u8> {
    let score = match value? {
        Value::Number(number) => number.as_u64()?,
        Value::String(text) => text.trim().parse::<u64>().ok()?,
        _ => return None,
    };
    (score <= 100).then_some(score as u8)
}

pub struct SubmitComment {
    signature: Signature,
}

impl SubmitComment {
    pub const NAME: &'static str = "submit_comment";

    /// Offered while investigating and still offered once the investigation
    /// tools are gone: handing over what it has is the one thing the model
    /// must always be able to do.
    pub const ROUNDS: [Round; 2] = [Round::Investigation, Round::Conclusion];

    pub fn new() -> Self {
        Self {
            signature: Signature::new(vec![
                Parameter::optional(
                    "path",
                    Shape::Path,
                    "Repository-relative path of this file as it appears in the diff. Omit to use \
                     the file under review.",
                ),
                Parameter::optional(
                    "line",
                    Shape::line(),
                    "First commentable line of the finding.",
                ),
                Parameter::optional(
                    "end_line",
                    Shape::line(),
                    "Last commentable line of the finding; omit if it is a single line.",
                ),
                Parameter::required(
                    "body",
                    Shape::text(),
                    "The problem only: what is wrong, why it is true, and what goes wrong. Do not \
                     put the fix here.",
                ),
                Parameter::required(
                    "suggestion",
                    Shape::text(),
                    "How to fix it. Do not restate the problem. Never an empty string.",
                ),
                Parameter::required(
                    "confidence_score",
                    Shape::percentage(),
                    "Integer 0-100: how sure you are that a reader should act on this.",
                ),
                Parameter::required(
                    "evidence",
                    Shape::Object(vec![
                        Parameter::required(
                            "diff_lines",
                            Shape::Array(Box::new(Shape::line())),
                            "At least one line from this file's diff that the finding rests on.",
                        ),
                        Parameter::optional(
                            "external_files",
                            Shape::Array(Box::new(Shape::Path)),
                            "Other files the finding used; omit if none.",
                        ),
                        Parameter::optional(
                            "tool_quote",
                            Shape::Object(vec![
                                Parameter::required(
                                    "tool",
                                    Shape::text(),
                                    "The tool whose output this quotes.",
                                ),
                                Parameter::required(
                                    "text",
                                    Shape::text(),
                                    "The quotation, copied verbatim from that tool's output.",
                                ),
                                Parameter::optional(
                                    "note",
                                    Shape::text(),
                                    "One sentence on why that warning applies here.",
                                ),
                            ]),
                            "A verbatim quotation from a checker, if the finding rests on one.",
                        ),
                    ]),
                    "What the finding rests on.",
                ),
            ]),
        }
    }

    pub fn description_text() -> &'static str {
        "Submit one finding for this file. Call once per finding; several \
         calls in one round are fine. body is the problem; suggestion is \
         the fix. path may be omitted (this file). Stop after this round; \
         do not wait for confirmation. If you have no locatable defect, do \
         not call this tool at all: call finish_review instead. A filed \
         \"no problems found\" is published as a finding and counts towards \
         the score, so it is worse than nothing."
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

impl Default for SubmitComment {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for SubmitComment {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        Self::description_text()
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn purpose(&self) -> Purpose {
        Purpose::Delivery
    }

    fn rounds(&self) -> &'static [Round] {
        &Self::ROUNDS
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        // Before the signature check, because an empty call is not a badly
        // formed finding: it is the model saying it has none, and the prompt
        // tells it to reply with a short message instead. Refusing here would
        // buy a round trip and the same empty call back.
        if is_blank_finding(arguments) {
            return Ok(ToolOutput::new(
                "nothing filed for this file. A call with no problem and no fix is not a finding; \
                 call finish_review when you have none."
                    .to_string(),
            )
            .finishing());
        }
        let checked = self.signature.validate(Self::NAME, arguments)?;
        let lines = checked
            .get("evidence")
            .and_then(|evidence| evidence.get("diff_lines"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if lines.is_empty() {
            return Err(ToolError::InvalidArguments {
                tool: Self::NAME.to_string(),
                reason: "evidence.diff_lines must name at least one line".to_string(),
            });
        }
        Ok(ToolOutput::new("recorded".to_string()).with_submission(arguments.clone()))
    }
}

/// How the model says it is done with nothing to file.
///
/// Reviewing a file and finding it sound is the ordinary outcome, and it needs
/// an ending of its own. Without one, the only way to end a turn without a
/// finding is to reply in prose — and a model that speaks function calls does
/// not read prose as a terminal action. Handed one tool and told to conclude,
/// it calls that tool: real runs came back with `submit_comment` carrying
/// `body: "No defect found in this change."` and `suggestion: "N/A"`, which
/// passed every check a finding has to pass (the fields were not empty, the
/// evidence named a changed line) and went out as a published comment with a
/// confidence of 95 attached to it.
///
/// The fix is this signal, not a guard on `submit_comment`'s wording. "There
/// is nothing wrong here" has no shape a check can recognise — any gate on the
/// body is a gate that guesses, and it would throw away real findings whose
/// wording happened to look reassuring.
///
/// It takes no arguments. Whatever the model wants to say about why it found
/// nothing belongs in its reply, which the trace records anyway; an argument
/// here would only be somewhere to write the finding it just said it did not
/// have.
pub struct FinishReview {
    signature: Signature,
}

impl FinishReview {
    pub const NAME: &'static str = "finish_review";

    /// Offered wherever `submit_comment` is: ending with nothing has to be
    /// available exactly when filing something is.
    pub const ROUNDS: [Round; 2] = [Round::Investigation, Round::Conclusion];

    pub fn new() -> Self {
        Self {
            signature: Signature::new(Vec::new()),
        }
    }

    pub fn description_text() -> &'static str {
        "End your review of this file with nothing to file. Call this instead \
         of submit_comment when you found no locatable defect, or when the \
         only things you could say are not defects in this change. It takes no \
         arguments and produces no comment. Say why in your reply if you like. \
         Do not use it to leave early while you still have a finding to file, \
         and do not file a placeholder finding to end the round."
    }
}

impl Default for FinishReview {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for FinishReview {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        Self::description_text()
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn purpose(&self) -> Purpose {
        Purpose::Delivery
    }

    fn rounds(&self) -> &'static [Round] {
        &Self::ROUNDS
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        self.signature.validate(Self::NAME, arguments)?;
        Ok(ToolOutput::new("nothing filed for this file".to_string()).finishing())
    }
}

/// The overall score, delivered the same way a finding is. `merge` used to read
/// this out of the chat body, which meant parsing JSON a model had written as
/// prose: it arrived wrapped in a markdown fence often enough that reviewbot
/// carried a fence stripper for it alone. A function call has a schema the
/// vendor enforces, so none of that is needed.
///
/// Registered like any other tool, on the scoring round alone: `merge` asks the
/// registry what that round offers rather than writing the schema out again.
pub struct SubmitSummary {
    signature: Signature,
}

impl SubmitSummary {
    pub const NAME: &'static str = "submit_summary";

    pub const ROUNDS: [Round; 1] = [Round::Scoring];

    pub fn new() -> Self {
        Self {
            signature: Signature::new(vec![
                Parameter::required(
                    "overall_score",
                    Shape::percentage(),
                    "Integer 0-100 for the review as a whole.",
                ),
                Parameter::required(
                    "summary",
                    Shape::text(),
                    "A few sentences on the state of this change.",
                ),
            ]),
        }
    }

    pub fn description_text() -> &'static str {
        "Submit the overall verdict for this review. Call exactly once, \
         after weighing the findings you were given. overall_score is how \
         much a reader should act on this review as a whole; summary is a \
         few sentences on the state of the change. This is the only way to \
         answer -- text in the reply is not read."
    }

    /// The score and the summary as the model wrote them. Whitespace is not a
    /// summary: a call carrying only spaces answered nothing, and accepting it
    /// would publish a verdict with no words behind it. It is refused so the
    /// re-ask can happen, the same as a missing score.
    pub fn read(arguments: &Value) -> Result<(u8, String), ToolError> {
        let checked = Self::new().signature.validate(Self::NAME, arguments)?;
        let Some(score) = checked
            .integer("overall_score")
            .and_then(|score| u8::try_from(score).ok())
        else {
            return Err(summary_invalid(
                "overall_score must be an integer between 0 and 100",
            ));
        };
        let summary = checked
            .text("summary")
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| {
                summary_invalid("summary must be a few sentences, not blank or whitespace")
            })?;
        Ok((score, summary.to_string()))
    }
}

impl Default for SubmitSummary {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for SubmitSummary {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        Self::description_text()
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn purpose(&self) -> Purpose {
        Purpose::Delivery
    }

    fn rounds(&self) -> &'static [Round] {
        &Self::ROUNDS
    }

    /// `merge` reads the verdict off the call itself, so this hands the
    /// arguments back as a submission after checking them, exactly as
    /// `submit_comment` does with a finding.
    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        Self::read(arguments)?;
        Ok(ToolOutput::new("recorded".to_string()).with_submission(arguments.clone()))
    }
}

fn summary_invalid(reason: &str) -> ToolError {
    ToolError::InvalidArguments {
        tool: SubmitSummary::NAME.to_string(),
        reason: reason.to_string(),
    }
}

/// No problem and no fix is the model saying it found nothing, whatever else
/// the call carries.
fn is_blank_finding(arguments: &Value) -> bool {
    let Some(object) = arguments.as_object() else {
        return false;
    };
    blank(object.get("body")) && blank(object.get("suggestion"))
}

fn blank(value: Option<&Value>) -> bool {
    !matches!(value.and_then(Value::as_str), Some(text) if !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

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
        assert!(SubmitComment::new().execute(&args).is_ok());
    }

    #[test]
    fn a_complete_comment_is_recorded() {
        let output = SubmitComment::new().execute(&ok_args()).expect("accepted");
        assert_eq!(output.text, "recorded");
        assert_eq!(output.submission.as_ref(), Some(&ok_args()));
    }

    #[test]
    fn a_missing_suggestion_is_refused_so_the_model_can_retry() {
        let mut args = ok_args();
        args.as_object_mut().unwrap().remove("suggestion");
        let error = SubmitComment::new().execute(&args).expect_err("refused");
        assert!(error.to_string().contains("suggestion"), "{error}");
    }

    #[test]
    fn a_finding_that_rests_on_no_diff_line_is_refused() {
        let mut args = ok_args();
        args["evidence"]["diff_lines"] = json!([]);
        let error = SubmitComment::new().execute(&args).expect_err("refused");
        assert!(error.to_string().contains("diff_lines"), "{error}");
    }

    #[test]
    fn a_blank_call_is_no_finding_not_an_error() {
        let output = SubmitComment::new()
            .execute(&json!({"path": "src/main.rs"}))
            .expect("empty call is not a defect");
        assert!(output.submission.is_none());
        assert!(output.text.contains("nothing filed"), "{}", output.text);
        assert!(
            output.finished,
            "a blank call is the model saying it has none"
        );
    }

    #[test]
    fn a_summary_call_carries_the_score_and_the_text() {
        let (score, summary) = SubmitSummary::read(&json!({
            "overall_score": 40,
            "summary": "one real finding"
        }))
        .expect("accepted");
        assert_eq!(score, 40);
        assert_eq!(summary, "one real finding");
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

    /// A summary of nothing but spaces used to pass the emptiness check and
    /// be stored as "no summary", which published a score with no words
    /// behind it. It is refused, which is what earns the one re-ask.
    #[test]
    fn a_blank_summary_is_refused_rather_than_read_as_no_summary() {
        for blank in ["", "   ", "\n\t "] {
            let error = SubmitSummary::read(&json!({"overall_score": 90, "summary": blank}))
                .expect_err("whitespace is not a summary");
            assert!(error.to_string().contains("summary"), "{error}");
        }
        let missing =
            SubmitSummary::read(&json!({"overall_score": 90})).expect_err("no summary at all");
        assert!(missing.to_string().contains("summary"), "{missing}");
    }

    #[test]
    fn an_omitted_path_becomes_the_file_under_review() {
        let mut args = ok_args();
        args.as_object_mut().unwrap().remove("path");
        let filled = SubmitComment::with_default_path(args, "src/main.rs");
        let output = SubmitComment::new().execute(&filled).expect("accepted");
        assert_eq!(output.submission.unwrap()["path"], "src/main.rs");
    }

    /// Both submissions are offered on their own rounds and nowhere else: the
    /// verdict has no business appearing while a file is under review, and a
    /// finding has none on the round that scores the finished list.
    #[test]
    fn each_submission_belongs_to_its_own_rounds() {
        let comment = SubmitComment::new();
        assert!(comment.offered_on(Round::Investigation));
        assert!(comment.offered_on(Round::Conclusion));
        assert!(!comment.offered_on(Round::Scoring));

        let summary = SubmitSummary::new();
        assert!(summary.offered_on(Round::Scoring));
        assert!(!summary.offered_on(Round::Investigation));
        assert!(!summary.offered_on(Round::Conclusion));
    }
}
