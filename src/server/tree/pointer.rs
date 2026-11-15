use super::*;

// --- Mouse --------------------------------------------------------------

/// What a mouse report is to the chooser, as `mode_tree_key` tells keys
/// apart: `MouseDown1Pane`, `MouseDown3Pane`, `DoubleClick1Pane`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Press {
    Down1,
    Down3,
    Double1,
    Other,
}

/// Runs what a key of the chooser asks for.
pub(super) fn apply(state: &mut State, client: ClientId, routed: Routed) {
    match routed {
        Routed::Command(cmds, dialect) => clients::run_binding(state, client, &cmds, dialect),
        Routed::Bound(cmds, dialect) => {
            clients::run_commands_for(state, client, &cmds, dialect, true, None, None);
        }
        Routed::ToPane(_) | Routed::Consumed => {}
    }
    clients::mark_dirty(state, client);
}

/// Makes line `line` of `client`'s chooser the current one.
pub(super) fn point_at(state: &mut State, client: ClientId, line: usize) {
    let Some(tree) = state.trees.open.get(&client) else {
        return;
    };
    let v = view(state, tree);
    let Some(tag) = v.lines.get(line).map(|l| l.tag) else {
        return;
    };
    if let Some(tree) = state.trees.open.get_mut(&client) {
        if tree.tag != Some(tag) {
            tree.preview_offset = 0;
        }
        tree.current = line;
        tree.offset = v.offset;
        tree.tag = Some(tag);
    }
}

/// A mouse report at cell `(x, y)` of the chooser's pane, as tmux's
/// `mode_tree_key` and `window_tree_mouse` take one: a press picks the
/// line, a double click chooses it, a right press picks it and opens the
/// mode's menu, and a press in the preview below the list picks a window
/// or pane of its strip. The wheel does nothing (recorded).
pub fn mouse(
    state: &mut State,
    client: ClientId,
    pane: PaneId,
    m: &crate::server::input::Mouse,
    action: Option<crate::server::input::MouseAction>,
    (x, y): (u16, u16),
) {
    use crate::server::input::{MouseAction, MouseButton, MouseKind};
    let Some(tree) = state.trees.open.get(&client).filter(|t| t.pane == pane) else {
        return;
    };
    let (preview_off, is_tree) = (
        tree.preview == PreviewMode::Off,
        matches!(tree.kind, Kind::Tree { .. }),
    );
    let left = matches!(m.kind, MouseKind::Down(MouseButton::Left));
    let right = matches!(m.kind, MouseKind::Down(MouseButton::Right));
    let press = match action {
        Some(MouseAction::DoubleClick) if left => Press::Double1,
        Some(MouseAction::Down) | None if left => Press::Down1,
        Some(MouseAction::Down) | None if right => Press::Down3,
        _ => Press::Other,
    };
    let v = view(state, tree);
    if v.lines.is_empty() {
        return;
    }
    let n = v.lines.len();
    let cols = state
        .model
        .panes
        .get(&pane)
        .map_or(0, |p| usize::from(p.cols));
    let (xu, yu) = (usize::from(x), usize::from(y));
    if xu > cols || yu > v.height {
        let line = if v.offset + yu > n - 1 {
            v.current
        } else {
            v.offset + yu
        };
        if press == Press::Down3 {
            open_menu(state, client, line, (x, y), true);
        }
        if preview_off {
            return;
        }
        if press == Press::Down1 && is_tree {
            strip_press(state, client, &v, xu, cols);
        }
        return;
    }
    if v.offset + yu < n {
        let line = v.offset + yu;
        if press != Press::Other {
            point_at(state, client, line);
        }
        match press {
            Press::Double1 => {
                let routed = key_press(state, client, &Key::plain(KeyCode::Enter));
                apply(state, client, routed);
            }
            Press::Down3 => open_menu(state, client, line, (x, y), false),
            _ => {}
        }
    } else if press == Press::Down3 {
        open_menu(state, client, v.current, (x, y), false);
    }
    clients::mark_dirty(state, client);
}

/// `window_tree_mouse`: a press at column `x` of the preview strip of the
/// current session or window. The arrows scroll the strip; a preview
/// picks its window or pane, which is then chosen.
pub(super) fn strip_press(state: &mut State, client: ClientId, v: &View, x: usize, cols: usize) {
    let line = &v.lines[v.current];
    let tag = line.tag;
    let Some(tree) = state.trees.open.get(&client) else {
        return;
    };
    let Some((entries, current)) = strip_of(state, tag) else {
        return;
    };
    let width = cols.saturating_sub(4);
    let Some(g) = strip(entries.len(), current, width, tree.preview_offset) else {
        return;
    };
    // The strip is drawn from column 2 (`mode_tree_draw`).
    let left = g.left.then_some(2 + 2);
    let right = g.right.then_some(2 + width - 3);
    if left.is_some_and(|l| x <= l) {
        let routed = key_press(state, client, &Key::plain(KeyCode::Char('<')));
        apply(state, client, routed);
        return;
    }
    if right.is_some_and(|r| x >= r) {
        let routed = key_press(state, client, &Key::plain(KeyCode::Char('>')));
        apply(state, client, routed);
        return;
    }
    let mut x = match left {
        Some(l) => x - l,
        None => x.saturating_sub(1),
    };
    if x == 0 || g.end == 0 {
        x = 0;
    } else {
        x /= g.each;
        if g.start + x >= g.end {
            x = g.end - 1;
        }
    }
    let at = g.start + x;
    let model = &state.model;
    let child = match tag {
        Tag::Session(s) => model
            .sessions
            .get(&s)
            .and_then(|s| s.windows.get(at))
            .map(|w| Tag::Window(*w)),
        Tag::Window(w) => model
            .windows
            .get(&w)
            .and_then(|w| w.pane_ids().get(at).copied())
            .map(Tag::Pane),
        _ => None,
    };
    let Some(child) = child else {
        return;
    };
    if let Some(tree) = state.trees.open.get_mut(&client) {
        tree.expanded.insert(tag, true);
    }
    rebuild(state, client, Some(child));
    let routed = key_press(state, client, &Key::plain(KeyCode::Enter));
    apply(state, client, routed);
}

/// The rows of a chooser's menu (the mode's `menu` items), by kind: the
/// label and the key it presses. `Tag All` shows its key as tmux spells
/// it, `[DC4]`.
pub(super) fn menu_rows(kind: &Kind) -> &'static [(&'static str, &'static str)] {
    const TREE: &[(&str, &str)] = &[
        ("Select", "Enter"),
        ("Expand", "Right"),
        ("Mark", "m"),
        ("", ""),
        ("Tag", "t"),
        ("Tag All", "C-t"),
        ("Tag None", "T"),
        ("", ""),
        ("Kill", "x"),
        ("Kill Tagged", "X"),
        ("", ""),
        ("Cancel", "q"),
    ];
    const CLIENTS: &[(&str, &str)] = &[
        ("Detach", "d"),
        ("Detach Tagged", "D"),
        ("", ""),
        ("Tag", "t"),
        ("Tag All", "C-t"),
        ("Tag None", "T"),
        ("", ""),
        ("Cancel", "q"),
    ];
    const BUFFERS: &[(&str, &str)] = &[
        ("Paste", "p"),
        ("Paste Tagged", "P"),
        ("", ""),
        ("Tag", "t"),
        ("Tag All", "C-t"),
        ("Tag None", "T"),
        ("", ""),
        ("Delete", "d"),
        ("Delete Tagged", "D"),
        ("", ""),
        ("Cancel", "q"),
    ];
    const CUSTOMIZE: &[(&str, &str)] = &[
        ("Select", "Enter"),
        ("Expand", "Right"),
        ("", ""),
        ("Tag", "t"),
        ("Tag All", "C-t"),
        ("Tag None", "T"),
        ("", ""),
        ("Cancel", "q"),
    ];
    match kind {
        Kind::Tree { .. } => TREE,
        Kind::Clients => CLIENTS,
        Kind::Buffers => BUFFERS,
        Kind::Customize { .. } => CUSTOMIZE,
    }
}

/// `mode_tree_display_menu`: the mode's menu for `line` (titled with its
/// name), or with `outside` the scroll menu, at the pointer.
pub(super) fn open_menu(
    state: &mut State,
    client: ClientId,
    line: usize,
    (x, y): (u16, u16),
    outside: bool,
) {
    use crate::server::menu::{self, Look, Menu, MenuItem, Position};
    let Some(tree) = state.trees.open.get(&client) else {
        return;
    };
    let v = view(state, tree);
    let Some(l) = v.lines.get(line) else {
        return;
    };
    let rows: &[(&str, &str)] = if outside {
        &[
            ("Scroll Left", "<"),
            ("Scroll Right", ">"),
            ("", ""),
            ("Cancel", "q"),
        ]
    } else {
        menu_rows(&tree.kind)
    };
    let title = if outside {
        String::new()
    } else {
        format!("#[align=centre]{}", l.name)
    };
    let items = rows
        .iter()
        .map(|(label, key)| {
            if label.is_empty() {
                return MenuItem::Separator;
            }
            // tmux's item key for Tag All is the raw control character,
            // which it spells `[DC4]` and no typed key matches.
            let (text, shown) = if *key == "C-t" {
                (format!("{label}#[default] #[align=right]([DC4])"), None)
            } else {
                (
                    (*label).to_string(),
                    crate::server::input::parse_key(key).ok(),
                )
            };
            MenuItem::Command {
                label: text,
                key: shown,
                command: (*key).to_string(),
            }
        })
        .collect();
    let Some(ctx) = state.clients.get(&client).map(|c| c.ctx.clone()) else {
        return;
    };
    let menu = Menu {
        title: Some(title),
        items,
        x: Position::Pointer(x),
        y: Position::Pointer(y),
        look: Look {
            mouse: true,
            internal: crate::server::menu::Internal::Tree(line),
            ..Look::default()
        },
    };
    menu::open(state, &ctx, client, menu);
}

/// Row `key` of the chooser's menu for `line` was chosen
/// (`mode_tree_menu_callback`): that line becomes current, and the key is
/// pressed.
pub fn menu_done(state: &mut State, client: ClientId, line: usize, key: &str) -> Routed {
    let Some(tree) = state.trees.open.get(&client) else {
        return Routed::Consumed;
    };
    if line >= view(state, tree).lines.len() {
        return Routed::Consumed;
    }
    point_at(state, client, line);
    let Ok(key) = crate::server::input::parse_key(key) else {
        return Routed::Consumed;
    };
    let routed = key_press(state, client, &key);
    clients::mark_dirty(state, client);
    routed
}
