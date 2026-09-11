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
pub mod progress;
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
use domain::{Comment, Confidence, Severity, Stage};
use platform::PlatformError;
use progress::{Event, Outcome, Progress};
use protocol::ProtocolError;
use record::Runs;
use record::{
    DirLock, LocalStorage, Meta, RecordError, Recorder, Reentry, RunIdentity, Storage, layout,
};
use security::{PathPolicy, PatternError};
use stage::input::{Input, Opening};
use stage::merge::{Merge, MergeOutput};
use stage::plan::{Plan, PlanOutput, SkippedFile};
use stage::publish::{Publish, PublishInput, PublishedComment};
use stage::report::{Report, ReportInput};
use stage::review::{Preamble, Review, ReviewOutput};
use stage::{Adapters, Equipment, StageContext, StageError};
use worktree::{Checkout, WorktreeError};

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
    #[error(transparent)]
    Redact(#[from] PatternError),
    /// The directory named by `--run-id` belongs to something else. A
    /// changed config is not this: that re-enters the run and drops the
    /// stages it reaches.
    #[error("run {run_id} records a different input ({recorded}), so it cannot be continued")]
    DifferentInput { run_id: String, recorded: String },
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
    /// 0 ok, 1 unexpected, 2 the config or something it names does not
    /// resolve, 3 budget, 4 platform or model, 5 publish partly failed. The
    /// exit code says how reviewbot itself did; it never encodes the
    /// review's conclusions.
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
            | Error::Stage(StageError::Diff(_))
            // So is anything else named on the command line that does not
            // resolve: a diff file that is not there, a run id nothing was
            // ever written under, a `--worktree` that is not a checkout or
            // is parked on the wrong commit. None of these is reviewbot
            // breaking, and a CI job that branches on the exit code has to
            // be able to tell the two apart.
            | Error::Stage(StageError::UnreadableInput { .. })
            | Error::Record(RecordError::RunNotFound { .. })
            | Error::Stage(StageError::Record(RecordError::RunNotFound { .. }))
            | Error::Stage(StageError::Worktree(
                WorktreeError::NotADirectory { .. }
                | WorktreeError::Unopenable { .. }
                | WorktreeError::NoHead { .. }
                | WorktreeError::HeadMismatch { .. },
            )) => 2,
            Error::Config(_)
            | Error::Stage(StageError::Config(_))
            | Error::DifferentInput { .. }
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
            Error::DifferentInput { run_id, .. } => Some(run_id),
            Error::Record(RecordError::RunNotFound { run_id, .. }) => Some(run_id),
            Error::Stage(StageError::Record(RecordError::RunNotFound { run_id, .. })) => {
                Some(run_id)
            }
            _ => None,
        }
    }

    /// Whether giving the same command again would carry on from this
    /// failure. Nearly always yes — that is the whole shape of re-entering a
    /// run, a changed config included. A directory that records another
    /// input is the exception: the command is what aimed at it, so
    /// repeating the command aims there again and fails the same way, and
    /// offering it back would be advice that cannot work.
    pub fn same_command_continues(&self) -> bool {
        match self {
            Error::InRun { source, .. } => source.same_command_continues(),
            Error::DifferentInput { .. } => false,
            _ => true,
        }
    }
}

/// Review a merge request URL or a raw diff. Runs the six stages in order,
/// skipping the ones a previous process already finished.
///
/// `progress` is told what the run is doing as it does it. A caller with
/// nothing to show hands in `progress::Silent`; nothing about the run
/// changes either way.
pub fn review(
    settings: &Settings,
    source: &Source,
    progress: &dyn Progress,
) -> Result<RunResult, Error> {
    let adapters = Adapters::real(settings, source.host().as_deref())?;
    review_with(settings, source, &adapters, progress)
}

/// The injection seam: tests hand in fake adapters and get the same
/// sequencing the public entry point runs.
///
/// Only the half of them that exists before the run directory can be handed
/// in. The worktree and the tools over it are this run's own to build, after
/// the lock, so a test fakes the platform and the protocol and gets the real
/// ones standing on top of them.
pub(crate) fn review_with(
    settings: &Settings,
    source: &Source,
    adapters: &Adapters,
    progress: &dyn Progress,
) -> Result<RunResult, Error> {
    if settings.options().publish && !matches!(source, Source::Url(_)) {
        return Err(Error::PublishNeedsPlatform);
    }
    // Everything from here to `RunStarted` is one wait with nothing to show for
    // it: a URL has its change fetched before anything can be named.
    progress.emit(Event::Opening);
    // The checkout is read here, because there is no worktree yet: the run
    // is named after this sha, and the worktree opens inside the directory
    // the name picks out.
    let checkout = settings
        .options()
        .worktree
        .as_ref()
        .map(Checkout::open)
        .transpose()
        .map_err(StageError::from)?;
    let input = Opening::new(adapters, source, checkout).identify()?;
    // The head sha is settled now, and repository reads are always by it.
    adapters.bind(&input)?;
    // The settings are not in the id: the same change at the same commit is
    // always the same directory, and a config edit re-enters it.
    let run_id = settings
        .options()
        .run_id
        .clone()
        .unwrap_or_else(|| input.run_id());

    let run_dir = settings.options().runs_dir.join(&run_id);
    // Nothing below may run without the lock, which is why it is taken with
    // the directory rather than later on with the recorder.
    let run = LockedRun::create(run_dir.clone())?;
    let selection = settings.selection()?;
    let frozen = Budget::freeze(&selection)?;
    let identity = RunIdentity {
        run_id: run_id.clone(),
        input,
        fingerprint: settings.fingerprint()?,
    };

    // Walking into a directory that already holds a run: it has to be this
    // same change — an explicit --run-id is the one way it might not be —
    // and whichever settings slice has changed since takes its stage and
    // every stage after it with it.
    let mut invalidated_from = None;
    let meta = match Recorder::peek_meta(run.storage.as_ref())? {
        Some(mut existing) => {
            match existing.reenter(
                &identity,
                &selection.model.name,
                &selection.provider.name,
                &frozen,
            ) {
                Reentry::OtherInput { recorded } => {
                    return Err(Error::DifferentInput { run_id, recorded });
                }
                Reentry::Invalidated { from } => invalidated_from = Some(from),
                Reentry::Resumed => {}
            }
            existing
        }
        None => Meta::start(
            identity,
            &selection.model.name,
            &selection.provider.name,
            &frozen,
            settings.options().publish,
        ),
    };

    let recorder = open_recorder(settings, run, meta)?;
    if let Some(from) = invalidated_from {
        recorder.discard_from(from)?;
    }
    // Said once the directory, the id and the model are settled and the
    // config has been agreed with, so nothing that has already announced
    // itself can still turn out to be the wrong run.
    let started = recorder.meta();
    progress.emit(Event::RunStarted {
        run_id: started.run_id.clone(),
        run_dir: recorder.run_dir().to_path_buf(),
        model: started.model.clone(),
        input: started.input.describe(),
        worktree: settings.options().worktree.clone(),
    });
    let result = finish(settings, adapters, recorder, source, progress)
        .map_err(|error| error.in_run(&run_id));
    if result.is_ok() {
        warn_if_too_many_runs(&settings.options().runs_dir);
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
    Ok(Recorder::open(
        run.storage,
        run.lock,
        meta,
        settings.config().review.max_tool_output_bytes as usize,
        settings.options().publish,
    )?)
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
/// spent. The budget is rebuilt here rather than handed in, because its whole
/// life is this call: it is restored from `meta.json` before the first stage
/// and written back after the last one. The lock is released when `recorder`
/// drops.
fn finish(
    settings: &Settings,
    adapters: &Adapters,
    mut recorder: Recorder,
    source: &Source,
    progress: &dyn Progress,
) -> Result<RunResult, Error> {
    let paths = PathPolicy::for_settings(settings)?;
    // The worktree and the tools over it, assembled here because here is the
    // first moment they can be: the lock is held, the directory the cache
    // goes in exists, and no stage has run.
    let equipment = Equipment::real(settings, adapters, recorder.run_dir())?;
    let mut budget = restore_budget(recorder.meta())?;

    let result = {
        let mut context = StageContext {
            settings,
            adapters,
            worktree: equipment.worktree(),
            tools: equipment.tools(),
            recorder: &mut recorder,
            budget: &mut budget,
            redactor: adapters.redactor(),
            paths: &paths,
            progress,
        };
        run_stages(&mut context, source)
    };
    recorder.record_spend(budget.spent())?;
    result
}

/// The whole of the ordering. The first four stages are skipped when their
/// checkpoint is already on disk; the last two are finishing work and run
/// every time. Every stage writes one checkpoint.
fn run_stages(context: &mut StageContext<'_>, source: &Source) -> Result<RunResult, Error> {
    context.stage_started(Stage::Input);
    let (changeset, from_checkpoint) = match context.completed(Stage::Input)? {
        Some(done) => (done, true),
        None => (Input::new(context).run(source)?, false),
    };
    context.stage_finished(
        Stage::Input,
        Outcome::Input {
            files: changeset.files.len(),
        },
        from_checkpoint,
    );

    let planned: Option<PlanOutput> = context.completed(Stage::Plan)?;
    let reviewed_before: Option<ReviewOutput> = context.completed(Stage::Review)?;
    // One set of bytes for both stages: `review` sends them and `plan`
    // holds their measured size back from the window, and those two have to
    // agree. Assembled only when one of them is going to run. The change
    // list comes from the changeset already in hand — a run re-entered with
    // only the report and the posting left has no business building either.
    let mut preamble = None;
    context.stage_started(Stage::Plan);
    let from_checkpoint = planned.is_some();
    let plan: PlanOutput = match planned {
        Some(done) => done,
        None => {
            let assembled = Preamble::assemble(context, &changeset)?;
            let plan = Plan::new(context).run(&changeset, &assembled)?;
            preamble = Some(assembled);
            plan
        }
    };
    context.stage_finished(
        Stage::Plan,
        Outcome::Plan {
            chunks: plan.chunks.len(),
            skipped: plan.skipped.len(),
        },
        from_checkpoint,
    );

    context.stage_started(Stage::Review);
    let from_checkpoint = reviewed_before.is_some();
    let reviewed: ReviewOutput = match reviewed_before {
        Some(done) => done,
        None => {
            let assembled = match preamble {
                Some(assembled) => assembled,
                None => Preamble::assemble(context, &changeset)?,
            };
            Review::new(context).run(&plan, &assembled)?
        }
    };
    context.stage_finished(
        Stage::Review,
        Outcome::Review {
            chunks: reviewed.chunks.len(),
            unreviewed: reviewed.unreviewed.len(),
        },
        from_checkpoint,
    );

    context.stage_started(Stage::Merge);
    let (merged, from_checkpoint): (MergeOutput, bool) = match context.completed(Stage::Merge)? {
        Some(done) => (done, true),
        None => (Merge::new(context).run(&changeset, &reviewed)?, false),
    };
    context.stage_finished(
        Stage::Merge,
        Outcome::Merge {
            comments: merged.comments.len(),
            overall: merged.overall_score,
        },
        from_checkpoint,
    );
    // Here the rule changes. Each of the four stages above costs a fetch or a
    // model call, and its checkpoint is how a re-entered run avoids paying
    // twice. The two below cost nothing, and giving the same command again is
    // the only way left to re-render a report or to finish posting comments
    // that never reached the MR -- a skip keyed on a checkpoint would take
    // both of those away. They still write their checkpoints and mark
    // themselves complete: `run show` and the run's terminal state read those.
    context.stage_started(Stage::Report);
    let summary = Report::new(context).run(&ReportInput {
        plan: &plan,
        merged: &merged,
        unreviewed: &reviewed.unreviewed,
        stopped: reviewed.stopped.as_deref(),
        unavailable: &reviewed.unavailable,
    })?;
    // No checkpoint to have been read back: these two run every time, so
    // what they say is always what this process just did.
    context.stage_finished(Stage::Report, Outcome::Report, false);

    context.stage_started(Stage::Publish);
    let published = Publish::new(context).run(&PublishInput {
        changeset: &changeset,
        merged: &merged,
        summary: &summary,
    })?;
    context.stage_finished(
        Stage::Publish,
        Outcome::Publish {
            posted: published.published.len(),
            already_there: published.skipped_as_duplicate,
            asked: published.posted_to_platform,
        },
        false,
    );

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
    plan: &'a PlanOutput,
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
    let Ok(count) = Runs::open(runs_dir).count() else {
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
        "{count} runs under {}; run `{command}` to delete them (pass `--keep-latest N` to retain the newest N)",
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
