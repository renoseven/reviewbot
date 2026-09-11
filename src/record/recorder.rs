use std::path::Path;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::domain::Stage;

use super::error::RecordError;
use super::layout;
use super::lock::DirLock;
use super::meta::{self, Meta};
use super::storage::Storage;
use super::trace::{PublishedView, Trace};

/// Owns the run directory for the length of the process: holds the lock,
/// keeps `meta.json` current, and reads and writes checkpoints.
pub struct Recorder {
    storage: Arc<dyn Storage>,
    meta: Meta,
    max_tool_output_bytes: usize,
    _lock: Box<dyn DirLock>,
}

impl Recorder {
    /// The lock arrives already taken, because the caller had to hold it
    /// before it touched anything else in the run directory; the recorder
    /// only keeps it alive until the run ends. Writes `meta.json`, keeping
    /// an existing one so completed stages survive into the next process.
    pub fn open(
        storage: Arc<dyn Storage>,
        lock: Box<dyn DirLock>,
        mut meta: Meta,
        max_tool_output_bytes: usize,
        publish: bool,
    ) -> Result<Self, RecordError> {
        if meta.publish != publish {
            meta.publish = publish;
            meta.updated_at = meta::now();
        }
        let recorder = Self {
            storage,
            meta,
            max_tool_output_bytes,
            _lock: lock,
        };
        recorder.write_meta()?;
        Ok(recorder)
    }

    /// Read `meta.json` without taking the lock, which is what a run walking
    /// into an existing directory needs before it can decide what its
    /// checkpoints are still worth.
    ///
    /// One that will not parse reads as no run at all: nothing in it can be
    /// trusted — not the fingerprint that says which checkpoints still
    /// answer the question, and not `spent` either, which is why letting
    /// `review` start over costs nothing that was still readable. So
    /// `review` starts a fresh run in the same directory, budget frozen
    /// again, and `run list` / `run show` show the directory with the
    /// fields they could not read left empty, rather than failing on a run
    /// nobody can do anything about.
    pub fn peek_meta(storage: &dyn Storage) -> Result<Option<Meta>, RecordError> {
        let Some(bytes) = storage.read(layout::META)? else {
            return Ok(None);
        };
        match serde_json::from_slice(&bytes) {
            Ok(meta) => Ok(Some(meta)),
            Err(error) => {
                tracing::warn!(
                    file = layout::META,
                    %error,
                    "{} will not parse; treating this run directory as a new run",
                    layout::META
                );
                Ok(None)
            }
        }
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    pub fn run_dir(&self) -> &Path {
        self.storage.location()
    }

    pub fn storage(&self) -> &dyn Storage {
        self.storage.as_ref()
    }

    /// The output of a stage that already finished, or `None` when it has to
    /// run. A checkpoint that will not parse counts as "has to run": the
    /// stage is marked incomplete and the run falls back to the last
    /// complete snapshot instead of being restarted from scratch.
    pub fn completed<T: DeserializeOwned>(
        &mut self,
        stage: Stage,
    ) -> Result<Option<T>, RecordError> {
        if !self.meta.is_complete(stage) {
            return Ok(None);
        }
        let file = layout::stage_file(stage);
        let Some(bytes) = self.storage.read(&file)? else {
            tracing::warn!(%stage, "checkpoint is missing; running the stage again");
            self.meta.mark_incomplete(stage);
            self.write_meta()?;
            return Ok(None);
        };
        match serde_json::from_slice(&bytes) {
            Ok(value) => Ok(Some(value)),
            Err(error) => {
                tracing::warn!(%stage, %error, "checkpoint will not parse; running the stage again");
                self.meta.mark_incomplete(stage);
                self.write_meta()?;
                Ok(None)
            }
        }
    }

    /// Delete the checkpoints of `stage` and every stage after it.
    ///
    /// Clearing the flags in `meta.json` is not enough on its own: these
    /// files parse perfectly — they were written under settings that have
    /// since changed, not corrupted — and `review` picks its own
    /// in-progress snapshot back up whether or not the stage was ever
    /// marked done, so a chunk reviewed under the old settings would be
    /// kept and the new ones would never reach the model.
    pub fn discard_from(&self, stage: Stage) -> Result<(), RecordError> {
        for dropped in Stage::ALL.into_iter().filter(|later| *later >= stage) {
            self.storage.remove(&layout::stage_file(dropped))?;
        }
        Ok(())
    }

    /// Write the checkpoint without marking the stage done. `review` does
    /// this after each chunk so a re-entered run does not pay for those
    /// again. The stage flag stays off until `complete`: a success flag
    /// with work still behind it would be a lie.
    pub fn save<T: Serialize>(&self, stage: Stage, output: &T) -> Result<(), RecordError> {
        self.write_json(&layout::stage_file(stage), output)
    }

    /// The checkpoint as last written, whether or not the stage finished.
    /// A file that will not parse is treated as missing: the stage starts
    /// again rather than trusting a half-written snapshot.
    pub fn saved<T: DeserializeOwned>(&self, stage: Stage) -> Result<Option<T>, RecordError> {
        let file = layout::stage_file(stage);
        let Some(bytes) = self.storage.read(&file)? else {
            return Ok(None);
        };
        match serde_json::from_slice(&bytes) {
            Ok(value) => Ok(Some(value)),
            Err(error) => {
                tracing::warn!(
                    %stage,
                    %error,
                    "in-progress checkpoint will not parse; starting the stage again"
                );
                Ok(None)
            }
        }
    }

    /// Write the checkpoint, then record the stage as done. In that order:
    /// a success flag with no file behind it would be a lie.
    pub fn complete<T: Serialize>(&mut self, stage: Stage, output: &T) -> Result<(), RecordError> {
        self.save(stage, output)?;
        self.meta.mark_complete(stage);
        self.write_meta()
    }

    pub fn write_trace(&self, trace: &Trace) -> Result<(), RecordError> {
        self.write_json(&layout::trace_file(trace.trace_id()), trace.internal())?;
        Ok(())
    }

    /// A trace a previous stage wrote, so `merge` can add its notes. One that
    /// will not parse is treated as missing: a broken trace should cost its
    /// comment its lookup file, not the whole report.
    pub fn read_trace(&self, trace_id: &str) -> Result<Option<Trace>, RecordError> {
        let file = layout::trace_file(trace_id);
        let Some(bytes) = self.storage.read(&file)? else {
            return Ok(None);
        };
        match serde_json::from_slice(&bytes) {
            Ok(trace) => Ok(Some(trace)),
            Err(error) => {
                tracing::warn!(trace_id, %error, "trace will not parse");
                Ok(None)
            }
        }
    }

    pub fn published_trace(&self, trace: &Trace) -> PublishedView {
        trace.published(self.max_tool_output_bytes)
    }

    pub fn write_artifact(&self, name: &str, bytes: &[u8]) -> Result<(), RecordError> {
        self.storage.write(name, bytes)
    }

    pub fn read_artifact(&self, name: &str) -> Result<Option<Vec<u8>>, RecordError> {
        self.storage.read(name)
    }

    /// A later command asked to publish. The run follows the last recording.
    pub fn intend_publish(&mut self) -> Result<(), RecordError> {
        if !self.meta.publish {
            self.meta.publish = true;
            self.meta.updated_at = meta::now();
            self.write_meta()?;
        }
        Ok(())
    }

    pub fn record_spend(&mut self, spent: f64) -> Result<(), RecordError> {
        self.meta.spent = spent;
        self.meta.updated_at = meta::now();
        self.write_meta()
    }

    fn write_meta(&self) -> Result<(), RecordError> {
        self.write_json(layout::META, &self.meta)
    }

    fn write_json<T: Serialize>(&self, file: &str, value: &T) -> Result<(), RecordError> {
        let bytes = serde_json::to_vec_pretty(value).map_err(|source| RecordError::Serialize {
            file: file.to_string(),
            source,
        })?;
        self.storage.write(file, &bytes)
    }
}
