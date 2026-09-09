//! One process per run directory. Take it or fail; never wait, never steal.

use std::io::Write;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};

use super::RecordError;

/// A run directory taken, for as long as this process owns the run. The
/// `Storage` that owns the directory decides what holding it means; the run
/// only keeps the guard alive. Releasing is dropping it, which is why there
/// is no unlock method and no way to hand it to anybody else.
pub trait DirLock: Send + Sync {}

/// The local filesystem's answer: `flock` on the run directory's `lock` file.
///
/// The kernel holds the lock, not the file. It releases it when the process
/// ends, however it ends — Ctrl-C, `SIGKILL`, a panic — so a lock is never
/// stale: there is no liveness probe, no `--force`, and no unlock command,
/// and a `lock` file left behind stops nobody.
///
/// Which is why a finished run leaves its file where it is. The file is a
/// hint and never a lock, so nothing is gained by deleting it, and something
/// is lost: between the unlink and the close, one process can hold the file
/// it opened a moment earlier while the next creates a fresh one and holds
/// that. The file goes when the run directory goes.
pub struct FileLock {
    /// The open file the kernel hangs the lock on. Closing it releases the
    /// lock, and closing it is all that dropping this does.
    file: std::fs::File,
    path: PathBuf,
}

impl FileLock {
    /// Fails immediately when someone else holds the lock. The `pid` line
    /// goes in afterwards for whoever reads the run directory and wonders
    /// who is in there; nothing reads it back, because the kernel already
    /// answered that question.
    pub fn take(path: &Path) -> Result<Self, RecordError> {
        let io = |source| RecordError::Io {
            path: path.to_path_buf(),
            source,
        };
        let file = std::fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(io)?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => return Err(RecordError::LockHeld),
            Err(errno) => return Err(io(errno.into())),
        }
        let lock = Self {
            file,
            path: path.to_path_buf(),
        };
        lock.write_owner()?;
        Ok(lock)
    }

    fn write_owner(&self) -> Result<(), RecordError> {
        let io = |source| RecordError::Io {
            path: self.path.clone(),
            source,
        };
        let mut file = &self.file;
        // The previous owner may have left a longer line behind.
        file.set_len(0).map_err(io)?;
        file.write_all(format!("pid {}\n", std::process::id()).as_bytes())
            .map_err(io)
    }
}

impl DirLock for FileLock {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_file_nobody_holds_does_not_block_the_next_run() {
        let root = tempfile::tempdir().expect("temp");
        let path = root.path().join("lock");
        // What a process killed with SIGKILL leaves: the file is still
        // there, its pid line still names a process that is gone.
        std::fs::write(&path, b"pid 424242\n").expect("leftover lock");

        let lock = FileLock::take(&path).expect("the leftover file blocks nothing");

        assert_eq!(
            std::fs::read_to_string(&path).expect("lock"),
            format!("pid {}\n", std::process::id()),
            "the new owner writes its own hint over the dead one"
        );
        drop(lock);
    }

    #[test]
    fn two_holders_of_one_run_directory_are_mutually_exclusive() {
        let root = tempfile::tempdir().expect("temp");
        let path = root.path().join("lock");

        let held = FileLock::take(&path).expect("first lock");
        let Err(refused) = FileLock::take(&path) else {
            panic!("the second holder is turned away while the first still holds");
        };
        assert!(matches!(refused, RecordError::LockHeld), "got {refused}");

        drop(held);
        FileLock::take(&path).expect("released, so the next one gets in");
    }
}
