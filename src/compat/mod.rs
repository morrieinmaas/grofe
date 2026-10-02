//! tmux compatibility, identity and entry (spec D1, D12, §4.6; TC.1): the
//! server's identity, the `argv[0]` check the CLI makes before its own
//! table, the dialect decision that sends a request's argv to the native
//! parser or to the tmux front end ([`tmux`], FE.1), the variables a pane
//! gets from the identity, and the `compat-miss.log` writer.

pub mod tmux;

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cmd::{self, Command};
use crate::paths::{self, Env};
use crate::protocol::{ClientCtx, Dialect};
use crate::server::State;
use crate::server::dispatch::options::Owner;
use crate::types::{PaneId, SessionId};

/// Who a server says it is (spec D12): grofe, or tmux when started through
/// a `tmux` name or with `compat-identity tmux`. Kept on `Model.identity`,
/// so formats (`#{version}`, TC.3) and tests read it from the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Identity {
    #[default]
    Grofe,
    Tmux,
}

/// The tmux release the compat suite is recorded against (spec D12):
/// `tmux -V`'s number, `TERM_PROGRAM_VERSION` under tmux identity.
pub fn compat_level() -> &'static str {
    "3.7b"
}

/// How the CLI that started this server process was invoked, set once by
/// `main.rs` before the server runs. Unset (a unit test) is grofe.
static STARTED_AS: OnceLock<Identity> = OnceLock::new();

/// Records the starting CLI's identity for [`server_started`]. Only the
/// first call counts.
pub fn set_started_as(identity: Identity) {
    let _ = STARTED_AS.set(identity);
}

/// The config files a CLI run as `tmux -f <file>...` started this server
/// with (`__server --file`, once per file), set by `main.rs` before the
/// server runs.
static START_CONFIG: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Records one `tmux -f <file>` for [`start_config`], in order.
pub fn set_start_config(file: PathBuf) {
    if let Ok(mut files) = START_CONFIG.lock() {
        files.push(file);
    }
}

/// The files `tmux -f` named, in order, or `None` if the server was
/// started without one. As in tmux 3.7b they replace the whole config and
/// each loads (recorded: `-f a -f b` loads both; a missing file is no
/// config and no error; `-f /dev/null` is none), and they bind only the
/// server they start.
pub fn start_config() -> Option<Vec<PathBuf>> {
    let files = START_CONFIG.lock().ok()?;
    (!files.is_empty()).then(|| files.clone())
}

/// Where a tmux-identity server appends its misses, set by
/// [`server_started`] so the parse path, which has no `State`, can log.
static MISS_LOG: OnceLock<PathBuf> = OnceLock::new();

/// Called once in the server, after its config is applied: fixes
/// `state.model.identity` from the starting CLI's `argv[0]` and the
/// `compat-identity` option. A later `set compat-identity` or `reload`
/// does not change it (spec D12: identity is fixed at start).
pub fn server_started(state: &mut State) {
    let started = STARTED_AS.get().copied().unwrap_or_default();
    let option = state
        .model
        .options
        .lookup("compat-identity", &[Owner::Global])
        .unwrap_or("auto");
    state.model.identity = decide_identity(started, option);
    // tmux's `getshell`: the server's `$SHELL`, the passwd shell, `/bin/sh`.
    let shell = crate::server::pane::pick_shell(std::env::var("SHELL").ok());
    state.model.options.set_default("default-shell", shell);
    if state.model.identity == Identity::Tmux {
        let _ = MISS_LOG.set(miss_log_path(&Env::from_process()));
        // tmux's built-in defaults where grofe's differ (FE.1 ruling).
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .ok();
        state.model.options.use_tmux_defaults(editor.as_deref());
        state.registry.key_tables.use_tmux_stock();
    }
}

/// The identity a server takes: `compat-identity` when it names one,
/// otherwise (`auto`) the starting CLI's.
fn decide_identity(started: Identity, option: &str) -> Identity {
    match option {
        "tmux" => Identity::Tmux,
        "grofe" => Identity::Grofe,
        _ => started,
    }
}

/// `$TMUX` as tmux 3.7b sets it: `<socket path>,<server pid>,<session id
/// number>`, recorded against real tmux (`/tmp/tmux-501/x,86444,0`).
pub fn tmux_value(socket: &Path, pid: u32, session: SessionId) -> String {
    format!("{},{pid},{}", socket.display(), session.0)
}

/// Variables a job the server runs for `session` gets (`run-shell`,
/// `if-shell`, popups, status sources), as tmux 3.7b's
/// `environ_for_session` sets them over the environment (recorded):
/// `TMUX` naming the session, or `-1` with none, the panes' `TERM`,
/// `COLORTERM`, and under tmux identity `TERM_PROGRAM=tmux` with the
/// compat level as its version. No `TMUX_PANE`: a job has no pane.
pub fn job_env(state: &State, session: Option<SessionId>) -> Vec<(String, String)> {
    let socket = &state.socket;
    let tmux = match session {
        Some(s) => tmux_value(socket, std::process::id(), s),
        None => format!("{},{},-1", socket.display(), std::process::id()),
    };
    let mut env = vec![
        ("TERM".to_string(), state.inner_term.to_string()),
        ("COLORTERM".to_string(), crate::term::COLORTERM.to_string()),
        ("TMUX".to_string(), tmux),
    ];
    if state.model.identity == Identity::Tmux {
        env.push(("TERM_PROGRAM".to_string(), "tmux".to_string()));
        env.push((
            "TERM_PROGRAM_VERSION".to_string(),
            compat_level().to_string(),
        ));
    }
    env
}

/// Variables a new or respawned pane of `session` gets from the server's
/// identity, applied over the session's environment (spec D12, §4.2):
/// `TMUX` and `TMUX_PANE` under both identities, and under tmux identity
/// `TERM_PROGRAM=tmux` with the compat level as its version, replacing the
/// outer terminal's.
pub fn pane_env(state: &State, session: SessionId, pane: PaneId) -> Vec<(String, String)> {
    let mut env = vec![
        (
            "TMUX".to_string(),
            tmux_value(&state.socket, std::process::id(), session),
        ),
        ("TMUX_PANE".to_string(), pane.to_string()),
    ];
    if state.model.identity == Identity::Tmux {
        env.push(("TERM_PROGRAM".to_string(), "tmux".to_string()));
        env.push((
            "TERM_PROGRAM_VERSION".to_string(),
            compat_level().to_string(),
        ));
    }
    env
}

/// The dialect a closed prompt's command runs in (spec D12). On a grofe
/// server it is always grofe. On a tmux-identity server, text typed at a
/// bare `:` prompt (no template) is tmux's; a template keeps the dialect of
/// the request or binding that opened the prompt, as stored command strings
/// keep theirs (spec §4.5).
pub fn prompt_dialect(state: &State, opener: Dialect, has_template: bool) -> Dialect {
    match state.model.identity {
        Identity::Grofe => Dialect::Grofe,
        Identity::Tmux if !has_template => Dialect::Tmux,
        Identity::Tmux => opener,
    }
}

/// Whether a request's `argv` runs `load-buffer -` (in either dialect), so
/// the CLI sends its stdin with it, as tmux's client reads its own stdin
/// for the server (TM.7).
pub fn loads_stdin(argv: &[String], dialect: Dialect) -> bool {
    cmd::parse::split_argv(argv).iter().any(|words| {
        let Some((verb, rest)) = words.split_first() else {
            return false;
        };
        let verb = match dialect {
            Dialect::Tmux => tmux::resolve(verb).map(|s| s.name).unwrap_or(""),
            Dialect::Grofe => verb.as_str(),
        };
        verb == "load-buffer" && rest.last().is_some_and(|p| p == "-")
    })
}

/// tmux 3.7b's refusal to attach from inside one of its own panes
/// (recorded: exit 1).
pub const TMUX_NESTED: &str = "sessions should be nested with care, unset $TMUX to force";

/// `compat-miss.log` in the state directory, derived as the server log is
/// (`paths::state_dir`), so a test's `XDG_STATE_HOME` keeps it.
pub fn miss_log_path(env: &Env) -> PathBuf {
    paths::state_dir(env).join("compat-miss.log")
}

/// Appends one line for `what` (a verb, flag or format variable the tmux
/// front end does not know) to `compat-miss.log`, on a tmux-identity
/// server only (spec D12). The path is the one [`server_started`] fixed,
/// so nothing but a started tmux-identity server ever writes it. Best
/// effort: a log that cannot be written never fails the request.
pub fn miss_log(_state: &State, what: &str) {
    note_miss(what);
}

/// [`miss_log`] for code that has no `State`: the tmux front end notes a
/// cosmetic flag it ignores while it translates (spec §4.5).
pub fn note_miss(what: &str) {
    if QUIET.get() {
        return;
    }
    if let Some(path) = MISS_LOG.get() {
        append_miss(path, what);
    }
}

thread_local! {
    /// Set while [`quietly`] runs.
    static QUIET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` with [`note_miss`] silent: for a second look at a request
/// that was already parsed (and its misses noted) once.
pub fn quietly<T>(f: impl FnOnce() -> T) -> T {
    let before = QUIET.replace(true);
    let out = f();
    QUIET.set(before);
    out
}

/// The longest `what` a miss line keeps, in bytes.
const MISS_MAX: usize = 256;

/// `compat-miss.log` stops growing past this size.
// ponytail: no rotation; a full log is reviewed and removed by hand.
const MISS_LOG_CAP: u64 = 1 << 20;

/// What a miss line says about a request's argv (strict, so nothing a
/// command carries is ever logged): each command's verb and flag letters
/// only ([`tmux::miss_form`]), never a value, a positional or an error
/// text that could quote one. A `run-shell` script, a `bind-key` body, an
/// `if-shell` branch, a `set` value, a `display` message and `send-keys`
/// keys all stay out.
fn miss_summary(argv: &[String]) -> String {
    cmd::parse::split_argv(argv)
        .iter()
        .map(|command| tmux::miss_form(command))
        .collect::<Vec<_>>()
        .join(" ; ")
}

/// `s` cut to at most `max` bytes, at a character boundary.
fn truncate(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// One `<unix seconds> <what>` line appended to `path` (`what` cut to
/// [`MISS_MAX`] bytes), its directory created private if missing, unless
/// the file is already past [`MISS_LOG_CAP`].
fn append_miss(path: &Path, what: &str) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MISS_LOG_CAP) {
        return;
    }
    if let Some(dir) = path.parent()
        && paths::ensure_private_dir(dir).is_err()
    {
        return;
    }
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let what = what.replace('\n', " ");
    let line = format!("{secs} {}\n", truncate(&what, MISS_MAX));
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// tmux's own socket directories for `uid`, which grofe run as `tmux`
/// never uses (spec D12): `$TMUX_TMPDIR/tmux-<uid>` (`TMUX_TMPDIR`
/// defaulting to `/tmp`) and the default `/tmp/tmux-<uid>`.
pub fn tmux_socket_dirs(tmux_tmpdir: Option<&str>, uid: u32) -> Vec<PathBuf> {
    let name = format!("tmux-{uid}");
    let mut dirs = vec![Path::new("/tmp").join(&name)];
    if let Some(dir) = tmux_tmpdir.filter(|d| !d.is_empty()) {
        dirs.push(Path::new(dir).join(&name));
    }
    dirs
}

/// `path` made absolute, with its deepest existing ancestor canonicalised
/// (symlinks and `..` resolved as far as the filesystem allows).
fn resolved(path: &Path) -> PathBuf {
    let absolute = match path.is_absolute() {
        true => path.to_path_buf(),
        false => std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf()),
    };
    let mut rest = Vec::new();
    let mut at = absolute.as_path();
    loop {
        if let Ok(canon) = std::fs::canonicalize(at) {
            return rest.iter().rev().fold(canon, |p, part| p.join(part));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                at = parent;
            }
            _ => return absolute,
        }
    }
}

/// Refuses a `-S` path inside one of tmux's own socket directories
/// ([`tmux_socket_dirs`]), so grofe run as `tmux` can never take over, or
/// be mistaken for, a real tmux server's socket.
pub fn refuse_tmux_socket(socket: &Path, dirs: &[PathBuf]) -> Result<(), String> {
    let socket = resolved(socket);
    for dir in dirs {
        if socket.starts_with(dir) || socket.starts_with(resolved(dir)) {
            return Err(format!(
                "grofe: refusing -S {}: it is in tmux's socket directory {}; run as tmux, \
                 grofe keeps its sockets in its own directory",
                socket.display(),
                dir.display()
            ));
        }
    }
    Ok(())
}

/// Whether the CLI runs under the name `tmux`: `argv[0]`'s basename is
/// exactly `tmux` (a symlink on `PATH`, spec D1).
pub fn invoked_as_tmux(argv0: &OsStr) -> bool {
    Path::new(argv0)
        .file_name()
        .is_some_and(|name| name == "tmux")
}

/// The commands a request's argv means: split at `;` and parsed natively,
/// or, for a tmux-dialect request, translated by the front end. A request
/// the front end cannot translate is a miss, logged once (the request is
/// parsed once, `dispatch::parse`).
pub fn parse_request(ctx: &ClientCtx, argv: &[String]) -> Result<Vec<Command>, String> {
    match ctx.dialect {
        Dialect::Grofe => cmd::parse_sequence(argv).map_err(|e| e.to_string()),
        Dialect::Tmux => tmux_dispatch(argv, ctx),
    }
}

/// One command whose argv is already split (a binding, the prompt, the
/// mouse), in the dialect it was stored in: `run_binding` sets
/// `ctx.dialect` from the binding (spec §4.5), never from the client that
/// pressed the key.
pub fn parse_split(ctx: &ClientCtx, argv: &[String]) -> Result<Vec<Command>, String> {
    match ctx.dialect {
        Dialect::Grofe => cmd::parse(argv).map(|c| vec![c]).map_err(|e| e.to_string()),
        Dialect::Tmux => logged(argv, tmux::dispatch_command(argv, ctx)),
    }
}

fn tmux_dispatch(argv: &[String], ctx: &ClientCtx) -> Result<Vec<Command>, String> {
    logged(argv, tmux::dispatch(argv, ctx))
}

/// The front end's answer for `argv`, a failure logged as a miss.
fn logged(
    argv: &[String],
    result: Result<Vec<Command>, tmux::TmuxError>,
) -> Result<Vec<Command>, String> {
    result.map_err(|e| {
        if let Some(path) = MISS_LOG.get() {
            append_miss(path, &miss_summary(argv));
        }
        e.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CLI sends its stdin only for a `load-buffer -`, by any tmux
    /// name, anywhere in a `;` chain; natively by its own name only.
    #[test]
    fn only_load_buffer_dash_reads_stdin() {
        let a = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(loads_stdin(&a(&["loadb", "-b", "x", "-"]), Dialect::Tmux));
        assert!(loads_stdin(
            &a(&["ls", ";", "load-buffer", "-"]),
            Dialect::Tmux
        ));
        assert!(loads_stdin(&a(&["load-buffer", "-"]), Dialect::Grofe));
        assert!(!loads_stdin(&a(&["loadb", "-"]), Dialect::Grofe));
        assert!(!loads_stdin(&a(&["load-buffer", "f.txt"]), Dialect::Tmux));
        assert!(!loads_stdin(&a(&["save-buffer", "-"]), Dialect::Tmux));
        assert!(!loads_stdin(&a(&["set-buffer", "-"]), Dialect::Tmux));
    }
    use crate::cmd::{KillServer, ListSessions};

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn ctx(dialect: Dialect) -> ClientCtx {
        ClientCtx {
            dialect,
            ..crate::server::test_ctx()
        }
    }

    /// The identity lives on the model. A unit test never sets
    /// `STARTED_AS`, so a server with no `compat-identity` is grofe.
    #[test]
    fn identity_is_on_the_model_and_starts_as_grofe() {
        let mut state = crate::server::test_state();
        assert_eq!(state.model.identity, Identity::Grofe);
        server_started(&mut state);
        assert_eq!(state.model.identity, Identity::Grofe);
        state.model.identity = Identity::Tmux;
        assert_eq!(state.model.identity, Identity::Tmux);
    }

    #[test]
    fn compat_identity_overrides_the_starting_name_and_auto_keeps_it() {
        use Identity::{Grofe, Tmux};
        assert_eq!(decide_identity(Grofe, "auto"), Grofe);
        assert_eq!(decide_identity(Tmux, "auto"), Tmux);
        assert_eq!(decide_identity(Grofe, "tmux"), Tmux);
        assert_eq!(decide_identity(Tmux, "grofe"), Grofe);
        assert_eq!(decide_identity(Tmux, "tmux"), Tmux);
    }

    /// A `compat-identity grofe` set before start is read by
    /// `server_started` (the `tmux` case is `tests/compat_identity.rs`'s,
    /// since a tmux identity here would log to the real state dir).
    #[test]
    fn server_started_reads_compat_identity() {
        let mut state = crate::server::test_state();
        state.model.identity = Identity::Tmux;
        state
            .model
            .options
            .set(Owner::Global, "compat-identity", "grofe", false)
            .unwrap();
        server_started(&mut state);
        assert_eq!(state.model.identity, Identity::Grofe);
    }

    #[test]
    fn the_compat_level_is_3_7b() {
        assert_eq!(compat_level(), "3.7b");
    }

    /// Recorded against tmux 3.7b: `TMUX=/private/tmp/tmux-502/x,86444,1`
    /// for the pane of session `$1`.
    #[test]
    fn tmux_value_is_socket_pid_and_session_number() {
        assert_eq!(
            tmux_value(Path::new("/run/grofe/default.sock"), 86444, SessionId(1)),
            "/run/grofe/default.sock,86444,1"
        );
    }

    #[test]
    fn panes_get_tmux_under_both_identities_and_term_program_under_tmux() {
        let mut state = crate::server::test_state();
        let tmux = format!("/nonexistent/grofe-test.sock,{},3", std::process::id());
        let grofe = pane_env(&state, SessionId(3), PaneId(7));
        assert_eq!(
            grofe,
            vec![
                ("TMUX".to_string(), tmux.clone()),
                ("TMUX_PANE".to_string(), "%7".to_string()),
            ]
        );
        state.model.identity = Identity::Tmux;
        let env = pane_env(&state, SessionId(3), PaneId(7));
        assert_eq!(env[..2], grofe[..]);
        assert_eq!(
            env[2..],
            [
                ("TERM_PROGRAM".to_string(), "tmux".to_string()),
                ("TERM_PROGRAM_VERSION".to_string(), "3.7b".to_string()),
            ]
        );
    }

    /// D12: prompt text is tmux dialect only on a tmux-identity server; a
    /// tmux opener on a grofe server does not make it so.
    #[test]
    fn only_bare_prompt_text_takes_the_tmux_identity() {
        let mut state = crate::server::test_state();
        let (g, t) = (Dialect::Grofe, Dialect::Tmux);
        for (opener, template) in [(g, false), (g, true), (t, false), (t, true)] {
            assert_eq!(prompt_dialect(&state, opener, template), g);
        }
        state.model.identity = Identity::Tmux;
        assert_eq!(prompt_dialect(&state, g, false), t);
        assert_eq!(prompt_dialect(&state, t, false), t);
        assert_eq!(prompt_dialect(&state, g, true), g);
        assert_eq!(prompt_dialect(&state, t, true), t);
    }

    #[test]
    fn the_miss_log_appends_one_line_per_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grofe").join("compat-miss.log");
        append_miss(&path, "no-such-verb");
        append_miss(&path, "set -Z: two\nlines");
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].ends_with(" no-such-verb"), "{text}");
        assert!(lines[1].ends_with(" set -Z: two lines"), "{text}");
        let env = Env {
            xdg_runtime_dir: None,
            tmpdir: None,
            xdg_state_home: Some(dir.path().display().to_string()),
            home: None,
            uid: 0,
        };
        assert_eq!(miss_log_path(&env), path);
    }

    #[test]
    fn tmux_s_socket_dirs_are_refused_and_others_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let tmpdir = tmp.path().display().to_string();
        let dirs = tmux_socket_dirs(Some(&tmpdir), 501);
        assert_eq!(
            dirs,
            [PathBuf::from("/tmp/tmux-501"), tmp.path().join("tmux-501")]
        );
        let own = tmp.path().join("tmux-501");
        std::fs::create_dir(&own).unwrap();
        for bad in [
            own.join("default"),
            own.join("x/../default"),
            own.join("missing/deeper"),
            PathBuf::from("/tmp/tmux-501/default"),
            PathBuf::from("/tmp/./tmux-501/work"),
        ] {
            assert!(
                refuse_tmux_socket(&bad, &dirs).is_err(),
                "{}",
                bad.display()
            );
        }
        // A symlink into tmux's directory is refused too.
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&own, &link).unwrap();
        assert!(refuse_tmux_socket(&link.join("default"), &dirs).is_err());
        for good in [
            tmp.path().join("grofe.sock"),
            tmp.path().join("tmux-5010/default"),
            PathBuf::from("/tmp/tmux-502/default"),
        ] {
            assert!(
                refuse_tmux_socket(&good, &dirs).is_ok(),
                "{}",
                good.display()
            );
        }
        assert_eq!(tmux_socket_dirs(None, 7), [PathBuf::from("/tmp/tmux-7")]);
    }

    #[test]
    fn a_miss_line_is_capped_at_256_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grofe").join("compat-miss.log");
        append_miss(&path, &"é".repeat(300));
        let text = std::fs::read_to_string(&path).unwrap();
        let (_, what) = text.trim_end().split_once(' ').unwrap();
        assert_eq!(what.len(), 256, "{what}");
    }

    #[test]
    fn the_miss_log_stops_growing_past_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compat-miss.log");
        std::fs::write(&path, vec![b'x'; MISS_LOG_CAP as usize + 1]).unwrap();
        append_miss(&path, "one more");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), MISS_LOG_CAP + 1);
    }

    #[test]
    fn no_argument_value_is_ever_logged() {
        let secret = "hunter2";
        let cases: &[(&[&str], &str)] = &[
            (&["setenv", "-g", "TOKEN", secret], "set-environment -g"),
            (&["send", "-t", "%1", secret, "Enter"], "send-keys -t"),
            (&["send-keys", "--", "-X", secret], "send-keys"),
            (
                &["run-shell", "-b", &format!("curl -H {secret}")],
                "run-shell -b",
            ),
            (&["run", "-d", "1", secret], "run-shell -d"),
            (
                &["bind-key", "-T", "root", "F1", "run-shell", secret],
                "bind-key -T",
            ),
            (
                &["if-shell", "-F", secret, "set @x 1", secret],
                "if-shell -F",
            ),
            (&["set", "-g", "@token", secret], "set-option -g"),
            (&["display", "-p", secret], "display-message -p"),
            (&["new", "-s", secret, "-c", secret], "new-session -s -c"),
            (&["no-such-verb", "-x", secret], "<unknown> -x"),
            (&["no-such-verb", &format!("-p{secret}")], "<unknown> -p"),
            (&[&format!("{secret}!"), "x"], "<unknown>"),
            (&[secret], "<unknown>"),
        ];
        for (words, want) in cases {
            let got = miss_summary(&argv(words));
            assert_eq!(&got, want, "{words:?}");
            assert!(!got.contains(secret), "{got}");
        }
        assert_eq!(
            miss_summary(&argv(&["set", "-g", "@a", secret, ";", "ls", "-F", secret])),
            "set-option -g ; list-sessions -F"
        );
    }

    #[test]
    fn only_the_basename_tmux_is_tmux() {
        for yes in ["tmux", "/usr/local/bin/tmux", "./tmux"] {
            assert!(invoked_as_tmux(OsStr::new(yes)), "{yes}");
        }
        for no in [
            "grofe",
            "/bin/grofe",
            "tmuxx",
            "tmux-3.7b",
            "/tmux/grofe",
            "",
        ] {
            assert!(!invoked_as_tmux(OsStr::new(no)), "{no}");
        }
    }

    #[test]
    fn grofe_requests_parse_natively() {
        let got = parse_request(&ctx(Dialect::Grofe), &argv(&["ls", ";", "kill-server"]));
        assert!(got.is_err(), "ls is not native: {got:?}");
        let got = parse_request(
            &ctx(Dialect::Grofe),
            &argv(&["list-sessions", ";", "kill-server"]),
        );
        assert_eq!(
            got,
            Ok(vec![
                Command::ListSessions(ListSessions::default()),
                Command::KillServer(KillServer),
            ])
        );
        let got = parse_split(&ctx(Dialect::Grofe), &argv(&["kill-server"]));
        assert_eq!(got, Ok(vec![Command::KillServer(KillServer)]));
    }

    /// FE.1: a tmux request is split at `;` as tmux's CLI splits it, then
    /// each command goes through the front end; an already split command
    /// keeps a `;` argument literal.
    #[test]
    fn tmux_requests_go_through_the_front_end() {
        let tmux = ctx(Dialect::Tmux);
        let got = parse_request(&tmux, &argv(&["ls", ";", "kill-server"]));
        assert_eq!(
            got,
            Ok(vec![
                Command::ListSessions(ListSessions::default()),
                Command::KillServer(KillServer),
            ])
        );
        let err = parse_request(&tmux, &argv(&["frobnicate"])).unwrap_err();
        assert_eq!(err, "unknown command: frobnicate");
        let got = parse_split(&tmux, &argv(&["send-keys", ";"])).unwrap();
        let Command::SendKeys(keys) = &got[0] else {
            panic!("{got:?}");
        };
        assert_eq!(keys.items.len(), 1, "{keys:?}");
    }
}
