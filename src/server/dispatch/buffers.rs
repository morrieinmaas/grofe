//! The buffer commands (TM.4, TM.7): `set-buffer`, `show-buffer`,
//! `load-buffer`, `save-buffer`, `list-buffers`, `delete-buffer`,
//! `paste-buffer` and `choose-buffer`,
//! with tmux 3.7b's flags, output and error text (`cmd-set-buffer.c`,
//! `cmd-save-buffer.c`, `cmd-list-buffers.c`, `cmd-paste-buffer.c`).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cmd::{
    self, ChooseBuffer, DeleteBuffer, ListBuffers, LoadBuffer, PasteBuffer, Resolve, SaveBuffer,
    SetBuffer, ShowBuffer,
};
use crate::protocol::{ClientCtx, Failure};
use crate::server::buffers::{self, BufferVars};
use crate::server::{Job, State, clients, dispatch, passthrough, tree};

/// `set-buffer`: a new automatic buffer, or with `-b` that named buffer;
/// `-a` appends to the `-b` buffer; `-n` renames (`-b`'s buffer, else the
/// newest automatic one) and ignores any data.
pub fn set_buffer(state: &mut State, _ctx: &ClientCtx, c: SetBuffer) -> Result<String, String> {
    let limit = buffers::limit(state);
    let bufs = &mut state.buffers;
    let found = c.name.as_deref().and_then(|n| bufs.get(n));
    if let Some(new) = &c.rename {
        let old = match (found, &c.name) {
            (Some(b), _) => b.name.clone(),
            (None, Some(name)) => return Err(format!("unknown buffer: {name}")),
            (None, None) => match bufs.top() {
                Some(b) => b.name.clone(),
                None => return Err("no buffer".to_string()),
            },
        };
        bufs.rename(&old, new)?;
        return Ok(String::new());
    }
    let Some(text) = c.data else {
        return Err("no data specified".to_string());
    };
    if text.is_empty() {
        return Ok(String::new());
    }
    let mut data = match found {
        Some(b) if c.append => b.data.clone(),
        _ => Vec::new(),
    };
    if data.len() + text.len() > buffers::MAX_BUFFER_BYTES {
        return Err(format!(
            "buffer too large: {} bytes (limit {})",
            data.len() + text.len(),
            buffers::MAX_BUFFER_BYTES
        ));
    }
    data.extend_from_slice(text.as_bytes());
    match &c.name {
        Some(name) => bufs.set(name, data),
        None => bufs.add(data, limit),
    }
    .map(|()| String::new())
}

/// `show-buffer`: the buffer's data as it is.
// ponytail: tmux shows a buffer to an attached client's command (a key, the
// prompt) escaped in view mode; grofe prints the data either way.
pub fn show_buffer(state: &mut State, _ctx: &ClientCtx, c: ShowBuffer) -> Result<String, String> {
    let b = match &c.name {
        Some(name) => state
            .buffers
            .get(name)
            .ok_or_else(|| format!("no buffer {name}"))?,
        None => state.buffers.top().ok_or("no buffers")?,
    };
    Ok(String::from_utf8_lossy(&b.data).into_owned())
}

/// A path a command names, as the CLI meant it: relative to its cwd.
fn cli_path(ctx: &ClientCtx, path: &str) -> PathBuf {
    ctx.cwd.join(path)
}

/// tmux's text for a file it could not read or write: the system's
/// message, then the path (`No such file or directory: /x/y`).
fn file_error(e: &io::Error, path: &Path) -> String {
    let what = match e.raw_os_error() {
        Some(code) => nix::errno::Errno::from_raw(code).desc().to_string(),
        None => e.to_string(),
    };
    format!("{what}: {}", path.display())
}

/// How long `load-buffer` may wait to read a file, or `save-buffer` to
/// write one. A FIFO waits for its other end, as tmux's client waits for
/// it; the bound keeps a writer or reader that never comes from holding the
/// request for good.
pub const FILE_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs a file's I/O (`io`) off the actor, so a FIFO or a terminal that
/// blocks holds only the request that named it, not every client. Its
/// result goes to `done` back on the actor, which answers the request (and
/// releases the rest of it, held meanwhile). Past `timeout` the request
/// fails, and `unstick` frees a thread still waiting on the file.
fn off_actor<T: Send + 'static>(
    state: &mut State,
    timeout: Duration,
    io: impl FnOnce() -> Result<T, String> + Send + 'static,
    unstick: impl FnOnce() + Send + 'static,
    done: impl FnOnce(&mut State, T) -> Result<String, String> + Send + 'static,
) -> Result<String, String> {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        // No runtime (unit tests): there is no actor to keep free.
        return io().and_then(|v| done(state, v));
    };
    let tx = dispatch::defer_reply(state);
    let jobs = state.jobs.clone();
    rt.spawn(async move {
        let answer: Result<String, Failure> =
            match tokio::time::timeout(timeout, tokio::task::spawn_blocking(io)).await {
                Ok(Ok(Ok(v))) => {
                    let job: Job = Box::new(move |state: &mut State| {
                        let _ = tx.send(done(state, v).map_err(Failure::from));
                    });
                    let _ = jobs.send(job).await;
                    return;
                }
                Ok(Ok(Err(e))) => Err(e.into()),
                Ok(Err(e)) => Err(e.to_string().into()),
                Err(_) => {
                    // Off the runtime: unsticking may wait for the thread
                    // to reach its open.
                    std::thread::spawn(unstick);
                    Err(format!("timed out after {timeout:?}").into())
                }
            };
        let _ = tx.send(answer);
    });
    Ok(String::new())
}

/// Frees a thread blocked opening a FIFO (`read`: for reading) by opening
/// its other end for a moment; anything else is left alone. The thread may
/// not have reached its `open` yet (a busy pool starts it late), and then
/// there is no one to meet: the other end is tried again until it is met,
/// or [`UNSTICK_FOR`] passes, so the thread is never left blocked for good.
fn unstick_fifo(path: &Path, read: bool) {
    use std::os::unix::fs::FileTypeExt;
    if !std::fs::metadata(path).is_ok_and(|m| m.file_type().is_fifo()) {
        return;
    }
    let mut options = OpenOptions::new();
    options.custom_flags(O_NONBLOCK);
    if read {
        options.write(true);
    } else {
        options.read(true);
    }
    let until = Instant::now() + UNSTICK_FOR;
    if read {
        // A non-blocking open for writing fails (ENXIO) while no reader
        // has the FIFO. One quick open and close can still leave the
        // reader blocked (macOS, seen: in its read, with no writer left),
        // so the writer comes back, held a moment each time, until the
        // reader has gone.
        let mut met = false;
        while Instant::now() < until {
            match options.open(path) {
                Ok(_writer) => {
                    met = true;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) if met => return,
                Err(_) => {}
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        return;
    }
    while options.open(path).is_err() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// How long [`unstick_fifo`] waits for the blocked thread to arrive.
const UNSTICK_FOR: Duration = Duration::from_secs(10);

const O_NONBLOCK: i32 = nix::fcntl::OFlag::O_NONBLOCK.bits();

/// The file's data, up to the buffer limit.
fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    File::open(path)
        .and_then(|f| {
            f.take(buffers::MAX_BUFFER_BYTES as u64 + 1)
                .read_to_end(&mut data)
        })
        .map_err(|e| file_error(&e, path))?;
    if data.len() > buffers::MAX_BUFFER_BYTES {
        return Err(format!(
            "buffer too large: more than {} bytes",
            buffers::MAX_BUFFER_BYTES
        ));
    }
    Ok(data)
}

/// Writes `data` to a file opened non-blocking, waiting while a FIFO's
/// reader is behind, but no longer than `timeout`.
fn write_file(mut f: File, data: &[u8], path: &Path, timeout: Duration) -> Result<(), String> {
    let until = Instant::now() + timeout;
    let mut rest = data;
    while !rest.is_empty() {
        match f.write(rest) {
            Ok(0) => return Err(file_error(&io::ErrorKind::WriteZero.into(), path)),
            Ok(n) => rest = &rest[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= until {
                    return Err(format!("timed out after {timeout:?}"));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(file_error(&e, path)),
        }
    }
    Ok(())
}

/// `load-buffer`: the file's data (`-`: the CLI's stdin) as a new
/// automatic buffer, or the `-b` one; empty data makes none, as in tmux.
/// `-w` also sends it to `-t`'s client, else the current one, if any. A
/// file is read off the actor ([`off_actor`]).
pub fn load_buffer(state: &mut State, ctx: &ClientCtx, c: LoadBuffer) -> Result<String, String> {
    if c.path == "-" {
        // ponytail: a binding has no CLI stdin, so it loads nothing; tmux
        // reads the server's own.
        let data = state
            .cli_io
            .as_ref()
            .and_then(|io| io.stdin.clone())
            .unwrap_or_default();
        return install_loaded(state, ctx, &c, data);
    }
    let path = cli_path(ctx, &c.path);
    let ctx = ctx.clone();
    let stuck = path.clone();
    off_actor(
        state,
        FILE_TIMEOUT,
        move || read_file(&path),
        move || unstick_fifo(&stuck, true),
        move |state, data| install_loaded(state, &ctx, &c, data),
    )
}

fn install_loaded(
    state: &mut State,
    ctx: &ClientCtx,
    c: &LoadBuffer,
    data: Vec<u8>,
) -> Result<String, String> {
    if data.is_empty() {
        return Ok(String::new());
    }
    if data.len() > buffers::MAX_BUFFER_BYTES {
        return Err(format!(
            "buffer too large: more than {} bytes",
            buffers::MAX_BUFFER_BYTES
        ));
    }
    if c.clipboard {
        let to = match c.client.as_deref() {
            Some(name) => clients::client_by_name(state, name).ok(),
            None => clients::current_client(state, ctx),
        };
        if let Some(id) = to {
            passthrough::send_selection(state, id, &data);
        }
    }
    let limit = buffers::limit(state);
    match &c.name {
        Some(name) => state.buffers.set(name, data),
        None => state.buffers.add(data, limit),
    }
    .map(|()| String::new())
}

/// `save-buffer`: the `-b` buffer, else the newest automatic one, to the
/// file (`-`: the CLI's stdout); `-a` appends. A new file is `0644` under
/// the CLI's umask, as tmux's client creates it; an existing one keeps its
/// mode. The file opens non-blocking, so a FIFO with no reader fails at
/// once (ENXIO) as in tmux 3.7b, and is written off the
/// actor ([`off_actor`]).
// ponytail: `-` prints the data as text, so bytes that are not UTF-8 come
// out replaced; tmux writes them as they are.
pub fn save_buffer(state: &mut State, ctx: &ClientCtx, c: SaveBuffer) -> Result<String, String> {
    let b = match &c.name {
        Some(name) => state
            .buffers
            .get(name)
            .ok_or_else(|| format!("no buffer {name}"))?,
        None => state.buffers.top().ok_or("no buffers")?,
    };
    if c.path == "-" {
        return Ok(String::from_utf8_lossy(&b.data).into_owned());
    }
    let data = b.data.clone();
    let path = cli_path(ctx, &c.path);
    let mode = 0o644 & !state.cli_io.as_ref().and_then(|io| io.umask).unwrap_or(0);
    let new = std::fs::symlink_metadata(&path).is_err();
    let mut options = OpenOptions::new();
    options.create(true).mode(mode).custom_flags(O_NONBLOCK);
    if c.append {
        options.append(true);
    } else {
        options.write(true).truncate(true);
    }
    let f = options
        .open(&path)
        .and_then(|f| {
            // The server's own umask must not narrow the CLI's.
            if new {
                f.set_permissions(std::fs::Permissions::from_mode(mode))?;
            }
            Ok(f)
        })
        .map_err(|e| file_error(&e, &path))?;
    off_actor(
        state,
        FILE_TIMEOUT + Duration::from_secs(1),
        move || write_file(f, &data, &path, FILE_TIMEOUT),
        || {},
        |_, ()| Ok(String::new()),
    )
}

/// `list-buffers`: one line per buffer, newest first, in tmux's template
/// or `-F`'s.
pub fn list_buffers(state: &mut State, _ctx: &ClientCtx, c: ListBuffers) -> Result<String, String> {
    let format = c.format.as_deref().unwrap_or(buffers::LIST_FORMAT);
    Ok(state
        .buffers
        .iter()
        .map(|b| cmd::expand(format, &BufferVars(b)) + "\n")
        .collect())
}

/// `delete-buffer`: the `-b` buffer, else the newest automatic one.
pub fn delete_buffer(
    state: &mut State,
    _ctx: &ClientCtx,
    c: DeleteBuffer,
) -> Result<String, String> {
    let bufs = &mut state.buffers;
    let name = match &c.name {
        Some(name) if bufs.get(name).is_some() => name.clone(),
        Some(name) => return Err(format!("unknown buffer: {name}")),
        None => bufs.top().map(|b| b.name.clone()).ok_or("no buffer")?,
    };
    bufs.delete(&name);
    Ok(String::new())
}

/// `paste-buffer`: the `-b` buffer, else the newest automatic one (none:
/// nothing happens), typed into the target pane ([`buffers::paste_bytes`]);
/// `-d` then deletes it.
pub fn paste_buffer(state: &mut State, ctx: &ClientCtx, c: PasteBuffer) -> Result<String, String> {
    let pane = state.model.resolve_pane(&c.target, ctx)?;
    let b = match &c.name {
        Some(name) => Some(
            state
                .buffers
                .get(name)
                .ok_or_else(|| format!("no buffer {name}"))?,
        ),
        None => state.buffers.top(),
    };
    let Some(b) = b else {
        return Ok(String::new());
    };
    let name = b.name.clone();
    // tmux pastes nothing into a pane taking no input, but `-d` still
    // deletes the buffer.
    if state.model.panes[&pane].input_off {
        if c.delete {
            state.buffers.delete(&name);
        }
        return Ok(String::new());
    }
    let handle = &state.model.panes[&pane].handle;
    let separator = match &c.separator {
        Some(s) => s.as_bytes(),
        None if c.raw => b"\n",
        None => b"\r",
    };
    let bracket = c.bracket && handle.modes().bracketed_paste;
    let bytes = buffers::paste_bytes(&b.data, separator, bracket, !c.unsanitized)?;
    handle
        .enqueue_input(bytes)
        .map_err(|e| format!("paste-buffer: {pane}: {e}"))?;
    if c.delete {
        state.buffers.delete(&name);
    }
    Ok(String::new())
}

/// `choose-buffer`: TC.9's chooser listing the buffers over the current
/// pane; Enter pastes the chosen one, `d` deletes it.
pub fn choose_buffer(
    state: &mut State,
    ctx: &ClientCtx,
    c: ChooseBuffer,
) -> Result<String, String> {
    tree::open_buffers(state, ctx, c.zoom, &c.opts, ctx.dialect)
}

#[cfg(test)]
mod tests {
    use nix::sys::stat::Mode;

    use super::*;
    use crate::server::test_state;

    fn fifo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fifo");
        nix::unistd::mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        (dir, path)
    }

    /// A FIFO nobody writes fails its request after the bound, and the
    /// thread waiting to open it is freed rather than left behind.
    #[tokio::test]
    async fn a_fifo_with_no_writer_times_out_and_frees_its_thread() {
        let (_dir, path) = fifo();
        let mut state = test_state();
        let (freed, was_freed) = std::sync::mpsc::channel();
        let stuck = path.clone();
        let io = move || {
            let read = read_file(&path);
            let _ = freed.send(());
            read
        };
        let out = off_actor(
            &mut state,
            Duration::from_millis(200),
            io,
            move || unstick_fifo(&stuck, true),
            |_, _| Ok(String::new()),
        );
        assert_eq!(out, Ok(String::new()));
        let answer = state.deferred_reply.take().expect("the answer comes later");
        let answer = tokio::time::timeout(Duration::from_secs(5), answer)
            .await
            .expect("answered within the bound")
            .unwrap();
        assert_eq!(answer.unwrap_err().err, "timed out after 200ms");
        tokio::task::spawn_blocking(move || was_freed.recv_timeout(Duration::from_secs(5)))
            .await
            .unwrap()
            .expect("the blocked open returned");
    }

    /// The thread may reach its open only after the bound has passed (a busy
    /// pool starts it late): unsticking still frees it, where one try on an
    /// FIFO with no reader yet left it blocked for good and the runtime
    /// waiting for it forever.
    #[tokio::test]
    async fn a_fifo_open_that_starts_after_the_bound_is_still_freed() {
        let (_dir, path) = fifo();
        let mut state = test_state();
        let (freed, was_freed) = std::sync::mpsc::channel();
        let stuck = path.clone();
        let io = move || {
            std::thread::sleep(Duration::from_millis(400));
            let read = read_file(&path);
            let _ = freed.send(());
            read
        };
        off_actor(
            &mut state,
            Duration::from_millis(100),
            io,
            move || unstick_fifo(&stuck, true),
            |_, _| Ok(String::new()),
        )
        .unwrap();
        let answer = state.deferred_reply.take().expect("the answer comes later");
        let answer = tokio::time::timeout(Duration::from_secs(5), answer)
            .await
            .expect("answered within the bound")
            .unwrap();
        assert_eq!(answer.unwrap_err().err, "timed out after 100ms");
        tokio::task::spawn_blocking(move || was_freed.recv_timeout(Duration::from_secs(8)))
            .await
            .unwrap()
            .expect("the late open was freed");
    }

    /// A reader that never reads stops a write at the bound.
    #[test]
    fn a_fifo_whose_reader_never_reads_times_out() {
        let (_dir, path) = fifo();
        let reader = OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK)
            .open(&path)
            .unwrap();
        let writer = OpenOptions::new()
            .write(true)
            .custom_flags(O_NONBLOCK)
            .open(&path)
            .unwrap();
        let data = vec![b'x'; 1 << 20];
        let started = Instant::now();
        let wrote = write_file(writer, &data, &path, Duration::from_millis(200));
        assert_eq!(wrote, Err("timed out after 200ms".to_string()));
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(reader);
    }
}
