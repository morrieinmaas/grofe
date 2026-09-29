//! TC.10: grofe's conditional, bindable mouse keys and the built-in mouse
//! model, the default key table and custom tables. Mouse reports are typed
//! into the attach pty as SGR 1006, as the outer terminal sends them; what
//! a binding did is read back through options it sets.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{TestServer, wait_until};

use crate::harness::{Harness, require_tool};

const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const WAIT: Duration = Duration::from_secs(10);

/// A server with one session `main` sized for an 80x24 client with a
/// status line, running `cmd`.
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

fn attach(srv: &TestServer, cols: u16, rows: u16) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], cols, rows);
    a.wait_for_output(MOUSE_ON, 0);
    a
}

/// An SGR 1006 report: `x` and `y` are 1-based client cells.
fn sgr(button: u8, x: u16, y: u16, release: bool) -> String {
    let end = if release { 'm' } else { 'M' };
    format!("\x1b[<{button};{x};{y}{end}")
}

/// A press and release of tmux's button `button` (1 left, 2 middle, 3
/// right), whose SGR code is one less.
fn click(a: &mut Harness, button: u8, x: u16, y: u16) {
    a.send(sgr(button - 1, x, y, false));
    a.send(sgr(button - 1, x, y, true));
}

/// [`click`], after the last click's wait for a second one has ended, so
/// this one is a `MouseDown` of its own (as in tmux, a second press of the
/// same button within 300 ms is `SecondClick` wherever it lands).
fn click_alone(a: &mut Harness, button: u8, x: u16, y: u16) {
    std::thread::sleep(Duration::from_millis(400));
    click(a, button, x, y);
}

fn var(srv: &TestServer, target: &str, name: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, name])
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn opt(srv: &TestServer, name: &str) -> String {
    srv.cmd(&["show-options", "-gv", name])
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn wait_opt(srv: &TestServer, name: &str, want: &str) {
    let got = wait_until(|| opt(srv, name) == want, WAIT);
    assert!(
        got.is_ok(),
        "{name}: want {want:?}, got {:?}",
        opt(srv, name)
    );
}

fn bind(srv: &TestServer, args: &[&str]) {
    let mut argv = vec!["bind-key"];
    argv.extend_from_slice(args);
    srv.cmd(&argv).expect("bind-key");
}

/// The right pane `%N` after splitting `main` side by side.
fn split_h(srv: &TestServer, cmd: &str) -> String {
    srv.cmd(&[
        "split-window",
        "-h",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "main",
        cmd,
    ])
    .expect("split")
    .trim()
    .to_string()
}

/// The first pane of `main`'s only window, before any split.
fn first_pane(srv: &TestServer) -> String {
    srv.cmd(&["list-panes", "-t", "main", "-F", "#{pane_id}"])
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Neovim with C-h mapped to write `log` in `dir`, so the test sees the
/// key arrive.
fn logging_nvim(dir: &Path) -> String {
    format!(
        "exec nvim --clean -c \"nnoremap <C-h> <Cmd>call writefile(['C-h'], '{}')<CR>\"",
        dir.join("log").display()
    )
}

fn read_log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("log")).unwrap_or_default()
}

/// The vim-tmux-navigator shape the owner's canopy uses: root `C-h` bound
/// to the conditional, `send-keys C-h` when `nvim` is in front, else
/// `select-pane -L`.
const NVIM: &str = "#{m/r:^n?vim$,#{pane_current_command}}";

/// Case 1: from a shell, C-h moves focus left; with `nvim` in front
/// (when it is installed) C-h reaches it and focus stays.
#[test]
fn c_h_goes_to_nvim_or_moves_focus_from_a_shell() {
    let dir = tempfile::tempdir().expect("tempdir");
    let srv = start("exec sh");
    let left = first_pane(&srv);
    let right = split_h(&srv, "exec sh");
    bind(
        &srv,
        &[
            "-n",
            "C-h",
            "if-shell",
            "-F",
            NVIM,
            "send-keys C-h",
            "select-pane -L",
        ],
    );
    let mut a = attach(&srv, 80, 24);
    a.wait_for("$");
    assert_eq!(var(&srv, &right, "#{pane_active}"), "1");
    a.send(b"\x08");
    let moved = wait_until(|| var(&srv, &left, "#{pane_active}") == "1", WAIT);
    assert!(moved.is_ok(), "focus did not move left");

    if !require_tool("nvim") {
        return;
    }
    srv.cmd(&["select-pane", "-t", &right]).expect("select");
    srv.cmd(&[
        "respawn-pane",
        "-k",
        "-t",
        &right,
        &logging_nvim(dir.path()),
    ])
    .expect("respawn");
    let fg = wait_until(
        || var(&srv, &right, "#{pane_current_command}") == "nvim",
        WAIT,
    );
    assert!(
        fg.is_ok(),
        "{}",
        var(&srv, &right, "#{pane_current_command}")
    );
    // Neovim is up once it has drawn its tildes.
    a.wait_until("nvim drawn", WAIT, |t| {
        t.lines().filter(|l| l.contains('~')).count() > 3
    });
    a.send(b"\x08");
    let got = wait_until(|| read_log(dir.path()).trim() == "C-h", WAIT);
    assert!(got.is_ok(), "nvim wrote {:?}", read_log(dir.path()));
    assert_eq!(var(&srv, &right, "#{pane_active}"), "1", "focus stayed");
}

/// Case 2: a shell-form condition that sleeps does not hold the server:
/// a second client's typing shows at once, and the branch runs after.
#[test]
fn a_sleeping_shell_condition_does_not_delay_another_client() {
    let srv = start("exec sh");
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "other",
        "-x",
        "80",
        "-y",
        "23",
        "exec cat",
    ])
    .expect("other");
    bind(
        &srv,
        &[
            "-n",
            "C-h",
            "if-shell",
            "sleep 2",
            "set -g @done yes",
            "set -g @done no",
        ],
    );
    let mut a = attach(&srv, 80, 24);
    let mut b = Harness::spawn_attach_with_env(srv.command(), "other", &[], 80, 24);
    b.wait_for_output(MOUSE_ON, 0);

    a.send(b"\x08");
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    b.send("typed-while-sleeping\r");
    b.wait_until("the echo", WAIT, |t| {
        t.lines()
            .filter(|l| l.contains("typed-while-sleeping"))
            .count()
            >= 2
    });
    let took = t.elapsed();
    assert!(took < Duration::from_millis(1000), "took {took:?}");
    assert_eq!(opt(&srv, "@done"), "", "the branch waited for the shell");
    wait_opt(&srv, "@done", "yes");
}

/// The shell form from the CLI: without `-b` the reply waits for the
/// branch, with `-b` it does not; a failing shell picks the else branch;
/// other requests are answered meanwhile.
#[test]
fn the_shell_form_waits_on_the_cli_unless_backgrounded() {
    let srv = start("exec sh");
    let t = Instant::now();
    let out = srv.cmd(&[
        "if-shell",
        "sleep 1",
        "display-message -p yes",
        "display-message -p no",
    ]);
    assert_eq!(out.as_deref(), Ok("yes\n"));
    assert!(
        t.elapsed() >= Duration::from_millis(900),
        "{:?}",
        t.elapsed()
    );
    let out = srv.cmd(&[
        "if-shell",
        "exit 3",
        "display-message -p yes",
        "display-message -p no",
    ]);
    assert_eq!(out.as_deref(), Ok("no\n"));
    // The shell expands formats first, as tmux's does.
    let out = srv.cmd(&[
        "if-shell",
        "test '#{session_name}' = main",
        "display-message -p main",
    ]);
    assert_eq!(out.as_deref(), Ok("main\n"));

    let t = Instant::now();
    srv.cmd(&["if-shell", "-b", "sleep 1", "set -g @bg done"])
        .expect("-b");
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(opt(&srv, "@bg"), "");
    wait_opt(&srv, "@bg", "done");

    // A request while another's shell runs is answered at once.
    let slow = std::thread::spawn({
        let mut cmd = srv.command();
        cmd.args(["if-shell", "sleep 2", "display-message -p late"]);
        move || cmd.output().expect("run")
    });
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    assert_eq!(var(&srv, "main", "#{session_name}"), "main");
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "{:?}",
        t.elapsed()
    );
    let out = slow.join().expect("join");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "late\n");
}

/// The status column of `needle` on the last row of `a`'s screen, as a
/// 1-based cell.
fn status_col(a: &mut Harness, row: usize, needle: &str) -> u16 {
    let text = a.text();
    let line = text.lines().nth(row).unwrap_or_default().to_string();
    let col = line
        .find(needle)
        .unwrap_or_else(|| panic!("{needle} in {line:?}"));
    line[..col].chars().count() as u16 + 1
}

/// The owner's `MouseDown1Status` binding: a click on the `net` range runs
/// its branch, anywhere else the else branch, `switch-client -t =`, which
/// (M36-R3) switches to the window clicked.
fn owner_status_binding(srv: &TestServer) {
    let log = log_path(srv);
    srv.cmd(&[
        "set-environment",
        "-g",
        "GROFE_TC10_LOG",
        &log.display().to_string(),
    ])
    .expect("setenv");
    srv.cmd(&[
        "set",
        "-g",
        "status-right",
        "#[range=user|net]NET#[norange] #[range=user|cpu]CPU#[norange]",
    ])
    .expect("status-right");
    bind(
        srv,
        &[
            "-n",
            "MouseDown1Status",
            "if-shell",
            "-F",
            "#{m:*net*,#{mouse_status_range}}",
            // As the owner's `display-popup "net-popup.sh #{mouse_status_range}"`:
            // a command's formats see the range (here run-shell's).
            "set -g @hit net ; run-shell -b 'printf %s #{mouse_status_range}:#{mouse_x},#{mouse_y} > \"$GROFE_TC10_LOG\"'",
            "switch-client -t =",
        ],
    );
}

/// Where the owner-shaped binding's shell writes the range it saw.
fn log_path(srv: &TestServer) -> std::path::PathBuf {
    srv.log_path().with_file_name("tc10-range.log")
}

/// Waits until the owner-shaped binding's shell has written `want`.
fn wait_log(srv: &TestServer, want: &str) {
    let log = log_path(srv);
    let read = || std::fs::read_to_string(&log).unwrap_or_default();
    let got = wait_until(|| read() == want, WAIT);
    assert!(got.is_ok(), "want {want:?}, got {:?}", read());
}

/// Cases 3 and 7: `MouseDown1Status` conditioned on the range fires its
/// branch only on `net`; the else branch's bare `=` is the window clicked.
#[test]
fn a_status_click_dispatches_on_its_range() {
    let srv = start("exec sh");
    srv.cmd(&["new-window", "-d", "-t", "main", "exec sh"])
        .expect("window 1");
    owner_status_binding(&srv);
    let mut a = attach(&srv, 80, 24);
    a.wait_until("the status ranges", WAIT, |t| t.contains("NET CPU"));
    srv.cmd(&["set", "-g", "@hit", "none"]).expect("reset");

    let cpu = status_col(&mut a, 23, "CPU");
    click_alone(&mut a, 1, cpu, 24);
    let net = status_col(&mut a, 23, "NET");
    click_alone(&mut a, 1, net + 1, 24);
    wait_opt(&srv, "@hit", "net");
    // `mouse_x` is the client column and `mouse_y` the status row, as
    // tmux 3.7b gives them (recorded: `5,0`).
    wait_log(&srv, &format!("net:{net},0"));
    assert_eq!(var(&srv, "main", "#{window_index}"), "1", "cpu switched");

    // Window 2's entry in the list: the else branch switches to it.
    let two = status_col(&mut a, 23, "2:");
    click_alone(&mut a, 1, two, 24);
    let switched = wait_until(|| var(&srv, "main", "#{window_index}") == "2", WAIT);
    assert!(
        switched.is_ok(),
        "window {}",
        var(&srv, "main", "#{window_index}")
    );
    assert_eq!(opt(&srv, "@hit"), "net");

    // Unbound, the wheel on the status line goes to the previous window.
    a.send(sgr(64, two, 24, false));
    let back = wait_until(|| var(&srv, "main", "#{window_index}") == "1", WAIT);
    assert!(
        back.is_ok(),
        "window {}",
        var(&srv, "main", "#{window_index}")
    );
    // Outside a mouse key's commands a bare `=` is no target (M36-R3).
    assert!(srv.cmd(&["select-window", "-t", "="]).is_err());
}

/// TC.11's note: with the status line on top, a status click lands on
/// the top row and a pane click on the row below it.
#[test]
fn clicks_map_with_the_status_line_on_top() {
    let srv = start("exec sh");
    srv.cmd(&["set", "-g", "status-position", "top"])
        .expect("top");
    owner_status_binding(&srv);
    let right = split_h(&srv, "exec sh");
    let left = first_pane(&srv);
    let mut a = attach(&srv, 80, 24);
    a.wait_until("the status on top", WAIT, |t| {
        t.lines().next().is_some_and(|l| l.contains("NET CPU"))
    });
    let net = status_col(&mut a, 0, "NET");
    click_alone(&mut a, 1, net, 1);
    wait_opt(&srv, "@hit", "net");
    wait_log(&srv, &format!("net:{},0", net - 1));
    // Row 2 is the window's first row: the left pane.
    click_alone(&mut a, 1, 3, 2);
    let focused = wait_until(|| var(&srv, &left, "#{pane_active}") == "1", WAIT);
    assert!(focused.is_ok(), "right still active: {right}");
}

/// Case 4: dragging the border between two side-by-side panes resizes
/// both as the drag goes (recorded on tmux 3.7b: the border follows the
/// pointer).
#[test]
fn dragging_a_border_resizes_both_panes() {
    let srv = start("exec sh");
    let right = split_h(&srv, "exec sh");
    let left = first_pane(&srv);
    let width = |p: &str| var(&srv, p, "#{pane_width}").parse::<u16>().unwrap_or(0);
    let mut a = attach(&srv, 80, 24);
    a.wait_for("$");
    let (l, r) = (width(&left), width(&right));
    let total = l + r;

    // The border is column `l`, cell `l + 1`.
    a.send(sgr(0, l + 1, 5, false));
    a.send(sgr(32, l + 4, 5, false));
    let moved = wait_until(|| width(&left) == l + 3, WAIT);
    assert!(moved.is_ok(), "{} {}", width(&left), width(&right));
    assert_eq!(width(&right), total - l - 3);
    a.send(sgr(32, 31, 6, false));
    a.send(sgr(0, 31, 6, true));
    let back = wait_until(|| width(&left) == 30, WAIT);
    assert!(back.is_ok(), "{} {}", width(&left), width(&right));
    assert_eq!(width(&right), total - 30);

    // A stacked split's border moves with the rows.
    let below = srv
        .cmd(&[
            "split-window",
            "-v",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            &left,
            "exec sh",
        ])
        .expect("split -v")
        .trim()
        .to_string();
    let height = |p: &str| var(&srv, p, "#{pane_height}");
    let top: u16 = height(&left).parse().expect("height");
    let border = top + 1;
    a.send(sgr(0, 5, border, false));
    a.send(sgr(32, 5, border - 3, false));
    a.send(sgr(0, 5, border - 3, true));
    let want = (top - 3).to_string();
    let up = wait_until(|| height(&left) == want, WAIT);
    assert!(up.is_ok(), "{} {}", height(&left), height(&below));
}

/// Case 5: `DoubleClick1Pane` fires once per double click, after the
/// click wait, and not for a single or a triple click; `TripleClick1Pane`
/// fires for the third.
#[test]
fn double_click_fires_once() {
    let srv = start("exec sh");
    bind(&srv, &["-n", "DoubleClick1Pane", "set", "-ga", "@log", "D"]);
    bind(&srv, &["-n", "TripleClick1Pane", "set", "-ga", "@log", "T"]);
    let mut a = attach(&srv, 80, 24);
    a.wait_for("$");
    click(&mut a, 1, 10, 5);
    click(&mut a, 1, 10, 5);
    wait_opt(&srv, "@log", "D");
    std::thread::sleep(Duration::from_millis(500));
    click(&mut a, 1, 10, 5);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(opt(&srv, "@log"), "D", "a single click");
    for _ in 0..3 {
        click(&mut a, 1, 10, 5);
    }
    wait_opt(&srv, "@log", "DT");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(opt(&srv, "@log"), "DT");
}

/// Mouse-bound commands act on the pane under the pointer, not the active
/// one; case 6's pointer, which `display-menu` opens at, is the clicked
/// cell (`#{mouse_x}`, `#{mouse_y}` inside the pane).
#[test]
fn a_mouse_binding_acts_on_the_pane_under_the_pointer() {
    let srv = start("exec sh");
    let right = split_h(&srv, "exec sh");
    let left = first_pane(&srv);
    bind(
        &srv,
        &[
            "-n",
            "MouseDown3Pane",
            "if-shell",
            "-F",
            "#{&&:#{==:#{mouse_x},4},#{==:#{mouse_y},6}}",
            "set -g @at pointer",
            "set -g @at elsewhere",
        ],
    );
    bind(&srv, &["-n", "MouseDown2Pane", "kill-pane"]);
    let mut a = attach(&srv, 80, 24);
    a.wait_for("$");
    assert_eq!(var(&srv, &right, "#{pane_active}"), "1");

    // Cell (5, 7) is the left pane's cell (4, 6).
    click_alone(&mut a, 3, 5, 7);
    wait_opt(&srv, "@at", "pointer");
    click_alone(&mut a, 3, 6, 7);
    wait_opt(&srv, "@at", "elsewhere");

    // `kill-pane` with no target kills the pane clicked.
    click_alone(&mut a, 2, 5, 7);
    let gone = wait_until(
        || {
            srv.cmd(&["display-message", "-p", "-t", &left, "x"])
                .is_err()
        },
        WAIT,
    );
    assert!(gone.is_ok(), "the left pane is still there");
    assert_eq!(var(&srv, &right, "#{pane_id}"), right);
}

/// The corpus's wheel condition routes by the pane under the pointer, as
/// tmux's stock `WheelUpPane` does: an alternate-screen pane is `send -M`,
/// a shell is copy mode, whichever pane is active.
#[test]
fn the_wheel_condition_routes_by_the_pointer_pane() {
    let srv = start("exec sh");
    let right = split_h(&srv, "printf '\\033[?1049h'; exec cat");
    wait_until(|| var(&srv, &right, "#{alternate_on}") == "1", WAIT).expect("alt screen");
    bind(
        &srv,
        &[
            "-n",
            "WheelUpPane",
            "if-shell",
            "-F",
            "#{||:#{alternate_on},#{pane_in_mode},#{mouse_any_flag}}",
            "set -g @w send",
            "set -g @w copy",
        ],
    );
    let mut a = attach(&srv, 80, 24);
    // The right pane is active; the wheel over the left one is copy mode.
    a.send(sgr(64, 5, 5, false));
    wait_opt(&srv, "@w", "copy");
    a.send(sgr(64, 60, 5, false));
    wait_opt(&srv, "@w", "send");
}

/// A key bound in a custom table runs after `switch-client -T` enters it;
/// the table then ends, so the key types again.
#[test]
fn a_custom_table_takes_the_next_key() {
    let srv = start("exec cat");
    bind(&srv, &["-T", "mine", "x", "set", "-g", "@t", "yes"]);
    bind(&srv, &["-n", "C-y", "switch-client", "-T", "mine"]);
    let mut a = attach(&srv, 80, 24);
    a.send(b"\x19x");
    wait_opt(&srv, "@t", "yes");
    a.send("x\r");
    let typed = wait_until(|| srv.capture("main").contains('x'), WAIT);
    assert!(typed.is_ok(), "{}", srv.capture("main"));
    let keys = srv.cmd(&["list-keys", "-T", "mine"]).expect("list-keys");
    assert!(keys.contains("-T mine x set -g @t yes"), "{keys}");
}

/// Case 8 (D4): M-S-Tab from a kitty terminal sends a pane nothing.
#[test]
fn meta_shift_tab_sends_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("log");
    let srv = start(&format!("stty raw -echo; exec cat > '{}'", log.display()));
    let mut a = attach(&srv, 80, 24);
    std::thread::sleep(Duration::from_millis(300));
    a.send(b"\x1b[9;4u\x1b[9;2uz");
    let got = wait_until(
        || std::fs::read(&log).unwrap_or_default().ends_with(b"z"),
        WAIT,
    );
    assert!(got.is_ok(), "{:?}", std::fs::read(&log));
    assert_eq!(std::fs::read(&log).unwrap_or_default(), b"\x1b[Zz");
}

/// Case 8 (D10): the newly wired stock keys run their verbs, and every
/// default carries tmux's note.
#[test]
fn the_new_default_keys_run_their_verbs() {
    let srv = start("exec sh");
    let right = split_h(&srv, "exec sh");
    let left = first_pane(&srv);
    let mut a = attach(&srv, 80, 24);
    a.wait_for("$");
    // `;` goes back to the previously active pane.
    a.send(b"\x02;");
    let last = wait_until(|| var(&srv, &left, "#{pane_active}") == "1", WAIT);
    assert!(last.is_ok(), "; did not go to the last pane");
    // `C-o` rotates: the left pane's process moves right.
    a.send(b"\x02\x0f");
    let second = || {
        let panes = srv.cmd(&["list-panes", "-t", "main", "-F", "#{pane_id}"]);
        panes
            .unwrap_or_default()
            .lines()
            .nth(1)
            .unwrap_or_default()
            .to_string()
    };
    let rotated = wait_until(|| second() == left, WAIT);
    assert!(rotated.is_ok(), "{}", second());
    // `E` spreads them evenly after a resize.
    srv.cmd(&["resize-pane", "-t", &right, "-x", "20"])
        .expect("resize");
    a.send(b"\x02E");
    let widths = || {
        let w = |p: &str| var(&srv, p, "#{pane_width}").parse::<u16>().unwrap_or(0);
        (w(&left), w(&right))
    };
    let even = wait_until(
        || {
            let (l, r) = widths();
            l.abs_diff(r) <= 1
        },
        WAIT,
    );
    assert!(even.is_ok(), "{:?}", widths());
    let keys = srv.cmd(&["list-keys", "-T", "prefix"]).expect("list-keys");
    let line = keys
        .lines()
        .find(|l| l.contains("-T prefix \\; last-pane") || l.contains("-T prefix ; last-pane"))
        .unwrap_or_else(|| panic!("{keys}"));
    assert!(
        line.contains("Move to the previously active pane"),
        "{line}"
    );
}

/// TC.11's note: with the status line on top, the wheel opens the view in
/// the pane under the pointer and the view is drawn below the status row,
/// its indicator on the pane's first row.
#[test]
fn the_view_draws_below_a_status_line_on_top() {
    let srv = start("seq 1 200; exec cat");
    srv.cmd(&["set", "-g", "status-position", "top"])
        .expect("top");
    let mut a = attach(&srv, 80, 24);
    a.wait_until("200 shown", WAIT, |t| {
        t.lines().any(|l| l.trim_end() == "200")
    });
    a.send(sgr(64, 10, 5, false));
    a.wait_until("the view", WAIT, |t| {
        let second = t.lines().nth(1).unwrap_or_default();
        second.trim_end().ends_with(']') && second.contains("[5/")
    });
    let text = a.text();
    let first = text.lines().next().unwrap_or_default();
    assert!(first.contains("[main]"), "status row replaced: {first:?}");
}

/// Case 6: a `MouseDown3Pane` menu with no position opens at the pointer,
/// where tmux 3.7b's `-x M -y M` puts it (recorded: a click at cell
/// (20, 8) draws the 11-wide box from (15, 8)), and its keys work.
#[test]
fn a_mouse_menu_opens_at_the_pointer() {
    let srv = start("exec sh");
    bind(
        &srv,
        &[
            "-n",
            "MouseDown3Pane",
            "display-menu",
            "-T",
            "T",
            "One",
            "o",
            "set -g @m one",
            "Two",
            "t",
            "set -g @m two",
        ],
    );
    let mut a = attach(&srv, 80, 24);
    a.wait_for("$");
    click_alone(&mut a, 3, 21, 9);
    a.wait_until("the menu", WAIT, |t| t.contains("One"));
    let text = a.text();
    let row = text.lines().nth(8).unwrap_or_default();
    let left = row.find('┌').map(|i| row[..i].chars().count());
    assert_eq!(left, Some(15), "{text}");
    assert!(row.contains("┌─T───────┐"), "{row:?}");
    assert!(
        text.lines().nth(9).unwrap_or_default().contains("One (o)"),
        "{text}"
    );
    a.send("t");
    wait_opt(&srv, "@m", "two");
}

/// Fix round 1: what the client types after a binding whose shell
/// condition is still running waits for the branch, in order, as tmux's
/// per-client queue does (recorded on tmux 3.7b: `Xab`); a failing branch
/// shows its error on the status line.
#[test]
fn keys_after_a_shell_condition_wait_for_its_branch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("log");
    let srv = start(&format!("stty raw -echo; exec cat > '{}'", log.display()));
    bind(
        &srv,
        &[
            "-n",
            "C-h",
            "if-shell",
            "sleep 0.5",
            "send-keys X",
            "send-keys Y",
        ],
    );
    bind(&srv, &["-n", "C-g", "if-shell", "true", "splat-window"]);
    let mut a = attach(&srv, 80, 24);
    std::thread::sleep(Duration::from_millis(300));
    a.send(b"\x08ab\r");
    let read = || std::fs::read(&log).unwrap_or_default();
    let got = wait_until(|| read().ends_with(b"\r"), WAIT);
    assert!(got.is_ok(), "{:?}", String::from_utf8_lossy(&read()));
    assert_eq!(read(), b"Xab\r");

    a.send(b"\x07");
    a.wait_until("the branch's error", WAIT, |t| {
        t.lines()
            .last()
            .is_some_and(|l| l.contains("unknown command: splat-window"))
    });
}
