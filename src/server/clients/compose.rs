use super::*;

/// Where things are on client `c`'s screen when drawn with `viewport`.
pub(super) fn overlay_ctx_at(
    model: &Model,
    id: ClientId,
    c: &ClientEntry,
    viewport: (u16, u16),
) -> OverlayCtx {
    let (cols, rows) = c.size;
    let geom = status_geometry(model, c.session, rows);
    let win_rows = rows - geom.lines;
    let top = u32::from(geom.window_top);
    let window = model
        .sessions
        .get(&c.session)
        .and_then(|s| model.windows.get(&s.active));
    let panes = window
        .map(visible_panes)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(pane, r)| {
            let (vx, vy) = (u32::from(viewport.0), u32::from(viewport.1));
            let left = u32::from(r.x).max(vx);
            let upper = u32::from(r.y).max(vy);
            let right = (u32::from(r.x) + u32::from(r.cols)).min(vx + u32::from(cols));
            let bottom = (u32::from(r.y) + u32::from(r.rows)).min(vy + u32::from(win_rows));
            (left < right && upper < bottom).then(|| {
                let rect = Rect {
                    x: (left - vx) as u16,
                    y: (upper - vy + top) as u16,
                    cols: (right - left) as u16,
                    rows: (bottom - upper) as u16,
                };
                (pane, rect)
            })
        })
        .collect();
    OverlayCtx {
        client: id,
        viewport,
        status_row: (geom.lines > 0).then_some(geom.first_row),
        window_top: geom.window_top,
        panes,
    }
}

/// Where things are on client `id`'s screen as last drawn, for an input
/// hook mapping a pointer to a pane.
pub fn overlay_ctx(state: &State, id: ClientId) -> Option<OverlayCtx> {
    let c = state.clients.get(&id)?;
    Some(overlay_ctx_at(&state.model, id, c, c.viewport))
}

/// A window option's style, looked up from the window's active pane.
pub(super) fn window_style(
    model: &Model,
    w: &crate::server::model::Window,
    name: &str,
) -> status::Style {
    let chain = options::chain(model, w.active);
    status::parse_style(model.options.lookup(name, &chain).unwrap_or(""))
}

/// What a pane's default-colour cells show: `window-style`'s colours, and
/// for the window's active pane `window-active-style`'s where it sets one
/// (tmux's `tty_default_colours`).
pub(super) fn window_style_colours<'a>(
    opt: &dyn Fn(&str) -> &'a str,
    vars: &dyn FormatVars,
    is_active: bool,
) -> (Color, Color) {
    let plain = status::base_style(opt("window-style"), vars);
    let active = status::base_style(opt("window-active-style"), vars);
    let fg = if is_active && active.fg != Color::Named(NamedColor::Foreground) {
        active.fg
    } else {
        plain.fg
    };
    let bg = if is_active && active.bg != Color::Named(NamedColor::Background) {
        active.bg
    } else {
        plain.bg
    };
    (fg, bg)
}

/// Tells every pane the colours its `window-style` gives it, which its
/// OSC 10 and OSC 11 queries are answered with.
pub(super) fn sync_pane_defaults(model: &Model) {
    for w in model.windows.values() {
        for id in w.pane_ids() {
            let Some(pane) = model.panes.get(&id) else {
                continue;
            };
            let (fg, bg) = pane_defaults(model, id);
            let (fg, bg) = with_terminal(model, colour_rgb(fg), colour_rgb(bg));
            pane.handle.set_default_colours(fg, bg);
        }
    }
}

/// The colours a pane about to spawn answers OSC 10 and 11 with, given to its
/// [`PaneSpawn`](crate::server::pane::PaneSpawn) so a program that asks at once
/// is answered with the style, not the fixed palette.
/// The window may not be in the model yet, so the styles are read as the
/// window's active pane would see them, without formats.
pub(crate) fn default_colours(
    model: &Model,
    (session, window): (SessionId, WindowId),
) -> (Option<Rgb>, Option<Rgb>) {
    let chain = [Owner::Window(window), Owner::Session(session)];
    let opt = |name: &str| model.options.lookup(name, &chain).unwrap_or("");
    let (fg, bg) = window_style_colours(&opt, &NoVars, true);
    with_terminal(model, colour_rgb(fg), colour_rgb(bg))
}

/// A colour `window-style` leaves to the terminal is the attached
/// terminal's own, when it reported one.
pub(super) fn with_terminal(
    model: &Model,
    fg: Option<Rgb>,
    bg: Option<Rgb>,
) -> (Option<Rgb>, Option<Rgb>) {
    let rgb = |[r, g, b]: [u8; 3]| Rgb { r, g, b };
    let term = model.terminal_colours;
    (fg.or(term.map(|t| rgb(t.0))), bg.or(term.map(|t| rgb(t.1))))
}

/// The colours the default-colour cells of `pane` show (`window-style`,
/// `window-active-style`), the terminal's own where they set none.
pub(crate) fn pane_defaults(model: &Model, pane: PaneId) -> (Color, Color) {
    let chain = options::chain(model, pane);
    let opt = |name: &str| model.options.lookup(name, &chain).unwrap_or("");
    let active = model
        .panes
        .get(&pane)
        .and_then(|p| model.windows.get(&p.window))
        .is_some_and(|w| w.active == pane);
    window_style_colours(&opt, &FormatCtx::new(model, pane), active)
}

/// `colour` as RGB (tmux's `colour_force_rgb`); `None` for the terminal's
/// own default.
pub(super) fn colour_rgb(colour: Color) -> Option<Rgb> {
    let index = match colour {
        Color::Spec(rgb) => return Some(rgb),
        Color::Indexed(n) => usize::from(n),
        Color::Named(name) => {
            let n = name as usize;
            if n >= 16 {
                return None;
            }
            n
        }
    };
    const ANSI: [u32; 16] = [
        0x000000, 0x800000, 0x008000, 0x808000, 0x000080, 0x800080, 0x008080, 0xc0c0c0, 0x808080,
        0xff0000, 0x00ff00, 0xffff00, 0x0000ff, 0xff00ff, 0x00ffff, 0xffffff,
    ];
    let v = match index {
        0..16 => ANSI[index],
        16..232 => {
            const LEVELS: [u32; 6] = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
            let i = index - 16;
            LEVELS[i / 36] << 16 | LEVELS[i / 6 % 6] << 8 | LEVELS[i % 6]
        }
        _ => {
            let g = 8 + 10 * (index as u32 - 232);
            g << 16 | g << 8 | g
        }
    };
    Some(Rgb {
        r: (v >> 16) as u8,
        g: (v >> 8) as u8,
        b: v as u8,
    })
}

/// How `w`'s visible `panes` look to client `c`: the colours their
/// default-colour cells take (`window-style`, and `window-active-style` for
/// the window's active pane where it sets one), their border styles and
/// titles (`pane-border-format`), and the window's border options.
pub(super) fn pane_looks(
    state: &State,
    c: &ClientEntry,
    w: &crate::server::model::Window,
    panes: &[PaneId],
) -> render::PaneLooks {
    let model = &state.model;
    let wchain = options::chain(model, w.active);
    let wopt = |name: &str| model.options.lookup(name, &wchain).unwrap_or("");
    let mut looks = render::PaneLooks {
        status: PaneStatus::parse(wopt("pane-border-status")),
        lines: PaneLines::parse(wopt("pane-border-lines")),
        indicators: Indicators::parse(wopt("pane-border-indicators")),
        // `fill-character`: its first character, if one cell wide.
        fill: wopt("fill-character")
            .chars()
            .next()
            .filter(|c| unicode_width::UnicodeWidthChar::width(*c) == Some(1)),
        ..Default::default()
    };
    // FR.2: the frames, their title and lines, and the drop preview.
    render::apply_frames(model, w, &mut looks);
    looks.lifted = crate::server::mouse::lifted(state, w.id).map(|(p, _)| p);
    let titled = looks.status != PaneStatus::Off || looks.frame_title;
    // grofe identity's framed titles end in a control tab while the mouse
    // is on; tmux identity keeps tmux's stock title, buttons and all.
    let tabbed = model.identity != crate::compat::Identity::Tmux
        && !looks.frames.is_empty()
        && looks.frame_title
        && wopt("mouse") == "on";
    let order = w.pane_ids();
    let base_index = model.pane_base_index(w.id) as usize;
    let now = local_now();
    for id in panes {
        let chain = options::chain(model, *id);
        let opt = |name: &str| model.options.lookup(name, &chain).unwrap_or("");
        let client_vars = ClientVars::new(model, c, Some(FormatCtx::new(model, *id)));
        let vars = options::ServerVars::new(state, *id, &client_vars);
        let (fg, bg) = window_style_colours(&opt, &vars, *id == w.active);
        let format = match opt("pane-border-format") {
            // The tab takes the place of the default format's buttons.
            f if tabbed && f == options::PANE_BORDER_FORMAT => {
                options::frame_title_format(opt("pane-frame-tab-ends"))
            }
            f => f.to_string(),
        };
        let title = if !titled {
            String::new()
        } else {
            status::expand_time(&format, &vars, &now)
        };
        let tab = tabbed.then(|| {
            render::frame_tab(
                opt("pane-frame-tab-ends"),
                w.zoomed,
                w.floating(*id).is_some(),
            )
        });
        // A blocked agent's frame asks for attention, active or not.
        let blocked = state
            .agents
            .get(*id)
            .is_some_and(|a| a.state == crate::server::agents::AgentState::Blocked);
        let (border, active_border) = if blocked {
            let style = status::base_style(opt("agent-blocked-border-style"), &vars);
            (style, style)
        } else {
            (
                status::base_style(opt("pane-border-style"), &vars),
                status::base_style(opt("pane-active-border-style"), &vars),
            )
        };
        looks.panes.insert(
            *id,
            render::PaneLook {
                fg,
                bg,
                border,
                active_border,
                title,
                tab,
                index: order.iter().position(|p| p == id).unwrap_or(0) + base_index,
            },
        );
    }
    looks
}

/// The `#[range=control|N]` at window column `x` of the top edge of
/// `pane`'s frame, which runs from column `x0` to `x1` (its corners), in
/// client `id`'s framed window: grofe identity's control tab, or a control
/// of the pane's `pane-border-format` (tmux's default `[z]` and `[x]`),
/// laid out as [`render::frame_title`] draws them.
pub(crate) fn frame_title_control(
    state: &State,
    id: ClientId,
    pane: PaneId,
    (x0, x1): (i32, i32),
    x: i32,
) -> Option<u8> {
    let c = state.clients.get(&id)?;
    let model = &state.model;
    let w = model
        .sessions
        .get(&c.session)
        .and_then(|s| model.windows.get(&s.active))?;
    let look = pane_looks(state, c, w, &[pane]);
    if !look.frame_title {
        return None;
    }
    let l = look.panes.get(&pane)?;
    let plain = status::Style::default();
    let edge = render::frame_title(
        (x0, x1),
        i32::from(w.cols),
        &l.title,
        l.tab.as_deref(),
        plain,
        '─',
    );
    match edge.range_at(x)?.kind {
        status::RangeKind::Control(n) => Some(n as u8),
        _ => None,
    }
}

/// The border titles client `id`'s window shows, as drawn: where each
/// pane's title is and what its `#[range=...]` parts cover, for the mouse.
pub(crate) fn border_titles(state: &State, id: ClientId) -> Vec<render::Title> {
    let Some(c) = state.clients.get(&id) else {
        return Vec::new();
    };
    let model = &state.model;
    let Some(w) = model
        .sessions
        .get(&c.session)
        .and_then(|s| model.windows.get(&s.active))
    else {
        return Vec::new();
    };
    if w.pane_status == PaneStatus::Off {
        return Vec::new();
    }
    let rects = visible_panes(w);
    let ids: Vec<PaneId> = rects.iter().map(|(id, _)| *id).collect();
    let look = pane_looks(state, c, w, &ids);
    let plain = status::Style::default();
    render::border_titles(
        &look,
        (i32::from(w.cols), i32::from(w.rows)),
        &rects,
        w.active,
        (plain, plain),
    )
}

/// A composed frame: the screen, its viewport, its status rows and the
/// client row the first of them is on.
pub(super) type Composed = (Screen, (u16, u16), Vec<StatusRow>, u16);

/// Composes client `c`'s next screen, the viewport it was drawn with, its
/// status rows and the client row the first of them is on: the window,
/// the status rows above or below it (`status-position`), then every
/// overlay (lowest priority first), then the wide-pair fix-up.
pub(super) fn compose_client(state: &State, id: ClientId, c: &ClientEntry) -> Option<Composed> {
    let model = &state.model;
    let s = model.sessions.get(&c.session)?;
    let w = model.windows.get(&s.active)?;
    let geom = status_geometry(model, s.id, c.size.1);
    let visible = visible_size(state, id, c);
    let area = w.area();
    let rects = visible_panes(w);
    let borders = if w.zoomed {
        Vec::new()
    } else {
        w.layout.borders(area)
    };
    // A pane moved with the mouse is drawn under the pointer, above
    // everything else, and its own place shows empty (FR.3).
    let lifted = crate::server::mouse::lifted(state, w.id);
    let is_lifted = |id: PaneId| lifted.is_some_and(|(p, _)| p == id);
    let mut cursor = None;
    let panes: Vec<_> = rects
        .into_iter()
        .filter(|(id, _)| !is_lifted(*id))
        .filter_map(|(id, rect)| {
            let snap = model.panes.get(&id)?.handle.snapshot_rows(0..rect.rows);
            if id == w.active {
                cursor = Some(CursorState {
                    row: snap.cursor.row.saturating_add(rect.y),
                    col: snap.cursor.col.saturating_add(rect.x),
                    ..snap.cursor
                });
            }
            Some((id, rect, snap))
        })
        .collect();
    let viewport = match c.pan {
        Some((window, x, y)) if window == w.id => follow((x, y), None, (w.cols, w.rows), visible),
        _ => follow(
            c.viewport,
            cursor.map(|p| (p.col, p.row)),
            (w.cols, w.rows),
            visible,
        ),
    };
    let shown: Vec<PaneId> = panes
        .iter()
        .map(|(id, _, _)| *id)
        .chain(w.floating.iter().map(|f| f.pane).filter(|_| !w.zoomed))
        .chain(lifted.map(|(p, _)| p))
        .collect();
    let look = pane_looks(state, c, w, &shown);
    let mut window = render::compose(&FrameInputs {
        client: visible,
        viewport,
        area,
        panes,
        borders,
        border_style: window_style(model, w, "pane-border-style"),
        active_border_style: window_style(model, w, "pane-active-border-style"),
        active: w.active,
        // The border is marked in the session the mark was made in.
        marked: model
            .marked_pane()
            .filter(|_| model.marked.is_some_and(|(s, ..)| s == c.session))
            .filter(|p| model.panes.get(p).is_some_and(|p| p.window == w.id)),
        status: None,
        cursor,
        graphics: state.graphics.remap(),
        images: Some(ImageCaps::of(&c.caps)),
        look: look.clone(),
    });
    screenimg::mark_covered(&mut window, 0);
    if (!w.zoomed && !w.floating.is_empty()) || lifted.is_some() {
        let mut stack = w.floating.clone();
        stack.sort_by_key(|f| f.z);
        let floating_pane = |id: PaneId, at: (i32, i32)| {
            let p = model.panes.get(&id)?;
            Some(render::FloatingPane {
                id,
                at,
                size: (p.cols, p.rows),
                snap: p.handle.snapshot_rows(0..p.rows),
                active: id == w.active,
            })
        };
        let floating: Vec<render::FloatingPane> = stack
            .iter()
            .filter(|f| !w.zoomed && !is_lifted(f.pane))
            .map(|f| (f.pane, (f.x, f.y)))
            .chain(lifted)
            .filter_map(|(id, at)| floating_pane(id, at))
            .collect();
        render::draw_floating(
            &mut window,
            &floating,
            viewport,
            &window_style(model, w, "pane-border-style"),
            &window_style(model, w, "pane-active-border-style"),
            &look,
        );
    }
    // Floating panes cover what is under them, images included.
    screenimg::hide_covered(&mut window);
    // The columns the sidebar holds stay blank here; its overlay draws them.
    let window = widen(window, c.size.0);
    let (rows, message) = status_rows(state, c, s, geom.lines);
    let pill = c.message.as_ref().and_then(|(text, _, level)| {
        Some((
            text,
            *level,
            pill_look(model, &options::chain(model, w.active), *level)?,
        ))
    });
    let mut lines: Vec<StyledLine> = rows.iter().map(|r| r.line.clone()).collect();
    let mut screen = if !lines.is_empty() {
        if let Some((line, m)) = &message
            && pill.is_none()
        {
            lines[*line] = m.line.clone();
        }
        render::with_status(window, &lines, geom.top)
    } else {
        // ponytail: with `status off` a message still takes the last row,
        // drawn over the window's last row while it shows.
        let mut window = window;
        if let Some((_, m)) = &message
            && pill.is_none()
            && let Some(last) = window.rows.checked_sub(1)
        {
            screenimg::mark_covered(&mut window, 0);
            render::draw_row(&mut window, last, &m.line);
            screenimg::hide_covered(&mut window);
        }
        window
    };
    let cx = overlay_ctx_at(model, id, c, viewport);
    screenimg::mark_covered(&mut screen, 0);
    if let Some((text, level, look)) = pill {
        let (row, span) = match &message {
            Some((line, _)) if geom.lines > 0 => (
                geom.first_row + *line as u16,
                pill_span(&rows, *line, screen.cols),
            ),
            _ => (screen.rows.saturating_sub(1), usize::from(screen.cols)),
        };
        render::draw_message_pill(&mut screen, row, span, text, level, &look);
    }
    for overlay in state.registry.overlays.iter().rev() {
        overlay.draw(state, id, &cx, &mut screen);
    }
    // tmux 3.7b hides the cursor while a message shows (`TTY_NOCURSOR`).
    if message.is_some() {
        screen.cursor = None;
    }
    render::fix_wide_pairs(&mut screen);
    // Popups, menus, prompts and choosers cover images the same way.
    screenimg::hide_covered(&mut screen);
    Some((screen, viewport, rows, geom.first_row))
}

/// `window` with blank columns added on the right up to `cols`.
pub(super) fn widen(window: Screen, cols: u16) -> Screen {
    if window.cols >= cols {
        return window;
    }
    let mut wide = Screen::new(cols, window.rows);
    let (old, new) = (usize::from(window.cols), usize::from(cols));
    for (row, cells) in window.cells.chunks(old.max(1)).enumerate() {
        wide.cells[row * new..row * new + old].clone_from_slice(cells);
    }
    wide.cursor = window.cursor;
    wide.images = window.images;
    wide
}

/// Client `c`'s `lines` status rows on session `s` (spec §4.10), and its
/// message row if one shows. Row N draws `status-format[N]`; an empty
/// `status-format[0]` (the default) is tmux's default row, spelled by
/// [`status::render_row`]; a row with no entry is blank in `status-style`.
/// Formats expand against the current window's active pane with the
/// server's and the client's variables over it (D9: `pane_title` and the
/// host), options come from that pane's chain, and every window of the
/// list expands against its own active pane.
pub(super) fn status_rows(
    state: &State,
    c: &ClientEntry,
    s: &Session,
    lines: u16,
) -> (Vec<StatusRow>, Option<(usize, StatusRow)>) {
    let model = &state.model;
    let width = c.size.0;
    let active = model.windows.get(&s.active).map(|w| w.active);
    let chain = active.map(|p| options::chain(model, p)).unwrap_or_default();
    let chain = if chain.is_empty() {
        vec![Owner::Session(s.id)]
    } else {
        chain
    };
    let opt = |name: &str, default: &'static str| {
        model
            .options
            .lookup(name, &chain)
            .map_or(default, |v| v as &str)
    };
    let client_vars = ClientVars::new(model, c, active.map(|p| FormatCtx::new(model, p)));
    let server_vars = active.map(|p| options::ServerVars::new(state, p, &client_vars));
    let vars: &dyn FormatVars = match &server_vars {
        Some(v) => v,
        None => &client_vars,
    };
    let style = status_style(opt);
    let style = style.as_str();
    let now = local_now();
    // The message, formatted by `message-format` (`#{message}` and
    // `#{command_prompt}` set), drawn over the status line `message-line`
    // names (clamped to the last), whose cells it keeps where it writes none.
    let message_format = |under: Option<&StatusRow>| {
        let (text, _, _) = c.message.as_ref()?;
        let vars = MessageVars {
            inner: vars,
            message: text,
            command_prompt: false,
        };
        let expanded = status::expand_time(opt("message-format", MESSAGE_FORMAT), &vars, &now);
        Some(match under {
            Some(row) => {
                let cells: Vec<(char, status::Style)> = row
                    .line
                    .spans
                    .iter()
                    .flat_map(|span| span.text.chars().map(move |ch| (ch, span.style)))
                    .collect();
                status::draw_over(&expanded, status::Style::default(), &cells)
            }
            None => status::draw(&expanded, status::Style::default(), width),
        })
    };
    if lines == 0 {
        return (Vec::new(), message_format(None).map(|m| (0, m)));
    }
    let formats = model.options.lookup_list("status-format", &chain);
    let rows: Vec<StatusRow> = (0..u32::from(lines))
        .map(|i| match formats.and_then(|f| f.get(&i)) {
            Some(t) if t.is_empty() && i == 0 => default_row(state, c, s, &chain, vars, now),
            Some(t) => status::draw_row(t, style, vars, width, &now),
            None => status::blank_row(style, vars, width),
        })
        .collect();
    let line = opt("message-line", "0")
        .parse::<usize>()
        .unwrap_or(0)
        .min(usize::from(lines) - 1);
    let message = message_format(rows.get(line)).map(|m| (line, m));
    (rows, message)
}

/// The format variables a message or prompt label is drawn with.
pub(crate) struct MessageVars<'a> {
    pub inner: &'a dyn FormatVars,
    pub message: &'a str,
    /// `#{command_prompt}`: the prompt is in vi's command mode.
    pub command_prompt: bool,
}

impl FormatVars for MessageVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        match name {
            "message" => Some(self.message.to_string()),
            "command_prompt" => Some(u8::from(self.command_prompt).to_string()),
            _ => self.inner.get(name),
        }
    }
}

/// tmux's default status row for client `c` on session `s`: status-left,
/// the window list, status-right.
pub(super) fn default_row(
    state: &State,
    c: &ClientEntry,
    s: &Session,
    chain: &[Owner],
    vars: &dyn FormatVars,
    now: libc::tm,
) -> StatusRow {
    let model = &state.model;
    let opt = |name: &str, default: &'static str| {
        model
            .options
            .lookup(name, chain)
            .map_or(default, |v| v as &str)
    };
    let length = |name: &str| opt(name, "0").parse::<u16>().unwrap_or(0);
    let listed: Vec<(&crate::server::model::Window, Vec<Owner>)> = s
        .windows
        .iter()
        .filter_map(|id| {
            let w = model.windows.get(id)?;
            Some((w, options::chain(model, w.active)))
        })
        .collect();
    let client_vars: Vec<ClientVars> = listed
        .iter()
        .map(|(w, _)| {
            let vars = FormatCtx::new(model, w.active).in_session(s.id);
            ClientVars::new(model, c, Some(vars))
        })
        .collect();
    let server_vars: Vec<options::ServerVars> = listed
        .iter()
        .zip(&client_vars)
        .map(|((w, _), v)| options::ServerVars::new(state, w.active, v))
        .collect();
    let entries: Vec<WindowStatus> = listed
        .iter()
        .zip(&server_vars)
        .map(|((w, chain), vars)| {
            let wopt = |name: &str, default: &'static str| {
                model
                    .options
                    .lookup(name, chain)
                    .map_or(default, |v| v as &str)
            };
            let defaults = WindowOptions::default();
            let current = w.id == s.active;
            WindowStatus {
                format: if current {
                    wopt("window-status-current-format", "")
                } else {
                    wopt("window-status-format", "")
                },
                vars,
                index: model.index_in(s.id, w.id).unwrap_or(w.index),
                current,
                last: s.last_windows.last() == Some(&w.id),
                bell: w.bell_flag,
                alert: w.activity_flag || w.silence_flag,
                options: WindowOptions {
                    separator: wopt("window-status-separator", defaults.separator),
                    style: wopt("window-status-style", defaults.style),
                    current_style: wopt("window-status-current-style", defaults.current_style),
                    last_style: wopt("window-status-last-style", defaults.last_style),
                    bell_style: wopt("window-status-bell-style", defaults.bell_style),
                    activity_style: wopt("window-status-activity-style", defaults.activity_style),
                },
            }
        })
        .collect();
    status::render_row(&StatusInputs {
        left: opt("status-left", ""),
        right: opt("status-right", ""),
        left_length: length("status-left-length"),
        right_length: length("status-right-length"),
        style: &status_style(opt),
        left_style: opt("status-left-style", "default"),
        right_style: opt("status-right-style", "default"),
        justify: opt("status-justify", "left"),
        vars,
        windows: &entries,
        width: c.size.0,
        now,
    })
}

/// `status-style` with `status-fg` and `status-bg` over it, as tmux draws
/// the status line (status.c): a colour other than `default` replaces the
/// style's.
pub(super) fn status_style<'a>(opt: impl Fn(&str, &'static str) -> &'a str) -> String {
    let mut style = opt("status-style", "").to_string();
    for (key, name) in [("fg", "status-fg"), ("bg", "status-bg")] {
        let colour = opt(name, "default");
        if colour != "default" && colour != "terminal" {
            style.push_str(&format!(",{key}={colour}"));
        }
    }
    style
}

/// The local broken-down time, for strftime sequences in the status line.
pub(super) fn local_now() -> libc::tm {
    // SAFETY: `time` with a null pointer only returns the time;
    // `localtime_r` writes into the zeroed `tm` we own (all-zero is a
    // valid `tm`).
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    }
}

/// Format variables for a client: `client_*` (real
/// `client_width`/`client_height`), falling through to the pane's
/// [`FormatCtx`].
pub struct ClientVars<'a> {
    pub(super) model: &'a Model,
    pub(super) client: &'a ClientEntry,
    pub(super) pane: Option<FormatCtx<'a>>,
}

impl<'a> ClientVars<'a> {
    pub fn new(
        model: &'a Model,
        client: &'a ClientEntry,
        pane: Option<FormatCtx<'a>>,
    ) -> ClientVars<'a> {
        // A client sees a linked window as its own session has it.
        let pane = pane.map(|p| p.or_session(client.session));
        ClientVars {
            model,
            client,
            pane,
        }
    }

    pub(super) fn session_name(&self, session: SessionId) -> String {
        self.model
            .sessions
            .get(&session)
            .map(|s| s.name.clone())
            .unwrap_or_default()
    }
}

impl FormatVars for ClientVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        let c = self.client;
        match name {
            "client_width" => Some(c.size.0.to_string()),
            "client_height" => Some(c.size.1.to_string()),
            "client_name" => Some(c.name.clone()),
            "client_tty" => Some(c.tty.clone().unwrap_or_default()),
            "client_pid" => Some(c.pid.unwrap_or(0).to_string()),
            "client_termname" => Some(c.term.clone()),
            // tmux: bytes not sent because the client fell behind; here,
            // the dropped frames' diffs.
            "client_discarded" => Some(c.outbox.discarded.to_string()),
            "client_key_table" => Some(c.keys.table().to_string()),
            // ponytail: grofe always speaks UTF-8 to the client.
            "client_flags" => Some({
                let mut flags = vec!["attached"];
                if c.focused {
                    flags.push("focused");
                }
                flags.extend(c.flags.names());
                if c.suspended {
                    flags.push("suspended");
                }
                flags.push("UTF-8");
                flags.join(",")
            }),
            // tmux: 1 while the client is in any table but its session's
            // `key-table` (always `root` here).
            "client_prefix" => Some(
                if c.keys.table() == ROOT_TABLE {
                    "0"
                } else {
                    "1"
                }
                .to_string(),
            ),
            "client_activity" => Some(
                c.activity_time
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs())
                    .to_string(),
            ),
            // The terminal's own answer to XDA is not asked for.
            "client_termtype" => Some(String::new()),
            "client_utf8" => Some("1".to_string()),
            // Only this user's processes can connect to the socket.
            "client_uid" => Some(nix::unistd::getuid().as_raw().to_string()),
            "client_user" => Some(
                nix::unistd::User::from_uid(nix::unistd::getuid())
                    .ok()
                    .flatten()
                    .map(|u| u.name)
                    .unwrap_or_default(),
            ),
            "client_readonly" | "client_control_mode" => Some("0".to_string()),
            "client_session" => Some(self.session_name(c.session)),
            "client_last_session" => Some(
                self.model
                    .client_last_session
                    .get(&c.ctx.client?)
                    .map(|s| self.session_name(*s))
                    .unwrap_or_default(),
            ),
            _ => self.pane.as_ref()?.get(name),
        }
    }

    fn loop_items(&self, kind: LoopKind, sort: LoopSort) -> Option<Vec<LoopItem<'_>>> {
        let pane = self.pane.as_ref()?;
        let specs = options::loop_specs(
            self.model,
            pane.pane_id(),
            None,
            Some(self.client.session),
            kind,
            sort,
        )?;
        Some(
            specs
                .into_iter()
                .map(|s| LoopItem {
                    vars: Box::new(ClientVars::new(
                        self.model,
                        self.client,
                        Some(match s.session {
                            Some(session) => {
                                FormatCtx::at(self.model, s.pane, s.level).in_session(session)
                            }
                            None => FormatCtx::at(self.model, s.pane, s.level),
                        }),
                    )),
                    pane: Some(s.pane),
                    active: s.active,
                    extras: s.extras,
                })
                .collect(),
        )
    }

    fn name_exists(&self, kind: NameKind, name: &str) -> Option<bool> {
        self.pane.as_ref()?.name_exists(kind, name)
    }

    fn pane_search(&self, term: &str, regex: bool, ignore: bool) -> Option<u32> {
        self.pane.as_ref()?.pane_search(term, regex, ignore)
    }

    fn reexpand(&self, text: &str) -> String {
        match &self.pane {
            Some(pane) => pane.reexpand(text),
            None => text.to_string(),
        }
    }
}
