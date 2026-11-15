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
    Ok(mux.wait_closed())
}

fn serve(chan: Channel, payload: &[u8], connect: &Connect) {
    let spec: OpenSpec = match serde_json::from_slice(payload) {
        Ok(s) => s,
        Err(e) => return chan.close(&format!("bad open: {e}")),
    };
    match spec {
        OpenSpec::Attach | OpenSpec::Control => match connect() {
            Ok(stream) => bridge(chan, stream),
            Err(e) => chan.close(&e),
        },
        OpenSpec::Tcp { port } => {
            let addr = SocketAddr::from(([127, 0, 0, 1], port));
            match TcpStream::connect_timeout(&addr, TCP_TIMEOUT) {
                Ok(stream) => bridge(chan, stream),
                Err(e) => chan.close(&format!("cannot connect to 127.0.0.1:{port}: {e}")),
            }
        }
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
    if let Err(e) = io::copy(&mut from_chan, &mut file).and_then(|_| file.flush()) {
        let _ = std::fs::remove_file(&path);
        return Err(format!("upload failed: {e}"));
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
