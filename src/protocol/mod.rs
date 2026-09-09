//! The wire protocol for model calls. `Request` and `Response` are the only
//! shapes anything above this layer sees; vendor JSON stops here.
//!
//! One of the three extension points.

pub mod openai;

use serde::{Deserialize, Serialize};

use crate::budget::{TokenUsage, estimate_tokens};
use crate::common::{Backoff, Secret};
use crate::config::{KNOWN_PROTOCOLS, Provider};

pub use openai::OpenAi;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("unknown protocol {protocol:?}; known protocols: {known}")]
    Unknown { protocol: String, known: String },
    /// Timeouts, resets, 5xx, 429, empty body, cut-off JSON.
    #[error("{protocol} request failed: {reason}")]
    Transient {
        protocol: &'static str,
        reason: String,
        status: Option<u16>,
    },
    /// 401/403/400/422 and every other 4xx except 429.
    #[error("{protocol} request failed: {reason}")]
    Fatal {
        protocol: &'static str,
        reason: String,
        status: Option<u16>,
    },
    /// A complete JSON body that is the wrong schema. Merge's scoring call
    /// may re-ask; the HTTP layer does not retry this.
    #[error("{protocol} returned something this build cannot read: {reason}")]
    Malformed {
        protocol: &'static str,
        reason: String,
    },
    #[error("{protocol} is not implemented yet")]
    NotImplemented { protocol: &'static str },
}

impl ProtocolError {
    /// Only transient failures may be retried. Everything else fails on the
    /// first attempt: a 401 will not fix itself.
    pub fn is_transient(&self) -> bool {
        matches!(self, ProtocolError::Transient { .. })
    }
}

/// One call. `instructions` stays byte for byte identical across the run so
/// the vendor's prompt cache can hit; only `input` changes.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Request {
    pub model: String,
    pub instructions: String,
    pub input: Vec<InputItem>,
    pub tools: Vec<ToolSchema>,
    pub max_output_tokens: u32,
    /// `reasoning.effort` on the wire. `None` means the field is not sent.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

impl Request {
    /// Everything that goes out, in tokens. One estimate serves both checks
    /// made before a call: whether the budget covers it and whether the
    /// answer still fits inside the context window.
    pub fn estimated_input_tokens(&self) -> u32 {
        let items: u32 = self
            .input
            .iter()
            .map(|item| match item {
                InputItem::Message { content, .. } => estimate_tokens(content),
                InputItem::FunctionCall {
                    name, arguments, ..
                } => estimate_tokens(name) + estimate_tokens(arguments),
                InputItem::FunctionCallOutput { output, .. } => estimate_tokens(output),
            })
            .sum();
        let tools: u32 = self
            .tools
            .iter()
            .map(|tool| {
                estimate_tokens(&tool.name)
                    + estimate_tokens(&tool.description)
                    + estimate_tokens(&tool.parameters.to_string())
            })
            .sum();
        estimate_tokens(&self.instructions) + items + tools
    }
}

/// The conversation, assembled by the caller. The protocol is stateless: a
/// tool round is replayed by putting the call and its output back into
/// `input`, never by pointing at a previous response.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputItem {
    Message {
        role: Role,
        content: String,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// What the model may call, generated from the tool registry.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Response {
    pub output: Vec<OutputItem>,
    pub usage: TokenUsage,
    /// Why generation stopped, when the vendor said it did. The value we
    /// act on is `max_output_tokens`; anything else is recorded and ignored.
    #[serde(default)]
    pub incomplete: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    Message {
        text: String,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// Chain of thought. Goes into the trace, never into a comment body.
    Reasoning {
        text: String,
    },
}

impl Response {
    /// The concatenated message text, which is what `merge` parses.
    pub fn output_text(&self) -> String {
        self.output
            .iter()
            .filter_map(|item| match item {
                OutputItem::Message { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Chain of thought. Goes into the internal trace, never a comment body.
    pub fn reasoning_text(&self) -> String {
        self.output
            .iter()
            .filter_map(|item| match item {
                OutputItem::Reasoning { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn function_calls(&self) -> impl Iterator<Item = (&str, &str, &str)> {
        self.output.iter().filter_map(|item| match item {
            OutputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => Some((call_id.as_str(), name.as_str(), arguments.as_str())),
            _ => None,
        })
    }

    /// No message and no tool call, and the vendor stopped because the output
    /// budget was gone. An empty reply that spent nothing is not this: that
    /// is the model saying it found nothing.
    pub fn truncated(&self, max_output_tokens: u32) -> bool {
        if !self.output_text().is_empty() || self.function_calls().next().is_some() {
            return false;
        }
        if self.incomplete.as_deref() == Some("max_output_tokens") {
            return true;
        }
        max_output_tokens > 0 && self.usage.output_tokens >= max_output_tokens
    }
}

pub trait Protocol: Send + Sync {
    fn name(&self) -> &'static str;

    fn send(&self, request: &Request) -> Result<Response, ProtocolError>;
}

/// The registry: a protocol string from `[[provider]]` to an implementation.
pub fn resolve(
    provider: &Provider,
    api_key: Secret,
    backoff: Backoff,
) -> Result<Box<dyn Protocol>, ProtocolError> {
    match provider.protocol.as_str() {
        "openai" => Ok(Box::new(OpenAi::new(
            provider.base_url.clone(),
            api_key,
            backoff,
        ))),
        other => Err(ProtocolError::Unknown {
            protocol: other.to_string(),
            known: KNOWN_PROTOCOLS.join(", "),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_text_ignores_tool_calls_and_reasoning() {
        let response = Response {
            output: vec![
                OutputItem::Reasoning {
                    text: "thinking".to_string(),
                },
                OutputItem::Message {
                    text: r#"{"comments":[]}"#.to_string(),
                },
                OutputItem::FunctionCall {
                    call_id: "c1".to_string(),
                    name: "cppcheck".to_string(),
                    arguments: "{}".to_string(),
                },
            ],
            usage: TokenUsage::default(),
            incomplete: None,
        };
        assert_eq!(response.output_text(), r#"{"comments":[]}"#);
        assert_eq!(response.reasoning_text(), "thinking");
        assert_eq!(response.function_calls().count(), 1);
        assert!(!response.truncated(4096));
    }

    #[test]
    fn a_reasoning_only_reply_that_hits_the_cap_is_truncated() {
        let response = Response {
            output: vec![OutputItem::Reasoning {
                text: "still thinking".to_string(),
            }],
            usage: TokenUsage {
                output_tokens: 4096,
                ..TokenUsage::default()
            },
            incomplete: Some("max_output_tokens".to_string()),
        };
        assert!(response.output_text().is_empty());
        assert!(response.truncated(4096));
        assert!(!Response::default().truncated(4096));
    }

    #[test]
    fn only_the_transient_variant_is_retried() {
        assert!(
            ProtocolError::Transient {
                protocol: "openai",
                reason: "HTTP 503".to_string(),
                status: Some(503),
            }
            .is_transient()
        );
        assert!(
            ProtocolError::Transient {
                protocol: "openai",
                reason: "empty body".to_string(),
                status: Some(200),
            }
            .is_transient()
        );
        assert!(
            !ProtocolError::Fatal {
                protocol: "openai",
                reason: "HTTP 401".to_string(),
                status: Some(401),
            }
            .is_transient()
        );
        assert!(
            !ProtocolError::Fatal {
                protocol: "openai",
                reason: "HTTP 400".to_string(),
                status: Some(400),
            }
            .is_transient()
        );
        assert!(
            !ProtocolError::Malformed {
                protocol: "openai",
                reason: "response has no output".to_string(),
            }
            .is_transient()
        );
        assert!(!ProtocolError::NotImplemented { protocol: "openai" }.is_transient());
    }
}
