//! CLI entry, subcommand table.
//!
//! `main` stays synchronous: no tokio runtime exists in the CLI, so it can
//! re-exec a server safely. Only `grofe __server` builds a runtime (spec
//! §4.1).

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use grofe::client;
use grofe::paths::{self, Env};
use grofe::protocol::{ATTACH_CHECK, ClientCtx, Dialect, Hello, Reply, Request, encode_hello};
use grofe::server;
use grofe::types::PaneId;
use nix::fcntl::{FcntlArg, FdFlag, Flock, FlockArg, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

const USAGE: &str = "usage: grofe [-L socket-name] [-S socket-path] <command> [args...]";

/// How long the CLI waits for a freshly started server to report ready.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a connect that found no socket waits before its one retry.
const CONNECT_RETRY: Duration = Duration::from_millis(5);

/// Commands that start a server when none is running (spec §4.1). Every
/// other command fails fast with `no server running on <socket>`.
const AUTO_START: &[&str] = &["new-session", "new", "start-server", "start"];

fn main() -> ExitCode {
    install_panic_report();
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    // Run as `tmux`, the front end reads the argv (spec D1); the transport
    // below is shared, with the dialect the front end chose in every hello.
    // The server re-exec (`__server`) is grofe's own, whatever the name.
    let argv0 = std::env::args_os().next().unwrap_or_default();
    let is_server = args.first().is_some_and(|a| a == "__server");
    let (globals, rest) = if grofe::compat::invoked_as_tmux(&argv0) && !is_server {
        match grofe::compat::tmux::cli(&args) {
            Ok(cli) if cli.version => {
                println!("tmux {}", grofe::compat::compat_level());
                return ExitCode::SUCCESS;
            }
            Ok(cli) => {
                let _ = DIALECT.set(cli.dialect);
                // `-f`: the config of a server this call starts (spec §4.5),
                // each file absolute, since the server does not keep this cwd.
                let cwd = std::env::current_dir().unwrap_or_default();
                let files = cli.files.iter().map(|f| cwd.join(f)).collect();
                let _ = CONFIG_FILES.set(files);
                let globals = Globals {
                    socket: cli.socket,
                    name: cli.name,
                };
                // Never a socket in tmux's own directory (spec D12).
                let dirs = grofe::compat::tmux_socket_dirs(
                    std::env::var("TMUX_TMPDIR").ok().as_deref(),
                    nix::unistd::geteuid().as_raw(),
                );
                if let Err(e) = grofe::compat::refuse_tmux_socket(&globals.socket(), &dirs) {
                    return fail(&e);
                }
                (globals, cli.argv)
            }
            Err(e) => return fail(&e),
        }
    } else {
        // `grofe -V`: grofe's own version, decided by `argv[0]` (spec D12).
        if args.first().is_some_and(|a| a == "-V") {
            println!("grofe {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        match parse_globals(&args) {
            Ok((globals, rest)) => (globals, rest.to_vec()),
            Err(e) => {
                eprintln!("grofe: {e}\n{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    };
    let rest = rest.as_slice();
    let Some(verb) = rest.first() else {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    };

    // The subcommand table: verbs the CLI handles itself. A later task that
    // adds one owns exactly one arm. Everything else is a server command.
    match verb.as_str() {
        "__server" => {}
        "attach-session" | "attach" => report_panics_for(&globals.socket(), "client"),
        _ => report_panics_for(&globals.socket(), "cli"),
    }
    match verb.as_str() {
        "__server" => run_server(&globals, &rest[1..]),
        "start-server" | "start" => {
            let (socket, own_dir) = globals.resolve();
            match connect_or_start(&socket, own_dir, false).and_then(|s| probe(s, &socket)) {
                // tmux runs the rest of a `start-server \; ...` chain (TPM's
                // `start-server \; show-option -gqv @tpm_plugins`); the
                // front end answers `start-server` itself with nothing.
                Ok(()) if dialect() == Dialect::Tmux && rest.len() > 1 => {
                    one_shot(&socket, own_dir, rest)
                }
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(&e),
            }
        }
        "attach-session" | "attach" => attach(&globals.socket(), rest),
        // File layers only: never starts or contacts a server (R8).
        "show-config" => grofe::config::show_config(&rest[1..]),
        // Reads a tmux.conf, no server anywhere (spec 5.5).
        "import-tmux" => grofe::import_tmux::run(&rest[1..]),
        "new-session" | "new" if attaches(rest) => new_and_attach(&globals, rest),
        _ => {
            let (socket, own_dir) = globals.resolve();
            one_shot(&socket, own_dir, rest)
        }
    }
}

/// The dialect every hello this process sends carries: grofe, unless the
/// tmux front end chose otherwise.
static DIALECT: OnceLock<Dialect> = OnceLock::new();

/// `tmux -f <file>...`: passed to a server this process starts.
static CONFIG_FILES: OnceLock<Vec<PathBuf>> = OnceLock::new();

fn dialect() -> Dialect {
    DIALECT.get().copied().unwrap_or_default()
}

/// The instance a panic report is written for, and the process's role
/// (`server`, the attach `client`, or a one-shot `cli` call), once the socket is known.
static PANIC_TARGET: OnceLock<(PathBuf, &'static str)> = OnceLock::new();

fn report_panics_for(socket: &Path, role: &'static str) {
    let _ = PANIC_TARGET.set((socket.to_path_buf(), role));
}

/// Installed first thing: a panic writes a report beside the instance's
/// server log (R22), then runs the previous hook. The attach client installs
/// its own hook later (T2.1), which restores the tty and then calls this one.
fn install_panic_report() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some((socket, role)) = PANIC_TARGET.get() {
            match write_panic_report(socket, role, info) {
                Ok(path) => eprintln!("grofe: panic report written to {}", path.display()),
                Err(e) => eprintln!("grofe: cannot write a panic report: {e}"),
            }
        }
        previous(info);
    }));
}

/// `<state dir>/grofe/<instance>.panic.<unix-ms>.log`, derived from the
/// server log's path so a test's `-S` socket keeps it in the test's state
/// dir.
fn panic_report_path(log: &Path, unix_ms: u128) -> PathBuf {
    let instance = log
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "grofe".to_string());
    log.with_file_name(format!("{instance}.panic.{unix_ms}.log"))
}

fn write_panic_report(
    socket: &Path,
    role: &str,
    info: &std::panic::PanicHookInfo<'_>,
) -> io::Result<PathBuf> {
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let log = paths::log_path(&Env::from_process(), socket);
    if let Some(dir) = log.parent() {
        paths::ensure_private_dir(dir)?;
    }
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(non-string panic payload)");
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "unknown".to_string());
    let thread = std::thread::current();
    let report = format!(
        "grofe panic report\n\
         version: {}\n\
         os: {} {}\n\
         role: {role}\n\
         pid: {}\n\
         socket: {}\n\
         thread: {}\n\
         location: {location}\n\
         message: {message}\n\
         backtrace:\n{}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::process::id(),
        socket.display(),
        thread.name().unwrap_or("<unnamed>"),
        std::backtrace::Backtrace::force_capture(),
    );
    let (mut file, path) = create_report(&log, unix_ms)?;
    file.write_all(report.as_bytes())?;
    Ok(path)
}

/// How many later milliseconds [`create_report`] tries when the name is
/// taken.
const REPORT_NAME_TRIES: u128 = 1000;

/// Creates a new report file, never reusing one: when two panics (two
/// threads, or a server and a client of the same instance) land in the
/// same millisecond, the later one takes the next free millisecond, so the
/// name keeps R22's `<instance>.panic.<unix-ms>.log` shape.
fn create_report(log: &Path, unix_ms: u128) -> io::Result<(File, PathBuf)> {
    let mut last = io::Error::from(io::ErrorKind::AlreadyExists);
    for ms in unix_ms..unix_ms + REPORT_NAME_TRIES {
        let path = panic_report_path(log, ms);
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

/// `-L` / `-S`, which come before the command, as in tmux.
struct Globals {
    socket: Option<PathBuf>,
    name: Option<String>,
}

impl Globals {
    /// `-S` wins, then `-L`, then `$GROFE` (set inside panes), then the
    /// default socket.
    fn socket(&self) -> PathBuf {
        self.resolve().0
    }

    /// The socket path, and whether its directory is grofe's own (the
    /// default or `-L` location) rather than one the user named with `-S`
    /// or `$GROFE`. Only grofe's own directory is verified private, as tmux
    /// does not police `-S` parents.
    fn resolve(&self) -> (PathBuf, bool) {
        if self.socket.is_none()
            && self.name.is_none()
            && let Some(s) = std::env::var_os("GROFE").filter(|s| !s.is_empty())
        {
            return (PathBuf::from(s), false);
        }
        let env = Env::from_process();
        let path = paths::socket_path(&env, self.socket.as_deref(), self.name.as_deref());
        (path, self.socket.is_none())
    }
}

fn parse_globals(args: &[String]) -> Result<(Globals, &[String]), String> {
    let mut globals = Globals {
        socket: None,
        name: None,
    };
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        match arg.as_str() {
            "-S" | "-L" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| format!("option {arg} requires an argument"))?;
                if arg == "-S" {
                    globals.socket = Some(PathBuf::from(value));
                } else {
                    globals.name = Some(value.clone());
                }
                i += 2;
            }
            "--" => {
                i += 1;
                break;
            }
            s if s.starts_with("-S") => {
                globals.socket = Some(PathBuf::from(&s[2..]));
                i += 1;
            }
            s if s.starts_with("-L") => {
                globals.name = Some(s[2..].to_string());
                i += 1;
            }
            s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option: {s}")),
            _ => break,
        }
    }
    Ok((globals, &args[i..]))
}

/// `grofe __server -S <sock> --ready-fd <n> [--exit-when-empty]
/// [--identity tmux] [--file <path>]`: the detached server process.
/// `--identity tmux` says the CLI that started it ran as `tmux` (spec
/// D12); `--file` is that CLI's `tmux -f`.
fn run_server(globals: &Globals, args: &[String]) -> ExitCode {
    let mut socket = globals.socket.clone();
    let mut ready_fd = None;
    let mut exit_when_empty = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--exit-when-empty" {
            exit_when_empty = true;
            continue;
        }
        match (arg.as_str(), it.next()) {
            ("-S", Some(path)) => socket = Some(PathBuf::from(path)),
            ("--identity", Some(name)) if name == "tmux" => {
                grofe::compat::set_started_as(grofe::compat::Identity::Tmux)
            }
            ("--file", Some(path)) => grofe::compat::set_start_config(PathBuf::from(path)),
            ("--ready-fd", Some(fd)) => match fd.parse() {
                Ok(fd) => ready_fd = Some(fd),
                Err(_) => return fail(&format!("bad --ready-fd: {fd}")),
            },
            _ => return fail(&format!("__server: unexpected argument {arg}")),
        }
    }
    let socket = socket.unwrap_or_else(|| globals.socket());
    report_panics_for(&socket, "server");
    match server::run_daemon(&socket, ready_fd, exit_when_empty) {
        0 => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("{}", msg.trim_end_matches('\n'));
    ExitCode::FAILURE
}

/// Whether `argv` is a `new-session` that attaches (no `-d`). One that does
/// not parse goes to the server as usual, which reports the error.
fn attaches(argv: &[String]) -> bool {
    matches!(
        grofe::cmd::parse(argv),
        Ok(grofe::cmd::Command::NewSession(c)) if !c.detached
    )
}

/// `new-session` without `-d`, as tmux 3.7b: without a terminal it fails
/// before creating anything; otherwise the session is created detached,
/// its id read back in the same request, and the client attaches to it
/// through [`attach`] (which refuses self-nesting and restores the tty).
///
/// The request carries no pane, even from inside one of this server's
/// panes, so the trailing `display-message` names the session just made
/// (the one with the latest activity) rather than the pane's; the new
/// session's cwd is then the CLI's own, as in tmux.
fn new_and_attach(globals: &Globals, argv: &[String]) -> ExitCode {
    if !io::stdin().is_terminal() {
        return fail("open terminal failed: not a terminal");
    }
    let (socket, own_dir) = globals.resolve();
    let mut create = vec![argv[0].clone(), "-d".to_string()];
    create.extend(argv[1..].iter().cloned());
    create.extend([";", "display-message", "-p", "#{session_id}"].map(String::from));
    let hello = match hello_for(&socket, false) {
        Ok(h) => h,
        Err(e) => return fail(&e),
    };
    let id = match connect_or_start(&socket, own_dir, true)
        .and_then(|stream| request_with(stream, &hello, &create))
    {
        Ok(Reply::Ok { out, .. }) => out.trim().to_string(),
        Ok(Reply::Err { err, .. }) => return fail(&err),
        Ok(Reply::Event(_)) => return fail("unexpected event reply"),
        Err(e) => return fail(&e),
    };
    let argv = ["attach-session".to_string(), "-t".to_string(), id];
    match attach_to(&socket, &argv) {
        Ok(reason) => {
            println!("[{reason}]");
            ExitCode::SUCCESS
        }
        // The session was made and is already gone (its command exited at
        // once, and an auto-started server with it): tmux 3.7b says
        // `[exited]` and exits 0.
        Err(e) if session_gone(&e) => {
            println!("[exited]");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

/// Whether an attach error means the session (or its server) went away
/// between being created and being attached to.
fn session_gone(err: &str) -> bool {
    err == "no sessions"
        || err.starts_with("no server running")
        || err.starts_with("can't find session")
        || err.contains("Broken pipe")
        || err.contains("Connection reset")
        || err.starts_with("server closed the connection")
}

/// Sends `argv` as one control-mode request and exits with its status.
fn one_shot(socket: &Path, own_dir: bool, argv: &[String]) -> ExitCode {
    if let Err(e) = control_hello(socket) {
        return fail(&e);
    }
    let stream = if AUTO_START.contains(&argv[0].as_str()) {
        // Implicit start: that server exits if it ends up with no sessions.
        connect_or_start(socket, own_dir, true)
    } else {
        connect(socket)
    };
    let stream = match stream {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    match request(stream, socket, argv) {
        Ok(Reply::Ok { out, .. }) => {
            let mut stdout = io::stdout().lock();
            let _ = stdout.write_all(out.as_bytes());
            let _ = stdout.flush();
            ExitCode::SUCCESS
        }
        Ok(Reply::Err {
            err, out, status, ..
        }) => {
            // A failing `run-shell` prints its output (and tmux's message)
            // on stdout and exits with its own status, as tmux 3.7b (D20).
            let mut stdout = io::stdout().lock();
            let _ = stdout.write_all(out.as_bytes());
            let _ = stdout.flush();
            if !err.is_empty() {
                eprintln!("{}", err.trim_end_matches('\n'));
            }
            let status = status.and_then(|s| u8::try_from(s).ok()).unwrap_or(1);
            ExitCode::from(status.max(1))
        }
        Ok(Reply::Event(_)) => fail("unexpected event reply"),
        Err(e) => fail(&e),
    }
}

/// `attach-session`: the attach client (spec §4.6), in this process. It
/// never starts a server (tmux's does, only to find no sessions, which is
/// what grofe then says). Self-nesting is the server's call
/// (it knows which session holds `$GROFE_PANE`); the client only sends
/// the pane and `--nest`.
fn attach(socket: &Path, argv: &[String]) -> ExitCode {
    match attach_to(socket, argv) {
        Ok(reason) => {
            println!("[{reason}]");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

/// [`attach`]'s work: the reason the client was let go, or the error.
fn attach_to(socket: &Path, argv: &[String]) -> Result<String, String> {
    let parsed = match grofe::cmd::parse(argv) {
        Ok(grofe::cmd::Command::AttachSession(a)) => a,
        Ok(_) => return Err("attach-session: unexpected command".to_string()),
        Err(e) => return Err(e.to_string()),
    };
    control_hello(socket)?;
    // tmux's attach starts a server, which finds no sessions; grofe's does
    // not start one and says what tmux would.
    let connect_attach = || match connect(socket) {
        Err(e) if e.starts_with("no server running") => Err("no sessions".to_string()),
        other => other,
    };
    // The target first (protocol::ATTACH_CHECK): a refusal never probes
    // the terminal.
    let check: Vec<String> = std::iter::once(ATTACH_CHECK.to_string())
        .chain(argv[1..].iter().cloned())
        .collect();
    match connect_attach().and_then(|s| request(s, socket, &check)) {
        Ok(Reply::Ok { .. }) => {}
        Ok(Reply::Err { err, .. }) => return Err(err),
        Ok(Reply::Event(_)) => return Err("unexpected event reply".to_string()),
        Err(e) => return Err(e),
    }
    let stream = connect_attach()?;
    let ctx = ClientCtx {
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        pane: client_pane(
            std::env::var_os("GROFE").as_deref(),
            std::env::var("GROFE_PANE").ok().as_deref(),
            socket,
        ),
        env: std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect(),
        client: None,
        dialect: dialect(),
    };
    let opts = client::AttachOptions {
        target: client::raw_target(&argv[1..]),
        detach_others: parsed.detach_others,
        nest: parsed.nest,
    };
    let term = std::env::var("TERM").unwrap_or_else(|_| "unknown".to_string());
    client::attach(stream, ctx, opts, term)
}

/// How long [`probe`] waits for an answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Checks that `stream` is a grofe server by asking it `list-sessions` in
/// grofe's dialect: `start-server` must not report success on a socket
/// that some other program (a real tmux) listens on.
fn probe(stream: UnixStream, socket: &Path) -> Result<(), String> {
    let not_grofe = |e: String| format!("{} is not a grofe server: {e}", socket.display());
    stream
        .set_read_timeout(Some(PROBE_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(PROBE_TIMEOUT)))
        .map_err(|e| not_grofe(e.to_string()))?;
    let ctx = ClientCtx {
        cwd: PathBuf::from("/"),
        pane: None,
        env: Vec::new(),
        client: None,
        dialect: Dialect::Grofe,
    };
    let hello = encode_hello(&Hello::Control(ctx)).map_err(|e| e.to_string())?;
    match request_with(stream, &hello, &["list-sessions".to_string()]) {
        Ok(Reply::Ok { .. } | Reply::Err { .. }) => Ok(()),
        Ok(Reply::Event(_)) => Err(not_grofe("unexpected event reply".to_string())),
        Err(e) => Err(not_grofe(e)),
    }
}

/// Connects to a running server, or explains that there is none.
///
/// A missing socket is retried once after a short wait: an auto-started
/// server deciding whether to exit moves its socket aside for a moment and
/// may put it back (see `server::exit_if_empty`).
fn connect(socket: &Path) -> Result<UnixStream, String> {
    let result = UnixStream::connect(socket).or_else(|e| {
        if e.kind() != io::ErrorKind::NotFound {
            return Err(e);
        }
        std::thread::sleep(CONNECT_RETRY);
        UnixStream::connect(socket)
    });
    result.map_err(|e| match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            format!("no server running on {}", socket.display())
        }
        _ => format!("error connecting to {}: {e}", socket.display()),
    })
}

/// Connects, starting a server first if none runs (spec §4.1): under a
/// flock on `<sock>.lock`, re-check; a socket that refuses connection is
/// stale and removed; then re-exec `grofe __server` and wait for readiness.
///
/// `own_dir` (the default or `-L` location): the socket directory is always
/// created or verified private, before any connect, so a pre-created
/// world-writable `/tmp/grofe-$UID` is refused. For a user-named `-S` path
/// the parent is only created when missing.
///
/// `exit_when_empty` marks a server started by a session-creating command
/// rather than `start-server`: it exits if it ends up with no sessions.
fn connect_or_start(
    socket: &Path,
    own_dir: bool,
    exit_when_empty: bool,
) -> Result<UnixStream, String> {
    if let Some(dir) = socket.parent().filter(|d| !d.as_os_str().is_empty())
        && (own_dir || !dir.exists())
    {
        paths::ensure_private_dir(dir)
            .map_err(|e| format!("socket directory {} is not usable: {e}", dir.display()))?;
    }
    if let Ok(stream) = UnixStream::connect(socket) {
        return Ok(stream);
    }
    let lock_path = paths::lock_path(socket);
    // Run as `tmux`, a socket grofe did not create (every one it creates
    // has had its lock file beside it) is never replaced: it may be another
    // program's, a real tmux's above all. Checked before the lock file is
    // made, so a refusal leaves nothing behind.
    if dialect() == Dialect::Tmux
        && !lock_path.exists()
        && std::fs::symlink_metadata(socket).is_ok()
    {
        return Err(format!(
            "refusing to replace {}: grofe did not create it",
            socket.display()
        ));
    }
    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| format!("cannot open {}: {e}", lock_path.display()))?;
    let _lock = Flock::lock(lock_file, FlockArg::LockExclusive)
        .map_err(|(_, e)| format!("cannot lock {}: {e}", lock_path.display()))?;

    match UnixStream::connect(socket) {
        Ok(stream) => return Ok(stream),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            // Stale: the file exists but nobody listens.
            std::fs::remove_file(socket)
                .map_err(|e| format!("cannot remove stale {}: {e}", socket.display()))?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("error connecting to {}: {e}", socket.display())),
    }
    start_server(socket, exit_when_empty)?;
    connect(socket)
}

/// Re-execs this binary as a detached `grofe __server` and waits for it to
/// report ready over an inherited pipe. Called with the start lock held.
fn start_server(socket: &Path, exit_when_empty: bool) -> Result<(), String> {
    let (read_end, write_end) = nix::unistd::pipe().map_err(|e| format!("pipe: {e}"))?;
    fcntl(&read_end, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(|e| format!("fcntl: {e}"))?;
    // The write end must survive the child's stdio setup, which owns 0-2.
    let write_end = if write_end.as_raw_fd() <= 2 {
        let fd = fcntl(&write_end, FcntlArg::F_DUPFD(3)).map_err(|e| format!("fcntl: {e}"))?;
        // The original must close too, or a server that dies at start-up
        // leaves a writer open and we wait out the whole timeout.
        drop(write_end);
        // SAFETY: F_DUPFD just returned this new fd; we are its only owner.
        unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) }
    } else {
        write_end
    };

    let exe = std::env::current_exe().map_err(|e| format!("cannot find grofe binary: {e}"))?;
    let mut cmd = Command::new(exe);
    cmd.arg("__server")
        .arg("-S")
        .arg(socket)
        .arg("--ready-fd")
        .arg(write_end.as_raw_fd().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if exit_when_empty {
        cmd.arg("--exit-when-empty");
    }
    // The identity is the starting CLI's (spec D12): only a CLI run as
    // `tmux` sends a tmux dialect.
    if dialect() == Dialect::Tmux {
        cmd.args(["--identity", "tmux"]);
    }
    for file in CONFIG_FILES.get().into_iter().flatten() {
        cmd.arg("--file").arg(file);
    }
    // SAFETY: setsid is async-signal-safe; nothing else runs between fork
    // and exec. It detaches the server from our session and terminal.
    unsafe {
        cmd.pre_exec(|| nix::unistd::setsid().map(drop).map_err(io::Error::from));
    }
    cmd.spawn()
        .map_err(|e| format!("cannot start server: {e}"))?;
    // Our copy must go, or the pipe never reaches EOF if the server dies.
    drop(write_end);

    let log = paths::log_path(&Env::from_process(), socket);
    match wait_ready(File::from(read_end), READY_TIMEOUT) {
        Ok(msg) if msg.trim() == "ok" => Ok(()),
        Ok(msg) if msg.trim().is_empty() => Err(format!(
            "server exited during start-up; see {}",
            log.display()
        )),
        Ok(msg) => Err(format!("server failed to start: {}", msg.trim())),
        Err(e) => Err(format!("{e}; see {}", log.display())),
    }
}

/// Reads the readiness pipe to EOF, bounded by `timeout`.
fn wait_ready(mut pipe: File, timeout: Duration) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ms = u16::try_from(left.as_millis()).unwrap_or(u16::MAX);
        if ms == 0 {
            return Err(format!("server not ready within {timeout:?}"));
        }
        let mut fds = [PollFd::new(pipe.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::from(ms)) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(format!("poll: {e}")),
        }
        let mut chunk = [0u8; 256];
        match pipe.read(&mut chunk) {
            Ok(0) => return Ok(String::from_utf8_lossy(&buf).into_owned()),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("reading readiness pipe: {e}")),
        }
    }
}

/// The pane id to send in this request's [`ClientCtx`] (spec §4.2, §4.6).
///
/// `$GROFE_PANE` is only meaningful for the server that set it: from inside
/// a pane of server A, `grofe -L other ...` or `grofe -S /x.sock ...`
/// targets a different server B, and A's `%N` must not be sent to B, which
/// could resolve it to an unrelated pane there (tmux avoids the same
/// mistake by checking `$TMUX`'s socket against the server it talks to).
/// So `grofe` (`$GROFE`) is trusted only when it identifies the same
/// socket as `target`, compared by canonical parent directory plus file
/// name rather than as raw strings (`paths::same_socket`), and an
/// unparsable `grofe_pane` is ignored either way.
fn client_pane(grofe: Option<&OsStr>, grofe_pane: Option<&str>, target: &Path) -> Option<PaneId> {
    let grofe = grofe?;
    if !paths::same_socket(Path::new(grofe), target) {
        return None;
    }
    grofe_pane?.parse().ok()
}

/// Sends the control hello and one request, and reads until its reply.
///
/// `target` is the socket this request is being sent to, used to decide
/// whether `$GROFE_PANE` applies (see [`client_pane`]).
fn request(stream: UnixStream, target: &Path, argv: &[String]) -> Result<Reply, String> {
    request_with(stream, &control_hello(target)?, argv)
}

/// Sends `hello` and one request, and reads until its reply.
fn request_with(stream: UnixStream, hello: &str, argv: &[String]) -> Result<Reply, String> {
    let req = serde_json::to_string(&Request {
        id: 1,
        argv: argv.to_vec(),
    })
    .map_err(|e| e.to_string())?;
    let mut writer = &stream;
    writer
        .write_all(format!("{hello}\n{req}\n").as_bytes())
        .map_err(|e| format!("sending request: {e}"))?;
    read_reply(&stream)
}

/// The control hello line for a request to `target`, or why it cannot be
/// sent (an environment too large for it), so that is known before
/// connecting.
fn control_hello(target: &Path) -> Result<String, String> {
    hello_for(target, true)
}

/// [`control_hello`], sending this CLI's pane only when `with_pane`.
fn hello_for(target: &Path, with_pane: bool) -> Result<String, String> {
    let pane = client_pane(
        std::env::var_os("GROFE").as_deref(),
        std::env::var("GROFE_PANE").ok().as_deref(),
        target,
    );
    let ctx = ClientCtx {
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        pane: pane.filter(|_| with_pane),
        env: std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect(),
        client: None,
        dialect: dialect(),
    };
    encode_hello(&Hello::Control(ctx)).map_err(|e| e.to_string())
}

/// Reads until the reply to the one request sent on `stream`.
fn read_reply(stream: &UnixStream) -> Result<Reply, String> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Err("server closed the connection without replying".to_string()),
            Ok(_) => {}
            Err(e) => return Err(format!("reading reply: {e}")),
        }
        match serde_json::from_str::<Reply>(&line) {
            Ok(Reply::Event(_)) => continue,
            Ok(reply) => return Ok(reply),
            Err(e) => return Err(format!("bad reply from server: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_taken_report_name_moves_to_the_next_millisecond() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("t.log");
        let (_, first) = create_report(&log, 42).unwrap();
        let (_, second) = create_report(&log, 42).unwrap();
        assert_eq!(first, tmp.path().join("t.panic.42.log"));
        assert_eq!(second, tmp.path().join("t.panic.43.log"));
    }

    #[test]
    fn client_pane_matches_the_same_socket() {
        let socket = Path::new("/run/user/501/grofe/default.sock");
        assert_eq!(
            client_pane(
                Some(OsStr::new("/run/user/501/grofe/default.sock")),
                Some("%3"),
                socket
            ),
            Some(PaneId(3))
        );
    }

    #[test]
    fn client_pane_matches_via_a_symlinked_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();

        let via_link = link.join("default.sock");
        let via_real = real_dir.join("default.sock");
        assert_eq!(
            client_pane(Some(via_link.as_os_str()), Some("%5"), &via_real),
            Some(PaneId(5))
        );
    }

    #[test]
    fn client_pane_ignores_a_different_socket() {
        let target = Path::new("/run/user/501/grofe/other.sock");
        assert_eq!(
            client_pane(
                Some(OsStr::new("/run/user/501/grofe/default.sock")),
                Some("%3"),
                target
            ),
            None
        );
    }

    #[test]
    fn client_pane_is_none_when_grofe_is_unset() {
        let target = Path::new("/run/user/501/grofe/default.sock");
        assert_eq!(client_pane(None, Some("%3"), target), None);
    }

    #[test]
    fn client_pane_ignores_garbage_grofe_pane() {
        let socket = Path::new("/run/user/501/grofe/default.sock");
        assert_eq!(
            client_pane(
                Some(OsStr::new("/run/user/501/grofe/default.sock")),
                Some("garbage"),
                socket
            ),
            None
        );
        assert_eq!(
            client_pane(
                Some(OsStr::new("/run/user/501/grofe/default.sock")),
                None,
                socket
            ),
            None
        );
    }
}
