//! `grofe proxy`: the remote end of the proxy protocol (see
//! `proxy_proto.rs`, `docs/protocol-proxy.md`). It speaks frames on its
//! stdin and stdout and opens what each channel asks for: the server socket
//! (attach, control), a local TCP port, or a file. It holds no session state.

use std::fs::OpenOptions;
use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::proxy_proto::{Channel, Mux, OpenSpec, ProxyHello, Role, handshake};

const TCP_TIMEOUT: Duration = Duration::from_secs(5);
const COPY_BUF: usize = 16 * 1024;
/// Names tried before an upload gives up on finding a free one.
const MAX_SUFFIX: u32 = 10_000;

/// Connects to the server socket, starting a server if none runs.
pub type Connect = Arc<dyn Fn() -> Result<UnixStream, String> + Send + Sync>;

/// Serves one proxy connection over `reader` and `writer` until it ends;
/// why it ended.
pub fn run<R, W>(reader: R, writer: W, connect: Connect) -> io::Result<String>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let mut reader = BufReader::new(reader);
    let mut writer = writer;
    let mine = ProxyHello::current(&["attach", "control", "tcp", "upload"]);
    handshake(&mut reader, &mut writer, &mine)?;
    let mux = Mux::start(reader, writer, Role::Proxy, move |chan, payload| {
        let connect = Arc::clone(&connect);
        std::thread::spawn(move || serve(chan, &payload, &connect));
    });
    let watch = mux.clone();
    std::thread::spawn(move || end_when_silent(&watch, silence_bound()));
    Ok(mux.wait_closed())
}

/// How long the client may send nothing before the proxy takes it for gone:
/// it pings every 5 s, so this is about three missed pings. A client whose
/// network went away never closes the stream (the TCP connection under ssh
/// lingers), and its server-side client would otherwise outlive it.
pub const SILENCE: Duration = Duration::from_secs(20);

/// Test hook: [`SILENCE`] in milliseconds. Test-only.
pub const TEST_SILENCE_ENV: &str = "GROFE_TEST_PROXY_SILENCE_MS";

fn silence_bound() -> Duration {
    std::env::var(TEST_SILENCE_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(SILENCE, Duration::from_millis)
}

/// Ends `mux` once its peer has been silent for `bound`, closing every
/// channel so the server drops the clients behind them.
fn end_when_silent(mux: &Mux, bound: Duration) {
    let step = (bound / 4).clamp(Duration::from_millis(50), Duration::from_secs(1));
    while !mux.is_closed() {
        if mux.idle() >= bound {
            mux.end("the client went silent");
            return;
        }
        std::thread::sleep(step);
    }
}

fn serve(chan: Channel, payload: &[u8], connect: &Connect) {
    let spec: OpenSpec = match serde_json::from_slice(payload) {
        Ok(s) => s,
        Err(e) => return chan.close(&format!("bad open: {e}")),
    };
    match spec {
        OpenSpec::Attach => match connect() {
            Ok(mut stream) => match pass_hello(&chan, &mut stream) {
                Ok(()) => bridge(chan, stream),
                Err(e) => chan.close(&e),
            },
            Err(e) => chan.close(&e),
        },
        OpenSpec::Control => match connect() {
            Ok(stream) => bridge(chan, stream),
            Err(e) => chan.close(&e),
        },
        OpenSpec::Tcp { port } => match connect_local(port) {
            Ok(stream) => bridge(chan, stream),
            Err(e) => chan.close(&format!("cannot connect to 127.0.0.1:{port}: {e}")),
        },
        OpenSpec::Upload { dir, name } => match upload(&chan, &dir, &name) {
            Ok(path) => {
                let mut c = chan.clone();
                let _ = c.write_all(path.as_bytes());
                let _ = c.shutdown_write();
                chan.close("");
            }
            Err(e) => chan.close(&e),
        },
    }
}

/// Connects to `port` on 127.0.0.1, or on ::1 when that is refused (a dev
/// server that listens on IPv6 localhost only, as Vite does).
fn connect_local(port: u16) -> io::Result<TcpStream> {
    let v4 = SocketAddr::from(([127, 0, 0, 1], port));
    match TcpStream::connect_timeout(&v4, TCP_TIMEOUT) {
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            let v6 = SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port));
            TcpStream::connect_timeout(&v6, TCP_TIMEOUT).map_err(|_| e)
        }
        other => other,
    }
}

/// A byte stream the proxy can bridge a channel to.
trait Duplex: Read + Write + Send + Sized + 'static {
    fn duplicate(&self) -> io::Result<Self>;
    fn shutdown(&self, how: Shutdown);
}

impl Duplex for UnixStream {
    fn duplicate(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn shutdown(&self, how: Shutdown) {
        let _ = UnixStream::shutdown(self, how);
    }
}

impl Duplex for TcpStream {
    fn duplicate(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn shutdown(&self, how: Shutdown) {
        let _ = TcpStream::shutdown(self, how);
    }
}

/// Copies both ways between the channel and `stream` until both are done.
fn bridge<S: Duplex>(chan: Channel, stream: S) {
    let Ok(mut from_stream) = stream.duplicate() else {
        return chan.close("cannot duplicate the connection");
    };
    let mut to_chan = chan.clone();
    let inbound = std::thread::spawn(move || {
        let mut buf = vec![0u8; COPY_BUF];
        let why = loop {
            match from_stream.read(&mut buf) {
                Ok(0) => break String::new(),
                Ok(n) => {
                    if to_chan.write_all(&buf[..n]).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => break e.to_string(),
            }
        };
        let _ = to_chan.shutdown_write();
        to_chan.close(&why);
    });

    let mut from_chan = chan.clone();
    let mut to_stream = stream;
    let mut buf = vec![0u8; COPY_BUF];
    loop {
        match from_chan.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if to_stream.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    // Eof: the peer is done sending, not necessarily done receiving.
    // Closed: nothing more can be delivered, so unblock the other half.
    to_stream.shutdown(if chan.is_closed() {
        Shutdown::Both
    } else {
        Shutdown::Write
    });
    let _ = inbound.join();
}

/// Receives a file from the channel; the path it was written to.
fn upload(chan: &Channel, dir: &str, name: &str) -> Result<String, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(format!("bad upload name {name:?}"));
    }
    let dir = expand_home(dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let dir = std::path::absolute(&dir).map_err(|e| e.to_string())?;
    let (mut file, path) = create_unique(&dir, name)?;
    let mut from_chan = chan.clone();
    let copied = io::copy(&mut from_chan, &mut file).and_then(|_| file.flush());
    // Only `eof` completes a file: a close ends the channel mid way, and a
    // truncated file must not stay.
    let why = match copied {
        Err(e) => Some(format!("upload failed: {e}")),
        Ok(()) if !chan.got_eof() => Some("upload failed: closed before the end".to_string()),
        Ok(()) => None,
    };
    if let Some(why) = why {
        let _ = std::fs::remove_file(&path);
        return Err(why);
    }
    Ok(path.display().to_string())
}

/// Creates `name` in `dir`, or `name (2)`, `name (3)`, ... when taken.
/// `create_new` makes the claim atomic, so nothing is ever overwritten.
fn create_unique(dir: &Path, name: &str) -> Result<(std::fs::File, PathBuf), String> {
    for n in 1..=MAX_SUFFIX {
        let candidate = if n == 1 {
            name.to_string()
        } else {
            format!("{name} ({n})")
        };
        let path = dir.join(candidate);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("cannot create {}: {e}", path.display())),
        }
    }
    Err(format!("no free name for {name}"))
}

fn expand_home(dir: &str) -> PathBuf {
    match (dir.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(dir),
    }
}

/// The longest attach hello line passed on.
const MAX_HELLO: usize = 64 * 1024;

/// Passes the attach hello (the channel's first line) to the server with
/// this proxy's own `SSH_AUTH_SOCK` added ([`add_agent`]).
fn pass_hello(chan: &Channel, stream: &mut UnixStream) -> Result<(), String> {
    let mut from_chan = chan.clone();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    // Byte by byte, so nothing after the hello is read ahead of the bridge.
    while line.last() != Some(&b'\n') {
        match from_chan.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => line.push(byte[0]),
            Err(e) => return Err(e.to_string()),
        }
        if line.len() > MAX_HELLO {
            return Err("attach hello too long".to_string());
        }
    }
    let agent = std::env::var("SSH_AUTH_SOCK")
        .ok()
        .filter(|a| live_agent(Path::new(a)));
    let out = match std::str::from_utf8(&line) {
        Ok(text) => add_agent(text, agent.as_deref()).into_bytes(),
        Err(_) => line,
    };
    stream.write_all(&out).map_err(|e| e.to_string())
}

/// Whether `path` is a socket something listens on. A detached
/// `proxy --quic` keeps the agent of the ssh session that started it, which
/// dies with that session: an attach must not repoint the link at it.
fn live_agent(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

/// `line` (an attach hello) with `SSH_AUTH_SOCK` set to `agent` in its
/// client env when the client sent none: the remote client sends no local
/// agent, and an attach over ssh should repoint the session's stable agent
/// link (spec ssh-remote §3) at the agent ssh forwarded to this proxy.
/// Anything else passes unchanged.
fn add_agent(line: &str, agent: Option<&str>) -> String {
    let Some(agent) = agent.filter(|a| a.starts_with('/')) else {
        return line.to_string();
    };
    let Ok(serde_json::Value::Object(mut hello)) = serde_json::from_str(line.trim_end()) else {
        return line.to_string();
    };
    let Some(serde_json::Value::Object(env)) = hello.get_mut("env") else {
        return line.to_string();
    };
    if env.contains_key("SSH_AUTH_SOCK") {
        return line.to_string();
    }
    env.insert("SSH_AUTH_SOCK".to_string(), agent.into());
    env.sort_keys();
    format!("{}\n", serde_json::Value::Object(hello))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_proxys_agent_goes_into_a_hello_without_one() {
        let hello = "{\"cols\":80,\"env\":{\"TERM\":\"x\"},\"grofe\":1}\n";
        assert_eq!(
            add_agent(hello, Some("/tmp/ssh-x/agent.1")),
            "{\"cols\":80,\"env\":{\"SSH_AUTH_SOCK\":\"/tmp/ssh-x/agent.1\",\"TERM\":\"x\"},\"grofe\":1}\n"
        );
        let own = "{\"env\":{\"SSH_AUTH_SOCK\":\"/a\"}}\n";
        assert_eq!(add_agent(own, Some("/b")), own);
        assert_eq!(add_agent(hello, None), hello);
        assert_eq!(add_agent(hello, Some("relative")), hello);
        assert_eq!(add_agent("not json\n", Some("/b")), "not json\n");
    }

    #[test]
    fn only_a_listening_socket_is_an_agent() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("live.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&live).unwrap();
        assert!(live_agent(&live));
        // What a dead ssh session leaves: a socket file nobody listens on.
        // Made by a datagram socket, never a listener: a listener closed here
        // can live on in a child another test forks meanwhile (macOS sets
        // close-on-exec after the socket is made), and then still answer.
        let dead = dir.path().join("dead.sock");
        drop(std::os::unix::net::UnixDatagram::bind(&dead).unwrap());
        assert!(dead.exists());
        assert!(!live_agent(&dead));
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert!(!live_agent(&file));
        assert!(!live_agent(&dir.path().join("missing")));
    }

    #[test]
    fn a_tcp_channel_reaches_a_port_on_ipv6_localhost_only() {
        let Ok(v6) = std::net::TcpListener::bind("[::1]:0") else {
            return; // no IPv6 here
        };
        let port = v6.local_addr().unwrap().port();
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
            return; // the v4 port is taken by something else
        }
        // 127.0.0.1:port is free (refused), so the connection goes to ::1.
        let stream = connect_local(port).unwrap();
        assert!(stream.peer_addr().unwrap().is_ipv6());
        let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = v4.local_addr().unwrap().port();
        assert!(connect_local(port).unwrap().peer_addr().unwrap().is_ipv4());
    }
}
