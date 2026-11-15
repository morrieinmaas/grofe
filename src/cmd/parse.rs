//! argv -> [`Command`] (spec §4.5). Hand-rolled, tmux-style: short flags
//! may be combined (`-dP`), a flag's value may be attached (`-tfoo`) or a
//! separate token (`-t foo`), and `--` ends flag parsing. `set`/`show` are
//! recognised as aliases for `set-option`/`show-options`; a handful of
//! other well-known tmux short names are recognised too.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Deref;

use super::keys;
use super::table::{self, Row};
use super::target::parse_target;
use super::{
    AgentCmd, AgentNew, AgentReport, AttachSession, BindKey, BreakPane, CapturePane, ChooseClient,
    ChooseOpts, ChooseTree, ClearHistory, Command, CommandPrompt, Condition, ConfirmBefore,
    DetachClient, DisplayMenu, DisplayMessage, DisplayPanes, DisplayPopup, DragPane, FindWindow,
    FloatPane, If, JoinPane, LinkWindow, ListClients, ListKeys, Mark, MenuEntry, MoveWindow,
    NewPane, NewScratchpad, NewSession, NewWindow, NotifyCmd, Offset, OptionScope, Pan,
    PaneSetResume, PaneSetup, ParseError, PasteBuffer, PipePane, PlacePane, PluginRestart,
    RebootCheck, RefreshClient, RegisterSource, Rename, ResizeAction, ResizePane, ResizeWindow,
    RespawnPane, RotateWindow, RunShell, SelectLayout, SelectPane, SelectWindow, SendKeys,
    SendKeysItem, SendPrefix, ServerAccess, SetBuffer, SetEnvironment, SetHook, SetOption,
    ShowEnvironment, ShowHooks, ShowMessages, ShowOptions, ShowScratchpad, SidebarOp, SortFlags,
    SourceFile, SplitWindow, Subscribe, SuspendClient, SwapPane, SwapWindow, SwitchClient, Target,
    UnbindKey, UnfloatPane, UnlinkWindow, WaitFor, WindowMove, WindowStep,
};
use crate::types::{CaptureBound, CaptureOpts, Dir, Edge, PaneId, SplitSize, WINDOW_MAXIMUM};

/// Whether a short/long flag takes a value, and if so whether it's
/// required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlagKind {
    Bool,
    /// A value is required, attached (`-tfoo`) or the next token (`-t
    /// foo`).
    Value,
}

#[derive(Default)]
pub(super) struct ParsedFlags {
    present: BTreeSet<char>,
    /// How many times each bool flag was given (`-EE`).
    counts: BTreeMap<char, u32>,
    values: BTreeMap<char, String>,
    /// Every value flag's value in order, repeats included (`-e A -e B`).
    repeated: Vec<(char, String)>,
    long_present: BTreeSet<&'static str>,
    positionals: Vec<String>,
}

/// What a row's builder reads: the verb, its argv and the scanned flags
/// (`Deref`s to them).
pub(super) struct Args<'a> {
    pub name: &'static str,
    pub argv: &'a [String],
    flags: ParsedFlags,
}

impl Deref for Args<'_> {
    type Target = ParsedFlags;

    fn deref(&self) -> &ParsedFlags {
        &self.flags
    }
}

/// Whether `spec` (`"t:aCg"`: a `:` after a letter means it takes a
/// value) has the short flag `c`, and of which kind.
fn find_short(spec: &str, c: char) -> Option<FlagKind> {
    let at = spec.find(c).filter(|_| c != ':')?;
    Some(if spec[at + c.len_utf8()..].starts_with(':') {
        FlagKind::Value
    } else {
        FlagKind::Bool
    })
}

/// Splits `argv` into recognised flags (per `table`) and trailing
/// positionals, tmux-style. Flag parsing stops at `--` or at the first
/// token that isn't a flag; everything after that is positional
/// verbatim, which is what lets a trailing shell command (e.g. `new-window
/// -d sleep 30`) keep its own `-`-prefixed arguments untouched.
///
/// `cmd_name` is threaded through only to word flag errors like tmux's
/// own CLI (`command <cmd_name>: unknown flag -z`).
fn scan(
    argv: &[String],
    spec: &str,
    long: &[&'static str],
    cmd_name: &str,
) -> Result<ParsedFlags, ParseError> {
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
            // No long flag takes a value; if one does, wire attached/separate
            // value handling in here.
            match long.iter().find(|n| **n == name) {
                Some(canonical) => {
                    flags.long_present.insert(canonical);
                }
                None => return Err(ParseError::UnknownLongFlag(cmd_name.to_string())),
            }
            i += 1;
            continue;
        }
        if tok.len() > 1 && tok.starts_with('-') {
            let chars: Vec<char> = tok[1..].chars().collect();
            let mut ci = 0;
            while ci < chars.len() {
                let c = chars[ci];
                match find_short(spec, c) {
                    None => return Err(ParseError::UnknownFlag(cmd_name.to_string(), c)),
                    Some(FlagKind::Bool) => {
                        flags.present.insert(c);
                        *flags.counts.entry(c).or_default() += 1;
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
                            flags.repeated.push((c, value.clone()));
                        } else {
                            flags.repeated.push((c, attached.clone()));
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

impl ParsedFlags {
    /// Whether the bool flag `c` was given.
    pub(super) fn on(&self, c: char) -> bool {
        self.present.contains(&c)
    }

    /// The value of the value flag `c`.
    pub(super) fn val(&self, c: char) -> Option<String> {
        self.values.get(&c).cloned()
    }

    /// The target of `c`, the current one when it is not given.
    pub(super) fn target(&self, c: char) -> Result<Target, ParseError> {
        target_or_current(&self.values, c)
    }

    /// The target of `c`, when it is given.
    pub(super) fn target_opt(&self, c: char) -> Result<Option<Target>, ParseError> {
        target_if_given(&self.values, c)
    }

    /// The first positional.
    pub(super) fn first(&self) -> Option<String> {
        self.positionals.first().cloned()
    }

    /// Every positional, when there is one: a trailing command.
    pub(super) fn command(&self) -> Option<Vec<String>> {
        (!self.positionals.is_empty()).then(|| self.positionals.clone())
    }
}

/// The `-O order` and `-r` of a listing command.
pub(super) fn sort_flags(flags: &ParsedFlags) -> SortFlags {
    SortFlags {
        order: flags.val('O'),
        reversed: flags.on('r'),
    }
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

/// The `VAR=value` words of every `-e`, in order; one with no `=` is
/// ignored, as tmux's `environ_put` does.
fn env_values(flags: &ParsedFlags) -> Vec<(String, String)> {
    flags
        .repeated
        .iter()
        .filter(|(c, _)| *c == 'e')
        .filter_map(|(_, v)| v.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

pub(super) fn parse_new_session(flags: &Args) -> Result<Command, ParseError> {
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
    let env = env_values(&flags);
    Ok(Command::NewSession(NewSession {
        detached: flags.on('d'),
        name: flags.val('s'),
        cwd: flags.val('c'),
        width,
        height,
        command: flags.command(),
        attach: flags.on('A'),
        print: flags.on('P'),
        format: flags.val('F'),
        window_name: flags.val('n'),
        group: flags.val('t'),
        env,
        detach_others: flags.on('D') || flags.on('X'),
        kill_parent: flags.on('X'),
        no_env: flags.on('E'),
        flags: flags.val('f'),
    }))
}

pub(super) fn select_window(f: &Args) -> Result<Command, ParseError> {
    // tmux tries next, then previous, then last.
    let step = if f.on('n') {
        Some(WindowMove::Next)
    } else if f.on('p') {
        Some(WindowMove::Previous)
    } else if f.on('l') {
        Some(WindowMove::Last)
    } else {
        None
    };
    Ok(Command::SelectWindow(SelectWindow {
        target: f.target('t')?,
        step,
        toggle: f.on('T'),
    }))
}

/// `-e` (input on) and `-d` (input off): `-e` wins, as in tmux.
pub(super) fn input_flag(flags: &ParsedFlags) -> Option<bool> {
    if flags.on('e') {
        Some(true)
    } else if flags.on('d') {
        Some(false)
    } else {
        None
    }
}

pub(super) fn parse_new_window(flags: &Args) -> Result<Command, ParseError> {
    let env = env_values(&flags);
    Ok(Command::NewWindow(NewWindow {
        detached: flags.on('d'),
        target: flags.target('t')?,
        name: flags.val('n'),
        cwd: flags.val('c'),
        command: flags.command(),
        after: flags.on('a'),
        print: flags.on('P'),
        format: flags.val('F'),
        env,
        before: flags.on('b'),
        kill: flags.on('k'),
        select_existing: flags.on('S'),
    }))
}

/// The styles and the staying of a new pane (`-s -S -R -k -m`).
fn pane_setup(flags: &ParsedFlags) -> PaneSetup {
    PaneSetup {
        style: flags.val('s'),
        active_border: flags.val('S'),
        inactive_border: flags.val('R'),
        keep: flags.on('k') || flags.on('m'),
        message: flags.val('m'),
    }
}

pub(super) fn parse_split_window(flags: &Args) -> Result<Command, ParseError> {
    split_window_of(flags).map(Command::SplitWindow)
}

/// The `split-window` the parsed flags mean (`new-pane -L` shares them).
fn split_window_of(flags: &ParsedFlags) -> Result<SplitWindow, ParseError> {
    let dir = match (flags.on('h'), flags.on('v')) {
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
    // tmux's `-l N%` (FE.1): a percent of the target pane, 1 to 100
    // (recorded: `101%` and `x%` are `size or position invalid tiled
    // geometry`).
    let (cells, l_percent) = match flags.values.get(&'l') {
        Some(s) if s.ends_with('%') => {
            let p = s[..s.len() - 1]
                .parse::<u8>()
                .ok()
                .filter(|p| (1..=100).contains(p))
                .ok_or_else(|| {
                    ParseError::Other("size or position invalid tiled geometry".to_string())
                })?;
            (None, Some(p))
        }
        Some(s) => (Some(parse_u16(s, "size")?), None),
        None => (None, None),
    };
    let size = match (percent, cells, l_percent) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            return Err(ParseError::Other(
                "only one of -p or -l allowed".to_string(),
            ));
        }
        (Some(p), None, None) | (None, None, Some(p)) => SplitSize::Percent(p),
        (None, Some(c), _) => SplitSize::Cells(c),
        (None, None, None) => SplitSize::Half,
    };
    let env = env_values(&flags);
    Ok(SplitWindow {
        setup: pane_setup(&flags),
        dir,
        cwd: flags.val('c'),
        size,
        target: flags.target('t')?,
        command: flags.command(),
        before: flags.on('b'),
        full: flags.on('f'),
        detached: flags.on('d'),
        print: flags.on('P'),
        format: flags.val('F'),
        env,
        zoom: flags.on('Z'),
    })
}

pub(super) fn parse_select_pane(flags: &Args) -> Result<Command, ParseError> {
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
        target: flags.target('t')?,
        last: flags.on('l'),
        keep_zoom: flags.on('Z'),
        title: flags.val('T'),
        input: input_flag(&flags),
        // tmux acts on -m and -M before anything else; -M wins.
        mark: if flags.on('M') {
            Mark::Clear
        } else if flags.on('m') {
            Mark::Toggle
        } else {
            Mark::No
        },
    }))
}

pub(super) fn parse_resize_pane(flags: &Args) -> Result<Command, ParseError> {
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
    let width = extent_value(&flags, 'x', 0, SizeWords::Trio("width"))?;
    let height = extent_value(&flags, 'y', 0, SizeWords::Trio("height"))?;
    let zoom = flags.on('Z');
    if zoom && !edges.is_empty() {
        return Err(ParseError::Other(
            "resize-pane: -Z can't combine with a direction".to_string(),
        ));
    }
    let action = if zoom {
        ResizeAction::ToggleZoom
    } else {
        if edges.is_empty() && width.is_none() && height.is_none() {
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
        target: flags.target('t')?,
        width,
        height,
    }))
}

/// `respawn-pane`, and `respawn-window` (M36-R6), which has its shape.
pub(super) fn respawn(flags: &Args) -> Result<RespawnPane, ParseError> {
    Ok(RespawnPane {
        kill: flags.on('k'),
        target: flags.target('t')?,
        cwd: flags.val('c'),
        env: env_values(flags),
        command: flags.command(),
    })
}

pub(super) fn parse_send_keys(flags: &Args) -> Result<Command, ParseError> {
    let literal = flags.on('l');
    let hex = flags.on('H');
    // `-X` (M5): the words are a copy-mode command and its arguments.
    let command = flags.on('X');
    if literal && hex {
        return Err(ParseError::Other(
            "send-keys: only one of -l or -H allowed".to_string(),
        ));
    }
    let mut items = Vec::with_capacity(flags.positionals.len());
    for arg in &flags.positionals {
        items.push(if command {
            SendKeysItem::Literal(arg.clone())
        } else if hex {
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
        target: flags.target('t')?,
        items,
        command,
        count: flags.val('N'),
        format: flags.on('F'),
        reset: flags.on('R'),
        client_key: flags.on('K'),
        client: flags.val('c'),
        mouse: flags.on('M'),
    }))
}

fn parse_capture_line(s: &str) -> Result<CaptureBound, ParseError> {
    s.parse::<i32>()
        .map(CaptureBound::Line)
        .map_err(|_| ParseError::Other(format!("invalid capture-pane bound: {s}")))
}

pub(super) fn parse_capture_pane(flags: &Args) -> Result<Command, ParseError> {
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
        target: flags.target('t')?,
        print: flags.on('p'),
        buffer: flags.val('b'),
        opts: CaptureOpts {
            start,
            end,
            escapes: flags.on('e'),
        },
        join: flags.on('J'),
        keep_blanks: flags.on('N'),
        written_only: flags.on('T'),
        numbered: flags.on('L'),
        flags: flags.on('F'),
        escaped: flags.on('C'),
        links: flags.on('H'),
        alternate: flags.on('a'),
        mode: flags.on('M'),
        pending: flags.on('P'),
        quiet: flags.on('q'),
    }))
}

pub(super) fn parse_clear_history(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::ClearHistory(ClearHistory {
        target: flags.target('t')?,
        hyperlinks: flags.on('H'),
    }))
}

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
    if flags.on('g') {
        Some(OptionScope::Global)
    } else if flags.on('s') {
        Some(OptionScope::Session)
    } else if flags.on('p') {
        Some(OptionScope::Pane)
    } else if flags.on('w') {
        Some(OptionScope::Window)
    } else {
        None
    }
}

pub(super) fn parse_set_option(flags: &Args) -> Result<Command, ParseError> {
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
        unset: flags.on('u') || flags.on('U'),
        unset_panes: flags.on('U'),
        persist: flags.long_present.contains("persist"),
        no_persist: flags.long_present.contains("no-persist"),
        target: flags.target('t')?,
        name,
        value,
        append: flags.on('a'),
        expand: flags.on('F'),
        quiet: flags.on('q'),
        only_if_unset: flags.on('o'),
        shown: None,
    }))
}

pub(super) fn parse_show_options(flags: &Args) -> Result<Command, ParseError> {
    let scope = parse_scope(&flags);
    let mut positionals = flags.positionals.iter();
    let name = positionals.next().cloned();
    if positionals.next().is_some() {
        return Err(ParseError::TooManyArguments("show-options".to_string(), 1));
    }
    Ok(Command::ShowOptions(ShowOptions {
        scope,
        target: flags.target('t')?,
        name,
        value_only: flags.on('v'),
        quiet: flags.on('q'),
        inherited: flags.on('A'),
        hooks: flags.on('H'),
    }))
}

pub(super) fn parse_display_message(flags: &Args) -> Result<Command, ParseError> {
    let mut positionals = flags.positionals.iter();
    let text = positionals
        .next()
        .or_else(|| flags.values.get(&'F'))
        .cloned();
    if positionals.next().is_some() {
        return Err(ParseError::TooManyArguments(
            "display-message".to_string(),
            1,
        ));
    }
    Ok(Command::DisplayMessage(DisplayMessage {
        print: flags.on('p'),
        target: flags.target('t')?,
        text,
        client: flags.val('c'),
        delay: flags.val('d'),
        all: flags.on('a'),
        literal: flags.on('l'),
        verbose: flags.on('v'),
        ignore_keys: flags.on('N'),
    }))
}

pub(super) fn parse_plugin_restart(flags: &Args) -> Result<Command, ParseError> {
    let name = flags
        .first()
        .ok_or_else(|| ParseError::TooFewArguments("plugin-restart".to_string(), 1))?;
    Ok(Command::PluginRestart(PluginRestart { name }))
}

/// `pane-set-resume [-t pane] [-u] [--expect <reason>] [-- argv...]` (T4.2,
/// spec §6). `--expect` is the one long flag here that takes a value,
/// which `tokenize` does not do, so it is taken out first: from the flags
/// before `--` or the first positional, as `--expect <reason>` or
/// `--expect=<reason>`.
pub(super) fn pane_set_resume(flags: &Args) -> Result<Command, ParseError> {
    let argv = flags.argv;
    const NAME: &str = "pane-set-resume";
    let mut rest = Vec::with_capacity(argv.len());
    let mut expect = None;
    let mut i = 0;
    while i < argv.len() {
        let tok = argv[i].as_str();
        if tok == "--" || !tok.starts_with('-') || tok == "-" {
            rest.extend_from_slice(&argv[i..]);
            break;
        }
        if tok == "--expect" {
            let reason = argv
                .get(i + 1)
                .ok_or_else(|| ParseError::Other(format!("{NAME}: --expect needs a reason")))?;
            expect = Some(reason.clone());
            i += 2;
            continue;
        }
        if let Some(reason) = tok.strip_prefix("--expect=") {
            expect = Some(reason.to_string());
            i += 1;
            continue;
        }
        rest.push(argv[i].clone());
        // `-t` takes the next word as its value, even one like `--expect`.
        if tok == "-t"
            && let Some(value) = argv.get(i + 1)
        {
            rest.push(value.clone());
            i += 1;
        }
        i += 1;
    }
    let flags = flags_for(&rest, "t:u", NAME)?;
    let unset = flags.on('u');
    let argv = flags.positionals.clone();
    let given = [unset, expect.is_some(), !argv.is_empty()]
        .iter()
        .filter(|&&g| g)
        .count();
    if given != 1 {
        return Err(ParseError::Other(format!(
            "{NAME}: give one of an argv after --, -u or --expect <reason>"
        )));
    }
    if expect.as_deref() == Some("") {
        return Err(ParseError::Other(format!(
            "{NAME}: --expect needs a reason"
        )));
    }
    Ok(Command::PaneSetResume(PaneSetResume {
        target: flags.target('t')?,
        unset,
        expect,
        argv,
    }))
}

/// `agent report|list|next|seen` (grofe's own). The report's long flags
/// take values, which `tokenize` does not do, so `report` is read by hand.
pub(super) fn agent(flags: &Args) -> Result<Command, ParseError> {
    let (sub, rest) = flags
        .argv
        .split_first()
        .ok_or_else(|| ParseError::Other("agent: subcommand required".into()))?;
    match sub.as_str() {
        "report" => parse_agent_report(rest),
        "list" => {
            let flags = flags_for(rest, "F:", "agent list")?;
            no_extra_positionals(&flags, "agent list", 0)?;
            Ok(Command::Agent(AgentCmd::List {
                format: flags.val('F'),
            }))
        }
        "next" => {
            let flags = flags_for(rest, "", "agent next")?;
            no_extra_positionals(&flags, "agent next", 0)?;
            Ok(Command::Agent(AgentCmd::Next))
        }
        "seen" => {
            let flags = flags_for(rest, "t:a", "agent seen")?;
            no_extra_positionals(&flags, "agent seen", 0)?;
            let all = flags.on('a');
            if all && flags.values.contains_key(&'t') {
                return Err(ParseError::Other("agent seen: -a and -t conflict".into()));
            }
            Ok(Command::Agent(AgentCmd::Seen {
                target: flags.target('t')?,
                all,
            }))
        }
        "new" => {
            // The kind may come first (`agent new claude -w`), so flags
            // are read from what follows it.
            let (lead, flag_argv) = match rest.split_first() {
                Some((k, tail)) if !k.starts_with('-') => (Some(k.clone()), tail),
                _ => (None, rest),
            };
            let flags = flags_for(flag_argv, "pc:n:wWb:d", "agent new")?;
            let mut positionals = flags.positionals.clone();
            if let Some(k) = lead {
                positionals.insert(0, k);
            }
            if positionals.len() > 1 {
                return Err(ParseError::TooManyArguments("agent new".into(), 1));
            }
            let kind = positionals
                .pop()
                .ok_or_else(|| ParseError::Other("agent new: kind required".into()))?;
            let worktree = match (flags.on('w'), flags.on('W')) {
                (true, true) => {
                    return Err(ParseError::Other("agent new: -w and -W conflict".into()));
                }
                (true, false) => Some(true),
                (false, true) => Some(false),
                (false, false) => None,
            };
            Ok(Command::Agent(AgentCmd::New(AgentNew {
                kind,
                split: flags.on('p'),
                cwd: flags.val('c'),
                name: flags.val('n'),
                worktree,
                branch: flags.val('b'),
                detached: flags.on('d'),
            })))
        }
        other => Err(ParseError::Other(format!(
            "agent: unknown subcommand {other}"
        ))),
    }
}

/// `notify list|dismiss <id>|clear|mute [duration]|unmute`.
pub(super) fn notify(flags: &Args) -> Result<Command, ParseError> {
    let (sub, rest) = flags
        .argv
        .split_first()
        .ok_or_else(|| ParseError::Other("notify: subcommand required".into()))?;
    let name = format!("notify {sub}");
    let flags = flags_for(rest, "", &name)?;
    let cmd = match sub.as_str() {
        "list" | "clear" | "unmute" => {
            no_extra_positionals(&flags, &name, 0)?;
            match sub.as_str() {
                "list" => NotifyCmd::List,
                "clear" => NotifyCmd::Clear,
                _ => NotifyCmd::Unmute,
            }
        }
        "dismiss" => {
            let id = one_positional(&flags, &name)?;
            NotifyCmd::Dismiss(
                id.parse()
                    .map_err(|_| ParseError::Other(format!("notify dismiss: bad id {id}")))?,
            )
        }
        "mute" => {
            no_extra_positionals(&flags, &name, 1)?;
            let until = flags.positionals.first().map(|d| parse_mute_duration(d));
            NotifyCmd::Mute(until.transpose()?)
        }
        other => {
            return Err(ParseError::Other(format!(
                "notify: unknown subcommand {other}"
            )));
        }
    };
    Ok(Command::Notify(cmd))
}

/// `30s`, `10m`, `2h`: a positive whole number and a unit.
fn parse_mute_duration(s: &str) -> Result<std::time::Duration, ParseError> {
    let bad = || ParseError::Other(format!("notify mute: bad duration {s}"));
    let split = s.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
    let n: u64 = s[..split].parse().ok().filter(|n| *n > 0).ok_or_else(bad)?;
    let unit = match &s[split..] {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err(bad()),
    };
    n.checked_mul(unit)
        .map(std::time::Duration::from_secs)
        .ok_or_else(bad)
}

fn parse_agent_report(argv: &[String]) -> Result<Command, ParseError> {
    const NAME: &str = "agent report";
    let mut state = None;
    let mut target = Target::Current;
    let (mut kind, mut msg, mut blocked, mut session_id, mut ts) = (None, None, None, None, None);
    let mut i = 0;
    while i < argv.len() {
        let tok = argv[i].as_str();
        if !tok.starts_with('-') || tok == "-" {
            if state.replace(tok.to_string()).is_some() {
                return Err(ParseError::TooManyArguments(NAME.to_string(), 1));
            }
            i += 1;
            continue;
        }
        let value = argv
            .get(i + 1)
            .ok_or_else(|| ParseError::Other(format!("{NAME}: {tok} needs a value")))?;
        match tok {
            "-t" => target = parse_target(value)?,
            "--kind" => kind = Some(value.clone()),
            "--msg" => msg = Some(value.clone()),
            "--blocked" => {
                if !matches!(value.as_str(), "permission" | "question" | "auth") {
                    return Err(ParseError::Other(format!(
                        "{NAME}: --blocked must be permission, question or auth"
                    )));
                }
                blocked = Some(value.clone());
            }
            "--session-id" => session_id = Some(value.clone()),
            "--ts" => {
                ts = Some(
                    value
                        .parse()
                        .map_err(|_| ParseError::Other(format!("invalid --ts: {value}")))?,
                )
            }
            _ => return Err(ParseError::Other(format!("{NAME}: unknown flag {tok}"))),
        }
        i += 2;
    }
    let state = state.ok_or_else(|| ParseError::Other(format!("{NAME}: state required")))?;
    Ok(Command::Agent(AgentCmd::Report(AgentReport {
        state,
        target,
        kind,
        msg,
        blocked,
        session_id,
        ts,
    })))
}

/// `reboot-check [--json]` (T4.5, spec §6).
pub(super) fn reboot_check(flags: &Args) -> Result<Command, ParseError> {
    if let Some(word) = flags.positionals.first() {
        return Err(ParseError::Other(format!(
            "{}: unexpected argument {word}",
            flags.name
        )));
    }
    Ok(Command::RebootCheck(RebootCheck {
        json: flags.long_present.contains("json"),
    }))
}

pub(super) fn parse_subscribe(flags: &Args) -> Result<Command, ParseError> {
    let mut target = flags.target_opt('t')?;
    // The usage is `subscribe <event>... [-t target]`: the flag comes after
    // the names, where the flag scan has already stopped.
    let mut events = Vec::new();
    let mut words = flags.positionals.iter().cloned();
    while let Some(word) = words.next() {
        if word == "-t" {
            let value = words
                .next()
                .ok_or_else(|| ParseError::MissingValue("subscribe".to_string(), 't'))?;
            target = Some(parse_target(&value)?);
        } else {
            events.push(word);
        }
    }
    Ok(Command::Subscribe(Subscribe { events, target }))
}

pub(super) fn parse_attach_session(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::AttachSession(AttachSession {
        target: flags.target('t')?,
        detach_others: flags.on('d') || flags.on('x'),
        nest: flags.long_present.contains("nest"),
        cwd: flags.val('c'),
        no_env: flags.on('E'),
        read_only: flags.on('r'),
        kill_parent: flags.on('x'),
        flags: flags.val('f'),
    }))
}

pub(super) fn parse_server_access(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::ServerAccess(ServerAccess {
        add: flags.on('a'),
        deny: flags.on('d'),
        list: flags.on('l'),
        read_only: flags.on('r'),
        write: flags.on('w'),
        user: flags.first(),
    }))
}

pub(super) fn parse_detach_client(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::DetachClient(DetachClient {
        session: flags.target_opt('s')?,
        client: flags.val('t'),
        all_others: flags.on('a'),
        kill_parent: flags.on('P'),
    }))
}

pub(super) fn parse_list_clients(flags: &Args) -> Result<Command, ParseError> {
    let target = match flags.values.get(&'t') {
        Some(raw) => Some(parse_target(raw)?),
        None => None,
    };
    Ok(Command::ListClients(ListClients {
        format: flags.val('F'),
        target,
        filter: flags.val('f'),
        sort: sort_flags(&flags),
    }))
}

/// `next-window`/`previous-window` (`-t` session, `-a` alert).
pub(super) fn window_step(flags: &Args) -> Result<WindowStep, ParseError> {
    Ok(WindowStep {
        target: flags.target('t')?,
        alert: flags.on('a'),
    })
}

pub(super) fn rename(flags: &Args) -> Result<Rename, ParseError> {
    let name = flags
        .first()
        .ok_or_else(|| ParseError::MissingArgument(flags.name.to_string()))?;
    Ok(Rename {
        target: flags.target('t')?,
        name,
    })
}

pub(super) fn parse_switch_client(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::SwitchClient(SwitchClient {
        target: flags.target_opt('t')?,
        last: flags.on('l'),
        next: flags.on('n'),
        previous: flags.on('p'),
        key_table: flags.val('T'),
        client: flags.val('c'),
        keep_zoom: flags.on('Z'),
        no_env: flags.on('E'),
        toggle_read_only: flags.on('r'),
        order: flags.val('O'),
    }))
}

pub(super) fn parse_send_prefix(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::SendPrefix(SendPrefix {
        target: flags.target('t')?,
        second: flags.on('2'),
    }))
}

/// `-T`, else `root` for `-n`, else `prefix`: tmux's key-table choice for
/// `bind-key` and `unbind-key`.
fn key_table(flags: &ParsedFlags) -> String {
    match flags.values.get(&'T') {
        Some(t) => t.clone(),
        None if flags.on('n') => "root".to_string(),
        None => "prefix".to_string(),
    }
}

pub(super) fn parse_bind_key(flags: &Args) -> Result<Command, ParseError> {
    let (key, command) = flags
        .positionals
        .split_first()
        .ok_or_else(|| ParseError::MissingArgument("bind-key".to_string()))?;
    Ok(Command::BindKey(BindKey {
        table: key_table(&flags),
        repeat: flags.on('r'),
        note: flags.val('N'),
        group: flags.val('G'),
        key: key.clone(),
        command: command.to_vec(),
    }))
}

pub(super) fn parse_unbind_key(flags: &Args) -> Result<Command, ParseError> {
    let mut all = flags.on('a');
    let mut key = flags.first();
    let quiet = flags.on('q');
    // tmux's wording; `-q` keeps it quiet and does nothing.
    match (all, &key) {
        (true, Some(_)) | (false, None) if quiet => (all, key) = (false, None),
        (true, Some(_)) => return Err(ParseError::Other("key given with -a".to_string())),
        (false, None) => return Err(ParseError::Other("missing key".to_string())),
        _ => {}
    }
    Ok(Command::UnbindKey(UnbindKey {
        table: key_table(&flags),
        all,
        key,
        quiet,
    }))
}

pub(super) fn parse_list_keys(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::ListKeys(ListKeys {
        table: flags.val('T'),
        first: flags.on('1'),
        notes: flags.on('N'),
        key: flags.first(),
        all: flags.on('a'),
        format: flags.val('F'),
        prefix: flags.val('P'),
        sort: sort_flags(&flags),
    }))
}

/// The `missing argument` of the verb `flags` was parsed for.
pub(super) fn missing(flags: &Args) -> ParseError {
    ParseError::MissingArgument(flags.name.to_string())
}

/// The single required positional of `cmd_name`.
fn one_positional(flags: &ParsedFlags, cmd_name: &str) -> Result<String, ParseError> {
    no_extra_positionals(flags, cmd_name, 1)?;
    flags
        .first()
        .ok_or_else(|| ParseError::MissingArgument(cmd_name.to_string()))
}

fn at_least_one_positional(flags: &ParsedFlags, cmd_name: &str) -> Result<Vec<String>, ParseError> {
    if flags.positionals.is_empty() {
        return Err(ParseError::MissingArgument(cmd_name.to_string()));
    }
    Ok(flags.positionals.clone())
}

pub(super) fn parse_source_file(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::SourceFile(SourceFile {
        quiet: flags.on('q'),
        format: flags.on('F'),
        parse_only: flags.on('n'),
        verbose: flags.on('v'),
        target: flags.target_opt('t')?,
        paths: at_least_one_positional(&flags, "source-file")?,
    }))
}

pub(super) fn parse_command_prompt(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::CommandPrompt(CommandPrompt {
        prompt: flags.val('p'),
        initial: flags.val('I'),
        template: flags.first(),
        background: flags.on('b'),
        key: flags.on('k'),
        single: flags.on('1'),
        prompt_type: flags.val('T'),
        numeric: flags.on('N'),
        incremental: flags.on('i'),
        expand: flags.on('F'),
        bspace_exit: flags.on('e'),
        literal: flags.on('l'),
        client: flags.val('t'),
    }))
}

pub(super) fn parse_pipe_pane(flags: &Args) -> Result<Command, ParseError> {
    let input = flags.on('I');
    Ok(Command::PipePane(PipePane {
        target: flags.target('t')?,
        input,
        // Neither -I nor -O is -O.
        output: flags.on('O') || !input,
        toggle: flags.on('o'),
        command: flags.first(),
    }))
}

pub(super) fn parse_run_shell(flags: &Args) -> Result<Command, ParseError> {
    // `-d <seconds>` (FE.1), as tmux 3.7b's `strtod`: a delay with no
    // command only waits (recorded: `invalid delay time: x`).
    let delay = match flags.values.get(&'d') {
        Some(s) => Some(
            s.parse::<f64>()
                .ok()
                .filter(|d| d.is_finite() && *d >= 0.0)
                .and_then(|d| std::time::Duration::try_from_secs_f64(d).ok())
                .ok_or_else(|| ParseError::Other(format!("invalid delay time: {s}")))?,
        ),
        None => None,
    };
    let command = flags.on('C');
    // A shell command is the first word, the others its `#{1}`, `#{2}`; a
    // command (`-C`) is the first word alone.
    let mut words = flags.positionals.iter();
    let cmd = match (&delay, flags.positionals.is_empty()) {
        (Some(_), true) => String::new(),
        // The command line is the first word, as tmux reads it; the others
        // are nothing to it.
        _ if command => {
            let first = words.next().cloned().unwrap_or_default();
            words.by_ref().for_each(drop);
            first
        }
        // tmux runs nothing for a bare `run-shell`.
        _ => words.next().cloned().unwrap_or_default(),
    };
    Ok(Command::RunShell(RunShell {
        cmd,
        delay,
        background: flags.on('b'),
        target: flags.target('t')?,
        command,
        stderr: flags.on('E'),
        cwd: flags.val('c'),
        args: words.cloned().collect(),
    }))
}

pub(super) fn parse_register_source(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::RegisterSource(RegisterSource {
        name: flags.val('n'),
        command: one_positional(&flags, "__register-source")?,
    }))
}

pub(super) fn parse_select_layout(flags: &Args) -> Result<Command, ParseError> {
    let (next, previous, undo, spread) =
        (flags.on('n'), flags.on('p'), flags.on('o'), flags.on('E'));
    // A layout name is optional: tmux takes none to mean the last preset
    // used (`-o`: the layout before the last change).
    no_extra_positionals(&flags, "select-layout", 1)?;
    let name = flags.first().unwrap_or_default();
    Ok(Command::SelectLayout(SelectLayout {
        target: flags.target('t')?,
        name,
        spread,
        next,
        previous,
        undo,
    }))
}

// ---- M3.6 verbs (TC.2) ----

/// A number as tmux's `args_percentage` reads it: an integer, or one
/// with a `%` suffix. `None` when it is neither.
fn amount(s: &str) -> Option<(i64, bool)> {
    let (digits, percent) = match s.strip_suffix('%') {
        Some(d) => (d, true),
        None => (s, false),
    };
    digits.parse::<i64>().ok().map(|n| (n, percent))
}

/// How tmux 3.7b words a bad size: `<what> invalid`, `<what> too small`
/// and `<what> too large`, or one fixed text for every failure
/// (`join-pane -l`).
#[derive(Clone, Copy)]
enum SizeWords {
    Trio(&'static str),
    Fixed(&'static str),
}

/// A size of at least `min`, in cells or `N%` up to 100, worded as tmux
/// 3.7b words it (recorded on `tmux -L grofe-tc2`): `display-popup -w 0`
/// is `width too small`, `-h 200%` `height too large`, `-w 5x` `width
/// invalid`; `join-pane -l` gives `size or position invalid tiled
/// geometry` for any bad value. Whether a size fits the window or client
/// is the handler's check.
fn parse_extent(s: &str, min: i64, words: SizeWords) -> Result<SplitSize, ParseError> {
    let fail = |why: &str| {
        ParseError::Other(match words {
            SizeWords::Trio(what) => format!("{what} {why}"),
            SizeWords::Fixed(text) => text.to_string(),
        })
    };
    let (n, percent) = amount(s).ok_or_else(|| fail("invalid"))?;
    if n < min {
        return Err(fail("too small"));
    }
    if percent {
        u8::try_from(n)
            .ok()
            .filter(|p| *p <= 100)
            .map(SplitSize::Percent)
            .ok_or_else(|| fail("too large"))
    } else {
        u16::try_from(n)
            .map(SplitSize::Cells)
            .map_err(|_| fail("too large"))
    }
}

fn extent_value(
    flags: &ParsedFlags,
    key: char,
    min: i64,
    words: SizeWords,
) -> Result<Option<SplitSize>, ParseError> {
    flags
        .values
        .get(&key)
        .map(|s| parse_extent(s, min, words))
        .transpose()
}

/// `new-pane`'s wording for every bad size or position (tmux 3.7b).
const NEW_PANE_WORDS: SizeWords = SizeWords::Trio("size or position");

/// A `new-pane -X`/`-Y` position: signed cells or `N%` (tmux 3.7b takes
/// `-X -1` and `-X 50%`).
fn offset_value(flags: &ParsedFlags, key: char) -> Result<Option<Offset>, ParseError> {
    let fail = |why: &str| ParseError::Other(format!("size or position {why}"));
    let Some(s) = flags.values.get(&key) else {
        return Ok(None);
    };
    let (n, percent) = amount(s).ok_or_else(|| fail("invalid"))?;
    let offset = if percent {
        if n < 0 {
            return Err(fail("too small"));
        }
        Offset::Percent(
            u8::try_from(n)
                .ok()
                .filter(|p| *p <= 100)
                .ok_or_else(|| fail("too large"))?,
        )
    } else {
        Offset::Cells(i32::try_from(n).map_err(|_| {
            if n < 0 {
                fail("too small")
            } else {
                fail("too large")
            }
        })?)
    };
    Ok(Some(offset))
}

fn target_if_given(
    values: &BTreeMap<char, String>,
    key: char,
) -> Result<Option<Target>, ParseError> {
    values.get(&key).map(|s| parse_target(s)).transpose()
}

fn flags_for(argv: &[String], spec: &str, cmd_name: &str) -> Result<ParsedFlags, ParseError> {
    scan(argv, spec, &[], cmd_name)
}

/// `join-pane` and `move-pane`, which share tmux's flags. `-p` is the
/// hidden one (in the args template, not the usage): a percentage, which
/// `-l` overrides when both are given (`layout_get_tiled_cell`).
pub(super) fn join_pane(flags: &Args) -> Result<JoinPane, ParseError> {
    let invalid = || ParseError::Other("size or position invalid tiled geometry".to_string());
    let size = match (flags.values.get(&'l'), flags.values.get(&'p')) {
        (Some(_), _) | (None, None) => extent_value(
            &flags,
            'l',
            0,
            SizeWords::Fixed("size or position invalid tiled geometry"),
        )?,
        (None, Some(p)) => Some(SplitSize::Percent(
            p.parse::<u8>()
                .ok()
                .filter(|p| *p <= 100)
                .ok_or_else(invalid)?,
        )),
    };
    Ok(JoinPane {
        src: flags.target('s')?,
        dst: flags.target('t')?,
        before: flags.on('b'),
        detach: flags.on('d'),
        full: flags.on('f'),
        horizontal: flags.on('h'),
        vertical: flags.on('v'),
        size,
    })
}

pub(super) fn parse_swap_pane(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::SwapPane(SwapPane {
        src: flags.target_opt('s')?,
        dst: flags.target('t')?,
        up: flags.on('U'),
        down: flags.on('D'),
        detach: flags.on('d'),
        keep_zoom: flags.on('Z'),
    }))
}

pub(super) fn parse_swap_window(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::SwapWindow(SwapWindow {
        src: flags.target_opt('s')?,
        dst: flags.target('t')?,
        select: flags.on('d'),
    }))
}

pub(super) fn parse_break_pane(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::BreakPane(BreakPane {
        src: flags.target('s')?,
        dst: flags.target_opt('t')?,
        detach: flags.on('d'),
        name: flags.val('n'),
        after: flags.on('a'),
        before: flags.on('b'),
        print: flags.on('P'),
        format: flags.val('F'),
    }))
}

pub(super) fn parse_resize_window(flags: &Args) -> Result<Command, ParseError> {
    let adjust = match flags.positionals.first() {
        Some(s) => s
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=i32::MAX as u32).contains(n))
            .ok_or_else(|| ParseError::Other(format!("adjustment {}", adjustment_why(s))))?,
        None => 1,
    };
    // tmux tries -L, -R, -U, then -D.
    let direction = [
        ('L', Edge::Left),
        ('R', Edge::Right),
        ('U', Edge::Up),
        ('D', Edge::Down),
    ]
    .into_iter()
    .find(|(c, _)| flags.present.contains(c))
    .map(|(_, e)| e);
    Ok(Command::ResizeWindow(ResizeWindow {
        direction,
        adjust,
        target: flags.target('t')?,
        width: flags
            .values
            .get(&'x')
            .map(|s| parse_window_size(s, "width"))
            .transpose()?,
        height: flags
            .values
            .get(&'y')
            .map(|s| parse_window_size(s, "height"))
            .transpose()?,
        largest: flags.on('A'),
        smallest: flags.on('a'),
    }))
}

/// Why `strtonum(s, 1, INT_MAX)` fails, as tmux words it after
/// `adjustment `.
fn adjustment_why(s: &str) -> &'static str {
    match s.parse::<i64>() {
        Ok(n) if n < 1 => "too small",
        Ok(_) => "too large",
        Err(_)
            if s.starts_with('-') && s[1..].bytes().all(|b| b.is_ascii_digit()) && s.len() > 1 =>
        {
            "too small"
        }
        Err(_) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => "too large",
        Err(_) => "invalid",
    }
}

pub(super) fn parse_rotate_window(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::RotateWindow(RotateWindow {
        target: flags.target('t')?,
        up: flags.on('U'),
        down: flags.on('D'),
        keep_zoom: flags.on('Z'),
    }))
}

pub(super) fn parse_move_window(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::MoveWindow(MoveWindow {
        src: flags.target('s')?,
        dst: flags.target_opt('t')?,
        after: flags.on('a'),
        renumber: flags.on('r'),
        before: flags.on('b'),
        detach: flags.on('d'),
        kill: flags.on('k'),
    }))
}

pub(super) fn parse_link_window(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::LinkWindow(LinkWindow {
        src: flags.target('s')?,
        dst: flags.target_opt('t')?,
        after: flags.on('a'),
        before: flags.on('b'),
        detach: flags.on('d'),
        kill: flags.on('k'),
    }))
}

pub(super) fn parse_unlink_window(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::UnlinkWindow(UnlinkWindow {
        target: flags.target('t')?,
        kill: flags.on('k'),
    }))
}

pub(super) fn parse_refresh_client(flags: &Args) -> Result<Command, ParseError> {
    let pan = [
        ('c', Pan::Reset),
        ('L', Pan::Left),
        ('R', Pan::Right),
        ('U', Pan::Up),
        ('D', Pan::Down),
    ]
    .into_iter()
    .find(|(f, _)| flags.present.contains(f))
    .map(|(_, p)| p);
    Ok(Command::RefreshClient(RefreshClient {
        client: flags.val('t'),
        status: flags.on('S'),
        pan,
        adjustment: flags.first(),
        // `-F` is `-f`'s alias, and goes first.
        flags: ['F', 'f']
            .iter()
            .filter_map(|f| flags.values.get(f).cloned())
            .collect(),
        clipboard: flags.on('l'),
        report: flags.val('r'),
        control: ['A', 'B', 'C'].iter().any(|f| flags.present.contains(f)),
    }))
}

pub(super) fn parse_show_messages(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::ShowMessages(ShowMessages {
        client: flags.val('t'),
        jobs: flags.on('J'),
        terminals: flags.on('T'),
    }))
}

/// How many times the bool flag `flag` is given in `argv` (`-NN` is two),
/// which the flag table does not count.
fn count_flag(argv: &[String], flag: char, valued: &str) -> u8 {
    let mut count: u8 = 0;
    let mut skip = false;
    for word in argv {
        if skip {
            skip = false;
            continue;
        }
        let Some(letters) = word
            .strip_prefix('-')
            .filter(|w| !w.is_empty() && *w != "-")
        else {
            break;
        };
        for (i, c) in letters.char_indices() {
            if c == flag {
                count = count.saturating_add(1);
            } else if valued.contains(c) {
                skip = i + c.len_utf8() == letters.len();
                break;
            }
        }
    }
    count
}

pub(super) fn choose_opts(flags: &Args) -> Result<ChooseOpts, ParseError> {
    Ok(ChooseOpts {
        target: flags.target('t')?,
        format: flags.val('F'),
        filter: flags.val('f'),
        key_format: flags.val('K'),
        sort: flags.val('O'),
        reversed: flags.on('r'),
        no_preview: count_flag(flags.argv, 'N', "FfKOt"),
        prompt_yes: flags.on('y'),
        groups: flags.on('G'),
        template: flags.first(),
    })
}

pub(super) fn parse_choose_client(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::ChooseClient(ChooseClient {
        zoom: flags.on('Z'),
        opts: choose_opts(flags)?,
    }))
}

pub(super) fn parse_suspend_client(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::SuspendClient(SuspendClient {
        client: flags.val('t'),
    }))
}

pub(super) fn parse_find_window(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::FindWindow(FindWindow {
        target: flags.target('t')?,
        zoom: flags.on('Z'),
        pattern: flags.first(),
        content: flags.on('C'),
        name: flags.on('N'),
        title: flags.on('T'),
        regex: flags.on('r'),
        ignore: flags.on('i'),
    }))
}

pub(super) fn parse_choose_tree(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::ChooseTree(ChooseTree {
        zoom: flags.on('Z'),
        sessions: flags.on('s'),
        windows: flags.on('w'),
        opts: choose_opts(flags)?,
    }))
}

pub(super) fn parse_wait_for(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::WaitFor(WaitFor {
        channel: flags.first(),
        signal: flags.on('S'),
        lock: flags.on('L'),
        unlock: flags.on('U'),
    }))
}

pub(super) fn parse_set_buffer(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::SetBuffer(SetBuffer {
        append: flags.on('a'),
        name: flags.val('b'),
        rename: flags.val('n'),
        data: flags.first(),
        clipboard: flags.on('w'),
        client: flags.val('t'),
    }))
}

pub(super) fn parse_paste_buffer(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::PasteBuffer(PasteBuffer {
        bracket: flags.on('p'),
        name: flags.val('b'),
        delete: flags.on('d'),
        raw: flags.on('r'),
        separator: flags.val('s'),
        unsanitized: flags.on('S'),
        target: flags.target('t')?,
    }))
}

pub(super) fn parse_display_popup(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::DisplayPopup(DisplayPopup {
        close_on_exit: flags.on('E'),
        close_on_zero: flags.counts.get(&'E').is_some_and(|n| *n > 1),
        no_border: flags.on('B'),
        border: flags.val('b'),
        client: flags.val('c'),
        cwd: flags.val('d'),
        env: flags
            .repeated
            .iter()
            .filter(|(c, _)| *c == 'e')
            .filter_map(|(_, v)| v.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        title: flags.val('T'),
        style: flags.val('s'),
        border_style: flags.val('S'),
        close: flags.on('C'),
        any_key: flags.on('k'),
        reset_flags: flags.on('N'),
        target: flags.target_opt('t')?,
        width: extent_value(&flags, 'w', 1, SizeWords::Trio("width"))?,
        height: extent_value(&flags, 'h', 1, SizeWords::Trio("height"))?,
        x: flags.val('x'),
        y: flags.val('y'),
        command: flags.command(),
    }))
}

pub(super) fn parse_new_pane(flags: &Args) -> Result<Command, ParseError> {
    if flags.on('L') {
        return split_window_of(flags).map(Command::SplitWindow);
    }
    let env = env_values(&flags);
    Ok(Command::NewPane(NewPane {
        print: flags.on('P'),
        format: flags.val('F'),
        target: flags.target('t')?,
        cwd: flags.val('c'),
        width: extent_value(&flags, 'x', 0, NEW_PANE_WORDS)?,
        height: extent_value(&flags, 'y', 0, NEW_PANE_WORDS)?,
        x: offset_value(&flags, 'X')?,
        y: offset_value(&flags, 'Y')?,
        env,
        zoom: flags.on('Z'),
        detached: flags.on('d'),
        setup: pane_setup(&flags),
        command: flags.command(),
    }))
}

pub(super) fn parse_float_pane(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::FloatPane(FloatPane {
        target: flags.target('t')?,
        width: extent_value(&flags, 'x', 0, NEW_PANE_WORDS)?,
        height: extent_value(&flags, 'y', 0, NEW_PANE_WORDS)?,
        x: offset_value(&flags, 'X')?,
        y: offset_value(&flags, 'Y')?,
    }))
}

pub(super) fn parse_unfloat_pane(flags: &Args) -> Result<Command, ParseError> {
    let dir = if flags.on('h') {
        Some(Dir::Horizontal)
    } else if flags.on('v') {
        Some(Dir::Vertical)
    } else {
        None
    };
    Ok(Command::UnfloatPane(UnfloatPane {
        target: flags.target('t')?,
        split: flags.target_opt('s')?,
        dir,
    }))
}

pub(super) fn parse_new_scratchpad(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::NewScratchpad(NewScratchpad {
        target: flags.target('t')?,
        cwd: flags.val('c'),
        command: flags.command(),
    }))
}

pub(super) fn parse_show_scratchpad(flags: &Args) -> Result<Command, ParseError> {
    let word = flags
        .positionals
        .first()
        .ok_or_else(|| ParseError::MissingArgument("show-scratchpad".to_string()))?;
    let pane = word
        .parse::<PaneId>()
        .map_err(|_| ParseError::Other(format!("invalid pane id: {word}")))?;
    Ok(Command::ShowScratchpad(ShowScratchpad {
        target: flags.target('t')?,
        pane,
    }))
}

pub(super) fn lua(flags: &Args) -> Result<Command, ParseError> {
    if flags.argv.is_empty() {
        return Err(ParseError::MissingArgument("lua".to_string()));
    }
    Ok(Command::Lua {
        src: flags.argv.join(" "),
    })
}

pub(super) fn lua_call(flags: &Args) -> Result<Command, ParseError> {
    let [id] = flags.argv else {
        return Err(ParseError::Other("lua-call needs one id".to_string()));
    };
    let id = id
        .parse()
        .map_err(|_| ParseError::Other(format!("lua-call: not a number: {id}")))?;
    Ok(Command::LuaCall { id })
}

pub(super) fn parse_drag_pane(flags: &Args) -> Result<Command, ParseError> {
    if !flags.on('M') {
        return Err(ParseError::Other("drag-pane needs -M".to_string()));
    }
    Ok(Command::DragPane(DragPane))
}

pub(super) fn parse_place_pane(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::PlacePane(PlacePane {
        target: flags.target('t')?,
        x: offset_value(&flags, 'X')?,
        y: offset_value(&flags, 'Y')?,
    }))
}

/// `display-menu`'s items, read as tmux 3.7b's `cmd_display_menu` reads
/// them: `name key command`, except an empty name, which is a separator
/// alone; a short last item is `not enough arguments`.
fn menu_entries(args: &[String]) -> Result<Vec<MenuEntry>, ParseError> {
    let mut items = Vec::new();
    let mut rest = args;
    while let Some((name, tail)) = rest.split_first() {
        if name.is_empty() {
            items.push(MenuEntry::Separator);
            rest = tail;
            continue;
        }
        let [key, command, tail @ ..] = tail else {
            return Err(ParseError::Other("not enough arguments".to_string()));
        };
        items.push(MenuEntry::Item {
            name: name.clone(),
            key: key.clone(),
            command: command.clone(),
        });
        rest = tail;
    }
    Ok(items)
}

pub(super) fn parse_display_menu(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::DisplayMenu(DisplayMenu {
        title: flags.val('T'),
        x: flags.val('x'),
        y: flags.val('y'),
        target: flags.target_opt('t')?,
        client: flags.val('c'),
        border: flags.val('b'),
        style: flags.val('s'),
        selected_style: flags.val('H'),
        border_style: flags.val('S'),
        starting_choice: flags.val('C'),
        stay_open: flags.on('O'),
        mouse: flags.on('M'),
        items: menu_entries(&flags.positionals)?,
    }))
}

pub(super) fn parse_display_panes(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::DisplayPanes(DisplayPanes {
        duration: flags.val('d'),
        background: flags.on('b'),
        no_keys: flags.on('N'),
        client: flags.val('t'),
        template: flags.first(),
    }))
}

pub(super) fn parse_confirm_before(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::ConfirmBefore(ConfirmBefore {
        prompt: flags.val('p'),
        default_yes: flags.on('y'),
        confirm_key: flags.val('c'),
        background: flags.on('b'),
        client: flags.val('t'),
        command: flags.first(),
    }))
}

pub(super) fn parse_if_shell(flags: &Args) -> Result<Command, ParseError> {
    let mut positionals = flags.positionals.iter().cloned();
    let condition = positionals.next().map(|c| {
        if flags.on('F') {
            Condition::Format(c)
        } else {
            Condition::Shell(c)
        }
    });
    Ok(Command::If(If {
        condition,
        then: positionals.next(),
        otherwise: positionals.next(),
        background: flags.on('b'),
        target: flags.target('t')?,
    }))
}

pub(super) fn parse_set_hook(flags: &Args) -> Result<Command, ParseError> {
    let mut positionals = flags.positionals.iter().cloned();
    Ok(Command::SetHook(SetHook {
        global: flags.on('g'),
        unset: flags.on('u'),
        append: flags.on('a'),
        pane: flags.on('p'),
        window: flags.on('w'),
        run: flags.on('R'),
        target: flags.target('t')?,
        target_text: flags.val('t'),
        hook: positionals.next(),
        command: positionals.next(),
    }))
}

pub(super) fn parse_show_hooks(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::ShowHooks(ShowHooks {
        global: flags.on('g'),
        window: flags.on('w'),
        pane: flags.on('p'),
        target: flags.target('t')?,
        target_text: flags.val('t'),
        hook: flags.first(),
    }))
}

pub(super) fn parse_set_environment(flags: &Args) -> Result<Command, ParseError> {
    let mut positionals = flags.positionals.iter().cloned();
    Ok(Command::SetEnvironment(SetEnvironment {
        global: flags.on('g'),
        unset: flags.on('u'),
        hidden: flags.on('h'),
        clear: flags.on('r'),
        format: flags.on('F'),
        target: flags.target('t')?,
        name: positionals.next(),
        value: positionals.next(),
    }))
}

pub(super) fn parse_show_environment(flags: &Args) -> Result<Command, ParseError> {
    Ok(Command::ShowEnvironment(ShowEnvironment {
        global: flags.on('g'),
        hidden: flags.on('h'),
        shell: flags.on('s'),
        target: flags.target('t')?,
        name: flags.first(),
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
///   literally, or makes the escape it names ([`escape`]: `\n`, `\t` and
///   the rest), `$VAR` and `${VAR}` expand (unset is empty), and a `~` or
///   `~user` that starts the word is that home directory;
/// - a `$` before anything that can't start a name is a literal `$`, but a
///   `$` that ends the string is a syntax error (D17), as is an unquoted
///   `$;` that ends it; `${` must close over a name (`invalid environment
///   variable`);
/// - an unquoted `;` ends a command wherever it is; a `\;` word is a
///   literal `;` argument (so a stored `a \; b` is one command, D24),
///   except a trailing one, which is dropped;
/// - an unterminated quote is closed at the end of the string.
///
/// - a `{` that starts an argument opens a command list, up to its `}`,
///   which is one argument: the parsed commands, quoted so that running it
///   splits back to exactly those words ([`list_text`]), as tmux
///   runs a parsed list (a prompt template, an `if-shell` branch, a menu
///   item). tmux's syntax errors: a `{` that starts a command, an unclosed
///   `{`, an unquoted `}` with no list open; lists nested 2000 deep are
///   `yacc stack overflow`. A `}` closing a `#{` is part of its word.
///
/// Empty commands are dropped.
pub fn split_command_string(s: &str) -> Result<Vec<Vec<String>>, ParseError> {
    split_commands(&mut s.chars().peekable())
}

/// How deep `{ }` lists nest before tmux 3.7b's parser gives up (recorded:
/// 1999 nested lists parse, 2000 are `yacc stack overflow`).
const MAX_LIST_DEPTH: usize = 2000;

/// One command list being read by [`split_commands`]: the top level, or a
/// `{ }` list inside it.
#[derive(Default)]
struct Level {
    cmds: Vec<Vec<String>>,
    /// Each command's words as source text that splits back to them
    /// ([`quote_word`], or `{ ... }` for a word that was a list).
    srcs: Vec<Vec<String>>,
    words: Vec<String>,
    word_srcs: Vec<String>,
    word: String,
    /// The current word's source text when it is a list.
    word_src: Option<String>,
    // A word has begun, even if it is still empty (`''`).
    started: bool,
    // The word so far is exactly one `\;`.
    escaped_semicolon: bool,
    // Open `#{` in the word so far, whose `}` is part of the word.
    formats: usize,
}

impl Level {
    fn end_word(&mut self) {
        if self.started {
            let src = self
                .word_src
                .take()
                .unwrap_or_else(|| quote_word(&self.word));
            self.word_srcs.push(src);
            self.words.push(std::mem::take(&mut self.word));
            self.started = false;
        }
    }

    fn end_command(&mut self) {
        if !self.words.is_empty() {
            self.cmds.push(std::mem::take(&mut self.words));
            self.srcs.push(std::mem::take(&mut self.word_srcs));
        }
    }

    /// The commands read and their source texts, the last word and command
    /// ended.
    fn finish(mut self) -> (Vec<Vec<String>>, Vec<Vec<String>>) {
        self.end_word();
        // A trailing `\;` word is dropped, as tmux drops it.
        if self.escaped_semicolon {
            self.words.pop();
            self.word_srcs.pop();
        }
        self.end_command();
        (self.cmds, self.srcs)
    }
}

/// [`split_command_string`] from `chars`. Nested `{ }` lists are levels on
/// an explicit stack, not recursion, so no input can exhaust the thread's
/// stack.
fn split_commands(
    chars: &mut std::iter::Peekable<std::str::Chars>,
) -> Result<Vec<Vec<String>>, ParseError> {
    let mut stack: Vec<Level> = vec![Level::default()];
    while let Some(c) = chars.next() {
        let in_list = stack.len() > 1;
        let Some(l) = stack.last_mut() else {
            break;
        };
        let fresh = !l.started;
        if !c.is_whitespace() && !(c == '\\' && fresh) {
            l.escaped_semicolon = false;
        }
        if fresh {
            l.formats = 0;
        }
        match c {
            c if c.is_whitespace() => l.end_word(),
            // A command cannot start with a list (tmux's grammar: `{{`
            // is a syntax error, recorded).
            '{' if fresh && l.words.is_empty() => return Err(syntax_error()),
            '{' if fresh => {
                if stack.len() >= MAX_LIST_DEPTH {
                    return Err(ParseError::Other("yacc stack overflow".to_string()));
                }
                stack.push(Level::default());
            }
            '}' if in_list && l.formats == 0 => {
                let (_, srcs) = stack.pop().map(Level::finish).unwrap_or_default();
                if let Some(parent) = stack.last_mut() {
                    let text = list_text(&srcs);
                    parent.word_src = Some(format!("{{ {text} }}"));
                    parent.word = text;
                    parent.started = true;
                }
            }
            '}' if l.formats > 0 => {
                l.formats -= 1;
                l.word.push('}');
            }
            // tmux 3.7b (recorded): an unquoted `}` with no list open is a
            // syntax error (`display }`, `display a}`).
            '}' => return Err(syntax_error()),
            '{' if l.word.ends_with('#') => {
                l.formats += 1;
                l.word.push('{');
            }
            ';' => {
                l.end_word();
                l.end_command();
            }
            '\\' => {
                l.started = true;
                l.escaped_semicolon = fresh && chars.peek() == Some(&';');
                read_escape(chars, &mut l.word)?;
            }
            '\'' => {
                l.started = true;
                for x in chars.by_ref() {
                    if x == '\'' {
                        break;
                    }
                    l.word.push(x);
                }
            }
            '"' => {
                l.started = true;
                if fresh && chars.peek() == Some(&'~') {
                    chars.next();
                    expand_tilde(chars, &mut l.word);
                }
                while let Some(x) = chars.next() {
                    match x {
                        '"' => break,
                        '\\' => read_escape(chars, &mut l.word)?,
                        '$' => expand_var(chars, &mut l.word)?,
                        x => l.word.push(x),
                    }
                }
            }
            '$' => {
                l.started = true;
                // tmux 3.7b's lexer quirk, recorded: `display $;` is a
                // syntax error, `display $; ` is `display $`.
                let mut ahead = chars.clone();
                if ahead.next() == Some(';') && ahead.next().is_none() {
                    return Err(syntax_error());
                }
                expand_var(chars, &mut l.word)?;
            }
            '~' if fresh => {
                l.started = true;
                expand_tilde(chars, &mut l.word);
            }
            c => {
                l.word.push(c);
                l.started = true;
            }
        }
    }
    // An unclosed `{`.
    if stack.len() > 1 {
        return Err(syntax_error());
    }
    Ok(stack.pop().map(|l| l.finish().0).unwrap_or_default())
}

/// One line of a tmux config file ([`split_config_file`]): the line it
/// starts on and its commands, which run as one group.
pub type ConfigLine = (usize, Vec<Vec<String>>);

/// What reading a config file asks of whoever runs it (tmux's parser does
/// both while it parses, `cmd-parse.y`).
pub trait ConfigHost {
    /// Whether a `%if`/`%elif` condition holds: `format` expanded, then
    /// tmux's `format_true` (neither empty nor `0`).
    fn condition(&mut self, format: &str) -> bool;
    /// `NAME=value` outside a false `%if` branch: set in the global
    /// environment now, so the lines after it see it (`$NAME`).
    fn assign(&mut self, assignment: &str, hidden: bool);
}

/// A host that expands nothing and sets nothing: a condition is its text.
struct Plain;

impl ConfigHost for Plain {
    fn condition(&mut self, format: &str) -> bool {
        !format.is_empty() && format != "0"
    }
    fn assign(&mut self, _: &str, _: bool) {}
}

/// [`split_config_file_with`] on a host that expands no format and sets no
/// variable.
pub fn split_config_file(text: &str) -> Result<Vec<ConfigLine>, (usize, ParseError)> {
    split_config_file_with(text, &mut Plain)
}

/// One open `%if`.
struct Scope {
    /// Some branch so far held.
    taken: bool,
    /// This branch's lines count.
    active: bool,
    else_seen: bool,
}

/// The state of [`split_config_file_with`]: the lines read, the open `%if`
/// scopes, and the host.
struct ConfigReader<'h> {
    lines: Vec<ConfigLine>,
    scopes: Vec<Scope>,
    host: &'h mut dyn ConfigHost,
    /// A `%if` format ran into the end of its line.
    broken_format: bool,
}

/// Whether `word` names an assignment, as tmux's lexer: `NAME=` with
/// `NAME` letters, digits and `_`, not starting with a digit.
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl ConfigReader<'_> {
    fn active(&self) -> bool {
        self.scopes.iter().all(|s| s.active)
    }

    /// One logical line, starting on `line`.
    fn line(&mut self, line: usize, text: &str) -> Result<(), (usize, ParseError)> {
        let at = |e: ParseError| (line, e);
        if std::mem::take(&mut self.broken_format) {
            return Err(at(syntax_error()));
        }
        // `%` starts a directive unless the word is all `%` and digits, then
        // it is a token (`%1`).
        if let Some(rest) = text.trim_start().strip_prefix('%') {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let word = &rest[..end];
            if !word.chars().all(|c| c == '%' || c.is_ascii_digit()) {
                return self.directive(word, &rest[end..]).map_err(at);
            }
        }
        let cmds = self.commands(text).map_err(at)?;
        if self.active() && !cmds.is_empty() {
            self.lines.push((line, cmds));
        }
        Ok(())
    }

    /// A physical line inside a `{ }` list, `cur[seg..]`: a directive is
    /// settled and cut out, a line in a false branch is cut out.
    fn block_line(
        &mut self,
        line: usize,
        cur: &mut String,
        seg: usize,
    ) -> Result<(), (usize, ParseError)> {
        if std::mem::take(&mut self.broken_format) {
            return Err((line, syntax_error()));
        }
        let text = cur[seg..].trim_start();
        let directive = text.strip_prefix('%').and_then(|rest| {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let word = &rest[..end];
            (!word.chars().all(|c| c == '%' || c.is_ascii_digit()))
                .then(|| (word.to_string(), rest[end..].to_string()))
        });
        let drop = match &directive {
            Some((word, rest)) => {
                self.directive(word, rest).map_err(|e| (line, e))?;
                true
            }
            None => !self.active(),
        };
        if drop {
            cur.truncate(seg);
        }
        Ok(())
    }

    /// `%if`, `%elif`, `%else`, `%endif` and `%hidden NAME=value`.
    fn directive(&mut self, word: &str, rest: &str) -> Result<(), ParseError> {
        match word {
            "if" => {
                let held = self.condition(rest)?;
                self.scopes.push(Scope {
                    taken: held,
                    active: held,
                    else_seen: false,
                });
            }
            "elif" => {
                if self.scopes.last().is_none_or(|s| s.else_seen) {
                    return Err(syntax_error());
                }
                let held = self.condition(rest)?;
                if let Some(scope) = self.scopes.last_mut() {
                    scope.active = held && !scope.taken;
                    scope.taken |= held;
                }
            }
            "else" | "endif" if !rest.trim().is_empty() => return Err(syntax_error()),
            "else" => match self.scopes.last_mut() {
                Some(scope) if !scope.else_seen => {
                    scope.active = !scope.taken;
                    scope.taken = true;
                    scope.else_seen = true;
                }
                _ => return Err(syntax_error()),
            },
            "endif" => {
                self.scopes.pop().ok_or_else(syntax_error)?;
            }
            "hidden" => {
                let mut cmds = split_command_string(rest)?;
                let assignment = match cmds.as_mut_slice() {
                    [cmd] if cmd.len() == 1 && is_assignment(&cmd[0]) => cmd.remove(0),
                    _ => return Err(syntax_error()),
                };
                if self.active() {
                    self.host.assign(&assignment, true);
                }
            }
            _ => return Err(syntax_error()),
        }
        Ok(())
    }

    /// The condition of a `%if` or `%elif`: exactly one word.
    fn condition(&mut self, rest: &str) -> Result<bool, ParseError> {
        match split_command_string(rest)?.as_slice() {
            [cmd] if cmd.len() == 1 => Ok(self.host.condition(&cmd[0])),
            _ => Err(syntax_error()),
        }
    }

    /// A line's commands, with `NAME=value` at the start of one taken out
    /// and set as tmux's parser sets it, so a `$NAME` later on the line
    /// (or after it) reads the new value.
    // ponytail: the whole line is read again after each assignment, so a
    // `$NAME` before the assignment on the same line sees it too.
    fn commands(&mut self, text: &str) -> Result<Vec<Vec<String>>, ParseError> {
        let mut applied = 0;
        // A line's newline follows a `$` at its end, which is then a plain
        // `$`; at the end of a string it is a syntax error.
        let text = if text.ends_with('$') {
            format!("{text}\n")
        } else {
            text.to_string()
        };
        loop {
            let mut cmds = split_command_string(&text)?;
            let assigning: Vec<usize> = (0..cmds.len())
                .filter(|i| cmds[*i].first().is_some_and(|w| is_assignment(w)))
                .collect();
            if let Some(i) = assigning.get(applied) {
                if self.active() {
                    self.host.assign(&cmds[*i][0], false);
                }
                applied += 1;
                continue;
            }
            for i in assigning {
                cmds[i].remove(0);
                // `A=1 B=2`: a command name cannot be an assignment.
                if cmds[i].first().is_some_and(|w| is_assignment(w)) {
                    return Err(syntax_error());
                }
            }
            cmds.retain(|c| !c.is_empty());
            return Ok(cmds);
        }
    }
}

/// Splits a tmux config file into its lines' commands, as tmux 3.7b's
/// `source-file` reads one (recorded on a private socket):
///
/// - a backslash-newline joins two lines anywhere, in quotes too (an odd
///   run of backslashes before the newline; `\\` then a newline does not);
/// - an unquoted newline ends a command and its line, except inside a
///   `{ }` list, where it separates the list's commands;
/// - quotes may span lines and keep the newline;
/// - a `#` that starts a word, outside quotes, is a comment to the end of
///   the line (`#{...}` too; mid-word, `a#b` is a word), except after a
///   `%` directive, where `#{...}` is a format, read raw to its closing
///   brace, spaces and all;
/// - each line is then a command string ([`split_command_string`]);
/// - `%if`, `%elif`, `%else` and `%endif` choose which lines count, the
///   condition a format `host` expands ([`ConfigHost::condition`]); a
///   false branch is still read, so a syntax error in it is an error;
/// - `NAME=value`, alone or before a command, and `%hidden NAME=value`
///   are set through `host` as they are read.
///
/// An error names the line it is on; an unterminated `{` or `%if` is
/// reported at the end of the file.
pub fn split_config_file_with(
    text: &str,
    host: &mut dyn ConfigHost,
) -> Result<Vec<ConfigLine>, (usize, ParseError)> {
    let mut reader = ConfigReader {
        lines: Vec::new(),
        scopes: Vec::new(),
        host,
        broken_format: false,
    };
    let mut cur = String::new();
    let mut start = None;
    let flush = |reader: &mut ConfigReader, start: &mut Option<usize>, cur: &mut String| {
        let text = std::mem::take(cur);
        match start.take() {
            Some(line) => reader.line(line, &text),
            None => Ok(()),
        }
    };
    let src = join_continuations(text);
    let mut chars = src.iter().copied().peekable();
    // Outside quotes: at a word boundary, the open `{ }` lists, and the
    // open `#{` in the current word.
    let (mut word_start, mut depth, mut formats) = (true, 0usize, 0usize);
    // Where the current physical line starts in `cur`, inside a `{ }` list.
    let mut seg = 0;
    // The `%if` scopes open where the outermost `{ }` list began.
    let mut open_scopes = 0;
    while let Some((c, line)) = chars.next() {
        if start.is_none() && !c.is_whitespace() {
            start = Some(line);
        }
        match c {
            // Inside a `{ }` list the directives still count (tmux reads
            // them at the start of a line there too): one is settled here
            // and its line leaves the text, as does a line in a false branch.
            '\n' if depth > 0 => {
                reader.block_line(line, &mut cur, seg)?;
                cur.push_str(" ; ");
                seg = cur.len();
            }
            '\n' => {
                seg = 0;
                flush(&mut reader, &mut start, &mut cur)?
            }
            // After a directive word a `#{` opens a format, not a comment.
            '#' if word_start
                && chars.peek().is_some_and(|(n, _)| *n == '{')
                && is_directive_word(&cur[seg..]) =>
            {
                chars.next();
                match raw_format(&mut chars) {
                    Some(format) => cur.push_str(&quote_word(&format)),
                    None => reader.broken_format = true,
                }
                word_start = false;
                continue;
            }
            '#' if word_start => {
                while chars.next_if(|(c, _)| *c != '\n').is_some() {}
                if cur.trim().is_empty() {
                    start = None;
                }
                continue;
            }
            c if c.is_whitespace() || c == ';' => cur.push(c),
            '\\' => {
                cur.push(c);
                if let Some((n, _)) = chars.next() {
                    cur.push(n);
                }
            }
            '\'' | '"' => {
                cur.push(c);
                while let Some((x, _)) = chars.next() {
                    cur.push(x);
                    if x == '\\' && c == '"' {
                        if let Some((n, _)) = chars.next() {
                            cur.push(n);
                        }
                    } else if x == c {
                        break;
                    }
                }
            }
            '{' if word_start => {
                if depth == 0 {
                    open_scopes = reader.scopes.len();
                }
                depth += 1;
                cur.push(c);
                continue;
            }
            '{' if cur.ends_with('#') => {
                formats += 1;
                cur.push(c);
            }
            '}' => {
                if formats > 0 {
                    formats -= 1;
                } else {
                    depth = depth.saturating_sub(1);
                    // A `%if` opened inside a list closes inside it.
                    if depth == 0 && reader.scopes.len() != open_scopes {
                        return Err((line, syntax_error()));
                    }
                }
                cur.push(c);
            }
            c => cur.push(c),
        }
        word_start = c.is_whitespace() || c == ';';
        if word_start {
            formats = 0;
        }
    }
    // An unclosed `{` or `%if` is an error at the end of the file, as tmux
    // reports it (recorded: a file of four lines ending in a newline says
    // line 5).
    let eof = src
        .last()
        .map_or(1, |(c, line)| line + usize::from(*c == '\n'));
    let unclosed = depth > 0;
    flush(&mut reader, &mut start, &mut cur)
        .map_err(|(line, e)| (if unclosed { eof } else { line }, e))?;
    if !reader.scopes.is_empty() {
        return Err((eof, syntax_error()));
    }
    Ok(reader.lines)
}

/// Whether `cur` is nothing but one `%` directive word and the blanks
/// after it (where tmux's lexer expects a condition).
fn is_directive_word(cur: &str) -> bool {
    let word = cur.trim();
    word.starts_with('%') && !word.contains(char::is_whitespace)
}

/// A format after `#{`, read raw to its closing brace as tmux's
/// `yylex_format`: `#{` nests, `#` escapes the next character, and the end
/// of the line is an error (`None`). The `#{` is in the result.
fn raw_format(chars: &mut impl Iterator<Item = (char, usize)>) -> Option<String> {
    let mut out = String::from("#{");
    let mut brackets = 1;
    loop {
        let (mut ch, _) = chars.next().filter(|(c, _)| *c != '\n')?;
        if ch == '#' {
            out.push('#');
            ch = chars.next().filter(|(c, _)| *c != '\n')?.0;
            if ch == '{' {
                brackets += 1;
            }
        } else if ch == '}' {
            brackets -= 1;
            if brackets == 0 {
                out.push('}');
                return Some(out);
            }
        }
        out.push(ch);
    }
}

/// `text`'s characters, each with its line number, with every
/// backslash-newline taken out (tmux's `yylex_getc`: an odd run of
/// backslashes before a newline).
fn join_continuations(text: &str) -> Vec<(char, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::with_capacity(chars.len());
    let (mut i, mut line) = (0, 1);
    while i < chars.len() {
        if chars[i] == '\\' {
            let run = chars[i..].iter().take_while(|c| **c == '\\').count();
            let joins = run % 2 == 1 && chars.get(i + run) == Some(&'\n');
            let kept = run - usize::from(joins);
            out.extend(std::iter::repeat_n(('\\', line), kept));
            i += run;
            if joins {
                i += 1;
                line += 1;
            }
            continue;
        }
        out.push((chars[i], line));
        if chars[i] == '\n' {
            line += 1;
        }
        i += 1;
    }
    out
}

/// A parsed `{ }` list as the one argument a command gets: the source text
/// of its commands joined by ` ; `, which split back to exactly the parsed
/// words, as tmux runs the parsed list itself. Nested lists stay `{ }`, so
/// the text grows with the input, not with its depth.
fn list_text(srcs: &[Vec<String>]) -> String {
    srcs.iter()
        .map(|w| w.join(" "))
        .collect::<Vec<_>>()
        .join(" ; ")
}

/// A word as source text that splits back to it: in single quotes, a `'`
/// as `'\''`, so backslashes, `$` and `~` stay as they are.
fn quote_word(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// One argument as tmux 3.7b's `args_escape` prints it: `''` when empty;
/// double quotes around a word holding any of ` #';${}%`, else single
/// quotes around one holding a space or `"`; a single such character
/// (or `~`) as a backslash and itself; inside, `\\`, `\"` (in double
/// quotes), `\$` before a name, C escapes (`\n`, `\t`, ...) and octal
/// for other control characters; a leading `~` escaped.
pub fn escape_arg(s: &str) -> String {
    const DQUOTED: &[char] = &[' ', '#', '\'', ';', '$', '{', '}', '%'];
    const SQUOTED: &[char] = &[' ', '"'];
    if s.is_empty() {
        return "''".to_string();
    }
    let quote = if s.contains(DQUOTED) {
        Some('"')
    } else if s.contains(SQUOTED) {
        Some('\'')
    } else {
        None
    };
    let mut chars = s.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && c != ' '
        && (quote.is_some() || c == '~')
    {
        return format!("\\{c}");
    }
    let dq = quote == Some('"');
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '$' if dq => {
                if it
                    .peek()
                    .is_some_and(|n| n.is_ascii_alphabetic() || *n == '_' || *n == '{')
                {
                    out.push('\\');
                }
                out.push('$');
            }
            '"' if dq => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{7}' => out.push_str("\\a"),
            '\u{b}' => out.push_str("\\v"),
            '\u{c}' => out.push_str("\\f"),
            '\0' => {
                out.push_str("\\0");
                if it.peek().is_some_and(|n| ('0'..='7').contains(n)) {
                    out.push_str("00");
                }
            }
            c if c.is_ascii_control() => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        }
    }
    match quote {
        Some(q) if out.starts_with('~') => format!("{q}\\{out}{q}"),
        Some(q) => format!("{q}{out}{q}"),
        None if out.starts_with('~') => format!("\\{out}"),
        None => out,
    }
}

/// The character a backslash before `c` stands for, in a bare word or in
/// double quotes, as tmux 3.7b's lexer reads it (recorded: `"a\tb\nc\sd\ee"`
/// is a tab, a newline, a space and an escape): `\a \b \e \f \n \r \s \t
/// \v`, and any other character itself.
fn escape(c: char) -> char {
    match c {
        'a' => '\x07',
        'b' => '\x08',
        'e' => '\x1b',
        'f' => '\x0c',
        'n' => '\n',
        'r' => '\r',
        's' => ' ',
        't' => '\t',
        'v' => '\x0b',
        c => c,
    }
}

/// After a backslash in a bare word or in double quotes, as tmux 3.7b's
/// `yylex_token_escape` reads it (recorded): three octal digits, the first
/// 0 to 3 (else `invalid octal escape`), `\u` and four hex digits, `\U` and
/// eight (`invalid \u argument` when one is not hex or the code point is
/// not a character), or the character `escape` stands for.
fn read_escape(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    out: &mut String,
) -> Result<(), ParseError> {
    let invalid = |t: char| ParseError::Other(format!("invalid \\{t} argument"));
    let octal = || ParseError::Other("invalid octal escape".to_string());
    let Some(c) = chars.next() else {
        out.push('\\');
        return Ok(());
    };
    match c {
        '4'..='7' => Err(octal()),
        '0'..='3' => {
            let mut value = c.to_digit(8).unwrap_or(0);
            for _ in 0..2 {
                match chars.next().and_then(|d| d.to_digit(8)) {
                    Some(d) => value = value * 8 + d,
                    None => return Err(octal()),
                }
            }
            out.push(char::from(value as u8));
            Ok(())
        }
        'u' | 'U' => {
            let size = if c == 'u' { 4 } else { 8 };
            let mut code = 0u32;
            for _ in 0..size {
                match chars.next() {
                    // The end of the line or file ends the word quietly.
                    None | Some('\n') => return Ok(()),
                    Some(d) => match d.to_digit(16) {
                        Some(d) => code = code.wrapping_mul(16).wrapping_add(d),
                        None => return Err(invalid(c)),
                    },
                }
            }
            out.push(char::from_u32(code).ok_or_else(|| invalid(c))?);
            Ok(())
        }
        c => {
            out.push(escape(c));
            Ok(())
        }
    }
}

/// Where [`split_command_string`] looks a `$NAME` up.
pub type VariableSource = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

static VARIABLES: std::sync::OnceLock<VariableSource> = std::sync::OnceLock::new();

/// Makes `source` the one [`split_command_string`] expands `$NAME` from,
/// for the rest of the process. A server sets its global environment
/// (`set-environment -g`, hidden variables included, as tmux 3.7b) once at
/// start; a later call changes nothing.
pub fn set_variable_source(source: VariableSource) {
    let _ = VARIABLES.set(source);
}

thread_local! {
    /// Variables [`with_variables`] lays over the source, last one first.
    static OVERLAY: std::cell::RefCell<Vec<(String, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs `f` with `vars` over [`split_command_string`]'s variable source:
/// the config loader's `[environment]`, which tmux would already have set
/// with `set-environment -g` when it read a later line (TP.0).
pub fn with_variables<R>(vars: &[(String, String)], f: impl FnOnce() -> R) -> R {
    let outer = OVERLAY.replace(vars.to_vec());
    let out = f();
    OVERLAY.set(outer);
    out
}

/// A variable's value for [`split_command_string`], empty when unset:
/// from [`with_variables`]'s overlay, then [`set_variable_source`]'s
/// source, else (outside a server) the process environment.
pub fn env_var(name: &str) -> String {
    let overlaid = OVERLAY.with_borrow(|vars| {
        vars.iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    });
    if let Some(value) = overlaid {
        return value;
    }
    match VARIABLES.get() {
        Some(source) => source(name),
        None => std::env::var(name).ok(),
    }
    .unwrap_or_default()
}

fn syntax_error() -> ParseError {
    ParseError::Other("syntax error".to_string())
}

/// After a `$`: `${NAME}` or `NAME` (letters, digits, `_`), expanded into
/// `out`; a `$` before anything else is literal. As tmux 3.7b's lexer: a
/// `$` at the end is a syntax error, and `${` must close over name
/// characters.
fn expand_var(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    out: &mut String,
) -> Result<(), ParseError> {
    let is_name = |c: &char| c.is_ascii_alphanumeric() || *c == '_';
    let mut name = String::new();
    match chars.peek() {
        None => return Err(syntax_error()),
        Some('{') => {
            chars.next();
            loop {
                match chars.next() {
                    Some('}') => break,
                    Some(c) if is_name(&c) => name.push(c),
                    _ => {
                        return Err(ParseError::Other(
                            "invalid environment variable".to_string(),
                        ));
                    }
                }
            }
        }
        Some(c) if c.is_ascii_alphabetic() || *c == '_' => {
            while let Some(c) = chars.next_if(is_name) {
                name.push(c);
            }
        }
        Some(_) => {
            out.push('$');
            return Ok(());
        }
    }
    out.push_str(&env_var(&name));
    Ok(())
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

pub use super::table::canonical_name;

/// The generic path of every table verb: scan the flags, check the
/// positional bounds (tmux's wording), hand them to the row's builder.
fn build(row: &Row, rest: &[String]) -> Result<Command, ParseError> {
    let flags = if row.raw {
        ParsedFlags::default()
    } else {
        scan(rest, row.flags, row.long, row.name)?
    };
    let given = flags.positionals.len();
    if !rest.is_empty() && given < row.min {
        return Err(ParseError::TooFewArguments(row.name.to_string(), row.min));
    }
    if given > row.max {
        return Err(ParseError::TooManyArguments(row.name.to_string(), row.max));
    }
    (row.build)(&Args {
        name: row.name,
        argv: rest,
        flags,
    })
}

/// Parses one command's argv (spec §4.5): `argv[0]` is the command name,
/// the rest are that command's own flags and positionals.
pub fn parse(argv: &[String]) -> Result<Command, ParseError> {
    let (name, rest) = argv.split_first().ok_or(ParseError::EmptyCommand)?;
    if let Some(row) = table::find(name) {
        return build(row, rest);
    }
    Err(ParseError::UnknownCommand(name.clone()))
}

pub(super) fn agent_sidebar(flags: &Args) -> Result<Command, ParseError> {
    let op = match flags.positionals.first().map(String::as_str) {
        Some("on") => SidebarOp::On,
        Some("off") => SidebarOp::Off,
        Some("toggle") => SidebarOp::Toggle,
        Some("focus") => SidebarOp::Focus,
        Some(other) => {
            return Err(ParseError::Other(format!(
                "agent-sidebar: unknown operation {other}"
            )));
        }
        None => {
            return Err(ParseError::Other(
                "agent-sidebar: on, off, toggle or focus required".into(),
            ));
        }
    };
    Ok(Command::AgentSidebar(op))
}

#[cfg(test)]
mod tests {
    #[test]
    fn agent_report_parses_flags() {
        assert_eq!(
            parse(&argv(&[
                "agent", "report", "done", "-t", "%3", "--msg", "ok", "--ts", "5"
            ])),
            Ok(Command::Agent(AgentCmd::Report(AgentReport {
                state: "done".into(),
                target: parse_target("%3").unwrap(),
                kind: None,
                msg: Some("ok".into()),
                blocked: None,
                session_id: None,
                ts: Some(5),
            })))
        );
    }

    #[test]
    fn report_needs_state() {
        assert_eq!(
            parse(&argv(&["agent", "report"])),
            Err(ParseError::Other("agent report: state required".into()))
        );
    }

    #[test]
    fn blocked_values() {
        assert!(parse(&argv(&["agent", "report", "blocked", "--blocked", "foo"])).is_err());
        assert!(parse(&argv(&["agent", "report", "blocked", "--blocked", "auth"])).is_ok());
    }

    #[test]
    fn seen_a_and_t_conflict() {
        assert!(parse(&argv(&["agent", "seen", "-a", "-t", "%1"])).is_err());
        assert_eq!(
            parse(&argv(&["agent", "seen", "-a"])),
            Ok(Command::Agent(AgentCmd::Seen {
                target: Target::Current,
                all: true
            }))
        );
    }

    #[test]
    fn unknown_subcommand() {
        assert_eq!(
            parse(&argv(&["agent", "x"])),
            Err(ParseError::Other("agent: unknown subcommand x".into()))
        );
    }

    #[test]
    fn agent_new_example() {
        assert_eq!(
            parse(&argv(&["agent", "new", "claude", "-w", "-b", "feat/x"])),
            Ok(Command::Agent(AgentCmd::New(AgentNew {
                kind: "claude".into(),
                split: false,
                cwd: None,
                name: None,
                worktree: Some(true),
                branch: Some("feat/x".into()),
                detached: false,
            })))
        );
        let Ok(Command::Agent(AgentCmd::New(n))) = parse(&argv(&[
            "agent", "new", "pi", "-p", "-d", "-W", "-c", "/r", "-n", "nm",
        ])) else {
            panic!("parse")
        };
        assert!(n.split && n.detached);
        assert_eq!(n.worktree, Some(false));
        assert_eq!(n.cwd.as_deref(), Some("/r"));
        assert_eq!(n.name.as_deref(), Some("nm"));
        assert!(parse(&argv(&["agent", "new"])).is_err());
    }

    #[test]
    fn w_and_big_w_conflict() {
        assert!(parse(&argv(&["agent", "new", "claude", "-w", "-W"])).is_err());
    }

    #[test]
    fn sidebar_op_required() {
        assert!(parse(&argv(&["agent-sidebar"])).is_err());
        assert!(parse(&argv(&["agent-sidebar", "sideways"])).is_err());
        assert_eq!(
            parse(&argv(&["agent-sidebar", "focus"])),
            Ok(Command::AgentSidebar(SidebarOp::Focus))
        );
        assert_eq!(parse(&argv(&["agent-grid"])), Ok(Command::AgentGrid));
        assert!(parse(&argv(&["agent-grid", "x"])).is_err());
    }

    #[test]
    fn agent_list_and_next() {
        assert_eq!(
            parse(&argv(&["agent", "list", "-F", "#{x}"])),
            Ok(Command::Agent(AgentCmd::List {
                format: Some("#{x}".into())
            }))
        );
        assert_eq!(
            parse(&argv(&["agent", "next"])),
            Ok(Command::Agent(AgentCmd::Next))
        );
    }

    use super::*;
    use crate::cmd::target::{PanePart, PaneRef, SessionRef, WindowRef};
    use crate::cmd::*;
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
                ..Default::default()
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

    /// TC.9: the corpus's `new-session -d -s x -n w -P -F fmt`
    /// (opensessions, sessionx), `display-message -c <client> -F fmt` and
    /// the stock `choose-tree -Zs`.
    #[test]
    fn tc9_flags_parse() {
        assert_eq!(
            parse(&argv(&[
                "new-session",
                "-dP",
                "-s",
                "x",
                "-n",
                "w",
                "-F",
                "#S"
            ]))
            .unwrap(),
            Command::NewSession(NewSession {
                detached: true,
                name: Some("x".to_string()),
                print: true,
                format: Some("#S".to_string()),
                window_name: Some("w".to_string()),
                ..Default::default()
            })
        );
        assert_eq!(
            parse(&argv(&[
                "display-message",
                "-p",
                "-c",
                "c1",
                "-d",
                "5",
                "-F",
                "#{pane_id}"
            ]))
            .unwrap(),
            Command::DisplayMessage(DisplayMessage {
                print: true,
                target: Target::Current,
                text: Some("#{pane_id}".to_string()),
                client: Some("c1".to_string()),
                delay: Some("5".to_string()),
                all: false,
                literal: false,
                verbose: false,
                ignore_keys: false,
            })
        );
        assert_eq!(
            parse(&argv(&["choose-tree", "-Zs"])).unwrap(),
            Command::ChooseTree(ChooseTree {
                zoom: true,
                sessions: true,
                windows: false,
                ..Default::default()
            })
        );
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
                target: Target::Session(SessionRef::Bare("work".to_string())),
                ..KillSession::default()
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
                filter: None,
                sort: SortFlags::default(),
            })
        );
    }

    #[test]
    fn alias_lsp_for_list_panes() {
        let cmd = parse(&argv(&["lsp", "-t", "work"])).unwrap();
        assert_eq!(
            cmd,
            Command::ListPanes(ListPanes {
                target: Target::Session(SessionRef::Bare("work".to_string())),
                format: None,
                all: false,
                session: false,
                filter: None,
                sort: SortFlags::default(),
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
                target: Target::Session(SessionRef::Bare("work".to_string())),
                name: Some("editor".to_string()),
                cwd: Some("/tmp".to_string()),
                command: Some(vec!["vim".to_string()]),
                after: false,
                print: false,
                format: None,
                ..NewWindow::default()
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
                before: false,
                full: false,
                detached: false,
                print: false,
                format: None,
                ..SplitWindow::default()
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
                before: false,
                full: false,
                detached: false,
                print: false,
                format: None,
                ..SplitWindow::default()
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
                before: false,
                full: false,
                detached: false,
                print: false,
                format: None,
                ..SplitWindow::default()
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
                before: false,
                full: false,
                detached: false,
                print: false,
                format: None,
                ..SplitWindow::default()
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
                ..KillPane::default()
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
    fn the_b3_pane_and_window_flags_parse() {
        let Ok(Command::SelectWindow(w)) = parse(&argv(&["select-window", "-nl", "-T"])) else {
            panic!("select-window");
        };
        assert_eq!((w.step, w.toggle), (Some(WindowMove::Next), true));
        let Ok(Command::LastPane(l)) = parse(&argv(&["last-pane", "-Z", "-d"])) else {
            panic!("last-pane");
        };
        assert_eq!((l.keep_zoom, l.input), (true, Some(false)));
        let Ok(Command::SelectPane(p)) = parse(&argv(&["select-pane", "-d", "-e"])) else {
            panic!("select-pane");
        };
        assert_eq!(p.input, Some(true));
        let Ok(Command::SendKeys(k)) = parse(&argv(&["send-keys", "-RK", "-c", "x", "a"])) else {
            panic!("send-keys");
        };
        assert_eq!(
            (k.reset, k.client_key, k.client.as_deref()),
            (true, true, Some("x"))
        );
        let Ok(Command::CapturePane(c)) = parse(&argv(&["capture-pane", "-b", "n"])) else {
            panic!("capture-pane");
        };
        assert_eq!((c.print, c.buffer.as_deref()), (false, Some("n")));
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
                last: false,
                keep_zoom: false,
                title: None,
                input: None,
                mark: Mark::No,
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
                last: false,
                keep_zoom: false,
                title: None,
                input: None,
                mark: Mark::No,
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
                width: None,
                height: None,
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
                width: None,
                height: None,
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
                width: None,
                height: None,
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
                width: None,
                height: None,
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
                cwd: None,
                env: Vec::new(),
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
                buffer: None,
                opts: CaptureOpts {
                    start: Some(CaptureBound::HistoryStart),
                    end: Some(CaptureBound::VisibleEnd),
                    escapes: false,
                },
                join: false,
                keep_blanks: false,
                written_only: false,
                numbered: false,
                flags: false,
                escaped: false,
                links: false,
                alternate: false,
                mode: false,
                pending: false,
                quiet: false,
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
                buffer: None,
                opts: CaptureOpts {
                    start: Some(CaptureBound::Line(-5)),
                    end: Some(CaptureBound::Line(0)),
                    escapes: false,
                },
                join: false,
                keep_blanks: false,
                written_only: false,
                numbered: false,
                flags: false,
                escaped: false,
                links: false,
                alternate: false,
                mode: false,
                pending: false,
                quiet: false,
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
                buffer: None,
                opts: CaptureOpts {
                    start: None,
                    end: None,
                    escapes: false,
                },
                join: false,
                keep_blanks: false,
                written_only: false,
                numbered: false,
                flags: false,
                escaped: false,
                links: false,
                alternate: false,
                mode: false,
                pending: false,
                quiet: false,
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
                unset_panes: false,
                persist: true,
                no_persist: false,
                target: Target::Current,
                name: "history-limit".to_string(),
                value: Some("50000".to_string()),
                append: false,
                expand: false,
                quiet: false,
                only_if_unset: false,
                shown: None,
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
                unset_panes: false,
                persist: false,
                no_persist: false,
                target: Target::Current,
                name: "@my-opt".to_string(),
                value: None,
                append: false,
                expand: false,
                quiet: false,
                only_if_unset: false,
                shown: None,
            })
        );
    }

    #[test]
    fn set_option_no_persist_is_parsed() {
        let Ok(Command::SetOption(c)) = parse(&argv(&["set", "-p", "--no-persist", "@a", "y"]))
        else {
            panic!("not a set-option");
        };
        assert!(c.no_persist && !c.persist);
    }

    #[test]
    fn pane_set_resume_parses_its_three_forms() {
        let pane = parse_target("%1").expect("target");
        assert_eq!(
            parse(&argv(&[
                "pane-set-resume",
                "-t",
                "%1",
                "--",
                "agent",
                "--resume",
                "a"
            ])),
            Ok(Command::PaneSetResume(PaneSetResume {
                target: pane.clone(),
                argv: argv(&["agent", "--resume", "a"]),
                ..PaneSetResume::default()
            }))
        );
        // Without `--`, the first word that is not a flag starts the argv.
        assert_eq!(
            parse(&argv(&["pane-set-resume", "nvim", "-S", "s.vim"])),
            Ok(Command::PaneSetResume(PaneSetResume {
                argv: argv(&["nvim", "-S", "s.vim"]),
                ..PaneSetResume::default()
            }))
        );
        assert_eq!(
            parse(&argv(&["pane-set-resume", "-t", "%1", "-u"])),
            Ok(Command::PaneSetResume(PaneSetResume {
                target: pane.clone(),
                unset: true,
                ..PaneSetResume::default()
            }))
        );
        for form in [
            &["pane-set-resume", "--expect", "no id", "-t", "%1"][..],
            &["pane-set-resume", "-t", "%1", "--expect=no id"][..],
        ] {
            assert_eq!(
                parse(&argv(form)),
                Ok(Command::PaneSetResume(PaneSetResume {
                    target: pane.clone(),
                    expect: Some("no id".to_string()),
                    ..PaneSetResume::default()
                })),
                "{form:?}"
            );
        }
        // A word after `--` that looks like `--expect` is the argv's.
        assert_eq!(
            parse(&argv(&["pane-set-resume", "--", "a", "--expect", "x"])),
            Ok(Command::PaneSetResume(PaneSetResume {
                argv: argv(&["a", "--expect", "x"]),
                ..PaneSetResume::default()
            }))
        );
    }

    #[test]
    fn pane_set_resume_takes_exactly_one_form() {
        for bad in [
            &["pane-set-resume"][..],
            &["pane-set-resume", "-u", "--", "a"][..],
            &["pane-set-resume", "--expect", "x", "--", "a"][..],
            &["pane-set-resume", "-u", "--expect", "x"][..],
            &["pane-set-resume", "--expect"][..],
            &["pane-set-resume", "--expect", ""][..],
            &["pane-set-resume", "--bogus", "--", "a"][..],
        ] {
            assert!(parse(&argv(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn reboot_check_takes_json_and_nothing_else() {
        assert_eq!(
            parse(&argv(&["reboot-check"])),
            Ok(Command::RebootCheck(RebootCheck { json: false }))
        );
        assert_eq!(
            parse(&argv(&["reboot-check", "--json"])),
            Ok(Command::RebootCheck(RebootCheck { json: true }))
        );
        for bad in [
            &["reboot-check", "--bogus"][..],
            &["reboot-check", "-x"][..],
            &["reboot-check", "word"][..],
        ] {
            assert!(parse(&argv(bad)).is_err(), "{bad:?}");
        }
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
                value_only: false,
                quiet: false,
                inherited: false,
                hooks: false,
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
                value_only: false,
                quiet: false,
                inherited: false,
                hooks: false,
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
                client: None,
                delay: None,
                all: false,
                literal: false,
                verbose: false,
                ignore_keys: false,
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
                target: None,
            })
        );
    }

    #[test]
    fn subscribe_with_no_events_means_all() {
        let cmd = parse(&argv(&["subscribe"])).unwrap();
        assert_eq!(cmd, Command::Subscribe(Subscribe::default()));
    }

    #[test]
    fn subscribe_takes_a_target_before_or_after_the_names() {
        let want = Command::Subscribe(Subscribe {
            events: vec!["pane-exited".to_string()],
            target: Some(parse_target("%3").unwrap()),
        });
        assert_eq!(
            parse(&argv(&["subscribe", "pane-exited", "-t", "%3"])).unwrap(),
            want
        );
        assert_eq!(
            parse(&argv(&["subscribe", "-t", "%3", "pane-exited"])).unwrap(),
            want
        );
        assert!(parse(&argv(&["subscribe", "pane-exited", "-t"])).is_err());
    }

    #[test]
    fn double_dash_ends_flags() {
        let cmd = parse(&argv(&["new-window", "-t", "work", "--", "-x"])).unwrap();
        assert_eq!(
            cmd,
            Command::NewWindow(NewWindow {
                detached: false,
                target: Target::Session(SessionRef::Bare("work".to_string())),
                name: None,
                cwd: None,
                command: Some(vec!["-x".to_string()]),
                after: false,
                print: false,
                format: None,
                ..NewWindow::default()
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
                step: None,
                toggle: false,
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
        Target::Session(SessionRef::Bare(name.to_string()))
    }

    #[test]
    fn attach_session_flags_and_alias() {
        let want = Command::AttachSession(AttachSession {
            target: session("work"),
            detach_others: true,
            nest: true,
            ..AttachSession::default()
        });
        let full = ["attach-session", "-d", "-t", "work", "--nest"];
        assert_eq!(parse(&argv(&full)), Ok(want.clone()));
        let short = ["attach", "-dtwork", "--nest"];
        assert_eq!(parse(&argv(&short)), Ok(want));
        assert_eq!(
            parse(&argv(&["attach"])),
            Ok(Command::AttachSession(AttachSession {
                target: Target::Current,
                ..AttachSession::default()
            }))
        );
    }

    /// tmux's `-c -E -f -r -x`: the session's directory, no environment
    /// update, client flags, a read-only client, and `-d` with a hangup.
    #[test]
    fn attach_session_takes_tmux_s_other_flags() {
        let Ok(Command::AttachSession(a)) = parse(&argv(&[
            "attach",
            "-r",
            "-E",
            "-c",
            "/w",
            "-f",
            "ignore-size",
            "-x",
        ])) else {
            panic!("attach")
        };
        assert!(a.read_only && a.no_env && a.kill_parent && a.detach_others);
        assert_eq!(a.cwd.as_deref(), Some("/w"));
        assert_eq!(a.flags.as_deref(), Some("ignore-size"));
    }

    #[test]
    fn detach_client_flags() {
        assert_eq!(
            parse(&argv(&["detach-client", "-a", "-s", "work", "-t", "c1"])),
            Ok(Command::DetachClient(DetachClient {
                session: Some(session("work")),
                client: Some("c1".to_string()),
                all_others: true,
                kill_parent: false,
            }))
        );
        assert_eq!(
            parse(&argv(&["detach-client"])),
            Ok(Command::DetachClient(DetachClient {
                session: None,
                client: None,
                all_others: false,
                kill_parent: false,
            }))
        );
    }

    #[test]
    fn list_clients_with_format() {
        assert_eq!(
            parse(&argv(&["list-clients", "-F", "#{client_width}"])),
            Ok(Command::ListClients(ListClients {
                format: Some("#{client_width}".to_string()),
                ..ListClients::default()
            }))
        );
        assert_eq!(
            parse(&argv(&["list-clients", "-t", "main", "-f", "1"])),
            Ok(Command::ListClients(ListClients {
                format: None,
                target: Some(Target::Session(SessionRef::Bare("main".to_string()))),
                filter: Some("1".to_string()),
                sort: SortFlags::default(),
            }))
        );
    }

    #[test]
    fn window_steps_take_a_session_target() {
        let step = WindowStep {
            target: session("s"),
            alert: false,
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
                client: None,
                keep_zoom: false,
                no_env: false,
                toggle_read_only: false,
                order: None,
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
                client: None,
                keep_zoom: false,
                no_env: false,
                toggle_read_only: false,
                order: None,
            }))
        );
    }

    #[test]
    fn send_prefix_takes_a_pane_target() {
        assert_eq!(
            parse(&argv(&["send-prefix"])),
            Ok(Command::SendPrefix(SendPrefix {
                target: Target::Current,
                second: false,
            }))
        );
        let Ok(Command::SendPrefix(c)) = parse(&argv(&["send-prefix", "-2"])) else {
            panic!("send-prefix -2");
        };
        assert!(c.second);
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
            // A `{ }` list is one argument, its parsed words quoted so they
            // split back as they were.
            (
                "command-prompt -I \"#S\" { rename-session -- \"%%\" }",
                vec![argv(&[
                    "command-prompt",
                    "-I",
                    "#S",
                    "'rename-session' '--' '%%'",
                ])],
            ),
            (
                "if -F 1 { display -p \"a b\" } { display c ; neww -n 'x;y' }",
                vec![argv(&[
                    "if",
                    "-F",
                    "1",
                    "'display' '-p' 'a b'",
                    "'display' 'c' ; 'neww' '-n' 'x;y'",
                ])],
            ),
            (
                "if -F 1 {display x}",
                vec![argv(&["if", "-F", "1", "'display' 'x'"])],
            ),
            ("if -F 1 {}", vec![argv(&["if", "-F", "1", ""])]),
            (
                "if -F 1 { display \"#{pane_id}\" }",
                vec![argv(&["if", "-F", "1", "'display' '#{pane_id}'"])],
            ),
            ("display a{b", vec![argv(&["display", "a{b"])]),
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
            ("display $ a", vec![argv(&["display", "$", "a"])]),
        ];
        for (input, want) in cases {
            assert_eq!(split_command_string(input), Ok(want), "{input:?}");
        }
        // tmux 3.7b's syntax errors: an unclosed list, a stray `}`, a
        // command that starts with a list (20000 `{`, from a config).
        let braces = "{".repeat(20000);
        for input in [
            "if 1 { display",
            "display }",
            "display a}",
            "{ display }",
            braces.as_str(),
        ] {
            assert_eq!(
                split_command_string(input),
                Err(syntax_error()),
                "{input:?}"
            );
        }
    }

    /// A list's words survive being run: splitting its argument again gives
    /// the parsed words back, backslashes and quotes included (tmux 3.7b:
    /// `if-shell -F 1 "if-shell -F 1 { display -p 'a\"b\c' }"` prints
    /// `a"b\c`), through nested lists too.
    #[test]
    fn a_list_splits_back_to_its_parsed_words() {
        let outer = split_command_string(r#"if-shell -F 1 { display -p 'a"b\c' }"#).unwrap();
        let inner = split_command_string(&outer[0][3]).unwrap();
        assert_eq!(inner, vec![argv(&["display", "-p", r#"a"b\c"#])]);
        let nested =
            split_command_string(r#"if -F 1 { if -F 1 { display -p "it's $ ~ \\t" } }"#).unwrap();
        let once = split_command_string(&nested[0][3]).unwrap();
        let twice = split_command_string(&once[0][3]).unwrap();
        assert_eq!(twice, vec![argv(&["display", "-p", r"it's $ ~ \t"])]);
    }

    /// tmux 3.7b's parser gives up on lists nested 2000 deep (recorded:
    /// 1999 parse); grofe stops at the same depth, with no recursion.
    #[test]
    fn lists_nested_2000_deep_are_tmux_s_stack_overflow() {
        let nest = |n: usize| {
            format!(
                "if -F 0 {}{{ display x }}{}",
                "{ if -F 0 ".repeat(n),
                " }".repeat(n)
            )
        };
        assert!(split_command_string(&nest(1998)).is_ok());
        assert_eq!(
            split_command_string(&nest(1999)),
            Err(ParseError::Other("yacc stack overflow".to_string()))
        );
    }

    /// `args_escape` as tmux 3.7b prints arguments (recorded in
    /// `list-keys` of tmux's stock table and of bound strings).
    #[test]
    fn escape_arg_is_tmux_s() {
        for (arg, want) in [
            ("", "''"),
            ("kill-pane", "kill-pane"),
            ("#S", "\"#S\""),
            ("%%", "\"%%\""),
            ("a b", "\"a b\""),
            ("a\"b", "'a\"b'"),
            ("say \"hi\" #x", "\"say \\\"hi\\\" #x\""),
            ("$", "\\$"),
            ("\"", "\\\""),
            (";", "\\;"),
            ("~", "\\~"),
            ("~/x", "\\~/x"),
            ("$HOME x", "\"\\$HOME x\""),
            ("$1 x", "\"$1 x\""),
            ("a\\b", "a\\\\b"),
            ("a\tb", "a\\tb"),
            (":=0", ":=0"),
        ] {
            assert_eq!(escape_arg(arg), want, "{arg:?}");
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

    #[test]
    fn bind_key_flags_and_command() {
        assert_eq!(
            parse(&argv(&["bind-key", "X", "split-window", "-h"])),
            Ok(Command::BindKey(BindKey {
                table: "prefix".to_string(),
                repeat: false,
                note: None,
                group: None,
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
                group: None,
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
    fn choose_agent_no_args() {
        assert_eq!(parse(&argv(&["choose-agent"])), Ok(Command::ChooseAgent));
        assert!(parse(&argv(&["choose-agent", "x"])).is_err());
    }

    #[test]
    fn choose_notification_no_args() {
        assert_eq!(
            parse(&argv(&["choose-notification"])),
            Ok(Command::ChooseNotification)
        );
        assert!(parse(&argv(&["choose-notification", "x"])).is_err());
    }

    #[test]
    fn notify_subcommands() {
        use std::time::Duration;
        let n = |w: &[&str]| parse(&argv(w));
        assert_eq!(n(&["notify", "list"]), Ok(Command::Notify(NotifyCmd::List)));
        assert_eq!(
            n(&["notify", "clear"]),
            Ok(Command::Notify(NotifyCmd::Clear))
        );
        assert_eq!(
            n(&["notify", "unmute"]),
            Ok(Command::Notify(NotifyCmd::Unmute))
        );
        assert_eq!(
            n(&["notify", "dismiss", "7"]),
            Ok(Command::Notify(NotifyCmd::Dismiss(7)))
        );
        assert_eq!(
            n(&["notify", "mute"]),
            Ok(Command::Notify(NotifyCmd::Mute(None)))
        );
        for (d, secs) in [("10m", 600), ("30s", 30), ("2h", 7200)] {
            assert_eq!(
                n(&["notify", "mute", d]),
                Ok(Command::Notify(NotifyCmd::Mute(Some(Duration::from_secs(
                    secs
                )))))
            );
        }
        assert!(n(&["notify", "dismiss", "x"]).is_err());
        assert!(n(&["notify", "dismiss"]).is_err());
        assert!(n(&["notify", "list", "x"]).is_err());
    }

    #[test]
    fn notify_unknown_sub() {
        assert!(parse(&argv(&["notify"])).is_err());
        assert_eq!(
            parse(&argv(&["notify", "frob"])),
            Err(ParseError::Other("notify: unknown subcommand frob".into()))
        );
    }

    #[test]
    fn mute_bad_duration() {
        for d in ["10", "m", "0m", "5d", "-3m", "1.5h", "10m5"] {
            assert!(parse(&argv(&["notify", "mute", d])).is_err(), "{d}");
        }
        assert!(parse(&argv(&["notify", "mute", "1m", "2m"])).is_err());
    }

    #[test]
    fn key_menu_flags() {
        assert_eq!(
            parse(&argv(&["key-menu"])),
            Ok(Command::KeyMenu(KeyMenu { table: None }))
        );
        assert_eq!(
            parse(&argv(&["key-menu", "-T", "prefix"])),
            Ok(Command::KeyMenu(KeyMenu {
                table: Some("prefix".to_string())
            }))
        );
    }

    #[test]
    fn key_menu_unknown_flag() {
        assert_eq!(
            parse(&argv(&["key-menu", "-x"])),
            Err(ParseError::UnknownFlag("key-menu".to_string(), 'x'))
        );
    }

    #[test]
    fn bind_g_requires_value() {
        assert!(parse(&argv(&["bind-key", "-G"])).is_err());
        let Ok(Command::BindKey(c)) =
            parse(&argv(&["bind-key", "-G", "agents", "u", "agent", "next"]))
        else {
            panic!("bind-key -G");
        };
        assert_eq!(c.group.as_deref(), Some("agents"));
        assert_eq!(c.key, "u");
        assert_eq!(c.command, argv(&["agent", "next"]));
    }

    #[test]
    fn unbind_key_needs_a_key_unless_all() {
        assert_eq!(
            parse(&argv(&["unbind", "-n", "M-h"])),
            Ok(Command::UnbindKey(UnbindKey {
                table: "root".to_string(),
                all: false,
                key: Some("M-h".to_string()),
                quiet: false,
            }))
        );
        assert_eq!(
            parse(&argv(&["unbind-key", "-a", "-T", "copy"])),
            Ok(Command::UnbindKey(UnbindKey {
                table: "copy".to_string(),
                all: true,
                key: None,
                quiet: false,
            }))
        );
        assert!(parse(&argv(&["unbind-key"])).is_err());
        assert!(parse(&argv(&["unbind-key", "-a", "x"])).is_err());
        // `-q` makes both a command that does nothing.
        for words in [&["unbind-key", "-q"][..], &["unbind-key", "-aq", "x"]] {
            let Ok(Command::UnbindKey(c)) = parse(&argv(words)) else {
                panic!("{words:?}");
            };
            assert!(c.quiet && !c.all && c.key.is_none(), "{words:?}");
        }
    }

    #[test]
    fn list_keys_source_file_and_reload() {
        assert_eq!(
            parse(&argv(&["lsk", "-T", "prefix"])),
            Ok(Command::ListKeys(ListKeys {
                table: Some("prefix".to_string()),
                first: false,
                notes: false,
                key: None,
                all: false,
                format: None,
                prefix: None,
                sort: SortFlags::default(),
            }))
        );
        assert_eq!(
            parse(&argv(&["source", "-q", "sub.toml"])),
            Ok(Command::SourceFile(SourceFile {
                quiet: true,
                format: false,
                parse_only: false,
                verbose: false,
                target: None,
                paths: vec!["sub.toml".to_string()],
            }))
        );
        assert_eq!(
            parse(&argv(&["source-file", "-nv", "a.conf", "b*.conf"])),
            Ok(Command::SourceFile(SourceFile {
                quiet: false,
                format: false,
                parse_only: true,
                verbose: true,
                target: None,
                paths: vec!["a.conf".to_string(), "b*.conf".to_string()],
            }))
        );
        assert!(parse(&argv(&["source-file"])).is_err());
        assert_eq!(parse(&argv(&["reload"])), Ok(Command::Reload(Reload)));
        assert!(parse(&argv(&["reload", "x"])).is_err());
    }

    #[test]
    fn environment_flags() {
        let Ok(Command::SetEnvironment(c)) = parse(&argv(&["set-environment", "-grF", "X"])) else {
            panic!();
        };
        assert!(c.global && c.clear && c.format && !c.unset && !c.hidden);
        let Ok(Command::ShowEnvironment(c)) = parse(&argv(&["show-environment", "-gs", "X"]))
        else {
            panic!();
        };
        assert!(c.global && c.shell && !c.hidden);
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
                background: false,
                key: false,
                single: false,
                prompt_type: None,
                numeric: false,
                incremental: false,
                expand: false,
                bspace_exit: false,
                literal: false,
                client: None,
            }))
        );
        assert_eq!(
            parse(&argv(&["command-prompt"])),
            Ok(Command::CommandPrompt(CommandPrompt {
                prompt: None,
                initial: None,
                template: None,
                background: false,
                key: false,
                single: false,
                prompt_type: None,
                numeric: false,
                incremental: false,
                expand: false,
                bspace_exit: false,
                literal: false,
                client: None,
            }))
        );
        assert_eq!(
            parse(&argv(&["run", "echo hi; echo two"])),
            Ok(Command::RunShell(RunShell {
                cmd: "echo hi; echo two".to_string(),
                background: false,
                delay: None,
                target: Target::Current,
                command: false,
                stderr: false,
                cwd: None,
                args: Vec::new(),
            }))
        );
        // tmux takes none: a `run-shell` with nothing to run runs nothing.
        assert!(parse(&argv(&["run-shell"])).is_ok());
    }

    #[test]
    fn select_layout_takes_a_name_and_a_target() {
        assert_eq!(
            parse(&argv(&["selectl", "-t", "%1", "main-vertical"])),
            Ok(Command::SelectLayout(SelectLayout {
                target: parse_target("%1").expect("target"),
                name: "main-vertical".to_string(),
                spread: false,
                next: false,
                previous: false,
                undo: false,
            }))
        );
        // The name is optional: no name is the last preset again.
        let Ok(Command::SelectLayout(c)) = parse(&argv(&["select-layout", "-o"])) else {
            panic!("select-layout -o");
        };
        assert!(c.undo && c.name.is_empty());
        let Ok(Command::SelectLayout(c)) = parse(&argv(&["select-layout"])) else {
            panic!("select-layout");
        };
        assert!(c.name.is_empty() && !c.undo);
    }

    fn lines(text: &str) -> Vec<(usize, Vec<Vec<&'static str>>)> {
        split_config_file(text)
            .unwrap_or_else(|e| panic!("{text:?}: {e:?}"))
            .into_iter()
            .map(|(line, cmds)| {
                let cmds = cmds
                    .into_iter()
                    .map(|c| c.into_iter().map(|w| &*w.leak()).collect())
                    .collect();
                (line, cmds)
            })
            .collect()
    }

    /// tmux 3.7b's `source-file`, recorded: a line per group, comments,
    /// continuations, quotes across lines and `{ }` lists across lines.
    #[test]
    fn a_config_file_splits_as_tmux_reads_it() {
        assert_eq!(
            lines("set -g @a 1 ; set -g @b 2\n\n  # note\nset -g @c x # c\n"),
            [
                (
                    1,
                    vec![vec!["set", "-g", "@a", "1"], vec!["set", "-g", "@b", "2"]]
                ),
                (4, vec![vec!["set", "-g", "@c", "x"]]),
            ]
        );
        // `#` starts a comment only at a word's start, `#{` included.
        assert_eq!(
            lines("set -g @e a#b\ndisplay -p #{session_name}\n"),
            [
                (1, vec![vec!["set", "-g", "@e", "a#b"]]),
                (2, vec![vec!["display", "-p"]]),
            ]
        );
        // A backslash-newline joins, in quotes too; an even run does not.
        assert_eq!(
            lines("set -g @g \"a \\\n b\"\nset -g @h a\\\\\nset -g \\\n @i 2\n"),
            [
                (1, vec![vec!["set", "-g", "@g", "a  b"]]),
                (3, vec![vec!["set", "-g", "@h", "a\\"]]),
                (4, vec![vec!["set", "-g", "@i", "2"]]),
            ]
        );
        // Quotes keep a newline.
        assert_eq!(
            lines("set -g @a \"x\ny\"\nset -g @c 'x\ny'\n"),
            [
                (1, vec![vec!["set", "-g", "@a", "x\ny"]]),
                (3, vec![vec!["set", "-g", "@c", "x\ny"]]),
            ]
        );
        // A list's lines are its commands; the list is one argument.
        assert_eq!(
            lines("bind x {\n  display a # one\n  display b\n}\nset -g @k 1\n"),
            [
                (1, vec![vec!["bind", "x", "'display' 'a' ; 'display' 'b'"]]),
                (5, vec![vec!["set", "-g", "@k", "1"]]),
            ]
        );
        // which-key's macro: a quoted value continued over lines, glued to
        // the word before it.
        assert_eq!(
            lines("set -gF command-alias[200] show-wk-menu=\\\n'#{@wk_cmd_show}'\n"),
            [(
                1,
                vec![vec![
                    "set",
                    "-gF",
                    "command-alias[200]",
                    "show-wk-menu=#{@wk_cmd_show}"
                ]]
            )]
        );
    }

    #[test]
    fn a_config_file_error_names_its_line() {
        let err = split_config_file("set -g @a 1\n\ndisplay }\n").unwrap_err();
        assert_eq!(err.0, 3);
        // An unclosed `{` is reported at the end of the file (tmux 3.7b).
        let err = split_config_file("set -g @u 1\nbind x {\n  display a\n\n").unwrap_err();
        assert_eq!(err.0, 5);
    }

    /// A host that expands nothing and records what it is asked.
    #[derive(Default)]
    struct Recorder {
        conditions: Vec<String>,
        assigned: Vec<(String, bool)>,
    }

    impl ConfigHost for Recorder {
        fn condition(&mut self, format: &str) -> bool {
            self.conditions.push(format.to_string());
            !format.is_empty() && format != "0"
        }
        fn assign(&mut self, assignment: &str, hidden: bool) {
            self.assigned.push((assignment.to_string(), hidden));
        }
    }

    fn read(text: &str) -> Result<Vec<ConfigLine>, (usize, ParseError)> {
        split_config_file_with(text, &mut Recorder::default())
    }

    fn lines_of(text: &str) -> Vec<usize> {
        read(text).unwrap().into_iter().map(|(l, _)| l).collect()
    }

    /// tmux 3.7b's `%if` family (recorded, `golden/fe_3/config-if`): the
    /// first true branch counts, a false branch is still read, scopes nest.
    #[test]
    fn conditions_choose_lines() {
        assert_eq!(lines_of("%if 1\na\n%else\nb\n%endif\n"), [2]);
        assert_eq!(lines_of("%if 0\na\n%else\nb\n%endif\n"), [4]);
        assert_eq!(
            lines_of("%if 0\na\n%elif 1\nb\n%elif 1\nc\n%else\nd\n%endif\ne\n"),
            [4, 10]
        );
        assert_eq!(lines_of("%if 0\n%if 1\na\n%endif\n%else\nb\n%endif\n"), [6]);
        // `0` and the empty string are false; anything else is true.
        assert_eq!(
            lines_of("%if ''\na\n%endif\n%if 0\nb\n%endif\n%if x\nc\n%endif\n"),
            [8]
        );
    }

    /// tmux 3.7b reads `%if` inside a `{ }` list too (checked on a private
    /// server: the false branch's line is not in the binding), and a `%if`
    /// opened inside a list must close inside it, at the `}` line.
    #[test]
    fn directives_work_inside_a_list() {
        let text = "bind x if-shell true {\none\n%if 0\nno\n%else\nyes\n%endif\ntwo\n}\n";
        let got = read(text).unwrap();
        assert_eq!(got.len(), 1);
        let all = got[0].1[0].join(" ");
        assert!(all.ends_with("'one' ; 'yes' ; 'two'"), "{all}");
        let syntax = |line| Err((line, ParseError::Other("syntax error".into())));
        assert_eq!(read("bind x if-shell true {\n%if 1\nz\n}\n"), syntax(4));
    }

    #[test]
    fn a_condition_is_one_word_and_a_raw_format_keeps_its_spaces() {
        let mut host = Recorder::default();
        let got = split_config_file_with(
            "%if #{==:a b,#{x}}\n%endif\n%if \"#{y}\" # c\n%endif\n%if #{a} # c\n%endif\n",
            &mut host,
        );
        assert_eq!(got, Ok(vec![]));
        assert_eq!(host.conditions, ["#{==:a b,#{x}}", "#{y}", "#{a}"]);
    }

    #[test]
    fn directive_errors_name_their_line() {
        let syntax = |line| Err((line, ParseError::Other("syntax error".into())));
        assert_eq!(read("a\n%else\n"), syntax(2));
        assert_eq!(read("a\n%endif\n"), syntax(2));
        assert_eq!(read("a\n%elif 1\n"), syntax(2));
        assert_eq!(read("a\n%if 1\nb\n"), syntax(4));
        assert_eq!(read("%if\n%endif\n"), syntax(1));
        assert_eq!(read("%if 1 2\n%endif\n"), syntax(1));
        assert_eq!(read("%if 1\n%else\n%elif 1\n%endif\n"), syntax(3));
        assert_eq!(read("%if 1\n%endif x\n"), syntax(2));
        assert_eq!(read("%frob\n"), syntax(1));
        assert_eq!(read("%hidden display x\n"), syntax(1));
        // A false branch is read all the same.
        assert_eq!(read("%if 0\n'a\n%endif\n%if 0\n"), syntax(5));
        // `%1` is a word, not a directive.
        assert_eq!(lines_of("select-window -t %1\n"), [1]);
    }

    /// `NAME=value` alone or before a command goes to the host, in order,
    /// outside false branches; `%hidden` is hidden.
    #[test]
    fn assignments_go_to_the_host() {
        let mut host = Recorder::default();
        let got = split_config_file_with(
            "A=1\nB=2 display x\n%hidden C=3\n%if 0\nD=4\n%hidden E=5\n%endif\ndisplay A=1\n",
            &mut host,
        )
        .unwrap();
        assert_eq!(
            got,
            [
                (2, vec![argv(&["display", "x"])]),
                (8, vec![argv(&["display", "A=1"])])
            ]
        );
        assert_eq!(
            host.assigned,
            [
                ("A=1".to_string(), false),
                ("B=2".to_string(), false),
                ("C=3".to_string(), true)
            ]
        );
        // A command name cannot be an assignment.
        assert!(read("A=1 B=2\n").is_err());
    }
}

#[cfg(test)]
mod choose_tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn choosers_take_tmux_s_flags_and_a_template() {
        let Ok(Command::ChooseTree(c)) = parse(&argv(&[
            "choose-tree",
            "-NN",
            "-O",
            "name",
            "-r",
            "-f",
            "#{==:a,b}",
            "-F",
            "#{x}",
            "-K",
            "#{line}",
            "-t",
            "%1",
            "-y",
            "-G",
            "-w",
            "join-pane -t '%%'",
        ])) else {
            panic!("choose-tree");
        };
        assert_eq!(c.opts.no_preview, 2);
        assert_eq!(c.opts.sort.as_deref(), Some("name"));
        assert!(c.opts.reversed && c.opts.prompt_yes && c.opts.groups && c.windows);
        assert_eq!(c.opts.filter.as_deref(), Some("#{==:a,b}"));
        assert_eq!(c.opts.format.as_deref(), Some("#{x}"));
        assert_eq!(c.opts.key_format.as_deref(), Some("#{line}"));
        assert_eq!(c.opts.template.as_deref(), Some("join-pane -t '%%'"));
        let Ok(Command::ChooseTree(c)) = parse(&argv(&["choose-tree", "-N"])) else {
            panic!("choose-tree -N");
        };
        assert_eq!(c.opts.no_preview, 1);
        let Ok(Command::ChooseBuffer(c)) = parse(&argv(&["choose-buffer", "-N", "-Z"])) else {
            panic!("choose-buffer");
        };
        assert!(c.zoom);
        assert_eq!(c.opts.no_preview, 1);
    }
}

#[cfg(test)]
mod unfloat_pane_tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn unfloat(words: &[&str]) -> UnfloatPane {
        match parse(&argv(words)) {
            Ok(Command::UnfloatPane(c)) => c,
            other => panic!("{words:?}: {other:?}"),
        }
    }

    #[test]
    fn unfloat_pane_defaults_to_the_current_pane_and_no_direction() {
        assert_eq!(
            unfloat(&["unfloat-pane"]),
            UnfloatPane {
                target: Target::Current,
                split: None,
                dir: None,
            }
        );
    }

    #[test]
    fn unfloat_pane_reads_its_targets_and_direction() {
        let c = unfloat(&["unfloat-pane", "-t", "%3", "-s", "%1", "-h"]);
        assert_eq!(c.target, parse_target("%3").unwrap());
        assert_eq!(c.split, Some(parse_target("%1").unwrap()));
        assert_eq!(c.dir, Some(Dir::Horizontal));
        assert_eq!(unfloat(&["unfloat-pane", "-v"]).dir, Some(Dir::Vertical));
        // As in `join-pane`, `-h` wins over `-v`.
        assert_eq!(
            unfloat(&["unfloat-pane", "-v", "-h"]).dir,
            Some(Dir::Horizontal)
        );
    }

    #[test]
    fn unfloat_pane_rejects_unknown_flags_and_positionals() {
        assert_eq!(
            parse(&argv(&["unfloat-pane", "-x", "3"]))
                .unwrap_err()
                .to_string(),
            "command unfloat-pane: unknown flag -x"
        );
        assert!(parse(&argv(&["unfloat-pane", "extra"])).is_err());
    }
}

#[cfg(test)]
mod scratchpad_tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn new_scratchpad_defaults_to_the_current_pane_and_shell() {
        let Ok(Command::NewScratchpad(c)) = parse(&argv(&["new-scratchpad"])) else {
            panic!("new-scratchpad");
        };
        assert_eq!(c.target, Target::Current);
        assert_eq!(c.cwd, None);
        assert_eq!(c.command, None);
    }

    #[test]
    fn new_scratchpad_reads_target_cwd_and_command() {
        let Ok(Command::NewScratchpad(c)) = parse(&argv(&["new-scratchpad", "-c", "/tmp", "htop"]))
        else {
            panic!("new-scratchpad -c");
        };
        assert_eq!(c.target, Target::Current);
        assert_eq!(c.cwd, Some("/tmp".to_string()));
        assert_eq!(c.command, Some(vec!["htop".to_string()]));
        let Ok(Command::NewScratchpad(c)) = parse(&argv(&["new-scratchpad", "-t", "%2"])) else {
            panic!("new-scratchpad -t");
        };
        assert_eq!(c.target, parse_target("%2").unwrap());
    }

    #[test]
    fn toggle_scratchpad_defaults_and_reads_target() {
        let Ok(Command::ToggleScratchpad(c)) = parse(&argv(&["toggle-scratchpad"])) else {
            panic!("toggle-scratchpad");
        };
        assert_eq!(c.target, Target::Current);
        let Ok(Command::ToggleScratchpad(c)) = parse(&argv(&["toggle-scratchpad", "-t", "%3"]))
        else {
            panic!("toggle-scratchpad -t");
        };
        assert_eq!(c.target, parse_target("%3").unwrap());
    }

    #[test]
    fn toggle_scratchpad_unknown_flag() {
        assert_eq!(
            parse(&argv(&["toggle-scratchpad", "-x"]))
                .unwrap_err()
                .to_string(),
            "command toggle-scratchpad: unknown flag -x"
        );
    }

    #[test]
    fn show_scratchpad_reads_its_pane_id() {
        let Ok(Command::ShowScratchpad(c)) = parse(&argv(&["show-scratchpad", "%4"])) else {
            panic!("show-scratchpad %4");
        };
        assert_eq!(c.target, Target::Current);
        assert_eq!(c.pane, PaneId(4));
        let Ok(Command::ShowScratchpad(c)) = parse(&argv(&["show-scratchpad", "-t", "%2", "%4"]))
        else {
            panic!("show-scratchpad -t");
        };
        assert_eq!(c.target, parse_target("%2").unwrap());
        assert_eq!(c.pane, PaneId(4));
    }

    #[test]
    fn show_scratchpad_needs_a_pane_id() {
        assert!(parse(&argv(&["show-scratchpad"])).is_err());
        assert!(parse(&argv(&["show-scratchpad", "4"])).is_err());
        assert!(parse(&argv(&["show-scratchpad", "%4", "%5"])).is_err());
    }

    #[test]
    fn list_scratchpads_takes_no_args() {
        assert!(matches!(
            parse(&argv(&["list-scratchpads"])),
            Ok(Command::ListScratchpads)
        ));
        assert!(parse(&argv(&["list-scratchpads", "x"])).is_err());
    }

    #[test]
    fn choose_scratchpad_takes_no_args() {
        assert!(matches!(
            parse(&argv(&["choose-scratchpad"])),
            Ok(Command::ChooseScratchpad)
        ));
        assert!(parse(&argv(&["choose-scratchpad", "x"])).is_err());
    }
}

#[cfg(test)]
mod lua_tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn test_parse_lua() {
        let Ok(Command::Lua { src }) = parse(&argv(&["lua", "return 1"])) else {
            panic!("lua");
        };
        assert_eq!(src, "return 1");
        let Ok(Command::Lua { src }) = parse(&argv(&["lua", "return", "1"])) else {
            panic!("lua words");
        };
        assert_eq!(src, "return 1");
        assert!(parse(&argv(&["lua"])).is_err());
    }

    #[test]
    fn test_lua_call_needs_number() {
        let Ok(Command::LuaCall { id }) = parse(&argv(&["lua-call", "7"])) else {
            panic!("lua-call");
        };
        assert_eq!(id, 7);
        assert!(parse(&argv(&["lua-call", "x"])).is_err());
        assert!(parse(&argv(&["lua-call"])).is_err());
        assert!(parse(&argv(&["lua-call", "1", "2"])).is_err());
    }

    #[test]
    fn test_lua_unknown_in_tmux_dialect() {
        for verb in ["lua", "lua-call"] {
            let err = crate::compat::tmux::resolve(verb).err().map(|e| e.0);
            assert_eq!(err, Some(format!("unknown command: {verb}")));
        }
        assert!(crate::compat::tmux::resolve("drag-pane").is_err());
    }
}
