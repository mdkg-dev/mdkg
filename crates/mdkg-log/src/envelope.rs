//! The content-addressed event envelope.
//!
//! An envelope's id is the sha256 of its RFC 8785 canonical JSON form with
//! `id` and `sig` removed, so ids are content addresses: identical events
//! have identical ids, and any change to a sealed event is detectable.

use mdkg_format::Digest;
use mdkg_format::canonical::CanonicalError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::hlc::Hlc;

/// The current envelope schema version.
pub const ENVELOPE_SCHEMA: u32 = 1;

/// How long a payload is kept. The hash commitment in
/// the envelope always remains, even after the payload expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "class")]
pub enum Retention {
    /// Never stored; only the commitment is logged.
    Ephemeral,
    /// Kept until `until_ms` (Unix milliseconds).
    Ttl { until_ms: u64 },
    /// Kept until deliberately deleted or shredded.
    Durable,
    /// Moved to cold storage but kept.
    Archive,
}

/// A reference to a payload stored outside the log (invariant 13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadRef {
    pub kind: String,
    pub media_type: String,
    /// `sha256:<hex>` of the payload bytes.
    pub digest: String,
    pub size: u64,
    /// Opaque location in the payload store.
    pub storage_ref: String,
    pub retention: Retention,
}

/// Who may see an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Private,
    Members,
    Public,
}

/// W3C trace context (invariant 24).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    pub traceparent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracestate: Option<String>,
}

/// An event before its id is computed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub schema: u32,
    /// `graph_id` of the owning shard.
    pub shard: String,
    /// Ids of the events this one directly follows (a Merkle DAG).
    pub parents: Vec<String>,
    pub actor: String,
    /// `mdkg.<domain>.<verb>`.
    #[serde(rename = "type")]
    pub event_type: String,
    pub subject: Vec<String>,
    /// Schema-declared inline-safe metadata only (IDs, hashes, states, counts).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub inline: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<PayloadRef>,
    pub hlc: Hlc,
    /// Assigned by the shard's sequencer once sequenced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub idempotency_key: String,
    /// End of the dedup window, Unix milliseconds.
    pub valid_until: u64,
    pub visibility: Visibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceContext>,
}

/// A sealed event: its id is the hash of its contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub id: String,
    #[serde(flatten)]
    pub draft: Draft,
    /// Reserved for signatures; empty until signing lands (decision 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    Canonical(CanonicalError),
    Serialize(String),
    IdMismatch {
        stored: String,
        computed: String,
    },
    BadType(String),
    /// Stored bytes differ from the canonical form (extra or default fields).
    NotCanonical,
}

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvelopeError::Canonical(e) => write!(f, "{e}"),
            EnvelopeError::Serialize(e) => write!(f, "cannot serialize envelope: {e}"),
            EnvelopeError::IdMismatch { stored, computed } => {
                write!(
                    f,
                    "envelope id {stored} does not match its contents ({computed})"
                )
            }
            EnvelopeError::BadType(t) => {
                write!(f, "event type {t} must look like mdkg.<domain>.<verb>")
            }
            EnvelopeError::NotCanonical => write!(f, "stored envelope is not in canonical form"),
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// The id covers everything except `seq`: sequencing happens after sealing
/// and must not change an event's identity (children reference it by id).
fn content_id(draft: &Draft) -> Result<String, EnvelopeError> {
    let unsequenced = Draft {
        seq: None,
        ..draft.clone()
    };
    let value =
        serde_json::to_value(&unsequenced).map_err(|e| EnvelopeError::Serialize(e.to_string()))?;
    Digest::of_record(&value)
        .map(|d| d.to_string())
        .map_err(EnvelopeError::Canonical)
}

fn valid_type(t: &str) -> bool {
    let parts: Vec<&str> = t.split('.').collect();
    parts.len() >= 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

impl Draft {
    /// Seals the draft, computing its content-addressed id.
    pub fn seal(self) -> Result<Envelope, EnvelopeError> {
        if !valid_type(&self.event_type) {
            return Err(EnvelopeError::BadType(self.event_type));
        }
        let id = content_id(&self)?;
        Ok(Envelope {
            id,
            draft: self,
            sig: None,
        })
    }
}

impl Envelope {
    /// Recomputes the id and checks it matches (tamper detection).
    pub fn verify(&self) -> Result<(), EnvelopeError> {
        let computed = content_id(&self.draft)?;
        if computed == self.id {
            Ok(())
        } else {
            Err(EnvelopeError::IdMismatch {
                stored: self.id.clone(),
                computed,
            })
        }
    }

    /// Parses one stored envelope strictly: the text must be exactly the
    /// canonical form of what it parses to, so unknown fields, explicit
    /// defaults or any other non-canonical bytes are rejected, and the id
    /// must match the contents.
    pub fn parse_canonical(text: &str) -> Result<Envelope, EnvelopeError> {
        let event: Envelope =
            serde_json::from_str(text).map_err(|e| EnvelopeError::Serialize(e.to_string()))?;
        if event.to_canonical_json()? != text {
            return Err(EnvelopeError::NotCanonical);
        }
        event.verify()?;
        Ok(event)
    }

    /// Canonical JSON of the whole envelope, for storage.
    pub fn to_canonical_json(&self) -> Result<String, EnvelopeError> {
        let value =
            serde_json::to_value(self).map_err(|e| EnvelopeError::Serialize(e.to_string()))?;
        mdkg_format::canonical::to_canonical_string(&value).map_err(EnvelopeError::Canonical)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn draft(key: &str, parents: Vec<String>, wall: u64) -> Draft {
        Draft {
            schema: ENVELOPE_SCHEMA,
            shard: "0192f4c2-1b2a-7c3d-8e4f-0123456789ab".into(),
            parents,
            actor: "human:nick".into(),
            event_type: "mdkg.lease.claim".into(),
            subject: vec![
                "mdkg://0192f4c2-1b2a-7c3d-8e4f-0123456789ab/0192f4c2-1b2a-7c3d-9e4f-0123456789ab"
                    .into(),
            ],
            inline: Map::new(),
            payload: None,
            hlc: Hlc { wall, logical: 0 },
            seq: None,
            idempotency_key: key.into(),
            valid_until: wall + 60_000,
            visibility: Visibility::Private,
            trace: None,
        }
    }

    #[test]
    fn identical_drafts_have_identical_ids() {
        let a = draft("k1", vec![], 1).seal().unwrap();
        let b = draft("k1", vec![], 1).seal().unwrap();
        assert_eq!(a.id, b.id);
        assert!(a.id.starts_with("sha256:"));
        a.verify().unwrap();
    }

    #[test]
    fn tampering_is_detected() {
        let mut e = draft("k1", vec![], 1).seal().unwrap();
        e.draft.actor = "agent:mallory".into();
        assert!(matches!(e.verify(), Err(EnvelopeError::IdMismatch { .. })));
    }

    #[test]
    fn signature_is_not_part_of_the_id() {
        let mut e = draft("k1", vec![], 1).seal().unwrap();
        e.sig = Some("ssh-sig:...".into());
        e.verify().unwrap();
    }

    #[test]
    fn rejects_malformed_types() {
        let mut d = draft("k1", vec![], 1);
        d.event_type = "Lease.Claim".into();
        assert!(matches!(d.seal(), Err(EnvelopeError::BadType(_))));
    }

    #[test]
    fn storage_form_round_trips() {
        let mut d = draft("k1", vec![], 1);
        d.payload = Some(PayloadRef {
            kind: "artifact".into(),
            media_type: "application/pdf".into(),
            digest: mdkg_format::Digest::of_bytes(b"pdf").to_string(),
            size: 3,
            storage_ref: "blob/1".into(),
            retention: Retention::Ttl { until_ms: 99 },
        });
        d.trace = Some(TraceContext {
            traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
            tracestate: None,
        });
        let e = d.seal().unwrap();
        let text = e.to_canonical_json().unwrap();
        let back: Envelope = serde_json::from_str(&text).unwrap();
        assert_eq!(back, e);
        back.verify().unwrap();
    }
}
