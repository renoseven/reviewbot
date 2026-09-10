//! `meta.json`: what this run is, what it was allowed to spend, and how far
//! it got. Never a credential.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::domain::Stage;

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

impl InputRecord {
    /// One line naming what is under review, for whoever is watching the run.
    /// Read off the identity rather than off `source`, because the identity
    /// is what the run is keyed by: two paths to the same merge request are
    /// one run and read as one line. A diff has no locator of its own, so it
    /// is named by where its bytes came from — which is all `source` ever is
    /// for a diff.
    pub fn describe(&self) -> String {
        match &self.identity {
            InputIdentity::Platform {
                host,
                project,
                number,
            } => format!("{host}/{project} #{number}"),
            InputIdentity::Diff { .. } if self.source == "-" => {
                "a diff on standard input".to_string()
            }
            InputIdentity::Diff { .. } => self.source.clone(),
        }
    }
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
    /// How far this run got: this stage finished, and so did every stage
    /// before it. One value rather than a set, because the stages are walked
    /// in order and so progress can only ever be a prefix — and because
    /// dropping a stage has to drop everything after it, which a set cannot
    /// say. A run recorded before this field existed reads as "nothing
    /// finished" and is worked out again rather than half trusted.
    #[serde(default)]
    pub completed_through: Option<Stage>,
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
            completed_through: None,
            created_at: timestamp,
            updated_at: timestamp,
        }
    }

    pub fn is_complete(&self, stage: Stage) -> bool {
        self.completed_through.is_some_and(|done| stage <= done)
    }

    /// Stages finish in order, so the run keeps the furthest one it reached.
    pub fn mark_complete(&mut self, stage: Stage) {
        self.completed_through = Some(self.completed_through.map_or(stage, |done| done.max(stage)));
        self.updated_at = now();
    }

    /// A checkpoint that will not parse means the stage did not really
    /// finish; drop it and everything after it so the run continues from the
    /// last complete snapshot. Later stages went on what this one produced,
    /// so keeping them would mean trusting conclusions drawn from a file
    /// nobody can read.
    pub fn mark_incomplete(&mut self, stage: Stage) {
        self.completed_through = stage.previous();
        self.updated_at = now();
    }

    /// The stages that finished, for anything that shows a run to a person.
    pub fn completed_stages(&self) -> Vec<Stage> {
        self.completed_through
            .map(|done| done.through().collect())
            .unwrap_or_default()
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}
