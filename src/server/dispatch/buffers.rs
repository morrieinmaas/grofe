//! The buffer commands (TM.4, TM.7): `set-buffer`, `show-buffer`,
//! `load-buffer`, `save-buffer`, `list-buffers`, `delete-buffer`,
//! `paste-buffer` and `choose-buffer`,
//! with tmux 3.7b's flags, output and error text (`cmd-set-buffer.c`,
//! `cmd-save-buffer.c`, `cmd-list-buffers.c`, `cmd-paste-buffer.c`).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::cmd::{
    self, ChooseBuffer, DeleteBuffer, ListBuffers, LoadBuffer, PasteBuffer, Resolve, SaveBuffer,
    SetBuffer, ShowBuffer,
};
use crate::protocol::ClientCtx;
use crate::server::buffers::{self, BufferVars};
use crate::server::{State, clients, passthrough, tree};

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

/// `load-buffer`: the file's data (`-`: the CLI's stdin) as a new
/// automatic buffer, or the `-b` one; empty data makes none, as in tmux.
/// `-w` also sends it to `-t`'s client, else the current one, if any.
pub fn load_buffer(state: &mut State, ctx: &ClientCtx, c: LoadBuffer) -> Result<String, String> {
    let data = if c.path == "-" {
        // ponytail: a binding has no CLI stdin, so it loads nothing; tmux
        // reads the server's own.
        state
            .cli_io
            .as_ref()
            .and_then(|io| io.stdin.clone())
            .unwrap_or_default()
    } else {
        let path = cli_path(ctx, &c.path);
        let mut data = Vec::new();
        File::open(&path)
            .and_then(|f| {
                f.take(buffers::MAX_BUFFER_BYTES as u64 + 1)
                    .read_to_end(&mut data)
            })
            .map_err(|e| file_error(&e, &path))?;
        data
    };
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
/// mode.
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
    let path = cli_path(ctx, &c.path);
    let mode = 0o644 & !state.cli_io.as_ref().and_then(|io| io.umask).unwrap_or(0);
    let new = std::fs::symlink_metadata(&path).is_err();
    let mut options = OpenOptions::new();
    options.create(true).mode(mode);
    if c.append {
        options.append(true);
    } else {
        options.write(true).truncate(true);
    }
    options
        .open(&path)
        .and_then(|mut f| {
            // The server's own umask must not narrow the CLI's.
            if new {
                f.set_permissions(std::fs::Permissions::from_mode(mode))?;
            }
            f.write_all(&b.data)
        })
        .map_err(|e| file_error(&e, &path))?;
    Ok(String::new())
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
    tree::open_buffers(state, ctx, c.zoom)
}
