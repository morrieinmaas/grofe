//! TC.11: status breadth (spec §4.10, §4.5's status-line row). Rows are
//! checked two ways: through `grofe::server::status`'s drawing API with
//! fixed variables and clock, for cells and colours (T2.6's style), and
//! through a pty-attached client for where the rows land and when they
//! repaint. Every expected layout below was recorded on tmux 3.7b (`-L
//! grofe-tc11 -f /dev/null`, a client attached in a pty).

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::collections::BTreeMap;
use std::time::Duration;

use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use common::TestServer;
use grofe::cmd::FormatVars;
use grofe::server::status::{
    RangeKind, StatusInputs, StatusRow, Style, WindowOptions, WindowStatus, draw, draw_row,
    parse_style, render_row, trim_left,
};
use harness::Harness;
use nix::libc;

const WAIT: Duration = Duration::from_secs(10);
const PREFIX: &str = "\x02";

struct Vars(BTreeMap<&'static str, String>);

impl FormatVars for Vars {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

fn vars(pairs: &[(&'static str, &str)]) -> Vars {
    Vars(pairs.iter().map(|(k, v)| (*k, v.to_string())).collect())
}

fn noon() -> libc::tm {
    // SAFETY: `libc::tm` is plain old data; zeroed is valid, and the
    // fields strftime reads are set below.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = 126;
    tm.tm_mon = 8;
    tm.tm_mday = 29;
    tm.tm_hour = 12;
    tm
}

fn text(row: &StatusRow) -> String {
    row.line.spans.iter().map(|s| s.text.as_str()).collect()
}

/// The style of the cell at column `x` (every test string is one column
/// per char).
fn style_at(row: &StatusRow, x: usize) -> Style {
    let mut col = 0;
    for span in &row.line.spans {
        let n = span.text.chars().count();
        if x < col + n {
            return span.style;
        }
        col += n;
    }
    panic!("column {x} is past the row: {:?}", text(row));
}

fn rgb(hex: u32) -> Color {
    Color::Spec(Rgb {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
    })
}

fn inputs<'a>(
    left: &'a str,
    right: &'a str,
    justify: &'a str,
    vars: &'a dyn FormatVars,
    windows: &'a [WindowStatus<'a>],
    width: u16,
) -> StatusInputs<'a> {
    StatusInputs {
        left,
        right,
        left_length: 10,
        right_length: 40,
        style: "bg=green,fg=black",
        left_style: "default",
        right_style: "default",
        justify,
        vars,
        windows,
        width,
        now: noon(),
    }
}

/// The three windows the tmux recordings used: 1 `alpha` (current), 2
/// `beta`, 3 `gamma`, with tmux's default window formats.
fn three_windows<'a>(v: &'a [Vars; 3], current: usize) -> Vec<WindowStatus<'a>> {
    (0..3)
        .map(|i| WindowStatus {
            format: "#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }",
            vars: &v[i],
            index: i as u32 + 1,
            current: i == current,
            last: false,
            bell: false,
            options: WindowOptions::default(),
        })
        .collect()
}

fn window_vars(current: usize) -> [Vars; 3] {
    let names = ["alpha", "beta", "gamma"];
    std::array::from_fn(|i| {
        let flags = if i == current { "*" } else { "" };
        vars(&[
            ("window_index", &(i + 1).to_string()),
            ("window_name", names[i]),
            ("window_flags", flags),
        ])
    })
}

/// Case 3 and `status-justify`: each justification places the window list
/// where tmux 3.7b does, between `LEFTLONGER` and `R` in 40 columns.
#[test]
fn status_justify_places_the_window_list_as_tmux() {
    let wv = window_vars(0);
    let windows = three_windows(&wv, 0);
    let v = vars(&[]);
    for (justify, want) in [
        ("left", "LEFTLONGER1:alpha* 2:beta  3:gamma     R"),
        ("centre", "LEFTLONGER  1:alpha* 2:beta  3:gamma   R"),
        ("right", "LEFTLONGER    1:alpha* 2:beta  3:gamma R"),
        (
            "absolute-centre",
            "LEFTLON1:alpha* 2:beta  3:gamma        R",
        ),
    ] {
        let row = render_row(&inputs("LEFTLONGER", "R", justify, &v, &windows, 40));
        assert_eq!(text(&row), want, "{justify}");
    }
}

/// Case 3: `#[align=centre]` centres its text between the left and right
/// parts, and `absolute-centre` centres on the whole row.
#[test]
fn align_centre_centres() {
    let base = parse_style("bg=green,fg=black");
    let row = draw("#[align=centre]mid", base, 40);
    assert_eq!(
        text(&row),
        format!("{}mid{}", " ".repeat(19), " ".repeat(18))
    );
    let row = draw("ab#[align=centre]mid#[align=right]zz", base, 40);
    assert_eq!(
        text(&row),
        format!("ab{}mid{}zz", " ".repeat(17), " ".repeat(16))
    );
    let row = draw(
        "abcdefghijklmnopqrstuvwxyz#[align=absolute-centre]MID#[align=right]zz",
        base,
        40,
    );
    assert_eq!(
        text(&row),
        format!("abcdefghijklmnopqrMIDvwxyz{}zz", " ".repeat(12))
    );
}

/// The gap between sections is `status-style`'s, and a style carries on
/// across `#[align=...]` (tmux 3.7b: `#[bg=red]row one#[align=right]R1`).
#[test]
fn a_style_carries_across_an_align_and_the_gap_is_the_base() {
    let base = parse_style("bg=green,fg=black");
    let row = draw("#[bg=red]row one#[align=right]R1", base, 20);
    assert_eq!(text(&row), "row one           R1");
    assert_eq!(style_at(&row, 0).bg, Color::Named(NamedColor::Red));
    assert_eq!(style_at(&row, 10).bg, Color::Named(NamedColor::Green));
    assert_eq!(style_at(&row, 19).bg, Color::Named(NamedColor::Red));
}

/// Case 2: `#[range=user|net]...#[norange]` records `net` under the cells
/// it spans, and a click there reads `net` as `mouse_status_range`.
#[test]
fn a_user_range_records_its_name_under_each_cell() {
    let row = draw("ab#[range=user|net]NET#[norange]cd", Style::default(), 20);
    assert_eq!(text(&row).trim_end(), "abNETcd");
    for x in 2..5 {
        let range = row.range_at(x).expect("a range under NET");
        assert_eq!(range.kind, RangeKind::User("net".to_string()), "{x}");
        assert_eq!(range.kind.mouse_status_range(), "net");
    }
    assert!(row.range_at(1).is_none());
    assert!(row.range_at(7).is_none());

    // The same through status-right, where canopy puts it, and the
    // default row's own left, window and right ranges.
    let v = vars(&[]);
    let wv = window_vars(0);
    let windows = three_windows(&wv, 0);
    let mut i = inputs(
        "[s] ",
        "#[range=user|net]NET#[norange] 12:00",
        "left",
        &v,
        &windows,
        60,
    );
    i.right_length = 40;
    let row = render_row(&i);
    let line = text(&row);
    let net = line.find("NET").expect("NET drawn") as u16;
    assert_eq!(
        row.range_at(net + 1).map(|r| r.kind.mouse_status_range()),
        Some("net".to_string())
    );
    assert_eq!(
        row.range_at(0).map(|r| r.kind.clone()),
        Some(RangeKind::Left)
    );
    let beta = line.find("2:beta").expect("beta drawn") as u16;
    assert_eq!(
        row.range_at(beta).map(|r| r.kind.clone()),
        Some(RangeKind::Window(2))
    );
    // The user range ended the right range where it began, as in tmux,
    // so the clock after it is in no range.
    assert!(row.range_at(59).is_none());
}

/// Case 4 (the drawing half): a colour chosen by `#{client_prefix}`
/// follows it. The pty test below checks the prefix state itself.
#[test]
fn the_client_prefix_colour_follows_client_prefix() {
    for (prefix, want) in [("1", NamedColor::Red), ("0", NamedColor::Green)] {
        let v = vars(&[("client_prefix", prefix)]);
        let row = draw_row("#[bg=#{?client_prefix,red,green}] S ", "", &v, 10, &noon());
        assert_eq!(style_at(&row, 1).bg, Color::Named(want), "{prefix}");
    }
}

/// Case 6: an overlong row trims the centre, then the window list (with
/// tmux's `<`/`>` markers around the current window), then status-right,
/// then status-left: the left-hand side is what stays, and a row of
/// `status-format[1]` truncates the same way.
#[test]
fn overlong_parts_truncate_on_tmuxs_side() {
    let v = vars(&[]);
    let windows: Vec<WindowStatus> = Vec::new();
    let mut i = inputs(
        "left-side-content-that-is-quite-long",
        "clock",
        "left",
        &v,
        &windows,
        30,
    );
    i.left_length = 100;
    i.right_length = 100;
    assert_eq!(text(&render_row(&i)), "left-side-content-that-is-quit");

    // The list shrinks first, keeping the current window in view.
    let wv = window_vars(2);
    let windows = three_windows(&wv, 2);
    let row = render_row(&inputs("LL", "RR", "centre", &v, &windows, 20));
    assert_eq!(text(&row), "LL<:beta  3:gamma*RR");
    let wv = window_vars(0);
    let windows = three_windows(&wv, 0);
    let mut i = inputs("LL", "RR", "left", &v, &windows, 20);
    i.style = "bg=blue";
    let row = render_row(&i);
    assert_eq!(text(&row), "LL1:alpha* 2:beta>RR");

    // status-right-length keeps the start of status-right, as tmux's `=N`.
    let mut i = inputs("", "0123456789", "left", &v, &[], 20);
    i.right_length = 4;
    assert!(text(&render_row(&i)).ends_with("0123"));

    // A status-format row keeps its left side too.
    let row = draw_row("abcdefghij#[align=right]RIGHT", "", &v, 8, &noon());
    assert_eq!(text(&row), "abcdefgh");
    assert_eq!(trim_left("#[fg=red]abc#[default]de", 3), "#[fg=red]abc");
}

/// canopy's theme (colour is data): `#[fg=#{@theme_blue}]` inside a window
/// format and `#{...}` inside `status-style` itself, with an empty
/// `window-status-separator`.
#[test]
fn canopy_style_options_expand_their_formats() {
    let theme = [
        ("@theme_fg", "#3c3836"),
        ("@theme_bg", "#fbf1c7"),
        ("@theme_grey", "#a89984"),
    ];
    let v = vars(&theme);
    let wv: [Vars; 2] = std::array::from_fn(|i| {
        let mut pairs = theme.to_vec();
        let index = (i + 1).to_string();
        pairs.push(("window_index", &index));
        vars(&pairs)
    });
    let format = "#[fg=#{@theme_grey},bg=default]<#[fg=#{@theme_bg},bg=#{@theme_grey}] #{window_index} #[fg=#{@theme_grey},bg=default]> ";
    let options = WindowOptions {
        separator: "",
        ..WindowOptions::default()
    };
    let windows: Vec<WindowStatus> = (0..2)
        .map(|i| WindowStatus {
            format,
            vars: &wv[i],
            index: i as u32 + 1,
            current: i == 0,
            last: false,
            bell: false,
            options,
        })
        .collect();
    let mut i = inputs("", "", "left", &v, &windows, 20);
    i.style = "bg=default,fg=#{@theme_fg}";
    let row = render_row(&i);
    assert_eq!(text(&row), "< 1 > < 2 >         ");
    assert_eq!(style_at(&row, 0).fg, rgb(0xa89984));
    assert_eq!(style_at(&row, 2).fg, rgb(0xfbf1c7));
    assert_eq!(style_at(&row, 2).bg, rgb(0xa89984));
    // The gap after the list is status-style, its format expanded.
    assert_eq!(style_at(&row, 15).fg, rgb(0x3c3836));
    assert_eq!(style_at(&row, 15).bg, Color::Named(NamedColor::Background));
}

/// tmux's `#[fill=...]`, `#[push-default]`/`#[pop-default]` and an
/// invalid directive, which changes nothing.
#[test]
fn fill_push_default_and_a_bad_directive() {
    let row = draw("#[fill=blue]x", Style::default(), 4);
    assert_eq!(style_at(&row, 3).bg, Color::Named(NamedColor::Blue));
    let row = draw(
        "#[fg=red]#[push-default]a#[fg=blue]b#[default]c#[pop-default]#[default]d",
        Style::default(),
        4,
    );
    assert_eq!(style_at(&row, 2).fg, Color::Named(NamedColor::Red));
    assert_eq!(style_at(&row, 3).fg, Color::Named(NamedColor::Foreground));
    let row = draw("#[fg=red,nosuch]a", Style::default(), 2);
    assert_eq!(style_at(&row, 0).fg, Color::Named(NamedColor::Foreground));
}

// ---- through a pty-attached client ----

fn start(cols: u16, rows: u16) -> TestServer {
    let srv = TestServer::start();
    let (x, y) = (cols.to_string(), (rows - 1).to_string());
    srv.cmd(&["new-session", "-d", "-s", "s", "-x", &x, "-y", &y, "cat"])
        .unwrap_or_else(|e| panic!("new-session: {e}"));
    srv
}

fn attach(srv: &TestServer, cols: u16, rows: u16) -> Harness {
    Harness::spawn_attach_with_env(srv.command(), "s", &[], cols, rows)
}

fn set(srv: &TestServer, args: &[&str]) {
    let mut argv = vec!["set"];
    argv.extend_from_slice(args);
    srv.cmd(&argv)
        .unwrap_or_else(|e| panic!("set {args:?}: {e}"));
}

fn line(h: &mut Harness, n: usize) -> String {
    h.text().lines().nth(n).unwrap_or_default().to_string()
}

fn display(srv: &TestServer, fmt: &str) -> String {
    srv.cmd(&["display", "-p", "-t", "s", fmt])
        .unwrap_or_else(|e| panic!("display {fmt}: {e}"))
        .trim_end()
        .to_string()
}

/// Case 1: `status 2` with `status-position top` draws both rows at the
/// top of the client, row 0 then row 1, and the window takes the rows
/// below them.
#[test]
fn two_rows_draw_at_the_top_in_order() {
    let srv = start(40, 8);
    set(&srv, &["-g", "status", "2"]);
    set(&srv, &["-g", "status-format[1]", "second row"]);
    set(&srv, &["-g", "status-position", "top"]);
    let mut h = attach(&srv, 40, 8);
    h.wait_until("two rows at the top", WAIT, |t| {
        let mut rows = t.lines();
        rows.next().is_some_and(|r| r.starts_with("[s] 1:"))
            && rows.next().is_some_and(|r| r.starts_with("second row"))
    });
    assert_eq!(display(&srv, "#{pane_height}"), "6");
    srv.cmd(&["send-keys", "-t", "s", "hello", "Enter"])
        .unwrap();
    h.wait_until("pane output below the rows", WAIT, |t| {
        t.lines().nth(2).is_some_and(|r| r.starts_with("hello"))
    });

    // Back to the bottom: the rows follow, in the same order.
    set(&srv, &["-g", "status-position", "bottom"]);
    h.wait_until("two rows at the bottom", WAIT, |t| {
        let rows: Vec<&str> = t.lines().collect();
        rows.len() == 8 && rows[6].starts_with("[s] 1:") && rows[7].starts_with("second row")
    });
    assert!(line(&mut h, 0).starts_with("hello"));
}

/// Case 4: a status segment coloured (here: worded) by `#{client_prefix}`
/// shows only while the prefix is pending, and is gone after the next key.
#[test]
fn the_client_prefix_segment_shows_only_while_the_prefix_is_pending() {
    let srv = start(40, 5);
    set(
        &srv,
        &["-g", "status-left", "#{?client_prefix,PREFIX,------} "],
    );
    set(&srv, &["-g", "status-left-length", "20"]);
    let mut h = attach(&srv, 40, 5);
    h.wait_until("no prefix", WAIT, |t| {
        t.lines().nth(4).is_some_and(|r| r.starts_with("------"))
    });
    h.send(PREFIX);
    h.wait_until("the prefix pending", WAIT, |t| {
        t.lines().nth(4).is_some_and(|r| r.starts_with("PREFIX"))
    });
    // An unbound key ends the prefix.
    h.send("j");
    h.wait_until("the prefix gone", WAIT, |t| {
        t.lines().nth(4).is_some_and(|r| r.starts_with("------"))
    });
}

/// Case 5: setting the option a status format reads repaints on the next
/// frame, long before `status-interval`.
#[test]
fn setting_a_theme_option_repaints_at_once() {
    let srv = start(40, 5);
    set(&srv, &["-g", "status-interval", "3600"]);
    set(&srv, &["-g", "@theme_blue", "one"]);
    set(&srv, &["-g", "status-right", "#{@theme_blue}"]);
    let mut h = attach(&srv, 40, 5);
    h.wait_until("the first value", WAIT, |t| t.contains("one"));
    set(&srv, &["-g", "@theme_blue", "two"]);
    h.wait_until_timeout_short(|t| t.lines().nth(4).is_some_and(|r| r.ends_with("two")));
}

trait ShortWait {
    fn wait_until_timeout_short(&mut self, done: impl Fn(&str) -> bool);
}

impl ShortWait for Harness {
    /// A repaint within one frame, with generous room for a loaded host:
    /// far below any status-interval.
    fn wait_until_timeout_short(&mut self, done: impl Fn(&str) -> bool) {
        self.wait_until("a repaint", Duration::from_secs(2), done);
    }
}

/// Case 7 (D8, D9): tmux's default window formats show `window_flags`,
/// last `-` and zoom `Z`, joined by `window-status-separator`; and
/// `#{pane_title}` and `#{host_short}` in status-right are filled in,
/// `select-pane -T` included.
#[test]
fn window_flags_and_the_pane_title_reach_the_status_line() {
    let srv = start(60, 5);
    set(&srv, &["-g", "automatic-rename", "off"]);
    srv.cmd(&["rename-window", "-t", "s:1", "a"]).unwrap();
    srv.cmd(&["new-window", "-t", "s", "-n", "b", "cat"])
        .unwrap();
    srv.cmd(&["split-window", "-t", "s", "cat"]).unwrap();
    srv.cmd(&["resize-pane", "-Z", "-t", "s"]).unwrap();
    set(
        &srv,
        &["-g", "status-right", "|#{pane_title}|#{host_short}|"],
    );
    set(&srv, &["-g", "status-right-length", "100"]);
    let host = display(&srv, "#{host_short}");
    assert!(!host.is_empty());
    let mut h = attach(&srv, 60, 5);
    h.wait_until("flags", WAIT, |t| {
        t.lines()
            .nth(4)
            .is_some_and(|r| r.starts_with("[s] 1:a- 2:b*Z"))
    });
    // The title defaults to the host name, as in tmux.
    h.wait_until("the title", WAIT, |t| t.contains(&format!("|{host}")));
    srv.cmd(&["select-pane", "-t", "s", "-T", "my #{window_index}"])
        .unwrap();
    h.wait_until("the new title", WAIT, |t| {
        t.contains(&format!("|my 2|{host}|"))
    });
    assert_eq!(display(&srv, "#{pane_title}"), "my 2");
}

/// The message area: a binding's error shows on the first status row, at
/// the top with `status-position top`, and the second row stays.
#[test]
fn a_message_takes_the_first_status_row() {
    let srv = start(40, 6);
    set(&srv, &["-g", "status", "2"]);
    set(&srv, &["-g", "status-format[1]", "second row"]);
    set(&srv, &["-g", "status-position", "top"]);
    srv.cmd(&["bind", "-n", "F5", "select-window", "-t", ":9"])
        .unwrap();
    let mut h = attach(&srv, 40, 6);
    h.wait_until("rows", WAIT, |t| {
        t.lines().nth(1).is_some_and(|r| r.starts_with("second"))
    });
    h.send("\x1b[15~");
    h.wait_until("the message on row 0", WAIT, |t| {
        let mut rows = t.lines();
        rows.next()
            .is_some_and(|r| r.starts_with("can't find window"))
            && rows.next().is_some_and(|r| r.starts_with("second row"))
    });
}

/// `set -a -F -q -o`, as recorded on tmux 3.7b.
#[test]
fn set_append_format_quiet_and_only_if_unset() {
    let srv = start(40, 5);
    set(&srv, &["-ag", "status-left", "X"]);
    assert_eq!(
        show(&srv, &["-g", "status-left"]),
        "status-left \"[#{session_name}] X\""
    );
    // A style joins with a comma; a number takes the value.
    set(&srv, &["-ag", "status-style", "bg=red"]);
    assert_eq!(
        show(&srv, &["-g", "status-style"]),
        "status-style bg=green,fg=black,bg=red"
    );
    set(&srv, &["-ag", "status-interval", "5"]);
    assert_eq!(show(&srv, &["-g", "status-interval"]), "status-interval 5");
    // At a scope with no value, append is the value alone.
    set(&srv, &["-a", "-t", "s", "status-left", "Y"]);
    assert_eq!(show(&srv, &["-t", "s", "status-left"]), "status-left Y");
    set(&srv, &["-ag", "@u", "x"]);
    set(&srv, &["-ag", "@u", "y"]);
    set(&srv, &["-Fag", "@u", "#{session_name}"]);
    assert_eq!(show(&srv, &["-g", "@u"]), "@u xys");
    set(&srv, &["-Fg", "@v", "#{nosuch}x"]);
    assert_eq!(show(&srv, &["-g", "@v"]), "@v x");

    assert_eq!(
        err(&srv, &["set", "-og", "status-left", "Z"]),
        "already set: status-left"
    );
    set(&srv, &["-o", "-t", "s", "status-right", "Z"]);
    assert_eq!(show(&srv, &["-t", "s", "status-right"]), "status-right Z");
    assert_eq!(
        err(&srv, &["set", "-o", "-t", "s", "status-right", "W"]),
        "already set: status-right"
    );
    set(&srv, &["-oq", "-t", "s", "status-right", "W"]);
    set(&srv, &["-og", "@new", "1"]);
    assert_eq!(err(&srv, &["set", "-og", "@new", "2"]), "already set: @new");
    set(&srv, &["-q", "-g", "nosuch", "x"]);
    set(&srv, &["-uq", "-g", "nosuch"]);
    // -q never hides a bad value.
    assert_eq!(
        err(&srv, &["set", "-q", "-g", "status-interval", "abc"]),
        "value is invalid: abc"
    );
}

fn show(srv: &TestServer, args: &[&str]) -> String {
    let mut argv = vec!["show"];
    argv.extend_from_slice(args);
    srv.cmd(&argv)
        .unwrap_or_else(|e| panic!("show {args:?}: {e}"))
        .trim_end()
        .to_string()
}

fn err(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args).expect_err("an error").trim_end().to_string()
}
