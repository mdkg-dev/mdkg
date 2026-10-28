//! Minting the UUIDv7 identities migrated nodes get.
//!
//! The identifiers are derived, not drawn at random, so that:
//! - `--dry-run` and `--apply <hash>` compute the same plan;
//! - each unmerged branch of a repository migrates the nodes it shares with
//!   main to the same ids, so those branches still merge;
//! - a parent graph can compute a child graph's new ids for its subgraph
//!   refs by planning the child, read only.
//!
//! They are still RFC 9562 version 7 UUIDs. The time field is the node's
//! `created` date at 00:00 UTC plus its legacy number in milliseconds, so
//! nodes sort by creation date and, within a day, in their old numbering
//! order. The 74 bits RFC 9562 leaves to the generator come from sha256
//! of the graph id, the legacy id and the node's file path inside the graph
//! folder, so no two nodes of one graph, and no two graphs, share an id:
//! two branches that each created the same numbered task id (0.6.0's
//! cross-branch collisions) get two ids. Nodes created after the
//! migration get random bits as usual.

use mdkg_format::Digest;
use mdkg_format::uuid7::uuid_v7;

/// Milliseconds in a day: a legacy number this large or larger can't stay
/// inside its creation day.
const DAY_MS: u64 = 86_400_000;

/// The first 10 bytes of sha256 over `parts`, each length-prefixed so no
/// two different part lists hash alike.
fn derived_bits(domain: &str, parts: &[&str]) -> [u8; 10] {
    let mut input = Vec::new();
    for part in std::iter::once(domain).chain(parts.iter().copied()) {
        input.extend_from_slice(&(part.len() as u64).to_be_bytes());
        input.extend_from_slice(part.as_bytes());
    }
    let hex = Digest::of_bytes(&input).hex().to_string();
    let mut out = [0u8; 10];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or_default();
    }
    out
}

/// The graph id a graph gets when it has none: its time is the
/// repository's root commit's, and its bits come from that commit and the
/// graph's folder inside the repository.
pub fn graph_id(root_commit: &str, root_commit_ms: u64, graph_path: &str) -> String {
    uuid_v7(
        root_commit_ms,
        derived_bits("mdkg-graph/v1", &[root_commit, graph_path]),
    )
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic Gregorian date of a day count since 1970-01-01 (Howard
/// Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Unix milliseconds as RFC 3339 UTC to the millisecond, the form 0.7.0
/// writes `created` and `updated` in.
pub fn rfc3339_ms(ms: u64) -> String {
    let days = i64::try_from(ms / DAY_MS).unwrap_or(0);
    let (y, m, d) = civil_from_days(days);
    let rem = ms % DAY_MS;
    let (h, rem) = (rem / 3_600_000, rem % 3_600_000);
    let (min, rem) = (rem / 60_000, rem % 60_000);
    let (s, milli) = (rem / 1000, rem % 1000);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}.{milli:03}Z")
}

/// The Unix milliseconds of a `YYYY-MM-DD` date's start, UTC.
pub fn date_ms(date: &str) -> Option<u64> {
    let b = date.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |s: &str| s.parse::<u32>().ok();
    let y = num(&date[..4])?;
    let m = num(&date[5..7])?;
    let d = num(&date[8..])?;
    let days_in = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => return None,
    };
    if d == 0 || d > days_in {
        return None;
    }
    let days = days_from_civil(i64::from(y), m, d);
    u64::try_from(days).ok().map(|days| days * DAY_MS)
}

/// The number after the last `-`, when it's all decimal digits; 0 when it
/// isn't a number at all; `None` when it's digits too large for `u64`
/// (refused, never read as 0: spec section 10.1).
fn legacy_part(legacy_id: &str) -> Option<u64> {
    match legacy_id.rsplit_once('-') {
        Some((_, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => n.parse().ok(),
        _ => Some(0),
    }
}

/// Why a node couldn't get an id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MintError {
    /// `created` isn't a `YYYY-MM-DD` date (or is before 1970).
    BadCreated,
    /// The legacy number doesn't fit inside one day's milliseconds.
    NumberTooLarge,
}

/// The UUIDv7 of a migrated node; `path` is its file's path inside the
/// graph folder (`work/<legacy-id>-<slug>.md`).
pub fn node_uuid(
    graph_id: &str,
    legacy_id: &str,
    created: &str,
    path: &str,
) -> Result<String, MintError> {
    // A full timestamp (`2026-10-24T…`) counts from its date.
    let day = created.get(..10).unwrap_or(created);
    let start = date_ms(day).ok_or(MintError::BadCreated)?;
    let number = legacy_part(legacy_id).ok_or(MintError::NumberTooLarge)?;
    if number >= DAY_MS {
        return Err(MintError::NumberTooLarge);
    }
    Ok(uuid_v7(
        start + number,
        derived_bits("mdkg-migrate/v1", &[graph_id, legacy_id, path]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdkg_format::ids::is_identity_uuid;
    use mdkg_format::uuid7::uuid_v7_timestamp;

    const G: &str = "0192f4c2-1b2a-7c3d-8e4f-0123456789ab";

    #[test]
    fn dates() {
        assert_eq!(date_ms("1970-01-01"), Some(0));
        assert_eq!(date_ms("2026-10-24"), Some(1_792_800_000_000));
        assert_eq!(date_ms("2024-02-29"), Some(1_709_164_800_000));
        assert_eq!(date_ms("2025-02-29"), None);
        assert_eq!(date_ms("2026-13-01"), None);
        assert_eq!(date_ms("2026-1-01"), None);
        assert_eq!(date_ms("nonsense!!"), None);
    }

    #[test]
    fn rfc3339() {
        assert_eq!(rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_ms(1_792_800_000_019), "2026-10-24T00:00:00.019Z");
        assert_eq!(
            rfc3339_ms(1_709_164_800_000 + 86_399_999),
            "2024-02-29T23:59:59.999Z"
        );
        for day in ["1999-12-31", "2000-02-29", "2026-03-01", "2100-02-28"] {
            assert_eq!(&rfc3339_ms(date_ms(day).unwrap())[..10], day);
        }
    }

    #[test]
    fn deterministic_valid_and_ordered() {
        let a = node_uuid(G, "task-2", "2026-10-24", "work/x.md").unwrap();
        assert_eq!(
            a,
            node_uuid(G, "task-2", "2026-10-24", "work/x.md").unwrap()
        );
        assert!(is_identity_uuid(&a), "{a}");
        assert_eq!(uuid_v7_timestamp(&a), Some(1_792_800_000_002));
        // Within a day, the old numbering order.
        let b = node_uuid(G, "task-10", "2026-10-24", "work/x.md").unwrap();
        assert!(a < b);
        // Across days, creation order, whatever the numbers.
        let c = node_uuid(G, "task-1", "2026-10-25", "work/x.md").unwrap();
        assert!(b < c);
        // Another node, another file or another graph gets other bits.
        assert_ne!(
            node_uuid(G, "task-2", "2026-10-24", "work/y.md").unwrap(),
            a
        );
        assert_ne!(
            node_uuid(G, "chk-2", "2026-10-24", "work/x.md").unwrap()[14..],
            a[14..]
        );
        let other = graph_id("abc", 0, "");
        assert_ne!(
            node_uuid(&other, "task-2", "2026-10-24", "work/x.md").unwrap(),
            a
        );
        // A timestamp counts from its day.
        assert_eq!(
            node_uuid(G, "task-2", "2026-10-24T10:00:00.000Z", "work/x.md").unwrap(),
            a
        );
    }

    #[test]
    fn refusals() {
        assert_eq!(
            node_uuid(G, "task-1", "soon", "work/x.md"),
            Err(MintError::BadCreated)
        );
        assert_eq!(
            node_uuid(G, "task-86400000", "2026-10-24", "work/x.md"),
            Err(MintError::NumberTooLarge)
        );
        assert_eq!(legacy_part("rule-guide"), Some(0));
        assert_eq!(legacy_part("task-+19"), Some(0));
        assert_eq!(legacy_part("task-19"), Some(19));
        assert_eq!(
            node_uuid(G, "task-99999999999999999999", "2026-10-24", "work/x.md"),
            Err(MintError::NumberTooLarge)
        );
    }

    #[test]
    fn the_spec_test_vectors() {
        // docs/format-0.7.md section 10.1.
        let root = "0123456789abcdef0123456789abcdef01234567";
        let g = graph_id(root, 1_760_745_600_000, "");
        assert_eq!(g, "0199f49d-b400-7f84-b960-fb50082705ab");
        assert_eq!(
            graph_id(root, 1_760_745_600_000, "projects/x"),
            "0199f49d-b400-7c37-a863-be5af3275130"
        );
        assert_eq!(
            node_uuid(&g, "task-19", "2026-10-24", "work/task-19-x.md").unwrap(),
            "01a16b35-0813-739b-92f7-2747dfa64e10"
        );
        assert_eq!(
            node_uuid(
                &g,
                "task-19",
                "2026-10-24T10:00:00.000Z",
                "work/task-19-x.md"
            )
            .unwrap(),
            "01a16b35-0813-739b-92f7-2747dfa64e10"
        );
        assert_eq!(
            node_uuid(&g, "rule-guide", "2026-10-24", "core/guide.md").unwrap(),
            "01a16b35-0800-7c25-b839-45a8d069d1ba"
        );
        assert_eq!(
            node_uuid(&g, "agent.repo-worker", "2026-01-05", "work/sub/agent.md").unwrap(),
            "019b8b74-1800-7fe6-a2ad-34345fb113a5"
        );
    }

    #[test]
    fn graph_ids() {
        let g = graph_id("0123abcd", 1_760_745_600_000, "");
        assert!(is_identity_uuid(&g));
        assert_eq!(uuid_v7_timestamp(&g), Some(1_760_745_600_000));
        assert_ne!(g, graph_id("0123abcd", 1_760_745_600_000, "projects/x"));
        // Length-prefixed parts: moving a boundary changes the bits.
        assert_ne!(
            derived_bits("d", &["ab", "c"]),
            derived_bits("d", &["a", "bc"])
        );
    }
}
