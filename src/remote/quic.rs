//! The roaming transport (spec ssh-remote §4): the proxy protocol over one
//! QUIC stream instead of ssh's stdio, so a changed client address (Wi-Fi
//! to tether, sleep and wake) keeps the session.
//!
//! `grofe proxy --quic` runs on the remote, started over ssh: it binds a UDP
//! port, makes a fresh self-signed certificate and a 32-byte random token,
//! prints one bootstrap line `{"port":N,"cert_sha256":"<hex>","token":"<hex>"}`
//! and detaches, so ssh exits. It then serves one client at a time: the
//! client's first bidirectional stream must start with the token, and the
//! rest of that stream is the proxy protocol. The client pins the
//! certificate by its SHA-256 (no CA) and sends the token only once the TLS
//! handshake has proved the server holds that certificate's key.
//!
//! QUIC security notes. The bootstrap line is the only secret, and it
//! travels over the ssh session that started the proxy, so only someone
//! who can run commands as this user on the remote has it. The token is
//! good for the life of that proxy: until it exits after
//! `remote-quic-idle` with no client, or until a newer `grofe proxy --quic`
//! for the same instance stops it (one proxy per instance). A peer without
//! the line can learn that a UDP port answers QUIC and can start
//! handshakes, which are bounded (a retry to validate its address, a
//! capped number of unauthenticated connections, each with a small receive
//! window and [`server`]'s token deadline), and it is closed at a wrong
//! token without taking the slot. It cannot reach the proxy protocol, the
//! server socket or any file. A peer that is not the pinned certificate's
//! holder cannot pose as the proxy: the client checks the hash and the
//! handshake signature before the token leaves it.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::QuicClientConfig;
use quinn::{
    ConnectionError, Endpoint, EndpointConfig, ReadError, RecvStream, SendStream, TokioRuntime,
    TransportConfig, VarInt, WriteError,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, SignatureScheme};
use serde::{Deserialize, Serialize};
use tokio::runtime::{Handle, Runtime};

/// Length of the token and of the certificate hash, in bytes.
pub const TOKEN_LEN: usize = 32;
/// How long [`connect`] waits for the QUIC handshake before it reads as
/// UDP being blocked.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// `remote-udp-ports`' default.
pub const DEFAULT_PORTS: &str = "60001-61000";
/// `remote-quic-idle`'s default: 7 days, in seconds.
pub const DEFAULT_IDLE_SECS: u64 = 7 * 24 * 60 * 60;
/// Overrides `remote-udp-ports` (tests).
pub const PORTS_ENV: &str = "GROFE_REMOTE_UDP_PORTS";

/// The name the client asks for in TLS; the pin, not the name, decides.
pub(crate) const SERVER_NAME: &str = "grofe";
pub(crate) const ALPN: &[u8] = b"grofe-proxy/1";
/// A QUIC connection with no packets for this long is gone; the client's
/// keep-alives hold a live path well inside it.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const KEEP_ALIVE: Duration = Duration::from_secs(5);
/// The connection's receive window before the peer is authenticated:
/// what an unauthenticated peer can make this end buffer.
const PRE_AUTH_WINDOW: u32 = 64 * 1024;
/// The connection's and the one stream's receive window once the peer is
/// authenticated (the client: the pinned server; the server: the token).
/// The proxy protocol multiplexes everything over the one stream, so this
/// is the whole link's bytes in flight.
pub(crate) const AUTHED_WINDOW: u32 = 4 * 1024 * 1024;

/// Application close codes on the QUIC connection.
pub(crate) const CLOSE_DONE: u32 = 0;
pub(crate) const CLOSE_BAD_TOKEN: u32 = 1;
pub(crate) const CLOSE_REPLACED: u32 = 2;

/// What the bootstrap line carries.
#[derive(Clone, PartialEq, Eq)]
pub struct Bootstrap {
    pub port: u16,
    pub cert_sha256: [u8; TOKEN_LEN],
    pub token: [u8; TOKEN_LEN],
}

/// Leaves the token out, so a logged or panicking `Bootstrap` never shows it.
impl std::fmt::Debug for Bootstrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bootstrap")
            .field("port", &self.port)
            .field("cert_sha256", &hex(&self.cert_sha256))
            .field("token", &"<redacted>")
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapLine {
    port: u16,
    cert_sha256: String,
    token: String,
}

impl Bootstrap {
    /// The one JSON line `grofe proxy --quic` prints.
    pub fn line(&self) -> String {
        let line = BootstrapLine {
            port: self.port,
            cert_sha256: hex(&self.cert_sha256),
            token: hex(&self.token),
        };
        serde_json::to_string(&line).unwrap_or_default()
    }
}

/// Parses the bootstrap line strictly: exactly the three fields, a non-zero
/// port, and 64 lowercase hex digits for the hash and for the token.
pub fn parse_bootstrap(line: &str) -> Result<Bootstrap, String> {
    let raw: BootstrapLine =
        serde_json::from_str(line.trim()).map_err(|e| format!("bad bootstrap line: {e}"))?;
    if raw.port == 0 {
        return Err("bad bootstrap line: port 0".into());
    }
    Ok(Bootstrap {
        port: raw.port,
        cert_sha256: unhex32(&raw.cert_sha256).map_err(|e| format!("bad cert_sha256: {e}"))?,
        token: unhex32(&raw.token).map_err(|e| format!("bad token: {e}"))?,
    })
}

/// Parses `remote-udp-ports`: `N` or `LOW-HIGH`, ports 1 to 65535.
pub fn parse_port_range(s: &str) -> Result<RangeInclusive<u16>, String> {
    let port = |p: &str| match p.trim().parse::<u16>() {
        Ok(0) | Err(_) => Err(format!("bad port range {s:?}")),
        Ok(n) => Ok(n),
    };
    let (low, high) = match s.split_once('-') {
        Some((a, b)) => (port(a)?, port(b)?),
        None => (port(s)?, port(s)?),
    };
    if low > high {
        return Err(format!("bad port range {s:?}: {low} is above {high}"));
    }
    Ok(low..=high)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex32(s: &str) -> Result<[u8; TOKEN_LEN], String> {
    if s.len() != TOKEN_LEN * 2 {
        return Err(format!(
            "want {} hex digits, got {}",
            TOKEN_LEN * 2,
            s.len()
        ));
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err("not lowercase hex".to_string()),
    };
    let mut out = [0u8; TOKEN_LEN];
    for (i, pair) in s.as_bytes().chunks(2).enumerate() {
        out[i] = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Ok(out)
}

/// Compares two secrets without an early exit, so the time taken does not
/// say how many leading bytes matched.
pub(crate) fn ct_eq(a: &[u8; TOKEN_LEN], b: &[u8; TOKEN_LEN]) -> bool {
    let mut diff = 0u8;
    for i in 0..TOKEN_LEN {
        diff |= a[i] ^ b[i];
    }
    std::hint::black_box(diff) == 0
}

pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// SHA-256 of `data` with the rustls provider's hash (no extra crate).
pub(crate) fn sha256(data: &[u8]) -> [u8; TOKEN_LEN] {
    let suite = rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256;
    let mut out = [0u8; TOKEN_LEN];
    if let Some(s) = suite.tls13() {
        debug_assert_eq!(
            s.common.hash_provider.algorithm(),
            rustls::crypto::hash::HashAlgorithm::SHA256
        );
        let digest = s.common.hash_provider.hash(data);
        out.copy_from_slice(digest.as_ref());
    }
    out
}

pub(crate) fn random32() -> io::Result<[u8; TOKEN_LEN]> {
    let mut out = [0u8; TOKEN_LEN];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut out)
        .map_err(|_| io::Error::other("no secure random source"))?;
    Ok(out)
}

/// Accepts exactly one server certificate: the end-entity certificate
/// whose SHA-256 is the pinned hash. No CA, no name or validity checks: the
/// pin came over ssh, which already authenticated the host. Handshake
/// signatures are still verified with the provider's algorithms, so the
/// peer must hold the pinned certificate's private key.
#[derive(Debug)]
pub struct PinnedCert {
    sha256: [u8; TOKEN_LEN],
    provider: Arc<CryptoProvider>,
}

impl PinnedCert {
    pub fn new(sha256: [u8; TOKEN_LEN]) -> PinnedCert {
        PinnedCert {
            sha256,
            provider: provider(),
        }
    }
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if ct_eq(&sha256(end_entity.as_ref()), &self.sha256) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub(crate) fn transport() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    // One stream carries the proxy protocol, which multiplexes itself.
    t.max_concurrent_bidi_streams(VarInt::from_u32(1));
    t.max_concurrent_uni_streams(VarInt::from_u32(0));
    t.max_idle_timeout(IDLE_TIMEOUT.try_into().ok());
    t.keep_alive_interval(Some(KEEP_ALIVE));
    t.receive_window(VarInt::from_u32(PRE_AUTH_WINDOW));
    t.stream_receive_window(VarInt::from_u32(AUTHED_WINDOW));
    // No unreliable datagrams: nothing here uses them.
    t.datagram_receive_buffer_size(None);
    Arc::new(t)
}

fn client_config(pin: [u8; TOKEN_LEN]) -> io::Result<quinn::ClientConfig> {
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCert::new(pin)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = QuicClientConfig::try_from(tls).map_err(io::Error::other)?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(transport());
    Ok(config)
}

pub(crate) fn runtime() -> io::Result<Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("grofe-quic")
        .enable_all()
        .build()
}

fn read_error(e: ReadError) -> io::Error {
    match e {
        ReadError::ConnectionLost(c) => connection_error(c),
        other => io::Error::new(io::ErrorKind::ConnectionReset, other),
    }
}

pub(crate) fn write_error(e: WriteError) -> io::Error {
    match e {
        WriteError::ConnectionLost(c) => connection_error(c),
        other => io::Error::new(io::ErrorKind::ConnectionReset, other),
    }
}

/// A refused token reads as `PermissionDenied`, a replaced connection as
/// `ConnectionAborted`, a timed out one as `TimedOut`.
pub(crate) fn connection_error(e: ConnectionError) -> io::Error {
    match &e {
        ConnectionError::ApplicationClosed(c)
            if c.error_code == VarInt::from_u32(CLOSE_BAD_TOKEN) =>
        {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the proxy refused the token",
            )
        }
        ConnectionError::ApplicationClosed(c)
            if c.error_code == VarInt::from_u32(CLOSE_REPLACED) =>
        {
            io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "another client took over the connection",
            )
        }
        ConnectionError::TimedOut => io::Error::new(io::ErrorKind::TimedOut, e),
        _ => io::Error::new(io::ErrorKind::ConnectionReset, e),
    }
}

/// The blocking read half of a QUIC stream. It keeps the runtime alive.
pub(crate) struct QuicReader {
    pub(crate) rt: Handle,
    pub(crate) _keep: Option<Arc<Runtime>>,
    pub(crate) recv: RecvStream,
}

impl Read for QuicReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.rt.block_on(self.recv.read(buf)) {
            Ok(Some(n)) => Ok(n),
            Ok(None) => Ok(0),
            Err(e) => Err(read_error(e)),
        }
    }
}

/// The blocking write half of a QUIC stream. It keeps the runtime alive.
pub(crate) struct QuicWriter {
    pub(crate) rt: Handle,
    pub(crate) _keep: Option<Arc<Runtime>>,
    pub(crate) send: SendStream,
}

impl Write for QuicWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.rt.block_on(self.send.write(buf)).map_err(write_error)
    }

    /// QUIC sends written data as soon as it can; nothing is held back.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The stream halves [`QuicLink::streams`] hands out once.
type Halves = (Box<dyn Read + Send>, Box<dyn Write + Send>);

/// A client's QUIC connection to `grofe proxy --quic`. Its tokio runtime
/// runs on a background thread for as long as the link or its streams live.
pub struct QuicLink {
    rt: Arc<Runtime>,
    endpoint: Endpoint,
    conn: quinn::Connection,
    streams: Option<Halves>,
}

impl QuicLink {
    /// The stream after the token, as a blocking pair for
    /// `ProxyConn::new(reader, writer)`. Only the first call has them.
    pub fn streams(&mut self) -> io::Result<Halves> {
        self.streams
            .take()
            .ok_or_else(|| io::Error::other("the streams were already taken"))
    }

    /// Moves the connection to a fresh local UDP socket, as a changed
    /// network would: QUIC migration carries the session to the new path.
    pub fn rebind(&self) -> io::Result<()> {
        let local = self.endpoint.local_addr()?;
        let socket = UdpSocket::bind(unspecified(&local))?;
        socket.set_nonblocking(true)?;
        let _guard = self.rt.enter();
        self.endpoint.rebind(socket)
    }

    /// This end's current UDP address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Ends the connection now rather than at the idle timeout.
    pub fn close(&self) {
        self.conn.close(VarInt::from_u32(CLOSE_DONE), b"");
    }
}

impl Drop for QuicLink {
    fn drop(&mut self) {
        self.close();
    }
}

fn unspecified(like: &SocketAddr) -> SocketAddr {
    match like {
        SocketAddr::V4(_) => SocketAddr::from(([0, 0, 0, 0], 0)),
        SocketAddr::V6(_) => SocketAddr::from(([0u16; 8], 0)),
    }
}

/// Connects to `grofe proxy --quic` on `host` with what its bootstrap line
/// said, and presents the token. A handshake that gets no answer within
/// [`CONNECT_TIMEOUT`] is `ErrorKind::TimedOut` (UDP blocked, say), so the
/// caller can say so and fall back to ssh; a certificate that is not the
/// pinned one fails the handshake with another error.
pub fn connect(host: &str, b: &Bootstrap) -> io::Result<QuicLink> {
    connect_within(host, b, CONNECT_TIMEOUT)
}

fn connect_within(host: &str, b: &Bootstrap, wait: Duration) -> io::Result<QuicLink> {
    let addr = (host, b.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::other(format!("{host} has no address")))?;
    let rt = Arc::new(runtime()?);
    let socket = UdpSocket::bind(unspecified(&addr))?;
    socket.set_nonblocking(true)?;
    let endpoint = {
        let _guard = rt.enter();
        Endpoint::new(
            EndpointConfig::default(),
            None,
            socket,
            Arc::new(TokioRuntime),
        )?
    };
    let config = client_config(b.cert_sha256)?;
    let token = b.token;
    let (conn, send, recv) = rt.block_on(async {
        let connecting = endpoint
            .connect_with(config, addr, SERVER_NAME)
            .map_err(io::Error::other)?;
        let conn = match tokio::time::timeout(wait, connecting).await {
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("no answer from {host} port {} over UDP (blocked?)", b.port),
                ));
            }
            Ok(Err(ConnectionError::TimedOut)) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("no answer from {host} port {} over UDP (blocked?)", b.port),
                ));
            }
            Ok(Err(e)) => return Err(io::Error::other(format!("QUIC handshake failed: {e}"))),
            Ok(Ok(c)) => c,
        };
        // The handshake is done and the certificate matched the pin: only
        // now does the token leave this machine.
        let (mut send, recv) = conn.open_bi().await.map_err(connection_error)?;
        send.write_all(&token).await.map_err(write_error)?;
        conn.set_receive_window(VarInt::from_u32(AUTHED_WINDOW));
        Ok((conn, send, recv))
    })?;
    let reader = QuicReader {
        rt: rt.handle().clone(),
        _keep: Some(Arc::clone(&rt)),
        recv,
    };
    let writer = QuicWriter {
        rt: rt.handle().clone(),
        _keep: Some(Arc::clone(&rt)),
        send,
    };
    Ok(QuicLink {
        rt,
        endpoint,
        conn,
        streams: Some((Box::new(reader), Box::new(writer))),
    })
}

pub mod server;

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Bootstrap {
        Bootstrap {
            port: 60001,
            cert_sha256: [0xab; TOKEN_LEN],
            token: [0x01; TOKEN_LEN],
        }
    }

    #[test]
    fn a_bootstrap_line_round_trips() {
        let line = sample().line();
        assert_eq!(
            line,
            format!(
                "{{\"port\":60001,\"cert_sha256\":\"{}\",\"token\":\"{}\"}}",
                "ab".repeat(32),
                "01".repeat(32)
            )
        );
        assert_eq!(parse_bootstrap(&line).unwrap(), sample());
        assert_eq!(parse_bootstrap(&format!("{line}\n")).unwrap(), sample());
    }

    #[test]
    fn a_bootstrap_line_is_parsed_strictly() {
        let h = "ab".repeat(32);
        let t = "01".repeat(32);
        let bad = [
            format!("{{\"cert_sha256\":\"{h}\",\"token\":\"{t}\"}}"),
            format!("{{\"port\":1,\"token\":\"{t}\"}}"),
            format!("{{\"port\":1,\"cert_sha256\":\"{h}\"}}"),
            format!("{{\"port\":0,\"cert_sha256\":\"{h}\",\"token\":\"{t}\"}}"),
            format!("{{\"port\":70000,\"cert_sha256\":\"{h}\",\"token\":\"{t}\"}}"),
            format!("{{\"port\":1,\"cert_sha256\":\"{h}\",\"token\":\"{t}\",\"x\":1}}"),
            format!(
                "{{\"port\":1,\"cert_sha256\":\"{}\",\"token\":\"{t}\"}}",
                "zz".repeat(32)
            ),
            format!(
                "{{\"port\":1,\"cert_sha256\":\"{}\",\"token\":\"{t}\"}}",
                "AB".repeat(32)
            ),
            format!(
                "{{\"port\":1,\"cert_sha256\":\"{h}\",\"token\":\"{}\"}}",
                "01".repeat(31)
            ),
            format!(
                "{{\"port\":1,\"cert_sha256\":\"{h}\",\"token\":\"{}\"}}",
                "01".repeat(33)
            ),
            "not json".to_string(),
        ];
        for line in bad {
            assert!(parse_bootstrap(&line).is_err(), "{line}");
        }
    }

    #[test]
    fn debug_leaves_the_token_out() {
        let shown = format!("{:?}", sample());
        assert!(!shown.contains(&"01".repeat(32)), "{shown}");
        assert!(shown.contains("redacted"), "{shown}");
    }

    #[test]
    fn port_ranges() {
        assert_eq!(parse_port_range("60001-61000").unwrap(), 60001..=61000);
        assert_eq!(parse_port_range("7000").unwrap(), 7000..=7000);
        assert_eq!(parse_port_range(" 1 - 2 ").unwrap(), 1..=2);
        for bad in ["", "0-10", "10-5", "a-b", "1-70000", "1-2-3", "-5"] {
            assert!(parse_port_range(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn constant_time_compare() {
        let a = [7u8; TOKEN_LEN];
        let mut b = a;
        assert!(ct_eq(&a, &b));
        b[TOKEN_LEN - 1] ^= 1;
        assert!(!ct_eq(&a, &b));
        b = a;
        b[0] ^= 0x80;
        assert!(!ct_eq(&a, &b));
    }

    #[test]
    fn sha256_is_sha256() {
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn self_signed() -> CertificateDer<'static> {
        rcgen::generate_simple_self_signed(vec!["grofe".to_string()])
            .unwrap()
            .cert
            .der()
            .clone()
    }

    #[test]
    fn the_verifier_takes_only_the_pinned_certificate() {
        let pinned = self_signed();
        let other = self_signed();
        let v = PinnedCert::new(sha256(pinned.as_ref()));
        let name = ServerName::try_from("anything.example").unwrap();
        let now = UnixTime::now();
        assert!(v.verify_server_cert(&pinned, &[], &name, &[], now).is_ok());
        assert!(v.verify_server_cert(&other, &[], &name, &[], now).is_err());
        // An intermediate that is the pinned cert does not make another
        // end-entity certificate acceptable.
        assert!(
            v.verify_server_cert(&other, std::slice::from_ref(&pinned), &name, &[], now)
                .is_err()
        );
        assert!(!v.supported_verify_schemes().is_empty());
    }

    #[test]
    fn a_silent_port_reads_as_udp_blocked() {
        // A bound socket that never answers: the handshake times out.
        let quiet = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = Bootstrap {
            port: quiet.local_addr().unwrap().port(),
            ..sample()
        };
        let err = connect_within("127.0.0.1", &b, Duration::from_millis(300))
            .err()
            .unwrap();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
    }
}
