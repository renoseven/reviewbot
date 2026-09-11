use serde::{Deserialize, Serialize};

use crate::budget::{TokenUsage, estimate_tokens};

/// One call. `instructions` stays byte for byte identical across the run so
/// the vendor's prompt cache can hit; only `input` changes.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Request {
    model: String,
    instructions: String,
    input: Vec<InputItem>,
    tools: Vec<ToolSchema>,
    max_output_tokens: u32,
    /// `reasoning.effort` on the wire. `None` means the field is not sent.
    #[serde(default)]
    reasoning_effort: Option<String>,
}

impl Request {
    pub fn compose(
        model: impl Into<String>,
        instructions: impl Into<String>,
        input: Vec<InputItem>,
        tools: Vec<ToolSchema>,
        max_output_tokens: u32,
    ) -> Self {
        Self {
            model: model.into(),
            instructions: instructions.into(),
            input,
            tools,
            max_output_tokens,
            reasoning_effort: None,
        }
    }

    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort;
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    pub fn input(&self) -> &[InputItem] {
        &self.input
    }

    pub fn tools(&self) -> &[ToolSchema] {
        &self.tools
    }

    pub fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }

    pub fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort.as_deref()
    }

    pub fn set_input(&mut self, input: Vec<InputItem>) {
        self.input = input;
    }

    pub fn push_input(&mut self, item: InputItem) {
        self.input.push(item);
    }

    pub fn set_tools(&mut self, tools: Vec<ToolSchema>) {
        self.tools = tools;
    }

    pub fn cap_output(&mut self, tokens: u32) {
        self.max_output_tokens = tokens;
    }

    pub fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.reasoning_effort = effort;
    }

    /// Everything that goes out, in tokens, for the one check that needs it:
    /// whether the answer still fits inside the context window. The budget
    /// does not use this — it charges input when the vendor bills it, since
    /// how much of this the cache will absorb is not knowable here.
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
