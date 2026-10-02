//! The choosers (TC.9): `choose-tree`, `find-window`, `choose-client` and
//! (TM.4) `choose-buffer`,
//! drawn over a pane of one client ([`TreeOverlay`]) and taking its keys
//! while open ([`TreeHook`]).
//!
//! Everything follows tmux 3.7b's `mode-tree.c`, `window-tree.c` and
//! `window-client.c`, read from its source and compared cell by cell in a
//! pty against a private server:
//!
//! - The list takes the top of the pane: each line is its key (`(0)` to
//!   `(9)`, then `(M-a)` to `(M-z)`), the tree lines (`├─> `, `└─> `,
//!   `│   `), `+ ` or `- ` for an item with children, the name (window and
//!   pane numbers right-aligned per depth), then `: ` and the item's text.
//!   The current line is drawn in `mode-style`.
//! - Below the list, a box titled with the item's name and the sort order
//!   holds a preview: a session's windows or a window's panes side by side,
//!   each with a small labelled box in its middle, or the pane itself.
//! - Enter (or a line's key) switches the client to the chosen session,
//!   window or pane (`choose-client` detaches the chosen client,
//!   `choose-buffer` pastes the chosen buffer into the pane, `paste-buffer
//!   -p`, and its `d` deletes the current buffer); `q`, Escape and C-g
//!   close it. `-Z` zooms the pane while it is open.
//!
//! grofe keeps a chooser per client, over one pane, where tmux puts the
//! pane itself in a mode that every client showing it sees.
// ponytail: tagging (`t`, `T`, C-t, `X`, `:`), kill (`x`), search (`/`,
// `n`, `N`), filter (`f`), sort (`O`, `r`), preview size (`v`), marks (`m`,
// `M`), swap (S-Up, S-Down), help and the mouse are not implemented; their
// keys do nothing. The choose-tree formats (`-F`, `-K`) and templates are
// tmux's defaults, and the styles are the defaults of `mode-style`,
// `tree-mode-preview-style`, `display-panes-colour` and
// `display-panes-active-colour`, until those are options and flags.

use std::collections::BTreeMap;
use std::time::Instant;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::cmd::{self, Resolve, Target};
use crate::protocol::{ClientCtx, Dialect};
use crate::server::dispatch::options::{FormatCtx, ServerVars};
use crate::server::input::{Input, InputHook, Key, KeyCode, ROOT_TABLE, Routed};
use crate::server::model::Model;
use crate::server::pane::{CellFlags, SnapCell};
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::server::status::{self, Style};
use crate::server::{State, clients};
use crate::types::{ClientId, PaneId, Rect, SessionId, WindowId};

/// tmux's `mode-style` default.
const MODE_STYLE: &str = "bg=yellow,fg=black";
/// `tree-mode-preview-style`'s default: `display-panes-active-colour` for
/// the active window or pane, else `display-panes-colour`.
const PREVIEW_STYLE: &str = "fg=blue";
const PREVIEW_ACTIVE_STYLE: &str = "fg=red";

/// What `choose-tree` and `find-window` open with.
pub struct Opts {
    /// `-Z`.
    pub zoom: bool,
    /// `-s`: sessions collapsed.
    pub sessions: bool,
    /// `-w`: windows collapsed under their sessions.
    pub windows: bool,
    /// `find-window`'s pattern: only windows whose name, a pane's title or
    /// a pane's visible text hold it.
    pub find: Option<String>,
}

/// Open choosers, per client. Lives on `State.trees`.
#[derive(Default)]
pub struct Trees {
    open: BTreeMap<ClientId, Tree>,
}

impl Trees {
    pub fn is_open(&self, id: ClientId) -> bool {
        self.open.contains_key(&id)
    }
}

/// `pane_in_mode` and `pane_mode` for a pane a chooser is open over, as
/// tmux's mode names it (`window-tree.c`, `window-client.c`,
/// `window-buffer.c`). `None` for any other pane or variable.
pub fn format_var(state: &State, pane: PaneId, name: &str) -> Option<String> {
    let tree = state.trees.open.values().find(|t| t.pane == pane)?;
    match name {
        "pane_in_mode" => Some("1".to_string()),
        "pane_mode" => Some(
            match tree.kind {
                Kind::Tree { .. } => "tree-mode",
                Kind::Clients => "client-mode",
                Kind::Buffers => "buffer-mode",
            }
            .to_string(),
        ),
        _ => None,
    }
}

/// What a line stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tag {
    Session(SessionId),
    Window(WindowId),
    Pane(PaneId),
    Client(ClientId),
    /// A paste buffer, by its id (`buffers::Buffer::id`).
    Buffer(u64),
}

enum Kind {
    Tree { find: Option<String> },
    Clients,
    Buffers,
}

struct Tree {
    kind: Kind,
    /// The pane it is drawn over.
    pane: PaneId,
    /// The window `-Z` zoomed, unzoomed again on close.
    unzoom: Option<WindowId>,
    /// Whether an item shows its children; an item not in here does
    /// (tmux's items made after the first build).
    expanded: BTreeMap<Tag, bool>,
    current: usize,
    offset: usize,
    /// The list's height at the last build (tmux's `mtd->height`).
    height: usize,
    /// The current line's item, to find it again after a rebuild.
    tag: Option<Tag>,
    /// How far the preview is scrolled (`<`, `>`).
    preview_offset: i32,
    /// Where it opened, for `H`.
    start: (Tag, Tag, Tag),
}

/// One item and its children, as `mode_tree_add` makes them.
struct Item {
    tag: Tag,
    name: String,
    text: String,
    /// Names are right-aligned against the other names at the same depth.
    align: bool,
    children: Vec<Item>,
}

/// One drawn line, as `mode_tree_build_lines` makes them.
struct Line {
    tag: Tag,
    depth: usize,
    last: bool,
    flat: bool,
    parent: Option<usize>,
    has_children: bool,
    expanded: bool,
    name: String,
    text: String,
    align: bool,
}

/// A tree as it is now: its lines, which is current, and the list's
/// height in the pane.
struct View {
    lines: Vec<Line>,
    current: usize,
    offset: usize,
    height: usize,
    no_matches: bool,
    rows: usize,
}

/// Opens the tree chooser on the current client, over the `target` pane.
/// Nothing happens without a client, or when one is open there already
/// (tmux does nothing for a pane already in the mode).
pub fn open_tree(
    state: &mut State,
    ctx: &ClientCtx,
    target: &Target,
    opts: Opts,
) -> Result<String, String> {
    let pane = state.model.resolve_pane(target, ctx)?;
    let Some(client) = clients::current_client(state, ctx) else {
        return Ok(String::new());
    };
    if state.trees.is_open(client) {
        return Ok(String::new());
    }
    let model = &state.model;
    let window = model.panes[&pane].window;
    let w = &model.windows[&window];
    let session = w.session;
    let start = (Tag::Session(session), Tag::Window(window), Tag::Pane(pane));
    let tag = if opts.sessions {
        Tag::Session(session)
    } else if opts.windows || w.pane_ids().len() == 1 {
        Tag::Window(window)
    } else {
        Tag::Pane(pane)
    };
    let mut tree = Tree {
        kind: Kind::Tree { find: opts.find },
        pane,
        unzoom: None,
        expanded: BTreeMap::new(),
        current: 0,
        offset: 0,
        height: 0,
        tag: Some(tag),
        preview_offset: 0,
        start,
    };
    // Sessions start collapsed with `-s`, windows with `-s` or `-w`.
    let (items, _) = build(state, &tree);
    for s in &items {
        tree.expanded.insert(s.tag, !opts.sessions);
        for w in &s.children {
            tree.expanded.insert(w.tag, !opts.sessions && !opts.windows);
        }
    }
    open(state, client, tree, opts.zoom);
    Ok(String::new())
}

/// Opens the client chooser on the current client, over its pane.
pub fn open_clients(state: &mut State, ctx: &ClientCtx, zoom: bool) -> Result<String, String> {
    open_list(state, ctx, Kind::Clients, zoom)
}

/// Opens the buffer chooser on the current client, over its pane (TM.4).
/// With no buffers nothing opens, as in tmux.
pub fn open_buffers(state: &mut State, ctx: &ClientCtx, zoom: bool) -> Result<String, String> {
    if state.buffers.is_empty() {
        return Ok(String::new());
    }
    open_list(state, ctx, Kind::Buffers, zoom)
}

/// A flat chooser (clients or buffers) over the current pane.
fn open_list(state: &mut State, ctx: &ClientCtx, kind: Kind, zoom: bool) -> Result<String, String> {
    let pane = state.model.resolve_pane(&Target::Current, ctx)?;
    let Some(client) = clients::current_client(state, ctx) else {
        return Ok(String::new());
    };
    if state.trees.is_open(client) {
        return Ok(String::new());
    }
    let none = Tag::Pane(pane);
    let tree = Tree {
        kind,
        pane,
        unzoom: None,
        expanded: BTreeMap::new(),
        current: 0,
        offset: 0,
        height: 0,
        tag: None,
        preview_offset: 0,
        start: (none, none, none),
    };
    open(state, client, tree, zoom);
    Ok(String::new())
}

fn open(state: &mut State, client: ClientId, mut tree: Tree, zoom: bool) {
    if zoom {
        tree.unzoom = zoom_pane(&mut state.model, tree.pane);
    }
    let view = view(state, &tree);
    tree.current = view.current;
    tree.offset = view.offset;
    tree.height = view.height;
    tree.tag = view.lines.get(view.current).map(|l| l.tag);
    state.trees.open.insert(client, tree);
    clients::changed(state);
}

/// Zooms `pane`'s window on it, as `resize-pane -Z` does, unless it is
/// zoomed already or has one pane; the window it zoomed.
fn zoom_pane(model: &mut Model, pane: PaneId) -> Option<WindowId> {
    let window = model.panes.get(&pane)?.window;
    let w = model.windows.get_mut(&window)?;
    if w.zoomed || w.pane_ids().len() < 2 {
        return None;
    }
    w.zoomed = true;
    w.set_active(pane);
    let (cols, rows) = (w.cols, w.rows);
    if let Some(p) = model.panes.get_mut(&pane) {
        p.cols = cols;
        p.rows = rows;
        if let Err(e) = p.handle.resize(cols, rows) {
            eprintln!("grofe server: resizing {pane}: {e}");
        }
    }
    Some(window)
}

/// Closes client `id`'s chooser, unzooming what it zoomed.
fn close(state: &mut State, id: ClientId) {
    if let Some(tree) = state.trees.open.remove(&id) {
        if let Some(window) = tree.unzoom {
            crate::server::dispatch::pane::unzoom(&mut state.model, window);
        }
        clients::changed(state);
    }
}

/// Client `id` went away: its chooser closes.
pub fn client_gone(state: &mut State, id: ClientId) {
    close(state, id);
}

// --- Building ---------------------------------------------------------

/// The items, and whether a filter matched nothing (then everything is
/// listed, as tmux does).
fn build(state: &State, tree: &Tree) -> (Vec<Item>, bool) {
    match &tree.kind {
        Kind::Clients => (client_items(state), false),
        Kind::Buffers => (buffer_items(state), false),
        Kind::Tree { find } => {
            let items = tree_items(state, find.as_deref());
            if find.is_some() && items.is_empty() {
                (tree_items(state, None), true)
            } else {
                (items, false)
            }
        }
    }
}

fn tree_items(state: &State, find: Option<&str>) -> Vec<Item> {
    let model = &state.model;
    let mut sessions = Vec::new();
    for s in model.sessions.values() {
        let mut windows = Vec::new();
        for w in s.windows.iter().filter_map(|w| model.windows.get(w)) {
            let panes = w.pane_ids();
            // tmux drops a one-pane window whose pane does not match, but
            // keeps a window of several panes even when none does.
            let Some(first) = panes.first() else {
                continue;
            };
            if panes.len() == 1 && !matches(state, *first, find) {
                continue;
            }
            let base = model.pane_base_index(w.id);
            let children = panes
                .iter()
                .enumerate()
                .filter(|(_, p)| matches(state, **p, find))
                .map(|(i, p)| Item {
                    tag: Tag::Pane(*p),
                    name: (base + i as u32).to_string(),
                    text: pane_text(state, *p),
                    align: true,
                    children: Vec::new(),
                })
                .collect();
            windows.push(Item {
                tag: Tag::Window(w.id),
                name: w.index.to_string(),
                text: window_text(state, w.active, panes.len()),
                align: true,
                children,
            });
        }
        if windows.is_empty() {
            continue;
        }
        let attached = if s.attached > 0 { " (attached)" } else { "" };
        sessions.push(Item {
            tag: Tag::Session(s.id),
            name: s.name.clone(),
            text: format!("{} windows{attached}", s.windows.len()),
            align: false,
            children: windows,
        });
    }
    sessions
}

/// `find-window`'s filter for one pane, as tmux 3.7b builds it: the window
/// name or the pane title as `#{m:*<pattern>*,...}`, or a line of the
/// pane's visible text as `#{C:<pattern>}` (a `*<pattern>*` glob per line,
/// trailing blanks dropped). The pattern reaches the format through a
/// variable, so its `#`, `,` and `}` are text, never format syntax.
// ponytail: evaluated on every draw of the chooser (tmux filters when it
// rebuilds the tree); fine for the dozens of panes a server holds.
fn matches(state: &State, pane: PaneId, find: Option<&str>) -> bool {
    let Some(pattern) = find else {
        return true;
    };
    let vars = FormatCtx::new(&state.model, pane);
    let server = ServerVars::new(state, pane, &vars);
    let glob = format!("*{pattern}*");
    let by_name = cmd::expand(
        "#{||:#{m:#{find_window_glob},#{window_name}},#{m:#{find_window_glob},#{pane_title}}}",
        &FindVars {
            inner: &server,
            glob: &glob,
            line: "",
        },
    );
    if by_name == "1" {
        return true;
    }
    state.model.panes.get(&pane).is_some_and(|p| {
        let visible = crate::types::CaptureOpts {
            start: None,
            end: None,
            escapes: false,
        };
        p.handle.capture(&visible).lines().any(|line| {
            let line = line.trim_end_matches(|c: char| c.is_ascii_whitespace());
            let vars = FindVars {
                inner: &server,
                glob: &glob,
                line,
            };
            cmd::expand("#{m:#{find_window_glob},#{find_window_line}}", &vars) == "1"
        })
    })
}

/// The variables `find-window`'s filter adds: its glob and the text line
/// being tried.
struct FindVars<'a> {
    inner: &'a dyn cmd::FormatVars,
    glob: &'a str,
    line: &'a str,
}

impl cmd::FormatVars for FindVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        match name {
            "find_window_glob" => Some(self.glob.to_string()),
            "find_window_line" => Some(self.line.to_string()),
            _ => self.inner.get(name),
        }
    }
}

fn expand(state: &State, pane: PaneId, fmt: &str) -> String {
    let vars = FormatCtx::new(&state.model, pane);
    cmd::expand(fmt, &ServerVars::new(state, pane, &vars))
}

/// `: "<title>"` when the pane has a title other than the host's name.
fn title_suffix(state: &State, pane: PaneId) -> String {
    let title = expand(state, pane, "#{pane_title}");
    if title.is_empty() || title == expand(state, pane, "#{host_short}") {
        String::new()
    } else {
        format!(": \"{title}\"")
    }
}

/// tmux's default tree format for a window.
fn window_text(state: &State, active: PaneId, panes: usize) -> String {
    let mut text = expand(state, active, "#{window_name}#{window_flags}");
    if panes == 1 {
        text.push_str(&title_suffix(state, active));
    }
    text
}

/// tmux's default tree format for a pane: its command and
/// `window_pane_printable_flags`.
fn pane_text(state: &State, pane: PaneId) -> String {
    let mut text = expand(state, pane, "#{pane_current_command}");
    let model = &state.model;
    if let Some(w) = model
        .panes
        .get(&pane)
        .and_then(|p| model.windows.get(&p.window))
    {
        if w.active == pane {
            text.push('*');
        }
        if w.last_pane() == Some(pane) {
            text.push('-');
        }
        if w.zoomed && w.active == pane {
            text.push('Z');
        }
    }
    text.push_str(&title_suffix(state, pane));
    text
}

/// tmux's `window-buffer.c` list: newest first (`sort: creation`), each
/// line its name and `#{t/p:buffer_created}: #{buffer_sample}`.
fn buffer_items(state: &State) -> Vec<Item> {
    state
        .buffers
        .iter()
        .map(|b| {
            let created = std::time::UNIX_EPOCH + std::time::Duration::from_secs(b.created);
            Item {
                tag: Tag::Buffer(b.id),
                name: b.name.clone(),
                text: format!(
                    "{}: {}",
                    clients::pretty_time(created),
                    crate::server::buffers::sample(&b.data)
                ),
                align: false,
                children: Vec::new(),
            }
        })
        .collect()
}

fn client_items(state: &State) -> Vec<Item> {
    let mut items: Vec<Item> = state
        .clients
        .iter()
        .map(|(id, c)| {
            let session = state
                .model
                .sessions
                .get(&c.session)
                .map(|s| s.name.clone())
                .unwrap_or_default();
            Item {
                tag: Tag::Client(*id),
                name: c.name.clone(),
                text: format!(
                    "{}: session {session}",
                    clients::pretty_time(c.activity_time)
                ),
                align: false,
                children: Vec::new(),
            }
        })
        .collect();
    items.sort_by(|a, b| a.name.cmp(&b.name));
    items
}

fn lines_of(items: &[Item], tree: &Tree) -> Vec<Line> {
    let mut out = Vec::new();
    add_lines(items, 0, None, tree, &mut out);
    out
}

fn add_lines(
    items: &[Item],
    depth: usize,
    parent: Option<usize>,
    tree: &Tree,
    out: &mut Vec<Line>,
) {
    let flat = items.iter().all(|i| i.children.is_empty());
    for (k, item) in items.iter().enumerate() {
        let expanded = tree.expanded.get(&item.tag).copied().unwrap_or(true);
        let at = out.len();
        out.push(Line {
            tag: item.tag,
            depth,
            last: k + 1 == items.len(),
            flat,
            parent,
            has_children: !item.children.is_empty(),
            expanded,
            name: item.name.clone(),
            text: item.text.clone(),
            align: item.align,
        });
        if expanded {
            add_lines(&item.children, depth + 1, Some(at), tree, out);
        }
    }
}

/// `mode_tree_set_height` for the normal preview: the list's rows out of
/// the pane's `rows`.
fn list_height(rows: usize, lines: usize) -> usize {
    let mut h = (rows / 3) * 2;
    if h > lines {
        h = rows / 2;
    }
    if h < 10 {
        h = rows;
    }
    if rows - h < 2 {
        h = rows;
    }
    h
}

/// `mode_tree_set_current`: the line for `tag`, scrolled into view against
/// the previous `height`; with no such line, `current` clamped.
fn set_current(
    lines: &[Line],
    tag: Option<Tag>,
    current: usize,
    offset: usize,
    height: usize,
) -> (usize, usize) {
    let scrolled = |c: usize| {
        if height > 0 && c > height - 1 {
            c - height + 1
        } else {
            0
        }
    };
    if let Some(found) = tag.and_then(|t| lines.iter().position(|l| l.tag == t)) {
        return (found, scrolled(found));
    }
    if current >= lines.len() && !lines.is_empty() {
        let last = lines.len() - 1;
        return (last, scrolled(last));
    }
    (current, offset)
}

/// The tree as it is now: rebuilt from the model, the current line found
/// again by its item (keeping the scroll while it is where it was).
fn view(state: &State, tree: &Tree) -> View {
    let (items, no_matches) = build(state, tree);
    let lines = lines_of(&items, tree);
    let rows = state
        .model
        .panes
        .get(&tree.pane)
        .map_or(0, |p| usize::from(p.rows));
    let (current, offset) = if lines.get(tree.current).map(|l| l.tag) == tree.tag {
        (tree.current, tree.offset)
    } else {
        set_current(&lines, tree.tag, tree.current, tree.offset, tree.height)
    };
    let height = list_height(rows, lines.len());
    View {
        lines,
        current,
        offset,
        height,
        no_matches,
        rows,
    }
}

// --- Keys ---------------------------------------------------------------

/// Takes a client's keys while its chooser is open, as tmux gives a pane
/// mode the keys the client's key table does not bind: after the prefix, the
/// prefix key itself, and a key bound in `root` still go to the tables.
pub struct TreeHook;

impl InputHook for TreeHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        _now: Instant,
    ) -> Option<Routed> {
        if !state.trees.is_open(client) {
            return None;
        }
        let tree_pane = state.trees.open.get(&client).map(|t| t.pane)?;
        let Some(window) = state.model.panes.get(&tree_pane).map(|p| p.window) else {
            close(state, client);
            return None;
        };
        // Keys go to the chooser only while its pane is the client's
        // active pane, as tmux gives a mode the keys of its own pane.
        let model = &state.model;
        let shown = state
            .clients
            .get(&client)
            .and_then(|c| model.sessions.get(&c.session))
            .is_some_and(|s| {
                s.active == window
                    && model
                        .windows
                        .get(&window)
                        .is_some_and(|w| w.active == tree_pane)
            });
        if !shown {
            return None;
        }
        let key = match input {
            Input::Key(key) => *key,
            Input::Paste { .. } => return Some(Routed::Consumed),
            Input::Mouse(_) | Input::Focus(_) => return None,
        };
        if goes_to_tables(state, client, &key) {
            return None;
        }
        let routed = key_press(state, client, &key);
        clients::mark_dirty(state, client);
        Some(routed)
    }
}

/// Whether `key` is the client's to route through its key tables.
fn goes_to_tables(state: &State, client: ClientId, key: &Key) -> bool {
    let Some(c) = state.clients.get(&client) else {
        return true;
    };
    if c.keys.table() != ROOT_TABLE {
        return true;
    }
    let settings = clients::key_settings(&state.model, c.session);
    if settings.prefix.is_some_and(|p| p.matches(key)) {
        return true;
    }
    state.registry.key_tables.get(ROOT_TABLE, key).is_some()
}

/// The line key `(0)`..`(9)`, `(M-a)`..`(M-z)` of line `i`.
fn line_key(i: usize) -> Option<String> {
    match i {
        0..=9 => Some(i.to_string()),
        10..=35 => Some(format!("M-{}", char::from(b'a' + (i - 10) as u8))),
        _ => None,
    }
}

/// The line a key chooses directly.
fn line_of_key(key: &Key, lines: usize) -> Option<usize> {
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    let (m, plain) = (key.mods, Default::default());
    let i = match c {
        '0'..='9' if m == plain => c as usize - '0' as usize,
        'a'..='z' if m.alt && !m.ctrl && !m.super_ => 10 + (c as usize - 'a' as usize),
        _ => return None,
    };
    (i < lines).then_some(i)
}

fn key_press(state: &mut State, client: ClientId, key: &Key) -> Routed {
    let Some(tree) = state.trees.open.get(&client) else {
        return Routed::Consumed;
    };
    let v = view(state, tree);
    if v.lines.is_empty() {
        close(state, client);
        return Routed::Consumed;
    }
    let (mut current, mut offset) = (v.current, v.offset);
    let (height, n) = (v.height.max(1), v.lines.len());
    let before = v.lines[current].tag;
    let mut rebuild = false;
    let mut expanded: Vec<(Tag, bool)> = Vec::new();
    let mut preview_step = 0;
    let mut start_tags = None;

    let up = |c: &mut usize, o: &mut usize, wrap: bool| {
        if *c == 0 {
            if wrap {
                *c = n - 1;
                if n >= height {
                    *o = n - height;
                }
            }
        } else {
            *c -= 1;
            if *c < *o {
                *o -= 1;
            }
        }
    };
    let down = |c: &mut usize, o: &mut usize, wrap: bool| {
        if *c == n - 1 {
            if wrap {
                *c = 0;
                *o = 0;
            }
        } else {
            *c += 1;
            if *c > *o + height - 1 {
                *o += 1;
            }
        }
    };

    if let Some(i) = line_of_key(key, n) {
        return choose(state, client, v.lines[i].tag);
    }
    let (m, plain) = (key.mods, Default::default());
    let ctrl = |c: char| key.code == KeyCode::Char(c) && m.ctrl && !m.alt;
    let alt = |c: char| key.code == KeyCode::Char(c) && m.alt && !m.ctrl;
    let is = |c: char| key.code == KeyCode::Char(c) && m == plain;
    let code = if m == plain { Some(key.code) } else { None };
    if is('q') || code == Some(KeyCode::Escape) || ctrl('[') || ctrl('g') {
        close(state, client);
        return Routed::Consumed;
    } else if code == Some(KeyCode::Enter) || ctrl('m') {
        return choose(state, client, v.lines[current].tag);
    } else if is('d') && matches!(tree.kind, Kind::Buffers) {
        // `window_buffer_key`: the current buffer goes, and the chooser
        // with the last one.
        if let Tag::Buffer(id) = v.lines[current].tag
            && let Some(name) = state.buffers.by_id(id).map(|b| b.name.clone())
        {
            state.buffers.delete(&name);
        }
        if state.buffers.is_empty() {
            close(state, client);
        }
        return Routed::Consumed;
    } else if code == Some(KeyCode::Up) || is('k') || ctrl('p') {
        up(&mut current, &mut offset, true);
    } else if code == Some(KeyCode::Down) || is('j') || ctrl('n') {
        down(&mut current, &mut offset, true);
    } else if code == Some(KeyCode::PageUp) || ctrl('b') {
        for _ in 0..height {
            if current == 0 {
                break;
            }
            up(&mut current, &mut offset, true);
        }
    } else if code == Some(KeyCode::PageDown) || ctrl('f') {
        for _ in 0..height {
            if current == n - 1 {
                break;
            }
            down(&mut current, &mut offset, true);
        }
    } else if is('g') || code == Some(KeyCode::Home) {
        current = 0;
        offset = 0;
    } else if is('G') || code == Some(KeyCode::End) {
        current = n - 1;
        offset = if current > height - 1 {
            current - height + 1
        } else {
            0
        };
    } else if code == Some(KeyCode::Left) || is('h') || is('-') {
        let line = &v.lines[current];
        let target = if line.flat || !line.expanded {
            line.parent
        } else {
            Some(current)
        };
        match target {
            None => up(&mut current, &mut offset, false),
            Some(p) => {
                expanded.push((v.lines[p].tag, false));
                current = p;
                rebuild = true;
            }
        }
    } else if code == Some(KeyCode::Right) || is('l') || is('+') {
        let line = &v.lines[current];
        if line.flat || line.expanded {
            down(&mut current, &mut offset, false);
        } else {
            expanded.push((line.tag, true));
            rebuild = true;
        }
    } else if alt('-') || alt('+') {
        for line in v.lines.iter().filter(|l| l.depth == 0) {
            expanded.push((line.tag, alt('+')));
        }
        rebuild = true;
    } else if is('<') {
        preview_step = -1;
    } else if is('>') {
        preview_step = 1;
    } else if is('H') {
        start_tags = Some(tree.start);
        rebuild = true;
    }

    let rows = v.rows;
    let Some(tree) = state.trees.open.get_mut(&client) else {
        return Routed::Consumed;
    };
    for (tag, on) in expanded {
        tree.expanded.insert(tag, on);
    }
    tree.current = current;
    tree.offset = offset;
    tree.tag = v.lines.get(current).map(|l| l.tag);
    if let Some((s, w, _)) = start_tags {
        tree.expanded.insert(s, true);
        tree.expanded.insert(w, true);
    }
    if rebuild {
        // `mode_tree_build`: the current item found again by its tag.
        let Some(tree) = state.trees.open.get(&client) else {
            return Routed::Consumed;
        };
        let (items, _) = build(state, tree);
        let lines = lines_of(&items, tree);
        let mut at = set_current(&lines, tree.tag, tree.current, tree.offset, tree.height);
        if let Some((_, w, p)) = start_tags {
            let find = |tag| lines.iter().position(|l| l.tag == tag);
            if find(p).is_some() {
                at = set_current(&lines, Some(p), at.0, at.1, tree.height);
            } else if find(w).is_some() {
                at = set_current(&lines, Some(w), at.0, at.1, tree.height);
            }
        }
        let height = list_height(rows, lines.len());
        let tag = lines.get(at.0).map(|l| l.tag);
        let Some(tree) = state.trees.open.get_mut(&client) else {
            return Routed::Consumed;
        };
        (tree.current, tree.offset) = at;
        tree.height = height;
        tree.tag = tag;
    }
    let preview_offset = match state.trees.open.get_mut(&client) {
        Some(tree) if tree.tag != Some(before) => {
            tree.preview_offset = 0;
            0
        }
        Some(tree) => tree.preview_offset + preview_step,
        None => return Routed::Consumed,
    };
    if preview_step != 0 {
        let clamped = clamp_preview_offset(state, client, preview_offset);
        if let Some(tree) = state.trees.open.get_mut(&client) {
            tree.preview_offset = clamped;
        }
    }
    Routed::Consumed
}

/// The chosen item: the chooser closes, then the client switches to it
/// (`switch-client -t`, tmux's `switch-client -Zt '%%'` with grofe's id
/// targets), or for `choose-client` the chosen client detaches
/// (`detach-client -t`, tmux's default template).
fn choose(state: &mut State, client: ClientId, tag: Tag) -> Routed {
    let pane = state.trees.open.get(&client).map(|t| t.pane);
    let argv: Vec<String> = match tag {
        Tag::Session(s) => vec!["switch-client".into(), "-t".into(), s.to_string()],
        Tag::Window(w) => vec!["switch-client".into(), "-t".into(), w.to_string()],
        Tag::Pane(p) => vec!["switch-client".into(), "-t".into(), p.to_string()],
        Tag::Client(c) => match state.clients.get(&c) {
            Some(entry) => vec!["detach-client".into(), "-t".into(), entry.name.clone()],
            None => Vec::new(),
        },
        // tmux's template, `paste-buffer -p -b '%%'`, into the pane.
        Tag::Buffer(id) => match (state.buffers.by_id(id), pane) {
            (Some(b), Some(p)) => ["paste-buffer", "-p", "-b", &b.name, "-t", &p.to_string()]
                .map(String::from)
                .to_vec(),
            _ => Vec::new(),
        },
    };
    close(state, client);
    if argv.is_empty() {
        return Routed::Consumed;
    }
    Routed::Command(vec![argv], Dialect::Grofe)
}

/// One preview in a strip: the pane shown, its label, and whether it is
/// the current window or active pane (the label's colour).
type Preview = (PaneId, String, bool);

/// The previews a session or window line shows side by side, and which
/// is its current window or active pane.
fn strip_of(state: &State, tag: Tag) -> Option<(Vec<Preview>, usize)> {
    let model = &state.model;
    match tag {
        Tag::Session(s) => {
            let s = model.sessions.get(&s)?;
            let entries: Vec<_> = s
                .windows
                .iter()
                .filter_map(|w| model.windows.get(w))
                .map(|w| {
                    (
                        w.active,
                        format!("{}:{}", w.index, w.name),
                        w.id == s.active,
                    )
                })
                .collect();
            let current = entries.iter().position(|e| e.2).unwrap_or(0);
            Some((entries, current))
        }
        Tag::Window(w) => {
            let w = model.windows.get(&w)?;
            let base = model.pane_base_index(w.id);
            let entries: Vec<_> = w
                .pane_ids()
                .into_iter()
                .enumerate()
                .map(|(i, p)| {
                    let label = format!("{}:{}", base + i as u32, state.passthrough.title(p));
                    (p, label, p == w.active)
                })
                .collect();
            let current = entries.iter().position(|e| e.2).unwrap_or(0);
            Some((entries, current))
        }
        Tag::Pane(_) | Tag::Client(_) | Tag::Buffer(_) => None,
    }
}

/// `offset` kept inside the previews the current line has, as
/// `window_tree_draw_session` and `_window` clamp `data->offset`.
fn clamp_preview_offset(state: &State, client: ClientId, offset: i32) -> i32 {
    let Some(tree) = state.trees.open.get(&client) else {
        return 0;
    };
    let v = view(state, tree);
    let Some(tag) = v.lines.get(v.current).map(|l| l.tag) else {
        return 0;
    };
    let cols = state
        .model
        .panes
        .get(&tree.pane)
        .map_or(0, |p| usize::from(p.cols));
    let Some((entries, current)) = strip_of(state, tag) else {
        return 0;
    };
    strip(entries.len(), current, cols.saturating_sub(4), offset).map_or(0, |g| g.offset)
}

/// How a strip of `total` previews fits `sx` columns, with the current
/// one in view and scrolled by `offset` (clamped): tmux's arithmetic in
/// `window_tree_draw_session`.
struct Strip {
    start: usize,
    end: usize,
    left: bool,
    right: bool,
    each: usize,
    remaining: usize,
    offset: i32,
}

fn strip(total: usize, current: usize, sx: usize, offset: i32) -> Option<Strip> {
    if total == 0 {
        return None;
    }
    let visible = if sx / total < 24 {
        (sx / 24).max(1)
    } else {
        total
    };
    let visible = visible.min(total);
    let (start, end) = if current < visible {
        (0, visible)
    } else if current >= total - visible {
        (total - visible, total)
    } else {
        let start = current - visible / 2;
        (start, start + visible)
    };
    let offset = offset.clamp(-(start as i32), (total - end) as i32);
    let start = (start as i32 + offset) as usize;
    let end = (end as i32 + offset) as usize;
    let (mut left, mut right) = (start != 0, end != total);
    if (left && right && sx <= 6) || ((left || right) && sx <= 3) {
        left = false;
        right = false;
    }
    let room = match (left, right) {
        (true, true) => sx - 6,
        (true, false) | (false, true) => sx - 3,
        (false, false) => sx,
    };
    let each = room / visible;
    if each == 0 {
        return None;
    }
    Some(Strip {
        start,
        end,
        left,
        right,
        each,
        remaining: room - visible * each,
        offset,
    })
}

// --- Drawing ------------------------------------------------------------

/// Draws a client's chooser over its pane, when that pane is on screen.
pub struct TreeOverlay;

impl Overlay for TreeOverlay {
    fn draw(&self, state: &State, client: ClientId, cx: &OverlayCtx, screen: &mut Screen) {
        let Some(tree) = state.trees.open.get(&client) else {
            return;
        };
        let Some(geom) = geometry(state, tree.pane, cx) else {
            return;
        };
        let v = view(state, tree);
        let mut canvas = Canvas { screen, geom };
        draw(state, tree, &v, &mut canvas);
        canvas.screen.cursor = None;
    }
}

/// Where a pane's cells land on a client's screen: its origin in client
/// cells (off screen when the client crops the window), the part that is
/// visible, and the pane's size.
struct Geom {
    origin: (i32, i32),
    clip: Rect,
    cols: usize,
    rows: usize,
}

fn geometry(state: &State, pane: PaneId, cx: &OverlayCtx) -> Option<Geom> {
    let clip = cx.panes.iter().find(|(p, _)| *p == pane).map(|(_, r)| *r)?;
    let model = &state.model;
    let w = model.windows.get(&model.panes.get(&pane)?.window)?;
    let (_, at) = clients::visible_panes(w)
        .into_iter()
        .find(|(id, _)| *id == pane)?;
    Some(Geom {
        origin: (
            i32::from(at.x) - i32::from(cx.viewport.0),
            i32::from(at.y) - i32::from(cx.viewport.1) + i32::from(cx.window_top),
        ),
        clip,
        cols: usize::from(at.cols),
        rows: usize::from(at.rows),
    })
}

/// A pane-sized area of a client's screen, drawn in pane cells.
struct Canvas<'a> {
    screen: &'a mut Screen,
    geom: Geom,
}

fn blank(style: &Style) -> SnapCell {
    SnapCell {
        c: ' ',
        fg: style.fg,
        bg: style.bg,
        flags: style.attrs,
        zerowidth: Vec::new(),
        underline_color: style.underline_color,
        hyperlink: None,
    }
}

impl Canvas<'_> {
    /// The screen index of pane cell `(x, y)`, if it is visible.
    fn at(&self, x: usize, y: usize) -> Option<usize> {
        let g = &self.geom;
        if x >= g.cols || y >= g.rows {
            return None;
        }
        let (sx, sy) = (g.origin.0 + x as i32, g.origin.1 + y as i32);
        let c = g.clip;
        let inside = sx >= i32::from(c.x)
            && sx < i32::from(c.x) + i32::from(c.cols)
            && sy >= i32::from(c.y)
            && sy < i32::from(c.y) + i32::from(c.rows);
        let (sx, sy) = (usize::try_from(sx).ok()?, usize::try_from(sy).ok()?);
        (inside && sx < usize::from(self.screen.cols) && sy < usize::from(self.screen.rows))
            .then(|| sy * usize::from(self.screen.cols) + sx)
    }

    fn cell(&mut self, x: usize, y: usize, cell: SnapCell) {
        if let Some(i) = self.at(x, y) {
            self.screen.cells[i] = cell;
        }
    }

    /// Puts `c` at `(x, y)`; a wide character takes two cells. Returns its
    /// width. A control character (no width) is dropped: text from a pane
    /// never reaches the terminal as bytes.
    fn put(&mut self, x: usize, y: usize, c: char, style: &Style) -> usize {
        let Some(width) = UnicodeWidthChar::width(c) else {
            return 0;
        };
        if width == 0 {
            return 0;
        }
        let mut cell = blank(style);
        cell.c = c;
        if width == 2 {
            cell.flags |= CellFlags::WIDE_CHAR;
            let mut spacer = blank(style);
            spacer.flags |= CellFlags::WIDE_CHAR_SPACER;
            self.cell(x + 1, y, spacer);
        }
        self.cell(x, y, cell);
        width
    }

    /// Writes `text` from `(x, y)` in at most `max` cells; the cells used.
    fn text(&mut self, x: usize, y: usize, text: &str, style: &Style, max: usize) -> usize {
        let mut col = 0;
        for c in text.chars() {
            let w = UnicodeWidthChar::width(c).unwrap_or(0);
            if col + w > max {
                break;
            }
            col += self.put(x + col, y, c, style);
        }
        col
    }

    fn fill(&mut self, x: usize, y: usize, n: usize, style: &Style) {
        for i in 0..n {
            self.cell(x + i, y, blank(style));
        }
    }

    /// `screen_write_box`: a `w` by `h` box in the default lines.
    fn frame(&mut self, x: usize, y: usize, w: usize, h: usize) {
        if w < 2 || h < 2 {
            return;
        }
        let style = Style::default();
        for i in 0..w {
            let (top, bottom) = match i {
                0 => ('┌', '└'),
                i if i + 1 == w => ('┐', '┘'),
                _ => ('─', '─'),
            };
            self.put(x + i, y, top, &style);
            self.put(x + i, y + h - 1, bottom, &style);
        }
        for j in 1..h - 1 {
            self.put(x, y + j, '│', &style);
            self.put(x + w - 1, y + j, '│', &style);
        }
    }

    fn vline(&mut self, x: usize, y: usize, h: usize) {
        for j in 0..h {
            self.put(x, y + j, '│', &Style::default());
        }
    }
}

/// `mode_tree_draw`.
fn draw(state: &State, tree: &Tree, v: &View, cv: &mut Canvas) {
    let (w, sy) = (cv.geom.cols, cv.geom.rows);
    let plain = Style::default();
    let mode = status::parse_style(MODE_STYLE);
    let mode_bg = Style {
        bg: mode.bg,
        ..Style::default()
    };
    for y in 0..sy {
        cv.fill(0, y, w, &plain);
    }
    if v.lines.is_empty() {
        return;
    }
    let keylen = (0..v.lines.len())
        .filter_map(line_key)
        .map(|k| k.len() + 3)
        .max()
        .unwrap_or(0);
    let mut alignlen: BTreeMap<usize, usize> = BTreeMap::new();
    for line in v.lines.iter().filter(|l| l.align) {
        let len = alignlen.entry(line.depth).or_default();
        *len = (*len).max(line.name.len());
    }
    let h = v.height;
    for (i, line) in v.lines.iter().enumerate().skip(v.offset).take(h) {
        let y = i - v.offset;
        let key = line_key(i).map(|k| format!("({k})")).unwrap_or_default();
        let symbol = if line.flat {
            ""
        } else if !line.has_children {
            "  "
        } else if line.expanded {
            "- "
        } else {
            "+ "
        };
        let mut start = String::new();
        if line.depth > 0 {
            let parent_last = line.parent.is_some_and(|p| v.lines[p].last);
            for _ in 1..line.depth {
                start.push_str(if parent_last { "    " } else { "│   " });
            }
            start.push_str(if line.last { "└─> " } else { "├─> " });
        }
        start.push_str(symbol);
        let name = if line.align {
            let width = alignlen.get(&line.depth).copied().unwrap_or(0);
            format!("{:>width$}", line.name)
        } else {
            line.name.clone()
        };
        let text = format!("{key:<keylen$}{start}{name}: ");
        let width = UnicodeWidthStr::width(text.as_str()).min(w);
        let style = if i == v.current {
            cv.fill(0, y, w, &mode_bg);
            mode
        } else {
            plain
        };
        let used = cv.text(0, y, &text, &style, w);
        cv.text(used, y, &line.text, &style, w - width);
    }

    if sy <= 4 || h < 2 || sy - h <= 4 || w <= 4 {
        return;
    }
    let line = &v.lines[v.current];
    cv.frame(0, h, w, sy - h);
    let order = match tree.kind {
        Kind::Tree { .. } => "index",
        Kind::Clients => "name",
        Kind::Buffers => "creation",
    };
    let title = format!(" {} (sort: {order})", line.name);
    if w - 2 >= title.len() {
        let mut x = 1 + cv.text(1, h, &title, &plain, w - 2);
        let filter = matches!(&tree.kind, Kind::Tree { find: Some(_) });
        let n = if v.no_matches {
            "no matches".len()
        } else {
            "active".len()
        };
        if filter && w - 2 >= title.len() + 10 + n + 2 {
            x += cv.text(x, h, " (filter: ", &plain, w);
            x += if v.no_matches {
                cv.text(x, h, "no matches", &mode, w)
            } else {
                cv.text(x, h, "active", &plain, w)
            };
            cv.text(x, h, ") ", &plain, w);
        } else {
            cv.text(x, h, " ", &plain, w);
        }
    }
    let (box_x, box_y) = (w - 4, sy - h - 2);
    if box_x == 0 || box_y == 0 {
        return;
    }
    let (x, y) = (2, h + 1);
    match line.tag {
        Tag::Session(_) | Tag::Window(_) => {
            if let Some((entries, current)) = strip_of(state, line.tag) {
                draw_strip(
                    state,
                    cv,
                    (x, y, box_x, box_y),
                    &entries,
                    current,
                    tree.preview_offset,
                );
            }
        }
        Tag::Pane(p) => preview(state, cv, p, (x, y, box_x, box_y)),
        Tag::Client(c) => draw_client(state, cv, c, (x, y, box_x, box_y)),
        Tag::Buffer(id) => draw_buffer(state, cv, id, (x, y, box_x, box_y)),
    }
}

/// `window_buffer_draw`: the buffer's first lines, each escaped as
/// `vis(3)` writes it.
fn draw_buffer(
    state: &State,
    cv: &mut Canvas,
    id: u64,
    (cx, cy, sx, sy): (usize, usize, usize, usize),
) {
    let Some(b) = state.buffers.by_id(id) else {
        return;
    };
    let lines = b.data.split(|c| *c == b'\n');
    for (i, line) in lines.take(sy).enumerate() {
        let text = crate::server::buffers::escape(line);
        cv.text(cx, cy + i, &text, &Style::default(), sx);
    }
}

/// `window_tree_draw_session` and `_window`: the previews side by side,
/// each with its label, `<` and `>` when some are scrolled off.
fn draw_strip(
    state: &State,
    cv: &mut Canvas,
    (cx, cy, sx, sy): (usize, usize, usize, usize),
    entries: &[Preview],
    current: usize,
    offset: i32,
) {
    let Some(g) = strip(entries.len(), current, sx, offset) else {
        return;
    };
    let plain = Style::default();
    if g.left {
        cv.vline(cx + 2, cy, sy);
        cv.put(cx, cy + sy / 2, '<', &plain);
    }
    if g.right {
        cv.vline(cx + sx - 3, cy, sy);
        cv.put(cx + sx - 1, cy + sy / 2, '>', &plain);
    }
    for (i, (pane, label, active)) in entries[g.start..g.end].iter().enumerate() {
        let x = if g.left { 3 } else { 0 } + i * g.each;
        let last = g.start + i + 1 == g.end;
        let width = if last {
            g.each + g.remaining
        } else {
            g.each - 1
        };
        preview(state, cv, *pane, (cx + x, cy, width, sy));
        let style = status::parse_style(if *active {
            PREVIEW_ACTIVE_STYLE
        } else {
            PREVIEW_STYLE
        });
        draw_label(cv, (cx + x, cy, width, sy), &style, label);
        if !last {
            cv.vline(cx + x + width, cy, sy);
        }
    }
}

/// `window_tree_draw_label`: `label` in a small box in the middle of the
/// `sx` by `sy` area at `(px, py)`.
fn draw_label(
    cv: &mut Canvas,
    (px, py, sx, sy): (usize, usize, usize, usize),
    style: &Style,
    label: &str,
) {
    if sx < 5 || sy < 3 {
        return;
    }
    let mut label: String = label.chars().filter(|c| !c.is_control()).collect();
    if UnicodeWidthStr::width(label.as_str()) > sx - 4 {
        let mut kept = String::new();
        let mut used = 0;
        for c in label.chars() {
            let w = UnicodeWidthChar::width(c).unwrap_or(0);
            if used + w > sx - 4 {
                break;
            }
            used += w;
            kept.push(c);
        }
        label = kept;
    }
    let width = UnicodeWidthStr::width(label.as_str());
    if width == 0 {
        return;
    }
    let (ox, oy) = ((sx - width).div_ceil(2), sy.div_ceil(2));
    cv.frame(px + ox - 2, py + oy - 1, width + 4, 3);
    cv.fill(px + ox - 1, py + oy, width + 2, &Style::default());
    cv.text(px + ox, py + oy, &label, style, width);
}

/// `screen_write_preview`: an `nx` by `ny` part of `pane`'s screen at
/// `(x, y)`, around its cursor when the cursor is on (its cell reversed),
/// else from the top left.
fn preview(
    state: &State,
    cv: &mut Canvas,
    pane: PaneId,
    (x, y, nx, ny): (usize, usize, usize, usize),
) {
    let Some(p) = state.model.panes.get(&pane) else {
        return;
    };
    let (cols, rows) = (usize::from(p.cols), usize::from(p.rows));
    let snap = p.handle.snapshot_rows(0..p.rows);
    let cursor = snap.cursor;
    let around = |c: usize, n: usize, size: usize| {
        let at = c.saturating_sub(n / 3);
        if at + n > size {
            size.saturating_sub(n)
        } else {
            at
        }
    };
    let (px, py) = if cursor.visible {
        (
            around(usize::from(cursor.col), nx, cols),
            around(usize::from(cursor.row), ny, rows),
        )
    } else {
        (0, 0)
    };
    let plain = Style::default();
    for j in 0..ny {
        for i in 0..nx {
            let cell = snap
                .rows
                .get(py + j)
                .and_then(|r| r.get(px + i))
                .cloned()
                .unwrap_or_else(|| blank(&plain));
            cv.cell(x + i, y + j, cell);
        }
    }
    if cursor.visible {
        let (ccol, crow) = (usize::from(cursor.col), usize::from(cursor.row));
        if let Some(mut cell) = snap.rows.get(crow).and_then(|r| r.get(ccol)).cloned() {
            cell.flags |= CellFlags::INVERSE;
            cv.cell(x + ccol - px, y + crow - py, cell);
        }
    }
}

/// `window_client_draw`: the client's current pane, a line, and its
/// status line as last drawn.
fn draw_client(
    state: &State,
    cv: &mut Canvas,
    client: ClientId,
    (cx, cy, sx, sy): (usize, usize, usize, usize),
) {
    let Some(c) = state.clients.get(&client) else {
        return;
    };
    let model = &state.model;
    let Some(pane) = model
        .sessions
        .get(&c.session)
        .and_then(|s| model.windows.get(&s.active))
        .map(|w| w.active)
    else {
        return;
    };
    let (rows, top) = clients::drawn_status(state, client).unwrap_or((&[], false));
    let lines = if rows.len() >= sy { 0 } else { rows.len() };
    let at = if top { lines } else { 0 };
    if sy < 2 + lines {
        return;
    }
    preview(state, cv, pane, (cx, cy + at, sx, sy - 2 - lines));
    let rule = if at != 0 { cy + 2 } else { cy + sy - 1 - lines };
    for i in 0..sx {
        cv.put(cx + i, rule, '─', &Style::default());
    }
    let first = if at != 0 { cy } else { cy + sy - lines };
    for (j, row) in rows.iter().take(lines).enumerate() {
        let mut col = 0;
        for span in &row.line.spans {
            let used = cv.text(cx + col, first + j, &span.text, &span.style, sx - col);
            col += used;
            if col >= sx {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_keys_are_digits_then_meta_letters() {
        assert_eq!(line_key(0).as_deref(), Some("0"));
        assert_eq!(line_key(9).as_deref(), Some("9"));
        assert_eq!(line_key(10).as_deref(), Some("M-a"));
        assert_eq!(line_key(35).as_deref(), Some("M-z"));
        assert_eq!(line_key(36), None);
        let key = |name| crate::server::input::parse_key(name).expect("key");
        assert_eq!(line_of_key(&key("3"), 5), Some(3));
        assert_eq!(line_of_key(&key("7"), 5), None);
        assert_eq!(line_of_key(&key("M-b"), 20), Some(11));
        assert_eq!(line_of_key(&key("b"), 20), None);
    }

    /// `mode_tree_set_height` in a 23-row pane: two thirds of it, half when
    /// the list is shorter, the whole pane when that leaves under 10 rows.
    #[test]
    fn the_list_takes_mode_trees_height() {
        assert_eq!(list_height(23, 5), 11);
        assert_eq!(list_height(23, 27), 14);
        assert_eq!(list_height(15, 3), 15);
        assert_eq!(list_height(12, 20), 12);
    }

    /// `window_tree_draw_session`'s strip at 76 columns: side by side while
    /// each gets 24 columns, else three with `<`/`>` and the current one in
    /// view, scrolled by the offset within its bounds.
    #[test]
    fn previews_share_the_width_as_tmux_does() {
        let g = strip(2, 0, 76, 0).expect("fits");
        assert_eq!((g.start, g.end, g.left, g.right), (0, 2, false, false));
        assert_eq!((g.each, g.remaining), (38, 0));
        let g = strip(7, 3, 76, 0).expect("fits");
        assert_eq!((g.start, g.end, g.left, g.right), (2, 5, true, true));
        assert_eq!((g.each, g.remaining), (23, 1));
        let g = strip(7, 3, 76, 9).expect("fits");
        assert_eq!((g.start, g.end, g.offset), (4, 7, 2));
        assert!(g.left && !g.right);
        let g = strip(7, 0, 76, -3).expect("fits");
        assert_eq!((g.start, g.offset), (0, 0));
        assert!(strip(0, 0, 76, 0).is_none());
        assert!(strip(3, 0, 0, 0).is_none());
    }

    fn line(tag: Tag) -> Line {
        Line {
            tag,
            depth: 0,
            last: false,
            flat: true,
            parent: None,
            has_children: false,
            expanded: true,
            name: String::new(),
            text: String::new(),
            align: false,
        }
    }

    #[test]
    fn the_current_line_is_found_again_by_its_item() {
        let lines: Vec<Line> = (0..20).map(|i| line(Tag::Pane(PaneId(i)))).collect();
        assert_eq!(
            set_current(&lines, Some(Tag::Pane(PaneId(15))), 0, 0, 10),
            (15, 6)
        );
        assert_eq!(
            set_current(&lines, Some(Tag::Pane(PaneId(3))), 9, 5, 10),
            (3, 0)
        );
        // A vanished item keeps the line, clamped to the list.
        assert_eq!(
            set_current(&lines, Some(Tag::Pane(PaneId(99))), 25, 0, 10),
            (19, 10)
        );
        assert_eq!(
            set_current(&lines, Some(Tag::Pane(PaneId(99))), 4, 2, 10),
            (4, 2)
        );
        // Before the first build the height is 0 and nothing scrolls.
        assert_eq!(
            set_current(&lines, Some(Tag::Pane(PaneId(15))), 0, 0, 0),
            (15, 0)
        );
    }
}
