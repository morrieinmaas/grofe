//! Menus (TC.7, spec §4.5 "Overlays"): `display-menu` at a caller's
//! position, nested menus and `display-panes`, drawn over one client's
//! frame ([`MenuOverlay`]) and taking its keys while open ([`MenuHook`]).
//! `confirm-before` is a one-key prompt (`server::prompt`), as in tmux.
//!
//! Everything here follows tmux 3.7b, recorded on a private server with a
//! pty client (`menu_add_item`, `screen_write_menu`, `menu_key_cb` and
//! `cmd_display_panes_draw_pane` are the tmux functions it matches):
//!
//! - A menu is a box `width + 4` wide and `items + 2` high. An item is its
//!   label, then `(key)` at the right edge; a label starting with `-` is
//!   drawn dim and cannot be chosen. The first item that can be chosen is
//!   highlighted when the menu opens.
//! - An item's key runs it, and wins over the navigation keys (Up, Down,
//!   `k`, `j`, Home, End, `g`, `G`, PageUp, PageDown, Enter); Escape, `q`,
//!   C-c and C-g close the menu; any other key is ignored.
//! - Choosing an item closes the menu before its command runs, so an item
//!   whose command opens another menu replaces this one, and one Escape
//!   closes what is left.
//! - `display-panes` labels each pane with its index, in big digits when
//!   the pane has room. A digit (or `a` to `z` for index 10 on) selects that
//!   pane; any other key closes the labels and goes on as usual.
//! - With the mouse (a menu a mouse key opens, or `-M`): no row starts chosen,
//!   and `menu_key_cb`'s rules apply to a report: outside the box (one column
//!   more than the box is inside, as tmux counts) a release closes the menu
//!   (`-O`: a press does, a release does not) and clears the row, inside a
//!   release chooses the row chosen last (`-O`: any press or release but the
//!   wheel and a drag), and any other report chooses the row under it. A
//!   release is any report whose low button bits are 3, so motion with no
//!   button is one. Without the mouse a menu takes no press of button 1 and
//!   any other report closes it.
//! - `menu-style`, `menu-selected-style` and `menu-border-style` (over
//!   which `-s -H -S` replace only the colours) and `menu-border-lines` or
//!   `-b` draw the box; a menu's mouse takes the client's mouse reports
//!   even with `mouse` off ([`wants_mouse`]).
//!
//! Pane-supplied text (a window name in a label) reaches the screen only as
//! cells: a control character is dropped, never written to the terminal.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use unicode_width::UnicodeWidthChar;

use crate::cmd;
use crate::config::Resolved;
use crate::protocol::{ClientCtx, Dialect, Failure};
use crate::server::input::{Input, InputHook, Key, KeyCode, Mouse, MouseButton, MouseKind, Routed};
use crate::server::mouse::MouseTarget;
use crate::server::pane::{CellFlags, SnapCell};
use crate::server::popup::Lines;
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::server::status::{self, Style};
use crate::server::{State, clients, dispatch, prompt};
use crate::types::{ClientId, PaneId, Rect};

/// tmux's `display-panes-time`, in milliseconds.
const PANES_TIME: Duration = Duration::from_millis(1000);

/// `display-panes-colour` and `display-panes-active-colour` as styles, and
/// `display-panes-time` as a duration, for the client's session.
fn panes_options(state: &State, id: ClientId) -> (String, String, Duration) {
    use crate::server::dispatch::options::Owner;
    let owners: Vec<Owner> = state
        .clients
        .get(&id)
        .map(|c| Owner::Session(c.session))
        .into_iter()
        .collect();
    let options = &state.model.options;
    let colour = |name: &str, default: &str| {
        format!("fg={}", options.lookup(name, &owners).unwrap_or(default))
    };
    let time = options
        .lookup("display-panes-time", &owners)
        .and_then(|t| t.parse::<u64>().ok())
        .map_or(PANES_TIME, Duration::from_millis);
    (
        colour("display-panes-colour", "blue"),
        colour("display-panes-active-colour", "red"),
        time,
    )
}

/// Open menus and pane labels, per client. Lives on `State.menus`.
#[derive(Default)]
pub struct Menus {
    open: BTreeMap<ClientId, Open>,
    /// The CLI replies of commands waiting for their menu to close.
    waiting: BTreeMap<ClientId, oneshot::Sender<Result<String, Failure>>>,
    /// Counts `display-panes` openings, so a timer only closes its own.
    generation: u64,
}

impl Menus {
    /// Whether client `id` has a menu or pane labels open.
    pub fn is_open(&self, id: ClientId) -> bool {
        self.open.contains_key(&id)
    }

    /// Closes client `id`'s menu, answering a CLI that waits on it.
    fn close(&mut self, id: ClientId) -> Option<Open> {
        if let Some(reply) = self.waiting.remove(&id) {
            let _ = reply.send(Ok(String::new()));
        }
        self.open.remove(&id)
    }
}

enum Open {
    Menu(Box<Shown>),
    Panes(PanesOpen),
}

/// `display-panes` while its labels are up.
struct PanesOpen {
    /// Counts openings, so a timer only closes its own.
    generation: u64,
    /// `-N`: the labels take no keys.
    no_keys: bool,
    /// The command a chosen pane runs, `%%` for its id (default
    /// `select-pane`), in the opener's dialect.
    template: Option<String>,
    dialect: Dialect,
}

/// A menu as a caller describes it: labels, title and commands are
/// formats, expanded when it opens (tmux's `menu_add_item`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub title: Option<String>,
    pub items: Vec<MenuItem>,
    pub x: Position,
    pub y: Position,
    pub look: Look,
}

/// How a menu looks and what it takes, as `display-menu`'s flags say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Look {
    /// `-b`, checked: `None` reads `menu-border-lines`, `Some(None)` is
    /// `none`.
    pub lines: Option<Option<Lines>>,
    /// `-s`, `-H` and `-S`: over the options, replacing only colours.
    pub style: Option<String>,
    pub selected: Option<String>,
    pub border: Option<String>,
    /// `-O`.
    pub stay_open: bool,
    /// `-M`.
    pub mouse: bool,
    /// `-C`.
    pub start: Start,
}

/// The row a menu starts on when it takes no mouse (tmux's
/// `starting_choice`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Start {
    /// Row 0, or the next row that can be chosen.
    #[default]
    First,
    /// `-C -`: none.
    None,
    At(u32),
}

/// One menu row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuItem {
    /// Runs `command` (a command string) when chosen.
    Command {
        label: String,
        key: Option<Key>,
        command: String,
    },
    Separator,
    /// Opens `menu` in this menu's place when chosen.
    Menu {
        label: String,
        key: Option<Key>,
        menu: Menu,
    },
}

/// Where a menu goes on one axis, in client cells, as tmux's `-x`/`-y`
/// read it. For `x` a number is the menu's left column; for `y` it is the
/// row just below the menu (tmux's convention: the menu sits above it).
/// Either is then moved inside the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Position {
    Cells(i64),
    /// `C`: the centre of the client.
    Centre,
    /// `P`: for `x` the current pane's left edge, for `y` the menu's
    /// bottom edge on the pane's.
    Pane,
    /// `R`, for `x` only: the menu's right edge on the current pane's (on
    /// `y`, as `Pane`).
    PaneRight,
    /// The pointer's client cell, for a menu a mouse key opens (TC.10, as
    /// tmux's `M`): centred on it for `x`, its top row for `y`.
    Pointer(u16),
    /// A `-x`/`-y` that reads the popup position variables (`W`, `S`, or a
    /// format naming `popup_*`): kept raw until the menu's size is known,
    /// then expanded as tmux does (see [`resolve`]).
    Expr(String),
}

/// An open menu, laid out.
struct Shown {
    title: Option<String>,
    /// `None` is a separator. Each label is in tmux's form, `-` and key
    /// column included.
    rows: Vec<Option<Row>>,
    /// tmux's `menu->width`: the box is 4 wider.
    width: usize,
    /// The box's top-left cell on the client.
    at: (u16, u16),
    choice: Option<usize>,
    /// What the chosen command runs in: the opener's.
    dialect: Dialect,
    /// The opener's context, for a nested menu's formats.
    ctx: ClientCtx,
    look: Look,
    /// `MENU_NOMOUSE`: no mouse event opened the menu and `-M` was not given.
    no_mouse: bool,
    /// The mouse event the menu was opened for: its rows' commands run for
    /// it, as tmux's `menu_key_cb` passes the item's event on.
    event: Option<MouseTarget>,
}

struct Row {
    label: String,
    key: Option<Key>,
    action: Action,
}

enum Action {
    Run(String),
    Open(Menu),
}

impl Row {
    fn enabled(&self) -> bool {
        !self.label.starts_with('-')
    }
}

impl Shown {
    fn enabled(&self, i: usize) -> bool {
        self.rows
            .get(i)
            .and_then(Option::as_ref)
            .is_some_and(Row::enabled)
    }

    /// Steps the choice to the previous (`back`) or next enabled row,
    /// wrapping, as tmux's Up and Down do.
    fn step(&mut self, back: bool) {
        let count = self.rows.len();
        if count == 0 {
            return;
        }
        let old = self.choice.unwrap_or(0);
        let mut i = self.choice;
        loop {
            let next = match (i, back) {
                (None, true) | (Some(0), true) => count - 1,
                (Some(n), true) => n - 1,
                (Some(n), false) if n + 1 < count => n + 1,
                _ => 0,
            };
            i = Some(next);
            if self.enabled(next) || next == old {
                break;
            }
        }
        self.choice = i.filter(|n| self.enabled(*n));
    }

    /// The row a menu that takes no mouse starts on, as tmux's
    /// `menu_prepare` finds it: row `start` or the next that can be
    /// chosen, wrapping; from past the end, the last that can be.
    fn starting_choice(&self, start: Start) -> Option<usize> {
        let count = self.rows.len() as i64;
        let start = match start {
            Start::First => 0,
            Start::None => return None,
            Start::At(n) => i64::from(n),
        };
        if start >= count {
            let last = count - 1;
            let mut choice = last + 1;
            loop {
                if self.enabled((choice - 1) as usize) {
                    return Some((choice - 1) as usize);
                }
                choice -= 1;
                if choice == 0 {
                    choice = count;
                }
                if choice == last + 1 {
                    return None;
                }
            }
        }
        let mut choice = start;
        loop {
            if self.enabled(choice as usize) {
                return Some(choice as usize);
            }
            choice += 1;
            if choice == count {
                choice = 0;
            }
            if choice == start {
                return None;
            }
        }
    }

    /// The first (`from_end` false) or last enabled row.
    fn edge(&mut self, from_end: bool) {
        let mut rows: Vec<usize> = (0..self.rows.len()).filter(|i| self.enabled(*i)).collect();
        if from_end {
            rows.reverse();
        }
        self.choice = rows.first().copied();
    }

    /// What `key` does to the menu.
    fn key(&mut self, key: &Key) -> Step {
        let hit = self.rows.iter().position(|r| {
            r.as_ref()
                .is_some_and(|r| r.enabled() && r.key.is_some_and(|k| k.matches(key)))
        });
        if let Some(i) = hit {
            return Step::Choose(i);
        }
        let m = key.mods;
        let plain = !m.ctrl && !m.alt;
        match key.code {
            KeyCode::Up | KeyCode::BackTab => self.step(true),
            KeyCode::Char('k') if plain => self.step(true),
            KeyCode::Down => self.step(false),
            KeyCode::Char('j') if plain => self.step(false),
            KeyCode::Home => self.edge(false),
            KeyCode::Char('g') if plain => self.edge(false),
            KeyCode::End => self.edge(true),
            KeyCode::Char('G') if plain => self.edge(true),
            // ponytail: a page is five steps; tmux counts a page of five
            // enabled rows, clamped at the ends without wrapping.
            KeyCode::PageUp => (0..5).for_each(|_| self.step(true)),
            KeyCode::Char('b') if m.ctrl => (0..5).for_each(|_| self.step(true)),
            KeyCode::PageDown => (0..5).for_each(|_| self.step(false)),
            KeyCode::Char('f') if m.ctrl => (0..5).for_each(|_| self.step(false)),
            KeyCode::Enter => return self.chosen(),
            KeyCode::Char('m') if m.ctrl => return self.chosen(),
            KeyCode::Escape => return Step::Close,
            KeyCode::Char('q') if plain => return Step::Close,
            KeyCode::Char('c' | 'g') if m.ctrl => return Step::Close,
            _ => return Step::Keep,
        }
        Step::Keep
    }
}

enum Step {
    Keep,
    Close,
    Choose(usize),
}

/// A report as tmux's `menu_key_cb` reads its button bits (`b & 195`, the
/// drag bit and the wheel).
struct Bits {
    buttons: u16,
    release: bool,
    drag: bool,
    wheel: bool,
}

impl Bits {
    fn of(kind: MouseKind) -> Bits {
        let code = |b: MouseButton| match b {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::Other(n) => 128 + u16::from(n.saturating_sub(8)),
        };
        let (buttons, drag) = match kind {
            MouseKind::Down(b) => (code(b), false),
            MouseKind::Up(_) => (3, false),
            MouseKind::Drag(b) => (code(b), true),
            MouseKind::Move => (3, true),
            MouseKind::WheelUp => (64, false),
            MouseKind::WheelDown => (65, false),
            MouseKind::WheelLeft => (66, false),
            MouseKind::WheelRight => (67, false),
        };
        Bits {
            buttons,
            release: buttons == 3,
            drag,
            wheel: matches!(buttons, 64 | 65),
        }
    }
}

impl Shown {
    /// tmux's `chosen:` label: the row chosen last runs, or the menu
    /// closes (a separator or a disabled row keeps a `-O` menu open).
    fn chosen(&self) -> Step {
        let Some(i) = self.choice else {
            return Step::Close;
        };
        if self.enabled(i) {
            Step::Choose(i)
        } else if self.look.stay_open {
            Step::Keep
        } else {
            Step::Close
        }
    }

    /// What a mouse report does, as tmux's `menu_key_cb`.
    fn mouse(&mut self, m: &Mouse) -> Step {
        let bits = Bits::of(m.kind);
        if self.no_mouse {
            return if bits.buttons == 0 {
                Step::Keep
            } else {
                Step::Close
            };
        }
        let (px, py) = (i64::from(self.at.0), i64::from(self.at.1));
        let (x, y) = (i64::from(m.col), i64::from(m.row));
        let count = self.rows.len() as i64;
        let outside = x < px || x > px + 4 + self.width as i64 || y < py + 1 || y > py + count;
        let stay = self.look.stay_open;
        if outside {
            if !stay && bits.release || stay && !bits.release && !bits.wheel && !bits.drag {
                return Step::Close;
            }
            self.choice = None;
            return Step::Keep;
        }
        if !stay && bits.release || stay && !bits.wheel && !bits.drag {
            return self.chosen();
        }
        self.choice = usize::try_from(y - py - 1).ok();
        Step::Keep
    }
}

/// The display width of `text` without its `#[...]` directives (tmux's
/// `format_width`).
fn format_width(text: &str) -> usize {
    tokens(text)
        .iter()
        .map(|t| match t {
            Token::Char(c) => char_width(*c),
            Token::Style(_) => 0,
        })
        .sum()
}

fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

enum Token<'a> {
    Char(char),
    /// A `#[...]` directive's content.
    Style(&'a str),
}

fn tokens(text: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        if text[i..].starts_with("#[")
            && let Some(end) = text[i + 2..].find(']')
        {
            out.push(Token::Style(&text[i + 2..i + 2 + end]));
            i += end + 3;
            continue;
        }
        out.push(Token::Char(c));
        i += c.len_utf8();
    }
    out
}

/// The last `limit` columns of `text`, its directives kept (tmux's
/// `format_trim_right`, which keeps the right-hand end: recorded, a long
/// item shows as `m name here>`).
fn trim_keep_right(text: &str, limit: usize) -> String {
    let mut drop = format_width(text).saturating_sub(limit);
    let mut out = String::new();
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        if text[i..].starts_with("#[")
            && let Some(end) = text[i + 2..].find(']')
        {
            out.push_str(&text[i..i + end + 3]);
            i += end + 3;
            continue;
        }
        let w = char_width(c);
        if drop > 0 && w <= drop {
            drop -= w;
        } else {
            drop = 0;
            out.push(c);
        }
        i += c.len_utf8();
    }
    out
}

/// A menu's rows in tmux's form (`menu_add_item`): a separator is dropped
/// at the top or after another, an item whose label expands to nothing is
/// dropped, and a label is cut to fit a client `cols` wide, its key shown
/// only while it fits. Returns the rows and the menu's width, which starts
/// at the title's.
fn lay_out(
    title: Option<&str>,
    items: Vec<(Option<String>, Option<Key>, Option<Action>)>,
    cols: u16,
) -> (Vec<Option<Row>>, usize) {
    let mut rows: Vec<Option<Row>> = Vec::new();
    let mut width = title.map_or(0, format_width);
    for (label, key, action) in items {
        let (Some(label), Some(action)) = (label, action) else {
            if rows.last().is_some_and(Option::is_some) {
                rows.push(None);
            }
            continue;
        };
        if label.is_empty() {
            continue;
        }
        let mut max = usize::from(cols).saturating_sub(4);
        let len = format_width(&label);
        let mut shown_key = key
            .filter(|_| !label.starts_with('-'))
            .map(|k| k.to_string());
        if let Some(k) = &shown_key {
            let keylen = k.chars().count() + 3;
            if keylen <= max / 4 {
                max -= keylen;
            } else if keylen >= max || len >= max - keylen {
                shown_key = None;
            }
        }
        let mut suffix = "";
        if len > max {
            max = max.saturating_sub(1);
            suffix = ">";
        }
        let trimmed = trim_keep_right(&label, max);
        let label = match shown_key {
            Some(k) => format!("{trimmed}{suffix}#[default] #[align=right]({k})"),
            None => format!("{trimmed}{suffix}"),
        };
        let mut w = format_width(&label);
        if label.starts_with('-') {
            w -= 1;
        }
        width = width.max(w);
        rows.push(Some(Row { label, key, action }));
    }
    (rows, width)
}

/// The menu box's top-left cell for a `w` by `h` box on a `client`-sized
/// terminal, `pane` being the current pane's rect in client cells, as
/// tmux's `cmd_display_menu_get_pos` places it; `None` when the box does
/// not fit, and tmux opens nothing.
fn place(
    (x, y): (Position, Position),
    (w, h): (u16, u16),
    client: (u16, u16),
    pane: Rect,
) -> Option<(u16, u16)> {
    let (cols, rows) = (i64::from(client.0), i64::from(client.1));
    let (w, h) = (i64::from(w), i64::from(h));
    if cols < w || rows < h {
        return None;
    }
    let mut n = match x {
        Position::Cells(n) => n,
        Position::Centre => (cols - 1) / 2 - w / 2,
        Position::Pane => i64::from(pane.x),
        Position::PaneRight => i64::from(pane.x) + i64::from(pane.cols) - w,
        Position::Pointer(x) => i64::from(x) - w / 2,
        Position::Expr(_) => 0,
    };
    if n.saturating_add(w) >= cols {
        n = cols - w;
    } else if n < 0 {
        n = 0;
    }
    let px = n;
    let mut n = match y {
        Position::Cells(n) => n,
        Position::Centre => (rows - 1) / 2 + h / 2,
        Position::Pane | Position::PaneRight => i64::from(pane.y) + i64::from(pane.rows),
        // tmux's `popup_mouse_top`: the row `h` below the pointer, at most
        // the client's last row.
        Position::Pointer(y) => (i64::from(y) + h).min(rows - 1),
        Position::Expr(_) => 0,
    };
    n = if n < h { 0 } else { n - h };
    if n.saturating_add(h) >= rows {
        n = rows - h;
    }
    Some((u16::try_from(px).ok()?, u16::try_from(n.max(0)).ok()?))
}

/// Opens `menu` on client `id`, formats expanded against `ctx`, unless the
/// client has a menu open already (tmux does nothing then) or the menu
/// does not fit. Returns whether it opened.
pub fn open(state: &mut State, ctx: &ClientCtx, id: ClientId, menu: Menu) -> bool {
    if state.menus.is_open(id) {
        return false;
    }
    let Some(client) = clients::overlay_ctx(state, id) else {
        return false;
    };
    let Some(size) = state.clients.get(&id).map(|c| c.size) else {
        return false;
    };
    let expand = |s: &str| prompt::expand_command(state, ctx, s);
    let title = menu.title.as_deref().map(expand);
    let items = menu
        .items
        .into_iter()
        .map(|item| match item {
            MenuItem::Separator => (None, None, None),
            MenuItem::Command {
                label,
                key,
                command,
            } => (
                Some(expand(&label)),
                key,
                Some(Action::Run(expand(&command))),
            ),
            MenuItem::Menu { label, key, menu } => {
                (Some(expand(&label)), key, Some(Action::Open(menu)))
            }
        })
        .collect();
    let (rows, width) = lay_out(title.as_deref(), items, size.0);
    let pane = current_pane_rect(state, ctx, &client).unwrap_or(Rect {
        x: 0,
        y: 0,
        cols: size.0,
        rows: size.1,
    });
    let (Ok(w), Ok(h)) = (u16::try_from(width + 4), u16::try_from(rows.len() + 2)) else {
        return false;
    };
    let (x, y) = (
        resolve(state, ctx, id, menu.x, Axis::X, (w, h), size, pane),
        resolve(state, ctx, id, menu.y, Axis::Y, (w, h), size, pane),
    );
    let Some(at) = place((x, y), (w, h), size, pane) else {
        return false;
    };
    let event = state.mouse.current().filter(|m| m.client == id).cloned();
    let no_mouse = event.is_none() && !menu.look.mouse;
    let start = menu.look.start;
    let mut shown = Shown {
        title,
        rows,
        width,
        at,
        choice: None,
        dialect: ctx.dialect,
        ctx: ctx.clone(),
        look: menu.look,
        no_mouse,
        event,
    };
    if no_mouse {
        shown.choice = shown.starting_choice(start);
    }
    state.menus.open.insert(id, Open::Menu(Box::new(shown)));
    clients::mark_dirty(state, id);
    clients::sync_mouse_modes(state);
    true
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
}

/// The names `cmd_display_menu_get_pos` adds to the format its `-x`/`-y`
/// expand in; one it does not set expands to nothing.
const POPUP_VARS: &[&str] = &[
    "popup_mouse_x",
    "popup_mouse_y",
    "popup_window_status_line_x",
    "popup_window_status_line_y",
    "popup_status_line_y",
    "popup_width",
    "popup_height",
    "popup_centre_x",
    "popup_centre_y",
    "popup_mouse_centre_x",
    "popup_mouse_centre_y",
    "popup_mouse_top",
    "popup_mouse_bottom",
    "popup_pane_top",
    "popup_pane_bottom",
    "popup_pane_left",
    "popup_pane_right",
];

/// The popup position variables of tmux's `cmd_display_menu_get_pos` for a
/// `w` by `h` box on a `(sx, sy)` client, `pane` being the current pane's
/// rect in client cells (its status offset and viewport already applied).
fn popup_vars(
    state: &State,
    ctx: &ClientCtx,
    id: ClientId,
    (w, h): (i64, i64),
    (sx, sy): (i64, i64),
    pane: Rect,
) -> BTreeMap<&'static str, i64> {
    use crate::cmd::Resolve;
    let mut v = BTreeMap::new();
    let pointer = crate::server::mouse::pointer(state, id);
    if let Some((mx, my)) = pointer {
        v.insert("popup_mouse_x", i64::from(mx));
        v.insert("popup_mouse_y", i64::from(my));
    }
    if let Some(spot) = clients::status_spot(state, id) {
        let lines = i64::from(spot.lines);
        let index = state
            .model
            .resolve_window(&cmd::Target::Current, ctx)
            .ok()
            .and_then(|w| {
                // The index under the client's session, not the first link's.
                let session = state.clients.get(&id)?.session;
                state.model.index_in(session, w)
            });
        let found = spot
            .windows
            .iter()
            .find(|(_, _, i)| Some(*i) == index)
            .copied();
        if let Some((line, start, _)) = found {
            v.insert("popup_window_status_line_x", i64::from(start));
            v.insert(
                "popup_window_status_line_y",
                if spot.top {
                    i64::from(line) + 1 + h
                } else {
                    sy - lines + i64::from(line)
                },
            );
        }
        v.insert(
            "popup_status_line_y",
            if spot.top { lines + h } else { sy - lines },
        );
    }
    v.insert("popup_width", w);
    v.insert("popup_height", h);
    v.insert("popup_centre_x", ((sx - 1) / 2 - w / 2).max(0));
    let n = (sy - 1) / 2 + h / 2;
    v.insert("popup_centre_y", if n >= sy { sy - h } else { n });
    if let Some((mx, my)) = pointer {
        let (mx, my) = (i64::from(mx), i64::from(my));
        v.insert("popup_mouse_centre_x", (mx - w / 2).max(0));
        let n = my - h / 2;
        v.insert("popup_mouse_centre_y", if n + h >= sy { sy - h } else { n });
        let n = my + h;
        v.insert("popup_mouse_top", if n >= sy { sy - 1 } else { n });
        v.insert("popup_mouse_bottom", (my - h).max(0));
    }
    let n = i64::from(pane.y) + h;
    v.insert("popup_pane_top", if n >= sy { sy - h } else { n });
    v.insert(
        "popup_pane_bottom",
        i64::from(pane.y) + i64::from(pane.rows),
    );
    v.insert("popup_pane_left", i64::from(pane.x));
    v.insert(
        "popup_pane_right",
        (i64::from(pane.x) + i64::from(pane.cols) - w).max(0),
    );
    v
}

/// A [`Position::Expr`] as the cell count tmux's `-x`/`-y` come to; any
/// other position is returned as it is.
#[allow(clippy::too_many_arguments)]
fn resolve(
    state: &State,
    ctx: &ClientCtx,
    id: ClientId,
    position: Position,
    axis: Axis,
    (w, h): (u16, u16),
    size: (u16, u16),
    pane: Rect,
) -> Position {
    match position {
        Position::Expr(spec) => Position::Cells(resolve_spec(
            state,
            ctx,
            id,
            &spec,
            axis,
            (w, h),
            size,
            pane,
        )),
        other => other,
    }
}

/// What a `-x`/`-y` value comes to in tmux's `cmd_display_menu_get_pos`:
/// the letter's variable (`W`, `M`, `S`), or the format itself, with the
/// popup variables filled in, read as `strtol` reads it. `(w, h)` is the
/// box, `size` the client, `pane` the current pane's rect on the client.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_spec(
    state: &State,
    ctx: &ClientCtx,
    id: ClientId,
    spec: &str,
    axis: Axis,
    (w, h): (u16, u16),
    (sx, sy): (u16, u16),
    pane: Rect,
) -> i64 {
    let named = match (spec, axis) {
        ("W", Axis::X) => "#{popup_window_status_line_x}",
        ("W", Axis::Y) => "#{popup_window_status_line_y}",
        ("M", Axis::X) => "#{popup_mouse_centre_x}",
        ("M", Axis::Y) => "#{popup_mouse_top}",
        ("S", Axis::Y) => "#{popup_status_line_y}",
        _ => spec,
    };
    let vars = popup_vars(
        state,
        ctx,
        id,
        (i64::from(w), i64::from(h)),
        (i64::from(sx), i64::from(sy)),
        pane,
    );
    let mut text = named.to_string();
    for name in POPUP_VARS {
        let value = vars.get(name).map(i64::to_string).unwrap_or_default();
        text = text.replace(&format!("#{{{name}}}"), &value);
    }
    strtol(&prompt::expand_command(state, ctx, &text))
}

/// C's `strtol(s, NULL, 10)`: blanks, a sign, then digits; 0 without any.
pub fn strtol(s: &str) -> i64 {
    let s = s.trim_start();
    let (sign, digits) = match s.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    digits[..end].parse::<i64>().map_or(0, |n| sign * n)
}

/// The rect, in client cells, of the pane a command with `ctx` runs
/// against.
fn current_pane_rect(state: &State, ctx: &ClientCtx, cx: &OverlayCtx) -> Option<Rect> {
    use crate::cmd::Resolve;
    let pane = state.model.resolve_pane(&cmd::Target::Current, ctx).ok()?;
    cx.panes.iter().find(|(p, _)| *p == pane).map(|(_, r)| *r)
}

/// How `display-panes` was asked for.
pub struct PanesArgs {
    /// `-d`; the `display-panes-time` option without it. Zero keeps the
    /// labels up until a key.
    pub duration: Option<Duration>,
    /// `-N`.
    pub no_keys: bool,
    /// The template, else `select-pane -t "%%%"`.
    pub template: Option<String>,
    /// The dialect the template runs in: the opener's.
    pub dialect: Dialect,
}

/// Opens `display-panes` on client `id`, unless something is open there
/// already. The labels close after `args.duration`.
pub fn open_panes(state: &mut State, id: ClientId, args: PanesArgs) -> bool {
    if state.menus.is_open(id) {
        return false;
    }
    state.menus.generation += 1;
    let generation = state.menus.generation;
    state.menus.open.insert(
        id,
        Open::Panes(PanesOpen {
            generation,
            no_keys: args.no_keys,
            template: args.template,
            dialect: args.dialect,
        }),
    );
    let (_, _, time) = panes_options(state, id);
    let time = args.duration.unwrap_or(time);
    clients::mark_dirty(state, id);
    // Without a runtime (unit tests) the labels stay until a key.
    if !time.is_zero()
        && let Ok(rt) = tokio::runtime::Handle::try_current()
    {
        let jobs = state.jobs.clone();
        rt.spawn(async move {
            tokio::time::sleep(time).await;
            let job: crate::server::Job = Box::new(move |state: &mut State| {
                if matches!(state.menus.open.get(&id), Some(Open::Panes(p)) if p.generation == generation)
                {
                    state.menus.close(id);
                    clients::mark_dirty(state, id);
                }
            });
            let _ = jobs.send(job).await;
        });
    }
    true
}

/// Whether client `id` has a menu open that takes the mouse, which
/// asks the terminal for every motion even with `mouse` off (tmux's menu
/// screen mode), and for the client's mouse reports.
pub fn wants_mouse(state: &State, id: ClientId) -> bool {
    matches!(state.menus.open.get(&id), Some(Open::Menu(m)) if !m.no_mouse)
}

/// The caller waits for client `id`'s menu to close, as tmux's
/// `display-menu` and `display-panes` do: a CLI request's reply, and a
/// binding's later commands and its client's input (`tm_6/waits`).
pub fn wait_for_close(state: &mut State, id: ClientId) {
    if state.menus.is_open(id) {
        let reply = dispatch::defer_reply(state);
        state.menus.waiting.insert(id, reply);
    }
}

/// Takes a client's keys while its menu or pane labels are open.
pub struct MenuHook;

impl InputHook for MenuHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        _now: Instant,
    ) -> Option<Routed> {
        let open = state.menus.open.get_mut(&client)?;
        let routed = match open {
            Open::Menu(shown) => {
                let step = match input {
                    Input::Key(key) => shown.key(key),
                    Input::Focus(_) => return None,
                    Input::Mouse(m) => shown.mouse(m),
                    Input::Paste { .. } => Step::Keep,
                };
                match step {
                    Step::Keep => Routed::Consumed,
                    Step::Close => {
                        state.menus.close(client);
                        Routed::Consumed
                    }
                    Step::Choose(i) => choose(state, client, i),
                }
            }
            Open::Panes(_) => match input {
                Input::Focus(_) => return None,
                Input::Key(key) => {
                    let Some(Open::Panes(panes)) = state.menus.close(client) else {
                        return None;
                    };
                    clients::mark_dirty(state, client);
                    // `-N`: no key is the labels'; it closes them and goes on.
                    if panes.no_keys {
                        return None;
                    }
                    let index = match key.code {
                        KeyCode::Char(c) if key.mods == Default::default() => pane_label_index(c),
                        _ => None,
                    };
                    // Any other key closes the labels and goes on as usual.
                    let index = index?;
                    match pane_at_index(state, client, index) {
                        Some(pane) => choose_pane(state, client, &panes, pane),
                        None => Routed::Consumed,
                    }
                }
                Input::Mouse(_) | Input::Paste { .. } => {
                    state.menus.close(client);
                    clients::mark_dirty(state, client);
                    return None;
                }
            },
        };
        clients::mark_dirty(state, client);
        clients::sync_mouse_modes(state);
        Some(routed)
    }
}

/// `display-panes` chose `pane`: its window leaves zoom, and the template
/// (`select-pane -t "%%%"` without one) runs with the pane's id for `%%`.
fn choose_pane(state: &mut State, id: ClientId, panes: &PanesOpen, pane: PaneId) -> Routed {
    if let Some(window) = state.model.panes.get(&pane).map(|p| p.window) {
        dispatch::pane::unzoom(&mut state.model, window);
    }
    let Some(template) = &panes.template else {
        let target = pane.to_string();
        let argv = ["select-pane", "-t", target.as_str()].map(String::from);
        return Routed::Command(vec![argv.to_vec()], Dialect::Grofe);
    };
    match cmd::split_command_string(&replace_template(template, &pane.to_string())) {
        Ok(cmds) if !cmds.is_empty() => Routed::Command(cmds, panes.dialect),
        Ok(_) => Routed::Consumed,
        Err(e) => {
            clients::show_message(state, id, &e.to_string());
            Routed::Consumed
        }
    }
}

/// tmux's `cmd_template_replace` for one answer that has nothing to
/// quote: `%1` everywhere and `%%` the first time stand for `answer`, and
/// a `%` after either is part of the mark (`%%%`).
fn replace_template(template: &str, answer: &str) -> String {
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::new();
    let mut replaced = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if c == '%' {
            let matched = match chars.get(i) {
                Some('1') => true,
                Some('%') if !replaced => {
                    replaced = true;
                    true
                }
                _ => false,
            };
            if matched {
                i += 1;
                if chars.get(i) == Some(&'%') {
                    i += 1;
                }
                out.push_str(answer);
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Row `i` of client `id`'s menu was chosen: the menu closes, then the
/// row's command runs or its nested menu opens.
fn choose(state: &mut State, id: ClientId, i: usize) -> Routed {
    let Some(Open::Menu(mut shown)) = state.menus.close(id) else {
        return Routed::Consumed;
    };
    let Some(Some(row)) = shown.rows.get_mut(i).map(Option::take) else {
        return Routed::Consumed;
    };
    match row.action {
        // As tmux appends the item's command to the client's queue: behind
        // the rest of the binding that waits for the menu (recorded: `BM`).
        // A CLI caller's menu runs it now. A menu a mouse key opened runs it
        // for that mouse event, so `-t=` in it is the pane clicked.
        // ponytail: tmux 3.7b drops a CLI menu's command while the client
        // waits on a prompt (recorded); grofe runs it.
        Action::Run(command) => match cmd::split_command_string(&command) {
            Ok(cmds) if !cmds.is_empty() => {
                let dialect = shown.dialect;
                match shown.event.take() {
                    Some(event) => {
                        let run = move |state: &mut State| {
                            clients::run_commands_for(
                                state,
                                id,
                                &cmds,
                                dialect,
                                false,
                                Some(event),
                            );
                        };
                        clients::in_input_order(state, id, Box::new(run));
                        Routed::Consumed
                    }
                    None if shown.ctx.client.is_none() => Routed::Command(cmds, dialect),
                    None => {
                        let run = move |state: &mut State| {
                            clients::run_binding(state, id, &cmds, dialect)
                        };
                        clients::in_input_order(state, id, Box::new(run));
                        Routed::Consumed
                    }
                }
            }
            Ok(_) => Routed::Consumed,
            Err(e) => {
                clients::show_message(state, id, &e.to_string());
                Routed::Consumed
            }
        },
        Action::Open(menu) => {
            open(state, &shown.ctx, id, menu);
            Routed::Consumed
        }
    }
}

/// The pane index a `display-panes` key names: `0` to `9`, then `a` to
/// `z` for 10 to 35.
fn pane_label_index(c: char) -> Option<u32> {
    match c {
        '0'..='9' => c.to_digit(10),
        'a'..='z' => Some(10 + u32::from(c) - u32::from('a')),
        _ => None,
    }
}

/// The pane with index `index` in client `id`'s current window.
fn pane_at_index(state: &State, id: ClientId, index: u32) -> Option<PaneId> {
    let model = &state.model;
    let session = model.sessions.get(&state.clients.get(&id)?.session)?;
    let window = model.windows.get(&session.active)?;
    let base = model.pane_base_index(window.id);
    let pos = usize::try_from(index.checked_sub(base)?).ok()?;
    window.pane_ids().get(pos).copied()
}

/// Draws a client's open menu or pane labels.
pub struct MenuOverlay;

impl Overlay for MenuOverlay {
    fn draw(&self, state: &State, client: ClientId, cx: &OverlayCtx, screen: &mut Screen) {
        match state.menus.open.get(&client) {
            Some(Open::Menu(shown)) => {
                draw_menu(shown, screen, &styles(state, client, shown));
                screen.cursor = None;
            }
            Some(Open::Panes(_)) => {
                let labels = pane_labels(state, client, cx);
                let (style, active, _) = panes_options(state, client);
                draw_panes(&labels, screen, &style, &active);
                screen.cursor = None;
            }
            None => {}
        }
    }
}

/// One pane to label: its index, its rect on the client, whether it is
/// the active pane.
struct PaneLabel {
    index: u32,
    rect: Rect,
    active: bool,
}

fn pane_labels(state: &State, client: ClientId, cx: &OverlayCtx) -> Vec<PaneLabel> {
    let model = &state.model;
    let Some(window) = state
        .clients
        .get(&client)
        .and_then(|c| model.sessions.get(&c.session))
        .and_then(|s| model.windows.get(&s.active))
    else {
        return Vec::new();
    };
    let base = model.pane_base_index(window.id);
    let ids = window.pane_ids();
    // ponytail: a pane cut by the viewport (D6 crop) is labelled within the
    // part the client shows, where tmux centres on the whole pane.
    cx.panes
        .iter()
        .filter_map(|(pane, rect)| {
            let pos = ids.iter().position(|p| p == pane)?;
            Some(PaneLabel {
                index: base + u32::try_from(pos).ok()?,
                rect: *rect,
                active: *pane == window.active,
            })
        })
        .collect()
}

/// A cell of `c` in `style`, the renderer's form.
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

/// Puts `c` at `(x, y)` if it is on screen; a wide character takes two
/// cells and is dropped at the right edge. Returns its width. A control
/// character (no width) is dropped: pane text never reaches the terminal
/// as bytes.
fn put(screen: &mut Screen, (x, y): (usize, usize), c: char, style: &Style) -> usize {
    let (cols, rows) = (usize::from(screen.cols), usize::from(screen.rows));
    let Some(width) = UnicodeWidthChar::width(c) else {
        return 0;
    };
    if y >= rows || x >= cols {
        return width;
    }
    let at = y * cols + x;
    if width == 0 {
        if x > 0 {
            screen.cells[at - 1].zerowidth.push(c);
        }
        return 0;
    }
    if width == 2 {
        if x + 1 >= cols {
            return width;
        }
        let mut spacer = cell(' ', style);
        spacer.flags |= CellFlags::WIDE_CHAR_SPACER;
        screen.cells[at + 1] = spacer;
        let mut wide = cell(c, style);
        wide.flags |= CellFlags::WIDE_CHAR;
        screen.cells[at] = wide;
    } else {
        screen.cells[at] = cell(c, style);
    }
    width
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Centre,
    Right,
}

/// Draws `text`, a format already expanded, at `(x, y)` within `width`
/// cells, as tmux's `format_draw`: `#[...]` directives change the style,
/// `#[default]` goes back to `base`, and `#[align=...]` puts what follows
/// at the left, the centre or the right. Nothing is drawn past `width`.
fn format_draw(screen: &mut Screen, (x, y): (usize, usize), width: usize, text: &str, base: Style) {
    let mut parts: [Vec<(char, Style)>; 3] = Default::default();
    let mut align = Align::Left;
    let mut directives = String::new();
    let mut style = base;
    for token in tokens(text) {
        match token {
            Token::Char(c) => parts[align as usize].push((c, style)),
            Token::Style(content) => {
                for item in content.split(',').map(str::trim) {
                    match item {
                        "default" => directives.clear(),
                        "align=left" => align = Align::Left,
                        "align=centre" | "align=center" => align = Align::Centre,
                        "align=right" => align = Align::Right,
                        item => {
                            directives.push(',');
                            directives.push_str(item);
                        }
                    }
                }
                style = base;
                status::apply_style_directive(&mut style, directives.trim_start_matches(','));
            }
        }
    }
    let part_width = |p: &[(char, Style)]| p.iter().map(|(c, _)| char_width(*c)).sum::<usize>();
    let [left, centre, right] = &parts;
    let starts = [
        0,
        (width / 2).saturating_sub(part_width(centre) / 2),
        width.saturating_sub(part_width(right)),
    ];
    for (part, start) in [left, centre, right].into_iter().zip(starts) {
        let mut col = start;
        for (c, style) in part {
            let w = char_width(*c);
            if col + w > width {
                break;
            }
            put(screen, (x + col, y), *c, style);
            col += w;
        }
    }
}

/// The three styles and the border lines a menu is drawn with.
struct Styles {
    menu: Style,
    selected: Style,
    border: Style,
    lines: Option<Lines>,
}

/// The styles of `shown` now, as tmux's `menu_reapply_styles` builds them:
/// the options of the client's current window (formats expanded), over
/// which `-s -H -S` replace only the colours; `-b` over `menu-border-lines`.
fn styles(state: &State, client: ClientId, shown: &Shown) -> Styles {
    use crate::server::dispatch::options::Owner;
    let session = state.clients.get(&client).map(|c| c.session);
    let window = session
        .and_then(|s| state.model.sessions.get(&s))
        .map(|s| s.active);
    let owners: Vec<Owner> = window
        .map(Owner::Window)
        .into_iter()
        .chain(session.map(Owner::Session))
        .collect();
    let option = |name: &str| {
        state
            .model
            .options
            .lookup(name, &owners)
            .unwrap_or_default()
    };
    let style = |name: &str, over: &Option<String>| {
        let mut st = status::parse_style(&prompt::expand_command(state, &shown.ctx, option(name)));
        if let Some(over) = over {
            let over = status::parse_style(over);
            (st.fg, st.bg) = (over.fg, over.bg);
        }
        st
    };
    Styles {
        menu: style("menu-style", &shown.look.style),
        selected: style("menu-selected-style", &shown.look.selected),
        border: style("menu-border-style", &shown.look.border),
        lines: shown.look.lines.unwrap_or_else(|| {
            Lines::parse(option("menu-border-lines")).unwrap_or(Some(Lines::Single))
        }),
    }
}

/// Draws a menu as tmux's `screen_write_menu` does: the box in
/// `menu-border-style` with the title in its top edge, each item's row
/// filled in `menu-style` (or `menu-selected-style` when chosen), its label
/// after one blank, a disabled one dim, and a separator as a line across
/// the box. `menu-border-lines` `none` still takes the cells, as blanks.
fn draw_menu(menu: &Shown, screen: &mut Screen, st: &Styles) {
    let (x, y) = (usize::from(menu.at.0), usize::from(menu.at.1));
    let (w, h) = (menu.width + 4, menu.rows.len() + 2);
    let border = &st.border;
    let ([tl, tr, bl, br, horizontal, vertical], left_join, right_join) = match st.lines {
        Some(lines) => (lines.chars(), lines.joins().0, lines.joins().1),
        None => ([' '; 6], ' ', ' '),
    };
    for i in 0..w {
        let (top, bottom) = match i {
            0 => (tl, bl),
            i if i + 1 == w => (tr, br),
            _ => (horizontal, horizontal),
        };
        put(screen, (x + i, y), top, border);
        put(screen, (x + i, y + h - 1), bottom, border);
    }
    for j in 1..h - 1 {
        put(screen, (x, y + j), vertical, border);
        put(screen, (x + w - 1, y + j), vertical, border);
    }
    if let Some(title) = &menu.title {
        format_draw(screen, (x + 2, y), w - 4, title, *border);
    }
    for (i, row) in menu.rows.iter().enumerate() {
        let row_y = y + 1 + i;
        let Some(row) = row else {
            put(screen, (x, row_y), left_join, border);
            for col in 1..w - 1 {
                put(screen, (x + col, row_y), horizontal, border);
            }
            put(screen, (x + w - 1, row_y), right_join, border);
            continue;
        };
        let base = if menu.choice == Some(i) && row.enabled() {
            st.selected
        } else {
            st.menu
        };
        for col in 1..w - 1 {
            put(screen, (x + col, row_y), ' ', &base);
        }
        match row.label.strip_prefix('-') {
            Some(label) => {
                let mut dim = base;
                dim.attrs |= CellFlags::DIM;
                format_draw(screen, (x + 2, row_y), menu.width, label, dim);
            }
            None => format_draw(screen, (x + 2, row_y), menu.width, &row.label, base),
        }
    }
}

/// tmux's `window_clock_table`: each digit 5 by 5, a set cell drawn as a
/// blank in the label's colour.
const DIGITS: [[&str; 5]; 10] = [
    ["#####", "#...#", "#...#", "#...#", "#####"],
    ["....#", "....#", "....#", "....#", "....#"],
    ["#####", "....#", "#####", "#....", "#####"],
    ["#####", "....#", "#####", "....#", "#####"],
    ["#...#", "#...#", "#####", "....#", "....#"],
    ["#####", "#....", "#####", "....#", "#####"],
    ["#####", "#....", "#####", "#...#", "#####"],
    ["#####", "....#", "....#", "....#", "....#"],
    ["#####", "#...#", "#####", "#...#", "#####"],
    ["#####", "#...#", "#####", "....#", "#####"],
];

/// Draws `display-panes`' labels as tmux's `cmd_display_panes_draw_pane`:
/// the index in big digits at the pane's centre, its size at the top
/// right and, from index 10, the letter that selects it under the digits;
/// a pane too small for the digits gets the index (and letter) as text.
/// The active pane's label is red, the others blue.
fn draw_panes(labels: &[PaneLabel], screen: &mut Screen, style: &str, active: &str) {
    for label in labels {
        let Rect { x, y, cols, rows } = label.rect;
        let (x, y, sx, sy) = (
            usize::from(x),
            usize::from(y),
            usize::from(cols),
            usize::from(rows),
        );
        let colour = if label.active { active } else { style };
        let fg = status::parse_style(colour);
        let bg = Style {
            bg: fg.fg,
            ..Style::default()
        };
        let index = label.index.to_string();
        let len = index.len();
        let letter = match label.index {
            10..36 => char::from(b'a' + (label.index - 10) as u8).to_string(),
            _ => String::new(),
        };
        if sx < len {
            continue;
        }
        let (px, py) = (sx / 2, sy / 2);
        let text = |screen: &mut Screen, col: usize, row: usize, s: &str| {
            for (i, c) in s.chars().enumerate() {
                put(screen, (x + col + i, y + row), c, &fg);
            }
        };
        if sx < len * 6 || sy < 5 {
            // As tmux, the index, a blank and the letter (so a blank alone
            // when there is no letter), or the index alone if that is too
            // wide.
            let all = len + letter.len() + 1;
            if sx >= all {
                text(screen, px - all / 2, py, &format!("{index} {letter}"));
            } else {
                text(screen, px - len / 2, py, &index);
            }
            continue;
        }
        let (mut dx, dy) = (px - len * 3, py - 2);
        for digit in index.bytes().map(|b| usize::from(b - b'0')) {
            for (j, line) in DIGITS[digit].iter().enumerate() {
                for (i, set) in line.bytes().enumerate() {
                    if set == b'#' {
                        put(screen, (x + dx + i, y + dy + j), ' ', &bg);
                    }
                }
            }
            dx += 6;
        }
        if sy <= 6 {
            continue;
        }
        let size = format!("{cols}x{rows}");
        if sx >= size.len() {
            text(screen, sx - size.len(), 0, &size);
        }
        if !letter.is_empty() {
            text(screen, px + len * 3 - letter.len() - 1, dy + 5, &letter);
        }
    }
}

/// Applies the config at start and on each `reload` (TC.7). grofe's
/// aliases are the `command-alias` list option, which the option store
/// already takes from the config, so there is nothing more to apply.
pub fn configure(_state: &mut State, _config: &Resolved) {}

/// Closes whatever menu or pane labels client `id` has open (tmux's
/// `server_client_clear_overlay`, for the menus).
pub fn clear(state: &mut State, id: ClientId) {
    if state.menus.close(id).is_some() {
        clients::mark_dirty(state, id);
        clients::sync_mouse_modes(state);
    }
}

/// Client `id` went away: its menu closes, and a CLI waiting on it gets
/// an empty success.
pub fn client_gone(state: &mut State, id: ClientId) {
    state.menus.close(id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::input::parse_key;

    fn screen(cols: u16, rows: u16) -> Screen {
        Screen::new(cols, rows)
    }

    fn text_rows(s: &Screen) -> Vec<String> {
        s.cells
            .chunks(usize::from(s.cols))
            .map(|row| {
                row.iter()
                    .filter(|c| !c.flags.contains(CellFlags::WIDE_CHAR_SPACER))
                    .map(|c| c.c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn at(s: &Screen, x: usize, y: usize) -> &SnapCell {
        &s.cells[y * usize::from(s.cols) + x]
    }

    fn item(label: &str, key: &str) -> (Option<String>, Option<Key>, Option<Action>) {
        (
            Some(label.to_string()),
            parse_key(key).ok(),
            Some(Action::Run(String::new())),
        )
    }

    fn sep() -> (Option<String>, Option<Key>, Option<Action>) {
        (None, None, None)
    }

    fn shown(
        title: Option<&str>,
        items: Vec<(Option<String>, Option<Key>, Option<Action>)>,
        at: (u16, u16),
        cols: u16,
    ) -> Shown {
        let (rows, width) = lay_out(title, items, cols);
        let mut s = Shown {
            title: title.map(str::to_string),
            rows,
            width,
            at,
            choice: None,
            dialect: Dialect::Grofe,
            ctx: crate::server::test_ctx(),
            look: Look::default(),
            no_mouse: true,
            event: None,
        };
        s.edge(false);
        s
    }

    fn selected() -> Style {
        status::parse_style("bg=yellow,fg=black")
    }

    /// tmux's default menu styles and `single` lines.
    fn defaults() -> Styles {
        Styles {
            menu: Style::default(),
            selected: selected(),
            border: Style::default(),
            lines: Some(Lines::Single),
        }
    }

    /// Recorded on tmux 3.7b, 80x24: `display-menu -T Title -x C -y C First
    /// a .. "" Second b .. -Disabled c .. Longer-name-here d ..`.
    #[test]
    fn a_menu_is_drawn_cell_for_cell_as_tmux_draws_it() {
        let items = vec![
            item("First", "a"),
            sep(),
            item("Second", "b"),
            item("-Disabled", "c"),
            item("Longer-name-here", "d"),
        ];
        let (rows, width) = lay_out(Some("Title"), items, 80);
        assert_eq!(width, 20);
        let full = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 23,
        };
        let origin = place(
            (Position::Centre, Position::Centre),
            (24, 7),
            (80, 24),
            full,
        );
        assert_eq!(origin, Some((27, 7)));
        let mut s = screen(80, 24);
        let mut menu = shown(
            Some("Title"),
            vec![
                item("First", "a"),
                sep(),
                item("Second", "b"),
                item("-Disabled", "c"),
                item("Longer-name-here", "d"),
            ],
            (27, 7),
            80,
        );
        assert_eq!(menu.rows.len(), rows.len());
        draw_menu(&menu, &mut s, &defaults());
        let got = text_rows(&s);
        let want = [
            "                           ┌─Title────────────────┐",
            "                           │ First            (a) │",
            "                           ├──────────────────────┤",
            "                           │ Second           (b) │",
            "                           │ Disabled             │",
            "                           │ Longer-name-here (d) │",
            "                           └──────────────────────┘",
        ];
        assert_eq!(got[7..14], want);
        // The chosen row (the first) is highlighted inside the box, border
        // excluded; the disabled label alone is dim.
        for x in 27..51 {
            let c = at(&s, x, 8);
            let want = (28..50).contains(&x);
            assert_eq!(c.bg == selected().bg, want, "x={x}");
        }
        assert_eq!(at(&s, 28, 8).fg, selected().fg);
        for x in 27..51 {
            let dim = at(&s, x, 11).flags.contains(CellFlags::DIM);
            assert_eq!(dim, (29..37).contains(&x), "x={x}");
        }
        assert_eq!(at(&s, 27, 7).bg, Style::default().bg);

        // Down skips the separator; recorded as tmux moves.
        menu.key(&parse_key("Down").expect("key"));
        assert_eq!(menu.choice, Some(2));
        let mut s = screen(80, 24);
        draw_menu(&menu, &mut s, &defaults());
        assert_eq!(at(&s, 28, 10).bg, selected().bg);
        assert_eq!(at(&s, 28, 8).bg, Style::default().bg);
    }

    /// Recorded: leading and doubled separators and empty labels are
    /// dropped, a trailing separator stays; the first enabled row is
    /// chosen; Up and Down wrap past separators and disabled rows.
    #[test]
    fn separators_and_navigation_follow_tmux() {
        let items = vec![
            sep(),
            item("-Dis first", "x"),
            sep(),
            sep(),
            item("", "h"),
            item("Name win", "n"),
            item("Enter", "Enter"),
            item("C-x item", "C-x"),
            sep(),
            item("Last", "l"),
            sep(),
        ];
        let mut m = shown(None, items, (2, 2), 80);
        let mut s = screen(80, 24);
        draw_menu(&m, &mut s, &defaults());
        let got = text_rows(&s);
        assert_eq!(
            got[2..12],
            [
                "  ┌────────────────┐",
                "  │ Dis first      │",
                "  ├────────────────┤",
                "  │ Name win   (n) │",
                "  │ Enter  (Enter) │",
                "  │ C-x item (C-x) │",
                "  ├────────────────┤",
                "  │ Last       (l) │",
                "  ├────────────────┤",
                "  └────────────────┘",
            ]
        );
        assert_eq!(m.choice, Some(2));
        m.key(&parse_key("Up").expect("key"));
        assert_eq!(m.choice, Some(6));
        m.key(&parse_key("Down").expect("key"));
        assert_eq!(m.choice, Some(2));
        m.key(&parse_key("j").expect("key"));
        assert_eq!(m.choice, Some(3));
        m.key(&parse_key("G").expect("key"));
        assert_eq!(m.choice, Some(6));
        m.key(&parse_key("g").expect("key"));
        assert_eq!(m.choice, Some(2));
        assert!(matches!(
            m.key(&parse_key("C-x").expect("key")),
            Step::Choose(4)
        ));
        // The item keyed Enter wins over choosing the highlighted row.
        assert!(matches!(
            m.key(&parse_key("Enter").expect("key")),
            Step::Choose(3)
        ));
        m.rows[3] = None;
        assert!(matches!(
            m.key(&parse_key("Enter").expect("key")),
            Step::Choose(2)
        ));
        // A disabled row's key does nothing.
        assert!(matches!(m.key(&parse_key("x").expect("key")), Step::Keep));
        assert!(matches!(m.key(&parse_key("z").expect("key")), Step::Keep));
        for k in ["Escape", "q", "C-c", "C-g"] {
            assert!(
                matches!(m.key(&parse_key(k).expect("key")), Step::Close),
                "{k}"
            );
        }
    }

    /// Recorded: an item keyed `q` runs instead of closing the menu.
    #[test]
    fn an_items_key_wins_over_the_menu_keys() {
        let mut m = shown(None, vec![item("One", "q"), item("Two", "t")], (0, 0), 80);
        assert!(matches!(
            m.key(&parse_key("q").expect("key")),
            Step::Choose(0)
        ));
    }

    /// Recorded: a centred title starts at `width / 2 - title / 2` (each
    /// halved on its own), a right-aligned one ends at the edge, and a
    /// title wider than every item sets the width.
    #[test]
    fn titles_align_as_tmux_aligns_them() {
        let cases = [
            ("LongTitleHere", "A", "┌─LongTitleHere─┐"),
            ("#[align=centre]AB", "Item number", "┌───────AB────────┐"),
            ("#[align=centre]ABC", "Item numbers", "┌────────ABC───────┐"),
            ("#[align=right]R", "Item numbers", "┌────────────────R─┐"),
            (
                "#[align=centre]T win",
                "C-x item (C-x)",
                "┌──────T win─────┐",
            ),
        ];
        for (title, label, want) in cases {
            let label_only = label.split(" (").next().unwrap_or(label);
            let key = if label.contains("(C-x)") { "C-x" } else { "a" };
            let m = shown(Some(title), vec![item(label_only, key)], (0, 0), 80);
            let mut s = screen(40, 5);
            draw_menu(&m, &mut s, &defaults());
            assert_eq!(text_rows(&s)[0], want, "{title}");
        }
    }

    /// Recorded: `#[fg=...]` in a title or label colours what follows,
    /// `#[default]` goes back to the row's style (the highlight on the
    /// chosen row).
    #[test]
    fn styles_in_labels_and_titles() {
        let m = shown(
            Some("#[fg=red]Red#[default] x"),
            vec![
                item("#[fg=blue]Blue#[default] rest", "a"),
                item("#[bold]B", "b"),
            ],
            (0, 8),
            80,
        );
        let mut s = screen(80, 24);
        draw_menu(&m, &mut s, &defaults());
        assert_eq!(text_rows(&s)[8], "┌─Red x─────────┐");
        let red = status::parse_style("fg=red").fg;
        let blue = status::parse_style("fg=blue").fg;
        assert_eq!(at(&s, 2, 8).fg, red);
        assert_eq!(at(&s, 6, 8).fg, Style::default().fg);
        assert_eq!(at(&s, 2, 9).fg, blue);
        assert_eq!(at(&s, 2, 9).bg, selected().bg);
        assert_eq!(at(&s, 6, 9).fg, selected().fg);
        assert!(at(&s, 2, 10).flags.contains(CellFlags::BOLD));
        assert!(!at(&s, 13, 10).flags.contains(CellFlags::BOLD));
    }

    /// Recorded on a 20-column client: a long label keeps its right-hand
    /// end and a `>`, its key while it fits; a disabled label cut this way
    /// loses its `-` and shows enabled, as in tmux.
    #[test]
    fn a_label_is_cut_to_fit_the_client() {
        let items = vec![
            item("A very long item name here", "a"),
            item("Short", "b"),
            item("-A disabled long item name", "c"),
        ];
        let m = shown(None, items, (0, 7), 20);
        let mut s = screen(20, 24);
        draw_menu(&m, &mut s, &defaults());
        assert_eq!(
            text_rows(&s)[7..12],
            [
                "┌──────────────────┐",
                "│ m name here> (a) │",
                "│ Short        (b) │",
                "│  long item name> │",
                "└──────────────────┘",
            ]
        );
    }

    /// Recorded on tmux 3.7b, 80x24, panes 0 (0,0 49x23), 1 (50,0 30x14),
    /// 2 (50,15 30x8), a 9 by 3 menu.
    #[test]
    fn positions_follow_tmux() {
        let pane0 = Rect {
            x: 0,
            y: 0,
            cols: 49,
            rows: 23,
        };
        let pane2 = Rect {
            x: 50,
            y: 15,
            cols: 30,
            rows: 8,
        };
        let p = |x, y, pane| place((x, y), (9, 3), (80, 24), pane);
        use Position::*;
        assert_eq!(p(Centre, Centre, pane0), Some((35, 9)));
        assert_eq!(p(PaneRight, Pane, pane0), Some((40, 20)));
        assert_eq!(p(Pane, Pane, pane0), Some((0, 20)));
        assert_eq!(p(PaneRight, Pane, pane2), Some((71, 20)));
        assert_eq!(p(Pane, Pane, pane2), Some((50, 20)));
        assert_eq!(p(Cells(78), Cells(30), pane0), Some((71, 21)));
        assert_eq!(p(Cells(-5), Cells(1), pane0), Some((0, 0)));
        assert_eq!(p(Cells(49), Cells(23), pane0), Some((49, 20)));
        assert_eq!(p(Cells(5), Cells(3), pane0), Some((5, 0)));
        assert_eq!(p(Cells(30), Cells(10), pane0), Some((30, 7)));
        // The largest number still lands inside, with no overflow.
        assert_eq!(p(Cells(i64::MAX), Cells(i64::MAX), pane0), Some((71, 21)));
        // 25 rows, 81 columns: the centre rounds as tmux's does.
        assert_eq!(
            place((Centre, Centre), (9, 3), (81, 25), pane0),
            Some((36, 10))
        );
        // Too big for the client: tmux opens nothing.
        assert_eq!(place((Centre, Centre), (81, 3), (80, 24), pane0), None);
        // TC.10, recorded on tmux 3.7b (`-x M -y M`, an 11 by 4 box): the
        // box is centred on the pointer's column, its top on its row, and
        // kept inside the client; near the bottom `popup_mouse_top` is at
        // most the last row, then the height less (`b3wpc/menu-bound-mouse`).
        let m = |x, y| place((Pointer(x), Pointer(y)), (11, 4), (80, 24), pane0);
        assert_eq!(m(20, 8), Some((15, 8)));
        assert_eq!(m(69, 2), Some((64, 2)));
        assert_eq!(m(4, 21), Some((0, 19)));
        assert_eq!(m(39, 0), Some((34, 0)));
        assert_eq!(m(1, 11), Some((0, 11)));
    }

    fn label(index: u32, (x, y, cols, rows): (u16, u16, u16, u16), active: bool) -> PaneLabel {
        PaneLabel {
            index,
            rect: Rect { x, y, cols, rows },
            active,
        }
    }

    /// The cells of `s` whose background is `bg`, per row.
    fn bg_cells(s: &Screen, bg: alacritty_terminal::vte::ansi::Color) -> Vec<Vec<usize>> {
        s.cells
            .chunks(usize::from(s.cols))
            .map(|row| {
                row.iter()
                    .enumerate()
                    .filter(|(_, c)| c.bg == bg)
                    .map(|(x, _)| x)
                    .collect()
            })
            .collect()
    }

    /// Recorded on tmux 3.7b: three panes, pane 0 active.
    /// The labels take `display-panes-colour` and
    /// `display-panes-active-colour`.
    #[test]
    fn display_panes_draws_in_the_colours_it_is_given() {
        let mut s = screen(80, 24);
        let labels = [
            label(3, (0, 0, 39, 23), true),
            label(4, (40, 0, 39, 23), false),
        ];
        draw_panes(&labels, &mut s, "fg=green", "fg=yellow");
        let any = |colour: &str| {
            let fg = status::parse_style(colour).fg;
            bg_cells(&s, fg).iter().any(|row| !row.is_empty())
        };
        assert!(any("fg=yellow") && any("fg=green"));
        assert!(!any("fg=red") && !any("fg=blue"));
    }

    #[test]
    fn display_panes_draws_big_digits_and_sizes() {
        let labels = [
            label(0, (0, 0, 49, 23), true),
            label(1, (50, 0, 30, 14), false),
            label(2, (50, 15, 30, 8), false),
        ];
        let mut s = screen(80, 24);
        draw_panes(&labels, &mut s, "fg=blue", "fg=red");
        let red = status::parse_style("fg=red").fg;
        let blue = status::parse_style("fg=blue").fg;
        let reds = bg_cells(&s, red);
        assert_eq!(reds[9], (21..26).collect::<Vec<_>>());
        assert_eq!(reds[10], [21, 25]);
        assert_eq!(reds[13], (21..26).collect::<Vec<_>>());
        let blues = bg_cells(&s, blue);
        for (row, cells) in blues.iter().enumerate().take(10).skip(5) {
            assert_eq!(cells, &[66], "row {row}");
        }
        assert_eq!(blues[17], (62..67).collect::<Vec<_>>());
        assert_eq!(blues[18], [66]);
        assert_eq!(blues[20], [62]);
        let rows = text_rows(&s);
        assert_eq!(&rows[0][44..49], "49x23");
        assert_eq!(at(&s, 44, 0).fg, red);
        assert_eq!(&rows[0][75..80], "30x14");
        assert_eq!(at(&s, 75, 0).fg, blue);
        assert_eq!(&rows[15][76..80], "30x8");
    }

    /// Recorded: every digit's shape, two-digit indices with their letter,
    /// and a pane too short for digits.
    #[test]
    fn display_panes_digits_letters_and_small_panes() {
        let mut s = screen(80, 24);
        draw_panes(
            &[label(3, (0, 0, 39, 23), true)],
            &mut s,
            "fg=blue",
            "fg=red",
        );
        let red = status::parse_style("fg=red").fg;
        let reds = bg_cells(&s, red);
        assert_eq!(reds[10], [20]);
        assert_eq!(reds[12], [20]);
        // 10 and 11 in 19- and 20-column panes, their letters under them.
        let mut s = screen(80, 24);
        draw_panes(
            &[
                label(10, (40, 0, 19, 23), false),
                label(11, (60, 0, 20, 23), false),
            ],
            &mut s,
            "fg=blue",
            "fg=red",
        );
        let blue = status::parse_style("fg=blue").fg;
        let blues = bg_cells(&s, blue);
        assert_eq!(blues[9], [47, 49, 50, 51, 52, 53, 68, 74]);
        assert_eq!(blues[10], [47, 49, 53, 68, 74]);
        let rows = text_rows(&s);
        assert_eq!(rows[14].chars().nth(53), Some('a'));
        assert_eq!(rows[14].chars().nth(74), Some('b'));
        // A 3-row pane: the index as text at its centre, then a blank.
        let mut s = screen(80, 24);
        draw_panes(
            &[label(1, (0, 20, 39, 3), true)],
            &mut s,
            "fg=blue",
            "fg=red",
        );
        assert_eq!(text_rows(&s)[21], "                  1");
        assert_eq!(at(&s, 18, 21).fg, red);
        assert_eq!(at(&s, 19, 21).fg, red);
        for (digit, shape) in DIGITS.iter().enumerate() {
            assert!(shape.iter().all(|l| l.len() == 5), "{digit}");
        }
    }

    #[test]
    fn display_panes_keys_name_indices() {
        assert_eq!(pane_label_index('0'), Some(0));
        assert_eq!(pane_label_index('9'), Some(9));
        assert_eq!(pane_label_index('a'), Some(10));
        assert_eq!(pane_label_index('z'), Some(35));
        assert_eq!(pane_label_index('A'), None);
    }

    /// Pane text reaches the screen only as cells: control characters in a
    /// label are dropped.
    #[test]
    fn control_characters_in_labels_are_dropped() {
        let m = shown(
            Some("t\x1b]52;c;x\x07"),
            vec![item("a\x1b[2Jb\u{9b}c", "a")],
            (0, 0),
            80,
        );
        let mut s = screen(40, 5);
        draw_menu(&m, &mut s, &defaults());
        for c in &s.cells {
            assert!(!c.c.is_control(), "{c:?}");
            assert!(c.zerowidth.iter().all(|z| !z.is_control()), "{c:?}");
        }
        assert!(
            text_rows(&s)[1].starts_with("│ a[2Jbc"),
            "{:?}",
            text_rows(&s)
        );
    }

    #[test]
    fn trimming_keeps_the_right_hand_end_and_directives() {
        assert_eq!(trim_keep_right("abcdef", 3), "def");
        assert_eq!(trim_keep_right("#[fg=red]abcdef", 2), "#[fg=red]ef");
        assert_eq!(trim_keep_right("ab", 5), "ab");
        assert_eq!(format_width("#[fg=red]ab#[default]日"), 4);
    }

    /// The menu of `b3wpc/menu-mouse`: 13 wide at (10, 3), Alpha on row 4,
    /// Beta on 5, a separator on 6, Gamma on 7, a disabled row on 8.
    fn mouse_menu(stay_open: bool) -> Shown {
        let mut m = shown(
            Some("Menu"),
            vec![
                item("Alpha", "a"),
                item("Beta", "b"),
                sep(),
                item("Gamma", "g"),
                item("-Dim", "d"),
            ],
            (10, 3),
            80,
        );
        m.no_mouse = false;
        m.choice = None;
        m.look.stay_open = stay_open;
        m
    }

    fn report(kind: MouseKind, col: u16, row: u16) -> Mouse {
        Mouse {
            kind,
            col,
            row,
            mods: Default::default(),
        }
    }

    /// Recorded on tmux 3.7b (`b3wpc/menu-mouse`): a drag highlights, a
    /// release chooses the row chosen last, and one with none closes.
    #[test]
    fn a_release_chooses_the_row_chosen_last() {
        let (left, down) = (MouseButton::Left, MouseKind::Down(MouseButton::Left));
        let mut m = mouse_menu(false);
        assert!(matches!(
            m.mouse(&report(MouseKind::Drag(left), 12, 5)),
            Step::Keep
        ));
        assert_eq!(m.choice, Some(1));
        // The release is wherever; the row is the one chosen last.
        assert!(matches!(
            m.mouse(&report(MouseKind::Up(left), 30, 9)),
            Step::Close
        ));
        let mut m = mouse_menu(false);
        m.mouse(&report(down, 12, 5));
        assert_eq!(m.choice, Some(1));
        assert!(matches!(
            m.mouse(&report(MouseKind::Up(left), 12, 8)),
            Step::Choose(1)
        ));
        // Motion with no button is a release too: with no row chosen it
        // closes the menu.
        let mut m = mouse_menu(false);
        assert!(matches!(
            m.mouse(&report(MouseKind::Move, 12, 5)),
            Step::Close
        ));
    }

    /// One column right of the box is inside; two is outside. The top
    /// and bottom borders are outside.
    #[test]
    fn the_inside_is_tmuxs_off_by_one() {
        let drag = MouseKind::Drag(MouseButton::Left);
        let mut m = mouse_menu(false);
        m.mouse(&report(drag, 23, 5));
        assert_eq!(m.choice, Some(1));
        m.mouse(&report(drag, 24, 5));
        assert_eq!(m.choice, None);
        m.mouse(&report(drag, 12, 5));
        m.mouse(&report(drag, 12, 3));
        assert_eq!(m.choice, None);
        m.mouse(&report(drag, 12, 5));
        m.mouse(&report(drag, 12, 9));
        assert_eq!(m.choice, None);
    }

    /// A separator or a disabled row chosen closes the menu, or with
    /// `-O` leaves it open.
    #[test]
    fn a_row_that_cannot_be_chosen_closes_a_menu_unless_it_stays_open() {
        for (stay, want_close) in [(false, true), (true, false)] {
            for row in [6, 8] {
                let mut m = mouse_menu(stay);
                m.choice = Some(row - 4);
                assert_eq!(
                    matches!(m.chosen(), Step::Close),
                    want_close,
                    "{row} {stay}"
                );
                assert_eq!(
                    matches!(m.chosen(), Step::Keep),
                    !want_close,
                    "{row} {stay}"
                );
            }
        }
    }

    /// `-O`: a press or release chooses, the wheel and a drag only
    /// highlight, and a press outside closes.
    #[test]
    fn a_stay_open_menu_chooses_on_a_press_or_release() {
        let (left, down, up) = (
            MouseButton::Left,
            MouseKind::Down(MouseButton::Left),
            MouseKind::Up(MouseButton::Left),
        );
        let mut m = mouse_menu(true);
        // No row chosen yet: a press closes it.
        assert!(matches!(m.mouse(&report(down, 12, 5)), Step::Close));
        let mut m = mouse_menu(true);
        m.mouse(&report(MouseKind::Move, 12, 5));
        assert_eq!(m.choice, Some(1));
        assert!(matches!(m.mouse(&report(down, 12, 5)), Step::Choose(1)));
        assert!(matches!(m.mouse(&report(up, 12, 5)), Step::Choose(1)));
        assert!(matches!(
            m.mouse(&report(MouseKind::WheelUp, 12, 4)),
            Step::Keep
        ));
        assert_eq!(m.choice, Some(0));
        m.mouse(&report(MouseKind::Drag(left), 12, 7));
        assert_eq!(m.choice, Some(3));
        // Outside: motion and a release leave it open and clear the row,
        // a press closes it.
        assert!(matches!(m.mouse(&report(up, 50, 5)), Step::Keep));
        assert_eq!(m.choice, None);
        assert!(matches!(m.mouse(&report(down, 50, 5)), Step::Close));
    }

    /// A menu no mouse event opened takes no press of button 1, and any
    /// other report closes it.
    #[test]
    fn a_menu_without_the_mouse_closes_on_any_other_report() {
        let mut m = mouse_menu(false);
        m.no_mouse = true;
        let left = MouseButton::Left;
        assert!(matches!(
            m.mouse(&report(MouseKind::Down(left), 12, 5)),
            Step::Keep
        ));
        assert!(matches!(
            m.mouse(&report(MouseKind::Drag(left), 12, 5)),
            Step::Keep
        ));
        for kind in [
            MouseKind::Up(left),
            MouseKind::Move,
            MouseKind::WheelUp,
            MouseKind::Down(MouseButton::Right),
        ] {
            assert!(
                matches!(m.mouse(&report(kind, 12, 5)), Step::Close),
                "{kind:?}"
            );
        }
    }

    /// Recorded (`b3wpc/menu-flags`): `-C` past the end takes the last row
    /// that can be chosen, a separator the next row, and a disabled row
    /// wraps.
    #[test]
    fn the_starting_choice_skips_rows_that_cannot_be_chosen() {
        let m = mouse_menu(false);
        assert_eq!(m.starting_choice(Start::First), Some(0));
        assert_eq!(m.starting_choice(Start::At(1)), Some(1));
        assert_eq!(m.starting_choice(Start::At(2)), Some(3));
        assert_eq!(m.starting_choice(Start::At(4)), Some(0));
        assert_eq!(m.starting_choice(Start::At(99)), Some(3));
        assert_eq!(m.starting_choice(Start::None), None);
    }

    /// Recorded: the pane menu of a click on the second last row opens one
    /// above the last row, not pushed down over the status line.
    #[test]
    fn a_menu_at_the_pointer_follows_popup_mouse_top() {
        let full = Rect {
            x: 0,
            y: 0,
            cols: 60,
            rows: 14,
        };
        // tmux: `popup_mouse_top` is at most the last row, then `h` less.
        let at = place(
            (Position::Pointer(20), Position::Pointer(5)),
            (22, 11),
            (60, 15),
            full,
        );
        assert_eq!(at, Some((9, 3)));
        let at = place(
            (Position::Pointer(20), Position::Pointer(2)),
            (22, 11),
            (60, 15),
            full,
        );
        assert_eq!(at, Some((9, 2)));
    }

    #[test]
    fn a_template_takes_the_pane_id_once() {
        assert_eq!(
            replace_template("select-pane -t \"%%%\"", "%3"),
            "select-pane -t \"%3\""
        );
        assert_eq!(
            replace_template("set -g @p got-%%", "%1"),
            "set -g @p got-%1"
        );
        assert_eq!(replace_template("a %1 b %1 c %%", "%2"), "a %2 b %2 c %2");
        assert_eq!(replace_template("a %% b %%", "%2"), "a %2 b %%");
        assert_eq!(replace_template("no mark", "%2"), "no mark");
    }
}
