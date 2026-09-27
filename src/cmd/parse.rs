//! argv -> [`Command`] (spec §4.5). Hand-rolled, tmux-style: short flags
//! may be combined (`-dP`), a flag's value may be attached (`-tfoo`) or a
//! separate token (`-t foo`), and `--` ends flag parsing. `set`/`show` are
//! recognised as aliases for `set-option`/`show-options`; a handful of
//! other well-known tmux short names are recognised too.

use std::collections::{BTreeMap, BTreeSet};

use super::keys;
use super::target::parse_target;
use super::{
    CapturePane, Command, DisplayMessage, HasSession, KillPane, KillServer, KillSession,
    KillWindow, ListPanes, ListSessions, ListWindows, NewSession, NewWindow, OptionScope,
    ParseError, ResizeAction, ResizePane, RespawnPane, SelectPane, SelectWindow, SendKeys,
    SendKeysItem, SetOption, ShowOptions, SplitWindow, Subscribe, Target,
};
use crate::types::{CaptureBound, CaptureOpts, Dir, Edge, SplitSize};

/// Whether a short/long flag takes a value, and if so whether it's
/// required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlagKind {
    Bool,
    /// A value is required, attached (`-tfoo`) or the next token (`-t
    /// foo`).
    Value,
    /// A value may be attached, or taken from the next token if that
    /// token doesn't itself look like a flag; otherwise the flag is
    /// present with no captured value (the caller applies its own
    /// default, e.g. `resize-pane -L` means "by 1 cell").
    OptionalValue,
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
fn tokenize(argv: &[String], table: &FlagTable) -> Result<ParsedFlags, ParseError> {
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
                // No M1 long flag currently takes a value; if one is
                // added, wire attached/separate value handling in here.
                _ => return Err(ParseError::UnknownLongFlag(name.to_string())),
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
                    None => return Err(ParseError::UnknownFlag(c)),
                    Some(FlagKind::Bool) => {
                        flags.present.insert(c);
                        ci += 1;
                    }
                    Some(FlagKind::Value) => {
                        flags.present.insert(c);
                        let attached: String = chars[ci + 1..].iter().collect();
                        if attached.is_empty() {
                            i += 1;
                            let value = argv.get(i).ok_or(ParseError::MissingValue(c))?;
                            flags.values.insert(c, value.clone());
                        } else {
                            flags.values.insert(c, attached);
                        }
                        ci = chars.len();
                    }
                    Some(FlagKind::OptionalValue) => {
                        flags.present.insert(c);
                        let attached: String = chars[ci + 1..].iter().collect();
                        if !attached.is_empty() {
                            flags.values.insert(c, attached);
                        } else if let Some(next) = argv.get(i + 1)
                            && !next.starts_with('-')
                        {
                            flags.values.insert(c, next.clone());
                            i += 1;
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

fn no_extra_positionals(flags: &ParsedFlags, cmd_name: &str) -> Result<(), ParseError> {
    if flags.positionals.is_empty() {
        Ok(())
    } else {
        Err(ParseError::TooManyArguments(cmd_name.to_string()))
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
    )?;
    let width = flags
        .values
        .get(&'x')
        .map(|s| parse_u16(s, "width"))
        .transpose()?;
    let height = flags
        .values
        .get(&'y')
        .map(|s| parse_u16(s, "height"))
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
    )?;
    no_extra_positionals(&flags, cmd_name)?;
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
    )?;
    no_extra_positionals(&flags, "list-sessions")?;
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
    )?;
    no_extra_positionals(&flags, "list-windows")?;
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
    )?;
    no_extra_positionals(&flags, "list-panes")?;
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
    )?;
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

const RESIZE_PANE_SHORT: &[(char, FlagKind)] = &[
    ('L', FlagKind::OptionalValue),
    ('R', FlagKind::OptionalValue),
    ('U', FlagKind::OptionalValue),
    ('D', FlagKind::OptionalValue),
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
    )?;
    let edges: Vec<char> = ['L', 'R', 'U', 'D']
        .into_iter()
        .filter(|c| flags.present.contains(c))
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
        match edges.as_slice() {
            [] => {
                return Err(ParseError::Other(
                    "resize-pane: expected one of -L -R -U -D -Z".to_string(),
                ));
            }
            [c] => {
                let edge = match *c {
                    'L' => Edge::Left,
                    'R' => Edge::Right,
                    'U' => Edge::Up,
                    'D' => Edge::Down,
                    _ => unreachable!("edges is built from exactly L/R/U/D"),
                };
                let amount = match flags.values.get(c) {
                    Some(s) => parse_u16(s, "resize amount")?,
                    None => 1,
                };
                ResizeAction::Edge(edge, amount)
            }
            _ => {
                return Err(ParseError::Other(
                    "resize-pane: only one direction allowed".to_string(),
                ));
            }
        }
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
    )?;
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
            escapes: false, // -e is M2 (spec §4.5).
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

fn parse_scope(flags: &ParsedFlags) -> Result<Option<OptionScope>, ParseError> {
    let scopes: Vec<OptionScope> = [
        ('g', OptionScope::Global),
        ('s', OptionScope::Session),
        ('w', OptionScope::Window),
        ('p', OptionScope::Pane),
    ]
    .into_iter()
    .filter(|(c, _)| flags.present.contains(c))
    .map(|(_, scope)| scope)
    .collect();
    match scopes.as_slice() {
        [] => Ok(None),
        [scope] => Ok(Some(*scope)),
        _ => Err(ParseError::Other(
            "only one of -g -s -w -p allowed".to_string(),
        )),
    }
}

fn parse_set_option(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(
        argv,
        &FlagTable {
            short: SET_OPTION_SHORT,
            long: SET_OPTION_LONG,
        },
    )?;
    let scope = parse_scope(&flags)?;
    let mut positionals = flags.positionals.iter();
    let name = positionals
        .next()
        .cloned()
        .ok_or_else(|| ParseError::MissingArgument("set-option".to_string()))?;
    let value = positionals.next().cloned();
    if positionals.next().is_some() {
        return Err(ParseError::TooManyArguments("set-option".to_string()));
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
    )?;
    let scope = parse_scope(&flags)?;
    let mut positionals = flags.positionals.iter();
    let name = positionals.next().cloned();
    if positionals.next().is_some() {
        return Err(ParseError::TooManyArguments("show-options".to_string()));
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
    )?;
    let mut positionals = flags.positionals.iter();
    let text = positionals.next().cloned();
    if positionals.next().is_some() {
        return Err(ParseError::TooManyArguments("display-message".to_string()));
    }
    Ok(Command::DisplayMessage(DisplayMessage {
        print: flags.present.contains(&'p'),
        target: target_or_current(&flags.values, 't')?,
        text,
    }))
}

fn parse_kill_server(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(argv, &NO_FLAGS)?;
    no_extra_positionals(&flags, "kill-server")?;
    Ok(Command::KillServer(KillServer))
}

fn parse_subscribe(argv: &[String]) -> Result<Command, ParseError> {
    let flags = tokenize(argv, &NO_FLAGS)?;
    Ok(Command::Subscribe(Subscribe {
        events: flags.positionals,
    }))
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
        assert_eq!(err.to_string(), "unknown flag -z");
    }

    #[test]
    fn too_many_arguments_error_matches_tmux() {
        let err = parse(&argv(&["kill-server", "extra"])).unwrap_err();
        assert_eq!(err.to_string(), "command kill-server: too many arguments");
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
    fn resize_pane_with_explicit_cell_count() {
        let cmd = parse(&argv(&["resize-pane", "-L", "5"])).unwrap();
        assert_eq!(
            cmd,
            Command::ResizePane(ResizePane {
                action: ResizeAction::Edge(Edge::Left, 5),
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
                action: ResizeAction::Edge(Edge::Down, 1),
                target: Target::Current,
            })
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
            Err(ParseError::TooManyArguments(_))
        ));
    }

    #[test]
    fn set_option_rejects_multiple_scope_flags() {
        assert!(matches!(
            parse(&argv(&["set-option", "-g", "-s", "name", "value"])),
            Err(ParseError::Other(_))
        ));
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
            Err(ParseError::TooManyArguments(_))
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
}
