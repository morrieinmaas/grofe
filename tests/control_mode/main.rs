//! tmux control mode (`-C`, docs/specs/2026-11-15-control-mode.md): each
//! case's command script runs against grofe through a `tmux` link, and the
//! transcript must match the one recorded from real tmux 3.7b once the
//! things that differ by run (times, command numbers, output chunking, the
//! fixtures path, a pty client's name) are normalised.
//!
//! The goldens are under `tests/fixtures/compat/golden/control_mode/`,
//! recorded from the compat container's tmux by
//! `scripts/record-compat-goldens.sh control_mode` (which runs
//! [`record_control_goldens`]). Nothing here touches a default tmux server.

#[path = "../common/mod.rs"]
mod common;
mod harness;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use harness::{Case, Client, fixtures_dir, grofe_side, normalise, real_side, run_case, session};

#[test]
fn normalise_drops_times_and_merges_output() {
    let raw = "%begin 1794733318 285 0\n%end 1794733318 285 0\n\
               %output %0 hi\\015\n%output %0 \\012\n%output %1 x\n%output %0 y\n\
               %begin 5 6 1\nparse error: x\n%error 5 6 1\n\
               %client-detached /dev/pts/3\n";
    assert_eq!(
        normalise(raw, Some("/dev/pts/3")),
        "%begin T N 0\n%end T N 0\n%output %0 hi\\015\\012\n%output %1 x\n%output %0 y\n\
         %begin T N 1\nparse error: x\n%error T N 1\n%client-detached CLIENT\n"
    );
}

const ATTACH: &[&str] = &["-C", "attach", "-t", "main"];

/// Every recorded case.
const CASES: &[Case] = &[
    Case {
        name: "commands",
        setup: &[],
        pty: None,
        start: ATTACH,
        script: &[
            "list-windows -F '#{window_id} #{window_name} #{window_layout}'",
            "new-window -d -n w2 'sleep 999'",
            "split-window -d -h -t @0 'sleep 998'",
            "select-window -t @1",
            "select-window -t @0",
            "rename-window -t @1 renamed",
            "send-keys -t %0 hi Enter",
            "bogus",
            "display -p a ; kill-window -t @99 ; display -p c",
            "display -p a ; bogus ; display -p c",
            "display -p a ; refresh-client -Z",
            "display -p a ; switch-client -X ; display -p c",
            "refresh-client -t nosuch -C 90x20",
            "switch-client -t %0",
            "switch-client -t @0",
            "switch-client -t '$0'",
            "refresh-client -Z",
            "refresh-client -C",
            "refresh-client -B",
            "detach-client -Z",
            "switch-client -X",
            "new-session -d -s two 'sleep 997'",
            "switch-client -ttwo",
            "kill-window -t @1",
            "kill-session -t two",
        ],
    },
    // Panes, sizing, and the flow control of `refresh-client`.
    Case {
        name: "flow",
        setup: &[],
        pty: None,
        start: ATTACH,
        script: &[
            "split-window -h -t @0 cat",
            "select-pane -t %0",
            "kill-pane -t %1",
            "refresh-client -C 100x30",
            "refresh-client -C 0x0",
            "refresh-client -C abc",
            "refresh-client -C @0:90x25",
            "refresh-client -C ' 0x 0'",
            "refresh-client -f pause-after=1",
            "send-keys -t %0 abc Enter",
            "refresh-client -A '%0:pause'",
            "send-keys -t %0 def Enter",
            "refresh-client -A '%0:continue'",
            "send-keys -t %0 DEF Enter",
            "refresh-client -A '%0:off'",
            "send-keys -t %0 ghi Enter",
            "refresh-client -A '%0:on'",
            "refresh-client -f no-output",
            "send-keys -t %0 jkl Enter",
            "refresh-client -f '!no-output'",
            "send-keys -t %0 JKL Enter",
            "refresh-client -F no-output",
            "send-keys -t %0 mno Enter",
            "refresh-client -F '!no-output'",
            "refresh-client -A '%0:bogus'",
            "refresh-client -A '%99:pause'",
            "refresh-client -A 'x:pause'",
            "refresh-client -A '%0:continue'",
            "refresh-client -A '% 0:pause'",
            "refresh-client -A '%0:continue'",
            "refresh-client -A '%0:pause'",
            "refresh-client -A '%0:pause'",
            "refresh-client -A '%0:on'",
            "send-keys -t %0 pqr Enter",
            "refresh-client -A '%0:off'",
            "refresh-client -A '%0:continue'",
            "send-keys -t %0 stu Enter",
            "refresh-client -A '%0:on'",
            "refresh-client -A '%0:continue'",
            "send-keys -t %0 vwx Enter",
            "detach-client",
        ],
    },
    // `-C new-session` creates the session it attaches to.
    Case {
        name: "new-session",
        setup: &[],
        pty: None,
        start: &["-C", "new-session", "-s", "x", "-n", "w0", "cat"],
        script: &[
            "list-windows -F '#{window_id} #{window_name}'",
            "detach-client",
        ],
    },
    // With no session to attach to, the attach block is an error and the
    // client leaves.
    Case {
        name: "no-target",
        setup: &[],
        pty: None,
        start: &["-C", "attach", "-t", "nosuch"],
        script: &[],
    },
    // Windows linked into and unlinked from another session, and that
    // session renamed and killed.
    Case {
        name: "links",
        setup: &["new-session -d -s two -n t0 'sleep 99'"],
        pty: None,
        start: ATTACH,
        script: &[
            "new-window -d -n w1 'sleep 98'",
            "link-window -s @2 -t two:5",
            "unlink-window -t two:5",
            "link-window -s @1 -t main:7",
            "unlink-window -t main:7",
            "rename-window -t two:0 x",
            "rename-session -t two deux",
            "kill-session -t deux",
        ],
    },
    // Modes, paste buffers, messages and config errors.
    Case {
        name: "notices",
        setup: &[],
        pty: None,
        start: ATTACH,
        script: &[
            "copy-mode -t %0",
            "send-keys -t %0 -X cancel",
            "set-buffer -b b1 hello",
            "set-buffer -b b1 world",
            "delete-buffer -b b1",
            "display-message 'hi there'",
            "display-message -t %0 '#{pane_id}'",
            "display-message hi there",
            "source-file {fixtures}/control-bad.conf",
            "source-file {fixtures}/control-bad.conf ; display -p after",
            "source-file {fixtures}/control-bad.conf /nonexistent.conf",
            "display-message -v 'v#{pane_id}'",
            "display-message -t nosuch fallback",
        ],
    },
    // `refresh-client -B`: each kind of subscription, a change, removal.
    Case {
        name: "subscriptions",
        setup: &[
            "split-window -d -t @0 'sleep 90'",
            "new-window -d -t main: -n w1 'sleep 91'",
        ],
        pty: None,
        start: ATTACH,
        script: &[
            "refresh-client -B 'sname::#{session_name}' -B 'pane0:%0:#{pane_id}-#{window_index}' \
             -B 'allp:%*:#{pane_index}' -B 'win1:@1:#{window_name}' -B 'allw:@*:#{window_name}'",
            "!wait",
            "rename-window -t @1 ren ; rename-session newname",
            "!wait",
            "refresh-client -B sname -B allp -B allw",
            "rename-window -t @1 again ; rename-session third",
            "!wait",
            "refresh-client -B 'bad:%x:#{pane_id}' -B nosuch -B 'one:%*' -B 'dash::-#{session_name}' \
             -B 'p0x:%0x:#{pane_id}' -B 'p00:%00:#{pane_index}' -B 'neg:%-1:#{session_id}' \
             -B 'sp1:% 1:#{pane_id}' -B 'sp2: %1:#{pane_id}' -B 'w0:@ 0:#{window_id}'",
            "!wait",
        ],
    },
    // Another client changing session and detaching.
    Case {
        name: "clients",
        setup: &["new-session -d -s two -x 80 -y 24 -n t0 'sleep 99'"],
        pty: Some("two"),
        start: ATTACH,
        script: &[
            "switch-client -c {client} -t main",
            "switch-client -c {client} -t two",
            "display-message -c {client} hello",
            "detach-client -s two",
        ],
    },
    // A zoomed window's layout notices, in and out.
    Case {
        name: "zoom",
        setup: &["split-window -d -t @0 'sleep 90'"],
        pty: None,
        start: ATTACH,
        script: &["resize-pane -Z -t %0", "resize-pane -Z -t %0"],
    },
    // `detach-client -a -t <other>` detaches the control client too.
    Case {
        name: "detach-all-but",
        setup: &["new-session -d -s two -x 80 -y 24 -n t0 'sleep 99'"],
        pty: Some("two"),
        start: ATTACH,
        script: &["detach-client -a -t {client}"],
    },
    // `detach-client -s` of the attached session detaches the control client.
    Case {
        name: "detach-session",
        setup: &[],
        pty: None,
        start: ATTACH,
        script: &["detach-client -s main"],
    },
    // `detach-client -a` detaches every other client.
    Case {
        name: "clients-all",
        setup: &["new-session -d -s two -x 80 -y 24 -n t0 'sleep 99'"],
        pty: Some("two"),
        start: ATTACH,
        script: &["detach-client -a"],
    },
];

fn golden_path(dir: &Path, name: &str) -> PathBuf {
    dir.join("control_mode").join(format!("{name}.golden"))
}

/// grofe's transcript of the case `name` must be its golden.
fn assert_golden(name: &str) {
    let case = CASES.iter().find(|c| c.name == name).expect("case");
    let path = golden_path(&fixtures_dir().join("golden"), name);
    let want = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e} (run scripts/record-compat-goldens.sh)",
            path.display()
        )
    });
    let got = run_case(&grofe_side(), case);
    assert_eq!(got, want, "grofe's transcript differs from tmux's ({name})");
}

#[test]
fn golden_commands() {
    assert_golden("commands");
}

#[test]
fn golden_flow() {
    assert_golden("flow");
}

#[test]
fn golden_new_session() {
    assert_golden("new-session");
}

#[test]
fn golden_no_target() {
    assert_golden("no-target");
}

#[test]
fn golden_links() {
    assert_golden("links");
}

#[test]
fn golden_notices() {
    assert_golden("notices");
}

#[test]
fn golden_subscriptions() {
    assert_golden("subscriptions");
}

#[test]
fn golden_clients() {
    assert_golden("clients");
}

#[test]
fn golden_zoom() {
    assert_golden("zoom");
}

#[test]
fn golden_detach_all_but() {
    assert_golden("detach-all-but");
}

#[test]
fn golden_detach_session() {
    assert_golden("detach-session");
}

#[test]
fn golden_clients_all() {
    assert_golden("clients-all");
}

/// Records every case from real tmux 3.7b. `scripts/record-compat-goldens.sh
/// control_mode` runs it with `GROFE_COMPAT_TMUX` a wrapper that runs tmux in
/// the compat container (the cases are driven from the host, as rustc does
/// not run in the emulated container).
#[test]
#[ignore = "records goldens from real tmux 3.7b; run scripts/record-compat-goldens.sh"]
fn record_control_goldens() {
    let tmux = std::env::var_os("GROFE_COMPAT_TMUX")
        .map(PathBuf::from)
        .expect("GROFE_COMPAT_TMUX unset; run scripts/record-compat-goldens.sh control_mode");
    let version = Command::new(&tmux).arg("-V").output().expect("tmux -V");
    assert_eq!(String::from_utf8_lossy(&version.stdout).trim(), "tmux 3.7b");
    let out = std::env::var_os("GROFE_COMPAT_OUT")
        .map_or_else(|| fixtures_dir().join("golden"), PathBuf::from);
    for case in CASES {
        let text = run_case(&real_side(&tmux), case);
        let path = golden_path(&out, case.name);
        fs::create_dir_all(path.parent().expect("parent")).expect("golden dir");
        fs::write(&path, text).expect("write golden");
        eprintln!("recorded control_mode/{}", case.name);
    }
}

/// The shape alone: the attach block, the session, the command blocks, and
/// `%exit` at EOF.
#[test]
fn grofe_speaks_the_framing() {
    let case = Case {
        name: "framing",
        setup: &[],
        pty: None,
        start: ATTACH,
        script: &["list-windows -F '#{window_id}'", "bogus"],
    };
    let want = "%begin T N 0\n%end T N 0\n%session-changed $0 main\n\
                %begin T N 1\n@0\n%end T N 1\n\
                %begin T N 1\nparse error: unknown command: bogus\n%error T N 1\n%exit\n";
    assert_eq!(run_case(&grofe_side(), &case), want);
}

/// `-CC` on a terminal: the stream is a DCS, `ESC P 1000 p` first and ST
/// after `%exit`, and the terminal does not echo.
#[test]
fn double_c_wraps_the_stream_in_a_dcs() {
    use nix::pty::openpty;
    use std::io::Read;
    use std::os::fd::AsFd;

    let grofe = grofe_side();
    session(&grofe);
    let pty = openpty(None, None).expect("openpty");
    let slave = |fd: &std::os::fd::OwnedFd| Stdio::from(fd.as_fd().try_clone_to_owned().unwrap());
    let mut child = grofe
        .command()
        .args(["-CC", "attach", "-t", "main"])
        .stdin(slave(&pty.slave))
        .stdout(slave(&pty.slave))
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn -CC");
    drop(pty.slave);
    let mut master = fs::File::from(pty.master);
    std::thread::sleep(Duration::from_millis(600));
    master.write_all(b"detach-client\n").expect("write");
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end && !got.ends_with(b"\x1b\\") {
        match master.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    let _ = child.wait();
    let text = String::from_utf8_lossy(&got).replace("\r\n", "\n");
    assert!(text.starts_with("\x1bP1000p%begin "), "{text:?}");
    assert!(text.ends_with("%exit\n\x1b\\"), "{text:?}");
    assert!(
        !text.contains("detach-client"),
        "the terminal echoed: {text:?}"
    );
}

/// A new pane's first output never precedes the `%window-add` that
/// introduces it, and a pane's `%output` is escaped as tmux does.
#[test]
fn output_follows_the_window_add_of_its_pane() {
    let grofe = grofe_side();
    let mut client = Client::start(&grofe, &["-C", "new-session", "-s", "x", "cat"]);
    client.settle(Duration::from_millis(500));
    client.send("new-window -d 'printf \"zzmark\\\\\\\\\\n\"; sleep 30'");
    let text = client.finish();
    let add = text.find("%window-add @1").expect(&text);
    let out = text
        .lines()
        .position(|l| l.starts_with("%output ") && l.contains("zzmark"));
    let out = out.expect(&text);
    let add_line = text[..add].lines().count();
    assert!(add_line < out, "output before its window-add:\n{text}");
    assert!(text.contains("zzmark\\134\\015\\012"), "{text}");
}
