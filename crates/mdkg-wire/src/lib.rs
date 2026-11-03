//! serve's wire (L1 of the serve design, edd-01a18803): what crosses the
//! network between machines and a coordinator, and nothing that decides
//! state.
//!
//! - [`sign`]: the client half of RFC 9421's signing profile (S5), the
//!   machine signature and the person signature over it (S41);
//! - [`keys`]: machine and TLS key files, fingerprints, and Ed25519
//!   verification;
//! - [`person`]: person-signature verification by key kind;
//! - [`codec`]: HTTP/1.1 framing and SSE frames behind the `Codec`,
//!   `HttpConnection` and `Io` traits `serve`'s listener is generic over;
//! - [`client_conn`]: one connection's client side: signed requests with
//!   JSON bodies, strict responses, and streams as an `EventStream`;
//! - [`sse`]: reading an SSE stream, bounded;
//! - [`digest`]: RFC 9530's `Content-Digest`;
//! - [`tls`]: TLS 1.3 with the host key pinned, the `TlsAcceptor` trait
//!   and the client connector;
//! - [`pair`]: the commitment-based pairing code (S6, S41);
//! - [`push`]: the push runner's private side: one checked private
//!   repository per push, and the destination's ssh settings (S38).

pub mod b64;
pub mod client_conn;
pub mod codec;
pub mod digest;
pub mod keys;
pub mod pair;
pub mod person;
pub mod push;
pub mod sign;
pub mod sse;
pub mod tls;

#[cfg(test)]
mod testutil;
