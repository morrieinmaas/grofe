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

mod layout_glue;

use grofe::client;
use grofe::paths::{self, Env};
use grofe::protocol::{ATTACH_CHECK, ClientCtx, Dialect, Hello, Reply, Request, encode_hello};
use grofe::server;
use grofe::types::PaneId;
use nix::fcntl::{FcntlArg, FdFlag, Flock, FlockArg, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

const USAGE: &str = "usage: grofe [-L socket-name] [-S socket-path] <command> [args...]";

/// `grofe --help`: the global options and the commands most sessions use.
/// Every other tmux command name works too.
const HELP: &str = "\
usage: grofe [-L socket-name] [-S socket-path] <command> [args...]

Options:
  -L socket-name   use the named server socket in grofe's socket directory
  -S socket-path   use the server socket at this path
  -V, --version    print grofe's version and exit
  -h, --help       print this help and exit

Common commands:
  new-session (new)         start a session (and a server when none runs)
  attach-session (attach)   attach to a session
  list-sessions             list sessions
  kill-session              kill a session
  new-window (neww)         open a window
  split-window (splitw)     split a pane
  send-keys (send)          send keys to a pane
  capture-pane (capturep)   print a pane's contents
  display-message (display) print a message or format
  source-file (source)      run a config file's commands
  kill-server               stop the server and every session
  show-config               show the merged config and where each value came from
  import-tmux               convert a tmux.conf to grofe's config
  layout                    bring up, save, list and pick layouts
  control                   a stdio bridge to the server's protocol

Commands take tmux's names and flags; `;` chains commands, as in tmux.
";

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
            // tmux's `usage(0)` prints to stdout.
            Ok(cli) if cli.help => {
                println!("{}", grofe::compat::tmux::USAGE);
                return ExitCode::SUCCESS;
            }
            Ok(cli) => {
                let _ = DIALECT.set(cli.dialect);
                let _ = NO_START.set(cli.no_start);
                let _ = LOGIN.set(cli.login);
                let _ = FEATURES.set(cli.features.clone());
                // `-f`: the config of a server this call starts (spec §4.5),
                // each file absolute, since the server does not keep this cwd.
                let cwd = std::env::current_dir().unwrap_or_default();
                let files = cli.files.iter().map(|f| cwd.join(f)).collect();
                let _ = CONFIG_FILES.set(files);
                let globals = Globals {
                    socket: cli.socket,
                    name: cli.name,
                    version: false,
                    help: false,
                };
                // Never a socket in tmux's own directory (spec D12).
                let dirs = grofe::compat::tmux_socket_dirs(
                    std::env::var("TMUX_TMPDIR").ok().as_deref(),
                    nix::unistd::geteuid().as_raw(),
                );
                if let Err(e) = grofe::compat::refuse_tmux_socket(&globals.socket(), &dirs) {
                    return fail(&e);
                }
                if let Some(command) = &cli.shell_command {
                    return shell_command(&globals, command);
                }
                if cli.foreground
                    && let Some(code) = run_foreground(&globals)
                {
                    return code;
                }
                (globals, cli.argv)
            }
            Err(e) => return fail(&e),
        }
    } else {
        match parse_globals(&args) {
            // `grofe -V`: grofe's own version, decided by `argv[0]` (spec
            // D12); under the tmux name `-V` and `-h` are tmux's, above.
            Ok((globals, _)) if globals.version => {
                println!("grofe {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            Ok((globals, _)) if globals.help => {
                print!("{HELP}");
                return ExitCode::SUCCESS;
            }
            Ok((globals, rest)) => (globals, rest.to_vec()),
            Err(e) => {
                eprintln!("grofe: {e}\n{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    };
    // Bare `grofe`, like bare `tmux`, means `new-session`.
    let rest = if rest.is_empty() {
        vec!["new-session".to_string()]
    } else {
        rest
    };
    let rest = rest.as_slice();
    let verb = &rest[0];

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
            // `start-server --restore` (spec §6): the server this starts
            // restores the snapshot whatever the config says. A restore
            // goes only into an empty server, and only at its start.
            let restore = rest.get(1).is_some_and(|a| a == "--restore");
            let rest: Vec<String> = rest
                .iter()
                .enumerate()
                .filter(|(i, _)| !restore || *i != 1)
                .map(|(_, a)| a.clone())
                .collect();
            let rest = rest.as_slice();
            if restore {
                if let Ok(stream) = UnixStream::connect(&socket) {
                    let argv = ["list-sessions", "-F", "#{session_name}"].map(String::from);
                    return match request(stream, &socket, &argv) {
                        Ok(Reply::Ok { out, .. }) if !out.trim().is_empty() => {
                            fail(server::restore::NOT_EMPTY)
                        }
                        // ponytail: an empty running server cannot be asked
                        // to restore (no verb for it); the restore runs
                        // only in a server this command starts.
                        Ok(_) => fail(&format!(
                            "a server is already running on {}: restore runs only at server start",
                            socket.display()
                        )),
                        Err(e) => fail(&e),
                    };
                }
                let snapshot = paths::snapshot_path(&Env::from_process(), &socket);
                if snapshot.exists() {
                    // SAFETY: the CLI has started no thread yet, so nothing
                    // reads the environment concurrently. The server it
                    // starts inherits the variable.
                    unsafe { std::env::set_var(server::restore::RESTORE_ENV, "1") };
                } else {
                    eprintln!(
                        "grofe: no snapshot to restore at {}; starting an empty server",
                        snapshot.display()
                    );
                }
            }
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
        // grofe's own verb: under the tmux name `layout` is an unknown command.
        "layout" if dialect() == Dialect::Grofe => layout_glue::run(&globals, &rest[1..]),
        // A stdio bridge to the server (T6.2).
        // grofe's own verb: under the tmux name `control` is an unknown command.
        "control" if dialect() == Dialect::Grofe && rest.len() == 1 => {
            control_bridge(&globals.socket())
        }
        "control" if dialect() == Dialect::Grofe => fail("grofe control: takes no arguments"),
        // grofe's own verb (T4.5): the server reads the snapshot and answers;
        // with no server there is nothing to lose, and none is started.
        "reboot-check" if dialect() == Dialect::Grofe => {
            let (socket, own_dir) = globals.resolve();
            match UnixStream::connect(&socket) {
                Ok(_) => one_shot(&socket, own_dir, rest),
                Err(_) => {
                    println!(
                        "no server running on {}: nothing to check",
                        socket.display()
                    );
                    ExitCode::SUCCESS
                }
            }
        }
        "new-session" | "new" if existing_for_a(&globals.socket(), rest).is_some() => {
            attach_existing(&globals.socket(), rest)
        }
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

/// `tmux -N`: this call never starts a server.
static NO_START: OnceLock<bool> = OnceLock::new();

/// `tmux -l`: the shell `-c` runs is a login shell.
static LOGIN: OnceLock<bool> = OnceLock::new();

/// `tmux -T`: terminal features for the client this call attaches.
static FEATURES: OnceLock<Vec<String>> = OnceLock::new();

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
    /// `-V` / `--version` among the global options.
    version: bool,
    /// `-h` / `--help` among the global options.
    help: bool,
}

impl Globals {
    /// `-S` wins, then `-L`, then under the tmux name `$TMUX`'s socket (as
    /// tmux 3.7b's `main`), then `$GROFE` (set inside panes), then the
    /// default socket.
    fn socket(&self) -> PathBuf {
        self.resolve().0
    }

    /// The socket path, and whether its directory is grofe's own (the
    /// default or `-L` location) rather than one the user named with `-S`,
    /// `$TMUX` or `$GROFE`. Only grofe's own directory is verified private,
    /// as tmux does not police `-S` parents.
    fn resolve(&self) -> (PathBuf, bool) {
        if self.socket.is_none() && self.name.is_none() {
            if let Some(s) = tmux_env_socket() {
                return (s, false);
            }
            if let Some(s) = std::env::var_os("GROFE").filter(|s| !s.is_empty()) {
                return (PathBuf::from(s), false);
            }
        }
        let env = Env::from_process();
        let path = paths::socket_path(&env, self.socket.as_deref(), self.name.as_deref());
        (path, self.socket.is_none())
    }
}

/// Under the tmux name, the socket `$TMUX` names (spec D12: tools pass it
/// around as tmux's), unless it is in tmux's own directory. Never under
/// the grofe name, which has `$GROFE`.
fn tmux_env_socket() -> Option<PathBuf> {
    if dialect() != Dialect::Tmux {
        return None;
    }
    let dirs = grofe::compat::tmux_socket_dirs(
        std::env::var("TMUX_TMPDIR").ok().as_deref(),
        nix::unistd::geteuid().as_raw(),
    );
    grofe::compat::tmux_env_socket(std::env::var_os("TMUX").as_deref(), &dirs)
}

fn parse_globals(args: &[String]) -> Result<(Globals, &[String]), String> {
    let mut globals = Globals {
        socket: None,
        name: None,
        version: false,
        help: false,
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
            "-V" | "--version" => {
                globals.version = true;
                i += 1;
            }
            "-h" | "--help" => {
                globals.help = true;
                i += 1;
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

/// `tmux -D`: the server in this process, not detached from the terminal
/// and never leaving for lack of sessions (`exit-empty` off), which tmux
/// does for a call with no command. With a server already on the socket
/// this is `None`: the call goes on as tmux's does, to its command (or the
/// default one) against that server.
fn run_foreground(globals: &Globals) -> Option<ExitCode> {
    let (socket, own_dir) = globals.resolve();
    if let Some(dir) = socket.parent().filter(|d| !d.as_os_str().is_empty())
        && (own_dir || !dir.exists())
        && let Err(e) = paths::ensure_private_dir(dir)
    {
        return Some(fail(&format!(
            "socket directory {} is not usable: {e}",
            dir.display()
        )));
    }
    match UnixStream::connect(&socket) {
        Ok(_) => return None,
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            // Stale: the file exists but nobody listens.
            let _ = std::fs::remove_file(&socket);
        }
        Err(_) => {}
    }
    report_panics_for(&socket, "server");
    Some(match server::run_daemon(&socket, None, false) {
        0 => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    })
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
    install_tmux_shim(&socket);
    match server::run_daemon(&socket, ready_fd, exit_when_empty) {
        0 => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

/// Makes a `tmux` link to this binary, which the server puts first on the
/// `PATH` of its panes, jobs and status sources, so a script that calls
/// `tmux` inside grofe reaches this server (spec D12). A real tmux would
/// read `$TMUX`, connect to grofe's socket and hang. Without the link the
/// server still runs, as before.
// ponytail: a shell rc that rebuilds PATH from scratch drops the link.
fn install_tmux_shim(socket: &Path) {
    let dir = grofe::paths::shim_dir(&grofe::paths::Env::from_process(), socket);
    let link = dir.join("tmux");
    let made = std::env::current_exe().and_then(|exe| {
        grofe::paths::ensure_private_dir(&dir)?;
        match std::fs::remove_file(&link) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        std::os::unix::fs::symlink(exe, &link)
    });
    match made {
        Ok(()) => grofe::compat::set_tmux_link_dir(dir),
        Err(e) => eprintln!("grofe server: no tmux link at {}: {e}", link.display()),
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("{}", msg.trim_end_matches('\n'));
    ExitCode::FAILURE
}

/// For `new-session -A -s <name>`: the id of the running session named
/// exactly `<name>`, if there is one (no server means none).
fn existing_for_a(socket: &Path, argv: &[String]) -> Option<String> {
    let Ok(grofe::cmd::Command::NewSession(c)) = grofe::cmd::parse(argv) else {
        return None;
    };
    let name = c.name.filter(|_| c.attach)?;
    let stream = UnixStream::connect(socket).ok()?;
    let argv = ["list-sessions", "-F", "#{session_id} #{session_name}"].map(String::from);
    let Ok(Reply::Ok { out, .. }) = request(stream, socket, &argv) else {
        return None;
    };
    out.lines()
        .filter_map(|l| l.split_once(' '))
        .find(|(_, n)| *n == name)
        .map(|(id, _)| id.to_string())
}

/// `new-session -A` for a session that exists: tmux 3.7b's
/// `attach-session`, even with `-d` (recorded): the client attaches when it
/// has a terminal, else fails as tmux does.
fn attach_existing(socket: &Path, argv: &[String]) -> ExitCode {
    let argv_of_new = argv;
    if !io::stdin().is_terminal() {
        return fail("open terminal failed: not a terminal");
    }
    match existing_for_a(socket, argv) {
        Some(id) => {
            let mut argv = vec!["attach-session".to_string(), "-t".to_string(), id];
            // `new-session -A` attaches as `attach-session` does, with
            // `-D`, `-X`, `-E`, `-c` and `-f` as it takes them.
            if let Ok(grofe::cmd::Command::NewSession(c)) = grofe::cmd::parse(argv_of_new) {
                if c.kill_parent {
                    argv.push("-x".to_string());
                } else if c.detach_others {
                    argv.push("-d".to_string());
                }
                if c.no_env {
                    argv.push("-E".to_string());
                }
                argv.extend(c.cwd.into_iter().flat_map(|d| ["-c".to_string(), d]));
                argv.extend(c.flags.into_iter().flat_map(|f| ["-f".to_string(), f]));
            }
            attach(socket, &argv)
        }
        None => fail("no sessions"),
    }
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
        // The id is the last line; `-P`'s output comes before it.
        // ponytail: tmux 3.7b shows `-P`'s output in view mode over the new
        // pane (recorded); grofe has no view mode until M5 and drops it.
        Ok(Reply::Ok { out, .. }) => out.lines().last().unwrap_or_default().trim().to_string(),
        Ok(Reply::Err { err, .. }) => return fail(&err),
        Ok(Reply::Event(_)) => return fail("unexpected event reply"),
        Err(e) => return fail(&e),
    };
    let mut argv = vec!["attach-session".to_string(), "-t".to_string(), id];
    // `new-session -f`: client flags for the client it attaches.
    if let Ok(grofe::cmd::Command::NewSession(c)) = grofe::cmd::parse(&create[..create.len() - 4]) {
        argv.extend(c.flags.into_iter().flat_map(|f| ["-f".to_string(), f]));
    }
    match attach_to(&socket, &argv, true) {
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
        || no_server(err)
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
        // tmux starts a server for `list-keys` and prints nothing: the server
        // has no session and is gone before it answers (recorded).
        Err(e) if dialect() == Dialect::Tmux && argv[0] == "list-keys" && no_server(&e) => {
            return ExitCode::SUCCESS;
        }
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
            // An error with status 0 is one a later `run-shell` cleared
            // (tmux 3.7b prints it and exits 0); with none, 1.
            let status = status.map_or(1, |s| u8::try_from(s).unwrap_or(1));
            ExitCode::from(status)
        }
        Ok(Reply::Event(_)) => fail("unexpected event reply"),
        Err(e) => fail(&e),
    }
}

/// What the two halves of [`control_bridge`] share.
#[derive(Default)]
struct BridgeState {
    /// Requests sent that have no reply yet.
    pending: usize,
    /// Ids of `subscribe` requests still waiting for their reply.
    subscribing: Vec<u64>,
    /// A `subscribe` was accepted: the stream goes on after stdin ends.
    subscribed: bool,
    /// Stdin has ended.
    eof: bool,
}

/// One request line from `input`, without its `\n` or `\r\n`: `None` at the
/// end of input, `Some(Err(why))` for a line the server could not take (past
/// the frame cap, which is skipped to its end without being kept, or not
/// UTF-8).
fn read_request_line(input: &mut impl BufRead) -> io::Result<Option<Result<String, String>>> {
    let max = grofe::protocol::MAX_MESSAGE_LEN;
    let mut buf = Vec::new();
    let mut over = false;
    loop {
        let chunk = input.fill_buf()?;
        if chunk.is_empty() {
            if buf.is_empty() && !over {
                return Ok(None);
            }
            break;
        }
        let end = chunk.iter().position(|b| *b == b'\n');
        let take = end.map_or(chunk.len(), |i| i + 1);
        if !over {
            buf.extend_from_slice(&chunk[..take]);
            if buf.len() > max + 2 {
                over = true;
                buf = Vec::new();
            }
        }
        input.consume(take);
        if end.is_some() {
            break;
        }
    }
    if over {
        return Ok(Some(Err(format!("request line over {max} bytes"))));
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    Ok(Some(String::from_utf8(buf).map_err(|_| {
        "request line is not valid UTF-8".to_string()
    })))
}

/// `grofe control` (T6.2): a stdio bridge to the control socket. Each line on
/// stdin is a request (spec §4.6), each line the server sends goes to stdout
/// as it came, so a plugin in any language needs no socket code. The hello is
/// this CLI's own. With stdin at its end, the bridge leaves once every reply
/// is out, unless a `subscribe` was sent; then it streams until the server
/// closes the connection or stdout does.
fn control_bridge(socket: &Path) -> ExitCode {
    use std::sync::{Arc, Mutex};

    let hello = match control_hello(socket) {
        Ok(h) => h,
        Err(e) => return fail(&e),
    };
    let stream = match connect(socket) {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => return fail(&format!("control: {e}")),
    };
    if let Err(e) = writeln!(writer, "{hello}") {
        return fail(&format!("sending hello: {e}"));
    }
    let state = Arc::new(Mutex::new(BridgeState::default()));

    let sender_state = Arc::clone(&state);
    std::thread::spawn(move || {
        let mut input = io::stdin().lock();
        loop {
            let line = match read_request_line(&mut input) {
                Ok(Some(Ok(line))) => line,
                Ok(Some(Err(why))) => {
                    // Not sent: the server would only close the connection.
                    let reply = serde_json::to_string(&Reply::err(0, &why)).unwrap_or_default();
                    let mut stdout = io::stdout().lock();
                    if writeln!(stdout, "{reply}")
                        .and_then(|()| stdout.flush())
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                Ok(None) | Err(_) => break,
            };
            if line.trim().is_empty() {
                continue;
            }
            {
                let mut s = sender_state.lock().unwrap();
                s.pending += 1;
                if let Ok(r) = serde_json::from_str::<Request>(&line)
                    && r.argv.first().is_some_and(|v| v == "subscribe")
                {
                    s.subscribing.push(r.id);
                }
            }
            if writeln!(writer, "{line}").is_err() {
                return;
            }
        }
        let mut s = sender_state.lock().unwrap();
        s.eof = true;
        if s.pending == 0 && !s.subscribed {
            std::process::exit(0);
        }
    });

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let value = serde_json::from_str::<serde_json::Value>(&line).unwrap_or_default();
        // The state stays locked through the write, so stdin's end cannot
        // exit the process half-way through a line.
        let mut s = state.lock().unwrap();
        let mut stdout = io::stdout().lock();
        if stdout.write_all(line.trim_end().as_bytes()).is_err()
            || stdout.write_all(b"\n").is_err()
            || stdout.flush().is_err()
        {
            return ExitCode::SUCCESS;
        }
        if value.get("event").is_none() {
            s.pending = s.pending.saturating_sub(1);
            let id = value.get("id").and_then(serde_json::Value::as_u64);
            if let Some(at) = s.subscribing.iter().position(|i| Some(*i) == id) {
                s.subscribing.remove(at);
                s.subscribed |= value.get("ok") == Some(&serde_json::Value::Bool(true));
            }
            if s.eof && s.pending == 0 && !s.subscribed {
                return ExitCode::SUCCESS;
            }
        }
    }
    if state.lock().unwrap().pending > 0 {
        return fail("server closed the connection without replying");
    }
    ExitCode::SUCCESS
}

/// tmux's `-c <shell-command>`: the command run by the default shell, as
/// `<shell> -c <command>` with `argv[0]` the shell's name and `$SHELL` set
/// to it, as tmux's `client_exec` runs it. The shell is a running server's
/// `default-shell`, else what a server started now would pick (`$SHELL`,
/// the passwd shell, `/bin/sh`). tmux starts a server only to ask; one
/// with no sessions exits at once, so none is started here.
fn shell_command(globals: &Globals, command: &str) -> ExitCode {
    let (socket, _) = globals.resolve();
    let ask = ["show-options", "-gv", "default-shell"].map(String::from);
    let set = connect(&socket)
        .and_then(|s| request(s, &socket, &ask))
        .ok()
        .and_then(|reply| match reply {
            Reply::Ok { out, .. } => Some(out.trim_end().to_string()),
            _ => None,
        })
        .filter(|v| !v.is_empty());
    let shell = server::pane::pick_shell(set.or_else(|| std::env::var("SHELL").ok()));
    let name = Path::new(&shell)
        .file_name()
        .map_or_else(|| shell.clone(), |n| n.to_string_lossy().into_owned());
    // `-l`: a login shell, which is its `argv[0]` starting with a dash.
    let name = match LOGIN.get() {
        Some(true) => format!("-{name}"),
        _ => name,
    };
    let err = Command::new(&shell)
        .arg0(name)
        .arg("-c")
        .arg(command)
        .env("SHELL", &shell)
        .exec();
    fail(&format!("execl failed: {err}"))
}

/// `attach-session`: the attach client (spec §4.6), in this process. It
/// never starts a server (tmux's does, only to find no sessions, which is
/// what grofe then says). Self-nesting is the server's call
/// (it knows which session holds `$GROFE_PANE`); the client only sends
/// the pane and `--nest`.
fn attach(socket: &Path, argv: &[String]) -> ExitCode {
    match attach_to(socket, argv, false) {
        Ok(reason) => {
            println!("[{reason}]");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

/// [`attach`]'s work: the reason the client was let go, or the error.
/// `created`: the session is the one this client's `new-session` just
/// made (no `client-attached` hook, as tmux).
fn attach_to(socket: &Path, argv: &[String], created: bool) -> Result<String, String> {
    let parsed = match grofe::cmd::parse(argv) {
        Ok(grofe::cmd::Command::AttachSession(a)) => a,
        Ok(_) => return Err("attach-session: unexpected command".to_string()),
        Err(e) => return Err(e.to_string()),
    };
    control_hello(socket)?;
    // tmux's attach starts a server, which finds no sessions; grofe's does
    // not start one and says what tmux would.
    let connect_attach = || match connect(socket) {
        Err(e) if no_server(&e) => Err("no sessions".to_string()),
        other => other,
    };
    // The target first (protocol::ATTACH_CHECK): a refusal never probes
    // the terminal.
    let tty = nix::unistd::ttyname(std::io::stdin())
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let check: Vec<String> = [ATTACH_CHECK.to_string(), tty]
        .into_iter()
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
        pane: cli_pane(socket),
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
        created,
        features: FEATURES.get().cloned().unwrap_or_default(),
        extras: grofe::protocol::AttachExtras {
            cwd: parsed.cwd.clone(),
            read_only: parsed.read_only,
            flags: parsed.flags.clone(),
            no_env: parsed.no_env,
            kill_parent: parsed.kill_parent,
        },
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
        // tmux 3.7b says `no server running` only for a stale socket, and
        // names the errno for a missing one (recorded, FE.2).
        io::ErrorKind::NotFound if dialect() == Dialect::Tmux => format!(
            "error connecting to {} (No such file or directory)",
            socket.display()
        ),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            format!("no server running on {}", socket.display())
        }
        _ => format!("error connecting to {}: {e}", socket.display()),
    })
}

/// Whether a connect error means no server runs there, in either
/// dialect's words ([`connect`]).
fn no_server(err: &str) -> bool {
    err.starts_with("no server running")
        || (err.starts_with("error connecting to ") && err.ends_with("(No such file or directory)"))
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
    // `tmux -N`: no server for this call, whatever command it is.
    if NO_START.get().copied().unwrap_or(false) {
        return connect(socket);
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

/// This CLI's pane for a request to `target`: `$GROFE_PANE`, and under the
/// tmux name `$TMUX_PANE` when `$TMUX` names `target` (a tool that kept
/// only tmux's variables, spec D12), each trusted as [`client_pane`] says.
fn cli_pane(target: &Path) -> Option<PaneId> {
    client_pane(
        std::env::var_os("GROFE").as_deref(),
        std::env::var("GROFE_PANE").ok().as_deref(),
        target,
    )
    .or_else(|| {
        client_pane(
            tmux_env_socket().as_deref().map(Path::as_os_str),
            std::env::var("TMUX_PANE").ok().as_deref(),
            target,
        )
    })
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
        stdin: cli_stdin(argv)?,
        umask: Some(cli_umask()),
    })
    .map_err(|e| e.to_string())?;
    let mut writer = &stream;
    writer
        .write_all(format!("{hello}\n{req}\n").as_bytes())
        .map_err(|e| format!("sending request: {e}"))?;
    read_reply(&stream)
}

/// Most bytes of stdin a `load-buffer -` sends: its base64 has to fit one
/// request line ([`grofe::protocol::MAX_MESSAGE_LEN`]).
// ponytail: below a buffer's 32 MiB; tmux streams the client's stdin.
const MAX_STDIN: usize = 12 << 20;

/// This CLI's stdin, base64, when `argv` runs `load-buffer -`.
fn cli_stdin(argv: &[String]) -> Result<Option<String>, String> {
    use base64::Engine as _;
    if !grofe::compat::loads_stdin(argv, dialect()) {
        return Ok(None);
    }
    let mut data = Vec::new();
    io::stdin()
        .lock()
        .take(MAX_STDIN as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|e| format!("reading stdin: {e}"))?;
    if data.len() > MAX_STDIN {
        return Err(format!("stdin too large (limit {MAX_STDIN} bytes)"));
    }
    Ok(Some(base64::engine::general_purpose::STANDARD.encode(data)))
}

/// This process's umask (read by setting it and putting it back).
fn cli_umask() -> u32 {
    use nix::sys::stat::{Mode, umask};
    let old = umask(Mode::from_bits_truncate(0o022));
    umask(old);
    old.bits() as u32
}

/// The control hello line for a request to `target`, or why it cannot be
/// sent (an environment too large for it), so that is known before
/// connecting.
fn control_hello(target: &Path) -> Result<String, String> {
    hello_for(target, true)
}

/// [`control_hello`], sending this CLI's pane only when `with_pane`.
fn hello_for(target: &Path, with_pane: bool) -> Result<String, String> {
    let pane = cli_pane(target);
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
