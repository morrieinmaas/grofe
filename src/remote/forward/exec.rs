//! The I/O of uploads and URLs over the proxy connection: the slow actions
//! ([`run_slow`]) the forwarding hook runs off the input thread.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::text::tilde;
use super::{Action, FALLBACK_UPLOAD_DIR, FilePaste, Input, UploadDir};
use crate::protocol::{ClientCtx, Dialect, Hello, Reply, Request, encode_hello};
use crate::remote::connect::Link;
use crate::remote::proxy_proto::{Channel, OpenSpec, ProxyConn};

/// Opens a channel on the current proxy connection.
pub type Opener = Arc<dyn Fn(&OpenSpec) -> io::Result<Channel> + Send + Sync>;

/// The current link, shared by everything that opens channels, so a
/// reconnect re-points them all at once.
#[derive(Clone, Default)]
pub struct LinkCell(Arc<Mutex<Option<Link>>>);

impl LinkCell {
    pub fn new(link: Link) -> LinkCell {
        let cell = LinkCell::default();
        cell.set(link);
        cell
    }

    /// Re-points the cell at `link` (after a reconnect).
    pub fn set(&self, link: Link) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(link);
    }

    /// The current link, if there is one.
    pub fn get(&self) -> Option<Link> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// An [`Opener`] over whatever link `cell` holds when a channel is opened.
pub fn opener(cell: LinkCell) -> Opener {
    Arc::new(move |spec| match cell.get() {
        Some(link) => link.open(spec),
        None => Err(io::Error::new(io::ErrorKind::NotConnected, "not connected")),
    })
}

/// Whether `action` may block for long (an upload, a round trip, a
/// program): run it off the input thread.
pub fn is_slow(action: &Action) -> bool {
    matches!(
        action,
        Action::OpenUrl(_)
            | Action::ResolveDir(_)
            | Action::Upload { .. }
            | Action::CheckFiles { .. }
            | Action::CheckOpen(_)
    )
}

/// Runs a slow action ([`is_slow`]) over `link`; `target` is the `-t`
/// whose active pane's directory takes uploads.
pub fn run_slow(link: &Link, target: Option<&str>, action: Action) -> Option<Input> {
    match action {
        Action::OpenUrl(url) => Some(Input::Opened(crate::open::open_locally(&url))),
        Action::ResolveDir(paste) => {
            Some(Input::DirResolved(paste, upload_dir(link.conn(), target)))
        }
        Action::Upload { dir, paste } => Some(upload(link, &dir, paste, &AtomicBool::new(false))),
        local @ (Action::CheckFiles { .. } | Action::CheckOpen(_)) => run_local(local),
        _ => None,
    }
}

/// Runs a check of local paths ([`Action::CheckFiles`], [`Action::CheckOpen`]):
/// no connection needed.
pub fn run_local(action: Action) -> Option<Input> {
    match action {
        Action::CheckFiles { paths, original } => Some(Input::FilesChecked {
            original,
            files: check_files(paths),
        }),
        Action::CheckOpen(url) => Some(Input::OpenChecked(super::text::local_file_to_open(&url))),
        _ => None,
    }
}

/// Runs an [`Action::Upload`] of `paste` into `dir`, stopping once
/// `cancel` is set; the [`Input::Uploaded`] that answers it.
pub fn upload(link: &Link, dir: &str, paste: FilePaste, cancel: &AtomicBool) -> Input {
    let done = upload_files(link.conn(), dir, &paste.files, &paste.sizes, cancel);
    Input::Uploaded {
        paste,
        arrived: done.arrived,
        error: done.error,
        cancelled: done.cancelled,
        grew: done.grew,
    }
}

/// Where a paste's files go: the active pane's directory of `target` on the
/// remote, else [`FALLBACK_UPLOAD_DIR`].
pub fn upload_dir(conn: &ProxyConn, target: Option<&str>) -> UploadDir {
    let mut cwd = vec!["display-message".to_string(), "-p".to_string()];
    if let Some(t) = target {
        cwd.extend(["-t".to_string(), t.to_string()]);
    }
    cwd.push("#{pane_current_path}".to_string());
    let home = ["show-environment", "-g", "HOME"]
        .map(String::from)
        .to_vec();
    let answers = control(conn, &[cwd, home]).unwrap_or_default();
    let path = answers
        .first()
        .map(|s| s.trim().to_string())
        .filter(|s| s.starts_with('/'));
    let home = answers
        .get(1)
        .and_then(|s| s.trim().strip_prefix("HOME=").map(str::to_string))
        .filter(|h| h.starts_with('/') && h != "/");
    match path {
        Some(path) => {
            let shown = tilde(&path, home.as_deref());
            UploadDir { path, shown }
        }
        None => UploadDir {
            path: FALLBACK_UPLOAD_DIR.to_string(),
            shown: FALLBACK_UPLOAD_DIR.to_string(),
        },
    }
}

/// Runs `requests` over one control channel; each one's output, in order,
/// or the first error.
fn control(conn: &ProxyConn, requests: &[Vec<String>]) -> Result<Vec<String>, String> {
    let chan = conn.open(&OpenSpec::Control).map_err(|e| e.to_string())?;
    let ctx = ClientCtx {
        cwd: PathBuf::from("/"),
        pane: None,
        env: Vec::new(),
        client: None,
        dialect: Dialect::Grofe,
        request: Default::default(),
    };
    let mut lines = encode_hello(&Hello::Control(ctx)).map_err(|e| e.to_string())?;
    for (i, argv) in requests.iter().enumerate() {
        let req = Request {
            id: i as u64 + 1,
            argv: argv.clone(),
            ..Default::default()
        };
        lines.push('\n');
        lines.push_str(&serde_json::to_string(&req).map_err(|e| e.to_string())?);
    }
    lines.push('\n');
    let mut writer = chan.clone();
    writer
        .write_all(lines.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut answers = vec![String::new(); requests.len()];
    let mut left = requests.len();
    let mut reader = BufReader::new(chan.clone());
    let mut line = String::new();
    while left > 0 {
        line.clear();
        if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            return Err("the server closed the control channel".to_string());
        }
        let (id, out) = match serde_json::from_str::<Reply>(&line) {
            Ok(Reply::Ok { id, out }) => (id, out),
            Ok(Reply::Err { id, .. }) => (id, String::new()),
            _ => continue,
        };
        if let Some(slot) = answers.get_mut((id as usize).wrapping_sub(1)) {
            *slot = out;
            left -= 1;
        }
    }
    chan.close("");
    Ok(answers)
}

/// What [`upload_files`] did: the remote paths of the files that arrived,
/// in order; why the next one failed (the rest were not tried); whether it
/// was cancelled; and the names of files that grew after the paste.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Upload {
    pub arrived: Vec<String>,
    pub error: Option<String>,
    pub cancelled: bool,
    pub grew: Vec<String>,
}

/// Uploads each of `files` into `dir` on the remote, at most its `sizes`
/// entry (its size when pasted) of each, stopping at the first failure or
/// once `cancel` is set.
pub fn upload_files(
    conn: &ProxyConn,
    dir: &str,
    files: &[PathBuf],
    sizes: &[u64],
    cancel: &AtomicBool,
) -> Upload {
    let mut done = Upload::default();
    for (path, &size) in files.iter().zip(sizes) {
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => {
                done.error = Some(format!("cannot upload {}: no name", path.display()));
                return done;
            }
        };
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) => {
                done.error = Some(format!("cannot read {}: {e}", path.display()));
                return done;
            }
        };
        let reader = Cancellable {
            inner: file.take(size),
            cancel,
        };
        match conn.upload(dir, &name, reader) {
            Ok(remote) => done.arrived.push(remote),
            Err(_) if cancel.load(Ordering::SeqCst) => {
                done.cancelled = true;
                return done;
            }
            Err(e) => {
                done.error = Some(format!("upload of {name} failed: {e}"));
                return done;
            }
        }
        if std::fs::metadata(path).is_ok_and(|m| m.len() > size) {
            done.grew.push(name);
        }
    }
    done
}

/// A reader that fails once `cancel` is set, which ends the upload channel
/// with a reason (the proxy then removes the partial file).
struct Cancellable<'a, R> {
    inner: R,
    cancel: &'a AtomicBool,
}

impl<R: Read> Read for Cancellable<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(io::Error::other("upload cancelled"));
        }
        self.inner.read(buf)
    }
}

/// Each of `paths` with its size, when every one is a local regular file;
/// `None` otherwise (a directory, a missing path: the paste stays text).
pub fn check_files(paths: Vec<PathBuf>) -> Option<Vec<(PathBuf, u64)>> {
    paths
        .into_iter()
        .map(|p| {
            let meta = std::fs::metadata(&p).ok().filter(|m| m.is_file())?;
            Some((p, meta.len()))
        })
        .collect()
}
