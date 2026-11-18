//! Port forwards: a listener on 127.0.0.1 per remote port, and the state
//! files `grofe -r host ports` reads.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::Opener;
use crate::remote::proxy_proto::{Channel, OpenSpec};

/// Ports tried after a forward's own before it gives up.
const MAX_PORT_TRIES: u16 = 100;
const COPY_BUF: usize = 16 * 1024;

/// One port forward: local connections to `local` reach `remote`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Forward {
    pub remote: u16,
    pub local: u16,
}

struct Running {
    local: SocketAddr,
    stop: Arc<AtomicBool>,
}

/// The port forwards of one client: a listener on 127.0.0.1 per remote port,
/// each connection bridged to a `tcp` channel.
pub struct Forwarder {
    host: String,
    open: Opener,
    /// This client's state file, for [`list_forwards`].
    state: Option<PathBuf>,
    running: BTreeMap<u16, Running>,
}

impl Forwarder {
    pub fn new(host: &str, open: Opener, state_dir: Option<PathBuf>) -> Forwarder {
        let state = state_dir.map(|d| d.join(host_key(host)).join(std::process::id().to_string()));
        Forwarder {
            host: host.to_string(),
            open,
            state,
            running: BTreeMap::new(),
        }
    }

    /// Starts forwarding `remote`: listens on 127.0.0.1 at the same port, or
    /// the next free one; the local port.
    pub fn start(&mut self, remote: u16) -> Result<u16, String> {
        if let Some(r) = self.running.get(&remote) {
            return Ok(r.local.port());
        }
        // ponytail: std sets SO_REUSEADDR, so on macOS this can bind
        // 127.0.0.1:<port> beside a local wildcard listener on the same port.
        let listener = (remote..=remote.saturating_add(MAX_PORT_TRIES))
            .find_map(|p| TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], p))).ok())
            .ok_or_else(|| format!("no free local port from {remote}"))?;
        let local = listener.local_addr().map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let open = Arc::clone(&self.open);
        let flag = Arc::clone(&stop);
        std::thread::spawn(move || accept_loop(listener, remote, open, flag));
        self.running.insert(remote, Running { local, stop });
        self.save();
        Ok(local.port())
    }

    /// Stops forwarding `remote`. Connections already bridged run on until
    /// either end closes them.
    pub fn stop(&mut self, remote: u16) {
        if let Some(r) = self.running.remove(&remote) {
            r.stop.store(true, Ordering::SeqCst);
            // Wakes the accept so the listener thread sees the flag.
            let _ = TcpStream::connect_timeout(&r.local, Duration::from_secs(1));
            self.save();
        }
    }

    pub fn forwards(&self) -> Vec<Forward> {
        self.running
            .iter()
            .map(|(&remote, r)| Forward {
                remote,
                local: r.local.port(),
            })
            .collect()
    }

    fn save(&self) {
        let Some(path) = &self.state else { return };
        if self.running.is_empty() {
            let _ = std::fs::remove_file(path);
            return;
        }
        let text: String = self
            .forwards()
            .iter()
            .map(|f| format!("{} {}\n", f.remote, f.local))
            .collect();
        let _ = write_state(path, &text);
    }

    pub fn host(&self) -> &str {
        &self.host
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        let ports: Vec<u16> = self.running.keys().copied().collect();
        for port in ports {
            self.stop(port);
        }
    }
}

fn write_state(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        crate::paths::ensure_private_dir(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

fn accept_loop(listener: TcpListener, remote: u16, open: Opener, stop: Arc<AtomicBool>) {
    for conn in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = conn else { continue };
        let open = Arc::clone(&open);
        std::thread::spawn(move || match open(&OpenSpec::Tcp { port: remote }) {
            Ok(chan) => bridge(chan, stream),
            Err(_) => {
                let _ = stream.shutdown(Shutdown::Both);
            }
        });
    }
}

/// Copies both ways between `chan` and `stream` until both are done. Unlike
/// `proxy.rs`'s bridge, a local half close only half closes the channel.
fn bridge(chan: Channel, stream: TcpStream) {
    let Ok(mut from_stream) = stream.try_clone() else {
        return chan.close("cannot duplicate the connection");
    };
    let mut to_chan = chan.clone();
    let inbound = std::thread::spawn(move || {
        let mut buf = vec![0u8; COPY_BUF];
        loop {
            match from_stream.read(&mut buf) {
                // The local end is done sending; the remote end may still
                // answer, and closes the channel when it is done.
                Ok(0) => {
                    let _ = to_chan.shutdown_write();
                    return;
                }
                Ok(n) => {
                    if to_chan.write_all(&buf[..n]).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return to_chan.close(&e.to_string()),
            }
        }
    });
    let mut from_chan = chan.clone();
    let mut to_stream = &stream;
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
    let _ = stream.shutdown(if chan.is_closed() {
        Shutdown::Both
    } else {
        Shutdown::Write
    });
    let _ = inbound.join();
    chan.close("");
}

/// The directory attached clients keep their forwards in: `forwards/`
/// beside the default socket, in the local runtime directory.
pub fn forwards_dir(env: &crate::paths::Env) -> PathBuf {
    crate::paths::socket_path(env, None, None)
        .parent()
        .unwrap_or_else(|| Path::new("/tmp"))
        .join("forwards")
}

/// `host` as one file name: bytes other than letters, digits and `._@-`
/// percent-encoded, and a leading dot.
fn host_key(host: &str) -> String {
    let key: String = host
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"._@-".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    // A leading dot could make `.` or `..`.
    match key.strip_prefix('.') {
        Some(rest) => format!("%2E{rest}"),
        None => key,
    }
}

/// The forwards every live client of `host` holds, from their state files
/// in `dir` ([`forwards_dir`]). A file whose client is gone is removed.
pub fn list_forwards(dir: &Path, host: &str) -> Vec<Forward> {
    let host_dir = dir.join(host_key(host));
    let Ok(entries) = std::fs::read_dir(&host_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(pid) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.parse::<i32>().ok())
            .filter(|&pid| pid > 0)
        else {
            continue;
        };
        if !alive(pid) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        out.extend(text.lines().filter_map(|line| {
            let (r, l) = line.split_once(' ')?;
            Some(Forward {
                remote: r.parse().ok()?,
                local: l.parse().ok()?,
            })
        }));
    }
    out.sort();
    out
}

fn alive(pid: i32) -> bool {
    use nix::errno::Errno;
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
        Ok(()) | Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

/// What `grofe -r host ports` prints: one `host:5173 -> localhost:5174`
/// line per forward.
pub fn format_forwards(host: &str, forwards: &[Forward]) -> String {
    forwards
        .iter()
        .map(|f| format!("{host}:{} -> localhost:{}\n", f.remote, f.local))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_next_free_port_is_taken_and_state_files_list_live_clients() {
        let dir = tempfile::tempdir().unwrap();
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let open: Opener = Arc::new(|_| Err(io::Error::other("no proxy")));
        let mut fwd = Forwarder::new("u@h:22", open, Some(dir.path().to_path_buf()));
        let local = fwd.start(port).unwrap();
        assert_ne!(local, port);
        assert!(local > port && local <= port + MAX_PORT_TRIES);
        let want = vec![Forward {
            remote: port,
            local,
        }];
        assert_eq!(list_forwards(dir.path(), "u@h:22"), want);
        assert_eq!(list_forwards(dir.path(), "other"), []);
        assert_eq!(
            format_forwards("h", &want),
            format!("h:{port} -> localhost:{local}\n")
        );
        // A dead client's file is dropped.
        let stale = dir.path().join(host_key("u@h:22")).join("999999999");
        std::fs::write(&stale, "1 2\n").unwrap();
        assert_eq!(list_forwards(dir.path(), "u@h:22"), want);
        assert!(!stale.exists());
        drop(fwd);
        assert_eq!(list_forwards(dir.path(), "u@h:22"), []);
    }

    #[test]
    fn host_keys_are_one_file_name() {
        assert_eq!(host_key("u@h.x:22"), "u@h.x%3A22");
        assert_eq!(host_key("a/../b"), "a%2F..%2Fb");
        assert_eq!(host_key(".."), "%2E.");
    }
}
