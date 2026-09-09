//! The identity of one run and everything it writes down: `run_id`, the run
//! directory, the lock, `meta.json`, atomic checkpoints, and traces.
//!
//! It does not know the order of the stages; it only records which of them
//! finished.

pub mod layout;
pub mod listing;
pub mod lock;
pub mod meta;
pub mod run_id;
pub mod storage;
pub mod trace;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;

pub use listing::{
    DEFAULT_KEEP, PruneReport, RunRow, RunShow, WARN_AFTER_RUNS, count_runs, list_run_dirs,
    list_runs, prune_runs, show_run,
};
pub use lock::DirLock;
pub use meta::{InputKind, InputRecord, Meta, RunIdentity};
pub use run_id::{InputIdentity, run_id};
pub use storage::{LocalStorage, Storage};
pub use trace::{Check, ContextFile, PublishedView, ToolCall, Trace};

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} already exists")]
    AlreadyExists { path: PathBuf },
    #[error("another process is already running this run ({path} exists)")]
    LockHeld { path: PathBuf },
    #[error("no run {run_id} under {runs_dir}")]
    RunNotFound { run_id: String, runs_dir: PathBuf },
    #[error("cannot read {file}: {source}")]
    Corrupt {
        file: String,
        source: serde_json::Error,
    },
    #[error("cannot serialize {file}: {source}")]
    Serialize {
        file: String,
        source: serde_json::Error,
    },
}

/// Owns the run directory for the length of the process: holds the lock,
/// keeps `meta.json` current, and reads and writes checkpoints.
pub struct Recorder {
    storage: Arc<dyn Storage>,
    meta: Meta,
    max_tool_output_bytes: usize,
    _lock: DirLock,
}

impl Recorder {
    /// Take the lock, then write `meta.json`. An existing `meta.json` is
    /// kept, so completed stages survive into the next process.
    pub fn open(storage: Arc<dyn Storage>, meta: Meta) -> Result<Self, RecordError> {
        let lock = DirLock::take(Arc::clone(&storage))?;
        let recorder = Self {
            storage,
            meta,
            max_tool_output_bytes: 65_536,
            _lock: lock,
        };
        recorder.write_meta()?;
        Ok(recorder)
    }

    /// Read `meta.json` without taking the lock, which is what `resume` needs
    /// before it can decide whether the fingerprint still matches.
    pub fn peek_meta(storage: &dyn Storage) -> Result<Option<Meta>, RecordError> {
        let Some(bytes) = storage.read(layout::META)? else {
            return Ok(None);
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| RecordError::Corrupt {
                file: layout::META.to_string(),
                source,
            })
    }

    pub fn with_max_tool_output_bytes(mut self, bytes: usize) -> Self {
        self.max_tool_output_bytes = bytes;
        self
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
        number: u8,
        stage: &str,
    ) -> Result<Option<T>, RecordError> {
        if !self.meta.is_complete(stage) {
            return Ok(None);
        }
        let file = layout::stage_file(number, stage);
        let Some(bytes) = self.storage.read(&file)? else {
            tracing::warn!(stage, "checkpoint is missing; running the stage again");
            self.meta.mark_incomplete(stage);
            self.write_meta()?;
            return Ok(None);
        };
        match serde_json::from_slice(&bytes) {
            Ok(value) => Ok(Some(value)),
            Err(error) => {
                tracing::warn!(stage, %error, "checkpoint will not parse; running the stage again");
                self.meta.mark_incomplete(stage);
                self.write_meta()?;
                Ok(None)
            }
        }
    }

    /// Write the checkpoint, then record the stage as done. In that order:
    /// a success flag with no file behind it would be a lie.
    pub fn complete<T: Serialize>(
        &mut self,
        number: u8,
        stage: &str,
        output: &T,
    ) -> Result<(), RecordError> {
        let file = layout::stage_file(number, stage);
        self.write_json(&file, output)?;
        self.meta.mark_complete(stage);
        self.write_meta()
    }

    pub fn write_trace(&self, trace: &Trace) -> Result<(), RecordError> {
        self.write_json(&layout::trace_file(&trace.trace_id), trace.internal())?;
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

    /// `--publish` is not part of the fingerprint, so every `review` records
    /// what it was asked to do and `resume` follows the last recording.
    pub fn set_publish_intent(&mut self, publish: bool) -> Result<(), RecordError> {
        if self.meta.publish != publish {
            self.meta.publish = publish;
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
