//! reviewbot as a library. `review` is the only entry: the same call starts a
//! run and re-enters one, because a run that already finished a stage keeps
//! that stage's checkpoint and is never charged for it twice.
//!
//! `stage::*` -> adapters (`platform` / `worktree` / `protocol` / `tool`) ->
//! infrastructure (`common` / `config` / `security` / `budget` / `record`) ->
//! `domain`. The order of the six stages exists only in `review`.

pub mod budget;
pub(crate) mod common;
pub mod config;
pub mod domain;
pub mod platform;
pub mod protocol;
pub mod record;
pub mod security;
pub mod stage;
pub mod tool;
pub mod worktree;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use budget::{Budget, BudgetError, Limit};
use config::{ConfigError, Settings};
use domain::{Comment, Confidence, Severity};
use platform::PlatformError;
use protocol::ProtocolError;
use record::{DirLock, LocalStorage, Meta, RecordError, Recorder, RunIdentity, Storage, layout};
use stage::input::Input;
use stage::merge::{Merge, MergeOutput};
use stage::orient::Orientation;
use stage::publish::{Publish, PublishInput, PublishedComment};
use stage::report::{Report, ReportInput};
use stage::review::{Review, ReviewOutput};
use stage::triage::{SkippedFile, Triage, TriagePlan};
use stage::{Adapters, StageContext, StageError};

pub use config::{RunOptions, Settings as Configuration};
pub use stage::input::Source;

/// What one run produced. `summary.json` and the stdout summary are both
/// rendered from this.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RunResult {
    pub run_id: String,
    pub model: String,
    pub comments: Vec<Comment>,
    /// The model's own number, stored as given. Never 0 standing in for
    /// "missing".
    pub overall_score: Option<u8>,
    pub summary: Option<String>,
    pub unscored_reason: Option<String>,
    pub skipped: Vec<SkippedFile>,
    pub unreviewed: Vec<String>,
    /// Why the run stopped short of reviewing everything, when it did. The
    /// run is still finished: the report and the unreviewed list are on
    /// disk, and only the exit code says the difference.
    pub stopped: Option<String>,
    pub published: Vec<PublishedComment>,
    pub spent: f64,
    /// `None` means no ceiling, which the CLI spells out rather than
    /// leaving blank.
    pub budget: Option<f64>,
    pub currency: String,
    pub report_path: PathBuf,
    pub summary_path: PathBuf,
}

impl RunResult {
    pub fn count(&self, band: Confidence) -> usize {
        self.comments
            .iter()
            .filter(|comment| comment.confidence == band)
            .count()
    }

    /// The same over the other axis. Both breakdowns reach the terminal,
    /// because "one certain finding" and "one critical finding" are the two
    /// halves of what a reader needs before deciding to open the report.
    pub fn count_severity(&self, band: Severity) -> usize {
        self.comments
            .iter()
            .filter(|comment| comment.severity == band)
            .count()
    }

    /// 0 for a run that reviewed everything, 3 for one the budget cut short.
    /// Both are finished runs; the code says how reviewbot itself did, never
    /// what the review concluded.
    pub fn exit_code(&self) -> i32 {
        match self.stopped {
            Some(_) => 3,
            None => 0,
        }
    }
}

/// One error type for the caller to branch on, which is how `main` picks an
/// exit code.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Record(#[from] RecordError),
    #[error(transparent)]
    Stage(#[from] StageError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Platform(#[from] PlatformError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("the config changed since run {run_id} started, so it cannot be continued")]
    FingerprintMismatch { run_id: String },
    #[error("--publish needs a merge request or pull request URL, not a diff")]
    PublishNeedsPlatform,
    /// Wraps a failure that happened after the run directory existed, so the
    /// caller can name the run the next attempt would go back into.
    #[error("{source}")]
    InRun {
        run_id: String,
        #[source]
        source: Box<Error>,
    },
}

impl Error {
    /// 0 ok, 1 unexpected, 2 config, 3 budget, 4 platform or model, 5 publish
    /// partly failed. The exit code says how reviewbot itself did; it never
    /// encodes the review's conclusions.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::InRun { source, .. } => source.exit_code(),
            // A host with no `[[platform]]` entry and a URL that will not
            // parse are broken references, not an unreachable service.
            Error::Platform(
                PlatformError::UnknownHost { .. } | PlatformError::UnparsableUrl { .. },
            )
            | Error::Stage(StageError::Platform(
                PlatformError::UnknownHost { .. } | PlatformError::UnparsableUrl { .. },
            ))
            // An mbox or a text file handed in as a diff is a broken input
            // reference, the same class as a URL that will not parse.
            | Error::Stage(StageError::Diff(_)) => 2,
            Error::Config(_)
            | Error::Stage(StageError::Config(_))
            | Error::FingerprintMismatch { .. }
            | Error::PublishNeedsPlatform => 2,
            Error::Budget(_) | Error::Stage(StageError::Budget(_)) => 3,
            Error::Platform(_)
            | Error::Protocol(_)
            | Error::Stage(StageError::Platform(_))
            | Error::Stage(StageError::Protocol(_)) => 4,
            Error::Stage(StageError::PublishIncomplete { .. }) => 5,
            _ => 1,
        }
    }

    pub fn run_id(&self) -> Option<&str> {
        match self {
            Error::InRun { run_id, .. } => Some(run_id),
            Error::FingerprintMismatch { run_id } => Some(run_id),
            Error::Record(RecordError::RunNotFound { run_id, .. }) => Some(run_id),
            Error::Stage(StageError::Record(RecordError::RunNotFound { run_id, .. })) => {
                Some(run_id)
            }
            _ => None,
        }
    }
}

/// Review a merge request URL or a raw diff. Runs the six stages in order,
/// skipping the ones a previous process already finished.
pub fn review(settings: &Settings, source: &Source) -> Result<RunResult, Error> {
    let adapters = Adapters::real(settings, source.host().as_deref())?;
    review_with(settings, source, &adapters)
}

/// The injection seam: tests hand in fake adapters and get the same
/// sequencing the public entry point runs.
pub(crate) fn review_with(
    settings: &Settings,
    source: &Source,
    adapters: &Adapters,
) -> Result<RunResult, Error> {
    if settings.options.publish && !matches!(source, Source::Url(_)) {
        return Err(Error::PublishNeedsPlatform);
    }
    let input = Input::identify(adapters, source)?;
    // The head sha is settled now, and repository reads are always by it.
    adapters.bind_repo(&input);
    let fingerprint = settings.fingerprint();
    let run_id = settings
        .options
        .run_id
        .clone()
        .unwrap_or_else(|| record::run_id(&input.identity, &input.head_sha, &fingerprint));

    let run_dir = settings.options.runs_dir.join(&run_id);
    // Nothing below may run without the lock, which is why it is taken with
    // the directory rather than later on with the recorder.
    let run = LockedRun::create(run_dir.clone())?;
    // The run's own worktree lives here, and is deleted with the run. A
    // checkout named on the command line ignores this: it was open before the
    // run id existed, which is why the id could be computed first.
    adapters.open_worktree(&run_dir)?;
    let selection = settings.selection()?;
    let frozen = Budget::freeze(&selection)?;

    // Re-entering a run is only sound while it answers the same question, so
    // an existing directory is refused when the config no longer matches the
    // one its checkpoints were written under. Without the check an explicit
    // --run-id would be the way around it.
    let meta = match Recorder::peek_meta(run.storage.as_ref())? {
        Some(existing) if existing.fingerprint != fingerprint => {
            return Err(Error::FingerprintMismatch { run_id });
        }
        Some(existing) => existing,
        None => Meta::start(
            RunIdentity {
                run_id: run_id.clone(),
                input,
                fingerprint,
            },
            &selection.model.name,
            &selection.provider.name,
            &frozen,
            settings.options.publish,
        ),
    };

    let mut recorder = open_recorder(settings, run, meta)?;
    recorder.set_publish_intent(settings.options.publish)?;
    let budget = restore_budget(recorder.meta())?;
    let result =
        finish(settings, adapters, recorder, budget, source).map_err(|error| error.in_run(&run_id));
    if result.is_ok() {
        warn_if_too_many_runs(&settings.options.runs_dir);
    }
    result
}

/// A run directory this process has taken: where the run writes, and the
/// lock that says it may. They are taken together and handed on together, so
/// no part of a run can reach the directory holding only one of them.
struct LockedRun {
    storage: Arc<dyn Storage>,
    lock: Box<dyn DirLock>,
}

impl LockedRun {
    /// Create the directory and lock it in one step, before the caller has
    /// anything else it could do with it. The directory is often already
    /// there: re-entering a run opens the one the last attempt left behind.
    /// The worktree waits for this: two processes that both got as far as
    /// opening one would already have written over each other, whichever of
    /// them lost the lock afterwards.
    fn create(run_dir: PathBuf) -> Result<Self, Error> {
        let storage: Arc<dyn Storage> = Arc::new(LocalStorage::create(run_dir)?);
        let lock = storage.lock()?;
        Ok(Self { storage, lock })
    }
}

fn open_recorder(settings: &Settings, run: LockedRun, meta: Meta) -> Result<Recorder, Error> {
    Ok(Recorder::open(run.storage, run.lock, meta)?
        .with_max_tool_output_bytes(settings.config.review.max_tool_output_bytes as usize))
}

fn restore_budget(meta: &Meta) -> Result<Budget, Error> {
    Ok(Budget::restore(
        Limit::from_value(meta.budget_limit)?,
        meta.currency.clone(),
        meta.price,
        meta.spent,
    ))
}

/// Assemble the stage context, run the sequence, and write down what was
/// spent. The lock is released when `recorder` drops.
fn finish(
    settings: &Settings,
    adapters: &Adapters,
    mut recorder: Recorder,
    mut budget: Budget,
    source: &Source,
) -> Result<RunResult, Error> {
    let paths = stage::path_policy(settings)?;

    let result = {
        let mut context = StageContext {
            settings,
            adapters,
            recorder: &mut recorder,
            budget: &mut budget,
            redactor: &adapters.redactor,
            paths: &paths,
        };
        run_stages(&mut context, source)
    };
    recorder.record_spend(budget.spent())?;
    result
}

/// Everything a request carries before the diff, assembled once for the run.
/// `tokens` is its measured size, which `triage` reserves before it cuts the
/// first chunk.
struct Preamble {
    instructions: String,
    narrative: Option<String>,
    tokens: u32,
}

/// The whole of the ordering. The first four stages are skipped when their
/// checkpoint is already on disk; the last two are finishing work and run
/// every time. Every stage writes one checkpoint.
fn run_stages(context: &mut StageContext<'_>, source: &Source) -> Result<RunResult, Error> {
    let changeset = match context.completed(stage::input::NUMBER, stage::input::NAME)? {
        Some(done) => done,
        None => Input::run(context, source)?,
    };
    let planned: Option<TriagePlan> =
        context.completed(stage::triage::NUMBER, stage::triage::NAME)?;
    let reviewed_before: Option<ReviewOutput> =
        context.completed(stage::review::NUMBER, stage::review::NAME)?;
    // One set of bytes for both stages: `review` sends them and `triage`
    // holds their measured size back from the window, and those two have to
    // agree. Assembled only when one of them is going to run, because the
    // layout digest in the instructions goes to the network -- a run re-entered
    // with only the report and the posting left has no business fetching a tree.
    let preamble = match planned.is_none() || reviewed_before.is_none() {
        true => {
            let orientation = Orientation::build(context, &changeset);
            let instructions = stage::review::assemble_instructions(
                &context.adapters.tools,
                context.adapters.worktree.reach(),
                context.redactor,
                &orientation,
            )?;
            // The author's own account of the change, fenced as material.
            // Not part of `instructions`: it rides in `input` beside the
            // diff, because it is prose whoever wrote the change wrote.
            let narrative =
                stage::review::narrative_preface(&changeset.narrative, context.redactor)?;
            let tokens = stage::review::prompt_tokens(
                &instructions,
                narrative.as_deref(),
                &context.adapters.tools,
            );
            Some(Preamble {
                instructions,
                narrative,
                tokens,
            })
        }
        false => None,
    };
    let plan: TriagePlan = match planned {
        Some(done) => done,
        None => {
            let preamble = preamble.as_ref().expect("assembled for triage");
            Triage::run(context, &changeset, preamble.tokens)?
        }
    };
    let reviewed: ReviewOutput = match reviewed_before {
        Some(done) => done,
        None => {
            let preamble = preamble.as_ref().expect("assembled for review");
            Review::run(
                context,
                &plan,
                &preamble.instructions,
                preamble.narrative.as_deref(),
            )?
        }
    };
    let merged: MergeOutput = match context.completed(stage::merge::NUMBER, stage::merge::NAME)? {
        Some(done) => done,
        None => Merge::run(context, &changeset, &reviewed)?,
    };
    // Here the rule changes. Each of the four stages above costs a fetch or a
    // model call, and its checkpoint is how a re-entered run avoids paying
    // twice. The two below cost nothing, and giving the same command again is
    // the only way left to re-render a report or to finish posting comments
    // that never reached the MR -- a skip keyed on a checkpoint would take
    // both of those away. They still write their checkpoints and mark
    // themselves complete: `run show` and the run's terminal state read those.
    let summary = Report::run(
        context,
        &ReportInput {
            plan: &plan,
            merged: &merged,
            unreviewed: &reviewed.unreviewed,
            cut_short: &reviewed.cut_short,
            unavailable: &reviewed.unavailable,
        },
    )?;
    let published = Publish::run(
        context,
        &PublishInput {
            changeset: &changeset,
            merged: &merged,
            summary: &summary,
        },
    )?;

    Ok(Finished {
        meta: context.recorder.meta(),
        merged: &merged,
        plan: &plan,
        reviewed: &reviewed,
        published: published.published,
        budget: context.budget,
        run_dir: context.recorder.run_dir(),
    }
    .result())
}

/// The four stage outputs a finished run is described from, gathered so the
/// description takes one argument rather than eight. It holds only what this
/// one call reads.
struct Finished<'a> {
    meta: &'a Meta,
    merged: &'a MergeOutput,
    plan: &'a TriagePlan,
    reviewed: &'a ReviewOutput,
    published: Vec<PublishedComment>,
    budget: &'a Budget,
    run_dir: &'a Path,
}

impl Finished<'_> {
    fn result(self) -> RunResult {
        RunResult {
            run_id: self.meta.run_id.clone(),
            model: self.meta.model.clone(),
            comments: self.merged.comments.clone(),
            overall_score: self.merged.overall_score,
            summary: self.merged.summary.clone(),
            unscored_reason: self.merged.unscored_reason.clone(),
            skipped: self.plan.skipped.clone(),
            unreviewed: self.reviewed.unreviewed.clone(),
            stopped: self.reviewed.stopped.clone(),
            published: self.published,
            spent: self.budget.spent(),
            budget: self.budget.ceiling(),
            currency: self.meta.currency.clone(),
            report_path: self.run_dir.join(layout::REPORT),
            summary_path: self.run_dir.join(layout::SUMMARY),
        }
    }
}

/// After a review, name the prune command once if the directory grew past
/// the warning threshold. Warning is not deletion.
fn warn_if_too_many_runs(runs_dir: &Path) {
    let Ok(count) = record::count_runs(runs_dir) else {
        return;
    };
    if count <= record::WARN_AFTER_RUNS {
        return;
    }
    let command = if runs_dir == crate::config::paths::default_runs_dir() {
        "reviewbot run prune".to_string()
    } else {
        format!("reviewbot run prune --runs-dir {}", runs_dir.display())
    };
    tracing::warn!(
        count,
        "{count} runs under {}; run `{command}` to delete them (pass `--keep N` to retain the newest N)",
        runs_dir.display(),
    );
}

impl Error {
    fn in_run(self, run_id: &str) -> Error {
        match self {
            already @ Error::InRun { .. } => already,
            other => Error::InRun {
                run_id: run_id.to_string(),
                source: Box::new(other),
            },
        }
    }
}
