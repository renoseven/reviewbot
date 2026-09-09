//! Where a run's state lives. The local filesystem is the first and so far
//! only implementation.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::lock::{DirLock, FileLock};
use super::{RecordError, layout};

/// Every write is atomic and every path is relative to the run directory.
pub trait Storage: Send + Sync {
    /// A human readable location, for error messages and the run summary.
    fn location(&self) -> &Path;

    /// Temporary file plus rename, so a reader never sees half a file.
    fn write(&self, relative: &str, bytes: &[u8]) -> Result<(), RecordError>;

    /// Take this run directory for the caller, or fail: no waiting, no
    /// stealing. The lock lasts as long as the guard, and every write the
    /// run makes belongs after this call.
    fn lock(&self) -> Result<Box<dyn DirLock>, RecordError>;

    fn read(&self, relative: &str) -> Result<Option<Vec<u8>>, RecordError>;

    fn remove(&self, relative: &str) -> Result<(), RecordError>;

    fn exists(&self, relative: &str) -> bool;
}

pub struct LocalStorage {
    root: PathBuf,
}

impl LocalStorage {
    /// Creates the run directory if it is not there yet.
    pub fn create(root: PathBuf) -> Result<Self, RecordError> {
        std::fs::create_dir_all(&root).map_err(|source| RecordError::Io {
            path: root.clone(),
            source,
        })?;
        Ok(Self { root })
    }

    pub fn open(root: PathBuf) -> Self {
        Self { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn ensure_parent(&self, path: &Path) -> Result<(), RecordError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| RecordError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        Ok(())
    }
}

impl Storage for LocalStorage {
    fn location(&self) -> &Path {
        &self.root
    }

    fn write(&self, relative: &str, bytes: &[u8]) -> Result<(), RecordError> {
        let path = self.path(relative);
        self.ensure_parent(&path)?;
        let temporary = path.with_file_name(format!(
            "{}.tmp",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        let io = |source| RecordError::Io {
            path: path.clone(),
            source,
        };
        let mut file = std::fs::File::create(&temporary).map_err(io)?;
        file.write_all(bytes).map_err(io)?;
        file.sync_all().map_err(io)?;
        drop(file);
        std::fs::rename(&temporary, &path).map_err(io)
    }

    /// The kernel holds it, on the run directory's `lock` file.
    fn lock(&self) -> Result<Box<dyn DirLock>, RecordError> {
        let path = self.path(layout::LOCK);
        self.ensure_parent(&path)?;
        Ok(Box::new(FileLock::take(&path)?))
    }

    fn read(&self, relative: &str) -> Result<Option<Vec<u8>>, RecordError> {
        let path = self.path(relative);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(RecordError::Io { path, source }),
        }
    }

    fn remove(&self, relative: &str) -> Result<(), RecordError> {
        let path = self.path(relative);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(RecordError::Io { path, source }),
        }
    }

    fn exists(&self, relative: &str) -> bool {
        self.path(relative).exists()
    }
}
