//! `Command`, per-command argument shapes, and `ParseError` for the M1,
//! M2, M3 and M3.6 command sets (spec §4.5). [`parse::parse`] turns argv into a `Command`;
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
    /// Fewer positionals than the command needs, worded like tmux's own
    /// CLI: `command wait-for: too few arguments (need at least 1)`. A
    /// bare M3.6 verb still parses (see [`WaitFor`]); its handler refuses
    /// it with this variant.
    #[error("command {0}: too few arguments (need at least {1})")]
    TooFewArguments(String, usize),
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
    /// `-A` (M3.6, TC.9): attach to the session named by `-s` if it
    /// exists, instead of creating it.
    pub attach: bool,
    /// `-P` (TC.9): print the new session (`-F`, else tmux's
    /// `#{session_name}:`).
    pub print: bool,
    /// `-F` (TC.9).
    pub format: Option<String>,
    /// `-n` (TC.9): the first window's name.
    pub window_name: Option<String>,
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
    /// `-f` (M3.6): a format; only sessions it expands true for are listed.
    pub filter: Option<String>,
}

/// `new-window` (`-d -t -n -c`, plus a trailing shell command; from M3.6
/// `-a -P -F`, TC.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWindow {
    pub detached: bool,
    pub target: Target,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub command: Option<Vec<String>>,
    /// `-a`: at the index after the target window.
    pub after: bool,
    /// `-P`: print the new window (`-F`, else tmux's default format).
    pub print: bool,
    /// `-F`.
    pub format: Option<String>,
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
    /// `-a` (M3.6): every window of every session.
    pub all: bool,
    /// `-f` (M3.6): a format; only windows it expands true for are listed.
    pub filter: Option<String>,
}

/// `split-window` (`-h -v -c -p -l -t`, plus a trailing shell command;
/// from M3.6 `-b -f -d -P -F`, TC.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitWindow {
    pub dir: Dir,
    pub cwd: Option<String>,
    pub size: SplitSize,
    pub target: Target,
    pub command: Option<Vec<String>>,
    /// `-b`: left of or above the target.
    pub before: bool,
    /// `-f`: beside the whole window.
    pub full: bool,
    /// `-d`: the new pane does not become active.
    pub detached: bool,
    /// `-P`: print the new pane (`-F`, else tmux's default format).
    pub print: bool,
    /// `-F`.
    pub format: Option<String>,
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
    /// `-l` (M3.6, TC.8): the target window's last pane, as `last-pane`.
    pub last: bool,
    /// `-Z` (M3.6, TC.8): a zoomed window stays zoomed.
    pub keep_zoom: bool,
    /// `-T` (M3.6, TC.11): sets the pane's title (expanded as a format)
    /// instead of selecting it, as tmux does.
    pub title: Option<String>,
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

/// `resize-pane` (`-L -R -U -D -Z -t`, an amount, and from M3.6 `-x -y`).
/// `width`/`height` set the pane's size outright, in cells or `N%` of the
/// window; they may come with a direction, or alone, in which case
/// `action` is `Edges` with no edges. They apply before the direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizePane {
    pub action: ResizeAction,
    pub target: Target,
    /// `-x` (M3.6, TC.8).
    pub width: Option<SplitSize>,
    /// `-y` (M3.6, TC.8).
    pub height: Option<SplitSize>,
}

/// `respawn-pane` (`-k`, plus a trailing shell command that replaces the
/// pane's resume argv; omitted means resume the original command).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RespawnPane {
    pub kill: bool,
    pub target: Target,
    pub command: Option<Vec<String>>,
    /// `-c` (M3.6, TC.8): the start directory, raw (a format).
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPanes {
    pub target: Target,
    pub format: Option<String>,
    /// `-a` (M3.6): every pane of every session.
    pub all: bool,
    /// `-s` (M3.6): every pane of the target's session.
    pub session: bool,
    /// `-f` (M3.6): a format; only panes it expands true for are listed.
    pub filter: Option<String>,
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
    /// `-J` (M3.6): join wrapped lines and keep trailing spaces.
    pub join: bool,
}

/// `set-option`/`show-options` scope flags (`-g -s -w -p`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionScope {
    Global,
    Session,
    Window,
    Pane,
}

/// `set-option`/`set` (`-g -s -w -p -u --persist`, and from M3.6 `-a -F
/// -q -o`, spec §4.5's options row). `scope` is `None` when no scope flag
/// was given: the option's own schema (dispatch, T1.5c) decides the
/// default scope for that option name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetOption {
    pub scope: Option<OptionScope>,
    pub unset: bool,
    pub persist: bool,
    pub target: Target,
    pub name: String,
    pub value: Option<String>,
    /// `-a`: append the value to the option's current value.
    pub append: bool,
    /// `-F`: expand the value as a format before setting it.
    pub expand: bool,
    /// `-q`: no error for an unknown or ambiguous option.
    pub quiet: bool,
    /// `-o`: set only if the option is not already set.
    pub only_if_unset: bool,
}

/// `show-options`/`show` (`-g -s -w -p -t`, and from M3.6 `-v -q`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowOptions {
    pub scope: Option<OptionScope>,
    pub target: Target,
    pub name: Option<String>,
    /// `-v`: the value alone, without the name.
    pub value_only: bool,
    /// `-q`: no error for an unknown option.
    pub quiet: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayMessage {
    pub print: bool,
    pub target: Target,
    /// The message/format text, raw (expanded at dispatch time), from the
    /// argument or else `-F`. `None` means tmux's default status-line
    /// format.
    pub text: Option<String>,
    /// `-c` (TC.9): the client the message goes to, raw.
    pub client: Option<String>,
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
    /// `-a` (M3.6, TC.8; `next-window` and `previous-window` only): the
    /// next or previous window with an alert.
    pub alert: bool,
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
    /// `-c` (M3.6, TC.9): the client to switch, raw like
    /// [`DetachClient::client`]; `None` is the requesting client.
    pub client: Option<String>,
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

/// `list-keys` (`-T`, and from M3.6 `-1 -N` with a key).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListKeys {
    pub table: Option<String>,
    /// `-1` (TC.7): only the first matching binding.
    pub first: bool,
    /// `-N` (TC.7): bindings with their notes.
    pub notes: bool,
    /// The one key to list, raw.
    pub key: Option<String>,
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

/// `command-prompt` (`-p -I`, then a template; R24; from M3.6 `-b -k -1
/// -T`, TC.7).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandPrompt {
    /// `-p`: the prompt text.
    pub prompt: Option<String>,
    /// `-I`: the text the prompt starts with.
    pub initial: Option<String>,
    /// The command the answer is put into at `%%`.
    pub template: Option<String>,
    /// `-b` (D21): the invoking client does not wait for the answer.
    pub background: bool,
    /// `-k`: read one key and answer with its name.
    pub key: bool,
    /// `-1`: answer after one keystroke.
    pub single: bool,
    /// `-T`: the prompt type (accepted; grofe has no completion).
    pub prompt_type: Option<String>,
}

/// `run-shell` (a shell command; from M3.6 `-b`, TC.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunShell {
    pub cmd: String,
    /// `-b`: run in the background; the caller does not wait.
    pub background: bool,
}

/// `select-layout` (`-t`, a layout name; from M3.6 `-E`, TC.8). `name` is
/// empty only with `-E`, which needs none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectLayout {
    pub target: Target,
    pub name: String,
    /// `-E`: spread the panes out evenly.
    pub spread: bool,
}

// M3.6 verbs (spec §4.5's rows, native forms; TC.2 parses them). Each
// struct names the task whose `server/dispatch` handler implements it.
// Flags follow tmux 3.7b's letters (its `list-commands` golden), limited to
// the ones a §4.5 row or the corpus (`docs/compat-coverage.md`) names; any
// other flag is an unknown flag natively.
//
// A verb whose tmux form needs a positional (`wait-for <channel>`) fails
// to parse without it (`too few arguments`), except when it is given no
// arguments at all: the bare verb parses, with that field `None`, because
// the stub-phase tests in each dispatch file run every verb bare and
// expect its handler to answer. So a handler sees `None` only for the bare
// verb, and refuses it with [`ParseError::TooFewArguments`]'s wording.

/// `join-pane` and `move-pane` (TC.8): move pane `-s` next to pane `-t`,
/// splitting `-t` as `split-window` would. `-l` is cells, or `N%` of the
/// target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinPane {
    /// `-s`.
    pub src: Target,
    /// `-t`.
    pub dst: Target,
    /// `-b`: before (left of or above) the target.
    pub before: bool,
    /// `-d`: the moved pane does not become active.
    pub detach: bool,
    /// `-f`: split the whole window, not just the target pane.
    pub full: bool,
    /// `-h`: side by side.
    pub horizontal: bool,
    /// `-v`: one above the other (tmux's default).
    pub vertical: bool,
    /// `-l`.
    pub size: Option<SplitSize>,
}

/// `move-pane` (TC.8, M36-R6): tmux's twin of `join-pane`, same flags.
pub type MovePane = JoinPane;

/// `swap-pane` (TC.8): swap `-s` (default: the current pane) with `-t`,
/// or with `-U`/`-D` the target with the previous or next pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapPane {
    /// `-s`; `None` when not given.
    pub src: Option<Target>,
    /// `-t`.
    pub dst: Target,
    /// `-U`: with the previous pane.
    pub up: bool,
    /// `-D`: with the next pane.
    pub down: bool,
}

/// `swap-window` (TC.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapWindow {
    /// `-s`; `None` when not given (the current window).
    pub src: Option<Target>,
    /// `-t`.
    pub dst: Target,
}

/// `break-pane` (TC.8): move pane `-s` into a window of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakPane {
    /// `-s`.
    pub src: Target,
    /// `-t`: where the new window goes; `None` is the next free index.
    pub dst: Option<Target>,
    /// `-d`: the new window does not become current.
    pub detach: bool,
    /// `-n`: the new window's name.
    pub name: Option<String>,
}

/// `resize-window` (TC.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizeWindow {
    pub target: Target,
    /// `-x`, in cells.
    pub width: Option<u16>,
    /// `-y`, in cells.
    pub height: Option<u16>,
    /// `-A`: the size of the largest attached client.
    pub largest: bool,
    /// `-a`: the size of the smallest attached client.
    pub smallest: bool,
}

/// `last-pane` (TC.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastPane {
    pub target: Target,
}

/// `rotate-window` (TC.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotateWindow {
    pub target: Target,
    /// `-U`: panes move up (tmux's default).
    pub up: bool,
    /// `-D`: panes move down.
    pub down: bool,
}

/// `move-window` (TC.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveWindow {
    /// `-s`.
    pub src: Target,
    /// `-t`; `None` when not given.
    pub dst: Option<Target>,
    /// `-a`: to the index after `-t`.
    pub after: bool,
    /// `-r`: renumber the windows of session `-t` instead.
    pub renumber: bool,
}

/// `next-layout` (TC.8, M36-R6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextLayout {
    pub target: Target,
}

/// `refresh-client` (TC.9).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RefreshClient {
    /// `-t`: the client, raw; `None` is the requesting client.
    pub client: Option<String>,
    /// `-S`: redraw only the status line.
    pub status: bool,
    /// `-c`, `-L`, `-R`, `-U` or `-D` (tmux's stock pan keys): which way
    /// the visible part of a window larger than the client moves; the first
    /// given in that order wins, as in tmux.
    pub pan: Option<Pan>,
    /// The pan's cells, raw (tmux checks it is 1 or more when it runs).
    pub adjustment: Option<String>,
}

/// `refresh-client`'s pan flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pan {
    /// `-c`: back to following the cursor.
    Reset,
    Left,
    Right,
    Up,
    Down,
}

/// `show-messages` (TC.9; tmux's `info` is `show-messages -JT`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShowMessages {
    /// `-t`: the client, raw.
    pub client: Option<String>,
    /// `-J`: also the job list.
    pub jobs: bool,
    /// `-T`: also the terminal list.
    pub terminals: bool,
}

/// `choose-client` (TC.9).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChooseClient {
    /// `-Z`: zoom the pane the chooser opens over.
    pub zoom: bool,
}

/// `suspend-client` (TC.9).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SuspendClient {
    /// `-t`: the client, raw.
    pub client: Option<String>,
}

/// `find-window` (TC.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindWindow {
    pub target: Target,
    /// `-Z`.
    pub zoom: bool,
    /// The text to match (required; `None` only for a bare `find-window`).
    pub pattern: Option<String>,
}

/// `choose-tree` (TC.9).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChooseTree {
    /// `-Z`.
    pub zoom: bool,
    /// `-s`: start with sessions collapsed (tmux's stock `prefix s`).
    pub sessions: bool,
    /// `-w`: start with windows expanded under their sessions.
    pub windows: bool,
}

/// `wait-for` (TC.9, M36-R1): block until `channel` is signalled, or with
/// `-S` signal its waiters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WaitFor {
    /// Required; `None` only for a bare `wait-for`.
    pub channel: Option<String>,
    /// `-S`.
    pub signal: bool,
}

/// `display-popup` (TC.6). Positions stay raw: numbers, tmux's position
/// letters (M36-R5 pins each with a golden) or formats, read by the
/// handler.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DisplayPopup {
    /// `-E`: close when the command exits.
    pub close_on_exit: bool,
    /// `-B`: no border.
    pub no_border: bool,
    /// `-b`: the border lines style (cosmetic).
    pub border: Option<String>,
    /// `-c`: the one client to show it on, raw.
    pub client: Option<String>,
    /// `-d`: the command's directory, raw (a format).
    pub cwd: Option<String>,
    /// `-e NAME=value`, in order, a later one winning; one without `=` is
    /// ignored, as tmux 3.7b ignores it.
    pub env: Vec<(String, String)>,
    /// `-T`: the title (cosmetic), raw.
    pub title: Option<String>,
    /// `-w`: cells, or `N%` of the client.
    pub width: Option<SplitSize>,
    /// `-h`: cells, or `N%` of the client.
    pub height: Option<SplitSize>,
    /// `-x`, raw.
    pub x: Option<String>,
    /// `-y`, raw.
    pub y: Option<String>,
    /// The command and its arguments; `None` is the default shell.
    pub command: Option<Vec<String>>,
}

/// A `new-pane` position (`-X`/`-Y`): cells from the window's origin,
/// possibly negative, or `N%` of the window. tmux 3.7b accepts both
/// (`-X -1`, `-X 50%`) and checks the range against the window when it
/// runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offset {
    Cells(i32),
    Percent(u8),
}

/// `new-pane` (TC.6, M36-R1): a floating pane. The flag letters follow
/// tmux 3.7b's usage (`-x width -y height -X x-position -Y y-position`),
/// which fzf 0.74.4 sends as `-x 138 -y 30 -X 31 -Y 9`; TC.6's golden
/// (M36-R2) settles their meaning. Sizes may be `N%` (`-x 50%`); whether
/// one fits the window is checked when it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPane {
    /// `-P`: print the new pane's `format`.
    pub print: bool,
    /// `-F`: the format `-P` prints, raw.
    pub format: Option<String>,
    /// `-t`.
    pub target: Target,
    /// `-c`: the command's directory, raw (a format).
    pub cwd: Option<String>,
    /// `-x`.
    pub width: Option<SplitSize>,
    /// `-y`.
    pub height: Option<SplitSize>,
    /// `-X`.
    pub x: Option<Offset>,
    /// `-Y`.
    pub y: Option<Offset>,
    pub command: Option<Vec<String>>,
}

/// One `display-menu` item: tmux reads its arguments as `name key command`
/// triples, except that an empty name is a separator on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    Separator,
    /// A name starting with `-` is shown disabled, as in tmux. `key` is a
    /// raw key name; `command` a stored command string, which may open a
    /// further `display-menu`.
    Item {
        name: String,
        key: String,
        command: String,
    },
}

/// `display-menu` (TC.7). Positions stay raw, as for [`DisplayPopup`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DisplayMenu {
    /// `-T`, raw.
    pub title: Option<String>,
    /// `-x`, raw.
    pub x: Option<String>,
    /// `-y`, raw.
    pub y: Option<String>,
    /// Required (at least one); empty only for a bare `display-menu`.
    pub items: Vec<MenuEntry>,
}

/// `display-panes` (TC.7): no flags natively.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DisplayPanes {}

/// `confirm-before` (TC.7).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfirmBefore {
    /// `-p`, raw.
    pub prompt: Option<String>,
    /// The stored command string run on `y`; required, `None` only for a
    /// bare `confirm-before`.
    pub command: Option<String>,
}

/// The condition of grofe's conditional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// `-F`: a format, true when it expands to neither empty nor `0`.
    Format(String),
    /// A shell command, true when it exits 0 (run off the actor, TC.10).
    Shell(String),
}

/// `if-shell` (TC.10), grofe's conditional (spec §4.5 bindings row): the
/// format form (`-F`) and the shell form, `-b` not waiting for the shell.
/// `then` and `otherwise` are stored command strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct If {
    /// Required with `then`; both `None` only for a bare `if-shell`.
    pub condition: Option<Condition>,
    pub then: Option<String>,
    pub otherwise: Option<String>,
    /// `-b`.
    pub background: bool,
    /// `-t`: the pane the format expands against.
    pub target: Target,
}

/// `set-hook` (TC.5): bind a command to one of grofe's events (the event
/// names are TC.5's; FE.3 maps tmux's hook names onto them).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SetHook {
    /// `-g`.
    pub global: bool,
    /// `-u`: remove the hook.
    pub unset: bool,
    /// Required; `None` only for a bare `set-hook`.
    pub hook: Option<String>,
    /// A stored command string.
    pub command: Option<String>,
}

/// `show-hooks` (TC.5).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShowHooks {
    /// `-g`.
    pub global: bool,
    pub hook: Option<String>,
}

/// `set-environment` (TC.5, spec §4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetEnvironment {
    /// `-g`: the global environment, else the session's.
    pub global: bool,
    /// `-u`: unset it.
    pub unset: bool,
    /// `-h`: hidden, never given to a new pane.
    pub hidden: bool,
    /// `-t`: the session.
    pub target: Target,
    /// Required; `None` only for a bare `set-environment`.
    pub name: Option<String>,
    pub value: Option<String>,
}

/// `show-environment` (TC.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowEnvironment {
    /// `-g`.
    pub global: bool,
    /// `-h`: the hidden variables.
    pub hidden: bool,
    /// `-t`: the session.
    pub target: Target,
    pub name: Option<String>,
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
    // M3.6 (the stub edit adds them; TC.2 parses their flags, the task
    // named on each struct implements it). No `list-commands`: it is
    // front-end only (M36-R1).
    JoinPane(JoinPane),
    SwapPane(SwapPane),
    SwapWindow(SwapWindow),
    BreakPane(BreakPane),
    ResizeWindow(ResizeWindow),
    LastPane(LastPane),
    RotateWindow(RotateWindow),
    MoveWindow(MoveWindow),
    MovePane(MovePane),
    NextLayout(NextLayout),
    /// `respawn-window` (TC.8): `respawn-pane`'s shape with a window target.
    RespawnWindow(RespawnPane),
    RefreshClient(RefreshClient),
    ShowMessages(ShowMessages),
    ChooseClient(ChooseClient),
    SuspendClient(SuspendClient),
    FindWindow(FindWindow),
    ChooseTree(ChooseTree),
    WaitFor(WaitFor),
    DisplayPopup(DisplayPopup),
    NewPane(NewPane),
    DisplayMenu(DisplayMenu),
    DisplayPanes(DisplayPanes),
    ConfirmBefore(ConfirmBefore),
    If(If),
    SetHook(SetHook),
    ShowHooks(ShowHooks),
    SetEnvironment(SetEnvironment),
    ShowEnvironment(ShowEnvironment),
}
