//! Where a run's state lives. The local filesystem is the first and so far
//! only implementation.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::RecordError;

/// Every write is atomic and every path is relative to the run directory.
pub trait Storage: Send + Sync {
    /// A human readable location, for error messages and the run summary.
    fn location(&self) -> &Path;

    /// Temporary file plus rename, so a reader never sees half a file.
    fn write(&self, relative: &str, bytes: &[u8]) -> Result<(), RecordError>;

    /// Fails if the file already exists. This is how the directory lock is
    /// taken: no waiting, no stealing.
    fn create_new(&self, relative: &str, bytes: &[u8]) -> Result<(), RecordError>;

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

    fn create_new(&self, relative: &str, bytes: &[u8]) -> Result<(), RecordError> {
        let path = self.path(relative);
        self.ensure_parent(&path)?;
        let mut file = std::fs::File::create_new(&path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::AlreadyExists {
                RecordError::AlreadyExists { path: path.clone() }
            } else {
                RecordError::Io {
                    path: path.clone(),
                    source,
                }
            }
        })?;
        file.write_all(bytes).map_err(|source| RecordError::Io {
            path: path.clone(),
            source,
        })
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
