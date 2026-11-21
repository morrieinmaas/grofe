use super::*;

/// The default key table (spec §5.3, the "tmux default bindings" row, D10):
/// tmux 3.7b's stock `prefix` bindings for the commands grofe has, with
/// repeat where tmux has `-r` and tmux's notes (`list-keys -N`). New panes
/// and windows open in the active pane's cwd (spec §5.3), which tmux leaves
/// to a `-c '#{pane_current_path}'` in the user's config, and prompts and
/// confirmations spell tmux's `#P`/`#W` as native formats. Left unbound:
/// `o` (needs `+`/`-` target offsets, FE.1's), `m`/`M` (marking), `C` and
/// `t` (out of scope, spec §4.5), `<`/`>` (tmux's window and pane menus),
/// `*` (`new-pane`). Copy mode binds `[`, `PPage` and the `copy-mode-vi` table.
pub fn default_bindings() -> KeyTables {
    let mut t = KeyTables::default();
    let mut bind = |key: &str, argv: &[&str], repeat: bool, note: &str| {
        let argv = argv.iter().map(|s| s.to_string()).collect();
        if let Ok(key) = parse_key(key) {
            let mut b = Binding::new(vec![argv], repeat, BindingSource::Default);
            b.desc = Some(note.to_string());
            t.insert(PREFIX_TABLE, key, b);
        }
    };
    for &(key, argv, repeat, note) in PREFIX_ROWS {
        bind(key, argv, repeat, note);
    }
    for n in 0..=9 {
        let target = format!(":{n}");
        bind(
            &n.to_string(),
            &["select-window", "-t", &target],
            false,
            &format!("Select window {n}"),
        );
    }
    for (table, rows) in [
        (COPY_MODE_VI_TABLE, COPY_MODE_VI),
        (COPY_MODE_TABLE, COPY_MODE_EMACS),
    ] {
        for (key, text) in rows {
            put(&mut t, table, key, text);
        }
    }
    // FE.1: stock `o`. Its `:.+` offset has no native spelling, so the
    // binding is tmux dialect, run through the front end (recorded on tmux
    // 3.7b: `select-pane -t :.+`, "Select the next pane").
    if let Ok(key) = parse_key("o") {
        let argv = ["select-pane", "-t", ":.+"].map(String::from).to_vec();
        let mut b = Binding::new(vec![argv], false, BindingSource::Default);
        b.desc = Some("Select the next pane".to_string());
        b.dialect = Dialect::Tmux;
        t.insert(PREFIX_TABLE, key, b);
    }
    // tmux 3.7b's stock mouse rows, in its dialect ([`NATIVE_STOCK_ROOT`]):
    // ordinary bindings, which the mouse module runs as they are (a border
    // drag, `resize-pane -M`, is the mouse's own).
    for (name, text) in TMUX_STOCK_ROOT {
        if NATIVE_STOCK_ROOT.contains(name) {
            t.insert_stock_root(name, text);
        }
    }
    // FR.4: grofe identity's pane menu, with Float and Unfloat; a server
    // with tmux identity binds tmux's stock menu instead.
    put(&mut t, ROOT_TABLE, "MouseDown3Pane", GROFE_PANE_MENU);
    // FR.3: Ctrl and drag anywhere in a pane moves it, as dragging a
    // framed tile's title does; grofe's own, so a tmux-identity server
    // drops it and the key is unbound, as in tmux.
    put(&mut t, ROOT_TABLE, "C-MouseDrag1Pane", "drag-pane -M");
    // The float control of a framed title's tab (`render::frame_tab`);
    // grofe's own, as the tab is.
    put(
        &mut t,
        ROOT_TABLE,
        "MouseDown1Control7",
        GROFE_FLOAT_CONTROL,
    );
    // A click on the status line's agents badge opens the tray; any other
    // click switches client, as tmux's stock row does.
    put(&mut t, ROOT_TABLE, "MouseDown1Status", GROFE_STATUS_CLICK);
    for (table, bindings) in t.tables.iter_mut() {
        for b in bindings.values_mut() {
            b.group = Some(default_group(table, &b.cmds).to_string());
        }
    }
    t
}

/// Binds `text`, a command string, to `key` in `table` as a default binding.
fn put(t: &mut KeyTables, table: &str, key: &str, text: &str) {
    if let (Ok(key), Ok(cmds)) = (parse_key(key), crate::cmd::split_command_string(text)) {
        t.insert(
            table,
            key,
            Binding::new(cmds, false, BindingSource::Default),
        );
    }
}

/// grofe's default `prefix` rows: the key, its argv, whether it repeats, its note.
#[rustfmt::skip]
const PREFIX_ROWS: &[(&str, &[&str], bool, &str)] = &[
    ("%", &["split-window", "-h", "-c", "#{pane_current_path}"], false, "Split window horizontally"),
    ("\"", &["split-window", "-c", "#{pane_current_path}"], false, "Split window vertically"),
    ("c", &["new-window", "-c", "#{pane_current_path}"], false, "Create a new window"),
    ("d", &["detach-client"], false, "Detach the current client"),
    ("n", &["next-window"], false, "Select the next window"),
    ("p", &["previous-window"], false, "Select the previous window"),
    ("l", &["last-window"], false, "Select the previously current window"),
    ("(", &["switch-client", "-p"], false, "Switch to previous client"),
    (")", &["switch-client", "-n"], false, "Switch to next client"),
    ("L", &["switch-client", "-l"], false, "Switch to the last client"),
    ("z", &["resize-pane", "-Z"], false, "Zoom the active pane"),
    ("C-b", &["send-prefix"], false, "Send the prefix key"),
    (":", &["command-prompt"], false, "Prompt for a command"),
    (",", &["command-prompt", "-I", "#W", "rename-window \"%%\""], false, "Rename current window"),
    ("$", &["command-prompt", "-I", "#S", "rename-session \"%%\""], false, "Rename current session"),
    // D10: the stock keys whose verbs are the structure verbs.
    (";", &["last-pane"], false, "Move to the previously active pane"),
    ("C-o", &["rotate-window"], false, "Rotate through the panes"),
    ("M-o", &["rotate-window", "-D"], false, "Rotate through the panes in reverse"),
    ("q", &["display-panes"], false, "Display pane numbers"),
    (".", &["command-prompt", "-T", "target", "move-window -t \"%%\""], false, "Move the current window"),
    ("'", &["command-prompt", "-T", "window-target", "-p", "index", "select-window -t \":%%\""], false, "Prompt for window index to select"),
    ("C-z", &["suspend-client"], false, "Suspend the current client"),
    ("D", &["choose-client", "-Z"], false, "Choose and detach a client from a list"),
    ("~", &["show-messages"], false, "Show messages"),
    ("E", &["select-layout", "-E"], false, "Spread panes out evenly"),
    ("M-n", &["next-window", "-a"], false, "Select the next window with an alert"),
    ("M-p", &["previous-window", "-a"], false, "Select the previous window with an alert"),
    ("/", &["command-prompt", "-k", "-p", "key", "list-keys -1N \"%%\""], false, "Describe key binding"),
    ("x", &["confirm-before", "-p", "kill-pane #{pane_index}? (y/n)", "kill-pane"], false, "Kill the active pane"),
    ("&", &["confirm-before", "-p", "kill-window #{window_name}? (y/n)", "kill-window"], false, "Kill current window"),
    ("!", &["break-pane"], false, "Break pane to a new window"),
    // grofe's own: tmux 3.7b has no key for `kill-server`.
    ("Q", &["confirm-before", "-p", "kill-server: stop grofe and every session? (y/n)", "kill-server"], false, "Stop the server and every session"),
    // grofe's own: tmux 3.7b binds no `u`.
    ("u", &["agent", "next"], false, "Jump to the agent that needs you"),
    // grofe's own: tmux 3.7b binds no `G`.
    ("G", &["copy-mode", "-h"], false, "Copy a match on screen by its label"),
    ("a", &["new-scratchpad"], false, "Open a scratchpad"),
    ("S", &["toggle-scratchpad"], false, "Hide or show a scratchpad"),
    ("A", &["choose-scratchpad"], false, "Choose a scratchpad from a list"),
    ("{", &["swap-pane", "-U"], false, "Swap the active pane with the pane above"),
    ("}", &["swap-pane", "-D"], false, "Swap the active pane with the pane below"),
    ("i", &["display-message"], false, "Display window information"),
    ("w", &["choose-tree", "-Zw"], false, "Choose a window from a list"),
    ("Space", &["key-menu", "-T", "prefix"], false, "Show the key menu"),
    // grofe's own: tmux identity keeps `C` for customize-mode.
    ("C", &["settings"], false, "Open the settings menu"),
    ("M-Space", &["next-layout"], false, "Select next layout"),
    ("U", &["choose-agent"], false, "Pick an agent"),
    ("b", &["agent-sidebar", "toggle"], false, "Toggle the agent sidebar"),
    ("O", &["agent-grid"], false, "Show all agents"),
    ("y", &["choose-notification"], false, "Show notifications"),
    ("?", &["list-keys", "-N"], false, "List key bindings"),
    ("r", &["refresh-client"], false, "Redraw the current client"),
    ("s", &["choose-tree", "-Zs"], false, "Choose a session from a list"),
    // TM.4: the buffer keys.
    ("]", &["paste-buffer", "-p"], false, "Paste the most recent paste buffer"),
    ("#", &["list-buffers"], false, "List all paste buffers"),
    ("=", &["choose-buffer", "-Z"], false, "Choose a paste buffer from a list"),
    ("-", &["delete-buffer"], false, "Delete the most recent paste buffer"),
    ("DC", &["refresh-client", "-c"], true, "Reset so the visible part of the window follows the cursor"),
    // FR.4: grofe's own, so a tmux-identity server drops them with the rest
    // of grofe's prefix defaults ([`KeyTables::use_tmux_stock`]) and gets
    // tmux's `f` (find-window) back. `f` is the main key; `F` stays for
    // configs that bind `f` themselves.
    ("f", &["if-shell", "-F", "#{pane_floating_flag}", "unfloat-pane", "float-pane"], false, "Float or unfloat the active pane"),
    ("F", &["if-shell", "-F", "#{pane_floating_flag}", "unfloat-pane", "float-pane"], false, "Float or unfloat the active pane"),
    ("Up", &["select-pane", "-U"], true, "Select the pane above the active pane"),
    ("M-Up", &["resize-pane", "-U", "5"], true, "Resize the pane up by 5"),
    ("C-Up", &["resize-pane", "-U"], true, "Resize the pane up"),
    ("Down", &["select-pane", "-D"], true, "Select the pane below the active pane"),
    ("M-Down", &["resize-pane", "-D", "5"], true, "Resize the pane down by 5"),
    ("C-Down", &["resize-pane", "-D"], true, "Resize the pane down"),
    ("Left", &["select-pane", "-L"], true, "Select the pane to the left of the active pane"),
    ("M-Left", &["resize-pane", "-L", "5"], true, "Resize the pane left by 5"),
    ("C-Left", &["resize-pane", "-L"], true, "Resize the pane left"),
    ("Right", &["select-pane", "-R"], true, "Select the pane to the right of the active pane"),
    ("M-Right", &["resize-pane", "-R", "5"], true, "Resize the pane right by 5"),
    ("C-Right", &["resize-pane", "-R"], true, "Resize the pane right"),
    ("S-Up", &["refresh-client", "-U", "10"], true, "Move the visible part of the window up"),
    ("S-Down", &["refresh-client", "-D", "10"], true, "Move the visible part of the window down"),
    ("S-Left", &["refresh-client", "-L", "10"], true, "Move the visible part of the window left"),
    ("S-Right", &["refresh-client", "-R", "10"], true, "Move the visible part of the window right"),
    ("M-1", &["select-layout", "even-horizontal"], false, "Set the even-horizontal layout"),
    ("M-2", &["select-layout", "even-vertical"], false, "Set the even-vertical layout"),
    ("M-3", &["select-layout", "main-horizontal"], false, "Set the main-horizontal layout"),
    ("M-4", &["select-layout", "main-vertical"], false, "Set the main-vertical layout"),
    ("M-5", &["select-layout", "tiled"], false, "Select the tiled layout"),
    ("M-6", &["select-layout", "main-horizontal-mirrored"], false, "Set the main-horizontal-mirrored layout"),
    ("M-7", &["select-layout", "main-vertical-mirrored"], false, "Set the main-vertical-mirrored layout"),
    // Copy mode's keys.
    ("[", &["copy-mode"], false, "Enter copy mode"),
    ("PPage", &["copy-mode", "-u"], false, "Enter copy mode and scroll up"),
];

/// The key menu section of a grofe default binding, from its table and
/// first command.
pub(super) fn default_group(table: &str, cmds: &[Vec<String>]) -> &'static str {
    if table == COPY_MODE_VI_TABLE || table == COPY_MODE_TABLE {
        return "copy";
    }
    if table == ROOT_TABLE {
        return "pane";
    }
    let argv = cmds.first().map(Vec::as_slice).unwrap_or_default();
    let word = |i: usize| argv.get(i).map(String::as_str).unwrap_or("");
    let last = argv.last().map(String::as_str).unwrap_or("");
    match word(0) {
        "split-window" | "select-pane" | "resize-pane" | "last-pane" | "rotate-window"
        | "display-panes" | "break-pane" | "swap-pane" | "if-shell" => "pane",
        "new-window" | "next-window" | "previous-window" | "last-window" | "select-window"
        | "kill-window" => "window",
        "switch-client" => "session",
        "choose-tree" if word(1).ends_with('s') => "session",
        "choose-tree" => "window",
        "confirm-before" if last == "kill-pane" => "pane",
        "confirm-before" if last == "kill-window" => "window",
        "command-prompt" if last.starts_with("rename-session") => "session",
        "command-prompt"
            if last.starts_with("rename-window")
                || last.starts_with("move-window")
                || last.starts_with("select-window") =>
        {
            "window"
        }
        "copy-mode" | "paste-buffer" | "list-buffers" | "choose-buffer" | "delete-buffer" => "copy",
        "next-layout" | "select-layout" => "layout",
        "agent" | "choose-agent" | "agent-sidebar" | "agent-grid" => "agents",
        "show-messages" | "choose-notification" => "notifications",
        "detach-client" | "choose-client" | "suspend-client" | "refresh-client" => "client",
        _ => "other",
    }
}

/// grofe identity's `MouseDown1Control7`, the tab's float control: the
/// pane clicked floats, or unfloats if it is floating.
pub const GROFE_FLOAT_CONTROL: &str =
    r##"if-shell -F -t = "#{pane_floating_flag}" { unfloat-pane -t = } { float-pane -t = }"##;

/// grofe identity's `MouseDown1Status`: the agents badge opens the
/// notification tray, any other range is tmux's `switch-client -t =`.
pub const GROFE_STATUS_CLICK: &str = r##"if-shell -F "#{==:#{mouse_status_range},agents}" { choose-notification } { switch-client -t = }"##;

/// grofe identity's `MouseDown3Pane` (FR.4): a pane that asked for the
/// mouse is focused and gets the click, as tmux's stock row sends it on;
/// any other opens
/// the pane's menu at the pointer, its items acting on the pane clicked.
/// Float shows on a tiled pane and Unfloat on a floating one.
pub const GROFE_PANE_MENU: &str = concat!(
    r##"if-shell -F "#{mouse_any_flag}" { select-pane -t = ; send-keys -M } { "##,
    r##"display-menu -T "#[align=centre]#{pane_index} (#{pane_id})" "##,
    r##""#{?pane_floating_flag,,Float}" f { float-pane -t = } "##,
    r##""#{?pane_floating_flag,Unfloat,}" f { unfloat-pane -t = } "##,
    r##"'' "##,
    r##""#{?pane_floating_flag,,Horizontal Split}" h { split-window -h -t = -c "#{pane_current_path}" } "##,
    r##""#{?pane_floating_flag,,Vertical Split}" v { split-window -v -t = -c "#{pane_current_path}" } "##,
    r##"'' "##,
    r##"Kill X { kill-pane -t = } "##,
    r##""#{?#{>:#{window_panes},1},,-}#{?window_zoomed_flag,Unzoom,Zoom}" z { resize-pane -Z -t = } "##,
    r##"}"##,
);

/// The copy-mode key table with `mode-keys vi`.
pub const COPY_MODE_VI_TABLE: &str = "copy-mode-vi";

/// tmux 3.7b's 88 stock `copy-mode-vi` bindings (`key-bindings.c`), in
/// native spelling: the key, then its commands as a command string. The
/// rows yank and the navigator add come from those plugins (TM.0's
/// decision 5).
#[rustfmt::skip]
pub const COPY_MODE_VI: &[(&str, &str)] = &[
    ("#", "send-keys -F -X search-backward -- '#{copy_cursor_word}'"),
    ("*", "send-keys -F -X search-forward -- '#{copy_cursor_word}'"),
    ("C-c", "send-keys -X cancel"),
    ("C-d", "send-keys -X halfpage-down"),
    ("C-e", "send-keys -X scroll-down"),
    ("C-b", "send-keys -X page-up"),
    ("C-f", "send-keys -X page-down"),
    ("C-h", "send-keys -X cursor-left"),
    ("C-j", "send-keys -X copy-pipe-and-cancel"),
    ("Enter", "send-keys -X copy-pipe-and-cancel"),
    ("C-u", "send-keys -X halfpage-up"),
    ("C-v", "send-keys -X rectangle-toggle"),
    ("C-y", "send-keys -X scroll-up"),
    ("Escape", "send-keys -X clear-selection"),
    ("C-[", "send-keys -X clear-selection"),
    ("Space", "send-keys -X begin-selection"),
    ("$", "send-keys -X end-of-line"),
    (",", "send-keys -X jump-reverse"),
    ("/", "command-prompt -T search -p '(search down)' { send-keys -X search-forward -- '%%' }"),
    ("0", "send-keys -X start-of-line"),
    ("1", "command-prompt -N -p '(repeat)' -I 1 { send-keys -N '%%' }"),
    ("2", "command-prompt -N -p '(repeat)' -I 2 { send-keys -N '%%' }"),
    ("3", "command-prompt -N -p '(repeat)' -I 3 { send-keys -N '%%' }"),
    ("4", "command-prompt -N -p '(repeat)' -I 4 { send-keys -N '%%' }"),
    ("5", "command-prompt -N -p '(repeat)' -I 5 { send-keys -N '%%' }"),
    ("6", "command-prompt -N -p '(repeat)' -I 6 { send-keys -N '%%' }"),
    ("7", "command-prompt -N -p '(repeat)' -I 7 { send-keys -N '%%' }"),
    ("8", "command-prompt -N -p '(repeat)' -I 8 { send-keys -N '%%' }"),
    ("9", "command-prompt -N -p '(repeat)' -I 9 { send-keys -N '%%' }"),
    (":", "command-prompt -p '(goto line)' { send-keys -X goto-line -- '%%' }"),
    (";", "send-keys -X jump-again"),
    ("?", "command-prompt -T search -p '(search up)' { send-keys -X search-backward -- '%%' }"),
    ("A", "send-keys -X append-selection-and-cancel"),
    ("B", "send-keys -X previous-space"),
    ("D", "send-keys -X copy-pipe-end-of-line-and-cancel"),
    ("E", "send-keys -X next-space-end"),
    ("F", "command-prompt -1 -p '(jump backward)' { send-keys -X jump-backward -- '%%' }"),
    ("G", "send-keys -X history-bottom"),
    ("H", "send-keys -X top-line"),
    ("J", "send-keys -X scroll-down"),
    ("K", "send-keys -X scroll-up"),
    ("L", "send-keys -X bottom-line"),
    ("M", "send-keys -X middle-line"),
    ("N", "send-keys -X search-reverse"),
    ("P", "send-keys -X toggle-position"),
    ("T", "command-prompt -1 -p '(jump to backward)' { send-keys -X jump-to-backward -- '%%' }"),
    ("V", "send-keys -X select-line"),
    ("W", "send-keys -X next-space"),
    ("X", "send-keys -X set-mark"),
    ("^", "send-keys -X back-to-indentation"),
    ("b", "send-keys -X previous-word"),
    ("e", "send-keys -X next-word-end"),
    ("f", "command-prompt -1 -p '(jump forward)' { send-keys -X jump-forward -- '%%' }"),
    ("g", "send-keys -X history-top"),
    ("h", "send-keys -X cursor-left"),
    ("j", "send-keys -X cursor-down"),
    ("k", "send-keys -X cursor-up"),
    ("z", "send-keys -X scroll-middle"),
    ("l", "send-keys -X cursor-right"),
    ("n", "send-keys -X search-again"),
    ("o", "send-keys -X other-end"),
    ("q", "send-keys -X cancel"),
    ("r", "send-keys -X refresh-from-pane"),
    ("t", "command-prompt -1 -p '(jump to forward)' { send-keys -X jump-to-forward -- '%%' }"),
    ("v", "send-keys -X rectangle-toggle"),
    ("w", "send-keys -X next-word"),
    ("{", "send-keys -X previous-paragraph"),
    ("}", "send-keys -X next-paragraph"),
    ("%", "send-keys -X next-matching-bracket"),
    ("Home", "send-keys -X start-of-line"),
    ("End", "send-keys -X end-of-line"),
    ("MouseDown1Pane", "select-pane"),
    ("MouseDrag1Pane", "select-pane; send-keys -X begin-selection"),
    ("MouseDragEnd1Pane", "send-keys -X copy-pipe-and-cancel"),
    ("WheelUpPane", "select-pane; send-keys -N 5 -X scroll-up"),
    ("WheelDownPane", "select-pane; send-keys -N 5 -X scroll-down"),
    ("DoubleClick1Pane", "select-pane; send-keys -X select-word; run-shell -d 0.3; send-keys -X copy-pipe-and-cancel"),
    ("TripleClick1Pane", "select-pane; send-keys -X select-line; run-shell -d 0.3; send-keys -X copy-pipe-and-cancel"),
    ("BSpace", "send-keys -X cursor-left"),
    ("NPage", "send-keys -X page-down"),
    ("PPage", "send-keys -X page-up"),
    ("Up", "send-keys -X cursor-up"),
    ("Down", "send-keys -X cursor-down"),
    ("Left", "send-keys -X cursor-left"),
    ("Right", "send-keys -X cursor-right"),
    ("M-x", "send-keys -X jump-to-mark"),
    ("C-Up", "send-keys -X scroll-up"),
    ("C-Down", "send-keys -X scroll-down"),
];

/// The copy-mode key table with `mode-keys emacs`.
pub const COPY_MODE_TABLE: &str = "copy-mode";

/// tmux 3.7b's 75 stock `copy-mode` (emacs) bindings (`key-bindings.c`), in
/// native spelling like [`COPY_MODE_VI`].
#[rustfmt::skip]
pub const COPY_MODE_EMACS: &[(&str, &str)] = &[
    ("Escape", "send-keys -X cancel"),
    ("Space", "send-keys -X page-down"),
    (",", "send-keys -X jump-reverse"),
    (";", "send-keys -X jump-again"),
    ("F", "command-prompt -1 -p '(jump backward)' { send-keys -X jump-backward -- '%%' }"),
    ("N", "send-keys -X search-reverse"),
    ("P", "send-keys -X toggle-position"),
    ("R", "send-keys -X rectangle-toggle"),
    ("T", "command-prompt -1 -p '(jump to backward)' { send-keys -X jump-to-backward -- '%%' }"),
    ("X", "send-keys -X set-mark"),
    ("f", "command-prompt -1 -p '(jump forward)' { send-keys -X jump-forward -- '%%' }"),
    ("g", "command-prompt -p '(goto line)' { send-keys -X goto-line -- '%%' }"),
    ("n", "send-keys -X search-again"),
    ("q", "send-keys -X cancel"),
    ("r", "send-keys -X refresh-from-pane"),
    ("t", "command-prompt -1 -p '(jump to forward)' { send-keys -X jump-to-forward -- '%%' }"),
    ("Home", "send-keys -X start-of-line"),
    ("End", "send-keys -X end-of-line"),
    ("NPage", "send-keys -X page-down"),
    ("PPage", "send-keys -X page-up"),
    ("Up", "send-keys -X cursor-up"),
    ("Down", "send-keys -X cursor-down"),
    ("Left", "send-keys -X cursor-left"),
    ("Right", "send-keys -X cursor-right"),
    ("MouseDown1Pane", "select-pane"),
    ("MouseDrag1Pane", "select-pane; send-keys -X begin-selection"),
    ("MouseDragEnd1Pane", "send-keys -X copy-pipe-and-cancel"),
    ("WheelDownPane", "select-pane; send-keys -N 5 -X scroll-down"),
    ("WheelUpPane", "select-pane; send-keys -N 5 -X scroll-up"),
    ("DoubleClick1Pane", "select-pane; send-keys -X select-word; run-shell -d 0.3; send-keys -X copy-pipe-and-cancel"),
    ("TripleClick1Pane", "select-pane; send-keys -X select-line; run-shell -d 0.3; send-keys -X copy-pipe-and-cancel"),
    ("M-1", "command-prompt -N -p '(repeat)' -I 1 { send-keys -N '%%' }"),
    ("M-2", "command-prompt -N -p '(repeat)' -I 2 { send-keys -N '%%' }"),
    ("M-3", "command-prompt -N -p '(repeat)' -I 3 { send-keys -N '%%' }"),
    ("M-4", "command-prompt -N -p '(repeat)' -I 4 { send-keys -N '%%' }"),
    ("M-5", "command-prompt -N -p '(repeat)' -I 5 { send-keys -N '%%' }"),
    ("M-6", "command-prompt -N -p '(repeat)' -I 6 { send-keys -N '%%' }"),
    ("M-7", "command-prompt -N -p '(repeat)' -I 7 { send-keys -N '%%' }"),
    ("M-8", "command-prompt -N -p '(repeat)' -I 8 { send-keys -N '%%' }"),
    ("M-9", "command-prompt -N -p '(repeat)' -I 9 { send-keys -N '%%' }"),
    ("M-<", "send-keys -X history-top"),
    ("M->", "send-keys -X history-bottom"),
    ("M-R", "send-keys -X top-line"),
    ("M-b", "send-keys -X previous-word"),
    ("M-f", "send-keys -X next-word-end"),
    ("M-l", "send-keys -X cursor-centre-horizontal"),
    ("M-m", "send-keys -X back-to-indentation"),
    ("M-r", "send-keys -X middle-line"),
    ("M-v", "send-keys -X page-up"),
    ("M-w", "send-keys -X copy-pipe-and-cancel"),
    ("M-x", "send-keys -X jump-to-mark"),
    ("M-{", "send-keys -X previous-paragraph"),
    ("M-}", "send-keys -X next-paragraph"),
    ("M-Up", "send-keys -X halfpage-up"),
    ("M-Down", "send-keys -X halfpage-down"),
    ("C-Space", "send-keys -X begin-selection"),
    ("C-[", "send-keys -X cancel"),
    ("C-a", "send-keys -X start-of-line"),
    ("C-b", "send-keys -X cursor-left"),
    ("C-c", "send-keys -X cancel"),
    ("C-e", "send-keys -X end-of-line"),
    ("C-f", "send-keys -X cursor-right"),
    ("C-g", "send-keys -X clear-selection"),
    ("C-k", "send-keys -X copy-pipe-end-of-line-and-cancel"),
    ("C-l", "send-keys -X recentre-top-bottom"),
    ("C-n", "send-keys -X cursor-down"),
    ("C-p", "send-keys -X cursor-up"),
    ("C-r", "command-prompt -T search -i -I '#{pane_search_string}' -p '(search up)' { send-keys -X search-backward-incremental -- '%%' }"),
    ("C-s", "command-prompt -T search -i -I '#{pane_search_string}' -p '(search down)' { send-keys -X search-forward-incremental -- '%%' }"),
    ("C-v", "send-keys -X page-down"),
    ("C-w", "send-keys -X copy-pipe-and-cancel"),
    ("C-Up", "send-keys -X scroll-up"),
    ("C-Down", "send-keys -X scroll-down"),
    ("C-M-b", "send-keys -X previous-matching-bracket"),
    ("C-M-f", "send-keys -X next-matching-bracket"),
];

/// How tmux 3.7b's `list-keys -T copy-mode` prints the stock emacs rows
/// whose template is a `{ }` list (the `b2b/emacs-keys` golden), for a
/// server with tmux identity ([`KeyTables::use_tmux_stock`]).
#[rustfmt::skip]
pub const TMUX_COPY_MODE_EMACS_SHOWN: &[(&str, &str)] = &[
    ("F", r#"command-prompt -1 -p "(jump backward)" { send-keys -X jump-backward -- "%%" }"#),
    ("T", r#"command-prompt -1 -p "(jump to backward)" { send-keys -X jump-to-backward -- "%%" }"#),
    ("f", r#"command-prompt -1 -p "(jump forward)" { send-keys -X jump-forward -- "%%" }"#),
    ("g", r#"command-prompt -p "(goto line)" { send-keys -X goto-line -- "%%" }"#),
    ("t", r#"command-prompt -1 -p "(jump to forward)" { send-keys -X jump-to-forward -- "%%" }"#),
    ("M-1", r#"command-prompt -N -I 1 -p (repeat) { send-keys -N "%%" }"#),
    ("M-2", r#"command-prompt -N -I 2 -p (repeat) { send-keys -N "%%" }"#),
    ("M-3", r#"command-prompt -N -I 3 -p (repeat) { send-keys -N "%%" }"#),
    ("M-4", r#"command-prompt -N -I 4 -p (repeat) { send-keys -N "%%" }"#),
    ("M-5", r#"command-prompt -N -I 5 -p (repeat) { send-keys -N "%%" }"#),
    ("M-6", r#"command-prompt -N -I 6 -p (repeat) { send-keys -N "%%" }"#),
    ("M-7", r#"command-prompt -N -I 7 -p (repeat) { send-keys -N "%%" }"#),
    ("M-8", r#"command-prompt -N -I 8 -p (repeat) { send-keys -N "%%" }"#),
    ("M-9", r#"command-prompt -N -I 9 -p (repeat) { send-keys -N "%%" }"#),
    ("C-r", r##"command-prompt -i -I "#{pane_search_string}" -T search -p "(search up)" { send-keys -X search-backward-incremental -- "%%" }"##),
    ("C-s", r##"command-prompt -i -I "#{pane_search_string}" -T search -p "(search down)" { send-keys -X search-forward-incremental -- "%%" }"##),
];

/// How tmux 3.7b's `list-keys -T copy-mode-vi` prints the stock rows whose
/// template is a `{ }` list (the `tm_0/list-keys-copy-mode-vi` golden),
/// for a server with tmux identity ([`KeyTables::use_tmux_stock`]).
#[rustfmt::skip]
pub const TMUX_COPY_MODE_VI_SHOWN: &[(&str, &str)] = &[
    ("/", r#"command-prompt -T search -p "(search down)" { send-keys -X search-forward -- "%%" }"#),
    ("?", r#"command-prompt -T search -p "(search up)" { send-keys -X search-backward -- "%%" }"#),
    (":", r#"command-prompt -p "(goto line)" { send-keys -X goto-line -- "%%" }"#),
    ("F", r#"command-prompt -1 -p "(jump backward)" { send-keys -X jump-backward -- "%%" }"#),
    ("T", r#"command-prompt -1 -p "(jump to backward)" { send-keys -X jump-to-backward -- "%%" }"#),
    ("f", r#"command-prompt -1 -p "(jump forward)" { send-keys -X jump-forward -- "%%" }"#),
    ("t", r#"command-prompt -1 -p "(jump to forward)" { send-keys -X jump-to-forward -- "%%" }"#),
    ("1", r#"command-prompt -N -I 1 -p (repeat) { send-keys -N "%%" }"#),
    ("2", r#"command-prompt -N -I 2 -p (repeat) { send-keys -N "%%" }"#),
    ("3", r#"command-prompt -N -I 3 -p (repeat) { send-keys -N "%%" }"#),
    ("4", r#"command-prompt -N -I 4 -p (repeat) { send-keys -N "%%" }"#),
    ("5", r#"command-prompt -N -I 5 -p (repeat) { send-keys -N "%%" }"#),
    ("6", r#"command-prompt -N -I 6 -p (repeat) { send-keys -N "%%" }"#),
    ("7", r#"command-prompt -N -I 7 -p (repeat) { send-keys -N "%%" }"#),
    ("8", r#"command-prompt -N -I 8 -p (repeat) { send-keys -N "%%" }"#),
    ("9", r#"command-prompt -N -I 9 -p (repeat) { send-keys -N "%%" }"#),
];

/// tmux 3.7b's stock `root` table, all of its mouse rows (`list-keys -T
/// root` on a `-f /dev/null` server, the `b3wpc/stock-root` golden): the key
/// and its commands as tmux prints them. A server with tmux identity binds
/// them all; the rows in [`NATIVE_STOCK_ROOT`] are bound on every server.
#[rustfmt::skip]
pub const TMUX_STOCK_ROOT: &[(&str, &str)] = &[
    ("MouseDown1Pane", r##"select-pane -t = \; send-keys -M"##),
    ("MouseDown1Status", r##"switch-client -t ="##),
    ("MouseDown1Border", r##"select-pane -M"##),
    ("MouseDown1ScrollbarUp", r##"if-shell -F -t = "#{pane_in_mode}" { send-keys -X page-up } { copy-mode -u }"##),
    ("MouseDown1ScrollbarDown", r##"if-shell -F -t = "#{pane_in_mode}" { send-keys -X page-down } { copy-mode -d }"##),
    ("MouseDown1Control8", r##"resize-pane -Z"##),
    ("MouseDown1Control9", r##"display-menu -O -T "Kill pane #{pane_index}?" -t = -x M -y M Yes y { kill-pane -t = } No n {  }"##),
    ("MouseDown2Pane", r##"select-pane -t = \; if-shell -F "#{||:#{pane_in_mode},#{mouse_any_flag}}" { send-keys -M } { paste-buffer -p }"##),
    ("MouseDown3Pane", r##"if-shell -F -t = "#{||:#{mouse_any_flag},#{&&:#{pane_in_mode},#{?#{m/r:(copy|view)-mode,#{pane_mode}},0,1}}}" { select-pane -t = ; send-keys -M } { display-menu -T "#[align=centre]#{pane_index} (#{pane_id})" -t = -x M -y M "#{?#{m/r:(copy|view)-mode,#{pane_mode}},Go To Top,}" < { send-keys -X history-top } "#{?#{m/r:(copy|view)-mode,#{pane_mode}},Go To Bottom,}" > { send-keys -X history-bottom } '' "#{?#{&&:#{buffer_size},#{!:#{pane_in_mode}}},Paste #[underscore]#{=/9/...:buffer_sample},}" p { paste-buffer } '' "#{?mouse_word,Search For #[underscore]#{=/9/...:mouse_word},}" C-r { if-shell -F "#{?#{m/r:(copy|view)-mode,#{pane_mode}},0,1}" "copy-mode -t=" ; send-keys -X -t = search-backward -- "#{q:mouse_word}" } "#{?mouse_word,Type #[underscore]#{=/9/...:mouse_word},}" C-y { copy-mode -q ; send-keys -l "#{q:mouse_word}" } "#{?mouse_word,Copy #[underscore]#{=/9/...:mouse_word},}" c { copy-mode -q ; set-buffer "#{q:mouse_word}" } "#{?mouse_line,Copy Line,}" l { copy-mode -q ; set-buffer "#{q:mouse_line}" } '' "#{?mouse_hyperlink,Type #[underscore]#{=/9/...:mouse_hyperlink},}" C-h { copy-mode -q ; send-keys -l "#{q:mouse_hyperlink}" } "#{?mouse_hyperlink,Copy #[underscore]#{=/9/...:mouse_hyperlink},}" h { copy-mode -q ; set-buffer "#{q:mouse_hyperlink}" } '' "#{?#{!:#{pane_floating_flag}},Horizontal Split,}" h { split-window -h } "#{?#{!:#{pane_floating_flag}},Vertical Split,}" v { split-window -v } '' "#{?#{&&:#{!:#{pane_floating_flag}},#{>:#{window_panes},1}},Swap Up,}" u { swap-pane -U } "#{?#{&&:#{!:#{pane_floating_flag}},#{>:#{window_panes},1}},Swap Down,}" d { swap-pane -D } "#{?pane_marked_set,,-}Swap Marked" s { swap-pane } '' Kill X { kill-pane } Respawn R { respawn-pane -k } "#{?pane_marked,Unmark,Mark}" m { select-pane -m } "#{?#{>:#{window_panes},1},,-}#{?window_zoomed_flag,Unzoom,Zoom}" z { resize-pane -Z } }"##),
    ("MouseDown3Status", r##"display-menu -T "#[align=centre]#{window_index}:#{window_name}" -t = -x W -y W "#{?#{>:#{session_windows},1},,-}Swap Left" l { swap-window -t :-1 } "#{?#{>:#{session_windows},1},,-}Swap Right" r { swap-window -t :+1 } "#{?pane_marked_set,,-}Swap Marked" s { swap-window } '' Kill X { kill-window } Respawn R { respawn-window -k } "#{?pane_marked,Unmark,Mark}" m { select-pane -m } Rename n { command-prompt -F -I "#W" { rename-window -t "#{window_id}" "%%" } } '' "New After" w { new-window -a } "New At End" W { new-window }"##),
    ("MouseDown3StatusLeft", r##"display-menu -T "#[align=centre]#{session_name}" -t = -x M -y W Next n { switch-client -n } Previous p { switch-client -p } '' Renumber N { move-window -r } Rename r { command-prompt -I "#S" { rename-session "%%" } } Detach d { detach-client } '' "New Session" s { new-session } "New Window" w { new-window }"##),
    ("MouseDrag1Pane", r##"if-shell -F "#{||:#{pane_in_mode},#{mouse_any_flag}}" { send-keys -M } { copy-mode -M }"##),
    ("MouseDrag1Border", r##"resize-pane -M"##),
    ("MouseDrag1ScrollbarSlider", r##"if-shell -F -t = "#{pane_in_mode}" { send-keys -X scroll-to-mouse } { copy-mode -S }"##),
    ("WheelDownStatus", r##"next-window"##),
    ("WheelUpPane", r##"if-shell -F "#{||:#{alternate_on},#{pane_in_mode},#{mouse_any_flag}}" { send-keys -M } { copy-mode -e }"##),
    ("WheelUpStatus", r##"previous-window"##),
    ("DoubleClick1Pane", r##"select-pane -t = \; if-shell -F "#{||:#{pane_in_mode},#{mouse_any_flag}}" { send-keys -M } { copy-mode -H ; send-keys -X select-word ; run-shell -d 0.3 ; send-keys -X copy-pipe-and-cancel }"##),
    ("TripleClick1Pane", r##"select-pane -t = \; if-shell -F "#{||:#{pane_in_mode},#{mouse_any_flag}}" { send-keys -M } { copy-mode -H ; send-keys -X select-line ; run-shell -d 0.3 ; send-keys -X copy-pipe-and-cancel }"##),
    ("M-MouseDown3Pane", r##"display-menu -T "#[align=centre]#{pane_index} (#{pane_id})" -t = -x M -y M "#{?#{m/r:(copy|view)-mode,#{pane_mode}},Go To Top,}" < { send-keys -X history-top } "#{?#{m/r:(copy|view)-mode,#{pane_mode}},Go To Bottom,}" > { send-keys -X history-bottom } '' "#{?#{&&:#{buffer_size},#{!:#{pane_in_mode}}},Paste #[underscore]#{=/9/...:buffer_sample},}" p { paste-buffer } '' "#{?mouse_word,Search For #[underscore]#{=/9/...:mouse_word},}" C-r { if-shell -F "#{?#{m/r:(copy|view)-mode,#{pane_mode}},0,1}" "copy-mode -t=" ; send-keys -X -t = search-backward -- "#{q:mouse_word}" } "#{?mouse_word,Type #[underscore]#{=/9/...:mouse_word},}" C-y { copy-mode -q ; send-keys -l "#{q:mouse_word}" } "#{?mouse_word,Copy #[underscore]#{=/9/...:mouse_word},}" c { copy-mode -q ; set-buffer "#{q:mouse_word}" } "#{?mouse_line,Copy Line,}" l { copy-mode -q ; set-buffer "#{q:mouse_line}" } '' "#{?mouse_hyperlink,Type #[underscore]#{=/9/...:mouse_hyperlink},}" C-h { copy-mode -q ; send-keys -l "#{q:mouse_hyperlink}" } "#{?mouse_hyperlink,Copy #[underscore]#{=/9/...:mouse_hyperlink},}" h { copy-mode -q ; set-buffer "#{q:mouse_hyperlink}" } '' "#{?#{!:#{pane_floating_flag}},Horizontal Split,}" h { split-window -h } "#{?#{!:#{pane_floating_flag}},Vertical Split,}" v { split-window -v } '' "#{?#{&&:#{!:#{pane_floating_flag}},#{>:#{window_panes},1}},Swap Up,}" u { swap-pane -U } "#{?#{&&:#{!:#{pane_floating_flag}},#{>:#{window_panes},1}},Swap Down,}" d { swap-pane -D } "#{?pane_marked_set,,-}Swap Marked" s { swap-pane } '' Kill X { kill-pane } Respawn R { respawn-pane -k } "#{?pane_marked,Unmark,Mark}" m { select-pane -m } "#{?#{>:#{window_panes},1},,-}#{?window_zoomed_flag,Unzoom,Zoom}" z { resize-pane -Z }"##),
    ("M-MouseDown3Status", r##"display-menu -T "#[align=centre]#{window_index}:#{window_name}" -t = -x W -y W "#{?#{>:#{session_windows},1},,-}Swap Left" l { swap-window -t :-1 } "#{?#{>:#{session_windows},1},,-}Swap Right" r { swap-window -t :+1 } "#{?pane_marked_set,,-}Swap Marked" s { swap-window } '' Kill X { kill-window } Respawn R { respawn-window -k } "#{?pane_marked,Unmark,Mark}" m { select-pane -m } Rename n { command-prompt -F -I "#W" { rename-window -t "#{window_id}" "%%" } } '' "New After" w { new-window -a } "New At End" W { new-window }"##),
    ("M-MouseDown3StatusLeft", r##"display-menu -T "#[align=centre]#{session_name}" -t = -x M -y W Next n { switch-client -n } Previous p { switch-client -p } '' Renumber N { move-window -r } Rename r { command-prompt -I "#S" { rename-session "%%" } } Detach d { detach-client } '' "New Session" s { new-session } "New Window" w { new-window }"##),
    ("C-MouseDown1Pane", r##"swap-pane -s @"##),
    ("C-MouseDown1Status", r##"swap-window -t @"##),
];

/// The stock `root` rows every server binds, whatever its identity: the
/// pane, status line, border and wheel rows. The right-button menus, the
/// Meta and Control variants and the scrollbar rows are tmux's alone.
pub const NATIVE_STOCK_ROOT: &[&str] = &[
    "MouseDown1Pane",
    "MouseDown1Status",
    "MouseDown1Border",
    "MouseDown1Control8",
    "MouseDown1Control9",
    "MouseDown2Pane",
    "MouseDrag1Pane",
    "MouseDrag1Border",
    "WheelDownStatus",
    "WheelUpPane",
    "WheelUpStatus",
    "DoubleClick1Pane",
    "TripleClick1Pane",
];

/// tmux 3.7b's stock prefix table (`list-keys -T prefix` on a `-f
/// /dev/null` server, the `fe_4/list-keys` golden; notes from its
/// `key-bindings.c`): the key, whether it repeats, its note, and its
/// commands as tmux prints them.
#[rustfmt::skip]
pub const TMUX_STOCK: &[(&str, bool, &str, &str)] = &[
    ("Space", false, "Select next layout", "next-layout"),
    ("!", false, "Break pane to a new window", "break-pane"),
    (r#"""#, false, "Split window vertically", "split-window"),
    ("#", false, "List all paste buffers", "list-buffers"),
    ("$", false, "Rename current session", r##"command-prompt -I "#S" { rename-session "%%" }"##),
    ("%", false, "Split window horizontally", "split-window -h"),
    ("&", false, "Kill current window", r#"confirm-before -p "kill-window #W? (y/n)" kill-window"#),
    ("'", false, "Prompt for window index to select", r#"command-prompt -T window-target -p index { select-window -t ":%%" }"#),
    ("(", false, "Switch to previous client", "switch-client -p"),
    (")", false, "Switch to next client", "switch-client -n"),
    ("*", false, "New floating pane", "new-pane"),
    (",", false, "Rename current window", r##"command-prompt -I "#W" { rename-window "%%" }"##),
    ("-", false, "Delete the most recent paste buffer", "delete-buffer"),
    (".", false, "Move the current window", r#"command-prompt -T target { move-window -t "%%" }"#),
    ("/", false, "Describe key binding", r#"command-prompt -k -p key { list-keys -1N "%%" }"#),
    ("0", false, "Select window 0", "select-window -t :=0"),
    ("1", false, "Select window 1", "select-window -t :=1"),
    ("2", false, "Select window 2", "select-window -t :=2"),
    ("3", false, "Select window 3", "select-window -t :=3"),
    ("4", false, "Select window 4", "select-window -t :=4"),
    ("5", false, "Select window 5", "select-window -t :=5"),
    ("6", false, "Select window 6", "select-window -t :=6"),
    ("7", false, "Select window 7", "select-window -t :=7"),
    ("8", false, "Select window 8", "select-window -t :=8"),
    ("9", false, "Select window 9", "select-window -t :=9"),
    (":", false, "Prompt for a command", "command-prompt"),
    (";", false, "Move to the previously active pane", "last-pane"),
    ("<", false, "Display window menu", r##"display-menu -T "#[align=centre]#{window_index}:#{window_name}" -x W -y W "#{?#{>:#{session_windows},1},,-}Swap Left" l { swap-window -t :-1 } "#{?#{>:#{session_windows},1},,-}Swap Right" r { swap-window -t :+1 } "#{?pane_marked_set,,-}Swap Marked" s { swap-window } '' Kill X { kill-window } Respawn R { respawn-window -k } "#{?pane_marked,Unmark,Mark}" m { select-pane -m } Rename n { command-prompt -F -I "#W" { rename-window -t "#{window_id}" "%%" } } '' "New After" w { new-window -a } "New At End" W { new-window }"##),
    ("=", false, "Choose a paste buffer from a list", "choose-buffer -Z"),
    (">", false, "Display pane menu", r##"display-menu -T "#[align=centre]#{pane_index} (#{pane_id})" -x P -y P "#{?#{m/r:(copy|view)-mode,#{pane_mode}},Go To Top,}" < { send-keys -X history-top } "#{?#{m/r:(copy|view)-mode,#{pane_mode}},Go To Bottom,}" > { send-keys -X history-bottom } '' "#{?#{&&:#{buffer_size},#{!:#{pane_in_mode}}},Paste #[underscore]#{=/9/...:buffer_sample},}" p { paste-buffer } '' "#{?mouse_word,Search For #[underscore]#{=/9/...:mouse_word},}" C-r { if-shell -F "#{?#{m/r:(copy|view)-mode,#{pane_mode}},0,1}" "copy-mode -t=" ; send-keys -X -t = search-backward -- "#{q:mouse_word}" } "#{?mouse_word,Type #[underscore]#{=/9/...:mouse_word},}" C-y { copy-mode -q ; send-keys -l "#{q:mouse_word}" } "#{?mouse_word,Copy #[underscore]#{=/9/...:mouse_word},}" c { copy-mode -q ; set-buffer "#{q:mouse_word}" } "#{?mouse_line,Copy Line,}" l { copy-mode -q ; set-buffer "#{q:mouse_line}" } '' "#{?mouse_hyperlink,Type #[underscore]#{=/9/...:mouse_hyperlink},}" C-h { copy-mode -q ; send-keys -l "#{q:mouse_hyperlink}" } "#{?mouse_hyperlink,Copy #[underscore]#{=/9/...:mouse_hyperlink},}" h { copy-mode -q ; set-buffer "#{q:mouse_hyperlink}" } '' "#{?#{!:#{pane_floating_flag}},Horizontal Split,}" h { split-window -h } "#{?#{!:#{pane_floating_flag}},Vertical Split,}" v { split-window -v } '' "#{?#{&&:#{!:#{pane_floating_flag}},#{>:#{window_panes},1}},Swap Up,}" u { swap-pane -U } "#{?#{&&:#{!:#{pane_floating_flag}},#{>:#{window_panes},1}},Swap Down,}" d { swap-pane -D } "#{?pane_marked_set,,-}Swap Marked" s { swap-pane } '' Kill X { kill-pane } Respawn R { respawn-pane -k } "#{?pane_marked,Unmark,Mark}" m { select-pane -m } "#{?#{>:#{window_panes},1},,-}#{?window_zoomed_flag,Unzoom,Zoom}" z { resize-pane -Z }"##),
    ("?", false, "List key bindings", "list-keys -N"),
    ("C", false, "Customize options", "customize-mode -Z"),
    ("D", false, "Choose and detach a client from a list", "choose-client -Z"),
    ("E", false, "Spread panes out evenly", "select-layout -E"),
    ("L", false, "Switch to the last client", "switch-client -l"),
    ("M", false, "Clear the marked pane", "select-pane -M"),
    ("[", false, "Enter copy mode", "copy-mode"),
    ("]", false, "Paste the most recent paste buffer", "paste-buffer -p"),
    ("c", false, "Create a new window", "new-window"),
    ("d", false, "Detach the current client", "detach-client"),
    ("f", false, "Search for a pane", r#"command-prompt { find-window -Z "%%" }"#),
    ("i", false, "Display window information", "display-message"),
    ("l", false, "Select the previously current window", "last-window"),
    ("m", false, "Toggle the marked pane", "select-pane -m"),
    ("n", false, "Select the next window", "next-window"),
    ("o", false, "Select the next pane", "select-pane -t :.+"),
    ("p", false, "Select the previous window", "previous-window"),
    ("q", false, "Display pane numbers", "display-panes"),
    ("r", false, "Redraw the current client", "refresh-client"),
    ("s", false, "Choose a session from a list", "choose-tree -Zs"),
    ("t", false, "Show a clock", "clock-mode"),
    ("w", false, "Choose a window from a list", "choose-tree -Zw"),
    ("x", false, "Kill the active pane", r#"confirm-before -p "kill-pane #P? (y/n)" kill-pane"#),
    ("z", false, "Zoom the active pane", "resize-pane -Z"),
    ("{", false, "Swap the active pane with the pane above", "swap-pane -U"),
    ("}", false, "Swap the active pane with the pane below", "swap-pane -D"),
    ("~", false, "Show messages", "show-messages"),
    ("DC", true, "Reset so the visible part of the window follows the cursor", "refresh-client -c"),
    ("PPage", false, "Enter copy mode and scroll up", "copy-mode -u"),
    ("Up", true, "Select the pane above the active pane", "select-pane -U"),
    ("Down", true, "Select the pane below the active pane", "select-pane -D"),
    ("Left", true, "Select the pane to the left of the active pane", "select-pane -L"),
    ("Right", true, "Select the pane to the right of the active pane", "select-pane -R"),
    ("M-1", false, "Set the even-horizontal layout", "select-layout even-horizontal"),
    ("M-2", false, "Set the even-vertical layout", "select-layout even-vertical"),
    ("M-3", false, "Set the main-horizontal layout", "select-layout main-horizontal"),
    ("M-4", false, "Set the main-vertical layout", "select-layout main-vertical"),
    ("M-5", false, "Select the tiled layout", "select-layout tiled"),
    ("M-6", false, "Set the main-horizontal-mirrored layout", "select-layout main-horizontal-mirrored"),
    ("M-7", false, "Set the main-vertical-mirrored layout", "select-layout main-vertical-mirrored"),
    ("M-n", false, "Select the next window with an alert", "next-window -a"),
    ("M-o", false, "Rotate through the panes in reverse", "rotate-window -D"),
    ("M-p", false, "Select the previous window with an alert", "previous-window -a"),
    ("M-Up", true, "Resize the pane up by 5", "resize-pane -U 5"),
    ("M-Down", true, "Resize the pane down by 5", "resize-pane -D 5"),
    ("M-Left", true, "Resize the pane left by 5", "resize-pane -L 5"),
    ("M-Right", true, "Resize the pane right by 5", "resize-pane -R 5"),
    ("C-b", false, "Send the prefix key", "send-prefix"),
    ("C-o", false, "Rotate through the panes", "rotate-window"),
    ("C-z", false, "Suspend the current client", "suspend-client"),
    ("C-Up", true, "Resize the pane up", "resize-pane -U"),
    ("C-Down", true, "Resize the pane down", "resize-pane -D"),
    ("C-Left", true, "Resize the pane left", "resize-pane -L"),
    ("C-Right", true, "Resize the pane right", "resize-pane -R"),
    ("S-Up", true, "Move the visible part of the window up", "refresh-client -U 10"),
    ("S-Down", true, "Move the visible part of the window down", "refresh-client -D 10"),
    ("S-Left", true, "Move the visible part of the window left", "refresh-client -L 10"),
    ("S-Right", true, "Move the visible part of the window right", "refresh-client -R 10"),
];
