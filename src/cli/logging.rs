//! A tracing writer that waits for the run directory before touching disk.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

use reviewbot::record::layout;

/// The handle shared by the subscriber and the CLI's progress consumer.
#[derive(Clone, Default)]
pub struct LogSink {
    state: Arc<Mutex<State>>,
}

impl LogSink {
    /// Preserve startup diagnostics, then send this and all later writes to
    /// the run's append-only log.
    pub fn switch_to(&self, run_dir: &Path) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if matches!(*state, State::File(_)) {
            return Ok(());
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join(layout::LOG))?;
        if let State::Buffer(buffer) = &*state {
            file.write_all(buffer)?;
            file.flush()?;
        }
        *state = State::File(file);
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogSink {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter {
            state: Arc::clone(&self.state),
        }
    }
}

pub struct LogWriter {
    state: Arc<Mutex<State>>,
}

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.flush()
    }
}

enum State {
    Buffer(Vec<u8>),
    File(File),
}

impl Default for State {
    fn default() -> Self {
        Self::Buffer(Vec::new())
    }
}

impl Write for State {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            State::Buffer(buffer) => buffer.write(bytes),
            State::File(file) => file.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            State::Buffer(buffer) => buffer.flush(),
            State::File(file) => file.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_flushes_the_buffer_and_appends_to_an_existing_log() {
        let directory = tempfile::tempdir().expect("temp dir");
        let log = directory.path().join(layout::LOG);
        std::fs::write(&log, "earlier\n").expect("existing log");
        let sink = LogSink::default();
        sink.make_writer().write_all(b"startup\n").expect("buffer");

        sink.switch_to(directory.path()).expect("switch");
        sink.make_writer().write_all(b"running\n").expect("file");

        assert_eq!(
            std::fs::read_to_string(log).expect("log"),
            "earlier\nstartup\nrunning\n"
        );
    }
}
