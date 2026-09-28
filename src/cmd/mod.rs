//! `Command`, per-command argument shapes, and `ParseError` for the M1,
//! M2 and M3 command sets (spec §4.5). [`parse::parse`] turns argv into a `Command`;
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
pub use parse::{parse, parse_sequence, split_command_string};
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
    /// A short flag (`-z`) the command doesn't recognise. Worded like
    /// tmux's own CLI: `command kill-server: unknown flag -z`.
    #[error("command {0}: unknown flag -{1}")]
    UnknownFlag(String, char),
    /// A `--long` flag tmux's single-dash-at-a-time flag scanner doesn't
    /// recognise. tmux never shows the offending text: `--anything` fails
    /// on the flag scanner's second character (itself a `-`), so the
    /// message is always `command set-option: invalid flag --`.
    #[error("command {0}: invalid flag --")]
    UnknownLongFlag(String),
    /// A value-taking flag given with no value. Worded like tmux's own
    /// CLI: `command kill-session: -t expects an argument`.
    #[error("command {0}: -{1} expects an argument")]
    MissingValue(String, char),
    /// More positionals than the command accepts. Worded like tmux's own
    /// CLI: `command select-pane: too many arguments (need at most 0)`.
    #[error("command {0}: too many arguments (need at most {1})")]
    TooManyArguments(String, usize),
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

/// What `resize-pane` does: move one or more borders by a shared cell
/// count, or toggle zoom (`-Z`). Per tmux, `-L`/`-R`/`-U`/`-D` are boolean
/// flags (`-LR` sets both) and the amount is a single trailing positional
/// shared by whichever directions were given, not a per-flag value:
/// `resize-pane -L 3 -R 5` has two positionals for a command that takes
/// at most one, so it's tmux's own too-many-arguments error, not two
/// separate amounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResizeAction {
    Edges(Vec<Edge>, u16),
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

/// `attach-session`/`attach` (`-t -d --nest`). `nest` is grofe's force for
/// self-nesting (spec §4.6); tmux's `-f` (client flags) is reserved for the
/// tmux front end (M3.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachSession {
    pub target: Target,
    /// `-d`: detach other clients attached to the session.
    pub detach_others: bool,
    pub nest: bool,
}

/// `detach-client` (`-s -t -a`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachClient {
    /// `-s`: detach every client attached to this session.
    pub session: Option<Target>,
    /// `-t`: the client, raw (T2.5a defines client names); `None` is the
    /// requesting client.
    pub client: Option<String>,
    /// `-a`: detach every client except the target one.
    pub all_others: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListClients {
    pub format: Option<String>,
}

/// `next-window`, `previous-window`, `last-window` (`-t` session).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowStep {
    pub target: Target,
}

/// `rename-session`/`rename-window` (`-t`, new name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rename {
    pub target: Target,
    pub name: String,
}

/// `switch-client` (`-t -l -n -p -T`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchClient {
    /// `-t`; `None` when not given.
    pub target: Option<Target>,
    /// `-l`: the client's last session.
    pub last: bool,
    /// `-n`: the next session.
    pub next: bool,
    /// `-p`: the previous session.
    pub previous: bool,
    /// `-T`: the key table to switch to.
    pub key_table: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendPrefix {
    pub target: Target,
}

/// `bind-key` (`-n -r -T -N`, then a key and its command).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindKey {
    /// `-T`, else `root` for `-n`, else `prefix` (tmux's order).
    pub table: String,
    /// `-r`.
    pub repeat: bool,
    /// `-N`: the binding's description.
    pub note: Option<String>,
    /// The key name, raw (parsed by the handler, which words the error).
    pub key: String,
    /// Everything after the key. One element is a command string (split
    /// with [`split_command_string`]); several are one command's argv.
    pub command: Vec<String>,
}

/// `unbind-key` (`-n -T -a`, then a key unless `-a`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnbindKey {
    pub table: String,
    /// `-a`: every binding in the table.
    pub all: bool,
    pub key: Option<String>,
}

/// `list-keys` (`-T`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListKeys {
    pub table: Option<String>,
}

/// `source-file` (`-q`, a path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub quiet: bool,
    pub path: String,
}

/// `reload` (grofe's own): re-read the config chain (spec §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reload;

/// `command-prompt` (`-p -I`, then a template; R24).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandPrompt {
    /// `-p`: the prompt text.
    pub prompt: Option<String>,
    /// `-I`: the text the prompt starts with.
    pub initial: Option<String>,
    /// The command the answer is put into at `%%`.
    pub template: Option<String>,
}

/// `run-shell` (a shell command).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunShell {
    pub cmd: String,
}

/// `select-layout` (`-t`, a layout name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectLayout {
    pub target: Target,
    pub name: String,
}

/// One parsed command (spec §4.5, M1 and M2 sets).
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
    // M2 (T2.5a, T2.5b implement them).
    AttachSession(AttachSession),
    DetachClient(DetachClient),
    ListClients(ListClients),
    NextWindow(WindowStep),
    PreviousWindow(WindowStep),
    LastWindow(WindowStep),
    RenameSession(Rename),
    RenameWindow(Rename),
    SwitchClient(SwitchClient),
    SendPrefix(SendPrefix),
    // M3 (T3.0 parses them; T3.4, T3.5 and T3.7 implement them).
    BindKey(BindKey),
    UnbindKey(UnbindKey),
    ListKeys(ListKeys),
    SourceFile(SourceFile),
    Reload(Reload),
    CommandPrompt(CommandPrompt),
    RunShell(RunShell),
    SelectLayout(SelectLayout),
}
