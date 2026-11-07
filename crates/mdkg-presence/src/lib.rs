//! Person keys that need their person present (the serve edd, S41): a P-256
//! key in the Secure Enclave, a FIDO2 credential, or the labelled file-key
//! opt-in; their enrolment evidence, its attestation, and the grade that
//! follows.

pub mod attest;
pub mod authdata;
pub mod cose;
pub mod device;
pub mod enclave;
pub mod enrol;
pub mod fido2;
pub mod file_key;
pub mod grade;
pub mod lock;
pub mod sig;
pub mod x509;

/// Whether this build has a FIDO2 backend: [`fido2::BACKEND`], set by
/// the `hid` module `fido2::platform()` compiled with (the `fido2` feature
/// as Cargo resolved it for this crate, on macOS or glibc Linux).
pub const FIDO2: bool = fido2::BACKEND;

#[cfg(test)]
mod cross_lane;
#[cfg(test)]
mod testutil;
