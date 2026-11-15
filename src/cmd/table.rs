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
    /// bare M3.6 verb parses and its handler refuses it).
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
];
