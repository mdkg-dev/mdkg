//! UUIDv7 (RFC 9562): time-ordered identifiers for new nodes.
//!
//! Generation is sans-IO: the caller supplies the Unix time in milliseconds
//! and 10 random bytes, so tests and simulations are deterministic.

/// Builds a lowercase UUIDv7 string.
///
/// Layout: 48-bit millisecond timestamp, 4-bit version (7), 12 random bits,
/// 2-bit variant (`10`), 62 random bits.
pub fn uuid_v7(unix_ms: u64, random: [u8; 10]) -> String {
    let mut bytes = [0u8; 16];
    let ts = unix_ms & 0xFFFF_FFFF_FFFF;
    bytes[..6].copy_from_slice(&ts.to_be_bytes()[2..]);
    bytes[6..].copy_from_slice(&random);
    bytes[6] = (bytes[6] & 0x0F) | 0x70;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// The millisecond timestamp encoded in a UUIDv7 string.
pub fn uuid_v7_timestamp(uuid: &str) -> Option<u64> {
    let hex: String = uuid.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || hex.as_bytes()[12] != b'7' {
        return None;
    }
    u64::from_str_radix(&hex[..12], 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::is_identity_uuid;

    #[test]
    fn layout_and_ordering() {
        let a = uuid_v7(1_760_745_600_000, [0xFF; 10]);
        let b = uuid_v7(1_760_745_600_001, [0x00; 10]);
        assert!(is_identity_uuid(&a), "{a}");
        assert!(is_identity_uuid(&b), "{b}");
        assert_eq!(&a[14..15], "7");
        assert!(
            a < b,
            "later timestamps sort later regardless of randomness"
        );
        assert_eq!(uuid_v7_timestamp(&a), Some(1_760_745_600_000));
    }

    #[test]
    fn fixed_vector() {
        assert_eq!(
            uuid_v7(
                0x0192_F4C2_1B2A,
                [0x0C, 0x3D, 0x8E, 0x4F, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB]
            ),
            "0192f4c2-1b2a-7c3d-8e4f-0123456789ab"
        );
    }
}
