//! The one external command implementation. Every `[[tool]]` entry becomes
//! an instance of this; adding a checker never means new Rust.
//!
//! The argv shape is fixed here: an array handed to `execve` with no shell,
//! one placeholder expanding to exactly one element. Everything the model
//! supplies is checked before the process is spawned, and everything the
//! process says is filtered and clipped before the model sees it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Map, Value};

use crate::common::{Backoff, truncate};
use crate::config::ToolEntry;
use crate::security::{EnvPolicy, Limits, PathPolicy};
use crate::worktree::{Abilities, WorktreeSource};

use super::availability::unavailable_description;
use super::signature::Signature;
use super::{Purpose, Round, Tool, ToolError, ToolOutput};

/// How often a running child is asked whether it is done.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A ceiling on what is read out of the pipes, so a command that never stops
/// printing cannot exhaust memory before the timeout notices it.
const READ_CEILING: u64 = 16 * 1024 * 1024;

/// What replaces a whole line that names a denied path. Not a surgical
/// rewrite: reviewbot does not parse a tool's output format, so it cannot
/// tell which part of the line the path belongs to.
const WITHHELD_LINE: &str = "[one line withheld by deny_paths]";

/// Everything a command tool needs beyond its `[[tool]]` entry: where it may
/// run, what it may inherit, which paths it may be pointed at, and how long
/// it may take. Baked in when the tool is registered, so `Tool::execute`
/// still takes nothing but the arguments the model wrote.
///
/// The worktree is held as the source rather than as a path: a run that opens
/// its own worktree only knows where it is once the run directory exists, and
/// that is later than the tools are registered.
pub struct CommandContext {
    environment: EnvPolicy,
    paths: PathPolicy,
    worktree: Arc<dyn WorktreeSource>,
    max_output_bytes: u64,
    backoff: Backoff,
}

impl CommandContext {
    pub fn new(
        environment: EnvPolicy,
        paths: PathPolicy,
        worktree: Arc<dyn WorktreeSource>,
        max_output_bytes: u64,
        backoff: Backoff,
    ) -> Self {
        Self {
            environment,
            paths,
            worktree,
            max_output_bytes,
            backoff,
        }
    }

    fn root(&self) -> &Path {
        self.worktree.root()
    }

    /// Why this run's worktree cannot answer a checker that needs these, when
    /// it cannot. The same answer the description was written from.
    fn refusal(&self, needs: Abilities) -> Option<&'static str> {
        let reach = self.worktree.reach();
        let missing = reach.unmet(needs);
        match missing.is_empty() {
            true => None,
            false => Some(super::availability::refusal(missing)),
        }
    }
}

pub struct CommandTool {
    entry: ToolEntry,
    context: CommandContext,
    limits: Limits,
    /// The `params` table, read as a declaration: the schema the model sees
    /// and the check its call is put through both come from here.
    signature: Signature,
    /// The config author's own words, plus what this run's worktree makes of
    /// them. Owned rather than borrowed from the entry, because a checker this
    /// run cannot answer has to say so where the model reads, and the only
    /// thing it reads is this.
    description: String,
    /// What the checker needs of the worktree, from the entry. Read both to
    /// write the description and to refuse the call, so the two agree.
    needs: Abilities,
}

impl CommandTool {
    pub fn new(entry: ToolEntry, context: CommandContext) -> Self {
        let limits = Limits::new(entry.timeout_ms, context.max_output_bytes);
        let signature = Signature::from_entry(&entry);
        // Every checker opens a file, so every one of them needs content. A
        // compiler, a history walk or a cross-file analysis needs the whole
        // project on top of that, and says so in its entry.
        let mut needs = Abilities::CONTENT;
        needs.set(Abilities::CHECKOUT, entry.requires_checkout);
        let reach = context.worktree.reach();
        let missing = reach.unmet(needs);
        let description = match missing.is_empty() {
            true => entry.description.clone(),
            false => unavailable_description(&entry.description, missing),
        };
        Self {
            entry,
            context,
            limits,
            signature,
            description,
            needs,
        }
    }

    pub fn bin(&self) -> &PathBuf {
        &self.entry.bin
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Expand `{name}` from the validated arguments. Each template element
    /// stays one argv element, so `; rm -rf /` travels as a literal value.
    pub fn argv(&self, arguments: &Map<String, Value>) -> Result<Vec<String>, ToolError> {
        self.entry
            .args
            .iter()
            .map(|template| self.expand(template, arguments))
            .collect()
    }

    fn expand(&self, template: &str, arguments: &Map<String, Value>) -> Result<String, ToolError> {
        let mut out = String::new();
        let mut rest = template;
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            let Some(close) = after.find('}') else {
                out.push('{');
                rest = after;
                continue;
            };
            let name = &after[..close];
            let value = arguments
                .get(name)
                .ok_or_else(|| self.invalid(format!("missing argument {name:?}")))?;
            out.push_str(
                &scalar(value)
                    .ok_or_else(|| self.invalid(format!("argument {name:?} must be a scalar")))?,
            );
            rest = &after[close + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// The arguments the model wrote, checked against the declaration and then
    /// put through the path check: a `path` reaches argv as what the check
    /// approved rather than as what the model typed.
    fn checked(&self, arguments: &Value) -> Result<Map<String, Value>, ToolError> {
        let checked = self.signature.validate(&self.entry.name, arguments)?;
        let mut values = checked.values().clone();
        for parameter in self.signature.parameters() {
            if !matches!(parameter.shape, crate::tool::Shape::Path) {
                continue;
            }
            let Some(given) = values.get(&parameter.name).and_then(Value::as_str) else {
                continue;
            };
            let path = self
                .context
                .paths
                .check_worktree_path(given, self.context.root())
                .map_err(|rejection| ToolError::Rejected {
                    tool: self.entry.name.clone(),
                    reason: rejection.to_string(),
                })?;
            values.insert(parameter.name.clone(), Value::String(path));
        }
        Ok(values)
    }

    /// The config already refused a `bin` inside the repository, but the
    /// worktree is only known now, so the same rule is applied again here.
    fn refuse_bin_inside_worktree(&self) -> Result<(), ToolError> {
        let root = resolved(self.context.root());
        let bin = resolved(&self.entry.bin);
        if bin.starts_with(&root) {
            return Err(ToolError::Rejected {
                tool: self.entry.name.clone(),
                reason: format!(
                    "{} is inside the worktree, so it is part of what is being reviewed",
                    self.entry.bin.display()
                ),
            });
        }
        Ok(())
    }

    /// Only a timeout or a kill earns another attempt. Everything else is
    /// either a result or a fault that will repeat.
    fn run(&self, argv: &[String]) -> Result<ToolOutput, ToolError> {
        let attempts = self.context.backoff.attempts();
        let mut last = None;
        for attempt in 0..attempts {
            match self.run_once(argv) {
                Ok(output) => return Ok(output),
                Err(error) if error.is_retryable() && attempt + 1 < attempts => {
                    let delay = self.context.backoff.delay_ms(attempt);
                    tracing::warn!(
                        tool = %self.entry.name,
                        attempt = attempt + 1,
                        attempts,
                        delay_ms = delay,
                        "{error}"
                    );
                    std::thread::sleep(Duration::from_millis(delay));
                    last = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last.unwrap_or_else(|| ToolError::Unavailable {
            tool: self.entry.name.clone(),
            reason: "no attempt was made".to_string(),
        }))
    }

    fn run_once(&self, argv: &[String]) -> Result<ToolOutput, ToolError> {
        let mut child = Command::new(&self.entry.bin)
            .args(argv)
            .current_dir(self.context.root())
            .env_clear()
            .envs(self.context.environment.apply(std::env::vars()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| ToolError::Unavailable {
                tool: self.entry.name.clone(),
                reason: format!("{}: {error}", self.entry.bin.display()),
            })?;

        // Both pipes are drained while the child runs; reading them in turn
        // would deadlock the moment the other one filled up.
        let out = child.stdout.take().map(drain);
        let err = child.stderr.take().map(drain);
        let status = wait_for(&mut child, Duration::from_millis(self.entry.timeout_ms));
        let printed = merge(join(out), join(err));

        match status {
            Ok(Some(status)) => Ok(self.finish(&printed, status)),
            Ok(None) => Err(ToolError::Timeout {
                tool: self.entry.name.clone(),
                timeout_ms: self.entry.timeout_ms,
            }),
            Err(reason) => Err(ToolError::Unavailable {
                tool: self.entry.name.clone(),
                reason,
            }),
        }
    }

    /// A non-zero exit is a result the model gets to read, not a failure of
    /// this stage; being killed by a signal is not, and comes back as one.
    fn finish(&self, printed: &str, status: ExitStatus) -> ToolOutput {
        let visible = self.withhold_denied_lines(printed);
        let clipped = truncate(&visible, self.context.max_output_bytes as usize);
        let mut text = clipped.text;
        if !status.success() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!("[{}]", describe(status)));
        }
        ToolOutput::clipped(text, clipped.omitted_bytes)
    }

    /// `deny_paths` applies to what comes back as well as to what goes in:
    /// a checker that lists files will hand paths over along with content.
    fn withhold_denied_lines(&self, text: &str) -> String {
        let root = format!("{}/", self.context.root().display());
        let withheld: Vec<&str> = text
            .lines()
            .map(|line| match self.names_a_denied_path(line, &root) {
                true => WITHHELD_LINE,
                false => line,
            })
            .collect();
        withheld.join("\n")
    }

    fn names_a_denied_path(&self, line: &str, root: &str) -> bool {
        fragment_pattern().find_iter(line).any(|fragment| {
            let text = fragment.as_str();
            if !text.contains('/') && !text.contains('.') {
                return false;
            }
            let relative = text
                .strip_prefix(root)
                .or_else(|| text.strip_prefix("./"))
                .unwrap_or(text);
            self.context.paths.is_denied(relative)
        })
    }

    fn invalid(&self, reason: impl Into<String>) -> ToolError {
        ToolError::InvalidArguments {
            tool: self.entry.name.clone(),
            reason: reason.into(),
        }
    }
}

/// Anything that looks like a path: at least one dot or slash, and none of
/// the punctuation a diagnostic wraps its paths in.
fn fragment_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"[A-Za-z0-9_.+/-]+").expect("a valid pattern"))
}

fn resolved(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn drain(pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = pipe.take(READ_CEILING).read_to_end(&mut buffer);
        buffer
    })
}

fn join(reader: Option<std::thread::JoinHandle<Vec<u8>>>) -> String {
    let bytes = reader
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// stdout and stderr as one text: gcc and cppcheck put their diagnostics on
/// stderr, and the model should not have to be told which stream said what.
fn merge(out: String, err: String) -> String {
    match (out.is_empty(), err.is_empty()) {
        (true, _) => err,
        (_, true) => out,
        _ if out.ends_with('\n') => format!("{out}{err}"),
        _ => format!("{out}\n{err}"),
    }
}

/// `Ok(None)` is the timeout. Polling rather than blocking is what makes the
/// deadline enforceable without a second crate.
fn wait_for(child: &mut Child, timeout: Duration) -> Result<Option<ExitStatus>, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) => {}
            Err(error) => return Err(error.to_string()),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn describe(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit status {code}"),
        None => format!("killed: {status}"),
    }
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

impl Tool for CommandTool {
    fn name(&self) -> &str {
        &self.entry.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    /// `params` is the schema the model sees, straight from the config.
    fn signature(&self) -> &Signature {
        &self.signature
    }

    /// A checker's answer means something even when it is silent: one this run
    /// could have answered and the model never called says nobody scanned this
    /// chunk.
    fn purpose(&self) -> Purpose {
        Purpose::Check
    }

    fn rounds(&self) -> &'static [Round] {
        &[Round::Investigation]
    }

    fn needs(&self) -> Abilities {
        self.needs
    }

    fn unavailable(&self) -> Option<&str> {
        self.context.refusal(self.needs)
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let checked = self.checked(arguments)?;
        self.refuse_bin_inside_worktree()?;
        let argv = self.argv(&checked)?;
        self.run(&argv)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{ParamKind, ParamSpec, SecuritySettings};

    /// A worktree with one readable file, which is all these tests point at.
    fn worktree() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("temp dir");
        std::fs::write(root.path().join("parse.c"), "int main(void){return 0;}\n")
            .expect("write source");
        root
    }

    fn policy(root: &Path) -> PathPolicy {
        let settings = SecuritySettings {
            deny_paths: vec!["secrets/**".to_string()],
            ..SecuritySettings::for_tests()
        };
        PathPolicy::new(&settings, &[], Some(root)).expect("valid globs")
    }

    fn entry(bin: &str, args: &[&str]) -> ToolEntry {
        let mut params = BTreeMap::new();
        params.insert(
            "path".to_string(),
            ParamSpec {
                kind: ParamKind::Path,
                description: None,
                pattern: None,
                choices: None,
            },
        );
        ToolEntry {
            name: "cppcheck".to_string(),
            description: "static analysis for C and C++".to_string(),
            bin: PathBuf::from(bin),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            params,
            requires_checkout: true,
            requires_build: false,
            timeout_ms: 5_000,
        }
    }

    /// A command tool only ever asks the worktree where it is and what it can
    /// answer: the process it spawns opens the files itself.
    struct RootOnly {
        root: PathBuf,
        reach: crate::worktree::Reach,
    }

    impl RootOnly {
        fn checkout(root: &Path) -> Self {
            Self {
                root: root.to_path_buf(),
                reach: crate::worktree::Reach {
                    content: crate::worktree::Content::Checkout,
                    search: crate::worktree::Search::Regex,
                },
            }
        }

        fn fetched(root: &Path) -> Self {
            Self {
                root: root.to_path_buf(),
                reach: crate::worktree::Reach {
                    content: crate::worktree::Content::Fetched,
                    search: crate::worktree::Search::Regex,
                },
            }
        }
    }

    impl WorktreeSource for RootOnly {
        fn root(&self) -> &Path {
            &self.root
        }

        fn reach(&self) -> crate::worktree::Reach {
            self.reach
        }

        fn open_in(&self, _run_dir: &Path) -> Result<(), crate::worktree::WorktreeError> {
            Ok(())
        }

        fn head_sha(&self) -> Result<Option<String>, crate::worktree::WorktreeError> {
            Ok(None)
        }

        fn supply(&self, _path: &str) -> Result<(), crate::worktree::WorktreeError> {
            Ok(())
        }

        fn list_files(
            &self,
            _glob: &str,
        ) -> Result<crate::worktree::Listing, crate::worktree::WorktreeError> {
            Ok(crate::worktree::Listing::default())
        }

        fn read_file(
            &self,
            _path: &str,
            _lines: Option<crate::worktree::LineRange>,
        ) -> Result<String, crate::worktree::WorktreeError> {
            Ok(String::new())
        }

        fn search(
            &self,
            _query: &str,
            _glob: Option<&str>,
        ) -> Result<Vec<crate::worktree::SearchHit>, crate::worktree::WorktreeError> {
            Ok(Vec::new())
        }
    }

    fn tool_in(root: &Path, entry: ToolEntry) -> CommandTool {
        CommandTool::new(
            entry,
            CommandContext::new(
                EnvPolicy::default(),
                policy(root),
                Arc::new(RootOnly::checkout(root)) as Arc<dyn WorktreeSource>,
                65_536,
                Backoff::new(0),
            ),
        )
    }

    /// Neither of the two tests below reaches the filesystem, so the root
    /// only has to be a path, not a directory.
    fn tool() -> CommandTool {
        tool_in(
            Path::new("/nonexistent/worktree"),
            entry("/usr/bin/cppcheck", &["--quiet", "--", "{path}"]),
        )
    }

    #[test]
    fn a_placeholder_expands_to_exactly_one_argv_element() {
        let mut arguments = Map::new();
        arguments.insert(
            "path".to_string(),
            Value::String("src/parse.c; rm -rf /".to_string()),
        );
        assert_eq!(
            tool().argv(&arguments).unwrap(),
            vec!["--quiet", "--", "src/parse.c; rm -rf /"]
        );
    }

    #[test]
    fn the_schema_shown_to_the_model_comes_from_params() {
        let schema = tool().signature().schema();
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(schema["required"][0], "path");
    }

    /// The child's environment is exactly what `EnvPolicy` let through, so a
    /// `DEEPSEEK_API_KEY` in this process cannot travel into a checker
    /// whatever this machine happens to have set.
    #[test]
    fn a_real_command_runs_in_the_worktree_with_only_the_whitelisted_environment() {
        let root = worktree();
        let tool = tool_in(root.path(), entry("/usr/bin/env", &[]));

        let output = tool
            .execute(&serde_json::json!({"path": "parse.c"}))
            .expect("env runs");

        let mut seen: Vec<String> = output
            .text
            .lines()
            .filter(|line| line.contains('='))
            .map(|line| line.to_string())
            .collect();
        let mut allowed: Vec<String> = EnvPolicy::default()
            .apply(std::env::vars())
            .into_iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        seen.sort();
        allowed.sort();
        assert_eq!(seen, allowed);
        for line in &seen {
            let name = line.split('=').next().unwrap_or_default();
            assert!(
                !name.ends_with("_API_KEY") && !name.ends_with("_TOKEN"),
                "{name} reached the child"
            );
        }
    }

    #[test]
    fn the_command_is_given_the_argv_array_rather_than_a_shell_line() {
        let root = worktree();
        let tool = tool_in(
            root.path(),
            entry("/bin/echo", &["--", "{path}", "; rm -rf /"]),
        );

        let output = tool
            .execute(&serde_json::json!({"path": "parse.c"}))
            .expect("echo runs");

        assert_eq!(output.text.trim(), "-- parse.c ; rm -rf /");
        assert!(
            root.path().join("parse.c").is_file(),
            "the worktree is still there"
        );
    }

    #[test]
    fn a_traversing_path_is_refused_before_anything_is_spawned() {
        let root = worktree();
        let marker = root.path().join("spawned");
        let script = format!("printf x > {}", marker.display());
        let tool = tool_in(
            root.path(),
            entry("/bin/sh", &["-c", script.as_str(), "{path}"]),
        );

        let error = tool
            .execute(&serde_json::json!({"path": "../etc/passwd"}))
            .expect_err("the path check comes first");

        assert!(matches!(error, ToolError::Rejected { .. }), "{error}");
        assert!(error.to_string().contains(".."), "{error}");
        assert!(!marker.exists(), "the command was never spawned");
    }

    #[test]
    fn an_argument_the_schema_does_not_declare_is_answered_not_run() {
        let root = worktree();
        let tool = tool_in(root.path(), entry("/bin/echo", &["{path}"]));
        let error = tool
            .execute(&serde_json::json!({"path": "parse.c", "extra": "1"}))
            .expect_err("unknown argument");
        assert!(
            matches!(error, ToolError::InvalidArguments { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_line_naming_a_denied_path_is_withheld_before_the_model_sees_it() {
        let root = worktree();
        let tool = tool_in(
            root.path(),
            entry(
                "/bin/echo",
                &["parse.c:1: warning: see secrets/deploy.toml", "{path}"],
            ),
        );

        let output = tool
            .execute(&serde_json::json!({"path": "parse.c"}))
            .expect("echo runs");

        assert!(
            !output.text.contains("secrets/deploy.toml"),
            "{}",
            output.text
        );
        assert!(output.text.contains(WITHHELD_LINE), "{}", output.text);
    }

    #[test]
    fn a_non_zero_exit_comes_back_as_a_result_with_its_status() {
        let root = worktree();
        let tool = tool_in(root.path(), entry("/bin/sh", &["-c", "exit 2"]));
        let output = tool
            .execute(&serde_json::json!({"path": "parse.c"}))
            .expect("a failing command is still an answer");
        assert!(output.text.contains("exit status 2"), "{}", output.text);
    }

    #[test]
    fn output_past_the_ceiling_is_clipped_and_says_how_much_is_missing() {
        let root = worktree();
        let mut small = entry("/bin/echo", &["0123456789012345678901234567890123456789"]);
        small.timeout_ms = 5_000;
        let tool = CommandTool::new(
            small,
            CommandContext::new(
                EnvPolicy::default(),
                policy(root.path()),
                Arc::new(RootOnly::checkout(root.path())) as Arc<dyn WorktreeSource>,
                8,
                Backoff::new(0),
            ),
        );

        let output = tool
            .execute(&serde_json::json!({"path": "parse.c"}))
            .expect("echo runs");

        assert_eq!(output.text, "01234567");
        assert!(output.omitted_bytes > 0);
        assert!(
            output.for_model().contains("more bytes not shown"),
            "{}",
            output.for_model()
        );
    }

    #[test]
    fn a_command_that_outlives_its_timeout_is_killed_and_reported() {
        let root = worktree();
        let mut slow = entry("/bin/sleep", &["30"]);
        slow.timeout_ms = 80;
        let tool = tool_in(root.path(), slow);

        let error = tool
            .execute(&serde_json::json!({"path": "parse.c"}))
            .expect_err("the deadline holds");

        assert!(matches!(error, ToolError::Timeout { .. }), "{error}");
        assert!(error.is_retryable());
    }

    /// `requires_checkout` no longer decides whether the checker exists. It is
    /// offered on every run and says, in the description the model reads and
    /// again if it calls anyway, that this run's worktree is not one — and
    /// that this is a fact about the run rather than a clean scan.
    #[test]
    fn a_checker_needing_a_checkout_is_offered_on_a_fetched_worktree_and_refuses() {
        let root = worktree();
        let marker = root.path().join("spawned");
        let script = format!("printf x > {}", marker.display());
        let checker = |worktree: Arc<dyn WorktreeSource>| {
            CommandTool::new(
                entry("/bin/sh", &["-c", script.as_str(), "{path}"]),
                CommandContext::new(
                    EnvPolicy::default(),
                    policy(root.path()),
                    worktree,
                    65_536,
                    Backoff::new(0),
                ),
            )
        };

        let fetched = checker(Arc::new(RootOnly::fetched(root.path())));
        let reason = fetched.unavailable().expect("not a checkout");
        assert!(reason.contains("whole checkout"), "{reason}");
        assert!(reason.contains("not about the code"), "{reason}");
        assert!(
            fetched.description().contains("NOT AVAILABLE THIS RUN"),
            "{}",
            fetched.description()
        );
        assert!(
            fetched.description().starts_with("static analysis"),
            "the config author's own words come first: {}",
            fetched.description()
        );

        let whole = checker(Arc::new(RootOnly::checkout(root.path())));
        assert!(whole.unavailable().is_none());
        assert_eq!(whole.description(), "static analysis for C and C++");
        assert!(!marker.exists(), "neither of them was run");
    }

    #[test]
    fn a_bin_inside_the_worktree_is_refused_even_after_the_config_passed() {
        let root = worktree();
        let bin = root.path().join("checker.sh");
        std::fs::write(&bin, "#!/bin/sh\necho hi\n").expect("write script");
        let tool = tool_in(root.path(), entry(bin.display().to_string().as_str(), &[]));

        let error = tool
            .execute(&serde_json::json!({"path": "parse.c"}))
            .expect_err("the binary is part of what is under review");

        assert!(matches!(error, ToolError::Rejected { .. }), "{error}");
        assert!(error.to_string().contains("inside the worktree"), "{error}");
    }
}
