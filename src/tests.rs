//! Crate level tests. The fakes live here rather than in the adapters so a
//! user config can never name one: adapters are injected through `review_with`,
//! which is crate visible.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::config::{PlatformKind, RunOptions, Settings};
use crate::domain::{Confidence, Stage};
use crate::platform::{
    Capabilities, ChangeRef, DiffRefs, ExistingComment, LineRange, Listing, OutgoingComment,
    Platform, PlatformChange, PlatformError, RepoSource, SearchHit,
};
use crate::progress::{Event, Outcome, Progress, Silent};
use crate::protocol::{OutputItem, Protocol, ProtocolError, Request, Response};
use crate::record::{LocalStorage, Runs, Storage, layout};
use crate::security::Redactor;
use crate::stage::Adapters;
use crate::tool::{SubmitComment, SubmitSummary};
use crate::{Error, RunResult, Source};

const CONFIG: &str = r#"
[review]
max_files_per_listing = 200
max_hits_per_search = 50
max_files_per_fetch = 20
max_file_bytes = 262144
max_tool_output_bytes = 32768
max_rounds = 100

[plan]
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

[[model]]
name = "deepseek-v4-pro"
provider = "deepseek"
input_per_1m_tokens = 4.0
output_per_1m_tokens = 12.0
context_window_tokens = 131072
max_output_tokens = 8192

[[platform]]
base_url = "https://gitlab.com/api/v4"
api_token = "GITLAB_TOKEN"
"#;

const URL: &str = "https://gitlab.com/acme/app/-/merge_requests/128";
const HEAD_SHA: &str = "4b1e0d2c0000000000000000000000000000abcd";

/// How often each fake was asked for something, so a second run over the same
/// input can prove it did not repeat work.
#[derive(Default)]
struct Calls {
    head_sha: AtomicUsize,
    fetch_change: AtomicUsize,
    send: AtomicUsize,
    existing_comments: AtomicUsize,
    post_comments: AtomicUsize,
}

impl Calls {
    fn get(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::SeqCst)
    }
}

/// The merge request itself: what a reader would see on it. It outlives the
/// adapters, because a run entered a second time builds its own and still has
/// to meet the comments the first entry left behind.
#[derive(Default)]
struct Mr {
    comments: Mutex<Vec<ExistingComment>>,
}

impl Mr {
    fn len(&self) -> usize {
        self.comments.lock().expect("mr").len()
    }
}

struct FakePlatform {
    calls: Arc<Calls>,
    repo: Arc<FakeRepo>,
    mr: Arc<Mr>,
    /// What `fetch_change` hands back. Empty by default: most of the URL
    /// tests are about locking and fingerprints and want no chunks at all.
    change: PlatformChange,
}

impl FakePlatform {
    fn new(calls: Arc<Calls>) -> Self {
        Self {
            calls,
            repo: Arc::new(FakeRepo::default()),
            mr: Arc::new(Mr::default()),
            change: PlatformChange {
                head_sha: HEAD_SHA.to_string(),
                base_sha: Some("base".to_string()),
                start_sha: Some("start".to_string()),
                ..PlatformChange::default()
            },
        }
    }

    /// A platform that returns a real diff and a real description, for the
    /// one thing only a URL run has: what the author wrote about the change.
    fn narrated(calls: Arc<Calls>) -> Self {
        let mut platform = Self::new(calls);
        platform.change.diff = DIFF.to_string();
        platform.change.narrative = crate::domain::Narrative {
            title: Some("bound the parser index".to_string()),
            description: Some("fixes the overflow reported in #12".to_string()),
            commits: vec!["bound the index".to_string()],
            more_commits: false,
        };
        platform
    }
}

impl Platform for FakePlatform {
    fn kind(&self) -> PlatformKind {
        PlatformKind::Gitlab
    }

    fn host(&self) -> &str {
        "gitlab.com"
    }

    fn repo(&self) -> crate::platform::Repo {
        crate::platform::Repo::new(
            Arc::clone(&self.repo) as Arc<dyn RepoSource>,
            Capabilities::default(),
        )
    }

    fn cached_body(&self, _path: &str) -> Option<String> {
        None
    }

    fn head_sha(&self, _change: &ChangeRef) -> Result<String, PlatformError> {
        self.calls.head_sha.fetch_add(1, Ordering::SeqCst);
        Ok(HEAD_SHA.to_string())
    }

    fn fetch_change(&self, _change: &ChangeRef) -> Result<PlatformChange, PlatformError> {
        self.calls.fetch_change.fetch_add(1, Ordering::SeqCst);
        Ok(self.change.clone())
    }

    fn existing_comments(
        &self,
        _change: &ChangeRef,
    ) -> Result<Vec<ExistingComment>, PlatformError> {
        self.calls.existing_comments.fetch_add(1, Ordering::SeqCst);
        Ok(self.mr.comments.lock().expect("mr").clone())
    }

    fn post_comments(
        &self,
        _change: &ChangeRef,
        _refs: &DiffRefs,
        comments: &[OutgoingComment],
    ) -> Result<Vec<ExistingComment>, PlatformError> {
        self.calls.post_comments.fetch_add(1, Ordering::SeqCst);
        let posted: Vec<ExistingComment> = comments
            .iter()
            .map(|comment| ExistingComment {
                marker: comment.marker.clone(),
                url: None,
                degraded_to_file: false,
            })
            .collect();
        self.mr
            .comments
            .lock()
            .expect("mr")
            .extend(posted.iter().cloned());
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

#[derive(Default)]
struct FakeRepo {
    /// Bodies `read_file` hands back. Empty unless a test plants them, so a
    /// checkout run can see a repository answer that is not the file on disk
    /// — the canary that a write into the checkout would leave behind.
    files: BTreeMap<String, String>,
}

impl FakeRepo {
    fn holding(files: &[(&str, &str)]) -> Self {
        Self {
            files: files
                .iter()
                .map(|(path, body)| ((*path).to_string(), (*body).to_string()))
                .collect(),
        }
    }
}

impl RepoSource for FakeRepo {
    fn list_files(&self, _glob: &str) -> Result<Listing, PlatformError> {
        Ok(Listing {
            files: Vec::new(),
            complete: true,
        })
    }

    fn read_file(&self, path: &str, _lines: Option<LineRange>) -> Result<String, PlatformError> {
        Ok(self.files.get(path).cloned().unwrap_or_default())
    }

    fn size(&self, path: &str) -> Result<u64, PlatformError> {
        Ok(self.files.get(path).map(String::len).unwrap_or(0) as u64)
    }

    fn search(
        &self,
        _kind: crate::platform::SearchKind,
        _query: &str,
        _glob: Option<&str>,
    ) -> Result<Vec<SearchHit>, PlatformError> {
        Ok(Vec::new())
    }
}

struct FakeProtocol {
    calls: Arc<Calls>,
    requests: Arc<Mutex<Vec<Request>>>,
    /// Replies in the order they were queued. An empty queue answers with an
    /// empty response, which is what most of these tests want.
    replies: Mutex<VecDeque<String>>,
}

impl FakeProtocol {
    fn new(calls: Arc<Calls>, requests: Arc<Mutex<Vec<Request>>>) -> Self {
        Self::scripted(calls, requests, Vec::new())
    }

    fn scripted(calls: Arc<Calls>, requests: Arc<Mutex<Vec<Request>>>, replies: Vec<&str>) -> Self {
        Self {
            calls,
            requests,
            replies: Mutex::new(replies.iter().map(|text| text.to_string()).collect()),
        }
    }
}

impl Protocol for FakeProtocol {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn send(&self, request: &Request) -> Result<Response, ProtocolError> {
        self.calls.send.fetch_add(1, Ordering::SeqCst);
        self.requests
            .lock()
            .expect("requests")
            .push(request.clone());
        let reply = self.replies.lock().expect("replies").pop_front();
        Ok(Response {
            output: match reply {
                Some(text) => reply_as_calls(&text, request)
                    .unwrap_or_else(|| vec![crate::protocol::OutputItem::Message { text }]),
                None => Vec::new(),
            },
            usage: crate::budget::TokenUsage::default(),
            incomplete: None,
        })
    }
}

/// The end-to-end fake queues JSON documents, which is what makes those
/// tests readable. Both submissions are function calls now, so a document
/// becomes whichever call the request advertises: a comments document
/// becomes one `submit_comment` per finding, a score document becomes one
/// `submit_summary`, and `{"tool": ...}` becomes that one call. A document
/// the request has no tool for stays a message, which is how the "it only
/// talked" paths are exercised.
fn reply_as_calls(text: &str, request: &Request) -> Option<Vec<OutputItem>> {
    let advertised = |name: &str| request.tools().iter().any(|tool| tool.name == name);
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("overall_score").is_some() {
        return advertised(SubmitSummary::NAME).then(|| {
            vec![OutputItem::FunctionCall {
                call_id: "submit-summary".to_string(),
                name: SubmitSummary::NAME.to_string(),
                arguments: text.to_string(),
            }]
        });
    }
    if let Some(name) = value.get("tool").and_then(|name| name.as_str()) {
        return advertised(name).then(|| {
            let arguments = match value.get("arguments") {
                Some(args) => args.to_string(),
                None => "{}".to_string(),
            };
            vec![OutputItem::FunctionCall {
                call_id: format!("call-{name}"),
                name: name.to_string(),
                arguments,
            }]
        });
    }
    if !advertised(SubmitComment::NAME) {
        return None;
    }
    let comments = value.get("comments")?.as_array()?;
    let mut calls: Vec<OutputItem> = comments
        .iter()
        .enumerate()
        .map(|(index, comment)| OutputItem::FunctionCall {
            call_id: format!("submit-{index}"),
            name: SubmitComment::NAME.to_string(),
            arguments: comment.to_string(),
        })
        .collect();
    // A comments document is the whole answer for that file. Filing is not
    // the end signal; the fake has to send it, or the loop asks again.
    if advertised(crate::tool::FinishReview::NAME) {
        calls.push(OutputItem::FunctionCall {
            call_id: "finish-review".to_string(),
            name: crate::tool::FinishReview::NAME.to_string(),
            arguments: "{}".to_string(),
        });
    }
    Some(calls)
}

/// The half of the adapters a run has before it has a directory. The
/// worktree and the tools are the run's own to build, once it is locked: a
/// URL run opens a cache in its run directory with this fake platform behind
/// it, and a diff run has nothing to read at all.
fn adapters(calls: Arc<Calls>) -> Adapters {
    Adapters::for_test(
        Some(Box::new(FakePlatform::new(Arc::clone(&calls)))),
        Box::new(FakeProtocol::new(calls, Arc::new(Mutex::new(Vec::new())))),
        Redactor::new().expect("patterns"),
    )
}

/// A URL run that really has something to say: a diff behind the change and a
/// merge request the caller holds on to, so two entries into the same run see
/// one MR rather than two.
fn publishing_adapters(calls: Arc<Calls>, mr: Arc<Mr>, replies: Vec<&str>) -> Adapters {
    let mut platform = FakePlatform::new(Arc::clone(&calls));
    platform.change.diff = DIFF.to_string();
    platform.mr = mr;
    Adapters::for_test(
        Some(Box::new(platform)),
        Box::new(FakeProtocol::scripted(
            calls,
            Arc::new(Mutex::new(Vec::new())),
            replies,
        )),
        Redactor::new().expect("patterns"),
    )
}

/// A URL run whose repository answers with a body that is not what the
/// checkout holds, and whose model is scripted to call tools. The mismatch
/// is the canary: a write into the checkout would leave the repository body
/// behind.
fn writing_adapters(calls: Arc<Calls>, replies: Vec<&str>) -> Adapters {
    let mut platform = FakePlatform::new(Arc::clone(&calls));
    platform.change.diff = DIFF.to_string();
    platform.repo = Arc::new(FakeRepo::holding(&[("src/parse.c", REPO_PARSE)]));
    Adapters::for_test(
        Some(Box::new(platform)),
        Box::new(FakeProtocol::scripted(
            calls,
            Arc::new(Mutex::new(Vec::new())),
            replies,
        )),
        Redactor::new().expect("patterns"),
    )
}

/// A checker that writes a marker into cwd and prints the file it was
/// pointed at. The write is the thing a checkout must not receive.
const CHECKER: &str = r#"
[[tool]]
name = "mark"
description = "drop a marker in cwd and print the file"
bin = "/bin/sh"
args = ["-c", "printf dropped > marker; /bin/cat \"$1\"", "mark", "{path}"]
params.path = { type = "path", description = "file to print" }
requires_checkout = false
"#;

const REPO_PARSE: &str = "I-AM-FROM-THE-REPO\n";
const CHECKOUT_PARSE: &str = "I-AM-FROM-THE-CHECKOUT\n";
const CHECKOUT_KEEP: &str = "do-not-touch\n";
const FETCH_CALL: &str = r#"{"tool":"fetch_repo_file","arguments":{"paths":["src/parse.c"]}}"#;
const MARK_CALL: &str = r#"{"tool":"mark","arguments":{"path":"src/parse.c"}}"#;

fn with_checker(config: &str) -> String {
    format!(
        "{}\n{CHECKER}",
        config.replace("[plan]", "[plan]\nskip_paths = [\"vendor/**\"]")
    )
}

fn planted_checkout(root: &Path) -> PathBuf {
    let checkout = root.join("checkout");
    std::fs::create_dir_all(checkout.join("src")).expect("src");
    std::fs::create_dir_all(checkout.join(".git")).expect("git");
    std::fs::write(checkout.join("src/parse.c"), CHECKOUT_PARSE).expect("parse.c");
    std::fs::write(checkout.join("src/keep.c"), CHECKOUT_KEEP).expect("keep.c");
    std::fs::write(checkout.join(".git/HEAD"), format!("{HEAD_SHA}\n")).expect("HEAD");
    checkout
}

/// Every path under `root`. A directory is `None`; a regular file is its
/// bytes. A new empty directory still shows up, which a file-only walk
/// would miss.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut tree = BTreeMap::new();
    walk_snapshot(root, root, &mut tree);
    tree
}

fn walk_snapshot(root: &Path, dir: &Path, tree: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let entry = entry.expect("entry");
        let path = entry.path();
        let relative = path.strip_prefix(root).expect("under root").to_path_buf();
        let file_type = entry.file_type().expect("type");
        if file_type.is_dir() {
            tree.insert(relative, None);
            walk_snapshot(root, &path, tree);
        } else if file_type.is_file() {
            tree.insert(relative, Some(std::fs::read(&path).expect("read file")));
        } else {
            tree.insert(
                relative,
                Some(format!("not-a-regular-file:{file_type:?}").into_bytes()),
            );
        }
    }
}

fn named_tools(watcher: &Watcher) -> Vec<String> {
    watcher
        .events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Tool { name } => Some(name),
            _ => None,
        })
        .collect()
}

struct Workspace {
    root: tempfile::TempDir,
    config_path: PathBuf,
    runs_dir: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        Self::with_config(CONFIG)
    }

    fn with_config(config: &str) -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let config_path = root.path().join("reviewbot.toml");
        std::fs::write(&config_path, config).expect("write config");
        let runs_dir = root.path().join("runs");
        Self {
            root,
            config_path,
            runs_dir,
        }
    }

    /// A diff on disk. A diff identifies a run by its content, so a run that
    /// is going to be re-entered needs a file that is still there to be read
    /// the second time round.
    fn diff_file(&self) -> Source {
        let path = self.root.path().join("change.diff");
        std::fs::write(&path, DIFF).expect("write diff");
        Source::Diff {
            origin: path.display().to_string(),
            content: DIFF.to_string(),
        }
    }

    fn settings(&self) -> Settings {
        self.settings_with(RunOptions {
            runs_dir: self.runs_dir.clone(),
            ..RunOptions::default()
        })
    }

    fn settings_with(&self, options: RunOptions) -> Settings {
        Settings::load(Some(&self.config_path), options).expect("valid config")
    }

    fn run_dir(&self, run_id: &str) -> PathBuf {
        self.runs_dir.join(run_id)
    }
}

fn review(workspace: &Workspace, calls: Arc<Calls>) -> Result<RunResult, Error> {
    crate::review_with(
        &workspace.settings(),
        &Source::Url(URL.to_string()),
        &adapters(calls),
        &Silent,
    )
}

fn stage_paths(run_dir: &Path) -> Vec<PathBuf> {
    Stage::ALL
        .into_iter()
        .map(|stage| run_dir.join(layout::stage_file(stage)))
        .collect()
}

/// Cut a finished run back to the first `keep` stages, the way an interrupted
/// process would have left it.
fn rewind_to(run_dir: &Path, keep: usize) {
    let meta_path = run_dir.join(layout::META);
    let mut meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&meta_path).expect("meta")).expect("meta json");
    // How far the run got is one value, so rewinding is naming the last stage
    // that survives rather than listing the ones that do.
    meta["completed_through"] = match keep {
        0 => serde_json::Value::Null,
        keep => serde_json::Value::String(Stage::ALL[keep - 1].name().to_string()),
    };
    std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta).unwrap()).expect("write meta");
    for path in stage_paths(run_dir).into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn a_full_run_writes_every_stage_and_releases_the_lock() {
    let workspace = Workspace::new();
    let calls = Arc::new(Calls::default());
    let result = review(&workspace, Arc::clone(&calls)).expect("run completes");

    let run_dir = workspace.run_dir(&result.run_id);
    assert!(run_dir.join(layout::META).is_file(), "meta.json is written");
    for path in stage_paths(&run_dir) {
        assert!(path.is_file(), "{} is written", path.display());
    }
    assert!(
        run_dir.join(layout::REPORT).is_file(),
        "the report is always written"
    );
    assert!(run_dir.join(layout::SUMMARY).is_file());
    assert!(run_dir.join(layout::PUBLISHED).is_file());
    // The `lock` file outlives the run that wrote it, and says nothing: the
    // next holder walks straight in.
    assert!(run_dir.join(layout::LOCK).is_file());
    LocalStorage::open(run_dir.clone())
        .lock()
        .expect("the finished run let go of the lock");

    assert!(result.comments.is_empty());
    assert_eq!(
        result.overall_score, None,
        "an unscored run reports null, never 0"
    );
    assert!(result.unscored_reason.is_some());
    assert_eq!(Calls::get(&calls.fetch_change), 1);
}

/// The same command again is the only way back into a run, so it has to walk
/// into the one it started rather than open a second one beside it.
#[test]
fn the_same_command_again_leaves_a_finished_input_stage_alone() {
    let workspace = Workspace::new();
    let first = review(&workspace, Arc::new(Calls::default())).expect("run completes");
    let run_dir = workspace.run_dir(&first.run_id);
    let input_file = run_dir.join(layout::stage_file(Stage::Input));
    let input_before = std::fs::read(&input_file).expect("input checkpoint");

    rewind_to(&run_dir, 1);
    assert!(!run_dir.join(layout::stage_file(Stage::Plan)).exists());

    let calls = Arc::new(Calls::default());
    let second = review(&workspace, Arc::clone(&calls)).expect("the second run completes");

    assert_eq!(second.run_id, first.run_id);
    assert_eq!(
        Calls::get(&calls.fetch_change),
        0,
        "the input stage is not run again"
    );
    assert_eq!(
        std::fs::read(&input_file).unwrap(),
        input_before,
        "the input checkpoint is untouched"
    );
    for path in stage_paths(&run_dir) {
        assert!(path.is_file(), "{} is back", path.display());
    }
}

/// The stage that costs money is the one that must not be repeated: a run
/// re-entered past `review` sends nothing to the model.
#[test]
fn the_same_command_again_leaves_the_first_three_stages_alone() {
    let workspace = Workspace::new();
    let first = review(&workspace, Arc::new(Calls::default())).expect("run completes");
    let run_dir = workspace.run_dir(&first.run_id);
    let untouched: Vec<Vec<u8>> = stage_paths(&run_dir)
        .iter()
        .take(3)
        .map(|path| std::fs::read(path).expect("checkpoint"))
        .collect();

    rewind_to(&run_dir, 3);

    let calls = Arc::new(Calls::default());
    review(&workspace, Arc::clone(&calls)).expect("the second run completes");

    assert_eq!(Calls::get(&calls.fetch_change), 0);
    assert_eq!(Calls::get(&calls.send), 0, "no model call is repeated");
    for (path, before) in stage_paths(&run_dir).iter().take(3).zip(untouched) {
        assert_eq!(
            std::fs::read(path).unwrap(),
            before,
            "{} is untouched",
            path.display()
        );
    }
}

#[test]
fn an_unreadable_checkpoint_falls_back_to_the_previous_snapshot() {
    let workspace = Workspace::new();
    let first = review(&workspace, Arc::new(Calls::default())).expect("run completes");
    let run_dir = workspace.run_dir(&first.run_id);
    std::fs::write(
        run_dir.join(layout::stage_file(Stage::Merge)),
        b"{ not json",
    )
    .expect("corrupt the merge checkpoint");

    let second = review(&workspace, Arc::new(Calls::default())).expect("the second run completes");

    assert_eq!(second.run_id, first.run_id);
    let merged: serde_json::Value = serde_json::from_slice(
        &std::fs::read(run_dir.join(layout::stage_file(Stage::Merge))).unwrap(),
    )
    .expect("the merge stage ran again and wrote valid json");
    assert!(merged.get("comments").is_some());
}

#[test]
fn a_second_process_on_the_same_run_directory_fails_at_once() {
    let workspace = Workspace::new();
    let options = RunOptions {
        runs_dir: workspace.runs_dir.clone(),
        run_id: Some("fixed-run".to_string()),
        ..RunOptions::default()
    };
    let settings = workspace.settings_with(options);
    let run_dir = workspace.run_dir("fixed-run");
    let storage: Arc<dyn Storage> =
        Arc::new(LocalStorage::create(run_dir.clone()).expect("run directory"));
    let held = storage.lock().expect("first lock");

    let error = crate::review_with(
        &settings,
        &Source::Url(URL.to_string()),
        &adapters(Arc::new(Calls::default())),
        &Silent,
    )
    .expect_err("the second open fails");
    assert!(
        error.to_string().contains("another process is running"),
        "got {error}"
    );
    assert!(
        !run_dir.join(crate::worktree::DIRECTORY).exists(),
        "the lock comes before the worktree, so the loser wrote nothing"
    );

    drop(held);
    LocalStorage::open(run_dir)
        .lock()
        .expect("the holder let go, so the next one gets in");
}

/// Writing needs the root-and-repo pair only `Worktree::Cache` holds. A
/// command-line checkout never has that pair, so a run that actually tries
/// to write — the model calls `fetch_repo_file`, and a checker is pointed
/// at a file — must leave the checkout's listing and every file's bytes
/// exactly as they were.
#[test]
fn a_run_over_a_checkout_leaves_every_file_byte_for_byte() {
    let workspace = Workspace::with_config(&with_checker(CONFIG));
    let checkout = planted_checkout(workspace.root.path());
    let before = snapshot(&checkout);
    let settings = workspace.settings_with(RunOptions {
        runs_dir: workspace.runs_dir.clone(),
        worktree: Some(checkout.clone()),
        ..RunOptions::default()
    });
    let watcher = Watcher::default();

    let result = crate::review_with(
        &settings,
        &Source::Url(URL.to_string()),
        &writing_adapters(
            Arc::new(Calls::default()),
            vec![FETCH_CALL, MARK_CALL, FINDING, SCORE],
        ),
        &watcher,
    )
    .expect("the run completes");

    let tools = named_tools(&watcher);
    assert!(
        tools.iter().any(|name| name == "fetch_repo_file"),
        "the model has to have called fetch_repo_file: {tools:?}"
    );
    assert!(
        tools.iter().any(|name| name == "mark"),
        "the checker has to have been pointed at a file: {tools:?}"
    );
    assert_eq!(
        snapshot(&checkout),
        before,
        "the checkout listing and every file's bytes are unchanged"
    );
    let run_dir = workspace.run_dir(&result.run_id);
    assert!(
        !run_dir.join(layout::CACHE).exists(),
        "a checkout run does not open a cache, so there is nowhere a write could land"
    );
    assert!(
        run_dir.join(layout::CHECKS).join("marker").is_file(),
        "the checker's write went to the run directory, not the checkout"
    );
}

/// `cache/` and `checks/` live inside the run directory, so `run prune`
/// taking the directory takes the fetched files and the checker scratch
/// with it. They cannot outlive the run.
#[test]
fn prune_takes_the_cache_and_the_checks_with_the_run() {
    let workspace = Workspace::with_config(&with_checker(CONFIG));
    let result = crate::review_with(
        &workspace.settings(),
        &Source::Url(URL.to_string()),
        &writing_adapters(Arc::new(Calls::default()), vec![MARK_CALL, FINDING, SCORE]),
        &Silent,
    )
    .expect("the run completes");

    let run_dir = workspace.run_dir(&result.run_id);
    let cache = run_dir.join(layout::CACHE);
    let checks = run_dir.join(layout::CHECKS);
    assert!(cache.is_dir(), "a URL run opens a cache");
    assert!(
        cache.join("src/parse.c").is_file(),
        "the file under review was fetched into the cache"
    );
    assert!(
        checks.join("marker").is_file(),
        "the checker wrote into checks/"
    );

    let report = Runs::open(&workspace.runs_dir)
        .prune(0, false)
        .expect("pruned");
    assert_eq!(report.deleted, vec![result.run_id.clone()]);
    assert!(!run_dir.exists(), "the run directory itself is gone");
    assert!(!cache.exists(), "the fetched files cannot outlive the run");
    assert!(
        !checks.exists(),
        "the checker scratch cannot outlive the run"
    );
}

/// Which stages this entry read back rather than ran, in stage order. What
/// a config change costs is exactly this list, so it is what these tests
/// assert on rather than file timestamps.
fn from_checkpoint(watcher: &Watcher) -> Vec<bool> {
    watcher.stages().iter().map(|(_, _, done)| *done).collect()
}

/// A diff run, which is the shape with real chunks behind it: the model is
/// asked twice for a first entry and must be asked again for whatever a
/// config change put back on the table.
fn enter_diff_run(
    workspace: &Workspace,
    settings: &Settings,
    progress: &dyn Progress,
    calls: Arc<Calls>,
) -> RunResult {
    crate::review_with(
        settings,
        &workspace.diff_file(),
        &diff_adapters(calls),
        progress,
    )
    .expect("the run completes")
}

/// The settings are not in the run id, so a config change walks back into
/// the run it already has and drops only what the change can reach.
/// `[plan]` is first read by plan, so the input checkpoint still answers
/// the question it was written for and is used again.
#[test]
fn a_changed_plan_table_reruns_from_plan_in_the_same_run() {
    let workspace = Workspace::new();
    let fresh = Arc::new(Calls::default());
    let first = enter_diff_run(
        &workspace,
        &workspace.settings(),
        &Silent,
        Arc::clone(&fresh),
    );

    std::fs::write(
        &workspace.config_path,
        CONFIG.replace("max_chunk_tokens = 24000", "max_chunk_tokens = 12000"),
    )
    .expect("rewrite config");

    let watcher = Watcher::default();
    let calls = Arc::new(Calls::default());
    let second = enter_diff_run(
        &workspace,
        &workspace.settings(),
        &watcher,
        Arc::clone(&calls),
    );

    assert_eq!(
        second.run_id, first.run_id,
        "no second directory, so no orphan"
    );
    assert_eq!(
        from_checkpoint(&watcher),
        vec![true, false, false, false, false, false],
        "input survived; plan and everything after it ran again"
    );
    assert_eq!(
        Calls::get(&calls.send),
        Calls::get(&fresh.send),
        "the files were really reviewed again, not read back off a snapshot \
         written under the old settings"
    );
}

/// One stage later, and the two before it are untouched: `[review]` is read
/// by nothing earlier than review.
#[test]
fn a_changed_review_table_leaves_input_and_plan_alone() {
    let workspace = Workspace::new();
    let fresh = Arc::new(Calls::default());
    let first = enter_diff_run(
        &workspace,
        &workspace.settings(),
        &Silent,
        Arc::clone(&fresh),
    );

    std::fs::write(
        &workspace.config_path,
        CONFIG.replace("max_files_per_fetch = 20", "max_files_per_fetch = 5"),
    )
    .expect("rewrite config");

    let watcher = Watcher::default();
    let calls = Arc::new(Calls::default());
    let second = enter_diff_run(
        &workspace,
        &workspace.settings(),
        &watcher,
        Arc::clone(&calls),
    );

    assert_eq!(second.run_id, first.run_id);
    assert_eq!(
        from_checkpoint(&watcher),
        vec![true, true, false, false, false, false],
    );
    assert_eq!(
        Calls::get(&calls.send),
        Calls::get(&fresh.send),
        "and review really ran again"
    );
}

/// Three things that look like changes and are not: asking for debug
/// logging must not cost a run its checkpoints, and neither `--publish` nor
/// `--retries` says anything about what the review will conclude. The last
/// two are why "run once to read the report, then run again with
/// `--publish`" pays the model once.
#[test]
fn debug_logging_publishing_and_retries_invalidate_nothing() {
    let workspace = Workspace::new();
    let mr = Arc::new(Mr::default());
    let first = crate::review_with(
        &workspace.settings(),
        &Source::Url(URL.to_string()),
        &publishing_adapters(Arc::new(Calls::default()), Arc::clone(&mr), Vec::new()),
        &Silent,
    )
    .expect("run completes");

    std::fs::write(
        &workspace.config_path,
        format!("{CONFIG}\n[log]\nlevel = \"debug\"\n"),
    )
    .expect("rewrite config");
    let settings = workspace.settings_with(RunOptions {
        runs_dir: workspace.runs_dir.clone(),
        retries: 7,
        publish: true,
        ..RunOptions::default()
    });

    let watcher = Watcher::default();
    let calls = Arc::new(Calls::default());
    let second = crate::review_with(
        &settings,
        &Source::Url(URL.to_string()),
        &publishing_adapters(Arc::clone(&calls), Arc::clone(&mr), Vec::new()),
        &watcher,
    )
    .expect("the same run continues");

    assert_eq!(second.run_id, first.run_id);
    assert_eq!(
        from_checkpoint(&watcher),
        vec![true, true, true, true, false, false],
        "the four paid stages all came back off their checkpoints"
    );
    assert_eq!(Calls::get(&calls.send), 0, "the model was not asked again");
    assert_eq!(
        Calls::get(&calls.existing_comments),
        1,
        "publish still ran, which is the point of asking for it on a second entry"
    );
}

/// Pinning the id is the one way to aim a run at a directory that has
/// nothing to do with this change, and it is refused there rather than
/// allowed to carry on from another change's checkpoints.
#[test]
fn an_explicit_run_id_pointed_at_another_input_fails() {
    let workspace = Workspace::new();
    let first = review(&workspace, Arc::new(Calls::default())).expect("run completes");

    let settings = workspace.settings_with(RunOptions {
        runs_dir: workspace.runs_dir.clone(),
        run_id: Some(first.run_id.clone()),
        ..RunOptions::default()
    });
    let error = crate::review_with(
        &settings,
        &workspace.diff_file(),
        &diff_adapters(Arc::new(Calls::default())),
        &Silent,
    )
    .expect_err("that directory records another input");

    assert!(matches!(error, Error::DifferentInput { .. }), "got {error}");
    assert_eq!(error.exit_code(), 2);
    assert_eq!(error.run_id(), Some(first.run_id.as_str()));
    assert!(
        error.to_string().contains("acme/app #128"),
        "it names what the directory does record: {error}"
    );
    for path in stage_paths(&workspace.run_dir(&first.run_id)) {
        assert!(path.is_file(), "{} was not written over", path.display());
    }
}

/// A `meta.json` nobody can read is a broken run, not a fatal one. Nothing
/// in it can be trusted — not the fingerprint that says which checkpoints
/// still answer the question, and not `spent`, which an unreadable file
/// cannot yield anyway — so `review` starts over in the same directory with
/// the budget frozen again, and the two read-only commands still answer.
#[test]
fn an_unreadable_meta_starts_a_fresh_run_and_still_lists() {
    let workspace = Workspace::new();
    let first = review(&workspace, Arc::new(Calls::default())).expect("run completes");
    let run_dir = workspace.run_dir(&first.run_id);
    std::fs::write(run_dir.join(layout::META), b"{ not json").expect("corrupt meta");

    let rows = Runs::open(&workspace.runs_dir)
        .list()
        .expect("run list still answers");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].run_id, first.run_id, "the directory is still named");
    assert!(
        rows[0].completed_stages.is_empty(),
        "and claims nothing it could not read"
    );
    let show = Runs::open(&workspace.runs_dir)
        .show(&first.run_id)
        .expect("run show still answers");
    assert_eq!(show.run_id, first.run_id);
    assert_eq!(show.spent, 0.0);

    let watcher = Watcher::default();
    let calls = Arc::new(Calls::default());
    let second = crate::review_with(
        &workspace.settings(),
        &Source::Url(URL.to_string()),
        &adapters(Arc::clone(&calls)),
        &watcher,
    )
    .expect("a fresh run in the same directory");

    assert_eq!(second.run_id, first.run_id);
    assert_eq!(
        from_checkpoint(&watcher),
        vec![false; 6],
        "every stage ran again, because none of them could be vouched for"
    );
    assert_eq!(Calls::get(&calls.fetch_change), 1);
    assert_eq!(
        second.budget,
        Some(10.0),
        "the ceiling was frozen a second time"
    );
    assert_eq!(second.spent, 0.0);
}

/// What each command line option puts in which slice, over the wiring the
/// unit tests in `config` cannot reach.
#[test]
fn run_parameters_do_not_change_identity_but_conclusions_do() {
    let workspace = Workspace::new();
    let baseline = workspace.settings().fingerprint().expect("serializable");

    let noisy = workspace.settings_with(RunOptions {
        runs_dir: workspace.runs_dir.join("elsewhere"),
        output_dir: Some(PathBuf::from("artifacts")),
        retries: 7,
        publish: true,
        ..RunOptions::default()
    });
    assert_eq!(
        noisy.fingerprint().expect("serializable"),
        baseline,
        "--retries, --publish and artifact locations stay out of every slice"
    );

    let other_model = workspace.settings_with(RunOptions {
        model: Some("deepseek-v4-pro".to_string()),
        ..RunOptions::default()
    });
    assert_eq!(
        baseline.earliest_change(&other_model.fingerprint().expect("serializable")),
        Some(Stage::Review),
        "--model is first read by review"
    );

    let with_worktree = workspace.settings_with(RunOptions {
        worktree: Some(PathBuf::from("/tmp/checkout")),
        ..RunOptions::default()
    });
    assert_eq!(
        baseline.earliest_change(&with_worktree.fingerprint().expect("serializable")),
        Some(Stage::Input),
        "the content source mode is settled by input"
    );
}

#[test]
fn publishing_a_raw_diff_fails_before_the_run_directory_exists() {
    let workspace = Workspace::new();
    let settings = workspace.settings_with(RunOptions {
        runs_dir: workspace.runs_dir.clone(),
        publish: true,
        ..RunOptions::default()
    });
    let source = Source::Diff {
        origin: "x.diff".to_string(),
        content: "--- a\n+++ b\n".to_string(),
    };

    let error = crate::review_with(
        &settings,
        &source,
        &adapters(Arc::new(Calls::default())),
        &Silent,
    )
    .expect_err("--publish needs a platform");
    assert!(matches!(error, Error::PublishNeedsPlatform));
    assert_eq!(error.exit_code(), 2);
    assert!(!workspace.runs_dir.exists(), "nothing was written");
}

#[test]
fn a_diff_run_completes_without_a_platform() {
    let workspace = Workspace::new();
    let source = Source::Diff {
        origin: "x.diff".to_string(),
        content: "--- a/src/parse.c\n+++ b/src/parse.c\n".to_string(),
    };
    let result = crate::review_with(
        &workspace.settings(),
        &source,
        &Adapters::for_test(
            None,
            Box::new(FakeProtocol::new(
                Arc::new(Calls::default()),
                Arc::new(Mutex::new(Vec::new())),
            )),
            Redactor::new().expect("patterns"),
        ),
        &Silent,
    )
    .expect("run completes");

    let run_dir = workspace.run_dir(&result.run_id);
    for path in stage_paths(&run_dir) {
        assert!(path.is_file(), "{} is written", path.display());
    }
}

/// Two files, one of them noise, so the whole of stage one and two can be
/// read off the checkpoints.
const DIFF: &str = "\
diff --git a/src/parse.c b/src/parse.c
--- a/src/parse.c
+++ b/src/parse.c
@@ -10,3 +10,4 @@ static int parse(void)
 int before(void);
-int gone(void);
+int added(void);
+int also_added(void);
 int after(void);
diff --git a/vendor/lib.c b/vendor/lib.c
--- a/vendor/lib.c
+++ b/vendor/lib.c
@@ -1,1 +1,2 @@
 int vendored(void);
+int more(void);
";

fn diff_source() -> Source {
    Source::Diff {
        origin: "change.diff".to_string(),
        content: DIFF.to_string(),
    }
}

fn diff_adapters(calls: Arc<Calls>) -> Adapters {
    let (adapters, _) = capturing_diff_adapters(calls);
    adapters
}

fn capturing_diff_adapters(calls: Arc<Calls>) -> (Adapters, Arc<Mutex<Vec<Request>>>) {
    scripted_diff_adapters(calls, Vec::new())
}

fn scripted_diff_adapters(
    calls: Arc<Calls>,
    replies: Vec<&str>,
) -> (Adapters, Arc<Mutex<Vec<Request>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let adapters = Adapters::for_test(
        None,
        Box::new(FakeProtocol::scripted(
            calls,
            Arc::clone(&requests),
            replies,
        )),
        Redactor::new().expect("patterns"),
    );
    (adapters, requests)
}

#[test]
fn a_real_diff_reaches_the_model_as_one_chunk_per_surviving_file() {
    let workspace =
        Workspace::with_config(&CONFIG.replace("[plan]", "[plan]\nskip_paths = [\"vendor/**\"]"));
    let calls = Arc::new(Calls::default());
    let result = crate::review_with(
        &workspace.settings(),
        &diff_source(),
        &diff_adapters(Arc::clone(&calls)),
        &Silent,
    )
    .expect("run completes");

    let run_dir = workspace.run_dir(&result.run_id);
    let input: serde_json::Value = serde_json::from_slice(
        &std::fs::read(run_dir.join(layout::stage_file(Stage::Input))).unwrap(),
    )
    .expect("input checkpoint");
    let files = input["files"].as_array().expect("files");
    assert_eq!(files.len(), 2, "both files are in the change set");
    assert_eq!(files[0]["new_path"], "src/parse.c");
    assert_eq!(
        files[0]["commentable_lines"],
        serde_json::json!([10, 11, 12, 13])
    );
    assert_eq!(files[0]["changed_lines"], serde_json::json!([11, 12]));

    let plan: serde_json::Value = serde_json::from_slice(
        &std::fs::read(run_dir.join(layout::stage_file(Stage::Plan))).unwrap(),
    )
    .expect("plan checkpoint");
    let chunks = plan["chunks"].as_array().expect("chunks");
    assert_eq!(chunks.len(), 1, "the vendored file was skipped");
    assert_eq!(chunks[0]["path"], "src/parse.c");
    assert!(
        chunks[0]["diff"]
            .as_str()
            .expect("diff text")
            .starts_with("--- a/src/parse.c\n+++ b/src/parse.c\n@@ "),
        "a chunk is a unified diff fragment of its own"
    );

    assert_eq!(result.skipped.len(), 1);
    assert_eq!(
        Calls::get(&calls.send),
        3,
        "one call for the chunk, then a score (stub has no scoring reply, so one re-ask)"
    );
    assert_eq!(result.exit_code(), 0);
}

#[test]
fn an_mbox_is_refused_instead_of_being_stripped() {
    let workspace = Workspace::new();
    let source = Source::Diff {
        origin: "series.patch".to_string(),
        content: format!(
            "From {} Mon Sep 17 00:00:00 2001\nFrom: A <a@example.com>\nSubject: [PATCH] x\n\n{DIFF}",
            "0".repeat(40)
        ),
    };
    let calls = Arc::new(Calls::default());
    let error = crate::review_with(
        &workspace.settings(),
        &source,
        &diff_adapters(Arc::clone(&calls)),
        &Silent,
    )
    .expect_err("only unified diff is accepted");

    assert!(
        error.to_string().contains("only unified diff"),
        "got {error}"
    );
    assert_eq!(error.exit_code(), 2);
    assert_eq!(Calls::get(&calls.send), 0, "nothing was sent to the model");
}

#[test]
fn a_budget_of_zero_still_finishes_the_run_and_names_what_it_could_not_review() {
    let workspace =
        Workspace::with_config(&CONFIG.replace("budget_per_run = 10.0", "budget_per_run = 0.0"));
    let calls = Arc::new(Calls::default());
    let result = crate::review_with(
        &workspace.settings(),
        &diff_source(),
        &diff_adapters(Arc::clone(&calls)),
        &Silent,
    )
    .expect("the run finishes rather than aborting");

    assert_eq!(Calls::get(&calls.send), 0, "not a cent was spent");
    assert_eq!(
        result.unreviewed,
        vec!["src/parse.c".to_string(), "vendor/lib.c".to_string()],
        "every surviving file is listed"
    );
    assert!(result.stopped.is_some());
    assert_eq!(result.exit_code(), 3);
    assert!(
        result
            .unscored_reason
            .as_deref()
            .expect("a missing score always says why")
            .contains("stopped"),
        "a run that reviewed nothing must not read as a clean one: {:?}",
        result.unscored_reason
    );
    assert!(
        result.report_path.is_file(),
        "the report M4 fills is already on disk"
    );
    assert!(result.summary_path.is_file());
}

#[test]
fn a_missing_config_names_the_path_it_looked_at() {
    let missing = std::path::Path::new("/nonexistent/reviewbot/reviewbot.toml");
    let error = Settings::load(Some(missing), RunOptions::default()).expect_err("no such file");
    assert!(
        error
            .to_string()
            .contains("/nonexistent/reviewbot/reviewbot.toml"),
        "got {error}"
    );
}

/// The whole path for the one thing a URL run has and a diff file does not:
/// what the author wrote about the change. It has to reach the model, it has
/// to arrive as material ahead of the diff, and it must not reach
/// `instructions` — which is the slot the prompt itself calls authoritative.
#[test]
fn what_the_author_wrote_reaches_every_review_request_as_material() {
    let workspace = Workspace::new();
    let calls = Arc::new(Calls::default());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let adapters = Adapters::for_test(
        Some(Box::new(FakePlatform::narrated(Arc::clone(&calls)))),
        Box::new(FakeProtocol::scripted(
            calls,
            Arc::clone(&requests),
            Vec::new(),
        )),
        Redactor::new().expect("patterns"),
    );

    crate::review_with(
        &workspace.settings(),
        &Source::Url(URL.to_string()),
        &adapters,
        &Silent,
    )
    .expect("run completes");

    let captured = requests.lock().expect("requests");
    let review_turns: Vec<&Request> = captured
        .iter()
        .filter(|request| {
            request
                .tools()
                .iter()
                .any(|tool| tool.name == "submit_comment")
        })
        .collect();
    assert!(!review_turns.is_empty(), "the diff produced no chunk");
    for request in &review_turns {
        assert!(
            !request.instructions().contains("bound the parser index"),
            "author prose in the authoritative slot"
        );
        match &request.input()[0] {
            crate::protocol::InputItem::Message { content, .. } => {
                assert!(content.contains("bound the parser index"), "{content}");
                assert!(content.contains("fixes the overflow"), "{content}");
                assert!(content.contains("material"), "{content}");
            }
            other => panic!("expected the description first, got {other:?}"),
        }
    }
}

#[test]
fn instructions_are_byte_identical_across_two_chunks() {
    let workspace = Workspace::new();
    let calls = Arc::new(Calls::default());
    let (adapters, requests) = capturing_diff_adapters(Arc::clone(&calls));
    crate::review_with(&workspace.settings(), &diff_source(), &adapters, &Silent)
        .expect("run completes");

    assert!(
        Calls::get(&calls.send) >= 2,
        "one call per file, then a score"
    );
    let captured = requests.lock().expect("requests");
    assert!(
        captured.len() >= 2,
        "two review turns, then a score: {}",
        captured.len()
    );
    assert_eq!(
        captured[0].instructions().as_bytes(),
        captured[1].instructions().as_bytes()
    );
    assert_ne!(captured[0].input(), captured[1].input());
    assert!(
        captured[0].instructions().contains("`submit_comment`"),
        "findings are delivered as a function call"
    );
    assert!(!captured[0].instructions().contains("read_repo_file"));
}

/// One chunk's worth of answer, on a line the fixture diff really added.
const FINDING: &str = r#"{"comments":[{"path":"src/parse.c","start_line":11,
    "body":"`added` is declared twice once the removed line comes back",
    "suggestion":"keep only one declaration of `added`",
    "severity_score":74,"confidence_score":91,"evidence":{"lines":[11,12]}}]}"#;

const SCORE: &str = r#"{"overall_score":54,"summary":"one certain finding; read it first"}"#;

#[test]
fn a_scored_run_writes_the_report_and_a_second_run_does_not_score_again() {
    let workspace =
        Workspace::with_config(&CONFIG.replace("[plan]", "[plan]\nskip_paths = [\"vendor/**\"]"));
    let source = workspace.diff_file();
    let calls = Arc::new(Calls::default());
    let (adapters, _) = scripted_diff_adapters(Arc::clone(&calls), vec![FINDING, SCORE]);
    let first = crate::review_with(&workspace.settings(), &source, &adapters, &Silent)
        .expect("run completes");

    assert_eq!(
        Calls::get(&calls.send),
        2,
        "one call for the chunk, one for the score"
    );
    assert_eq!(first.comments.len(), 1);
    assert_eq!(
        first.comments[0].confidence_score, 91,
        "the model's number is published as given"
    );
    assert_eq!(first.comments[0].confidence, Confidence::Certain);
    assert_eq!(
        first.comments[0].suggestion,
        "keep only one declaration of `added`"
    );
    assert_eq!(first.overall_score, Some(54));
    assert!(first.unscored_reason.is_none());

    let report = std::fs::read_to_string(&first.report_path).expect("the report is on disk");
    assert!(
        report.contains("## [major 74% / certain 91%] `src/parse.c:11`"),
        "both of the model's numbers reach the report, in that order: {report}"
    );
    assert!(
        report.contains("suggestion:\nkeep only one declaration of `added`"),
        "{report}"
    );
    assert!(report.contains("overall: 54 / 100"), "{report}");
    assert!(
        report.contains("trace: `review-src_parse.c`"),
        "the report names the trace id instead of inlining it: {report}"
    );
    assert!(
        !report.contains("<details>"),
        "invocation details stay in traces/: {report}"
    );

    let run_dir = workspace.run_dir(&first.run_id);
    rewind_to(&run_dir, 4);
    let second_calls = Arc::new(Calls::default());
    let (second_adapters, _) = scripted_diff_adapters(Arc::clone(&second_calls), Vec::new());
    let second = crate::review_with(&workspace.settings(), &source, &second_adapters, &Silent)
        .expect("the second run completes");

    assert_eq!(second.run_id, first.run_id);
    assert_eq!(
        Calls::get(&second_calls.send),
        0,
        "the merge checkpoint already holds the score"
    );
    assert_eq!(second.overall_score, Some(54));
    assert_eq!(second.comments.len(), 1);
}

/// The reason the last two stages have no checkpoint skip. A run that got all
/// the way through is entered again with nothing left to compute: the report
/// still has to be rendered — that is how somebody gets it back after
/// deleting it — and the MR still has to be looked at, and still has to hear
/// nothing, because everything on it is already there.
#[test]
fn a_finished_run_entered_again_rewrites_the_report_and_says_nothing_twice() {
    let workspace =
        Workspace::with_config(&CONFIG.replace("[plan]", "[plan]\nskip_paths = [\"vendor/**\"]"));
    let settings = workspace.settings_with(RunOptions {
        runs_dir: workspace.runs_dir.clone(),
        publish: true,
        ..RunOptions::default()
    });
    let mr = Arc::new(Mr::default());

    let first = crate::review_with(
        &settings,
        &Source::Url(URL.to_string()),
        &publishing_adapters(
            Arc::new(Calls::default()),
            Arc::clone(&mr),
            vec![FINDING, SCORE],
        ),
        &Silent,
    )
    .expect("run completes");

    assert_eq!(
        first.published.len(),
        2,
        "the finding and the summary went out"
    );
    assert_eq!(mr.len(), 2);

    // Whatever the report stage would have been skipped over, it has to
    // replace: this is not a renderer's output.
    let run_dir = workspace.run_dir(&first.run_id);
    std::fs::write(run_dir.join(layout::REPORT), b"stale").expect("overwrite the report");

    let calls = Arc::new(Calls::default());
    let second = crate::review_with(
        &settings,
        &Source::Url(URL.to_string()),
        &publishing_adapters(Arc::clone(&calls), Arc::clone(&mr), Vec::new()),
        &Silent,
    )
    .expect("the second entry completes");

    assert_eq!(second.run_id, first.run_id);
    assert_eq!(Calls::get(&calls.send), 0, "nothing is reviewed again");
    let report = std::fs::read_to_string(run_dir.join(layout::REPORT)).expect("the report is back");
    assert!(
        report.contains("## [major 74% / certain 91%] `src/parse.c:11`"),
        "the report stage ran and rendered it from the checkpoints: {report}"
    );

    assert_eq!(
        Calls::get(&calls.existing_comments),
        1,
        "what the MR already says is what decides, so it is read every time"
    );
    assert_eq!(Calls::get(&calls.post_comments), 0);
    assert_eq!(mr.len(), 2, "not one comment was said twice");
    assert_eq!(second.published.len(), 2, "published.json carries over");
}

const SECRET_KEY: &str = "sk-abcdefghijklmnopqrstuvwxyz";

#[test]
fn a_key_in_the_diff_never_reaches_the_model_or_the_trace() {
    let workspace = Workspace::new();
    let calls = Arc::new(Calls::default());
    let (adapters, requests) = capturing_diff_adapters(Arc::clone(&calls));
    let source = Source::Diff {
        origin: "secret.diff".to_string(),
        content: format!(
            "\
diff --git a/src/parse.c b/src/parse.c
--- a/src/parse.c
+++ b/src/parse.c
@@ -1,1 +1,2 @@
 int parse(void);
+char *key = \"{SECRET_KEY}\";
"
        ),
    };
    let result = crate::review_with(&workspace.settings(), &source, &adapters, &Silent)
        .expect("run completes");

    let captured = requests.lock().expect("requests");
    assert!(
        !captured.is_empty(),
        "the review turn still goes out even when a score follows"
    );
    for request in captured.iter() {
        let sent = serde_json::to_string(request).expect("request json");
        assert!(!sent.contains(SECRET_KEY), "{sent}");
    }
    match &captured[0].input()[0] {
        crate::protocol::InputItem::Message { content, .. } => {
            assert!(content.contains("<redacted:api-key>"), "{content}");
        }
        other => panic!("expected a user message, got {other:?}"),
    }

    let run_dir = workspace.run_dir(&result.run_id);
    let traces = run_dir.join("traces");
    for entry in std::fs::read_dir(&traces).expect("traces") {
        let path = entry.expect("entry").path();
        let text = std::fs::read_to_string(&path).expect("trace");
        assert!(
            !text.contains(SECRET_KEY),
            "{} still has the key",
            path.display()
        );
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("review-"))
        {
            assert!(
                text.contains("<redacted:api-key>"),
                "{} should carry the redacted placeholder",
                path.display()
            );
        }
    }
}

/// A watcher that keeps everything it is told. The status screen is built out
/// of exactly this sequence, so the sequence is what a test can hold on to —
/// and it is all a test should hold on to, because how any of it looks is the
/// consumer's business and not the run's.
#[derive(Default)]
struct Watcher {
    events: Mutex<Vec<Event>>,
}

impl Progress for Watcher {
    fn emit(&self, event: Event) {
        self.events.lock().expect("events").push(event);
    }
}

impl Watcher {
    fn events(&self) -> Vec<Event> {
        self.events.lock().expect("events").clone()
    }

    /// Every stage that finished, in order, paired with what it said it did.
    /// The pairing is checked on the way through: a screen that shows one
    /// line per stage cannot be written against a channel that opens a stage
    /// before closing the last one.
    fn stages(&self) -> Vec<(Stage, Outcome, bool)> {
        let mut finished = Vec::new();
        let mut open: Option<Stage> = None;
        for event in self.events() {
            match event {
                Event::StageStarted { stage } => {
                    assert_eq!(open, None, "{stage} started while another stage was open");
                    open = Some(stage);
                }
                Event::StageFinished {
                    stage,
                    outcome,
                    from_checkpoint,
                } => {
                    assert_eq!(open, Some(stage), "{stage} finished unannounced");
                    open = None;
                    finished.push((stage, outcome, from_checkpoint));
                }
                _ => {}
            }
        }
        assert_eq!(open, None, "a stage was announced and never finished");
        finished
    }

    fn chunks(&self) -> Vec<(usize, usize, String)> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Chunk {
                    index, of, path, ..
                } => Some((index, of, path)),
                _ => None,
            })
            .collect()
    }
}

/// What a watcher hears from a run that does all the work: the run naming
/// itself, then six stages in order, each with the numbers the final summary
/// prints, and one `Chunk` per file the plan cut.
#[test]
fn a_full_run_announces_every_stage_and_numbers_the_chunks() {
    let workspace = Workspace::new();
    let watcher = Watcher::default();
    let result = crate::review_with(
        &workspace.settings(),
        &diff_source(),
        &diff_adapters(Arc::new(Calls::default())),
        &watcher,
    )
    .expect("run completes");

    assert_eq!(
        watcher.events().first(),
        Some(&Event::Opening),
        "the wait before a run can name itself is accounted for"
    );
    assert_eq!(
        watcher.events().get(1),
        Some(&Event::RunStarted {
            run_id: result.run_id.clone(),
            run_dir: workspace.run_dir(&result.run_id),
            model: "deepseek-v4-flash".to_string(),
            input: "change.diff".to_string(),
            worktree: None,
        }),
        "and then it says what it is, before doing anything"
    );

    let stages = watcher.stages();
    let announced: Vec<Stage> = stages.iter().map(|(stage, _, _)| *stage).collect();
    assert_eq!(
        announced,
        Stage::ALL.to_vec(),
        "all six, in the order lib.rs runs them"
    );
    assert_eq!(
        stages
            .iter()
            .map(|(_, outcome, from_checkpoint)| (outcome, from_checkpoint))
            .collect::<Vec<_>>(),
        vec![
            (&Outcome::Input { files: 2 }, &false),
            (
                &Outcome::Plan {
                    chunks: 2,
                    skipped: 0,
                },
                &false,
            ),
            (
                &Outcome::Review {
                    chunks: 2,
                    unreviewed: 0,
                },
                &false,
            ),
            (
                &Outcome::Merge {
                    comments: 0,
                    overall: None,
                },
                &false,
            ),
            (&Outcome::Report, &false),
            (
                &Outcome::Publish {
                    posted: 0,
                    already_there: 0,
                    asked: false,
                },
                &false,
            ),
        ],
        "a finished stage says what it produced, in the summary's own numbers"
    );

    assert_eq!(
        watcher.chunks(),
        vec![
            (1, 2, "src/parse.c".to_string()),
            (2, 2, "vendor/lib.c".to_string()),
        ],
        "chunks are counted from 1, for a reader rather than for the loop"
    );
    assert!(
        watcher
            .events()
            .iter()
            .any(|event| matches!(event, Event::Round { round: 1, of: 100 })),
        "a round is counted against [review].max_rounds"
    );
    assert!(
        watcher
            .events()
            .iter()
            .any(|event| matches!(event, Event::Spend { currency, .. } if currency == "CNY")),
        "every settled call says what the run has spent"
    );
}

/// A tool call brackets the same work recorded in the trace. Delivering a
/// finding is a tool call like any other; ending the file is a second one.
#[test]
fn a_tool_the_model_calls_is_named_as_it_goes_out_and_returns() {
    let workspace =
        Workspace::with_config(&CONFIG.replace("[plan]", "[plan]\nskip_paths = [\"vendor/**\"]"));
    let watcher = Watcher::default();
    let (adapters, _) = scripted_diff_adapters(Arc::new(Calls::default()), vec![FINDING, SCORE]);
    crate::review_with(
        &workspace.settings(),
        &workspace.diff_file(),
        &adapters,
        &watcher,
    )
    .expect("run completes");

    let tools: Vec<(&str, String)> = watcher
        .events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Tool { name } => Some(("started", name)),
            Event::ToolDone { name, .. } => Some(("finished", name)),
            _ => None,
        })
        .collect();
    assert_eq!(
        tools,
        vec![
            ("started", "submit_comment".to_string()),
            ("finished", "submit_comment".to_string()),
            ("started", "finish_review".to_string()),
            ("finished", "finish_review".to_string()),
        ]
    );
}

/// The reason a skipped stage still has to be announced: a run entered again
/// does no work at all in the first four stages, and a screen that showed
/// only what ran would show almost nothing. The numbers are still there, and
/// they say where they came from.
#[test]
fn a_run_entered_again_reports_every_stage_off_its_checkpoints() {
    let workspace = Workspace::new();
    let source = workspace.diff_file();
    let first = crate::review_with(
        &workspace.settings(),
        &source,
        &diff_adapters(Arc::new(Calls::default())),
        &Silent,
    )
    .expect("run completes");

    let watcher = Watcher::default();
    let calls = Arc::new(Calls::default());
    let second = crate::review_with(
        &workspace.settings(),
        &source,
        &diff_adapters(Arc::clone(&calls)),
        &watcher,
    )
    .expect("the second entry completes");

    assert_eq!(second.run_id, first.run_id);
    assert_eq!(Calls::get(&calls.send), 0, "nothing was reviewed again");

    let stages = watcher.stages();
    let announced: Vec<Stage> = stages.iter().map(|(stage, _, _)| *stage).collect();
    assert_eq!(announced, Stage::ALL.to_vec());
    for (stage, _, from_checkpoint) in stages.iter().take(4) {
        assert!(from_checkpoint, "{stage} was skipped and has to say so");
    }
    assert_eq!(
        stages[1].1,
        Outcome::Plan {
            chunks: 2,
            skipped: 0
        }
    );
    for (stage, _, from_checkpoint) in stages.iter().skip(4) {
        assert!(!from_checkpoint, "{stage} runs every time");
    }
    assert!(
        watcher.chunks().is_empty(),
        "no chunk was looked at, so none is reported"
    );
}

/// The error every caller branches on stays small enough that returning it is
/// not itself a lint. There is not one suppression attribute in this
/// repository and that is deliberate, so the type has to be small rather than
/// the warning silenced — and the way it grows is somebody putting a vendor's
/// hundred-byte error inline in a variant, which is easy to do by accident.
#[test]
fn the_error_type_stays_small_enough_to_return() {
    let size = std::mem::size_of::<Error>();
    assert!(
        size < 128,
        "Error is {size} bytes; box the payload of whichever variant grew"
    );
}
