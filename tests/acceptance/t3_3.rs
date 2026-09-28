//! T3.3: passthrough, end to end through `grofe attach`. OSC 52 stores,
//! titles, bells and hyperlinks reach the outer terminal; OSC 7 sets the
//! pane's cwd; bracketed paste and focus reports reach a pane that asked
//! for them; the bell sets `window_bell_flag` as tmux does (R13).

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::{TestServer, wait_until};

use crate::harness::{Harness, on_path, require_tool};

const WAIT: Duration = Duration::from_secs(10);

/// A server with one session `main` running `cmd`, sized for an 80x24
/// client with a status line.
fn start(cmd: &str) -> TestServer {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "23",
        cmd,
    ])
    .expect("new-session");
    srv
}

/// A client attached to `main`, past its first full redraw.
fn attach(srv: &TestServer) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    a.wait_for_output(b"\x1b[2J", 0);
    a
}

fn display(srv: &TestServer, target: &str, format: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, format])
        .unwrap_or_else(|e| panic!("display {format}: {e}"))
        .trim_end()
        .to_string()
}

fn wait_display(srv: &TestServer, target: &str, format: &str, want: &str) {
    if let Err(e) = wait_until(|| display(srv, target, format) == want, WAIT) {
        panic!(
            "{e}: {format} of {target} is {:?}, want {want:?}",
            display(srv, target, format)
        );
    }
}

fn count(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

// ---- OSC 52 ----

/// Case 1: a pane's OSC 52 store reaches the outer stream while
/// `set-clipboard` is `on` (the default, R14), and not while it is `off`.
#[test]
fn osc52_stores_follow_set_clipboard() {
    let srv = start("while read l; do printf '\\033]52;c;aGVsbG8K\\007'; echo stored; done");
    let mut a = attach(&srv);
    let store: &[u8] = b"\x1b]52;c;aGVsbG8K";

    a.send("\r");
    a.wait_for_output(store, 0);

    srv.cmd(&["set", "-g", "set-clipboard", "off"])
        .expect("set-clipboard off");
    a.send("\r");
    a.wait_until("two stores", WAIT, |t| t.matches("stored").count() >= 2);

    // Pane events are handled in order: once the third store (on again)
    // is out, the second one would have been too.
    srv.cmd(&["set", "-g", "set-clipboard", "on"])
        .expect("set-clipboard on");
    let mark = a.output().len();
    a.send("\r");
    a.wait_for_output(b"\x1b]52;c;aGVsbG8K\x07", mark);
    assert_eq!(
        count(a.output(), store),
        2,
        "the store made while off leaked"
    );
}

/// The `nvim` version, if at least 0.10 (its OSC 52 provider).
fn nvim_0_10() -> bool {
    if !on_path("nvim") {
        return false;
    }
    let Ok(out) = Command::new("nvim").arg("--version").output() else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let version = text
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("NVIM v"))
        .unwrap_or("");
    let mut parts = version.split('.').map(|n| n.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (major, minor) >= (0, 10)
}

/// Case 1, Neovim variant: `"+yy` through the OSC 52 provider reaches the
/// outer stream. Skipped below 0.10, CI included (its Linux leg has 0.9.5).
#[test]
fn neovim_yank_to_plus_reaches_the_outer_terminal() {
    if !nvim_0_10() {
        eprintln!("skipping: needs nvim 0.10 or newer");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("y.txt");
    fs::write(&file, "hello\n").expect("write file");
    let setup = "lua local o = require('vim.ui.clipboard.osc52'); \
                 vim.g.clipboard = { name = 'OSC 52', \
                 copy = { ['+'] = o.copy('+'), ['*'] = o.copy('*') }, \
                 paste = { ['+'] = o.paste('+'), ['*'] = o.paste('*') } }";
    let srv = start(&format!(
        "exec nvim --clean --cmd {} {}",
        shell_quote(setup),
        shell_quote(&file.display().to_string())
    ));
    let mut a = attach(&srv);
    a.wait_for("hello");
    let mark = a.output().len();
    a.send("\"+yy");
    // `hello` is `aGVsbG8`, then `K` or `=` for the line end nvim sends.
    a.wait_for_output(b"\x1b]52;c;aGVsbG8", mark);
}

// ---- bracketed paste and focus ----

/// Case 2: a paste into a pane with mode 2004 arrives as one bracketed
/// paste.
#[test]
fn a_multi_line_paste_arrives_as_one_bracketed_paste() {
    let srv = start("printf '\\033[?2004h'; stty -echo; exec cat -v");
    let mut a = attach(&srv);
    a.send("\x1b[200~one\ntwo\n\x1b[201~\r");
    a.wait_for("^[[201~");
    let text = a.text();
    assert_eq!(text.matches("^[[200~").count(), 1, "{text}");
    assert_eq!(text.matches("^[[201~").count(), 1, "{text}");
    let lines: Vec<&str> = text.lines().map(str::trim_end).take(3).collect();
    assert_eq!(lines, ["^[[200~one", "two", "^[[201~"], "{text}");
}

/// Case 2, Neovim: an indented multi-line paste keeps its indent (no
/// staircase), because it arrives bracketed.
#[test]
fn a_paste_into_neovim_keeps_its_lines() {
    if !require_tool("nvim") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("p.txt");
    let srv = start(&format!(
        "exec nvim --clean {}",
        shell_quote(&file.display().to_string())
    ));
    let mut a = attach(&srv);
    a.wait_for("p.txt");
    a.send("i");
    a.wait_for("-- INSERT --");
    a.send("\x1b[200~  one\n  two\n\x1b[201~");
    a.wait_for("  two");
    a.send("\x1b");
    // nvim leaves insert mode once it has taken the ESC as Escape, not
    // as the start of Meta-:.
    a.wait_until("normal mode", WAIT, |t| !t.contains("-- INSERT --"));
    a.send(":wq\r");
    if let Err(e) = wait_until(
        || fs::read_to_string(&file).is_ok_and(|s| s.starts_with("  one\n  two\n")),
        WAIT,
    ) {
        panic!("{e}: file is {:?}", fs::read_to_string(&file));
    }
}

/// Case 3 and R28: focus reports from the outer terminal reach a pane
/// with mode 1004, whole and right after a late probe reply (splits are
/// covered by the client's unit test) (the harness never answers the probe, so the client's
/// late-reply filter is active throughout).
#[test]
fn focus_reports_reach_a_pane_that_asked() {
    let srv = start("printf '\\033[?1004h'; stty -echo; exec cat -v");
    let mut a = attach(&srv);
    a.send("\x1b[Ia\r");
    a.wait_for("^[[Ia");
    a.send("\x1b[?62;22c\x1b[Ic\r");
    a.wait_for("^[[Ic");
    assert!(!a.text().contains("62;22c"), "{}", a.text());
}

// ---- bell ----

/// Case 4 (R13, as tmux 3.7b): a bell in a window that is not current sets
/// its flag and rings the client; in the current window of an attached
/// session it rings but sets nothing; with no client attached it sets the
/// current window's flag; selecting the window clears it.
#[test]
fn bells_set_the_window_flag_as_tmux_does() {
    let ring = "while read l; do printf '\\007'; done";
    let srv = start(ring);
    srv.cmd(&["new-window", "-t", "main", ring])
        .expect("new-window");
    let mut a = attach(&srv);
    let flag = "#{window_bell_flag}";

    let mark = a.output().len();
    srv.cmd(&["send-keys", "-t", "main:1", "Enter"])
        .expect("send-keys");
    wait_display(&srv, "main:1", flag, "1");
    a.wait_for_output(b"\x07", mark);

    let mark = a.output().len();
    srv.cmd(&["send-keys", "-t", "main:2", "Enter"])
        .expect("send-keys");
    a.wait_for_output(b"\x07", mark);
    assert_eq!(display(&srv, "main:2", flag), "0");

    srv.cmd(&["select-window", "-t", "main:1"])
        .expect("select-window");
    assert_eq!(display(&srv, "main:1", flag), "0");

    a.send("\x02d");
    a.wait_exit();
    wait_display(&srv, "main", "#{session_attached}", "0");
    srv.cmd(&["send-keys", "-t", "main:1", "Enter"])
        .expect("send-keys");
    wait_display(&srv, "main:1", flag, "1");
}

// ---- title ----

/// Case 5 (R15): the active pane's title reaches the clients showing it,
/// a pane in another window's does not until that window is shown, and
/// neither renames the window.
#[test]
fn titles_reach_the_clients_showing_the_pane() {
    let srv = start("while read l; do printf '\\033]2;hi\\007'; echo sent-hi; done");
    srv.cmd(&[
        "new-window",
        "-t",
        "main",
        "while read l; do printf '\\033]2;yo\\007'; done",
    ])
    .expect("new-window");
    let mut a = attach(&srv);
    let names =
        display(&srv, "main:1", "#{window_name}") + &display(&srv, "main:2", "#{window_name}");

    // Window 1 is not shown: its title stays in grofe.
    srv.cmd(&["send-keys", "-t", "main:1", "Enter"])
        .expect("send-keys");
    wait_until(|| srv.capture("main:1").contains("sent-hi"), WAIT).expect("hi sent");
    // The shown pane's title, sent after, is relayed; pane events are
    // handled in order, so window 1's was handled first.
    a.send("\r");
    a.wait_for_output(b"\x1b]2;yo", 0);
    assert_eq!(count(a.output(), b"\x1b]2;hi"), 0);

    // Showing window 1 re-sends its pane's title, and back again.
    let mark = a.output().len();
    srv.cmd(&["select-window", "-t", "main:1"])
        .expect("select-window");
    let mark = a.wait_for_output(b"\x1b]2;hi\x07", mark);
    srv.cmd(&["select-window", "-t", "main:2"])
        .expect("select-window");
    a.wait_for_output(b"\x1b]2;yo\x07", mark);

    let now =
        display(&srv, "main:1", "#{window_name}") + &display(&srv, "main:2", "#{window_name}");
    assert_eq!(now, names, "a title renamed a window");
}

// ---- OSC 7 ----

fn file_uri(path: &Path) -> String {
    format!(
        "file://localhost{}",
        path.display().to_string().replace(' ', "%20")
    )
}

/// Case 6: OSC 7 sets `pane_current_path`, over the sampled cwd of the
/// shell (which stays in `A`), whole and split across two writes.
#[test]
fn osc7_sets_the_pane_current_path() {
    let a_dir = tempfile::tempdir().expect("tempdir");
    let root = tempfile::tempdir().expect("tempdir");
    let b1 = root.path().join("b one");
    let b2 = root.path().join("b two");
    fs::create_dir(&b1).expect("mkdir");
    fs::create_dir(&b2).expect("mkdir");
    let a_path = shell_quote(&a_dir.path().display().to_string());

    let srv = start(&format!(
        "cd {a_path}; while read l; do printf '\\033]7;%s\\007' {}; done",
        shell_quote(&file_uri(&b1))
    ));
    let uri = file_uri(&b2);
    let (head, tail) = uri.split_at(uri.len() / 2);
    srv.cmd(&[
        "new-window",
        "-t",
        "main",
        &format!(
            "cd {a_path}; while read l; do printf '\\033]7;%s' {}; sleep 0.2; printf '%s\\007' {}; done",
            shell_quote(head),
            shell_quote(tail)
        ),
    ])
    .expect("new-window");

    for (target, want) in [("main:1", &b1), ("main:2", &b2)] {
        srv.cmd(&["send-keys", "-t", target, "Enter"])
            .expect("send-keys");
        wait_display(
            &srv,
            target,
            "#{pane_current_path}",
            &want.display().to_string(),
        );
    }
}

// ---- OSC 8 ----

/// Case 7, hyperlinks (R20): re-emitted from the cells, open before `X`
/// and closed right after it.
#[test]
fn hyperlinks_are_reemitted_around_their_text() {
    let srv = start("read l; printf '\\033]8;;https://example.com\\007X\\033]8;;\\007'; exec cat");
    let mut a = attach(&srv);
    let mark = a.output().len();
    a.send("\r");
    let open = a.wait_for_output(b";https://example.com\x1b\\", mark);
    let close: &[u8] = b"\x1b]8;;\x1b\\";
    let end = a.wait_for_output(close, open);
    let between = &a.output()[open..end - close.len()];
    assert_eq!(
        count(between, b"X"),
        1,
        "{:?}",
        String::from_utf8_lossy(between)
    );
    assert!(
        count(&a.output()[mark..open], b"\x1b]8;id=") == 1,
        "the open carries the link's id"
    );
}

// ---- OSC 9 and 777 ----

/// Case 7, notifications: OSC 9 and 777 reach the outer stream verbatim.
#[test]
fn notifications_are_reemitted_verbatim() {
    let srv = start("read l; printf '\\033]9;done\\007\\033]777;notify;T;B\\007'; exec cat");
    let mut a = attach(&srv);
    let mark = a.output().len();
    a.send("\r");
    a.wait_for_output(b"\x1b]9;done\x07", mark);
    a.wait_for_output(b"\x1b]777;notify;T;B\x07", mark);
}

// ---- cursor colour ----

/// R16: a client gets its active pane's cursor colour, a reset when it
/// shows a pane that set none, the colour again on returning, and the
/// reset when the pane sends OSC 112.
#[test]
fn cursor_colour_follows_the_active_pane() {
    let srv =
        start("read l; printf '\\033]12;#ff0000\\007'; read l; printf '\\033]112\\007'; exec cat");
    let mut a = attach(&srv);
    let set: &[u8] = b"\x1b]12;#ff0000\x07";
    let reset: &[u8] = b"\x1b]112\x07";

    a.send("\r");
    let at = a.wait_for_output(set, 0);
    srv.cmd(&["new-window", "-t", "main", "cat"])
        .expect("new-window");
    let at = a.wait_for_output(reset, at);
    srv.cmd(&["select-window", "-t", "main:1"])
        .expect("select-window");
    let at = a.wait_for_output(set, at);
    a.send("\r");
    a.wait_for_output(reset, at);
}

// ---- pane focus ----

/// Focus reports follow the active pane, as recorded on tmux 3.7b
/// (`-f /dev/null`, `focus-events on`, two panes in mode 1004): attaching
/// gives the active pane a focus-in; `select-pane` gives the old pane a
/// focus-out and the new one a focus-in; while the client itself is
/// unfocused a `select-pane` sends nothing, and its focus-in then reaches
/// the pane that is active.
#[test]
fn focus_reports_follow_the_active_pane() {
    let tui = "printf '\\033[?1004h'; stty -echo -icanon; printf 'ready '; exec cat -v";
    let srv = start(tui);
    srv.cmd(&["split-window", "-t", "main", tui])
        .expect("split-window");
    let (p1, p2) = ("main:1.1", "main:1.2");
    let seen = |target: &str, want: &str| {
        let want = format!("ready {want}");
        let want = want.trim_end();
        if let Err(e) = wait_until(|| srv.capture(target).trim() == want, WAIT) {
            panic!(
                "{e}: {target} shows {:?}, want {want:?}",
                srv.capture(target).trim()
            );
        }
    };
    // Both panes asked for mode 1004 before the client attaches.
    seen(p1, "");
    seen(p2, "");

    let mut a = attach(&srv);
    seen(p2, "^[[I");
    seen(p1, "");

    srv.cmd(&["select-pane", "-t", p1]).expect("select-pane");
    seen(p1, "^[[I");
    seen(p2, "^[[I^[[O");
    srv.cmd(&["select-pane", "-t", p2]).expect("select-pane");
    seen(p1, "^[[I^[[O");
    seen(p2, "^[[I^[[O^[[I");

    a.send("\x1b[O");
    seen(p2, "^[[I^[[O^[[I^[[O");
    srv.cmd(&["select-pane", "-t", p1]).expect("select-pane");
    // Focus follows the active pane when the client is next drawn: let
    // that frame, which moves the cursor up into pane 1, go out first.
    let deadline = std::time::Instant::now() + WAIT;
    while a.cursor().1 >= 11 {
        assert!(
            std::time::Instant::now() < deadline,
            "the cursor stayed in pane 2"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    a.send("\x1b[I");
    seen(p1, "^[[I^[[O^[[I");
    seen(p2, "^[[I^[[O^[[I^[[O");
}

/// Detach and reattach, as recorded on tmux 3.7b: the session's last
/// client leaving gives the active pane a focus-out, the next attach a
/// focus-in; a second client, or one of two leaving, sends nothing.
#[test]
fn focus_reports_on_detach_and_reattach() {
    let tui = "printf '\\033[?1004h'; stty -echo -icanon; printf 'ready '; exec cat -v";
    let srv = start(tui);
    srv.cmd(&["split-window", "-t", "main", tui])
        .expect("split-window");
    let (p1, p2) = ("main:1.1", "main:1.2");
    let seen = |target: &str, want: &str| {
        let want = format!("ready {want}");
        let want = want.trim_end();
        if let Err(e) = wait_until(|| srv.capture(target).trim() == want, WAIT) {
            panic!(
                "{e}: {target} shows {:?}, want {want:?}",
                srv.capture(target).trim()
            );
        }
    };
    let clients = || {
        srv.cmd(&["list-clients"])
            .unwrap_or_default()
            .lines()
            .count()
    };
    seen(p1, "");
    seen(p2, "");

    let mut a = attach(&srv);
    seen(p2, "^[[I");
    a.send("\x02d");
    a.wait_exit();
    seen(p2, "^[[I^[[O");

    let mut a = attach(&srv);
    seen(p2, "^[[I^[[O^[[I");
    let mut b = attach(&srv);
    wait_until(|| clients() == 2, WAIT).expect("two clients");
    b.send("\x02d");
    b.wait_exit();
    wait_until(|| clients() == 1, WAIT).expect("one client");
    // A frame for the remaining client runs the focus bookkeeping again.
    srv.cmd(&["select-pane", "-t", p2]).expect("select-pane");
    a.send("x");
    seen(p2, "^[[I^[[O^[[Ix");
    seen(p1, "");
}
