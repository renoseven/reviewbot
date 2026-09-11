//! `meta.json`: what this run is, what it was allowed to spend, and how far
//! it got. Never a credential.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::domain::Stage;

use super::run_id::InputIdentity;
use crate::budget::{Budget, Price};
use crate::config::Fingerprint;

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
    pub fn run_id(&self) -> String {
        self.identity.run_id(&self.head_sha)
    }

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
    /// The settings this run's checkpoints were written under, one digest
    /// per stage that reads settings of its own. Three rather than one so
    /// that a change invalidates the stages it can reach and no others.
    pub fingerprint: Fingerprint,
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

/// What names a run, gathered before the run directory exists. The same
/// value whether the directory turns out to be new or to hold a run
/// already, so the two paths cannot disagree about what is being asked for.
#[derive(Clone, Debug)]
pub struct RunIdentity {
    pub run_id: String,
    pub input: InputRecord,
    pub fingerprint: Fingerprint,
}

/// What a directory that already holds a run says about being re-entered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reentry {
    /// Same input, same settings: every checkpoint still answers the
    /// question being asked, so the run carries on where it stopped.
    Resumed,
    /// A settings slice changed. `from` is the stage that slice is named
    /// after: it and everything after it are dropped, and the run goes on
    /// under the settings in hand rather than refusing or opening a second
    /// directory beside this one.
    Invalidated { from: Stage },
    /// This directory records another change, or the same one at another
    /// commit. Nothing in it can be continued, so the caller has to fail:
    /// `recorded` is what it does hold, which is the only useful thing to
    /// say to whoever pointed `--run-id` here.
    OtherInput { recorded: String },
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

    /// Walk into a directory that already holds a run.
    ///
    /// The input is checked first and refusing is the only answer: an
    /// explicit `--run-id` is the one way to aim a run at a directory it has
    /// nothing to do with, and continuing on another change's checkpoints
    /// would review the wrong thing under this one's name.
    ///
    /// Then the settings, slice by slice. The earliest one that differs
    /// names the stage the rerun starts from; that stage and everything
    /// after it are dropped, the new slices are written down, and the run
    /// goes on. `--publish` is not in any slice and is recorded separately
    /// on every entry, so reading a report and then asking for it to be
    /// posted stays one run and one model bill.
    pub fn reenter(
        &mut self,
        identity: &RunIdentity,
        model: &str,
        provider: &str,
        budget: &Budget,
    ) -> Reentry {
        if self.input.identity != identity.input.identity
            || self.input.head_sha != identity.input.head_sha
        {
            return Reentry::OtherInput {
                recorded: self.input.describe(),
            };
        }
        let Some(from) = self.fingerprint.earliest_change(&identity.fingerprint) else {
            return Reentry::Resumed;
        };
        tracing::warn!(
            stage = %from,
            "the {from} settings changed since this run was recorded; \
             running {from} and every stage after it again"
        );
        self.mark_incomplete(from);
        self.fingerprint = identity.fingerprint.clone();
        // Which model is asked, what it costs and what the run may spend all
        // come off the `review` slice and nothing else, so they can only
        // differ when that is the slice that changed — and then going on
        // under the settings in hand means going on under these.
        self.adopt(model, provider, budget);
        Reentry::Invalidated { from }
    }

    /// The five facts the `review` slice settles — the same ones `start`
    /// writes down for a new run — set together because they are read off
    /// one slice and so can only change together.
    fn adopt(&mut self, model: &str, provider: &str, budget: &Budget) {
        self.model = model.to_string();
        self.provider = provider.to_string();
        self.budget_limit = budget.limit().as_value();
        self.currency = budget.currency().to_string();
        self.price = *budget.price();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::Limit;

    fn fingerprint() -> Fingerprint {
        Fingerprint {
            input: "i".to_string(),
            plan: "t".to_string(),
            review: "r".to_string(),
        }
    }

    fn budget(limit: f64) -> Budget {
        Budget::restore(
            Limit::from_value(limit).expect("a limit"),
            "CNY".to_string(),
            Price {
                input_per_1m_tokens: 2.0,
                cached_input_per_1m_tokens: None,
                output_per_1m_tokens: 3.0,
            },
            0.0,
        )
    }

    fn identity(number: u64, head_sha: &str) -> RunIdentity {
        RunIdentity {
            run_id: "abcd".to_string(),
            input: InputRecord {
                kind: InputKind::Url,
                source: format!("https://gitlab.com/acme/app/-/merge_requests/{number}"),
                identity: InputIdentity::Platform {
                    host: "gitlab.com".to_string(),
                    project: "acme/app".to_string(),
                    number,
                },
                head_sha: head_sha.to_string(),
            },
            fingerprint: fingerprint(),
        }
    }

    fn finished() -> Meta {
        let mut meta = Meta::start(
            identity(128, "4b1e0d2"),
            "flash",
            "deepseek",
            &budget(10.0),
            false,
        );
        meta.mark_complete(Stage::Publish);
        meta
    }

    #[test]
    fn matching_settings_resume_every_checkpoint() {
        let mut meta = finished();
        assert_eq!(
            meta.reenter(
                &identity(128, "4b1e0d2"),
                "flash",
                "deepseek",
                &budget(10.0)
            ),
            Reentry::Resumed
        );
        assert_eq!(meta.completed_through, Some(Stage::Publish));
    }

    /// The whole of the invalidation rule: the slice that changed names the
    /// stage, and the stage takes everything after it with it.
    #[test]
    fn a_changed_slice_drops_its_stage_and_everything_after_it() {
        for (change, from, survives) in [
            (
                Fingerprint {
                    plan: "other".to_string(),
                    ..fingerprint()
                },
                Stage::Plan,
                Some(Stage::Input),
            ),
            (
                Fingerprint {
                    review: "other".to_string(),
                    ..fingerprint()
                },
                Stage::Review,
                Some(Stage::Plan),
            ),
            (
                Fingerprint {
                    input: "other".to_string(),
                    ..fingerprint()
                },
                Stage::Input,
                None,
            ),
        ] {
            let mut meta = finished();
            let mut asked = identity(128, "4b1e0d2");
            asked.fingerprint = change;
            assert_eq!(
                meta.reenter(&asked, "flash", "deepseek", &budget(10.0)),
                Reentry::Invalidated { from }
            );
            assert_eq!(meta.completed_through, survives);
            assert_eq!(
                meta.fingerprint, asked.fingerprint,
                "the new slices are what the next entry compares against"
            );
        }
    }

    /// Continuing under the settings in hand means recording them: the
    /// model that is about to be asked is the one the summary has to name.
    #[test]
    fn an_invalidated_run_records_the_model_and_ceiling_it_will_run_under() {
        let mut meta = finished();
        let mut asked = identity(128, "4b1e0d2");
        asked.fingerprint.review = "other".to_string();

        meta.reenter(&asked, "pro", "anthropic", &budget(20.0));

        assert_eq!(meta.model, "pro");
        assert_eq!(meta.provider, "anthropic");
        assert_eq!(meta.budget_limit, 20.0);
    }

    /// The hole an explicit `--run-id` would otherwise leave open: another
    /// merge request, or the same one at another commit, under settings
    /// that happen to match.
    #[test]
    fn another_input_is_refused_however_well_the_settings_match() {
        for asked in [identity(129, "4b1e0d2"), identity(128, "aaaaaaa")] {
            let mut meta = finished();
            assert_eq!(
                meta.reenter(&asked, "flash", "deepseek", &budget(10.0)),
                Reentry::OtherInput {
                    recorded: "gitlab.com/acme/app #128".to_string()
                }
            );
            assert_eq!(
                meta.completed_through,
                Some(Stage::Publish),
                "and nothing about the run it found was touched"
            );
        }
    }

    /// `--publish` is in no slice, so it cannot invalidate anything; it is
    /// recorded on every entry instead, which is what keeps "read the
    /// report, then post it" one run.
    #[test]
    fn publishing_is_recorded_rather_than_fingerprinted() {
        let mut meta = finished();
        assert!(!meta.publish);
        assert_eq!(
            meta.reenter(
                &identity(128, "4b1e0d2"),
                "flash",
                "deepseek",
                &budget(10.0)
            ),
            Reentry::Resumed
        );
    }
}
