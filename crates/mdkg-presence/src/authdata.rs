//! FIDO2 authenticator data (WebAuthn section 6.1), as `makeCredential`
//! returns it at enrolment: the RP id hash, the flags, the signature
//! counter, and the attested credential with its COSE public key.
//!
//! Enrolment asks for no extensions, so data with the extension flag set is
//! refused, as is anything left over after the credential.

use std::fmt;

use crate::cose::{self, CoseError, PublicKey};

pub const FLAG_UP: u8 = 0x01;
pub const FLAG_UV: u8 = 0x04;
/// Backup eligibility: the credential may be copied to other
/// authenticators (a synced passkey).
pub const FLAG_BE: u8 = 0x08;
/// Backup state: it has been.
pub const FLAG_BS: u8 = 0x10;
pub const FLAG_AT: u8 = 0x40;
pub const FLAG_ED: u8 = 0x80;

/// The longest credential id accepted (CTAP2 allows up to 1023 bytes).
pub const MAX_CREDENTIAL_ID: usize = 1023;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthData {
    pub rp_id_hash: [u8; 32],
    pub flags: u8,
    pub sign_count: u32,
    pub aaguid: [u8; 16],
    pub credential_id: Vec<u8>,
    pub public_key: PublicKey,
}

impl AuthData {
    pub fn user_present(&self) -> bool {
        self.flags & FLAG_UP != 0
    }

    pub fn user_verified(&self) -> bool {
        self.flags & FLAG_UV != 0
    }

    /// Whether the credential may live on other authenticators too.
    pub fn backed_up_or_eligible(&self) -> bool {
        self.flags & (FLAG_BE | FLAG_BS) != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthDataError {
    Truncated,
    /// No attested credential data (the AT flag is clear).
    NoCredential,
    /// Extension data, which enrolment never asks for.
    Extensions,
    CredentialIdTooLong,
    /// A credential id of no bytes, which no assertion could name.
    EmptyCredentialId,
    Key(CoseError),
    /// Bytes after the credential's public key.
    TrailingBytes,
}

impl fmt::Display for AuthDataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthDataError::Truncated => f.write_str("the authenticator data is truncated"),
            AuthDataError::NoCredential => {
                f.write_str("the authenticator data has no attested credential")
            }
            AuthDataError::Extensions => f.write_str(
                "the authenticator data carries extensions, which enrolment doesn't ask for",
            ),
            AuthDataError::CredentialIdTooLong => f.write_str("the credential id is too long"),
            AuthDataError::EmptyCredentialId => f.write_str("the credential id is empty"),
            AuthDataError::Key(e) => e.fmt(f),
            AuthDataError::TrailingBytes => {
                f.write_str("the authenticator data has bytes after the credential")
            }
        }
    }
}

impl std::error::Error for AuthDataError {}

/// Parses the authenticator data of a `makeCredential` response.
pub fn parse_attested(buf: &[u8]) -> Result<AuthData, AuthDataError> {
    let fixed = buf.get(..37).ok_or(AuthDataError::Truncated)?;
    let rp_id_hash: [u8; 32] = fixed[..32]
        .try_into()
        .map_err(|_| AuthDataError::Truncated)?;
    let flags = fixed[32];
    let sign_count = u32::from_be_bytes(
        fixed[33..37]
            .try_into()
            .map_err(|_| AuthDataError::Truncated)?,
    );
    if flags & FLAG_ED != 0 {
        return Err(AuthDataError::Extensions);
    }
    if flags & FLAG_AT == 0 {
        return Err(AuthDataError::NoCredential);
    }
    let rest = &buf[37..];
    let aaguid: [u8; 16] = rest
        .get(..16)
        .ok_or(AuthDataError::Truncated)?
        .try_into()
        .map_err(|_| AuthDataError::Truncated)?;
    let len_bytes = rest.get(16..18).ok_or(AuthDataError::Truncated)?;
    let id_len = usize::from(u16::from_be_bytes([len_bytes[0], len_bytes[1]]));
    if id_len > MAX_CREDENTIAL_ID {
        return Err(AuthDataError::CredentialIdTooLong);
    }
    if id_len == 0 {
        return Err(AuthDataError::EmptyCredentialId);
    }
    let credential_id = rest
        .get(18..18 + id_len)
        .ok_or(AuthDataError::Truncated)?
        .to_vec();
    let key_bytes = &rest[18 + id_len..];
    let (public_key, used) = cose::decode_key(key_bytes).map_err(AuthDataError::Key)?;
    if used != key_bytes.len() {
        return Err(AuthDataError::TrailingBytes);
    }
    Ok(AuthData {
        rp_id_hash,
        flags,
        sign_count,
        aaguid,
        credential_id,
        public_key,
    })
}

/// Builds attested authenticator data. For fakes and tests.
pub fn build_attested(
    rp_id_hash: [u8; 32],
    flags: u8,
    sign_count: u32,
    aaguid: [u8; 16],
    credential_id: &[u8],
    key: &PublicKey,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);
    out.extend_from_slice(&rp_id_hash);
    out.push(flags | FLAG_AT);
    out.extend_from_slice(&sign_count.to_be_bytes());
    out.extend_from_slice(&aaguid);
    let id_len = u16::try_from(credential_id.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&id_len.to_be_bytes());
    out.extend_from_slice(credential_id);
    out.extend_from_slice(&cose::encode_key(key));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> PublicKey {
        PublicKey::Ed25519([5; 32])
    }

    #[test]
    fn parses_what_it_builds() {
        let buf = build_attested([1; 32], FLAG_UP | FLAG_UV, 9, [2; 16], b"cred", &key());
        let a = parse_attested(&buf).expect("parses");
        assert_eq!(a.rp_id_hash, [1; 32]);
        assert!(a.user_present() && a.user_verified());
        assert_eq!(a.sign_count, 9);
        assert_eq!(a.aaguid, [2; 16]);
        assert_eq!(a.credential_id, b"cred");
        assert_eq!(a.public_key, key());
    }

    #[test]
    fn refuses_missing_credential_extensions_and_trailing_bytes() {
        let mut buf = build_attested([1; 32], FLAG_UP, 0, [0; 16], b"c", &key());
        buf[32] &= !FLAG_AT;
        assert_eq!(parse_attested(&buf), Err(AuthDataError::NoCredential));
        buf[32] |= FLAG_AT | FLAG_ED;
        assert_eq!(parse_attested(&buf), Err(AuthDataError::Extensions));
        let mut buf = build_attested([1; 32], FLAG_UP, 0, [0; 16], b"c", &key());
        buf.push(0);
        assert_eq!(parse_attested(&buf), Err(AuthDataError::TrailingBytes));
    }

    #[test]
    fn refuses_truncation_anywhere() {
        let buf = build_attested([1; 32], FLAG_UP, 0, [0; 16], b"cred", &key());
        for n in 0..buf.len() {
            assert!(parse_attested(&buf[..n]).is_err(), "prefix of {n} bytes");
        }
    }

    #[test]
    fn refuses_an_empty_credential_id() {
        let buf = build_attested([1; 32], FLAG_UP, 0, [0; 16], b"", &key());
        assert_eq!(parse_attested(&buf), Err(AuthDataError::EmptyCredentialId));
    }

    #[test]
    fn refuses_an_overlong_credential_id() {
        let mut buf = build_attested([1; 32], FLAG_UP, 0, [0; 16], b"c", &key());
        buf[53..55].copy_from_slice(&1024u16.to_be_bytes());
        assert_eq!(
            parse_attested(&buf),
            Err(AuthDataError::CredentialIdTooLong)
        );
    }
}
