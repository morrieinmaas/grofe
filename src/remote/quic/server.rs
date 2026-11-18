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

use nix::fcntl::{Flock, FlockArg};
use quinn::crypto::rustls::QuicServerConfig;
use quinn::{Endpoint, EndpointConfig, TokioRuntime, VarInt};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::runtime::Handle;
use tokio::sync::{Notify, Semaphore};

use super::{
    ALPN, AUTHED_WINDOW, Bootstrap, CLOSE_BAD_TOKEN, CLOSE_DONE, CLOSE_REPLACED, DEFAULT_IDLE_SECS,
    DEFAULT_PORTS, PORTS_ENV, QuicReader, QuicWriter, SERVER_NAME, TOKEN_LEN, connection_error,
    ct_eq, parse_port_range, provider, random32, runtime, sha256, transport,
};
use crate::remote::proxy::Connect;

/// How long an incoming connection has to finish its handshake and present
/// the token.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Unauthenticated connections handled at once; more are refused.
const MAX_PENDING: usize = 16;
/// At most one line a minute about refused or failed connections.
const FAIL_LOG_EVERY: Duration = Duration::from_secs(60);

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
    // No session tickets either: every connection is a full handshake.
    tls.send_tls13_tickets = 0;
    let crypto = QuicServerConfig::try_from(tls).map_err(io::Error::other)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(transport());
    // On by default; the roaming transport is for exactly this.
    config.migration(true);
    // Few connections may wait to be handled, each with little buffered.
    config.max_incoming(MAX_PENDING);
    config.incoming_buffer_size(64 * 1024);
    config.incoming_buffer_size_total(1024 * 1024);
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
    let server_log = crate::paths::log_path(&crate::paths::Env::from_process(), socket);
    if let Some(dir) = server_log.parent() {
        crate::paths::ensure_private_dir(dir)?;
    }
    stop_older(&server_log);
    let token = random32()?;
    let seed = random32()?;
    let udp = bind_udp(&opts.ports, u16::from_le_bytes([seed[0], seed[1]]))?;
    udp.set_nonblocking(true)?;
    let port = udp.local_addr()?.port();
    let (config, cert_sha256) = server_config()?;
    let line = Bootstrap {
        port,
        cert_sha256,
        token,
    }
    .line();
    let (log, pid_file) = proxy_files(&server_log, port);
    // Held until this process exits: it is what marks the pid file live.
    let Some(_lock) = detach(&pid_file, &log)? else {
        let mut out = io::stdout().lock();
        writeln!(out, "{line}")?;
        return out.flush();
    };
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

/// Stops the other `grofe proxy --quic` processes of the instance whose
/// server log is `server_log` (one proxy per instance) and removes their
/// pid files. A live proxy holds an exclusive lock on its pid file for as
/// long as it runs, so a pid file whose lock can be taken is stale (its
/// proxy died without removing it) and its pid is never signalled: after a
/// crash the number may belong to an unrelated process by now. Only a pid
/// file that is locked, so whose proxy is still running, gets SIGTERM.
fn stop_older(server_log: &Path) {
    let (Some(dir), Some(stem)) = (server_log.parent(), server_log.file_stem()) else {
        return;
    };
    let prefix = format!("{}-quic-", stem.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(port) = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".pid"))
        else {
            continue;
        };
        if port.parse::<u16>().is_err() {
            continue;
        }
        let path = entry.path();
        let Ok(file) = OpenOptions::new().read(true).open(&path) else {
            continue;
        };
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            // Nobody holds it: stale.
            Ok(_stale) => {
                let _ = std::fs::remove_file(&path);
            }
            Err((mut file, nix::errno::Errno::EWOULDBLOCK)) => {
                let mut text = String::new();
                let _ = io::Read::read_to_string(&mut file, &mut text);
                // Empty while the proxy that holds it is still writing it.
                let Ok(pid) = text.trim().parse::<i32>() else {
                    continue;
                };
                if pid > 1 && pid != std::process::id() as i32 {
                    let pid = nix::unistd::Pid::from_raw(pid);
                    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
                    eprintln!("grofe proxy --quic: stopped the older proxy {pid}");
                }
                let _ = std::fs::remove_file(&path);
            }
            Err(_) => {}
        }
    }
}

/// Double forks so the proxy outlives ssh. `None` in the foreground
/// process, once the middle one has written the detached one's pid to
/// `pid_file` and exited, so the pid file exists by the time the bootstrap
/// line is out. In the detached process, the pid file's lock (see
/// [`stop_older`]), which it must hold until it exits; it is a session
/// leader with its working directory at `/`, stdin and stdout on
/// `/dev/null` and stderr appended to `log`, so nothing holds ssh's channel
/// open.
fn detach(pid_file: &Path, log: &Path) -> io::Result<Option<Flock<std::fs::File>>> {
    use nix::sys::wait::{WaitStatus, waitpid};
    use nix::unistd::{ForkResult, fork, setsid};

    // SAFETY: the caller has started no thread, so the child is a whole
    // copy of a single-threaded process with no lock held elsewhere.
    match unsafe { fork() }.map_err(io::Error::from)? {
        ForkResult::Parent { child } => match waitpid(child, None).map_err(io::Error::from)? {
            WaitStatus::Exited(_, 0) => Ok(None),
            other => Err(io::Error::other(format!(
                "the detached proxy did not start: {other:?}"
            ))),
        },
        ForkResult::Child => {
            let middle = || -> io::Result<Option<Flock<std::fs::File>>> {
                setsid().map_err(io::Error::from)?;
                let file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(pid_file)?;
                // Locked before the fork, so the detached process inherits
                // the lock with the open file and holds it from its first
                // instant; this process leaving does not release it.
                let mut lock = Flock::lock(file, FlockArg::LockExclusiveNonblock)
                    .map_err(|(_, e)| io::Error::from(e))?;
                // SAFETY: still single-threaded, as above.
                match unsafe { fork() }.map_err(io::Error::from)? {
                    ForkResult::Parent { child } => {
                        writeln!(lock, "{child}")?;
                        // Dropping a `Flock` unlocks the open file, which the
                        // child shares: this process must leave it as it is.
                        std::mem::forget(lock);
                        Ok(None)
                    }
                    ForkResult::Child => Ok(Some(lock)),
                }
            };
            match middle() {
                Ok(Some(lock)) => {
                    std::env::set_current_dir("/")?;
                    reset_signals()?;
                    redirect_stdio(log)?;
                    Ok(Some(lock))
                }
                // The middle process leaves at once, without running the
                // foreground's exit path a second time or dropping (and so
                // unlocking) the lock its child now shares.
                // SAFETY: `_exit` ends the process; nothing runs after it.
                Ok(None) => unsafe { nix::libc::_exit(0) },
                Err(_) => unsafe { nix::libc::_exit(1) },
            }
        }
    }
}

/// Unblocks every signal and gives SIGTERM, SIGINT and SIGHUP their default
/// action: a mask or ignore the starting process had (an attach client
/// blocks these for its signal thread, and a reconnect bootstraps from
/// there) must not make the detached proxy deaf to `kill`.
fn reset_signals() -> io::Result<()> {
    use nix::sys::signal::{SigHandler, SigSet, SigmaskHow, Signal, signal, sigprocmask};
    sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None).map_err(io::Error::from)?;
    for sig in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
        // SAFETY: the default action installs no handler code.
        unsafe { signal(sig, SigHandler::SigDfl) }.map_err(io::Error::from)?;
    }
    Ok(())
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

/// Holds back lines about refused or failed connections so a flood of them
/// cannot fill the log: at most one per [`FAIL_LOG_EVERY`], which also
/// counts those held back before it.
#[derive(Default)]
struct FailLog {
    next: Option<Instant>,
    held: u64,
}

impl FailLog {
    /// Whether a failure at `now` gets a line; if so, how many were held
    /// back since the last one.
    fn note(&mut self, now: Instant) -> Option<u64> {
        match self.next {
            Some(next) if now < next => {
                self.held += 1;
                None
            }
            _ => {
                self.next = Some(now + FAIL_LOG_EVERY);
                Some(std::mem::take(&mut self.held))
            }
        }
    }
}

fn log_failure(fails: &Mutex<FailLog>, what: &str) {
    let Ok(mut f) = fails.lock() else { return };
    match f.note(Instant::now()) {
        Some(0) => eprintln!("grofe proxy --quic: {what}"),
        Some(held) => eprintln!("grofe proxy --quic: {what} ({held} more not logged)"),
        None => {}
    }
}

/// The client slot: at most one authenticated connection.
struct Slot {
    generation: u64,
    current: Option<quinn::Connection>,
    idle_since: Instant,
}

/// What [`serve`] and every [`admit`] share.
struct Gate {
    token: [u8; TOKEN_LEN],
    slot: Mutex<Slot>,
    /// Wakes [`serve`] when the slot changes.
    changed: Notify,
    connect: Connect,
    rt: Handle,
    pending: Arc<Semaphore>,
    fails: Mutex<FailLog>,
}

/// How long [`serve`] may wait for the next connection.
enum Wait {
    /// A client is connected: no idle timeout runs.
    Forever,
    For(Duration),
    /// Idle for `remote-quic-idle`: time to exit.
    Done,
}

fn lock_slot(gate: &Gate) -> io::Result<std::sync::MutexGuard<'_, Slot>> {
    gate.slot
        .lock()
        .map_err(|_| io::Error::other("slot poisoned"))
}

fn idle_wait(gate: &Gate, idle: Duration) -> io::Result<Wait> {
    let s = lock_slot(gate)?;
    Ok(match s.current {
        Some(_) => Wait::Forever,
        None => match idle.checked_sub(s.idle_since.elapsed()) {
            Some(left) if !left.is_zero() => Wait::For(left),
            _ => Wait::Done,
        },
    })
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
    let gate = Arc::new(Gate {
        token,
        slot: Mutex::new(Slot {
            generation: 0,
            current: None,
            idle_since: Instant::now(),
        }),
        changed: Notify::new(),
        connect,
        rt,
        pending: Arc::new(Semaphore::new(MAX_PENDING)),
        fails: Mutex::new(FailLog::default()),
    });
    loop {
        let wait = match idle_wait(&gate, idle)? {
            Wait::Forever => None,
            Wait::For(d) => Some(d),
            Wait::Done => break,
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
                on_incoming(&gate, incoming);
            }
            _ = gate.changed.notified() => {}
            _ = sleep => {}
        }
    }
    endpoint.close(VarInt::from_u32(CLOSE_DONE), b"idle");
    Ok(())
}

/// One incoming connection: a retry first, then [`admit`] on its own task
/// while an unauthenticated slot is free.
fn on_incoming(gate: &Arc<Gate>, incoming: quinn::Incoming) {
    // A retry first proves the peer can receive at the address it claims,
    // before any handshake work or state.
    if !incoming.remote_address_validated() && incoming.may_retry() {
        let _ = incoming.retry();
        return;
    }
    let Ok(permit) = Arc::clone(&gate.pending).try_acquire_owned() else {
        incoming.refuse();
        log_failure(
            &gate.fails,
            "refused a connection: too many unauthenticated",
        );
        return;
    };
    let gate = Arc::clone(gate);
    tokio::spawn(async move {
        let admitted = admit(incoming, Arc::clone(&gate)).await;
        drop(permit);
        if let Err(e) = admitted {
            log_failure(&gate.fails, &e.to_string());
        }
    });
}

/// The handshake, the first stream and the token it starts with, all
/// within [`TOKEN_TIMEOUT`].
async fn handshake(
    incoming: quinn::Incoming,
) -> io::Result<(
    quinn::Connection,
    quinn::SendStream,
    quinn::RecvStream,
    [u8; TOKEN_LEN],
)> {
    let from = incoming.remote_address();
    let first = async {
        let conn = incoming.await.map_err(connection_error)?;
        let stream = async {
            let (send, mut recv) = conn.accept_bi().await.map_err(connection_error)?;
            let mut got = [0u8; TOKEN_LEN];
            recv.read_exact(&mut got).await.map_err(io::Error::other)?;
            Ok::<_, io::Error>((send, recv, got))
        };
        match stream.await {
            Ok(v) => Ok((conn, v)),
            Err(e) => {
                conn.close(VarInt::from_u32(CLOSE_BAD_TOKEN), b"no token");
                Err(e)
            }
        }
    };
    // Dropping the handshake on timeout abandons the connection, which
    // quinn then closes.
    match tokio::time::timeout(TOKEN_TIMEOUT, first).await {
        Ok(Ok((conn, (send, recv, got)))) => Ok((conn, send, recv, got)),
        Ok(Err(e)) => Err(io::Error::new(e.kind(), format!("{from}: {e}"))),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{from}: no handshake and token in time"),
        )),
    }
}

/// Puts `conn` in the slot, closing any current client; its generation.
fn take_slot(gate: &Gate, conn: &quinn::Connection) -> io::Result<u64> {
    let mut s = lock_slot(gate)?;
    if let Some(old) = s.current.take() {
        old.close(VarInt::from_u32(CLOSE_REPLACED), b"replaced");
    }
    s.generation += 1;
    s.current = Some(conn.clone());
    Ok(s.generation)
}

/// Empties the slot if `generation` still holds it.
fn release_slot(gate: &Gate, generation: u64) {
    if let Ok(mut s) = gate.slot.lock()
        && s.generation == generation
    {
        s.current = None;
        s.idle_since = Instant::now();
    }
}

/// Handshakes one incoming connection and checks its token, all within
/// [`TOKEN_TIMEOUT`]. A good one takes the slot, closing any current
/// client, and runs the proxy protocol over its stream on a plain thread;
/// a bad or missing one closes only that connection.
async fn admit(incoming: quinn::Incoming, gate: Arc<Gate>) -> io::Result<()> {
    let from = incoming.remote_address();
    let (conn, send, recv, got) = handshake(incoming).await?;
    if !ct_eq(&got, &gate.token) {
        conn.close(VarInt::from_u32(CLOSE_BAD_TOKEN), b"bad token");
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refused a bad token from {from}"),
        ));
    }
    conn.set_receive_window(VarInt::from_u32(AUTHED_WINDOW));
    let generation = take_slot(&gate, &conn)?;
    gate.changed.notify_one();
    eprintln!("grofe proxy --quic: client {}", conn.remote_address());
    let reader = QuicReader {
        rt: gate.rt.clone(),
        _keep: None,
        recv,
    };
    let writer = QuicWriter {
        rt: gate.rt.clone(),
        _keep: None,
        send,
    };
    std::thread::spawn(move || {
        let why = match crate::remote::proxy::run(reader, writer, Arc::clone(&gate.connect)) {
            Ok(why) => why,
            Err(e) => e.to_string(),
        };
        conn.close(VarInt::from_u32(CLOSE_DONE), b"");
        release_slot(&gate, generation);
        gate.changed.notify_one();
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

    #[test]
    fn failures_get_one_line_a_minute_with_a_count_of_the_rest() {
        let mut log = FailLog::default();
        let t = Instant::now();
        assert_eq!(log.note(t), Some(0));
        for i in 1..=5 {
            assert_eq!(log.note(t + Duration::from_secs(i)), None);
        }
        assert_eq!(log.note(t + FAIL_LOG_EVERY), Some(5));
        assert_eq!(log.note(t + FAIL_LOG_EVERY + Duration::from_secs(1)), None);
        assert_eq!(log.note(t + FAIL_LOG_EVERY * 2), Some(1));
    }
}
