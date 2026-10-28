//! Node identity in format 0.7 (section 4 of the spec): `<type>-<uuid>`,
//! with a node type such as `goal` and a UUID such as
//! `0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b`.
//!
//! - The UUID is the identity: an RFC 9562 UUID in lowercase, hyphenated
//!   hex. mdkg mints version 7, locally, with no counter or lease; other
//!   versions come only from migrated graphs (0.6.0's identity v2 used v4).
//! - The type is a checked hint: `retype` changes it and `validate` flags a
//!   stale one, so two ids with the same UUID name the same node
//!   ([`NodeId::same_node`]). Key maps by [`NodeId::uuid`].
//! - Refs add an optional graph and revision pin ([`NodeRef`]):
//!   `mdkg://<graph_id>/<id>` and `<ref>@sha256:<hex>`.
//! - A unique prefix of the UUID, with or without the type, names a node
//!   the way a short SHA names a commit ([`IdPrefix`]).
//!
//! Everything here is sans-IO except [`NodeId::mint`] and
//! [`Uuid::now`], which read the clock and the OS random source and sit
//! behind the default `mint` feature.

use std::fmt;
use std::str::FromStr;

/// Why a string isn't an id, a UUID, a ref or a prefix. The messages never
/// echo the input, which may contain secrets or terminal control characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdError {
    /// No `<type>-` before the UUID.
    MissingType,
    /// The type isn't `[a-z][a-z0-9_]*`, at most 32 characters.
    BadType,
    /// Not 36 characters of lowercase hex in 8-4-4-4-12 groups.
    BadUuid,
    /// A UUID whose version isn't one RFC 9562 defines (1 to 8).
    BadVersion,
    /// A UUID whose variant bits aren't `10` (RFC 9562).
    BadVariant,
    /// A Unix millisecond time past 2^48 - 1, which a UUIDv7 can't hold.
    TimeOutOfRange,
    /// A `mdkg://` ref that isn't `mdkg://<graph_id>/<type>-<uuid>`.
    BadUri,
    /// A pin that isn't `@sha256:` and 64 lowercase hex digits.
    BadPin,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            IdError::MissingType => "an id must be <type>-<uuid>",
            IdError::BadType => {
                "an id's type must be [a-z][a-z0-9_]*, at most 32 characters, and not 8 hex digits"
            }
            IdError::BadUuid => "a UUID must be lowercase hex in 8-4-4-4-12 groups",
            IdError::BadVersion => "a UUID's version must be 1 to 8 (RFC 9562)",
            IdError::BadVariant => "an id's UUID must have the RFC 9562 variant",
            IdError::TimeOutOfRange => "a UUIDv7 time must be below 2^48 milliseconds",
            IdError::BadUri => "a graph ref must be mdkg://<graph_id>/<type>-<uuid>",
            IdError::BadPin => "a pin must be @sha256: and 64 lowercase hex digits",
        })
    }
}

impl std::error::Error for IdError {}

/// The largest Unix millisecond time a UUIDv7 holds.
pub const MAX_UNIX_MS: u64 = 0xFFFF_FFFF_FFFF;

/// An RFC 9562 UUID: a version from 1 to 8 and the variant `10`. mdkg
/// mints version 7 (a 48-bit Unix millisecond time, 12 bits of `rand_a`, 62
/// of `rand_b`), which orders by its bytes in time order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Uuid([u8; 16]);

/// The type's former name, from when only UUIDv7s were ids.
pub type Uuid7 = Uuid;

impl Uuid {
    /// A UUIDv7 from its parts: the time, then `rand_a` (the low 12 bits are
    /// used) and `rand_b` (the low 62 bits are used).
    pub fn from_fields(unix_ms: u64, rand_a: u16, rand_b: u64) -> Result<Self, IdError> {
        if unix_ms > MAX_UNIX_MS {
            return Err(IdError::TimeOutOfRange);
        }
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&unix_ms.to_be_bytes()[2..]);
        bytes[6..8].copy_from_slice(&(0x7000 | (rand_a & 0x0FFF)).to_be_bytes());
        let b = (rand_b & 0x3FFF_FFFF_FFFF_FFFF) | 0x8000_0000_0000_0000;
        bytes[8..].copy_from_slice(&b.to_be_bytes());
        Ok(Self(bytes))
    }

    /// A UUIDv7 from a time and 10 random bytes, laid out as
    /// [`crate::uuid7::uuid_v7`] lays them out.
    pub fn from_random(unix_ms: u64, random: [u8; 10]) -> Result<Self, IdError> {
        let rand_a = u16::from_be_bytes([random[0], random[1]]);
        let mut b = [0u8; 8];
        b.copy_from_slice(&random[2..]);
        Self::from_fields(unix_ms, rand_a, u64::from_be_bytes(b))
    }

    /// Checks the version and variant of 16 bytes.
    pub fn from_bytes(bytes: [u8; 16]) -> Result<Self, IdError> {
        if !(1..=8).contains(&(bytes[6] >> 4)) {
            return Err(IdError::BadVersion);
        }
        if bytes[8] >> 6 != 0b10 {
            return Err(IdError::BadVariant);
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// The RFC 9562 version: 7 for every UUID mdkg mints.
    pub fn version(&self) -> u8 {
        self.0[6] >> 4
    }

    /// The Unix millisecond time a UUIDv7 was minted at; `None` for other
    /// versions.
    pub fn unix_ms(&self) -> Option<u64> {
        if self.version() != 7 {
            return None;
        }
        let mut b = [0u8; 8];
        b[2..].copy_from_slice(&self.0[..6]);
        Some(u64::from_be_bytes(b))
    }

    /// The 32 hex digits with no hyphens, for prefix queries.
    pub fn simple(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Parses lowercase hyphenated hex (the only form mdkg writes).
    pub fn parse(value: &str) -> Result<Self, IdError> {
        let b = value.as_bytes();
        if b.len() != 36 {
            return Err(IdError::BadUuid);
        }
        let mut bytes = [0u8; 16];
        let mut nibbles = 0usize;
        for (i, &c) in b.iter().enumerate() {
            if matches!(i, 8 | 13 | 18 | 23) {
                if c != b'-' {
                    return Err(IdError::BadUuid);
                }
                continue;
            }
            let v = lower_hex(c).ok_or(IdError::BadUuid)?;
            bytes[nibbles / 2] |= if nibbles.is_multiple_of(2) { v << 4 } else { v };
            nibbles += 1;
        }
        Self::from_bytes(bytes)
    }

    /// A UUIDv7 for now, from the OS random source. UUIDs minted this way in
    /// one process strictly increase (the `uuid` crate's shared v7 context),
    /// even if the clock steps back.
    #[cfg(feature = "mint")]
    pub fn now() -> Self {
        Self(uuid::Uuid::now_v7().into_bytes())
    }
}

fn lower_hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let h = self.simple();
        write!(
            f,
            "{}-{}-{}-{}-{}",
            &h[..8],
            &h[8..12],
            &h[12..16],
            &h[16..20],
            &h[20..]
        )
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Uuid({self})")
    }
}

impl FromStr for Uuid {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        Self::parse(s)
    }
}

/// Mints UUIDv7s that strictly increase from caller-supplied times and
/// randomness, for deterministic callers: tests, simulations, and migrate
/// minting from `created` dates.
///
/// It follows RFC 9562 method 1: `rand_a` is a 12-bit counter, seeded from
/// 11 random bits whenever the time moves forward. A time that doesn't
/// move forward reuses the last one and bumps the counter, and a full
/// counter carries into the next millisecond.
#[derive(Debug, Clone, Default)]
pub struct Minter {
    last: Option<(u64, u16)>,
}

impl Minter {
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// The next UUIDv7: at `unix_ms` unless that would not sort after the
    /// last one.
    pub fn next(&mut self, unix_ms: u64, random: [u8; 10]) -> Result<Uuid7, IdError> {
        let seed = u16::from_be_bytes([random[0], random[1]]) & 0x07FF;
        let (ms, counter) = match self.last {
            Some((last_ms, counter)) if unix_ms <= last_ms => {
                if counter < 0x0FFF {
                    (last_ms, counter + 1)
                } else {
                    (last_ms.checked_add(1).ok_or(IdError::TimeOutOfRange)?, seed)
                }
            }
            _ => (unix_ms, seed),
        };
        let mut b = [0u8; 8];
        b.copy_from_slice(&random[2..]);
        let uuid = Uuid7::from_fields(ms, counter, u64::from_be_bytes(b))?;
        self.last = Some((ms, counter));
        Ok(uuid)
    }
}

/// `[a-z][a-z0-9_]*`, at most 32 characters, and not exactly 8 hex digits
/// (which [`IdPrefix`] reads as a UUID's first group): a type an id can
/// carry. Which types a graph accepts is validation's business, not the id
/// grammar's.
pub fn is_id_type(value: &str) -> bool {
    let mut chars = value.chars();
    value.len() <= 32
        && matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
        && !is_hex_group(value)
}

/// Exactly 8 lowercase hex digits: a UUID's first group.
fn is_hex_group(value: &str) -> bool {
    value.len() == 8 && value.bytes().all(|c| lower_hex(c).is_some())
}

/// A node id: `<type>-<uuid>`.
///
/// Equality and hashing compare both parts, as the strings do. Two ids with
/// one UUID and different types name the same node under a stale type;
/// compare them with [`NodeId::same_node`].
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct NodeId {
    node_type: String,
    uuid: Uuid7,
}

impl NodeId {
    pub fn new(node_type: &str, uuid: Uuid7) -> Result<Self, IdError> {
        if !is_id_type(node_type) {
            return Err(IdError::BadType);
        }
        Ok(Self {
            node_type: node_type.to_string(),
            uuid,
        })
    }

    /// A new id for now (see [`Uuid7::now`]).
    #[cfg(feature = "mint")]
    pub fn mint(node_type: &str) -> Result<Self, IdError> {
        Self::new(node_type, Uuid7::now())
    }

    /// Parses `<type>-<uuid>`, lowercase, exactly as mdkg writes it.
    pub fn parse(value: &str) -> Result<Self, IdError> {
        let (node_type, uuid) = value.split_once('-').ok_or(IdError::MissingType)?;
        if node_type.is_empty() {
            return Err(IdError::MissingType);
        }
        if !is_id_type(node_type) {
            return Err(IdError::BadType);
        }
        Self::new(node_type, Uuid7::parse(uuid)?)
    }

    pub fn node_type(&self) -> &str {
        &self.node_type
    }

    pub fn uuid(&self) -> Uuid7 {
        self.uuid
    }

    /// The same UUID, whatever the type says.
    pub fn same_node(&self, other: &NodeId) -> bool {
        self.uuid == other.uuid
    }

    /// The same node under another type (what `retype` writes).
    pub fn with_type(&self, node_type: &str) -> Result<Self, IdError> {
        Self::new(node_type, self.uuid)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.node_type, self.uuid)
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({self})")
    }
}

impl FromStr for NodeId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        Self::parse(s)
    }
}

/// Ids order by UUID (time), then type.
impl Ord for NodeId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.uuid
            .cmp(&other.uuid)
            .then_with(|| self.node_type.cmp(&other.node_type))
    }
}

impl PartialOrd for NodeId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A reference to a node: an id, optionally in another graph and optionally
/// pinned to one revision of the node.
///
/// | Form | Example |
/// |---|---|
/// | in this graph | `goal-0192f3a4-…` |
/// | in a graph | `mdkg://0192f3a0-…/goal-0192f3a4-…` |
/// | pinned | either form, then `@sha256:<64 hex>` |
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct NodeRef {
    pub graph: Option<Uuid7>,
    pub id: NodeId,
    /// The 64 hex digits after `@sha256:`.
    pub pin: Option<String>,
}

impl NodeRef {
    pub fn local(id: NodeId) -> Self {
        Self {
            graph: None,
            id,
            pin: None,
        }
    }

    pub fn parse(value: &str) -> Result<Self, IdError> {
        let (body, pin) = match value.split_once('@') {
            Some((body, pin)) => {
                let hex = pin.strip_prefix("sha256:").ok_or(IdError::BadPin)?;
                if hex.len() != 64 || !hex.bytes().all(|c| lower_hex(c).is_some()) {
                    return Err(IdError::BadPin);
                }
                (body, Some(hex.to_string()))
            }
            None => (value, None),
        };
        let (graph, id) = match body.strip_prefix("mdkg://") {
            Some(rest) => {
                let (graph, id) = rest.split_once('/').ok_or(IdError::BadUri)?;
                let graph = Uuid7::parse(graph).map_err(|_| IdError::BadUri)?;
                (Some(graph), NodeId::parse(id).map_err(|_| IdError::BadUri)?)
            }
            None => (None, NodeId::parse(body)?),
        };
        Ok(Self { graph, id, pin })
    }
}

impl fmt::Display for NodeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(graph) = &self.graph {
            write!(f, "mdkg://{graph}/")?;
        }
        write!(f, "{}", self.id)?;
        if let Some(pin) = &self.pin {
            write!(f, "@sha256:{pin}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for NodeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeRef({self})")
    }
}

impl FromStr for NodeRef {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        Self::parse(s)
    }
}

/// A short form of an id: the leading hex digits of its UUID, with or
/// without `<type>-` in front, as a short SHA names a commit.
///
/// - `0192f3a4`, `0192f3a4-7b`, `0192f3a47b` and `task-0192f3` are prefixes.
///   Hyphens are optional, but where present they sit where the full form
///   has them.
/// - A first segment of exactly 8 hex digits is the UUID's first group, not
///   a type: [`is_id_type`] refuses such types.
/// - Input is read case-insensitively; at least [`IdPrefix::MIN_HEX`] hex
///   digits are needed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdPrefix {
    node_type: Option<String>,
    hex: String,
}

impl IdPrefix {
    /// UUIDv7's leading digits are time: 4 digits narrow to about 194 days,
    /// and a burst minted in one second needs about 12.
    pub const MIN_HEX: usize = 4;

    /// `None` when the handle isn't shaped like a prefix.
    pub fn parse(handle: &str) -> Option<Self> {
        let handle = handle.to_ascii_lowercase();
        let (node_type, rest) = match handle.split_once('-') {
            Some((first, _)) if is_hex_group(first) => (None, handle.as_str()),
            Some((first, rest)) => {
                if !is_id_type(first) {
                    return None;
                }
                (Some(first.to_string()), rest)
            }
            None => (None, handle.as_str()),
        };
        let mut hex = String::with_capacity(32);
        for (i, c) in rest.bytes().enumerate() {
            if c == b'-' {
                if !matches!(i, 8 | 13 | 18 | 23) {
                    return None;
                }
            } else if lower_hex(c).is_some() {
                // A digit where the full form has a hyphen means none are used.
                if matches!(i, 8 | 13 | 18 | 23) && rest.contains('-') {
                    return None;
                }
                hex.push(c as char);
            } else {
                return None;
            }
        }
        (hex.len() >= Self::MIN_HEX && hex.len() <= 32).then_some(Self { node_type, hex })
    }

    pub fn node_type(&self) -> Option<&str> {
        self.node_type.as_deref()
    }

    /// The hex digits, lowercase, no hyphens: a prefix of [`Uuid7::simple`].
    pub fn hex(&self) -> &str {
        &self.hex
    }

    pub fn matches(&self, id: &NodeId) -> bool {
        self.node_type
            .as_deref()
            .is_none_or(|t| t == id.node_type())
            && id.uuid().simple().starts_with(&self.hex)
    }
}

/// What a prefix matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrefixMatch<'a> {
    None,
    Unique(&'a NodeId),
    /// Every match, in id order.
    Ambiguous(Vec<&'a NodeId>),
}

/// Matches a prefix against a set of ids. An id listed twice counts once.
pub fn match_prefix<'a>(
    prefix: &IdPrefix,
    ids: impl IntoIterator<Item = &'a NodeId>,
) -> PrefixMatch<'a> {
    let mut found: Vec<&NodeId> = ids.into_iter().filter(|id| prefix.matches(id)).collect();
    found.sort();
    found.dedup();
    match found.len() {
        0 => PrefixMatch::None,
        1 => PrefixMatch::Unique(found[0]),
        _ => PrefixMatch::Ambiguous(found),
    }
}

/// The shortest prefix of each id's UUID, at least [`IdPrefix::MIN_HEX`]
/// digits, that no other id in the set shares: what to show for short ids.
pub fn shortest_unique_hex(id: &NodeId, all: &[NodeId]) -> usize {
    let mine = id.uuid().simple();
    let longest_shared = all
        .iter()
        .filter(|other| other.uuid() != id.uuid())
        .map(|other| {
            other
                .uuid()
                .simple()
                .bytes()
                .zip(mine.bytes())
                .take_while(|(a, b)| a == b)
                .count()
        })
        .max()
        .unwrap_or(0);
    (longest_shared + 1).clamp(IdPrefix::MIN_HEX, 32)
}

impl serde::Serialize for Uuid {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Uuid {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Uuid7::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for NodeId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for NodeId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        NodeId::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for NodeRef {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for NodeRef {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        NodeRef::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const ID: &str = "goal-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b";

    fn id(s: &str) -> NodeId {
        NodeId::parse(s).unwrap()
    }

    #[test]
    fn parses_and_displays() {
        let n = id(ID);
        assert_eq!(n.node_type(), "goal");
        assert_eq!(n.to_string(), ID);
        assert_eq!(n.uuid().unix_ms(), Some(0x0192_f3a4_7b2c));
        // Any RFC 9562 version is an id (migrated identity v2 nodes are v4);
        // only a v7 has a time.
        for v in ["1", "4", "8"] {
            let other = ID.replacen("-7def-", &format!("-{v}def-"), 1);
            let n = id(&other);
            assert_eq!(n.uuid().version().to_string(), v);
            assert_eq!(n.uuid().unix_ms(), None);
        }
        for v in ["0", "9", "f"] {
            let other = ID.replacen("-7def-", &format!("-{v}def-"), 1);
            assert_eq!(NodeId::parse(&other), Err(IdError::BadVersion), "{v}");
        }
        assert_eq!(n.uuid().simple(), "0192f3a47b2c7def8a9b1c2d3e4f5a6b");
        assert_eq!(
            id("work_order-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b").node_type(),
            "work_order"
        );
    }

    #[test]
    fn rejects_bad_ids() {
        let bad = [
            ("task-19", IdError::BadUuid),
            ("0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b", IdError::BadType),
            (
                "-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b",
                IdError::MissingType,
            ),
            ("task", IdError::MissingType),
            (
                "Task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b",
                IdError::BadType,
            ),
            (
                "task-0192F3A4-7b2c-7def-8a9b-1c2d3e4f5a6b",
                IdError::BadUuid,
            ),
            (
                "task-0192f3a4-7b2c-0def-8a9b-1c2d3e4f5a6b",
                IdError::BadVersion,
            ),
            (
                "task-0192f3a4-7b2c-7def-ca9b-1c2d3e4f5a6b",
                IdError::BadVariant,
            ),
            ("goal-0192f3a47b2c7def8a9b1c2d3e4f5a6b", IdError::BadUuid),
            (
                "task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b ",
                IdError::BadUuid,
            ),
            (
                "root:task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b",
                IdError::BadType,
            ),
            (
                "tásk-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b",
                IdError::BadType,
            ),
            (
                "deadbeef-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b",
                IdError::BadType,
            ),
        ];
        for (input, err) in bad {
            assert_eq!(NodeId::parse(input), Err(err), "{input}");
        }
    }

    #[test]
    fn matches_the_legacy_layout() {
        let random = [0x0C, 0x3D, 0x8E, 0x4F, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB];
        let u = Uuid7::from_random(0x0192_F4C2_1B2A, random).unwrap();
        assert_eq!(
            u.to_string(),
            crate::uuid7::uuid_v7(0x0192_F4C2_1B2A, random)
        );
        assert_eq!(
            Uuid7::from_random(MAX_UNIX_MS + 1, random),
            Err(IdError::TimeOutOfRange)
        );
    }

    #[test]
    fn retype_keeps_the_node() {
        let task = id(ID).with_type("task").unwrap();
        assert!(task.same_node(&id(ID)));
        assert_ne!(task, id(ID));
        assert_eq!(id(ID).with_type("Bad"), Err(IdError::BadType));
        assert_eq!(id(ID).with_type("deadbeef"), Err(IdError::BadType));
        // Hex type names of other lengths are fine.
        assert!(id(ID).with_type("deadbee").is_ok());
        assert!(id(ID).with_type("deadbeefa").is_ok());
    }

    #[test]
    fn refs() {
        let graph = "0192f3a0-0000-7000-8000-000000000000";
        let pin = "a".repeat(64);
        for s in [
            ID.to_string(),
            format!("mdkg://{graph}/{ID}"),
            format!("{ID}@sha256:{pin}"),
            format!("mdkg://{graph}/{ID}@sha256:{pin}"),
        ] {
            assert_eq!(NodeRef::parse(&s).unwrap().to_string(), s);
        }
        let r = NodeRef::parse(&format!("mdkg://{graph}/{ID}@sha256:{pin}")).unwrap();
        assert_eq!(r.graph.unwrap().to_string(), graph);
        assert_eq!(r.pin.as_deref(), Some(pin.as_str()));
        assert_eq!(
            NodeRef::parse(&format!("{ID}@sha1:{pin}")),
            Err(IdError::BadPin)
        );
        assert_eq!(
            NodeRef::parse(&format!("{ID}@sha256:{}", "A".repeat(64))),
            Err(IdError::BadPin)
        );
        assert_eq!(
            NodeRef::parse(&format!("mdkg://{graph}")),
            Err(IdError::BadUri)
        );
        // The 0.6.0 identity ref has a bare UUID where 0.7 has an id.
        assert_eq!(
            NodeRef::parse(&format!("mdkg://{graph}/{graph}")),
            Err(IdError::BadUri)
        );
        assert_eq!(NodeRef::parse("root:task-19"), Err(IdError::BadType));
    }

    #[test]
    fn prefixes() {
        let a = id("task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b");
        let b = id("task-0192f3a4-7b2d-7def-8a9b-1c2d3e4f5a6b");
        let c = id("goal-0193aaaa-0000-7000-8000-000000000000");
        let all = [a.clone(), b.clone(), c.clone()];
        let p = |s: &str| IdPrefix::parse(s).unwrap();
        assert_eq!(
            match_prefix(&p("0192"), &all),
            PrefixMatch::Ambiguous(vec![&a, &b])
        );
        assert_eq!(
            match_prefix(&p("0192f3a4-7b2c"), &all),
            PrefixMatch::Unique(&a)
        );
        assert_eq!(
            match_prefix(&p("0192F3A47B2D"), &all),
            PrefixMatch::Unique(&b)
        );
        assert_eq!(match_prefix(&p("goal-0193"), &all), PrefixMatch::Unique(&c));
        assert_eq!(match_prefix(&p("task-0193"), &all), PrefixMatch::None);
        assert_eq!(
            match_prefix(&p(&a.to_string()), &all),
            PrefixMatch::Unique(&a)
        );
        assert_eq!(match_prefix(&p("0192"), [&a, &a]), PrefixMatch::Unique(&a));
        // `dec` is hex but 3 digits long, so it's the type.
        assert_eq!(p("dec-0192").node_type(), Some("dec"));
        assert_eq!(p("0192f3a4-7b").node_type(), None);
        assert_eq!(p("deadbee-0192").node_type(), Some("deadbee"));
        assert_eq!(p("deadbeefa-0192").node_type(), Some("deadbeefa"));
        for bad in [
            "019",
            "task-19",
            "0192f3a47-b2c",
            "fix",
            "0192 f3",
            "",
            "task-",
            "0192f3a4-7b2c7def",
        ] {
            assert_eq!(IdPrefix::parse(bad), None, "{bad}");
        }
        assert_eq!(shortest_unique_hex(&a, &all), 12);
        assert_eq!(shortest_unique_hex(&c, &all), 4);
    }

    #[test]
    fn serde_round_trips() {
        let n = id(ID);
        let json = serde_json::to_string(&n).unwrap();
        assert_eq!(json, format!("\"{ID}\""));
        assert_eq!(serde_json::from_str::<NodeId>(&json).unwrap(), n);
        assert!(serde_json::from_str::<NodeId>("\"task-19\"").is_err());
    }

    #[cfg(feature = "mint")]
    #[test]
    fn minted_ids_increase() {
        let ids: Vec<NodeId> = (0..20_000).map(|_| NodeId::mint("task").unwrap()).collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        assert!(
            ids.iter()
                .all(|i| NodeId::parse(&i.to_string()).as_ref() == Ok(i))
        );
        assert_eq!(NodeId::mint("Task"), Err(IdError::BadType));
    }

    #[test]
    fn minter_carries_a_full_counter() {
        let mut m = Minter::new();
        let mut last = m.next(1000, [0xFF; 10]).unwrap();
        for _ in 0..5000 {
            let next = m.next(1000, [0xFF; 10]).unwrap();
            assert!(next > last);
            last = next;
        }
        assert!(last.unix_ms() > Some(1000));
        let mut m = Minter::new();
        m.next(MAX_UNIX_MS, [0; 10]).unwrap();
        for _ in 0..0x0FFF {
            m.next(MAX_UNIX_MS, [0; 10]).unwrap();
        }
        assert_eq!(m.next(MAX_UNIX_MS, [0; 10]), Err(IdError::TimeOutOfRange));
    }

    proptest! {
        #[test]
        fn uuid_round_trips(ms in 0..=MAX_UNIX_MS, random in any::<[u8; 10]>()) {
            let u = Uuid7::from_random(ms, random).unwrap();
            prop_assert_eq!(u.unix_ms(), Some(ms));
            prop_assert_eq!(Uuid7::parse(&u.to_string()), Ok(u));
            let n = NodeId::new("task", u).unwrap();
            prop_assert_eq!(NodeId::parse(&n.to_string()), Ok(n.clone()));
            for len in IdPrefix::MIN_HEX..=32 {
                let p = IdPrefix::parse(&u.simple()[..len]).unwrap();
                prop_assert!(p.matches(&n));
            }
            let full = n.to_string();
            for cut in 9..full.len() {
                if let Some(p) = IdPrefix::parse(&full[..cut]) {
                    prop_assert!(p.matches(&n), "{}", &full[..cut]);
                }
            }
        }

        #[test]
        fn typed_prefixes_match(t in "[a-z][a-z0-9_]{0,31}", ms in 0..=MAX_UNIX_MS, random in any::<[u8; 10]>()) {
            let u = Uuid7::from_random(ms, random).unwrap();
            if let Ok(n) = NodeId::new(&t, u) {
                let full = n.to_string();
                let p = IdPrefix::parse(&full[..t.len() + 1 + IdPrefix::MIN_HEX]).unwrap();
                prop_assert_eq!(p.node_type(), Some(t.as_str()));
                prop_assert!(p.matches(&n));
            }
        }

        #[test]
        fn parse_never_panics(s in "\\PC{0,48}") {
            let _ = NodeId::parse(&s);
            let _ = NodeRef::parse(&s);
            let _ = IdPrefix::parse(&s);
        }

        #[test]
        fn minter_strictly_increases(steps in proptest::collection::vec((0u64..2000, any::<[u8; 10]>()), 1..300)) {
            let mut m = Minter::new();
            let mut last: Option<Uuid7> = None;
            for (ms, random) in steps {
                let u = m.next(ms, random).unwrap();
                prop_assert!(u.unix_ms() >= Some(ms));
                if let Some(prev) = last {
                    prop_assert!(u > prev);
                }
                last = Some(u);
            }
        }

        #[test]
        fn prefix_match_is_exact(raw in proptest::collection::vec((0u64..50, any::<[u8; 10]>()), 1..40), len in 4usize..12) {
            let ids: Vec<NodeId> = raw.iter().map(|(ms, r)| NodeId::new("task", Uuid7::from_random(*ms, *r).unwrap()).unwrap()).collect();
            let target = &ids[0];
            let p = IdPrefix::parse(&target.uuid().simple()[..len]).unwrap();
            let want: Vec<&NodeId> = ids.iter().filter(|i| i.uuid().simple().starts_with(p.hex())).collect();
            match match_prefix(&p, &ids) {
                PrefixMatch::Unique(u) => prop_assert!(want.iter().all(|w| *w == u)),
                PrefixMatch::Ambiguous(all) => prop_assert!(all.len() > 1 && all.iter().all(|a| want.contains(a))),
                PrefixMatch::None => prop_assert!(false, "the target always matches"),
            }
        }
    }
}
