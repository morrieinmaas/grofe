//! The server side of remote sessions (spec 2026-11-15-ssh-remote §3, §4):
//! the stable ssh-agent link, `open-url` and `grofe open`, listening ports and
//! the `pane-echo` hint, each through the control socket.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{CMD_TIMEOUT, TestServer, run_bounded, wait_until};
use grofe::event::Event;
use grofe::protocol::{Caps, ClientCtx, Dialect, Hello, encode_hello};

const WAIT: Duration = Duration::from_secs(10);

fn server() -> TestServer {
    TestServer::start()
}

/// The agent link of the session `$N`, as the server derives it.
fn link(srv: &TestServer, n: u32) -> PathBuf {
    grofe::paths::agent_link(&srv.socket, n)
}

/// Runs `args` with `SSH_AUTH_SOCK` set to `agent` in the CLI's own
/// environment, as a client on another machine would have it.
fn run_with_agent(srv: &TestServer, agent: &str, args: &[&str]) {
    let mut cmd = srv.command();
    cmd.env("SSH_AUTH_SOCK", agent).args(args);
    let out = run_bounded(cmd, CMD_TIMEOUT);
    assert!(out.status.success(), "{args:?}: {}", out.stderr);
}

/// A pane command that writes `$SSH_AUTH_SOCK|$BROWSER` to `file`, then stays.
fn write_env(file: &Path) -> String {
    format!(
        "printf '%s|%s' \"$SSH_AUTH_SOCK\" \"$BROWSER\" > {}; sleep 100",
        file.display()
    )
}

/// What a pane started with `args` (a new-session or split-window up to its
/// command) saw as `(SSH_AUTH_SOCK, BROWSER)`.
fn env_of_new_pane(srv: &TestServer, name: &str, args: &[&str]) -> (String, String) {
    let file = srv.home.join(name);
    let command = write_env(&file);
    let mut argv = args.to_vec();
    argv.push(&command);
    srv.cmd(&argv).expect("start the pane");
    wait_until("the pane to write its environment", WAIT, || {
        fs::read_to_string(&file).is_ok()
    });
    let text = fs::read_to_string(&file).expect("env file");
    let (agent, browser) = text.split_once('|').expect("agent|browser");
    (agent.to_string(), browser.to_string())
}

/// Attaches a raw client to `session` whose environment has `agent`; the
/// returned stream keeps it attached.
fn attach_with_agent(srv: &TestServer, session: &str, agent: &str) -> UnixStream {
    let mut stream = UnixStream::connect(&srv.socket).expect("connect");
    let hello = Hello::Attach {
        ctx: ClientCtx {
            cwd: srv.home.clone(),
            pane: None,
            env: vec![("SSH_AUTH_SOCK".to_string(), agent.to_string())],
            client: None,
            dialect: Dialect::Grofe,
            request: Default::default(),
        },
        cols: 80,
        rows: 24,
        term: "xterm-256color".to_string(),
        caps: Caps::default(),
        target: Some(session.to_string()),
        detach_others: false,
        nest: false,
        tty: None,
        created: false,
        extras: Default::default(),
    };
    let line = encode_hello(&hello).expect("hello") + "\n";
    stream.write_all(line.as_bytes()).expect("send hello");
    let mut reply = String::new();
    BufReader::new(stream.try_clone().expect("clone"))
        .read_line(&mut reply)
        .expect("attach reply");
    assert!(reply.contains("\"ok\"") || reply.contains("out"), "{reply}");
    stream
}

#[test]
fn the_agent_link_follows_the_creating_client_and_every_attach() {
    let srv = server();
    run_with_agent(&srv, "/tmp/agent-one", &["new-session", "-d", "-s", "main"]);
    let link = link(&srv, 0);
    assert_eq!(
        fs::read_link(&link).expect("link"),
        Path::new("/tmp/agent-one")
    );

    let _client = attach_with_agent(&srv, "main", "/tmp/agent-two");
    wait_until("the attach to repoint the link", WAIT, || {
        fs::read_link(&link).is_ok_and(|t| t == Path::new("/tmp/agent-two"))
    });

    // Every pane, old or new, has the link and so the current agent.
    let (agent, _) = env_of_new_pane(&srv, "split", &["split-window", "-t", "main"]);
    assert_eq!(agent, link.display().to_string());
}

#[test]
fn the_first_pane_has_the_link_too() {
    let srv = server();
    let file = srv.home.join("first");
    let command = write_env(&file);
    run_with_agent(
        &srv,
        "/tmp/agent-one",
        &["new-session", "-d", "-s", "main", &command],
    );
    wait_until("the first pane's environment", WAIT, || file.exists());
    let text = fs::read_to_string(&file).expect("env file");
    assert_eq!(
        text.split('|').next(),
        Some(link(&srv, 0).display().to_string().as_str())
    );
}

#[test]
fn a_client_with_no_agent_leaves_the_link_alone() {
    let srv = server();
    run_with_agent(&srv, "/tmp/agent-one", &["new-session", "-d", "-s", "main"]);
    let mut cmd = srv.command();
    cmd.env_remove("SSH_AUTH_SOCK")
        .args(["new-session", "-d", "-s", "other"]);
    assert!(run_bounded(cmd, CMD_TIMEOUT).status.success());
    assert_eq!(
        fs::read_link(link(&srv, 0)).expect("link"),
        Path::new("/tmp/agent-one")
    );
    // The new session's link starts at the global environment's agent
    // (the server's own), or is not made when there is none.
    let global = srv
        .cmd(&["show-environment", "-g", "SSH_AUTH_SOCK"])
        .ok()
        .and_then(|l| l.trim().strip_prefix("SSH_AUTH_SOCK=").map(PathBuf::from));
    assert_eq!(fs::read_link(link(&srv, 1)).ok(), global);
}

#[test]
fn a_link_nothing_made_starts_at_the_global_agent() {
    let srv = server();
    srv.cmd(&["start-server"]).expect("start");
    srv.cmd(&[
        "set-environment",
        "-g",
        "SSH_AUTH_SOCK",
        "/tmp/agent-global",
    ])
    .expect("global agent");
    // A session made by a client with no agent: nothing points its link.
    let mut cmd = srv.command();
    cmd.env_remove("SSH_AUTH_SOCK")
        .args(["new-session", "-d", "-s", "main"]);
    assert!(run_bounded(cmd, CMD_TIMEOUT).status.success());
    let (agent, _) = env_of_new_pane(&srv, "pane", &["split-window", "-t", "main"]);
    let link = link(&srv, 0);
    assert_eq!(agent, link.display().to_string());
    assert_eq!(
        fs::read_link(&link).expect("the link"),
        Path::new("/tmp/agent-global")
    );
}

#[test]
fn a_link_a_dead_server_left_does_not_carry_over() {
    let srv = TestServer::unstarted();
    // The socket's directory first: the link's name hashes its real path.
    fs::create_dir_all(srv.socket.parent().unwrap()).expect("socket dir");
    let stale = link(&srv, 0);
    grofe::paths::ensure_private_dir(stale.parent().unwrap()).expect("agent dir");
    std::os::unix::fs::symlink("/tmp/agent-of-a-dead-server", &stale).expect("plant");
    srv.cmd(&["start-server"]).expect("start");
    srv.cmd(&[
        "set-environment",
        "-g",
        "SSH_AUTH_SOCK",
        "/tmp/agent-global",
    ])
    .expect("global agent");
    let mut cmd = srv.command();
    cmd.env_remove("SSH_AUTH_SOCK")
        .args(["new-session", "-d", "-s", "main"]);
    assert!(run_bounded(cmd, CMD_TIMEOUT).status.success());
    assert_eq!(
        fs::read_link(&stale).expect("the new session's link"),
        Path::new("/tmp/agent-global")
    );
}

#[test]
fn the_link_goes_with_its_session() {
    let srv = server();
    run_with_agent(&srv, "/tmp/agent-one", &["new-session", "-d", "-s", "main"]);
    srv.cmd(&["new-session", "-d", "-s", "keep"]).expect("keep");
    let link = link(&srv, 0);
    assert!(fs::symlink_metadata(&link).is_ok());
    srv.cmd(&["kill-session", "-t", "main"])
        .expect("kill-session");
    wait_until("the link to go", WAIT, || {
        fs::symlink_metadata(&link).is_err()
    });
}

#[test]
fn stable_ssh_agent_off_passes_the_clients_agent_through() {
    let srv = server();
    srv.cmd(&["start-server"]).expect("start");
    srv.cmd(&["set-option", "-g", "stable-ssh-agent", "off"])
        .expect("set");
    let file = srv.home.join("off");
    let command = write_env(&file);
    run_with_agent(
        &srv,
        "/tmp/agent-one",
        &["new-session", "-d", "-s", "main", &command],
    );
    wait_until("the pane's environment", WAIT, || file.exists());
    let text = fs::read_to_string(&file).expect("env file");
    assert_eq!(text.split('|').next(), Some("/tmp/agent-one"));
    assert!(fs::symlink_metadata(link(&srv, 0)).is_err(), "no link made");
}

#[test]
fn browser_is_the_grofe_open_shim_unless_forward_browser_is_off() {
    let srv = server();
    let (_, browser) = env_of_new_pane(&srv, "on", &["new-session", "-d", "-s", "main"]);
    // One absolute program, so `"$BROWSER" "$url"` works.
    let shim = Path::new(&browser);
    assert!(shim.is_absolute(), "{browser}");
    assert_eq!(shim.file_name().unwrap(), "grofe-open");
    srv.cmd(&["set-option", "-g", "forward-browser", "off"])
        .expect("set");
    let (_, off) = env_of_new_pane(&srv, "off", &["split-window", "-t", "main"]);
    assert_ne!(off, browser);
}

#[test]
fn browser_works_quoted_when_grofe_is_not_on_the_path() {
    let srv = TestServer::builder().path_env("/usr/bin:/bin").start();
    let mut sub = srv.subscribe(&["open-url"]).expect("subscribe");
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "command -v grofe || \"$BROWSER\" 'https://example.com/b?c=d'; sleep 100",
    ])
    .expect("session");
    match sub.next(WAIT).expect("the event") {
        Event::OpenUrl { url, .. } => assert_eq!(url, "https://example.com/b?c=d"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn open_url_reaches_subscribers_that_name_it() {
    let srv = server();
    let mut named = srv.subscribe(&["open-url"]).expect("subscribe");
    let mut bare = srv.subscribe(&[]).expect("bare subscribe");
    srv.cmd(&["open-url", "https://example.com/a?b=c"])
        .expect("open-url");
    match named.next(WAIT).expect("the event") {
        Event::OpenUrl { url, pane } => {
            assert_eq!(url, "https://example.com/a?b=c");
            assert_eq!(pane, None, "the CLI is not in a pane");
        }
        other => panic!("{other:?}"),
    }
    // A bare subscription does not get events added after v1.0.
    assert!(bare.next(Duration::from_millis(300)).is_err());
}

#[test]
fn open_url_names_the_pane_it_came_from() {
    let srv = server();
    srv.cmd(&["new-session", "-d", "-s", "main"])
        .expect("session");
    let pane = srv
        .cmd(&["display-message", "-p", "-t", "main", "#{pane_id}"])
        .expect("pane id");
    let mut sub = srv.subscribe(&["open-url"]).expect("subscribe");
    let mut cmd = srv.command();
    cmd.env("GROFE", &srv.socket)
        .env("GROFE_PANE", pane.trim())
        .args(["open-url", "http://x/"]);
    assert!(run_bounded(cmd, CMD_TIMEOUT).status.success());
    match sub.next(WAIT).expect("the event") {
        Event::OpenUrl { pane: Some(p), .. } => assert_eq!(p.to_string(), pane.trim()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn open_url_refuses_other_schemes_with_a_message() {
    let srv = server();
    let mut sub = srv.subscribe(&["open-url"]).expect("subscribe");
    for url in ["javascript:alert(1)", "ftp://x/y", "ssh://host", "x"] {
        let err = srv.cmd(&["open-url", url]).expect_err(url);
        assert!(
            err.contains("refused") && err.contains("http, https and file"),
            "{err}"
        );
    }
    assert!(
        sub.next(Duration::from_millis(300)).is_err(),
        "nothing delivered"
    );
}

#[test]
fn open_url_without_a_subscriber_says_so() {
    let srv = server();
    let err = srv
        .cmd(&["open-url", "https://example.com"])
        .expect_err("no subscriber");
    assert!(err.contains(grofe::open::NO_SUBSCRIBER), "{err}");
}

/// A script standing in for `open`/`xdg-open`: writes its argument to `out`.
fn fake_opener(srv: &TestServer, out: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = srv.home.join("opener");
    fs::write(
        &path,
        format!("#!/bin/sh\nprintf '%s' \"$1\" > {}\n", out.display()),
    )
    .expect("write the opener");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

#[test]
fn grofe_open_falls_back_to_the_local_opener_without_a_subscriber() {
    let srv = server();
    let out = srv.home.join("opened");
    let opener = fake_opener(&srv, &out);
    let mut cmd = srv.command();
    cmd.env(grofe::open::OPENER_ENV, &opener)
        .args(["open", "https://example.com/z"]);
    let res = run_bounded(cmd, CMD_TIMEOUT);
    assert!(res.status.success(), "{}", res.stderr);
    assert_eq!(
        fs::read_to_string(&out).expect("opened"),
        "https://example.com/z"
    );
}

#[test]
fn grofe_open_with_no_server_still_opens_locally() {
    let srv = TestServer::unstarted();
    let out = srv.home.join("opened");
    let opener = fake_opener(&srv, &out);
    let mut cmd = srv.command();
    cmd.env(grofe::open::OPENER_ENV, &opener)
        .args(["open", "http://x/"]);
    assert!(run_bounded(cmd, CMD_TIMEOUT).status.success());
    assert_eq!(fs::read_to_string(&out).expect("opened"), "http://x/");
    assert!(!srv.is_running(), "it did not start a server");
}

#[test]
fn grofe_open_hands_the_url_to_a_subscriber_and_does_not_open_it() {
    let srv = server();
    let out = srv.home.join("opened");
    let opener = fake_opener(&srv, &out);
    let mut sub = srv.subscribe(&["open-url"]).expect("subscribe");
    let mut cmd = srv.command();
    cmd.env(grofe::open::OPENER_ENV, &opener)
        .args(["open", "https://example.com/q"]);
    assert!(run_bounded(cmd, CMD_TIMEOUT).status.success());
    assert!(matches!(
        sub.next(WAIT).expect("event"),
        Event::OpenUrl { url, .. } if url == "https://example.com/q"
    ));
    assert!(!out.exists(), "the local opener stayed quiet");
}

#[test]
fn grofe_open_refuses_other_schemes_and_never_runs_the_opener() {
    let srv = server();
    let out = srv.home.join("opened");
    let opener = fake_opener(&srv, &out);
    for url in ["javascript:alert(1)", "ftp://x"] {
        let mut cmd = srv.command();
        cmd.env(grofe::open::OPENER_ENV, &opener)
            .args(["open", url]);
        let res = run_bounded(cmd, CMD_TIMEOUT);
        assert!(!res.status.success(), "{url}");
        assert!(res.stderr.contains("refused"), "{}", res.stderr);
    }
    assert!(!out.exists());
}

/// Not a test of its own: the process the ports tests start in a pane. It
/// listens, writes its port to `GROFE_T_PORTFILE`, and exits once
/// `GROFE_T_STOPFILE` exists.
#[test]
#[ignore = "the pane process of the ports tests"]
fn listener_child() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let file = std::env::var("GROFE_T_PORTFILE").expect("port file");
    fs::write(file, port.to_string()).expect("write the port");
    let stop = std::env::var("GROFE_T_STOPFILE").expect("stop file");
    for _ in 0..1200 {
        if Path::new(&stop).exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A session whose pane runs [`listener_child`]: the port it listens on and
/// the file that stops it.
fn listening_pane(srv: &TestServer) -> (u16, PathBuf) {
    let port_file = srv.home.join("port");
    let stop_file = srv.home.join("stop");
    let exe = std::env::current_exe().expect("test exe");
    let command = format!(
        "GROFE_T_PORTFILE={} GROFE_T_STOPFILE={} {} --ignored --exact --quiet listener_child",
        port_file.display(),
        stop_file.display(),
        exe.display()
    );
    srv.cmd(&["new-session", "-d", "-s", "main", &command])
        .expect("session");
    wait_until("the listener's port", WAIT, || port_file.exists());
    let port = loop {
        if let Ok(text) = fs::read_to_string(&port_file)
            && let Ok(port) = text.trim().parse()
        {
            break port;
        }
    };
    (port, stop_file)
}

#[test]
fn pane_ports_and_list_ports_show_what_the_pane_listens_on() {
    let srv = server();
    let (port, _stop) = listening_pane(&srv);
    let pane = srv
        .cmd(&["display-message", "-p", "-t", "main", "#{pane_id}"])
        .expect("pane id");
    wait_until("#{pane_ports}", WAIT, || {
        srv.cmd(&["display-message", "-p", "-t", "main", "#{pane_ports}"])
            .is_ok_and(|out| out.trim() == port.to_string())
    });
    let want = format!("{} {port}\n", pane.trim());
    assert_eq!(srv.cmd(&["list-ports"]).expect("list-ports"), want);
    assert_eq!(srv.cmd(&["list-ports", "-a"]).expect("list-ports -a"), want);
    // Another session's pane is not in the window's list, only in -a's.
    srv.cmd(&["new-session", "-d", "-s", "quiet"])
        .expect("quiet");
    assert_eq!(srv.cmd(&["list-ports", "-t", "quiet"]).expect("quiet"), "");
    assert_eq!(srv.cmd(&["list-ports", "-a"]).expect("-a"), want);
}

#[test]
fn ports_changed_is_sent_when_a_port_opens_and_when_it_closes() {
    let srv = server();
    let mut sub = srv.subscribe(&["ports-changed"]).expect("subscribe");
    let (port, stop) = listening_pane(&srv);
    let pane = srv
        .cmd(&["display-message", "-p", "-t", "main", "#{pane_id}"])
        .expect("pane id");
    let opened = sub.next(WAIT).expect("ports-changed on open");
    assert_eq!(
        opened,
        Event::PortsChanged {
            pane: pane.trim().parse().expect("pane id"),
            ports: vec![port]
        }
    );
    fs::write(&stop, "").expect("stop the listener");
    let closed = sub.next(WAIT).expect("ports-changed on close");
    assert!(matches!(closed, Event::PortsChanged { ports, .. } if ports.is_empty()));
}

#[test]
fn pane_echo_is_sent_when_the_ptys_echo_flag_changes() {
    let srv = server();
    let mut sub = srv.subscribe(&["pane-echo"]).expect("subscribe");
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "stty -echo; sleep 5; stty echo; sleep 100",
    ])
    .expect("session");
    let pane = srv
        .cmd(&["display-message", "-p", "-t", "main", "#{pane_id}"])
        .expect("pane id");
    let pane = pane.trim().parse().expect("pane id");
    assert_eq!(
        sub.next(WAIT).expect("echo off"),
        Event::PaneEcho { pane, echo: false }
    );
    assert_eq!(
        sub.next(WAIT).expect("echo on"),
        Event::PaneEcho { pane, echo: true }
    );
}

#[test]
fn a_subscriber_in_place_of_one_that_went_still_gets_the_current_echo() {
    let srv = server();
    // A fixed echo state: a shell's line editor flips the flag as it starts,
    // which would race the snapshots compared below.
    srv.cmd(&["new-session", "-d", "-s", "main", "stty -echo; sleep 100"])
        .expect("session");
    let pane = srv
        .cmd(&["display-message", "-p", "-t", "main", "#{pane_id}"])
        .expect("pane id");
    let pane = pane.trim().parse().expect("pane id");
    let mut _first = srv.subscribe(&["pane-echo"]).expect("first");
    // Wait for the pane's echo to be off before the snapshots are taken.
    let mut off = false;
    while !off {
        off = _first.next(WAIT).expect("echo off") == (Event::PaneEcho { pane, echo: false });
    }
    let mut second = srv.subscribe(&["pane-echo"]).expect("second");
    let current = second.next(WAIT).expect("current echo");
    assert_eq!(current, Event::PaneEcho { pane, echo: false });
    // The second goes; a third takes its place, so the count is as before.
    drop(second);
    wait_until("the server to see the second go", WAIT, || {
        srv.cmd(&["list-clients"]).is_ok()
    });
    std::thread::sleep(Duration::from_millis(300));
    let mut third = srv.subscribe(&["pane-echo"]).expect("third");
    assert_eq!(
        third.next(WAIT).expect("current echo for the third too"),
        current
    );
}

#[test]
fn nothing_polls_without_a_subscriber() {
    let srv = server();
    let _ = listening_pane(&srv);
    // A bare subscription names neither event, so it starts no poller and
    // gets none.
    let mut bare = srv.subscribe(&[]).expect("bare");
    assert!(bare.next(Duration::from_millis(2500)).is_err());
}

#[test]
fn a_late_subscriber_gets_the_current_ports_and_echo_of_every_pane() {
    let srv = server();
    // A first subscriber keeps the poller running and has seen everything.
    let mut first = srv.subscribe(&["ports-changed"]).expect("first");
    let (port, _stop) = listening_pane(&srv);
    let pane = srv
        .cmd(&["display-message", "-p", "-t", "main", "#{pane_id}"])
        .expect("pane id");
    let pane = pane.trim().parse().expect("pane id");
    first.next(WAIT).expect("the first sees the port");

    let mut ports = srv.subscribe(&["ports-changed"]).expect("late ports");
    assert_eq!(
        ports.next(WAIT).expect("current ports"),
        Event::PortsChanged {
            pane,
            ports: vec![port]
        }
    );
    let mut echo = srv.subscribe(&["pane-echo"]).expect("late echo");
    assert_eq!(
        echo.next(WAIT).expect("current echo"),
        Event::PaneEcho { pane, echo: true }
    );
}

#[test]
fn the_links_go_when_the_server_exits() {
    let srv = server();
    run_with_agent(&srv, "/tmp/agent-one", &["new-session", "-d", "-s", "main"]);
    let link = link(&srv, 0);
    assert!(fs::symlink_metadata(&link).is_ok());
    srv.cmd(&["kill-server"]).ok();
    wait_until("the link to go with the server", WAIT, || {
        fs::symlink_metadata(&link).is_err()
    });
}

/// Writes an executable `sh` script at `path`.
fn script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write the script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// Runs `grofe open <url>` in a new pane of `srv`; its exit status, once
/// it has one.
fn grofe_open_in_a_pane(srv: &TestServer, name: &str, url: &str) -> String {
    let out = srv.home.join(format!("{name}.exit"));
    let command = format!(
        "{} open '{url}'; echo $? > {}; sleep 100",
        env!("CARGO_BIN_EXE_grofe"),
        out.display()
    );
    srv.cmd(&["new-session", "-d", "-s", name, &command])
        .expect("session");
    wait_until("grofe open to finish", WAIT, || {
        fs::read_to_string(&out).is_ok_and(|s| !s.is_empty())
    });
    fs::read_to_string(&out).unwrap().trim().to_string()
}

#[test]
fn an_opener_that_runs_browser_does_not_loop_back_into_grofe_open() {
    // A headless xdg-open (and macOS's open, here) runs $BROWSER: the shim.
    let bin = tempfile::tempdir().expect("bin");
    let count = bin.path().join("count");
    for name in ["xdg-open", "open"] {
        script(
            &bin.path().join(name),
            &format!(
                "echo run >> '{}'\n[ -n \"$BROWSER\" ] && exec \"$BROWSER\" \"$1\"\nexit 3",
                count.display()
            ),
        );
    }
    let path = format!("{}:/usr/bin:/bin", bin.path().display());
    let srv = TestServer::builder().path_env(&path).start();
    let status = grofe_open_in_a_pane(&srv, "loop", "https://example.com/loop");
    assert_ne!(status, "0", "nothing could open it");
    assert_eq!(
        fs::read_to_string(&count).unwrap(),
        "run\n",
        "the opener ran once"
    );
}

#[test]
fn grofe_open_falls_back_to_the_panes_own_browser() {
    let srv = TestServer::builder().path_env("/usr/bin:/bin").start();
    let got = srv.home.join("browsed");
    let browser = srv.home.join("my-browser");
    script(&browser, &format!("echo \"$1\" >> '{}'", got.display()));
    srv.cmd(&["start-server"]).expect("start");
    srv.cmd(&[
        "set-environment",
        "-g",
        "BROWSER",
        browser.to_str().unwrap(),
    ])
    .expect("BROWSER");
    // The pane keeps it beside the shim.
    let env = srv.home.join("env");
    let command = format!(
        "printf '%s|%s' \"$BROWSER\" \"$GROFE_PREV_BROWSER\" > {}; sleep 100",
        env.display()
    );
    srv.cmd(&["new-session", "-d", "-s", "env", &command])
        .expect("session");
    wait_until("the pane's env", WAIT, || env.exists());
    let text = fs::read_to_string(&env).unwrap();
    let (shim, prev) = text.split_once('|').unwrap();
    assert!(shim.ends_with("/grofe-open"), "{shim}");
    assert_eq!(prev, browser.display().to_string());
    // With nobody to forward to, the user's own browser opens it, even a
    // scheme that is never forwarded.
    assert_eq!(
        grofe_open_in_a_pane(&srv, "a", "https://example.com/p"),
        "0"
    );
    assert_eq!(grofe_open_in_a_pane(&srv, "b", "ftp://example.com/f"), "0");
    assert_eq!(
        fs::read_to_string(&got).unwrap(),
        "https://example.com/p\nftp://example.com/f\n"
    );
}
