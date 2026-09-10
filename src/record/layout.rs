//! File names inside a run directory. Checkpoints are not a separate place;
//! they are these files.

use crate::domain::Stage;

pub const META: &str = "meta.json";
pub const LOCK: &str = "lock";
pub const LOG: &str = "log";
pub const PUBLISHED: &str = "published.json";
pub const REPORT: &str = "report.md";
pub const SUMMARY: &str = "summary.json";

/// Files fetched at the reviewed commit. The worktree cache lives here when
/// this run has a repository and no command-line checkout.
pub const CACHE: &str = "cache";

/// Writable cwd for checkers. Empty except for whatever a checker drops;
/// they must not write into the checkout or the cache.
pub const CHECKS: &str = "checks";

/// `stages/<n>-<stage>.json`, the name later milestones keep overwriting.
pub fn stage_file(stage: Stage) -> String {
    format!("stages/{}-{stage}.json", stage.number())
}

pub fn trace_file(trace_id: &str) -> String {
    format!("traces/{trace_id}.json")
}

/// The two artifacts `--output-dir` exports carry the run id in their names,
/// because that directory may hold several runs.
pub fn exported_report(run_id: &str) -> String {
    format!("report-{run_id}.md")
}

pub fn exported_summary(run_id: &str) -> String {
    format!("summary-{run_id}.json")
}
