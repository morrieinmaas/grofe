//! TOML layouts (`docs/layouts.md`): `grofe layout up|save|list` against a
//! real server. Every test runs in the harness's temp dirs, so the layouts
//! directory is `<config_home>/grofe/layouts` of the test, never the real one.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{CmdOutput, TestServer, wait_until};

const WAIT: Duration = Duration::from_secs(10);

fn layouts_dir(srv: &TestServer) -> PathBuf {
    srv.config_home.join("grofe").join("layouts")
}

fn write_layout(srv: &TestServer, name: &str, text: &str) -> PathBuf {
    let dir = layouts_dir(srv);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.toml"));
    fs::write(&path, text).unwrap();
    path
}

/// A directory inside the harness's home, canonical (macOS `/var` is a
/// link) so it compares with what a pane reports.
fn dir(srv: &TestServer, name: &str) -> PathBuf {
    let d = srv.home.join(name);
    fs::create_dir_all(&d).unwrap();
    fs::canonicalize(d).unwrap()
}

fn run(srv: &TestServer, args: &[&str]) -> CmdOutput {
    srv.run(args)
}

/// `layout up <args> -d`, which must succeed.
fn up_ok(srv: &TestServer, name: &str) -> String {
    let out = run(srv, &["layout", "up", name, "-d"]);
    assert!(out.status.success(), "layout up: {}", out.stderr);
    out.stdout
}

fn lines(srv: &TestServer, args: &[&str]) -> Vec<String> {
    srv.cmd(args)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn windows(srv: &TestServer, session: &str) -> Vec<String> {
    lines(
        srv,
        &[
            "list-windows",
            "-t",
            session,
            "-F",
            "#{window_name}|#{window_active}|#{window_layout}",
        ],
    )
}

fn panes(srv: &TestServer, target: &str) -> Vec<Vec<String>> {
    lines(
        srv,
        &[
            "list-panes",
            "-t",
            target,
            "-F",
            "#{pane_id}|#{pane_floating_flag}|#{pane_active}|#{pane_current_path}|#{pane_dead}|#{pane_x},#{pane_y},#{pane_width},#{pane_height}|#{pane_title}",
        ],
    )
    .iter()
    .map(|l| l.split('|').map(str::to_string).collect())
    .collect()
}

fn no_sessions(srv: &TestServer) -> bool {
    srv.cmd(&["list-sessions"]).is_err()
}

#[test]
fn up_builds_windows_panes_and_cwds() {
    let srv = TestServer::unstarted();
    let (a, b) = (dir(&srv, "a"), dir(&srv, "b"));
    fs::create_dir_all(a.join("sub")).unwrap();
    write_layout(
        &srv,
        "dev",
        &format!(
            r#"
name = "dev"
root = "{a}"

[[window]]
name = "edit"
cwd = "sub"

[[window.pane]]
[[window.pane]]
cwd = "{b}"

[[window]]
name = "logs"
"#,
            a = a.display(),
            b = b.display()
        ),
    );
    let out = up_ok(&srv, "dev");
    assert!(out.contains("created session dev"), "{out}");
    let w = windows(&srv, "dev");
    assert_eq!(w.len(), 2, "{w:?}");
    assert!(w[0].starts_with("edit|"), "{w:?}");
    assert!(w[1].starts_with("logs|"), "{w:?}");
    let p = panes(&srv, "dev:edit");
    assert_eq!(p.len(), 2);
    assert_eq!(Path::new(&p[0][3]), a.join("sub"));
    assert_eq!(Path::new(&p[1][3]), b);
    assert_eq!(Path::new(&panes(&srv, "dev:logs")[0][3]), a);
}

#[test]
fn up_is_idempotent_and_does_not_rebuild() {
    let srv = TestServer::unstarted();
    write_layout(&srv, "dev", "name = \"dev\"\n[[window]]\nname = \"one\"\n");
    up_ok(&srv, "dev");
    srv.cmd(&["new-window", "-t", "dev:", "-n", "mine"])
        .unwrap();
    let again = up_ok(&srv, "dev");
    assert!(again.contains("already running"), "{again}");
    assert_eq!(windows(&srv, "dev").len(), 2, "nothing rebuilt");
    // Without -d and without a terminal: built or found, and no attach.
    let out = run(&srv, &["layout", "up", "dev"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(out.stdout.contains("not attaching"), "{}", out.stdout);
}

#[test]
fn a_session_name_override_builds_a_second_session() {
    let srv = TestServer::unstarted();
    write_layout(&srv, "dev", "name = \"dev\"\n");
    up_ok(&srv, "dev");
    let out = run(&srv, &["layout", "up", "dev", "-d", "--session", "dev2"]);
    assert!(out.status.success(), "{}", out.stderr);
    let names = lines(&srv, &["list-sessions", "-F", "#{session_name}"]);
    assert_eq!(names, ["dev", "dev2"]);
}

#[test]
fn env_levels_reach_panes_and_are_not_interpolated() {
    let srv = TestServer::unstarted();
    let out_dir = dir(&srv, "out");
    let probe = |n: &str| {
        format!(
            "printf '%s:%s:%s:%s' \\\"$S\\\" \\\"$W\\\" \\\"$P\\\" \\\"$L\\\" > {}/{n}",
            out_dir.display()
        )
    };
    write_layout(
        &srv,
        "env",
        &format!(
            r#"
name = "env"
env = {{ S = "session", L = "$(touch {pwn})" }}

[[window]]
env = {{ W = "window" }}

[[window.pane]]
env = {{ P = "pane" }}
cmd = "{first}"

[[window.pane]]
cmd = "{second}"

[[window]]
[[window.pane]]
cmd = "{third}"
"#,
            pwn = out_dir.join("pwned").display(),
            first = probe("first"),
            second = probe("second"),
            third = probe("third"),
        ),
    );
    up_ok(&srv, "env");
    let read = |n: &str| {
        let f = out_dir.join(n);
        wait_until(|| f.exists(), WAIT).unwrap_or_else(|e| panic!("{n}: {e}"));
        fs::read_to_string(f).unwrap()
    };
    let l = format!("$(touch {})", out_dir.join("pwned").display());
    assert_eq!(read("first"), format!("session:window:pane:{l}"));
    // The first pane's own variable did not leak to its sibling or the next window.
    assert_eq!(read("second"), format!("session:window::{l}"));
    assert_eq!(read("third"), format!("session:::{l}"));
    assert!(!out_dir.join("pwned").exists());
}

#[test]
fn cmd_is_typed_and_exec_is_the_pane_command() {
    let srv = TestServer::unstarted();
    write_layout(
        &srv,
        "run",
        r#"
name = "run"
[[window]]
[[window.pane]]
cmd = "echo typed-marker"
[[window.pane]]
cmd = "echo exec-marker"
exec = true
"#,
    );
    up_ok(&srv, "run");
    let p = panes(&srv, "run:");
    assert_eq!(p.len(), 2);
    let screen = |id: &str| srv.capture(id);
    wait_until(|| screen(&p[0][0]).contains("typed-marker"), WAIT).unwrap();
    // The dead pane's `remain-on-exit-format` line scrolls the screen up one
    // row, as tmux's does, so the marker may already be in the history.
    let history = |id: &str| {
        srv.cmd(&["capture-pane", "-p", "-S", "-", "-t", id])
            .unwrap_or_default()
    };
    wait_until(|| history(&p[1][0]).contains("exec-marker"), WAIT).unwrap();
    // The typed command was entered at a shell, which stays (the prompt is
    // `$` or, as root in CI, `#`); the exec pane ended with its command
    // (the harness keeps it dead).
    let typed = screen(&p[0][0]);
    assert!(
        typed.contains("echo typed-marker")
            && typed
                .lines()
                .any(|l| l.ends_with("typed-marker") && !l.contains("echo")),
        "{typed}"
    );
    assert_eq!(p[0][4], "0", "typed pane alive: {p:?}");
    wait_until(|| panes(&srv, "run:")[1][4] == "1", WAIT).expect("exec pane ended");
}

#[test]
fn focus_picks_the_window_and_the_pane() {
    let srv = TestServer::unstarted();
    write_layout(
        &srv,
        "f",
        r#"
name = "f"
[[window]]
name = "a"
[[window]]
name = "b"
focus = true
[[window.pane]]
[[window.pane]]
focus = true
[[window.pane]]
[[window]]
name = "c"
"#,
    );
    up_ok(&srv, "f");
    let w = windows(&srv, "f");
    assert!(
        w[0].starts_with("a|0|") && w[1].starts_with("b|1|") && w[2].starts_with("c|0|"),
        "{w:?}"
    );
    let p = panes(&srv, "f:b");
    assert_eq!(
        p.iter().map(|x| x[2].as_str()).collect::<Vec<_>>(),
        ["0", "1", "0"]
    );
}

#[test]
fn layout_presets_and_strings_go_through_the_codec() {
    let srv = TestServer::unstarted();
    write_layout(
        &srv,
        "l",
        r#"
name = "l"
[[window]]
name = "even"
layout = "even-horizontal"
[[window.pane]]
[[window.pane]]
[[window.pane]]
[[window]]
name = "sized"
split = "h"
[[window.pane]]
[[window.pane]]
size = "30%"
"#,
    );
    up_ok(&srv, "l");
    let p = panes(&srv, "l:even");
    let widths: Vec<u32> = p
        .iter()
        .map(|x| x[5].split(',').nth(2).unwrap().parse().unwrap())
        .collect();
    assert_eq!(widths.iter().sum::<u32>() + 2, 80, "{widths:?}");
    let q = panes(&srv, "l:sized");
    let w1: u32 = q[1][5].split(',').nth(2).unwrap().parse().unwrap();
    assert!((22..=26).contains(&w1), "30% of 80 columns, got {w1}");
}

#[test]
fn wait_for_a_regex_holds_back_the_next_pane() {
    let srv = TestServer::unstarted();
    let d = dir(&srv, "w");
    write_layout(
        &srv,
        "w",
        &format!(
            r#"
name = "w"
[[window]]
[[window.pane]]
cmd = "sleep 1; touch {d}/first; echo reg-rea\"\"dy"
wait = {{ regex = "reg-ready", timeout = 20 }}
[[window.pane]]
cmd = "test -e {d}/first && touch {d}/regex-ok || touch {d}/regex-bad"
wait = {{ path = "{d}/regex-ok", timeout = 20 }}
[[window.pane]]
cmd = "test -e {d}/regex-ok && touch {d}/path-ok || touch {d}/path-bad"
"#,
            d = d.display()
        ),
    );
    up_ok(&srv, "w");
    assert!(
        d.join("regex-ok").exists(),
        "the second pane started after the regex matched"
    );
    wait_until(
        || d.join("path-ok").exists() || d.join("path-bad").exists(),
        WAIT,
    )
    .unwrap();
    assert!(d.join("path-ok").exists());
    assert!(!d.join("regex-bad").exists());
}

#[test]
fn wait_that_times_out_fails_and_removes_the_session() {
    let srv = TestServer::unstarted();
    write_layout(
        &srv,
        "t",
        r#"
name = "t"
[[window]]
[[window.pane]]
cmd = "echo something"
wait = { regex = "never-appears", timeout = 1 }
[[window.pane]]
"#,
    );
    let out = run(&srv, &["layout", "up", "t", "-d"]);
    assert!(!out.status.success());
    assert!(out.stderr.contains("timed out after 1s"), "{}", out.stderr);
    assert!(
        !srv.cmd(&["list-sessions", "-F", "#{session_name}"])
            .unwrap_or_default()
            .contains('t')
    );
}

#[test]
fn several_sessions_in_one_file() {
    let srv = TestServer::unstarted();
    write_layout(
        &srv,
        "multi",
        r#"
[[session]]
name = "one"
[[session.window]]
name = "w1"
[[session]]
name = "two"
attach = false
[[session.window]]
name = "w2"
[[session.window.pane]]
[[session.window.pane]]
"#,
    );
    up_ok(&srv, "multi");
    let names = lines(&srv, &["list-sessions", "-F", "#{session_name}"]);
    assert_eq!(names, ["one", "two"]);
    assert_eq!(panes(&srv, "two:").len(), 2);
    // --session selects one from a multi-session file.
    let srv2 = TestServer::unstarted();
    write_layout(
        &srv2,
        "multi",
        &fs::read_to_string(layouts_dir(&srv).join("multi.toml")).unwrap(),
    );
    let out = run(&srv2, &["layout", "up", "multi", "-d", "--session", "two"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        lines(&srv2, &["list-sessions", "-F", "#{session_name}"]),
        ["two"]
    );
    let bad = run(&srv2, &["layout", "up", "multi", "-d", "--session", "nope"]);
    assert!(!bad.status.success());
}

#[test]
fn list_shows_names_files_and_sessions() {
    let srv = TestServer::unstarted();
    let a = write_layout(&srv, "alpha", "name = \"a1\"\n");
    write_layout(
        &srv,
        "beta",
        "[[session]]\nname = \"b1\"\n[[session]]\nname = \"b2\"\n",
    );
    write_layout(&srv, "broken", "oops = 1\n");
    fs::write(layouts_dir(&srv).join("notes.txt"), "x").unwrap();
    let out = run(&srv, &["layout", "list"]);
    assert!(out.status.success(), "{}", out.stderr);
    let l: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(l.len(), 3, "{l:?}");
    assert_eq!(l[0], format!("alpha\t{}\ta1", a.display()));
    assert!(
        l[1].starts_with("beta\t") && l[1].ends_with("\tb1,b2"),
        "{}",
        l[1]
    );
    assert!(
        l[2].starts_with("broken\t") && l[2].contains("\t! "),
        "{}",
        l[2]
    );
}

#[test]
fn validation_errors_carry_file_and_line_and_create_nothing() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "[[window]]\nname = \"a\"\n[[window.pane]]\ncmdd = \"x\"\n",
            ":4:",
            "unknown field",
        ),
        ("name = \"a\"\nbogus = 1\n", ":2:", "unknown field"),
        ("[[window]]\nlayout = \"nonsense\"\n", ":2:", "layout"),
        (
            "[[window]]\n[[window.pane]]\n[[window.pane]]\nwait = { regex = \"(\" }\n",
            ":4:",
            "wait.regex",
        ),
        (
            "[[window]]\n[[window.pane]]\nsize = \"30%\"\n",
            ":3:",
            "first pane",
        ),
        (
            "[[window]]\nlayout = \"tiled\"\n[[window.pane]]\n[[window.pane]]\nsize = 10\n",
            ":5:",
            "layout",
        ),
        (
            "[[window]]\n[[window.pane]]\nexec = true\n",
            ":3:",
            "needs a cmd",
        ),
        (
            "[[window]]\n[[window.float]]\nwidth = \"150%\"\n",
            ":3:",
            "at most 100",
        ),
        (
            "[[window]]\n[[window.float]]\nborder = \"fancy\"\n",
            ":3:",
            "border",
        ),
        (
            "[[window]]\n[[window.pane]]\nwait = { regex = \"a\", path = \"b\" }\n",
            ":3:",
            "exactly one",
        ),
        (
            "[[window]]\nenv = { \"A B\" = \"x\" }\n",
            ":2:",
            "variable name",
        ),
        ("[[window]]\nfocus = \"yes\"\n", ":2:", "invalid type"),
        ("name = \"a.b\"\n", ":1:", "session name"),
        (
            "[[window]]\nfocus = true\n[[window]]\nfocus = true\n",
            "",
            "more than one window",
        ),
        (
            "[[session]]\nname = \"x\"\n[[session]]\nname = \"x\"\n",
            "",
            "used twice",
        ),
        ("not toml [", ":1:", ""),
    ];
    for (i, (text, line, msg)) in cases.iter().enumerate() {
        let srv = TestServer::unstarted();
        let path = write_layout(&srv, "bad", text);
        let out = run(&srv, &["layout", "up", "bad", "-d"]);
        assert!(!out.status.success(), "case {i} succeeded: {text}");
        let file = path.display().to_string();
        assert!(
            out.stderr.contains(&format!("{file}{line}")),
            "case {i}: {:?} lacks {file}{line}",
            out.stderr
        );
        assert!(
            out.stderr.contains(msg),
            "case {i}: {:?} lacks {msg:?}",
            out.stderr
        );
        assert!(no_sessions(&srv), "case {i} created a session");
    }
}

#[test]
fn names_cannot_leave_the_layouts_directory() {
    let srv = TestServer::unstarted();
    write_layout(&srv, "ok", "name = \"ok\"\n");
    let outside = srv.config_home.join("grofe").join("outside.toml");
    fs::write(&outside, "name = \"outside\"\n").unwrap();
    for bad in [
        "../outside",
        "..",
        "a/b",
        ".hidden",
        "",
        "x/../../outside",
        "-d",
    ] {
        let up = run(&srv, &["layout", "up", bad, "-d"]);
        assert!(!up.status.success(), "up {bad:?} worked");
        let save = run(&srv, &["layout", "save", "ok", bad]);
        assert!(!save.status.success(), "save {bad:?} worked");
    }
    assert!(no_sessions(&srv));
    assert_eq!(
        fs::read_to_string(&outside).unwrap(),
        "name = \"outside\"\n"
    );
    // A session whose name is no layout name needs one given.
    srv.cmd(&["new-session", "-d", "-s", "has space"]).unwrap();
    let out = run(&srv, &["layout", "save", "has space"]);
    assert!(!out.status.success());
    assert!(out.stderr.contains("not a layout name"), "{}", out.stderr);
    assert!(
        !srv.config_home
            .join("grofe")
            .join("has space.toml")
            .exists()
    );
    // An explicit path is the user's own business.
    let p = srv.home.join("explicit.toml");
    fs::write(&p, "name = \"explicit\"\n").unwrap();
    let ok = run(&srv, &["layout", "up", p.to_str().unwrap(), "-d"]);
    assert!(ok.status.success(), "{}", ok.stderr);
}

#[test]
fn hooks_run_in_order_and_before_can_veto() {
    let srv = TestServer::unstarted();
    let d = dir(&srv, "hooks");
    write_layout(
        &srv,
        "h",
        &format!(
            r#"
name = "h"
root = "{d}"
before = "echo before >> log; test ! -e {d}/veto"
on_start = "echo start >> log; echo $GROFE_LAYOUT_SESSION >> who"
after = "echo after >> log; cat log > snapshot"
on_stop = "echo stop >> log"
[[window]]
[[window.pane]]
cmd = "echo pane >> log"
"#,
            d = d.display()
        ),
    );
    fs::write(d.join("veto"), "").unwrap();
    let out = run(&srv, &["layout", "up", "h", "-d"]);
    assert!(!out.status.success());
    assert!(no_sessions(&srv), "a failing before hook creates nothing");
    fs::remove_file(d.join("veto")).unwrap();
    up_ok(&srv, "h");
    let snap = fs::read_to_string(d.join("snapshot")).unwrap();
    assert_eq!(
        snap.lines().filter(|l| *l != "pane").collect::<Vec<_>>(),
        ["before", "before", "start", "after"]
    );
    assert_eq!(fs::read_to_string(d.join("who")).unwrap(), "h\n");
    srv.cmd(&["kill-session", "-t", "h"]).unwrap();
    wait_until(
        || fs::read_to_string(d.join("log")).is_ok_and(|l| l.contains("stop")),
        WAIT,
    )
    .expect("on_stop ran after the session ended");
}

#[test]
fn a_float_is_created_with_new_pane_and_keeps_its_geometry() {
    let srv = TestServer::unstarted();
    write_layout(
        &srv,
        "fl",
        r#"
name = "fl"
[[window]]
name = "w"
[[window.pane]]
[[window.pane]]
[[window.float]]
cmd = "sleep 60"
exec = true
x = 10
y = "10%"
width = 30
height = "25%"
title = "mon"
border = "heavy"
focus = true
"#,
    );
    up_ok(&srv, "fl");
    let p = panes(&srv, "fl:w");
    assert_eq!(p.len(), 3, "{p:?}");
    let f = &p[2];
    assert_eq!(f[1], "1", "floating: {f:?}");
    assert_eq!(f[2], "1", "the float has focus: {p:?}");
    assert_eq!(f[5], "10,2,30,6", "{f:?}");
    assert_eq!(f[6], "mon");
    let opts = srv.cmd(&["show-options", "-p", "-t", &f[0]]).unwrap();
    assert!(opts.contains("pane-border-lines heavy"), "{opts}");
}

#[test]
fn a_float_too_large_for_the_window_is_refused_as_new_pane_refuses() {
    let srv = TestServer::unstarted();
    write_layout(
        &srv,
        "big",
        "name = \"big\"\n[[window]]\n[[window.float]]\nwidth = 500\n",
    );
    let out = run(&srv, &["layout", "up", "big", "-d"]);
    assert!(!out.status.success());
    assert!(
        out.stderr.contains("size or position too large"),
        "{}",
        out.stderr
    );
    assert!(no_sessions(&srv), "the half-built session is removed");
}

#[test]
fn save_then_up_in_a_fresh_server_round_trips() {
    let a = TestServer::unstarted();
    let (d1, d2) = (dir(&a, "d1"), dir(&a, "d2"));
    write_layout(
        &a,
        "src",
        &format!(
            r#"
name = "proj"
root = "{d1}"

[[window]]
name = "code"
layout = "main-vertical"
focus = true
[[window.pane]]
[[window.pane]]
cwd = "{d2}"
focus = true
[[window.pane]]
cmd = "sleep 120"
exec = true

[[window]]
name = "single"
cwd = "{d2}"

[[window.float]]
cmd = "sleep 120"
exec = true
x = 12
y = 3
width = 40
height = 8
title = "pop"
border = "double"
"#,
            d1 = d1.display(),
            d2 = d2.display()
        ),
    );
    up_ok(&a, "src");
    let saved = run(&a, &["layout", "save", "proj", "saved"]);
    assert!(saved.status.success(), "{}", saved.stderr);
    let again = run(&a, &["layout", "save", "proj", "saved"]);
    assert!(!again.status.success(), "no overwrite without --force");
    assert!(again.stderr.contains("--force"), "{}", again.stderr);
    assert!(
        run(&a, &["layout", "save", "proj", "saved", "--force"])
            .status
            .success()
    );
    let text = fs::read_to_string(layouts_dir(&a).join("saved.toml")).unwrap();

    let b = TestServer::unstarted();
    write_layout(&b, "saved", &text);
    up_ok(&b, "saved");

    assert_eq!(
        windows(&a, "proj"),
        windows(&b, "proj"),
        "saved file:\n{text}"
    );
    for w in ["code", "single"] {
        let strip = |srv: &TestServer| {
            panes(srv, &format!("proj:{w}"))
                .into_iter()
                .map(|mut p| {
                    p.remove(0);
                    p.remove(3); // pane_dead: the exec panes are cat/sleep, alive on both
                    p
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(strip(&a), strip(&b), "window {w}, saved file:\n{text}");
    }
    let p = panes(&b, "proj:single");
    assert_eq!(
        p.iter().filter(|x| x[1] == "1").count(),
        1,
        "the float came back: {p:?}"
    );
    let f = p.iter().find(|x| x[1] == "1").unwrap();
    assert_eq!((f[5].as_str(), f[6].as_str()), ("12,3,40,8", "pop"));
    // The running window layout string, floats and all, matches.
    assert_eq!(
        lines(
            &a,
            &["list-windows", "-t", "proj", "-F", "#{window_layout}"]
        ),
        lines(
            &b,
            &["list-windows", "-t", "proj", "-F", "#{window_layout}"]
        )
    );
}

#[test]
fn the_tmux_identity_does_not_know_the_verb() {
    use std::os::unix::fs::symlink;
    let srv = TestServer::unstarted();
    let bin = srv.home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let link = bin.join("tmux");
    symlink(env!("CARGO_BIN_EXE_grofe"), &link).unwrap();
    let tmux = |args: &[&str]| {
        let mut cmd = std::process::Command::new(&link);
        cmd.arg("-S")
            .arg(&srv.socket)
            .args(args)
            .current_dir(&srv.home)
            .env_remove("GROFE")
            .env_remove("GROFE_PANE")
            .env_remove("TMUX")
            .env("HOME", &srv.home)
            .env("XDG_CONFIG_HOME", &srv.config_home)
            .env("XDG_STATE_HOME", &srv.state_home)
            .env("XDG_RUNTIME_DIR", &srv.runtime_dir)
            .env("TMUX_TMPDIR", srv.home.join("tmux-tmpdir"))
            .env("GROFE_RESTORE_ON_START", "0");
        common::run_bounded(cmd, common::CMD_TIMEOUT)
    };
    let started = tmux(&["start-server"]);
    assert!(started.status.success(), "{}", started.stderr);
    write_layout(&srv, "dev", "name = \"dev\"\n");
    for args in [&["layout", "list"][..], &["layout", "up", "dev", "-d"]] {
        let out = tmux(args);
        assert!(
            !out.status.success(),
            "tmux {args:?} worked: {}",
            out.stdout
        );
        assert!(out.stderr.contains("unknown command"), "{}", out.stderr);
    }
    let table = tmux(&["list-commands", "-F", "#{command_list_name}"]);
    assert!(
        !table.stdout.lines().any(|l| l.starts_with("layout")),
        "{}",
        table.stdout
    );
    assert!(
        srv.cmd(&["list-sessions"]).is_err()
            || !srv.cmd(&["list-sessions"]).unwrap().contains("dev")
    );
}

/// The pids of this server's `layout __watch` helpers still running.
fn watchers(srv: &TestServer) -> Vec<String> {
    let out = std::process::Command::new("ps")
        .args(["-Ao", "pid=,args="])
        .output()
        .unwrap();
    let socket = srv.socket.display().to_string();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("__watch") && l.contains(&socket))
        .map(|l| l.split_whitespace().next().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn save_by_session_name_never_resolves_a_path() {
    let srv = TestServer::unstarted();
    write_layout(&srv, "seed", "name = \"seed\"\n");
    up_ok(&srv, "seed");
    // tmux session names may hold `/` and `~`; as a default file name such a
    // name must not turn into a path outside the layouts directory.
    for name in ["~/evil", "/evil-abs"] {
        srv.cmd(&["new-session", "-d", "-s", name]).unwrap();
        let out = run(&srv, &["layout", "save", name, "--force"]);
        assert!(
            !out.status.success(),
            "save {name:?} worked: {}",
            out.stdout
        );
        assert!(out.stderr.contains("not a layout name"), "{}", out.stderr);
    }
    assert!(!srv.home.join("evil").exists());
    assert!(!Path::new("/evil-abs").exists());
}

#[test]
fn save_force_replaces_a_symlink_and_not_what_it_points_to() {
    use std::os::unix::fs::symlink;
    let srv = TestServer::unstarted();
    write_layout(&srv, "s", "name = \"s\"\n");
    up_ok(&srv, "s");
    let target = srv.home.join("precious");
    fs::write(&target, "keep").unwrap();
    let link = layouts_dir(&srv).join("x.toml");
    symlink(&target, &link).unwrap();
    let refused = run(&srv, &["layout", "save", "s", "x"]);
    assert!(
        !refused.status.success(),
        "an existing link without --force"
    );
    let out = run(&srv, &["layout", "save", "s", "x", "--force"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
    assert!(
        !fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(fs::read_to_string(&link).unwrap().contains("name = \"s\""));
}

#[test]
fn hashes_in_cwds_and_titles_are_not_formats() {
    let srv = TestServer::unstarted();
    let odd = dir(&srv, "a#{session_name}b");
    write_layout(
        &srv,
        "hash",
        &format!(
            r#"
name = "hash"
root = "{odd}"
[[window]]
name = "w"
[[window.pane]]
[[window.pane]]
[[window.float]]
title = "t#{{session_name}}#S"
"#,
            odd = odd.display()
        ),
    );
    up_ok(&srv, "hash");
    let p = panes(&srv, "hash:w");
    assert_eq!(Path::new(&p[0][3]), odd, "{p:?}");
    assert_eq!(Path::new(&p[1][3]), odd, "{p:?}");
    assert_eq!(Path::new(&p[2][3]), odd, "{p:?}");
    assert_eq!(p[2][6], "t#{session_name}#S", "{p:?}");
    let root = srv
        .cmd(&["display-message", "-p", "-t", "hash", "#{session_path}"])
        .unwrap();
    assert_eq!(Path::new(root.trim()), odd);
}

#[test]
fn the_first_pane_starts_once_with_its_env_and_the_session_dir_is_root() {
    let srv = TestServer::unstarted();
    let root = dir(&srv, "root");
    fs::create_dir_all(root.join("sub")).unwrap();
    let count = root.join("count");
    write_layout(
        &srv,
        "once",
        &format!(
            r#"
name = "once"
root = "{root}"
env = {{ S = "s" }}
[[window]]
name = "first"
cwd = "sub"
env = {{ W = "w" }}
[[window.pane]]
cmd = "echo \"$S$W\" >> {count}; exec sleep 60"
exec = true
"#,
            root = root.display(),
            count = count.display()
        ),
    );
    up_ok(&srv, "once");
    wait_until(|| count.exists(), WAIT).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(fs::read_to_string(&count).unwrap(), "sw\n", "started once");
    let w = windows(&srv, "once");
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0].starts_with("first|1|"), "{w:?}");
    let index = srv
        .cmd(&[
            "display-message",
            "-p",
            "-t",
            "once:first",
            "#{window_index}",
        ])
        .unwrap();
    let base = srv.cmd(&["show-options", "-gv", "base-index"]).unwrap();
    assert_eq!(
        index.trim(),
        base.trim(),
        "the first window has the base index"
    );
    let p = panes(&srv, "once:first");
    assert_eq!(Path::new(&p[0][3]), root.join("sub"));
    let path = srv
        .cmd(&["display-message", "-p", "-t", "once", "#{session_path}"])
        .unwrap();
    assert_eq!(Path::new(path.trim()), root, "session directory is root");
    // The window's variable is the window's: not left in the session.
    let env = srv.cmd(&["show-environment", "-t", "once"]).unwrap();
    assert!(env.contains("S=s") && !env.contains("W=w"), "{env}");
}

#[test]
fn on_stop_runs_once_when_the_server_goes_and_no_watcher_outlives_it() {
    let srv = TestServer::unstarted();
    let d = dir(&srv, "stop");
    write_layout(
        &srv,
        "st",
        &format!(
            "name = \"st\"\nroot = \"{}\"\non_stop = \"echo stop >> log\"\n",
            d.display()
        ),
    );
    up_ok(&srv, "st");
    // Running already: no second watcher.
    up_ok(&srv, "st");
    wait_until(|| watchers(&srv).len() == 1, WAIT).unwrap_or_else(|e| {
        panic!("{e}: {:?}", watchers(&srv));
    });
    srv.cmd(&["kill-server"]).unwrap();
    wait_until(|| d.join("log").exists(), WAIT).expect("on_stop ran");
    wait_until(|| watchers(&srv).is_empty(), WAIT)
        .unwrap_or_else(|e| panic!("{e}: {:?}", watchers(&srv)));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(fs::read_to_string(d.join("log")).unwrap(), "stop\n");
}

#[test]
fn a_failed_build_still_runs_on_stop_for_the_session_it_removed() {
    let srv = TestServer::unstarted();
    let d = dir(&srv, "fail");
    write_layout(
        &srv,
        "bad",
        &format!(
            "name = \"bad\"\nroot = \"{}\"\nbefore = \"echo before >> log\"\non_stop = \"echo stop >> log\"\n[[window]]\n[[window.float]]\nwidth = 500\n",
            d.display()
        ),
    );
    let out = run(&srv, &["layout", "up", "bad", "-d"]);
    assert!(!out.status.success());
    wait_until(
        || fs::read_to_string(d.join("log")).is_ok_and(|l| l.contains("stop")),
        WAIT,
    )
    .expect("on_stop undid what before did");
    assert_eq!(fs::read_to_string(d.join("log")).unwrap(), "before\nstop\n");
}

/// A source session for the frames round trips: three tiled panes side
/// by side, which [`save_and_up`] makes no preset's: the first wider by
/// hand, the last split again below.
const FRAMED_SRC: &str = r#"
name = "fr"
[[window]]
name = "w"
split = "h"
[[window.pane]]
[[window.pane]]
[[window.pane]]
"#;

/// Saves `fr` from `from` and brings it up in `to`; returns the saved
/// file's text after checking `up` worked and gave three tiled panes in
/// the source's arrangement.
fn save_and_up(from: &TestServer, to: &TestServer) -> String {
    write_layout(from, "src", FRAMED_SRC);
    up_ok(from, "src");
    let ids: Vec<String> = panes(from, "fr:w")
        .into_iter()
        .map(|p| p[0].clone())
        .collect();
    from.cmd(&["resize-pane", "-t", &ids[0], "-R", "6"])
        .unwrap();
    from.cmd(&["split-window", "-d", "-v", "-t", &ids[2]])
        .unwrap();
    let saved = run(from, &["layout", "save", "fr", "saved"]);
    assert!(saved.status.success(), "{}", saved.stderr);
    let text = fs::read_to_string(layouts_dir(from).join("saved.toml")).unwrap();
    write_layout(to, "saved", &text);
    let out = run(to, &["layout", "up", "saved", "-d"]);
    assert!(
        out.status.success(),
        "layout up: {}\nsaved:\n{text}",
        out.stderr
    );
    let rects = |srv: &TestServer| -> Vec<Vec<u32>> {
        panes(srv, "fr:w")
            .iter()
            .map(|p| p[5].split(',').map(|n| n.parse().unwrap()).collect())
            .collect()
    };
    let (a, b) = (rects(from), rects(to));
    assert_eq!(b.len(), 4, "{b:?}");
    // [x, y, w, h]: each pair of panes sits the same way round.
    for i in 0..4 {
        for j in 0..4 {
            let way = |r: &[Vec<u32>]| (r[i][0].cmp(&r[j][0]), r[i][1].cmp(&r[j][1]));
            assert_eq!(way(&a), way(&b), "from {a:?} to {b:?}");
        }
    }
    // The hand-made width comes across, give or take the frames.
    assert!(a[0][2].abs_diff(b[0][2]) <= 3, "from {a:?} to {b:?}");
    text
}

#[test]
fn a_layout_saved_from_a_framed_window_comes_up_unframed() {
    let framed = TestServer::builder().frames(true).unstarted();
    let plain = TestServer::unstarted();
    let text = save_and_up(&framed, &plain);
    // What is stored is the framed window's own `#{window_layout}` (its
    // content rects, the root inset from the corner), which a framed
    // window takes back exactly.
    assert!(text.contains(&srv_layout(&framed, "fr:w")), "{text}");
    // The target is unframed: its first pane is at the corner.
    let first = &panes(&plain, "fr:w")[0][5];
    assert!(first.starts_with("0,0,"), "{first}");
}

#[test]
fn a_layout_saved_from_an_unframed_window_comes_up_framed() {
    let plain = TestServer::unstarted();
    let framed = TestServer::builder().frames(true).unstarted();
    save_and_up(&plain, &framed);
    // The target really is framed: its content is inset from the corner.
    let first = &panes(&framed, "fr:w")[0][5];
    assert!(first.starts_with("2,2,"), "{first}");
}

#[test]
fn a_framed_save_round_trips_exactly_into_a_framed_window() {
    let a = TestServer::builder().frames(true).unstarted();
    let b = TestServer::builder().frames(true).unstarted();
    let text = save_and_up(&a, &b);
    assert_eq!(windows(&a, "fr"), windows(&b, "fr"), "saved:\n{text}");
}

fn srv_layout(srv: &TestServer, target: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, "#{window_layout}"])
        .unwrap()
        .trim()
        .to_string()
}

/// A tmux-identity server whose config frames its windows. `grofe
/// layout` speaks grofe's CLI to it, so it still sees `pane-frames`.
fn tmux_identity_framed() -> TestServer {
    let srv = TestServer::builder().frames(true).unstarted();
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("config.toml"),
        "compat-identity = \"tmux\"\npane-frames = true\n",
    )
    .unwrap();
    srv
}

#[test]
fn a_framed_tmux_identity_window_takes_a_layout_framed() {
    let plain = TestServer::unstarted();
    let framed = tmux_identity_framed();
    save_and_up(&plain, &framed);
    // Framed, with tmux identity's gap of 0: one cell in.
    let first = &panes(&framed, "fr:w")[0][5];
    assert!(first.starts_with("1,1,"), "{first}");
}

#[test]
fn a_framed_tmux_identity_window_saves_a_layout_that_comes_up_unframed() {
    let framed = tmux_identity_framed();
    let plain = TestServer::unstarted();
    save_and_up(&framed, &plain);
    let first = &panes(&plain, "fr:w")[0][5];
    assert!(first.starts_with("0,0,"), "{first}");
}
