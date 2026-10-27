//! The image placements of one pane (G3, `docs/graphics-placement.md`).
//!
//! Lives in the pane's emulator, mutated by the reader as it parses and
//! read by the pane's snapshots. Placements are anchored to absolute rows:
//! `v` is the absolute id of the viewport's first row, a placement is at
//! `v + line` for the cursor line it was made on, and a scroll adds to `v`
//! and to nothing else, so images follow their text up and out into the
//! scrollback.
//!
//! Two kinds are kept. Kitty placements (`a=T`/`a=p` without `U=1`) are
//! rectangles of cells over an image the actor holds; sixel images are the
//! parsed picture itself (tmux's model: they are freed by writes over them).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_PAD_INDIFFERENT};

use crate::server::graphics::{Cmd, encode};
use crate::server::sixel::{self, Sixel};

/// Most kitty placements kept per pane; the oldest go first.
pub const MAX_KITTY: usize = 512;
/// Most sixel images kept per pane (tmux keeps 19 for the whole server).
pub const MAX_SIXEL: usize = 19;
/// Most pixel bytes of sixel images kept per pane.
pub const MAX_SIXEL_BYTES: usize = 16 * 1024 * 1024;
/// Most image sizes remembered per pane.
const MAX_SIZES: usize = 1024;
/// Image ids the reader gives an `a=T` with no `i=`, so that the actor and
/// the store name the image alike.
pub const ANON_BASE: u32 = 0x8000_0000;
/// Most rows or columns a placement is moved the cursor by.
const MAX_MOVE: u32 = 1000;
/// A relative placement's chain is followed this deep.
const MAX_DEPTH: u32 = 8;

static SERIAL: AtomicU32 = AtomicU32::new(0);

/// A number no other placement of this process has: the outer terminal's
/// placement id, so two panes never share one.
fn next_serial() -> u32 {
    loop {
        let n = SERIAL.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if n != 0 {
            return n;
        }
    }
}

/// The cursor's screen cell, 0-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub line: u32,
    pub col: u32,
}

/// What `on_apc` says about the command.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ApcOut {
    /// Escape sequences to feed the emulator after the command: the cursor
    /// movement a placement causes.
    pub inject: Vec<u8>,
    /// What the actor gets.
    pub forward: Forward,
}

/// The command as the actor should see it.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum Forward {
    /// The command as it came.
    #[default]
    Same,
    /// These commands instead (none drops it).
    Replace(Vec<Vec<u8>>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Erase {
    /// `ED 2`: the screen.
    All,
    /// `ED 3`: the scrollback.
    Saved,
    /// `ED 0`.
    Below,
    /// `ED 1`.
    Above,
}

#[derive(Debug, Clone)]
enum Anchor {
    Abs {
        row: u64,
        col: u32,
    },
    /// `cols`/`rows` from the top-left cell of another placement.
    Rel {
        image: u32,
        pid: u32,
        dx: i64,
        dy: i64,
    },
}

#[derive(Debug, Clone)]
struct Placement {
    serial: u32,
    image: u32,
    /// The placement id the program gave (`p=`), 0 for none.
    user_pid: u32,
    anchor: Anchor,
    cols: u32,
    rows: u32,
    /// The source rectangle in image pixels: x, y, width, height.
    src: [u32; 4],
    /// `X`, `Y`: pixel offset inside the first cell.
    off: (u32, u32),
    z: i32,
}

#[derive(Debug, Clone)]
struct SixelImg {
    serial: u32,
    row: u64,
    col: u32,
    cells: (u32, u32),
    image: Arc<Sixel>,
}

#[derive(Debug, Default)]
struct Layer {
    /// The absolute id of the viewport's first row.
    v: u64,
    kitty: Vec<Placement>,
    sixel: Vec<SixelImg>,
}

impl Layer {
    fn is_empty(&self) -> bool {
        self.kitty.is_empty() && self.sixel.is_empty()
    }

    /// The absolute row and column of `p`'s top-left cell.
    fn pos(&self, p: &Placement, depth: u32) -> Option<(i64, i64)> {
        match p.anchor {
            Anchor::Abs { row, col } => Some((row as i64, i64::from(col))),
            Anchor::Rel { image, pid, dx, dy } => {
                if depth >= MAX_DEPTH {
                    return None;
                }
                let parent = self
                    .kitty
                    .iter()
                    .find(|q| q.image == image && q.user_pid == pid && q.serial != p.serial)?;
                let (row, col) = self.pos(parent, depth + 1)?;
                Some((row + dy, col + dx))
            }
        }
    }

    /// Drops relative placements whose parent is gone, until none is left.
    fn drop_orphans(&mut self) {
        loop {
            let before = self.kitty.len();
            let alive: Vec<(u32, u32)> = self.kitty.iter().map(|p| (p.image, p.user_pid)).collect();
            self.kitty.retain(|p| match p.anchor {
                Anchor::Abs { .. } => true,
                Anchor::Rel { image, pid, .. } => alive.contains(&(image, pid)),
            });
            if self.kitty.len() == before {
                return;
            }
        }
    }
}

/// What the parser looked like before an operation that may scroll.
#[derive(Debug, Clone, Copy)]
pub struct Probe {
    pub history: usize,
    pub line: u32,
    pub rows: u32,
    /// A character is waiting to wrap to the next line.
    pub needs_wrap: bool,
}

/// An operation that may scroll the viewport.
#[derive(Debug, Clone, Copy)]
pub enum ScrollOp {
    /// `LF`, `IND`, `NEL`.
    Feed,
    /// A character written at the end of a line.
    Wrap,
    /// `SU`.
    Up(usize),
    /// `DL`.
    Delete(usize),
    /// `ED 2`.
    Clear,
}

/// A transmit still arriving in chunks.
#[derive(Debug)]
struct Pending {
    id: u32,
    size: Option<(u32, u32)>,
    /// How to place it once whole (`a=T` without `U=1`).
    place: Option<PlaceKeys>,
}

/// The placement keys of a command.
#[derive(Debug, Clone, Default)]
struct PlaceKeys {
    c: Option<u32>,
    r: Option<u32>,
    x: Option<u32>,
    y: Option<u32>,
    w: Option<u32>,
    h: Option<u32>,
    xo: u32,
    yo: u32,
    z: i32,
    user_pid: u32,
    /// `P`, `Q`, `H`, `V`.
    parent: Option<(u32, u32, i64, i64)>,
    no_move: bool,
}

impl PlaceKeys {
    fn from(cmd: &Cmd) -> PlaceKeys {
        PlaceKeys {
            c: cmd.num(b'c').filter(|n| *n > 0),
            r: cmd.num(b'r').filter(|n| *n > 0),
            x: cmd.num(b'x'),
            y: cmd.num(b'y'),
            w: cmd.num(b'w').filter(|n| *n > 0),
            h: cmd.num(b'h').filter(|n| *n > 0),
            xo: cmd.num(b'X').unwrap_or(0),
            yo: cmd.num(b'Y').unwrap_or(0),
            z: cmd
                .int(b'z')
                .unwrap_or(0)
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            user_pid: cmd.num(b'p').unwrap_or(0),
            parent: cmd.num(b'P').filter(|n| *n > 0).map(|p| {
                (
                    p,
                    cmd.num(b'Q').unwrap_or(0),
                    cmd.int(b'H').unwrap_or(0),
                    cmd.int(b'V').unwrap_or(0),
                )
            }),
            no_move: cmd.num(b'C') == Some(1),
        }
    }
}

/// What a client's frame is built from: a pane's visible placements, in
/// pane cells (negative where they start above or left of the pane).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaneImages {
    pub kitty: Vec<ViewKitty>,
    pub sixel: Vec<ViewSixel>,
}

impl PaneImages {
    pub fn is_empty(&self) -> bool {
        self.kitty.is_empty() && self.sixel.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewKitty {
    pub serial: u32,
    /// The pane's own image id (the actor maps it to an outer one).
    pub image: u32,
    pub col: i32,
    pub row: i32,
    pub cols: u32,
    pub rows: u32,
    /// The source rectangle: x, y, width, height in image pixels.
    pub src: [u32; 4],
    pub off: (u32, u32),
    pub z: i32,
}

#[derive(Debug, Clone)]
pub struct ViewSixel {
    pub serial: u32,
    pub col: i32,
    pub row: i32,
    pub cells: (u32, u32),
    pub image: Arc<Sixel>,
    /// The cell size the pane measured it in.
    pub cell_px: (u32, u32),
}

impl PartialEq for ViewSixel {
    fn eq(&self, other: &Self) -> bool {
        self.serial == other.serial
            && (self.col, self.row, self.cells) == (other.col, other.row, other.cells)
            && Arc::ptr_eq(&self.image, &other.image)
    }
}

/// One pane's placements.
#[derive(Debug)]
pub struct Store {
    main: Layer,
    alt: Layer,
    in_alt: bool,
    cols: u32,
    rows: u32,
    history_limit: usize,
    /// Pixel sizes of the images the pane transmitted, oldest first.
    sizes: Vec<(u32, (u32, u32))>,
    pending: Option<Pending>,
    anon: u32,
    cell_px: (u32, u32),
    /// The scrolling region, `(top, bottom + 1)`.
    region: (u32, u32),
}

impl Store {
    pub fn new(cols: u32, rows: u32, history_limit: usize) -> Store {
        Store {
            main: Layer::default(),
            alt: Layer::default(),
            in_alt: false,
            cols,
            rows,
            history_limit,
            sizes: Vec::new(),
            pending: None,
            anon: 0,
            cell_px: sixel::DEFAULT_CELL_PX,
            region: (0, rows),
        }
    }

    fn layer(&self) -> &Layer {
        if self.in_alt { &self.alt } else { &self.main }
    }

    fn layer_mut(&mut self) -> &mut Layer {
        if self.in_alt {
            &mut self.alt
        } else {
            &mut self.main
        }
    }

    /// Nothing to track or draw (cheap checks first).
    pub fn is_empty(&self) -> bool {
        self.main.is_empty() && self.alt.is_empty()
    }

    /// The cell size sixel and cell counts are measured in; `None` is
    /// tmux's default.
    pub fn set_cell_px(&mut self, px: Option<(u16, u16)>) {
        self.cell_px = match px {
            Some((w, h)) if w > 0 && h > 0 => (u32::from(w), u32::from(h)),
            _ => sixel::DEFAULT_CELL_PX,
        };
    }

    /// Whether the history is full (or off), so that a scroll leaves its
    /// size as it was.
    pub fn history_full(&self, history: usize) -> bool {
        self.in_alt || history >= self.history_limit
    }

    // ---- terminal events ----

    /// Whether the screen holds a sixel image.
    pub fn has_sixel(&self) -> bool {
        !self.layer().sixel.is_empty()
    }

    /// `DECSTBM`, as the emulator takes it.
    pub fn set_region(&mut self, top: usize, bottom: Option<usize>) {
        let bottom = bottom.unwrap_or(self.rows as usize);
        if top >= bottom {
            return;
        }
        let start = (top as i64 - 1).max(0) as u32;
        self.region = (start.min(self.rows), bottom.min(self.rows as usize) as u32);
    }

    /// `RI` with the cursor on `line`: tmux frees every sixel image when it
    /// scrolls the region down.
    pub fn reverse_index(&mut self, line: u32) {
        if line == self.region.0 {
            self.free_sixels();
        }
    }

    /// An operation that may have scrolled the viewport ran; `before` is
    /// how things stood, `history` the scrollback size now.
    pub fn scroll_op(&mut self, op: ScrollOp, before: Probe, history: usize) {
        if self.region != (0, self.rows) {
            // Only a region that is the whole screen moves the viewport.
            let feed =
                matches!(op, ScrollOp::Feed | ScrollOp::Wrap) && before.line + 1 == self.region.1;
            if feed || matches!(op, ScrollOp::Up(_)) {
                self.check_line(self.region.0, self.region.1 - self.region.0);
            }
            return;
        }
        let rows = before.rows;
        let full = self.in_alt || history >= self.history_limit;
        let k = if !full {
            history.saturating_sub(before.history) as u32
        } else {
            match op {
                ScrollOp::Feed => u32::from(before.line + 1 == rows),
                ScrollOp::Wrap => u32::from(before.needs_wrap && before.line + 1 == rows),
                ScrollOp::Up(n) => (n.max(1) as u32).min(rows),
                ScrollOp::Delete(n) if before.line == 0 => (n.max(1) as u32).min(rows),
                ScrollOp::Delete(_) => 0,
                ScrollOp::Clear => rows,
            }
        };
        if k > 0 {
            self.scrolled(k);
        }
    }

    /// The viewport scrolled up `k` lines.
    pub fn scrolled(&mut self, k: u32) {
        self.layer_mut().v += u64::from(k);
    }

    /// Frees what has scrolled out of reach: kitty placements wholly above
    /// the scrollback, sixel images wholly above the screen.
    pub fn prune(&mut self, history: usize) {
        let layer = self.layer_mut();
        let floor = layer.v.saturating_sub(history as u64);
        layer.kitty.retain(|p| match p.anchor {
            Anchor::Abs { row, .. } => row + u64::from(p.rows) > floor,
            Anchor::Rel { .. } => true,
        });
        let v = layer.v;
        layer.sixel.retain(|s| s.row + u64::from(s.cells.1) > v);
        layer.drop_orphans();
    }

    /// The program entered (`true`) or left the alternate screen: that
    /// screen starts empty and is dropped when left.
    pub fn alt_changed(&mut self, now_alt: bool) {
        if now_alt == self.in_alt {
            return;
        }
        self.alt = Layer::default();
        self.in_alt = now_alt;
    }

    /// `RIS`: everything goes.
    pub fn reset(&mut self) {
        self.main = Layer::default();
        self.alt = Layer::default();
        self.in_alt = false;
        self.pending = None;
        self.region = (0, self.rows);
    }

    /// An erase of the display, called before the emulator does it.
    pub fn erase(&mut self, mode: Erase, cursor: Cursor) {
        let rows = self.rows;
        match mode {
            Erase::All => {
                let layer = self.layer_mut();
                let v = layer.v;
                let on_screen = |p: &Placement, layer: &Layer| {
                    layer.pos(p, 0).is_some_and(|(row, _)| {
                        let r = row - v as i64;
                        r + i64::from(p.rows) > 0 && r < i64::from(rows)
                    })
                };
                let gone: Vec<u32> = layer
                    .kitty
                    .iter()
                    .filter(|p| on_screen(p, layer))
                    .map(|p| p.serial)
                    .collect();
                layer.kitty.retain(|p| !gone.contains(&p.serial));
                layer.sixel.clear();
                layer.drop_orphans();
            }
            Erase::Saved => {
                let layer = self.layer_mut();
                let v = layer.v;
                layer.kitty.retain(|p| match p.anchor {
                    Anchor::Abs { row, .. } => row + u64::from(p.rows) > v,
                    Anchor::Rel { .. } => true,
                });
                layer.drop_orphans();
            }
            Erase::Below => self.check_line(cursor.line, rows.saturating_sub(cursor.line)),
            Erase::Above => self.check_line(0, cursor.line.saturating_sub(1)),
        }
    }

    /// tmux's `image_check_line`: a sixel overlapping rows `py..py+ny` is
    /// freed.
    pub fn check_line(&mut self, py: u32, ny: u32) {
        let layer = self.layer_mut();
        if layer.sixel.is_empty() {
            return;
        }
        let v = layer.v as i64;
        let (py, end) = (i64::from(py), i64::from(py) + i64::from(ny));
        layer.sixel.retain(|s| {
            let top = s.row as i64 - v;
            !(end > top && py < top + i64::from(s.cells.1))
        });
    }

    /// tmux's `image_check_area`: a sixel overlapping the cells is freed.
    pub fn check_area(&mut self, px: u32, py: u32, nx: u32, ny: u32) {
        let layer = self.layer_mut();
        if layer.sixel.is_empty() {
            return;
        }
        let v = layer.v as i64;
        let (px, py) = (i64::from(px), i64::from(py));
        let (nx, ny) = (i64::from(nx), i64::from(ny));
        layer.sixel.retain(|s| {
            let top = s.row as i64 - v;
            let hit = py < top + i64::from(s.cells.1)
                && py + ny > top
                && px < i64::from(s.col) + i64::from(s.cells.0)
                && px + nx > i64::from(s.col);
            !hit
        });
    }

    /// Frees every sixel image of the screen.
    pub fn free_sixels(&mut self) {
        self.layer_mut().sixel.clear();
    }

    /// The pane was resized. `history` is its size before and after.
    pub fn resized(&mut self, cols: u32, rows: u32, history: (usize, usize)) {
        let rows_changed = rows != self.rows;
        let full = self.history_full(history.0);
        self.cols = cols;
        self.rows = rows;
        self.region = (0, rows);
        self.alt = Layer::default();
        self.main.sixel.clear();
        if self.in_alt {
            return;
        }
        // Placements keep their cells through a reflow; what moved into or
        // out of the history moves the viewport. With the history full it
        // cannot be told how far, so they go.
        if rows_changed && full {
            self.main.kitty.clear();
            return;
        }
        let delta = history.1 as i64 - history.0 as i64;
        self.main.v = (self.main.v as i64 + delta).max(0) as u64;
    }

    // ---- views ----

    /// The visible placements, or `None` while there are none.
    pub fn view(&self) -> Option<PaneImages> {
        let layer = self.layer();
        if layer.is_empty() {
            return None;
        }
        let v = layer.v as i64;
        let rows = i64::from(self.rows);
        let mut out = PaneImages::default();
        for p in &layer.kitty {
            let Some((row, col)) = layer.pos(p, 0) else {
                continue;
            };
            let r = row - v;
            if r + i64::from(p.rows) <= 0 || r >= rows {
                continue;
            }
            out.kitty.push(ViewKitty {
                serial: p.serial,
                image: p.image,
                col: col as i32,
                row: r as i32,
                cols: p.cols,
                rows: p.rows,
                src: p.src,
                off: p.off,
                z: p.z,
            });
        }
        for s in &layer.sixel {
            let r = s.row as i64 - v;
            if r + i64::from(s.cells.1) <= 0 || r >= rows {
                continue;
            }
            out.sixel.push(ViewSixel {
                serial: s.serial,
                col: s.col as i32,
                row: r as i32,
                cells: s.cells,
                image: Arc::clone(&s.image),
                cell_px: self.cell_px,
            });
        }
        (!out.is_empty()).then_some(out)
    }

    // ---- graphics commands ----

    fn size_of(&self, id: u32) -> Option<(u32, u32)> {
        self.sizes.iter().find(|(i, _)| *i == id).map(|(_, s)| *s)
    }

    fn remember_size(&mut self, id: u32, size: (u32, u32)) {
        self.sizes.retain(|(i, _)| *i != id);
        self.sizes.push((id, size));
        if self.sizes.len() > MAX_SIZES {
            self.sizes.remove(0);
        }
    }

    /// One APC `_G` from the pane that is not a query. `cur` is the cursor
    /// where the command arrived.
    pub fn on_apc(&mut self, apc: &[u8], cur: Cursor) -> ApcOut {
        let Some(cmd) = Cmd::parse(apc) else {
            return ApcOut::default();
        };
        if cmd.ch(b'a') == Some(b'q') {
            return ApcOut::default();
        }
        if let Some(p) = self.pending.take() {
            if cmd.num(b'm') == Some(1) {
                self.pending = Some(p);
                return ApcOut::default();
            }
            return self.complete(p, cur);
        }
        match cmd.ch(b'a').unwrap_or(b't') {
            b't' | b'T' => self.transmit(&cmd, cur),
            b'p' => self.put_cmd(&cmd, cur),
            b'd' => self.delete(&cmd, cur),
            _ => ApcOut::default(),
        }
    }

    fn transmit(&mut self, cmd: &Cmd, cur: Cursor) -> ApcOut {
        let virtual_ = cmd.num(b'U') == Some(1);
        let direct = cmd.ch(b'a') == Some(b'T') && !virtual_;
        let mut id = cmd.num(b'i').filter(|i| *i != 0);
        if id.is_none() && direct && cmd.num(b'I').is_none() {
            self.anon += 1;
            id = Some(ANON_BASE + self.anon);
        }
        let Some(id) = id else {
            return ApcOut::default();
        };
        let size = image_size(cmd);
        // The outer terminal must not place a direct transmit by itself:
        // the renderer does, per client, clipped.
        let forward = if direct {
            let keys = cmd.rewritten(&[(b'a', "t".into()), (b'i', id.to_string())], &[]);
            Forward::Replace(vec![encode(&keys, cmd.payload)])
        } else {
            Forward::Same
        };
        let p = Pending {
            id,
            size,
            place: direct.then(|| PlaceKeys::from(cmd)),
        };
        if cmd.num(b'm') == Some(1) {
            self.pending = Some(p);
            return ApcOut {
                inject: Vec::new(),
                forward,
            };
        }
        let mut out = self.complete(p, cur);
        out.forward = forward;
        out
    }

    /// A transmit is whole: its size is known, old placements of the id
    /// are gone, and a direct one is placed.
    fn complete(&mut self, p: Pending, cur: Cursor) -> ApcOut {
        let layer = self.layer_mut();
        layer.kitty.retain(|q| q.image != p.id);
        layer.drop_orphans();
        let mut out = ApcOut::default();
        if let Some(size) = p.size {
            self.remember_size(p.id, size);
            if let Some(keys) = p.place {
                out.inject = self.put(p.id, size, &keys, cur);
            }
        }
        out
    }

    fn put_cmd(&mut self, cmd: &Cmd, cur: Cursor) -> ApcOut {
        if cmd.num(b'U') == Some(1) {
            return ApcOut::default();
        }
        let Some(id) = cmd.num(b'i').filter(|i| *i != 0) else {
            return ApcOut::default();
        };
        let Some(size) = self.size_of(id) else {
            return ApcOut::default();
        };
        let keys = PlaceKeys::from(cmd);
        ApcOut {
            inject: self.put(id, size, &keys, cur),
            forward: Forward::Same,
        }
    }

    /// Makes a placement of image `id`; returns the cursor movement.
    fn put(&mut self, id: u32, img_px: (u32, u32), keys: &PlaceKeys, cur: Cursor) -> Vec<u8> {
        let sx = keys.x.unwrap_or(0).min(img_px.0);
        let sy = keys.y.unwrap_or(0).min(img_px.1);
        let sw = keys.w.unwrap_or(u32::MAX).min(img_px.0 - sx);
        let sh = keys.h.unwrap_or(u32::MAX).min(img_px.1 - sy);
        if sw == 0 || sh == 0 {
            return Vec::new();
        }
        let (cw, ch) = self.cell_px;
        let (cols, rows) = cell_span(keys, (sw, sh), (cw, ch));
        let layer = self.layer_mut();
        let anchor = match keys.parent {
            Some((image, pid, dx, dy)) => {
                let found = layer
                    .kitty
                    .iter()
                    .any(|q| q.image == image && q.user_pid == pid && pid != 0);
                if !found || (image == id && pid == keys.user_pid) {
                    return Vec::new();
                }
                Anchor::Rel { image, pid, dx, dy }
            }
            None => Anchor::Abs {
                row: layer.v + u64::from(cur.line),
                col: cur.col,
            },
        };
        let relative = matches!(anchor, Anchor::Rel { .. });
        if keys.user_pid != 0 {
            layer
                .kitty
                .retain(|q| !(q.image == id && q.user_pid == keys.user_pid));
        }
        layer.kitty.push(Placement {
            serial: next_serial(),
            image: id,
            user_pid: keys.user_pid,
            anchor,
            cols,
            rows,
            src: [sx, sy, sw, sh],
            off: (
                keys.xo.min(cw.saturating_sub(1)),
                keys.yo.min(ch.saturating_sub(1)),
            ),
            z: keys.z,
        });
        if layer.kitty.len() > MAX_KITTY {
            layer.kitty.remove(0);
        }
        layer.drop_orphans();
        if relative || keys.no_move {
            return Vec::new();
        }
        let mut inject = Vec::new();
        for _ in 1..rows.min(MAX_MOVE) {
            inject.extend_from_slice(b"\x1bD");
        }
        inject.extend_from_slice(format!("\x1b[{}C", cols.min(MAX_MOVE)).as_bytes());
        inject
    }

    fn delete(&mut self, cmd: &Cmd, cur: Cursor) -> ApcOut {
        let d = cmd.ch(b'd').unwrap_or(b'a');
        let free = d.is_ascii_uppercase();
        let rows = i64::from(self.rows);
        let layer = self.layer_mut();
        let v = layer.v as i64;
        let cursor_cell = (i64::from(cur.col), i64::from(cur.line));
        let cell = |cmd: &Cmd| {
            (
                i64::from(cmd.num(b'x').unwrap_or(1)) - 1,
                i64::from(cmd.num(b'y').unwrap_or(1)) - 1,
            )
        };
        // Whether placement `p` covers screen cell (x, y).
        let covers = |layer: &Layer, p: &Placement, (x, y): (i64, i64)| {
            layer.pos(p, 0).is_some_and(|(row, col)| {
                let r = row - v;
                (col..col + i64::from(p.cols)).contains(&x)
                    && (r..r + i64::from(p.rows)).contains(&y)
            })
        };
        let visible = |layer: &Layer, p: &Placement| {
            layer.pos(p, 0).is_some_and(|(row, _)| {
                let r = row - v;
                r + i64::from(p.rows) > 0 && r < rows
            })
        };
        let lower = d.to_ascii_lowercase();
        let hit: Vec<u32> = match lower {
            b'a' => layer
                .kitty
                .iter()
                .filter(|p| visible(layer, p))
                .map(|p| p.serial)
                .collect(),
            b'i' => {
                let (id, pid) = (cmd.num(b'i'), cmd.num(b'p').filter(|p| *p != 0));
                layer
                    .kitty
                    .iter()
                    .filter(|p| Some(p.image) == id && pid.is_none_or(|w| p.user_pid == w))
                    .map(|p| p.serial)
                    .collect()
            }
            b'r' => {
                let (lo, hi) = (cmd.num(b'x').unwrap_or(0), cmd.num(b'y').unwrap_or(0));
                layer
                    .kitty
                    .iter()
                    .filter(|p| (lo..=hi).contains(&p.image))
                    .map(|p| p.serial)
                    .collect()
            }
            b'c' => layer
                .kitty
                .iter()
                .filter(|p| covers(layer, p, cursor_cell))
                .map(|p| p.serial)
                .collect(),
            b'p' => {
                let at = cell(cmd);
                layer
                    .kitty
                    .iter()
                    .filter(|p| covers(layer, p, at))
                    .map(|p| p.serial)
                    .collect()
            }
            b'q' => {
                let at = cell(cmd);
                let z = cmd.int(b'z').unwrap_or(0);
                layer
                    .kitty
                    .iter()
                    .filter(|p| i64::from(p.z) == z && covers(layer, p, at))
                    .map(|p| p.serial)
                    .collect()
            }
            b'x' => {
                let x = i64::from(cmd.num(b'x').unwrap_or(1)) - 1;
                layer
                    .kitty
                    .iter()
                    .filter(|p| {
                        visible(layer, p)
                            && layer
                                .pos(p, 0)
                                .is_some_and(|(_, col)| (col..col + i64::from(p.cols)).contains(&x))
                    })
                    .map(|p| p.serial)
                    .collect()
            }
            b'y' => {
                let y = i64::from(cmd.num(b'y').unwrap_or(1)) - 1;
                layer
                    .kitty
                    .iter()
                    .filter(|p| {
                        layer.pos(p, 0).is_some_and(|(row, _)| {
                            let r = row - v;
                            (r..r + i64::from(p.rows)).contains(&y)
                        })
                    })
                    .map(|p| p.serial)
                    .collect()
            }
            b'z' => {
                let z = cmd.int(b'z').unwrap_or(0);
                layer
                    .kitty
                    .iter()
                    .filter(|p| i64::from(p.z) == z)
                    .map(|p| p.serial)
                    .collect()
            }
            // `n`/`N` (image numbers) and `f`/`F` (frames) are the actor's.
            _ => return ApcOut::default(),
        };
        let touched: Vec<u32> = layer
            .kitty
            .iter()
            .filter(|p| hit.contains(&p.serial))
            .map(|p| p.image)
            .collect();
        layer.kitty.retain(|p| !hit.contains(&p.serial));
        layer.drop_orphans();
        // `i`, `r` and `a` the actor answers by itself, as it frees by id.
        if matches!(lower, b'a' | b'i' | b'r') {
            return ApcOut::default();
        }
        let mut freed = Vec::new();
        if free {
            for image in touched {
                let used = self.main.kitty.iter().any(|p| p.image == image)
                    || self.alt.kitty.iter().any(|p| p.image == image);
                if !used && !freed.contains(&image) {
                    freed.push(image);
                }
            }
        }
        for image in &freed {
            self.sizes.retain(|(i, _)| i != image);
        }
        ApcOut {
            inject: Vec::new(),
            forward: Forward::Replace(
                freed
                    .into_iter()
                    .map(|image| format!("\x1b_Ga=d,d=I,i={image},q=2\x1b\\").into_bytes())
                    .collect(),
            ),
        }
    }

    // ---- sixel ----

    /// A `DCS q` image whose parameters gave `p2`, from the pane, with the
    /// cursor where it began: stores it as tmux's `screen_write_sixelimage`
    /// does and returns the screen scrolling and cursor movement it causes.
    pub fn on_sixel(&mut self, body: &[u8], p2: u32, cur: Cursor) -> Vec<u8> {
        if self.rows <= 1 || self.cols == 0 {
            return Vec::new();
        }
        let (cw, ch) = self.cell_px;
        let Some(mut si) = Sixel::parse(body, p2, cw, ch) else {
            return Vec::new();
        };
        let (mut x, mut y) = si.size_in_cells();
        if x > self.cols || y > self.rows - 1 {
            let sx = if x > self.cols.saturating_sub(cur.col) {
                self.cols.saturating_sub(cur.col)
            } else {
                x
            };
            let sy = y.min(self.rows - 1);
            let Some(new) = si.scale((0, 0), (0, y - sy, sx, sy), true) else {
                return Vec::new();
            };
            si = new;
            (x, y) = si.size_in_cells();
        }
        let room = self.rows - cur.line;
        let mut inject = Vec::new();
        let mut scrolled = 0;
        if room <= y {
            scrolled = y - room + 1;
            inject.extend_from_slice(format!("\x1b[{};1H", self.rows).as_bytes());
            for _ in 0..scrolled {
                inject.extend_from_slice(b"\x1bD");
            }
        }
        let top = cur.line.saturating_sub(scrolled);
        let layer = self.layer_mut();
        layer.sixel.push(SixelImg {
            serial: next_serial(),
            row: layer.v + u64::from(scrolled) + u64::from(top),
            col: cur.col,
            cells: (x, y),
            image: Arc::new(si),
        });
        while layer.sixel.len() > MAX_SIXEL
            || layer.sixel.iter().map(|s| s.image.bytes()).sum::<usize>() > MAX_SIXEL_BYTES
        {
            layer.sixel.remove(0);
        }
        let last = (cur.line + y).min(self.rows - 1);
        inject.extend_from_slice(format!("\x1b[{};1H", last + 1).as_bytes());
        inject
    }
}

/// How many cells a placement covers.
fn cell_span(keys: &PlaceKeys, src: (u32, u32), cell: (u32, u32)) -> (u32, u32) {
    let (sw, sh) = (f64::from(src.0), f64::from(src.1));
    let (cw, ch) = (f64::from(cell.0), f64::from(cell.1));
    let (cols, rows) = match (keys.c, keys.r) {
        (Some(c), Some(r)) => (c, r),
        (Some(c), None) => (c, (f64::from(c) * cw * sh / sw / ch).ceil() as u32),
        (None, Some(r)) => ((f64::from(r) * ch * sw / sh / cw).ceil() as u32, r),
        (None, None) => (
            ((sw + f64::from(keys.xo)) / cw).ceil() as u32,
            ((sh + f64::from(keys.yo)) / ch).ceil() as u32,
        ),
    };
    (cols.max(1), rows.max(1))
}

/// The pixel size of the image a transmit command carries.
fn image_size(cmd: &Cmd) -> Option<(u32, u32)> {
    let keyed = || {
        let (s, v) = (cmd.num(b's')?, cmd.num(b'v')?);
        (s > 0 && v > 0).then_some((s, v))
    };
    if cmd.num(b'f') != Some(100) {
        return keyed();
    }
    let head = match cmd.ch(b't').unwrap_or(b'd') {
        b'd' => {
            let take = cmd.payload.len().min(44) / 4 * 4;
            STANDARD.decode(&cmd.payload[..take]).ok()?
        }
        b'f' | b't' => {
            let name = STANDARD_PAD_INDIFFERENT.decode(cmd.payload).ok()?;
            read_head(std::path::Path::new(std::str::from_utf8(&name).ok()?))?
        }
        _ => return keyed(),
    };
    png_size(&head).or_else(keyed)
}

/// The first bytes of a regular file, or `None`.
fn read_head(path: &std::path::Path) -> Option<Vec<u8>> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if !path.is_absolute() || !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOCTTY)
        .open(path)
        .ok()?;
    let mut head = Vec::new();
    file.take(24).read_to_end(&mut head).ok()?;
    Some(head)
}

/// The size in a PNG's header.
pub fn png_size(head: &[u8]) -> Option<(u32, u32)> {
    if head.len() < 24 || head[..8] != [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] {
        return None;
    }
    if &head[12..16] != b"IHDR" {
        return None;
    }
    let be = |i: usize| u32::from_be_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
    let (w, h) = (be(16), be(20));
    (w > 0 && h > 0).then_some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CUR: Cursor = Cursor { line: 2, col: 3 };

    fn store() -> Store {
        let mut s = Store::new(80, 24, 100);
        s.set_cell_px(Some((10, 20)));
        s
    }

    fn apc(body: &str) -> Vec<u8> {
        format!("\x1b_G{body}\x1b\\").into_bytes()
    }

    fn put(s: &mut Store, body: &str, cur: Cursor) -> ApcOut {
        s.on_apc(&apc(body), cur)
    }

    fn view(s: &Store) -> Vec<ViewKitty> {
        s.view().map(|v| v.kitty).unwrap_or_default()
    }

    /// A 40x40 pixel raw image: 4x2 cells at 10x20.
    const RAW: &str = "a=T,i=1,f=24,s=40,v=40;AAAA";

    #[test]
    fn a_direct_transmit_is_placed_at_the_cursor_and_moves_it() {
        let mut s = store();
        let out = put(&mut s, RAW, CUR);
        assert_eq!(out.inject, b"\x1bD\x1b[4C");
        assert_eq!(
            out.forward,
            Forward::Replace(vec![apc("a=t,i=1,f=24,s=40,v=40;AAAA")])
        );
        let v = view(&s);
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].col, v[0].row, v[0].cols, v[0].rows), (3, 2, 4, 2));
        assert_eq!(v[0].src, [0, 0, 40, 40]);
    }

    #[test]
    fn c1_does_not_move_the_cursor_and_c_r_scale() {
        let mut s = store();
        let out = put(&mut s, "a=T,i=1,f=24,s=40,v=40,C=1,c=10,r=3;AAAA", CUR);
        assert!(out.inject.is_empty());
        let v = view(&s);
        assert_eq!((v[0].cols, v[0].rows), (10, 3));
        // One dimension keeps the aspect: 5 columns of 10px is 50px wide, so
        // 50 tall, 2.5 rows of 20px.
        put(&mut s, "a=p,i=1,c=5,C=1", CUR);
        assert_eq!(view(&s)[1].rows, 3);
    }

    #[test]
    fn a_placement_follows_scrolling_and_leaves_with_the_scrollback() {
        let mut s = store();
        put(&mut s, "a=T,i=1,f=24,s=40,v=40,C=1;AAAA", CUR);
        s.scrolled(1);
        assert_eq!(view(&s)[0].row, 1);
        s.scrolled(1);
        assert_eq!(view(&s)[0].row, 0);
        // Rows 0 and 1 of the image are above the screen after 3 and 4.
        s.scrolled(1);
        assert_eq!(view(&s)[0].row, -1);
        s.scrolled(1);
        assert!(view(&s).is_empty(), "wholly above the screen");
        // Still in the scrollback until it is further than the history.
        s.prune(100);
        assert!(!s.is_empty());
        s.scrolled(200);
        s.prune(100);
        assert!(s.is_empty());
    }

    #[test]
    fn chunked_transmits_place_when_whole() {
        let mut s = store();
        let first = put(&mut s, "a=T,i=7,f=24,s=20,v=20,m=1;AAAA", CUR);
        assert!(first.inject.is_empty());
        assert!(matches!(first.forward, Forward::Replace(_)));
        assert!(view(&s).is_empty());
        put(&mut s, "m=1;BBBB", CUR);
        let last = put(&mut s, "m=0;CCCC", CUR);
        assert_eq!(last.forward, Forward::Same);
        assert_eq!(view(&s).len(), 1);
        assert_eq!(last.inject, b"\x1b[2C");
    }

    #[test]
    fn anonymous_images_get_an_id_and_virtual_ones_are_left_alone() {
        let mut s = store();
        let out = put(&mut s, "a=T,f=24,s=10,v=10;AAAA", CUR);
        let Forward::Replace(v) = out.forward else {
            panic!("rewritten");
        };
        let text = String::from_utf8_lossy(&v[0]).into_owned();
        assert!(text.contains(&format!("i={}", ANON_BASE + 1)), "{text}");
        let virt = put(&mut s, "a=T,U=1,i=3,f=24,s=10,v=10,c=2,r=2;AAAA", CUR);
        assert_eq!(virt, ApcOut::default());
        assert_eq!(view(&s).len(), 1);
    }

    #[test]
    fn placement_ids_replace_and_retransmits_clear() {
        let mut s = store();
        put(&mut s, "a=T,i=1,f=24,s=40,v=40,p=5,C=1;AAAA", CUR);
        put(
            &mut s,
            "a=p,i=1,p=5,C=1,c=2,r=1",
            Cursor { line: 9, col: 9 },
        );
        let v = view(&s);
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].col, v[0].row), (9, 9));
        // No id: another placement.
        put(&mut s, "a=p,i=1,C=1", CUR);
        assert_eq!(view(&s).len(), 2);
        // Transmitting the id again deletes its placements.
        put(&mut s, "a=t,i=1,f=24,s=40,v=40;AAAA", CUR);
        assert!(view(&s).is_empty());
    }

    #[test]
    fn relative_placements_follow_their_parent_and_die_with_it() {
        let mut s = store();
        put(&mut s, "a=T,i=1,f=24,s=20,v=20,p=1,C=1;AAAA", CUR);
        let out = put(
            &mut s,
            "a=T,i=2,f=24,s=20,v=20,p=1,P=1,Q=1,H=2,V=3;AAAA",
            Cursor { line: 0, col: 0 },
        );
        assert!(
            out.inject.is_empty(),
            "relative placements do not move the cursor"
        );
        let v = view(&s);
        assert_eq!((v[1].col, v[1].row), (5, 5));
        // The parent moves, the child with it.
        put(&mut s, "a=p,i=1,p=1,C=1", Cursor { line: 10, col: 10 });
        let v = view(&s);
        let child = v.iter().find(|p| p.image == 2).map(|p| (p.col, p.row));
        assert_eq!(child, Some((12, 13)));
        // A parent that does not exist: nothing placed.
        let none = put(&mut s, "a=p,i=2,p=9,P=4,Q=4,C=1", CUR);
        assert!(none.inject.is_empty());
        assert_eq!(view(&s).len(), 2);
        // Deleting the parent deletes the child.
        put(&mut s, "a=d,d=i,i=1", CUR);
        assert!(view(&s).is_empty());
    }

    /// Three images: one at (3,2) 4x2, one at (20,5) 2x1 z=-1, one at
    /// (3,10) 1x1.
    fn three() -> Store {
        let mut s = store();
        put(
            &mut s,
            "a=T,i=1,f=24,s=40,v=40,C=1;AAAA",
            Cursor { line: 2, col: 3 },
        );
        put(
            &mut s,
            "a=T,i=2,f=24,s=20,v=20,C=1,z=-1,c=2,r=1;AAAA",
            Cursor { line: 5, col: 20 },
        );
        put(
            &mut s,
            "a=T,i=3,f=24,s=10,v=10,C=1,c=1,r=1;AAAA",
            Cursor { line: 10, col: 3 },
        );
        assert_eq!(view(&s).len(), 3);
        s
    }

    fn ids(s: &Store) -> Vec<u32> {
        let mut v: Vec<u32> = view(s).iter().map(|p| p.image).collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn delete_by_id_and_range() {
        let mut s = three();
        assert_eq!(put(&mut s, "a=d,d=i,i=2", CUR).forward, Forward::Same);
        assert_eq!(ids(&s), [1, 3]);
        put(&mut s, "a=d,d=r,x=1,y=1", CUR);
        assert_eq!(ids(&s), [3]);
        put(&mut s, "a=d,d=a", CUR);
        assert!(ids(&s).is_empty());
    }

    #[test]
    fn delete_by_position() {
        // Cursor at (4, 3): inside image 1 only.
        let mut s = three();
        put(&mut s, "a=d,d=c", Cursor { line: 3, col: 4 });
        assert_eq!(ids(&s), [2, 3]);
        // A cell, 1-based: image 2 is at column 21, row 6.
        let mut s = three();
        put(&mut s, "a=d,d=p,x=21,y=6", CUR);
        assert_eq!(ids(&s), [1, 3]);
        // The same cell with a z that does not match deletes nothing.
        let mut s = three();
        put(&mut s, "a=d,d=q,x=21,y=6,z=5", CUR);
        assert_eq!(ids(&s).len(), 3);
        put(&mut s, "a=d,d=q,x=21,y=6,z=-1", CUR);
        assert_eq!(ids(&s), [1, 3]);
        // A column and a row.
        let mut s = three();
        put(&mut s, "a=d,d=x,x=5", CUR);
        assert_eq!(ids(&s), [2, 3]);
        let mut s = three();
        put(&mut s, "a=d,d=y,y=11", CUR);
        assert_eq!(ids(&s), [1, 2]);
        // A z-index.
        let mut s = three();
        put(&mut s, "a=d,d=z,z=-1", CUR);
        assert_eq!(ids(&s), [1, 3]);
    }

    #[test]
    fn uppercase_frees_images_left_without_placements() {
        let mut s = three();
        let out = put(&mut s, "a=d,d=P,x=21,y=6", CUR);
        assert_eq!(out.forward, Forward::Replace(vec![apc("a=d,d=I,i=2,q=2")]));
        // The lowercase form keeps the data: nothing for the actor.
        let out = put(&mut s, "a=d,d=p,x=4,y=3", CUR);
        assert_eq!(out.forward, Forward::Replace(vec![]));
        // A second placement keeps the image alive.
        let mut s = three();
        put(&mut s, "a=p,i=2,C=1", CUR);
        let out = put(&mut s, "a=d,d=Z,z=-1", CUR);
        assert_eq!(out.forward, Forward::Replace(vec![]), "{:?}", out.forward);
    }

    #[test]
    fn delete_all_leaves_what_scrolled_off() {
        let mut s = three();
        s.scrolled(8);
        // Image 1 and 2 are wholly above the screen now, image 3 is at row 2.
        put(&mut s, "a=d,d=a", CUR);
        assert!(view(&s).is_empty());
        // Image 1 was rows -6..-4: still in the store as scrollback.
        assert!(!s.is_empty());
    }

    #[test]
    fn erase_and_reset_and_alt_screen() {
        let mut s = three();
        s.erase(Erase::Saved, CUR);
        assert_eq!(ids(&s).len(), 3, "on-screen placements stay");
        s.erase(Erase::All, CUR);
        assert!(s.is_empty());
        let mut s = three();
        s.alt_changed(true);
        assert!(view(&s).is_empty(), "the alt screen starts empty");
        put(&mut s, "a=T,i=9,f=24,s=10,v=10,C=1;AAAA", CUR);
        assert_eq!(ids(&s), [9]);
        s.alt_changed(false);
        assert_eq!(ids(&s), [1, 2, 3], "and is dropped when left");
        s.reset();
        assert!(s.is_empty());
    }

    #[test]
    fn resize_rules() {
        let mut s = three();
        s.resized(80, 24, (0, 0));
        assert_eq!(ids(&s).len(), 3);
        // Fewer rows with room in the history: lines moved into it.
        s.resized(80, 20, (0, 3));
        assert_eq!(view(&s)[0].row, -1);
        // A width change keeps them where they are.
        s.resized(70, 20, (3, 3));
        assert_eq!(ids(&s).len(), 3);
        // Full history cannot tell how far things moved.
        let mut s = store();
        put(&mut s, RAW, CUR);
        s.resized(80, 20, (100, 100));
        assert!(s.is_empty());
    }

    #[test]
    fn png_headers_give_sizes() {
        let png = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/graphics/red-blue-4x4.png"),
        )
        .expect("fixture");
        assert_eq!(png_size(&png), Some((4, 4)));
        assert_eq!(png_size(&png[..20]), None);
        let b64 = STANDARD.encode(&png);
        let mut s = store();
        let out = put(&mut s, &format!("a=T,i=4,f=100,C=1;{b64}"), CUR);
        assert!(out.inject.is_empty());
        let v = view(&s);
        assert_eq!((v[0].cols, v[0].rows, v[0].src), (1, 1, [0, 0, 4, 4]));
    }

    // ---- sixel ----

    /// 20x40 pixels: 2x2 cells at 10x20, drawn with two colours.
    const SIXEL: &[u8] = b"q#0;2;100;0;0#0!20~-!20~-#0!20~-!20~-#0!20~-!20~";

    #[test]
    fn a_sixel_is_stored_at_the_cursor_and_moves_it_below() {
        let mut s = store();
        let inject = s.on_sixel(SIXEL, 0, CUR);
        let v = s.view().expect("stored");
        assert_eq!(
            (v.sixel[0].col, v.sixel[0].row, v.sixel[0].cells),
            (3, 2, (2, 2))
        );
        assert_eq!(inject, b"\x1b[5;1H", "column 0 of the row after the image");
    }

    #[test]
    fn a_sixel_that_does_not_fit_scrolls_the_screen() {
        let mut s = store();
        // Cursor on row 22 of 24: 2 rows of room, image is 2 rows: scroll 1.
        let inject = s.on_sixel(SIXEL, 0, Cursor { line: 22, col: 0 });
        assert_eq!(inject, b"\x1b[24;1H\x1bD\x1b[24;1H");
        // The reader feeds the scroll to the emulator, which counts it.
        s.scrolled(1);
        let v = s.view().expect("stored");
        assert_eq!(v.sixel[0].row, 21);
    }

    #[test]
    fn sixel_is_freed_as_tmux_frees_it() {
        let place = |s: &mut Store| {
            s.on_sixel(SIXEL, 0, Cursor { line: 5, col: 5 });
        };
        let mut s = store();
        place(&mut s);
        s.check_area(0, 5, 5, 1);
        assert!(!s.is_empty(), "left of it");
        s.check_area(6, 6, 1, 1);
        assert!(s.is_empty(), "text written over a cell of it");
        place(&mut s);
        s.check_line(7, 1);
        assert!(!s.is_empty(), "below it");
        s.check_line(6, 3);
        assert!(s.is_empty(), "a line operation over its rows");
        place(&mut s);
        s.erase(Erase::All, CUR);
        assert!(s.is_empty());
        place(&mut s);
        s.scrolled(7);
        s.prune(100);
        assert!(s.is_empty(), "scrolled off the top");
        // The alternate screen hides it and gives it back, as tmux's saved
        // images.
        place(&mut s);
        s.alt_changed(true);
        assert!(s.view().is_none());
        s.alt_changed(false);
        assert!(s.view().is_some());
    }

    #[test]
    fn a_sixel_too_big_is_cut_to_the_pane() {
        let mut s = Store::new(10, 10, 100);
        s.set_cell_px(Some((10, 20)));
        // 200x400 pixels: 20x20 cells, in a 10x10 pane.
        let mut body = b"q#0;2;1;1;1".to_vec();
        for _ in 0..67 {
            body.extend_from_slice(b"!200~-");
        }
        s.on_sixel(&body, 0, Cursor { line: 0, col: 0 });
        let v = s.view().expect("stored");
        assert_eq!(v.sixel[0].cells, (10, 9), "pane width, height less one");
    }

    #[test]
    fn at_most_nineteen_sixel_images() {
        let mut s = store();
        for i in 0..25 {
            s.on_sixel(SIXEL, 0, Cursor { line: 0, col: i });
        }
        assert_eq!(s.view().map(|v| v.sixel.len()), Some(MAX_SIXEL));
    }
}
