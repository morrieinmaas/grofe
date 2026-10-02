//! TM.5: copy targets. `copy-pipe` and its forms run their command (or
//! `copy-command`) with the selection on stdin, off the actor; every copy
//! reaches the clients showing the pane as OSC 52 under `set-clipboard`
//! `on` and `external`; a pane's own OSC 52 makes a buffer only under
//! `on`. Each pipe command writes into the test's own temporary directory,
//! never a real clipboard. The expected bytes are tmux 3.7b's, recorded
//! with a private `-L` server (see the TM.5 report).

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::fs;
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use base64::Engine as _;
use common::{CMD_TIMEOUT, TestServer, wait_until};

use crate::harness::Harness;

const WAIT: Duration = Duration::from_secs(10);

/// The pane's text: two lines, then the pane waits.
const TEXT: &str = "printf 'hello world\\nsecond line\\n'; exec sleep 100000";

/// A server with session `main` running `cmd`, sized for an 80x24 client.
fn start(cmd: &str) -> TestServer {
    let srv = TestServer::start();
    new_session(&srv, "main", cmd);
    srv
}

fn new_session(srv: &TestServer, name: &str, cmd: &str) {
    srv.cmd(&["new-session", "-d", "-s", name, "-x", "80", "-y", "23", cmd])
        .expect("new-session");
}

/// A client attached to `session`, past its first full redraw.
fn attach(srv: &TestServer, session: &str) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), session, &[], 80, 24);
    a.wait_for_output(b"\x1b[2J", 0);
    a
}

fn display(srv: &TestServer, format: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", "main", format])
        .unwrap_or_else(|e| panic!("display {format}: {e}"))
        .trim_end()
        .to_string()
}

fn wait_display(srv: &TestServer, format: &str, want: &str) {
    if let Err(e) = wait_until(|| display(srv, format) == want, WAIT) {
        panic!("{e}: {format} is {:?}, want {want:?}", display(srv, format));
    }
}

/// `send-keys -X` in `main`'s pane.
fn x(srv: &TestServer, args: &[&str]) {
    let mut argv = vec!["send-keys", "-t", "main", "-X"];
    argv.extend_from_slice(args);
    srv.cmd(&argv)
        .unwrap_or_else(|e| panic!("send -X {args:?}: {e}"));
}

/// Waits for the pane's text, enters copy mode and selects its first line
/// (`hello world` and its newline).
fn select_first_line(srv: &TestServer) {
    wait_text(srv, "second line");
    srv.cmd(&["copy-mode", "-t", "main"]).expect("copy-mode");
    x(srv, &["history-top"]);
    x(srv, &["select-line"]);
}

fn wait_text(srv: &TestServer, text: &str) {
    if let Err(e) = wait_until(|| srv.capture("main").contains(text), WAIT) {
        panic!("{e}: {text:?} never showed:\n{}", srv.capture("main"));
    }
}

/// Waits for `path` to hold exactly `want`.
fn wait_file(path: &Path, want: &str) {
    let read = || fs::read_to_string(path).unwrap_or_default();
    if let Err(e) = wait_until(|| read() == want, WAIT) {
        panic!("{e}: {} holds {:?}, want {want:?}", path.display(), read());
    }
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// OSC 52 as tmux 3.7b sends a copy: no selection parameter.
fn copy_osc52(text: &str) -> Vec<u8> {
    let data = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;;{data}\x07").into_bytes()
}

fn count(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

fn buffers(srv: &TestServer) -> Vec<String> {
    srv.cmd(&["list-buffers", "-F", "#{buffer_sample}"])
        .expect("list-buffers")
        .lines()
        .map(str::to_string)
        .collect()
}

/// Case 1: `y` bound to `copy-pipe-and-cancel 'cat > <dir>/clip'` writes
/// the selection to the file and leaves copy mode; it also stores a
/// buffer, as a plain copy would.
#[test]
fn y_pipes_the_selection_and_leaves_copy_mode() {
    let dir = tempfile::tempdir().unwrap();
    let clip = dir.path().join("clip");
    let srv = start(TEXT);
    srv.cmd(&[
        "bind-key",
        "-T",
        "copy-mode-vi",
        "y",
        "send-keys",
        "-X",
        "copy-pipe-and-cancel",
        &format!("cat > {}", quote(&clip)),
    ])
    .expect("bind y");
    let mut a = attach(&srv, "main");
    select_first_line(&srv);
    a.send("y");
    wait_file(&clip, "hello world\n");
    wait_display(&srv, "#{pane_in_mode}", "0");
    assert_eq!(
        srv.cmd(&["show-buffer"]).expect("show-buffer"),
        "hello world\n"
    );
}

/// Cases 2 and 3: under `on` (and `external`) a copy reaches the outer
/// pty as OSC 52 with the selection's base64; under `off` it does not,
/// and neither does a copy with `-C`. The buffer is stored either way.
#[test]
fn a_copy_sends_osc52_unless_set_clipboard_is_off() {
    let srv = start("printf 'one\\ntwo\\nthree\\nfour\\n'; exec sleep 100000");
    let mut a = attach(&srv, "main");
    wait_text(&srv, "four");
    srv.cmd(&["copy-mode", "-t", "main"]).expect("copy-mode");
    x(&srv, &["history-top"]);
    let copy_line = |srv: &TestServer, flags: &[&str]| {
        x(srv, &["select-line"]);
        let mut args = vec!["copy-pipe-no-clear"];
        args.extend_from_slice(flags);
        x(srv, &args);
        x(srv, &["clear-selection"]);
        x(srv, &["cursor-down"]);
    };

    srv.cmd(&["set", "-g", "set-clipboard", "on"]).unwrap();
    copy_line(&srv, &[]);
    a.wait_for_output(&copy_osc52("one\n"), 0);

    srv.cmd(&["set", "-g", "set-clipboard", "off"]).unwrap();
    copy_line(&srv, &[]);
    srv.cmd(&["set", "-g", "set-clipboard", "on"]).unwrap();
    copy_line(&srv, &["-C"]);
    srv.cmd(&["set", "-g", "set-clipboard", "external"])
        .unwrap();
    copy_line(&srv, &[]);
    // Copies are sent in order: once `four` is out, `two` and `three`
    // would have been too.
    a.wait_for_output(&copy_osc52("four\n"), 0);
    let out = a.output().to_vec();
    assert_eq!(count(&out, &copy_osc52("two\n")), 0, "sent under off");
    assert_eq!(count(&out, &copy_osc52("three\n")), 0, "sent with -C");
    assert_eq!(
        count(&out, b"\x1b]52;"),
        2,
        "{:?}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(buffers(&srv), ["four\\n", "three\\n", "two\\n", "one\\n"]);
}

/// OSC 52 goes to the clients showing the pane's window, as tmux's
/// `tty_write` does, not to a client of another session.
#[test]
fn osc52_goes_to_the_clients_showing_the_pane() {
    let srv = start(TEXT);
    new_session(&srv, "other", "exec sleep 100000");
    let mut a = attach(&srv, "main");
    let mut b = attach(&srv, "other");
    let mut c = attach(&srv, "main");
    select_first_line(&srv);
    x(&srv, &["copy-pipe-and-cancel"]);
    let osc = copy_osc52("hello world\n");
    a.wait_for_output(&osc, 0);
    c.wait_for_output(&osc, 0);
    sleep(Duration::from_millis(300));
    assert_eq!(count(b.output(), b"\x1b]52;"), 0);
}

/// Case 4: a `copy-pipe` whose command sleeps 10 s delays neither the
/// next key nor a frame: the key after it moves the cursor at once, and
/// two clients typing into another session see no frame gap over 50 ms
/// (or twice the machine's own baseline over as long a run, as TC.4
/// allows: [`frame_bound`]) while it runs. The command still finishes.
#[test]
fn a_slow_pipe_delays_neither_a_key_nor_a_frame() {
    let dir = tempfile::tempdir().unwrap();
    let done = dir.path().join("done");
    let srv = start(TEXT);
    new_session(&srv, "s", "cat");
    srv.cmd(&[
        "bind-key",
        "-T",
        "copy-mode-vi",
        "y",
        "send-keys",
        "-X",
        "copy-pipe",
        &format!("sleep 10; echo slept > {}", quote(&done)),
    ])
    .unwrap();
    let mut keys = attach(&srv, "main");
    let mut a = attach(&srv, "s");
    let mut b = attach(&srv, "s");
    for h in [&mut a, &mut b] {
        h.wait_until("the status row", CMD_TIMEOUT, |text| {
            text.lines().last().unwrap_or_default().starts_with("[s]")
        });
    }
    // As long as the pipe runs, so a loaded machine's worst gap counts in
    // the baseline as it would during the pipe.
    let (baseline, _) = force_frames(&mut a, &mut b, Duration::from_secs(10), || false);

    select_first_line(&srv);
    assert_eq!(display(&srv, "#{copy_cursor_y}"), "0");
    let pressed = Instant::now();
    keys.send("yj");
    wait_display(&srv, "#{copy_cursor_y}", "1");
    let moved = pressed.elapsed();
    assert!(
        moved < Duration::from_secs(2),
        "the next key took {moved:?}"
    );

    let (longest, frames) = force_frames(&mut a, &mut b, Duration::from_secs(30), || done.exists());
    let ran = pressed.elapsed();
    let bound = frame_bound(baseline);
    eprintln!("pipe ran {ran:?}; longest frame gap {longest:?}, baseline {baseline:?}");
    wait_file(&done, "slept\n");
    assert!(
        ran >= Duration::from_secs(9),
        "the pipe did not sleep: {ran:?}"
    );
    assert!(frames > 5, "frames were not forced: {frames}");
    assert!(
        longest < bound,
        "a frame waited {longest:?} (baseline {baseline:?}, bound {bound:?})"
    );
    assert_eq!(display(&srv, "#{pane_in_mode}"), "1");
}

/// A forced frame every 1/60 s.
const FRAME: Duration = Duration::from_micros(16_667);
/// The longest frame interval the acceptance text allows.
const FRAME_LIMIT: Duration = Duration::from_millis(50);

/// The frame gap to hold a run to, as `tests/sources.rs` does: the
/// acceptance limit, or on a machine whose own baseline is already near it
/// (a loaded host), twice that. A frame that waited on the pipe would wait
/// its 10 s, far past either.
fn frame_bound(baseline: Duration) -> Duration {
    FRAME_LIMIT.max(baseline * 2)
}

/// Types into `a` at 60 Hz and reads both clients until `done` holds or
/// `max` passes: the longest gap between two reads that brought a client
/// output (TC.4's method), and how many there were.
fn force_frames(
    a: &mut Harness,
    b: &mut Harness,
    max: Duration,
    mut done: impl FnMut() -> bool,
) -> (Duration, usize) {
    let start = Instant::now();
    let mut next_key = start;
    let mut typed = 0usize;
    let mut seen = [a.output().len(), b.output().len()];
    let mut last = [start, start];
    let mut longest = Duration::ZERO;
    let mut frames = 0;
    loop {
        let now = Instant::now();
        if now >= next_key {
            a.send(if typed % 40 == 39 { "\r" } else { "x" });
            typed += 1;
            next_key += FRAME;
        }
        for (i, h) in [&mut *a, &mut *b].into_iter().enumerate() {
            let len = h.output().len();
            if len > seen[i] {
                seen[i] = len;
                longest = longest.max(now - last[i]);
                last[i] = now;
                frames += 1;
            }
        }
        if done() || now - start >= max {
            break;
        }
        sleep(Duration::from_millis(1));
    }
    let end = Instant::now();
    for at in last {
        longest = longest.max(end - at);
    }
    (longest, frames)
}

/// A pane that writes an OSC 52 store of each line it reads, then
/// `stored`.
const STORES: &str = "while read l; do printf '\\033]52;c;%s\\007' \"$l\"; echo stored; done";

/// Cases 5 and 6: a pane's own OSC 52 makes a buffer (and reaches the
/// outer terminal) under `on`; under `external` it does neither, as in
/// tmux 3.7b (recorded: `input_osc_52` acts only under `on`), and under
/// `off` neither.
#[test]
fn a_panes_osc52_makes_a_buffer_only_under_on() {
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
    let srv = start(STORES);
    let mut a = attach(&srv, "main");
    let store = |srv: &TestServer, text: &str| {
        srv.cmd(&["send-keys", "-t", "main", &b64(text), "Enter"])
            .unwrap();
    };

    srv.cmd(&["set", "-g", "set-clipboard", "on"]).unwrap();
    store(&srv, "app-on");
    a.wait_for_output(format!("\x1b]52;c;{}\x07", b64("app-on")).as_bytes(), 0);
    wait_until(|| buffers(&srv) == ["app-on"], WAIT).expect("a buffer under on");

    for mode in ["external", "off"] {
        srv.cmd(&["set", "-g", "set-clipboard", mode]).unwrap();
        store(&srv, &format!("app-{mode}"));
    }
    srv.cmd(&["set", "-g", "set-clipboard", "on"]).unwrap();
    store(&srv, "app-last");
    // Pane events are handled in order.
    wait_until(|| buffers(&srv).len() == 2, WAIT).expect("the last store");
    assert_eq!(buffers(&srv), ["app-last", "app-on"]);
    let out = a.output().to_vec();
    for gone in ["app-external", "app-off"] {
        let osc = format!("\x1b]52;c;{}", b64(gone));
        assert_eq!(count(&out, osc.as_bytes()), 0, "{gone} reached the client");
    }
}

/// A pane cannot read the clipboard or a buffer: an OSC 52 query gets no
/// answer and does not reach the outer terminal.
#[test]
fn a_pane_cannot_read_the_clipboard() {
    let srv = start(
        "stty -echo -icanon min 0 time 10; printf '\\033]52;c;?\\007'; \
         head -c 64 | od -c; echo END; exec sleep 100000",
    );
    srv.cmd(&["set-buffer", "secret"]).unwrap();
    let mut a = attach(&srv, "main");
    wait_text(&srv, "END");
    let text = srv.capture("main");
    let dump: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(dump, ["0000000", "END"], "the pane read something:\n{text}");
    assert_eq!(count(a.output(), b"\x1b]52;"), 0);
}

/// Case 7: with no argument (or an empty one), `copy-command` runs; an
/// explicit command is a format, expanded against the pane before the
/// copy, but `copy-command` is not (tmux 3.7b, recorded).
#[test]
fn copy_command_runs_with_no_argument() {
    let dir = tempfile::tempdir().unwrap();
    let cc = dir.path().join("cc");
    let raw = dir.path().join("raw");
    let fmt = dir.path().join("fmt");
    let srv = start(TEXT);
    srv.cmd(&[
        "set",
        "-g",
        "copy-command",
        &format!(
            "cat > {}; echo '#{{pane_id}}' > {}",
            quote(&cc),
            quote(&raw)
        ),
    ])
    .unwrap();
    select_first_line(&srv);
    x(&srv, &["copy-pipe-and-cancel"]);
    wait_file(&cc, "hello world\n");
    wait_file(&raw, "#{pane_id}\n");
    wait_display(&srv, "#{pane_in_mode}", "0");

    fs::remove_file(&cc).unwrap();
    select_first_line(&srv);
    x(&srv, &["copy-pipe-and-cancel", ""]);
    wait_file(&cc, "hello world\n");

    let pane = display(&srv, "#{pane_id}");
    select_first_line(&srv);
    x(
        &srv,
        &[
            "copy-pipe-and-cancel",
            &format!(
                "echo '#{{pane_id}} #{{selection_present}}' > {}",
                quote(&fmt)
            ),
        ],
    );
    wait_file(&fmt, &format!("{pane} 1\n"));
}

/// The selection reaches the command only on stdin: shell syntax in it
/// runs nothing.
#[test]
fn the_selection_never_reaches_the_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let clip = dir.path().join("clip");
    let pwned = dir.path().join("pwned");
    let line = format!("$(touch {p}) `touch {p}` ; touch {p}", p = quote(&pwned));
    let srv = start(&format!(
        "printf '%s\\nend\\n' {}; exec sleep 100000",
        shell_quote(&line)
    ));
    wait_text(&srv, "end");
    srv.cmd(&["copy-mode", "-t", "main"]).unwrap();
    x(&srv, &["history-top"]);
    x(&srv, &["select-line"]);
    x(
        &srv,
        &["copy-pipe-and-cancel", &format!("cat > {}", quote(&clip))],
    );
    wait_file(&clip, &format!("{line}\n"));
    // The pipe is done once the file is whole; give a stray `touch` time.
    sleep(Duration::from_millis(200));
    assert!(!pwned.exists(), "the selection ran as a command");
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The command runs under `/bin/sh` with the server's environment and
/// the session's, `TMUX` set and no pane of its own (`TMUX_PANE`,
/// `GROFE_PANE`), stdout and stderr going nowhere, and no descriptor
/// beyond the standard three.
#[test]
fn the_command_gets_the_server_environment_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let out = |name: &str| dir.path().join(name);
    let srv = start(TEXT);
    srv.cmd(&["set-environment", "-g", "TM5_GLOBAL", "glob"])
        .unwrap();
    srv.cmd(&["set-environment", "-t", "main", "TM5_SESSION", "sess"])
        .unwrap();
    select_first_line(&srv);
    let script = format!(
        "env > {env}; ls /dev/fd > {fds}; cat > {stdin}; \
         echo tm5-stdout-leak; echo tm5-stderr-leak >&2; echo ok > {done}",
        env = quote(&out("env")),
        fds = quote(&out("fds")),
        stdin = quote(&out("stdin")),
        done = quote(&out("done")),
    );
    x(&srv, &["copy-pipe-and-cancel", &script]);
    wait_file(&out("done"), "ok\n");
    assert_eq!(fs::read_to_string(out("stdin")).unwrap(), "hello world\n");

    let env = fs::read_to_string(out("env")).unwrap();
    let var = |name: &str| {
        env.lines()
            .find_map(|l| l.strip_prefix(&format!("{name}=")))
            .map(str::to_string)
    };
    assert_eq!(var("TM5_GLOBAL").as_deref(), Some("glob"), "{env}");
    assert_eq!(var("TM5_SESSION").as_deref(), Some("sess"), "{env}");
    assert!(var("TMUX").is_some_and(|v| !v.is_empty()), "{env}");
    assert_eq!(var("TMUX_PANE"), None, "{env}");
    assert_eq!(var("GROFE_PANE"), None, "{env}");

    let fds = fs::read_to_string(out("fds")).unwrap();
    let extra: Vec<&str> = fds
        .split_whitespace()
        .filter(|fd| !["0", "1", "2", "3"].contains(fd))
        .collect();
    assert!(extra.is_empty(), "inherited descriptors {extra:?}");
    let log = srv.log();
    assert!(!log.contains("tm5-stdout-leak") && !log.contains("tm5-stderr-leak"));
}

/// The forms, as tmux 3.7b (recorded): `copy-pipe` clears the selection
/// and stays, `-no-clear` keeps it, `-P` stores no buffer, the
/// `-end-of-line` and `-line` forms copy what their copy commands do, and
/// an empty selection still runs the command, with nothing on stdin and
/// no buffer. Three positionals do nothing.
#[test]
fn the_forms_copy_and_pipe_as_tmux() {
    let dir = tempfile::tempdir().unwrap();
    let f = |name: &str| dir.path().join(name);
    let cat = |name: &str| format!("cat > {}", quote(&f(name)));
    let srv = start(TEXT);

    wait_text(&srv, "second line");
    srv.cmd(&["copy-mode", "-t", "main"]).unwrap();
    x(&srv, &["copy-pipe", &cat("empty")]);
    wait_file(&f("empty"), "");
    assert!(buffers(&srv).is_empty());

    x(&srv, &["history-top"]);
    x(&srv, &["select-line"]);
    x(&srv, &["copy-pipe-no-clear", &cat("nc")]);
    wait_file(&f("nc"), "hello world\n");
    assert_eq!(display(&srv, "#{pane_in_mode} #{selection_present}"), "1 1");

    x(&srv, &["copy-pipe", "-P", &cat("p")]);
    wait_file(&f("p"), "hello world\n");
    assert_eq!(display(&srv, "#{pane_in_mode} #{selection_present}"), "1 0");
    assert_eq!(buffers(&srv).len(), 1, "-P stored a buffer");

    x(&srv, &["select-line"]);
    x(&srv, &["copy-pipe-and-cancel", "true", "prefix", "extra"]);
    assert_eq!(display(&srv, "#{pane_in_mode} #{selection_present}"), "1 1");
    x(&srv, &["clear-selection"]);

    x(&srv, &["start-of-line"]);
    x(&srv, &["cursor-right"]);
    x(&srv, &["copy-pipe-end-of-line", &cat("eol")]);
    wait_file(&f("eol"), "ello world");
    assert_eq!(display(&srv, "#{pane_in_mode}"), "1");

    x(&srv, &["copy-pipe-line-and-cancel", &cat("line")]);
    wait_file(&f("line"), "hello world");
    wait_display(&srv, "#{pane_in_mode}", "0");
    assert_eq!(
        buffers(&srv),
        ["hello world", "ello world", "hello world\\n"]
    );
}

/// A pane's store goes, with the pane's own selection parameter, only to
/// the clients showing its window (tmux 3.7b, recorded), and its
/// parameter keeps only tmux's characters.
#[test]
fn a_panes_store_goes_to_the_clients_showing_it() {
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
    let srv = start("while read p d; do printf '\\033]52;%s;%s\\007' \"$p\" \"$d\"; done");
    new_session(&srv, "other", "exec sleep 100000");
    let mut a = attach(&srv, "main");
    let mut b = attach(&srv, "other");
    srv.cmd(&["set", "-g", "set-clipboard", "on"]).unwrap();
    srv.cmd(&[
        "send-keys",
        "-t",
        "main",
        &format!("p {}", b64("prim")),
        "Enter",
    ])
    .unwrap();
    a.wait_for_output(format!("\x1b]52;p;{}\x07", b64("prim")).as_bytes(), 0);
    srv.cmd(&[
        "send-keys",
        "-t",
        "main",
        &format!("sx {}", b64("bad")),
        "Enter",
    ])
    .unwrap();
    a.wait_for_output(format!("\x1b]52;s;{}\x07", b64("bad")).as_bytes(), 0);
    sleep(Duration::from_millis(300));
    assert_eq!(count(b.output(), b"\x1b]52;"), 0);
    assert_eq!(buffers(&srv), ["bad", "prim"]);
}

/// An expanded command that reads as a flag (`-P` here, as a pane title
/// could) stays the command: the copy still stores its buffer.
#[test]
fn an_expanded_command_cannot_become_a_flag() {
    let srv = start(TEXT);
    srv.cmd(&["set", "-g", "@tm5", "-P"]).unwrap();
    select_first_line(&srv);
    x(&srv, &["copy-pipe-and-cancel", "#{@tm5}"]);
    wait_display(&srv, "#{pane_in_mode}", "0");
    assert_eq!(buffers(&srv), ["hello world\\n"]);
}

/// `#{pane_current_path}` is the pane process's own cwd, as tmux 3.7b
/// reads it, never the text an OSC 7 carried, so a reported path with a
/// newline and braces cannot reach a `-c '#{pane_current_path}'` or a
/// shell line. `#{pane_path}` keeps the text OSC 7 sent, never decoded.
#[test]
fn an_osc7_path_never_becomes_the_current_path() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().canonicalize().unwrap();
    let srv = start(&format!(
        "cd {}; while read u; do printf '\\033]7;%s\\007' \"$u\"; echo set; done",
        quote(&real)
    ));
    let send = |uri: &str| {
        srv.cmd(&["send-keys", "-t", "main", uri, "Enter"]).unwrap();
    };
    send("file://localhost/tmp/x{y}");
    wait_display(&srv, "#{pane_path}", "file://localhost/tmp/x{y}");
    send("file://localhost/tmp/a%0A{b};touch%20x");
    // As tmux 3.7b keeps it: the text, never decoded (TM.7).
    wait_display(
        &srv,
        "#{pane_path}",
        "file://localhost/tmp/a%0A{b};touch%20x",
    );
    send("file://localhost/tmp/c%1Bd");
    send("file://localhost/tmp/done");
    wait_display(&srv, "#{pane_path}", "file://localhost/tmp/done");
    wait_display(&srv, "#{pane_current_path}", &real.display().to_string());
    let shown = srv
        .cmd(&["list-panes", "-F", "#{pane_path}|#{pane_current_path}"])
        .unwrap();
    assert!(!shown.trim_end().chars().any(char::is_control), "{shown:?}");
}
