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

/// What a pane of a new session `name` reads back, as `od -c` prints it
/// with the blanks removed, after it sends XTVERSION (`CSI > q`, `CSI > 0
/// q`), `CSI > 1 q` (not XTVERSION) and DA1 as a fence.
fn xtversion_replies(srv: &TestServer, name: &str) -> String {
    let path = srv.home.join(format!("xtv-{name}"));
    let script = format!(
        "stty -icanon -echo min 0 time 10; printf '\\033[>q\\033[>0q\\033[>1q\\033[c'; \
         dd bs=1024 count=8 2>/dev/null | od -An -c | tr -d ' \\n' > {0}.tmp; \
         mv {0}.tmp {0}; sleep 60",
        path.display()
    );
    grofe(srv, &["new-session", "-d", "-s", name, &script]).expect("new-session");
    wait_until(|| path.exists(), CMD_TIMEOUT).expect("the pane wrote its replies");
    fs::read_to_string(&path).unwrap()
}

/// XTVERSION is answered once per query, before the DA1 fence, with the
/// server's name and version: grofe's own, or under tmux identity tmux's
/// as tmux 3.7b answers it (`DCS > | tmux 3.7b ST`, recorded).
#[test]
fn xtversion_names_the_server_identity() {
    let srv = TestServer::unstarted();
    grofe(&srv, &["start-server"]).expect("start-server");
    let reply = format!("033P>|grofe{}033\\", env!("CARGO_PKG_VERSION"));
    assert_eq!(
        xtversion_replies(&srv, "g"),
        format!("{reply}{reply}033[?1;2;4c")
    );

    let srv = start_as_tmux();
    let reply = "033P>|tmux3.7b033\\";
    assert_eq!(
        xtversion_replies(&srv, "t"),
        format!("{reply}{reply}033[?1;2;4c")
    );
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

    // A socket of its own: a CLI whose request goes unanswered connects once
    // more to see whether a server is still there, and that connection
    // waits in the first listener's queue.
    let fake = root(&srv).join("fake2.sock");
    let listener = UnixListener::bind(&fake).unwrap();
    let fake_arg = fake.display().to_string();
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

/// Case 6, and the `:` prompt: from inside a pane, a `tmux` attach is
/// refused to any session, with tmux's text, as tmux 3.7b refuses a
/// client on one of its panes' terminals (recorded, TP.10); a `grofe`
/// attach is refused only to the pane's own session, with grofe's text,
/// and attaches to another. Text typed at the nested client's `:` prompt
/// is tmux dialect on this tmux-identity server, so the front end answers
/// it (FE.1: `setw` is tmux's, not grofe's).
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

    // Bare `tmux` is `new-session` (FE.1), which needs a terminal to
    // attach, as in tmux 3.7b.
    type_line(
        &srv,
        "outer",
        &format!("{link} < /dev/null 2> {home}/bare; echo rc=$? >> {home}/bare"),
    );
    assert_eq!(
        wait_file(&srv.home.join("bare")),
        "open terminal failed: not a terminal\nrc=1\n"
    );

    type_line(
        &srv,
        "outer",
        &format!("{link} attach -t inner 2> {home}/other-t; echo rc=$? >> {home}/other-t"),
    );
    let other = wait_file(&srv.home.join("other-t"));
    assert_eq!(other, format!("{TMUX_NESTED}\nrc=1\n"));

    grofe(&srv, &["bind-key", ":", "command-prompt"]).expect("bind :");
    type_line(
        &srv,
        "outer",
        &format!("{grofe_bin} attach -t inner; echo rc=$? > {home}/other"),
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
    type_line(&srv, "outer", "setw -g @typed at-the-prompt");
    wait_until(
        || {
            // tmux's global window scope, which formats see.
            grofe(&srv, &["display-message", "-p", "#{@typed}"])
                .is_ok_and(|out| out.trim() == "at-the-prompt")
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
        lines.iter().all(|l| l.ends_with(" <unknown> -x")),
        "{lines:?}"
    );
}

/// A tmux command grofe cannot do, in a sourced file, fails only its own
/// line and is noted in `compat-miss.log` like any other miss (B1a).
#[test]
fn a_refused_line_of_a_sourced_file_is_noted() {
    let srv = start_as_tmux();
    let new = tmux(&srv, &["new-session", "-d", "-s", "main", "sleep 100000"]);
    assert!(new.status.success(), "{}", new.stderr);
    let file = srv.home.join("lack.conf");
    fs::write(&file, "set -g @a 1\ndetach-client -E true\nset -g @b 2\n").unwrap();
    let out = tmux(&srv, &["source-file", file.to_str().unwrap()]);
    assert!(!out.status.success());
    let value = |name: &str| tmux(&srv, &["show-options", "-gqv", name]).stdout;
    assert_eq!((value("@a").trim(), value("@b").trim()), ("1", "2"));
    let lines = miss_lines(&srv);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("detach-client -E"), "{lines:?}");
}

/// A copy-mode command grofe does not know does nothing, as in tmux (which
/// answers success), and is noted once by name in `compat-miss.log`.
#[test]
fn an_unknown_copy_mode_command_is_noted_once() {
    let srv = start_as_tmux();
    let new = tmux(&srv, &["new-session", "-d", "-s", "main", "sleep 100000"]);
    assert!(new.status.success(), "{}", new.stderr);
    assert!(tmux(&srv, &["copy-mode", "-t", "main"]).status.success());
    for _ in 0..2 {
        let out = tmux(
            &srv,
            &["send-keys", "-t", "main", "-X", "bogus-command", "secret"],
        );
        assert!(out.status.success(), "{}", out.stderr);
        assert_eq!((out.stdout.as_str(), out.stderr.as_str()), ("", ""));
    }
    let out = tmux(&srv, &["send-keys", "-t", "main", "-X", "Not A Name; rm"]);
    assert!(out.status.success());
    let lines = miss_lines(&srv);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].ends_with(" send-keys -X bogus-command: ignored"),
        "{lines:?}"
    );
    assert!(
        lines[1].ends_with(" send-keys -X (not a name): ignored"),
        "{lines:?}"
    );
    assert!(lines.iter().all(|l| !l.contains("secret")), "{lines:?}");
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
    // As tmux, a stopped server leaves its socket behind; that is stale too.
    assert!(srv.socket.exists(), "a stopped tmux leaves its socket");
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
    // FE.1: the front end answers it now, from grofe's own server.
    assert!(run(&["-S", &socket, "list-sessions"]).status.success());

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
        cmd.arg("-h");
        isolate(&srv, &mut cmd);
        let out = run_bounded(cmd, CMD_TIMEOUT);
        assert!(out.status.success(), "{name}: {}", out.stderr);
        assert!(out.stdout.starts_with("usage: grofe"), "{name}");
    }
}

/// tmux 3.7b (recorded): `-s` names the server table. A session option
/// set there is stored and shown by name (`show -sv base-index` is `3`)
/// but read by nothing, an unset one shows nothing, and a bare `show -s`
/// lists the server options and the user options set with `-s`, no
/// session option.
#[test]
fn show_s_lists_the_server_table_and_names_what_set_s_stored() {
    let srv = start_as_tmux();
    let ok = |args: &[&str]| {
        let out = tmux(&srv, args);
        assert!(out.status.success(), "{args:?}: {}", out.stderr);
        out.stdout
    };
    ok(&["new-session", "-d", "-s", "s"]);
    assert_eq!(ok(&["show", "-s", "mouse"]), "");
    ok(&["set", "-s", "base-index", "3"]);
    assert_eq!(ok(&["show", "-sv", "base-index"]), "3\n");
    assert_eq!(ok(&["show", "-gv", "base-index"]), "0\n");
    ok(&["set", "-s", "@srv", "x"]);
    assert_eq!(ok(&["show", "-sv", "@srv"]), "x\n");
    let table = ok(&["show", "-s"]);
    assert!(table.contains("escape-time 10\n"), "{table}");
    assert!(
        table.contains("default-terminal tmux-256color\n"),
        "{table}"
    );
    assert!(table.starts_with("@srv x\n"), "{table}");
    for absent in ["base-index", "mouse", "status"] {
        let listed = table.lines().any(|l| l.split(' ').next() == Some(absent));
        assert!(!listed, "{absent} in {table}");
    }
}

/// TC.13 regression: `tmux -c <shell-command>` (tmux-fzf's `menu.sh`) runs
/// the command with the default shell, as tmux 3.7b's `client_exec` does:
/// `argv[0]` the shell's name, `$SHELL` the shell, no `$TMUX`, and the
/// command's exit status; with no server running it uses `$SHELL`, and a
/// running server's `default-shell` wins. A command after it is a usage
/// error (all recorded on a private tmux 3.7b socket).
#[test]
fn tmux_c_runs_the_command_with_the_default_shell() {
    let srv = TestServer::unstarted();
    let mut cmd = Command::new(tmux_link(&srv));
    isolate(&srv, &mut cmd);
    cmd.env("SHELL", "/bin/sh").args([
        "-S",
        &srv.socket.display().to_string(),
        "-c",
        "echo \"$0 $SHELL ${TMUX-unset}\"; exit 3",
    ]);
    let out = run_bounded(cmd, CMD_TIMEOUT);
    assert_eq!(out.status.code(), Some(3), "{}", out.stderr);
    assert_eq!(out.stdout, "sh /bin/sh unset\n");

    let out = tmux(&srv, &["-c", "true", "ls"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stderr.starts_with("usage: tmux [-2CDhlNuVv]"),
        "{}",
        out.stderr
    );

    let srv = start_as_tmux();
    let shell = srv.home.join("marker-shell");
    fs::write(&shell, "#!/bin/sh\necho \"marker $SHELL\"\n").unwrap();
    fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let shell = shell.display().to_string();
    let out = tmux(&srv, &["set", "-g", "default-shell", &shell]);
    assert!(out.status.success(), "{}", out.stderr);
    let out = tmux(&srv, &["-c", "echo not this shell"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, format!("marker {shell}\n"));

    // An explicit /bin/sh is set like any other shell, whatever $SHELL is.
    let out = tmux(&srv, &["set", "-g", "default-shell", "/bin/sh"]);
    assert!(out.status.success(), "{}", out.stderr);
    let mut cmd = Command::new(tmux_link(&srv));
    isolate(&srv, &mut cmd);
    cmd.env("SHELL", &shell).args([
        "-S",
        &srv.socket.display().to_string(),
        "-c",
        "echo \"$0 $SHELL\"",
    ]);
    let out = run_bounded(cmd, CMD_TIMEOUT);
    assert_eq!(out.stdout, "sh /bin/sh\n", "{}", out.stderr);

    // tmux's checkshell refuses what it could not run.
    for bad in ["/nonexistent", "sh"] {
        let out = tmux(&srv, &["set", "-g", "default-shell", bad]);
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(
            out.stderr.trim_end(),
            format!("not a suitable shell: {bad}")
        );
    }
    let _ = tmux(&srv, &["kill-server"]);
}

/// TC.13 regression: a `run-shell` or `if-shell` job gets what tmux 3.7b's
/// `environ_for_session` sets (recorded): `$TMUX` naming the job's session,
/// tmux's `TERM_PROGRAM`, and no `$TMUX_PANE`. fzf's `--tmux` decides from
/// `$TMUX`, so `bind K run-shell tsess` opens its picker.
#[test]
fn run_shell_jobs_get_tmux_s_variables() {
    let srv = start_as_tmux();
    grofe(&srv, &["new-session", "-d", "-s", "a"]).expect("new-session");
    let probe = "echo \"$TMUX ${TMUX_PANE-unset} $TERM_PROGRAM $TERM_PROGRAM_VERSION\"";
    let out = tmux(&srv, &["run-shell", probe]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        format!("{} unset tmux 3.7b\n", expected_tmux(&srv, "a"))
    );
    let out = tmux(
        &srv,
        &[
            "if-shell",
            "[ -n \"$TMUX\" ] && [ -z \"$TMUX_PANE\" ]",
            "display -p yes",
            "display -p no",
        ],
    );
    assert_eq!(out.stdout, "yes\n", "{}", out.stderr);
    let _ = tmux(&srv, &["kill-server"]);
}

/// `list-plugins`, `plugin-restart`, `float-pane` and `place-pane` are
/// grofe's own verbs (spec §7.4):
/// under the `tmux` name they are unknown commands, as in tmux 3.7b, even
/// on a server that has them, and a grofe-name call still runs them.
#[test]
fn plugin_verbs_are_unknown_commands_under_the_tmux_name() {
    let srv = start_as_tmux();
    for args in [
        &["list-plugins"][..],
        &["plugin-restart", "p"],
        &["float-pane"],
        &["place-pane", "-X", "1"],
    ] {
        let out = tmux(&srv, args);
        assert!(!out.status.success(), "{args:?}: {out:?}");
        assert_eq!(
            out.stderr,
            format!("unknown command: {}\n", args[0]),
            "{args:?}"
        );
    }
    assert_eq!(grofe(&srv, &["list-plugins"]), Ok(String::new()));
    let _ = tmux(&srv, &["kill-server"]);
}

/// B4 review: `tmux -D` against a server already running is a client with
/// no command, which tmux's server makes `new-session` (as bare `tmux`),
/// not grofe's usage text.
#[test]
fn tmux_dash_d_with_a_running_server_runs_new_session() {
    let srv = start_as_tmux();
    let bare = tmux(&srv, &[]);
    let foreground = tmux(&srv, &["-D"]);
    assert!(
        !foreground.stdout.contains("usage"),
        "{}",
        foreground.stdout
    );
    assert_eq!(
        (foreground.status.code(), foreground.stderr),
        (bare.status.code(), bare.stderr)
    );
    let _ = tmux(&srv, &["kill-server"]);
}

/// `<link> <args>` in the harness's environment plus `vars` (no `GROFE`,
/// `GROFE_PANE`, `TMUX` or `TMUX_PANE` unless `vars` sets them).
fn run_with(srv: &TestServer, link: &Path, vars: &[(&str, &str)], args: &[&str]) -> CmdOutput {
    let mut cmd = Command::new(link);
    isolate(srv, &mut cmd);
    for (k, v) in vars {
        cmd.env(k, v);
    }
    cmd.args(args);
    run_bounded(cmd, CMD_TIMEOUT)
}

/// Run as `tmux` with no `-L`/`-S`, the socket is `$TMUX`'s first field,
/// as tmux 3.7b's `main` takes it: a tool that kept only `$TMUX` (and
/// `$TMUX_PANE`) from a pane reaches that pane's server, and that pane.
#[test]
fn the_tmux_name_follows_the_socket_in_dollar_tmux() {
    let srv = start_as_tmux();
    let env = pane_env(&srv, &["new-session", "-d", "-s", "rt"], "env-rt");
    let tmux_var = var(&env, "TMUX").to_string();
    let pane = var(&env, "TMUX_PANE").to_string();
    assert!(tmux_var.starts_with(&srv.socket.display().to_string()));
    // A second window becomes the active one, so only the pane context
    // names the first pane.
    grofe(&srv, &["new-window", "-t", "rt"]).expect("new-window");
    let link = tmux_link(&srv);

    let out = run_with(&srv, &link, &[("TMUX", &tmux_var)], &["list-sessions"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(out.stdout.starts_with("rt: "), "{}", out.stdout);

    let out = run_with(
        &srv,
        &link,
        &[("TMUX", &tmux_var), ("TMUX_PANE", &pane)],
        &["display-message", "-p", "#{pane_id}"],
    );
    assert_eq!(out.stdout.trim(), pane, "{}", out.stderr);

    // `$TMUX` wins over a `$GROFE` naming another server.
    let other = root(&srv).join("other.sock").display().to_string();
    let out = run_with(
        &srv,
        &link,
        &[("TMUX", &tmux_var), ("GROFE", &other)],
        &["list-sessions"],
    );
    assert!(out.status.success(), "{}", out.stderr);

    // `-S` and `-L` win over `$TMUX`.
    let out = run_with(
        &srv,
        &link,
        &[("TMUX", &tmux_var)],
        &["-S", &other, "list-sessions"],
    );
    assert!(!out.status.success());
    assert!(out.stderr.contains(&other), "{}", out.stderr);
    let out = run_with(
        &srv,
        &link,
        &[("TMUX", &tmux_var)],
        &["-L", "grofe-envsock", "list-sessions"],
    );
    assert!(!out.status.success());
    let named = srv.runtime_dir.join("grofe/grofe-envsock.sock");
    assert!(
        out.stderr.contains(&named.display().to_string()),
        "{}",
        out.stderr
    );

    // An empty `$TMUX`, or one with an empty first field, is not a socket.
    let default = srv.runtime_dir.join("grofe/default.sock");
    for value in ["", ",1,0"] {
        let out = run_with(&srv, &link, &[("TMUX", value)], &["list-sessions"]);
        assert!(!out.status.success(), "TMUX={value:?}");
        assert!(
            out.stderr.contains(&default.display().to_string()),
            "TMUX={value:?}: {}",
            out.stderr
        );
    }
    let _ = tmux(&srv, &["kill-server"]);
}

/// A `$TMUX_PANE` naming no pane of the server is ignored under the tmux
/// name, as tmux 3.7b's `cmd_find_inside_pane` ignores it (recorded): the
/// request falls back to the best session, it is not refused. A server
/// started from inside another tmux keeps that `$TMUX_PANE` in its global
/// environment, so every plugin job's `tmux display -p` otherwise failed.
#[test]
fn a_stale_dollar_tmux_pane_falls_back_under_the_tmux_name() {
    let srv = start_as_tmux();
    grofe(&srv, &["new-session", "-d", "-s", "st"]).expect("new-session");
    let active = grofe(&srv, &["display-message", "-p", "#{pane_id}"]).expect("display");
    let tmux_var = format!("{},1,0", srv.socket.display());
    let link = tmux_link(&srv);
    let out = run_with(
        &srv,
        &link,
        &[("TMUX", &tmux_var), ("TMUX_PANE", "%99")],
        &["display-message", "-p", "#{pane_id}"],
    );
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, active);

    // The case a plugin hits: the server's environment carries the stale
    // pane into its jobs.
    grofe(&srv, &["set-environment", "-g", "TMUX_PANE", "%99"]).expect("setenv");
    // `##`: run-shell expands its command first, and the job's own
    // display-message must look the pane up. A `%0` already in the text
    // is a strftime conversion to display-message, `0` on macOS.
    let out = tmux(
        &srv,
        &["run-shell", "tmux display-message -p 'job ##{pane_id}'"],
    );
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, format!("job {active}"));

    // The grofe name keeps its rule: a stale `$GROFE_PANE` is an error.
    let grofe_bin = Path::new(env!("CARGO_BIN_EXE_grofe"));
    let sock = srv.socket.display().to_string();
    let out = run_with(
        &srv,
        grofe_bin,
        &[("GROFE", &sock), ("GROFE_PANE", "%99")],
        &["-S", &sock, "display-message", "-p", "#{pane_id}"],
    );
    assert!(!out.status.success(), "{}", out.stdout);
    assert!(
        out.stderr.contains("can't find pane: %99"),
        "{}",
        out.stderr
    );
    let _ = tmux(&srv, &["kill-server"]);
}

/// The `grofe` name keeps its own rule: `$GROFE`, never `$TMUX`.
#[test]
fn the_grofe_name_ignores_dollar_tmux() {
    let srv = start_as_tmux();
    grofe(&srv, &["new-session", "-d", "-s", "n"]).expect("new-session");
    let grofe_bin = Path::new(env!("CARGO_BIN_EXE_grofe"));
    let tmux_var = format!("{},1,0", srv.socket.display());
    let out = run_with(&srv, grofe_bin, &[("TMUX", &tmux_var)], &["list-sessions"]);
    assert!(!out.status.success(), "{}", out.stdout);
    let default = srv.runtime_dir.join("grofe/default.sock");
    assert!(
        out.stderr.contains(&default.display().to_string()),
        "{}",
        out.stderr
    );
    let other = format!("{},1,0", root(&srv).join("other.sock").display());
    let socket = srv.socket.display().to_string();
    let out = run_with(
        &srv,
        grofe_bin,
        &[("TMUX", &other), ("GROFE", &socket)],
        &["list-sessions"],
    );
    assert!(out.status.success(), "{}", out.stderr);
    let _ = tmux(&srv, &["kill-server"]);
}
