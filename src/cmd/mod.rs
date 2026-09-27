//! `Command`, per-command argument shapes, and `ParseError` for the M1
//! command set (spec §4.5). [`parse::parse`] turns argv into a `Command`;
//! `target`, `format` and `keys` hold the target grammar, `#{...}`
//! expansion and the key-name table that a command's flags may reference.
//!
//! Every CLI call, key binding and control-mode request goes through
//! [`parse::parse`]: the CLI, key bindings, control mode and (from M3)
//! plugins all produce argv shaped like a shell command line.

pub mod format;
pub mod keys;
pub mod parse;
pub mod target;

pub use format::{FormatVars, expand};
pub use keys::{is_key_name, key_bytes};
pub use parse::parse;
pub use target::{PanePart, PaneRef, Resolve, SessionRef, Target, WindowRef};

use crate::types::{CaptureOpts, Dir, Edge, SplitSize};

/// An error parsing a command's argv, worded like tmux's own CLI errors
/// (spec §4.5: bad flags give tmux-like errors).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("no command given")]
    EmptyCommand,
    #[error("unknown command: {0}")]
    UnknownCommand(String),
    #[error("unknown flag -{0}")]
    UnknownFlag(char),
    #[error("unknown flag --{0}")]
    UnknownLongFlag(String),
    #[error("-{0}: option requires an argument")]
    MissingValue(char),
    #[error("command {0}: too many arguments")]
    TooManyArguments(String),
    #[error("command {0}: missing argument")]
    MissingArgument(String),
    /// A target string that isn't in the supported grammar (spec §4.5):
    /// fnmatch patterns, `{last}`/`{next}`, `!`, `+`/`-` offsets, extra
    /// `:`/`.` separators, or a pane named rather than indexed.
    #[error("unsupported target form: {0}")]
    UnsupportedTarget(String),
    /// A target string that matches a supported shape (`$N`, `@N`, `%N`,
    /// or a `session[:window[.pane]]` position) but whose id/number is
    /// malformed.
    #[error("bad target: {0}")]
    BadTarget(String),
    #[error("{0}")]
    Other(String),
}

/// `new-session` (`-d -s -c -x -y`, plus a trailing shell command).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewSession {
    pub detached: bool,
    pub name: Option<String>,
    /// `-c`; raw string, expanded against a format at dispatch time.
    pub cwd: Option<String>,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub command: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillSession {
    pub target: Target,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HasSession {
    pub target: Target,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListSessions {
    pub format: Option<String>,
}

/// `new-window` (`-d -t -n -c`, plus a trailing shell command).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWindow {
    pub detached: bool,
    pub target: Target,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub command: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillWindow {
    pub target: Target,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectWindow {
    pub target: Target,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListWindows {
    pub target: Target,
    pub format: Option<String>,
}

/// `split-window` (`-h -v -c -p -l -t`, plus a trailing shell command).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitWindow {
    pub dir: Dir,
    pub cwd: Option<String>,
    pub size: SplitSize,
    pub target: Target,
    pub command: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillPane {
    pub target: Target,
}

/// `select-pane` (`-L -R -U -D -t`). `edge` is `None` when no direction
/// flag was given: tmux's `select-pane -t <target>` with no direction
/// just makes `<target>` the active pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectPane {
    pub edge: Option<Edge>,
    pub target: Target,
}

/// What `resize-pane` does: move a border by a cell count, or toggle zoom
/// (`-Z`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResizeAction {
    Edge(Edge, u16),
    ToggleZoom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizePane {
    pub action: ResizeAction,
    pub target: Target,
}

/// `respawn-pane` (`-k`, plus a trailing shell command that replaces the
/// pane's resume argv; omitted means resume the original command).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RespawnPane {
    pub kill: bool,
    pub target: Target,
    pub command: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPanes {
    pub target: Target,
    pub format: Option<String>,
}

/// A single `send-keys` argument, already classified (spec §4.5): a key
/// name to encode via [`keys::key_bytes`] at dispatch time, literal text,
/// or (`-H`) one hex-encoded byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendKeysItem {
    Key(String),
    Literal(String),
    Hex(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendKeys {
    pub target: Target,
    pub items: Vec<SendKeysItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturePane {
    pub target: Target,
    pub print: bool,
    pub opts: CaptureOpts,
}

/// `set-option`/`show-options` scope flags (`-g -s -w -p`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionScope {
    Global,
    Session,
    Window,
    Pane,
}

/// `set-option`/`set` (`-g -s -w -p -u --persist`). `scope` is `None` when
/// no scope flag was given: the option's own schema (dispatch, T1.5c)
/// decides the default scope for that option name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetOption {
    pub scope: Option<OptionScope>,
    pub unset: bool,
    pub persist: bool,
    pub target: Target,
    pub name: String,
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowOptions {
    pub scope: Option<OptionScope>,
    pub target: Target,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayMessage {
    pub print: bool,
    pub target: Target,
    /// The message/format text, raw (expanded at dispatch time). `None`
    /// means tmux's default status-line format.
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KillServer;

/// `subscribe` (control mode only): event names to subscribe to; empty
/// means all events.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Subscribe {
    pub events: Vec<String>,
}

/// One parsed command (spec §4.5, M1 set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    NewSession(NewSession),
    KillSession(KillSession),
    HasSession(HasSession),
    ListSessions(ListSessions),
    NewWindow(NewWindow),
    KillWindow(KillWindow),
    SelectWindow(SelectWindow),
    ListWindows(ListWindows),
    SplitWindow(SplitWindow),
    KillPane(KillPane),
    SelectPane(SelectPane),
    ResizePane(ResizePane),
    RespawnPane(RespawnPane),
    ListPanes(ListPanes),
    SendKeys(SendKeys),
    CapturePane(CapturePane),
    SetOption(SetOption),
    ShowOptions(ShowOptions),
    DisplayMessage(DisplayMessage),
    KillServer(KillServer),
    Subscribe(Subscribe),
}
