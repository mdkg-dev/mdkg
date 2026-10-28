//! Shard export and independent verification without the running coordinator.
//!
//! An export holds a shard's events and checkpoint headers. Anyone can verify
//! it without the coordinator: every event id matches its contents, every
//! parent precedes its child, the header chain links up, and the latest
//! header's log root matches the exported events.

use serde::{Deserialize, Serialize};

use crate::checkpoint::{CheckpointHeader, verify_headers};
use crate::envelope::Envelope;
use crate::log::{Log, verify_chain};

pub const EXPORT_FORMAT: &str = "mdkg-shard-export";
pub const EXPORT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardExport {
    pub format: String,
    pub version: u32,
    pub shard: String,
    pub events: Vec<Envelope>,
    pub headers: Vec<CheckpointHeader>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    Format(String),
    Events(String),
    Headers(String),
    RootMismatch {
        header: String,
        expected: String,
        actual: String,
    },
    ForeignEvent(String),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::Format(m) | VerifyError::Events(m) | VerifyError::Headers(m) => {
                f.write_str(m)
            }
            VerifyError::RootMismatch {
                header,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "header {header} commits to log root {expected}, but the events hash to {actual}"
                )
            }
            VerifyError::ForeignEvent(id) => write!(f, "event {id} belongs to another shard"),
        }
    }
}

impl std::error::Error for VerifyError {}

impl ShardExport {
    pub fn new(log: &Log, headers: Vec<CheckpointHeader>) -> Self {
        Self {
            format: EXPORT_FORMAT.into(),
            version: EXPORT_VERSION,
            shard: log.shard().to_string(),
            events: log.events().to_vec(),
            headers,
        }
    }

    /// Verifies the export with no coordinator involved.
    pub fn verify(&self) -> Result<(), VerifyError> {
        if self.format != EXPORT_FORMAT || self.version != EXPORT_VERSION {
            return Err(VerifyError::Format(format!(
                "unsupported export {}/{}",
                self.format, self.version
            )));
        }
        if let Some(e) = self.events.iter().find(|e| e.draft.shard != self.shard) {
            return Err(VerifyError::ForeignEvent(e.id.clone()));
        }
        verify_chain(&self.events).map_err(|e| VerifyError::Events(e.to_string()))?;
        if let Some(h) = self.headers.iter().find(|h| h.draft.shard != self.shard) {
            return Err(VerifyError::Headers(format!(
                "header {} belongs to shard {}, not {}",
                h.id, h.draft.shard, self.shard
            )));
        }
        verify_headers(&self.headers).map_err(|e| VerifyError::Headers(e.to_string()))?;
        // Every exported event must rebuild; nothing may be silently skipped.
        let log = self.rebuild()?;
        if let Some(last) = self.headers.last() {
            let actual = log.root().to_string();
            if last.draft.log_root != actual {
                return Err(VerifyError::RootMismatch {
                    header: last.id.clone(),
                    expected: last.draft.log_root.clone(),
                    actual,
                });
            }
        }
        Ok(())
    }

    fn rebuild(&self) -> Result<Log, VerifyError> {
        let mut log = Log::new(self.shard.clone());
        for event in &self.events {
            log.replay(event.clone())
                .map_err(|e| VerifyError::Events(e.to_string()))?;
        }
        Ok(log)
    }

    /// Parses an export strictly: every event must be in canonical form, so
    /// unknown fields or extra bytes can't ride along unhashed.
    pub fn parse(text: &str) -> Result<Self, VerifyError> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| VerifyError::Format(e.to_string()))?;
        let export: ShardExport = serde_json::from_value(value.clone())
            .map_err(|e| VerifyError::Format(e.to_string()))?;
        let raw_events = value
            .get("events")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        for (raw, event) in raw_events.iter().zip(&export.events) {
            let raw_text = mdkg_format::canonical::to_canonical_string(raw)
                .map_err(|e| VerifyError::Format(e.to_string()))?;
            let canonical = event
                .to_canonical_json()
                .map_err(|e| VerifyError::Format(e.to_string()))?;
            if raw_text != canonical {
                return Err(VerifyError::Format(format!(
                    "event {} is not in canonical form",
                    event.id
                )));
            }
        }
        Ok(export)
    }

    /// Imports a verified export into a fresh log.
    pub fn into_log(self) -> Result<Log, VerifyError> {
        self.verify()?;
        self.rebuild()
    }
}
