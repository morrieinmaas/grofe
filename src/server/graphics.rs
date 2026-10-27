//! Kitty graphics through Unicode placeholders (spec §4.9, T3.8).
//!
//! A pane transmits an image with `a=t`/`a=T` and shows it by printing
//! U+10EEEE placeholder cells whose foreground colour is the image id
//! (virtual placement, `U=1`). grofe never draws images itself: it
//! forwards the commands to every client whose terminal has kitty
//! graphics, and the renderer draws the placeholder cells like any text.
//!
//! - **Ids.** Each pane's image ids are its own, so two panes may both use
//!   id 1. Every `(pane, id)` gets an outer id ([`Remap`]); forwarded
//!   commands carry it, and the renderer rewrites the placeholder cells to
//!   it ([`rewrite_row`]).
//! - **Media.** File, temp file and shared memory transmits are read here
//!   and forwarded as inline data: the outer terminal may be on another
//!   host, and temp files and shared memory are gone after one read.
//! - **Cache.** The last transmit per image (plus its virtual placements)
//!   is kept under [`CACHE_CAP`] and re-sent on attach and full redraw.
//! - **Queries** (`a=q`) are answered on the pane's reader task
//!   ([`answer_query`]), ahead of the emulator's DA1 reply.
//! - Direct placements (no `U=1`), animation and sixel are dropped, unless
//!   the `graphics` option is on (G3, `docs/graphics-placement.md`): then a
//!   direct transmit is forwarded as `a=t`, frames and animation control
//!   (`a=f`, `a=a`, `a=c`) are forwarded and cached after their image, and
//!   the placements themselves are the pane store's and the renderer's.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use alacritty_terminal::vte::ansi::{Color, Rgb};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_PAD_INDIFFERENT};
use nix::libc;

use crate::server::pane::SnapCell;
use crate::server::{State, clients};
use crate::types::{ClientId, PaneId};

/// The Unicode placeholder character.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// Most bytes of image commands kept for re-sending.
// ponytail: a constant; it stays below `clients::RAW_CAP` (16 MiB) so a
// full redraw's re-send fits a client's raw queue. An image bigger than
// this is dropped, not forwarded without caching.
pub const CACHE_CAP: usize = 12 * 1024 * 1024;

/// Base64 bytes per forwarded chunk (kitty's limit).
const CHUNK: usize = 4096;

/// Per-pane image id remap and the image data cache.
#[derive(Debug)]
pub struct Graphics {
    remap: Remap,
    /// Outer ids in use.
    taken: HashSet<u32>,
    /// The last outer id handed out.
    next: u32,
    /// Oldest first.
    cache: Vec<Image>,
    cached: usize,
    cap: usize,
    /// A chunked transmit (`m=1`) still arriving, per pane.
    pending: HashMap<PaneId, Pending>,
    /// Clients the cache was re-sent to.
    sent: HashSet<ClientId>,
    /// The `graphics` option is on (G3): set before each command.
    pub direct: bool,
}

impl Default for Graphics {
    fn default() -> Self {
        Graphics::with_cap(CACHE_CAP)
    }
}

/// Pane-local image ids to the ids clients see.
#[derive(Debug, Default)]
pub struct Remap {
    ids: HashMap<(PaneId, u32), u32>,
}

impl Remap {
    /// The outer id for `pane`'s image `local`.
    pub fn get(&self, pane: PaneId, local: u32) -> Option<u32> {
        self.ids.get(&(pane, local)).copied()
    }
}

#[derive(Debug)]
struct Image {
    id: u32,
    /// The transmit, as forwarded (all its chunks).
    data: Arc<[u8]>,
    /// Virtual placements (`a=p,U=1`) by placement id, as forwarded.
    placements: Vec<(u32, Vec<u8>)>,
    /// Animation frames and control commands, in order (G3).
    extras: Vec<Vec<u8>>,
}

impl Image {
    fn size(&self) -> usize {
        self.data.len()
            + self.placements.iter().map(|(_, p)| p.len()).sum::<usize>()
            + self.extras.iter().map(Vec::len).sum::<usize>()
    }
}

/// A transmit being put together. Its outer id is allocated only once it
/// is whole, so a transmit that fails leaks none.
#[derive(Debug)]
struct Pending {
    /// False for a transmit being dropped: its chunks are swallowed.
    forward: bool,
    /// Frame data (`a=f`): kept after its image, whose chunks say `a=f`.
    frame: bool,
    local: u32,
    q: u32,
    /// The first chunk's keys (`i=` set on `finish`) and payload.
    head: Vec<(u8, String)>,
    first: Vec<u8>,
    /// The later chunks, as forwarded.
    rest: Vec<u8>,
}

impl Pending {
    fn dropped() -> Pending {
        Pending {
            forward: false,
            frame: false,
            local: 0,
            q: 2,
            head: Vec::new(),
            first: Vec::new(),
            rest: Vec::new(),
        }
    }

    fn len(&self) -> usize {
        self.first.len() + self.rest.len()
    }
}

/// What one command from a pane leads to.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// For every client with kitty graphics, ahead of its next frame.
    pub forward: Vec<u8>,
    /// For the pane (kitty's `OK` or error reply).
    pub reply: Option<Vec<u8>>,
    /// The id map changed: placeholder cells render differently.
    pub remapped: bool,
}

impl Graphics {
    /// Empty, keeping at most `cap` bytes of image commands.
    pub fn with_cap(cap: usize) -> Graphics {
        Graphics {
            remap: Remap::default(),
            taken: HashSet::new(),
            next: 0,
            cache: Vec::new(),
            cached: 0,
            cap,
            pending: HashMap::new(),
            sent: HashSet::new(),
            direct: false,
        }
    }

    /// What the renderer needs to rewrite placeholder cells; `None` while
    /// no pane has an image.
    pub fn remap(&self) -> Option<&Remap> {
        (!self.remap.ids.is_empty()).then_some(&self.remap)
    }

    /// Handles one framed APC `_G` from `pane`.
    pub fn handle(&mut self, pane: PaneId, apc: &[u8]) -> Outcome {
        let mut out = Outcome::default();
        let Some(cmd) = Cmd::parse(apc) else {
            return out;
        };
        // Answered by the pane's reader (`answer_query`), never part of a
        // chunked transmit.
        if cmd.ch(b'a') == Some(b'q') {
            return out;
        }
        if let Some(p) = self.pending.remove(&pane) {
            self.continue_transmit(pane, p, &cmd, &mut out);
            return out;
        }
        match cmd.ch(b'a').unwrap_or(b't') {
            b't' | b'T' => self.transmit(pane, &cmd, &mut out),
            b'p' => self.place(pane, &cmd, &mut out),
            b'd' => self.delete(pane, &cmd, &mut out),
            b'f' if self.direct => self.transmit(pane, &cmd, &mut out),
            b'a' | b'c' if self.direct => self.animate(pane, &cmd, &mut out),
            _ => self.swallow(pane, &cmd),
        }
        out
    }

    /// Drops the rest of a chunked command that is not forwarded.
    fn swallow(&mut self, pane: PaneId, cmd: &Cmd) {
        if cmd.num(b'm') == Some(1) {
            self.pending.insert(pane, Pending::dropped());
        }
    }

    fn transmit(&mut self, pane: PaneId, cmd: &Cmd, out: &mut Outcome) {
        let virtual_placement = cmd.num(b'U') == Some(1);
        let action = cmd.ch(b'a').unwrap_or(b't');
        let frame = action == b'f';
        // ponytail: image numbers (`I=`) are not supported, only ids.
        let local = cmd.num(b'i').filter(|i| *i != 0);
        let Some(local) = local.filter(|_| frame || action == b't' || virtual_placement) else {
            self.swallow(pane, cmd);
            return;
        };
        let q = cmd.num(b'q').unwrap_or(0);
        if frame && self.remap.get(pane, local).is_none() {
            out.reply = reply(local, q, Err("ENOENT:no such image".into()));
            self.swallow(pane, cmd);
            return;
        }
        match cmd.ch(b't').unwrap_or(b'd') {
            b'd' => {
                let p = Pending {
                    forward: true,
                    frame,
                    local,
                    q,
                    head: cmd.rewritten(&[(b'i', String::new()), (b'q', "2".into())], &[]),
                    first: cmd.payload.to_vec(),
                    rest: Vec::new(),
                };
                if p.len() > self.cap {
                    out.reply = reply(local, q, Err("EFBIG:image too large".into()));
                    self.swallow(pane, cmd);
                    return;
                }
                if cmd.num(b'm') == Some(1) {
                    self.pending.insert(pane, p);
                } else {
                    self.finish(pane, p, out);
                }
            }
            medium @ (b'f' | b't' | b's') => {
                let data = read_medium(medium, cmd, self.cap);
                let data = match data {
                    Ok(data) => data,
                    Err(e) => {
                        out.reply = reply(local, q, Err(e));
                        return;
                    }
                };
                let keys = cmd.rewritten(&[(b'i', String::new()), (b'q', "2".into())], b"tSOm");
                let (head, first, rest) = inline_chunks(keys, &data);
                let p = Pending {
                    forward: true,
                    frame,
                    local,
                    q,
                    head,
                    first,
                    rest,
                };
                self.finish(pane, p, out);
            }
            _ => out.reply = reply(local, q, Err("EINVAL:unsupported medium".into())),
        }
    }

    fn continue_transmit(&mut self, pane: PaneId, mut p: Pending, cmd: &Cmd, out: &mut Outcome) {
        let more = cmd.num(b'm') == Some(1);
        if p.forward {
            let m = if more { "1" } else { "0" };
            let mut keys = vec![(b'm', m.to_string()), (b'q', "2".to_string())];
            if p.frame {
                keys.insert(0, (b'a', "f".to_string()));
            }
            p.rest.extend_from_slice(&encode(&keys, cmd.payload));
            if p.len() > self.cap {
                out.reply = reply(p.local, p.q, Err("EFBIG:image too large".into()));
                p = Pending::dropped();
            }
        }
        if more {
            self.pending.insert(pane, p);
        } else if p.forward {
            self.finish(pane, p, out);
        }
    }

    /// A whole transmit: forward it, cache it, answer the pane.
    fn finish(&mut self, pane: PaneId, mut p: Pending, out: &mut Outcome) {
        let outer = self.outer_id(pane, p.local, out);
        for (k, v) in &mut p.head {
            if *k == b'i' {
                *v = outer.to_string();
            }
        }
        let mut bytes = encode(&p.head, &p.first);
        bytes.extend_from_slice(&p.rest);
        out.forward.extend_from_slice(&bytes);
        if p.frame {
            self.keep_extra(outer, bytes);
            out.reply = reply(p.local, p.q, Ok(()));
            return;
        }
        self.uncache(outer);
        let image = Image {
            id: outer,
            data: bytes.into(),
            placements: Vec::new(),
            extras: Vec::new(),
        };
        self.cached += image.size();
        self.cache.push(image);
        self.evict();
        out.reply = reply(p.local, p.q, Ok(()));
    }

    /// Animation data or control for image `outer`: re-sent after it.
    fn keep_extra(&mut self, outer: u32, bytes: Vec<u8>) {
        if let Some(image) = self.cache.iter_mut().find(|i| i.id == outer) {
            self.cached += bytes.len();
            image.extras.push(bytes);
            self.evict();
        }
    }

    /// `a=a` (animation control) and `a=c` (frame composition).
    fn animate(&mut self, pane: PaneId, cmd: &Cmd, out: &mut Outcome) {
        let q = cmd.num(b'q').unwrap_or(0);
        let Some(local) = cmd.num(b'i').filter(|i| *i != 0) else {
            return;
        };
        let Some(outer) = self.remap.get(pane, local) else {
            out.reply = reply(local, q, Err("ENOENT:no such image".into()));
            return;
        };
        let keys = cmd.rewritten(&[(b'i', outer.to_string()), (b'q', "2".into())], &[]);
        let bytes = encode(&keys, b"");
        out.forward.extend_from_slice(&bytes);
        self.keep_extra(outer, bytes);
        out.reply = reply(local, q, Ok(()));
    }

    fn place(&mut self, pane: PaneId, cmd: &Cmd, out: &mut Outcome) {
        let q = cmd.num(b'q').unwrap_or(0);
        let Some(local) = cmd.num(b'i') else {
            return;
        };
        let Some(outer) = self.remap.get(pane, local) else {
            out.reply = reply(local, q, Err("ENOENT:no such image".into()));
            return;
        };
        if cmd.num(b'U') != Some(1) {
            // A direct placement is the pane store's and the renderer's.
            if self.direct {
                out.reply = reply(local, q, Ok(()));
            }
            return;
        }
        let keys = cmd.rewritten(&[(b'i', outer.to_string()), (b'q', "2".into())], &[]);
        let bytes = encode(&keys, b"");
        out.forward.extend_from_slice(&bytes);
        let placement = cmd.num(b'p').unwrap_or(0);
        if let Some(image) = self.cache.iter_mut().find(|i| i.id == outer) {
            let before = image.size();
            image.placements.retain(|(p, _)| *p != placement);
            image.placements.push((placement, bytes));
            self.cached = self.cached - before + image.size();
            self.evict();
        }
        out.reply = reply(local, q, Ok(()));
    }

    /// Deletes apply to the sending pane's images only: a screen-wide
    /// selector becomes one delete per image of that pane.
    fn delete(&mut self, pane: PaneId, cmd: &Cmd, out: &mut Outcome) {
        let d = cmd.ch(b'd').unwrap_or(b'a');
        let free = d.is_ascii_uppercase();
        let mine = |g: &Graphics| -> Vec<(u32, u32)> {
            let mut ids: Vec<(u32, u32)> = g
                .remap
                .ids
                .iter()
                .filter(|((p, _), _)| *p == pane)
                .map(|((_, local), outer)| (*local, *outer))
                .collect();
            ids.sort_unstable();
            ids
        };
        let mut placement = None;
        let targets: Vec<(u32, u32)> = match d.to_ascii_lowercase() {
            b'i' => {
                placement = cmd.num(b'p');
                cmd.num(b'i')
                    .and_then(|l| Some((l, self.remap.get(pane, l)?)))
                    .into_iter()
                    .collect()
            }
            b'r' => {
                let (lo, hi) = (cmd.num(b'x').unwrap_or(0), cmd.num(b'y').unwrap_or(0));
                mine(self)
                    .into_iter()
                    .filter(|(l, _)| (lo..=hi).contains(l))
                    .collect()
            }
            b'a' | b'c' | b'p' | b'q' | b'x' | b'y' | b'z' => mine(self),
            b'f' if self.direct => {
                if let Some(local) = cmd.num(b'i')
                    && let Some(outer) = self.remap.get(pane, local)
                {
                    let keys = cmd.rewritten(&[(b'i', outer.to_string()), (b'q', "2".into())], &[]);
                    let bytes = encode(&keys, b"");
                    out.forward.extend_from_slice(&bytes);
                    self.keep_extra(outer, bytes);
                }
                Vec::new()
            }
            // ponytail: `d=n` (image numbers) is dropped.
            _ => Vec::new(),
        };
        for (local, outer) in targets {
            let sel = if free { "I" } else { "i" };
            let mut keys = vec![
                (b'a', "d".to_string()),
                (b'd', sel.to_string()),
                (b'i', outer.to_string()),
            ];
            if let Some(p) = placement {
                keys.push((b'p', p.to_string()));
            }
            keys.push((b'q', "2".to_string()));
            out.forward.extend_from_slice(&encode(&keys, b""));
            if free {
                self.uncache(outer);
                self.remap.ids.remove(&(pane, local));
                self.taken.remove(&outer);
                out.remapped = true;
            } else if let Some(image) = self.cache.iter_mut().find(|i| i.id == outer) {
                let before = image.size();
                image
                    .placements
                    .retain(|(p, _)| placement.is_some_and(|want| *p != want));
                self.cached = self.cached - before + image.size();
            }
        }
    }

    /// The outer id for `pane`'s image `local`, allocating one (below
    /// 2^24 while any is free, so the high byte stays 0).
    fn outer_id(&mut self, pane: PaneId, local: u32, out: &mut Outcome) -> u32 {
        if let Some(outer) = self.remap.get(pane, local) {
            return outer;
        }
        let mut outer = self.next;
        loop {
            outer = if outer >= 0xff_ffff { 1 } else { outer + 1 };
            if !self.taken.contains(&outer) {
                break;
            }
            if outer == self.next {
                // ponytail: 2^24 live images; past that, ids above 2^24.
                outer = (0x100_0000..u32::MAX)
                    .find(|o| !self.taken.contains(o))
                    .unwrap_or(u32::MAX);
                break;
            }
        }
        if outer <= 0xff_ffff {
            self.next = outer;
        }
        self.taken.insert(outer);
        self.remap.ids.insert((pane, local), outer);
        out.remapped = true;
        outer
    }

    fn uncache(&mut self, outer: u32) {
        if let Some(i) = self.cache.iter().position(|i| i.id == outer) {
            let image = self.cache.remove(i);
            self.cached -= image.size();
        }
    }

    /// Drops the oldest images until the cache fits its cap. Their ids
    /// stay mapped: the outer terminal still has them.
    fn evict(&mut self) {
        while self.cached > self.cap && !self.cache.is_empty() {
            let image = self.cache.remove(0);
            self.cached -= image.size();
        }
    }

    /// Everything cached, oldest first, for a client that redraws from
    /// scratch.
    pub fn resend(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for image in &self.cache {
            out.extend_from_slice(&image.data);
            for extra in &image.extras {
                out.extend_from_slice(extra);
            }
            for (_, p) in &image.placements {
                out.extend_from_slice(p);
            }
        }
        out
    }

    /// Forgets the images of panes that are gone; returns the deletes to
    /// forward.
    pub fn prune(&mut self, live: impl Fn(PaneId) -> bool) -> Vec<u8> {
        self.pending.retain(|p, _| live(*p));
        let dead: Vec<((PaneId, u32), u32)> = self
            .remap
            .ids
            .iter()
            .filter(|((p, _), _)| !live(*p))
            .map(|(k, v)| (*k, *v))
            .collect();
        let mut out = Vec::new();
        for (key, outer) in dead {
            self.remap.ids.remove(&key);
            self.taken.remove(&outer);
            self.uncache(outer);
            out.extend_from_slice(format!("\x1b_Ga=d,d=I,i={outer},q=2\x1b\\").as_bytes());
        }
        out
    }
}

/// One framed APC `_G ... ESC \` from `pane` (T3.3's scanner).
pub fn on_graphics(state: &mut State, pane: PaneId, bytes: Vec<u8>) {
    prune(state);
    state.graphics.direct = images_enabled(&state.model);
    let out = state.graphics.handle(pane, &bytes);
    if let (Some(reply), Some(p)) = (out.reply, state.model.panes.get(&pane)) {
        let _ = p.handle.enqueue_input(reply);
    }
    to_kitty_clients(state, &out.forward);
    if out.remapped {
        clients::mark_all_dirty(state);
    }
}

fn prune(state: &mut State) {
    let panes = &state.model.panes;
    let deletes = state.graphics.prune(|p| panes.contains_key(&p));
    to_kitty_clients(state, &deletes);
}

/// Whether the `graphics` option is on (G3).
pub fn images_enabled(model: &crate::server::model::Model) -> bool {
    model.options.lookup("graphics", &[]) == Some("on")
}

/// The `graphics` option may have changed: tells every pane's reader.
pub fn sync_option(model: &crate::server::model::Model) {
    let on = images_enabled(model);
    for p in model.panes.values() {
        p.handle.set_images(on);
    }
}

/// The clients whose terminals show kitty graphics.
fn kitty_clients(state: &State) -> Vec<ClientId> {
    state
        .clients
        .iter()
        .filter(|(_, c)| c.caps.kitty_graphics)
        .map(|(id, _)| *id)
        .collect()
}

fn to_kitty_clients(state: &mut State, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    for id in kitty_clients(state) {
        clients::queue_raw(state, id, bytes);
    }
}

/// The answer to one framed APC `_G` from a pane, if it is a query (`a=q`)
/// that wants one, written straight back to the pane by its reader;
/// `graphics_ok` says whether an attached client shows kitty graphics.
/// Runs on the pane's reader task, never the actor. The query id is the
/// pane's own; nothing is stored.
pub fn answer_query(apc: &[u8], graphics_ok: &AtomicBool) -> Option<Vec<u8>> {
    let cmd = Cmd::parse(apc)?;
    if cmd.ch(b'a') != Some(b'q') {
        return None;
    }
    let id = cmd.num(b'i').filter(|i| *i != 0)?;
    let q = cmd.num(b'q').unwrap_or(0);
    let result = if !graphics_ok.load(Ordering::Relaxed) {
        Err("EINVAL:no attached terminal shows kitty graphics".to_string())
    } else if !matches!(cmd.num(b'f').unwrap_or(32), 24 | 32 | 100) {
        Err("EINVAL:unsupported format".to_string())
    } else {
        match cmd.ch(b't').unwrap_or(b'd') {
            b'd' => Ok(()),
            medium @ (b'f' | b't' | b's') => read_medium(medium, &cmd, CACHE_CAP).map(|_| ()),
            _ => Err("EINVAL:unsupported medium".to_string()),
        }
    };
    reply(id, q, result)
}

/// Whether a framed APC `_G` is a query (`a=q`), which only the pane's
/// reader answers.
pub fn is_query(apc: &[u8]) -> bool {
    Cmd::parse(apc).is_some_and(|c| c.ch(b'a') == Some(b'q'))
}

/// Kitty's reply to the pane for command `id`, as its `q` allows.
fn reply(id: u32, q: u32, result: Result<(), String>) -> Option<Vec<u8>> {
    let msg = match result {
        Ok(()) if q == 0 => "OK".to_string(),
        Err(e) if q < 2 => e,
        _ => return None,
    };
    Some(format!("\x1b_Gi={id};{msg}\x1b\\").into_bytes())
}

/// Whether a pane's graphics queries are answered: whether any attached
/// client shows kitty graphics. New panes start with it (`PaneSpawn`).
pub fn panes_graphics_ok(state: &State) -> bool {
    state.clients.values().any(|c| c.caps.kitty_graphics)
}

/// The set of attached clients (or their sessions) changed: attach,
/// detach, switch. Tells every pane's reader whether to answer graphics
/// queries (`PaneHandle::set_graphics_ok`), and forgets the images of
/// panes that are gone.
pub fn on_clients_changed(state: &mut State) {
    let ok = panes_graphics_ok(state);
    for p in state.model.panes.values() {
        p.handle.set_graphics_ok(ok);
    }
    let clients = &state.clients;
    state.graphics.sent.retain(|c| clients.contains_key(c));
    prune(state);
}

/// Client `id` is about to get a full redraw (attach, or after a dropped
/// frame): on its first, the cached images go out ahead of it. Later full
/// redraws send nothing: the terminal keeps virtual placements and their
/// data across the redraw's `ESC [2J` (kitty's `clear_filter_func` and
/// Ghostty's ED2 both skip them), and the images forwarded since went out
/// to this client as they came.
// ponytail: forwarded bytes that `clients::RAW_CAP` dropped for a client
// that stopped reading are not recovered until it attaches again.
pub fn on_full_redraw(state: &mut State, id: ClientId) {
    prune(state);
    if !state
        .clients
        .get(&id)
        .is_some_and(|c| c.caps.kitty_graphics)
    {
        return;
    }
    // ponytail: once per client, so a wipe on the terminal's side (Ghostty
    // Cmd+K, kitty's clear) loses the images until the client reattaches.
    if !state.graphics.sent.insert(id) {
        return;
    }
    let bytes = state.graphics.resend();
    if !bytes.is_empty() {
        clients::queue_raw(state, id, &bytes);
    }
}

// ---- commands ----

/// One parsed graphics command: its keys, in order, and its payload.
pub(crate) struct Cmd<'a> {
    keys: Vec<(u8, &'a [u8])>,
    pub(crate) payload: &'a [u8],
}

impl<'a> Cmd<'a> {
    pub(crate) fn parse(apc: &'a [u8]) -> Option<Cmd<'a>> {
        let body = apc.strip_prefix(b"\x1b_G")?.strip_suffix(b"\x1b\\")?;
        let (head, payload) = match body.iter().position(|b| *b == b';') {
            Some(i) => (&body[..i], &body[i + 1..]),
            None => (body, &b""[..]),
        };
        // Only what kitty's grammar allows ever reaches an outer APC: a
        // key is one letter, a value letters, digits, `_` and `-`, the
        // payload base64. Anything else (C0 bytes above all) drops the
        // command.
        let keys = head
            .split(|b| *b == b',')
            .filter(|kv| !kv.is_empty())
            .map(|kv| match kv {
                [k, b'=', v @ ..]
                    if k.is_ascii_alphabetic()
                        && v.iter()
                            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(b)) =>
                {
                    Some((*k, v))
                }
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        if !payload
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(b))
        {
            return None;
        }
        Some(Cmd { keys, payload })
    }

    pub(crate) fn get(&self, key: u8) -> Option<&'a [u8]> {
        self.keys.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }

    pub(crate) fn num(&self, key: u8) -> Option<u32> {
        std::str::from_utf8(self.get(key)?).ok()?.parse().ok()
    }

    /// A signed number key (`z`, `H`, `V`).
    pub(crate) fn int(&self, key: u8) -> Option<i64> {
        std::str::from_utf8(self.get(key)?).ok()?.parse().ok()
    }

    pub(crate) fn ch(&self, key: u8) -> Option<u8> {
        match self.get(key)? {
            [c] => Some(*c),
            _ => None,
        }
    }

    /// The keys with `set` replacing (or appended after) existing ones and
    /// `drop` removed.
    pub(crate) fn rewritten(&self, set: &[(u8, String)], drop: &[u8]) -> Vec<(u8, String)> {
        let mut keys: Vec<(u8, String)> = self
            .keys
            .iter()
            .filter(|(k, _)| !drop.contains(k))
            .map(|(k, v)| {
                let v = match set.iter().find(|(s, _)| s == k) {
                    Some((_, new)) => new.clone(),
                    None => String::from_utf8_lossy(v).into_owned(),
                };
                (*k, v)
            })
            .collect();
        for (k, v) in set {
            if !keys.iter().any(|(have, _)| have == k) {
                keys.push((*k, v.clone()));
            }
        }
        keys
    }
}

pub(crate) fn encode(keys: &[(u8, String)], payload: &[u8]) -> Vec<u8> {
    let mut out = b"\x1b_G".to_vec();
    for (i, (k, v)) in keys.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.push(*k);
        out.push(b'=');
        out.extend_from_slice(v.as_bytes());
    }
    if !payload.is_empty() {
        out.push(b';');
        out.extend_from_slice(payload);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// `data` as inline (`t=d`) chunks: the first chunk's keys (`keys`, plus
/// `m=1` when more follow) and payload, then the later chunks encoded.
fn inline_chunks(
    mut keys: Vec<(u8, String)>,
    data: &[u8],
) -> (Vec<(u8, String)>, Vec<u8>, Vec<u8>) {
    let b64 = STANDARD.encode(data);
    let chunks: Vec<&[u8]> = b64.as_bytes().chunks(CHUNK).collect();
    let Some((first, later)) = chunks.split_first() else {
        return (keys, Vec::new(), Vec::new());
    };
    if !later.is_empty() {
        keys.push((b'm', "1".to_string()));
    }
    let mut rest = Vec::new();
    for (i, chunk) in later.iter().enumerate() {
        let more = if i + 1 < later.len() { "1" } else { "0" };
        rest.extend_from_slice(&encode(&[(b'm', more.to_string())], chunk));
    }
    (keys, first.to_vec(), rest)
}

// ---- media ----

/// Reads a file (`t=f`), temp file (`t=t`) or shared memory (`t=s`)
/// transmit's data, as the server user, honouring `S=` and `O=`. A temp
/// file kitty would delete, and a shared memory object, are removed once
/// read. Errors are kitty's `ECODE:message`.
// ponytail: the read runs on the actor; a slow filesystem stalls it.
fn read_medium(medium: u8, cmd: &Cmd, cap: usize) -> Result<Vec<u8>, String> {
    let name = STANDARD_PAD_INDIFFERENT
        .decode(cmd.payload)
        .ok()
        .and_then(|n| String::from_utf8(n).ok())
        .ok_or_else(|| "EINVAL:bad file name".to_string())?;
    let size = cmd.num(b'S').unwrap_or(0) as u64;
    let offset = cmd.num(b'O').unwrap_or(0) as u64;
    if medium == b's' {
        return read_shm(&name, size, offset, cap);
    }
    let path = Path::new(&name);
    if !path.is_absolute() {
        return Err("EINVAL:path is not absolute".into());
    }
    // Kitty's rules: nothing under /proc, /sys or /dev (but /dev/shm), and a
    // `t=t` file only in a temp directory with the marker in its name.
    let real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if ["/proc", "/sys", "/dev"]
        .iter()
        .any(|d| real.starts_with(d))
        && !real.starts_with("/dev/shm")
    {
        return Err("EPERM:refusing to read from a system directory".into());
    }
    if medium == b't' && !kitty_would_delete(path) {
        return Err("EBADF:a t=t file must be a temporary file".into());
    }
    // Refused before opening, so that opening a device or fifo never
    // happens; `metadata` follows symlinks.
    let meta = fs::metadata(path).map_err(|e| format!("ENOENT:{e}"))?;
    if !meta.is_file() {
        return Err("EINVAL:not a regular file".into());
    }
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(|e| format!("ENOENT:{e}"))?;
    let meta = file.metadata().map_err(|e| format!("ENOENT:{e}"))?;
    if !meta.is_file() {
        return Err("EINVAL:not a regular file".into());
    }
    let want = span(meta.len(), size, offset, cap)?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("EINVAL:{e}"))?;
    let mut data = Vec::with_capacity(want);
    file.take(want as u64)
        .read_to_end(&mut data)
        .map_err(|e| format!("EINVAL:{e}"))?;
    if medium == b't' {
        let _ = fs::remove_file(path);
    }
    Ok(data)
}

/// How many bytes to read from a source of `len` bytes for `S=size`,
/// `O=offset`, refused when its base64 would not fit `cap`.
fn span(len: u64, size: u64, offset: u64, cap: usize) -> Result<usize, String> {
    let rest = len
        .checked_sub(offset)
        .ok_or_else(|| "EINVAL:offset past the end".to_string())?;
    let want = if size > 0 { size.min(rest) } else { rest };
    if want.div_ceil(3) * 4 > cap as u64 {
        return Err("EFBIG:image too large".into());
    }
    Ok(want as usize)
}

/// Kitty reads (and deletes) a `t=t` file only in a temp directory and with
/// `tty-graphics-protocol` in its path.
fn kitty_would_delete(path: &Path) -> bool {
    if !path.to_string_lossy().contains("tty-graphics-protocol") {
        return false;
    }
    let Some(dir) = path.parent().and_then(|d| d.canonicalize().ok()) else {
        return false;
    };
    let mut temps = vec![
        std::env::temp_dir(),
        PathBuf::from("/tmp"),
        PathBuf::from("/dev/shm"),
    ];
    if let Some(t) = std::env::var_os("TMPDIR") {
        temps.push(PathBuf::from(t));
    }
    temps
        .iter()
        .filter_map(|t| t.canonicalize().ok())
        .any(|t| dir.starts_with(t))
}

/// Reads and unlinks a POSIX shared memory object (it has to be mapped:
/// macOS cannot `read` one).
fn read_shm(name: &str, size: u64, offset: u64, cap: usize) -> Result<Vec<u8>, String> {
    let cname = CString::new(name).map_err(|_| "EINVAL:bad name".to_string())?;
    // SAFETY: a valid C string; the returned fd is owned below.
    let fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDONLY, 0) };
    if fd < 0 {
        return Err(format!("ENOENT:{}", std::io::Error::last_os_error()));
    }
    // SAFETY: shm_open returned a new fd that nothing else owns.
    let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let read = || -> Result<Vec<u8>, String> {
        let len = file.metadata().map_err(|e| format!("EINVAL:{e}"))?.len();
        let want = span(len, size, offset, cap)?;
        if want == 0 {
            return Ok(Vec::new());
        }
        let map_len = (offset as usize) + want;
        // SAFETY: mapping `map_len` bytes, within the object's size, of a
        // valid fd, read only; unmapped before returning.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(format!("EINVAL:{}", std::io::Error::last_os_error()));
        }
        // SAFETY: `ptr` maps `map_len` readable bytes.
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, map_len) };
        let data = bytes[offset as usize..].to_vec();
        // SAFETY: the mapping made above; `bytes` is not used after this.
        unsafe { libc::munmap(ptr, map_len) };
        Ok(data)
    };
    let data = read();
    // SAFETY: a valid C string.
    unsafe { libc::shm_unlink(cname.as_ptr()) };
    data
}

// ---- placeholder cells ----

/// Kitty's row and column diacritics: index `n` encodes the number `n`.
static DIACRITICS: [char; 297] = [
    '\u{0305}',
    '\u{030D}',
    '\u{030E}',
    '\u{0310}',
    '\u{0312}',
    '\u{033D}',
    '\u{033E}',
    '\u{033F}',
    '\u{0346}',
    '\u{034A}',
    '\u{034B}',
    '\u{034C}',
    '\u{0350}',
    '\u{0351}',
    '\u{0352}',
    '\u{0357}',
    '\u{035B}',
    '\u{0363}',
    '\u{0364}',
    '\u{0365}',
    '\u{0366}',
    '\u{0367}',
    '\u{0368}',
    '\u{0369}',
    '\u{036A}',
    '\u{036B}',
    '\u{036C}',
    '\u{036D}',
    '\u{036E}',
    '\u{036F}',
    '\u{0483}',
    '\u{0484}',
    '\u{0485}',
    '\u{0486}',
    '\u{0487}',
    '\u{0592}',
    '\u{0593}',
    '\u{0594}',
    '\u{0595}',
    '\u{0597}',
    '\u{0598}',
    '\u{0599}',
    '\u{059C}',
    '\u{059D}',
    '\u{059E}',
    '\u{059F}',
    '\u{05A0}',
    '\u{05A1}',
    '\u{05A8}',
    '\u{05A9}',
    '\u{05AB}',
    '\u{05AC}',
    '\u{05AF}',
    '\u{05C4}',
    '\u{0610}',
    '\u{0611}',
    '\u{0612}',
    '\u{0613}',
    '\u{0614}',
    '\u{0615}',
    '\u{0616}',
    '\u{0617}',
    '\u{0657}',
    '\u{0658}',
    '\u{0659}',
    '\u{065A}',
    '\u{065B}',
    '\u{065D}',
    '\u{065E}',
    '\u{06D6}',
    '\u{06D7}',
    '\u{06D8}',
    '\u{06D9}',
    '\u{06DA}',
    '\u{06DB}',
    '\u{06DC}',
    '\u{06DF}',
    '\u{06E0}',
    '\u{06E1}',
    '\u{06E2}',
    '\u{06E4}',
    '\u{06E7}',
    '\u{06E8}',
    '\u{06EB}',
    '\u{06EC}',
    '\u{0730}',
    '\u{0732}',
    '\u{0733}',
    '\u{0735}',
    '\u{0736}',
    '\u{073A}',
    '\u{073D}',
    '\u{073F}',
    '\u{0740}',
    '\u{0741}',
    '\u{0743}',
    '\u{0745}',
    '\u{0747}',
    '\u{0749}',
    '\u{074A}',
    '\u{07EB}',
    '\u{07EC}',
    '\u{07ED}',
    '\u{07EE}',
    '\u{07EF}',
    '\u{07F0}',
    '\u{07F1}',
    '\u{07F3}',
    '\u{0816}',
    '\u{0817}',
    '\u{0818}',
    '\u{0819}',
    '\u{081B}',
    '\u{081C}',
    '\u{081D}',
    '\u{081E}',
    '\u{081F}',
    '\u{0820}',
    '\u{0821}',
    '\u{0822}',
    '\u{0823}',
    '\u{0825}',
    '\u{0826}',
    '\u{0827}',
    '\u{0829}',
    '\u{082A}',
    '\u{082B}',
    '\u{082C}',
    '\u{082D}',
    '\u{0951}',
    '\u{0953}',
    '\u{0954}',
    '\u{0F82}',
    '\u{0F83}',
    '\u{0F86}',
    '\u{0F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

fn diacritic(c: char) -> Option<u32> {
    DIACRITICS.binary_search(&c).ok().map(|i| i as u32)
}

/// A placeholder cell with every value kitty's rules give it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Resolved {
    fg: Color,
    ul: Option<Color>,
    row: u32,
    col: u32,
    msb: u32,
}

impl Resolved {
    /// The image id, or `None` for a colour that names none.
    fn id(&self) -> Option<u32> {
        let low = match self.fg {
            Color::Spec(Rgb { r, g, b }) => (r as u32) << 16 | (g as u32) << 8 | b as u32,
            Color::Indexed(i) => i as u32,
            Color::Named(n) if (n as usize) < 16 => n as u32,
            Color::Named(_) => return None,
        };
        let id = low | self.msb << 24;
        (id != 0).then_some(id)
    }
}

/// How many leading diacritics `cell` gives (at most three) and their
/// values.
fn given(cell: &SnapCell) -> (usize, [u32; 3]) {
    let mut vals = [0; 3];
    let mut n = 0;
    for c in cell.zerowidth.iter().take(3) {
        let Some(v) = diacritic(*c) else { break };
        vals[n] = v;
        n += 1;
    }
    (n, vals)
}

/// Kitty's rules: values a cell omits come from the placeholder cell on
/// its left when that one has the same colours (and row, and column one
/// less, as far as given); otherwise they are 0.
fn resolve(cell: &SnapCell, left: Option<&Resolved>) -> Option<Resolved> {
    if cell.c != PLACEHOLDER {
        return None;
    }
    let (n, v) = given(cell);
    let (fg, ul) = (cell.fg, cell.underline_color);
    let left = left.filter(|l| l.fg == fg && l.ul == ul);
    let (row, col, msb) = match (n, left) {
        (3.., _) => (v[0], v[1], v[2]),
        (2, Some(l)) if l.row == v[0] && l.col + 1 == v[1] => (v[0], v[1], l.msb),
        (2, _) => (v[0], v[1], 0),
        (1, Some(l)) if l.row == v[0] => (v[0], l.col + 1, l.msb),
        (1, _) => (v[0], 0, 0),
        (_, Some(l)) => (l.row, l.col + 1, l.msb),
        (_, None) => (0, 0, 0),
    };
    Some(Resolved {
        fg,
        ul,
        row,
        col,
        msb,
    })
}

/// Rewrites `pane`'s placeholder cells in one row of its cells (left to
/// right, as the pane has them) to the outer ids: the id as a truecolor
/// foreground, the high byte's diacritic written, replaced or dropped, and
/// omitted diacritics kept omitted where the outer terminal infers the
/// same values. A cell naming an image grofe does not know is a blank.
pub fn rewrite_row(cells: &mut [SnapCell], pane: PaneId, remap: Option<&Remap>) {
    let mut src_left: Option<Resolved> = None;
    let mut out_left: Option<Resolved> = None;
    for cell in cells.iter_mut() {
        let src = resolve(cell, src_left.as_ref());
        src_left = src;
        let Some(src) = src else {
            out_left = None;
            continue;
        };
        let outer = src
            .id()
            .and_then(|local| remap.and_then(|r| r.get(pane, local)));
        let Some(outer) = outer else {
            cell.c = ' ';
            cell.zerowidth.clear();
            out_left = None;
            continue;
        };
        let want = Resolved {
            fg: Color::Spec(Rgb {
                r: (outer >> 16) as u8,
                g: (outer >> 8) as u8,
                b: outer as u8,
            }),
            msb: outer >> 24,
            ..src
        };
        let (n, _) = given(cell);
        cell.fg = want.fg;
        encode_cell(cell, n, want, out_left.as_ref());
        out_left = resolve(cell, out_left.as_ref());
    }
}

/// Writes `want`'s diacritics into `cell` (which gave `n`): as few as the
/// outer terminal needs, given the cell `left` of it, but no fewer than
/// the cell had (the high byte excepted).
fn encode_cell(cell: &mut SnapCell, n: usize, want: Resolved, left: Option<&Resolved>) {
    let rest: Vec<char> = cell.zerowidth[n..].to_vec();
    let vals = [want.row, want.col, want.msb];
    for m in n.min(2)..=3 {
        let Some(marks) = vals[..m]
            .iter()
            .map(|v| DIACRITICS.get(*v as usize).copied())
            .collect::<Option<Vec<char>>>()
        else {
            continue;
        };
        cell.zerowidth = marks.into_iter().chain(rest.iter().copied()).collect();
        if resolve(cell, left) == Some(want) {
            return;
        }
    }
}

/// The placeholder cell at `row[x]` with all its values explicit, for when
/// the cell on its left is not shown (a cropped viewport). `None` when it
/// is not a placeholder or already explicit.
pub fn detach_left(row: &[SnapCell], x: usize) -> Option<SnapCell> {
    let cell = row.get(x)?;
    if cell.c != PLACEHOLDER || given(cell).0 >= 3 {
        return None;
    }
    let mut left = None;
    for c in &row[..x] {
        left = resolve(c, left.as_ref());
    }
    let want = resolve(cell, left.as_ref())?;
    let mut out = cell.clone();
    let (n, _) = given(cell);
    out.zerowidth.drain(..n);
    let mut marks = Vec::new();
    for v in [want.row, want.col, want.msb] {
        marks.push(*DIACRITICS.get(v as usize)?);
    }
    out.zerowidth.splice(0..0, marks);
    Some(out)
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::Term;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::Processor;

    use super::*;
    use crate::protocol::Caps;
    use crate::server::pane::{CellFlags, RowsSnapshot};
    use crate::server::render::{self, FrameInputs, Screen};
    use crate::server::status::Style;
    use crate::types::Rect;

    const A: PaneId = PaneId(1);
    const B: PaneId = PaneId(2);

    fn apc(body: &str) -> Vec<u8> {
        format!("\x1b_G{body}\x1b\\").into_bytes()
    }

    fn text(b: &[u8]) -> String {
        String::from_utf8_lossy(b).into_owned()
    }

    /// Every APC in `bytes`, as its body.
    fn apcs(bytes: &[u8]) -> Vec<String> {
        text(bytes)
            .split("\x1b_G")
            .skip(1)
            .map(|s| s.trim_end_matches("\x1b\\").to_string())
            .collect()
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/graphics")
            .join(name)
    }

    /// Case 1: two panes that both transmit id 1 are forwarded with
    /// distinct ids, each with `q=2`.
    #[test]
    fn two_panes_with_id_1_get_distinct_outer_ids() {
        let mut g = Graphics::default();
        let a = g.handle(A, &apc("a=T,U=1,i=1,f=100,c=2,r=2;AAAA"));
        let b = g.handle(B, &apc("a=T,U=1,i=1,f=100,c=2,r=2;BBBB"));
        assert!(a.remapped && b.remapped);
        let (ia, ib) = (g.remap.get(A, 1), g.remap.get(B, 1));
        assert!(ia.is_some() && ib.is_some() && ia != ib, "{ia:?} {ib:?}");
        assert_eq!(
            apcs(&a.forward),
            [format!(
                "a=T,U=1,i={},f=100,c=2,r=2,q=2;AAAA",
                ia.unwrap_or(0)
            )]
        );
        assert_eq!(
            apcs(&b.forward),
            [format!(
                "a=T,U=1,i={},f=100,c=2,r=2,q=2;BBBB",
                ib.unwrap_or(0)
            )]
        );
        assert!(ia.unwrap_or(0) < 1 << 24, "the high byte stays 0");
        // q=0 by default: the pane gets kitty's OK under its own id.
        assert_eq!(a.reply.as_deref(), Some(&b"\x1b_Gi=1;OK\x1b\\"[..]));
        // The same pane and id again keeps its outer id.
        let again = g.handle(A, &apc("a=T,U=1,i=1,q=2;CCCC"));
        assert!(!again.remapped && again.reply.is_none());
        assert_eq!(g.remap.get(A, 1), ia);
    }

    /// Case 2: a cap of three small images, four sent: the first is gone,
    /// and a late attach gets the last three in transmit order.
    #[test]
    fn the_cache_evicts_oldest_first_at_its_cap() {
        let one = |i: u32| apc(&format!("a=T,U=1,i={i},q=2;DATA"));
        let size = Graphics::default().handle(A, &one(1)).forward.len();
        let mut g = Graphics::with_cap(3 * size);
        for i in 1..=4 {
            g.handle(A, &one(i));
        }
        let got = apcs(&g.resend());
        let outer = |i| g.remap.get(A, i).unwrap_or(0);
        let want: Vec<String> = (2..=4)
            .map(|i| format!("a=T,U=1,i={},q=2;DATA", outer(i)))
            .collect();
        assert_eq!(got, want);
        // Re-sending an image makes it the newest.
        let two = outer(2);
        g.handle(A, &one(2));
        let order: Vec<String> = apcs(&g.resend());
        assert!(order[2].contains(&format!("i={two},")), "{order:?}");
    }

    /// Case 4: direct placements are dropped, with or without data.
    #[test]
    fn direct_placements_are_dropped() {
        let mut g = Graphics::default();
        let t = g.handle(A, &apc("a=t,i=5,f=100,q=2;AAAA"));
        assert_eq!(apcs(&t.forward).len(), 1, "a plain transmit is kept");
        assert_eq!(g.handle(A, &apc("a=p,i=5,q=2")), Outcome::default());
        assert_eq!(
            g.handle(A, &apc("a=T,i=6,f=100,q=2;AAAA")),
            Outcome::default()
        );
        let p = g.handle(A, &apc("a=p,U=1,i=5,c=2,r=1,q=2"));
        assert_eq!(apcs(&p.forward).len(), 1, "a virtual placement is kept");
        // A chunked direct transmit is swallowed whole.
        g.handle(A, &apc("a=T,i=7,f=32,m=1;AAAA"));
        assert_eq!(g.handle(A, &apc("m=0;BBBB")), Outcome::default());
        assert!(g.remap.get(A, 7).is_none());
    }

    /// No `a=` key is `a=t` (snacks.nvim); a later `a=p,U=1` is cached
    /// with its image and re-sent after it.
    #[test]
    fn a_transmit_without_an_action_and_its_placement_are_cached() {
        let mut g = Graphics::default();
        g.handle(A, &apc("i=9,f=100,q=2;AAAA"));
        g.handle(A, &apc("a=p,U=1,i=9,p=3,c=4,r=2,q=2"));
        g.handle(A, &apc("a=p,U=1,i=9,p=3,c=5,r=2,q=2"));
        let o = g.remap.get(A, 9).unwrap_or(0);
        assert_eq!(
            apcs(&g.resend()),
            [
                format!("i={o},f=100,q=2;AAAA"),
                format!("a=p,U=1,i={o},p=3,c=5,r=2,q=2"),
            ]
        );
        // Deleting the placement keeps the data.
        let d = g.handle(A, &apc("a=d,d=i,i=9,p=3,q=2"));
        assert_eq!(apcs(&d.forward), [format!("a=d,d=i,i={o},p=3,q=2")]);
        assert_eq!(apcs(&g.resend()), [format!("i={o},f=100,q=2;AAAA")]);
    }

    /// Chunks are forwarded together once the last arrives, in order,
    /// each with `q=2`, and cached whole.
    #[test]
    fn chunked_transmits_are_kept_in_order() {
        let mut g = Graphics::default();
        let first = g.handle(A, &apc("a=T,U=1,q=2,f=32,s=1,v=1,i=1,m=1;AAAA"));
        assert!(first.forward.is_empty());
        assert!(g.handle(A, &apc("m=1;BBBB")).forward.is_empty());
        let last = g.handle(A, &apc("m=0"));
        let o = g.remap.get(A, 1).unwrap_or(0);
        let want = [
            format!("a=T,U=1,q=2,f=32,s=1,v=1,i={o},m=1;AAAA"),
            "m=1,q=2;BBBB".to_string(),
            "m=0,q=2".to_string(),
        ];
        assert_eq!(apcs(&last.forward), want);
        assert_eq!(apcs(&g.resend()), want);
    }

    /// Case 10: a delete-all from pane A deletes A's images only, one
    /// `d=I` each, and B's stays cached.
    #[test]
    fn delete_all_applies_to_the_sending_pane_only() {
        let mut g = Graphics::default();
        g.handle(A, &apc("a=T,U=1,i=1,q=2;AAAA"));
        g.handle(A, &apc("a=T,U=1,i=2,q=2;AAAA"));
        g.handle(B, &apc("a=T,U=1,i=1,q=2;BBBB"));
        let (a1, a2, b1) = (
            g.remap.get(A, 1).unwrap_or(0),
            g.remap.get(A, 2).unwrap_or(0),
            g.remap.get(B, 1).unwrap_or(0),
        );
        let out = g.handle(A, &apc("a=d,d=A"));
        assert_eq!(
            apcs(&out.forward),
            [format!("a=d,d=I,i={a1},q=2"), format!("a=d,d=I,i={a2},q=2")]
        );
        assert!(out.remapped);
        assert_eq!(apcs(&g.resend()), [format!("a=T,U=1,i={b1},q=2;BBBB")]);
        assert_eq!(g.remap.get(A, 1), None);
        assert_eq!(g.remap.get(B, 1), Some(b1));

        // Lowercase keeps the data.
        g.handle(A, &apc("a=T,U=1,i=3,q=2;AAAA"));
        let a3 = g.remap.get(A, 3).unwrap_or(0);
        let out = g.handle(A, &apc("a=d,d=a,q=2"));
        assert_eq!(apcs(&out.forward), [format!("a=d,d=i,i={a3},q=2")]);
        assert_eq!(apcs(&g.resend()).len(), 2);
    }

    /// Images of a pane that is gone are deleted and forgotten.
    #[test]
    fn a_gone_pane_loses_its_images() {
        let mut g = Graphics::default();
        g.handle(A, &apc("a=T,U=1,i=1,q=2;AAAA"));
        g.handle(B, &apc("a=T,U=1,i=1,q=2;BBBB"));
        let a1 = g.remap.get(A, 1).unwrap_or(0);
        let out = g.prune(|p| p == B);
        assert_eq!(apcs(&out), [format!("a=d,d=I,i={a1},q=2")]);
        assert_eq!(apcs(&g.resend()).len(), 1);
        assert!(g.prune(|p| p == B).is_empty());
    }

    /// Case 7: a `t=f` transmit is forwarded and cached as inline data.
    #[test]
    fn a_file_transmit_is_forwarded_as_inline_data() {
        let png = fixture("red-blue-4x4.png");
        let data = fs::read(&png).expect("fixture");
        let name = STANDARD.encode(png.to_string_lossy().as_bytes());
        let mut g = Graphics::default();
        let out = g.handle(
            A,
            &apc(&format!("a=T,U=1,t=f,f=100,i=4,c=2,r=2,q=2;{name}")),
        );
        let o = g.remap.get(A, 4).unwrap_or(0);
        assert_eq!(
            apcs(&out.forward),
            [format!(
                "a=T,U=1,f=100,i={o},c=2,r=2,q=2;{}",
                STANDARD.encode(&data)
            )]
        );
        assert_eq!(g.resend(), out.forward);
        assert!(png.exists(), "a t=f file is left alone");
    }

    /// Large media go out as 4096-byte chunks, `m=1` up to the last.
    #[test]
    fn large_media_are_chunked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.rgb");
        fs::write(&path, vec![7u8; 6000]).expect("write");
        let name = STANDARD.encode(path.to_string_lossy().as_bytes());
        let mut g = Graphics::default();
        let body = format!("a=t,t=f,f=24,s=50,v=40,S=4000,O=1000,i=2;{name}");
        let out = g.handle(A, &apc(&body));
        let got = apcs(&out.forward);
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0].starts_with("a=t,f=24,s=50,v=40,i=1,q=2,m=1;"));
        assert!(got[1].starts_with("m=0;"));
        let b64: String = got
            .iter()
            .map(|c| c.split(';').nth(1).unwrap_or(""))
            .collect();
        assert_eq!(STANDARD.decode(b64).ok(), Some(vec![7u8; 4000]));
        assert_eq!(got[0].split(';').nth(1).map(str::len), Some(CHUNK));
    }

    /// Case 7: directories, fifos, relative paths, missing files and files
    /// over the cap are refused with an error reply and nothing forwarded.
    #[test]
    fn bad_file_transmits_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fifo = dir.path().join("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).expect("mkfifo");
        let big = dir.path().join("big");
        fs::write(&big, vec![0u8; 3000]).expect("write");
        let cases = [
            (dir.path().to_path_buf(), "EINVAL:"),
            (fifo, "EINVAL:"),
            (PathBuf::from("relative/file.png"), "EINVAL:"),
            (dir.path().join("missing"), "ENOENT:"),
            (big, "EFBIG:"),
        ];
        for (path, code) in cases {
            let mut g = Graphics::with_cap(1024);
            let name = STANDARD.encode(path.to_string_lossy().as_bytes());
            let out = g.handle(A, &apc(&format!("a=T,U=1,t=f,i=3;{name}")));
            assert!(out.forward.is_empty(), "{path:?}");
            let reply = text(out.reply.as_deref().unwrap_or_default());
            assert!(
                reply.starts_with(&format!("\x1b_Gi=3;{code}")),
                "{path:?}: {reply:?}"
            );
            assert!(g.remap().is_none(), "{path:?}");
            // q=2 silences the error.
            let quiet = g.handle(A, &apc(&format!("a=T,U=1,t=f,i=3,q=2;{name}")));
            assert_eq!(quiet, Outcome::default());
        }
    }

    /// Case 7: a `t=t` file is read and unlinked only in a temp directory
    /// with `tty-graphics-protocol` in its path; any other is refused (as
    /// kitty does) and left alone.
    #[test]
    fn temp_file_transmits_are_unlinked_as_kitty_does() {
        let dir = tempfile::Builder::new()
            .prefix("grofe-t38")
            .tempdir_in(std::env::temp_dir())
            .expect("tempdir");
        let doomed = dir.path().join("tty-graphics-protocol-x.png");
        let kept = dir.path().join("other.png");
        for p in [&doomed, &kept] {
            fs::copy(fixture("red-blue-4x4.png"), p).expect("copy");
        }
        let mut g = Graphics::default();
        let name = STANDARD.encode(doomed.to_string_lossy().as_bytes());
        let out = g.handle(A, &apc(&format!("a=T,U=1,t=t,i=1,q=2;{name}")));
        assert_eq!(apcs(&out.forward).len(), 1, "{doomed:?}");
        assert!(!doomed.exists(), "the temp file was unlinked");

        let name = STANDARD.encode(kept.to_string_lossy().as_bytes());
        let out = g.handle(A, &apc(&format!("a=T,U=1,t=t,i=2;{name}")));
        assert!(out.forward.is_empty(), "no marker, no read");
        let reply = text(out.reply.as_deref().unwrap_or_default());
        assert!(reply.starts_with("\x1b_Gi=2;EBADF:"), "{reply:?}");
        assert!(kept.exists(), "a name without the marker is kept");
    }

    /// Security: a `t=t` transmit of a file outside the temp directories
    /// (the user's own secrets), even with the marker in its name, and any
    /// `t=f` of /proc, /sys or /dev, is refused and nothing is read.
    #[test]
    fn media_transmits_cannot_read_arbitrary_or_system_files() {
        let home = tempfile::Builder::new()
            .prefix("tty-graphics-protocol-")
            .tempdir_in(std::env::current_dir().expect("cwd"))
            .expect("tempdir in the working directory");
        let secret = home.path().join("tty-graphics-protocol-secret");
        fs::write(&secret, b"hunter2").expect("write");
        for (medium, path, code) in [
            ("t", secret.to_string_lossy().to_string(), "EBADF:"),
            ("f", "/dev/zero".to_string(), "EPERM:"),
            ("f", "/proc/self/environ".to_string(), "EPERM:"),
        ] {
            let mut g = Graphics::default();
            let name = STANDARD.encode(path.as_bytes());
            let out = g.handle(A, &apc(&format!("a=T,U=1,t={medium},i=5;{name}")));
            assert!(out.forward.is_empty(), "{path}");
            let reply = text(out.reply.as_deref().unwrap_or_default());
            assert!(
                reply.starts_with(&format!("\x1b_Gi=5;{code}")),
                "{path}: {reply:?}"
            );
        }
        assert!(secret.exists(), "a refused file is not unlinked");
    }

    /// Shared memory is read, forwarded inline and unlinked.
    #[test]
    fn shared_memory_transmits_are_read_and_unlinked() {
        let name = format!("/grofe-t38-{}", std::process::id());
        let cname = CString::new(name.clone()).expect("name");
        // SAFETY: a valid C string; the fd is owned right after.
        let fd = unsafe {
            libc::shm_open(
                cname.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_EXCL,
                0o600,
            )
        };
        assert!(fd >= 0, "shm_open: {}", std::io::Error::last_os_error());
        // SAFETY: shm_open returned a fresh fd.
        let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        file.set_len(12).expect("ftruncate");
        // SAFETY: 12 bytes of a valid shared fd, unmapped below.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                12,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        // SAFETY: the 12 bytes just mapped.
        unsafe {
            std::ptr::copy_nonoverlapping(b"0123456789ab".as_ptr(), ptr as *mut u8, 12);
            libc::munmap(ptr, 12);
        }
        let mut g = Graphics::default();
        let b64 = STANDARD.encode(name.as_bytes());
        let out = g.handle(
            A,
            &apc(&format!("a=T,U=1,t=s,f=24,s=1,v=1,S=3,O=2,i=1;{b64}")),
        );
        let o = g.remap.get(A, 1).unwrap_or(0);
        assert_eq!(
            apcs(&out.forward),
            [format!(
                "a=T,U=1,f=24,s=1,v=1,i={o},q=2;{}",
                STANDARD.encode(b"234")
            )]
        );
        // SAFETY: a valid C string.
        let again = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDONLY, 0) };
        assert!(again < 0, "the object was unlinked");
    }

    /// Case 8: queries are answered for supported media and formats while
    /// a client shows kitty graphics; `q` silences as kitty does; nothing
    /// that is not a query is answered.
    #[test]
    fn queries_are_answered_from_what_grofe_supports() {
        let yes = AtomicBool::new(true);
        let no = AtomicBool::new(false);
        let q = |body: &str, ok: &AtomicBool| answer_query(&apc(body), ok).map(|r| text(&r));
        let ok = Some("\x1b_Gi=31;OK\x1b\\".to_string());
        assert_eq!(q("i=31,s=1,v=1,a=q,t=d,f=24;AAAA", &yes), ok);
        assert_eq!(q("a=q,i=31,f=100;AAAA", &yes), ok);
        let err = q("i=31,s=1,v=1,a=q,t=d,f=24;AAAA", &no).unwrap_or_default();
        assert!(err.starts_with("\x1b_Gi=31;EINVAL:"), "{err:?}");
        assert!(q("a=q,i=31,f=8;AAAA", &yes).is_some_and(|r| r.contains("EINVAL")));
        assert_eq!(q("a=q,i=31,q=1;AAAA", &yes), None);
        assert!(q("a=q,i=31,q=1;AAAA", &no).is_some());
        assert_eq!(q("a=q,i=31,q=2;AAAA", &no), None);
        assert_eq!(q("a=T,i=31;AAAA", &yes), None);

        // File media: OK for a regular file, ENOENT for a missing one.
        let png = STANDARD.encode(fixture("red-blue-4x4.png").to_string_lossy().as_bytes());
        assert_eq!(
            q(&format!("a=q,i=2,t=f,f=100;{png}"), &yes),
            Some("\x1b_Gi=2;OK\x1b\\".to_string())
        );
        let gone = STANDARD.encode(b"/nonexistent/grofe-t38.png");
        let reply = q(&format!("a=q,i=2,t=f;{gone}"), &yes).unwrap_or_default();
        assert!(reply.starts_with("\x1b_Gi=2;ENOENT:"), "{reply:?}");
        // The actor ignores a query.
        let mut g = Graphics::default();
        assert_eq!(g.handle(A, &apc("i=31,a=q,t=d;AAAA")), Outcome::default());
    }

    /// Fix round 2: only kitty's grammar is forwarded. A key is one
    /// letter, a value `[0-9a-zA-Z_-]*`, the payload base64; a command
    /// with anything else (C0 bytes above all) is dropped whole.
    #[test]
    fn malformed_commands_are_dropped() {
        let yes = AtomicBool::new(true);
        for body in [
            "a=T,U=1,i=1,c=2\r,q=2;AAAA",
            "a=T,U=1,i=1,c=2\n,q=2;AAAA",
            "a=T,U=1,i=1,c=\x082,q=2;AAAA",
            "a=T,U=1,i=1,q=2;AA\rAA",
            "a=T,U=1,i=1,q=2;AA\x1bAA",
            "a=T,U=1,i=1,q=2;AA AA",
            "a=T,U=1,i=1,xy=2,q=2;AAAA",
            "a=T,U=1,i=1,1=2,q=2;AAAA",
            "a=T,U=1,i=1,c:2,q=2;AAAA",
            "a=T,U=1,i=1,c=2.5,q=2;AAAA",
        ] {
            let mut g = Graphics::default();
            assert_eq!(g.handle(A, &apc(body)), Outcome::default(), "{body:?}");
            assert!(g.remap().is_none(), "{body:?}");
            let query = body.replacen("a=T", "a=q", 1);
            assert_eq!(answer_query(&apc(&query), &yes), None, "{query:?}");
        }
        let mut g = Graphics::default();
        let ok = g.handle(A, &apc("a=T,U=1,i=1,c=2,X=-1,z=_3,q=2;AA+/AA=="));
        assert_eq!(apcs(&ok.forward).len(), 1, "a valid command still goes");
        // A bad chunk drops only itself; the transmit stays pending.
        let mut g = Graphics::default();
        g.handle(A, &apc("a=T,U=1,i=1,q=2,m=1;AAAA"));
        assert_eq!(g.handle(A, &apc("m=0;AA\nAA")), Outcome::default());
        assert!(g.pending.contains_key(&A));
    }

    /// Fix round 2: an outer id is allocated only for a transmit that
    /// completes; one that fails or never ends leaks none.
    #[test]
    fn failed_transmits_leak_no_outer_id() {
        let mut g = Graphics::with_cap(16);
        g.handle(A, &apc("a=T,U=1,i=1,q=2,m=1;AAAA"));
        assert!(g.remap().is_none(), "nothing while chunks arrive");
        let out = g.handle(A, &apc("m=1;BBBBBBBBBBBBBBBB"));
        assert!(out.forward.is_empty() && !out.remapped);
        g.handle(A, &apc("m=0;CCCC"));
        assert!(g.remap().is_none(), "a transmit over the cap");
        assert!(g.taken.is_empty());

        let mut g = Graphics::default();
        g.handle(A, &apc("a=T,U=1,i=1,q=2,m=1;AAAA"));
        g.prune(|p| p != A);
        assert!(g.remap().is_none() && g.taken.is_empty() && g.pending.is_empty());

        let mut g = Graphics::default();
        let name = STANDARD.encode(b"/nonexistent/grofe-t38.png");
        g.handle(A, &apc(&format!("a=T,U=1,t=f,i=1,q=2;{name}")));
        assert!(g.remap().is_none() && g.taken.is_empty());
    }

    /// Fix round 2: a query that arrives while a chunked transmit is
    /// pending is neither folded into it nor ends it.
    #[test]
    fn a_query_between_chunks_leaves_the_transmit_alone() {
        let mut g = Graphics::default();
        g.handle(A, &apc("a=T,U=1,i=1,q=2,m=1;AAAA"));
        assert_eq!(
            g.handle(A, &apc("a=q,i=31,t=d,f=24;AAAA")),
            Outcome::default()
        );
        let out = g.handle(A, &apc("m=0;BBBB"));
        let o = g.remap.get(A, 1).unwrap_or(0);
        assert_eq!(
            apcs(&out.forward),
            [
                format!("a=T,U=1,i={o},q=2,m=1;AAAA"),
                "m=0,q=2;BBBB".to_string()
            ]
        );
        assert!(is_query(&apc("a=q,i=31;AAAA")));
        assert!(!is_query(&apc("a=T,i=31;AAAA")));
    }

    /// Fix round 2: a placeholder's underline colour (the placement id)
    /// reaches a kitty terminal even when it lacks styled underlines.
    #[test]
    fn the_placement_id_is_kept_for_kitty_terminals() {
        let ph = PLACEHOLDER.to_string();
        let bytes = format!("\x1b[38;5;1;58:2::0:0:7m{ph}{}x", marks(&[0, 0]));
        let row = row_of(bytes.as_bytes());
        let remap = remap_of(&[(A, 1, 1)]);
        let screen = compose_two(row.clone(), row, &remap);
        let kitty = Caps {
            kitty_graphics: true,
            ..Caps::default()
        };
        let out = text(&render::diff(None, &screen, &kitty));
        assert!(out.contains(";58;2;0;0;7m"), "{out:?}");
        // Other cells keep the client's caps: no underline colour there.
        let x = out.find('x').unwrap_or(0);
        let sgr = out[..x].rfind("\x1b[").unwrap_or(0);
        assert!(!out[sgr..x].contains("58;"), "{out:?}");
        let plain = text(&render::diff(None, &screen, &Caps::default()));
        assert!(!plain.contains("58;"), "{plain:?}");
    }

    // ---- placeholder cells ----

    /// `bytes` printed on a fresh 10x1 alacritty terminal, as a pane row.
    fn row_of(bytes: &[u8]) -> Vec<SnapCell> {
        let mut term = Term::new(Config::default(), &TermSize::new(10, 1), VoidListener);
        Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new().advance(&mut term, bytes);
        let row = &term.grid()[Line(0)];
        (0..10)
            .map(|c| {
                let cell = &row[Column(c)];
                SnapCell {
                    c: cell.c,
                    fg: cell.fg,
                    bg: cell.bg,
                    flags: cell.flags,
                    zerowidth: cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default(),
                    underline_color: cell.underline_color(),
                    hyperlink: None,
                }
            })
            .collect()
    }

    fn marks(v: &[usize]) -> String {
        v.iter().map(|i| DIACRITICS[*i]).collect()
    }

    /// A pane with a 1-row, `cols`-wide placeholder image; returns the
    /// remap with `(pane, local)` mapped to `outer`.
    fn remap_of(pairs: &[(PaneId, u32, u32)]) -> Remap {
        let mut r = Remap::default();
        for (p, l, o) in pairs {
            r.ids.insert((*p, *l), *o);
        }
        r
    }

    fn rgb(id: u32) -> Color {
        Color::Spec(Rgb {
            r: (id >> 16) as u8,
            g: (id >> 8) as u8,
            b: id as u8,
        })
    }

    /// Case 9: ids as `38;5;N`, `38;2;R;G;B` and `38:2:R:G:B` all read as
    /// the same image and render with the outer id as truecolor.
    #[test]
    fn every_colour_form_is_rewritten() {
        let ph = PLACEHOLDER.to_string();
        for sgr in ["38;5;1", "38;2;0;0;1", "38:2:0:0:1"] {
            let bytes = format!("\x1b[{sgr}m{ph}{}{ph}{}", marks(&[0, 0]), marks(&[0, 1]));
            let mut row = row_of(bytes.as_bytes());
            assert_eq!(row[0].c, PLACEHOLDER, "{sgr}");
            rewrite_row(&mut row, A, Some(&remap_of(&[(A, 1, 0x0a0b0c)])));
            for (x, col) in [(0, 0), (1, 1)] {
                assert_eq!(row[x].fg, rgb(0x0a0b0c), "{sgr} cell {x}");
                assert_eq!(
                    row[x].zerowidth,
                    marks(&[0, col]).chars().collect::<Vec<_>>()
                );
            }
        }
    }

    /// Case 9: a high-byte diacritic (icat's random ids) is read, and
    /// replaced or dropped to match the outer id; the underline colour (the
    /// placement id) passes through.
    #[test]
    fn the_high_byte_diacritic_follows_the_outer_id() {
        let ph = PLACEHOLDER.to_string();
        // 0x862033e2: colour 32:51:226, high byte 0x86.
        let bytes = format!(
            "\x1b[38:2:32:51:226;58:2::0:0:7m{ph}{}{ph}{}{ph}",
            marks(&[0, 0, 0x86]),
            marks(&[0, 1, 0x86]),
        );
        let row = row_of(bytes.as_bytes());
        let ul = row[0].underline_color;
        assert!(ul.is_some());

        let mut low = row.clone();
        rewrite_row(&mut low, A, Some(&remap_of(&[(A, 0x862033e2, 5)])));
        for (x, cell) in low.iter().take(3).enumerate() {
            assert_eq!((cell.fg, cell.underline_color), (rgb(5), ul), "cell {x}");
        }
        assert_eq!(low[0].zerowidth, marks(&[0, 0]).chars().collect::<Vec<_>>());
        assert_eq!(low[1].zerowidth, marks(&[0, 1]).chars().collect::<Vec<_>>());
        assert!(low[2].zerowidth.is_empty(), "inherits, as before");

        let mut high = row.clone();
        rewrite_row(
            &mut high,
            A,
            Some(&remap_of(&[(A, 0x862033e2, 0x0100_0005)])),
        );
        assert_eq!(
            high[0].zerowidth,
            marks(&[0, 0, 1]).chars().collect::<Vec<_>>()
        );
        // Inherited from the left neighbour, so it can go.
        assert_eq!(
            high[1].zerowidth,
            marks(&[0, 1]).chars().collect::<Vec<_>>()
        );
        assert!(high[2].zerowidth.is_empty());

        // A low id gaining a high byte gets the diacritic written.
        let bytes = format!("\x1b[38;5;1m{ph}{}{ph}{}", marks(&[0, 0]), marks(&[0, 1]));
        let mut row = row_of(bytes.as_bytes());
        rewrite_row(&mut row, A, Some(&remap_of(&[(A, 1, 0x0200_0003)])));
        assert_eq!(
            row[0].zerowidth,
            marks(&[0, 0, 2]).chars().collect::<Vec<_>>()
        );
        // The second cell inherits the high byte from its left neighbour.
        assert_eq!(row[1].zerowidth, marks(&[0, 1]).chars().collect::<Vec<_>>());
    }

    /// Placeholders naming an unknown image, or none, are blanks.
    #[test]
    fn unknown_images_render_blank() {
        let ph = PLACEHOLDER.to_string();
        let bytes = format!("\x1b[38;5;9m{ph}{}x", marks(&[0, 0]));
        let mut row = row_of(bytes.as_bytes());
        rewrite_row(&mut row, A, None);
        assert_eq!((row[0].c, row[1].c), (' ', 'x'));
        assert!(row[0].zerowidth.is_empty());
    }

    fn compose_two(a: Vec<SnapCell>, b: Vec<SnapCell>, remap: &Remap) -> Screen {
        let ra = Rect {
            x: 0,
            y: 0,
            cols: 10,
            rows: 1,
        };
        let rb = Rect { x: 11, ..ra };
        let area = Rect { cols: 21, ..ra };
        render::compose(&FrameInputs {
            client: (21, 1),
            viewport: (0, 0),
            area,
            panes: vec![
                (
                    A,
                    ra,
                    RowsSnapshot {
                        rows: vec![a],
                        ..Default::default()
                    },
                ),
                (
                    B,
                    rb,
                    RowsSnapshot {
                        rows: vec![b],
                        ..Default::default()
                    },
                ),
            ],
            borders: Vec::new(),
            border_style: Style::default(),
            active_border_style: Style::default(),
            active: A,
            status: None,
            cursor: None,
            graphics: Some(remap),
            images: None,
            look: Default::default(),
        })
    }

    /// Case 1, rendered: each pane's placeholder cells carry its own outer
    /// id, as truecolor even for a terminal without it.
    #[test]
    fn each_pane_renders_its_own_outer_id() {
        let ph = PLACEHOLDER.to_string();
        let bytes = format!("\x1b[38;5;1m{ph}{}{ph}", marks(&[0, 0]));
        let row = row_of(bytes.as_bytes());
        let remap = remap_of(&[(A, 1, 1), (B, 1, 2)]);
        let screen = compose_two(row.clone(), row, &remap);
        assert_eq!(screen.cells[0].fg, rgb(1));
        assert_eq!(screen.cells[1].fg, rgb(1));
        assert_eq!(screen.cells[11].fg, rgb(2));
        assert_eq!(screen.cells[12].fg, rgb(2));
        let out = text(&render::diff(None, &screen, &Caps::default()));
        assert!(out.contains("38;2;0;0;1m"), "{out:?}");
        assert!(out.contains("38;2;0;0;2m"), "{out:?}");
        assert!(!out.contains("38;5"), "never downgraded: {out:?}");
    }

    /// A viewport that crops a placeholder's left neighbour spells out
    /// the values it inherited.
    #[test]
    fn a_cropped_placeholder_spells_out_its_values() {
        let ph = PLACEHOLDER.to_string();
        let bytes = format!("\x1b[38;5;1m{ph}{}{ph}{ph}", marks(&[3, 0]));
        let row = row_of(bytes.as_bytes());
        let area = Rect {
            x: 0,
            y: 0,
            cols: 10,
            rows: 1,
        };
        let remap = remap_of(&[(A, 1, 1)]);
        let screen = render::compose(&FrameInputs {
            client: (8, 1),
            viewport: (2, 0),
            area,
            panes: vec![(
                A,
                area,
                RowsSnapshot {
                    rows: vec![row],
                    ..Default::default()
                },
            )],
            borders: Vec::new(),
            border_style: Style::default(),
            active_border_style: Style::default(),
            active: A,
            status: None,
            cursor: None,
            graphics: Some(&remap),
            images: None,
            look: Default::default(),
        });
        assert_eq!(screen.cells[0].c, PLACEHOLDER);
        assert_eq!(
            screen.cells[0].zerowidth,
            marks(&[3, 2, 0]).chars().collect::<Vec<_>>()
        );
        assert_eq!(
            screen.cells[0].flags & CellFlags::WIDE_CHAR,
            CellFlags::empty()
        );
    }
}
