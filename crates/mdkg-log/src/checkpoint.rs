//! Checkpoint headers: a verifiable chain of shard roots, like block headers.
//!
//! Each header commits to the log (by Merkle root), the knowledge plane (by
//! Merkle root over node files) and the previous header. A light replica that
//! trusts one header can verify any node or event with an inclusion proof.

use mdkg_format::Digest;
use mdkg_format::canonical::CanonicalError;
use serde::{Deserialize, Serialize};

use crate::hlc::Hlc;
use crate::merkle::MerkleTree;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderDraft {
    pub shard: String,
    pub seq_from: u64,
    pub seq_to: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_header: Option<String>,
    pub log_root: String,
    pub knowledge_root: String,
    pub hlc: Hlc,
    /// The sequencer that produced this checkpoint.
    pub sequencer: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointHeader {
    pub id: String,
    #[serde(flatten)]
    pub draft: HeaderDraft,
    /// Reserved for signatures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderError {
    Canonical(CanonicalError),
    IdMismatch(String),
    BrokenChain {
        header: String,
        expected_prev: Option<String>,
    },
    SeqGap {
        header: String,
    },
}

impl std::fmt::Display for HeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeaderError::Canonical(e) => write!(f, "{e}"),
            HeaderError::IdMismatch(id) => {
                write!(f, "checkpoint header {id} does not match its contents")
            }
            HeaderError::BrokenChain {
                header,
                expected_prev,
            } => {
                write!(
                    f,
                    "checkpoint header {header} does not follow {expected_prev:?}"
                )
            }
            HeaderError::SeqGap { header } => {
                write!(f, "checkpoint header {header} leaves a sequence gap")
            }
        }
    }
}

impl std::error::Error for HeaderError {}

fn header_id(draft: &HeaderDraft) -> Result<String, HeaderError> {
    let value = serde_json::to_value(draft).unwrap_or(serde_json::Value::Null);
    Digest::of_record(&value)
        .map(|d| d.to_string())
        .map_err(HeaderError::Canonical)
}

impl HeaderDraft {
    pub fn seal(self) -> Result<CheckpointHeader, HeaderError> {
        Ok(CheckpointHeader {
            id: header_id(&self)?,
            draft: self,
            sig: None,
        })
    }
}

impl CheckpointHeader {
    pub fn verify(&self) -> Result<(), HeaderError> {
        if header_id(&self.draft)? == self.id {
            Ok(())
        } else {
            Err(HeaderError::IdMismatch(self.id.clone()))
        }
    }
}

/// Merkle root over knowledge-plane files: relative path -> blob hash.
pub fn knowledge_root(files: Vec<(String, Digest)>) -> Digest {
    MerkleTree::build(files).root()
}

/// Verifies a header chain: ids match, each links to the previous, and
/// sequence ranges are contiguous.
pub fn verify_headers(headers: &[CheckpointHeader]) -> Result<(), HeaderError> {
    let mut prev: Option<&CheckpointHeader> = None;
    for header in headers {
        header.verify()?;
        let expected = prev.map(|p| p.id.clone());
        if header.draft.prev_header != expected {
            return Err(HeaderError::BrokenChain {
                header: header.id.clone(),
                expected_prev: expected,
            });
        }
        if let Some(p) = prev
            && header.draft.seq_from != p.draft.seq_to + 1
        {
            return Err(HeaderError::SeqGap {
                header: header.id.clone(),
            });
        }
        prev = Some(header);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(prev: Option<String>, from: u64, to: u64) -> HeaderDraft {
        HeaderDraft {
            shard: "s".into(),
            seq_from: from,
            seq_to: to,
            prev_header: prev,
            log_root: Digest::of_bytes(b"log").to_string(),
            knowledge_root: knowledge_root(vec![(
                ".mdkg/work/task-1.md".into(),
                Digest::of_bytes(b"x"),
            )])
            .to_string(),
            hlc: Hlc {
                wall: 1,
                logical: 0,
            },
            sequencer: "local".into(),
        }
    }

    #[test]
    fn a_chain_verifies_and_breaks_when_reordered() {
        let a = draft(None, 1, 10).seal().unwrap();
        let b = draft(Some(a.id.clone()), 11, 20).seal().unwrap();
        verify_headers(&[a.clone(), b.clone()]).unwrap();
        assert!(verify_headers(&[b.clone(), a.clone()]).is_err());
        let gap = draft(Some(a.id.clone()), 15, 20).seal().unwrap();
        assert!(matches!(
            verify_headers(&[a, gap]),
            Err(HeaderError::SeqGap { .. })
        ));
    }
}
