//! One process per run directory. Take it or fail; never wait, never steal.

use std::sync::Arc;

use super::{RecordError, layout, storage::Storage};

pub struct DirLock {
    storage: Arc<dyn Storage>,
}

impl DirLock {
    /// Fails immediately when the lock file is already there. A lock left
    /// behind by a killed process stays until someone removes it.
    pub fn take(storage: Arc<dyn Storage>) -> Result<Self, RecordError> {
        let owner = format!("pid {}\n", std::process::id());
        match storage.create_new(layout::LOCK, owner.as_bytes()) {
            Ok(()) => Ok(Self { storage }),
            Err(RecordError::AlreadyExists { path }) => Err(RecordError::LockHeld { path }),
            Err(other) => Err(other),
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        if let Err(error) = self.storage.remove(layout::LOCK) {
            tracing::warn!(%error, "could not remove the run directory lock");
        }
    }
}
