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
    if state.model.identity == Identity::Tmux {
        let _ = MISS_LOG.set(miss_log_path(&Env::from_process()));
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

/// The dialect a closed prompt's command runs in (spec D12): the dialect of
/// the request that opened it, except that text typed at a bare `:` prompt
/// (no template) on a tmux-identity server is tmux's.
pub fn prompt_dialect(state: &State, opener: Dialect, has_template: bool) -> Dialect {
    if opener == Dialect::Tmux || (state.model.identity == Identity::Tmux && !has_template) {
        Dialect::Tmux
    } else {
        Dialect::Grofe
    }
}

/// tmux 3.7b's refusal to attach from inside one of its own panes
/// (recorded: exit 1).
pub const TMUX_NESTED: &str = "sessions should be nested with care, unset $TMUX to force";

/// An attach error as the CLI prints it: grofe's self-nesting refusal
/// becomes tmux's for a tmux-dialect request (spec §4.6), every other
/// error is kept.
pub fn attach_error(dialect: Dialect, err: String) -> String {
    let self_nest =
        err.starts_with("refusing to attach session ") && err.contains(" from inside itself");
    if dialect == Dialect::Tmux && self_nest {
        TMUX_NESTED.to_string()
    } else {
        err
    }
}

/// `compat-miss.log` in the state directory, derived as the server log is
/// (`paths::state_dir`), so a test's `XDG_STATE_HOME` keeps it.
pub fn miss_log_path(env: &Env) -> PathBuf {
    paths::state_dir(env).join("compat-miss.log")
}

/// Appends one line for `what` (a verb, flag or format variable the tmux
/// front end does not know) to `compat-miss.log`, on a tmux-identity
/// server only (spec D12). Best effort: a log that cannot be written never
/// fails the request.
pub fn miss_log(state: &State, what: &str) {
    if state.model.identity == Identity::Tmux {
        append_miss(&miss_log_path(&Env::from_process()), what);
    }
}

/// [`miss_log`] for the parse path, which has no `State`: logs only once
/// [`server_started`] fixed a tmux identity.
fn log_miss(what: &str) {
    if let Some(path) = MISS_LOG.get() {
        append_miss(path, what);
    }
}

/// One `<unix seconds> <what>` line appended to `path`, its directory
/// created private if missing.
fn append_miss(path: &Path, what: &str) {
    if let Some(dir) = path.parent()
        && paths::ensure_private_dir(dir).is_err()
    {
        return;
    }
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("{secs} {}\n", what.replace('\n', " "));
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
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
        Dialect::Tmux => tmux_dispatch(argv, ctx),
    }
}

fn tmux_dispatch(argv: &[String], ctx: &ClientCtx) -> Result<Vec<Command>, String> {
    tmux::dispatch(argv, ctx).map_err(|e| {
        log_miss(&format!("{}: {e}", argv.join(" ")));
        e.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn only_bare_prompt_text_takes_the_tmux_identity() {
        let mut state = crate::server::test_state();
        let (g, t) = (Dialect::Grofe, Dialect::Tmux);
        assert_eq!(prompt_dialect(&state, g, false), g);
        assert_eq!(prompt_dialect(&state, g, true), g);
        assert_eq!(prompt_dialect(&state, t, true), t);
        state.model.identity = Identity::Tmux;
        assert_eq!(prompt_dialect(&state, g, false), t);
        assert_eq!(prompt_dialect(&state, g, true), g);
        assert_eq!(prompt_dialect(&state, t, true), t);
    }

    #[test]
    fn a_tmux_attach_says_tmux_s_self_nesting_refusal() {
        let grofe = "refusing to attach session main from inside itself (use --nest to force)";
        assert_eq!(attach_error(Dialect::Grofe, grofe.to_string()), grofe);
        assert_eq!(attach_error(Dialect::Tmux, grofe.to_string()), TMUX_NESTED);
        assert_eq!(
            attach_error(Dialect::Tmux, "can't find session: x".to_string()),
            "can't find session: x"
        );
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
    fn grofe_requests_parse_natively_and_tmux_ones_are_refused() {
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
        let err = parse_request(&ctx(Dialect::Tmux), &argv(&["list-sessions"])).unwrap_err();
        assert_eq!(err, "tmux front end is not implemented: list-sessions");
        let got = parse_split(&ctx(Dialect::Grofe), &argv(&["kill-server"]));
        assert_eq!(got, Ok(vec![Command::KillServer(KillServer)]));
        let err = parse_split(&ctx(Dialect::Tmux), &argv(&["kill-server"])).unwrap_err();
        assert_eq!(err, "tmux front end is not implemented: kill-server");
    }
}
