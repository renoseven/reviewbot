//! A run directory, a frozen budget and a scripted model for the stage
//! tests. Compiled only for tests, so no user config can reach any of it.
//!
//! It exists because two stages need a real run directory to be tested at
//! all: `merge` writes its notes into traces, and `publish` reads them back.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::budget::{Budget, Limit, Price, TokenUsage};
use crate::config::{RunOptions, Settings};
use crate::protocol::{OutputItem, Protocol, ProtocolError, Request, Response};
use crate::record::{LocalStorage, Meta, Recorder, RunIdentity, Storage, layout};
use crate::security::{PathPolicy, Redactor};
use crate::tool::{Registry, SubmitComment};

use super::{Adapters, StageContext};

const CONFIG: &str = r#"
[review]
max_tool_rounds = 12
max_files_listed = 200
max_search_hits = 50
max_read_bytes = 262144
max_tool_output_bytes = 32768

[triage]
max_chunk_tokens = 24000
skip_over_bytes = 262144

[security]
allow_extensions = ["rs", "toml", "c", "h"]

[[provider]]
name = "deepseek"
protocol = "openai"
base_url = "https://api.deepseek.com"
api_key = "DEEPSEEK_API_KEY"
currency = "CNY"
budget = 10.0

[[model]]
name = "deepseek-v4-flash"
default = true
provider = "deepseek"
input_per_1m = 2.0
cached_input_per_1m = 0.2
output_per_1m = 3.0
context_window = 131072
max_output_tokens = 4096
"#;

/// One scripted turn. A turn that only calls tools carries no message, which
/// is exactly the shape the review loop has to keep going through.
#[derive(Clone)]
pub enum Reply {
    Message(String),
    /// `(name, arguments)` per call; the fixture numbers the call ids.
    Calls(Vec<(String, String)>),
    /// Text and calls in one turn, which is what a real reply looks like when
    /// the model says something as it submits.
    Saying(String, Vec<(String, String)>),
    /// Empty of message and tool calls; the vendor billed `output_tokens`.
    Truncated {
        output_tokens: u32,
    },
}

impl Reply {
    pub fn calls(calls: &[(&str, &str)]) -> Self {
        Reply::Calls(Self::named(calls))
    }

    pub fn saying(text: &str, calls: &[(&str, &str)]) -> Self {
        Reply::Saying(text.to_string(), Self::named(calls))
    }

    fn named(calls: &[(&str, &str)]) -> Vec<(String, String)> {
        calls
            .iter()
            .map(|(name, arguments)| (name.to_string(), arguments.to_string()))
            .collect()
    }

    fn function_calls(calls: Vec<(String, String)>, turn: usize) -> Vec<OutputItem> {
        calls
            .into_iter()
            .enumerate()
            .map(|(index, (name, arguments))| OutputItem::FunctionCall {
                call_id: format!("call-{turn}-{index}"),
                name,
                arguments,
            })
            .collect()
    }

    fn into_response(self, turn: usize) -> Response {
        match self {
            Reply::Message(text) => Response {
                output: vec![OutputItem::Message { text }],
                usage: TokenUsage::default(),
                incomplete: None,
            },
            Reply::Calls(calls) => Response {
                output: Self::function_calls(calls, turn),
                usage: TokenUsage::default(),
                incomplete: None,
            },
            Reply::Saying(text, calls) => Response {
                output: std::iter::once(OutputItem::Message { text })
                    .chain(Self::function_calls(calls, turn))
                    .collect(),
                usage: TokenUsage::default(),
                incomplete: None,
            },
            Reply::Truncated { output_tokens } => Response {
                output: Vec::new(),
                usage: TokenUsage {
                    output_tokens,
                    ..TokenUsage::default()
                },
                incomplete: Some("max_output_tokens".to_string()),
            },
        }
    }
}

/// Replies in the order they were queued. An exhausted queue answers with an
/// empty response, which is what a model that found nothing to say looks
/// like on the wire.
struct ScriptedProtocol {
    replies: Mutex<VecDeque<Reply>>,
    sent: Arc<Mutex<Vec<Request>>>,
}

impl Protocol for ScriptedProtocol {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn send(&self, request: &Request) -> Result<Response, ProtocolError> {
        let turn = {
            let mut sent = self.sent.lock().expect("sent");
            sent.push(request.clone());
            sent.len()
        };
        let reply = self.replies.lock().expect("replies").pop_front();
        Ok(reply
            .map(|reply| reply.into_response(turn))
            .unwrap_or_default())
    }
}

pub struct StageFixture {
    _root: tempfile::TempDir,
    settings: Settings,
    adapters: Adapters,
    recorder: Recorder,
    budget: Budget,
    paths: PathPolicy,
    sent: Arc<Mutex<Vec<Request>>>,
}

impl StageFixture {
    pub fn new(replies: Vec<&str>) -> Self {
        Self::with_budget(replies, Limit::Amount(10.0))
    }

    pub fn with_budget(replies: Vec<&str>, limit: Limit) -> Self {
        Self::scripted(
            replies
                .into_iter()
                .map(|text| Reply::Message(text.to_string()))
                .collect(),
            limit,
        )
    }

    /// The full surface: turns that call tools as well as turns that talk.
    pub fn scripted(replies: Vec<Reply>, limit: Limit) -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let config_path = root.path().join("reviewbot.toml");
        std::fs::write(&config_path, CONFIG).expect("write config");
        let settings = Settings::load(
            Some(&config_path),
            RunOptions {
                runs_dir: root.path().join("runs"),
                ..RunOptions::default()
            },
        )
        .expect("valid config");

        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut tools = Registry::new();
        tools.register(Box::new(SubmitComment));
        let adapters = Adapters {
            platform: None,
            protocol: Box::new(ScriptedProtocol {
                replies: Mutex::new(replies.into_iter().collect()),
                sent: Arc::clone(&sent),
            }),
            tools,
            worktree: None,
            redactor: Redactor::new(),
        };

        let selection = settings.selection().expect("a selected model");
        let budget = Budget::restore(
            limit,
            selection.provider.currency.clone(),
            Price::from_model(selection.model),
            0.0,
        );
        let storage: Arc<dyn Storage> = Arc::new(
            LocalStorage::create(settings.options.runs_dir.join("test-run"))
                .expect("run directory"),
        );
        let meta = Meta::start(
            RunIdentity {
                run_id: "test-run".to_string(),
                input: crate::record::InputRecord {
                    kind: crate::record::InputKind::Diff,
                    source: "change.diff".to_string(),
                    identity: crate::record::InputIdentity::diff("--- a\n+++ b\n"),
                    head_sha: String::new(),
                },
                fingerprint: settings.fingerprint(),
            },
            &selection.model.name,
            &selection.provider.name,
            &budget,
            false,
        );
        let recorder = Recorder::open(storage, meta).expect("run directory");
        let paths = PathPolicy::new(&settings.config.security, &settings.written_paths(), None)
            .expect("valid globs");

        Self {
            _root: root,
            settings,
            adapters,
            recorder,
            budget,
            paths,
            sent,
        }
    }

    pub fn context(&mut self) -> StageContext<'_> {
        StageContext {
            settings: &self.settings,
            adapters: &self.adapters,
            recorder: &mut self.recorder,
            budget: &mut self.budget,
            redactor: &self.adapters.redactor,
            paths: &self.paths,
        }
    }

    pub fn recorder(&self) -> &Recorder {
        &self.recorder
    }

    pub fn with_platform(mut self, platform: Box<dyn crate::platform::Platform>) -> Self {
        self.adapters.platform = Some(platform);
        self
    }

    pub fn with_tools(mut self, tools: Registry) -> Self {
        self.adapters.tools = tools;
        self
    }

    /// Turns the review loop's own ceiling down, so a test does not have to
    /// script the default number of rounds to reach it.
    pub fn with_max_tool_rounds(mut self, rounds: u32) -> Self {
        self.settings.config.review.max_tool_rounds = rounds;
        self
    }

    pub fn with_max_tool_output_bytes(mut self, bytes: u64) -> Self {
        self.settings.config.review.max_tool_output_bytes = bytes;
        self
    }

    /// Shrinks the window the loop measures itself against, so a test can
    /// reach the context stop without scripting a megabyte of tool output.
    pub fn with_context_window(mut self, tokens: u32) -> Self {
        for model in &mut self.settings.config.models {
            model.context_window = tokens;
            model.max_output_tokens = tokens / 8;
        }
        self
    }

    pub fn enable_publish(&mut self) {
        self.recorder
            .set_publish_intent(true)
            .expect("publish intent");
    }

    /// Every request the scripted model received, in order.
    pub fn sent(&self) -> Vec<Request> {
        self.sent.lock().expect("sent").clone()
    }

    pub fn report(&self) -> String {
        let bytes = self
            .recorder
            .read_artifact(layout::REPORT)
            .expect("readable")
            .expect("the report is always written");
        String::from_utf8(bytes).expect("the report is text")
    }
}
