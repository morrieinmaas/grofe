//! `grofe proxy --quic`, the proxy side of the roaming transport (see the
//! parent module): bind, print the bootstrap line, detach, then serve one
//! authenticated client at a time until `remote-quic-idle` passes without
//! one.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::net::{SocketAddr, UdpSocket};
use std::ops::RangeInclusive;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quinn::crypto::rustls::QuicServerConfig;
use quinn::{Endpoint, EndpointConfig, TokioRuntime, VarInt};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::runtime::Handle;
use tokio::sync::Notify;

use super::{
    ALPN, Bootstrap, CLOSE_BAD_TOKEN, CLOSE_DONE, CLOSE_REPLACED, DEFAULT_IDLE_SECS, DEFAULT_PORTS,
    PORTS_ENV, QuicReader, QuicWriter, SERVER_NAME, TOKEN_LEN, connection_error, ct_eq,
    parse_port_range, provider, random32, runtime, sha256, transport,
};
use crate::remote::proxy::Connect;

/// How long an accepted connection has to present the token.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(10);

/// Where to bind and how long to wait idle.
struct ServeOptions {
    ports: RangeInclusive<u16>,
    idle: Duration,
}

/// `remote-udp-ports` and `remote-quic-idle` from the config files, as a
/// CLI call reads them; [`PORTS_ENV`] overrides the ports.
// ponytail: `-f` config files given to this CLI are not read here; the
// default config chain is.
fn serve_options() -> Result<ServeOptions, String> {
    let resolved = match crate::config::load(&crate::config::Env::from_process()) {
        Ok(r) => Some(r),
        Err(errors) => {
            for e in errors {
                eprintln!("grofe proxy --quic: {e}");
            }
            eprintln!("grofe proxy --quic: using the built-in defaults");
            None
        }
    };
    let get = |key: &str| resolved.as_ref().and_then(|r| r.get(key)).map(String::from);
    let ports = std::env::var(PORTS_ENV)
        .ok()
        .or_else(|| get("remote-udp-ports"))
        .unwrap_or_else(|| DEFAULT_PORTS.into());
    let idle = match get("remote-quic-idle") {
        Some(s) => s
            .parse::<u64>()
            .map_err(|_| format!("bad remote-quic-idle {s:?}"))?,
        None => DEFAULT_IDLE_SECS,
    };
    Ok(ServeOptions {
        ports: parse_port_range(&ports)?,
        idle: Duration::from_secs(idle),
    })
}

/// Binds a UDP socket on all interfaces (the client must reach it) at a
/// free port from `ports`, starting at a place in the range picked by
/// `seed`. IPv6's any address first, else IPv4's.
// ponytail: dual stack relies on the system's IPV6_V6ONLY default being
// off (Linux's and macOS's); where it is on, IPv4 clients cannot reach a
// proxy that bound `[::]`.
fn bind_udp(ports: &RangeInclusive<u16>, seed: u16) -> io::Result<UdpSocket> {
    let (low, high) = (u32::from(*ports.start()), u32::from(*ports.end()));
    let span = high - low + 1;
    let start = u32::from(seed) % span;
    let mut last = io::Error::other("empty port range");
    for i in 0..span {
        let port = (low + (start + i) % span) as u16;
        for any in [
            SocketAddr::from(([0u16; 8], port)),
            SocketAddr::from(([0, 0, 0, 0], port)),
        ] {
            match UdpSocket::bind(any) {
                Ok(s) => return Ok(s),
                Err(e) => last = e,
            }
        }
    }
    Err(io::Error::new(
        last.kind(),
        format!("no free UDP port in {low}-{high}: {last}"),
    ))
}

/// A fresh self-signed certificate: the server config and the
/// certificate's SHA-256.
fn server_config() -> io::Result<(quinn::ServerConfig, [u8; TOKEN_LEN])> {
    let made =
        rcgen::generate_simple_self_signed(vec![SERVER_NAME.into()]).map_err(io::Error::other)?;
    let cert = made.cert.der().clone();
    let hash = sha256(cert.as_ref());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(made.signing_key.serialize_der()));
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(io::Error::other)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    // No 0-RTT: early data is replayable, and the token rides the first
    // stream.
    tls.max_early_data_size = 0;
    let crypto = QuicServerConfig::try_from(tls).map_err(io::Error::other)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(transport());
    // On by default; the roaming transport is for exactly this.
    config.migration(true);
    Ok((config, hash))
}

/// The detached proxy's log and pid file, beside `server_log` (the
/// instance's `paths::log_path`): `<instance>-quic.log` and
/// `<instance>-quic-<port>.pid`.
pub fn proxy_files(server_log: &Path, port: u16) -> (PathBuf, PathBuf) {
    let log = server_log;
    let stem = log
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "grofe".into());
    let dir = log.parent().map(Path::to_path_buf).unwrap_or_default();
    (
        dir.join(format!("{stem}-quic.log")),
        dir.join(format!("{stem}-quic-{port}.pid")),
    )
}

/// `grofe proxy --quic` for the instance at `socket`: binds, makes the
/// certificate and token, detaches, prints the bootstrap line and returns
/// in the foreground process. The detached process serves until
/// `remote-quic-idle` passes with no client, then exits; it never returns.
/// Call it before this process starts any thread (it forks).
pub fn proxy_quic(socket: &Path, connect: Connect) -> io::Result<()> {
    let opts = serve_options().map_err(io::Error::other)?;
    let token = random32()?;
    let udp = bind_udp(&opts.ports, u16::from_le_bytes([token[0], token[31]]))?;
    udp.set_nonblocking(true)?;
    let port = udp.local_addr()?.port();
    let (config, cert_sha256) = server_config()?;
    let line = Bootstrap {
        port,
        cert_sha256,
        token,
    }
    .line();
    let server_log = crate::paths::log_path(&crate::paths::Env::from_process(), socket);
    let (log, pid_file) = proxy_files(&server_log, port);
    if let Some(dir) = log.parent() {
        crate::paths::ensure_private_dir(dir)?;
    }
    if !detach(&pid_file, &log)? {
        let mut out = io::stdout().lock();
        writeln!(out, "{line}")?;
        return out.flush();
    }
    // The detached process: stdin and stdout are /dev/null, stderr the log.
    eprintln!(
        "grofe proxy --quic: pid {} serving on UDP port {port}",
        std::process::id()
    );
    let ended = runtime().and_then(|rt| {
        let handle = rt.handle().clone();
        rt.block_on(serve(udp, config, token, opts.idle, connect, handle))
    });
    match ended {
        Ok(()) => eprintln!(
            "grofe proxy --quic: no client for {}s, exiting",
            opts.idle.as_secs()
        ),
        Err(e) => eprintln!("grofe proxy --quic: {e}"),
    }
    let _ = std::fs::remove_file(&pid_file);
    std::process::exit(0)
}

/// Double forks so the proxy outlives ssh. False in the foreground
/// process, once the middle one has written the detached one's pid to
/// `pid_file` and exited, so the pid file exists by the time the bootstrap
/// line is out. True in the detached process, a session leader with stdin
/// and stdout on `/dev/null` and stderr appended to `log`: nothing holds
/// ssh's channel open.
fn detach(pid_file: &Path, log: &Path) -> io::Result<bool> {
    use nix::sys::wait::{WaitStatus, waitpid};
    use nix::unistd::{ForkResult, fork, setsid};

    // SAFETY: the caller has started no thread, so the child is a whole
    // copy of a single-threaded process with no lock held elsewhere.
    match unsafe { fork() }.map_err(io::Error::from)? {
        ForkResult::Parent { child } => match waitpid(child, None).map_err(io::Error::from)? {
            WaitStatus::Exited(_, 0) => Ok(false),
            other => Err(io::Error::other(format!(
                "the detached proxy did not start: {other:?}"
            ))),
        },
        ForkResult::Child => {
            let middle = || -> io::Result<bool> {
                setsid().map_err(io::Error::from)?;
                // SAFETY: still single-threaded, as above.
                match unsafe { fork() }.map_err(io::Error::from)? {
                    ForkResult::Parent { child } => {
                        let mut f = OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(true)
                            .mode(0o600)
                            .open(pid_file)?;
                        writeln!(f, "{child}")?;
                        Ok(false)
                    }
                    ForkResult::Child => Ok(true),
                }
            };
            match middle() {
                Ok(true) => {
                    redirect_stdio(log)?;
                    Ok(true)
                }
                // The middle process leaves at once, without running the
                // foreground's exit path a second time.
                // SAFETY: `_exit` ends the process; nothing runs after it.
                Ok(false) => unsafe { nix::libc::_exit(0) },
                Err(_) => unsafe { nix::libc::_exit(1) },
            }
        }
    }
}

fn redirect_stdio(log: &Path) -> io::Result<()> {
    use nix::unistd::{dup2_stderr, dup2_stdin, dup2_stdout};
    let null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(log)?;
    dup2_stdin(&null).map_err(io::Error::from)?;
    dup2_stdout(&null).map_err(io::Error::from)?;
    dup2_stderr(&log).map_err(io::Error::from)?;
    Ok(())
}

/// The client slot: at most one authenticated connection.
struct Slot {
    generation: u64,
    current: Option<quinn::Connection>,
    idle_since: Instant,
}

async fn serve(
    udp: UdpSocket,
    config: quinn::ServerConfig,
    token: [u8; TOKEN_LEN],
    idle: Duration,
    connect: Connect,
    rt: Handle,
) -> io::Result<()> {
    let endpoint = Endpoint::new(
        EndpointConfig::default(),
        Some(config),
        udp,
        Arc::new(TokioRuntime),
    )?;
    let slot = Arc::new(Mutex::new(Slot {
        generation: 0,
        current: None,
        idle_since: Instant::now(),
    }));
    let changed = Arc::new(Notify::new());
    loop {
        let wait = {
            let s = slot.lock().map_err(|_| io::Error::other("slot poisoned"))?;
            match s.current {
                Some(_) => None,
                None => match idle.checked_sub(s.idle_since.elapsed()) {
                    Some(left) if !left.is_zero() => Some(left),
                    _ => break,
                },
            }
        };
        let sleep = async {
            match wait {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let admit = admit(
                    incoming,
                    token,
                    Arc::clone(&slot),
                    Arc::clone(&changed),
                    Arc::clone(&connect),
                    rt.clone(),
                );
                tokio::spawn(async move {
                    if let Err(e) = admit.await {
                        eprintln!("grofe proxy --quic: {e}");
                    }
                });
            }
            _ = changed.notified() => {}
            _ = sleep => {}
        }
    }
    endpoint.close(VarInt::from_u32(CLOSE_DONE), b"idle");
    Ok(())
}

/// Handshakes one incoming connection and checks its token. A good one
/// takes the slot, closing any current client, and runs the proxy
/// protocol over its stream on a plain thread; a bad or missing one closes
/// only that connection.
// ponytail: unauthenticated handshakes are bounded only by quinn's own
// limits and TOKEN_TIMEOUT, not by a count of our own.
async fn admit(
    incoming: quinn::Incoming,
    token: [u8; TOKEN_LEN],
    slot: Arc<Mutex<Slot>>,
    changed: Arc<Notify>,
    connect: Connect,
    rt: Handle,
) -> io::Result<()> {
    let conn = incoming.await.map_err(connection_error)?;
    let first = async {
        let (send, mut recv) = conn.accept_bi().await.map_err(connection_error)?;
        let mut got = [0u8; TOKEN_LEN];
        recv.read_exact(&mut got).await.map_err(io::Error::other)?;
        Ok::<_, io::Error>((send, recv, got))
    };
    let (send, recv, got) = match tokio::time::timeout(TOKEN_TIMEOUT, first).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            conn.close(VarInt::from_u32(CLOSE_BAD_TOKEN), b"no token");
            return Err(e);
        }
        Err(_) => {
            conn.close(VarInt::from_u32(CLOSE_BAD_TOKEN), b"no token");
            return Err(io::Error::new(io::ErrorKind::TimedOut, "no token in time"));
        }
    };
    if !ct_eq(&got, &token) {
        conn.close(VarInt::from_u32(CLOSE_BAD_TOKEN), b"bad token");
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refused a bad token from {}", conn.remote_address()),
        ));
    }
    let generation = {
        let mut s = slot.lock().map_err(|_| io::Error::other("slot poisoned"))?;
        if let Some(old) = s.current.take() {
            old.close(VarInt::from_u32(CLOSE_REPLACED), b"replaced");
        }
        s.generation += 1;
        s.current = Some(conn.clone());
        s.generation
    };
    changed.notify_one();
    eprintln!("grofe proxy --quic: client {}", conn.remote_address());
    let reader = QuicReader {
        rt: rt.clone(),
        _keep: None,
        recv,
    };
    let writer = QuicWriter {
        rt,
        _keep: None,
        send,
    };
    std::thread::spawn(move || {
        let why = match crate::remote::proxy::run(reader, writer, connect) {
            Ok(why) => why,
            Err(e) => e.to_string(),
        };
        conn.close(VarInt::from_u32(CLOSE_DONE), b"");
        if let Ok(mut s) = slot.lock()
            && s.generation == generation
        {
            s.current = None;
            s.idle_since = Instant::now();
        }
        changed.notify_one();
        eprintln!("grofe proxy --quic: client gone: {why}");
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bound_port_is_in_the_range_and_skips_a_taken_one() {
        let a = bind_udp(&(40000..=40999), 7).unwrap();
        let port = a.local_addr().unwrap().port();
        assert!((40000..=40999).contains(&port), "{port}");
        let b = bind_udp(&(port..=port), 0);
        assert!(b.is_err(), "the only port is taken");
    }
}
