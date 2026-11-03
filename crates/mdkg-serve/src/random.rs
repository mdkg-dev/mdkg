//! Random bytes for boot ids, server nonces and enrolment's `Ns`, from the
//! OS's generator. No dependency: `/dev/urandom` on every Unix mdkg ships
//! for.

use std::io::{self, Read};

/// `N` bytes from the OS's generator.
pub fn bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut out = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut out)?;
    Ok(out)
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_differ() {
        let a = bytes::<16>().expect("urandom");
        let b = bytes::<16>().expect("urandom");
        assert_ne!(a, b);
        assert_eq!(hex(&[0, 0xab, 0x10]), "00ab10");
    }
}
