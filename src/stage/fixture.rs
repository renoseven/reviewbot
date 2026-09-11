//! A run directory, a frozen budget and a scripted model for the stage
//! tests. Compiled only for tests, so no user config can reach any of it.
//!
//! It exists because some stages need a real run directory to be tested at
//! all: `merge` writes its notes into traces, `publish` reads them back, and
//! `report` writes the artifacts that sit beside them.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::budget::{Budget, Limit, Price, TokenUsage};
use crate::config::{RunOptions, Settings};
use crate::progress::{Event, Progress};
use crate::protocol::{OutputItem, Protocol, ProtocolError, Request, Response};
use crate::record::{LocalStorage, Meta, Recorder, RunIdentity, Storage, layout};
use crate::security::{PathPolicy, Redactor};
use crate::tool::{Registry, SubmitComment};
use crate::worktree::Worktree;

use super::{Adapters, StageContext};

const CONFIG: &str = r#"
[review]
max_files_per_listing = 200
max_hits_per_search = 50
max_files_per_fetch = 20
max_file_bytes = 262144
max_tool_output_bytes = 32768
max_rounds = 100

[triage]
max_chunk_tokens = 24000
skip_files_over_bytes = 262144

[security]
allow_extensions = ["rs", "toml", "c", "h"]

[[provider]]
name = "deepseek"
protocol = "openai"
base_url = "https://api.deepseek.com"
api_key = "DEEPSEEK_API_KEY"
currency = "CNY"
budget_per_run = 10.0

[[model]]
name = "deepseek-v4-flash"
default = true
provider = "deepseek"
input_per_1m_tokens = 2.0
cached_input_per_1m_tokens = 0.2
output_per_1m_tokens = 3.0
context_window_tokens = 131072
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
    /// The next send fails, so a test can stop a stage mid-way and come back.
    Fail,
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
            Reply::Fail => unreachable!("Fail is handled in send, not turned into a response"),
        }
    }
}

/// Replies in the order they were queued. An exhausted queue answers with an
/// empty response, which is what a model that found nothing to say looks
/// like on the wire.
struct ScriptedProtocol {
    replies: Arc<Mutex<VecDeque<Reply>>>,
    sent: Arc<Mutex<Vec<Request>>>,
    /// What every reply reports having used, when a test asked for turns
    /// that cost money. Zero otherwise, which is what most stage tests
    /// want: no budget moves and nothing to reason about.
    billed: Arc<Mutex<TokenUsage>>,
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
        if matches!(reply, Some(Reply::Fail)) {
            return Err(ProtocolError::Fatal {
                protocol: "openai",
                reason: "scripted failure".to_string(),
                status: None,
            });
        }
        let mut response = reply
            .map(|reply| reply.into_response(turn))
            .unwrap_or_default();
        let billed = *self.billed.lock().expect("billed");
        if billed != TokenUsage::default() {
            response.usage = billed;
        }
        Ok(response)
    }
}

pub struct StageFixture {
    _root: tempfile::TempDir,
    settings: Settings,
    adapters: Adapters,
    /// The other half of a real run's adapters, held apart the same way: a
    /// stage test that wants content puts its own worktree and its own tools
    /// in these two slots.
    worktree: Arc<Worktree>,
    tools: Registry,
    recorder: Recorder,
    budget: Budget,
    paths: PathPolicy,
    sent: Arc<Mutex<Vec<Request>>>,
    replies: Arc<Mutex<VecDeque<Reply>>>,
    billed: Arc<Mutex<TokenUsage>>,
    progress: Heard,
}

/// A watcher that keeps what it was told, so a stage test can assert what the
/// stage said as well as what it did.
#[derive(Default)]
pub struct Heard {
    events: Mutex<Vec<Event>>,
}

impl Heard {
    fn heard(&self) -> Vec<Event> {
        self.events.lock().expect("watcher").clone()
    }
}

impl Progress for Heard {
    fn emit(&self, event: Event) {
        self.events.lock().expect("watcher").push(event);
    }
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
        let replies = Arc::new(Mutex::new(replies.into_iter().collect()));
        let billed = Arc::new(Mutex::new(TokenUsage::default()));
        let mut tools = Registry::new();
        tools.register(Box::new(SubmitComment::new()));
        tools.register(Box::new(crate::tool::FinishReview::new()));
        tools.register(Box::new(crate::tool::SubmitSummary::new()));
        let adapters = Adapters {
            platform: None,
            protocol: Box::new(ScriptedProtocol {
                replies: Arc::clone(&replies),
                sent: Arc::clone(&sent),
                billed: Arc::clone(&billed),
            }),
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
        let lock = storage.lock().expect("run directory lock");
        let recorder = Recorder::open(storage, lock, meta).expect("run directory");
        let paths = PathPolicy::new(
            &settings.config.security,
            &settings.written_paths().expect("cwd"),
            None,
        )
        .expect("valid globs");

        Self {
            _root: root,
            settings,
            adapters,
            // Nothing to read: a stage test that wants content says so with
            // `with_cache`.
            worktree: Arc::new(Worktree::Empty),
            tools,
            recorder,
            budget,
            paths,
            sent,
            replies,
            billed,
            progress: Heard::default(),
        }
    }

    pub fn context(&mut self) -> StageContext<'_> {
        StageContext {
            settings: &self.settings,
            adapters: &self.adapters,
            worktree: self.worktree.as_ref(),
            tools: &self.tools,
            recorder: &mut self.recorder,
            budget: &mut self.budget,
            redactor: &self.adapters.redactor,
            paths: &self.paths,
            progress: &self.progress,
        }
    }

    /// What the stage said about itself. Kept here as well as asserted over a
    /// whole run, because some of it is only decidable inside one stage: that
    /// the turn after the last round is not announced as another round, for one.
    pub fn progress(&self) -> Vec<Event> {
        self.progress.heard()
    }

    pub fn recorder(&self) -> &Recorder {
        &self.recorder
    }

    pub fn with_platform(mut self, platform: Box<dyn crate::platform::Platform>) -> Self {
        self.adapters.platform = Some(platform);
        self
    }

    pub fn with_tools(mut self, tools: Registry) -> Self {
        self.tools = tools;
        self
    }

    /// A cache of this fixture's own, in its run directory, with `repo`
    /// behind it. Used where what is being tested is the worktree's part of
    /// the loop rather than the loop itself.
    pub fn with_cache(mut self, repo: crate::platform::Repo) -> Self {
        self.worktree = Arc::new(
            Worktree::open(
                None,
                Some(repo),
                &self.settings.options.runs_dir.join("test-run"),
            )
            .expect("a cache"),
        );
        self
    }

    pub fn worktree(&self) -> &Worktree {
        &self.worktree
    }

    /// Make every scripted turn report this usage, so a test about running
    /// out of money has turns that cost some. Without it a reply is free and
    /// the budget never moves.
    pub fn billing(self, usage: TokenUsage) -> Self {
        *self.billed.lock().expect("billed") = usage;
        self
    }

    /// Replace the frozen prices. A stage test that wants a real model's
    /// output ceiling on a fixture that otherwise uses toy numbers uses this,
    /// rather than rewriting the whole config.
    pub fn with_price(mut self, price: Price) -> Self {
        self.budget = Budget::restore(
            self.budget.limit(),
            self.budget.currency().to_string(),
            price,
            self.budget.spent(),
        );
        self
    }

    /// Shrinks the window the loop measures itself against, so a test can
    /// reach the context stop without scripting a megabyte of tool output.
    pub fn with_context_window(mut self, tokens: u32) -> Self {
        for model in &mut self.settings.config.models {
            model.context_window_tokens = tokens;
            model.max_output_tokens = tokens / 8;
        }
        self
    }

    /// The count ceiling for one file. Tests that need to hit it without
    /// filling the window set a small one here.
    pub fn with_max_rounds(mut self, rounds: u32) -> Self {
        self.settings.config.review.max_rounds = rounds;
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

    /// Queue more replies after a scripted failure, so the same fixture can
    /// pick the stage up again.
    pub fn queue(&self, extra: impl IntoIterator<Item = Reply>) {
        self.replies.lock().expect("replies").extend(extra);
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
