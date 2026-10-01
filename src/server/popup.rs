//! Popups (TC.6, spec §4.5 "Overlays"): `display-popup`, a command in its
//! own pty drawn over one client's frame ([`PopupOverlay`]) and taking that client's input while open ([`PopupHook`]). A popup is
//! not a pane in the model. Its pty's events reach the actor as
//! [`super::Job`]s.
//!
//! Everything here follows tmux 3.7b, recorded on a private server with a
//! pty client:
//!
//! - A popup is a box `-w` by `-h` cells, border included, placed as a menu
//!   is (`cmd_display_menu_get_pos`). Its command runs on a pty the size of
//!   the inside of the box. The title is a format, drawn in the top edge
//!   from the third column.
//! - Keys go to the command. Escape and C-c close the popup instead when it
//!   has no `-E`, or when its command has exited; without `-E` a popup
//!   stays open after its command exits.
//! - A second popup on a client that has one open does nothing.
//! - From the CLI, `display-popup` answers when the popup closes, with its
//!   command's exit status, or 129 when closing the popup hung it up.
//! - A client resize fits the box to the client and resizes the pty.
//!
//! The command's output goes through the same emulator and passthrough
//! scanner as a pane's, and only reaches the client as cells, so a popup
//! cannot write raw bytes to the terminal. Closing a popup hangs up its
//! process group; the pty is kept until the command has exited and been
//! reaped, so no zombie is left, and a group that outlives
//! [`KILL_GRACE`] is killed.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;
use unicode_width::UnicodeWidthChar;

use crate::protocol::Failure;
use crate::server::input::{self, Input, InputHook, Key, KeyCode, Routed};
use crate::server::pane::{
    CellFlags, CursorState, PaneEvent, PaneEventKind, PaneHandle, PaneSpawn, SnapCell,
};
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::server::status::{self, Style};
use crate::server::{Job, State, clients, passthrough};
use crate::types::{ClientId, Rect};

/// How long a closed popup's process group has to exit after its hangup
/// before it is killed.
pub const KILL_GRACE: Duration = Duration::from_secs(1);

/// The status a CLI gets for a popup closed while its command ran, which
/// the hangup ended: 128 + SIGHUP, as tmux 3.7b reports it.
const HUNG_UP: i32 = 129;

/// Open popups, per client (TC.6). Lives on `State.popups`.
#[derive(Default)]
pub struct Popups {
    open: BTreeMap<ClientId, Popup>,
    /// Popups closed while their command ran, by generation: kept until
    /// the command exits, so its pane reader reaps it.
    closing: BTreeMap<u64, Closing>,
    /// Counts popups opened, so events from an old one are ignored.
    generation: u64,
}

impl Popups {
    /// Whether client `id` has a popup open.
    pub fn is_open(&self, id: ClientId) -> bool {
        self.open.contains_key(&id)
    }
}

/// The border lines of a popup (tmux's `popup-border-lines`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lines {
    Single,
    Rounded,
    Double,
    Heavy,
    Simple,
    /// Blanks for a border.
    Padded,
}

impl Lines {
    /// tmux's value names; `none` is no border at all, `Ok(None)`.
    pub fn parse(value: &str) -> Result<Option<Lines>, String> {
        Ok(Some(match value {
            "single" => Lines::Single,
            "rounded" => Lines::Rounded,
            "double" => Lines::Double,
            "heavy" => Lines::Heavy,
            "simple" => Lines::Simple,
            "padded" => Lines::Padded,
            "none" => return Ok(None),
            _ => return Err(format!("popup-border-lines unknown value: {value}")),
        }))
    }

    /// Top-left, top-right, bottom-left, bottom-right, horizontal, vertical.
    fn chars(self) -> [char; 6] {
        match self {
            Lines::Single => ['┌', '┐', '└', '┘', '─', '│'],
            Lines::Rounded => ['╭', '╮', '╰', '╯', '─', '│'],
            Lines::Double => ['╔', '╗', '╚', '╝', '═', '║'],
            Lines::Heavy => ['┏', '┓', '┗', '┛', '━', '┃'],
            Lines::Simple => ['+', '+', '+', '+', '-', '|'],
            Lines::Padded => [' '; 6],
        }
    }
}

/// Where a popup goes on one axis, as tmux's `-x`/`-y` read it: for `x` a
/// number is the left column, for `y` the row just below the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Cells(i64),
    /// `C`: the centre of the client.
    Centre,
    /// `P`: for `x` the current pane's left edge, for `y` the box's bottom
    /// on the pane's.
    Pane,
    /// `R`, for `x`: the box's right edge on the current pane's.
    PaneRight,
}

/// The box's top-left cell for a `w` by `h` box on a `client`-sized
/// terminal, `pane` being the current pane's rect on the client, as tmux
/// 3.7b's `cmd_display_menu_get_pos` places a popup (recorded: `C`, `P`,
/// `R`, numbers, and numbers past the edge). The caller has checked that
/// the box fits.
pub fn place(
    (x, y): (Position, Position),
    (w, h): (u16, u16),
    client: (u16, u16),
    pane: Rect,
) -> (u16, u16) {
    let (cols, rows) = (i64::from(client.0), i64::from(client.1));
    let (w, h) = (i64::from(w), i64::from(h));
    let mut px = match x {
        Position::Cells(n) => n,
        Position::Centre => (cols - 1) / 2 - w / 2,
        Position::Pane => i64::from(pane.x),
        Position::PaneRight => i64::from(pane.x) + i64::from(pane.cols) - w,
    };
    if px.saturating_add(w) >= cols {
        px = cols - w;
    } else if px < 0 {
        px = 0;
    }
    let mut py = match y {
        Position::Cells(n) => n,
        Position::Centre => (rows - 1) / 2 + h / 2,
        Position::Pane | Position::PaneRight => i64::from(pane.y) + i64::from(pane.rows),
    };
    py = if py < h { 0 } else { py - h };
    if py.saturating_add(h) >= rows {
        py = rows - h;
    }
    let clamp = |n: i64| u16::try_from(n.max(0)).unwrap_or(0);
    (clamp(px), clamp(py))
}

/// What a popup looks like and how it behaves, as its command asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The box's top-left cell on the client, border included.
    pub at: (i32, i32),
    /// The box's size, border included.
    pub size: (u16, u16),
    /// `None`: no border, the command has the whole box.
    pub lines: Option<Lines>,
    /// Expanded; drawn in the top edge.
    pub title: Option<String>,
    pub border_style: Style,
    /// `-E`: close when the command exits.
    pub close_on_exit: bool,
}

impl Frame {
    /// The box as a `client`-sized terminal shows it, as tmux 3.7b's
    /// `popup_resize_cb` fits it (recorded, 80x24 to 40x12 and back): no
    /// larger than the client, moved left and up until it fits, and back
    /// to its own size and place once the client has room again.
    fn fitted(&self, (cols, rows): (u16, u16)) -> Frame {
        let (w, h) = (self.size.0.min(cols), self.size.1.min(rows));
        let fit = |at: i32, len: u16, room: u16| {
            if at + i32::from(len) > i32::from(room) {
                i32::from(room) - i32::from(len)
            } else {
                at
            }
        };
        Frame {
            at: (fit(self.at.0, w, cols), fit(self.at.1, h, rows)),
            size: (w, h),
            ..self.clone()
        }
    }

    /// The inside of the box: the command's pty.
    fn inside(&self) -> ((i32, i32), (u16, u16)) {
        let (w, h) = self.size;
        match self.lines {
            Some(_) => (
                (self.at.0 + 1, self.at.1 + 1),
                (w.saturating_sub(2), h.saturating_sub(2)),
            ),
            None => (self.at, self.size),
        }
    }
}

/// A popup's command.
struct Running {
    handle: PaneHandle,
    /// The forwarder task, reading the pty's events for the actor.
    forwarder: JoinHandle<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.forwarder.abort();
    }
}

impl Running {
    /// Hangs up the command's process group (its session leader's, since a
    /// pane's child starts its own session), as a closing terminal would.
    fn hang_up(&self) {
        self.signal(Signal::SIGHUP);
    }

    fn signal(&self, signal: Signal) {
        if let Some(pid) = self.handle.pid().and_then(|p| i32::try_from(p).ok()) {
            let _ = killpg(Pid::from_raw(pid), signal);
        }
    }
}

struct Popup {
    generation: u64,
    /// As it was asked for; drawn [`Frame::fitted`] to the client.
    frame: Frame,
    /// The pty's size now, so a client resize resizes it once.
    pty: (u16, u16),
    running: Running,
    /// The command's exit status once it has exited; after that the
    /// process is reaped, so it is never signalled again.
    exited: Option<Option<i32>>,
    /// The CLI reply of a `display-popup` waiting for this popup to close.
    waiting: Option<oneshot::Sender<Result<String, Failure>>>,
}

/// A popup closed while its command ran.
struct Closing {
    running: Running,
    waiting: Option<oneshot::Sender<Result<String, Failure>>>,
}

/// What a CLI waiting on a popup gets for exit status `status`.
fn answer(waiting: Option<oneshot::Sender<Result<String, Failure>>>, status: i32) {
    let Some(reply) = waiting else {
        return;
    };
    let result = match status {
        0 => Ok(String::new()),
        n => Err(Failure {
            err: String::new(),
            out: String::new(),
            status: Some(n),
        }),
    };
    let _ = reply.send(result);
}

/// Opens a popup on client `id` framed by `frame`, running `spawn` on a pty
/// the size of its inside, unless the client has one open already (tmux
/// then does nothing) or the box is too small for its border. Must run
/// within a tokio runtime. Returns whether it opened.
pub fn open(
    state: &mut State,
    id: ClientId,
    frame: Frame,
    mut spawn: PaneSpawn,
) -> Result<bool, String> {
    if state.popups.is_open(id) || !state.clients.contains_key(&id) {
        return Ok(false);
    }
    let (_, (cols, rows)) = frame.inside();
    if cols == 0 || rows == 0 {
        return Ok(false);
    }
    spawn.cols = cols;
    spawn.rows = rows;
    spawn.graphics_ok = false;
    let (events, events_rx) = mpsc::channel(64);
    let wake = Arc::new(Notify::new());
    let cwd = spawn.cwd.clone();
    let handle = PaneHandle::spawn(spawn, events, Arc::clone(&wake))
        .map_err(|e| format!("can't start popup in {}: {e}", cwd.display()))?;
    state.popups.generation += 1;
    let generation = state.popups.generation;
    let forwarder = tokio::spawn(forward(
        state.jobs.clone(),
        (id, generation),
        events_rx,
        wake,
    ));
    state.popups.open.insert(
        id,
        Popup {
            generation,
            frame,
            pty: (cols, rows),
            running: Running { handle, forwarder },
            exited: None,
            waiting: None,
        },
    );
    clients::mark_dirty(state, id);
    Ok(true)
}

/// A CLI caller (no `ctx.client`) of `display-popup` waits for client
/// `id`'s popup to close, and gets its command's exit status.
pub fn wait_for_close(state: &mut State, id: ClientId) {
    if !state.popups.is_open(id) {
        return;
    }
    let reply = crate::server::dispatch::defer_reply(state);
    if let Some(popup) = state.popups.open.get_mut(&id) {
        popup.waiting = Some(reply);
    }
}

/// Reads one popup's pty events and hands them to the actor: pane-output
/// taps are dropped (they are for panes), redraws are coalesced to one
/// pending job.
async fn forward(
    jobs: mpsc::Sender<Job>,
    (id, generation): (ClientId, u64),
    mut events: mpsc::Receiver<PaneEvent>,
    wake: Arc<Notify>,
) {
    let pending = Arc::new(AtomicBool::new(false));
    loop {
        let job: Job = tokio::select! {
            event = events.recv() => match event {
                None => return,
                Some(PaneEvent { kind: PaneEventKind::Output(_), .. }) => continue,
                Some(event) => Box::new(move |state: &mut State| {
                    on_event(state, id, generation, event.kind);
                }),
            },
            () = wake.notified() => {
                if pending.swap(true, Ordering::AcqRel) {
                    continue;
                }
                let pending = Arc::clone(&pending);
                Box::new(move |state: &mut State| {
                    pending.store(false, Ordering::Release);
                    if state.popups.open.get(&id).is_some_and(|p| p.generation == generation) {
                        clients::mark_dirty(state, id);
                    }
                })
            }
        };
        if jobs.send(job).await.is_err() {
            return;
        }
    }
}

/// One event from the pty of popup `generation`, shown on client `id`.
fn on_event(state: &mut State, id: ClientId, generation: u64, kind: PaneEventKind) {
    if let PaneEventKind::Exited(status) = kind {
        exited(state, id, generation, status);
        return;
    }
    if !state
        .popups
        .open
        .get(&id)
        .is_some_and(|p| p.generation == generation)
    {
        return;
    }
    // As a pane's, but to the popup's own client.
    // ponytail: titles, OSC 7 cwds and kitty graphics from a popup are
    // dropped; tmux keeps a popup out of all three too.
    match kind {
        PaneEventKind::Bell => clients::queue_raw(state, id, b"\x07"),
        PaneEventKind::Clipboard(text)
            if state.model.options.lookup("set-clipboard", &[]) == Some("on") =>
        {
            clients::queue_raw(state, id, &passthrough::osc52(&text));
        }
        PaneEventKind::Osc(body) if !body.starts_with(b"12;") && !body.starts_with(b"112") => {
            clients::queue_raw(state, id, &passthrough::osc(&body));
        }
        _ => {}
    }
}

/// Popup `generation`'s command exited with `status` (`None`: a signal).
fn exited(state: &mut State, id: ClientId, generation: u64, status: Option<i32>) {
    if let Some(closing) = state.popups.closing.remove(&generation) {
        answer(closing.waiting, status.unwrap_or(HUNG_UP));
        return;
    }
    let Some(popup) = state
        .popups
        .open
        .get_mut(&id)
        .filter(|p| p.generation == generation)
    else {
        return;
    };
    popup.exited = Some(status);
    if popup.frame.close_on_exit {
        close(state, id);
    } else {
        clients::mark_dirty(state, id);
    }
}

/// Closes client `id`'s popup. A command still running is hung up and kept
/// until it exits (see [`KILL_GRACE`]); the CLI waiting on the popup is
/// answered then, with the command's status.
pub fn close(state: &mut State, id: ClientId) {
    let Some(popup) = state.popups.open.remove(&id) else {
        return;
    };
    clients::mark_dirty(state, id);
    if let Some(status) = popup.exited {
        answer(popup.waiting, status.unwrap_or(HUNG_UP));
        return;
    }
    popup.running.hang_up();
    let generation = popup.generation;
    state.popups.closing.insert(
        generation,
        Closing {
            running: popup.running,
            waiting: popup.waiting,
        },
    );
    // Without a runtime (unit tests) there is no timer; the entry waits for
    // its exit event.
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        let jobs = state.jobs.clone();
        rt.spawn(async move {
            tokio::time::sleep(KILL_GRACE).await;
            let job: Job = Box::new(move |state: &mut State| {
                if let Some(closing) = state.popups.closing.get(&generation) {
                    closing.running.signal(Signal::SIGKILL);
                }
            });
            let _ = jobs.send(job).await;
        });
    }
}

/// The server is exiting: every popup closes, and each command gets
/// [`KILL_GRACE`] to go after its hangup before its group is killed. The
/// actor is stopping, so this waits here instead of on a timer job, and
/// reaps each command itself.
pub fn shutdown(state: &mut State) {
    use nix::sys::wait::{WaitPidFlag, waitpid};
    let ids: Vec<ClientId> = state.popups.open.keys().copied().collect();
    for id in ids {
        close(state, id);
    }
    let groups: Vec<Pid> = std::mem::take(&mut state.popups.closing)
        .into_values()
        .filter_map(|c| c.running.handle.pid().and_then(|p| i32::try_from(p).ok()))
        .map(Pid::from_raw)
        .collect();
    let deadline = Instant::now() + KILL_GRACE;
    let mut left = groups;
    while !left.is_empty() && Instant::now() < deadline {
        // The leader is reaped here (it is ours); the group is gone once
        // no process is left in it.
        left.retain(|pid| {
            let _ = waitpid(*pid, Some(WaitPidFlag::WNOHANG));
            killpg(*pid, None).is_ok()
        });
        std::thread::sleep(Duration::from_millis(20));
    }
    for pid in left {
        let _ = killpg(pid, Signal::SIGKILL);
        let _ = waitpid(pid, None);
    }
}

/// Clients were resized: each popup's pty takes the size of its box as
/// fitted to its client now ([`Frame::fitted`]), so its command gets the
/// SIGWINCH; the overlay then draws the fitted box.
pub fn clients_resized(state: &mut State) {
    let State {
        popups, clients, ..
    } = state;
    for (id, popup) in popups.open.iter_mut() {
        let Some(size) = clients.get(id).map(|c| c.size) else {
            continue;
        };
        let (_, inside) = popup.frame.fitted(size).inside();
        if inside != popup.pty && inside.0 > 0 && inside.1 > 0 {
            popup.pty = inside;
            let _ = popup.running.handle.resize(inside.0, inside.1);
        }
    }
}

/// Client `id` went away: its popup closes.
pub fn client_gone(state: &mut State, id: ClientId) {
    close(state, id);
}

/// Whether `key` is one that closes a popup (Escape, C-c).
fn closes(key: &Key) -> bool {
    let m = key.mods;
    match key.code {
        KeyCode::Escape => !m.ctrl && !m.alt,
        KeyCode::Char('c') => m.ctrl && !m.alt,
        _ => false,
    }
}

/// Takes a client's input while its popup is open: keys and pastes go to
/// the popup's command, encoded for its terminal modes; Escape and C-c
/// close a popup that has no `-E` or whose command has exited.
pub struct PopupHook;

impl InputHook for PopupHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        _now: Instant,
    ) -> Option<Routed> {
        let popup = state.popups.open.get(&client)?;
        let may_close = !popup.frame.close_on_exit || popup.exited.is_some();
        let running = popup.exited.is_none();
        let handle = &popup.running.handle;
        let bytes = match input {
            Input::Focus(_) => return None,
            Input::Key(key) if may_close && closes(key) => {
                close(state, client);
                return Some(Routed::Consumed);
            }
            Input::Key(key) => input::encode(key, &handle.modes()),
            Input::Paste { data, start, end } => {
                input::encode_paste(data, *start, *end, &handle.modes())
            }
            Input::Mouse(m) => mouse_bytes(state, client, &popup.frame, m, &handle.modes()),
        };
        if running && !bytes.is_empty() && handle.enqueue_input(bytes).is_err() {
            eprintln!("grofe server: input queue full for a popup");
        }
        Some(Routed::Consumed)
    }
}

/// A mouse report for a popup's command, as tmux 3.7b forwards one
/// (recorded): only with `mouse` on, only inside the box's inside, at the
/// cell there, and only when the command asked for that kind of report.
/// Anything else is eaten.
// ponytail: tmux also drags and resizes a popup by its border; grofe does
// not.
fn mouse_bytes(
    state: &State,
    client: ClientId,
    frame: &Frame,
    m: &input::Mouse,
    modes: &crate::types::InputModes,
) -> Vec<u8> {
    use crate::server::dispatch::options::Owner;
    let Some(session) = state.clients.get(&client).map(|c| c.session) else {
        return Vec::new();
    };
    if state
        .model
        .options
        .lookup("mouse", &[Owner::Session(session)])
        != Some("on")
    {
        return Vec::new();
    }
    let Some(size) = state.clients.get(&client).map(|c| c.size) else {
        return Vec::new();
    };
    let ((x, y), (cols, rows)) = frame.fitted(size).inside();
    let (dx, dy) = (i32::from(m.col) - x, i32::from(m.row) - y);
    let inside = (0..i32::from(cols)).contains(&dx) && (0..i32::from(rows)).contains(&dy);
    if !inside {
        return Vec::new();
    }
    crate::server::mouse::encode(m, dx as u16, dy as u16, modes).unwrap_or_default()
}

/// Draws a client's popup: its box, its title, the command's screen inside
/// it, and its cursor.
pub struct PopupOverlay;

impl Overlay for PopupOverlay {
    fn draw(&self, state: &State, client: ClientId, _: &OverlayCtx, screen: &mut Screen) {
        let Some(popup) = state.popups.open.get(&client) else {
            return;
        };
        let frame = &popup.frame.fitted((screen.cols, screen.rows));
        let ((x, y), (cols, rows)) = frame.inside();
        if cols == 0 || rows == 0 {
            return;
        }
        if let Some(lines) = frame.lines {
            draw_box(screen, frame, lines);
        }
        let snap = popup.running.handle.snapshot_rows(0..rows);
        for (r, row) in snap.rows.iter().enumerate() {
            for (c, cell) in row.iter().take(usize::from(cols)).enumerate() {
                set(screen, (x + c as i32, y + r as i32), cell.clone());
            }
        }
        let cursor = snap.cursor;
        let (cx, cy) = (x + i32::from(cursor.col), y + i32::from(cursor.row));
        let on_screen =
            (0..i32::from(screen.cols)).contains(&cx) && (0..i32::from(screen.rows)).contains(&cy);
        screen.cursor = (cursor.visible && on_screen).then_some(CursorState {
            col: cx as u16,
            row: cy as u16,
            ..cursor
        });
    }
}

/// Puts `cell` at `(x, y)` if that is on screen.
fn set(screen: &mut Screen, (x, y): (i32, i32), cell: SnapCell) {
    let (Ok(x), Ok(y)) = (u16::try_from(x), u16::try_from(y)) else {
        return;
    };
    if x < screen.cols && y < screen.rows {
        let at = usize::from(y) * usize::from(screen.cols) + usize::from(x);
        screen.cells[at] = cell;
    }
}

fn cell(c: char, style: &Style) -> SnapCell {
    SnapCell {
        c,
        fg: style.fg,
        bg: style.bg,
        flags: style.attrs,
        zerowidth: Vec::new(),
        underline_color: style.underline_color,
        hyperlink: None,
    }
}

/// Draws the box's border in its style, then the title in its top edge
/// from the third column, cut to the edge's inside less two cells, as
/// tmux's `screen_write_box` does.
fn draw_box(screen: &mut Screen, frame: &Frame, lines: Lines) {
    let [tl, tr, bl, br, horizontal, vertical] = lines.chars();
    let style = &frame.border_style;
    let (x, y) = frame.at;
    let (w, h) = (i32::from(frame.size.0), i32::from(frame.size.1));
    for i in 0..w {
        let (top, bottom) = match i {
            0 => (tl, bl),
            i if i + 1 == w => (tr, br),
            _ => (horizontal, horizontal),
        };
        set(screen, (x + i, y), cell(top, style));
        set(screen, (x + i, y + h - 1), cell(bottom, style));
    }
    for j in 1..h - 1 {
        set(screen, (x, y + j), cell(vertical, style));
        set(screen, (x + w - 1, y + j), cell(vertical, style));
    }
    let Some(title) = frame.title.as_deref() else {
        return;
    };
    let room = usize::try_from(w - 4).unwrap_or(0);
    let width = visible_width(title).min(room);
    if width == 0 {
        return;
    }
    let row = status::draw(title, *style, width as u16);
    let mut col = x + 2;
    for span in &row.line.spans {
        for c in span.text.chars() {
            let Some(cw) = UnicodeWidthChar::width(c).filter(|w| *w > 0) else {
                continue;
            };
            if col + cw as i32 > x + 2 + width as i32 {
                return;
            }
            let mut first = cell(c, &span.style);
            if cw == 2 {
                first.flags |= CellFlags::WIDE_CHAR;
                let mut spacer = cell(' ', &span.style);
                spacer.flags |= CellFlags::WIDE_CHAR_SPACER;
                set(screen, (col + 1, y), spacer);
            }
            set(screen, (col, y), first);
            col += cw as i32;
        }
    }
}

/// How many cells an expanded format takes once drawn: its `#[...]` style
/// directives take none, and `##` is one `#`.
fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if let Some(after) = rest.strip_prefix("#[")
            && let Some(end) = after.find(']')
        {
            rest = &after[end + 1..];
            continue;
        }
        if let Some(after) = rest.strip_prefix("##") {
            width += 1;
            rest = after;
            continue;
        }
        width += UnicodeWidthChar::width(c).unwrap_or(0);
        rest = &rest[c.len_utf8()..];
    }
    width
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT: (u16, u16) = (60, 15);

    fn full() -> Rect {
        Rect {
            x: 0,
            y: 0,
            cols: 60,
            rows: 14,
        }
    }

    /// Recorded on tmux 3.7b with a 60x15 client: `C` centres as a menu
    /// does, and `-w 80% -h 60%` (48x9) lands at (5, 2).
    #[test]
    fn centre_is_tmuxs() {
        let c = (Position::Centre, Position::Centre);
        assert_eq!(place(c, (48, 9), CLIENT, full()), (5, 2));
        assert_eq!(place(c, (30, 5), CLIENT, full()), (14, 4));
    }

    /// Recorded with the right pane of a `split-window -h` (x 31, 29 wide)
    /// current: `-x R -y P` puts the box's right edge on the pane's and its
    /// bottom on the pane's; `-x P -y 0` its left on the pane's, at the top.
    #[test]
    fn pane_letters_follow_the_current_pane() {
        let pane = Rect {
            x: 31,
            y: 0,
            cols: 29,
            rows: 14,
        };
        let rp = (Position::PaneRight, Position::Pane);
        assert_eq!(place(rp, (20, 5), CLIENT, pane), (40, 9));
        let p0 = (Position::Pane, Position::Cells(0));
        assert_eq!(place(p0, (20, 5), CLIENT, pane), (31, 0));
    }

    /// Recorded: numbers past the edge come back inside, and `-y` is the
    /// row below the box.
    #[test]
    fn numbers_are_clamped_inside_the_client() {
        let big = (Position::Cells(100), Position::Cells(100));
        assert_eq!(place(big, (20, 5), CLIENT, full()), (40, 10));
        let small = (Position::Cells(3), Position::Cells(8));
        assert_eq!(place(small, (20, 5), CLIENT, full()), (3, 3));
        // The largest number still lands inside, with no overflow.
        let huge = (Position::Cells(i64::MAX), Position::Cells(i64::MAX));
        assert_eq!(place(huge, (20, 5), CLIENT, full()), (40, 10));
    }

    #[test]
    fn border_lines_are_tmuxs_names() {
        assert_eq!(Lines::parse("rounded"), Ok(Some(Lines::Rounded)));
        assert_eq!(Lines::parse("none"), Ok(None));
        assert_eq!(
            Lines::parse("bogus"),
            Err("popup-border-lines unknown value: bogus".to_string())
        );
        assert_eq!(Lines::Heavy.chars()[0], '┏');
    }

    #[test]
    fn a_title_is_as_wide_as_its_text() {
        assert_eq!(visible_width("title"), 5);
        assert_eq!(visible_width("x#[fg=red]RED"), 4);
        assert_eq!(visible_width("a##b"), 3);
        assert_eq!(visible_width(" network "), 9);
    }

    /// The box is drawn around the inside, the title from the third
    /// column, cut two short of the edge, as recorded (`-b double`).
    #[test]
    fn the_box_and_its_title() {
        let mut screen = Screen::new(30, 7);
        let frame = Frame {
            at: (2, 1),
            size: (20, 5),
            lines: Some(Lines::Double),
            title: Some("a-very-long-title-longer-than-box".to_string()),
            border_style: Style::default(),
            close_on_exit: false,
        };
        draw_box(&mut screen, &frame, Lines::Double);
        let row = |y: usize| -> String {
            screen.cells[y * 30..(y + 1) * 30]
                .iter()
                .map(|c| c.c)
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        assert_eq!(row(1), "  ╔═a-very-long-titl═╗");
        assert_eq!(row(2), "  ║                  ║");
        assert_eq!(row(5), "  ╚══════════════════╝");
        assert_eq!(frame.inside(), ((3, 2), (18, 3)));
    }

    /// A box partly off screen: only its visible part is drawn.
    #[test]
    fn a_box_off_screen_is_clipped() {
        let mut screen = Screen::new(10, 5);
        let frame = Frame {
            at: (-2, -2),
            size: (6, 4),
            lines: Some(Lines::Single),
            title: None,
            border_style: Style::default(),
            close_on_exit: false,
        };
        draw_box(&mut screen, &frame, Lines::Single);
        let text: String = screen.cells[..10].iter().map(|c| c.c).collect();
        assert_eq!(text, "   │      ");
        let text: String = screen.cells[10..20].iter().map(|c| c.c).collect();
        assert_eq!(text, "───┘      ");
    }

    #[test]
    fn close_keys_are_escape_and_c_c() {
        assert!(closes(&Key::plain(KeyCode::Escape)));
        assert!(closes(&input::parse_key("C-c").unwrap()));
        assert!(!closes(&Key::plain(KeyCode::Char('q'))));
        assert!(!closes(&Key::plain(KeyCode::Char('c'))));
    }

    /// The reviewer's payload: C1 CSI and ST and CR/LF inside an OSC 9 are
    /// dropped on the way to the client, exactly as for a pane's OSC 9.
    #[test]
    fn a_popups_osc_is_cleaned_as_a_panes() {
        let mut state = crate::server::test_state();
        let id = ClientId(1);
        let body = b"9;X\xc2\x9b2J\xc2\x9c\r\n".to_vec();
        assert_eq!(passthrough::osc(&body), b"\x1b]9;X2J\x07".to_vec());
        // With no popup open on the client nothing is queued at all.
        on_event(&mut state, id, 1, PaneEventKind::Osc(body));
        assert!(state.clients.is_empty());
    }

    /// No popup opens without its client, and a waiting CLI gets the
    /// command's status: success for 0, that exit status otherwise.
    #[tokio::test]
    async fn no_client_opens_nothing_and_the_answer_carries_the_status() {
        use crate::types::{PaneId, SessionId};
        let mut state = crate::server::test_state();
        let id = ClientId(1);
        // No client: nothing opens.
        let frame = Frame {
            at: (0, 0),
            size: (10, 4),
            lines: Some(Lines::Single),
            title: None,
            border_style: Style::default(),
            close_on_exit: false,
        };
        let spawn = PaneSpawn {
            id: PaneId(0),
            session: SessionId(0),
            argv: Some(vec!["/bin/sh".into(), "-c".into(), "exit 3".into()]),
            cwd: "/".into(),
            env: Vec::new(),
            cols: 0,
            rows: 0,
            history_limit: 10,
            socket: state.socket.clone(),
            term: state.inner_term,
            cell_px: None,
            graphics_ok: false,
        };
        assert_eq!(open(&mut state, id, frame, spawn), Ok(false));
        assert!(!state.popups.is_open(id));

        // Closing an exited popup answers at once, with its status.
        let (tx, rx) = oneshot::channel();
        answer(Some(tx), 3);
        let got = rx.await.expect("answered");
        assert_eq!(got.map_err(|f| f.status), Err(Some(3)));
        let (tx, rx) = oneshot::channel();
        answer(Some(tx), 0);
        assert_eq!(rx.await.expect("answered"), Ok(String::new()));
    }
}
