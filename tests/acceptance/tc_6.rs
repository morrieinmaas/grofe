//! TC.6: popups and floating panes. The expected screens are tmux 3.7b's,
//! recorded on a private server with a pty client of the same size: a
//! popup at 80% by 60% with a rounded border and title runs in its `-d`
//! directory; `-c <client>` opens on that client only (D15); inside a
//! popup, attaching another session shows it and detaching returns to the
//! popup's shell; a hidden environment variable stays out of the popup,
//! as tmux 3.7b keeps it out; `-E` closes on exit, otherwise Escape or
//! C-c close; closing hangs up the command's process group, leaving no
//! zombie; the fzf-shaped `new-pane` lands where tmux puts its floating
//! pane, with its size and environment.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::{TestServer, wait_child, wait_until};

use crate::harness::Harness;

const WAIT: Duration = Duration::from_secs(10);

/// A server with session `main` running `cat`, sized for a 60x15 client.
fn start() -> TestServer {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "60",
        "-y",
        "14",
        "cat",
    ])
    .expect("new-session");
    srv
}

fn attach(srv: &TestServer, session: &str) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), session, &[], 60, 15);
    a.wait_until("the status row", WAIT, |text| {
        text.lines().last().unwrap_or_default().starts_with('[')
    });
    a
}

/// The name of the client attached last.
fn last_client(srv: &TestServer) -> String {
    let out = srv
        .cmd(&["list-clients", "-F", "#{client_name}"])
        .expect("list-clients");
    out.lines().last().expect("a client").to_string()
}

/// `display-popup ...` from the CLI, which waits until the popup closes.
fn popup(srv: &TestServer, args: &[&str]) -> Child {
    let mut cmd = srv.command();
    cmd.arg("display-popup")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn().expect("spawn display-popup")
}

fn rows(text: &str) -> Vec<String> {
    text.lines().map(|l| l.trim_end().to_string()).collect()
}

/// A file a popup's command writes, once it holds a whole line.
fn read(path: &Path) -> String {
    wait_until(
        || fs::read_to_string(path).is_ok_and(|s| s.ends_with('\n')),
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    fs::read_to_string(path).expect("read")
}

fn canonical_dir(srv: &TestServer, name: &str) -> PathBuf {
    let dir = srv.home.join(name);
    fs::create_dir_all(&dir).expect("mkdir");
    dir.canonicalize().expect("canonical")
}

/// Whether process `pid` still exists, a zombie included (`ps` lists one
/// until its parent reaps it).
fn exists(pid: &str) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .expect("ps");
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

/// Case 1: `-w 80% -h 60% -b rounded -T title -d <dir>` on a 60x15
/// client, cell for cell as tmux 3.7b draws it (48x9 at (5, 2)), and the
/// command runs in `<dir>`. Keys go to the command; Escape closes a popup
/// without `-E`, and the CLI then answers 129, the hung-up command's
/// status, as tmux's does.
#[test]
fn a_rounded_popup_with_a_title_runs_in_its_directory() {
    let srv = start();
    let mut a = attach(&srv, "main");
    let dir = canonical_dir(&srv, "popdir");
    let pwd = srv.home.join("pwd.txt");
    let script = format!("pwd > {}; echo hi; exec cat", pwd.display());
    let child = popup(
        &srv,
        &[
            "-w",
            "80%",
            "-h",
            "60%",
            "-b",
            "rounded",
            "-T",
            "title",
            "-d",
            dir.to_str().expect("utf-8"),
            &script,
        ],
    );
    a.wait_for("│hi");
    let screen = rows(&a.text());
    let side = format!("     │{:46}│", "");
    let mut want = vec![String::new(), String::new()];
    want.push(format!("     ╭─title{}╮", "─".repeat(40)));
    want.push(format!("     │hi{:44}│", ""));
    want.extend(std::iter::repeat_n(side, 6));
    want.push(format!("     ╰{}╯", "─".repeat(46)));
    assert_eq!(screen[..11], want[..], "screen:\n{}", screen.join("\n"));
    assert_eq!(a.cursor(), (6, 4), "the popup's cursor, after its output");
    assert_eq!(read(&pwd).trim_end(), dir.to_str().expect("utf-8"));

    a.send("x");
    a.wait_for("│x");
    a.send("\x1b");
    a.wait_until("the popup to close", WAIT, |t| !t.contains("title"));
    let status = wait_child(child, WAIT).expect("display-popup answers");
    assert_eq!(status.code(), Some(129));
}

/// Case 2 (D15): with two clients attached, `-c` shows the popup on the
/// client it names and nowhere else.
#[test]
fn a_popup_opens_on_the_client_it_names_only() {
    let srv = start();
    let mut a = attach(&srv, "main");
    let mut b = attach(&srv, "main");
    let name = last_client(&srv);
    let child = popup(
        &srv,
        &["-c", &name, "-w", "30", "-h", "5", "echo only-b; exec cat"],
    );
    b.wait_for("│only-b");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!a.text().contains("only-b"), "A shows B's popup");
    b.send("\x03");
    b.wait_until("the popup to close", WAIT, |t| !t.contains("only-b"));
    wait_child(child, WAIT).expect("display-popup answers");
}

/// Case 3 (spec §4.6): a shell in a popup attaches another session, which
/// then shows inside the popup; detaching returns to the popup's shell,
/// and the outer client stays on its own session throughout. Attaching
/// the popup's own session is refused, as a pane's would be.
#[test]
fn attaching_from_a_popup_shows_the_session_and_detaching_returns() {
    let srv = start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "other",
        "-x",
        "60",
        "-y",
        "14",
        "sh",
        "-c",
        "echo OTHER; exec cat",
    ])
    .expect("new-session other");
    let mut a = attach(&srv, "main");
    let child = popup(&srv, &["-w", "40", "-h", "10", "/bin/sh"]);
    a.wait_for("│");
    let grofe = env!("CARGO_BIN_EXE_grofe");
    let sock = srv.socket.display().to_string();
    a.send(format!("PS1='$ '; {grofe} -S {sock} attach -t other\r"));
    a.wait_for("│OTHER");
    a.wait_for("│[other]");
    let sessions = srv
        .cmd(&["list-clients", "-F", "#{session_name}"])
        .expect("list-clients");
    assert_eq!(sessions.lines().next(), Some("main"), "{sessions}");
    assert!(sessions.lines().any(|s| s == "other"), "{sessions}");

    a.send("\x02d");
    a.wait_for("detached (from session other)");
    a.wait_until("the popup's shell again", WAIT, |t| {
        !t.contains("│OTHER") && t.lines().last().unwrap_or("").starts_with("[main]")
    });
    let sessions = srv
        .cmd(&["list-clients", "-F", "#{session_name}"])
        .expect("list-clients");
    assert_eq!(sessions.trim_end(), "main");

    a.send(format!("{grofe} -S {sock} attach -t main\r"));
    a.wait_for("refusing to attach session main");
    a.send("\x03");
    assert_eq!(wait_child(child, WAIT).expect("answers").code(), Some(129));
}

/// Case 4: a hidden variable (`set-environment -gh`) stays out of the
/// popup, as tmux 3.7b keeps it out of a popup's command (recorded); a
/// visible global and a session variable reach it, and so does `TMUX`,
/// while `TMUX_PANE` does not.
#[test]
fn a_popup_gets_the_visible_environment_and_not_the_hidden() {
    let srv = start();
    srv.cmd(&["set-environment", "-gh", "GROFE_TC6_SECRET", "x"])
        .expect("hidden");
    srv.cmd(&["set-environment", "-g", "GROFE_TC6_VIS", "y"])
        .expect("global");
    srv.cmd(&["set-environment", "-t", "main", "GROFE_TC6_SESS", "z"])
        .expect("session");
    // A server started from inside a multiplexer inherits its TMUX_PANE,
    // which a popup would get as tmux's would.
    srv.cmd(&["set-environment", "-gu", "TMUX_PANE"])
        .expect("unset");
    let _a = attach(&srv, "main");
    let out = srv.home.join("env.txt");
    let script = format!(
        "echo \"[$GROFE_TC6_SECRET][$GROFE_TC6_VIS][$GROFE_TC6_SESS][${{TMUX:+tmux}}][$TMUX_PANE]\" > {}",
        out.display()
    );
    let child = popup(&srv, &["-E", &script]);
    assert!(wait_child(child, WAIT).expect("answers").success());
    assert_eq!(read(&out), "[][y][z][tmux][]\n");

    // -e sets variables, a later one winning; one without `=` is ignored
    // (recorded).
    let out = srv.home.join("env-e.txt");
    let script = format!(
        "echo \"[$GROFE_TC6_VIS][$GROFE_TC6_E]\" > {}",
        out.display()
    );
    let args = [
        "-E",
        "-e",
        "GROFE_TC6_VIS",
        "-e",
        "GROFE_TC6_E=a",
        "-e",
        "GROFE_TC6_E=b",
        &script,
    ];
    let child = popup(&srv, &args);
    assert!(wait_child(child, WAIT).expect("answers").success());
    assert_eq!(read(&out), "[y][b]\n");
}

/// Case 5: `-E` closes when the command exits, and the CLI gets its exit
/// status; while it runs, Escape and C-c go to the command. Without `-E`
/// the popup stays after its command exits, other keys do nothing, and
/// C-c closes it.
#[test]
fn close_on_exit_and_the_close_keys() {
    let srv = start();
    let mut a = attach(&srv, "main");
    let status = srv.run(&["display-popup", "-E", "exit 3"]).status;
    assert_eq!(status.code(), Some(3));

    let child = popup(
        &srv,
        &[
            "-E",
            "-w",
            "20",
            "-h",
            "5",
            "trap 'echo INT' INT; echo ready; while :; do read x && [ \"$x\" = end ] && exit 0; done",
        ],
    );
    a.wait_for("│ready");
    a.send("\x1b");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        a.text().contains("│ready"),
        "Escape closed a running -E popup"
    );
    a.send("\x03");
    a.wait_for("INT");
    a.send("end\r");
    a.wait_until("the popup to close", WAIT, |t| !t.contains("│ready"));
    assert!(wait_child(child, WAIT).expect("answers").success());

    let mut b = attach(&srv, "main");
    let child = popup(
        &srv,
        &["-c", &last_client(&srv), "-w", "20", "-h", "5", "echo done"],
    );
    b.wait_for("│done");
    b.send("q");
    std::thread::sleep(Duration::from_millis(300));
    assert!(b.text().contains("│done"), "q closed the popup");
    b.send("\x03");
    b.wait_until("the popup to close", WAIT, |t| !t.contains("│done"));
    assert!(wait_child(child, WAIT).expect("answers").success());
}

/// Closing a popup hangs up its process group: the command and a job it
/// started in the background both go, and neither is left a zombie.
#[test]
fn closing_a_popup_ends_its_process_group_without_zombies() {
    let srv = start();
    let mut a = attach(&srv, "main");
    let pids = srv.home.join("pids.txt");
    let script = format!("sleep 300 & echo $$ $! > {}; echo up; wait", pids.display());
    let child = popup(&srv, &["-w", "20", "-h", "5", &script]);
    a.wait_for("│up");
    let line = read(&pids);
    let pids: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(pids.len(), 2, "{line:?}");
    assert!(pids.iter().all(|p| exists(p)));
    a.send("\x1b");
    wait_until(|| pids.iter().all(|p| !exists(p)), WAIT)
        .unwrap_or_else(|e| panic!("popup processes still there: {e}"));
    assert_eq!(wait_child(child, WAIT).expect("answers").code(), Some(129));
}

/// A popup's output reaches the client only as cells: a title or an OSC
/// the command writes is not passed on raw.
#[test]
fn a_popup_cannot_write_to_the_terminal() {
    let srv = start();
    let mut a = attach(&srv, "main");
    // The reviewer's OSC 9 (a C1 CSI and ST and a CR/LF in the body), an
    // OSC 777 with a C1 CSI, an OSC 52 store and two titles.
    let script = srv.home.join("osc.sh");
    fs::write(
        &script,
        "printf '\\033]9;X\\302\\2332J\\302\\234\\r\\n\\007'\n\
         printf '\\033]777;notify;T;B\\302\\233x\\007'\n\
         printf '\\033]52;c;aGk=\\007'\n\
         printf '\\033]0;tc6-evil\\007\\033]2;tc6-evil2\\033\\\\'\n\
         echo seen; exec cat\n",
    )
    .expect("script");
    let command = format!("sh {}", script.display());
    let child = popup(&srv, &["-w", "30", "-h", "5", &command]);
    a.wait_for("│seen");
    let raw = a.output().to_vec();
    let has = |needle: &[u8]| raw.windows(needle.len()).any(|w| w == needle);
    assert!(!has(b"tc6-evil"), "a popup title reached the terminal");
    assert!(
        !has("\u{9b}".as_bytes()) && !has("\u{9c}".as_bytes()),
        "a C1 control"
    );
    assert!(has(b"\x1b]9;X2J\x07"), "OSC 9 not relayed clean");
    assert!(
        has(b"\x1b]777;notify;T;Bx\x07"),
        "OSC 777 not relayed clean"
    );
    assert!(has(b"\x1b]52;c;aGk=\x07"), "OSC 52 not relayed");
    a.send("\x1b");
    wait_child(child, WAIT).expect("answers");
}

/// Case 7 (M36-R5): `-x R -y P` with the right pane of a `split-window -h`
/// current puts the box's right edge on the pane's and its bottom on the
/// pane's, as tmux 3.7b does (recorded: a 20x5 box at (40, 9)).
#[test]
fn position_letters_follow_the_current_pane() {
    let srv = start();
    let mut a = attach(&srv, "main");
    srv.cmd(&["split-window", "-h", "-t", "main", "cat"])
        .expect("split");
    let child = popup(
        &srv,
        &[
            "-x",
            "R",
            "-y",
            "P",
            "-w",
            "20",
            "-h",
            "5",
            "echo RP; exec cat",
        ],
    );
    a.wait_for("│RP");
    let screen = rows(&a.text());
    let left = format!("{:30}│{:9}", "", "");
    let edge = "─".repeat(18);
    let want = [
        format!("{left}┌{edge}┐"),
        format!("{left}│RP{:16}│", ""),
        format!("{left}│{:18}│", ""),
        format!("{left}│{:18}│", ""),
        format!("{left}└{edge}┘"),
    ];
    assert_eq!(screen[9..14], want[..], "screen:\n{}", screen.join("\n"));
    a.send("\x1b");
    wait_child(child, WAIT).expect("answers");
}

/// Case 6 (M36-R1): fzf 0.74's flow over `new-pane -P -F #{pane_id} -t
/// %N -c <dir> -x -y -X -Y sh -c ...`, a tmux 3.7 floating pane. As tmux
/// 3.7b does (recorded, `-x 30 -y 8 -X 5 -Y 3`): it prints the new pane's
/// id, which is the command's `TMUX_PANE` and takes `set-option -p`; the
/// command runs in `<dir>` on a 30x8 pty whose border sits around the
/// window cells from (5, 3); it is listed and counted with the window's
/// panes; keys go to it; with `remain-on-exit off` it goes when its
/// command exits, and the tiled pane is active again. fzf then waits with
/// `wait-for`, which is TC.9's and not merged: a file stands in for it.
#[test]
fn an_fzf_shaped_new_pane_is_a_floating_pane() {
    let srv = start();
    let mut a = attach(&srv, "main");
    let dir = canonical_dir(&srv, "npdir");
    let out = srv.home.join("np.txt");
    let done = srv.home.join("np.done");
    let grofe = env!("CARGO_BIN_EXE_grofe");
    let sock = srv.socket.display().to_string();
    let script = format!(
        "{grofe} -S {sock} set-option -p -t \"$TMUX_PANE\" remain-on-exit off; \
         {grofe} -S {sock} set-option -p -t \"$TMUX_PANE\" @fzf-border-label picker; \
         echo \"$TMUX_PANE $(pwd) $(stty size)\" > {out}; echo in-np; read line; \
         echo \"got $line\" >> {out}; touch {done}",
        out = out.display(),
        done = done.display(),
    );
    let target = srv
        .cmd(&["display-message", "-p", "-t", "main", "#{pane_id}"])
        .expect("pane id");
    let target = target.trim_end();
    let printed = srv
        .cmd(&[
            "new-pane",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            target,
            "-c",
            dir.to_str().expect("utf-8"),
            "-x",
            "30",
            "-y",
            "8",
            "-X",
            "5",
            "-Y",
            "3",
            "sh",
            "-c",
            &script,
        ])
        .expect("new-pane");
    let id = printed.trim_end();
    assert!(id.starts_with('%') && id != target, "{printed:?}");
    a.wait_for("│in-np");
    let screen = rows(&a.text());
    let side = format!("    │{:30}│", "");
    let mut want = vec![String::new(), String::new()];
    want.push(format!("    ┌{}┐", "─".repeat(30)));
    want.push(format!("    │in-np{:25}│", ""));
    want.extend(std::iter::repeat_n(side, 7));
    want.push(format!("    └{}┘", "─".repeat(30)));
    assert_eq!(screen[..12], want[..], "screen:\n{}", screen.join("\n"));
    assert_eq!(
        read(&out).trim_end(),
        format!("{id} {} 8 30", dir.display()),
        "TMUX_PANE, cwd and pty size"
    );
    let label = srv
        .cmd(&["display-message", "-p", "-t", id, "#{@fzf-border-label}"])
        .expect("label");
    assert_eq!(label.trim_end(), "picker");
    let panes = srv
        .cmd(&[
            "list-panes", "-t", "main", "-F",
            "#{pane_id} #{pane_left} #{pane_top} #{pane_width} #{pane_height} #{pane_floating_flag} #{pane_active} #{window_panes}",
        ])
        .expect("list-panes");
    assert_eq!(
        panes,
        format!("{target} 0 0 60 14 0 0 2\n{id} 5 3 30 8 1 1 2\n")
    );
    a.send("picked\r");
    wait_until(|| done.exists(), WAIT).expect("the command finished");
    a.wait_until("the floating pane to close", WAIT, |t| !t.contains("in-np"));
    assert!(read(&out).contains("got picked"));
    let panes = srv
        .cmd(&[
            "list-panes",
            "-t",
            "main",
            "-F",
            "#{pane_id} #{pane_active}",
        ])
        .expect("list-panes");
    assert_eq!(panes, format!("{target} 1\n"));
}

/// A floating pane belongs to its window, as tmux 3.7b's does: every
/// client showing the window draws it, the prefix and bindings reach it
/// (prefix `c` from inside it makes a window), and it outlives a detach.
/// A split of it is refused with tmux's text.
#[test]
fn a_floating_pane_is_the_windows_on_every_client() {
    let srv = start();
    let mut a = attach(&srv, "main");
    let mut b = attach(&srv, "main");
    srv.cmd(&[
        "new-pane",
        "-x",
        "20",
        "-y",
        "4",
        "-X",
        "5",
        "-Y",
        "2",
        "sh",
        "-c",
        "echo floaty; exec cat",
    ])
    .expect("new-pane");
    a.wait_for("│floaty");
    b.wait_for("│floaty");
    let float = srv
        .cmd(&["display-message", "-p", "-t", "main", "#{pane_id}"])
        .expect("active pane");
    let float = float.trim_end();
    assert_eq!(
        srv.cmd(&["split-window", "-t", float]),
        Err("size or position can't split a floating pane\n".to_string())
    );

    a.send("typed");
    a.wait_for("│typed");
    b.wait_for("│typed");
    a.send("\x02c");
    wait_until(
        || {
            srv.cmd(&["list-windows", "-t", "main"])
                .is_ok_and(|w| w.lines().count() == 2)
        },
        WAIT,
    )
    .expect("prefix c made a window");

    a.send("\x02d");
    a.wait_exit();
    drop(b);
    let panes = srv
        .cmd(&[
            "list-panes",
            "-t",
            "main:1",
            "-F",
            "#{pane_id} #{pane_floating_flag}",
        ])
        .expect("list-panes");
    assert!(panes.contains(&format!("{float} 1")), "{panes}");
}

/// Several floating panes stack as tmux 3.7b's do (recorded): the newest
/// on top and active; selecting a lower one raises it; with the tiled
/// pane active they stay drawn over it in their order.
#[test]
fn floating_panes_stack_and_rise_when_selected() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "60",
        "-y",
        "14",
        "sh",
        "-c",
        "echo TILED; exec cat",
    ])
    .expect("new-session");
    let mut a = attach(&srv, "main");
    let tiled = srv
        .cmd(&["display-message", "-p", "-t", "main", "#{pane_id}"])
        .expect("pane");
    let first = srv
        .cmd(&[
            "new-pane",
            "-P",
            "-F",
            "#{pane_id}",
            "-x",
            "20",
            "-y",
            "5",
            "-X",
            "5",
            "-Y",
            "2",
            "sh",
            "-c",
            "echo F1; exec cat",
        ])
        .expect("first");
    srv.cmd(&[
        "new-pane",
        "-x",
        "20",
        "-y",
        "5",
        "-X",
        "15",
        "-Y",
        "4",
        "sh",
        "-c",
        "echo F2; exec cat",
    ])
    .expect("second");
    a.wait_for("│F2");
    let top = [
        "TILED",
        "    ┌────────────────────┐",
        "    │F1                  │",
        "    │         ┌────────────────────┐",
        "    │         │F2                  │",
        "    │         │                    │",
        "    │         │                    │",
        "    └─────────│                    │",
        "              │                    │",
        "              └────────────────────┘",
    ];
    assert_eq!(rows(&a.text())[..10], top[..]);
    srv.cmd(&["select-pane", "-t", first.trim_end()])
        .expect("select");
    let raised = [
        "TILED",
        "    ┌────────────────────┐",
        "    │F1                  │",
        "    │                    │─────────┐",
        "    │                    │         │",
        "    │                    │         │",
        "    │                    │         │",
        "    └────────────────────┘         │",
        "              │                    │",
        "              └────────────────────┘",
    ];
    a.wait_until("F1 on top", WAIT, |t| rows(t)[..10] == raised[..]);
    srv.cmd(&["select-pane", "-t", tiled.trim_end()])
        .expect("select tiled");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        rows(&a.text())[..10],
        raised[..],
        "still drawn over the tiled pane"
    );
}

/// With `mouse` on, a report inside the popup reaches a command that asked
/// for mouse reports, at the popup cell, as tmux 3.7b forwards it
/// (recorded); one outside the popup does not.
#[test]
fn a_popup_gets_mouse_reports_inside_it() {
    let srv = start();
    srv.cmd(&["set", "-g", "mouse", "on"]).expect("mouse on");
    let mut a = attach(&srv, "main");
    let child = popup(
        &srv,
        &[
            "-w",
            "30",
            "-h",
            "6",
            "printf '\\033[?1000h\\033[?1006hready\\n'; stty -echo -icanon; exec cat -v",
        ],
    );
    a.wait_for("│ready");
    // The box is at (14, 4), so its inside starts at (15, 5).
    a.send("\x1b[<0;2;2M\x1b[<0;2;2m");
    a.send("\x1b[<0;21;7M\x1b[<0;21;7m");
    a.wait_for("^[[<0;6;2M^[[<0;6;2m");
    assert!(!a.text().contains(";2;2"), "a report outside the popup");
    a.send("\x03");
    wait_child(child, WAIT).expect("answers");
}

/// `kill-server` with a popup whose command ignores the hangup still ends
/// it: after the grace period its process group is killed, and nothing is
/// left behind.
#[test]
fn kill_server_ends_a_popup_that_ignores_the_hangup() {
    let srv = start();
    let mut a = attach(&srv, "main");
    let pids = srv.home.join("hup.txt");
    let script = format!(
        "trap '' HUP; echo $$ > {}; echo up; while :; do sleep 1; done",
        pids.display()
    );
    let child = popup(&srv, &["-w", "30", "-h", "5", &script]);
    a.wait_for("│up");
    let pid = read(&pids).trim_end().to_string();
    assert!(exists(&pid));
    srv.cmd(&["kill-server"]).expect("kill-server");
    wait_until(|| !exists(&pid), WAIT)
        .unwrap_or_else(|e| panic!("popup command {pid} outlived the server: {e}"));
    let _ = wait_child(child, WAIT);
    drop(a);
}

/// A client resize fits the popup as tmux 3.7b does (recorded, 80x24 to
/// 40x12 and back): no larger than the client, moved until it fits, over
/// the status line when it must be, and its pty resized (the command sees
/// the SIGWINCH); with room again it goes back to its own size and place.
#[test]
fn a_client_resize_fits_the_popup_and_its_pty() {
    let srv = start();
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    a.wait_until("the status row", WAIT, |t| {
        t.lines().nth(23).unwrap_or_default().starts_with('[')
    });
    let child = popup(
        &srv,
        &[
            "-w",
            "60",
            "-h",
            "16",
            "-T",
            "ti",
            "trap 'stty size' WINCH; stty size; while :; do sleep 0.2; done",
        ],
    );
    a.wait_for("│14 58");
    assert_eq!(
        rows(&a.text())[3],
        format!("         ┌─ti{}┐", "─".repeat(55))
    );

    a.resize(40, 12);
    a.wait_for("│10 38");
    let screen = rows(&a.text());
    assert_eq!(screen[0], format!("┌─ti{}┐", "─".repeat(35)), "{screen:#?}");
    assert_eq!(screen[1], format!("│14 58{:33}│", ""));
    assert_eq!(
        screen[11],
        format!("└{}┘", "─".repeat(38)),
        "over the status line"
    );

    a.resize(80, 24);
    a.wait_until("the popup's own size again", WAIT, |t| {
        t.lines().nth(3).unwrap_or("").starts_with("         ┌─ti─")
            && t.lines()
                .nth(6)
                .unwrap_or("")
                .starts_with("         │14 58")
    });
    a.send("\x03");
    wait_child(child, WAIT).expect("answers");
}
