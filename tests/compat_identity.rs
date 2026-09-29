//! Identity and entry (TC.1, spec D12, §4.2, §4.6): grofe run through a
//! `tmux` symlink reads tmux's global flags and `-V`, starts a server with
//! tmux identity, and every pane gets `TMUX`/`TMUX_PANE`. Until FE.1 lands,
//! the server's front end still refuses every tmux-dialect command, so
//! these tests start a tmux-identity server with `tmux start-server` and
//! make sessions with the `grofe` CLI.
//!
//! Every test runs in its own `TestServer` directories, on a private `-S`
//! socket, with `TMUX_TMPDIR` pointed into that directory: nothing here
//! reaches a real tmux server or the owner's sockets.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind};
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;

use common::{CMD_TIMEOUT, CmdOutput, TestServer, run_bounded, wait_child, wait_until};
use grofe::protocol::{Dialect, Hello, decode_hello};

const TMUX_NESTED: &str = "sessions should be nested with care, unset $TMUX to force";

/// The harness's root: the parent of its `home/`, `state/`, `run/` dirs.
fn root(srv: &TestServer) -> PathBuf {
    srv.home.parent().expect("root").to_path_buf()
}

/// A `tmux` symlink to the grofe binary, in the harness's `bin/`.
fn tmux_link(srv: &TestServer) -> PathBuf {
    let bin = root(srv).join("bin");
    fs::create_dir_all(&bin).unwrap();
    let link = bin.join("tmux");
    if !link.exists() {
        symlink(env!("CARGO_BIN_EXE_grofe"), &link).unwrap();
    }
    link
}

/// `TMUX_TMPDIR` for every command: tmux's own socket directory, inside the
/// harness, so a real tmux socket could only ever appear there.
fn tmux_tmpdir(srv: &TestServer) -> PathBuf {
    let dir = root(srv).join("tmux-tmpdir");
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The harness's isolated environment on `cmd`, the outer terminal named
/// `outer-term`, and no `TMUX` from wherever the tests run.
fn isolate(srv: &TestServer, cmd: &mut Command) {
    cmd.current_dir(&srv.home)
        .env_remove("GROFE")
        .env_remove("GROFE_PANE")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("HOME", &srv.home)
        .env("XDG_CONFIG_HOME", &srv.config_home)
        .env("XDG_STATE_HOME", &srv.state_home)
        .env("XDG_RUNTIME_DIR", &srv.runtime_dir)
        .env("TMUX_TMPDIR", tmux_tmpdir(srv))
        .env("TERM_PROGRAM", "outer-term")
        .env("TERM_PROGRAM_VERSION", "9.9")
        .env("GROFE_RESTORE_ON_START", "0")
        .env("GROFE_REMAIN_ON_EXIT", "1");
}

/// `tmux <args>` through the symlink, no socket flag added.
fn tmux_bare(srv: &TestServer, args: &[&str]) -> CmdOutput {
    let mut cmd = Command::new(tmux_link(srv));
    isolate(srv, &mut cmd);
    cmd.args(args);
    run_bounded(cmd, CMD_TIMEOUT)
}

/// `tmux -S <harness socket> <args>` through the symlink.
fn tmux(srv: &TestServer, args: &[&str]) -> CmdOutput {
    let socket = srv.socket.display().to_string();
    let mut all = vec!["-S", socket.as_str()];
    all.extend_from_slice(args);
    tmux_bare(srv, &all)
}

/// `grofe -S <harness socket> <args>`: `Ok(stdout)` or `Err(stderr)`.
fn grofe(srv: &TestServer, args: &[&str]) -> Result<String, String> {
    let mut cmd = srv.command();
    isolate(srv, &mut cmd);
    cmd.args(args);
    let out = run_bounded(cmd, CMD_TIMEOUT);
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(out.stderr)
    }
}

/// A server started by `tmux start-server`: tmux identity (spec D12).
fn start_as_tmux() -> TestServer {
    let srv = TestServer::unstarted();
    let out = tmux(&srv, &["start-server"]);
    assert!(out.status.success(), "start-server: {}", out.stderr);
    srv
}

/// Runs `env` in a new pane (a new session `name`, or a window in it when
/// it exists) and returns the pane's environment.
fn pane_env(srv: &TestServer, verb: &[&str], file: &str) -> BTreeMap<String, String> {
    let path = srv.home.join(file);
    let command = format!("env > {}.tmp && mv {0}.tmp {0}; sleep 60", path.display());
    let mut args = verb.to_vec();
    args.push(&command);
    grofe(srv, &args).expect("spawn an env pane");
    wait_until(|| path.exists(), CMD_TIMEOUT).expect("the pane wrote its env");
    fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn var<'a>(env: &'a BTreeMap<String, String>, name: &str) -> &'a str {
    env.get(name).map(String::as_str).unwrap_or("<unset>")
}

/// `<socket path>,<server pid>,<session id number>`, from the server's
/// own formats.
fn expected_tmux(srv: &TestServer, session: &str) -> String {
    let out = grofe(
        srv,
        &[
            "display-message",
            "-p",
            "-t",
            session,
            "#{socket_path},#{pid},#{session_id}",
        ],
    )
    .expect("display");
    out.trim().replace('$', "")
}

/// Case 1 and 2, and `-V`: a server started through the `tmux` symlink
/// gives its panes tmux's variables, and a later `grofe` call does not
/// change that.
#[test]
fn a_tmux_start_gives_panes_tmux_identity() {
    let srv = start_as_tmux();
    let env = pane_env(&srv, &["new-session", "-d", "-s", "a"], "env-a");
    let tmux_var = var(&env, "TMUX");
    let fields: Vec<&str> = tmux_var.split(',').collect();
    assert_eq!(fields.len(), 3, "TMUX={tmux_var}");
    assert!(!fields[0].is_empty());
    assert!(fields[1].parse::<u32>().is_ok() && fields[2].parse::<u32>().is_ok());
    assert_eq!(tmux_var, expected_tmux(&srv, "a"));
    assert_eq!(fields[0], srv.socket.display().to_string());
    assert!(var(&env, "TMUX_PANE").starts_with('%'));
    assert_eq!(var(&env, "TMUX_PANE"), var(&env, "GROFE_PANE"));
    assert_eq!(var(&env, "TERM_PROGRAM"), "tmux");
    assert_eq!(var(&env, "TERM_PROGRAM_VERSION"), "3.7b");

    let env = pane_env(&srv, &["new-window", "-t", "a"], "env-b");
    assert_eq!(var(&env, "TERM_PROGRAM"), "tmux", "identity is server-wide");
    assert_eq!(var(&env, "TERM_PROGRAM_VERSION"), "3.7b");
    assert_eq!(var(&env, "TMUX_PANE"), var(&env, "GROFE_PANE"));

    assert_eq!(
        grofe(&srv, &["display-message", "-p", "#{version}"]).as_deref(),
        Ok("3.7b\n")
    );
    let out = tmux_bare(&srv, &["-V"]);
    assert!(out.status.success());
    assert_eq!(out.stdout, "tmux 3.7b\n");
}

/// Case 3: a server started as `grofe` keeps the outer terminal's
/// `TERM_PROGRAM`, and still sets `TMUX` in tmux's format.
#[test]
fn a_grofe_start_keeps_the_outer_term_program() {
    // The server starts from the outer terminal: panes take `TERM_PROGRAM`
    // from its environment, as tmux's take theirs (`update-environment`
    // does not name it, TC.5).
    let srv = TestServer::unstarted();
    grofe(&srv, &["start-server"]).expect("start-server");
    let env = pane_env(&srv, &["new-session", "-d", "-s", "g"], "env-g");
    assert_eq!(var(&env, "TERM_PROGRAM"), "outer-term");
    assert_eq!(var(&env, "TERM_PROGRAM_VERSION"), "9.9");
    assert_eq!(var(&env, "TMUX"), expected_tmux(&srv, "g"));
    assert_eq!(var(&env, "TMUX_PANE"), var(&env, "GROFE_PANE"));
    let version = grofe(&srv, &["display-message", "-p", "#{version}"]).unwrap();
    assert_ne!(version, "3.7b\n");
    // A later tmux call does not change the identity either.
    tmux(&srv, &["no-such-verb"]);
    let env = pane_env(&srv, &["new-window", "-t", "g"], "env-g2");
    assert_eq!(var(&env, "TERM_PROGRAM"), "outer-term");
}

/// Case 4: `compat-identity = "tmux"` in config overrides a `grofe` start.
#[test]
fn compat_identity_in_config_overrides_a_grofe_start() {
    let srv = TestServer::unstarted();
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.toml"), "compat-identity = \"tmux\"\n").unwrap();
    grofe(&srv, &["start-server"]).expect("start-server");
    let env = pane_env(&srv, &["new-session", "-d", "-s", "c"], "env-c");
    assert_eq!(var(&env, "TERM_PROGRAM"), "tmux");
    assert_eq!(var(&env, "TERM_PROGRAM_VERSION"), "3.7b");
}

/// The first line a CLI run with `cmd` sends to a socket nobody answers.
fn first_line_sent(mut cmd: Command, listener: &UnixListener) -> String {
    listener.set_nonblocking(true).unwrap();
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + CMD_TIMEOUT;
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("no connection: {e}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(CMD_TIMEOUT)).unwrap();
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).unwrap();
    drop(stream);
    wait_child(child, CMD_TIMEOUT).expect("the CLI exits");
    line
}

/// Case 5: a `tmux`-invoked CLI sends `dialect: tmux`, a `grofe` one
/// leaves it off (grofe), and a hello without the field parses as grofe.
#[test]
fn the_hello_carries_the_invoking_name_s_dialect() {
    let srv = TestServer::unstarted();
    let fake = root(&srv).join("fake.sock");
    let listener = UnixListener::bind(&fake).unwrap();
    let fake_arg = fake.display().to_string();

    let mut cmd = Command::new(tmux_link(&srv));
    isolate(&srv, &mut cmd);
    cmd.args(["-S", &fake_arg, "list-sessions"]);
    let hello = first_line_sent(cmd, &listener);
    let value: serde_json::Value = serde_json::from_str(&hello).unwrap();
    assert_eq!(value["dialect"], "tmux", "{hello}");
    match decode_hello(hello.trim()).unwrap() {
        Hello::Control(ctx) => assert_eq!(ctx.dialect, Dialect::Tmux),
        other => panic!("{other:?}"),
    }

    let mut cmd = srv.command();
    isolate(&srv, &mut cmd);
    cmd.args(["-S", &fake_arg, "list-sessions"]);
    let hello = first_line_sent(cmd, &listener);
    let value: serde_json::Value = serde_json::from_str(&hello).unwrap();
    assert!(value.get("dialect").is_none(), "{hello}");
    match decode_hello(hello.trim()).unwrap() {
        Hello::Control(ctx) => assert_eq!(ctx.dialect, Dialect::Grofe),
        other => panic!("{other:?}"),
    }
}

/// Types `line` and Enter into the shell in `target`.
fn type_line(srv: &TestServer, target: &str, line: &str) {
    grofe(srv, &["send-keys", "-t", target, line, "Enter"]).expect("send-keys");
}

/// `path`'s text once the shell has written its `rc=<status>` line.
fn wait_file(path: &Path) -> String {
    wait_until(
        || fs::read_to_string(path).is_ok_and(|t| t.contains("rc=") && t.ends_with('\n')),
        CMD_TIMEOUT,
    )
    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    fs::read_to_string(path).unwrap()
}

/// Case 6, and the `:` prompt: from inside a pane, attaching to another
/// session succeeds and attaching to the pane's own session is refused,
/// with tmux's text for a `tmux` attach and grofe's for a `grofe` one.
/// Text typed at the nested client's `:` prompt is tmux dialect on this
/// tmux-identity server, so the front end (a stub until FE.1) answers it.
#[test]
fn attach_from_a_pane_refuses_only_its_own_session() {
    let srv = start_as_tmux();
    grofe(&srv, &["new-session", "-d", "-s", "outer", "sh"]).unwrap();
    grofe(&srv, &["new-session", "-d", "-s", "inner", "sleep 60"]).unwrap();
    let link = tmux_link(&srv).display().to_string();
    let grofe_bin = env!("CARGO_BIN_EXE_grofe");
    let home = srv.home.display();

    type_line(
        &srv,
        "outer",
        &format!("{link} attach -t outer 2> {home}/own; echo rc=$? >> {home}/own"),
    );
    let own = wait_file(&srv.home.join("own"));
    assert_eq!(own, format!("{TMUX_NESTED}\nrc=1\n"));
    type_line(
        &srv,
        "outer",
        &format!("{grofe_bin} attach -t outer 2> {home}/own-g; echo rc=$? >> {home}/own-g"),
    );
    let own = wait_file(&srv.home.join("own-g"));
    assert!(
        own.starts_with("refusing to attach session outer from inside itself"),
        "{own}"
    );
    assert!(own.ends_with("rc=1\n"), "{own}");

    // Bare `tmux` is `new-session`; the front end refuses it until FE.1.
    type_line(
        &srv,
        "outer",
        &format!("{link} 2> {home}/bare; echo rc=$? >> {home}/bare"),
    );
    assert_eq!(
        wait_file(&srv.home.join("bare")),
        "tmux front end is not implemented: new-session\nrc=1\n"
    );

    grofe(&srv, &["bind-key", ":", "command-prompt"]).expect("bind :");
    type_line(
        &srv,
        "outer",
        &format!("{link} attach -t inner; echo rc=$? > {home}/other"),
    );
    wait_until(
        || {
            grofe(&srv, &["list-clients", "-F", "#{client_session}"])
                .is_ok_and(|out| out.trim() == "inner")
        },
        CMD_TIMEOUT,
    )
    .expect("a client attached to inner from outer's pane");

    grofe(&srv, &["send-keys", "-t", "outer", "C-b", ":"]).unwrap();
    type_line(&srv, "outer", "display-message typed-at-the-prompt");
    wait_until(
        || {
            srv.capture("outer")
                .contains("tmux front end is not implemented")
        },
        CMD_TIMEOUT,
    )
    .unwrap_or_else(|e| panic!("{e}: {}", srv.capture("outer")));

    grofe(&srv, &["detach-client", "-s", "inner"]).expect("detach");
    assert_eq!(wait_file(&srv.home.join("other")), "rc=0\n");
}

fn miss_lines(srv: &TestServer) -> Vec<String> {
    fs::read_to_string(srv.state_home.join("grofe/compat-miss.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Case 7: each tmux-dialect request the front end cannot translate adds
/// one line to `compat-miss.log`, in the test server's state dir; grofe
/// requests add none.
#[test]
fn misses_append_one_line_each_to_the_test_state_dir() {
    let srv = start_as_tmux();
    for _ in 0..2 {
        let out = tmux(&srv, &["no-such-verb", "-x"]);
        assert!(!out.status.success());
    }
    assert!(grofe(&srv, &["no-such-verb"]).is_err());
    let lines = miss_lines(&srv);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines.iter().all(|l| l.contains("no-such-verb -x")),
        "{lines:?}"
    );
}

/// A grofe-identity server logs no misses, even for a tmux request.
#[test]
fn a_grofe_server_logs_no_misses() {
    let srv = TestServer::start();
    assert!(!tmux(&srv, &["no-such-verb"]).status.success());
    assert!(miss_lines(&srv).is_empty());
}

/// Bare `tmux` without a terminal fails as tmux 3.7b does, before it
/// starts or creates anything.
#[test]
fn bare_tmux_without_a_terminal_fails_as_tmux_does() {
    let srv = TestServer::unstarted();
    let out = tmux(&srv, &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stderr, "open terminal failed: not a terminal\n");
    assert_eq!(out.stdout, "");
    assert!(!srv.socket.exists());
}

/// A private copy of tmux's socket layout under the harness's
/// `TMUX_TMPDIR`: `tmux-<uid>/default` (live), `tmux-<uid>/stale`.
fn fake_tmux_dir(srv: &TestServer) -> (PathBuf, UnixListener) {
    let uid = nix::unistd::geteuid().as_raw();
    let dir = tmux_tmpdir(srv).join(format!("tmux-{uid}"));
    fs::create_dir_all(&dir).unwrap();
    let live = UnixListener::bind(dir.join("default")).unwrap();
    live.set_nonblocking(true).unwrap();
    drop(UnixListener::bind(dir.join("stale")).unwrap());
    (dir, live)
}

/// Run as `tmux`, `-S` into tmux's own socket directory is refused before
/// any connection, and a stale socket there is left in place.
#[test]
fn a_socket_in_tmux_s_own_directory_is_refused() {
    let srv = TestServer::unstarted();
    let (dir, live) = fake_tmux_dir(&srv);
    for name in ["default", "stale", "new", "sub/deeper"] {
        let path = dir.join(name).display().to_string();
        for verb in ["start-server", "new-session", "list-sessions"] {
            let out = tmux_bare(
                &srv,
                &["-S", &path, verb, "-d"][..if verb == "new-session" { 4 } else { 3 }],
            );
            assert_eq!(out.status.code(), Some(1), "{name} {verb}");
            assert!(
                out.stderr.contains("in tmux's socket directory"),
                "{name} {verb}: {}",
                out.stderr
            );
        }
    }
    match live.accept() {
        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
        other => panic!("something connected to the fake tmux socket: {other:?}"),
    }
    let mut left: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["default", "stale"], "nothing added or removed");
    // `$GROFE` pointing there is refused as well.
    let mut cmd = Command::new(tmux_link(&srv));
    isolate(&srv, &mut cmd);
    cmd.env("GROFE", dir.join("default")).arg("list-sessions");
    let out = run_bounded(cmd, CMD_TIMEOUT);
    assert!(
        out.stderr.contains("in tmux's socket directory"),
        "{}",
        out.stderr
    );
}

/// Run as `tmux`, a stale socket grofe did not create is never removed,
/// and `start-server` on a socket some other program listens on fails
/// instead of reporting success.
#[test]
fn foreign_sockets_are_never_taken_over() {
    let srv = TestServer::unstarted();
    let stale = root(&srv).join("foreign-stale.sock");
    drop(UnixListener::bind(&stale).unwrap());
    let stale_arg = stale.display().to_string();
    for verb in ["start-server", "new-session"] {
        let out = tmux_bare(
            &srv,
            &["-S", &stale_arg, verb, "-d"][..if verb == "new-session" { 4 } else { 3 }],
        );
        assert!(!out.status.success(), "{verb}");
        assert!(
            out.stderr.contains("grofe did not create it"),
            "{verb}: {}",
            out.stderr
        );
        assert!(stale.exists(), "{verb} removed the foreign socket");
    }

    let foreign = root(&srv).join("foreign-live.sock");
    let listener = UnixListener::bind(&foreign).unwrap();
    let taker = std::thread::spawn(move || {
        // Answers nothing and hangs up, as a non-grofe server would.
        let (stream, _) = listener.accept().unwrap();
        drop(stream);
    });
    let out = tmux_bare(
        &srv,
        &["-S", &foreign.display().to_string(), "start-server"],
    );
    taker.join().unwrap();
    assert!(!out.status.success(), "start-server reported success");
    assert!(
        out.stderr.contains("is not a grofe server"),
        "{}",
        out.stderr
    );
    assert!(foreign.exists());

    // A socket grofe did create, left stale by a crash, is still replaced.
    assert!(tmux(&srv, &["start-server"]).status.success());
    assert!(grofe(&srv, &["kill-server"]).is_ok());
    drop(UnixListener::bind(&srv.socket).unwrap());
    let out = tmux(&srv, &["start-server"]);
    assert!(out.status.success(), "{}", out.stderr);
}

/// Run as `tmux`, grofe never connects to tmux's own sockets: not the one
/// `$TMUX` names, not `TMUX_TMPDIR`'s, not with `-L`. Everything it
/// touches is under grofe's directory in the harness.
#[test]
fn the_tmux_name_never_touches_a_real_tmux() {
    let srv = TestServer::unstarted();
    let uid = nix::unistd::geteuid().as_raw();
    let real_dir = tmux_tmpdir(&srv).join(format!("tmux-{uid}"));
    fs::create_dir_all(&real_dir).unwrap();
    let real = real_dir.join("default");
    let listener = UnixListener::bind(&real).unwrap();
    listener.set_nonblocking(true).unwrap();
    let run = |args: &[&str]| {
        let mut cmd = Command::new(tmux_link(&srv));
        isolate(&srv, &mut cmd);
        cmd.env("TMUX", format!("{},1,0", real.display()))
            .env("TMUX_PANE", "%0")
            .args(args);
        run_bounded(cmd, CMD_TIMEOUT)
    };
    let socket = srv.socket.display().to_string();

    assert_eq!(run(&["-V"]).stdout, "tmux 3.7b\n");
    let out = run(&["list-sessions"]);
    assert!(!out.status.success());
    let default = srv.runtime_dir.join("grofe/default.sock");
    assert!(
        out.stderr.contains(&default.display().to_string()),
        "{}",
        out.stderr
    );
    let out = run(&["-L", "grofe-tc1", "kill-server"]);
    let named = srv.runtime_dir.join("grofe/grofe-tc1.sock");
    assert!(
        out.stderr.contains(&named.display().to_string()),
        "{}",
        out.stderr
    );
    assert!(run(&["-S", &socket, "start-server"]).status.success());
    assert!(!run(&["-S", &socket, "list-sessions"]).status.success());

    match listener.accept() {
        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
        other => panic!("something connected to the real tmux socket: {other:?}"),
    }
    let entries: Vec<_> = fs::read_dir(&real_dir).unwrap().collect();
    assert_eq!(entries.len(), 1, "only the fake socket: {entries:?}");
    assert_eq!(
        fs::read_dir(tmux_tmpdir(&srv)).unwrap().count(),
        1,
        "nothing new in TMUX_TMPDIR"
    );
}

/// Only the basename `tmux` is tmux: other names are grofe.
#[test]
fn other_names_behave_as_grofe() {
    let srv = TestServer::unstarted();
    let bin = root(&srv).join("bin");
    fs::create_dir_all(&bin).unwrap();
    for name in ["grofe", "tmuxx", "grofe-tmux"] {
        let link = bin.join(name);
        symlink(env!("CARGO_BIN_EXE_grofe"), &link).unwrap();
        let mut cmd = Command::new(&link);
        isolate(&srv, &mut cmd);
        let out = run_bounded(cmd, CMD_TIMEOUT);
        assert!(out.status.success(), "{name}: {}", out.stderr);
        assert!(out.stdout.starts_with("usage: grofe"), "{name}");
    }
}
