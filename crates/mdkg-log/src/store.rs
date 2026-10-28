//! Durable storage for a shard's log: canonical-JSON lines in segment files.
//!
//! This is the only I/O in the crate. The log logic stays in `log.rs`; this
//! module just persists and reloads envelopes. A new segment starts once the
//! current one reaches `segment_events` events.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::envelope::Envelope;

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Corrupt {
        file: PathBuf,
        line: usize,
        message: String,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "{e}"),
            StoreError::Corrupt {
                file,
                line,
                message,
            } => {
                write!(f, "{}:{line}: {message}", file.display())
            }
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

#[derive(Debug, Clone)]
pub struct SegmentStore {
    dir: PathBuf,
    segment_events: usize,
}

impl SegmentStore {
    pub fn open(dir: impl Into<PathBuf>, segment_events: usize) -> Result<Self, StoreError> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            segment_events: segment_events.max(1),
        })
    }

    fn segments(&self) -> Result<Vec<PathBuf>, StoreError> {
        let mut files: Vec<PathBuf> = fs::read_dir(&self.dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .filter(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            })
            .collect();
        files.sort();
        Ok(files)
    }

    fn segment_path(&self, n: usize) -> PathBuf {
        self.dir.join(format!("{n:08}.jsonl"))
    }

    /// Drops a torn (unterminated) last line left by a crash mid-write. It
    /// was never acknowledged: `append` only returns after the full line and
    /// its newline are synced.
    fn repair_torn_tail(path: &Path) -> Result<(), StoreError> {
        let bytes = fs::read(path)?;
        if bytes.is_empty() || bytes.ends_with(b"\n") {
            return Ok(());
        }
        let keep = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        let file = fs::OpenOptions::new().write(true).open(path)?;
        file.set_len(keep as u64)?;
        file.sync_data()?;
        Ok(())
    }

    /// Appends envelopes, rolling to a new segment when the current one is full.
    pub fn append(&self, events: &[Envelope]) -> Result<(), StoreError> {
        let mut segments = self.segments()?;
        if let Some(last) = segments.last() {
            Self::repair_torn_tail(last)?;
        }
        let mut current = segments.pop();
        let mut count = match &current {
            Some(p) => fs::read_to_string(p)?.lines().count(),
            None => 0,
        };
        for event in events {
            if current.is_none() || count >= self.segment_events {
                let n = current
                    .as_deref()
                    .and_then(Path::file_stem)
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<usize>().ok())
                    .map_or(1, |n| n + 1);
                current = Some(self.segment_path(n));
                count = 0;
            }
            let line = event.to_canonical_json().map_err(|e| StoreError::Corrupt {
                file: current.clone().unwrap_or_default(),
                line: count + 1,
                message: e.to_string(),
            })?;
            let path = current
                .as_ref()
                .map_or_else(|| self.segment_path(1), Clone::clone);
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            writeln!(file, "{line}")?;
            file.sync_data()?;
            count += 1;
        }
        Ok(())
    }

    /// Reads every envelope in order. Each line must be canonical and verify
    /// (tamper detection). A torn last line from a crash mid-write is ignored.
    pub fn read_all(&self) -> Result<Vec<Envelope>, StoreError> {
        let segments = self.segments()?;
        let mut out = Vec::new();
        for (n, file) in segments.iter().enumerate() {
            let text = fs::read_to_string(file)?;
            let torn_tail = n + 1 == segments.len() && !text.is_empty() && !text.ends_with('\n');
            let lines: Vec<&str> = text.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if torn_tail && i + 1 == lines.len() {
                    break;
                }
                let event = Envelope::parse_canonical(line).map_err(|e| StoreError::Corrupt {
                    file: file.clone(),
                    line: i + 1,
                    message: e.to_string(),
                })?;
                out.push(event);
            }
        }
        Ok(out)
    }
}
