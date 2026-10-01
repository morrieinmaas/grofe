//! The buffer commands (TM.4): `set-buffer`, `show-buffer`,
//! `list-buffers`, `delete-buffer`, `paste-buffer` and `choose-buffer`,
//! with tmux 3.7b's flags, output and error text (`cmd-set-buffer.c`,
//! `cmd-save-buffer.c`, `cmd-list-buffers.c`, `cmd-paste-buffer.c`).

use crate::cmd::{
    self, ChooseBuffer, DeleteBuffer, ListBuffers, PasteBuffer, Resolve, SetBuffer, ShowBuffer,
};
use crate::protocol::ClientCtx;
use crate::server::buffers::{self, BufferVars};
use crate::server::{State, dispatch};

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
    let bytes = buffers::paste_bytes(&b.data, separator, bracket)?;
    handle
        .enqueue_input(bytes)
        .map_err(|e| format!("paste-buffer: {pane}: {e}"))?;
    if c.delete {
        state.buffers.delete(&name);
    }
    Ok(String::new())
}

/// `choose-buffer`: needs TC.9's chooser (`tree.rs`) to list buffers,
/// which its public API does not offer yet (a rule 9 contract change).
pub fn choose_buffer(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: ChooseBuffer,
) -> Result<String, String> {
    dispatch::not_implemented("choose-buffer")
}
