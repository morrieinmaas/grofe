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
    let here = "#{pane_current_path}";
    bind(
        "%",
        &["split-window", "-h", "-c", here],
        false,
        "Split window horizontally",
    );
    bind(
        "\"",
        &["split-window", "-c", here],
        false,
        "Split window vertically",
    );
    bind(
        "c",
        &["new-window", "-c", here],
        false,
        "Create a new window",
    );
    bind("d", &["detach-client"], false, "Detach the current client");
    bind("n", &["next-window"], false, "Select the next window");
    bind(
        "p",
        &["previous-window"],
        false,
        "Select the previous window",
    );
    bind(
        "l",
        &["last-window"],
        false,
        "Select the previously current window",
    );
    bind(
        "(",
        &["switch-client", "-p"],
        false,
        "Switch to previous client",
    );
    bind(
        ")",
        &["switch-client", "-n"],
        false,
        "Switch to next client",
    );
    bind(
        "L",
        &["switch-client", "-l"],
        false,
        "Switch to the last client",
    );
    bind("z", &["resize-pane", "-Z"], false, "Zoom the active pane");
    bind("C-b", &["send-prefix"], false, "Send the prefix key");
    bind(":", &["command-prompt"], false, "Prompt for a command");
    bind(
        ",",
        &["command-prompt", "-I", "#W", "rename-window \"%%\""],
        false,
        "Rename current window",
    );
    bind(
        "$",
        &["command-prompt", "-I", "#S", "rename-session \"%%\""],
        false,
        "Rename current session",
    );
    for n in 0..=9 {
        let target = format!(":{n}");
        bind(
            &n.to_string(),
            &["select-window", "-t", &target],
            false,
            &format!("Select window {n}"),
        );
    }
    for (key, flag, side) in [
        ("Up", "-U", "above"),
        ("Down", "-D", "below"),
        ("Left", "-L", "to the left of"),
        ("Right", "-R", "to the right of"),
    ] {
        let dir = key.to_ascii_lowercase();
        bind(
            key,
            &["select-pane", flag],
            true,
            &format!("Select the pane {side} the active pane"),
        );
        bind(
            &format!("M-{key}"),
            &["resize-pane", flag, "5"],
            true,
            &format!("Resize the pane {dir} by 5"),
        );
        bind(
            &format!("C-{key}"),
            &["resize-pane", flag],
            true,
            &format!("Resize the pane {dir}"),
        );
    }
    // D10: the stock keys whose verbs are the structure verbs.
    bind(
        ";",
        &["last-pane"],
        false,
        "Move to the previously active pane",
    );
    bind("C-o", &["rotate-window"], false, "Rotate through the panes");
    bind(
        "M-o",
        &["rotate-window", "-D"],
        false,
        "Rotate through the panes in reverse",
    );
    bind("q", &["display-panes"], false, "Display pane numbers");
    bind(
        ".",
        &["command-prompt", "-T", "target", "move-window -t \"%%\""],
        false,
        "Move the current window",
    );
    bind(
        "'",
        &[
            "command-prompt",
            "-T",
            "window-target",
            "-p",
            "index",
            "select-window -t \":%%\"",
        ],
        false,
        "Prompt for window index to select",
    );
    bind(
        "C-z",
        &["suspend-client"],
        false,
        "Suspend the current client",
    );
    bind(
        "D",
        &["choose-client", "-Z"],
        false,
        "Choose and detach a client from a list",
    );
    bind("~", &["show-messages"], false, "Show messages");
    bind(
        "E",
        &["select-layout", "-E"],
        false,
        "Spread panes out evenly",
    );
    bind(
        "M-n",
        &["next-window", "-a"],
        false,
        "Select the next window with an alert",
    );
    bind(
        "M-p",
        &["previous-window", "-a"],
        false,
        "Select the previous window with an alert",
    );
    bind(
        "/",
        &["command-prompt", "-k", "-p", "key", "list-keys -1N \"%%\""],
        false,
        "Describe key binding",
    );
    bind(
        "x",
        &[
            "confirm-before",
            "-p",
            "kill-pane #{pane_index}? (y/n)",
            "kill-pane",
        ],
        false,
        "Kill the active pane",
    );
    bind(
        "&",
        &[
            "confirm-before",
            "-p",
            "kill-window #{window_name}? (y/n)",
            "kill-window",
        ],
        false,
        "Kill current window",
    );
    bind("!", &["break-pane"], false, "Break pane to a new window");
    // FR.4: grofe's own, so a tmux-identity server drops them with the rest
    // of grofe's prefix defaults ([`KeyTables::use_tmux_stock`]) and gets
    // tmux's `f` (find-window) back. `f` is the main key; `F` stays for
    // configs that bind `f` themselves.
    // grofe's own: tmux 3.7b has no key for `kill-server`.
    bind(
        "Q",
        &[
            "confirm-before",
            "-p",
            "kill-server: stop grofe and every session? (y/n)",
            "kill-server",
        ],
        false,
        "Stop the server and every session",
    );
    // grofe's own: tmux 3.7b binds no `u`.
    bind(
        "u",
        &["agent", "next"],
        false,
        "Jump to the agent that needs you",
    );
    for key in ["f", "F"] {
        bind(
            key,
            &[
                "if-shell",
                "-F",
                "#{pane_floating_flag}",
                "unfloat-pane",
                "float-pane",
            ],
            false,
            "Float or unfloat the active pane",
        );
    }
    // grofe's own: tmux 3.7b binds no `G`.
    bind(
        "G",
        &["copy-mode", "-h"],
        false,
        "Copy a match on screen by its label",
    );
    bind("a", &["new-scratchpad"], false, "Open a scratchpad");
    bind(
        "S",
        &["toggle-scratchpad"],
        false,
        "Hide or show a scratchpad",
    );
    bind(
        "A",
        &["choose-scratchpad"],
        false,
        "Choose a scratchpad from a list",
    );
    bind(
        "{",
        &["swap-pane", "-U"],
        false,
        "Swap the active pane with the pane above",
    );
    bind(
        "}",
        &["swap-pane", "-D"],
        false,
        "Swap the active pane with the pane below",
    );
    bind(
        "i",
        &["display-message"],
        false,
        "Display window information",
    );
    bind(
        "w",
        &["choose-tree", "-Zw"],
        false,
        "Choose a window from a list",
    );
    bind(
        "Space",
        &["key-menu", "-T", "prefix"],
        false,
        "Show the key menu",
    );
    bind("M-Space", &["next-layout"], false, "Select next layout");
    bind("U", &["choose-agent"], false, "Pick an agent");
    bind(
        "b",
        &["agent-sidebar", "toggle"],
        false,
        "Toggle the agent sidebar",
    );
    bind("O", &["agent-grid"], false, "Show all agents");
    bind("y", &["choose-notification"], false, "Show notifications");
    bind("?", &["list-keys", "-N"], false, "List key bindings");
    bind("r", &["refresh-client"], false, "Redraw the current client");
    bind(
        "s",
        &["choose-tree", "-Zs"],
        false,
        "Choose a session from a list",
    );
    // TM.4: the buffer keys.
    bind(
        "]",
        &["paste-buffer", "-p"],
        false,
        "Paste the most recent paste buffer",
    );
    bind("#", &["list-buffers"], false, "List all paste buffers");
    bind(
        "=",
        &["choose-buffer", "-Z"],
        false,
        "Choose a paste buffer from a list",
    );
    bind(
        "-",
        &["delete-buffer"],
        false,
        "Delete the most recent paste buffer",
    );
    for (key, flag, note) in [
        ("S-Up", "-U", "Move the visible part of the window up"),
        ("S-Down", "-D", "Move the visible part of the window down"),
        ("S-Left", "-L", "Move the visible part of the window left"),
        ("S-Right", "-R", "Move the visible part of the window right"),
    ] {
        bind(key, &["refresh-client", flag, "10"], true, note);
    }
    bind(
        "DC",
        &["refresh-client", "-c"],
        true,
        "Reset so the visible part of the window follows the cursor",
    );
    for (key, layout, verb) in [
        ("M-1", "even-horizontal", "Set"),
        ("M-2", "even-vertical", "Set"),
        ("M-3", "main-horizontal", "Set"),
        ("M-4", "main-vertical", "Set"),
        ("M-5", "tiled", "Select"),
        ("M-6", "main-horizontal-mirrored", "Set"),
        ("M-7", "main-vertical-mirrored", "Set"),
    ] {
        bind(
            key,
            &["select-layout", layout],
            false,
            &format!("{verb} the {layout} layout"),
        );
    }
    // Copy mode's keys.
    bind("[", &["copy-mode"], false, "Enter copy mode");
    bind(
        "PPage",
        &["copy-mode", "-u"],
        false,
        "Enter copy mode and scroll up",
    );
    for (table, rows) in [
        (COPY_MODE_VI_TABLE, COPY_MODE_VI),
        (COPY_MODE_TABLE, COPY_MODE_EMACS),
    ] {
        for (key, text) in rows {
            if let (Ok(key), Ok(cmds)) = (parse_key(key), crate::cmd::split_command_string(text)) {
                t.insert(
                    table,
                    key,
                    Binding::new(cmds, false, BindingSource::Default),
                );
            }
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
    if let (Ok(key), Ok(cmds)) = (
        parse_key("MouseDown3Pane"),
        crate::cmd::split_command_string(GROFE_PANE_MENU),
    ) {
        t.insert(
            ROOT_TABLE,
            key,
            Binding::new(cmds, false, BindingSource::Default),
        );
    }
    // FR.3: Ctrl and drag anywhere in a pane moves it, as dragging a
    // framed tile's title does; grofe's own, so a tmux-identity server
    // drops it and the key is unbound, as in tmux.
    if let Ok(key) = parse_key("C-MouseDrag1Pane") {
        let argv = ["drag-pane", "-M"].map(String::from).to_vec();
        t.insert(
            ROOT_TABLE,
            key,
            Binding::new(vec![argv], false, BindingSource::Default),
        );
    }
    // The float control of a framed title's tab (`render::frame_tab`);
    // grofe's own, as the tab is.
    if let (Ok(key), Ok(cmds)) = (
        parse_key("MouseDown1Control7"),
        crate::cmd::split_command_string(GROFE_FLOAT_CONTROL),
    ) {
        t.insert(
            ROOT_TABLE,
            key,
            Binding::new(cmds, false, BindingSource::Default),
        );
    }
    // A click on the status line's agents badge opens the tray; any other
    // click switches client, as tmux's stock row does.
    if let (Ok(key), Ok(cmds)) = (
        parse_key("MouseDown1Status"),
        crate::cmd::split_command_string(GROFE_STATUS_CLICK),
    ) {
        t.insert(
            ROOT_TABLE,
            key,
            Binding::new(cmds, false, BindingSource::Default),
        );
    }
    for (table, bindings) in t.tables.iter_mut() {
        for b in bindings.values_mut() {
            b.group = Some(default_group(table, &b.cmds).to_string());
        }
    }
    t
}

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
pub const COPY_MODE_VI: &[(&str, &str)] = &[
    (
        r#"#"#,
        r#"send-keys -F -X search-backward -- '#{copy_cursor_word}'"#,
    ),
    (
        r#"*"#,
        r#"send-keys -F -X search-forward -- '#{copy_cursor_word}'"#,
    ),
    (r#"C-c"#, r#"send-keys -X cancel"#),
    (r#"C-d"#, r#"send-keys -X halfpage-down"#),
    (r#"C-e"#, r#"send-keys -X scroll-down"#),
    (r#"C-b"#, r#"send-keys -X page-up"#),
    (r#"C-f"#, r#"send-keys -X page-down"#),
    (r#"C-h"#, r#"send-keys -X cursor-left"#),
    (r#"C-j"#, r#"send-keys -X copy-pipe-and-cancel"#),
    (r#"Enter"#, r#"send-keys -X copy-pipe-and-cancel"#),
    (r#"C-u"#, r#"send-keys -X halfpage-up"#),
    (r#"C-v"#, r#"send-keys -X rectangle-toggle"#),
    (r#"C-y"#, r#"send-keys -X scroll-up"#),
    (r#"Escape"#, r#"send-keys -X clear-selection"#),
    (r#"C-["#, r#"send-keys -X clear-selection"#),
    (r#"Space"#, r#"send-keys -X begin-selection"#),
    (r#"$"#, r#"send-keys -X end-of-line"#),
    (r#","#, r#"send-keys -X jump-reverse"#),
    (
        r#"/"#,
        r#"command-prompt -T search -p '(search down)' { send-keys -X search-forward -- '%%' }"#,
    ),
    (r#"0"#, r#"send-keys -X start-of-line"#),
    (
        r#"1"#,
        r#"command-prompt -N -p '(repeat)' -I 1 { send-keys -N '%%' }"#,
    ),
    (
        r#"2"#,
        r#"command-prompt -N -p '(repeat)' -I 2 { send-keys -N '%%' }"#,
    ),
    (
        r#"3"#,
        r#"command-prompt -N -p '(repeat)' -I 3 { send-keys -N '%%' }"#,
    ),
    (
        r#"4"#,
        r#"command-prompt -N -p '(repeat)' -I 4 { send-keys -N '%%' }"#,
    ),
    (
        r#"5"#,
        r#"command-prompt -N -p '(repeat)' -I 5 { send-keys -N '%%' }"#,
    ),
    (
        r#"6"#,
        r#"command-prompt -N -p '(repeat)' -I 6 { send-keys -N '%%' }"#,
    ),
    (
        r#"7"#,
        r#"command-prompt -N -p '(repeat)' -I 7 { send-keys -N '%%' }"#,
    ),
    (
        r#"8"#,
        r#"command-prompt -N -p '(repeat)' -I 8 { send-keys -N '%%' }"#,
    ),
    (
        r#"9"#,
        r#"command-prompt -N -p '(repeat)' -I 9 { send-keys -N '%%' }"#,
    ),
    (
        r#":"#,
        r#"command-prompt -p '(goto line)' { send-keys -X goto-line -- '%%' }"#,
    ),
    (r#";"#, r#"send-keys -X jump-again"#),
    (
        r#"?"#,
        r#"command-prompt -T search -p '(search up)' { send-keys -X search-backward -- '%%' }"#,
    ),
    (r#"A"#, r#"send-keys -X append-selection-and-cancel"#),
    (r#"B"#, r#"send-keys -X previous-space"#),
    (r#"D"#, r#"send-keys -X copy-pipe-end-of-line-and-cancel"#),
    (r#"E"#, r#"send-keys -X next-space-end"#),
    (
        r#"F"#,
        r#"command-prompt -1 -p '(jump backward)' { send-keys -X jump-backward -- '%%' }"#,
    ),
    (r#"G"#, r#"send-keys -X history-bottom"#),
    (r#"H"#, r#"send-keys -X top-line"#),
    (r#"J"#, r#"send-keys -X scroll-down"#),
    (r#"K"#, r#"send-keys -X scroll-up"#),
    (r#"L"#, r#"send-keys -X bottom-line"#),
    (r#"M"#, r#"send-keys -X middle-line"#),
    (r#"N"#, r#"send-keys -X search-reverse"#),
    (r#"P"#, r#"send-keys -X toggle-position"#),
    (
        r#"T"#,
        r#"command-prompt -1 -p '(jump to backward)' { send-keys -X jump-to-backward -- '%%' }"#,
    ),
    (r#"V"#, r#"send-keys -X select-line"#),
    (r#"W"#, r#"send-keys -X next-space"#),
    (r#"X"#, r#"send-keys -X set-mark"#),
    (r#"^"#, r#"send-keys -X back-to-indentation"#),
    (r#"b"#, r#"send-keys -X previous-word"#),
    (r#"e"#, r#"send-keys -X next-word-end"#),
    (
        r#"f"#,
        r#"command-prompt -1 -p '(jump forward)' { send-keys -X jump-forward -- '%%' }"#,
    ),
    (r#"g"#, r#"send-keys -X history-top"#),
    (r#"h"#, r#"send-keys -X cursor-left"#),
    (r#"j"#, r#"send-keys -X cursor-down"#),
    (r#"k"#, r#"send-keys -X cursor-up"#),
    (r#"z"#, r#"send-keys -X scroll-middle"#),
    (r#"l"#, r#"send-keys -X cursor-right"#),
    (r#"n"#, r#"send-keys -X search-again"#),
    (r#"o"#, r#"send-keys -X other-end"#),
    (r#"q"#, r#"send-keys -X cancel"#),
    (r#"r"#, r#"send-keys -X refresh-from-pane"#),
    (
        r#"t"#,
        r#"command-prompt -1 -p '(jump to forward)' { send-keys -X jump-to-forward -- '%%' }"#,
    ),
    (r#"v"#, r#"send-keys -X rectangle-toggle"#),
    (r#"w"#, r#"send-keys -X next-word"#),
    (r#"{"#, r#"send-keys -X previous-paragraph"#),
    (r#"}"#, r#"send-keys -X next-paragraph"#),
    (r#"%"#, r#"send-keys -X next-matching-bracket"#),
    (r#"Home"#, r#"send-keys -X start-of-line"#),
    (r#"End"#, r#"send-keys -X end-of-line"#),
    (r#"MouseDown1Pane"#, r#"select-pane"#),
    (
        r#"MouseDrag1Pane"#,
        r#"select-pane; send-keys -X begin-selection"#,
    ),
    (
        r#"MouseDragEnd1Pane"#,
        r#"send-keys -X copy-pipe-and-cancel"#,
    ),
    (
        r#"WheelUpPane"#,
        r#"select-pane; send-keys -N 5 -X scroll-up"#,
    ),
    (
        r#"WheelDownPane"#,
        r#"select-pane; send-keys -N 5 -X scroll-down"#,
    ),
    (
        r#"DoubleClick1Pane"#,
        r#"select-pane; send-keys -X select-word; run-shell -d 0.3; send-keys -X copy-pipe-and-cancel"#,
    ),
    (
        r#"TripleClick1Pane"#,
        r#"select-pane; send-keys -X select-line; run-shell -d 0.3; send-keys -X copy-pipe-and-cancel"#,
    ),
    (r#"BSpace"#, r#"send-keys -X cursor-left"#),
    (r#"NPage"#, r#"send-keys -X page-down"#),
    (r#"PPage"#, r#"send-keys -X page-up"#),
    (r#"Up"#, r#"send-keys -X cursor-up"#),
    (r#"Down"#, r#"send-keys -X cursor-down"#),
    (r#"Left"#, r#"send-keys -X cursor-left"#),
    (r#"Right"#, r#"send-keys -X cursor-right"#),
    (r#"M-x"#, r#"send-keys -X jump-to-mark"#),
    (r#"C-Up"#, r#"send-keys -X scroll-up"#),
    (r#"C-Down"#, r#"send-keys -X scroll-down"#),
];

/// The copy-mode key table with `mode-keys emacs`.
pub const COPY_MODE_TABLE: &str = "copy-mode";

/// tmux 3.7b's 75 stock `copy-mode` (emacs) bindings (`key-bindings.c`), in
/// native spelling like [`COPY_MODE_VI`].
pub const COPY_MODE_EMACS: &[(&str, &str)] = &[
    (r#"Escape"#, r#"send-keys -X cancel"#),
    (r#"Space"#, r#"send-keys -X page-down"#),
    (r#","#, r#"send-keys -X jump-reverse"#),
    (r#";"#, r#"send-keys -X jump-again"#),
    (
        r#"F"#,
        r#"command-prompt -1 -p '(jump backward)' { send-keys -X jump-backward -- '%%' }"#,
    ),
    (r#"N"#, r#"send-keys -X search-reverse"#),
    (r#"P"#, r#"send-keys -X toggle-position"#),
    (r#"R"#, r#"send-keys -X rectangle-toggle"#),
    (
        r#"T"#,
        r#"command-prompt -1 -p '(jump to backward)' { send-keys -X jump-to-backward -- '%%' }"#,
    ),
    (r#"X"#, r#"send-keys -X set-mark"#),
    (
        r#"f"#,
        r#"command-prompt -1 -p '(jump forward)' { send-keys -X jump-forward -- '%%' }"#,
    ),
    (
        r#"g"#,
        r#"command-prompt -p '(goto line)' { send-keys -X goto-line -- '%%' }"#,
    ),
    (r#"n"#, r#"send-keys -X search-again"#),
    (r#"q"#, r#"send-keys -X cancel"#),
    (r#"r"#, r#"send-keys -X refresh-from-pane"#),
    (
        r#"t"#,
        r#"command-prompt -1 -p '(jump to forward)' { send-keys -X jump-to-forward -- '%%' }"#,
    ),
    (r#"Home"#, r#"send-keys -X start-of-line"#),
    (r#"End"#, r#"send-keys -X end-of-line"#),
    (r#"NPage"#, r#"send-keys -X page-down"#),
    (r#"PPage"#, r#"send-keys -X page-up"#),
    (r#"Up"#, r#"send-keys -X cursor-up"#),
    (r#"Down"#, r#"send-keys -X cursor-down"#),
    (r#"Left"#, r#"send-keys -X cursor-left"#),
    (r#"Right"#, r#"send-keys -X cursor-right"#),
    (r#"MouseDown1Pane"#, r#"select-pane"#),
    (
        r#"MouseDrag1Pane"#,
        r#"select-pane; send-keys -X begin-selection"#,
    ),
    (
        r#"MouseDragEnd1Pane"#,
        r#"send-keys -X copy-pipe-and-cancel"#,
    ),
    (
        r#"WheelDownPane"#,
        r#"select-pane; send-keys -N 5 -X scroll-down"#,
    ),
    (
        r#"WheelUpPane"#,
        r#"select-pane; send-keys -N 5 -X scroll-up"#,
    ),
    (
        r#"DoubleClick1Pane"#,
        r#"select-pane; send-keys -X select-word; run-shell -d 0.3; send-keys -X copy-pipe-and-cancel"#,
    ),
    (
        r#"TripleClick1Pane"#,
        r#"select-pane; send-keys -X select-line; run-shell -d 0.3; send-keys -X copy-pipe-and-cancel"#,
    ),
    (
        r#"M-1"#,
        r#"command-prompt -N -p '(repeat)' -I 1 { send-keys -N '%%' }"#,
    ),
    (
        r#"M-2"#,
        r#"command-prompt -N -p '(repeat)' -I 2 { send-keys -N '%%' }"#,
    ),
    (
        r#"M-3"#,
        r#"command-prompt -N -p '(repeat)' -I 3 { send-keys -N '%%' }"#,
    ),
    (
        r#"M-4"#,
        r#"command-prompt -N -p '(repeat)' -I 4 { send-keys -N '%%' }"#,
    ),
    (
        r#"M-5"#,
        r#"command-prompt -N -p '(repeat)' -I 5 { send-keys -N '%%' }"#,
    ),
    (
        r#"M-6"#,
        r#"command-prompt -N -p '(repeat)' -I 6 { send-keys -N '%%' }"#,
    ),
    (
        r#"M-7"#,
        r#"command-prompt -N -p '(repeat)' -I 7 { send-keys -N '%%' }"#,
    ),
    (
        r#"M-8"#,
        r#"command-prompt -N -p '(repeat)' -I 8 { send-keys -N '%%' }"#,
    ),
    (
        r#"M-9"#,
        r#"command-prompt -N -p '(repeat)' -I 9 { send-keys -N '%%' }"#,
    ),
    (r#"M-<"#, r#"send-keys -X history-top"#),
    (r#"M->"#, r#"send-keys -X history-bottom"#),
    (r#"M-R"#, r#"send-keys -X top-line"#),
    (r#"M-b"#, r#"send-keys -X previous-word"#),
    (r#"M-f"#, r#"send-keys -X next-word-end"#),
    (r#"M-l"#, r#"send-keys -X cursor-centre-horizontal"#),
    (r#"M-m"#, r#"send-keys -X back-to-indentation"#),
    (r#"M-r"#, r#"send-keys -X middle-line"#),
    (r#"M-v"#, r#"send-keys -X page-up"#),
    (r#"M-w"#, r#"send-keys -X copy-pipe-and-cancel"#),
    (r#"M-x"#, r#"send-keys -X jump-to-mark"#),
    (r#"M-{"#, r#"send-keys -X previous-paragraph"#),
    (r#"M-}"#, r#"send-keys -X next-paragraph"#),
    (r#"M-Up"#, r#"send-keys -X halfpage-up"#),
    (r#"M-Down"#, r#"send-keys -X halfpage-down"#),
    (r#"C-Space"#, r#"send-keys -X begin-selection"#),
    (r#"C-["#, r#"send-keys -X cancel"#),
    (r#"C-a"#, r#"send-keys -X start-of-line"#),
    (r#"C-b"#, r#"send-keys -X cursor-left"#),
    (r#"C-c"#, r#"send-keys -X cancel"#),
    (r#"C-e"#, r#"send-keys -X end-of-line"#),
    (r#"C-f"#, r#"send-keys -X cursor-right"#),
    (r#"C-g"#, r#"send-keys -X clear-selection"#),
    (r#"C-k"#, r#"send-keys -X copy-pipe-end-of-line-and-cancel"#),
    (r#"C-l"#, r#"send-keys -X recentre-top-bottom"#),
    (r#"C-n"#, r#"send-keys -X cursor-down"#),
    (r#"C-p"#, r#"send-keys -X cursor-up"#),
    (
        r#"C-r"#,
        r#"command-prompt -T search -i -I '#{pane_search_string}' -p '(search up)' { send-keys -X search-backward-incremental -- '%%' }"#,
    ),
    (
        r#"C-s"#,
        r#"command-prompt -T search -i -I '#{pane_search_string}' -p '(search down)' { send-keys -X search-forward-incremental -- '%%' }"#,
    ),
    (r#"C-v"#, r#"send-keys -X page-down"#),
    (r#"C-w"#, r#"send-keys -X copy-pipe-and-cancel"#),
    (r#"C-Up"#, r#"send-keys -X scroll-up"#),
    (r#"C-Down"#, r#"send-keys -X scroll-down"#),
    (r#"C-M-b"#, r#"send-keys -X previous-matching-bracket"#),
    (r#"C-M-f"#, r#"send-keys -X next-matching-bracket"#),
];

/// How tmux 3.7b's `list-keys -T copy-mode` prints the stock emacs rows
/// whose template is a `{ }` list (the `b2b/emacs-keys` golden), for a
/// server with tmux identity ([`KeyTables::use_tmux_stock`]).
pub const TMUX_COPY_MODE_EMACS_SHOWN: &[(&str, &str)] = &[
    (
        r#"F"#,
        r#"command-prompt -1 -p "(jump backward)" { send-keys -X jump-backward -- "%%" }"#,
    ),
    (
        r#"T"#,
        r#"command-prompt -1 -p "(jump to backward)" { send-keys -X jump-to-backward -- "%%" }"#,
    ),
    (
        r#"f"#,
        r#"command-prompt -1 -p "(jump forward)" { send-keys -X jump-forward -- "%%" }"#,
    ),
    (
        r#"g"#,
        r#"command-prompt -p "(goto line)" { send-keys -X goto-line -- "%%" }"#,
    ),
    (
        r#"t"#,
        r#"command-prompt -1 -p "(jump to forward)" { send-keys -X jump-to-forward -- "%%" }"#,
    ),
    (
        r#"M-1"#,
        r#"command-prompt -N -I 1 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"M-2"#,
        r#"command-prompt -N -I 2 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"M-3"#,
        r#"command-prompt -N -I 3 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"M-4"#,
        r#"command-prompt -N -I 4 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"M-5"#,
        r#"command-prompt -N -I 5 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"M-6"#,
        r#"command-prompt -N -I 6 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"M-7"#,
        r#"command-prompt -N -I 7 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"M-8"#,
        r#"command-prompt -N -I 8 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"M-9"#,
        r#"command-prompt -N -I 9 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"C-r"#,
        r##"command-prompt -i -I "#{pane_search_string}" -T search -p "(search up)" { send-keys -X search-backward-incremental -- "%%" }"##,
    ),
    (
        r#"C-s"#,
        r##"command-prompt -i -I "#{pane_search_string}" -T search -p "(search down)" { send-keys -X search-forward-incremental -- "%%" }"##,
    ),
];

/// How tmux 3.7b's `list-keys -T copy-mode-vi` prints the stock rows whose
/// template is a `{ }` list (the `tm_0/list-keys-copy-mode-vi` golden),
/// for a server with tmux identity ([`KeyTables::use_tmux_stock`]).
pub const TMUX_COPY_MODE_VI_SHOWN: &[(&str, &str)] = &[
    (
        r#"/"#,
        r#"command-prompt -T search -p "(search down)" { send-keys -X search-forward -- "%%" }"#,
    ),
    (
        r#"?"#,
        r#"command-prompt -T search -p "(search up)" { send-keys -X search-backward -- "%%" }"#,
    ),
    (
        r#":"#,
        r#"command-prompt -p "(goto line)" { send-keys -X goto-line -- "%%" }"#,
    ),
    (
        r#"F"#,
        r#"command-prompt -1 -p "(jump backward)" { send-keys -X jump-backward -- "%%" }"#,
    ),
    (
        r#"T"#,
        r#"command-prompt -1 -p "(jump to backward)" { send-keys -X jump-to-backward -- "%%" }"#,
    ),
    (
        r#"f"#,
        r#"command-prompt -1 -p "(jump forward)" { send-keys -X jump-forward -- "%%" }"#,
    ),
    (
        r#"t"#,
        r#"command-prompt -1 -p "(jump to forward)" { send-keys -X jump-to-forward -- "%%" }"#,
    ),
    (
        r#"1"#,
        r#"command-prompt -N -I 1 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"2"#,
        r#"command-prompt -N -I 2 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"3"#,
        r#"command-prompt -N -I 3 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"4"#,
        r#"command-prompt -N -I 4 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"5"#,
        r#"command-prompt -N -I 5 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"6"#,
        r#"command-prompt -N -I 6 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"7"#,
        r#"command-prompt -N -I 7 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"8"#,
        r#"command-prompt -N -I 8 -p (repeat) { send-keys -N "%%" }"#,
    ),
    (
        r#"9"#,
        r#"command-prompt -N -I 9 -p (repeat) { send-keys -N "%%" }"#,
    ),
];

/// tmux 3.7b's stock `root` table, all of its mouse rows (`list-keys -T
/// root` on a `-f /dev/null` server, the `b3wpc/stock-root` golden): the key
/// and its commands as tmux prints them. A server with tmux identity binds
/// them all; the rows in [`NATIVE_STOCK_ROOT`] are bound on every server.
pub const TMUX_STOCK_ROOT: &[(&str, &str)] = &[
    (r#"MouseDown1Pane"#, r##"select-pane -t = \; send-keys -M"##),
    (r#"MouseDown1Status"#, r##"switch-client -t ="##),
    (r#"MouseDown1Border"#, r##"select-pane -M"##),
    (
        r#"MouseDown1ScrollbarUp"#,
        r##"if-shell -F -t = "#{pane_in_mode}" { send-keys -X page-up } { copy-mode -u }"##,
    ),
    (
        r#"MouseDown1ScrollbarDown"#,
        r##"if-shell -F -t = "#{pane_in_mode}" { send-keys -X page-down } { copy-mode -d }"##,
    ),
    (r#"MouseDown1Control8"#, r##"resize-pane -Z"##),
    (
        r#"MouseDown1Control9"#,
        r##"display-menu -O -T "Kill pane #{pane_index}?" -t = -x M -y M Yes y { kill-pane -t = } No n {  }"##,
    ),
    (
        r#"MouseDown2Pane"#,
        r##"select-pane -t = \; if-shell -F "#{||:#{pane_in_mode},#{mouse_any_flag}}" { send-keys -M } { paste-buffer -p }"##,
    ),
    (
        r#"MouseDown3Pane"#,
        r##"if-shell -F -t = "#{||:#{mouse_any_flag},#{&&:#{pane_in_mode},#{?#{m/r:(copy|view)-mode,#{pane_mode}},0,1}}}" { select-pane -t = ; send-keys -M } { display-menu -T "#[align=centre]#{pane_index} (#{pane_id})" -t = -x M -y M "#{?#{m/r:(copy|view)-mode,#{pane_mode}},Go To Top,}" < { send-keys -X history-top } "#{?#{m/r:(copy|view)-mode,#{pane_mode}},Go To Bottom,}" > { send-keys -X history-bottom } '' "#{?#{&&:#{buffer_size},#{!:#{pane_in_mode}}},Paste #[underscore]#{=/9/...:buffer_sample},}" p { paste-buffer } '' "#{?mouse_word,Search For #[underscore]#{=/9/...:mouse_word},}" C-r { if-shell -F "#{?#{m/r:(copy|view)-mode,#{pane_mode}},0,1}" "copy-mode -t=" ; send-keys -X -t = search-backward -- "#{q:mouse_word}" } "#{?mouse_word,Type #[underscore]#{=/9/...:mouse_word},}" C-y { copy-mode -q ; send-keys -l "#{q:mouse_word}" } "#{?mouse_word,Copy #[underscore]#{=/9/...:mouse_word},}" c { copy-mode -q ; set-buffer "#{q:mouse_word}" } "#{?mouse_line,Copy Line,}" l { copy-mode -q ; set-buffer "#{q:mouse_line}" } '' "#{?mouse_hyperlink,Type #[underscore]#{=/9/...:mouse_hyperlink},}" C-h { copy-mode -q ; send-keys -l "#{q:mouse_hyperlink}" } "#{?mouse_hyperlink,Copy #[underscore]#{=/9/...:mouse_hyperlink},}" h { copy-mode -q ; set-buffer "#{q:mouse_hyperlink}" } '' "#{?#{!:#{pane_floating_flag}},Horizontal Split,}" h { split-window -h } "#{?#{!:#{pane_floating_flag}},Vertical Split,}" v { split-window -v } '' "#{?#{&&:#{!:#{pane_floating_flag}},#{>:#{window_panes},1}},Swap Up,}" u { swap-pane -U } "#{?#{&&:#{!:#{pane_floating_flag}},#{>:#{window_panes},1}},Swap Down,}" d { swap-pane -D } "#{?pane_marked_set,,-}Swap Marked" s { swap-pane } '' Kill X { kill-pane } Respawn R { respawn-pane -k } "#{?pane_marked,Unmark,Mark}" m { select-pane -m } "#{?#{>:#{window_panes},1},,-}#{?window_zoomed_flag,Unzoom,Zoom}" z { resize-pane -Z } }"##,
    ),
    (
        r#"MouseDown3Status"#,
        r##"display-menu -T "#[align=centre]#{window_index}:#{window_name}" -t = -x W -y W "#{?#{>:#{session_windows},1},,-}Swap Left" l { swap-window -t :-1 } "#{?#{>:#{session_windows},1},,-}Swap Right" r { swap-window -t :+1 } "#{?pane_marked_set,,-}Swap Marked" s { swap-window } '' Kill X { kill-window } Respawn R { respawn-window -k } "#{?pane_marked,Unmark,Mark}" m { select-pane -m } Rename n { command-prompt -F -I "#W" { rename-window -t "#{window_id}" "%%" } } '' "New After" w { new-window -a } "New At End" W { new-window }"##,
    ),
    (
        r#"MouseDown3StatusLeft"#,
        r##"display-menu -T "#[align=centre]#{session_name}" -t = -x M -y W Next n { switch-client -n } Previous p { switch-client -p } '' Renumber N { move-window -r } Rename r { command-prompt -I "#S" { rename-session "%%" } } Detach d { detach-client } '' "New Session" s { new-session } "New Window" w { new-window }"##,
    ),
    (
        r#"MouseDrag1Pane"#,
        r##"if-shell -F "#{||:#{pane_in_mode},#{mouse_any_flag}}" { send-keys -M } { copy-mode -M }"##,
    ),
    (r#"MouseDrag1Border"#, r##"resize-pane -M"##),
    (
        r#"MouseDrag1ScrollbarSlider"#,
        r##"if-shell -F -t = "#{pane_in_mode}" { send-keys -X scroll-to-mouse } { copy-mode -S }"##,
    ),
    (r#"WheelDownStatus"#, r##"next-window"##),
    (
        r#"WheelUpPane"#,
        r##"if-shell -F "#{||:#{alternate_on},#{pane_in_mode},#{mouse_any_flag}}" { send-keys -M } { copy-mode -e }"##,
    ),
    (r#"WheelUpStatus"#, r##"previous-window"##),
    (
        r#"DoubleClick1Pane"#,
        r##"select-pane -t = \; if-shell -F "#{||:#{pane_in_mode},#{mouse_any_flag}}" { send-keys -M } { copy-mode -H ; send-keys -X select-word ; run-shell -d 0.3 ; send-keys -X copy-pipe-and-cancel }"##,
    ),
    (
        r#"TripleClick1Pane"#,
        r##"select-pane -t = \; if-shell -F "#{||:#{pane_in_mode},#{mouse_any_flag}}" { send-keys -M } { copy-mode -H ; send-keys -X select-line ; run-shell -d 0.3 ; send-keys -X copy-pipe-and-cancel }"##,
    ),
    (
        r#"M-MouseDown3Pane"#,
        r##"display-menu -T "#[align=centre]#{pane_index} (#{pane_id})" -t = -x M -y M "#{?#{m/r:(copy|view)-mode,#{pane_mode}},Go To Top,}" < { send-keys -X history-top } "#{?#{m/r:(copy|view)-mode,#{pane_mode}},Go To Bottom,}" > { send-keys -X history-bottom } '' "#{?#{&&:#{buffer_size},#{!:#{pane_in_mode}}},Paste #[underscore]#{=/9/...:buffer_sample},}" p { paste-buffer } '' "#{?mouse_word,Search For #[underscore]#{=/9/...:mouse_word},}" C-r { if-shell -F "#{?#{m/r:(copy|view)-mode,#{pane_mode}},0,1}" "copy-mode -t=" ; send-keys -X -t = search-backward -- "#{q:mouse_word}" } "#{?mouse_word,Type #[underscore]#{=/9/...:mouse_word},}" C-y { copy-mode -q ; send-keys -l "#{q:mouse_word}" } "#{?mouse_word,Copy #[underscore]#{=/9/...:mouse_word},}" c { copy-mode -q ; set-buffer "#{q:mouse_word}" } "#{?mouse_line,Copy Line,}" l { copy-mode -q ; set-buffer "#{q:mouse_line}" } '' "#{?mouse_hyperlink,Type #[underscore]#{=/9/...:mouse_hyperlink},}" C-h { copy-mode -q ; send-keys -l "#{q:mouse_hyperlink}" } "#{?mouse_hyperlink,Copy #[underscore]#{=/9/...:mouse_hyperlink},}" h { copy-mode -q ; set-buffer "#{q:mouse_hyperlink}" } '' "#{?#{!:#{pane_floating_flag}},Horizontal Split,}" h { split-window -h } "#{?#{!:#{pane_floating_flag}},Vertical Split,}" v { split-window -v } '' "#{?#{&&:#{!:#{pane_floating_flag}},#{>:#{window_panes},1}},Swap Up,}" u { swap-pane -U } "#{?#{&&:#{!:#{pane_floating_flag}},#{>:#{window_panes},1}},Swap Down,}" d { swap-pane -D } "#{?pane_marked_set,,-}Swap Marked" s { swap-pane } '' Kill X { kill-pane } Respawn R { respawn-pane -k } "#{?pane_marked,Unmark,Mark}" m { select-pane -m } "#{?#{>:#{window_panes},1},,-}#{?window_zoomed_flag,Unzoom,Zoom}" z { resize-pane -Z }"##,
    ),
    (
        r#"M-MouseDown3Status"#,
        r##"display-menu -T "#[align=centre]#{window_index}:#{window_name}" -t = -x W -y W "#{?#{>:#{session_windows},1},,-}Swap Left" l { swap-window -t :-1 } "#{?#{>:#{session_windows},1},,-}Swap Right" r { swap-window -t :+1 } "#{?pane_marked_set,,-}Swap Marked" s { swap-window } '' Kill X { kill-window } Respawn R { respawn-window -k } "#{?pane_marked,Unmark,Mark}" m { select-pane -m } Rename n { command-prompt -F -I "#W" { rename-window -t "#{window_id}" "%%" } } '' "New After" w { new-window -a } "New At End" W { new-window }"##,
    ),
    (
        r#"M-MouseDown3StatusLeft"#,
        r##"display-menu -T "#[align=centre]#{session_name}" -t = -x M -y W Next n { switch-client -n } Previous p { switch-client -p } '' Renumber N { move-window -r } Rename r { command-prompt -I "#S" { rename-session "%%" } } Detach d { detach-client } '' "New Session" s { new-session } "New Window" w { new-window }"##,
    ),
    (r#"C-MouseDown1Pane"#, r##"swap-pane -s @"##),
    (r#"C-MouseDown1Status"#, r##"swap-window -t @"##),
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
pub const TMUX_STOCK: &[(&str, bool, &str, &str)] = &[
    (r#"Space"#, false, r#"Select next layout"#, r#"next-layout"#),
    (
        r#"!"#,
        false,
        r#"Break pane to a new window"#,
        r#"break-pane"#,
    ),
    (
        r#"""#,
        false,
        r#"Split window vertically"#,
        r#"split-window"#,
    ),
    (
        r#"#"#,
        false,
        r#"List all paste buffers"#,
        r#"list-buffers"#,
    ),
    (
        r#"$"#,
        false,
        r#"Rename current session"#,
        r##"command-prompt -I "#S" { rename-session "%%" }"##,
    ),
    (
        r#"%"#,
        false,
        r#"Split window horizontally"#,
        r#"split-window -h"#,
    ),
    (
        r#"&"#,
        false,
        r#"Kill current window"#,
        r#"confirm-before -p "kill-window #W? (y/n)" kill-window"#,
    ),
    (
        r#"'"#,
        false,
        r#"Prompt for window index to select"#,
        r#"command-prompt -T window-target -p index { select-window -t ":%%" }"#,
    ),
    (
        r#"("#,
        false,
        r#"Switch to previous client"#,
        r#"switch-client -p"#,
    ),
    (
        r#")"#,
        false,
        r#"Switch to next client"#,
        r#"switch-client -n"#,
    ),
    (r#"*"#, false, r#"New floating pane"#, r#"new-pane"#),
    (
        r#","#,
        false,
        r#"Rename current window"#,
        r##"command-prompt -I "#W" { rename-window "%%" }"##,
    ),
    (
        r#"-"#,
        false,
        r#"Delete the most recent paste buffer"#,
        r#"delete-buffer"#,
    ),
    (
        r#"."#,
        false,
        r#"Move the current window"#,
        r#"command-prompt -T target { move-window -t "%%" }"#,
    ),
    (
        r#"/"#,
        false,
        r#"Describe key binding"#,
        r#"command-prompt -k -p key { list-keys -1N "%%" }"#,
    ),
    (
        r#"0"#,
        false,
        r#"Select window 0"#,
        r#"select-window -t :=0"#,
    ),
    (
        r#"1"#,
        false,
        r#"Select window 1"#,
        r#"select-window -t :=1"#,
    ),
    (
        r#"2"#,
        false,
        r#"Select window 2"#,
        r#"select-window -t :=2"#,
    ),
    (
        r#"3"#,
        false,
        r#"Select window 3"#,
        r#"select-window -t :=3"#,
    ),
    (
        r#"4"#,
        false,
        r#"Select window 4"#,
        r#"select-window -t :=4"#,
    ),
    (
        r#"5"#,
        false,
        r#"Select window 5"#,
        r#"select-window -t :=5"#,
    ),
    (
        r#"6"#,
        false,
        r#"Select window 6"#,
        r#"select-window -t :=6"#,
    ),
    (
        r#"7"#,
        false,
        r#"Select window 7"#,
        r#"select-window -t :=7"#,
    ),
    (
        r#"8"#,
        false,
        r#"Select window 8"#,
        r#"select-window -t :=8"#,
    ),
    (
        r#"9"#,
        false,
        r#"Select window 9"#,
        r#"select-window -t :=9"#,
    ),
    (
        r#":"#,
        false,
        r#"Prompt for a command"#,
        r#"command-prompt"#,
    ),
    (
        r#";"#,
        false,
        r#"Move to the previously active pane"#,
        r#"last-pane"#,
    ),
    (
        r#"<"#,
        false,
        r#"Display window menu"#,
        r##"display-menu -T "#[align=centre]#{window_index}:#{window_name}" -x W -y W "#{?#{>:#{session_windows},1},,-}Swap Left" l { swap-window -t :-1 } "#{?#{>:#{session_windows},1},,-}Swap Right" r { swap-window -t :+1 } "#{?pane_marked_set,,-}Swap Marked" s { swap-window } '' Kill X { kill-window } Respawn R { respawn-window -k } "#{?pane_marked,Unmark,Mark}" m { select-pane -m } Rename n { command-prompt -F -I "#W" { rename-window -t "#{window_id}" "%%" } } '' "New After" w { new-window -a } "New At End" W { new-window }"##,
    ),
    (
        r#"="#,
        false,
        r#"Choose a paste buffer from a list"#,
        r#"choose-buffer -Z"#,
    ),
    (
        r#">"#,
        false,
        r#"Display pane menu"#,
        r##"display-menu -T "#[align=centre]#{pane_index} (#{pane_id})" -x P -y P "#{?#{m/r:(copy|view)-mode,#{pane_mode}},Go To Top,}" < { send-keys -X history-top } "#{?#{m/r:(copy|view)-mode,#{pane_mode}},Go To Bottom,}" > { send-keys -X history-bottom } '' "#{?#{&&:#{buffer_size},#{!:#{pane_in_mode}}},Paste #[underscore]#{=/9/...:buffer_sample},}" p { paste-buffer } '' "#{?mouse_word,Search For #[underscore]#{=/9/...:mouse_word},}" C-r { if-shell -F "#{?#{m/r:(copy|view)-mode,#{pane_mode}},0,1}" "copy-mode -t=" ; send-keys -X -t = search-backward -- "#{q:mouse_word}" } "#{?mouse_word,Type #[underscore]#{=/9/...:mouse_word},}" C-y { copy-mode -q ; send-keys -l "#{q:mouse_word}" } "#{?mouse_word,Copy #[underscore]#{=/9/...:mouse_word},}" c { copy-mode -q ; set-buffer "#{q:mouse_word}" } "#{?mouse_line,Copy Line,}" l { copy-mode -q ; set-buffer "#{q:mouse_line}" } '' "#{?mouse_hyperlink,Type #[underscore]#{=/9/...:mouse_hyperlink},}" C-h { copy-mode -q ; send-keys -l "#{q:mouse_hyperlink}" } "#{?mouse_hyperlink,Copy #[underscore]#{=/9/...:mouse_hyperlink},}" h { copy-mode -q ; set-buffer "#{q:mouse_hyperlink}" } '' "#{?#{!:#{pane_floating_flag}},Horizontal Split,}" h { split-window -h } "#{?#{!:#{pane_floating_flag}},Vertical Split,}" v { split-window -v } '' "#{?#{&&:#{!:#{pane_floating_flag}},#{>:#{window_panes},1}},Swap Up,}" u { swap-pane -U } "#{?#{&&:#{!:#{pane_floating_flag}},#{>:#{window_panes},1}},Swap Down,}" d { swap-pane -D } "#{?pane_marked_set,,-}Swap Marked" s { swap-pane } '' Kill X { kill-pane } Respawn R { respawn-pane -k } "#{?pane_marked,Unmark,Mark}" m { select-pane -m } "#{?#{>:#{window_panes},1},,-}#{?window_zoomed_flag,Unzoom,Zoom}" z { resize-pane -Z }"##,
    ),
    (r#"?"#, false, r#"List key bindings"#, r#"list-keys -N"#),
    (
        r#"C"#,
        false,
        r#"Customize options"#,
        r#"customize-mode -Z"#,
    ),
    (
        r#"D"#,
        false,
        r#"Choose and detach a client from a list"#,
        r#"choose-client -Z"#,
    ),
    (
        r#"E"#,
        false,
        r#"Spread panes out evenly"#,
        r#"select-layout -E"#,
    ),
    (
        r#"L"#,
        false,
        r#"Switch to the last client"#,
        r#"switch-client -l"#,
    ),
    (
        r#"M"#,
        false,
        r#"Clear the marked pane"#,
        r#"select-pane -M"#,
    ),
    (r#"["#, false, r#"Enter copy mode"#, r#"copy-mode"#),
    (
        r#"]"#,
        false,
        r#"Paste the most recent paste buffer"#,
        r#"paste-buffer -p"#,
    ),
    (r#"c"#, false, r#"Create a new window"#, r#"new-window"#),
    (
        r#"d"#,
        false,
        r#"Detach the current client"#,
        r#"detach-client"#,
    ),
    (
        r#"f"#,
        false,
        r#"Search for a pane"#,
        r#"command-prompt { find-window -Z "%%" }"#,
    ),
    (
        r#"i"#,
        false,
        r#"Display window information"#,
        r#"display-message"#,
    ),
    (
        r#"l"#,
        false,
        r#"Select the previously current window"#,
        r#"last-window"#,
    ),
    (
        r#"m"#,
        false,
        r#"Toggle the marked pane"#,
        r#"select-pane -m"#,
    ),
    (r#"n"#, false, r#"Select the next window"#, r#"next-window"#),
    (
        r#"o"#,
        false,
        r#"Select the next pane"#,
        r#"select-pane -t :.+"#,
    ),
    (
        r#"p"#,
        false,
        r#"Select the previous window"#,
        r#"previous-window"#,
    ),
    (r#"q"#, false, r#"Display pane numbers"#, r#"display-panes"#),
    (
        r#"r"#,
        false,
        r#"Redraw the current client"#,
        r#"refresh-client"#,
    ),
    (
        r#"s"#,
        false,
        r#"Choose a session from a list"#,
        r#"choose-tree -Zs"#,
    ),
    (r#"t"#, false, r#"Show a clock"#, r#"clock-mode"#),
    (
        r#"w"#,
        false,
        r#"Choose a window from a list"#,
        r#"choose-tree -Zw"#,
    ),
    (
        r#"x"#,
        false,
        r#"Kill the active pane"#,
        r#"confirm-before -p "kill-pane #P? (y/n)" kill-pane"#,
    ),
    (
        r#"z"#,
        false,
        r#"Zoom the active pane"#,
        r#"resize-pane -Z"#,
    ),
    (
        r#"{"#,
        false,
        r#"Swap the active pane with the pane above"#,
        r#"swap-pane -U"#,
    ),
    (
        r#"}"#,
        false,
        r#"Swap the active pane with the pane below"#,
        r#"swap-pane -D"#,
    ),
    (r#"~"#, false, r#"Show messages"#, r#"show-messages"#),
    (
        r#"DC"#,
        true,
        r#"Reset so the visible part of the window follows the cursor"#,
        r#"refresh-client -c"#,
    ),
    (
        r#"PPage"#,
        false,
        r#"Enter copy mode and scroll up"#,
        r#"copy-mode -u"#,
    ),
    (
        r#"Up"#,
        true,
        r#"Select the pane above the active pane"#,
        r#"select-pane -U"#,
    ),
    (
        r#"Down"#,
        true,
        r#"Select the pane below the active pane"#,
        r#"select-pane -D"#,
    ),
    (
        r#"Left"#,
        true,
        r#"Select the pane to the left of the active pane"#,
        r#"select-pane -L"#,
    ),
    (
        r#"Right"#,
        true,
        r#"Select the pane to the right of the active pane"#,
        r#"select-pane -R"#,
    ),
    (
        r#"M-1"#,
        false,
        r#"Set the even-horizontal layout"#,
        r#"select-layout even-horizontal"#,
    ),
    (
        r#"M-2"#,
        false,
        r#"Set the even-vertical layout"#,
        r#"select-layout even-vertical"#,
    ),
    (
        r#"M-3"#,
        false,
        r#"Set the main-horizontal layout"#,
        r#"select-layout main-horizontal"#,
    ),
    (
        r#"M-4"#,
        false,
        r#"Set the main-vertical layout"#,
        r#"select-layout main-vertical"#,
    ),
    (
        r#"M-5"#,
        false,
        r#"Select the tiled layout"#,
        r#"select-layout tiled"#,
    ),
    (
        r#"M-6"#,
        false,
        r#"Set the main-horizontal-mirrored layout"#,
        r#"select-layout main-horizontal-mirrored"#,
    ),
    (
        r#"M-7"#,
        false,
        r#"Set the main-vertical-mirrored layout"#,
        r#"select-layout main-vertical-mirrored"#,
    ),
    (
        r#"M-n"#,
        false,
        r#"Select the next window with an alert"#,
        r#"next-window -a"#,
    ),
    (
        r#"M-o"#,
        false,
        r#"Rotate through the panes in reverse"#,
        r#"rotate-window -D"#,
    ),
    (
        r#"M-p"#,
        false,
        r#"Select the previous window with an alert"#,
        r#"previous-window -a"#,
    ),
    (
        r#"M-Up"#,
        true,
        r#"Resize the pane up by 5"#,
        r#"resize-pane -U 5"#,
    ),
    (
        r#"M-Down"#,
        true,
        r#"Resize the pane down by 5"#,
        r#"resize-pane -D 5"#,
    ),
    (
        r#"M-Left"#,
        true,
        r#"Resize the pane left by 5"#,
        r#"resize-pane -L 5"#,
    ),
    (
        r#"M-Right"#,
        true,
        r#"Resize the pane right by 5"#,
        r#"resize-pane -R 5"#,
    ),
    (r#"C-b"#, false, r#"Send the prefix key"#, r#"send-prefix"#),
    (
        r#"C-o"#,
        false,
        r#"Rotate through the panes"#,
        r#"rotate-window"#,
    ),
    (
        r#"C-z"#,
        false,
        r#"Suspend the current client"#,
        r#"suspend-client"#,
    ),
    (
        r#"C-Up"#,
        true,
        r#"Resize the pane up"#,
        r#"resize-pane -U"#,
    ),
    (
        r#"C-Down"#,
        true,
        r#"Resize the pane down"#,
        r#"resize-pane -D"#,
    ),
    (
        r#"C-Left"#,
        true,
        r#"Resize the pane left"#,
        r#"resize-pane -L"#,
    ),
    (
        r#"C-Right"#,
        true,
        r#"Resize the pane right"#,
        r#"resize-pane -R"#,
    ),
    (
        r#"S-Up"#,
        true,
        r#"Move the visible part of the window up"#,
        r#"refresh-client -U 10"#,
    ),
    (
        r#"S-Down"#,
        true,
        r#"Move the visible part of the window down"#,
        r#"refresh-client -D 10"#,
    ),
    (
        r#"S-Left"#,
        true,
        r#"Move the visible part of the window left"#,
        r#"refresh-client -L 10"#,
    ),
    (
        r#"S-Right"#,
        true,
        r#"Move the visible part of the window right"#,
        r#"refresh-client -R 10"#,
    ),
];
