//! The command table (WS10): one row per verb with its alias, flag spec,
//! positional bounds and a builder from the parsed flags to the
//! [`Command`]. The generic part (tokenize, arity, tmux-worded errors) is
//! [`super::parse::parse`]; a verb with real parsing logic has a
//! hand-written builder in `parse.rs` that its row names.

use super::parse::{self, Args};
use super::*;

/// "No upper bound" for [`Row::max`].
pub(super) const ANY: usize = usize::MAX;

pub(super) struct Row {
    pub name: &'static str,
    /// tmux's short name for the verb, or "".
    pub alias: &'static str,
    /// Short flags: a letter, then `:` when it takes a value (`"t:aCg"`).
    pub flags: &'static str,
    /// `--long` flags (none takes a value).
    pub long: &'static [&'static str],
    /// Positionals the verb needs when it is given any argument at all (a
    /// bare verb parses and its handler refuses it).
    pub min: usize,
    pub max: usize,
    /// The builder reads `argv` itself: no flag scan.
    pub raw: bool,
    pub build: fn(&Args) -> Result<Command, ParseError>,
}

const fn row(
    name: &'static str,
    alias: &'static str,
    flags: &'static str,
    max: usize,
    build: fn(&Args) -> Result<Command, ParseError>,
) -> Row {
    Row {
        name,
        alias,
        flags,
        long: &[],
        min: 0,
        max,
        raw: false,
        build,
    }
}

impl Row {
    const fn long(mut self, long: &'static [&'static str]) -> Row {
        self.long = long;
        self
    }

    const fn min(mut self, min: usize) -> Row {
        self.min = min;
        self
    }

    const fn raw(mut self) -> Row {
        self.raw = true;
        self
    }
}

/// The verb `name` (or its alias) names.
pub(super) fn find(name: &str) -> Option<&'static Row> {
    ROWS.iter()
        .find(|r| r.name == name || (!r.alias.is_empty() && r.alias == name))
}

/// Resolves a tmux short name to the verb's full name. `set`/`show` are
/// load-bearing (spec §4.5). Public for `confirm-before`'s prompt, which
/// names the command (TC.7).
pub fn canonical_name(name: &str) -> &str {
    find(name).map_or(name, |r| r.name)
}

pub(super) static ROWS: &[Row] = &[
    row("has-session", "", "t:", 0, |f| {
        Ok(Command::HasSession(HasSession {
            target: f.target('t')?,
        }))
    }),
    row("kill-server", "", "", 0, |_| {
        Ok(Command::KillServer(KillServer))
    }),
    row("list-plugins", "", "", 0, |_| {
        Ok(Command::ListPlugins(ListPlugins))
    }),
    row("reload", "", "", 0, |_| Ok(Command::Reload(Reload))),
    row("config-file", "", "", 0, |_| Ok(Command::ConfigFile)),
    row("choose-agent", "", "", 0, |_| Ok(Command::ChooseAgent)),
    row("agent-grid", "", "", 0, |_| Ok(Command::AgentGrid)),
    row("choose-notification", "", "", 0, |_| {
        Ok(Command::ChooseNotification)
    }),
    row("list-scratchpads", "", "", 0, |_| {
        Ok(Command::ListScratchpads)
    }),
    row("choose-scratchpad", "", "", 0, |_| {
        Ok(Command::ChooseScratchpad)
    }),
    row("clock-mode", "", "t:", 0, |f| {
        Ok(Command::ClockMode(ClockMode {
            target: f.target('t')?,
        }))
    }),
    row("next-layout", "nextl", "t:", 0, |f| {
        Ok(Command::NextLayout(NextLayout {
            target: f.target('t')?,
        }))
    }),
    row("previous-layout", "prevl", "t:", 0, |f| {
        Ok(Command::PreviousLayout(NextLayout {
            target: f.target('t')?,
        }))
    }),
    row("toggle-scratchpad", "", "t:", 0, |f| {
        Ok(Command::ToggleScratchpad(ToggleScratchpad {
            target: f.target('t')?,
        }))
    }),
    row("lock-server", "lock", "", 0, |_| {
        Ok(Command::Lock(Lock {
            scope: LockScope::Server,
        }))
    }),
    row("lock-session", "locks", "t:", 0, |f| {
        Ok(Command::Lock(Lock {
            scope: LockScope::Session(f.target('t')?),
        }))
    }),
    row("lock-client", "lockc", "t:", 0, |f| {
        Ok(Command::Lock(Lock {
            scope: LockScope::Client(f.val('t')),
        }))
    }),
    row("show-prompt-history", "showphist", "T:", 0, |f| {
        Ok(Command::PromptHistory(PromptHistory {
            clear: false,
            kind: f.val('T'),
        }))
    }),
    row("clear-prompt-history", "clearphist", "T:", 0, |f| {
        Ok(Command::PromptHistory(PromptHistory {
            clear: true,
            kind: f.val('T'),
        }))
    }),
    row("key-menu", "", "T:", 0, |f| {
        Ok(Command::KeyMenu(KeyMenu { table: f.val('T') }))
    }),
    row("kill-session", "", "t:aCg", 0, |f| {
        Ok(Command::KillSession(KillSession {
            target: f.target('t')?,
            others: f.on('a'),
            clear_alerts: f.on('C'),
            group: f.on('g'),
        }))
    }),
    row("kill-window", "killw", "t:a", 0, |f| {
        Ok(Command::KillWindow(KillWindow {
            target: f.target('t')?,
            all: f.on('a'),
        }))
    }),
    row("kill-pane", "killp", "t:a", 0, |f| {
        Ok(Command::KillPane(KillPane {
            target: f.target('t')?,
            others: f.on('a'),
        }))
    }),
    row("last-pane", "", "t:Zde", 0, |f| {
        Ok(Command::LastPane(LastPane {
            target: f.target('t')?,
            keep_zoom: f.on('Z'),
            input: parse::input_flag(f),
        }))
    }),
    row(
        "select-window",
        "selectw",
        "t:lnpT",
        0,
        parse::select_window,
    ),
    row("list-sessions", "", "F:f:O:r", 0, |f| {
        Ok(Command::ListSessions(ListSessions {
            format: f.val('F'),
            filter: f.val('f'),
            sort: parse::sort_flags(f),
        }))
    }),
    row("list-windows", "lsw", "t:F:af:O:r", 0, |f| {
        Ok(Command::ListWindows(ListWindows {
            target: f.target('t')?,
            format: f.val('F'),
            all: f.on('a'),
            filter: f.val('f'),
            sort: parse::sort_flags(f),
        }))
    }),
    row("list-panes", "lsp", "t:F:asf:O:r", 0, |f| {
        Ok(Command::ListPanes(ListPanes {
            target: f.target('t')?,
            format: f.val('F'),
            all: f.on('a'),
            session: f.on('s'),
            filter: f.val('f'),
            sort: parse::sort_flags(f),
        }))
    }),
    row(
        "new-session",
        "new",
        "AdPF:n:s:c:x:y:t:e:DXEf:",
        ANY,
        parse::parse_new_session,
    ),
    row(
        "new-window",
        "neww",
        "daPF:t:n:c:e:bkS",
        ANY,
        parse::parse_new_window,
    ),
    row(
        "split-window",
        "splitw",
        "hbfdPF:vc:p:l:t:e:Zs:S:R:km:",
        ANY,
        parse::parse_split_window,
    ),
    row(
        "select-pane",
        "selectp",
        "T:LlZRUDdemMt:",
        0,
        parse::parse_select_pane,
    ),
    row(
        "resize-pane",
        "resizep",
        "LRUDZt:x:y:",
        1,
        parse::parse_resize_pane,
    ),
    row(
        "send-keys",
        "send",
        "lHt:XN:FRKc:M",
        ANY,
        parse::parse_send_keys,
    ),
    row(
        "capture-pane",
        "capturep",
        "peJNTqaCFHLMPS:E:t:b:",
        0,
        parse::parse_capture_pane,
    ),
    row(
        "clear-history",
        "clearhist",
        "Ht:",
        0,
        parse::parse_clear_history,
    ),
    row(
        "set-option",
        "set",
        "gswput:aFqoU",
        ANY,
        parse::parse_set_option,
    )
    .long(&["persist", "no-persist"]),
    row(
        "show-options",
        "show",
        "gswpt:vqAH",
        ANY,
        parse::parse_show_options,
    ),
    row(
        "display-message",
        "display",
        "palvt:c:d:F:NC",
        ANY,
        parse::parse_display_message,
    ),
    row("subscribe", "", "t:c:", ANY, parse::parse_subscribe),
    row(
        "attach-session",
        "attach",
        "t:dc:Erxf:",
        0,
        parse::parse_attach_session,
    )
    .long(&["nest"]),
    row("detach-client", "", "s:t:aP", 0, parse::parse_detach_client),
    row(
        "list-clients",
        "",
        "F:f:t:O:r",
        0,
        parse::parse_list_clients,
    ),
    row(
        "switch-client",
        "",
        "t:lnpT:c:ZErO:F",
        0,
        parse::parse_switch_client,
    ),
    row("send-prefix", "", "t:2", 0, parse::parse_send_prefix),
    row("bind-key", "bind", "nrT:N:G:", ANY, parse::parse_bind_key),
    row("unbind-key", "unbind", "naqT:", 1, parse::parse_unbind_key),
    row(
        "list-keys",
        "lsk",
        "T:1NaF:P:O:r",
        1,
        parse::parse_list_keys,
    ),
    row(
        "source-file",
        "source",
        "t:qFnv",
        ANY,
        parse::parse_source_file,
    ),
    row("plugin-restart", "", "", 1, parse::parse_plugin_restart),
    row(
        "command-prompt",
        "",
        "p:I:bk1T:NiFeClt:",
        1,
        parse::parse_command_prompt,
    ),
    row(
        "run-shell",
        "run",
        "bd:t:CEc:s:",
        ANY,
        parse::parse_run_shell,
    ),
    row(
        "select-layout",
        "selectl",
        "t:Enop",
        1,
        parse::parse_select_layout,
    ),
    row("swap-pane", "", "s:t:UDdZ", 0, parse::parse_swap_pane),
    row("swap-window", "", "ds:t:", 0, parse::parse_swap_window),
    row("break-pane", "", "dabPF:n:s:t:", 0, parse::parse_break_pane),
    row(
        "resize-window",
        "",
        "t:x:y:AaLRUD",
        1,
        parse::parse_resize_window,
    ),
    row("pipe-pane", "", "IOot:", 1, parse::parse_pipe_pane),
    row("rotate-window", "", "t:UDZ", 0, parse::parse_rotate_window),
    row("move-window", "", "s:t:abdkr", 0, parse::parse_move_window),
    row("link-window", "", "s:t:abdk", 0, parse::parse_link_window),
    row("unlink-window", "", "t:k", 0, parse::parse_unlink_window),
    row("server-access", "", "adlrw", 1, parse::parse_server_access),
    row(
        "refresh-client",
        "",
        "t:ScLRUDF:f:lr:A:B:C:",
        1,
        parse::parse_refresh_client,
    ),
    row("show-messages", "", "t:JT", 0, parse::parse_show_messages),
    row(
        "choose-client",
        "",
        "F:f:K:NO:rt:yZ",
        1,
        parse::parse_choose_client,
    ),
    row("suspend-client", "", "t:", 0, parse::parse_suspend_client),
    row("find-window", "", "t:ZCNTri", 1, parse::parse_find_window).min(1),
    row(
        "choose-tree",
        "",
        "F:f:GK:NO:rst:wyZ",
        1,
        parse::parse_choose_tree,
    ),
    row("wait-for", "", "SLU", 1, parse::parse_wait_for).min(1),
    row(
        "display-popup",
        "",
        "EBb:c:d:e:T:w:h:x:y:s:S:t:CkN",
        ANY,
        parse::parse_display_popup,
    ),
    row(
        "new-pane",
        "",
        "PF:t:c:x:y:X:Y:e:ZdLbfhvl:p:s:S:R:km:",
        ANY,
        parse::parse_new_pane,
    ),
    row("float-pane", "", "t:x:y:X:Y:", 0, parse::parse_float_pane),
    row("unfloat-pane", "", "t:s:hv", 0, parse::parse_unfloat_pane),
    row(
        "new-scratchpad",
        "",
        "t:c:",
        ANY,
        parse::parse_new_scratchpad,
    ),
    row("show-scratchpad", "", "t:", 1, parse::parse_show_scratchpad),
    row("open-url", "", "", 1, parse::parse_open_url).min(1),
    row("list-ports", "", "t:a", 0, |f| {
        Ok(Command::ListPorts(ListPorts {
            target: f.target('t')?,
            all: f.on('a'),
        }))
    }),
    row("set-client-info", "", "t:", 2, parse::parse_set_client_info).min(2),
    row("drag-pane", "", "M", 0, parse::parse_drag_pane),
    row("place-pane", "", "t:X:Y:", 0, parse::parse_place_pane),
    row(
        "display-menu",
        "",
        "T:x:y:t:c:b:s:H:S:C:OM",
        ANY,
        parse::parse_display_menu,
    )
    .min(1),
    row("display-panes", "", "bd:Nt:", 1, parse::parse_display_panes),
    row(
        "confirm-before",
        "",
        "bc:p:t:y",
        1,
        parse::parse_confirm_before,
    )
    .min(1),
    row("if-shell", "", "Fbt:", 3, parse::parse_if_shell).min(2),
    row("set-hook", "", "guapwRt:", 2, parse::parse_set_hook).min(1),
    row("show-hooks", "", "gwpt:", 1, parse::parse_show_hooks),
    row(
        "set-environment",
        "",
        "guhrFt:",
        2,
        parse::parse_set_environment,
    )
    .min(1),
    row(
        "show-environment",
        "",
        "ghst:",
        1,
        parse::parse_show_environment,
    ),
    row(
        "__register-source",
        "",
        "n:",
        1,
        parse::parse_register_source,
    ),
    row("set-buffer", "", "ab:n:t:w", 1, parse::parse_set_buffer),
    row(
        "paste-buffer",
        "",
        "dprSb:s:t:",
        0,
        parse::parse_paste_buffer,
    ),
    row("respawn-pane", "respawnp", "kt:c:e:", ANY, |f| {
        Ok(Command::RespawnPane(parse::respawn(f)?))
    }),
    row("respawn-window", "", "kt:c:e:", ANY, |f| {
        Ok(Command::RespawnWindow(parse::respawn(f)?))
    }),
    row("next-window", "", "t:a", 0, |f| {
        Ok(Command::NextWindow(parse::window_step(f)?))
    }),
    row("previous-window", "", "t:a", 0, |f| {
        Ok(Command::PreviousWindow(parse::window_step(f)?))
    }),
    row("last-window", "", "t:", 0, |f| {
        Ok(Command::LastWindow(WindowStep {
            target: f.target('t')?,
            alert: false,
        }))
    }),
    row("rename-session", "", "t:", 1, |f| {
        Ok(Command::RenameSession(parse::rename(f)?))
    }),
    row("rename-window", "", "t:", 1, |f| {
        Ok(Command::RenameWindow(parse::rename(f)?))
    }),
    row("pane-set-resume", "", "", ANY, parse::pane_set_resume).raw(),
    row("agent", "", "", ANY, parse::agent).raw(),
    row("notify", "", "", ANY, parse::notify).raw(),
    row("lua", "", "", ANY, parse::lua).raw(),
    row("lua-call", "", "", ANY, parse::lua_call).raw(),
    row("reboot-check", "", "", ANY, parse::reboot_check).long(&["json"]),
    row("join-pane", "", "bdfhvl:p:s:t:", 0, |f| {
        Ok(Command::JoinPane(parse::join_pane(f)?))
    }),
    row("move-pane", "", "bdfhvl:p:s:t:", 0, |f| {
        Ok(Command::MovePane(parse::join_pane(f)?))
    }),
    row("show-buffer", "", "b:", 0, |f| {
        Ok(Command::ShowBuffer(ShowBuffer { name: f.val('b') }))
    }),
    row("load-buffer", "", "b:t:w", 1, |f| {
        Ok(Command::LoadBuffer(LoadBuffer {
            name: f.val('b'),
            client: f.val('t'),
            clipboard: f.on('w'),
            path: f.first().ok_or_else(|| parse::missing(f))?,
        }))
    }),
    row("save-buffer", "", "ab:", 1, |f| {
        Ok(Command::SaveBuffer(SaveBuffer {
            append: f.on('a'),
            name: f.val('b'),
            path: f.first().ok_or_else(|| parse::missing(f))?,
        }))
    }),
    row("list-buffers", "", "F:f:O:r", 0, |f| {
        Ok(Command::ListBuffers(ListBuffers {
            format: f.val('F'),
            filter: f.val('f'),
            sort: parse::sort_flags(f),
        }))
    }),
    row("delete-buffer", "", "b:", 0, |f| {
        Ok(Command::DeleteBuffer(DeleteBuffer { name: f.val('b') }))
    }),
    row("copy-mode", "", "ehHMudqSs:t:", 0, |f| {
        Ok(Command::CopyMode(CopyMode {
            page_up: f.on('u'),
            scroll_exit: f.on('e'),
            hide_position: f.on('H'),
            hints: f.on('h'),
            mouse: f.on('M'),
            cancel: f.on('q'),
            page_down: f.on('d'),
            slider: f.on('S'),
            source: f.target_opt('s')?,
            target: f.target('t')?,
        }))
    }),
    row("customize-mode", "", "F:f:Nt:yZ", 0, |f| {
        Ok(Command::CustomizeMode(CustomizeMode {
            zoom: f.on('Z'),
            opts: parse::choose_opts(f)?,
        }))
    }),
    row("choose-buffer", "", "F:f:K:NO:rt:yZ", 1, |f| {
        Ok(Command::ChooseBuffer(ChooseBuffer {
            zoom: f.on('Z'),
            opts: parse::choose_opts(f)?,
        }))
    }),
    row("agent-sidebar", "", "", 1, parse::agent_sidebar),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::tmux::specs;

    /// Flags tmux's usage leaves out but its args template takes, or that
    /// grofe adds (`bind-key -G`).
    const BEYOND_USAGE: &[(&str, char)] = &[
        ("bind-key", 'G'),
        ("copy-mode", 'h'),
        ("select-pane", 'P'),
        ("select-pane", 'g'),
        ("join-pane", 'p'),
        ("move-pane", 'p'),
        ("refresh-client", 'F'),
        ("run-shell", 's'),
        ("load-buffer", 'w'),
        ("choose-buffer", 'y'),
        ("choose-client", 'y'),
        ("choose-tree", 'y'),
        ("customize-mode", 'y'),
        ("list-buffers", 'r'),
        ("list-clients", 'r'),
        ("new-pane", 'E'),
        ("new-pane", 'L'),
        ("split-window", 'E'),
        ("switch-client", 'F'),
    ];

    /// The flags of a tmux usage line: `[-dErx]` and `[-L|-S|-U]` are bool
    /// flags, `[-c working-directory]` takes a value.
    fn usage_flags(usage: &str) -> Vec<(char, bool)> {
        let mut flags = Vec::new();
        for part in usage.split("[-").skip(1) {
            let group = part.split([']', ' ']).next().unwrap_or("");
            let takes_value = part[group.len()..].starts_with(' ');
            let letters: Vec<char> = group.chars().filter(|c| c.is_alphanumeric()).collect();
            let last = letters.len().saturating_sub(1);
            for (i, c) in letters.into_iter().enumerate() {
                flags.push((c, takes_value && i == last));
            }
        }
        flags
    }

    fn row_flags(row: &Row) -> Vec<(char, bool)> {
        let mut flags: Vec<(char, bool)> = Vec::new();
        let mut chars = row.flags.chars().peekable();
        while let Some(c) = chars.next() {
            flags.push((c, chars.next_if_eq(&':').is_some()));
        }
        flags
    }

    #[test]
    fn every_row_matches_tmux_list_commands() {
        let mut checked = 0;
        for row in ROWS {
            let Some(spec) = specs().iter().find(|s| s.name == row.name) else {
                continue;
            };
            checked += 1;
            if !row.alias.is_empty() {
                assert_eq!(spec.alias, Some(row.alias), "{} alias", row.name);
            }
            let listed = usage_flags(spec.usage);
            for (flag, takes_value) in row_flags(row) {
                if BEYOND_USAGE.contains(&(row.name, flag)) {
                    continue;
                }
                // tmux can list a flag twice (`split-window -l` is in the
                // bool group and `[-l size]`).
                assert!(
                    listed.contains(&(flag, takes_value)),
                    "{} -{flag} (value: {takes_value}) is not in tmux's `{}`",
                    row.name,
                    spec.usage
                );
            }
            if row.min > 0 {
                assert_eq!(row.min, spec.min_args(), "{} min", row.name);
            }
        }
        assert!(checked > 60, "only {checked} rows are tmux verbs");
    }

    #[test]
    fn names_and_aliases_are_unique() {
        for (i, a) in ROWS.iter().enumerate() {
            for b in &ROWS[i + 1..] {
                assert_ne!(a.name, b.name);
                assert!(a.alias.is_empty() || a.alias != b.alias && a.alias != b.name);
            }
        }
    }

    #[test]
    fn canonical_name_reads_the_table() {
        assert_eq!(canonical_name("neww"), "new-window");
        assert_eq!(canonical_name("attach"), "attach-session");
        assert_eq!(canonical_name("new"), "new-session");
        assert_eq!(canonical_name("new-window"), "new-window");
        assert_eq!(canonical_name("no-such-verb"), "no-such-verb");
    }
}
