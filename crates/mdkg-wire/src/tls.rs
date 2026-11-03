//! TLS 1.3 with the host key pinned (S5): the [`TlsAcceptor`] `serve`'s
//! listener uses on TCP, and the matching client connector.
//!
//! - **TLS 1.3 only**, through rustls with the ring provider; TLS 1.2 is
//!   not built in.
//! - **No certificates.** The host presents its Ed25519 TLS key
//!   ([`ServeKey`]) as an RFC 7250 raw public key, and a client accepts the
//!   connection only if that key's fingerprint ([`crate::keys::fingerprint`]
//!   of its 32 bytes) is the one it pinned at pairing (S6), and the
//!   handshake's signature verifies under it. There is no CA, no name and
//!   no expiry to check, so there is nothing else to trust.
//! - **Machines aren't authenticated here**: every request carries its
//!   RFC 9421 signature, which `serve` checks.
//! - **Pairing** (S6): before a key is pinned, [`connect_for_pairing`]
//!   accepts whatever raw key the host presents, checks the handshake is
//!   signed with it, and returns its fingerprint for the pairing code. It
//!   is pinned only once both screens' codes match.
//! - **Time.** A timeout is per operation, as on a plain socket: each TLS
//!   read or write makes its timeout a deadline for every socket read and
//!   write it does, in both directions, so a peer trickling bytes can't
//!   stretch a handshake, a record or a write past it (Claude #9799,
//!   Terra 1 #9853 and #9945).
//! - **Ending.** Only a received close_notify is a clean end; a close
//!   without one reads as `UnexpectedEof` (orchestrator's call on Terra 1
//!   #9945), which the codec takes as a clean close only between
//!   exchanges. [`Io::close_gracefully`] sends close_notify.
//!
//! **Residual:** the seed and PKCS#8 copies made to build the TLS signer
//! are zeroized (approved, #10001); the key pair ring builds from them inside
//! rustls's signer isn't, and lives as long as the acceptor (see
//! `keys`'s residual).

use std::cell::Cell;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{
    CertificateDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer, UnixTime,
};
use rustls::server::AlwaysResolvesServerRawPublicKeys;
use rustls::sign::{CertifiedKey, SigningKey};
use rustls::{
    CertificateError, ClientConfig, ClientConnection, DigitallySignedStruct, ServerConfig,
    ServerConnection, SignatureScheme, StreamOwned,
};
use zeroize::Zeroizing;

use crate::codec::{Closer, Io, SocketCloser};
use crate::keys::{ServeKey, fingerprint};

/// The listener's side: TLS 1.3 handshakes with the host key.
pub trait TlsAcceptor: Send + Sync {
    /// The handshake on an accepted TCP stream, finished within `timeout`
    /// (what is left of the listener's 10 s pre-auth deadline).
    fn accept(&self, tcp: TcpStream, timeout: Duration) -> io::Result<Box<dyn Io>>;
}

/// An Ed25519 public key's DER SubjectPublicKeyInfo, before its 32 bytes.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];
/// An Ed25519 private key's PKCS#8, before its 32-byte seed.
const ED25519_PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn tls_error(e: rustls::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "timed out")
}

/// A TCP stream whose every read and write is bounded by the deadline in
/// force: the time left is set on the socket before each one.
pub(crate) struct Deadlined {
    sock: TcpStream,
    read_by: Cell<Option<Instant>>,
    write_by: Cell<Option<Instant>>,
    /// The timeouts `Io::set_read_timeout` and `set_write_timeout` set:
    /// each TLS read or write turns its own into a deadline for every
    /// socket read and write it makes (orchestrator's call on #9945).
    read_timeout: Cell<Option<Duration>>,
    write_timeout: Cell<Option<Duration>>,
}

impl Deadlined {
    fn new(sock: TcpStream) -> Self {
        Self {
            sock,
            read_by: Cell::new(None),
            write_by: Cell::new(None),
            read_timeout: Cell::new(None),
            write_timeout: Cell::new(None),
        }
    }

    fn left(by: Option<Instant>) -> io::Result<Option<Duration>> {
        match by {
            None => Ok(None),
            Some(t) => {
                let left = t.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    Err(timed_out())
                } else {
                    Ok(Some(left))
                }
            }
        }
    }
}

impl Read for Deadlined {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.sock
            .set_read_timeout(Self::left(self.read_by.get())?)?;
        self.sock.read(buf)
    }
}

impl Write for Deadlined {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.write_by.get() {
            // Without blocking past the deadline, which the socket's send
            // timeout alone doesn't ensure on macOS (Terra 1 #9908).
            Some(deadline) => crate::codec::write_nonblocking(
                &mut self.sock,
                TcpStream::set_nonblocking,
                buf,
                deadline,
            ),
            None => {
                self.sock.set_write_timeout(None)?;
                self.sock.write(buf)
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

/// The server config: `spki` presented as a raw public key, signed for
/// by `signer`.
fn server_config(spki: Vec<u8>, signer: Arc<dyn SigningKey>) -> io::Result<Arc<ServerConfig>> {
    let certified = CertifiedKey::new(vec![CertificateDer::from(spki)], signer);
    let config = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_error)?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(AlwaysResolvesServerRawPublicKeys::new(Arc::new(
            certified,
        ))));
    Ok(Arc::new(config))
}

fn spki_of(public_key: &[u8; 32]) -> Vec<u8> {
    let mut spki = ED25519_SPKI_PREFIX.to_vec();
    spki.extend_from_slice(public_key);
    spki
}

/// The TLS signer for a host key. The PKCS#8 it is built from is borrowed,
/// never copied, and zeroized after (Claude #9799, Terra 1 #9853; the zeroize
/// approval, #10001).
fn signer_of(key: &ServeKey) -> io::Result<Arc<dyn SigningKey>> {
    // Sized for both parts, so it never moves; both zeroized on drop.
    let mut pkcs8 = Zeroizing::new(Vec::with_capacity(ED25519_PKCS8_PREFIX.len() + 32));
    pkcs8.extend_from_slice(&ED25519_PKCS8_PREFIX);
    pkcs8.extend_from_slice(&*key.seed());
    rustls::crypto::ring::sign::any_eddsa_type(&PrivatePkcs8KeyDer::from(&pkcs8[..]))
        .map_err(tls_error)
}

/// The host's acceptor: its TLS key as a raw public key, TLS 1.3 only.
#[derive(Debug, Clone)]
pub struct HostAcceptor {
    config: Arc<ServerConfig>,
}

impl HostAcceptor {
    pub fn new(key: &ServeKey) -> io::Result<Self> {
        Ok(Self {
            config: server_config(spki_of(&key.public_key()), signer_of(key)?)?,
        })
    }
}

impl TlsAcceptor for HostAcceptor {
    fn accept(&self, tcp: TcpStream, timeout: Duration) -> io::Result<Box<dyn Io>> {
        let deadline = Instant::now() + timeout;
        let mut conn = ServerConnection::new(self.config.clone()).map_err(tls_error)?;
        let mut sock = Deadlined::new(tcp);
        handshake(&mut conn, &mut sock, deadline)?;
        Ok(Box::new(TlsIo::Server(StreamOwned::new(conn, sock))))
    }
}

/// Connects to a host over `tcp`, accepting only the TLS key whose
/// fingerprint is `pinned`, within `timeout`.
pub fn connect(tcp: TcpStream, pinned: &str, timeout: Duration) -> io::Result<Box<dyn Io>> {
    let (stream, _) = connect_stream(tcp, Some(pinned), timeout)?;
    Ok(Box::new(TlsIo::Client(stream)))
}

/// Connects to a host whose key isn't pinned yet, for the pairing exchange
/// only (S6): accepts the raw Ed25519 key it presents, checks the
/// handshake is signed with it, and returns its fingerprint, which goes
/// into the pairing code as `serve_fp`. Pin it only once both screens'
/// codes match; until then nothing proves it is the host's.
pub fn connect_for_pairing(tcp: TcpStream, timeout: Duration) -> io::Result<(Box<dyn Io>, String)> {
    let (stream, fp) = connect_stream(tcp, None, timeout)?;
    Ok((Box::new(TlsIo::Client(stream)), fp))
}

fn connect_stream(
    tcp: TcpStream,
    pinned: Option<&str>,
    timeout: Duration,
) -> io::Result<(StreamOwned<ClientConnection, Deadlined>, String)> {
    let deadline = Instant::now() + timeout;
    let provider = provider();
    let verifier = Arc::new(HostKey {
        pinned: pinned.map(str::to_owned),
        algorithms: provider.signature_verification_algorithms,
    });
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_error)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    // The name is never checked against a raw key: don't send it.
    config.enable_sni = false;
    let name = ServerName::try_from("mdkg").map_err(|e| io::Error::other(e.to_string()))?;
    let mut conn = ClientConnection::new(Arc::new(config), name).map_err(tls_error)?;
    let mut sock = Deadlined::new(tcp);
    handshake(&mut conn, &mut sock, deadline)?;
    let presented = conn
        .peer_certificates()
        .and_then(|c| c.first())
        .and_then(|c| ed25519_key(c.as_ref()))
        .map(fingerprint)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no host key presented"))?;
    Ok((StreamOwned::new(conn, sock), presented))
}

/// Drives a handshake to its end before `deadline`, every socket read and
/// write bounded by it, then lifts the deadline for the caller's own.
fn handshake<S: rustls::SideData>(
    conn: &mut rustls::ConnectionCommon<S>,
    sock: &mut Deadlined,
    deadline: Instant,
) -> io::Result<()> {
    sock.read_by.set(Some(deadline));
    sock.write_by.set(Some(deadline));
    while conn.is_handshaking() || conn.wants_write() {
        match conn.complete_io(sock) {
            Ok((0, 0)) if conn.is_handshaking() => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "closed during the TLS handshake",
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Err(timed_out()),
            Err(e) => return Err(e),
        }
    }
    sock.read_by.set(None);
    sock.write_by.set(None);
    sock.sock.set_read_timeout(None)?;
    sock.sock.set_write_timeout(None)
}

/// A client's check of the host: the raw Ed25519 key it presents, the
/// pinned one if there is a pin, and the handshake signed with it.
#[derive(Debug)]
struct HostKey {
    pinned: Option<String>,
    algorithms: WebPkiSupportedAlgorithms,
}

/// The 32-byte Ed25519 key in a raw public key's SubjectPublicKeyInfo.
fn ed25519_key(spki: &[u8]) -> Option<&[u8]> {
    spki.strip_prefix(&ED25519_SPKI_PREFIX[..])
        .filter(|k| k.len() == 32)
}

impl ServerCertVerifier for HostKey {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let ok = intermediates.is_empty()
            && ed25519_key(end_entity.as_ref()).is_some_and(|k| match &self.pinned {
                Some(pin) => fingerprint(k) == *pin,
                None => true,
            });
        if ok {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature_with_raw_key(
            message,
            &SubjectPublicKeyInfoDer::from(cert.as_ref()),
            dss,
            &self.algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// A TLS stream over TCP, as the codec's [`Io`].
enum TlsIo {
    Server(StreamOwned<ServerConnection, Deadlined>),
    Client(StreamOwned<ClientConnection, Deadlined>),
}

impl TlsIo {
    fn sock(&self) -> &Deadlined {
        match self {
            TlsIo::Server(s) => &s.sock,
            TlsIo::Client(s) => &s.sock,
        }
    }
}

/// A zero timeout is refused, as std's sockets refuse it, so a caller gets
/// the same answer on every transport (Terra 3 #10083).
fn nonzero(t: Option<Duration>) -> io::Result<Option<Duration>> {
    if t == Some(Duration::ZERO) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot set a 0 duration timeout",
        ));
    }
    Ok(t)
}

/// How long a graceful close may take to send its close_notify.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

impl TlsIo {
    /// Runs one TLS operation with `timeout` as the deadline of every
    /// socket read and write it makes, in both directions (a TLS write may
    /// need a read, and a read a write), then lifts it (Terra 1 #9945).
    fn within<R>(&mut self, timeout: Option<Duration>, op: impl FnOnce(&mut Self) -> R) -> R {
        self.within_by(timeout.map(|t| Instant::now() + t), op)
    }

    /// [`within`](Self::within) to an absolute deadline, taken as given,
    /// so a thread descheduled before the operation can't extend it
    /// (Terra 3 #10182).
    fn within_by<R>(&mut self, deadline: Option<Instant>, op: impl FnOnce(&mut Self) -> R) -> R {
        self.sock().read_by.set(deadline);
        self.sock().write_by.set(deadline);
        let r = op(self);
        self.sock().read_by.set(None);
        self.sock().write_by.set(None);
        r
    }
}

impl Read for TlsIo {
    /// A close without close_notify is `UnexpectedEof`, never a clean end
    /// (orchestrator's call on #9945): only a received close_notify
    /// reads as `Ok(0)`.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let t = self.sock().read_timeout.get();
        self.within(t, |io| match io {
            TlsIo::Server(s) => s.read(buf),
            TlsIo::Client(s) => s.read(buf),
        })
    }
}

impl Write for TlsIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let t = self.sock().write_timeout.get();
        self.within(t, |io| match io {
            TlsIo::Server(s) => s.write(buf),
            TlsIo::Client(s) => s.write(buf),
        })
    }
    fn flush(&mut self) -> io::Result<()> {
        let t = self.sock().write_timeout.get();
        self.within(t, |io| match io {
            TlsIo::Server(s) => s.flush(),
            TlsIo::Client(s) => s.flush(),
        })
    }
}

impl Io for TlsIo {
    /// Per operation, as on a plain socket: each read gets `t`, which
    /// bounds every socket read and write it makes.
    fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.sock().read_timeout.set(nonzero(t)?);
        Ok(())
    }
    fn set_write_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.sock().write_timeout.set(nonzero(t)?);
        Ok(())
    }
    fn closer(&self) -> io::Result<Box<dyn Closer>> {
        Ok(Box::new(SocketCloser(self.sock().sock.try_clone()?)))
    }
    /// Bounded by `deadline` itself through every socket operation, which
    /// write without blocking past it. The records are flushed under the
    /// same deadline, so the codec's flush after it has nothing left to
    /// send under a deadline of its own (Terra 3 #10182).
    fn write_by(&mut self, buf: &[u8], deadline: Instant) -> io::Result<usize> {
        if deadline <= Instant::now() {
            return Err(timed_out());
        }
        self.within_by(Some(deadline), |io| match io {
            TlsIo::Server(s) => s.write(buf).and_then(|n| s.flush().map(|()| n)),
            TlsIo::Client(s) => s.write(buf).and_then(|n| s.flush().map(|()| n)),
        })
    }
    /// Sends close_notify within its own deadline, then half-closes the
    /// socket so the peer sees the end (Terra 1 #9945).
    fn close_gracefully(&mut self) -> io::Result<()> {
        let sent = self.within(Some(CLOSE_TIMEOUT), |io| match io {
            TlsIo::Server(s) => {
                s.conn.send_close_notify();
                s.flush()
            }
            TlsIo::Client(s) => {
                s.conn.send_close_notify();
                s.flush()
            }
        });
        let shut = self.sock().sock.shutdown(std::net::Shutdown::Write);
        sent.and(shut)
    }
}

#[cfg(test)]
mod tests;
