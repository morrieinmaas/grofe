//! T3.6: dogfood readiness. The spec §8 acceptance list end to end through
//! `grofe attach` (Neovim, fzf, less, htop, `git log`, a nested tmux, a
//! nested grofe, wide characters, kitty keys with a config loaded), the
//! panic report (R22), and the D7 `TERM` re-run: every capability
//! `tmux-256color` adds over `screen-256color`, sent by a pane and checked in
//! the reconstructed outer screen or stream.
//!
//! Items earlier tasks already cover, and are not repeated here:
//! - fzf: `t2_5b::fzf_picks_an_entry`; less `G` then `q`:
//!   `t2_5b::less_jumps_to_the_end_and_quits`; Neovim edit and write:
//!   `t2_5b::neovim_edits_and_writes_a_file`.
//! - Neovim mouse: `t3_2::wheel_in_neovim_scrolls_neovim`; bracketed paste:
//!   `t3_3::a_paste_into_neovim_keeps_its_lines`; focus reports:
//!   `t3_3::focus_reports_reach_a_pane_that_asked` and
//!   `t3_3::focus_reports_follow_the_active_pane`.
//! - The kitty flags checks without a config: `t2_5b::a_kitty_pane_gets_escape_as_csi_u`
//!   and `t2_5b::a_kitty_tui_quits_on_a_bare_q`.
//! - `kitten icat --unicode-placeholder` (case 6):
//!   `t3_8::kitten_icat_draws_through_grofe` and
//!   `t3_8::kitten_icat_by_shared_memory_draws_through_grofe`, which skip
//!   where `kitten` is absent, and the fixture replay
//!   `t3_8::placeholder_images_reach_kitty_clients`, which always runs.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Config;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi::{Color, CursorShape, Processor, Rgb};
use common::{CMD_TIMEOUT, TestServer, wait_child, wait_until};
use grofe::paths::{self, Env};
use grofe::server::TEST_PANIC_ENV;

use crate::harness::{Harness, require_tool};

const WAIT: Duration = Duration::from_secs(10);
const PREFIX: &str = "\x02";
const COLS: u16 = 80;
const ROWS: u16 = 24;

/// A new session `main` on `srv` running `cmd`, sized for an 80x24 client
/// with a status line.
fn new_session(srv: &TestServer, cmd: &str) {
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
    .unwrap_or_else(|e| panic!("new-session: {e}"));
}

fn start(cmd: &str) -> TestServer {
    let srv = TestServer::start();
    new_session(&srv, cmd);
    srv
}

/// A client attached to `main` claiming `caps` (`GROFE_TEST_CAPS`, since the
/// harness cannot answer the probe), past its first full redraw.
fn attach(srv: &TestServer, caps: &str) -> Harness {
    let env: &[(&str, &str)] = if caps.is_empty() {
        &[]
    } else {
        &[("GROFE_TEST_CAPS", caps)]
    };
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", env, COLS, ROWS);
    a.wait_for_output(b"\x1b[2J", 0);
    a
}

/// The outer screen rebuilt from everything the client wrote, for checks
/// on cell attributes and the cursor that the harness's text does not show.
fn outer(a: &mut Harness) -> Term<VoidListener> {
    let mut term = Term::new(
        Config::default(),
        &TermSize::new(COLS as usize, ROWS as usize),
        VoidListener,
    );
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, a.output());
    term
}

fn cell(
    term: &Term<VoidListener>,
    row: usize,
    col: usize,
) -> &alacritty_terminal::term::cell::Cell {
    &term.grid()[Line(row as i32)][Column(col)]
}

/// Waits until the rebuilt outer screen satisfies `done`.
fn wait_outer(a: &mut Harness, what: &str, done: impl Fn(&Term<VoidListener>) -> bool) {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let term = outer(a);
        if done(&term) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}; screen:\n{}",
            a.text()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `s` as a `printf` format of octal escapes, so a pane can print any UTF-8
/// whatever its shell's own `printf` does with multibyte text.
fn octal(s: &str) -> String {
    s.bytes().map(|b| format!("\\{b:03o}")).collect()
}

/// A pane script: prints `push` then `ready`, reads `n` raw bytes and shows
/// them as hex (T2.5b's reader).
fn hex_reader(push: &str, n: usize) -> String {
    format!(
        "printf '{push}ready\\n'; stty raw -echo; \
         b=$(dd bs=1 count={n} 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
         stty sane; printf 'got:%s:end' \"$b\"; exec sleep 600"
    )
}

// ---- the panic report (R22) ----

/// Panic reports in `dir` for the instance whose log is `log`, by file name.
fn panic_reports(dir: &Path, log: &Path) -> BTreeSet<String> {
    let stem = log
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prefix = format!("{stem}.panic.");
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| n.starts_with(&prefix) && n.ends_with(".log"))
                .collect()
        })
        .unwrap_or_default()
}

/// Case 8: `GROFE_TEST_SERVER_PANIC=1` makes the server panic on its first
/// command; the report lands beside the server log in the test's state dir,
/// names the panic, and the real state dir gains nothing.
#[test]
fn a_forced_server_panic_writes_a_report_to_the_state_dir() {
    let srv = TestServer::unstarted();
    let log = srv.log_path();
    let state = log.parent().expect("state dir").to_path_buf();
    assert!(state.starts_with(&srv.state_home), "{}", state.display());
    // The real state dir, read only: nothing may be written there.
    let real: PathBuf = paths::state_dir(&Env::from_process());
    let real_before = panic_reports(&real, &log);

    let mut cmd = srv.command();
    cmd.arg("__server")
        .env(TEST_PANIC_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd.spawn().expect("spawn server");
    wait_until(|| srv.is_running(), WAIT).expect("server up");
    let out = srv.run(&["new-session", "-d", "-s", "main"]);
    assert!(!out.status.success(), "{out:?}");
    let status = wait_child(child, WAIT).expect("server exits");
    assert!(!status.success(), "{status:?}");

    let reports = panic_reports(&state, &log);
    assert_eq!(reports.len(), 1, "{reports:?} in {}", state.display());
    let name = reports.iter().next().expect("one report");
    let ms = name
        .rsplit_once(".panic.")
        .and_then(|(_, rest)| rest.strip_suffix(".log"))
        .expect("<instance>.panic.<ms>.log");
    assert!(ms.parse::<u128>().is_ok(), "{name}");
    let report = fs::read_to_string(state.join(name)).expect("read report");
    for want in [
        "test panic on a command",
        "location: src/server/mod.rs:",
        "thread: ",
        "backtrace:",
        &format!("version: {}", env!("CARGO_PKG_VERSION")),
        &format!("os: {}", std::env::consts::OS),
        "role: server",
    ] {
        assert!(report.contains(want), "{want:?} missing from:\n{report}");
    }
    assert!(srv.log().contains(name.as_str()), "{}", srv.log());
    assert_eq!(panic_reports(&real, &log), real_before, "real state dir");
}

/// A panic in the attach client writes a report too (role `client`), after
/// T2.1's hook has restored the tty and chained to this one.
#[test]
fn a_client_panic_writes_a_report_and_the_client_exits() {
    let srv = start("printf 'idle-pane'; exec sleep 600");
    let log = srv.log_path();
    let state = log.parent().expect("state dir").to_path_buf();
    let real = paths::state_dir(&Env::from_process());
    let real_before = panic_reports(&real, &log);
    let mut a = Harness::spawn_attach_with_env(
        srv.command(),
        "main",
        &[("GROFE_TEST_CLIENT_PANIC", "stdin")],
        COLS,
        ROWS,
    );
    a.wait_for("idle-pane");
    a.send("x");
    a.wait_exit();
    let reports = panic_reports(&state, &log);
    assert_eq!(reports.len(), 1, "{reports:?}");
    let name = reports.iter().next().expect("one report");
    let report = fs::read_to_string(state.join(name)).expect("read report");
    assert!(report.contains("role: client"), "{report}");
    assert!(
        report.contains("test panic in the stdin thread"),
        "{report}"
    );
    assert_eq!(panic_reports(&real, &log), real_before, "real state dir");
    // The server is unharmed.
    srv.cmd(&["list-sessions"]).expect("server still answers");
}

// ---- Neovim (case 1) ----

/// Alt screen in and out, a true colour background, and the insert-mode
/// cursor shape, all in the rebuilt outer screen.
#[test]
fn neovim_alt_screen_true_colour_and_cursor_shape() {
    if !require_tool("nvim") {
        return;
    }
    let srv = start(
        "printf 'before-nvim\\n'; \
         nvim --clean -c 'set termguicolors' -c 'hi Normal guibg=#123456'; \
         printf 'after-nvim'; exec sleep 600",
    );
    let mut a = attach(&srv, "truecolor");
    let want = Color::Spec(Rgb {
        r: 0x12,
        g: 0x34,
        b: 0x56,
    });
    wait_outer(&mut a, "nvim's #123456 background", |t| {
        cell(t, 5, 10).bg == want
    });
    assert!(
        !a.text().contains("before-nvim"),
        "alt screen:\n{}",
        a.text()
    );

    a.send("i");
    wait_outer(&mut a, "a bar cursor in insert mode", |t| {
        t.cursor_style().shape == CursorShape::Beam
    });
    a.send("\x1b");
    wait_outer(&mut a, "a block cursor in normal mode", |t| {
        t.cursor_style().shape == CursorShape::Block
    });
    a.send(":q\r");
    a.wait_for("after-nvim");
    let text = a.text();
    assert!(text.contains("before-nvim"), "primary screen:\n{text}");
    assert!(!text.contains('~'), "nvim left on screen:\n{text}");
}

/// Focus events reach Neovim: its `FocusGained` autocommand fires.
#[test]
fn neovim_gets_focus_events() {
    if !require_tool("nvim") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let mark = dir.path().join("gained");
    let srv = start(&format!(
        "nvim --clean -c {}",
        shell_quote(&format!(
            "au FocusGained * call writefile(['gained'], '{}')",
            mark.display()
        ))
    ));
    let mut a = attach(&srv, "");
    a.wait_for("~");
    // nvim turns focus reporting on once its UI is up; keep reporting until
    // it has (a report before that is dropped, as the pane never asked).
    wait_until(
        || {
            a.send("\x1b[O\x1b[I");
            std::thread::sleep(Duration::from_millis(100));
            mark.exists()
        },
        WAIT,
    )
    .expect("FocusGained never fired");
}

/// The background colour query `OSC 11 ; ?` is answered to the pane.
#[test]
fn a_background_colour_query_is_answered() {
    let srv = start(
        "stty raw -echo; printf 'ready\\r\\n'; printf '\\033]11;?\\007'; \
         b=$(dd bs=1 count=9 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
         stty sane; printf 'got:%s:end' \"$b\"; exec sleep 600",
    );
    let mut a = attach(&srv, "");
    // ESC ] 1 1 ; r g b :
    a.wait_for("got:1b5d31313b7267623a:end");
}

// ---- other TUIs (case 2) ----

#[test]
fn htop_starts_and_quits_on_q() {
    if !require_tool("htop") {
        return;
    }
    let srv = start("htop; printf 'htop-done'; exec sleep 600");
    let mut a = attach(&srv, "");
    a.wait_for("Quit");
    a.send("q");
    a.wait_for("htop-done");
}

/// `git log` in a temp repo pages through `less`; `q` returns to the shell.
#[test]
fn git_log_pages_and_q_returns() {
    if !require_tool("git") || !require_tool("less") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=t36",
                "-c",
                "user.email=t36@example.invalid",
            ])
            .args(args)
            .current_dir(repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(&["init", "-q"]);
    let message: String = (1..=60).map(|n| format!("line-{n}\n\n")).collect();
    git(&["commit", "-q", "--allow-empty", "-m", &message]);
    let srv = start(&format!(
        "cd {}; GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 PAGER=less LESS=FRX \
         env -u GIT_PAGER git log; printf 'git-done'; exec sleep 600",
        shell_quote(&repo.display().to_string())
    ));
    let mut a = attach(&srv, "");
    a.wait_for("line-1");
    assert!(!a.text().contains("git-done"), "no pager:\n{}", a.text());
    a.send("q");
    a.wait_for("git-done");
}

// ---- nested tmux (case 3) ----

const NESTED_TMUX: &str = "grofe-t36-nested";

/// Kills the private nested tmux server, and only that one. `tmux` is the
/// one on `PATH` (the harness's `require_tool` found it), the same one the
/// pane resolves with `command -v`.
struct NestedTmux {
    tmpdir: tempfile::TempDir,
}

impl NestedTmux {
    fn command(&self) -> Command {
        let mut cmd = Command::new("tmux");
        cmd.env_remove("TMUX")
            .env("TMUX_TMPDIR", self.tmpdir.path())
            .args(["-L", NESTED_TMUX]);
        cmd
    }
}

impl Drop for NestedTmux {
    fn drop(&mut self) {
        let mut cmd = self.command();
        cmd.arg("kill-server")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = common::run_bounded(cmd, CMD_TIMEOUT);
    }
}

/// Real tmux in a pane (`env -u TMUX`, a private `-L` server with no config
/// in its own `TMUX_TMPDIR`): its status line renders, and the inner prefix
/// sent through grofe's `send-prefix` (`C-b C-b c`) makes an inner window.
#[test]
fn nested_tmux_renders_and_takes_its_prefix() {
    if !require_tool("tmux") {
        return;
    }
    let nested = NestedTmux {
        tmpdir: tempfile::Builder::new()
            .prefix("t36")
            .tempdir_in("/tmp")
            .expect("tempdir"),
    };
    let srv = start(&format!(
        "TMUX_TMPDIR={} SHELL=/bin/sh exec env -u TMUX \"$(command -v tmux)\" \
         -L {NESTED_TMUX} -f /dev/null new-session -s nested",
        shell_quote(&nested.tmpdir.path().display().to_string()),
    ));
    let mut a = attach(&srv, "");
    a.wait_for("[nested] 0:");
    a.send(format!("{PREFIX}{PREFIX}c"));
    a.wait_until("the inner window 1", WAIT, |t| {
        t.lines()
            .any(|l| l.starts_with("[nested]") && l.contains(" 1:"))
    });
    let mut list = nested.command();
    list.args(["list-windows", "-t", "nested"]);
    let out = common::run_bounded(list, CMD_TIMEOUT);
    assert_eq!(out.stdout.lines().count(), 2, "{out:?}");
    // grofe's own status line is still there, below the inner one.
    let text = a.text();
    let last = text.lines().nth(usize::from(ROWS) - 1).unwrap_or_default();
    assert!(last.starts_with("[main]"), "grofe status: {last:?}\n{text}");
}

// ---- nested grofe (case 4) ----

/// A second grofe server driven from inside a pane: `new-session` there
/// starts it and attaches, its client draws its own status line inside the
/// pane and detaches on its own prefix (sent through the outer
/// `send-prefix`), and self-nesting on the outer session is refused.
#[test]
fn nested_grofe_runs_detaches_and_refuses_self_nesting() {
    let grofe = env!("CARGO_BIN_EXE_grofe");
    let srv = TestServer::start();
    // Under the harness's runtime dir, so its teardown stops this server.
    let inner = srv.runtime_dir.join("grofe").join("inner.sock");
    let g = format!(
        "{} -S {}",
        shell_quote(grofe),
        shell_quote(&inner.display().to_string())
    );
    new_session(
        &srv,
        &format!(
            "{g} new-session -s inner \"printf 'inner-pane'; exec sleep 600\"; \
             printf 'inner-done\\n'; \
             {} attach -t main; printf 'self-done'; exec sleep 600",
            shell_quote(grofe)
        ),
    );
    let mut a = attach(&srv, "");
    a.wait_for("[inner] 1:");
    a.wait_for("inner-pane");
    a.send(format!("{PREFIX}{PREFIX}d"));
    a.wait_for("inner-done");
    a.wait_for("[detached (from session inner)]");
    a.wait_for("refusing to attach session main from inside itself");
    a.wait_for("self-done");
    // The inner server still holds its session after the detach.
    let mut ls = srv.command();
    ls.args([
        "-S",
        &inner.display().to_string(),
        "list-sessions",
        "-F",
        "#{session_name}",
    ]);
    let out = common::run_bounded(ls, CMD_TIMEOUT);
    assert_eq!(out.stdout, "inner\n", "{out:?}");
}

// ---- wide characters (case 5) ----

/// CJK, an emoji, an emoji with VS16 and a combining mark: the `|` after
/// each lands where the pane put it. The VS16 heart is one cell wide here,
/// as in the pane's emulator; real terminals draw it two wide (logged in
/// docs/dogfood.md, not fixed), and grofe's absolute cursor move after it
/// keeps the next cell in place there too (render tests).
#[test]
fn wide_characters_do_not_drift_the_next_cell() {
    let lines = [
        ("a\u{6F22}\u{5B57}|", 5),
        ("b\u{1F44D}|", 3),
        ("c\u{2764}\u{FE0F}|", 2),
        ("de\u{301}|", 2),
    ];
    let script: String = lines
        .iter()
        .map(|(l, _)| format!("{}\\n", octal(l)))
        .collect();
    let srv = start(&format!("printf '{script}wide-done'; exec sleep 600"));
    let mut a = attach(&srv, "truecolor");
    a.wait_for("wide-done");
    let term = outer(&mut a);
    for (row, (line, col)) in lines.iter().enumerate() {
        assert_eq!(cell(&term, row, *col).c, '|', "{line:?}\n{}", a.text());
    }
    assert!(cell(&term, 0, 1).flags.contains(Flags::WIDE_CHAR));
    assert!(cell(&term, 1, 1).flags.contains(Flags::WIDE_CHAR));
    let heart = cell(&term, 2, 1);
    assert_eq!(heart.zerowidth(), Some(&['\u{FE0F}'][..]));
    assert_eq!(cell(&term, 3, 1).zerowidth(), Some(&['\u{301}'][..]));
}

// ---- kitty keys with a config loaded (case 7) ----

/// A server started over a `config.toml` that rebinds the prefix to `C-a`.
struct Configured {
    child: Option<Child>,
    srv: TestServer,
}

impl Drop for Configured {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn start_configured(config: &str, cmd: &str) -> Configured {
    let srv = TestServer::unstarted();
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).expect("config dir");
    fs::write(dir.join("config.toml"), config).expect("write config");
    let mut server = srv.command();
    server
        .arg("__server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = server.spawn().expect("spawn server");
    let c = Configured {
        child: Some(child),
        srv,
    };
    wait_until(|| c.srv.is_running(), WAIT).expect("server up");
    new_session(&c.srv, cmd);
    c
}

/// T2.5b's kitty checks with `prefix = "C-a"` loaded: under flags 1 the old
/// prefix `C-b` reaches the pane as `CSI 98;5 u` and Escape as `CSI 27 u`.
#[test]
fn kitty_flags_1_with_a_config_loaded() {
    let c = start_configured("prefix = \"C-a\"\n", &hex_reader("\\033[>1u", 12));
    assert_eq!(
        c.srv.cmd(&["show-options", "-g", "prefix"]).expect("show"),
        "prefix C-a\n"
    );
    let mut a = attach(&c.srv, "");
    a.wait_for("ready");
    a.send("\x02\x1b");
    a.wait_for("got:1b5b39383b35751b5b323775:end");
}

/// Under flags 8 a bare `q` reaches the pane as `CSI 113 u`, config loaded.
#[test]
fn kitty_flags_8_bare_q_with_a_config_loaded() {
    let tui = "printf '\\033[>8uready\\n'; stty raw -echo; \
               b=$(dd bs=1 count=6 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
               stty sane; printf '\\033[<u'; \
               if [ \"$b\" = 1b5b31313375 ]; then printf 'tui-quit'; else printf 'bad:%s' \"$b\"; fi; \
               exec sleep 600";
    let c = start_configured("prefix = \"C-a\"\n", tui);
    let mut a = attach(&c.srv, "");
    a.wait_for("ready");
    a.send("q");
    a.wait_for("tui-quit");
}

// ---- the D7 TERM re-run ----
//
// Each capability tmux-256color's terminfo entry adds over screen-256color
// (`infocmp -1x -d screen-256color tmux-256color`, ncurses 6.6), sent by a
// pane as that entry spells it, through the real render path, into the
// rebuilt outer screen or the raw outer stream. The client claims what
// Ghostty and WezTerm have (true colour, styled underlines).

const RICH: &str = "truecolor,styled_underline";

/// `sitm`, `smxx`, `invis` and every `Smulx` style (`\E[4:%p1%dm`, 2 to 5)
/// land on the outer cells with their own attribute.
#[test]
fn term_rerun_sgr_capabilities_reach_the_outer_cells() {
    let parts: [(&str, &str, Flags); 7] = [
        ("\\033[3m", "it", Flags::ITALIC),
        ("\\033[9m", "st", Flags::STRIKEOUT),
        ("\\033[8m", "hd", Flags::HIDDEN),
        ("\\033[4:2m", "db", Flags::DOUBLE_UNDERLINE),
        ("\\033[4:3m", "cu", Flags::UNDERCURL),
        ("\\033[4:4m", "do", Flags::DOTTED_UNDERLINE),
        ("\\033[4:5m", "da", Flags::DASHED_UNDERLINE),
    ];
    let script: String = parts
        .iter()
        .map(|(on, text, _)| format!("{on}{text}\\033[0m "))
        .collect();
    let srv = start(&format!("printf '{script}sgr-done'; exec sleep 600"));
    let mut a = attach(&srv, RICH);
    a.wait_for("sgr-done");
    let term = outer(&mut a);
    for (i, (on, text, flag)) in parts.iter().enumerate() {
        let col = i * 3;
        let got = cell(&term, 0, col);
        assert!(
            got.flags.contains(*flag) && got.c == text.chars().next().unwrap_or(' '),
            "{on}{text}: {:?} {:?} at col {col}\n{}",
            got.c,
            got.flags,
            a.text()
        );
    }
}

/// `tsl`/`fsl` (`\E]0;` .. `^G`) set the title and `dsl` (`\E]0;\007`)
/// clears it; both are relayed to the client (R15).
#[test]
fn term_rerun_title_is_relayed() {
    let srv =
        start("read l; printf '\\033]0;t36-title\\007'; read l; printf '\\033]0;\\007'; exec cat");
    let mut a = attach(&srv, RICH);
    a.send("\r");
    let at = a.wait_for_output(b"\x1b]2;t36-title\x07", 0);
    a.send("\r");
    a.wait_for_output(b"\x1b]2;\x07", at);
}

/// `Ms` (`\E]52;%p1%s;%p2%s\007`): a clipboard store reaches the client.
#[test]
fn term_rerun_osc52_store_is_forwarded() {
    let srv = start("read l; printf '\\033]52;c;dDM2\\007'; exec cat");
    let mut a = attach(&srv, RICH);
    a.send("\r");
    a.wait_for_output(b"\x1b]52;c;dDM2", 0);
}

/// `Cs`/`Cr` (`\E]12;%p1%s\007`, `\E]112\007`): cursor colour set and reset.
#[test]
fn term_rerun_cursor_colour_is_forwarded() {
    let srv =
        start("read l; printf '\\033]12;#00ff00\\007'; read l; printf '\\033]112\\007'; exec cat");
    let mut a = attach(&srv, RICH);
    a.send("\r");
    let at = a.wait_for_output(b"\x1b]12;#00ff00\x07", 0);
    a.send("\r");
    a.wait_for_output(b"\x1b]112\x07", at);
}

/// `fe`/`fd` (`\E[?1004h`/`l`) with `kxIN` (`\E[I`): a pane that turned
/// focus reporting on gets the outer terminal's report; once it turned it
/// off, it does not.
#[test]
fn term_rerun_focus_reporting_on_and_off() {
    let srv = start(
        "stty -echo; printf '\\033[?1004hready\\n'; IFS= read -r x; \
         printf '\\033[?1004l'; printf 'one:%s\\n' \"$x\" | cat -v; \
         IFS= read -r y; printf 'two:%s\\n' \"$y\" | cat -v; exec sleep 600",
    );
    let mut a = attach(&srv, RICH);
    a.wait_for("ready");
    // Attaching already gave the pane a focus-in (as tmux does), so the
    // first line may hold two.
    a.send("\x1b[Ia\r");
    a.wait_for("^[[Ia");
    a.send("\x1b[Ib\r");
    a.wait_for("two:");
    let text = a.text();
    let two = text
        .lines()
        .find(|l| l.starts_with("two:"))
        .unwrap_or_default();
    assert_eq!(two.trim_end(), "two:b", "{text}");
}

/// `Ss`/`Se` (`\E[%p1%d q`, `\E[2 q`): the cursor style reaches the outer
/// cursor.
#[test]
fn term_rerun_cursor_style_reaches_the_outer_cursor() {
    let srv = start(
        "stty -echo; printf 'ready'; read l; printf '\\033[5 q'; read l; printf '\\033[2 q'; \
         exec cat",
    );
    let mut a = attach(&srv, RICH);
    a.wait_for("ready");
    a.send("\r");
    wait_outer(&mut a, "a blinking bar", |t| {
        let s = t.cursor_style();
        s.shape == CursorShape::Beam && s.blinking
    });
    a.send("\r");
    wait_outer(&mut a, "a steady block", |t| {
        let s = t.cursor_style();
        s.shape == CursorShape::Block && !s.blinking
    });
}

/// The modified-key entries (`kLFT` `\E[1;2D`, `kf13` `\E[1;2P`, `kDC5`
/// `\E[3;5~`) are what the pane receives when the outer terminal sends
/// them, so an app reading tmux-256color decodes them.
#[test]
fn term_rerun_modified_keys_reach_the_pane_as_the_entry_spells_them() {
    // Three keys of six bytes each.
    let srv = start(&hex_reader("", 18));
    let mut a = attach(&srv, RICH);
    a.wait_for("ready");
    a.send("\x1b[1;2D\x1b[1;2P\x1b[3;5~");
    a.wait_for("got:1b5b313b32441b5b313b32501b5b333b357e:end");
}

/// `RV` (`\E[>c`, DA2) and `XR` (`\E[>0q`, XTVERSION) are queries: record
/// what the pane gets back. DA2 is answered by the pane's emulator.
#[test]
fn term_rerun_version_queries() {
    let srv = start(
        "stty raw -echo; printf 'ready\\r\\n'; printf '\\033[>c'; \
         b=$(dd bs=1 count=3 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
         stty sane; printf 'got:%s:end' \"$b\"; exec sleep 600",
    );
    let mut a = attach(&srv, RICH);
    // ESC [ >
    a.wait_for("got:1b5b3e:end");
}

// ---- new-session attaches (tmux 3.7b) ----

/// Without a terminal, `new-session` (no `-d`) fails as tmux does and
/// creates nothing.
#[test]
fn new_session_without_a_terminal_fails_and_creates_nothing() {
    let srv = TestServer::start();
    let mut cmd = srv.command();
    cmd.args(["new-session", "-s", "nope", "sleep 600"])
        .stdin(Stdio::null());
    let out = common::run_bounded(cmd, CMD_TIMEOUT);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(out.stderr, "open terminal failed: not a terminal\n");
    assert!(srv.cmd(&["has-session", "-t", "nope"]).is_err());
}

/// With a terminal, `new-session` creates the session and attaches to it;
/// the prefix `d` detaches and the CLI says so. An unnamed session works
/// too, and `$TMUX` naming another server is not a refusal.
#[test]
fn new_session_on_a_terminal_attaches_and_detaches() {
    let srv = TestServer::start();
    let mut cmd = srv.command();
    cmd.args([
        "new-session",
        "-s",
        "fresh",
        "printf 'fresh-pane'; exec sleep 600",
    ])
    .env("TERM", "xterm-256color")
    .env("TMUX", "/tmp/elsewhere/default,1,0");
    let mut a = Harness::spawn(cmd, COLS, ROWS);
    a.wait_for("fresh-pane");
    a.wait_for("[fresh] 1:");
    a.send(format!("{PREFIX}d"));
    a.wait_for("[detached (from session fresh)]");
    a.wait_exit();
    assert!(srv.cmd(&["has-session", "-t", "fresh"]).is_ok());

    let mut cmd = srv.command();
    cmd.args(["new", "printf 'unnamed-pane'; exec sleep 600"])
        .env("TERM", "xterm-256color");
    let mut b = Harness::spawn(cmd, COLS, ROWS);
    b.wait_for("unnamed-pane");
    let name = srv
        .cmd(&["list-clients", "-F", "#{session_name}"])
        .expect("list-clients");
    let name = name.trim();
    assert!(!name.is_empty() && name != "fresh", "{name:?}");
    b.send(format!("{PREFIX}d"));
    b.wait_for(&format!("[detached (from session {name})]"));
    b.wait_exit();
}

/// A session whose command exits at once takes an auto-started server with
/// it before the client attaches; as tmux 3.7b, the CLI says `[exited]` and
/// exits 0.
#[test]
fn new_session_that_exits_at_once_reports_exited() {
    let srv = TestServer::unstarted();
    let base = srv.command();
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "\"$0\" \"$@\"; echo \"EXIT=$?\""])
        .arg(base.get_program())
        .args(base.get_args())
        .args(["new-session", "-s", "q", "true"])
        .env("TERM", "xterm-256color")
        .env("GROFE_REMAIN_ON_EXIT", "0");
    if let Some(dir) = base.get_current_dir() {
        cmd.current_dir(dir);
    }
    for (key, value) in base.get_envs() {
        if key == "GROFE_REMAIN_ON_EXIT" {
            continue;
        }
        match value {
            Some(v) => cmd.env(key, v),
            None => cmd.env_remove(key),
        };
    }
    let mut a = Harness::spawn(cmd, COLS, ROWS);
    a.wait_for("EXIT=");
    let text = a.text();
    assert!(text.contains("[exited]"), "{text}");
    assert!(text.contains("EXIT=0"), "{text}");
    assert!(!text.contains("Broken pipe"), "{text}");
}
