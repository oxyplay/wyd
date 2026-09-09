//! Bounded, file-backed run output.
//!
//! Each stream is an append-only file capped at a per-stream limit. Once the
//! cap is hit the pipe is still drained (a full pipe would deadlock the child)
//! but the bytes are dropped and the stream is marked truncated. We keep the
//! **head** of the output, not a rolling tail: cursors are plain byte offsets
//! into the file, so they stay stable across reads and never shift.
//!
//! Output is never parsed for instructions by wyd — it is data, and it may
//! contain secrets. The files live in a 0700 directory owned by the user.

use crate::model::run::LogState;
use parking_lot::Mutex;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Which stream a sink or cursor refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    pub fn as_str(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stdout" => Some(Stream::Stdout),
            "stderr" => Some(Stream::Stderr),
            _ => None,
        }
    }
}

/// One bounded chunk of output, as read by a client.
#[derive(Debug, Clone)]
pub struct LogChunk {
    pub data: String,
    pub next_cursor: u64,
    pub truncated: bool,
    /// `true` when the cursor is at the current end of the retained data.
    pub eof: bool,
}

/// Total on-disk bytes across all retained run logs, shared by every sink so
/// a global cap can be enforced without rescanning the disk.
#[derive(Debug, Default)]
pub struct GlobalBudget {
    used: Mutex<u64>,
    limit: u64,
}

impl GlobalBudget {
    pub fn new(limit: u64, used: u64) -> Self {
        Self {
            used: Mutex::new(used),
            limit,
        }
    }

    /// Take `n` bytes from the budget, returning how many were granted.
    fn take(&self, n: u64) -> u64 {
        let mut used = self.used.lock();
        let room = self.limit.saturating_sub(*used);
        let granted = n.min(room);
        *used += granted;
        granted
    }

    pub fn release(&self, n: u64) {
        let mut used = self.used.lock();
        *used = used.saturating_sub(n);
    }
}

/// The writer side of one stream, owned by the run's drain thread.
pub struct LogSink {
    file: File,
    written: u64,
    limit: u64,
    truncated: bool,
    state: Arc<Mutex<LogState>>,
    budget: Arc<GlobalBudget>,
    stream: Stream,
}

impl LogSink {
    /// Open (or create) the file for one stream. Existing bytes count against
    /// both the per-stream limit and the global budget.
    pub fn open(
        path: &Path,
        stream: Stream,
        limit: u64,
        state: Arc<Mutex<LogState>>,
        budget: Arc<GlobalBudget>,
    ) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(path)?;
        let written = file.metadata()?.len();
        let sink = Self {
            file,
            written,
            limit,
            truncated: written >= limit,
            state,
            budget,
            stream,
        };
        sink.publish();
        Ok(sink)
    }

    /// Append as much of `buf` as the per-stream and global caps allow. The
    /// rest is dropped — never buffered, never blocking the child.
    pub fn write_all_bounded(&mut self, buf: &[u8]) -> io::Result<()> {
        let room = self.limit.saturating_sub(self.written);
        let want = (buf.len() as u64).min(room);
        if want == 0 {
            if !buf.is_empty() {
                self.truncated = true;
                self.publish();
            }
            return Ok(());
        }
        let granted = self.budget.take(want);
        if granted < want {
            self.truncated = true;
        }
        if granted > 0 {
            let bytes = &buf[..granted as usize];
            self.file.write_all(bytes)?;
            self.written += granted;
        }
        if (granted as usize) < buf.len() {
            self.truncated = true;
        }
        self.publish();
        Ok(())
    }

    pub fn finish(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    fn publish(&self) {
        let mut state = self.state.lock();
        match self.stream {
            Stream::Stdout => {
                state.stdout_bytes = self.written;
                state.stdout_truncated = self.truncated;
            }
            Stream::Stderr => {
                state.stderr_bytes = self.written;
                state.stderr_truncated = self.truncated;
            }
        }
    }
}

/// Bytes stored under `dir`, excluding the per-run tmp directory (its size
/// is not part of the log budget).
pub fn dir_bytes(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.file_name() != "tmp")
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// Read one bounded chunk from a retained stream file.
pub fn read_chunk(path: &Path, cursor: u64, max_bytes: usize) -> io::Result<LogChunk> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        // No file yet (nothing written): an empty, finished chunk.
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(LogChunk {
                data: String::new(),
                next_cursor: 0,
                truncated: false,
                eof: true,
            });
        }
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    let start = cursor.min(len);
    file.seek(SeekFrom::Start(start))?;
    let take = (len - start).min(max_bytes as u64) as usize;
    let mut buf = vec![0u8; take];
    file.read_exact(&mut buf)?;
    Ok(LogChunk {
        // Invalid UTF-8 and control sequences are shown as replacement
        // characters; wyd never interprets output as instructions.
        data: String::from_utf8_lossy(&buf).into_owned(),
        next_cursor: start + take as u64,
        truncated: false,
        eof: start + take as u64 >= len,
    })
}

/// Paths of one run's private working area.
#[derive(Debug, Clone)]
pub struct RunPaths {
    root: PathBuf,
}

impl RunPaths {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dir(&self, id: i64) -> PathBuf {
        self.root.join(id.to_string())
    }

    pub fn stream_file(&self, id: i64, stream: Stream) -> PathBuf {
        self.dir(id).join(stream.as_str())
    }

    /// Per-run `TMPDIR`. Convenience, not a sandbox: the command can still
    /// write anywhere it has permission to.
    pub fn tmp(&self, id: i64) -> PathBuf {
        self.dir(id).join("tmp")
    }

    /// Create the run directory (0700) and its tmp subdirectory.
    pub fn prepare(&self, id: i64) -> io::Result<()> {
        let dir = self.dir(id);
        std::fs::create_dir_all(&dir)?;
        restrict(&dir)?;
        std::fs::create_dir_all(self.tmp(id))?;
        Ok(())
    }

    /// Remove a run's directory. Only the wyd-owned run root is ever touched;
    /// a symlinked tmp is unlinked, never followed.
    pub fn remove(&self, id: i64) -> io::Result<()> {
        let dir = self.dir(id);
        let root = std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        let resolved = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if !resolved.starts_with(&root) || resolved == root {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing to remove {} outside the run root", dir.display()),
            ));
        }
        std::fs::remove_dir_all(&dir)
    }
}

#[cfg(unix)]
fn restrict(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wyd-logtest-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn cap_drops_the_tail_and_keeps_cursors_stable() {
        let dir = tmpdir("cap");
        let path = dir.join("stdout");
        let state = Arc::new(Mutex::new(LogState::default()));
        let budget = Arc::new(GlobalBudget::new(1024, 0));
        let mut sink = LogSink::open(&path, Stream::Stdout, 10, state.clone(), budget).unwrap();

        sink.write_all_bounded(b"0123456789ABCDEF").unwrap();
        sink.finish().unwrap();

        let first = read_chunk(&path, 0, 1024).unwrap();
        assert_eq!(first.data, "0123456789");
        assert_eq!(first.next_cursor, 10);
        // Truncation is recorded even though the pipe kept being drained.
        assert!(state.lock().stdout_truncated);
        assert_eq!(state.lock().stdout_bytes, 10);
        // Reading from the same cursor twice is stable.
        assert_eq!(read_chunk(&path, 0, 4).unwrap().data, "0123");
        assert_eq!(read_chunk(&path, 4, 4).unwrap().data, "4567");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn global_budget_stops_writes_without_error() {
        let dir = tmpdir("budget");
        let path = dir.join("stdout");
        let state = Arc::new(Mutex::new(LogState::default()));
        // Global budget already exhausted by a previous run.
        let budget = Arc::new(GlobalBudget::new(4, 4));
        let mut sink = LogSink::open(&path, Stream::Stdout, 100, state, budget).unwrap();
        sink.write_all_bounded(b"hello").unwrap();
        sink.finish().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_reads_as_finished_empty() {
        let chunk = read_chunk(Path::new("/nonexistent/wyd/stdout"), 0, 16).unwrap();
        assert!(chunk.data.is_empty());
        assert!(chunk.eof);
    }

    #[test]
    fn remove_refuses_paths_outside_the_root() {
        let dir = tmpdir("remove");
        let paths = RunPaths::new(dir.join("runs"));
        std::fs::create_dir_all(paths.dir(1)).unwrap();
        paths.remove(1).unwrap();
        assert!(!paths.dir(1).exists());
        // Root itself is never removed.
        std::fs::create_dir_all(paths.root()).unwrap();
        assert!(paths.remove(0).is_err() || paths.root().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
