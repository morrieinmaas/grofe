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

pub use format::{
    FormatVars, Inner, LoopItem, LoopKind, LoopSort, NameKind, SortOrder, all_variables, expand,
    expand_time, expand_traced,
};
pub use keys::{is_key_name, key_bytes};
pub use parse::{
    ConfigHost, parse, parse_sequence, split_command_string, split_config_file,
    split_config_file_with,
};
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
    /// `-t` (B1e): the session to group with, raw (a name that is no
    /// session makes a new group of that name).
    pub group: Option<String>,
    /// `-e`: variables of the session's environment, which its panes get.
    pub env: Vec<(String, String)>,
    /// `-D`: with `-A`, detach the other clients of the session found.
    pub detach_others: bool,
    /// `-X`: like `-D`, and the detached clients' parents get SIGHUP.
    pub kill_parent: bool,
    /// `-E`: the session's environment is not taken from the client's.
    pub no_env: bool,
    /// `-f`: client flags for the client this attaches.
    pub flags: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KillSession {
    pub target: Target,
    /// `-a`: every session but the target.
    pub others: bool,
    /// `-C`: clear the target's alert flags instead.
    pub clear_alerts: bool,
    /// `-g`: the target's whole session group.
    pub group: bool,
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
    /// `-O` and `-r`.
    pub sort: SortFlags,
}

/// The `-O order` and `-r` of the commands that list things (tmux 3.7b's
/// `sort.c`), as given: the order is checked when the command runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SortFlags {
    pub order: Option<String>,
    pub reversed: bool,
}

/// `new-window` (`-d -t -n -c`, plus a trailing shell command; from M3.6
/// `-a -P -F`, TC.8).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
    /// `-e`: variables for the new pane's environment.
    pub env: Vec<(String, String)>,
    /// `-b`: at the index before the target window.
    pub before: bool,
    /// `-k`: a window at the index asked for goes, instead of an error.
    pub kill: bool,
    /// `-S`: with `-n`, select a window of that name if there is one.
    pub select_existing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillWindow {
    pub target: Target,
    /// `-a` (FE.1): every window of the target's session but the target.
    pub all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectWindow {
    pub target: Target,
    /// `-n`, `-p`, `-l`: the target's session moves to its next, previous
    /// or last window instead (tmux's order of precedence).
    pub step: Option<WindowMove>,
    /// `-T`: selecting the current window goes to the last one instead.
    pub toggle: bool,
}

/// Which window `select-window -n/-p/-l` moves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowMove {
    Next,
    Previous,
    Last,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListWindows {
    pub target: Target,
    pub format: Option<String>,
    /// `-a` (M3.6): every window of every session.
    pub all: bool,
    /// `-f` (M3.6): a format; only windows it expands true for are listed.
    pub filter: Option<String>,
    /// `-O` and `-r`.
    pub sort: SortFlags,
}

/// What `split-window` and `new-pane` set on the pane they make: its styles
/// (`-s`, `-S`, `-R`) and whether it stays when its command exits (`-k`, or
/// `-m` with the message it shows).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaneSetup {
    /// `-s`: the pane's `window-style` and `window-active-style`.
    pub style: Option<String>,
    /// `-S`: its `pane-active-border-style`.
    pub active_border: Option<String>,
    /// `-R`: its `pane-border-style`.
    pub inactive_border: Option<String>,
    /// `-k` or `-m`: `remain-on-exit key`.
    pub keep: bool,
    /// `-m`: its `remain-on-exit-format`.
    pub message: Option<String>,
}

/// `split-window` (`-h -v -c -p -l -t`, plus a trailing shell command;
/// from M3.6 `-b -f -d -P -F`, TC.8).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
    /// `-e`: variables for the new pane's environment.
    pub env: Vec<(String, String)>,
    /// `-Z`: the new pane fills the window.
    pub zoom: bool,
    /// `-s -S -R -k -m`.
    pub setup: PaneSetup,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KillPane {
    pub target: Target,
    /// `-a`: every pane of the window but the target.
    pub others: bool,
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
    /// `-e` / `-d`: let the pane take input again, or stop it taking any
    /// (tmux's `PANE_INPUTOFF`), instead of selecting it.
    pub input: Option<bool>,
    /// `-m` / `-M`: toggle or clear the server's marked pane instead of
    /// selecting.
    pub mark: Mark,
}

/// `select-pane -m` / `-M`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mark {
    #[default]
    No,
    /// `-m`: mark the target pane, or unmark it if it is the marked pane.
    Toggle,
    /// `-M`: clear the marked pane.
    Clear,
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
    /// `-e`: variables for the new process's environment.
    pub env: Vec<(String, String)>,
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
    /// `-O` and `-r`.
    pub sort: SortFlags,
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
    /// `-X` (M5): the items are a copy-mode command and its arguments,
    /// run by the target pane's mode.
    pub command: bool,
    /// `-N` (M5): the repeat count, a format expanded when it runs.
    pub count: Option<String>,
    /// `-F` (M5): the mode command expands its arguments as formats.
    pub format: bool,
    /// `-R`: the pane's terminal state is reset first (palette, modes,
    /// screen).
    pub reset: bool,
    /// `-K`: the keys go to the client's key tables, as if it typed them.
    pub client_key: bool,
    /// `-c`: the client `-K` types on, else the target session's.
    pub client: Option<String>,
    /// `-M`: the mouse event the command runs for goes to its pane.
    pub mouse: bool,
}

/// `clear-history` (`-H`): drops the pane's scrollback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearHistory {
    pub target: Target,
    /// `-H`: forget the pane's hyperlinks too.
    pub hyperlinks: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturePane {
    pub target: Target,
    pub print: bool,
    /// `-b` (without `-p`): the buffer the text goes to, else a new one.
    pub buffer: Option<String>,
    pub opts: CaptureOpts,
    /// `-J` (M3.6): join wrapped lines and keep trailing spaces.
    pub join: bool,
    /// `-N` (B2b): keep each line's trailing spaces, up to the cells the
    /// line has allocated.
    pub keep_blanks: bool,
    /// `-T` (B2b): stop each line at the last cell the program wrote.
    pub written_only: bool,
    /// `-L`: each line starts with its number.
    pub numbered: bool,
    /// `-F`: each line starts with its flags.
    pub flags: bool,
    /// `-C`: backslashes, and the ESC of `-e`, are written out.
    pub escaped: bool,
    /// `-H`: the hyperlinks of the lines, not their text.
    pub links: bool,
    /// `-a`: the alternate screen's saved text.
    pub alternate: bool,
    /// `-M`: the screen of the pane's mode.
    pub mode: bool,
    /// `-P`: the output not yet parsed.
    pub pending: bool,
    /// `-q`: with `-a`, no error without an alternate screen.
    pub quiet: bool,
}

/// `set-option`/`show-options` scope flags (`-g -s -w -p`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionScope {
    Global,
    Session,
    Window,
    Pane,
    /// tmux's server scope for a user option (`set -s @x`): the tmux front
    /// end builds it (FE.1); natively `-s` is the session scope.
    Server,
    /// tmux's global window scope for a user option (`set -gw @x`), apart
    /// from the global session one, as tmux keeps them (FE.1).
    GlobalWindow,
}

/// `set-option`/`set` (`-g -s -w -p -u --persist`, and from M3.6 `-a -F
/// -q -o`, spec §4.5's options row). `scope` is `None` when no scope flag
/// was given: the option's own schema (dispatch, T1.5c) decides the
/// default scope for that option name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetOption {
    pub scope: Option<OptionScope>,
    pub unset: bool,
    /// `-U`: unset, and for a window option in the window's panes too.
    pub unset_panes: bool,
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
    /// The value as the tmux front end was sent it, when it rewrote it
    /// into grofe's format spelling (FE.3): what a tmux-dialect `show`
    /// prints. Never parsed; `None` natively.
    pub shown: Option<String>,
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
    /// `-A`: the options the scope inherits and does not set too, shown
    /// as `name* value`.
    pub inherited: bool,
    /// `-H`: the hooks the scope sets too, in a listing of all of it.
    pub hooks: bool,
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
    /// `-d` (TC.9): how long the message shows, in milliseconds, raw
    /// (checked at dispatch time, as tmux does); 0 means until a key.
    pub delay: Option<String>,
    /// `-a` (B2b): print every format variable as `name=value`.
    pub all: bool,
    /// `-l` (B2b): the text is shown as it is, not expanded.
    pub literal: bool,
    /// `-v` (B2b): print how the format expanded, step by step.
    pub verbose: bool,
    /// `-N`: keys do not take the message down (only its time does).
    pub ignore_keys: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KillServer;

/// `subscribe <event>... [-t target]` (control mode only): event names or
/// `*` patterns; empty means every event but `pane-output`. `target`, when
/// given, limits the events to a pane, window or session (spec §7.2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Subscribe {
    pub events: Vec<String>,
    pub target: Option<Target>,
}

/// `attach-session`/`attach` (`-t -d --nest`). `nest` is grofe's force for
/// self-nesting (spec §4.6); tmux's `-f` (client flags) is reserved for the
/// tmux front end (M3.6).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AttachSession {
    pub target: Target,
    /// `-d`: detach other clients attached to the session.
    pub detach_others: bool,
    pub nest: bool,
    /// `-c`: the session's directory for new windows from now on.
    pub cwd: Option<String>,
    /// `-E`: do not update the session's environment from the client's.
    pub no_env: bool,
    /// `-r`: a read-only client.
    pub read_only: bool,
    /// `-x`: like `-d`, and the detached clients' parents get SIGHUP.
    pub kill_parent: bool,
    /// `-f`: client flags (`read-only`, `ignore-size`, ...).
    pub flags: Option<String>,
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
    /// `-P`: the detached clients' parents are sent SIGHUP.
    pub kill_parent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListClients {
    pub format: Option<String>,
    /// `-t` (M3.6, FE.2): only the clients showing this session.
    pub target: Option<Target>,
    /// `-f` (M3.6, FE.2): a format; only clients it expands true for are
    /// listed.
    pub filter: Option<String>,
    /// `-O` and `-r`.
    pub sort: SortFlags,
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
    /// `-Z`: a zoomed window stays zoomed on the pane switched to.
    pub keep_zoom: bool,
    /// `-E`: the session's environment is not updated from the client's.
    pub no_env: bool,
    /// `-r`: toggles the client's read-only flag, and reverses `-n`/`-p`.
    pub toggle_read_only: bool,
    /// `-O`: the order `-n` and `-p` walk the sessions in.
    pub order: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendPrefix {
    pub target: Target,
    /// `-2`: the `prefix2` key instead of `prefix`.
    pub second: bool,
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

/// `unbind-key` (`-n -T -a -q`, then a key unless `-a`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnbindKey {
    pub table: String,
    /// `-a`: every binding in the table.
    pub all: bool,
    pub key: Option<String>,
    /// `-q`: an unknown key or table is no error. (A missing key, or a key
    /// with `-a`, parse as neither `all` nor a `key`: nothing to do.)
    pub quiet: bool,
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
    /// `-a`: with `-N`, the bindings that have no note too.
    pub all: bool,
    /// `-F`: the format of a line.
    pub format: Option<String>,
    /// `-P`: the prefix string `-N` lines start with.
    pub prefix: Option<String>,
    /// `-O` and `-r`.
    pub sort: SortFlags,
}

/// `source-file` (`-q`, `-F`, `-n`, `-v`, a path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub quiet: bool,
    /// `-F`: the path is a format.
    pub format: bool,
    /// `-n`: read a tmux file and report its errors, run nothing.
    pub parse_only: bool,
    /// `-v`: print each command as it is read.
    pub verbose: bool,
    /// `-t`: the pane the file's commands and `-F` run for.
    pub target: Option<Target>,
    /// One or more paths, each possibly a glob.
    pub paths: Vec<String>,
}

/// `reload` (grofe's own): re-read the config chain (spec §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reload;

/// `list-plugins` (grofe's own, T6.3): each configured plugin's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ListPlugins;

/// `plugin-restart <name>` (grofe's own, T6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRestart {
    pub name: String,
}

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
    /// `-N` (FE.1): only digits may be typed.
    pub numeric: bool,
    /// `-i` (FE.1): tmux's incremental prompt (copy-mode search).
    pub incremental: bool,
    /// `-F` (FE.1): the template is expanded as a format when the prompt
    /// opens.
    pub expand: bool,
    /// `-e`: Backspace on an empty line cancels the prompt.
    pub bspace_exit: bool,
    /// `-l`: `-p` and `-I` are one prompt, commas and all.
    pub literal: bool,
    /// `-t`: the client, raw.
    pub client: Option<String>,
}

/// `run-shell` (a shell command; from M3.6 `-b`, TC.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunShell {
    /// Empty only with `-d` and no command: then only the delay.
    pub cmd: String,
    /// `-b`: run in the background; the caller does not wait.
    pub background: bool,
    /// `-d` (FE.1): seconds to wait before running.
    pub delay: Option<std::time::Duration>,
    /// `-t` (FE.3): the pane the command's formats expand against.
    pub target: Target,
    /// `-C`: `cmd` is a command of the server's, not a shell command.
    pub command: bool,
    /// `-E`: the shell's stderr is shown with its stdout.
    pub stderr: bool,
    /// `-c`: the directory the command starts in, else the client's.
    pub cwd: Option<String>,
    /// The arguments after the command: `#{1}`, `#{2}`, ... in it.
    pub args: Vec<String>,
}

/// `select-layout` (`-t`, a layout name; from M3.6 `-E`, TC.8). `name` is
/// empty only with `-E`, which needs none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectLayout {
    pub target: Target,
    pub name: String,
    /// `-E`: spread the panes out evenly.
    pub spread: bool,
    /// `-n`: the preset after the last one, as `next-layout`.
    pub next: bool,
    /// `-p`: the preset before the last one, as `previous-layout`.
    pub previous: bool,
    /// `-o`: the layout the window had before the last `select-layout`.
    pub undo: bool,
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
    /// `-d`: no pane becomes active because of the swap.
    pub detach: bool,
    /// `-Z`: a zoomed window stays zoomed.
    pub keep_zoom: bool,
}

/// `swap-window` (TC.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapWindow {
    /// `-s`; `None` when not given (the current window).
    pub src: Option<Target>,
    /// `-t`.
    pub dst: Target,
    /// `-d`: in tmux 3.7b the destination place becomes its session's
    /// current window (the flag is the opposite of what its manual says).
    pub select: bool,
}

/// `break-pane` (TC.8): move pane `-s` into a window of its own.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BreakPane {
    /// `-s`.
    pub src: Target,
    /// `-t`: where the new window goes; `None` is the next free index.
    pub dst: Option<Target>,
    /// `-d`: the new window does not become current.
    pub detach: bool,
    /// `-n`: the new window's name.
    pub name: Option<String>,
    /// `-a`: at the index after `-t`'s window, later windows moving up.
    pub after: bool,
    /// `-b`: at the index before `-t`'s window.
    pub before: bool,
    /// `-P`: print the new window (`-F`, else tmux's default format).
    pub print: bool,
    /// `-F`.
    pub format: Option<String>,
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
    /// `-L`, `-R`, `-U`, `-D`: the width or height, after `-x`/`-y`, less
    /// or more by `adjust` cells (tmux checks them in that order).
    pub direction: Option<Edge>,
    /// The `adjustment`, in cells (1 when not given).
    pub adjust: u32,
}

/// `last-pane` (TC.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastPane {
    pub target: Target,
    /// `-Z`: a zoomed window stays zoomed on the last pane.
    pub keep_zoom: bool,
    /// `-e` / `-d`: let the last pane take input, or stop it, instead.
    pub input: Option<bool>,
}

/// `clock-mode [-t pane]` (B1d): the pane shows a clock until a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockMode {
    pub target: Target,
}

/// `pipe-pane [-IOo] [-t pane] [command]` (B2b): the pane's output goes to
/// a shell command, or what it prints goes into the pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipePane {
    pub target: Target,
    /// `-I`: the command's output is typed into the pane.
    pub input: bool,
    /// `-O`: the pane's output goes to the command (so does no `-I`).
    pub output: bool,
    /// `-o`: only open a pipe if there was none.
    pub toggle: bool,
    /// None or empty: only close the pane's pipe.
    pub command: Option<String>,
}

/// `rotate-window` (TC.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotateWindow {
    pub target: Target,
    /// `-U`: panes move up (tmux's default).
    pub up: bool,
    /// `-D`: panes move down.
    pub down: bool,
    /// `-Z`: a zoomed window stays zoomed, on the new active pane.
    pub keep_zoom: bool,
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
    /// `-b` (B1e): to the index before `-t`.
    pub before: bool,
    /// `-d` (B1e): do not make the window current.
    pub detach: bool,
    /// `-k` (B1e): unlink a window at the index.
    pub kill: bool,
}

/// `link-window` (B1e): window `-s` also appears in `-t`'s session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkWindow {
    /// `-s`.
    pub src: Target,
    /// `-t`; `None` when not given.
    pub dst: Option<Target>,
    /// `-a`: to the index after `-t`.
    pub after: bool,
    /// `-b`: to the index before `-t`.
    pub before: bool,
    /// `-d`: do not make the window current there.
    pub detach: bool,
    /// `-k`: unlink a window already at the index.
    pub kill: bool,
}

/// `unlink-window` (B1e).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlinkWindow {
    pub target: Target,
    /// `-k`: close the window if this is its last link.
    pub kill: bool,
}

/// `next-layout` (TC.8, M36-R6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextLayout {
    pub target: Target,
}

/// `lock-server`, `lock-session` and `lock-client` (B4): the lock command
/// runs on every client of the server, of the session, or on one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    pub scope: LockScope,
}

/// Whose clients a [`Lock`] locks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockScope {
    Server,
    Session(Target),
    /// `-t`: the client, raw; `None` is the requesting client.
    Client(Option<String>),
}

/// `server-access [-adlrw] [user]` (B4): who else may use the server.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerAccess {
    /// `-a`: allow the user.
    pub add: bool,
    /// `-d`: deny the user.
    pub deny: bool,
    /// `-l`: list the users.
    pub list: bool,
    /// `-r`: read-only access.
    pub read_only: bool,
    /// `-w`: read-write access.
    pub write: bool,
    pub user: Option<String>,
}

/// `show-prompt-history` and `clear-prompt-history` (B4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptHistory {
    pub clear: bool,
    /// `-T`: one prompt type's history; `None` is every type's.
    pub kind: Option<String>,
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
    /// `-F` then `-f`: client flags, applied in that order.
    pub flags: Vec<String>,
    /// `-l`: ask the terminal for its clipboard.
    pub clipboard: bool,
    /// `-r`: a pane's colour report; only a control client's panes use it.
    pub report: Option<String>,
    /// `-A`, `-B` or `-C`: control mode's, which a client that is not in it
    /// is refused.
    pub control: bool,
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

/// What `choose-tree`, `choose-client` and `choose-buffer` share (tmux's
/// `cmd_choose_tree_exec`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChooseOpts {
    /// `-t`: the pane the chooser opens over.
    pub target: Target,
    /// `-F`: the format of each item's text.
    pub format: Option<String>,
    /// `-f`: a format an item must make true to be listed.
    pub filter: Option<String>,
    /// `-K`: the format of each line's key.
    pub key_format: Option<String>,
    /// `-O`: the sort order, raw.
    pub sort: Option<String>,
    /// `-r`: reverse it.
    pub reversed: bool,
    /// How many `-N`: one starts without the preview, two with a big one.
    pub no_preview: u8,
    /// `-y`: the kill prompts accept at once.
    pub prompt_yes: bool,
    /// `-G`: keep the sessions of a group apart.
    pub groups: bool,
    /// The command to run for the chosen item, `%%` standing for it.
    pub template: Option<String>,
}

/// `choose-client` (TC.9).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChooseClient {
    /// `-Z`: zoom the pane the chooser opens over.
    pub zoom: bool,
    pub opts: ChooseOpts,
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
    /// `-C`: look in the text of the panes.
    pub content: bool,
    /// `-N`: look in the windows' names.
    pub name: bool,
    /// `-T`: look in the panes' titles.
    pub title: bool,
    /// `-r`: the pattern is a regular expression.
    pub regex: bool,
    /// `-i`: case does not matter.
    pub ignore: bool,
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
    pub opts: ChooseOpts,
}

/// `wait-for` (TC.9, M36-R1): block until `channel` is signalled, or with
/// `-S` signal its waiters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WaitFor {
    /// Required; `None` only for a bare `wait-for`.
    pub channel: Option<String>,
    /// `-S`.
    pub signal: bool,
    /// `-L`: take the channel's lock, waiting for whoever holds it (`-S`
    /// wins over it).
    pub lock: bool,
    /// `-U`: let go of the channel's lock (`-S` and `-L` win over it).
    pub unlock: bool,
}

/// `display-popup` (TC.6). Positions stay raw: numbers, tmux's position
/// letters (M36-R5 pins each with a golden) or formats, read by the
/// handler.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DisplayPopup {
    /// `-E`: close when the command exits.
    pub close_on_exit: bool,
    /// `-EE`: only when the command exited with status 0.
    pub close_on_zero: bool,
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
    /// `-T`: the title, raw.
    pub title: Option<String>,
    /// `-s`: the style of the box, over `popup-style`, raw.
    pub style: Option<String>,
    /// `-S`: the style of the border, over `popup-border-style`, raw.
    pub border_style: Option<String>,
    /// `-C`: close the client's popup and do nothing else.
    pub close: bool,
    /// `-k`: any key closes the popup once its command has exited.
    pub any_key: bool,
    /// `-N`: when changing a popup that is open, reset its flags.
    pub reset_flags: bool,
    /// `-t`: the pane the formats expand against and the popup is placed
    /// against; the current one when not given.
    pub target: Option<Target>,
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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
    /// `-e`: variables for the new pane's environment.
    pub env: Vec<(String, String)>,
    /// `-Z`: accepted; a floating pane never changes the window's zoom.
    pub zoom: bool,
    /// `-d`: the new pane does not become active.
    pub detached: bool,
    /// `-s -S -R -k -m`.
    pub setup: PaneSetup,
}

/// `float-pane` (grofe's own; tmux 3.7b has no such verb): a tiled pane
/// becomes a floating one, keeping its process and history. The flags are
/// `new-pane`'s: `-x`/`-y` the size, `-X`/`-Y` the place, each defaulting as
/// there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloatPane {
    /// `-t`.
    pub target: Target,
    pub width: Option<SplitSize>,
    pub height: Option<SplitSize>,
    pub x: Option<Offset>,
    pub y: Option<Offset>,
}

/// `unfloat-pane` (grofe's own; tmux 3.7b has no such verb): a floating
/// pane goes back into the layout, keeping its process and history, by
/// splitting the tiled pane under its centre, or `-s`, or the window's last
/// active tiled pane, or its first. `dir` is `-h` (side by side) or `-v`
/// (one above the other), `-h` winning as in `join-pane`; without either
/// the split runs along that tiled pane's longer side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnfloatPane {
    /// `-t`.
    pub target: Target,
    /// `-s`: the tiled pane to split.
    pub split: Option<Target>,
    pub dir: Option<Dir>,
}

/// `place-pane` (grofe's own): moves a floating pane to the window cell
/// `-X`, `-Y` of its top-left content cell, as `new-pane` reads them; an
/// axis not given stays. The place is held inside the window as a mouse
/// drag holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacePane {
    /// `-t`.
    pub target: Target,
    pub x: Option<Offset>,
    pub y: Option<Offset>,
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
    /// `-t`: the pane the formats expand against, the menu is placed
    /// against and the items run for; the current one when not given.
    pub target: Option<Target>,
    /// `-c`: the one client to show it on, raw.
    pub client: Option<String>,
    /// `-b`: the border lines, raw.
    pub border: Option<String>,
    /// `-s`: the style of the box, over `menu-style`, raw.
    pub style: Option<String>,
    /// `-H`: the style of the chosen item, over `menu-selected-style`, raw.
    pub selected_style: Option<String>,
    /// `-S`: the style of the border, over `menu-border-style`, raw.
    pub border_style: Option<String>,
    /// `-C`: the item chosen to start with (`-` for none), raw.
    pub starting_choice: Option<String>,
    /// `-O`: stay open after the button that opened the menu is released.
    pub stay_open: bool,
    /// `-M`: take the mouse even when no mouse event opened the menu.
    pub mouse: bool,
    /// Required (at least one); empty only for a bare `display-menu`.
    pub items: Vec<MenuEntry>,
}

/// `display-panes` (TC.7).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DisplayPanes {
    /// `-d`: how long the labels stay, in milliseconds, raw; 0 for ever.
    pub duration: Option<String>,
    /// `-b`: the caller does not wait for the labels to close.
    pub background: bool,
    /// `-N`: the labels take no keys; any key closes them.
    pub no_keys: bool,
    /// `-t`: the one client to show them on, raw.
    pub client: Option<String>,
    /// The command a chosen pane runs; `%%` is the pane's id. Default
    /// `select-pane -t "%%%"`.
    pub template: Option<String>,
}

/// `confirm-before` (TC.7).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfirmBefore {
    /// `-p`, raw.
    pub prompt: Option<String>,
    /// `-y`: Enter confirms too.
    pub default_yes: bool,
    /// `-c`: the key that confirms (default `y`), raw.
    pub confirm_key: Option<String>,
    /// `-b`: the caller does not wait for the answer.
    pub background: bool,
    /// `-t`: the client that is asked, by name or tty.
    pub client: Option<String>,
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
/// names are TC.5's; FE.3 maps tmux's hook names onto them). The flags are
/// tmux's: `-g` the global hooks, `-p`/`-w`/`-t` a pane's, window's or
/// session's own, `-a` append, `-u` remove, `-R` run now; a hook is an
/// array, so the name may end in `[N]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SetHook {
    /// `-g`.
    pub global: bool,
    /// `-u`: remove the hook.
    pub unset: bool,
    /// `-a`: add the command after the others.
    pub append: bool,
    /// `-p`: a pane's hook (a window's hook that panes may override).
    pub pane: bool,
    /// `-w`: a window's hook.
    pub window: bool,
    /// `-R`: run the hook now instead of setting it.
    pub run: bool,
    /// `-t`: where the hook is set or run.
    pub target: Target,
    /// The `-t` text, for the errors that name it.
    pub target_text: Option<String>,
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
    /// `-w`: tmux's window and pane hooks, listed apart from the rest.
    pub window: bool,
    /// `-p`: a pane's own hooks.
    pub pane: bool,
    /// `-t`: whose hooks.
    pub target: Target,
    /// The `-t` text, for the errors that name it.
    pub target_text: Option<String>,
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
    /// `-r`: mark it removed (`-NAME`), so a new pane lacks it.
    pub clear: bool,
    /// `-F`: the value is a format.
    pub format: bool,
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
    /// `-s`: shell `export` and `unset` lines.
    pub shell: bool,
    /// `-t`: the session.
    pub target: Target,
    pub name: Option<String>,
}

/// `__register-source [-n name] <command>` (FE.1): grofe's own hidden
/// verb, which registers a status source (`sources::register`). The tmux
/// front end emits it for a `#(cmd)` a rewrite turns into `#{@src-...}`
/// (FE.3), so the registration runs on the server with its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterSource {
    /// `-n`: a named source (`@<name>`); none is an inline one keyed by
    /// the command (`@src-<hash>`, `sources::inline_key`). A named source
    /// registered this way never expires, as a config one does not (an
    /// inline one expires unused, `sources::INLINE_EXPIRY`); kept so,
    /// since only `#(cmd)` rewrites register sources here.
    pub name: Option<String>,
    pub command: String,
}

/// `set-buffer [-a] [-b name] [-n new-name] [data]` (TM.4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SetBuffer {
    /// `-a`: append to the `-b` buffer (no `-b` makes a new buffer, as
    /// tmux 3.7b does).
    pub append: bool,
    /// `-b`: the buffer; none is a new automatic one.
    pub name: Option<String>,
    /// `-n`: rename the buffer (the data, if any, is ignored).
    pub rename: Option<String>,
    pub data: Option<String>,
    /// `-w`: also send the data to the `-t` client's clipboard (the
    /// current client's, else).
    pub clipboard: bool,
    /// `-t`: the client `-w` sends to, raw.
    pub client: Option<String>,
}

/// `show-buffer [-b name]` (TM.4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShowBuffer {
    pub name: Option<String>,
}

/// `load-buffer [-w] [-b name] [-t target-client] path` (TM.7): `path`
/// is the CLI's (`-` its stdin); `-w` also sends the data to the target
/// client's clipboard.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoadBuffer {
    pub name: Option<String>,
    pub client: Option<String>,
    pub clipboard: bool,
    pub path: String,
}

/// `save-buffer [-a] [-b name] path` (TM.7): `path` is the CLI's (`-` its
/// stdout); `-a` appends.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SaveBuffer {
    pub append: bool,
    pub name: Option<String>,
    pub path: String,
}

/// `list-buffers [-F format]` (TM.4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListBuffers {
    pub format: Option<String>,
    /// `-f`: a format; only buffers it expands true for are listed.
    pub filter: Option<String>,
    /// `-O` and `-r`.
    pub sort: SortFlags,
}

/// `delete-buffer [-b name]` (TM.4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeleteBuffer {
    pub name: Option<String>,
}

/// `paste-buffer [-dpr] [-b name] [-s separator] [-t pane]` (TM.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteBuffer {
    /// `-p`: bracketed-paste markers, for a pane that asked for them.
    pub bracket: bool,
    pub name: Option<String>,
    /// `-d`: delete the buffer after pasting.
    pub delete: bool,
    /// `-r`: line feeds stay line feeds (a separator of LF).
    pub raw: bool,
    /// `-s`: what each line feed becomes (default CR).
    pub separator: Option<String>,
    /// `-S`: send control characters as they are, unsanitized.
    pub unsanitized: bool,
    pub target: Target,
}

/// `copy-mode [-deHMqSu] [-s src-pane] [-t pane]` (M5, TM.1; `-H` and `-M`
/// TM.6; `-d -q -s -S` B1d).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyMode {
    /// `-u`: start a page up.
    pub page_up: bool,
    /// `-e`: scrolling back to the bottom leaves the mode.
    pub scroll_exit: bool,
    /// `-H`: a new mode hides its position indicator.
    pub hide_position: bool,
    /// `-M`: in the pane under the running mouse key, starting a drag
    /// there; nothing outside a mouse key.
    pub mouse: bool,
    /// `-q`: leave every mode of the pane instead of entering one.
    pub cancel: bool,
    /// `-d`: start a page down (with `-e`, leaving the mode at the bottom).
    pub page_down: bool,
    /// `-S`: a scrollbar slider drag.
    pub slider: bool,
    /// `-s`: show this pane's screen instead of the target's.
    pub source: Option<Target>,
    pub target: Target,
}

/// `customize-mode` (WP-A): tmux's option and key browser.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CustomizeMode {
    /// `-Z`.
    pub zoom: bool,
    /// `-t`, `-F`, `-f`, `-N` and `-y`, as the choosers take them.
    pub opts: ChooseOpts,
}

/// `choose-buffer [-Z]` (TM.4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChooseBuffer {
    pub zoom: bool,
    pub opts: ChooseOpts,
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
    ClearHistory(ClearHistory),
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
    // M6 (the stub edit adds them; T6.3 implements them).
    ListPlugins(ListPlugins),
    PluginRestart(PluginRestart),
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
    ClockMode(ClockMode),
    PipePane(PipePane),
    RotateWindow(RotateWindow),
    MoveWindow(MoveWindow),
    LinkWindow(LinkWindow),
    UnlinkWindow(UnlinkWindow),
    MovePane(MovePane),
    NextLayout(NextLayout),
    PreviousLayout(NextLayout),
    Lock(Lock),
    ServerAccess(ServerAccess),
    PromptHistory(PromptHistory),
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
    FloatPane(FloatPane),
    UnfloatPane(UnfloatPane),
    PlacePane(PlacePane),
    DisplayMenu(DisplayMenu),
    DisplayPanes(DisplayPanes),
    ConfirmBefore(ConfirmBefore),
    If(If),
    SetHook(SetHook),
    ShowHooks(ShowHooks),
    SetEnvironment(SetEnvironment),
    ShowEnvironment(ShowEnvironment),
    RegisterSource(RegisterSource),
    /// A tmux command grofe cannot do, in a sourced file: it fails with
    /// this text when its turn comes, and the file's other lines run.
    Refused(String),
    // M5: buffers (TM.4).
    SetBuffer(SetBuffer),
    ShowBuffer(ShowBuffer),
    LoadBuffer(LoadBuffer),
    SaveBuffer(SaveBuffer),
    ListBuffers(ListBuffers),
    DeleteBuffer(DeleteBuffer),
    PasteBuffer(PasteBuffer),
    ChooseBuffer(ChooseBuffer),
    CustomizeMode(CustomizeMode),
    // M5: copy mode (TM.1).
    CopyMode(CopyMode),
}
