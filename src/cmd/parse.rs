//! argv -> [`Command`] (spec §4.5). Hand-rolled, tmux-style: short flags
//! may be combined (`-dP`), a flag's value may be attached (`-tfoo`) or a
//! separate token (`-t foo`), and `--` ends flag parsing. `set`/`show` are
//! recognised as aliases for `set-option`/`show-options`; a handful of
//! other well-known tmux short names are recognised too.

use std::collections::{BTreeMap, BTreeSet};

use super::keys;
use super::target::parse_target;
use super::{
    AttachSession, BindKey, CapturePane, Command, CommandPrompt, DetachClient, DisplayMessage,
    HasSession, KillPane, KillServer, KillSession, KillWindow, ListClients, ListKeys, ListPanes,
    ListSessions, ListWindows, NewSession, NewWindow, OptionScope, ParseError, Reload, Rename,
    ResizeAction, ResizePane, RespawnPane, RunShell, SelectLayout, SelectPane, SelectWindow,
    SendKeys, SendKeysItem, SendPrefix, SetOption, ShowOptions, SourceFile, SplitWindow, Subscribe,
    SwitchClient, Target, UnbindKey, WindowStep,
};
use crate::types::{CaptureBound, CaptureOpts, Dir, Edge, SplitSize, WINDOW_MAXIMUM};

/// Whether a short/long flag takes a value, and if so whether it's
/// required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlagKind {
    Bool,
    /// A value is required, attached (`-tfoo`) or the next token (`-t
    /// foo`).
    Value,
}

struct FlagTable {
    short: &'static [(char, FlagKind)],
    long: &'static [(&'static str, FlagKind)],
}

const NO_FLAGS: FlagTable = FlagTable {
    short: &[],
    long: &[],
};

#[derive(Default)]
struct ParsedFlags {
    present: BTreeSet<char>,
    values: BTreeMap<char, String>,
    long_present: BTreeSet<&'static str>,
    positionals: Vec<String>,
}

fn find_short(table: &FlagTable, c: char) -> Option<FlagKind> {
    for &(fc, kind) in table.short {
        if fc == c {
            return Some(kind);
        }
    }
    None
}

fn find_long(table: &FlagTable, name: &str) -> Option<(&'static str, FlagKind)> {
    for &(n, kind) in table.long {
        if n == name {
            return Some((n, kind));
        }
    }
    None
}

/// Splits `argv` into recognised flags (per `table`) and trailing
/// positionals, tmux-style. Flag parsing stops at `--` or at the first
/// token that isn't a flag; everything after that is positional
/// verbatim, which is what lets a trailing shell command (e.g. `new-window
/// -d sleep 30`) keep its own `-`-prefixed arguments untouched.
///
/// `cmd_name` is threaded through only to word flag errors like tmux's
/// own CLI (`command <cmd_name>: unknown flag -z`).
fn tokenize(argv: &[String], table: &FlagTable, cmd_name: &str) -> Result<ParsedFlags, ParseError> {
    let mut flags = ParsedFlags::default();
    let mut i = 0;
    let mut flags_done = false;
    while i < argv.len() {
        let tok = argv[i].as_str();
        if flags_done {
            flags.positionals.push(tok.to_string());
            i += 1;
            continue;
        }
        if tok == "--" {
            flags_done = true;
            i += 1;
            continue;
        }
        if let Some(name) = tok.strip_prefix("--") {
            match find_long(table, name) {
                Some((canonical, FlagKind::Bool)) => {
                    flags.long_present.insert(canonical);
                }
                // No long flag currently takes a value; if one is
                // added, wire attached/separate value handling in here.
                _ => return Err(ParseError::UnknownLongFlag(cmd_name.to_string())),
            }
            i += 1;
            continue;
        }
        if tok.len() > 1 && tok.starts_with('-') {
            let chars: Vec<char> = tok[1..].chars().collect();
            let mut ci = 0;
            while ci < chars.len() {
                let c = chars[ci];
                match find_short(table, c) {
                    None => return Err(ParseError::UnknownFlag(cmd_name.to_string(), c)),
                    Some(FlagKind::Bool) => {
                        flags.present.insert(c);
                        ci += 1;
                    }
                    Some(FlagKind::Value) => {
                        flags.present.insert(c);
                        let attached: String = chars[ci + 1..].iter().collect();
                        if attached.is_empty() {
                            i += 1;
                            let value = argv
                                .get(i)
                                .ok_or_else(|| ParseError::MissingValue(cmd_name.to_string(), c))?;
                            flags.values.insert(c, value.clone());
                        } else {
                            flags.values.insert(c, attached);
                        }
                        ci = chars.len();
                    }
                }
            }
            i += 1;
            continue;
        }
        // First positional (including a lone "-"): flags are done, per
        // tmux's own non-permuting flag parsing.
        flags_done = true;
        flags.positionals.push(tok.to_string());
        i += 1;
    }
    Ok(flags)
}

fn target_or_current(values: &BTreeMap<char, String>, key: char) -> Result<Target, ParseError> {
    match values.get(&key) {
        Some(s) => parse_target(s),
        None => Ok(Target::Current),
    }
}

fn parse_u8(s: &str, what: &str) -> Result<u8, ParseError> {
    s.parse()
        .map_err(|_| ParseError::Other(format!("invalid {what}: {s}")))
}

fn parse_u16(s: &str, what: &str) -> Result<u16, ParseError> {
    s.parse()
        .map_err(|_| ParseError::Other(format!("invalid {what}: {s}")))
}

/// `new-session -x/-y`, as tmux 3.7b reads them: `strtonum(1, USHRT_MAX)`
/// with its `too small` / `too large` / `invalid` wording, then capped at
/// [`WINDOW_MAXIMUM`].
fn parse_window_size(s: &str, what: &str) -> Result<u16, ParseError> {
    let err = |why: &str| ParseError::Other(format!("{what} {why}"));
    let digits = s.strip_prefix(['-', '+']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(err("invalid"));
    }
    match s.parse::<i64>() {
        Ok(n) if n < 1 => Err(err("too small")),
        Ok(n) if n > i64::from(u16::MAX) => Err(err("too large")),
        Ok(n) => Ok((n as u16).min(WINDOW_MAXIMUM)),
        Err(_) if s.starts_with('-') => Err(err("too small")),
        Err(_) => Err(err("too large")),
    }
}

/// Rejects positionals beyond `max`, worded like tmux's own CLI:
/// `command <cmd_name>: too many arguments (need at most <max>)`.
fn no_extra_positionals(flags: &ParsedFlags, cmd_name: &str, max: usize) -> Result<(), ParseError> {
    if flags.positionals.len() <= max {
        Ok(())
    } else {
        Err(ParseError::TooManyArguments(cmd_name.to_string(), max))
    }
}

const NEW_SESSION_SHORT: &[(char, FlagKind)] = &[
    ('d', FlagKind::Bool),
    ('s', FlagKind::Value),
    ('c', FlagKind::Value),
    ('x', FlagKind::Value),
    ('y', FlagKind::Value),
];

fn parse_new_session(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: NEW_SESSION_SHORT,
            long: &[],
        },
        "new-session",
    )?;
    let width = flags
        .values
        .get(&'x')
        .map(|s| parse_window_size(s, "width"))
        .transpose()?;
    let height = flags
        .values
        .get(&'y')
        .map(|s| parse_window_size(s, "height"))
        .transpose()?;
    Ok(Command::NewSession(NewSession {
        detached: flags.present.contains(&'d'),
        name: flags.values.get(&'s').cloned(),
        cwd: flags.values.get(&'c').cloned(),
        width,
        height,
        command: (!flags.positionals.is_empty()).then_some(flags.positionals),
    }))
}

const TARGET_ONLY_SHORT: &[(char, FlagKind)] = &[('t', FlagKind::Value)];

fn parse_target_only(argv: &[String], cmd_name: &str) -> Result<Target, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: TARGET_ONLY_SHORT,
            long: &[],
        },
        cmd_name,
    )?;
    no_extra_positionals(&flags, cmd_name, 0)?;
    target_or_current(&flags.values, 't')
}

fn parse_kill_session(argv: &[String]) -> Result<Command, ParseError> {
    Ok(Command::KillSession(KillSession {
        target: parse_target_only(argv, "kill-session")?,
    }))
}

fn parse_has_session(argv: &[String]) -> Result<Command, ParseError> {
    Ok(Command::HasSession(HasSession {
        target: parse_target_only(argv, "has-session")?,
    }))
}

fn parse_kill_window(argv: &[String]) -> Result<Command, ParseError> {
    Ok(Command::KillWindow(KillWindow {
        target: parse_target_only(argv, "kill-window")?,
    }))
}

fn parse_select_window(argv: &[String]) -> Result<Command, ParseError> {
    Ok(Command::SelectWindow(SelectWindow {
        target: parse_target_only(argv, "select-window")?,
    }))
}

fn parse_kill_pane(argv: &[String]) -> Result<Command, ParseError> {
    Ok(Command::KillPane(KillPane {
        target: parse_target_only(argv, "kill-pane")?,
    }))
}

const FORMAT_ONLY_SHORT: &[(char, FlagKind)] = &[('F', FlagKind::Value)];

fn parse_list_sessions(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: FORMAT_ONLY_SHORT,
            long: &[],
        },
        "list-sessions",
    )?;
    no_extra_positionals(&flags, "list-sessions", 0)?;
    Ok(Command::ListSessions(ListSessions {
        format: flags.values.get(&'F').cloned(),
    }))
}

const TARGET_FORMAT_SHORT: &[(char, FlagKind)] = &[('t', FlagKind::Value), ('F', FlagKind::Value)];

fn parse_list_windows(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: TARGET_FORMAT_SHORT,
            long: &[],
        },
        "list-windows",
    )?;
    no_extra_positionals(&flags, "list-windows", 0)?;
    Ok(Command::ListWindows(ListWindows {
        target: target_or_current(&flags.values, 't')?,
        format: flags.values.get(&'F').cloned(),
    }))
}

fn parse_list_panes(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: TARGET_FORMAT_SHORT,
            long: &[],
        },
        "list-panes",
    )?;
    no_extra_positionals(&flags, "list-panes", 0)?;
    Ok(Command::ListPanes(ListPanes {
        target: target_or_current(&flags.values, 't')?,
        format: flags.values.get(&'F').cloned(),
    }))
}

const NEW_WINDOW_SHORT: &[(char, FlagKind)] = &[
    ('d', FlagKind::Bool),
    ('t', FlagKind::Value),
    ('n', FlagKind::Value),
    ('c', FlagKind::Value),
];

fn parse_new_window(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: NEW_WINDOW_SHORT,
            long: &[],
        },
        "new-window",
    )?;
    Ok(Command::NewWindow(NewWindow {
        detached: flags.present.contains(&'d'),
        target: target_or_current(&flags.values, 't')?,
        name: flags.values.get(&'n').cloned(),
        cwd: flags.values.get(&'c').cloned(),
        command: (!flags.positionals.is_empty()).then_some(flags.positionals),
    }))
}

const SPLIT_WINDOW_SHORT: &[(char, FlagKind)] = &[
    ('h', FlagKind::Bool),
    ('v', FlagKind::Bool),
    ('c', FlagKind::Value),
    ('p', FlagKind::Value),
    ('l', FlagKind::Value),
    ('t', FlagKind::Value),
];

fn parse_split_window(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: SPLIT_WINDOW_SHORT,
            long: &[],
        },
        "split-window",
    )?;
    let dir = match (flags.present.contains(&'h'), flags.present.contains(&'v')) {
        (true, true) => {
            return Err(ParseError::Other(
                "only one of -h or -v allowed".to_string(),
            ));
        }
        (true, false) => Dir::Horizontal,
        // tmux's own default (neither flag given) is a vertical split.
        (false, true) | (false, false) => Dir::Vertical,
    };
    let percent = flags
        .values
        .get(&'p')
        .map(|s| parse_u8(s, "percent"))
        .transpose()?;
    let cells = flags
        .values
        .get(&'l')
        .map(|s| parse_u16(s, "size"))
        .transpose()?;
    let size = match (percent, cells) {
        (Some(_), Some(_)) => {
            return Err(ParseError::Other(
                "only one of -p or -l allowed".to_string(),
            ));
        }
        (Some(p), None) => SplitSize::Percent(p),
        (None, Some(c)) => SplitSize::Cells(c),
        (None, None) => SplitSize::Half,
    };
    Ok(Command::SplitWindow(SplitWindow {
        dir,
        cwd: flags.values.get(&'c').cloned(),
        size,
        target: target_or_current(&flags.values, 't')?,
        command: (!flags.positionals.is_empty()).then_some(flags.positionals),
    }))
}

const SELECT_PANE_SHORT: &[(char, FlagKind)] = &[
    ('L', FlagKind::Bool),
    ('R', FlagKind::Bool),
    ('U', FlagKind::Bool),
    ('D', FlagKind::Bool),
    ('t', FlagKind::Value),
];

fn parse_select_pane(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: SELECT_PANE_SHORT,
            long: &[],
        },
        "select-pane",
    )?;
    no_extra_positionals(&flags, "select-pane", 0)?;
    let edges: Vec<Edge> = [
        ('L', Edge::Left),
        ('R', Edge::Right),
        ('U', Edge::Up),
        ('D', Edge::Down),
    ]
    .into_iter()
    .filter(|(c, _)| flags.present.contains(c))
    .map(|(_, edge)| edge)
    .collect();
    if edges.len() > 1 {
        return Err(ParseError::Other(
            "select-pane: only one direction allowed".to_string(),
        ));
    }
    Ok(Command::SelectPane(SelectPane {
        edge: edges.into_iter().next(),
        target: target_or_current(&flags.values, 't')?,
    }))
}

// tmux's own resize-pane grammar: -L/-R/-U/-D/-Z are all boolean flags
// (`-LR` sets two directions at once), and the cell count is a single
// trailing positional shared by whichever directions were given, not a
// per-flag value.
const RESIZE_PANE_SHORT: &[(char, FlagKind)] = &[
    ('L', FlagKind::Bool),
    ('R', FlagKind::Bool),
    ('U', FlagKind::Bool),
    ('D', FlagKind::Bool),
    ('Z', FlagKind::Bool),
    ('t', FlagKind::Value),
];

fn parse_resize_pane(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: RESIZE_PANE_SHORT,
            long: &[],
        },
        "resize-pane",
    )?;
    no_extra_positionals(&flags, "resize-pane", 1)?;
    let edges: Vec<Edge> = [
        ('L', Edge::Left),
        ('R', Edge::Right),
        ('U', Edge::Up),
        ('D', Edge::Down),
    ]
    .into_iter()
    .filter(|(c, _)| flags.present.contains(c))
    .map(|(_, edge)| edge)
    .collect();
    let zoom = flags.present.contains(&'Z');
    if zoom && !edges.is_empty() {
        return Err(ParseError::Other(
            "resize-pane: -Z can't combine with a direction".to_string(),
        ));
    }
    let action = if zoom {
        ResizeAction::ToggleZoom
    } else {
        if edges.is_empty() {
            return Err(ParseError::Other(
                "resize-pane: expected one of -L -R -U -D -Z".to_string(),
            ));
        }
        let amount = match flags.positionals.first() {
            Some(s) => parse_u16(s, "resize amount")?,
            None => 1,
        };
        ResizeAction::Edges(edges, amount)
    };
    Ok(Command::ResizePane(ResizePane {
        action,
        target: target_or_current(&flags.values, 't')?,
    }))
}

const RESPAWN_PANE_SHORT: &[(char, FlagKind)] = &[('k', FlagKind::Bool), ('t', FlagKind::Value)];

fn parse_respawn_pane(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: RESPAWN_PANE_SHORT,
            long: &[],
        },
        "respawn-pane",
    )?;
    Ok(Command::RespawnPane(RespawnPane {
        kill: flags.present.contains(&'k'),
        target: target_or_current(&flags.values, 't')?,
        command: (!flags.positionals.is_empty()).then_some(flags.positionals),
    }))
}

const SEND_KEYS_SHORT: &[(char, FlagKind)] = &[
    ('l', FlagKind::Bool),
    ('H', FlagKind::Bool),
    ('t', FlagKind::Value),
];

fn parse_send_keys(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: SEND_KEYS_SHORT,
            long: &[],
        },
        "send-keys",
    )?;
    let literal = flags.present.contains(&'l');
    let hex = flags.present.contains(&'H');
    if literal && hex {
        return Err(ParseError::Other(
            "send-keys: only one of -l or -H allowed".to_string(),
        ));
    }
    let mut items = Vec::with_capacity(flags.positionals.len());
    for arg in &flags.positionals {
        items.push(if hex {
            let byte = u8::from_str_radix(arg, 16)
                .map_err(|_| ParseError::Other(format!("invalid hex byte: {arg}")))?;
            SendKeysItem::Hex(byte)
        } else if literal || !keys::is_key_name(arg) {
            SendKeysItem::Literal(arg.clone())
        } else {
            SendKeysItem::Key(arg.clone())
        });
    }
    Ok(Command::SendKeys(SendKeys {
        target: target_or_current(&flags.values, 't')?,
        items,
    }))
}

const CAPTURE_PANE_SHORT: &[(char, FlagKind)] = &[
    ('p', FlagKind::Bool),
    ('e', FlagKind::Bool),
    ('S', FlagKind::Value),
    ('E', FlagKind::Value),
    ('t', FlagKind::Value),
];

fn parse_capture_line(s: &str) -> Result<CaptureBound, ParseError> {
    s.parse::<i32>()
        .map(CaptureBound::Line)
        .map_err(|_| ParseError::Other(format!("invalid capture-pane bound: {s}")))
}

fn parse_capture_pane(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: CAPTURE_PANE_SHORT,
            long: &[],
        },
        "capture-pane",
    )?;
    no_extra_positionals(&flags, "capture-pane", 0)?;
    let start = match flags.values.get(&'S') {
        Some(s) if s == "-" => Some(CaptureBound::HistoryStart),
        Some(s) => Some(parse_capture_line(s)?),
        None => None,
    };
    let end = match flags.values.get(&'E') {
        Some(s) if s == "-" => Some(CaptureBound::VisibleEnd),
        Some(s) => Some(parse_capture_line(s)?),
        None => None,
    };
    Ok(Command::CapturePane(CapturePane {
        target: target_or_current(&flags.values, 't')?,
        print: flags.present.contains(&'p'),
        opts: CaptureOpts {
            start,
            end,
            escapes: flags.present.contains(&'e'),
        },
    }))
}

const SET_OPTION_SHORT: &[(char, FlagKind)] = &[
    ('g', FlagKind::Bool),
    ('s', FlagKind::Bool),
    ('w', FlagKind::Bool),
    ('p', FlagKind::Bool),
    ('u', FlagKind::Bool),
    ('t', FlagKind::Value),
];
const SET_OPTION_LONG: &[(&str, FlagKind)] = &[("persist", FlagKind::Bool)];

/// Resolves `-g -s -w -p` to a scope, by tmux's own precedence: never an
/// error, just a fixed order of preference over whichever flags were
/// given. Verified against tmux 3.7b's actual `options_scope_from_flags`
/// (options.c): it checks `-s` first and returns immediately (server
/// scope, unconditionally, even alongside other flags), then `-p`
/// (pane), then `-w` (window), and only falls through to session scope
/// once none of those are present; `-g` only ever modifies the window or
/// session branches there, and is never even consulted once `-s` or `-p`
/// has matched. grofe simplifies `-g` to always win outright (round-1
/// ruling: grofe has one flat Global scope, unlike tmux's separate
/// per-type global tables), so grofe's order is `-g`, then `-s`
/// (grofe's own scope, not tmux's real "-s = server"), then `-p`, then
/// `-w`. Confirmed live: `set -wp`/`set -pw` land on the pane (not the
/// window), and `set -sw`/`set -sp` land on grofe's session scope.
fn parse_scope(flags: &ParsedFlags) -> Option<OptionScope> {
    if flags.present.contains(&'g') {
        Some(OptionScope::Global)
    } else if flags.present.contains(&'s') {
        Some(OptionScope::Session)
    } else if flags.present.contains(&'p') {
        Some(OptionScope::Pane)
    } else if flags.present.contains(&'w') {
        Some(OptionScope::Window)
    } else {
        None
    }
}

fn parse_set_option(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: SET_OPTION_SHORT,
            long: SET_OPTION_LONG,
        },
        "set-option",
    )?;
    let scope = parse_scope(&flags);
    let mut positionals = flags.positionals.iter();
    let name = positionals
        .next()
        .cloned()
        .ok_or_else(|| ParseError::MissingArgument("set-option".to_string()))?;
    let value = positionals.next().cloned();
    if positionals.next().is_some() {
        return Err(ParseError::TooManyArguments("set-option".to_string(), 2));
    }
    Ok(Command::SetOption(SetOption {
        scope,
        unset: flags.present.contains(&'u'),
        persist: flags.long_present.contains("persist"),
        target: target_or_current(&flags.values, 't')?,
        name,
        value,
    }))
}

const SHOW_OPTIONS_SHORT: &[(char, FlagKind)] = &[
    ('g', FlagKind::Bool),
    ('s', FlagKind::Bool),
    ('w', FlagKind::Bool),
    ('p', FlagKind::Bool),
    ('t', FlagKind::Value),
];

fn parse_show_options(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: SHOW_OPTIONS_SHORT,
            long: &[],
        },
        "show-options",
    )?;
    let scope = parse_scope(&flags);
    let mut positionals = flags.positionals.iter();
    let name = positionals.next().cloned();
    if positionals.next().is_some() {
        return Err(ParseError::TooManyArguments("show-options".to_string(), 1));
    }
    Ok(Command::ShowOptions(ShowOptions {
        scope,
        target: target_or_current(&flags.values, 't')?,
        name,
    }))
}

const DISPLAY_MESSAGE_SHORT: &[(char, FlagKind)] = &[('p', FlagKind::Bool), ('t', FlagKind::Value)];

fn parse_display_message(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: DISPLAY_MESSAGE_SHORT,
            long: &[],
        },
        "display-message",
    )?;
    let mut positionals = flags.positionals.iter();
    let text = positionals.next().cloned();
    if positionals.next().is_some() {
        return Err(ParseError::TooManyArguments(
            "display-message".to_string(),
            1,
        ));
    }
    Ok(Command::DisplayMessage(DisplayMessage {
        print: flags.present.contains(&'p'),
        target: target_or_current(&flags.values, 't')?,
        text,
    }))
}

fn parse_kill_server(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(argv, &NO_FLAGS, "kill-server")?;
    no_extra_positionals(&flags, "kill-server", 0)?;
    Ok(Command::KillServer(KillServer))
}

fn parse_subscribe(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(argv, &NO_FLAGS, "subscribe")?;
    Ok(Command::Subscribe(Subscribe {
        events: flags.positionals,
    }))
}

const ATTACH_SESSION_SHORT: &[(char, FlagKind)] = &[('t', FlagKind::Value), ('d', FlagKind::Bool)];
const ATTACH_SESSION_LONG: &[(&str, FlagKind)] = &[("nest", FlagKind::Bool)];

fn parse_attach_session(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: ATTACH_SESSION_SHORT,
            long: ATTACH_SESSION_LONG,
        },
        "attach-session",
    )?;
    no_extra_positionals(&flags, "attach-session", 0)?;
    Ok(Command::AttachSession(AttachSession {
        target: target_or_current(&flags.values, 't')?,
        detach_others: flags.present.contains(&'d'),
        nest: flags.long_present.contains("nest"),
    }))
}

const DETACH_CLIENT_SHORT: &[(char, FlagKind)] = &[
    ('s', FlagKind::Value),
    ('t', FlagKind::Value),
    ('a', FlagKind::Bool),
];

fn parse_detach_client(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: DETACH_CLIENT_SHORT,
            long: &[],
        },
        "detach-client",
    )?;
    no_extra_positionals(&flags, "detach-client", 0)?;
    Ok(Command::DetachClient(DetachClient {
        session: flags
            .values
            .get(&'s')
            .map(|s| parse_target(s))
            .transpose()?,
        client: flags.values.get(&'t').cloned(),
        all_others: flags.present.contains(&'a'),
    }))
}

fn parse_list_clients(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: FORMAT_ONLY_SHORT,
            long: &[],
        },
        "list-clients",
    )?;
    no_extra_positionals(&flags, "list-clients", 0)?;
    Ok(Command::ListClients(ListClients {
        format: flags.values.get(&'F').cloned(),
    }))
}

fn parse_window_step(argv: &[String], cmd_name: &str) -> Result<WindowStep, ParseError> {
    Ok(WindowStep {
        target: parse_target_only(argv, cmd_name)?,
    })
}

fn parse_rename(argv: &[String], cmd_name: &str) -> Result<Rename, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: TARGET_ONLY_SHORT,
            long: &[],
        },
        cmd_name,
    )?;
    no_extra_positionals(&flags, cmd_name, 1)?;
    let name = flags
        .positionals
        .first()
        .cloned()
        .ok_or_else(|| ParseError::MissingArgument(cmd_name.to_string()))?;
    Ok(Rename {
        target: target_or_current(&flags.values, 't')?,
        name,
    })
}

const SWITCH_CLIENT_SHORT: &[(char, FlagKind)] = &[
    ('t', FlagKind::Value),
    ('l', FlagKind::Bool),
    ('n', FlagKind::Bool),
    ('p', FlagKind::Bool),
    ('T', FlagKind::Value),
];

fn parse_switch_client(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: SWITCH_CLIENT_SHORT,
            long: &[],
        },
        "switch-client",
    )?;
    no_extra_positionals(&flags, "switch-client", 0)?;
    Ok(Command::SwitchClient(SwitchClient {
        target: flags
            .values
            .get(&'t')
            .map(|s| parse_target(s))
            .transpose()?,
        last: flags.present.contains(&'l'),
        next: flags.present.contains(&'n'),
        previous: flags.present.contains(&'p'),
        key_table: flags.values.get(&'T').cloned(),
    }))
}

fn parse_send_prefix(argv: &[String]) -> Result<Command, ParseError> {
    Ok(Command::SendPrefix(SendPrefix {
        target: parse_target_only(argv, "send-prefix")?,
    }))
}

/// `-T`, else `root` for `-n`, else `prefix`: tmux's key-table choice for
/// `bind-key` and `unbind-key`.
fn key_table(flags: &ParsedFlags) -> String {
    match flags.values.get(&'T') {
        Some(t) => t.clone(),
        None if flags.present.contains(&'n') => "root".to_string(),
        None => "prefix".to_string(),
    }
}

const BIND_KEY_SHORT: &[(char, FlagKind)] = &[
    ('n', FlagKind::Bool),
    ('r', FlagKind::Bool),
    ('T', FlagKind::Value),
    ('N', FlagKind::Value),
];

fn parse_bind_key(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: BIND_KEY_SHORT,
            long: &[],
        },
        "bind-key",
    )?;
    let (key, command) = flags
        .positionals
        .split_first()
        .ok_or_else(|| ParseError::MissingArgument("bind-key".to_string()))?;
    Ok(Command::BindKey(BindKey {
        table: key_table(&flags),
        repeat: flags.present.contains(&'r'),
        note: flags.values.get(&'N').cloned(),
        key: key.clone(),
        command: command.to_vec(),
    }))
}

const UNBIND_KEY_SHORT: &[(char, FlagKind)] = &[
    ('n', FlagKind::Bool),
    ('a', FlagKind::Bool),
    ('T', FlagKind::Value),
];

fn parse_unbind_key(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: UNBIND_KEY_SHORT,
            long: &[],
        },
        "unbind-key",
    )?;
    no_extra_positionals(&flags, "unbind-key", 1)?;
    let all = flags.present.contains(&'a');
    let key = flags.positionals.first().cloned();
    // tmux's wording.
    match (all, &key) {
        (true, Some(_)) => return Err(ParseError::Other("key given with -a".to_string())),
        (false, None) => return Err(ParseError::Other("missing key".to_string())),
        _ => {}
    }
    Ok(Command::UnbindKey(UnbindKey {
        table: key_table(&flags),
        all,
        key,
    }))
}

const TABLE_ONLY_SHORT: &[(char, FlagKind)] = &[('T', FlagKind::Value)];

fn parse_list_keys(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: TABLE_ONLY_SHORT,
            long: &[],
        },
        "list-keys",
    )?;
    no_extra_positionals(&flags, "list-keys", 0)?;
    Ok(Command::ListKeys(ListKeys {
        table: flags.values.get(&'T').cloned(),
    }))
}

/// The single required positional of `cmd_name`.
fn one_positional(flags: &ParsedFlags, cmd_name: &str) -> Result<String, ParseError> {
    no_extra_positionals(flags, cmd_name, 1)?;
    flags
        .positionals
        .first()
        .cloned()
        .ok_or_else(|| ParseError::MissingArgument(cmd_name.to_string()))
}

fn parse_source_file(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: &[('q', FlagKind::Bool)],
            long: &[],
        },
        "source-file",
    )?;
    Ok(Command::SourceFile(SourceFile {
        quiet: flags.present.contains(&'q'),
        path: one_positional(&flags, "source-file")?,
    }))
}

fn parse_reload(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(argv, &NO_FLAGS, "reload")?;
    no_extra_positionals(&flags, "reload", 0)?;
    Ok(Command::Reload(Reload))
}

const COMMAND_PROMPT_SHORT: &[(char, FlagKind)] = &[('p', FlagKind::Value), ('I', FlagKind::Value)];

fn parse_command_prompt(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: COMMAND_PROMPT_SHORT,
            long: &[],
        },
        "command-prompt",
    )?;
    no_extra_positionals(&flags, "command-prompt", 1)?;
    Ok(Command::CommandPrompt(CommandPrompt {
        prompt: flags.values.get(&'p').cloned(),
        initial: flags.values.get(&'I').cloned(),
        template: flags.positionals.first().cloned(),
    }))
}

fn parse_run_shell(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(argv, &NO_FLAGS, "run-shell")?;
    Ok(Command::RunShell(RunShell {
        cmd: one_positional(&flags, "run-shell")?,
    }))
}

fn parse_select_layout(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: TARGET_ONLY_SHORT,
            long: &[],
        },
        "select-layout",
    )?;
    Ok(Command::SelectLayout(SelectLayout {
        target: target_or_current(&flags.values, 't')?,
        name: one_positional(&flags, "select-layout")?,
    }))
}

/// Splits a stored command string (a binding, a prompt answer, a config
/// value) into commands and their words, as tmux 3.7b's parser reads it
/// (recorded on a private socket):
///
/// - whitespace separates words; quoted parts join with whatever touches
///   them (`'it'"'"'s'` is one word) and may be empty (`''`);
/// - single quotes keep everything literal;
/// - in double quotes and bare words a backslash keeps the next character
///   literally, `$VAR` and `${VAR}` expand (unset is empty), and a `~` or
///   `~user` that starts the word is that home directory;
/// - an unquoted `;` ends a command wherever it is; a `\;` word is a
///   literal `;` argument, except a trailing one, which is dropped;
/// - an unterminated quote is closed at the end of the string.
///
/// Empty commands are dropped. Never fails today; the `Result` is the
/// contract for a stricter grammar.
// ponytail: no `\n`/`\e`/octal/`\u` escapes, `#` comments or `%if`.
pub fn split_command_string(s: &str) -> Result<Vec<Vec<String>>, ParseError> {
    let mut cmds = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    // A word has begun, even if it is still empty (`''`).
    let mut started = false;
    // The word so far is exactly one `\;`.
    let mut escaped_semicolon = false;
    let mut chars = s.chars().peekable();

    let end_word = |word: &mut String, started: &mut bool, words: &mut Vec<String>| {
        if *started {
            words.push(std::mem::take(word));
            *started = false;
        }
    };
    let end_command = |words: &mut Vec<String>, cmds: &mut Vec<Vec<String>>| {
        if !words.is_empty() {
            cmds.push(std::mem::take(words));
        }
    };

    while let Some(c) = chars.next() {
        let fresh = !started;
        if !c.is_whitespace() && !(c == '\\' && fresh) {
            escaped_semicolon = false;
        }
        match c {
            c if c.is_whitespace() => end_word(&mut word, &mut started, &mut words),
            ';' => {
                end_word(&mut word, &mut started, &mut words);
                end_command(&mut words, &mut cmds);
            }
            '\\' => {
                started = true;
                let next = chars.next().unwrap_or('\\');
                escaped_semicolon = fresh && next == ';';
                word.push(next);
            }
            '\'' => {
                started = true;
                for x in chars.by_ref() {
                    if x == '\'' {
                        break;
                    }
                    word.push(x);
                }
            }
            '"' => {
                started = true;
                if fresh && chars.peek() == Some(&'~') {
                    chars.next();
                    expand_tilde(&mut chars, &mut word);
                }
                while let Some(x) = chars.next() {
                    match x {
                        '"' => break,
                        '\\' => word.push(chars.next().unwrap_or('\\')),
                        '$' => expand_var(&mut chars, &mut word),
                        x => word.push(x),
                    }
                }
            }
            '$' => {
                started = true;
                expand_var(&mut chars, &mut word);
            }
            '~' if fresh => {
                started = true;
                expand_tilde(&mut chars, &mut word);
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    end_word(&mut word, &mut started, &mut words);
    // A trailing `\;` word is dropped, as tmux drops it.
    if escaped_semicolon {
        words.pop();
    }
    end_command(&mut words, &mut cmds);
    Ok(cmds)
}

/// A variable's value for [`split_command_string`], empty when unset.
// ponytail: the server process's environment; TC.5's global environment
// (`set-environment -g`) replaces it.
fn env_var(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

/// After a `$`: `${NAME}` or `NAME` (letters, digits, `_`), expanded into
/// `out`; a `$` before anything else is literal.
fn expand_var(chars: &mut std::iter::Peekable<std::str::Chars>, out: &mut String) {
    let is_name = |c: &char| c.is_ascii_alphanumeric() || *c == '_';
    let mut name = String::new();
    if chars.peek() == Some(&'{') {
        chars.next();
        for c in chars.by_ref() {
            if c == '}' {
                break;
            }
            name.push(c);
        }
    } else if chars
        .peek()
        .is_some_and(|c| c.is_ascii_alphabetic() || *c == '_')
    {
        while let Some(c) = chars.next_if(is_name) {
            name.push(c);
        }
    } else {
        out.push('$');
        return;
    }
    out.push_str(&env_var(&name));
}

/// After a word-initial `~`: `~` is `$HOME` (else the user's passwd home),
/// `~user` that user's home; an unknown user stays as written.
fn expand_tilde(chars: &mut std::iter::Peekable<std::str::Chars>, out: &mut String) {
    let mut user = String::new();
    while let Some(c) = chars.next_if(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        user.push(c);
    }
    let home = if user.is_empty() {
        std::env::var("HOME")
            .ok()
            .filter(|h| !h.is_empty())
            .or_else(|| {
                nix::unistd::User::from_uid(nix::unistd::getuid())
                    .ok()
                    .flatten()
                    .map(|u| u.dir.display().to_string())
            })
    } else {
        nix::unistd::User::from_name(&user)
            .ok()
            .flatten()
            .map(|u| u.dir.display().to_string())
    };
    match home {
        Some(home) => out.push_str(&home),
        None => {
            out.push('~');
            out.push_str(&user);
        }
    }
}

/// Parses argv into the commands it runs, in order. Every caller that runs
/// commands (the CLI, control mode, bindings, the prompt) goes through
/// this, so sequences have one home. The whole sequence is parsed before
/// anything runs, as tmux does, so a parse error anywhere runs nothing.
pub fn parse_sequence(argv: &[String]) -> Result<Vec<Command>, ParseError> {
    let cmds = split_argv(argv)
        .iter()
        .map(|words| parse(words))
        .collect::<Result<Vec<_>, _>>()?;
    if cmds.is_empty() {
        return Err(ParseError::EmptyCommand);
    }
    Ok(cmds)
}

/// Splits argv into commands as tmux 3.7b's `cmd_parse_from_arguments`
/// does (recorded on `tmux -L grofe-t35 -f /dev/null`): an argument that
/// ends in `;` ends a command (`;` alone, or `a;`, which leaves `a`); one
/// that ends in `\;` is a literal argument ending in `;`. Empty commands
/// are dropped.
pub fn split_argv(argv: &[String]) -> Vec<Vec<String>> {
    let mut cmds = Vec::new();
    let mut words = Vec::new();
    for arg in argv {
        let Some(stem) = arg.strip_suffix(';') else {
            words.push(arg.clone());
            continue;
        };
        if let Some(literal) = stem.strip_suffix('\\') {
            words.push(format!("{literal};"));
            continue;
        }
        if !stem.is_empty() {
            words.push(stem.to_string());
        }
        if !words.is_empty() {
            cmds.push(std::mem::take(&mut words));
        }
    }
    if !words.is_empty() {
        cmds.push(words);
    }
    cmds
}

/// A stored command's argv made safe for [`parse_sequence`]: an argument
/// that ends in `;` (a quoted `';'` in a binding string, R12) gets the
/// `\;` that [`split_argv`] reads back as a literal, so a stored sequence
/// splits only between its commands.
pub fn escape_stored(argv: Vec<String>) -> Vec<String> {
    argv.into_iter()
        .map(|a| match a.strip_suffix(';') {
            Some(stem) => format!("{stem}\\;"),
            None => a,
        })
        .collect()
}

/// The inverse of [`escape_stored`], for printing a stored argv.
pub fn unescape_stored(arg: &str) -> String {
    match arg.strip_suffix("\\;") {
        Some(stem) => format!("{stem};"),
        None => arg.to_string(),
    }
}

/// Resolves a handful of well-known tmux short names to the full command
/// name `parse` matches on. `set`/`show` are load-bearing (spec §4.5);
/// the rest are recognised where trivially table-driven.
fn canonical_name(name: &str) -> &str {
    match name {
        "set" => "set-option",
        "show" => "show-options",
        "new" => "new-session",
        "neww" => "new-window",
        "splitw" => "split-window",
        "killp" => "kill-pane",
        "killw" => "kill-window",
        "selectp" => "select-pane",
        "selectw" => "select-window",
        "resizep" => "resize-pane",
        "respawnp" => "respawn-pane",
        "lsp" => "list-panes",
        "lsw" => "list-windows",
        "send" => "send-keys",
        "display" => "display-message",
        "capturep" => "capture-pane",
        "attach" => "attach-session",
        "bind" => "bind-key",
        "unbind" => "unbind-key",
        "lsk" => "list-keys",
        "source" => "source-file",
        "run" => "run-shell",
        "selectl" => "select-layout",
        other => other,
    }
}

/// Parses one command's argv (spec §4.5): `argv[0]` is the command name,
/// the rest are that command's own flags and positionals.
pub fn parse(argv: &[String]) -> Result<Command, ParseError> {
    let (name, rest) = argv.split_first().ok_or(ParseError::EmptyCommand)?;
    match canonical_name(name) {
        "new-session" => parse_new_session(rest),
        "kill-session" => parse_kill_session(rest),
        "has-session" => parse_has_session(rest),
        "list-sessions" => parse_list_sessions(rest),
        "new-window" => parse_new_window(rest),
        "kill-window" => parse_kill_window(rest),
        "select-window" => parse_select_window(rest),
        "list-windows" => parse_list_windows(rest),
        "split-window" => parse_split_window(rest),
        "kill-pane" => parse_kill_pane(rest),
        "select-pane" => parse_select_pane(rest),
        "resize-pane" => parse_resize_pane(rest),
        "respawn-pane" => parse_respawn_pane(rest),
        "list-panes" => parse_list_panes(rest),
        "send-keys" => parse_send_keys(rest),
        "capture-pane" => parse_capture_pane(rest),
        "set-option" => parse_set_option(rest),
        "show-options" => parse_show_options(rest),
        "display-message" => parse_display_message(rest),
        "kill-server" => parse_kill_server(rest),
        "subscribe" => parse_subscribe(rest),
        "attach-session" => parse_attach_session(rest),
        "detach-client" => parse_detach_client(rest),
        "list-clients" => parse_list_clients(rest),
        "next-window" => Ok(Command::NextWindow(parse_window_step(rest, "next-window")?)),
        "previous-window" => Ok(Command::PreviousWindow(parse_window_step(
            rest,
            "previous-window",
        )?)),
        "last-window" => Ok(Command::LastWindow(parse_window_step(rest, "last-window")?)),
        "rename-session" => Ok(Command::RenameSession(parse_rename(
            rest,
            "rename-session",
        )?)),
        "rename-window" => Ok(Command::RenameWindow(parse_rename(rest, "rename-window")?)),
        "switch-client" => parse_switch_client(rest),
        "send-prefix" => parse_send_prefix(rest),
        "bind-key" => parse_bind_key(rest),
        "unbind-key" => parse_unbind_key(rest),
        "list-keys" => parse_list_keys(rest),
        "source-file" => parse_source_file(rest),
        "reload" => parse_reload(rest),
        "command-prompt" => parse_command_prompt(rest),
        "run-shell" => parse_run_shell(rest),
        "select-layout" => parse_select_layout(rest),
        _ => Err(ParseError::UnknownCommand(name.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::target::{PanePart, PaneRef, SessionRef, WindowRef};
    use crate::types::{PaneId, WindowId};

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn empty_argv_is_an_error() {
        assert_eq!(parse(&argv(&[])), Err(ParseError::EmptyCommand));
    }

    #[test]
    fn unknown_command_error_matches_tmux() {
        let err = parse(&argv(&["foo"])).unwrap_err();
        assert_eq!(err.to_string(), "unknown command: foo");
    }

    #[test]
    fn unknown_flag_error_matches_tmux() {
        let err = parse(&argv(&["kill-server", "-z"])).unwrap_err();
        assert_eq!(err.to_string(), "command kill-server: unknown flag -z");
    }

    #[test]
    fn missing_value_error_matches_tmux() {
        let err = parse(&argv(&["kill-session", "-t"])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "command kill-session: -t expects an argument"
        );
    }

    #[test]
    fn unknown_long_flag_error_matches_tmux() {
        let err = parse(&argv(&["set-option", "--bogus", "name", "value"])).unwrap_err();
        assert_eq!(err.to_string(), "command set-option: invalid flag --");
    }

    #[test]
    fn too_many_arguments_error_matches_tmux() {
        let err = parse(&argv(&["kill-server", "extra"])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "command kill-server: too many arguments (need at most 0)"
        );
    }

    #[test]
    fn new_session_full_flags_and_trailing_command() {
        let cmd = parse(&argv(&[
            "new-session",
            "-d",
            "-s",
            "work",
            "-c",
            "/tmp",
            "-x",
            "80",
            "-y",
            "24",
            "sleep",
            "30",
        ]))
        .unwrap();
        assert_eq!(
            cmd,
            Command::NewSession(NewSession {
                detached: true,
                name: Some("work".to_string()),
                cwd: Some("/tmp".to_string()),
                width: Some(80),
                height: Some(24),
                command: Some(vec!["sleep".to_string(), "30".to_string()]),
            })
        );
    }

    #[test]
    fn new_session_attached_value_form() {
        let cmd = parse(&argv(&["new-session", "-swork"])).unwrap();
        assert_eq!(
            cmd,
            Command::NewSession(NewSession {
                name: Some("work".to_string()),
                ..Default::default()
            })
        );
    }

    #[test]
    fn new_session_bare_defaults() {
        let cmd = parse(&argv(&["new-session"])).unwrap();
        assert_eq!(cmd, Command::NewSession(NewSession::default()));
    }

    #[test]
    fn alias_new_for_new_session() {
        assert_eq!(
            parse(&argv(&["new", "-d"])).unwrap(),
            Command::NewSession(NewSession {
                detached: true,
                ..Default::default()
            })
        );
    }

    #[test]
    fn kill_session_with_target() {
        let cmd = parse(&argv(&["kill-session", "-t", "work"])).unwrap();
        assert_eq!(
            cmd,
            Command::KillSession(KillSession {
                target: Target::Session(SessionRef::Name("work".to_string())),
            })
        );
    }

    #[test]
    fn has_session_default_target_is_current() {
        let cmd = parse(&argv(&["has-session"])).unwrap();
        assert_eq!(
            cmd,
            Command::HasSession(HasSession {
                target: Target::Current
            })
        );
    }

    #[test]
    fn list_sessions_with_format() {
        let cmd = parse(&argv(&["list-sessions", "-F", "#{session_name}"])).unwrap();
        assert_eq!(
            cmd,
            Command::ListSessions(ListSessions {
                format: Some("#{session_name}".to_string()),
            })
        );
    }

    #[test]
    fn alias_lsp_for_list_panes() {
        let cmd = parse(&argv(&["lsp", "-t", "work"])).unwrap();
        assert_eq!(
            cmd,
            Command::ListPanes(ListPanes {
                target: Target::Session(SessionRef::Name("work".to_string())),
                format: None,
            })
        );
    }

    #[test]
    fn new_window_full() {
        let cmd = parse(&argv(&[
            "new-window",
            "-d",
            "-t",
            "work",
            "-n",
            "editor",
            "-c",
            "/tmp",
            "vim",
        ]))
        .unwrap();
        assert_eq!(
            cmd,
            Command::NewWindow(NewWindow {
                detached: true,
                target: Target::Session(SessionRef::Name("work".to_string())),
                name: Some("editor".to_string()),
                cwd: Some("/tmp".to_string()),
                command: Some(vec!["vim".to_string()]),
            })
        );
    }

    #[test]
    fn split_window_defaults_to_vertical_and_half() {
        let cmd = parse(&argv(&["split-window"])).unwrap();
        assert_eq!(
            cmd,
            Command::SplitWindow(SplitWindow {
                dir: Dir::Vertical,
                cwd: None,
                size: SplitSize::Half,
                target: Target::Current,
                command: None,
            })
        );
    }

    #[test]
    fn split_window_horizontal_with_percent_and_cwd_format() {
        let cmd = parse(&argv(&[
            "split-window",
            "-h",
            "-p",
            "30",
            "-c",
            "#{pane_current_path}",
        ]))
        .unwrap();
        assert_eq!(
            cmd,
            Command::SplitWindow(SplitWindow {
                dir: Dir::Horizontal,
                cwd: Some("#{pane_current_path}".to_string()),
                size: SplitSize::Percent(30),
                target: Target::Current,
                command: None,
            })
        );
    }

    #[test]
    fn split_window_with_cells() {
        let cmd = parse(&argv(&["split-window", "-l", "10"])).unwrap();
        assert_eq!(
            cmd,
            Command::SplitWindow(SplitWindow {
                dir: Dir::Vertical,
                cwd: None,
                size: SplitSize::Cells(10),
                target: Target::Current,
                command: None,
            })
        );
    }

    #[test]
    fn split_window_rejects_both_h_and_v() {
        assert!(matches!(
            parse(&argv(&["split-window", "-h", "-v"])),
            Err(ParseError::Other(_))
        ));
    }

    #[test]
    fn split_window_rejects_both_p_and_l() {
        assert!(matches!(
            parse(&argv(&["split-window", "-p", "50", "-l", "10"])),
            Err(ParseError::Other(_))
        ));
    }

    #[test]
    fn split_window_trailing_shell_command_keeps_its_own_flags() {
        let cmd = parse(&argv(&["split-window", "-h", "ls", "-la"])).unwrap();
        assert_eq!(
            cmd,
            Command::SplitWindow(SplitWindow {
                dir: Dir::Horizontal,
                cwd: None,
                size: SplitSize::Half,
                target: Target::Current,
                command: Some(vec!["ls".to_string(), "-la".to_string()]),
            })
        );
    }

    #[test]
    fn kill_pane_target_pane_form() {
        let cmd = parse(&argv(&["kill-pane", "-t", "work:1.2"])).unwrap();
        assert_eq!(
            cmd,
            Command::KillPane(KillPane {
                target: Target::Pane(PaneRef {
                    session: SessionRef::Name("work".to_string()),
                    window: WindowRef::Index(1),
                    pane: PanePart::Index(2),
                }),
            })
        );
    }

    #[test]
    fn select_pane_combined_bool_flags_conflict() {
        assert!(matches!(
            parse(&argv(&["select-pane", "-LR"])),
            Err(ParseError::Other(_))
        ));
    }

    #[test]
    fn select_pane_single_direction() {
        let cmd = parse(&argv(&["select-pane", "-L", "-t", "%3"])).unwrap();
        assert_eq!(
            cmd,
            Command::SelectPane(SelectPane {
                edge: Some(Edge::Left),
                target: Target::Pane(PaneRef {
                    session: SessionRef::Current,
                    window: WindowRef::Current,
                    pane: PanePart::Id(PaneId(3)),
                }),
            })
        );
    }

    #[test]
    fn select_pane_no_direction_just_activates() {
        let cmd = parse(&argv(&["select-pane", "-t", "%3"])).unwrap();
        assert_eq!(
            cmd,
            Command::SelectPane(SelectPane {
                edge: None,
                target: Target::Pane(PaneRef {
                    session: SessionRef::Current,
                    window: WindowRef::Current,
                    pane: PanePart::Id(PaneId(3)),
                }),
            })
        );
    }

    #[test]
    fn select_pane_rejects_trailing_positionals() {
        let err = parse(&argv(&["select-pane", "-t", "%3", "extra"])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "command select-pane: too many arguments (need at most 0)"
        );
    }

    #[test]
    fn resize_pane_with_explicit_cell_count() {
        let cmd = parse(&argv(&["resize-pane", "-L", "5"])).unwrap();
        assert_eq!(
            cmd,
            Command::ResizePane(ResizePane {
                action: ResizeAction::Edges(vec![Edge::Left], 5),
                target: Target::Current,
            })
        );
    }

    #[test]
    fn resize_pane_defaults_to_one_cell() {
        let cmd = parse(&argv(&["resize-pane", "-D"])).unwrap();
        assert_eq!(
            cmd,
            Command::ResizePane(ResizePane {
                action: ResizeAction::Edges(vec![Edge::Down], 1),
                target: Target::Current,
            })
        );
    }

    /// tmux: `-L`/`-R`/`-U`/`-D` are boolean flags, so `-LR` sets both
    /// directions, sharing the one trailing amount.
    #[test]
    fn resize_pane_combined_direction_flags() {
        let cmd = parse(&argv(&["resize-pane", "-LR", "5"])).unwrap();
        assert_eq!(
            cmd,
            Command::ResizePane(ResizePane {
                action: ResizeAction::Edges(vec![Edge::Left, Edge::Right], 5),
                target: Target::Current,
            })
        );
    }

    /// tmux: with only one shared trailing amount, `-L 3 -R 5` has two
    /// positionals for a command that takes at most one.
    #[test]
    fn resize_pane_two_amounts_is_too_many_arguments() {
        let err = parse(&argv(&["resize-pane", "-L", "3", "-R", "5"])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "command resize-pane: too many arguments (need at most 1)"
        );
    }

    #[test]
    fn resize_pane_zoom_toggle() {
        let cmd = parse(&argv(&["resize-pane", "-Z", "-t", "%1"])).unwrap();
        assert_eq!(
            cmd,
            Command::ResizePane(ResizePane {
                action: ResizeAction::ToggleZoom,
                target: Target::Pane(PaneRef {
                    session: SessionRef::Current,
                    window: WindowRef::Current,
                    pane: PanePart::Id(PaneId(1)),
                }),
            })
        );
    }

    #[test]
    fn resize_pane_requires_a_direction_or_zoom() {
        assert!(matches!(
            parse(&argv(&["resize-pane"])),
            Err(ParseError::Other(_))
        ));
    }

    #[test]
    fn resize_pane_rejects_zoom_with_direction() {
        assert!(matches!(
            parse(&argv(&["resize-pane", "-Z", "-L"])),
            Err(ParseError::Other(_))
        ));
    }

    #[test]
    fn respawn_pane_kill_and_command() {
        let cmd = parse(&argv(&["respawn-pane", "-k", "-t", "%2", "bash"])).unwrap();
        assert_eq!(
            cmd,
            Command::RespawnPane(RespawnPane {
                kill: true,
                target: Target::Pane(PaneRef {
                    session: SessionRef::Current,
                    window: WindowRef::Current,
                    pane: PanePart::Id(PaneId(2)),
                }),
                command: Some(vec!["bash".to_string()]),
            })
        );
    }

    #[test]
    fn send_keys_classifies_key_names_and_literal_text() {
        let cmd = parse(&argv(&["send-keys", "-t", "%1", "hello", "Enter", "C-c"])).unwrap();
        let Command::SendKeys(send) = cmd else {
            panic!("expected SendKeys");
        };
        assert_eq!(
            send.target,
            Target::Pane(PaneRef {
                session: SessionRef::Current,
                window: WindowRef::Current,
                pane: PanePart::Id(PaneId(1)),
            })
        );
        assert_eq!(
            send.items,
            vec![
                SendKeysItem::Literal("hello".to_string()),
                SendKeysItem::Key("Enter".to_string()),
                SendKeysItem::Key("C-c".to_string()),
            ]
        );
    }

    #[test]
    fn send_keys_literal_flag_forces_literal() {
        let cmd = parse(&argv(&["send-keys", "-l", "Enter"])).unwrap();
        let Command::SendKeys(send) = cmd else {
            panic!("expected SendKeys");
        };
        assert_eq!(send.items, vec![SendKeysItem::Literal("Enter".to_string())]);
    }

    #[test]
    fn send_keys_hex_flag() {
        let cmd = parse(&argv(&["send-keys", "-H", "1b", "5b", "41"])).unwrap();
        let Command::SendKeys(send) = cmd else {
            panic!("expected SendKeys");
        };
        assert_eq!(
            send.items,
            vec![
                SendKeysItem::Hex(0x1b),
                SendKeysItem::Hex(0x5b),
                SendKeysItem::Hex(0x41),
            ]
        );
    }

    #[test]
    fn send_keys_hex_rejects_bad_byte() {
        assert!(matches!(
            parse(&argv(&["send-keys", "-H", "zz"])),
            Err(ParseError::Other(_))
        ));
    }

    #[test]
    fn send_keys_rejects_l_and_h_together() {
        assert!(matches!(
            parse(&argv(&["send-keys", "-l", "-H", "41"])),
            Err(ParseError::Other(_))
        ));
    }

    #[test]
    fn alias_send_for_send_keys() {
        let cmd = parse(&argv(&["send", "Enter"])).unwrap();
        assert!(matches!(cmd, Command::SendKeys(_)));
    }

    #[test]
    fn capture_pane_with_history_start_and_visible_end() {
        let cmd = parse(&argv(&["capture-pane", "-p", "-S", "-", "-E", "-"])).unwrap();
        assert_eq!(
            cmd,
            Command::CapturePane(CapturePane {
                target: Target::Current,
                print: true,
                opts: CaptureOpts {
                    start: Some(CaptureBound::HistoryStart),
                    end: Some(CaptureBound::VisibleEnd),
                    escapes: false,
                },
            })
        );
    }

    #[test]
    fn capture_pane_with_negative_line_numbers() {
        let cmd = parse(&argv(&["capture-pane", "-S", "-5", "-E", "0"])).unwrap();
        assert_eq!(
            cmd,
            Command::CapturePane(CapturePane {
                target: Target::Current,
                print: false,
                opts: CaptureOpts {
                    start: Some(CaptureBound::Line(-5)),
                    end: Some(CaptureBound::Line(0)),
                    escapes: false,
                },
            })
        );
    }

    #[test]
    fn capture_pane_defaults_are_none() {
        let cmd = parse(&argv(&["capture-pane"])).unwrap();
        assert_eq!(
            cmd,
            Command::CapturePane(CapturePane {
                target: Target::Current,
                print: false,
                opts: CaptureOpts {
                    start: None,
                    end: None,
                    escapes: false,
                },
            })
        );
    }

    #[test]
    fn capture_pane_rejects_trailing_positionals() {
        let err = parse(&argv(&["capture-pane", "-p", "extra"])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "command capture-pane: too many arguments (need at most 0)"
        );
    }

    #[test]
    fn set_option_alias_and_scope_and_persist() {
        let cmd = parse(&argv(&["set", "-g", "--persist", "history-limit", "50000"])).unwrap();
        assert_eq!(
            cmd,
            Command::SetOption(SetOption {
                scope: Some(OptionScope::Global),
                unset: false,
                persist: true,
                target: Target::Current,
                name: "history-limit".to_string(),
                value: Some("50000".to_string()),
            })
        );
    }

    #[test]
    fn set_option_unset_with_no_value() {
        let cmd = parse(&argv(&["set-option", "-u", "@my-opt"])).unwrap();
        assert_eq!(
            cmd,
            Command::SetOption(SetOption {
                scope: None,
                unset: true,
                persist: false,
                target: Target::Current,
                name: "@my-opt".to_string(),
                value: None,
            })
        );
    }

    #[test]
    fn set_option_requires_a_name() {
        assert!(matches!(
            parse(&argv(&["set-option"])),
            Err(ParseError::MissingArgument(_))
        ));
    }

    #[test]
    fn set_option_too_many_positionals() {
        assert!(matches!(
            parse(&argv(&["set-option", "name", "value", "extra"])),
            Err(ParseError::TooManyArguments(_, _))
        ));
    }

    /// tmux's real `options_scope_from_flags` (options.c) never rejects a
    /// flag combination; it picks by a fixed precedence instead. Verified
    /// against tmux 3.7b's source and live behaviour (`set -wp`/`set -pw`
    /// land on the pane, not the window): `-p` beats `-w` when both are
    /// given, order of the flags doesn't matter.
    #[test]
    fn set_option_w_and_p_together_is_pane() {
        let cmd = parse(&argv(&["set-option", "-wp", "name", "value"])).unwrap();
        let Command::SetOption(set) = cmd else {
            panic!("expected SetOption");
        };
        assert_eq!(set.scope, Some(OptionScope::Pane));
    }

    #[test]
    fn set_option_p_and_w_together_is_pane_regardless_of_flag_order() {
        let cmd = parse(&argv(&["set-option", "-pw", "name", "value"])).unwrap();
        let Command::SetOption(set) = cmd else {
            panic!("expected SetOption");
        };
        assert_eq!(set.scope, Some(OptionScope::Pane));
    }

    /// tmux checks `-s` first and returns immediately, before ever
    /// looking at `-p`/`-w`; grofe's `-s` is its own Session scope (not
    /// tmux's real server scope), so `-sw`/`-sp` land on Session.
    #[test]
    fn set_option_s_and_w_together_is_session() {
        let cmd = parse(&argv(&["set-option", "-sw", "name", "value"])).unwrap();
        let Command::SetOption(set) = cmd else {
            panic!("expected SetOption");
        };
        assert_eq!(set.scope, Some(OptionScope::Session));
    }

    #[test]
    fn set_option_s_and_p_together_is_session() {
        let cmd = parse(&argv(&["set-option", "-sp", "name", "value"])).unwrap();
        let Command::SetOption(set) = cmd else {
            panic!("expected SetOption");
        };
        assert_eq!(set.scope, Some(OptionScope::Session));
    }

    /// tmux: `-g` combined with `-w`/`-p`/`-s` is a common idiom (`set -gw
    /// ...`) and always succeeds; grofe has one flat Global scope, so `-g`
    /// wins over whatever else was given.
    #[test]
    fn set_option_g_with_w_is_global() {
        let cmd = parse(&argv(&["set-option", "-gw", "name", "value"])).unwrap();
        let Command::SetOption(set) = cmd else {
            panic!("expected SetOption");
        };
        assert_eq!(set.scope, Some(OptionScope::Global));
    }

    #[test]
    fn set_option_g_with_p_is_global() {
        let cmd = parse(&argv(&["set-option", "-gp", "name", "value"])).unwrap();
        let Command::SetOption(set) = cmd else {
            panic!("expected SetOption");
        };
        assert_eq!(set.scope, Some(OptionScope::Global));
    }

    #[test]
    fn set_option_g_with_s_is_global() {
        let cmd = parse(&argv(&["set-option", "-gs", "name", "value"])).unwrap();
        let Command::SetOption(set) = cmd else {
            panic!("expected SetOption");
        };
        assert_eq!(set.scope, Some(OptionScope::Global));
    }

    #[test]
    fn show_options_alias_and_scope() {
        let cmd = parse(&argv(&["show", "-w"])).unwrap();
        assert_eq!(
            cmd,
            Command::ShowOptions(ShowOptions {
                scope: Some(OptionScope::Window),
                target: Target::Current,
                name: None,
            })
        );
    }

    #[test]
    fn show_options_g_with_w_is_global() {
        let cmd = parse(&argv(&["show-options", "-gw"])).unwrap();
        assert_eq!(
            cmd,
            Command::ShowOptions(ShowOptions {
                scope: Some(OptionScope::Global),
                target: Target::Current,
                name: None,
            })
        );
    }

    #[test]
    fn display_message_print_and_text() {
        let cmd = parse(&argv(&["display-message", "-p", "#{pane_uid}"])).unwrap();
        assert_eq!(
            cmd,
            Command::DisplayMessage(DisplayMessage {
                print: true,
                target: Target::Current,
                text: Some("#{pane_uid}".to_string()),
            })
        );
    }

    #[test]
    fn display_message_too_many_positionals() {
        assert!(matches!(
            parse(&argv(&["display-message", "one", "two"])),
            Err(ParseError::TooManyArguments(_, _))
        ));
    }

    #[test]
    fn kill_server_bare() {
        assert_eq!(
            parse(&argv(&["kill-server"])).unwrap(),
            Command::KillServer(KillServer)
        );
    }

    #[test]
    fn subscribe_with_event_names() {
        let cmd = parse(&argv(&["subscribe", "pane-exited", "pane-output"])).unwrap();
        assert_eq!(
            cmd,
            Command::Subscribe(Subscribe {
                events: vec!["pane-exited".to_string(), "pane-output".to_string()],
            })
        );
    }

    #[test]
    fn subscribe_with_no_events_means_all() {
        let cmd = parse(&argv(&["subscribe"])).unwrap();
        assert_eq!(cmd, Command::Subscribe(Subscribe { events: vec![] }));
    }

    #[test]
    fn double_dash_ends_flags() {
        let cmd = parse(&argv(&["new-window", "-t", "work", "--", "-x"])).unwrap();
        assert_eq!(
            cmd,
            Command::NewWindow(NewWindow {
                detached: false,
                target: Target::Session(SessionRef::Name("work".to_string())),
                name: None,
                cwd: None,
                command: Some(vec!["-x".to_string()]),
            })
        );
    }

    #[test]
    fn window_id_target_has_no_session() {
        let cmd = parse(&argv(&["select-window", "-t", "@5"])).unwrap();
        assert_eq!(
            cmd,
            Command::SelectWindow(SelectWindow {
                target: Target::Window(SessionRef::Current, WindowRef::Id(WindowId(5))),
            })
        );
    }

    /// Every M1 command name (and its listed aliases) parses at all, with
    /// a minimal argv. This is the "every M1 command parses" acceptance
    /// check as one table, on top of the per-command detail tests above.
    #[test]
    fn every_m1_command_parses_with_a_bare_target() {
        let cases: &[&[&str]] = &[
            &["new-session"],
            &["kill-session"],
            &["has-session"],
            &["list-sessions"],
            &["new-window"],
            &["kill-window"],
            &["select-window"],
            &["list-windows"],
            &["split-window"],
            &["kill-pane"],
            &["select-pane"],
            &["resize-pane", "-Z"],
            &["respawn-pane"],
            &["list-panes"],
            &["send-keys", "Enter"],
            &["capture-pane"],
            &["set-option", "name", "value"],
            &["show-options"],
            &["display-message"],
            &["kill-server"],
            &["subscribe"],
        ];
        for case in cases {
            let result = parse(&argv(case));
            assert!(result.is_ok(), "{case:?} failed: {result:?}");
        }
    }

    fn session(name: &str) -> Target {
        Target::Session(SessionRef::Name(name.to_string()))
    }

    #[test]
    fn attach_session_flags_and_alias() {
        let want = Command::AttachSession(AttachSession {
            target: session("work"),
            detach_others: true,
            nest: true,
        });
        let full = ["attach-session", "-d", "-t", "work", "--nest"];
        assert_eq!(parse(&argv(&full)), Ok(want.clone()));
        let short = ["attach", "-dtwork", "--nest"];
        assert_eq!(parse(&argv(&short)), Ok(want));
        assert_eq!(
            parse(&argv(&["attach"])),
            Ok(Command::AttachSession(AttachSession {
                target: Target::Current,
                detach_others: false,
                nest: false,
            }))
        );
    }

    /// tmux's `-f` takes client flags; natively it is not a flag at all
    /// (`--nest` forces nesting), so the tmux front end can own it.
    #[test]
    fn attach_session_rejects_dash_f() {
        let err = parse(&argv(&["attach", "-f"])).unwrap_err();
        assert_eq!(err.to_string(), "command attach-session: unknown flag -f");
    }

    #[test]
    fn detach_client_flags() {
        assert_eq!(
            parse(&argv(&["detach-client", "-a", "-s", "work", "-t", "c1"])),
            Ok(Command::DetachClient(DetachClient {
                session: Some(session("work")),
                client: Some("c1".to_string()),
                all_others: true,
            }))
        );
        assert_eq!(
            parse(&argv(&["detach-client"])),
            Ok(Command::DetachClient(DetachClient {
                session: None,
                client: None,
                all_others: false,
            }))
        );
    }

    #[test]
    fn list_clients_with_format() {
        assert_eq!(
            parse(&argv(&["list-clients", "-F", "#{client_width}"])),
            Ok(Command::ListClients(ListClients {
                format: Some("#{client_width}".to_string()),
            }))
        );
    }

    #[test]
    fn window_steps_take_a_session_target() {
        let step = WindowStep {
            target: session("s"),
        };
        assert_eq!(
            parse(&argv(&["next-window", "-t", "s"])),
            Ok(Command::NextWindow(step.clone()))
        );
        assert_eq!(
            parse(&argv(&["previous-window", "-t", "s"])),
            Ok(Command::PreviousWindow(step.clone()))
        );
        assert_eq!(
            parse(&argv(&["last-window", "-t", "s"])),
            Ok(Command::LastWindow(step))
        );
    }

    #[test]
    fn renames_take_a_target_and_one_name() {
        assert_eq!(
            parse(&argv(&["rename-session", "-t", "a", "b"])),
            Ok(Command::RenameSession(Rename {
                target: session("a"),
                name: "b".to_string(),
            }))
        );
        assert_eq!(
            parse(&argv(&["rename-window", "logs"])),
            Ok(Command::RenameWindow(Rename {
                target: Target::Current,
                name: "logs".to_string(),
            }))
        );
        let err = parse(&argv(&["rename-window"])).unwrap_err();
        assert_eq!(err.to_string(), "command rename-window: missing argument");
        let err = parse(&argv(&["rename-session", "a", "b"])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "command rename-session: too many arguments (need at most 1)"
        );
    }

    #[test]
    fn switch_client_flags() {
        assert_eq!(
            parse(&argv(&[
                "switch-client",
                "-l",
                "-n",
                "-p",
                "-t",
                "s",
                "-T",
                "prefix"
            ])),
            Ok(Command::SwitchClient(SwitchClient {
                target: Some(session("s")),
                last: true,
                next: true,
                previous: true,
                key_table: Some("prefix".to_string()),
            }))
        );
        assert_eq!(
            parse(&argv(&["switch-client"])),
            Ok(Command::SwitchClient(SwitchClient {
                target: None,
                last: false,
                next: false,
                previous: false,
                key_table: None,
            }))
        );
    }

    #[test]
    fn send_prefix_takes_a_pane_target() {
        assert_eq!(
            parse(&argv(&["send-prefix"])),
            Ok(Command::SendPrefix(SendPrefix {
                target: Target::Current,
            }))
        );
    }

    #[test]
    fn capture_pane_e_sets_escapes() {
        let Ok(Command::CapturePane(c)) = parse(&argv(&["capture-pane", "-e", "-p"])) else {
            panic!("capture-pane -e did not parse");
        };
        assert!(c.opts.escapes);
        assert!(c.print);
    }

    // ---- M3 commands (T3.0) ----

    /// Quotes, escapes, `;` and expansion, as tmux 3.7b's string parser
    /// reads them (recorded with `bind X '<string>'` then `list-keys` on
    /// `tmux -L grofe-t30 -f /dev/null`).
    #[test]
    fn split_command_string_table() {
        let home = std::env::var("HOME").expect("HOME");
        let path = std::env::var("PATH").expect("PATH");
        let root = nix::unistd::User::from_name("root")
            .ok()
            .flatten()
            .map(|u| u.dir.display().to_string())
            .expect("root's home");
        let cases: Vec<(&str, Vec<Vec<String>>)> = vec![
            ("", vec![]),
            ("   ", vec![]),
            ("split-window -h", vec![argv(&["split-window", "-h"])]),
            (
                "split-window ; select-pane -L",
                vec![argv(&["split-window"]), argv(&["select-pane", "-L"])],
            ),
            // A `;` ends the command even inside a word.
            (
                "split-window;select-pane -L",
                vec![argv(&["split-window"]), argv(&["select-pane", "-L"])],
            ),
            (
                "split-window; select-pane -L",
                vec![argv(&["split-window"]), argv(&["select-pane", "-L"])],
            ),
            // A `\;` word is a literal `;` argument; a trailing one is
            // dropped.
            (
                "display a \\; display b",
                vec![argv(&["display", "a", ";", "display", "b"])],
            ),
            ("display a \\;", vec![argv(&["display", "a"])]),
            ("display a \\;  ", vec![argv(&["display", "a"])]),
            ("display \\;b", vec![argv(&["display", ";b"])]),
            // Quoted or escaped inside a word, `;` is literal.
            ("display 'a;b'", vec![argv(&["display", "a;b"])]),
            ("display \"a;b\"", vec![argv(&["display", "a;b"])]),
            ("display a\\;b", vec![argv(&["display", "a;b"])]),
            ("display ';'", vec![argv(&["display", ";"])]),
            // Empty commands vanish.
            ("a ;; b ;", vec![argv(&["a"]), argv(&["b"])]),
            ("; a", vec![argv(&["a"])]),
            // Quotes group, join with what touches them, and may be empty.
            (
                "run-shell 'touch /tmp/x'",
                vec![argv(&["run-shell", "touch /tmp/x"])],
            ),
            ("rename-window \"%%\"", vec![argv(&["rename-window", "%%"])]),
            (
                "set status-right ''",
                vec![argv(&["set", "status-right", ""])],
            ),
            ("display 'it'\"'\"'s'", vec![argv(&["display", "it's"])]),
            (
                "display \"say \\\"hi\\\"\"",
                vec![argv(&["display", "say \"hi\""])],
            ),
            (
                "display 'no \\escape'",
                vec![argv(&["display", "no \\escape"])],
            ),
            ("a\\ b\tc", vec![argv(&["a b", "c"])]),
            ("display \"a\\\\b\"", vec![argv(&["display", "a\\b"])]),
            // An unterminated quote closes at the end.
            (
                "display 'unterminated",
                vec![argv(&["display", "unterminated"])],
            ),
            (
                "display \"unterminated",
                vec![argv(&["display", "unterminated"])],
            ),
            // Expansion: variables and a leading `~`, not in single quotes.
            ("display \"$HOME\"", vec![argv(&["display", &home])]),
            ("display '$HOME'", vec![argv(&["display", "$HOME"])]),
            ("display \\$HOME", vec![argv(&["display", "$HOME"])]),
            ("display ~", vec![argv(&["display", &home])]),
            (
                "display \"${HOME}x\"",
                vec![argv(&["display", &format!("{home}x")])],
            ),
            (
                "display $PATH/x",
                vec![argv(&["display", &format!("{path}/x")])],
            ),
            (
                "display \"$GROFE_T30_UNSET\"y",
                vec![argv(&["display", "y"])],
            ),
            (
                "display ~/a",
                vec![argv(&["display", &format!("{home}/a")])],
            ),
            ("display \"~\"", vec![argv(&["display", &home])]),
            ("display a~ '~'", vec![argv(&["display", "a~", "~"])]),
            ("display ~root", vec![argv(&["display", &root])]),
            ("display $ a$", vec![argv(&["display", "$", "a$"])]),
        ];
        for (input, want) in cases {
            assert_eq!(split_command_string(input), Ok(want), "{input:?}");
        }
    }

    #[test]
    fn parse_sequence_splits_at_semicolon_arguments() {
        assert_eq!(
            parse_sequence(&argv(&["kill-server"])),
            Ok(vec![Command::KillServer(KillServer)])
        );
        assert_eq!(parse_sequence(&argv(&[])), Err(ParseError::EmptyCommand));
        assert_eq!(parse_sequence(&argv(&[";"])), Err(ParseError::EmptyCommand));
        let two = parse_sequence(&argv(&["kill-server", ";", "kill-server"]));
        assert_eq!(two.map(|c| c.len()), Ok(2));
        // A parse error anywhere fails the whole sequence.
        assert_eq!(
            parse_sequence(&argv(&["kill-server", ";", "no-such"])),
            Err(ParseError::UnknownCommand("no-such".to_string()))
        );
    }

    /// Recorded on tmux 3.7b: `display -p 'a;' display -p b` prints `a`
    /// then `b`; `display -p 'a\;'` prints `a;`.
    #[test]
    fn split_argv_follows_tmux() {
        let cases: &[(&[&str], &[&[&str]])] = &[
            (&["a", "b"], &[&["a", "b"]]),
            (&["a", ";", "b", "-L"], &[&["a"], &["b", "-L"]]),
            (&["a", "x;", "b"], &[&["a", "x"], &["b"]]),
            (&["a", "x\\;"], &[&["a", "x;"]]),
            (&["a", "\\;", "b"], &[&["a", ";", "b"]]),
            (&[";", "a", ";", ";"], &[&["a"]]),
            (&["a", "x;y"], &[&["a", "x;y"]]),
        ];
        for (given, want) in cases {
            let want: Vec<Vec<String>> = want.iter().map(|c| argv(c)).collect();
            assert_eq!(split_argv(&argv(given)), want, "{given:?}");
        }
    }

    /// R12: a stored `';'` stays one literal argument through the split.
    #[test]
    fn stored_arguments_survive_the_argv_split() {
        let words = ["send-keys", ";", "a;", "b\\;", "c"];
        let stored = escape_stored(argv(&words));
        assert_eq!(split_argv(&stored), vec![argv(&words)]);
        let shown: Vec<String> = stored.iter().map(|a| unescape_stored(a)).collect();
        assert_eq!(shown, argv(&words));
    }

    #[test]
    fn bind_key_flags_and_command() {
        assert_eq!(
            parse(&argv(&["bind-key", "X", "split-window", "-h"])),
            Ok(Command::BindKey(BindKey {
                table: "prefix".to_string(),
                repeat: false,
                note: None,
                key: "X".to_string(),
                command: argv(&["split-window", "-h"]),
            }))
        );
        assert_eq!(
            parse(&argv(&[
                "bind",
                "-nr",
                "-N",
                "a note",
                "M-h",
                "select-pane"
            ])),
            Ok(Command::BindKey(BindKey {
                table: "root".to_string(),
                repeat: true,
                note: Some("a note".to_string()),
                key: "M-h".to_string(),
                command: argv(&["select-pane"]),
            }))
        );
        // `-T` wins over `-n`, as in tmux.
        let Ok(Command::BindKey(c)) = parse(&argv(&["bind-key", "-n", "-T", "copy", "q", "x"]))
        else {
            panic!("bind-key -n -T");
        };
        assert_eq!(c.table, "copy");
        assert_eq!(
            parse(&argv(&["bind-key"])),
            Err(ParseError::MissingArgument("bind-key".to_string()))
        );
    }

    #[test]
    fn unbind_key_needs_a_key_unless_all() {
        assert_eq!(
            parse(&argv(&["unbind", "-n", "M-h"])),
            Ok(Command::UnbindKey(UnbindKey {
                table: "root".to_string(),
                all: false,
                key: Some("M-h".to_string()),
            }))
        );
        assert_eq!(
            parse(&argv(&["unbind-key", "-a", "-T", "copy"])),
            Ok(Command::UnbindKey(UnbindKey {
                table: "copy".to_string(),
                all: true,
                key: None,
            }))
        );
        assert!(parse(&argv(&["unbind-key"])).is_err());
        assert!(parse(&argv(&["unbind-key", "-a", "x"])).is_err());
    }

    #[test]
    fn list_keys_source_file_and_reload() {
        assert_eq!(
            parse(&argv(&["lsk", "-T", "prefix"])),
            Ok(Command::ListKeys(ListKeys {
                table: Some("prefix".to_string()),
            }))
        );
        assert_eq!(
            parse(&argv(&["source", "-q", "sub.toml"])),
            Ok(Command::SourceFile(SourceFile {
                quiet: true,
                path: "sub.toml".to_string(),
            }))
        );
        assert!(parse(&argv(&["source-file"])).is_err());
        assert_eq!(parse(&argv(&["reload"])), Ok(Command::Reload(Reload)));
        assert!(parse(&argv(&["reload", "x"])).is_err());
    }

    #[test]
    fn command_prompt_and_run_shell() {
        assert_eq!(
            parse(&argv(&[
                "command-prompt",
                "-I",
                "old",
                "-p",
                "name",
                "rename-window '%%'"
            ])),
            Ok(Command::CommandPrompt(CommandPrompt {
                prompt: Some("name".to_string()),
                initial: Some("old".to_string()),
                template: Some("rename-window '%%'".to_string()),
            }))
        );
        assert_eq!(
            parse(&argv(&["command-prompt"])),
            Ok(Command::CommandPrompt(CommandPrompt {
                prompt: None,
                initial: None,
                template: None,
            }))
        );
        assert_eq!(
            parse(&argv(&["run", "echo hi; echo two"])),
            Ok(Command::RunShell(RunShell {
                cmd: "echo hi; echo two".to_string(),
            }))
        );
        assert!(parse(&argv(&["run-shell"])).is_err());
    }

    #[test]
    fn select_layout_takes_a_name_and_a_target() {
        assert_eq!(
            parse(&argv(&["selectl", "-t", "%1", "main-vertical"])),
            Ok(Command::SelectLayout(SelectLayout {
                target: parse_target("%1").expect("target"),
                name: "main-vertical".to_string(),
            }))
        );
        assert!(parse(&argv(&["select-layout"])).is_err());
    }
}
