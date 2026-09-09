//! `POST {base_url}/responses`. One implementation serves every OpenAI
//! compatible vendor, DeepSeek included; adding such a vendor is a
//! `[[provider]]` entry, not code.
//!
//! Stateless by design: no `previous_response_id`, no `store`, no streaming.

use std::error::Error as _;
use std::io::ErrorKind;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::budget::TokenUsage;
use crate::config::{Backoff, Secret};
use crate::security::Redactor;

use super::{InputItem, OutputItem, Protocol, ProtocolError, Request, Response, Role};

const PROTOCOL: &str = OpenAi::NAME;
/// Thinking models can spend minutes inside one completion. 60s cut the
/// JSON off while the model was still reasoning.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const ERROR_SNIPPET: usize = 160;

pub struct OpenAi {
    client: reqwest::blocking::Client,
    base_url: String,
    api_key: Secret,
    backoff: Backoff,
    redactor: Redactor,
}

impl OpenAi {
    pub const NAME: &'static str = "openai";

    pub fn new(base_url: String, api_key: Secret, backoff: Backoff) -> Self {
        let mut redactor = Redactor::new();
        redactor.hide_value(api_key.expose());
        let client = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("reqwest TLS client");
        Self {
            client,
            base_url,
            api_key,
            backoff,
            redactor,
        }
    }

    pub fn endpoint(&self) -> String {
        format!("{}/responses", self.base_url.trim_end_matches('/'))
    }

    fn send_once(&self, body: &VendorRequest<'_>) -> Result<Response, SendFailure> {
        let response = self
            .client
            .post(self.endpoint())
            .header("Authorization", format!("Bearer {}", self.api_key.expose()))
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .map_err(|error| self.transport_error(error))?;

        let status = response.status();
        let retry_after = retry_after_delay(response.headers());
        let text = response
            .text()
            .map_err(|error| self.transport_error(error))?;
        let code = status.as_u16();

        if !status.is_success() {
            return Err(self.status_error(code, &text, retry_after));
        }
        parse_responses_body(&text, &self.redactor).map_err(|error| {
            if error.is_transient() {
                SendFailure::transient(error, retry_after)
            } else {
                SendFailure::fatal(error)
            }
        })
    }

    fn transport_error(&self, error: reqwest::Error) -> SendFailure {
        let reason = self.redactor.redact(&error.to_string());
        let transient = error.is_timeout()
            || error.is_connect()
            || error.is_request()
            || is_connection_reset(&error);
        let protocol_error = if transient {
            ProtocolError::Transient {
                protocol: PROTOCOL,
                reason,
                status: error.status().map(|status| status.as_u16()),
            }
        } else {
            ProtocolError::Fatal {
                protocol: PROTOCOL,
                reason,
                status: error.status().map(|status| status.as_u16()),
            }
        };
        if transient {
            SendFailure::transient(protocol_error, None)
        } else {
            SendFailure::fatal(protocol_error)
        }
    }

    fn status_error(&self, status: u16, body: &str, retry_after: Option<Duration>) -> SendFailure {
        let reason = format!("HTTP {status}: {}", snippet(body, &self.redactor));
        if is_transient_status(status) {
            SendFailure::transient(
                ProtocolError::Transient {
                    protocol: PROTOCOL,
                    reason,
                    status: Some(status),
                },
                retry_after,
            )
        } else {
            SendFailure::fatal(ProtocolError::Fatal {
                protocol: PROTOCOL,
                reason,
                status: Some(status),
            })
        }
    }

    fn delay(&self, failure: &SendFailure, attempt: u32) -> Duration {
        match failure.retry_after {
            Some(after) => after,
            None => Duration::from_millis(
                self.backoff
                    .delay_with_jitter_ms(attempt, jitter_fraction()),
            ),
        }
    }
}

impl Protocol for OpenAi {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn send(&self, request: &Request) -> Result<Response, ProtocolError> {
        let body = VendorRequest::from(request);
        let attempts = self.backoff.attempts();
        let mut last_error = None;
        for attempt in 0..attempts {
            tracing::debug!(
                url = %self.endpoint(),
                model = body.model,
                attempt = attempt + 1,
                attempts,
                "POST /responses"
            );
            match self.send_once(&body) {
                Ok(response) => return Ok(response),
                Err(failure) if failure.is_transient() && attempt + 1 < attempts => {
                    let delay = self.delay(&failure, attempt);
                    tracing::warn!(
                        attempt = attempt + 1,
                        attempts,
                        delay_ms = delay.as_millis() as u64,
                        status = failure.status(),
                        reason = %failure.error,
                        "retrying model call"
                    );
                    std::thread::sleep(delay);
                    last_error = Some(failure.error);
                }
                Err(failure) => return Err(failure.error),
            }
        }
        Err(last_error.unwrap_or_else(|| ProtocolError::Transient {
            protocol: PROTOCOL,
            reason: "retries exhausted".to_string(),
            status: None,
        }))
    }
}

struct SendFailure {
    error: ProtocolError,
    retry_after: Option<Duration>,
}

impl SendFailure {
    fn transient(error: ProtocolError, retry_after: Option<Duration>) -> Self {
        Self { error, retry_after }
    }

    fn fatal(error: ProtocolError) -> Self {
        Self {
            error,
            retry_after: None,
        }
    }

    fn is_transient(&self) -> bool {
        self.error.is_transient()
    }

    fn status(&self) -> Option<u16> {
        match &self.error {
            ProtocolError::Transient { status, .. } | ProtocolError::Fatal { status, .. } => {
                *status
            }
            _ => None,
        }
    }
}

#[derive(Serialize)]
struct VendorRequest<'a> {
    model: &'a str,
    instructions: &'a str,
    input: Vec<VendorInput<'a>>,
    tools: Vec<VendorTool<'a>>,
    max_output_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<VendorReasoning<'a>>,
}

#[derive(Serialize)]
struct VendorReasoning<'a> {
    effort: &'a str,
}

#[derive(Serialize)]
struct VendorInput<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<&'a str>,
}

#[derive(Serialize)]
struct VendorTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

impl<'a> From<&'a Request> for VendorRequest<'a> {
    fn from(request: &'a Request) -> Self {
        Self {
            model: &request.model,
            instructions: &request.instructions,
            input: request.input.iter().map(VendorInput::from).collect(),
            tools: request
                .tools
                .iter()
                .map(|tool| VendorTool {
                    kind: "function",
                    name: &tool.name,
                    description: &tool.description,
                    parameters: &tool.parameters,
                })
                .collect(),
            max_output_tokens: request.max_output_tokens,
            reasoning: request
                .reasoning_effort
                .as_deref()
                .map(|effort| VendorReasoning { effort }),
        }
    }
}

impl<'a> From<&'a InputItem> for VendorInput<'a> {
    fn from(item: &'a InputItem) -> Self {
        match item {
            InputItem::Message { role, content } => Self {
                kind: "message",
                role: Some(match role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                }),
                content: Some(content),
                call_id: None,
                name: None,
                arguments: None,
                output: None,
            },
            // The call goes back out with its output, because a stateless
            // protocol has no other way to say which output answered what.
            InputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => Self {
                kind: "function_call",
                role: None,
                content: None,
                call_id: Some(call_id),
                name: Some(name),
                arguments: Some(arguments),
                output: None,
            },
            InputItem::FunctionCallOutput { call_id, output } => Self {
                kind: "function_call_output",
                role: None,
                content: None,
                call_id: Some(call_id),
                name: None,
                arguments: None,
                output: Some(output),
            },
        }
    }
}

#[derive(Debug, Deserialize)]
struct VendorResponse {
    #[serde(default)]
    output: Option<Vec<VendorOutput>>,
    #[serde(default)]
    output_text: Option<String>,
    #[serde(default)]
    usage: Option<VendorUsage>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
}

#[derive(Debug, Deserialize)]
struct IncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

/// Vendors send function arguments as a JSON string or as an object. Either
/// becomes the string the rest of the pipeline already knows how to parse.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum WireArguments {
    Text(String),
    Value(serde_json::Value),
}

impl WireArguments {
    fn into_string(self) -> String {
        match self {
            Self::Text(text) => text,
            Self::Value(serde_json::Value::String(text)) => text,
            Self::Value(value) => value.to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct VendorOutput {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    content: Option<VendorContent>,
    #[serde(default)]
    summary: Option<Vec<VendorPart>>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<WireArguments>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum VendorContent {
    Text(String),
    Parts(Vec<VendorPart>),
}

#[derive(Debug, Deserialize)]
struct VendorPart {
    #[serde(rename = "type")]
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VendorUsage {
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
    #[serde(default)]
    cached_tokens: Option<u32>,
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u32>,
    #[serde(default)]
    input_tokens_details: Option<InputTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct InputTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
}

fn parse_responses_body(body: &str, redactor: &Redactor) -> Result<Response, ProtocolError> {
    if body.trim().is_empty() {
        return Err(ProtocolError::Transient {
            protocol: PROTOCOL,
            reason: "empty body".to_string(),
            status: Some(200),
        });
    }
    match serde_json::from_str::<VendorResponse>(body) {
        Ok(parsed) => parsed.into_response(redactor),
        Err(error) if error.is_eof() => Err(ProtocolError::Transient {
            protocol: PROTOCOL,
            reason: format!("truncated response: {}", snippet(body, redactor)),
            status: Some(200),
        }),
        Err(error) if serde_json::from_str::<serde_json::Value>(body).is_ok() => {
            Err(ProtocolError::Malformed {
                protocol: PROTOCOL,
                reason: format!(
                    "response JSON is the wrong shape: {error}; {}",
                    snippet(body, redactor)
                ),
            })
        }
        Err(_) => Err(ProtocolError::Fatal {
            protocol: PROTOCOL,
            reason: format!("response is not JSON: {}", snippet(body, redactor)),
            status: Some(200),
        }),
    }
}

impl VendorResponse {
    fn into_response(self, redactor: &Redactor) -> Result<Response, ProtocolError> {
        if self.output.is_none() && self.output_text.is_none() {
            return Err(ProtocolError::Malformed {
                protocol: PROTOCOL,
                reason: format!("response has no output: {}", snippet("{}", redactor)),
            });
        }
        let usage = usage_from(self.usage.as_ref());
        let incomplete = self.incomplete_reason();
        let mut output = Vec::new();
        for item in self.output.iter().flatten() {
            if let Some(mapped) = item.to_output_item() {
                output.push(mapped);
            }
        }
        let has_message = output
            .iter()
            .any(|item| matches!(item, OutputItem::Message { .. }));
        if !has_message && let Some(text) = self.output_text.filter(|text| !text.is_empty()) {
            output.push(OutputItem::Message { text });
        }
        Ok(Response {
            output,
            usage,
            incomplete,
        })
    }

    fn incomplete_reason(&self) -> Option<String> {
        let reason = self
            .incomplete_details
            .as_ref()
            .and_then(|details| details.reason.clone())
            .filter(|reason| !reason.is_empty());
        match (self.status.as_deref(), reason) {
            (Some("incomplete"), None) => Some("incomplete".to_string()),
            (_, reason) => reason,
        }
    }
}

impl VendorOutput {
    fn to_output_item(&self) -> Option<OutputItem> {
        match self.kind.as_str() {
            "message" | "output_text" => {
                let text = self.message_text();
                if text.is_empty() {
                    None
                } else {
                    Some(OutputItem::Message { text })
                }
            }
            "function_call" => Some(OutputItem::FunctionCall {
                call_id: self.call_id.clone().unwrap_or_default(),
                name: self.name.clone().unwrap_or_default(),
                arguments: self
                    .arguments
                    .clone()
                    .map(WireArguments::into_string)
                    .filter(|text| !text.trim().is_empty())
                    .unwrap_or_else(|| "{}".to_string()),
            }),
            "reasoning" => {
                let text = self.reasoning_text();
                if text.is_empty() {
                    None
                } else {
                    Some(OutputItem::Reasoning { text })
                }
            }
            _ => None,
        }
    }

    fn message_text(&self) -> String {
        let from_content = match &self.content {
            Some(VendorContent::Text(text)) => text.clone(),
            Some(VendorContent::Parts(parts)) => parts
                .iter()
                .filter(|part| {
                    matches!(
                        part.kind.as_deref(),
                        None | Some("output_text") | Some("text")
                    )
                })
                .filter_map(|part| part.text.as_deref())
                .collect::<Vec<_>>()
                .join(""),
            None => String::new(),
        };
        if !from_content.is_empty() {
            from_content
        } else {
            self.text.clone().unwrap_or_default()
        }
    }

    fn reasoning_text(&self) -> String {
        let mut parts = Vec::new();
        if let Some(text) = &self.text {
            parts.push(text.as_str());
        }
        if let Some(summary) = &self.summary {
            for part in summary {
                if let Some(text) = &part.text {
                    parts.push(text.as_str());
                }
            }
        }
        if let Some(VendorContent::Text(text)) = &self.content {
            parts.push(text.as_str());
        }
        if let Some(VendorContent::Parts(items)) = &self.content {
            for part in items {
                if let Some(text) = &part.text {
                    parts.push(text.as_str());
                }
            }
        }
        parts.join("")
    }
}

fn usage_from(usage: Option<&VendorUsage>) -> TokenUsage {
    let Some(usage) = usage else {
        return TokenUsage::default();
    };
    let cached = usage
        .input_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens)
        .or(usage.cached_tokens)
        .or(usage.prompt_cache_hit_tokens)
        .unwrap_or(0);
    // `output_tokens` already includes reasoning tokens. Adding
    // `output_tokens_details.reasoning_tokens` would bill thinking twice.
    TokenUsage {
        input_tokens: usage.input_tokens.unwrap_or(0),
        cached_input_tokens: cached,
        output_tokens: usage.output_tokens.unwrap_or(0),
    }
}

fn is_transient_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

fn is_connection_reset(error: &reqwest::Error) -> bool {
    let mut source = error.source();
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<std::io::Error>()
            && matches!(
                io.kind(),
                ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::ConnectionAborted
            )
        {
            return true;
        }
        source = err.source();
    }
    false
}

fn retry_after_delay(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get("retry-after")?.to_str().ok()?.trim();
    if let Ok(seconds) = raw.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let when = httpdate::parse_http_date(raw).ok()?;
    Some(
        when.duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

fn snippet(text: &str, redactor: &Redactor) -> String {
    let redacted = redactor.redact(text);
    redacted.chars().take(ERROR_SNIPPET).collect()
}

fn jitter_fraction() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos) / 1_000_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{InputItem, Role};

    fn key() -> Secret {
        Secret::from("sk-testkey12xxxxxxxx".to_string())
    }

    fn protocol(base_url: String) -> OpenAi {
        OpenAi::new(base_url, key(), Backoff::new(2))
    }

    fn sample_request() -> Request {
        Request {
            model: "deepseek-v4-flash".to_string(),
            instructions: "review this".to_string(),
            input: vec![InputItem::Message {
                role: Role::User,
                content: "--- a/x\n+++ b/x\n".to_string(),
            }],
            tools: Vec::new(),
            max_output_tokens: 4096,
            reasoning_effort: None,
        }
    }

    fn responses_json() -> serde_json::Value {
        serde_json::json!({
            "id": "resp_123",
            "object": "response",
            "output": [
                {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "thinking"}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {"type": "output_text", "text": "{\"comments\":[]}"}
                    ]
                },
                {
                    "type": "function_call",
                    "call_id": "c1",
                    "name": "cppcheck",
                    "arguments": "{}"
                }
            ],
            "output_text": "{\"comments\":[]}",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "input_tokens_details": {"cached_tokens": 20},
                "output_tokens_details": {"reasoning_tokens": 10}
            }
        })
    }

    async fn send_blocking(base_url: String, request: Request) -> Result<Response, ProtocolError> {
        // The blocking client owns a runtime; build it off the test runtime.
        tokio::task::spawn_blocking(move || protocol(base_url).send(&request))
            .await
            .expect("join")
    }

    #[test]
    fn base_url_is_a_prefix_not_a_full_endpoint() {
        let protocol = protocol("https://api.deepseek.com/".to_string());
        assert_eq!(protocol.endpoint(), "https://api.deepseek.com/responses");
    }

    #[test]
    fn reasoning_effort_is_omitted_from_the_wire_when_unset() {
        let body = serde_json::to_value(VendorRequest::from(&sample_request())).unwrap();
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn reasoning_effort_is_sent_when_the_model_entry_sets_it() {
        let mut request = sample_request();
        request.reasoning_effort = Some("low".to_string());
        let body = serde_json::to_value(VendorRequest::from(&request)).unwrap();
        assert_eq!(body["reasoning"]["effort"], "low");
    }

    #[test]
    fn message_text_comes_from_content_and_usage_keeps_cached_tokens() {
        let redactor = Redactor::new();
        let body = serde_json::to_string(&responses_json()).unwrap();
        let response = parse_responses_body(&body, &redactor).expect("parsed");
        assert_eq!(response.output_text(), r#"{"comments":[]}"#);
        assert_eq!(
            response.usage,
            TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 20,
                output_tokens: 50,
            }
        );
        assert_eq!(response.function_calls().count(), 1);
        assert!(
            response
                .output
                .iter()
                .any(|item| matches!(item, OutputItem::Reasoning { text } if text == "thinking"))
        );
        assert!(!response.truncated(4096));
    }

    #[test]
    fn function_call_arguments_may_be_an_object() {
        let redactor = Redactor::new();
        let body = serde_json::json!({
            "output": [{
                "type": "function_call",
                "call_id": "c1",
                "name": "submit_comment",
                "arguments": {"path": "src/main.rs", "body": "problem"}
            }]
        })
        .to_string();
        let response = parse_responses_body(&body, &redactor).expect("parsed");
        let calls: Vec<_> = response.function_calls().collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "submit_comment");
        let parsed: serde_json::Value = serde_json::from_str(calls[0].2).expect("json");
        assert_eq!(parsed["path"], "src/main.rs");
        assert_eq!(parsed["body"], "problem");
    }

    #[test]
    fn incomplete_status_is_kept_when_the_message_is_missing() {
        let redactor = Redactor::new();
        let body = r#"{
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "reasoning", "summary": [{"type": "summary_text", "text": "still thinking"}]}],
            "usage": {"input_tokens": 80, "output_tokens": 4096}
        }"#;
        let response = parse_responses_body(body, &redactor).expect("parsed");
        assert!(response.output_text().is_empty());
        assert_eq!(response.incomplete.as_deref(), Some("max_output_tokens"));
        assert_eq!(response.usage.output_tokens, 4096);
        assert!(response.truncated(4096));
    }

    #[test]
    fn output_text_is_used_when_message_content_is_missing() {
        let redactor = Redactor::new();
        let body = r#"{"output":[],"output_text":"{\"comments\":[]}"}"#;
        let response = parse_responses_body(body, &redactor).expect("parsed");
        assert_eq!(response.output_text(), r#"{"comments":[]}"#);
    }

    #[test]
    fn empty_body_and_cut_off_json_are_transient() {
        let redactor = Redactor::new();
        let empty = parse_responses_body("", &redactor).expect_err("empty");
        assert!(empty.is_transient());
        let cut = parse_responses_body("{\"output\":[", &redactor).expect_err("cut off");
        assert!(cut.is_transient());
    }

    #[test]
    fn a_complete_wrong_schema_is_malformed_not_transient() {
        let redactor = Redactor::new();
        let error = parse_responses_body("{\"not\":\"responses\"}", &redactor).expect_err("wrong");
        assert!(matches!(error, ProtocolError::Malformed { .. }));
        assert!(!error.is_transient());
    }

    #[test]
    fn a_complete_non_json_body_is_fatal_not_transient() {
        let redactor = Redactor::new();
        let error = parse_responses_body("not json", &redactor).expect_err("plain");
        assert!(!error.is_transient());
        assert!(matches!(error, ProtocolError::Fatal { .. }));
    }

    #[test]
    fn error_snippets_are_redacted() {
        let mut redactor = Redactor::new();
        redactor.hide_value("sk-testkey12xxxxxxxx");
        let shown = snippet(
            "Authorization: Bearer sk-testkey12xxxxxxxx leaked",
            &redactor,
        );
        assert!(!shown.contains("sk-testkey12xxxxxxxx"), "{shown}");
    }

    #[tokio::test]
    async fn a_responses_shaped_body_maps_to_response_and_usage() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/responses"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer sk-testkey12xxxxxxxx",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(responses_json()))
            .expect(1)
            .mount(&server)
            .await;

        let response = send_blocking(server.uri(), sample_request())
            .await
            .expect("200");
        assert_eq!(response.output_text(), r#"{"comments":[]}"#);
        assert_eq!(response.usage.cached_input_tokens, 20);
        assert_eq!(response.usage.output_tokens, 50);

        let received = server.received_requests().await.expect("received");
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json");
        assert_eq!(body["model"], "deepseek-v4-flash");
        assert_eq!(body["tools"], serde_json::json!([]));
        assert_eq!(body["max_output_tokens"], 4096);
        assert!(body.get("reasoning").is_none());
        assert!(body.get("stream").is_none());
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("store").is_none());
    }

    #[tokio::test]
    async fn two_503s_then_200_succeeds_after_three_requests() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/responses"))
            .respond_with(wiremock::ResponseTemplate::new(503).insert_header("Retry-After", "0"))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/responses"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(responses_json()))
            .mount(&server)
            .await;

        let response = send_blocking(server.uri(), sample_request())
            .await
            .expect("eventual 200");
        assert_eq!(response.usage.input_tokens, 100);
        let received = server.received_requests().await.expect("received");
        assert_eq!(received.len(), 3);
    }

    #[tokio::test]
    async fn http_401_is_fatal_and_is_not_retried() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/responses"))
            .respond_with(wiremock::ResponseTemplate::new(401).set_body_string(r#"{"error":"no"}"#))
            .expect(1)
            .mount(&server)
            .await;

        let error = send_blocking(server.uri(), sample_request())
            .await
            .expect_err("401");
        assert!(!error.is_transient());
        assert!(matches!(
            error,
            ProtocolError::Fatal {
                status: Some(401),
                ..
            }
        ));
        assert_eq!(server.received_requests().await.expect("received").len(), 1);
    }

    #[tokio::test]
    async fn http_429_honors_retry_after_and_then_succeeds() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/responses"))
            .respond_with(wiremock::ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/responses"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(responses_json()))
            .mount(&server)
            .await;

        send_blocking(server.uri(), sample_request())
            .await
            .expect("recovered from 429");
        assert_eq!(server.received_requests().await.expect("received").len(), 2);
    }

    #[tokio::test]
    async fn empty_bodies_are_retried_then_fail_as_transient() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/responses"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string("")
                    .insert_header("Retry-After", "0"),
            )
            .expect(3)
            .mount(&server)
            .await;

        let error = send_blocking(server.uri(), sample_request())
            .await
            .expect_err("still empty");
        assert!(error.is_transient());
        assert_eq!(server.received_requests().await.expect("received").len(), 3);
    }

    #[tokio::test]
    async fn a_key_in_the_request_does_not_appear_in_error_text() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/responses"))
            .respond_with(
                wiremock::ResponseTemplate::new(401).set_body_string("leaked sk-testkey12xxxxxxxx"),
            )
            .mount(&server)
            .await;

        let error = send_blocking(server.uri(), sample_request())
            .await
            .expect_err("401");
        let shown = error.to_string();
        assert!(!shown.contains("sk-testkey12xxxxxxxx"), "{shown}");
    }
}
