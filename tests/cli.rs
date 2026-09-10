//! The CLI contract: which stream gets what, and which exit code comes back.
//! These are the only things that need a subprocess to verify.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime};

use assert_cmd::prelude::*;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn example_config() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/config/example.toml")
}

fn reviewbot() -> Command {
    let mut command = Command::cargo_bin("reviewbot").expect("binary is built");
    command.env("DEEPSEEK_API_KEY", "not-a-real-key");
    command
}

#[test]
fn config_check_validates_locally_and_writes_nothing_to_stderr() {
    let output = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["config", "check"])
        .output()
        .expect("run");

    assert!(output.status.success(), "{output:?}");
    assert!(
        output.stderr.is_empty(),
        "a successful run writes no stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = fixture("valid.toml");
    assert_eq!(
        stdout,
        format!(
            "Path:       {}\n\
             Provider:   deepseek\n\
             Credential: readable\n\
             Budget:     10.00 CNY\n\
             Platforms:  1\n\
             Models:     2\n\
             Tools:      2\n\
             ok\n",
            path.display()
        )
    );
}

#[test]
fn config_check_in_json_prints_parsable_json_and_nothing_else() {
    let output = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["--format", "json", "config", "check"])
        .output()
        .expect("run");

    assert!(output.status.success(), "{output:?}");
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is pure json");
    assert_eq!(parsed["model"], "deepseek-v4-flash");
    assert_eq!(parsed["selected_by"], "default = true");
}

#[test]
fn quiet_leaves_stdout_empty_on_success() {
    let output = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["-q", "config", "check"])
        .output()
        .expect("run");

    assert!(output.status.success());
    assert!(output.stdout.is_empty(), "-q writes no stdout");
}

#[test]
fn a_missing_config_exits_two_and_names_the_path_on_stderr() {
    let output = reviewbot()
        .args(["--config", "/nonexistent/reviewbot.toml"])
        .args(["config", "check"])
        .output()
        .expect("run");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("/nonexistent/reviewbot.toml"), "{stderr}");
}

#[test]
fn an_unreadable_credential_is_a_config_error() {
    let output = Command::cargo_bin("reviewbot")
        .expect("binary is built")
        .env_remove("DEEPSEEK_API_KEY")
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["config", "check"])
        .output()
        .expect("run");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("DEEPSEEK_API_KEY"), "{stderr}");
}

#[test]
fn a_diff_input_with_publish_fails_before_anything_is_written() {
    let directory = tempfile::tempdir().expect("temp dir");
    let diff = fixture("change.diff");
    let runs = directory.path().join("runs");

    let output = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["--runs-dir", runs.to_str().unwrap()])
        .args(["review", "--publish", diff.to_str().unwrap()])
        .output()
        .expect("run");

    assert_eq!(output.status.code(), Some(2));
    assert!(!runs.exists(), "no run directory was created");
}

#[test]
fn the_input_format_is_judged_by_content_not_by_extension() {
    let directory = tempfile::tempdir().expect("temp dir");
    let runs = directory.path().join("runs");
    // The same bytes the fixture holds, so the mbox this wraps differs from a
    // real diff only by the header git format-patch puts on it.
    let diff = std::fs::read_to_string(fixture("change.diff")).expect("fixture");

    // git format-patch output is refused outright rather than stripped.
    let mbox = directory.path().join("series.patch");
    std::fs::write(
        &mbox,
        format!(
            "From {} Mon Sep 17 00:00:00 2001\nFrom: A <a@example.com>\nSubject: [PATCH] fix\n\n{diff}",
            "0".repeat(40)
        ),
    )
    .expect("write mbox");

    let refused = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["--runs-dir", runs.to_str().unwrap()])
        .args(["review", mbox.to_str().unwrap()])
        .output()
        .expect("run");
    assert_eq!(refused.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("only unified diff"), "{stderr}");

    // The same extension holding a real unified diff gets past the parser
    // and only stops later, where the model call is still a stub.
    let accepted_path = directory.path().join("change.patch");
    std::fs::write(&accepted_path, diff).expect("write diff");
    let accepted = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["--runs-dir", runs.to_str().unwrap()])
        .args(["review", accepted_path.to_str().unwrap()])
        .output()
        .expect("run");
    let stderr = String::from_utf8_lossy(&accepted.stderr);
    assert!(
        !stderr.contains("only unified diff"),
        "a .patch file holding a unified diff is accepted: {stderr}"
    );
}

/// A subprocess is the lowest layer that can prove stdout is a pipe. A zero
/// budget keeps this offline while still completing all six stages and
/// producing the ordinary final summary after the retained progress history.
#[test]
fn a_piped_review_appends_progress_then_the_text_summary() {
    let directory = tempfile::tempdir().expect("temp dir");
    let config = directory.path().join("reviewbot.toml");
    let configured = std::fs::read_to_string(fixture("valid.toml"))
        .expect("fixture")
        .replace("budget_per_run = 10.0", "budget_per_run = 0.0");
    std::fs::write(&config, configured).expect("config");
    let diff = fixture("change.diff");
    let runs = directory.path().join("runs");

    let output = reviewbot()
        .args(["--config", config.to_str().unwrap()])
        .args(["--runs-dir", runs.to_str().unwrap()])
        .args(["review", diff.to_str().unwrap()])
        .output()
        .expect("run");

    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(output.stderr.is_empty(), "finished runs write no stderr");
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    let tracing_line = "input normalized into a change set";
    assert!(
        !stdout.contains(tracing_line),
        "tracing never reaches stdout: {stdout}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains(tracing_line),
        "tracing never reaches stderr"
    );
    assert!(
        stdout.contains("[1/6] input     1 file\n"),
        "the pipe retains finished stages: {stdout}"
    );
    assert!(
        stdout.contains("[6/6] publish   nothing posted"),
        "all six stages reach the pipe: {stdout}"
    );

    let summary = stdout
        .split_once("run_id     ")
        .map(|(_, summary)| summary)
        .expect("final text summary");
    let fields: Vec<&str> = summary.lines().collect();
    assert!(fields[0].len() >= 16, "run id has a value: {summary}");
    assert_eq!(fields[1], "model      deepseek-v4-flash");
    assert!(fields.iter().any(|line| line.starts_with("comments   ")));
    assert!(fields.iter().any(|line| line.starts_with("budget     ")));
    assert!(fields.iter().any(|line| line.starts_with("report     ")));
    assert!(fields.iter().any(|line| line.starts_with("summary    ")));
    assert_eq!(
        fields.iter().find(|line| line.starts_with("overall")),
        Some(&"overall    not scored"),
        "the verdict line carries the verdict and not the reason: {summary}"
    );
    // What went wrong reads last, in the same shape every other error takes:
    // a blank line, then `error:`.
    assert_eq!(
        &fields[fields.len() - 2..],
        [
            "",
            "error: budget is 0 CNY: this run may not spend anything"
        ],
        "why the run is incomplete is the final line: {summary}"
    );
    assert_eq!(
        fields
            .iter()
            .filter(|line| line.contains("this run may not spend anything"))
            .count(),
        1,
        "and it is said once: {summary}"
    );

    let run_dir = std::fs::read_dir(&runs)
        .expect("runs")
        .next()
        .expect("one run")
        .expect("run entry")
        .path();
    let log = std::fs::read_to_string(run_dir.join("log")).expect("run log");
    assert!(
        log.contains(tracing_line),
        "the run log contains tracing output: {log}"
    );
}

/// Invalidating part of a run is worth a warning, and the warning goes
/// where every other line of tracing goes: the run's own log. Not stdout,
/// which carries the progress and the summary, and not stderr, which
/// carries the one sentence that ended the process — and this did not end
/// it, since a changed config re-enters the run rather than refusing it.
#[test]
fn a_config_change_leaves_a_line_in_the_run_log_saying_what_it_cost() {
    let directory = tempfile::tempdir().expect("temp dir");
    let config = directory.path().join("reviewbot.toml");
    let configured = std::fs::read_to_string(fixture("valid.toml"))
        .expect("fixture")
        .replace("budget_per_run = 10.0", "budget_per_run = 0.0");
    std::fs::write(&config, &configured).expect("config");
    let diff = fixture("change.diff");
    let runs = directory.path().join("runs");

    let review = || {
        reviewbot()
            .args(["--config", config.to_str().unwrap()])
            .args(["--runs-dir", runs.to_str().unwrap()])
            .args(["review", diff.to_str().unwrap()])
            .output()
            .expect("run")
    };
    let first = review();
    assert_eq!(first.status.code(), Some(3), "{first:?}");

    std::fs::write(
        &config,
        configured.replace("max_chunk_tokens = 24000", "max_chunk_tokens = 12000"),
    )
    .expect("rewrite config");
    let second = review();

    assert_eq!(
        second.status.code(),
        Some(3),
        "the run continues rather than refusing: {second:?}"
    );
    assert!(second.stderr.is_empty(), "no tracing reaches stderr");
    let run_dir = std::fs::read_dir(&runs)
        .expect("runs")
        .next()
        .expect("one run, because the settings are not in its id")
        .expect("run entry")
        .path();
    let log = std::fs::read_to_string(run_dir.join("log")).expect("run log");
    assert!(
        log.contains("the triage settings changed since this run was recorded")
            && log.contains("running triage and every stage after it again"),
        "the log names the slice and where the rerun starts: {log}"
    );
    assert!(
        !String::from_utf8_lossy(&second.stdout).contains("settings changed"),
        "and it is not on stdout either"
    );
}

#[test]
fn a_target_that_is_neither_a_url_nor_a_file_says_so() {
    let output = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["review", "/nonexistent/change.diff"])
        .output()
        .expect("run");

    assert_ne!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot open"), "{stderr}");
}

#[test]
fn run_list_json_is_an_object_with_runs_dir() {
    let directory = tempfile::tempdir().expect("temp dir");
    let runs = directory.path().join("runs");
    std::fs::create_dir_all(&runs).expect("runs dir");

    let output = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["--runs-dir", runs.to_str().unwrap()])
        .args(["--format", "json", "run", "list"])
        .output()
        .expect("run");

    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "success writes no stderr");
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is pure json");
    assert!(parsed["runs_dir"].is_string(), "{parsed}");
    assert!(parsed["runs"].is_array(), "{parsed}");
    assert!(
        parsed["runs_dir"].as_str().unwrap().ends_with("runs"),
        "{}",
        parsed["runs_dir"]
    );
}

#[test]
fn config_info_json_is_parseable() {
    let output = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["--format", "json", "config", "info"])
        .output()
        .expect("run");

    assert!(output.status.success(), "{output:?}");
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is pure json");
    assert!(parsed["models"].is_array(), "{parsed}");
    assert_eq!(parsed["models"][0]["name"], "deepseek-v4-flash");
    assert_eq!(parsed["models"][0]["currency"], "CNY");
    assert!(parsed["platforms"].is_array(), "{parsed}");
    assert!(parsed["providers"].is_array(), "{parsed}");
    assert!(parsed["tools"].is_array(), "{parsed}");
}

#[test]
fn config_info_text_has_every_section_and_the_file_field_names() {
    let output = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["config", "info"])
        .output()
        .expect("run");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for heading in [
        "PLATFORMS\n+",
        "PROVIDERS\n+",
        "MODELS\n+",
        "TOOLS\n+",
        "+-",
        "BASE_URL",
        "TOKEN_FROM",
        "BUDGET_PER_RUN",
        "KEY_FROM",
        "IN_1M",
        "CACHED_1M",
        "OUT_1M",
        "MAX_OUT",
        "PURPOSE",
        "ROUNDS",
    ] {
        assert!(
            stdout.contains(heading),
            "missing column {heading}: {stdout}"
        );
    }
    assert!(stdout.contains("3.00 CNY"), "{stdout}");
    assert!(stdout.contains("0.10 CNY"), "{stdout}");
    assert!(stdout.contains("9.00 CNY"), "{stdout}");
    assert!(
        !stdout.contains("BASE URL"),
        "a multi-word title is joined with _: {stdout}"
    );
    assert!(
        !stdout.contains("HOST") && !stdout.contains("KIND"),
        "host and kind are derived, not columns: {stdout}"
    );
}

#[test]
fn run_list_text_aligns_spent_with_currency_and_utc() {
    let directory = tempfile::tempdir().expect("temp dir");
    let run = directory.path().join("runs").join("2dc40c10f9a4d1b8");
    std::fs::create_dir_all(&run).expect("run dir");
    std::fs::write(
        run.join("meta.json"),
        r#"{
            "run_id": "2dc40c10f9a4d1b8",
            "input": {
                "kind": "diff",
                "source": "1.diff",
                "identity": {"kind": "diff", "content_sha256": "abc"},
                "head_sha": ""
            },
            "model": "deepseek-v4-flash",
            "provider": "deepseek",
            "fingerprint": {"input": "i", "triage": "t", "review": "r"},
            "budget_limit": 10.0,
            "currency": "CNY",
            "price": {"input_per_1m_tokens": 3.0, "cached_input_per_1m_tokens": 0.1, "output_per_1m_tokens": 9.0},
            "spent": 0.0316,
            "publish": false,
            "completed_through": "publish",
            "created_at": 1788936240,
            "updated_at": 1788936240
        }"#,
    )
    .expect("meta");

    let output = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["--runs-dir", run.parent().unwrap().to_str().unwrap()])
        .args(["run", "list"])
        .output()
        .expect("run");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Runs dir"), "{stdout}");
    let header = stdout.lines().nth(1).expect("table header");
    assert!(header.contains("RUN ID"), "{header}");
    assert!(header.contains("SPENT"), "{header}");
    assert!(header.contains("UPDATED"), "{header}");
    assert!(stdout.contains("0.0316 CNY"), "{stdout}");
    assert!(stdout.contains("2026-09-09 06:44:00 UTC"), "{stdout}");
    assert!(
        stdout.contains("input, triage, review, merge, report, publish"),
        "{stdout}"
    );
}

#[test]
fn config_info_prints_tool_contracts_the_way_tool_list_did() {
    let output = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["config", "info"])
        .output()
        .expect("run");

    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "success writes no stderr");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("NAME"), "{stdout}");
    assert!(stdout.contains("PURPOSE"), "{stdout}");
    assert!(stdout.contains("ROUNDS"), "{stdout}");
    assert!(stdout.contains("search_local_regex"), "{stdout}");
    assert!(stdout.contains("read_local_file"), "{stdout}");
    assert!(stdout.contains("cppcheck"), "{stdout}");
    assert!(stdout.contains("typecheck"), "{stdout}");
    assert!(stdout.contains("investigation"), "{stdout}");
    assert!(stdout.contains("scoring"), "{stdout}");
    assert!(!stdout.contains("NEEDS"), "needs stay in json: {stdout}");
    assert!(!stdout.contains("ABOUT"), "about stays in json: {stdout}");
    assert!(
        !stdout.contains("Submit one finding"),
        "descriptions stay in json: {stdout}"
    );
    assert!(!stdout.contains("registered"), "{stdout}");
}

#[test]
fn run_prune_dry_run_lists_and_does_not_delete() {
    let directory = tempfile::tempdir().expect("temp dir");
    let runs = directory.path().join("runs");
    for name in ["old", "mid", "new"] {
        let dir = runs.join(name);
        std::fs::create_dir_all(&dir).expect("run dir");
        std::fs::write(dir.join("report.md"), name).expect("report");
    }
    // Distinct mtimes so prune has an order.
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    let mid = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
    let new = SystemTime::UNIX_EPOCH + Duration::from_secs(300);
    std::fs::File::open(runs.join("old"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    std::fs::File::open(runs.join("mid"))
        .unwrap()
        .set_modified(mid)
        .unwrap();
    std::fs::File::open(runs.join("new"))
        .unwrap()
        .set_modified(new)
        .unwrap();

    let output = reviewbot()
        .args(["--runs-dir", runs.to_str().unwrap()])
        .args(["run", "prune", "--keep-latest", "2", "--dry-run"])
        .output()
        .expect("run");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout, "would prune 1 run, retaining the 2 most recent.\n");
    assert!(runs.join("old").join("report.md").is_file());
    assert!(runs.join("mid").is_dir());
    assert!(runs.join("new").is_dir());

    let done = reviewbot()
        .args(["--runs-dir", runs.to_str().unwrap()])
        .args(["run", "prune", "--keep-latest", "2"])
        .output()
        .expect("run");
    assert!(done.status.success(), "{done:?}");
    assert_eq!(
        String::from_utf8_lossy(&done.stdout),
        "pruned 1 run, retaining the 2 most recent.\n"
    );
    assert!(!runs.join("old").exists());
    assert!(runs.join("mid").is_dir());
    assert!(runs.join("new").is_dir());
}

#[test]
fn quiet_json_still_prints_json() {
    let output = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["-q", "--format", "json", "config", "info"])
        .output()
        .expect("run");

    assert!(output.status.success(), "{output:?}");
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("json wins over -q");
    assert!(parsed["models"].is_array());
}

#[test]
fn the_example_config_passes_config_check_with_dummy_env() {
    let output = reviewbot()
        .env("GITLAB_TOKEN", "not-a-real-token")
        .env("GITHUB_TOKEN", "not-a-real-token")
        .args(["--config", example_config().to_str().unwrap()])
        .args(["config", "check"])
        .output()
        .expect("run");

    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "success writes no stderr");
}

#[test]
fn an_unknown_subcommand_exits_two() {
    let output = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["nosuch"])
        .output()
        .expect("run");

    assert_eq!(output.status.code(), Some(2));
}

/// A reference that does not resolve is the user's typo, not reviewbot
/// falling over, and a CI job branching on the exit code has to be able to
/// tell those apart. Exercised through the binary because the exit code is
/// the thing being asserted.
#[test]
fn a_reference_that_does_not_resolve_is_a_config_error_not_an_unexpected_one() {
    let target = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["review", "/nonexistent/change.diff"])
        .output()
        .expect("run");
    assert_eq!(target.status.code(), Some(2), "{target:?}");

    let run = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["run", "show", "deadbeef"])
        .output()
        .expect("run");
    assert_eq!(run.status.code(), Some(2), "{run:?}");

    let worktree = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["review", fixture("change.diff").to_str().unwrap()])
        .args(["--worktree", "/nonexistent/checkout"])
        .output()
        .expect("run");
    assert_eq!(worktree.status.code(), Some(2), "{worktree:?}");
}

/// Argument parsing is the one error path that does not come back through
/// the library, so it is the one that can quietly stop looking like the
/// others. `--help` and `--version` arrive the same way and must not be
/// dressed up as failures.
#[test]
fn a_parse_error_is_set_off_like_every_other_error_and_help_is_not() {
    let refused = reviewbot().args(["review"]).output().expect("run");
    assert_eq!(refused.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.starts_with("error: "),
        "a parse error opens like the rest: {stderr:?}"
    );
    assert!(refused.stdout.is_empty(), "and says nothing on stdout");

    for flag in ["--help", "--version"] {
        let asked = reviewbot().args([flag]).output().expect("run");
        assert_eq!(asked.status.code(), Some(0), "{flag}");
        assert!(asked.stderr.is_empty(), "{flag} is not a failure");
        assert!(
            !String::from_utf8_lossy(&asked.stdout).starts_with('\n'),
            "{flag} is not set off like an error"
        );
    }
}

/// The token column names where the credential comes from, never the
/// credential: an inline secret is refused at parse time, so a config that
/// got this far has only a source to show.
#[test]
fn config_info_names_platform_hosts_and_provider_budgets() {
    let output = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["config", "info"])
        .output()
        .expect("run");

    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "success writes no stderr");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("TOKEN_FROM"), "{stdout}");
    assert!(stdout.contains("gitlab.com"), "{stdout}");
    assert!(stdout.contains("gitlab"), "the resolved kind: {stdout}");
    assert!(stdout.contains("GITLAB_TOKEN"), "{stdout}");
    assert!(stdout.contains("BUDGET_PER_RUN"), "{stdout}");
    assert!(stdout.contains("10.00 CNY"), "{stdout}");

    let json = reviewbot()
        .args(["--config", fixture("valid.toml").to_str().unwrap()])
        .args(["--format", "json", "config", "info"])
        .output()
        .expect("run");
    let parsed: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("stdout is pure json");
    assert_eq!(parsed["providers"][0]["protocol"], "openai");
    assert_eq!(parsed["providers"][0]["currency"], "CNY");
    assert_eq!(parsed["providers"][0]["api_key"], "DEEPSEEK_API_KEY");
}

#[test]
fn config_init_writes_the_example_and_prints_the_path() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("nested").join("config.toml");

    let output = reviewbot()
        .args(["--config", path.to_str().unwrap()])
        .args(["config", "init"])
        .output()
        .expect("run");

    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "success writes no stderr");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(path.to_str().unwrap()), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(&path).expect("written"),
        reviewbot::config::EXAMPLE_CONFIG
    );

    let refused = reviewbot()
        .args(["--config", path.to_str().unwrap()])
        .args(["config", "init"])
        .output()
        .expect("run");
    assert_eq!(refused.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.starts_with("error: "),
        "nothing was printed before this, so it is not set off: {stderr:?}"
    );
    assert!(stderr.contains("will not overwrite"), "{stderr}");
}

#[test]
fn run_remove_deletes_one_run_and_prints_nothing() {
    let directory = tempfile::tempdir().expect("temp dir");
    let runs = directory.path().join("runs");
    std::fs::create_dir_all(runs.join("keep")).expect("keep");
    std::fs::create_dir_all(runs.join("gone")).expect("gone");
    std::fs::write(runs.join("gone").join("report.md"), "gone").expect("report");

    let output = reviewbot()
        .args(["--runs-dir", runs.to_str().unwrap()])
        .args(["run", "remove", "gone"])
        .output()
        .expect("run");

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "success writes no stdout");
    assert!(output.stderr.is_empty(), "success writes no stderr");
    assert!(!runs.join("gone").exists());
    assert!(runs.join("keep").is_dir());

    let missing = reviewbot()
        .args(["--runs-dir", runs.to_str().unwrap()])
        .args(["run", "remove", "gone"])
        .output()
        .expect("run");
    assert_eq!(missing.status.code(), Some(2), "{missing:?}");
}
