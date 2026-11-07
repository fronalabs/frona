//! Synchronous, explicitly coordinated JSONL storage. No execution happens on open.
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    ops::Range,
    path::{Path, PathBuf},
};

use super::{
    Conversation, HistoryCheckpoint, HistoryDecodeError, HistoryRecord, HistoryReplay,
    HistoryWritePhase, decode_jsonl, encode_jsonl,
    history::{HistoryWriteError, HistoryWriter},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonlOperation {
    ResolvePath,
    Create,
    Open,
    Read,
    SynchronizeFile,
    SynchronizeDirectory,
    Truncate,
}

/// Load/setup failures occur before execution. Required transition-write failures
/// instead use `HistoryFailure` and stop the attached conversation.
#[derive(Debug, thiserror::Error)]
pub enum JsonlError {
    #[error("JSONL {operation:?} failed for {path}: {source}")]
    Io {
        path: PathBuf,
        operation: JsonlOperation,
        #[source]
        source: io::Error,
    },
    #[error("invalid JSONL history {path}: {source}")]
    Decode {
        path: PathBuf,
        #[source]
        source: HistoryDecodeError,
    },
    #[error("JSONL history changed since open: {path}")]
    Changed { path: PathBuf },
    #[error("a new JSONL writer must be attached before any history or other writer")]
    AlreadyAttached,
}

impl JsonlError {
    fn io(path: &Path, operation: JsonlOperation, source: io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            operation,
            source,
        }
    }
}

/// Loaded, immutable evidence. Opening never executes, repairs, or attaches a
/// writer. The caller must coordinate all readers/writers; there are no locks.
#[derive(Debug)]
pub struct Jsonl {
    path: PathBuf,
    bytes: Vec<u8>,
    replay: HistoryReplay,
    incomplete_tail: Option<Range<usize>>,
}

impl Jsonl {
    /// Durably create a new file, refusing to overwrite any existing path.
    /// The empty file is not replayable until its attached conversation commits
    /// `Opened`. This lets run configuration be supplied before the first write.
    /// All operations here and subsequent required writes are synchronous.
    /// A creation/synchronization failure may leave an empty file; no rollback
    /// or overwrite is attempted. Directory durability is implemented on Unix;
    /// other platforms report an explicit unsupported I/O error.
    pub fn create(path: impl AsRef<Path>) -> Result<JsonlWriter, JsonlError> {
        let path = absolute(path.as_ref())?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| JsonlError::io(&path, JsonlOperation::Create, e))?;
        file.sync_all()
            .map_err(|e| JsonlError::io(&path, JsonlOperation::SynchronizeFile, e))?;
        sync_directory(path.parent().expect("absolute file path has parent"))
            .map_err(|e| JsonlError::io(&path, JsonlOperation::SynchronizeDirectory, e))?;
        Ok(JsonlWriter::new(file))
    }

    /// Read and validate a stable file without mutating it. A complete corrupt
    /// record is an error; an unterminated suffix is exposed and never applied.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, JsonlError> {
        let path = absolute(path.as_ref())?;
        let mut file =
            File::open(&path).map_err(|e| JsonlError::io(&path, JsonlOperation::Open, e))?;
        let bytes = read(&mut file, &path)?;
        let decoded = decode_jsonl(&bytes).map_err(|source| JsonlError::Decode {
            path: path.clone(),
            source,
        })?;
        Ok(Self {
            path,
            bytes,
            replay: decoded.replay,
            incomplete_tail: decoded.incomplete_tail,
        })
    }

    pub fn conversation(&self) -> &Conversation {
        &self.replay.conversation
    }

    pub fn checkpoint(&self) -> Option<(u64, &HistoryCheckpoint)> {
        self.replay
            .checkpoint
            .as_ref()
            .map(|(seq, saved)| (*seq, saved))
    }

    pub fn checkpoint_stale(&self) -> bool {
        self.replay.checkpoint_stale
    }

    pub fn incomplete_tail(&self) -> Option<Range<usize>> {
        self.incomplete_tail.clone()
    }

    /// Deliberately detach the loaded evidence. Later mutations are memory-only;
    /// this never modifies the source file or accepts unfinished work.
    pub fn into_memory(self) -> HistoryReplay {
        self.replay
    }

    /// Explicitly allow future writes. Under caller-owned coordination, reopen
    /// and compare the entire file to the loaded bytes using the same handle
    /// used for truncation and appends. Only the reported incomplete suffix may
    /// be removed, and that truncation is synchronized before returning.
    /// No action is retried and stopped/terminal status remains unchanged.
    pub fn into_writable(mut self) -> Result<Conversation, JsonlError> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| JsonlError::io(&self.path, JsonlOperation::Open, e))?;
        if read(&mut file, &self.path)? != self.bytes {
            return Err(JsonlError::Changed { path: self.path });
        }
        if let Some(tail) = self.incomplete_tail {
            file.set_len(tail.start as u64)
                .map_err(|e| JsonlError::io(&self.path, JsonlOperation::Truncate, e))?;
        }
        // A complete surviving record may come from an unacknowledged write.
        // Establish durability of the entire accepted prefix before continuation.
        file.sync_all()
            .map_err(|e| JsonlError::io(&self.path, JsonlOperation::SynchronizeFile, e))?;
        self.replay
            .conversation
            .attach_restored_jsonl(JsonlWriter::new(file));
        Ok(self.replay.conversation)
    }
}

fn absolute(path: &Path) -> Result<PathBuf, JsonlError> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|e| JsonlError::io(path, JsonlOperation::ResolvePath, e))
    }
}

fn read(file: &mut File, path: &Path) -> Result<Vec<u8>, JsonlError> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| JsonlError::io(path, JsonlOperation::Read, e))?;
    Ok(bytes)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "durable directory synchronization is not implemented on this platform",
    ))
}

trait DurableWrite: Write + Send + Sync + std::fmt::Debug {
    fn synchronize(&self) -> io::Result<()>;
}

impl DurableWrite for File {
    fn synchronize(&self) -> io::Result<()> {
        self.sync_all()
    }
}

/// A required synchronous writer. Attach it to a fresh conversation; it cannot
/// be cloned, reused, or used to append arbitrary records outside the state machine.
#[derive(Debug)]
pub struct JsonlWriter {
    file: Box<dyn DurableWrite>,
}

impl JsonlWriter {
    fn new(file: File) -> Self {
        Self {
            file: Box::new(file),
        }
    }
}

impl HistoryWriter for JsonlWriter {
    fn commit(&mut self, record: &HistoryRecord) -> Result<(), HistoryWriteError> {
        let failure = |phase, cause: String| HistoryWriteError { phase, cause };
        let bytes = encode_jsonl(std::slice::from_ref(record))
            .map_err(|e| failure(HistoryWritePhase::Serialize, e.to_string()))?;
        self.file
            .write_all(&bytes)
            .map_err(|e| failure(HistoryWritePhase::Append, e.to_string()))?;
        self.file
            .flush()
            .map_err(|e| failure(HistoryWritePhase::Flush, e.to_string()))?;
        self.file
            .synchronize()
            .map_err(|e| failure(HistoryWritePhase::Synchronize, e.to_string()))
    }
}

#[cfg(test)]
mod tests;
