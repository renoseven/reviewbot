use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Names no file on purpose. The lock is the kernel's; a user shown a
    /// path would delete it, and deleting it would win nothing.
    #[error("another process is running this run")]
    LockHeld,
    #[error("no run {run_id} under {runs_dir}")]
    RunNotFound { run_id: String, runs_dir: PathBuf },
    #[error("no trace {trace_id} in run {run_id}")]
    TraceNotFound { run_id: String, trace_id: String },
    #[error("cannot serialize {file}: {source}")]
    Serialize {
        file: String,
        source: serde_json::Error,
    },
}
