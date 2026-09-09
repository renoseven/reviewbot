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
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Map, Value};

use crate::config::{Backoff, ParamKind, ParamSpec, ToolEntry};
use crate::security::{EnvPolicy, Limits, PathPolicy, truncate};

use super::{Origin, Tool, ToolError, ToolOutput};

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
pub struct CommandContext {
    environment: EnvPolicy,
    paths: PathPolicy,
    worktree: PathBuf,
    max_output_bytes: u64,
    backoff: Backoff,
}

impl CommandContext {
    pub fn new(
        environment: EnvPolicy,
        paths: PathPolicy,
        worktree: PathBuf,
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
}

pub struct CommandTool {
    entry: ToolEntry,
    context: CommandContext,
    limits: Limits,
}

impl CommandTool {
    pub fn new(entry: ToolEntry, context: CommandContext) -> Self {
        let limits = Limits::new(entry.timeout_ms, context.max_output_bytes);
        Self {
            entry,
            context,
            limits,
        }
    }

    pub fn bin(&self) -> &PathBuf {
        &self.entry.bin
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Schema for `tool list`, without constructing a runnable tool.
    pub fn parameters_for(entry: &ToolEntry) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for (name, spec) in &entry.params {
            let mut property = Map::new();
            property.insert(
                "type".to_string(),
                Value::String(json_type(spec.kind).to_string()),
            );
            if let Some(description) = &spec.description {
                property.insert(
                    "description".to_string(),
                    Value::String(description.clone()),
                );
            }
            if let Some(pattern) = &spec.pattern {
                property.insert("pattern".to_string(), Value::String(pattern.clone()));
            }
            if let Some(choices) = &spec.choices {
                property.insert(
                    "enum".to_string(),
                    Value::Array(choices.iter().cloned().map(Value::String).collect()),
                );
            }
            properties.insert(name.clone(), Value::Object(property));
            required.push(Value::String(name.clone()));
        }
        serde_json::json!({
            "type": "object",
            "properties": Value::Object(properties),
            "required": Value::Array(required),
            "additionalProperties": false,
        })
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

    /// Every argument against the schema the model was shown, before any
    /// process exists. A rejection is an answer to the model, not a failure
    /// of the review, so the reason has to read like one.
    fn validated(&self, arguments: &Value) -> Result<Map<String, Value>, ToolError> {
        let Some(given) = arguments.as_object() else {
            return Err(self.invalid("the arguments have to be a JSON object"));
        };
        for name in given.keys() {
            if !self.entry.params.contains_key(name) {
                return Err(self.invalid(format!(
                    "unknown argument {name:?}; this tool takes {}",
                    self.parameter_names()
                )));
            }
        }
        let mut checked = Map::new();
        for (name, spec) in &self.entry.params {
            let Some(value) = given.get(name) else {
                return Err(self.invalid(format!("missing argument {name:?}")));
            };
            checked.insert(name.clone(), self.checked_value(name, spec, value)?);
        }
        Ok(checked)
    }

    /// A `path` walks the full path check and comes back repository relative,
    /// so what reaches argv is what the check approved rather than what the
    /// model wrote.
    fn checked_value(
        &self,
        name: &str,
        spec: &ParamSpec,
        value: &Value,
    ) -> Result<Value, ToolError> {
        match spec.kind {
            ParamKind::Path => {
                let given = self.text_of(name, value)?;
                let path = self
                    .context
                    .paths
                    .check_worktree_path(given, &self.context.worktree)
                    .map_err(|rejection| ToolError::Rejected {
                        tool: self.entry.name.clone(),
                        reason: rejection.to_string(),
                    })?;
                self.check_shape(name, spec, &path)?;
                Ok(Value::String(path))
            }
            ParamKind::String => {
                let given = self.text_of(name, value)?;
                self.check_shape(name, spec, given)?;
                Ok(value.clone())
            }
            ParamKind::Integer if value.is_i64() || value.is_u64() => Ok(value.clone()),
            ParamKind::Number if value.is_number() => Ok(value.clone()),
            ParamKind::Boolean if value.is_boolean() => Ok(value.clone()),
            other => Err(self.invalid(format!(
                "argument {name:?} has to be {}",
                match other {
                    ParamKind::Integer => "an integer",
                    ParamKind::Number => "a number",
                    _ => "a boolean",
                }
            ))),
        }
    }

    fn text_of<'a>(&self, name: &str, value: &'a Value) -> Result<&'a str, ToolError> {
        value
            .as_str()
            .ok_or_else(|| self.invalid(format!("argument {name:?} has to be a string")))
    }

    fn check_shape(&self, name: &str, spec: &ParamSpec, value: &str) -> Result<(), ToolError> {
        if let Some(choices) = &spec.choices
            && !choices.iter().any(|choice| choice == value)
        {
            return Err(self.invalid(format!(
                "argument {name:?} has to be one of {}",
                choices.join(", ")
            )));
        }
        if let Some(pattern) = &spec.pattern {
            let compiled = Regex::new(pattern).map_err(|error| ToolError::Unavailable {
                tool: self.entry.name.clone(),
                reason: format!("the pattern configured for {name:?} will not compile: {error}"),
            })?;
            if !compiled.is_match(value) {
                return Err(self.invalid(format!(
                    "argument {name:?} does not match the pattern {pattern}"
                )));
            }
        }
        Ok(())
    }

    /// The config already refused a `bin` inside the repository, but the
    /// worktree is only known now, so the same rule is applied again here.
    fn refuse_bin_inside_worktree(&self) -> Result<(), ToolError> {
        let root = resolved(&self.context.worktree);
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
            .current_dir(&self.context.worktree)
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
        let root = format!("{}/", self.context.worktree.display());
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

    fn parameter_names(&self) -> String {
        self.entry
            .params
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
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
        &self.entry.description
    }

    /// `params` is the schema the model sees, straight from the config.
    fn parameters(&self) -> Value {
        Self::parameters_for(&self.entry)
    }

    fn origin(&self) -> Origin {
        Origin::Config
    }

    /// An external command runs with its cwd at the worktree root, so there
    /// is nowhere to run one without a worktree, whatever the entry says.
    fn requires_worktree(&self) -> bool {
        true
    }

    fn requires_build(&self) -> bool {
        self.entry.requires_build
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let checked = self.validated(arguments)?;
        self.refuse_bin_inside_worktree()?;
        let argv = self.argv(&checked)?;
        self.run(&argv)
    }
}

/// `path` is a JSON string with the full path check applied on top.
fn json_type(kind: ParamKind) -> &'static str {
    match kind {
        ParamKind::Path | ParamKind::String => "string",
        ParamKind::Integer => "integer",
        ParamKind::Number => "number",
        ParamKind::Boolean => "boolean",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{ParamSpec, SecuritySettings};

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
            requires_worktree: true,
            requires_build: false,
            timeout_ms: 5_000,
        }
    }

    fn tool_in(root: &Path, entry: ToolEntry) -> CommandTool {
        CommandTool::new(
            entry,
            CommandContext::new(
                EnvPolicy::default(),
                policy(root),
                root.to_path_buf(),
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
        let schema = tool().parameters();
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
                root.path().to_path_buf(),
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
