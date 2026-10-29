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

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::cmd::{self, ChooseOpts, Resolve, Target};
use crate::protocol::{ClientCtx, Dialect};
use crate::server::dispatch::options::{self, FormatCtx, Level, ServerVars};
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
    /// `-F -f -K -O -r -N -y -G`, the template, and the target pane.
    pub chooser: ChooseOpts,
    /// The dialect of the command that opened it, which its template runs
    /// in.
    pub dialect: Dialect,
}

impl Opts {
    pub fn new(dialect: Dialect) -> Opts {
        Opts {
            zoom: false,
            sessions: false,
            windows: false,
            find: None,
            chooser: ChooseOpts::default(),
            dialect,
        }
    }
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

/// Whether a chooser is open over the pane, which puts it in a mode.
pub fn is_over(state: &State, pane: PaneId) -> bool {
    state.trees.open.values().any(|t| t.pane == pane)
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

/// A sort order (tmux's `sort_order`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    Activity,
    Creation,
    Index,
    Modifier,
    Name,
    Unsorted,
    Size,
    Z,
}

impl Order {
    /// `sort_order_from_string`.
    fn parse(s: &str) -> Option<Order> {
        Some(match s.to_ascii_lowercase().as_str() {
            "activity" => Order::Activity,
            "creation" => Order::Creation,
            "index" | "key" => Order::Index,
            "modifier" => Order::Modifier,
            "name" | "title" => Order::Name,
            "order" => Order::Unsorted,
            "size" => Order::Size,
            "z" => Order::Z,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Order::Activity => "activity",
            Order::Creation => "creation",
            Order::Index => "index",
            Order::Modifier => "modifier",
            Order::Name => "name",
            Order::Unsorted => "order",
            Order::Size => "size",
            Order::Z => "z",
        }
    }
}

impl Kind {
    /// The orders `O` steps through (`window_tree_order_seq` and the
    /// others).
    fn orders(&self) -> &'static [Order] {
        match self {
            Kind::Tree { .. } => &[Order::Index, Order::Name, Order::Activity, Order::Z],
            Kind::Clients => &[Order::Name, Order::Size, Order::Creation, Order::Activity],
            Kind::Buffers => &[Order::Creation, Order::Name, Order::Size],
        }
    }

    /// The command a chosen item runs when none is given.
    fn default_template(&self) -> &'static str {
        match self {
            Kind::Tree { .. } => "switch-client -Zt '%%'",
            Kind::Clients => "detach-client -t '%%'",
            Kind::Buffers => "paste-buffer -p -b '%%'",
        }
    }

    fn default_format(&self) -> &'static str {
        match self {
            Kind::Tree { .. } => TREE_FORMAT,
            Kind::Clients => "#{t/p:client_activity}: session #{session_name}",
            Kind::Buffers => "#{t/p:buffer_created}: #{buffer_sample}",
        }
    }
}

/// `WINDOW_TREE_DEFAULT_FORMAT`.
const TREE_FORMAT: &str = "#{?pane_format,\
#{?pane_marked,#[reverse],}#{?pane_floating_flag,#[italics],}\
#{pane_current_command}#{pane_flags}\
#{?#{&&:#{pane_title},#{!=:#{pane_title},#{host_short}}},: \"#{pane_title}\",}\
,window_format,\
#{?window_marked_flag,#[reverse],}\
#{window_name}#{window_flags}\
#{?#{&&:#{==:#{window_panes},1},#{&&:#{pane_title},#{!=:#{pane_title},#{host_short}}}},: \"#{pane_title}\",}\
,\
#{session_windows} windows\
#{?session_grouped, \
(group #{session_group}: \
#{session_group_list}),\
}\
#{?session_attached, (attached),}\
}";

/// How big the preview under the list is (`v` steps Normal, Off, Big).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewMode {
    Off,
    Normal,
    Big,
}

/// The last search (`/`), for `n` and `N`.
#[derive(Clone)]
struct Search {
    text: String,
    icase: bool,
    forward: bool,
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
    /// The command a chosen item runs (`None`: the mode's own), `-F`, `-K`,
    /// the filter (`-f`, then `f`), and the dialect the command runs in.
    template: Option<String>,
    format: Option<String>,
    key_format: Option<String>,
    filter: Option<String>,
    dialect: Dialect,
    order: Order,
    reversed: bool,
    /// `t`: the items to act on together.
    tagged: BTreeSet<Tag>,
    preview: PreviewMode,
    search: Option<Search>,
    /// `-y`: kill prompts are answered yes.
    prompt_yes: bool,
    /// `-G` was not given: a session group is listed once.
    squash_groups: bool,
}

/// One item and its children, as `mode_tree_add` makes them.
struct Item {
    tag: Tag,
    /// The session a window or pane is seen in.
    session: Option<SessionId>,
    name: String,
    text: String,
    /// Names are right-aligned against the other names at the same depth.
    align: bool,
    children: Vec<Item>,
}

/// One drawn line, as `mode_tree_build_lines` makes them.
struct Line {
    tag: Tag,
    session: Option<SessionId>,
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
    /// The key of each line (`None`: it has none).
    keys: Vec<Option<String>>,
    current: usize,
    offset: usize,
    height: usize,
    no_matches: bool,
    rows: usize,
}

impl Tree {
    /// `fmt` in grofe's format language: tmux's, when the chooser was
    /// opened in the tmux dialect.
    fn native(&self, fmt: &str) -> String {
        if self.dialect == Dialect::Tmux {
            crate::server::dispatch::options::tmux_template(fmt)
        } else {
            fmt.to_string()
        }
    }

    /// The format of the items' text: `-F`, else the mode's own.
    fn item_format(&self) -> String {
        match &self.format {
            Some(f) => self.native(f),
            None => crate::server::dispatch::options::tmux_template(self.kind.default_format()),
        }
    }

    fn new(
        kind: Kind,
        pane: PaneId,
        chooser: &ChooseOpts,
        dialect: Dialect,
    ) -> Result<Tree, String> {
        let order = match chooser.sort.as_deref() {
            Some(name) => Order::parse(name).ok_or("invalid sort order")?,
            None => kind.orders()[0],
        };
        let none = Tag::Pane(pane);
        Ok(Tree {
            pane,
            unzoom: None,
            expanded: BTreeMap::new(),
            current: 0,
            offset: 0,
            height: 0,
            tag: None,
            preview_offset: 0,
            start: (none, none, none),
            template: chooser.template.clone(),
            format: chooser.format.clone(),
            key_format: chooser.key_format.clone(),
            filter: chooser.filter.clone(),
            dialect,
            order,
            reversed: chooser.reversed,
            tagged: BTreeSet::new(),
            preview: match chooser.no_preview {
                0 => PreviewMode::Normal,
                1 => PreviewMode::Off,
                _ => PreviewMode::Big,
            },
            search: None,
            prompt_yes: chooser.prompt_yes,
            squash_groups: !chooser.groups,
            kind,
        })
    }
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
    let kind = Kind::Tree { find: opts.find };
    // tmux reads `-O` before it looks at anything else.
    let mut tree = Tree::new(kind, PaneId(0), &opts.chooser, opts.dialect)?;
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
    let shown = state.clients.get(&client).map(|c| c.session);
    let session = model.link_session(window, shown).unwrap_or(w.session);
    tree.pane = pane;
    tree.start = (Tag::Session(session), Tag::Window(window), Tag::Pane(pane));
    tree.tag = Some(if opts.sessions {
        Tag::Session(session)
    } else if opts.windows || w.pane_ids().len() == 1 {
        Tag::Window(window)
    } else {
        Tag::Pane(pane)
    });
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
pub fn open_clients(
    state: &mut State,
    ctx: &ClientCtx,
    zoom: bool,
    chooser: &ChooseOpts,
    dialect: Dialect,
) -> Result<String, String> {
    open_list(state, ctx, Kind::Clients, zoom, chooser, dialect)
}

/// Opens the buffer chooser on the current client, over its pane (TM.4).
/// With no buffers nothing opens, as in tmux.
pub fn open_buffers(
    state: &mut State,
    ctx: &ClientCtx,
    zoom: bool,
    chooser: &ChooseOpts,
    dialect: Dialect,
) -> Result<String, String> {
    if let Some(sort) = chooser.sort.as_deref() {
        Order::parse(sort).ok_or("invalid sort order")?;
    }
    if state.buffers.is_empty() {
        return Ok(String::new());
    }
    open_list(state, ctx, Kind::Buffers, zoom, chooser, dialect)
}

/// A flat chooser (clients or buffers) over the target pane.
fn open_list(
    state: &mut State,
    ctx: &ClientCtx,
    kind: Kind,
    zoom: bool,
    chooser: &ChooseOpts,
    dialect: Dialect,
) -> Result<String, String> {
    let mut tree = Tree::new(kind, PaneId(0), chooser, dialect)?;
    let pane = state.model.resolve_pane(&chooser.target, ctx)?;
    let Some(client) = clients::current_client(state, ctx) else {
        return Ok(String::new());
    };
    if state.trees.is_open(client) {
        return Ok(String::new());
    }
    tree.pane = pane;
    let none = Tag::Pane(pane);
    tree.start = (none, none, none);
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

/// Closes every chooser open over `pane` (it leaves its modes).
pub fn close_over(state: &mut State, pane: PaneId) {
    let ids: Vec<ClientId> = state
        .trees
        .open
        .iter()
        .filter(|(_, t)| t.pane == pane)
        .map(|(id, _)| *id)
        .collect();
    for id in ids {
        close(state, id);
    }
}

/// Client `id` went away: its chooser closes.
pub fn client_gone(state: &mut State, id: ClientId) {
    close(state, id);
}

// --- Building ---------------------------------------------------------

/// tmux's `format_true`.
fn format_true(s: &str) -> bool {
    !s.is_empty() && s != "0"
}

/// The items, and whether a filter matched nothing (then everything is
/// listed, as tmux does).
fn build(state: &State, tree: &Tree) -> (Vec<Item>, bool) {
    let filtered = |filter: Option<&str>| match &tree.kind {
        Kind::Clients => client_items(state, tree, filter),
        Kind::Buffers => buffer_items(state, tree, filter),
        Kind::Tree { find } => tree_items(state, tree, find.as_deref(), filter),
    };
    let items = filtered(tree.filter.as_deref());
    let finding = matches!(&tree.kind, Kind::Tree { find: Some(_) });
    if items.is_empty() && (tree.filter.is_some() || finding) {
        let all = match &tree.kind {
            Kind::Tree { .. } => tree_items(state, tree, None, None),
            _ => filtered(None),
        };
        return (all, true);
    }
    (items, false)
}

fn tree_items(state: &State, tree: &Tree, find: Option<&str>, filter: Option<&str>) -> Vec<Item> {
    use std::cmp::Ordering::Equal;
    let model = &state.model;
    let (order, reversed) = (tree.order, tree.reversed);
    let format = tree.item_format();
    let filter = filter.map(|f| tree.native(f));
    let filter = filter.as_deref();
    // The session the target pane is in, for `-G`.
    let current = model
        .panes
        .get(&tree.pane)
        .and_then(|p| model.windows.get(&p.window))
        .map(|w| w.session);
    let mut sessions: Vec<&crate::server::model::Session> = model.sessions.values().collect();
    sessions.sort_by(|a, b| {
        let o = match order {
            Order::Index => a.id.cmp(&b.id),
            Order::Creation => a.created.cmp(&b.created),
            Order::Activity => b.activity.cmp(&a.activity),
            _ => Equal,
        };
        let r = o.then(a.name.cmp(&b.name));
        if reversed { r.reverse() } else { r }
    });
    if order == Order::Unsorted && reversed {
        sessions.reverse();
    }
    let mut out = Vec::new();
    for s in sessions {
        if tree.squash_groups {
            let peers = model.group_peers(s.id);
            if !peers.is_empty() {
                let mut group = peers.clone();
                group.push(s.id);
                let first = group.iter().min().copied();
                let in_current = current.is_some_and(|c| group.contains(&c));
                if (in_current && Some(s.id) != current) || (!in_current && first != Some(s.id)) {
                    continue;
                }
            }
        }
        let mut windows: Vec<&crate::server::model::Window> = s
            .windows
            .iter()
            .filter_map(|w| model.windows.get(w))
            .collect();
        windows.sort_by(|a, b| {
            let o = match order {
                Order::Index => model
                    .index_in(s.id, a.id)
                    .unwrap_or(a.index)
                    .cmp(&model.index_in(s.id, b.id).unwrap_or(b.index)),
                Order::Creation => a.id.cmp(&b.id),
                Order::Activity => b.activity.cmp(&a.activity),
                Order::Size => (u32::from(a.cols) * u32::from(a.rows))
                    .cmp(&(u32::from(b.cols) * u32::from(b.rows))),
                _ => Equal,
            };
            let r = o.then(a.name.cmp(&b.name));
            if reversed { r.reverse() } else { r }
        });
        if order == Order::Unsorted && reversed {
            windows.reverse();
        }
        let mut kids = Vec::new();
        for w in windows {
            let ids = w.pane_ids();
            // tmux drops a one-pane window whose pane does not match, but
            // keeps a window of several panes even when none does.
            let Some(first) = ids.first() else {
                continue;
            };
            if ids.len() == 1
                && !(matches(state, *first, find) && keeps(state, tree.pane, s.id, *first, filter))
            {
                continue;
            }
            let base = model.pane_base_index(w.id);
            let mut panes: Vec<(usize, PaneId)> = ids.iter().copied().enumerate().collect();
            panes.sort_by(|(ia, a), (ib, b)| {
                let title = |p: &PaneId| state.passthrough.title(*p);
                let area = |p: &PaneId| {
                    model
                        .panes
                        .get(p)
                        .map_or(0, |p| u32::from(p.cols) * u32::from(p.rows))
                };
                let o = match order {
                    Order::Index | Order::Z => ia.cmp(ib),
                    Order::Creation => a.cmp(b),
                    Order::Size => area(a).cmp(&area(b)),
                    _ => Equal,
                };
                let r = o.then(title(a).cmp(&title(b)));
                if reversed { r.reverse() } else { r }
            });
            if order == Order::Unsorted && reversed {
                panes.reverse();
            }
            let children = panes
                .iter()
                .filter(|(_, p)| {
                    matches(state, *p, find) && keeps(state, tree.pane, s.id, *p, filter)
                })
                .map(|(i, p)| Item {
                    tag: Tag::Pane(*p),
                    session: Some(s.id),
                    name: (base + *i as u32).to_string(),
                    text: expand_tag(state, tree.pane, Some(s.id), Tag::Pane(*p), &format, None),
                    align: true,
                    children: Vec::new(),
                })
                .collect();
            kids.push(Item {
                tag: Tag::Window(w.id),
                session: Some(s.id),
                name: model.index_in(s.id, w.id).unwrap_or(w.index).to_string(),
                text: expand_tag(
                    state,
                    tree.pane,
                    Some(s.id),
                    Tag::Window(w.id),
                    &format,
                    None,
                ),
                align: true,
                children,
            });
        }
        if kids.is_empty() {
            continue;
        }
        out.push(Item {
            tag: Tag::Session(s.id),
            session: Some(s.id),
            name: s.name.clone(),
            text: expand_tag(
                state,
                tree.pane,
                Some(s.id),
                Tag::Session(s.id),
                &format,
                None,
            ),
            align: false,
            children: kids,
        });
    }
    out
}

/// `window_tree_filter_pane`: the filter (a format) is true for the pane.
fn keeps(
    state: &State,
    at: PaneId,
    session: SessionId,
    pane: PaneId,
    filter: Option<&str>,
) -> bool {
    filter.is_none_or(|f| {
        format_true(&expand_tag(
            state,
            at,
            Some(session),
            Tag::Pane(pane),
            f,
            None,
        ))
    })
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

/// The variables of an item's format plus the line number a key format
/// reads.
struct WithLine<'a> {
    inner: &'a dyn cmd::FormatVars,
    line: Option<usize>,
}

impl cmd::FormatVars for WithLine<'_> {
    fn get(&self, name: &str) -> Option<String> {
        match (name, self.line) {
            ("line", Some(n)) => Some(n.to_string()),
            _ => self.inner.get(name),
        }
    }
}

/// `fmt` expanded for the item `tag`, as tmux's `format_defaults` sets up
/// a session, a winlink or a pane (`session` is the session a window or
/// pane is listed under), a client or a paste buffer; `line` is what a key
/// format sees as `#{line}`.
fn expand_tag(
    state: &State,
    at: PaneId,
    session: Option<SessionId>,
    tag: Tag,
    fmt: &str,
    line: Option<usize>,
) -> String {
    let model = &state.model;
    let active_of = |s: SessionId| {
        model
            .sessions
            .get(&s)
            .and_then(|s| model.windows.get(&s.active))
            .map(|w| w.active)
    };
    let (pane, ctx) = match tag {
        Tag::Session(s) => match active_of(s) {
            Some(p) => (p, FormatCtx::at(model, p, Level::Session).in_session(s)),
            None => return fmt.to_string(),
        },
        Tag::Window(w) => {
            let Some(win) = model.windows.get(&w) else {
                return fmt.to_string();
            };
            let ctx = FormatCtx::at(model, win.active, Level::Window);
            let ctx = match session {
                Some(s) => ctx.in_session(s),
                None => ctx,
            };
            (win.active, ctx)
        }
        Tag::Pane(p) => {
            let ctx = FormatCtx::at(model, p, Level::Pane);
            let ctx = match session {
                Some(s) => ctx.in_session(s),
                None => ctx,
            };
            (p, ctx)
        }
        Tag::Client(id) => {
            let Some(c) = state.clients.get(&id) else {
                return fmt.to_string();
            };
            let Some(p) = active_of(c.session) else {
                return fmt.to_string();
            };
            let vars = clients::ClientVars::new(model, c, Some(FormatCtx::new(model, p)));
            let server = ServerVars::new(state, p, &vars);
            return cmd::expand(
                fmt,
                &WithLine {
                    inner: &server,
                    line,
                },
            );
        }
        Tag::Buffer(id) => {
            let Some(b) = state.buffers.by_id(id) else {
                return fmt.to_string();
            };
            if !model.panes.contains_key(&at) {
                return cmd::expand(fmt, &crate::server::buffers::BufferVars(b));
            }
            let ctx = FormatCtx::new(model, at);
            let server = ServerVars::new(state, at, &ctx);
            let vars = BufferOver {
                buffer: crate::server::buffers::BufferVars(b),
                inner: &server,
            };
            return cmd::expand(fmt, &WithLine { inner: &vars, line });
        }
    };
    let server = ServerVars::new(state, pane, &ctx);
    cmd::expand(
        fmt,
        &WithLine {
            inner: &server,
            line,
        },
    )
}

/// A buffer's variables over a pane's.
struct BufferOver<'a> {
    buffer: crate::server::buffers::BufferVars<'a>,
    inner: &'a dyn cmd::FormatVars,
}

impl cmd::FormatVars for BufferOver<'_> {
    fn get(&self, name: &str) -> Option<String> {
        self.buffer.get(name).or_else(|| self.inner.get(name))
    }
}

/// tmux's `window-buffer.c` list: each line its name and the format
/// (`#{t/p:buffer_created}: #{buffer_sample}`).
fn buffer_items(state: &State, tree: &Tree, filter: Option<&str>) -> Vec<Item> {
    let format = tree.item_format();
    let format = format.as_str();
    let filter = filter.map(|f| tree.native(f));
    let filter = filter.as_deref();
    let mut list: Vec<&crate::server::buffers::Buffer> = state.buffers.iter().collect();
    let order = tree.order;
    list.sort_by(|a, b| {
        let o = match order {
            Order::Creation => b.id.cmp(&a.id),
            Order::Size => a.data.len().cmp(&b.data.len()),
            _ => std::cmp::Ordering::Equal,
        };
        let r = o.then(a.name.cmp(&b.name));
        if tree.reversed { r.reverse() } else { r }
    });
    list.into_iter()
        .filter(|b| {
            filter.is_none_or(|f| {
                format_true(&expand_tag(
                    state,
                    tree.pane,
                    None,
                    Tag::Buffer(b.id),
                    f,
                    None,
                ))
            })
        })
        .map(|b| Item {
            tag: Tag::Buffer(b.id),
            session: None,
            name: b.name.clone(),
            text: expand_tag(state, tree.pane, None, Tag::Buffer(b.id), format, None),
            align: false,
            children: Vec::new(),
        })
        .collect()
}

fn client_items(state: &State, tree: &Tree, filter: Option<&str>) -> Vec<Item> {
    let format = tree.item_format();
    let format = format.as_str();
    let filter = filter.map(|f| tree.native(f));
    let filter = filter.as_deref();
    let mut list: Vec<(&ClientId, &clients::ClientEntry)> = state.clients.iter().collect();
    let order = tree.order;
    list.sort_by(|(ia, a), (ib, b)| {
        let o = match order {
            Order::Size => a.size.0.cmp(&b.size.0).then(a.size.1.cmp(&b.size.1)),
            Order::Creation => ia.cmp(ib),
            Order::Activity => b.activity_time.cmp(&a.activity_time),
            _ => std::cmp::Ordering::Equal,
        };
        let r = o.then(a.name.cmp(&b.name));
        if tree.reversed { r.reverse() } else { r }
    });
    list.into_iter()
        .filter(|(id, _)| {
            filter.is_none_or(|f| {
                format_true(&expand_tag(
                    state,
                    tree.pane,
                    None,
                    Tag::Client(**id),
                    f,
                    None,
                ))
            })
        })
        .map(|(id, c)| Item {
            tag: Tag::Client(*id),
            session: None,
            name: c.name.clone(),
            text: expand_tag(state, tree.pane, None, Tag::Client(*id), format, None),
            align: false,
            children: Vec::new(),
        })
        .collect()
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
            session: item.session,
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

/// `mode_tree_set_height` for the preview `mode`: the list's rows out of
/// the pane's `rows`.
fn list_height(rows: usize, lines: usize, mode: PreviewMode) -> usize {
    let mut h = match mode {
        PreviewMode::Off => rows,
        PreviewMode::Normal => {
            let mut h = (rows / 3) * 2;
            if h > lines {
                h = rows / 2;
            }
            if h < 10 {
                h = rows;
            }
            h
        }
        PreviewMode::Big => (rows / 4).min(lines).max(2),
    };
    if rows - h.min(rows) < 2 {
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

/// The line key `(0)`..`(9)`, `(M-a)`..`(M-z)` of line `i`.
fn line_key(i: usize) -> Option<String> {
    match i {
        0..=9 => Some(i.to_string()),
        10..=35 => Some(format!("M-{}", char::from(b'a' + (i - 10) as u8))),
        _ => None,
    }
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
    let height = list_height(rows, lines.len(), tree.preview);
    // `mode_tree_check_selected`: a current line below the list scrolls.
    let offset = if height > 0 && current > height - 1 {
        offset.max(current - height + 1)
    } else {
        offset
    };
    let key_format = tree.key_format.as_deref().map(|f| tree.native(f));
    let keys = lines
        .iter()
        .enumerate()
        .map(|(i, l)| match &key_format {
            None => line_key(i),
            Some(fmt) => {
                let text = expand_tag(state, tree.pane, l.session, l.tag, fmt, Some(i));
                crate::server::input::parse_key(text.trim())
                    .ok()
                    .map(|k| k.to_string())
            }
        })
        .collect();
    View {
        lines,
        keys,
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
    if settings.is_prefix(key) {
        return true;
    }
    state.registry.key_tables.get(ROOT_TABLE, key).is_some()
}

/// The line a key chooses directly: the line whose key it is.
fn line_of_key(key: &Key, keys: &[Option<String>]) -> Option<usize> {
    let name = key.to_string();
    keys.iter()
        .position(|k| k.as_deref() == Some(name.as_str()))
}

/// Rebuilds the tree for `client`, keeping `tag` as the current item
/// (`mode_tree_build`), then scrolling as `mode_tree_set_current` does.
fn rebuild(state: &mut State, client: ClientId, tag: Option<Tag>) {
    let Some(tree) = state.trees.open.get(&client) else {
        return;
    };
    let (items, _) = build(state, tree);
    let lines = lines_of(&items, tree);
    let rows = state
        .model
        .panes
        .get(&tree.pane)
        .map_or(0, |p| usize::from(p.rows));
    let tag = tag.or(tree.tag);
    let height = list_height(rows, lines.len(), tree.preview);
    let (current, offset) = set_current(&lines, tag, tree.current, tree.offset, tree.height);
    let tag = lines.get(current).map(|l| l.tag);
    if let Some(tree) = state.trees.open.get_mut(&client) {
        tree.current = current;
        tree.offset = offset;
        tree.height = height;
        tree.tag = tag;
        if height > 0 && current > height - 1 {
            tree.offset = tree.offset.max(current - height + 1);
        }
    }
}

/// The item `tag` and everything under it in `items`.
fn find_item(items: &[Item], tag: Tag) -> Option<&Item> {
    for item in items {
        if item.tag == tag {
            return Some(item);
        }
        if let Some(found) = find_item(&item.children, tag) {
            return Some(found);
        }
    }
    None
}

fn descendants(item: &Item, out: &mut Vec<Tag>) {
    for child in &item.children {
        out.push(child.tag);
        descendants(child, out);
    }
}

/// The pane an item stands for (a session's or window's active one).
fn pane_of(state: &State, tag: Tag) -> Option<PaneId> {
    let model = &state.model;
    match tag {
        Tag::Pane(p) => Some(p),
        Tag::Window(w) => model.windows.get(&w).map(|w| w.active),
        Tag::Session(s) => model
            .sessions
            .get(&s)
            .and_then(|s| model.windows.get(&s.active))
            .map(|w| w.active),
        Tag::Client(_) | Tag::Buffer(_) => None,
    }
}

/// What `%%` stands for in the command run for an item
/// (`window_tree_get_target` and the others): `=session:`,
/// `=session:index.`, `=session:index.%pane`, a client's tty, a buffer's
/// name.
fn target_name(state: &State, session: Option<SessionId>, tag: Tag) -> Option<String> {
    let model = &state.model;
    match tag {
        Tag::Session(s) => Some(format!("={}:", model.sessions.get(&s)?.name)),
        Tag::Window(w) => {
            let s = session.or_else(|| model.windows.get(&w).map(|w| w.session))?;
            Some(format!(
                "={}:{}.",
                model.sessions.get(&s)?.name,
                model.index_in(s, w)?
            ))
        }
        Tag::Pane(p) => {
            let w = model.panes.get(&p)?.window;
            let s = session.or_else(|| model.windows.get(&w).map(|w| w.session))?;
            Some(format!(
                "={}:{}.%{}",
                model.sessions.get(&s)?.name,
                model.index_in(s, w)?,
                p.0
            ))
        }
        Tag::Client(c) => {
            let c = state.clients.get(&c)?;
            Some(c.tty.clone().unwrap_or_else(|| c.name.clone()))
        }
        Tag::Buffer(id) => Some(state.buffers.by_id(id)?.name.clone()),
    }
}

/// `mode_tree_run_command`: `template` with `%%` the item's `name`,
/// split into the commands to run; a parse error shows on the status line
/// with a capital.
fn template_commands(
    state: &mut State,
    client: ClientId,
    template: &str,
    names: &[String],
) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for name in names {
        let command = crate::server::prompt::replace_template(template, name);
        if command.is_empty() {
            continue;
        }
        match cmd::split_command_string(&command) {
            Ok(cmds) => out.extend(cmds),
            Err(e) => {
                let mut text = e.to_string();
                if let Some(first) = text.get(..1) {
                    text = first.to_uppercase() + &text[1..];
                }
                clients::show_message(state, client, &text);
            }
        }
    }
    out
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
    let mut rebuild_to: Option<Option<Tag>> = None;
    let mut expanded: Vec<(Tag, bool)> = Vec::new();
    let mut preview_step = 0;
    let mut start_tags = None;
    let kind_tree = matches!(tree.kind, Kind::Tree { .. });
    let kind_buffers = matches!(tree.kind, Kind::Buffers);
    let kind_clients = matches!(tree.kind, Kind::Clients);

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

    if let Some(i) = line_of_key(key, &v.keys) {
        return choose(state, client, i);
    }
    let (m, plain) = (key.mods, Default::default());
    let ctrl = |c: char| key.code == KeyCode::Char(c) && m.ctrl && !m.alt;
    let alt = |c: char| key.code == KeyCode::Char(c) && m.alt && !m.ctrl;
    let is = |c: char| key.code == KeyCode::Char(c) && m == plain;
    let shifted = |code: KeyCode| key.code == code && m.shift && !m.ctrl && !m.alt;
    let code = if m == plain { Some(key.code) } else { None };
    let line = &v.lines[current];
    let line_tag = line.tag;
    let line_session = line.session;
    if is('q') || code == Some(KeyCode::Escape) || ctrl('[') || ctrl('g') {
        close(state, client);
        return Routed::Consumed;
    } else if code == Some(KeyCode::Enter) || ctrl('m') || (is('p') && kind_buffers) {
        return choose(state, client, current);
    } else if is('P') && kind_buffers {
        let tagged = tagged_lines(&v, tree);
        if tagged.is_empty() {
            return Routed::Consumed;
        }
        return choose_many(state, client, &tagged);
    } else if is('d') && kind_buffers {
        // `window_buffer_do_delete`: the selection moves off first.
        if let Tag::Buffer(id) = line_tag
            && let Some(name) = state.buffers.by_id(id).map(|b| b.name.clone())
        {
            if current + 1 < n {
                current += 1;
            } else {
                current = current.saturating_sub(1);
            }
            let next = v.lines.get(current).map(|l| l.tag);
            state.buffers.delete(&name);
            if state.buffers.is_empty() {
                close(state, client);
            } else if let Some(t) = state.trees.open.get_mut(&client) {
                t.tag = next;
                t.current = current.min(n - 2);
            }
        }
        return Routed::Consumed;
    } else if is('D') && (kind_buffers || kind_clients) {
        let tagged = tagged_lines(&v, tree);
        if kind_buffers {
            for tag in tagged {
                if let Tag::Buffer(id) = tag
                    && let Some(name) = state.buffers.by_id(id).map(|b| b.name.clone())
                {
                    state.buffers.delete(&name);
                }
            }
            if state.buffers.is_empty() {
                close(state, client);
            }
            return Routed::Consumed;
        }
        return detach_clients(state, client, &tagged, "detach-client");
    } else if (is('d') || is('x') || is('z')) && kind_clients {
        let verb = match key.code {
            KeyCode::Char('z') => "suspend-client",
            KeyCode::Char('x') => "detach-client-kill",
            _ => "detach-client",
        };
        return detach_clients(state, client, &[line_tag], verb);
    } else if (is('X') || is('Z')) && kind_clients {
        let verb = if is('Z') {
            "suspend-client"
        } else {
            "detach-client-kill"
        };
        let tagged = tagged_lines(&v, tree);
        return detach_clients(state, client, &tagged, verb);
    } else if code == Some(KeyCode::Up) || is('k') || ctrl('p') {
        up(&mut current, &mut offset, true);
    } else if code == Some(KeyCode::Down) || is('j') || ctrl('n') {
        down(&mut current, &mut offset, true);
    } else if shifted(KeyCode::Up) || is('K') {
        return swap_windows(state, client, &v, current, -1);
    } else if shifted(KeyCode::Down) || is('J') {
        return swap_windows(state, client, &v, current, 1);
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
    } else if is('t') {
        // Parents and children are never both tagged: the current item's
        // parents and children are untagged first.
        let (items, _) = build(state, tree);
        let mut gone = Vec::new();
        let mut parent = line.parent;
        while let Some(p) = parent {
            gone.push(v.lines[p].tag);
            parent = v.lines[p].parent;
        }
        if let Some(item) = find_item(&items, line_tag) {
            descendants(item, &mut gone);
        }
        let on = !tree.tagged.contains(&line_tag);
        let Some(tagged) = state.trees.open.get_mut(&client).map(|t| &mut t.tagged) else {
            return Routed::Consumed;
        };
        if on {
            for tag in gone {
                tagged.remove(&tag);
            }
            tagged.insert(line_tag);
        } else {
            tagged.remove(&line_tag);
        }
        // tmux hands the key a mouse event, so the cursor moves on.
        down(&mut current, &mut offset, false);
    } else if is('T') {
        if let Some(tree) = state.trees.open.get_mut(&client) {
            tree.tagged.clear();
        }
        return Routed::Consumed;
    } else if ctrl('t') {
        let all: BTreeSet<Tag> = v
            .lines
            .iter()
            .filter(|l| l.parent.is_none())
            .map(|l| l.tag)
            .collect();
        if let Some(tree) = state.trees.open.get_mut(&client) {
            tree.tagged = all;
        }
        return Routed::Consumed;
    } else if is('O') {
        let orders = tree.kind.orders();
        let next = orders
            .iter()
            .position(|o| *o == tree.order)
            .map_or(0, |i| (i + 1) % orders.len());
        let order = orders[next];
        if let Some(tree) = state.trees.open.get_mut(&client) {
            tree.order = order;
        }
        rebuild_to = Some(None);
    } else if is('r') {
        if let Some(tree) = state.trees.open.get_mut(&client) {
            tree.reversed = !tree.reversed;
        }
        rebuild_to = Some(None);
    } else if code == Some(KeyCode::Left) || is('h') || is('-') {
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
                rebuild_to = Some(None);
            }
        }
    } else if code == Some(KeyCode::Right) || is('l') || is('+') {
        if line.flat || line.expanded {
            down(&mut current, &mut offset, false);
        } else {
            expanded.push((line_tag, true));
            rebuild_to = Some(None);
        }
    } else if alt('-') || alt('+') {
        for l in v.lines.iter().filter(|l| l.depth == 0) {
            expanded.push((l.tag, alt('+')));
        }
        rebuild_to = Some(None);
    } else if is('<') && kind_tree {
        preview_step = -1;
    } else if is('>') && kind_tree {
        preview_step = 1;
    } else if is('H') && kind_tree {
        start_tags = Some(tree.start);
        rebuild_to = Some(None);
    } else if is('v') {
        let next = match tree.preview {
            PreviewMode::Off => PreviewMode::Big,
            PreviewMode::Normal => PreviewMode::Off,
            PreviewMode::Big => PreviewMode::Normal,
        };
        if let Some(tree) = state.trees.open.get_mut(&client) {
            tree.preview = next;
        }
        rebuild_to = Some(None);
    } else if is('?') || is('/') || ctrl('s') {
        return ask_search(state, client);
    } else if is('n') || is('N') {
        let forward = is('n');
        let found = {
            let Some(tree) = state.trees.open.get_mut(&client) else {
                return Routed::Consumed;
            };
            if let Some(s) = tree.search.as_mut() {
                s.forward = forward;
            }
            tree.search.clone()
        };
        if found.is_some() {
            search_set(state, client);
        }
        return Routed::Consumed;
    } else if is('f') {
        let initial = tree.filter.clone().unwrap_or_default();
        crate::server::prompt::ask(
            state,
            client,
            "(filter) ",
            &initial,
            "search",
            false,
            Box::new(move |state, client, text| {
                if let Some(tree) = state.trees.open.get_mut(&client) {
                    tree.filter = (!text.is_empty()).then(|| text.to_string());
                }
                rebuild(state, client, None);
                Routed::Consumed
            }),
        );
        return Routed::Consumed;
    } else if is('m') && kind_tree {
        let Some(pane) = pane_of(state, line_tag) else {
            return Routed::Consumed;
        };
        return Routed::Command(
            vec![vec![
                "select-pane".into(),
                "-m".into(),
                "-t".into(),
                format!("%{}", pane.0),
            ]],
            Dialect::Tmux,
        );
    } else if is('M') && kind_tree {
        return Routed::Command(vec![vec!["select-pane".into(), "-M".into()]], Dialect::Tmux);
    } else if is('x') && kind_tree {
        return kill_prompt(state, client, &[(line_tag, line_session)], false);
    } else if is('X') && kind_tree {
        let tagged = tagged_lines(&v, tree);
        if tagged.is_empty() {
            return Routed::Consumed;
        }
        let items: Vec<_> = v
            .lines
            .iter()
            .filter(|l| tagged.contains(&l.tag))
            .map(|l| (l.tag, l.session))
            .collect();
        return kill_prompt(state, client, &items, true);
    } else if is(':') {
        let tagged = tagged_lines(&v, tree);
        let label = if tagged.is_empty() {
            "(current) ".to_string()
        } else {
            format!("({} tagged) ", tagged.len())
        };
        let items: Vec<(Tag, Option<SessionId>)> = if tagged.is_empty() {
            vec![(line_tag, line_session)]
        } else {
            v.lines
                .iter()
                .filter(|l| tagged.contains(&l.tag))
                .map(|l| (l.tag, l.session))
                .collect()
        };
        crate::server::prompt::ask(
            state,
            client,
            &label,
            "",
            "command",
            false,
            Box::new(move |state, client, text| {
                if text.is_empty() {
                    return Routed::Consumed;
                }
                let names: Vec<String> = items
                    .iter()
                    .filter_map(|(t, s)| target_name(state, *s, *t))
                    .collect();
                let dialect = state
                    .trees
                    .open
                    .get(&client)
                    .map_or(Dialect::Tmux, |t| t.dialect);
                let cmds = template_commands(state, client, text, &names);
                Routed::Command(cmds, dialect)
            }),
        );
        return Routed::Consumed;
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
    if rebuild_to.is_some() {
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
        let height = list_height(rows, lines.len(), tree.preview);
        let tag = lines.get(at.0).map(|l| l.tag);
        let Some(tree) = state.trees.open.get_mut(&client) else {
            return Routed::Consumed;
        };
        (tree.current, tree.offset) = at;
        tree.height = height;
        tree.tag = tag;
        if height > 0 && tree.current > height - 1 {
            tree.offset = tree.offset.max(tree.current - height + 1);
        }
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

/// The tagged items that are lines now, in line order (tmux counts and
/// visits only the lines it lists).
fn tagged_lines(v: &View, tree: &Tree) -> Vec<Tag> {
    v.lines
        .iter()
        .map(|l| l.tag)
        .filter(|t| tree.tagged.contains(t))
        .collect()
}

/// `mode_tree_search_set`'s prompt.
fn ask_search(state: &mut State, client: ClientId) -> Routed {
    crate::server::prompt::ask(
        state,
        client,
        "(search) ",
        "",
        "search",
        false,
        Box::new(move |state, client, text| {
            if let Some(tree) = state.trees.open.get_mut(&client) {
                tree.search = (!text.is_empty()).then(|| Search {
                    text: text.to_string(),
                    icase: text
                        .chars()
                        .all(|c| c == c.to_lowercase().next().unwrap_or(c)),
                    forward: true,
                });
            }
            search_set(state, client);
            Routed::Consumed
        }),
    );
    Routed::Consumed
}

/// Whether the item `tag` matches the search (`window_tree_search`,
/// `window_buffer_search`; a client by name).
fn search_matches(state: &State, tag: Tag, item: &Item, search: &Search) -> bool {
    let has = |hay: &str| {
        if search.icase {
            hay.to_lowercase().contains(&search.text.to_lowercase())
        } else {
            hay.contains(&search.text)
        }
    };
    match tag {
        Tag::Session(_) | Tag::Client(_) => has(&item.name),
        Tag::Window(w) => state.model.windows.get(&w).is_some_and(|w| has(&w.name)),
        Tag::Pane(p) => pane_command(state, p).is_some_and(|c| has(&c)),
        Tag::Buffer(id) => state
            .buffers
            .by_id(id)
            .is_some_and(|b| has(&b.name) || has(&String::from_utf8_lossy(&b.data))),
    }
}

/// The name of the program running in `pane`, which a pane search reads.
fn pane_command(state: &State, pane: PaneId) -> Option<String> {
    let vars = FormatCtx::new(&state.model, pane);
    let text = cmd::expand(
        "#{pane_current_command}",
        &ServerVars::new(state, pane, &vars),
    );
    (!text.is_empty()).then_some(text)
}

/// `mode_tree_search_set`: moves to the next item that matches, searching
/// all items (also those folded away) from the current one on, round the
/// ends; its parents open.
fn search_set(state: &mut State, client: ClientId) {
    let Some(tree) = state.trees.open.get(&client) else {
        return;
    };
    let Some(search) = tree.search.clone() else {
        return;
    };
    let (items, _) = build(state, tree);
    // Every item in tree order, with its parents.
    fn flatten<'a>(items: &'a [Item], parents: &mut Vec<Tag>, out: &mut Vec<(&'a Item, Vec<Tag>)>) {
        for item in items {
            out.push((item, parents.clone()));
            parents.push(item.tag);
            flatten(&item.children, parents, out);
            parents.pop();
        }
    }
    let mut all = Vec::new();
    flatten(&items, &mut Vec::new(), &mut all);
    let Some(from) = all.iter().position(|(i, _)| Some(i.tag) == tree.tag) else {
        return;
    };
    let count = all.len();
    let mut found = None;
    for step in 1..count {
        let i = if search.forward {
            (from + step) % count
        } else {
            (from + count - step) % count
        };
        let (item, parents) = &all[i];
        if search_matches(state, item.tag, item, &search) {
            found = Some((item.tag, parents.clone()));
            break;
        }
    }
    let Some((tag, parents)) = found else {
        return;
    };
    if let Some(tree) = state.trees.open.get_mut(&client) {
        for p in parents {
            tree.expanded.insert(p, true);
        }
    }
    rebuild(state, client, Some(tag));
}

/// `x` and `X`: asks `Kill session s? `, `Kill window n? `, `Kill pane n? `
/// or `Kill N tagged? `; `y` kills.
fn kill_prompt(
    state: &mut State,
    client: ClientId,
    items: &[(Tag, Option<SessionId>)],
    tagged: bool,
) -> Routed {
    let model = &state.model;
    let label = if tagged {
        format!("Kill {} tagged? ", items.len())
    } else {
        let (tag, session) = items[0];
        match tag {
            Tag::Session(s) => match model.sessions.get(&s) {
                Some(s) => format!("Kill session {}? ", s.name),
                None => return Routed::Consumed,
            },
            Tag::Window(w) => {
                let s = session.or_else(|| model.windows.get(&w).map(|w| w.session));
                match s.and_then(|s| model.index_in(s, w)) {
                    Some(i) => format!("Kill window {i}? "),
                    None => return Routed::Consumed,
                }
            }
            Tag::Pane(p) => {
                let idx = model.panes.get(&p).and_then(|pane| {
                    let w = model.windows.get(&pane.window)?;
                    let at = w.pane_ids().iter().position(|x| *x == p)?;
                    Some(model.pane_base_index(w.id) + at as u32)
                });
                match idx {
                    Some(i) => format!("Kill pane {i}? "),
                    None => return Routed::Consumed,
                }
            }
            _ => return Routed::Consumed,
        }
    };
    let items: Vec<(Tag, Option<SessionId>)> = items.to_vec();
    let kill = move |state: &mut State, _client: ClientId, text: &str| {
        let mut chars = text.chars();
        let yes = matches!(chars.next(), Some(c) if c.eq_ignore_ascii_case(&'y'))
            && chars.next().is_none();
        if !yes {
            return Routed::Consumed;
        }
        let cmds: Vec<Vec<String>> = items
            .iter()
            .filter_map(|(t, s)| {
                let name = target_name(state, *s, *t)?;
                let verb = match t {
                    Tag::Session(_) => "kill-session",
                    Tag::Window(_) => "kill-window",
                    Tag::Pane(_) => "kill-pane",
                    _ => return None,
                };
                Some(vec![verb.to_string(), "-t".to_string(), name])
            })
            .collect();
        Routed::Command(cmds, Dialect::Tmux)
    };
    let yes = state.trees.open.get(&client).is_some_and(|t| t.prompt_yes);
    if yes {
        return kill(state, client, "y");
    }
    crate::server::prompt::ask(state, client, &label, "", "command", true, Box::new(kill));
    Routed::Consumed
}

/// `window_client_do_detach` for each of `tags`.
fn detach_clients(state: &mut State, client: ClientId, tags: &[Tag], verb: &str) -> Routed {
    let cmds: Vec<Vec<String>> = tags
        .iter()
        .filter_map(|t| {
            let Tag::Client(_) = t else {
                return None;
            };
            let name = target_name(state, None, *t)?;
            Some(match verb {
                "detach-client-kill" => {
                    vec!["detach-client".into(), "-P".into(), "-t".into(), name]
                }
                v => vec![v.into(), "-t".into(), name],
            })
        })
        .collect();
    let _ = client;
    Routed::Command(cmds, Dialect::Tmux)
}

/// `window_tree_swap`: with the current line a window, swaps it with the
/// next window line up or down at the same depth under the same session,
/// when the order is the index.
fn swap_windows(
    state: &mut State,
    client: ClientId,
    v: &View,
    current: usize,
    direction: i32,
) -> Routed {
    let Some(tree) = state.trees.open.get(&client) else {
        return Routed::Consumed;
    };
    if !matches!(tree.kind, Kind::Tree { .. }) || tree.order != Order::Index {
        return Routed::Consumed;
    }
    let depth = v.lines[current].depth;
    let mut other = current;
    loop {
        let next = other as i64 + i64::from(direction);
        if next < 0 || next as usize >= v.lines.len() {
            return Routed::Consumed;
        }
        other = next as usize;
        if v.lines[other].depth <= depth {
            break;
        }
    }
    if v.lines[other].depth != depth {
        return Routed::Consumed;
    }
    let (a, b) = (&v.lines[current], &v.lines[other]);
    let (Tag::Window(_), Tag::Window(_)) = (a.tag, b.tag) else {
        return Routed::Consumed;
    };
    if a.session != b.session {
        return Routed::Consumed;
    }
    let (Some(src), Some(dst)) = (
        target_name(state, a.session, a.tag),
        target_name(state, b.session, b.tag),
    ) else {
        return Routed::Consumed;
    };
    Routed::Command(
        vec![vec![
            "swap-window".into(),
            "-s".into(),
            src,
            "-t".into(),
            dst,
        ]],
        Dialect::Tmux,
    )
}

/// The chosen line: the chooser closes, then the command of the template
/// runs for its item (`%%`) on the client, as tmux's `mode_tree_run_command`
/// does.
fn choose(state: &mut State, client: ClientId, line: usize) -> Routed {
    let Some(tree) = state.trees.open.get(&client) else {
        return Routed::Consumed;
    };
    let v = view(state, tree);
    let Some(l) = v.lines.get(line) else {
        return Routed::Consumed;
    };
    let names: Vec<String> = target_name(state, l.session, l.tag).into_iter().collect();
    run_chosen(state, client, names)
}

/// `P`: the command for each of the tagged items.
fn choose_many(state: &mut State, client: ClientId, tags: &[Tag]) -> Routed {
    let names: Vec<String> = tags
        .iter()
        .filter_map(|t| target_name(state, None, *t))
        .collect();
    run_chosen(state, client, names)
}

fn run_chosen(state: &mut State, client: ClientId, names: Vec<String>) -> Routed {
    let Some(tree) = state.trees.open.get(&client) else {
        return Routed::Consumed;
    };
    let (template, dialect) = match &tree.template {
        Some(t) => (t.clone(), tree.dialect),
        None => (tree.kind.default_template().to_string(), Dialect::Tmux),
    };
    close(state, client);
    let cmds = template_commands(state, client, &template, &names);
    if cmds.is_empty() {
        return Routed::Consumed;
    }
    Routed::Command(cmds, dialect)
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
                        format!(
                            "{}:{}",
                            model.index_in(s.id, w.id).unwrap_or(w.index),
                            w.name
                        ),
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
    let chain = options::chain(&state.model, tree.pane);
    let mode = status::parse_style(
        state
            .model
            .options
            .lookup("mode-style", &chain)
            .unwrap_or(MODE_STYLE),
    );
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
    let keylen = v
        .keys
        .iter()
        .flatten()
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
        let key = v.keys[i]
            .as_ref()
            .map(|k| format!("({k})"))
            .unwrap_or_default();
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
        let tagged = if tree.tagged.contains(&line.tag) {
            "*"
        } else {
            ""
        };
        let text = format!("{key:<keylen$}{start}{name}{tagged}: ");
        let width = UnicodeWidthStr::width(text.as_str()).min(w);
        let mut style = if i == v.current {
            cv.fill(0, y, w, &mode_bg);
            mode
        } else {
            plain
        };
        if !tagged.is_empty() {
            style.attrs ^= CellFlags::BOLD;
        }
        let used = cv.text(0, y, &text, &style, w);
        // The item's text is drawn as a format (`#[...]` styles work).
        let drawn = status::draw(&line.text, style, (w - width) as u16);
        let mut x = used;
        for span in &drawn.line.spans {
            x += cv.text(x, y, &span.text, &span.style, w.saturating_sub(x));
        }
    }

    if tree.preview == PreviewMode::Off || sy <= 4 || h < 2 || sy - h <= 4 || w <= 4 {
        return;
    }
    let line = &v.lines[v.current];
    cv.frame(0, h, w, sy - h);
    let title = format!(
        " {} (sort: {}{})",
        line.name,
        tree.order.name(),
        if tree.reversed { ", reversed" } else { "" }
    );
    if w - 2 >= title.len() {
        let mut x = 1 + cv.text(1, h, &title, &plain, w - 2);
        let filter = tree.filter.is_some() || matches!(&tree.kind, Kind::Tree { find: Some(_) });
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
        let keys: Vec<Option<String>> = (0..5).map(line_key).collect();
        assert_eq!(line_of_key(&key("3"), &keys), Some(3));
        assert_eq!(line_of_key(&key("7"), &keys), None);
        let keys: Vec<Option<String>> = (0..20).map(line_key).collect();
        assert_eq!(line_of_key(&key("M-b"), &keys), Some(11));
        assert_eq!(line_of_key(&key("b"), &keys), None);
    }

    /// `mode_tree_set_height` in a 23-row pane: two thirds of it, half when
    /// the list is shorter, the whole pane when that leaves under 10 rows.
    #[test]
    fn the_list_takes_mode_trees_height() {
        use PreviewMode::*;
        assert_eq!(list_height(23, 5, Normal), 11);
        assert_eq!(list_height(23, 27, Normal), 14);
        assert_eq!(list_height(15, 3, Normal), 15);
        assert_eq!(list_height(12, 20, Normal), 12);
        // No preview: the whole pane; a big one: a quarter, 2 at least.
        assert_eq!(list_height(23, 5, Off), 23);
        assert_eq!(list_height(24, 50, Big), 6);
        assert_eq!(list_height(24, 3, Big), 3);
        assert_eq!(list_height(24, 1, Big), 2);
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
            session: None,
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
