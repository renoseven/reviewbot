//! Crate level tests. The fakes live here rather than in the adapters so a
//! user config can never name one: adapters are injected through
//! `review_with` / `resume_with`, which are crate visible.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::config::{PlatformKind, RunOptions, Settings};
use crate::domain::Confidence;
use crate::platform::{
    Capabilities, ChangeRef, DiffRefs, ExistingComment, LineRange, Listing, OutgoingComment,
    Platform, PlatformChange, PlatformError, RepoSource, SearchHit,
};
use crate::protocol::{OutputItem, Protocol, ProtocolError, Request, Response};
use crate::record::{DirLock, LocalStorage, Storage, layout};
use crate::security::Redactor;
use crate::stage::Adapters;
use crate::tool::{FinishReview, Registry, SubmitComment, SubmitSummary};
use crate::worktree::FetchedWorktree;
use crate::{Error, RunResult, Source};

const CONFIG: &str = r#"
[review]
max_tool_rounds = 12
max_files_per_listing = 200
max_hits_per_search = 50
max_file_bytes = 262144
max_tool_output_bytes = 32768

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

[[model]]
name = "deepseek-v4-pro"
provider = "deepseek"
input_per_1m_tokens = 4.0
output_per_1m_tokens = 12.0
context_window_tokens = 131072
max_output_tokens = 8192

[[platform]]
host = "gitlab.com"
base_url = "https://gitlab.com/api/v4"
api_token = "GITLAB_TOKEN"
"#;

const URL: &str = "https://gitlab.com/acme/app/-/merge_requests/128";
const HEAD_SHA: &str = "4b1e0d2c0000000000000000000000000000abcd";

/// How often each fake was asked for something, so a resume can prove it did
/// not repeat work.
#[derive(Default)]
struct Calls {
    head_sha: AtomicUsize,
    fetch_change: AtomicUsize,
    send: AtomicUsize,
}

impl Calls {
    fn get(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::SeqCst)
    }
}

struct FakePlatform {
    calls: Arc<Calls>,
    repo: Arc<FakeRepo>,
    /// What `fetch_change` hands back. Empty by default: most of the URL
    /// tests are about locking and fingerprints and want no chunks at all.
    change: PlatformChange,
}

impl FakePlatform {
    fn new(calls: Arc<Calls>) -> Self {
        Self {
            calls,
            repo: Arc::new(FakeRepo),
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
        platform.change.narrative = crate::domain::Narrative::new(
            Some("bound the parser index".to_string()),
            Some("fixes the overflow reported in #12".to_string()),
            vec!["bound the index".to_string()],
        );
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

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
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
        Ok(Vec::new())
    }

    fn post_comments(
        &self,
        _change: &ChangeRef,
        _refs: &DiffRefs,
        _comments: &[OutgoingComment],
    ) -> Result<Vec<ExistingComment>, PlatformError> {
        Ok(Vec::new())
    }

    fn bind_repo(&self, _change: &ChangeRef, _head_sha: &str) {}

    fn repo_source(&self) -> Arc<dyn RepoSource> {
        Arc::clone(&self.repo) as Arc<dyn RepoSource>
    }
}

struct FakeRepo;

impl RepoSource for FakeRepo {
    fn list_files(&self, _glob: &str) -> Result<Listing, PlatformError> {
        Ok(Listing {
            paths: Vec::new(),
            complete: true,
        })
    }

    fn read_file(&self, _path: &str, _lines: Option<LineRange>) -> Result<String, PlatformError> {
        Ok(String::new())
    }

    fn search(&self, _query: &str, _glob: Option<&str>) -> Result<Vec<SearchHit>, PlatformError> {
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
/// `submit_summary`. A document the request has no tool for stays a message,
/// which is how the "it only talked" paths are exercised.
fn reply_as_calls(text: &str, request: &Request) -> Option<Vec<OutputItem>> {
    let advertised = |name: &str| request.tools.iter().any(|tool| tool.name == name);
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
    if !advertised(SubmitComment::NAME) {
        return None;
    }
    let comments = value.get("comments")?.as_array()?;
    Some(
        comments
            .iter()
            .enumerate()
            .map(|(index, comment)| OutputItem::FunctionCall {
                call_id: format!("submit-{index}"),
                name: SubmitComment::NAME.to_string(),
                arguments: comment.to_string(),
            })
            .collect(),
    )
}

fn adapters(calls: Arc<Calls>) -> Adapters {
    let mut tools = Registry::new();
    tools.register(Box::new(SubmitComment::new()));
    tools.register(Box::new(FinishReview::new()));
    tools.register(Box::new(SubmitSummary::new()));
    Adapters {
        platform: Some(Box::new(FakePlatform::new(Arc::clone(&calls)))),
        protocol: Box::new(FakeProtocol::new(calls, Arc::new(Mutex::new(Vec::new())))),
        tools,
        // One worktree, and this one fills itself from the fake platform.
        worktree: Arc::new(FetchedWorktree::new(
            Some(Arc::new(FakeRepo) as Arc<dyn RepoSource>),
            Capabilities::default(),
        )),
        redactor: Redactor::new(),
    }
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

    /// A diff on disk. `resume` re-reads the file the run started from and
    /// checks that it still holds the same content, so a diff run that is
    /// going to be resumed needs a real path.
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
    )
}

const STAGE_FILES: [(u8, &str); 5] = [
    (1, "input"),
    (2, "triage"),
    (3, "review"),
    (4, "merge"),
    (5, "publish"),
];

fn stage_paths(run_dir: &Path) -> Vec<PathBuf> {
    STAGE_FILES
        .iter()
        .map(|(number, name)| run_dir.join(layout::stage_file(*number, name)))
        .collect()
}

/// Cut a finished run back to the first `keep` stages, the way an interrupted
/// process would have left it.
fn rewind_to(run_dir: &Path, keep: usize) {
    let meta_path = run_dir.join(layout::META);
    let mut meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&meta_path).expect("meta")).expect("meta json");
    let completed: Vec<serde_json::Value> = STAGE_FILES
        .iter()
        .take(keep)
        .map(|(_, name)| serde_json::Value::String(name.to_string()))
        .collect();
    meta["completed_stages"] = serde_json::Value::Array(completed);
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
    assert!(
        !run_dir.join(layout::LOCK).exists(),
        "the lock is gone after a clean exit"
    );

    assert!(result.comments.is_empty());
    assert_eq!(
        result.overall_score, None,
        "an unscored run reports null, never 0"
    );
    assert!(result.unscored_reason.is_some());
    assert_eq!(Calls::get(&calls.fetch_change), 1);
}

#[test]
fn resume_after_input_leaves_the_input_stage_alone() {
    let workspace = Workspace::new();
    let first = review(&workspace, Arc::new(Calls::default())).expect("run completes");
    let run_dir = workspace.run_dir(&first.run_id);
    let input_file = run_dir.join(layout::stage_file(1, "input"));
    let input_before = std::fs::read(&input_file).expect("input checkpoint");

    rewind_to(&run_dir, 1);
    assert!(!run_dir.join(layout::stage_file(2, "triage")).exists());

    let calls = Arc::new(Calls::default());
    let resumed = crate::resume_with(
        &workspace.settings(),
        &first.run_id,
        &adapters(Arc::clone(&calls)),
    )
    .expect("resume completes");

    assert_eq!(resumed.run_id, first.run_id);
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

#[test]
fn resume_after_review_leaves_the_first_three_stages_alone() {
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
    crate::resume_with(
        &workspace.settings(),
        &first.run_id,
        &adapters(Arc::clone(&calls)),
    )
    .expect("resume completes");

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
    std::fs::write(run_dir.join(layout::stage_file(4, "merge")), b"{ not json")
        .expect("corrupt the merge checkpoint");

    let resumed = crate::resume_with(
        &workspace.settings(),
        &first.run_id,
        &adapters(Arc::new(Calls::default())),
    )
    .expect("resume completes");

    assert_eq!(resumed.run_id, first.run_id);
    let merged: serde_json::Value = serde_json::from_slice(
        &std::fs::read(run_dir.join(layout::stage_file(4, "merge"))).unwrap(),
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
    let held = DirLock::take(Arc::clone(&storage)).expect("first lock");

    let error = crate::review_with(
        &settings,
        &Source::Url(URL.to_string()),
        &adapters(Arc::new(Calls::default())),
    )
    .expect_err("the second open fails");
    assert!(error.to_string().contains("already running"), "got {error}");

    drop(held);
    assert!(!run_dir.join(layout::LOCK).exists());
}

#[test]
fn resume_refuses_a_run_whose_config_has_changed() {
    let workspace = Workspace::new();
    let first = review(&workspace, Arc::new(Calls::default())).expect("run completes");

    let changed = CONFIG.replace("budget_per_run = 10.0", "budget_per_run = 20.0");
    std::fs::write(&workspace.config_path, changed).expect("rewrite config");

    let error = crate::resume_with(
        &workspace.settings(),
        &first.run_id,
        &adapters(Arc::new(Calls::default())),
    )
    .expect_err("the fingerprint no longer matches");
    assert!(matches!(error, Error::FingerprintMismatch { .. }));
    assert_eq!(error.exit_code(), 2);
}

#[test]
fn an_explicit_run_id_does_not_get_around_the_fingerprint_check() {
    let workspace = Workspace::new();
    let first = review(&workspace, Arc::new(Calls::default())).expect("run completes");

    let changed = CONFIG.replace("budget_per_run = 10.0", "budget_per_run = 20.0");
    std::fs::write(&workspace.config_path, changed).expect("rewrite config");
    let settings = workspace.settings_with(RunOptions {
        runs_dir: workspace.runs_dir.clone(),
        run_id: Some(first.run_id.clone()),
        ..RunOptions::default()
    });

    let error = crate::review_with(
        &settings,
        &Source::Url(URL.to_string()),
        &adapters(Arc::new(Calls::default())),
    )
    .expect_err("the same check applies");
    assert!(matches!(error, Error::FingerprintMismatch { .. }));
}

#[test]
fn run_parameters_do_not_change_identity_but_conclusions_do() {
    let workspace = Workspace::new();
    let baseline = workspace.settings().fingerprint();

    let noisy = workspace.settings_with(RunOptions {
        runs_dir: workspace.runs_dir.join("elsewhere"),
        output_dir: Some(PathBuf::from("artifacts")),
        retries: 7,
        publish: true,
        ..RunOptions::default()
    });
    assert_eq!(
        noisy.fingerprint(),
        baseline,
        "--retries, --publish and artifact locations stay out of the fingerprint"
    );

    let other_model = workspace.settings_with(RunOptions {
        model: Some("deepseek-v4-pro".to_string()),
        ..RunOptions::default()
    });
    assert_ne!(other_model.fingerprint(), baseline, "--model is in it");

    let with_worktree = workspace.settings_with(RunOptions {
        worktree: Some(PathBuf::from("/tmp/checkout")),
        ..RunOptions::default()
    });
    assert_ne!(
        with_worktree.fingerprint(),
        baseline,
        "the content source mode is in it"
    );

    std::fs::write(
        &workspace.config_path,
        CONFIG.replace("budget_per_run = 10.0", "budget_per_run = 11.0"),
    )
    .expect("rewrite config");
    assert_ne!(
        workspace.settings().fingerprint(),
        baseline,
        "any parsed config change is in it"
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

    let error = crate::review_with(&settings, &source, &adapters(Arc::new(Calls::default())))
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
        &Adapters {
            platform: None,
            protocol: Box::new(FakeProtocol::new(
                Arc::new(Calls::default()),
                Arc::new(Mutex::new(Vec::new())),
            )),
            tools: Registry::new(),
            worktree: Arc::new(FetchedWorktree::new(None, Capabilities::default())),
            redactor: Redactor::new(),
        },
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
    let mut tools = Registry::new();
    tools.register(Box::new(SubmitComment::new()));
    tools.register(Box::new(FinishReview::new()));
    tools.register(Box::new(SubmitSummary::new()));
    let adapters = Adapters {
        platform: None,
        protocol: Box::new(FakeProtocol::scripted(
            calls,
            Arc::clone(&requests),
            replies,
        )),
        tools,
        worktree: Arc::new(FetchedWorktree::new(None, Capabilities::default())),
        redactor: Redactor::new(),
    };
    (adapters, requests)
}

#[test]
fn a_real_diff_reaches_the_model_as_one_chunk_per_surviving_file() {
    let workspace = Workspace::with_config(
        &CONFIG.replace("[triage]", "[triage]\nskip_paths = [\"vendor/**\"]"),
    );
    let calls = Arc::new(Calls::default());
    let result = crate::review_with(
        &workspace.settings(),
        &diff_source(),
        &diff_adapters(Arc::clone(&calls)),
    )
    .expect("run completes");

    let run_dir = workspace.run_dir(&result.run_id);
    let input: serde_json::Value = serde_json::from_slice(
        &std::fs::read(run_dir.join(layout::stage_file(1, "input"))).unwrap(),
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
        &std::fs::read(run_dir.join(layout::stage_file(2, "triage"))).unwrap(),
    )
    .expect("triage checkpoint");
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
    let mut tools = Registry::new();
    tools.register(Box::new(SubmitComment::new()));
    tools.register(Box::new(FinishReview::new()));
    tools.register(Box::new(SubmitSummary::new()));
    let adapters = Adapters {
        platform: Some(Box::new(FakePlatform::narrated(Arc::clone(&calls)))),
        protocol: Box::new(FakeProtocol::scripted(
            calls,
            Arc::clone(&requests),
            Vec::new(),
        )),
        tools,
        worktree: Arc::new(FetchedWorktree::new(None, Capabilities::default())),
        redactor: Redactor::new(),
    };

    crate::review_with(
        &workspace.settings(),
        &Source::Url(URL.to_string()),
        &adapters,
    )
    .expect("run completes");

    let captured = requests.lock().expect("requests");
    let review_turns: Vec<&Request> = captured
        .iter()
        .filter(|request| {
            request
                .tools
                .iter()
                .any(|tool| tool.name == "submit_comment")
        })
        .collect();
    assert!(!review_turns.is_empty(), "the diff produced no chunk");
    for request in &review_turns {
        assert!(
            !request.instructions.contains("bound the parser index"),
            "author prose in the authoritative slot"
        );
        match &request.input[0] {
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
    crate::review_with(&workspace.settings(), &diff_source(), &adapters).expect("run completes");

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
        captured[0].instructions.as_bytes(),
        captured[1].instructions.as_bytes()
    );
    assert_ne!(captured[0].input, captured[1].input);
    assert!(
        captured[0].instructions.contains("`submit_comment`"),
        "findings are delivered as a function call"
    );
    assert!(!captured[0].instructions.contains("read_repo_file"));
}

/// One chunk's worth of answer, on a line the fixture diff really added.
const FINDING: &str = r#"{"comments":[{"path":"src/parse.c","line":11,
    "body":"`added` is declared twice once the removed line comes back",
    "suggestion":"keep only one declaration of `added`",
    "severity_score":74,"confidence_score":91,"evidence":{"diff_lines":[11,12]}}]}"#;

const SCORE: &str = r#"{"overall_score":54,"summary":"one certain finding; read it first"}"#;

#[test]
fn a_scored_run_writes_the_report_and_resume_does_not_score_again() {
    let workspace = Workspace::with_config(
        &CONFIG.replace("[triage]", "[triage]\nskip_paths = [\"vendor/**\"]"),
    );
    let source = workspace.diff_file();
    let calls = Arc::new(Calls::default());
    let (adapters, _) = scripted_diff_adapters(Arc::clone(&calls), vec![FINDING, SCORE]);
    let first =
        crate::review_with(&workspace.settings(), &source, &adapters).expect("run completes");

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
    let resumed_calls = Arc::new(Calls::default());
    let (resumed_adapters, _) = scripted_diff_adapters(Arc::clone(&resumed_calls), Vec::new());
    let resumed = crate::resume_with(&workspace.settings(), &first.run_id, &resumed_adapters)
        .expect("resume completes");

    assert_eq!(
        Calls::get(&resumed_calls.send),
        0,
        "the merge checkpoint already holds the score"
    );
    assert_eq!(resumed.overall_score, Some(54));
    assert_eq!(resumed.comments.len(), 1);
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
    let result =
        crate::review_with(&workspace.settings(), &source, &adapters).expect("run completes");

    let captured = requests.lock().expect("requests");
    assert!(
        !captured.is_empty(),
        "the review turn still goes out even when a score follows"
    );
    for request in captured.iter() {
        let sent = serde_json::to_string(request).expect("request json");
        assert!(!sent.contains(SECRET_KEY), "{sent}");
    }
    match &captured[0].input[0] {
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
