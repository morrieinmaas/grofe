//! Format, option, status and hook rows (FE.3): tmux format strings
//! rewritten into grofe's, `#()` to status sources, hook names onto grofe
//! events, the environment verbs.
//!
//! Rows and rewrites only: the schema, the dispatcher and the helpers a
//! translation uses ([`super::Invocation`]) are `mod.rs`'s. The most
//! specific matching row decides, and between rows as specific this
//! file's come before the core rows, so a row here naming a core form
//! takes it over. Rewrites ([`super::Rewrite`]) apply before any row.
//!
//! **Formats.** [`to_native`] rewrites a tmux 3.7b format into grofe's
//! format language (spec §4.5's Formats row, D22); grofe's own expander
//! (`cmd::format::expand`) then runs on the result, so nothing here
//! expands a format. tmux's syntax and grofe's differ in these places
//! (each recorded on tmux 3.7b):
//!
//! - the short forms `#D #F #H #I #P #S #T #W #h` become `#{pane_id}` and
//!   the rest; any other `#x` stays `#x`, as tmux prints it;
//! - `##` is `#` in both, and `##{` a literal `#{`;
//! - `#,` is a literal comma and `#}` a literal brace; outside a `#{...}`
//!   they become the plain character, inside one `#,` becomes a nested
//!   format that expands to a comma, so grofe's expander does not split on
//!   it;
//! - `#{l:text}` is `text`, unexpanded;
//! - a trailing `#` is dropped, and an unterminated `#{` or `#(` drops
//!   the rest of the format;
//! - `#(cmd)` in an option the status line reads becomes `#{@src-<hash>}`,
//!   the option a status source for `cmd` keeps its last output line in
//!   (D8, TC.4), registered by a `__register-source` run first. Anywhere
//!   else (`display -p`, `list-* -F`, `if -F`, `run-shell`) it is empty,
//!   as tmux's CLI prints it: a job tmux starts for a CLI client is never
//!   read back.
//!
//! tmux's other modifiers (`E:`, `T:`, `q:`, `=N:`, `s/a/b/:`, `n:`, `w:`,
//! the loops and the rest) are grofe's own too, and pass through as they are
//! written: the expander follows tmux's `format_build_modifiers`.
//!
//! **Where formats are.** Every argument tmux expands as a format gets
//! [`to_native`]: `display-message`'s message and `-F`; `list-*`'s `-F`
//! and `-f`; `-P -F` of the creating verbs; `-c`/`-d` start directories;
//! `if-shell`'s condition and `run-shell`'s command; prompt, popup and
//! menu texts; `select-pane -T`; `rename-window`/`rename-session`; and
//! the value of `set` for the options tmux reads as formats, or for any
//! option with `-F`. `set` keeps the value as sent (`SetOption::shown`),
//! which a tmux-dialect `show` prints; with no option name, `show` names
//! tmux's table (session, window or server), listed from
//! `fe_3/tmux-tables.golden`.

use std::fmt::Write as _;

use super::{
    Arg, FlagMark, GrofeTarget, Invocation, MappingKind, Rewrite, Rewritten, Row, SEMANTIC,
    TmuxError, row,
};
use crate::cmd::{Command, RegisterSource};

// ---- Format rewriting ------------------------------------------------------

/// What a `#(cmd)` becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Jobs {
    /// A status source's option, registered first: an option value the
    /// status line reads.
    Sources,
    /// Nothing: a format expanded once, for a CLI reply or a condition.
    Empty,
}

/// tmux 3.7b's one-letter short forms (`format_upper`, `format_lower`).
const SHORT_FORMS: &[(char, &str)] = &[
    ('D', "pane_id"),
    ('F', "window_flags"),
    ('H', "host"),
    ('I', "window_index"),
    ('P', "pane_index"),
    ('S', "session_name"),
    ('T', "pane_title"),
    ('W', "window_name"),
    ('h', "host_short"),
];

/// The modifier prefixes grofe's expander knows (TC.3), longest first.
const NATIVE_MODIFIERS: &[&str] = &[
    "?", "==:", "!=:", ">=:", ">:", "<=:", "<:", "&&:", "||:", "!!:", "!:", "m:", "m/", "b:", "t:",
    "q:",
];

/// `fmt`, a tmux format, in grofe's format language, with the status
/// sources its `#(cmd)`s need registered first ([`Rewritten::before`]).
pub fn to_native(fmt: &str, jobs: Jobs) -> Rewritten {
    let mut r = Rewriter {
        jobs,
        before: Vec::new(),
        depth: 0,
    };
    let value = r.convert(fmt, false);
    Rewritten {
        value,
        before: r.before,
    }
}

struct Rewriter {
    jobs: Jobs,
    before: Vec<Command>,
    /// How many `#{` the text being rewritten is inside.
    depth: usize,
}

impl Rewriter {
    /// `s` rewritten; `in_body` when it is (part of) the body of a
    /// `#{...}`, where a bare comma or brace would end a piece.
    fn convert(&mut self, s: &str, in_body: bool) -> String {
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while let Some(ch) = s[i..].chars().next() {
            if ch != '#' {
                out.push(ch);
                i += ch.len_utf8();
                continue;
            }
            // tmux drops a trailing `#`.
            let Some(next) = s[i + 1..].chars().next() else {
                break;
            };
            match next {
                '#' => {
                    out.push_str("##");
                    i += 2;
                }
                ',' => {
                    out.push_str(if in_body { COMMA } else { "," });
                    i += 2;
                }
                '}' => {
                    if in_body {
                        out.push_str(self.rbrace());
                    } else {
                        out.push('}');
                    }
                    i += 2;
                }
                '{' => {
                    // An unterminated `#{` drops the rest, as in tmux.
                    let Some(len) = brace_len(&s[i + 2..]) else {
                        break;
                    };
                    let body = &s[i + 2..i + 2 + len];
                    // tmux never reaches a `#{` this deep (its loop limit
                    // has given nothing a level above), and grofe's
                    // expander stops at the same depth, so its text goes.
                    if self.depth <= crate::cmd::format::LOOP_LIMIT {
                        self.depth += 1;
                        out.push_str(&self.brace(body, in_body));
                        self.depth -= 1;
                    }
                    i += 2 + len + 1;
                }
                '(' => {
                    let Some(len) = job_len(&s[i + 2..]) else {
                        break;
                    };
                    out.push_str(&self.job(&s[i + 2..i + 2 + len]));
                    i += 2 + len + 1;
                }
                c => {
                    match SHORT_FORMS.iter().find(|(f, _)| *f == c) {
                        Some((_, name)) => {
                            let _ = write!(out, "#{{{name}}}");
                        }
                        None => {
                            out.push('#');
                            out.push(c);
                        }
                    }
                    i += 1 + c.len_utf8();
                }
            }
        }
        out
    }

    /// One `#{body}`, rewritten whole.
    fn brace(&mut self, body: &str, in_body: bool) -> String {
        if let Some(text) = body.strip_prefix("l:") {
            return self.literal(text, in_body);
        }
        if let Some(rest) = body.strip_prefix('?') {
            let parts = split_top(rest);
            return self.branches(&parts);
        }
        if let Some(m) = NATIVE_MODIFIERS.iter().find(|m| body.starts_with(**m)) {
            let rest = &body[m.len()..];
            return format!("#{{{m}{}}}", self.convert(rest, true));
        }
        format!("#{{{}}}", self.convert(body, true))
    }

    /// tmux's `#{?c1,v1,c2,v2,...,else}` as nested grofe conditions: the
    /// first true condition's value, else the last part when the count is
    /// odd, else nothing (recorded: `#{?1,a}` and `#{?,,,}` are empty,
    /// `#{?,a,session_name,b,c}` is `b`).
    /// The chain is one native level deep however long it is: grofe's
    /// expander takes an else branch that is a whole `#{?...}` in a loop.
    fn branches(&mut self, parts: &[&str]) -> String {
        let mut out = String::new();
        let (pairs, last) = parts.as_chunks::<2>();
        for [cond, value] in pairs {
            let cond = self.convert(cond, true);
            let value = self.convert(value, true);
            let _ = write!(out, "#{{?{cond},{value},");
        }
        if let [last] = last {
            out.push_str(&self.convert(last, true));
        }
        out.push_str(&"}".repeat(pairs.len()));
        out
    }

    /// `#{l:text}`: `text` with tmux's escapes (`##`, `#,`, `#}`) undone and
    /// nothing else expanded (recorded: `#{l:a##b#,c#}d}` is `a#b,c}d`), as
    /// a grofe format that expands to it.
    fn literal(&mut self, text: &str, in_body: bool) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            let ch = match (ch, chars.peek()) {
                ('#', Some(&n)) if "#,}".contains(n) => {
                    chars.next();
                    n
                }
                _ => ch,
            };
            match ch {
                '#' => out.push_str("##"),
                ',' if in_body => out.push_str(COMMA),
                '}' if in_body => out.push_str(self.rbrace()),
                c => out.push(c),
            }
        }
        out
    }

    /// A `}` inside a `#{...}`, which grofe's language has no escape for:
    /// a user option holding one, set first.
    fn rbrace(&mut self) -> &'static str {
        let set = Command::SetOption(crate::cmd::SetOption {
            scope: Some(crate::cmd::OptionScope::Global),
            unset: false,
            persist: false,
            target: crate::cmd::Target::Current,
            name: RBRACE_OPTION.to_string(),
            value: Some("}".to_string()),
            append: false,
            expand: false,
            quiet: false,
            only_if_unset: false,
            shown: None,
        });
        if !self.before.contains(&set) {
            self.before.push(set);
        }
        RBRACE
    }

    /// A `#(cmd)`: its source's option, or nothing.
    // ponytail: tmux expands `cmd` as a format before it runs it; a source
    // runs it as written.
    fn job(&mut self, cmd: &str) -> String {
        if self.jobs == Jobs::Empty {
            return String::new();
        }
        let key = crate::server::sources::inline_key(cmd);
        let register = Command::RegisterSource(RegisterSource {
            // Named, so it does not expire while an option still reads it
            // (an inline source expires an hour after it was last
            // registered, and nothing registers it again); it stops as
            // soon as no option reads it (`sources::prune_unreferenced`).
            name: Some(key.clone()),
            command: cmd.to_string(),
        });
        if !self.before.contains(&register) {
            self.before.push(register);
        }
        format!("#{{@{key}}}")
    }
}

/// A nested format that expands to a comma: the else branch of a false
/// condition is everything after its second comma.
const COMMA: &str = "#{?,,,}";

/// The user option that holds a `}` for [`Rewriter::rbrace`]; `@src-`
/// options are the front end's own, and tmux's `show` leaves them out.
const RBRACE_OPTION: &str = "@src-rbrace";
const RBRACE: &str = "#{@src-rbrace}";

/// A `#{?...}` body's parts, split at each comma outside a nested `#{`
/// and not escaped (`#,`), as tmux's `format_skip` splits them.
fn split_top(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut parts = Vec::new();
    let (mut depth, mut start, mut i) = (0usize, 0, 0);
    while i < b.len() {
        match b[i] {
            b'#' if b.get(i + 1) == Some(&b'{') => {
                depth += 1;
                i += 2;
            }
            b'#' if b.get(i + 1).is_some_and(|c| b",#}:".contains(c)) => i += 2,
            b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    parts.push(&s[start..]);
    parts
}

/// The length of a `#{...}` body, `s` starting just after its `#{`, as
/// tmux's `format_skip` finds its end: `#{` opens a level, `}` closes one,
/// and `#` escapes a following `,#{}:`. `None` when it never closes.
fn brace_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 1usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'#' if b.get(i + 1) == Some(&b'{') => {
                depth += 1;
                i += 2;
            }
            b'#' if b.get(i + 1).is_some_and(|c| b",#}:".contains(c)) => i += 2,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// The length of a `#(...)` command, `s` starting just after its `#(`:
/// parentheses nest, as tmux counts them. `None` when it never closes.
fn job_len(s: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

// ---- Where formats are -------------------------------------------------------

/// Options tmux reads as formats, by base name (`status-format[N]` is
/// `status-format`).
const FORMAT_OPTIONS: &[&str] = &[
    "automatic-rename-format",
    "pane-border-format",
    "set-titles-string",
    "status-format",
    "status-left",
    "status-right",
    "window-status-current-format",
    "window-status-format",
];

/// An argument tmux expands once, for a reply or a condition.
fn format_arg(_: &Invocation<'_>, value: &str) -> Result<Rewritten, TmuxError> {
    Ok(to_native(value, Jobs::Empty))
}

/// `set`'s value: rewritten for an option tmux reads as a format, for a
/// style that holds a format (tmux expands a style only then, so a
/// `#F5F5F5` colour stays a colour), and for any option with `-F`, which
/// expands it now. A user option's value is text: tmux never expands it
/// when a format reads it.
fn option_value(inv: &Invocation<'_>, value: &str) -> Result<Rewritten, TmuxError> {
    if inv.getopt().has('F') {
        return Ok(to_native(value, Jobs::Empty));
    }
    let Some(name) = inv.option_name() else {
        return Ok(Rewritten::value(value));
    };
    let base = name.split('[').next().unwrap_or(name);
    let style = base.ends_with("-style") && value.contains("#{");
    if FORMAT_OPTIONS.contains(&base) || style {
        Ok(to_native(value, Jobs::Sources))
    } else {
        Ok(Rewritten::value(value))
    }
}

const fn rewrite(verb: &'static str, arg: Arg) -> Rewrite {
    Rewrite {
        verb,
        arg,
        rewrite: format_arg,
    }
}

/// This area's argument rewrites, applied before any row matches.
pub(crate) fn rewrites() -> Vec<Rewrite> {
    use Arg::{Flag, Positional};
    let mut out = vec![
        rewrite("display-message", Positional(0)),
        rewrite("display-message", Flag('F')),
        rewrite("if-shell", Positional(0)),
        rewrite("run-shell", Positional(0)),
        rewrite("list-commands", Flag('F')),
        rewrite("select-pane", Flag('T')),
        rewrite("confirm-before", Flag('p')),
        rewrite("command-prompt", Flag('I')),
        rewrite("command-prompt", Flag('p')),
        rewrite("display-popup", Flag('T')),
        rewrite("display-popup", Flag('d')),
        rewrite("display-menu", Flag('T')),
        rewrite("respawn-pane", Flag('c')),
        rewrite("respawn-window", Flag('c')),
    ];
    for verb in [
        "list-sessions",
        "list-windows",
        "list-panes",
        "list-clients",
    ] {
        out.push(rewrite(verb, Flag('F')));
        out.push(rewrite(verb, Flag('f')));
    }
    for verb in [
        "new-session",
        "new-window",
        "split-window",
        "new-pane",
        "break-pane",
    ] {
        out.push(rewrite(verb, Flag('F')));
        out.push(rewrite(verb, Flag('c')));
    }
    out.push(rewrite("rename-window", Positional(0)));
    out.push(rewrite("rename-session", Positional(0)));
    out
}

/// `set`/`setw`: the option command with its value rewritten
/// ([`option_value`]), the sources that needs registered first, and the
/// value as sent kept for a tmux-dialect `show` (`SetOption::shown`).
fn set_value(inv: &Invocation<'_>, window: bool) -> Result<Vec<Command>, TmuxError> {
    let verb = "set-option";
    let at = inv.getopt().rest.min(inv.args.len()) + 1;
    let Some(sent) = inv.args.get(at) else {
        return super::option_command(inv, verb, window);
    };
    let got = option_value(inv, sent)?;
    let mut args = inv.args.to_vec();
    args[at] = got.value.clone();
    let rewritten = Invocation {
        verb: inv.verb,
        args: &args,
        ctx: inv.ctx,
        spec: inv.spec,
    };
    let mut out = got.before;
    for mut c in super::option_command(&rewritten, verb, window)? {
        // With `-F` the value is expanded, and shows as expanded.
        if let Command::SetOption(set) = &mut c
            && got.value != *sent
            && !set.expand
        {
            set.shown = Some(sent.clone());
        }
        out.push(c);
    }
    Ok(out)
}

fn set_option(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    set_value(inv, false)
}

fn set_window_option(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    set_value(inv, true)
}

/// `show`/`showw` with no option name: tmux's table the flags name
/// (`-s` the server's, `-gw` the global window one, `-g` the session
/// one), which a tmux-dialect `show` lists as tmux does.
fn show_value(inv: &Invocation<'_>, window: bool) -> Result<Vec<Command>, TmuxError> {
    let got = inv.getopt();
    let mut out = super::option_command(inv, "show-options", window)?;
    if inv.option_name().is_none() {
        let scope = if got.has('s') {
            Some(crate::cmd::OptionScope::Server)
        } else if got.has('g') && (window || got.has('w')) {
            Some(crate::cmd::OptionScope::GlobalWindow)
        } else {
            None
        };
        for c in &mut out {
            if let (Command::ShowOptions(show), Some(scope)) = (c, scope) {
                show.scope = Some(scope);
            }
        }
    }
    Ok(out)
}

fn show_options(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    show_value(inv, false)
}

fn show_window_options(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    show_value(inv, true)
}

// ---- Hooks -------------------------------------------------------------------

/// Every hook name tmux 3.7b knows (recorded: `show-hooks -g`, and the
/// window and pane hooks it shows by name).
const TMUX_HOOKS: &[&str] = &[
    "after-bind-key",
    "after-capture-pane",
    "after-copy-mode",
    "after-display-message",
    "after-display-panes",
    "after-kill-pane",
    "after-list-buffers",
    "after-list-clients",
    "after-list-keys",
    "after-list-panes",
    "after-list-sessions",
    "after-list-windows",
    "after-load-buffer",
    "after-lock-server",
    "after-new-session",
    "after-new-window",
    "after-paste-buffer",
    "after-pipe-pane",
    "after-queue",
    "after-refresh-client",
    "after-rename-session",
    "after-rename-window",
    "after-resize-pane",
    "after-resize-window",
    "after-save-buffer",
    "after-select-layout",
    "after-select-pane",
    "after-select-window",
    "after-send-keys",
    "after-set-buffer",
    "after-set-environment",
    "after-set-hook",
    "after-set-option",
    "after-show-environment",
    "after-show-messages",
    "after-show-options",
    "after-split-window",
    "after-unbind-key",
    "alert-activity",
    "alert-bell",
    "alert-silence",
    "client-active",
    "client-attached",
    "client-detached",
    "client-focus-in",
    "client-focus-out",
    "client-resized",
    "client-session-changed",
    "client-light-theme",
    "client-dark-theme",
    "command-error",
    "session-closed",
    "session-created",
    "session-renamed",
    "session-window-changed",
    "window-linked",
    "window-unlinked",
    "pane-died",
    "pane-exited",
    "pane-focus-in",
    "pane-focus-out",
    "pane-mode-changed",
    "pane-set-clipboard",
    "pane-title-changed",
    "window-layout-changed",
    "window-pane-changed",
    "window-renamed",
    "window-resized",
];

/// tmux's hook names in its order: its session table, or with `window`
/// its window and pane table (the last eleven of [`TMUX_HOOKS`]).
pub(crate) fn tmux_hook_names(window: bool) -> &'static [&'static str] {
    let at = TMUX_HOOKS.len() - 11;
    if window {
        &TMUX_HOOKS[at..]
    } else {
        &TMUX_HOOKS[..at]
    }
}

/// The grofe event a tmux hook name fires on (TC.5's `EVENTS`, each of
/// which names its tmux hook, or tmux's error for a name that is no hook.
pub fn hook_event(tmux: &str) -> Result<&'static str, TmuxError> {
    crate::server::hooks::EVENTS
        .iter()
        .find(|e| e.tmux == tmux)
        .map(|e| e.name)
        .ok_or_else(|| TmuxError(format!("invalid option: {tmux}")))
}

/// The native `set-hook`/`show-hooks` argv for a tmux one: the same flags,
/// the hook named by its grofe event (and its `[N]` index kept). `set-hook
/// -R` runs whatever name it is given and says nothing of an unknown one,
/// so the name goes through as it is.
fn hook_args(inv: &Invocation<'_>) -> Result<Vec<String>, TmuxError> {
    let got = inv.getopt();
    let mut args = Vec::new();
    for (flag, value) in &got.flags {
        args.push(format!("-{flag}"));
        args.extend(value.map(str::to_string));
    }
    let mut positionals = inv.positionals().iter();
    if let Some(name) = positionals.next() {
        args.push("--".to_string());
        let (base, index) = match name.split_once('[') {
            Some((base, rest)) => (base, Some(rest)),
            None => (name.as_str(), None),
        };
        // tmux reads `name[N]` and calls anything else after the name
        // ambiguous.
        let well_formed = |rest: &str| {
            rest.strip_suffix(']')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        };
        if index.is_some_and(|rest| !well_formed(rest)) && !got.has('R') {
            return Err(TmuxError(format!("ambiguous option: {name}")));
        }
        match hook_event(base) {
            Ok(event) => args.push(match index {
                Some(rest) => format!("{event}[{rest}"),
                None => event.to_string(),
            }),
            Err(_) if got.has('R') => args.push(name.clone()),
            Err(e) => {
                return Err(match index {
                    Some(_) => TmuxError(format!("invalid option: {name}")),
                    None => e,
                });
            }
        }
    }
    args.extend(positionals.cloned());
    Ok(args)
}

/// `set-hook` and `show-hooks`: onto grofe's event and scopes. The command
/// is stored as written, tagged tmux dialect, and translated through this
/// front end each time it fires (FE.1's stored strings).
fn set_hook(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    let args = hook_args(inv)?;
    inv.native("set-hook", &args)
}

fn show_hooks(inv: &Invocation<'_>) -> Result<Vec<Command>, TmuxError> {
    let args = hook_args(inv)?;
    inv.native("show-hooks", &args)
}

// ---- tmux's option tables ------------------------------------------------------

/// tmux 3.7b's `show -g`, `show -gw` and `show -s` on a fresh server
/// (recorded in TC.0b's container as the golden `fe_3/tmux-tables`, which
/// `tests/compat/fe_3.rs` keeps this file equal to): the names, their
/// order and the default texts the tmux dialect's `show` prints.
const TMUX_TABLES: &str = include_str!("tmux-options.txt");

/// One of tmux's three option tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    /// `show -g`.
    Session,
    /// `show -gw`.
    Window,
    /// `show -s`.
    Server,
}

/// `table`'s lines as tmux prints them, each split into its name (with
/// `[N]` for an element) and its quoted value text.
pub fn tmux_table(table: Table) -> Vec<(&'static str, &'static str)> {
    let section = match table {
        Table::Session => "[session]",
        Table::Window => "[window]",
        Table::Server => "[server]",
    };
    TMUX_TABLES
        .lines()
        .skip_while(|l| *l != section)
        .skip(1)
        .take_while(|l| !l.starts_with('['))
        .map(|l| l.split_once(' ').unwrap_or((l, "")))
        .collect()
}

/// tmux's default for `name` (`status-format[0]` for an element), as the
/// text a `show -v` prints: from the first table that holds it.
pub fn tmux_default(name: &str) -> Option<String> {
    [Table::Session, Table::Window, Table::Server]
        .into_iter()
        .flat_map(tmux_table)
        .find(|(n, _)| *n == name)
        .map(|(_, text)| unquote(text))
}

/// A value as `show` quoted it, unquoted: `''` is empty; `"..."` keeps
/// what a backslash escapes; `'...'` is literal; a lone `\c` is `c`.
pub fn unquote(text: &str) -> String {
    if text == "''" {
        return String::new();
    }
    if let Some(inner) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match (c, chars.clone().next()) {
                ('\\', Some(n)) => {
                    out.push(n);
                    chars.next();
                }
                _ => out.push(c),
            }
        }
        return out;
    }
    if let Some(inner) = text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')) {
        return inner.to_string();
    }
    match text.strip_prefix('\\') {
        Some(rest) if rest.chars().count() == 1 => rest.to_string(),
        _ => text.to_string(),
    }
}

// ---- Rows --------------------------------------------------------------------

const COSMETIC: &[FlagMark] = &[FlagMark::Cosmetic];

/// This area's rows.
pub(crate) fn rows() -> Vec<Row> {
    use GrofeTarget::{Native, Translate, Unsupported};
    use MappingKind::{Behaviour, FlagRename, PassThrough};
    vec![
        row(
            "set-option",
            FlagRename,
            Translate(set_option),
            &[],
            "M3.6",
            "fe_3/show",
        ),
        row(
            "set-window-option",
            FlagRename,
            Translate(set_window_option),
            &[],
            "M3.6",
            "fe_3/show",
        ),
        row(
            "show-options",
            FlagRename,
            Translate(show_options),
            &[],
            "M3.6",
            "fe_3/show",
        ),
        row(
            "show-window-options",
            FlagRename,
            Translate(show_window_options),
            &[],
            "M3.6",
            "fe_3/show",
        ),
        row(
            "set-hook",
            Behaviour,
            Translate(set_hook),
            &[],
            "B2a",
            "b2a/hook-scopes",
        ),
        row(
            "show-hooks",
            Behaviour,
            Translate(show_hooks),
            &[],
            "B2a",
            "b2a/hook-scopes",
        ),
        row(
            "set-environment",
            PassThrough,
            Native("set-environment"),
            &[],
            "M3.6",
            "fe_3/environment",
        ),
        row(
            "set-environment -F",
            Behaviour,
            Unsupported("a value expanded as a format"),
            SEMANTIC,
            "M3.6",
            "",
        ),
        row(
            "set-environment -r",
            Behaviour,
            Unsupported("a variable removed from new panes' environment"),
            SEMANTIC,
            "M3.6",
            "",
        ),
        row(
            "show-environment",
            PassThrough,
            Native("show-environment"),
            &[],
            "M3.6",
            "fe_3/environment",
        ),
        row(
            "show-environment -s",
            Behaviour,
            Unsupported("output as shell commands"),
            COSMETIC,
            "M3.6",
            "",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native(fmt: &str) -> String {
        let got = to_native(fmt, Jobs::Empty);
        assert!(got.before.is_empty());
        got.value
    }

    struct NoVars;

    impl crate::cmd::FormatVars for NoVars {
        fn get(&self, _: &str) -> Option<String> {
            None
        }
    }

    /// Hostile nesting is rewritten and expanded at once, with tmux's
    /// answers: from 100 deep `#{!:x}` gives `0` (recorded), and a flat
    /// chain of conditions is not nesting at all in tmux.
    #[test]
    fn deep_and_long_formats_are_bounded() {
        let start = crate::thread_cpu_time();
        let n = 100_000;
        let deep = format!("{}x{}", "#{!:".repeat(n), "}".repeat(n));
        assert_eq!(crate::cmd::expand(&native(&deep), &NoVars), "0");
        let unclosed = format!("a{}", "#{!:".repeat(n));
        assert_eq!(crate::cmd::expand(&native(&unclosed), &NoVars), "a");
        let chain = format!("#{{?0,x{},last}}", ",0,a".repeat(n));
        assert_eq!(crate::cmd::expand(&native(&chain), &NoVars), "last");
        let took = crate::thread_cpu_time() - start;
        assert!(took < std::time::Duration::from_millis(900), "{took:?}");
    }

    #[test]
    fn short_forms_become_variables() {
        assert_eq!(
            native("#S:#I.#P #W #T #D #F #H #h"),
            "#{session_name}:#{window_index}.#{pane_index} #{window_name} #{pane_title} \
             #{pane_id} #{window_flags} #{host} #{host_short}"
        );
        // Any other letter stays as tmux prints it.
        assert_eq!(native("#Q#1#-#!#a"), "#Q#1#-#!#a");
        assert_eq!(
            native("#[fg=red]#S#[default]"),
            "#[fg=red]#{session_name}#[default]"
        );
    }

    #[test]
    fn escapes_are_grofe_s() {
        assert_eq!(native("## ##{pane_id} ##S"), "## ##{pane_id} ##S");
        assert_eq!(native("a#,b#}c"), "a,b}c");
        assert_eq!(
            native("#{?session_name,a#,b,c}"),
            "#{?session_name,a#{?,,,}b,c}"
        );
        assert_eq!(native("#{?1,a}"), "#{?1,a,}");
        assert_eq!(native("#{?,,,}"), "#{?,,#{?,,}}");
        assert_eq!(native("#{?@x,a,@y,b,c}"), "#{?@x,a,#{?@y,b,c}}");
        assert_eq!(native("x#"), "x");
        assert_eq!(native("a#{?1,b"), "a");
        assert_eq!(native("a#(echo x"), "a");
    }

    #[test]
    fn short_forms_inside_bodies_and_conditions() {
        assert_eq!(native("#{?#W, #W,}"), "#{?#{window_name}, #{window_name},}");
        assert_eq!(native("#{==:#S,main}"), "#{==:#{session_name},main}");
        assert_eq!(
            native("#{?#{==:#W,x},#[bold]#I,}"),
            "#{?#{==:#{window_name},x},#[bold]#{window_index},}"
        );
    }

    #[test]
    fn a_literal_stays_unexpanded() {
        assert_eq!(native("#{l:#S}"), "##S");
        assert_eq!(native("#{l:a##b#,c#}d}"), "a##b,c}d");
        assert_eq!(native("#{?1,#{l:a,b},c}"), "#{?1,a#{?,,,}b,c}");
    }

    #[test]
    fn a_job_is_a_named_source_where_the_status_reads_it() {
        let key = crate::server::sources::inline_key("date");
        let got = to_native("a#(date)b#(date)", Jobs::Sources);
        assert_eq!(got.value, format!("a#{{@{key}}}b#{{@{key}}}"));
        assert_eq!(
            got.before,
            vec![Command::RegisterSource(RegisterSource {
                name: Some(key.clone()),
                command: "date".into(),
            })]
        );
        let nested = to_native("#{?@pet,#(pet (x)) ,}", Jobs::Sources);
        let pet = crate::server::sources::inline_key("pet (x)");
        assert_eq!(nested.value, format!("#{{?@pet,#{{@{pet}}} ,}}"));
        assert_eq!(native("a#(date)b"), "ab");
    }

    #[test]
    fn the_other_modifiers_pass_through_for_the_expander() {
        assert_eq!(native("#{E:status-left}"), "#{E:status-left}");
        assert_eq!(native("#{=21:pane_title}"), "#{=21:pane_title}");
        assert_eq!(
            native("#{W:#I ,[#I] }"),
            "#{W:#{window_index} ,[#{window_index}] }"
        );
    }

    /// A `#(cmd)` source stops once no option reads it: a `set` that
    /// overwrites, `-u`, and `-a` that keeps the old one.
    #[test]
    fn old_job_sources_stop_once_no_option_reads_them() {
        use crate::protocol::{ClientCtx, Dialect};
        fn run(state: &mut crate::server::State, words: &[&str]) {
            let ctx = ClientCtx {
                dialect: Dialect::Tmux,
                ..crate::server::test_ctx()
            };
            let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            crate::server::dispatch::run(state, &ctx, &argv).expect("runs");
        }
        let mut state = crate::server::test_state();
        for i in 0..25 {
            run(
                &mut state,
                &["set", "-g", "status-right", &format!("#(echo {i})")],
            );
        }
        run(&mut state, &["set", "-ag", "status-right", "#(echo more)"]);
        run(&mut state, &["set", "-g", "status-left", "#(echo left)"]);
        run(&mut state, &["set", "-gu", "status-left"]);
        let running = crate::server::sources::running(&state);
        assert_eq!(running, 2, "the last and the append");
        run(&mut state, &["set", "-g", "status-right", "plain"]);
        assert_eq!(crate::server::sources::running(&state), 0);
    }

    #[test]
    fn tmux_s_tables_are_the_recorded_ones() {
        let session = tmux_table(Table::Session);
        assert_eq!(session.len(), 68);
        assert_eq!(session.first(), Some(&("activity-action", "other")));
        assert_eq!(
            tmux_default("window-status-format").as_deref(),
            Some("#I:#W#{?window_flags,#{window_flags}, }")
        );
        assert_eq!(tmux_default("default-command").as_deref(), Some(""));
        assert_eq!(
            unquote(r##""!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~""##),
            r##"!"#$%&'()*+,-./:;<=>?@[\]^`{|}~"##
        );
        assert_eq!(tmux_table(Table::Window).len(), 74);
        assert!(
            tmux_table(Table::Server)
                .iter()
                .any(|(n, _)| *n == "command-alias[5]")
        );
    }

    #[test]
    fn the_fifteen_hooks_map_onto_grofe_events() {
        for tmux in [
            "after-select-pane",
            "after-select-window",
            "after-new-window",
            "after-kill-pane",
            "after-resize-pane",
            "after-resize-window",
            "client-resized",
            "client-session-changed",
            "session-created",
            "session-closed",
            "pane-exited",
            "pane-died",
            "alert-bell",
            "alert-activity",
            "alert-silence",
        ] {
            let event = hook_event(tmux).unwrap();
            assert!(crate::server::hooks::event(event).is_some(), "{tmux}");
        }
        assert_eq!(hook_event("nope").unwrap_err().0, "invalid option: nope");
        assert_eq!(hook_event("client-focus-in"), Ok("client-focus-in"));
        assert_eq!(hook_event("window-resized"), Ok("window-size-changed"));
    }
}
