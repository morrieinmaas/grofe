use super::*;

/// tmux's `mode-style` default.
pub(super) const MODE_STYLE: &str = "bg=yellow,fg=black";

/// What `find-window` looks for and where (`-C`, `-N`, `-T`, `-r`, `-i`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Find {
    pub pattern: String,
    /// `-C`: the text of the pane.
    pub content: bool,
    /// `-N`: the window's name.
    pub name: bool,
    /// `-T`: the pane's title.
    pub title: bool,
    /// `-r`: the pattern is a regular expression, not a glob.
    pub regex: bool,
    /// `-i`: case does not matter.
    pub ignore: bool,
}

/// What `choose-tree` and `find-window` open with.
pub struct Opts {
    /// `-Z`.
    pub zoom: bool,
    /// `-s`: sessions collapsed.
    pub sessions: bool,
    /// `-w`: windows collapsed under their sessions.
    pub windows: bool,
    /// `find-window`'s pattern: only windows whose name, a pane's title or
    /// a pane's visible text hold it, by `-N`, `-T` and `-C`.
    pub find: Option<Find>,
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
    pub(super) open: BTreeMap<ClientId, Tree>,
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
                Kind::Customize { .. } => "options-mode",
            }
            .to_string(),
        ),
        _ => None,
    }
}

/// What a line stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Tag {
    Session(SessionId),
    Window(WindowId),
    Pane(PaneId),
    Client(ClientId),
    /// A paste buffer, by its id (`buffers::Buffer::id`).
    Buffer(u64),
    /// An item of customize-mode, by `customize`'s tag.
    Custom(u64),
}

pub(super) enum Kind {
    Tree {
        find: Option<Find>,
    },
    Clients,
    Buffers,
    /// customize-mode; `hide_global` is `H`.
    Customize {
        hide_global: bool,
    },
}

/// A sort order (tmux's `sort_order`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Order {
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
    pub(super) fn parse(s: &str) -> Option<Order> {
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

    pub(super) fn name(self) -> &'static str {
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
    pub(super) fn orders(&self) -> &'static [Order] {
        match self {
            Kind::Tree { .. } => &[Order::Index, Order::Name, Order::Activity, Order::Z],
            Kind::Clients => &[Order::Name, Order::Size, Order::Creation, Order::Activity],
            Kind::Buffers => &[Order::Creation, Order::Name, Order::Size],
            Kind::Customize { .. } => &[],
        }
    }

    /// The command a chosen item runs when none is given.
    pub(super) fn default_template(&self) -> &'static str {
        match self {
            Kind::Tree { .. } => "switch-client -Zt '%%'",
            Kind::Clients => "detach-client -t '%%'",
            Kind::Buffers => "paste-buffer -p -b '%%'",
            Kind::Customize { .. } => "",
        }
    }

    pub(super) fn default_format(&self) -> &'static str {
        match self {
            Kind::Tree { .. } => TREE_FORMAT,
            Kind::Clients => "#{t/p:client_activity}: session #{session_name}",
            Kind::Buffers => "#{t/p:buffer_created}: #{buffer_sample}",
            Kind::Customize { .. } => crate::server::customize::DEFAULT_FORMAT,
        }
    }
}

/// `WINDOW_TREE_DEFAULT_FORMAT`.
pub(super) const TREE_FORMAT: &str = "#{?pane_format,\
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
pub(super) enum PreviewMode {
    Off,
    Normal,
    Big,
}

/// The last search (`/`), for `n` and `N`.
#[derive(Clone)]
pub(super) struct Search {
    pub(super) text: String,
    pub(super) icase: bool,
    pub(super) forward: bool,
}

pub(super) struct Tree {
    pub(super) kind: Kind,
    /// The pane it is drawn over.
    pub(super) pane: PaneId,
    /// The window `-Z` zoomed, unzoomed again on close.
    pub(super) unzoom: Option<WindowId>,
    /// Whether an item shows its children; an item not in here does
    /// (tmux's items made after the first build).
    pub(super) expanded: BTreeMap<Tag, bool>,
    pub(super) current: usize,
    pub(super) offset: usize,
    /// The list's height at the last build (tmux's `mtd->height`).
    pub(super) height: usize,
    /// The current line's item, to find it again after a rebuild.
    pub(super) tag: Option<Tag>,
    /// How far the preview is scrolled (`<`, `>`).
    pub(super) preview_offset: i32,
    /// Where it opened, for `H`.
    pub(super) start: (Tag, Tag, Tag),
    /// The command a chosen item runs (`None`: the mode's own), `-F`, `-K`,
    /// the filter (`-f`, then `f`), and the dialect the command runs in.
    pub(super) template: Option<String>,
    pub(super) format: Option<String>,
    pub(super) key_format: Option<String>,
    pub(super) filter: Option<String>,
    pub(super) dialect: Dialect,
    pub(super) order: Order,
    pub(super) reversed: bool,
    /// `t`: the items to act on together.
    pub(super) tagged: BTreeSet<Tag>,
    pub(super) preview: PreviewMode,
    pub(super) search: Option<Search>,
    /// `-y`: kill prompts are answered yes.
    pub(super) prompt_yes: bool,
    /// `-G` was not given: a session group is listed once.
    pub(super) squash_groups: bool,
    /// The session of the client it opened for (customize-mode's scope).
    pub(super) session: Option<SessionId>,
    /// The help popup is up (`F1`, `C-h`): any key closes it.
    pub(super) help: bool,
}

/// One item and its children, as `mode_tree_add` makes them.
pub(super) struct Item {
    pub(super) tag: Tag,
    /// The session a window or pane is seen in.
    pub(super) session: Option<SessionId>,
    pub(super) name: String,
    pub(super) text: String,
    /// Names are right-aligned against the other names at the same depth.
    pub(super) align: bool,
    pub(super) children: Vec<Item>,
    /// customize-mode's: no text after the name, may not be tagged, shows
    /// its parent's preview, and starts expanded (`None`: as the chooser's
    /// own rule has it).
    pub(super) plain: bool,
    pub(super) no_tag: bool,
    pub(super) as_parent: bool,
    pub(super) start_expanded: Option<bool>,
}

impl Item {
    /// The fields only customize-mode sets, at their usual values.
    pub(super) fn plain_defaults() -> Item {
        Item {
            tag: Tag::Custom(0),
            session: None,
            name: String::new(),
            text: String::new(),
            align: false,
            children: Vec::new(),
            plain: false,
            no_tag: false,
            as_parent: false,
            start_expanded: None,
        }
    }
}

/// One drawn line, as `mode_tree_build_lines` makes them.
pub(super) struct Line {
    pub(super) tag: Tag,
    pub(super) session: Option<SessionId>,
    pub(super) depth: usize,
    pub(super) last: bool,
    pub(super) flat: bool,
    pub(super) parent: Option<usize>,
    pub(super) has_children: bool,
    pub(super) expanded: bool,
    pub(super) name: String,
    pub(super) text: String,
    pub(super) align: bool,
    pub(super) plain: bool,
    pub(super) no_tag: bool,
    pub(super) as_parent: bool,
}

/// A tree as it is now: its lines, which is current, and the list's
/// height in the pane.
pub(super) struct View {
    pub(super) lines: Vec<Line>,
    /// The key of each line (`None`: it has none).
    pub(super) keys: Vec<Option<String>>,
    pub(super) current: usize,
    pub(super) offset: usize,
    pub(super) height: usize,
    pub(super) no_matches: bool,
    pub(super) rows: usize,
}

impl Tree {
    /// `fmt` in grofe's format language: tmux's, when the chooser was
    /// opened in the tmux dialect.
    pub(super) fn native(&self, fmt: &str) -> String {
        if self.dialect == Dialect::Tmux {
            crate::server::dispatch::options::tmux_template(fmt)
        } else {
            fmt.to_string()
        }
    }

    /// `mode_tree_set_height`: the list's rows out of the pane's `rows`;
    /// customize-mode leaves its preview twelve.
    pub(super) fn list_height(&self, rows: usize, lines: usize) -> usize {
        if let Kind::Customize { .. } = self.kind {
            if self.preview == PreviewMode::Off {
                return rows;
            }
            let h = if rows > 12 { rows - 12 } else { rows };
            return if rows - h < 2 { rows } else { h };
        }
        list_height(rows, lines, self.preview)
    }

    /// The format of the items' text: `-F`, else the mode's own.
    pub(super) fn item_format(&self) -> String {
        match &self.format {
            Some(f) => self.native(f),
            None => crate::server::dispatch::options::tmux_template(self.kind.default_format()),
        }
    }

    pub(super) fn new(
        kind: Kind,
        pane: PaneId,
        chooser: &ChooseOpts,
        dialect: Dialect,
    ) -> Result<Tree, String> {
        let order = match chooser.sort.as_deref() {
            Some(name) => Order::parse(name).ok_or("invalid sort order")?,
            None => kind.orders().first().copied().unwrap_or(Order::Index),
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
            session: None,
            help: false,
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

/// Opens customize-mode on the current client, over the target pane.
pub fn open_customize(
    state: &mut State,
    ctx: &ClientCtx,
    zoom: bool,
    chooser: &ChooseOpts,
    dialect: Dialect,
) -> Result<String, String> {
    let mut tree = Tree::new(
        Kind::Customize { hide_global: false },
        PaneId(0),
        chooser,
        dialect,
    )?;
    let pane = state.model.resolve_pane(&chooser.target, ctx)?;
    let Some(client) = clients::current_client(state, ctx) else {
        return Ok(String::new());
    };
    if state.trees.is_open(client) {
        return Ok(String::new());
    }
    tree.pane = pane;
    tree.session = state.clients.get(&client).map(|c| c.session);
    let none = Tag::Pane(pane);
    tree.start = (none, none, none);
    open(state, client, tree, zoom);
    Ok(String::new())
}

/// A flat chooser (clients or buffers) over the target pane.
pub(super) fn open_list(
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

pub(super) fn open(state: &mut State, client: ClientId, mut tree: Tree, zoom: bool) {
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
pub(super) fn zoom_pane(model: &mut Model, pane: PaneId) -> Option<WindowId> {
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
pub(super) fn close(state: &mut State, id: ClientId) {
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
