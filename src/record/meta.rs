//! `meta.json`: what this run is, what it was allowed to spend, and how far
//! it got. Never a credential.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::run_id::InputIdentity;
use crate::budget::{Budget, Price};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Url,
    Diff,
}

/// What was handed in, plus what it resolved to.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct InputRecord {
    pub kind: InputKind,
    /// The URL, the diff file path, or `-` for standard input.
    pub source: String,
    pub identity: InputIdentity,
    /// Empty for diff input without `--worktree`.
    pub head_sha: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Meta {
    pub run_id: String,
    pub input: InputRecord,
    pub model: String,
    pub provider: String,
    pub fingerprint: String,
    /// The frozen ceiling, keeping `-1` recognizable as "no ceiling".
    pub budget_limit: f64,
    pub currency: String,
    pub price: Price,
    pub spent: f64,
    /// Whether this run should post to the MR/PR. Recorded rather than
    /// recomputed, so a run re-entered without `--publish` still knows what it
    /// was asked for.
    pub publish: bool,
    /// Stage names that finished, so a run re-entered can skip them.
    pub completed_stages: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// What names a run, gathered before the run directory exists.
#[derive(Clone, Debug)]
pub struct RunIdentity {
    pub run_id: String,
    pub input: InputRecord,
    pub fingerprint: String,
}

impl Meta {
    /// A brand new run. The budget is frozen here and only spent down after.
    pub fn start(
        identity: RunIdentity,
        model: &str,
        provider: &str,
        budget: &Budget,
        publish: bool,
    ) -> Self {
        let timestamp = now();
        Self {
            run_id: identity.run_id,
            input: identity.input,
            model: model.to_string(),
            provider: provider.to_string(),
            fingerprint: identity.fingerprint,
            budget_limit: budget.limit().as_value(),
            currency: budget.currency().to_string(),
            price: *budget.price(),
            spent: 0.0,
            publish,
            completed_stages: Vec::new(),
            created_at: timestamp,
            updated_at: timestamp,
        }
    }

    pub fn is_complete(&self, stage: &str) -> bool {
        self.completed_stages.iter().any(|name| name == stage)
    }

    pub fn mark_complete(&mut self, stage: &str) {
        if !self.is_complete(stage) {
            self.completed_stages.push(stage.to_string());
        }
        self.updated_at = now();
    }

    /// A checkpoint that will not parse means the stage did not really
    /// finish; drop it and everything after it so the run continues from the
    /// last complete snapshot.
    pub fn mark_incomplete(&mut self, stage: &str) {
        self.completed_stages.retain(|name| name != stage);
        self.updated_at = now();
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}
