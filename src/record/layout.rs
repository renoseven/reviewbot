//! File names inside a run directory. Checkpoints are not a separate place;
//! they are these files.

pub const META: &str = "meta.json";
pub const LOCK: &str = "lock";
pub const LOG: &str = "log";
pub const PUBLISHED: &str = "published.json";
pub const REPORT: &str = "report.md";
pub const SUMMARY: &str = "summary.json";

/// `stages/<n>-<stage>.json`, the name later milestones keep overwriting.
pub fn stage_file(number: u8, stage: &str) -> String {
    format!("stages/{number}-{stage}.json")
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
