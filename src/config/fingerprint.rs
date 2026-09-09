//! The identity of a set of settings. Two runs with the same fingerprint may
//! share a run directory; anything else must not.

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::file::Config;

/// Everything that changes the conclusion. Secret *values* never appear here
/// (the config only holds their source), and neither do artifact locations
/// (`--output-dir`, `--runs-dir`) or run parameters (`--retries`, `-v`, `-q`,
/// `--format`, `--publish`).
#[derive(Serialize)]
struct Fingerprinted<'a> {
    config: &'a Config,
    model: Option<&'a str>,
    worktree: bool,
}

/// `model` is the `--model` value as given on the command line, `worktree`
/// is whether `--worktree` was given at all (the content source mode).
pub fn fingerprint(config: &Config, model: Option<&str>, worktree: bool) -> String {
    let canonical = serde_json::to_vec(&Fingerprinted {
        config,
        model,
        worktree,
    })
    .expect("config is serializable");
    let digest = Sha256::digest(&canonical);
    format!("{digest:x}")
}
