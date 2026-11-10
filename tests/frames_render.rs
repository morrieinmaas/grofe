//! FR.2: drawing framed panes (spec 2026-11-05-frames.md, Drawing). Each
//! tiled pane gets its own frame round its content, `rounded` by default
//! with frames on, the active one in `pane-active-border-style`, the title
//! (`pane-border-format`) in the top edge unless `pane-frame-title` is off,
//! gap cells left blank; floating panes take the same frames above the
//! tiles, and the drop preview is a frame above the tiles and below the
//! floating panes. Frames off draws `rounded` as `single`.
//!
//! The compose tests build the frame inputs by hand from a padded layout
//! tree; the attached-client tests go through the server's wiring.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};
use common::TestServer;
use grofe::server::borders::PaneLines;
use grofe::server::layout::LayoutTree;
use grofe::server::pane::{CellFlags, CursorShape, CursorState, RowsSnapshot, SnapCell};
use grofe::server::render::{
    FloatingPane, FrameInputs, PaneLook, PaneLooks, Screen, compose, draw_floating, frame_tab,
    frame_title,
};
use grofe::server::status::{Style, parse_style};
use grofe::types::{Dir, PaneId, Rect, SplitSize};
use harness::Harness;

// ---- compose helpers -------------------------------------------------------

/// `text` through a pane-sized emulator, snapshotted as a pane is.
fn pane(cols: u16, rows: u16, text: &str) -> RowsSnapshot {
    let mut term = Term::new(
        Config::default(),
        &TermSize::new(cols as usize, rows as usize),
        VoidListener,
    );
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, text.replace('\n', "\r\n").as_bytes());
    let grid = term.grid();
    let rows_out = (0..grid.screen_lines())
        .map(|r| {
            (0..grid.columns())
                .map(|c| {
                    let cell = &grid[Line(r as i32)][Column(c)];
                    SnapCell {
                        c: cell.c,
                        fg: cell.fg,
                        bg: cell.bg,
                        flags: cell.flags,
                        zerowidth: Vec::new(),
                        underline_color: None,
                        hyperlink: None,
                    }
                })
                .collect()
        })
        .collect();
    let point = grid.cursor.point;
    RowsSnapshot {
        rows: rows_out,
        cursor: CursorState {
            row: point.line.0 as u16,
            col: point.column.0 as u16,
            visible: term.mode().contains(TermMode::SHOW_CURSOR),
            shape: CursorShape::Block,
            blink: false,
        },
        images: None,
    }
}

fn area(cols: u16, rows: u16) -> Rect {
    Rect {
        x: 0,
        y: 0,
        cols,
        rows,
    }
}

fn grown(r: Rect) -> Rect {
    Rect {
        x: r.x - 1,
        y: r.y - 1,
        cols: r.cols + 2,
        rows: r.rows + 2,
    }
}

const ACTIVE: &str = "fg=green";

/// Panes 1 | (2 / 3) in a 40x12 window, framed with gap `gap` (pad
/// `1 + gap`), or with tmux's shared borders when `gap` is `None`; pane
/// `n` shows `n: name` as its title and its name as its content.
fn three(gap: Option<u16>, lines: PaneLines, frame_title: bool) -> FrameInputs<'static> {
    let a = area(40, 12);
    let mut tree = LayoutTree::new(PaneId(1));
    if let Some(g) = gap {
        tree.set_pad(1 + g);
    }
    tree.split(a, PaneId(1), Dir::Horizontal, PaneId(2), SplitSize::Half)
        .expect("split 1");
    tree.split(a, PaneId(2), Dir::Vertical, PaneId(3), SplitSize::Half)
        .expect("split 2");
    let names = ["one", "two", "three"];
    let rects = tree.compute(a);
    let panes: Vec<(PaneId, Rect, RowsSnapshot)> = rects
        .iter()
        .map(|(id, r)| (*id, *r, pane(r.cols, r.rows, names[id.0 as usize - 1])))
        .collect();
    let mut looks = PaneLooks {
        lines,
        frame_title,
        frames: if gap.is_some() {
            rects.iter().map(|(id, r)| (*id, grown(*r))).collect()
        } else {
            Vec::new()
        },
        ..Default::default()
    };
    for (id, _) in &rects {
        let n = id.0 as usize;
        looks
            .panes
            .insert(*id, look(n, &format!("{n}: {}", names[n - 1])));
    }
    FrameInputs {
        client: (40, 12),
        viewport: (0, 0),
        area: a,
        borders: tree.borders(a),
        panes,
        border_style: Style::default(),
        active_border_style: parse_style(ACTIVE),
        active: PaneId(1),
        marked: None,
        status: None,
        cursor: None,
        graphics: None,
        images: None,
        look: looks,
    }
}

fn look(index: usize, title: &str) -> PaneLook {
    PaneLook {
        fg: Color::Named(NamedColor::Foreground),
        bg: Color::Named(NamedColor::Background),
        border: Style::default(),
        active_border: parse_style(ACTIVE),
        title: title.to_string(),
        tab: None,
        index,
    }
}

fn text(screen: &Screen) -> String {
    screen
        .cells
        .chunks(screen.cols as usize)
        .map(|row| {
            row.iter()
                .filter(|c| !c.flags.contains(CellFlags::WIDE_CHAR_SPACER))
                .map(|c| c.c)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn at(screen: &Screen, x: u16, y: u16) -> &SnapCell {
    &screen.cells[usize::from(y) * usize::from(screen.cols) + usize::from(x)]
}

/// The screen's text, then a map of which cells are in the active
/// style (`A`), another style (`s`), or the defaults (`.`).
fn dump(screen: &Screen) -> String {
    let active = parse_style(ACTIVE);
    let styles: Vec<String> = screen
        .cells
        .chunks(screen.cols as usize)
        .map(|row| {
            row.iter()
                .map(|c| {
                    if c.fg == active.fg {
                        'A'
                    } else if c.fg != Color::Named(NamedColor::Foreground)
                        || c.bg != Color::Named(NamedColor::Background)
                        || !c.flags.is_empty()
                    {
                        's'
                    } else {
                        '.'
                    }
                })
                .collect()
        })
        .collect();
    format!("{}\n--- styles\n{}\n", text(screen), styles.join("\n"))
}

fn is_active(cell: &SnapCell) -> bool {
    cell.fg == parse_style(ACTIVE).fg
}

// ---- compose tests ---------------------------------------------------------

#[test]
fn a_three_pane_framed_window_matches_its_fixture() {
    let screen = compose(&three(Some(1), PaneLines::Rounded, true));
    insta::assert_snapshot!("three_pane_framed_gap_1", dump(&screen));
}

#[test]
fn the_active_frame_takes_the_active_style_and_the_others_the_border_style() {
    let inputs = three(Some(1), PaneLines::Rounded, true);
    let screen = compose(&inputs);
    let rect = |id: u32| {
        inputs
            .look
            .frames
            .iter()
            .find(|(p, _)| p.0 == id)
            .unwrap()
            .1
    };
    for id in [1, 2, 3] {
        let r = rect(id);
        let corners = [
            (r.x, r.y),
            (r.x + r.cols - 1, r.y),
            (r.x, r.y + r.rows - 1),
            (r.x + r.cols - 1, r.y + r.rows - 1),
        ];
        for (x, y) in corners {
            assert_eq!(
                is_active(at(&screen, x, y)),
                id == 1,
                "pane {id} at {x},{y}"
            );
        }
        assert_eq!(is_active(at(&screen, r.x, r.y + 1)), id == 1);
    }
    // Gap cells are blank in the window's defaults.
    for (x, y) in [(0, 0), (0, 5), (20, 5), (39, 11)] {
        let c = at(&screen, x, y);
        assert_eq!(c.c, ' ', "gap {x},{y}");
        assert_eq!(c.fg, Color::Named(NamedColor::Foreground));
        assert_eq!(c.bg, Color::Named(NamedColor::Background));
    }
}

/// The control tab sits at the right end of a frame's top edge, one edge
/// cell before the corner, in the frame's own style: its ends plain, its
/// controls reversed, so it reads as a pill of the frame's colour. The
/// title keeps the left and is cut before the tab.
#[test]
fn the_control_tab_ends_the_title_edge_in_the_frames_colours() {
    let mut inputs = three(Some(1), PaneLines::Rounded, true);
    for l in inputs.look.panes.values_mut() {
        l.tab = Some(frame_tab("▐▌", false, false));
    }
    let screen = compose(&inputs);
    for (id, f) in inputs.look.frames.clone() {
        let (y, x1) = (f.y, f.x + f.cols - 1);
        let tab: String = (x1 - 8..x1 - 1).map(|x| at(&screen, x, y).c).collect();
        assert_eq!(tab, "▐z f x▌", "{id}:\n{}", text(&screen));
        assert_eq!(at(&screen, x1 - 1, y).c, '─');
        assert_eq!(at(&screen, x1, y).c, '╮');
        let end = at(&screen, x1 - 8, y);
        let z = at(&screen, x1 - 7, y);
        assert!(!end.flags.contains(CellFlags::INVERSE), "{id}");
        assert!(z.flags.contains(CellFlags::INVERSE), "{id}");
        assert_eq!(is_active(end), id == PaneId(1), "{id}");
        assert_eq!(is_active(z), id == PaneId(1), "{id}");
        let row = text(&screen)
            .lines()
            .nth(usize::from(y))
            .unwrap()
            .to_string();
        let n = id.0;
        assert!(row.contains(&format!("╭─ {n}: ")), "{row}");
    }
    // Zoomed and floating, the controls are unzoom and tile.
    assert_eq!(frame_tab("", true, true).matches("u").count(), 1);
}

/// Each control of the tab is a click range: zoom on its first two
/// cells, float on the middle three, close on the last two.
#[test]
fn the_control_tabs_cells_are_its_controls() {
    use grofe::server::status::RangeKind;
    let tab = frame_tab("▐▌", false, false);
    let edge = frame_title((0, 30), 80, "t", Some(&tab), Style::default(), '─');
    let (tx, _) = edge.tab.as_ref().expect("a tab");
    assert_eq!(*tx, 30 - 1 - 7);
    let got: Vec<Option<RangeKind>> = (0..7)
        .map(|i| edge.range_at(tx + i).map(|r| r.kind.clone()))
        .collect();
    let c = |n| Some(RangeKind::Control(n));
    assert_eq!(got, vec![c(8), c(8), c(7), c(7), c(7), c(9), c(9)]);
}

#[test]
fn pane_frame_title_off_draws_plain_top_edges() {
    let screen = compose(&three(Some(1), PaneLines::Rounded, false));
    let shown = text(&screen);
    let rows: Vec<&str> = shown.lines().collect();
    assert_eq!(
        rows[1], " ╭─────────────────╮ ╭────────────────╮",
        "{shown}"
    );
    assert!(!shown.contains("1: one"), "{shown}");
}

#[test]
fn a_title_too_long_is_cut_and_never_overwrites_the_corners() {
    let mut inputs = three(Some(1), PaneLines::Rounded, true);
    for l in inputs.look.panes.values_mut() {
        l.title = "a title far too long for any of these panes".to_string();
    }
    let screen = compose(&inputs);
    let rows: Vec<String> = text(&screen).lines().map(str::to_string).collect();
    assert_eq!(rows[1], " ╭─ a title far to─╮ ╭─ a title far t─╮");
    // A pane too narrow for a title keeps a plain edge.
    let mut inputs = three(Some(1), PaneLines::Rounded, true);
    let narrow = Rect {
        x: 5,
        y: 5,
        cols: 6,
        rows: 3,
    };
    inputs.look.frames = vec![(PaneId(1), narrow)];
    let screen = compose(&inputs);
    let row: String = (5..11).map(|x| at(&screen, x, 5).c).collect();
    assert_eq!(row, "╭────╮");
}

#[test]
fn a_wide_character_title_is_never_split_and_never_overruns() {
    let mut inputs = three(Some(1), PaneLines::Rounded, true);
    for l in inputs.look.panes.values_mut() {
        // 2 cells each: pane 1's title room (15 cells, a space first)
        // fits seven, pane 2's (14) would cut the seventh in two.
        l.title = "漢字漢字漢字漢字漢字".to_string();
    }
    let screen = compose(&inputs);
    let rows: Vec<String> = text(&screen).lines().map(str::to_string).collect();
    assert_eq!(rows[1], " ╭─ 漢字漢字漢字漢─╮ ╭─ 漢字漢字漢字──╮");
    // The corners and the edge cell before each stay the frame's.
    for (x, ch) in [
        (1, '╭'),
        (2, '─'),
        (18, '─'),
        (19, '╮'),
        (21, '╭'),
        (36, '─'),
        (37, '─'),
        (38, '╮'),
    ] {
        assert_eq!(at(&screen, x, 1).c, ch, "{x}: {}", rows[1]);
    }
}

#[test]
fn explicit_lines_win_and_the_marked_frame_is_reversed() {
    let mut inputs = three(Some(1), PaneLines::Heavy, true);
    inputs.marked = Some(PaneId(3));
    let screen = compose(&inputs);
    let shown = text(&screen);
    assert!(shown.contains("┏━ 1: one ━━━━━━━━┓"), "{shown}");
    let r = inputs.look.frames[2].1;
    assert!(at(&screen, r.x, r.y).flags.contains(CellFlags::INVERSE));
    assert!(
        !at(&screen, r.x + 1, r.y + 1)
            .flags
            .contains(CellFlags::INVERSE)
    );
}

#[test]
fn the_drop_preview_is_drawn_above_the_tiles_and_below_floating_panes() {
    let mut inputs = three(Some(1), PaneLines::Rounded, true);
    // Inside pane 1's content (x 2..19, y 2..10): its frame covers the
    // content cells it lands on.
    let preview = Rect {
        x: 4,
        y: 3,
        cols: 10,
        rows: 2,
    };
    inputs.look.drop_preview = Some(preview);
    inputs.active = PaneId(2);
    let mut screen = compose(&inputs);
    assert_eq!(at(&screen, 3, 2).c, '╭');
    assert!(is_active(at(&screen, 3, 2)));
    assert_eq!(at(&screen, 14, 5).c, '╯');
    assert_eq!(at(&screen, 3, 4).c, '│');
    // A floating pane over the preview's right half covers it.
    let float = FloatingPane {
        id: PaneId(4),
        at: (10, 4),
        size: (8, 2),
        snap: pane(8, 2, "float"),
        active: false,
    };
    let mut looks = inputs.look.clone();
    looks.panes.insert(PaneId(4), look(3, "3: float"));
    draw_floating(
        &mut screen,
        &[float],
        (0, 0),
        &Style::default(),
        &parse_style(ACTIVE),
        &looks,
    );
    assert_eq!(at(&screen, 9, 3).c, '╭');
    assert!(!is_active(at(&screen, 9, 3)));
    assert_eq!(at(&screen, 10, 4).c, 'f');
    // The preview's corner at (14, 5) is under the float's content.
    assert_eq!(at(&screen, 14, 5).c, ' ');
    assert!(!is_active(at(&screen, 14, 5)));
    // Its left part still shows.
    assert_eq!(at(&screen, 3, 2).c, '╭');
    insta::assert_snapshot!("floating_over_framed_tiles", text(&screen));
}

#[test]
fn a_floating_pane_takes_the_tiles_frame_style() {
    let inputs = three(Some(1), PaneLines::Rounded, true);
    let mut screen = compose(&inputs);
    let float = FloatingPane {
        id: PaneId(4),
        at: (12, 4),
        size: (14, 3),
        snap: pane(14, 3, "floating"),
        active: true,
    };
    let mut looks = inputs.look.clone();
    looks.panes.insert(PaneId(4), look(3, "3: float"));
    draw_floating(
        &mut screen,
        &[float],
        (0, 0),
        &Style::default(),
        &parse_style(ACTIVE),
        &looks,
    );
    let row: String = (11..27).map(|x| at(&screen, x, 3).c).collect();
    assert_eq!(row, "╭─ 3: float ───╮");
    let row: String = (11..27).map(|x| at(&screen, x, 7).c).collect();
    assert_eq!(row, "╰──────────────╯");
    assert!(is_active(at(&screen, 11, 3)));
}

#[test]
fn frames_off_draws_rounded_as_single() {
    let single = compose(&three(None, PaneLines::Single, true));
    let rounded = compose(&three(None, PaneLines::Rounded, true));
    assert_eq!(single, rounded);
    let shown = text(&single);
    assert!(shown.contains('┬') || shown.contains('├'), "{shown}");
    assert!(!shown.contains('╭'), "{shown}");
}

#[test]
fn frames_off_ignores_the_frame_title() {
    // The default look (what every caller passed before FR.2) draws no
    // frame: the shared borders are unchanged.
    let mut plain = three(None, PaneLines::Single, false);
    plain.look.frame_title = false;
    let with_title = three(None, PaneLines::Single, true);
    assert_eq!(compose(&plain), compose(&with_title));
}

// ---- attached client -------------------------------------------------------

/// A server whose config sets `config`, a 40x12 session `f` running
/// `cat` split into 1 | (2 / 3), and a client attached at 40x13.
fn attached(config: &str) -> (TestServer, Harness) {
    let srv = TestServer::builder().frames(true).unstarted();
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).expect("config dir");
    fs::write(dir.join("config.toml"), config).expect("write config");
    srv.cmd(&["start-server"]).expect("start-server");
    // `exec`, so the pane runs cat whatever `sh` is: dash keeps itself as
    // the parent of a `-c` command on a terminal, and titles name `sh`.
    let cmds: [&[&str]; 5] = [
        &[
            "new-session",
            "-d",
            "-s",
            "f",
            "-x",
            "40",
            "-y",
            "12",
            "exec cat",
        ],
        &[
            "set",
            "-g",
            "pane-border-format",
            "#{pane_index}: #{pane_current_command}",
        ],
        &["set", "-g", "status", "off"],
        &["split-window", "-h", "-t", "f", "exec cat"],
        &["split-window", "-v", "-t", "f:.1", "exec cat"],
    ];
    for args in cmds {
        srv.cmd(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
    }
    let client = Harness::spawn_attach_with_env(srv.command(), "f", &[], 40, 12);
    (srv, client)
}

#[test]
fn an_attached_client_sees_rounded_frames_with_titles() {
    let (_srv, mut client) = attached("pane-frames = true\n");
    client.wait_until("three titled frames", harness::DEFAULT_TIMEOUT, |t| {
        t.matches(": ca").count() == 3 && t.matches("▐z f x▌").count() == 3
    });
    let shown = client.text();
    assert_eq!(shown.matches("╭─ ").count(), 3, "{shown}");
    assert!(shown.contains('╯'), "{shown}");
    assert!(!shown.contains('┬'), "{shown}");
}

#[test]
fn tmux_identity_with_frames_on_draws_rounded_unless_the_lines_are_set() {
    let (srv, mut client) =
        attached("compat-identity = \"tmux\"\npane-frames = true\npane-frame-title = false\n");
    client.wait_for("╭──");
    assert!(!client.text().contains(": cat"), "{}", client.text());
    srv.cmd(&["set", "-w", "-t", "f", "pane-border-lines", "double"])
        .expect("set lines");
    client.wait_for("╔══");
}

#[test]
fn tmux_identity_with_frames_on_keeps_a_global_single_set_at_runtime() {
    let (srv, mut client) =
        attached("compat-identity = \"tmux\"\npane-frames = true\npane-frame-title = false\n");
    client.wait_for("╭──");
    srv.cmd(&["set", "-g", "pane-border-lines", "single"])
        .expect("set lines");
    client.wait_for("┌──");
    let shown = client.text();
    assert!(!shown.contains('╭'), "{shown}");
    assert!(!shown.contains('┬'), "{shown}");
}

#[test]
fn tmux_identity_with_frames_on_keeps_a_single_the_config_sets() {
    let (_srv, mut client) = attached(
        "compat-identity = \"tmux\"\npane-frames = true\npane-frame-title = false\n\
         [pane-border]\nlines = \"single\"\n",
    );
    client.wait_for("┌──");
    let shown = client.text();
    assert_eq!(shown.matches('┌').count(), 3, "{shown}");
    assert!(!shown.contains('╭'), "{shown}");
}

/// Under tmux identity a frame's title is tmux's stock
/// `pane-border-format`, its `[z][x]` included, and grofe's control tab is
/// not drawn.
#[test]
fn tmux_identity_with_frames_on_keeps_the_stock_title_buttons() {
    let (srv, mut client) = attached("compat-identity = \"tmux\"\npane-frames = true\n");
    srv.cmd(&["set", "-gu", "pane-border-format"])
        .expect("default format");
    srv.cmd(&["set", "-g", "mouse", "on"]).expect("mouse");
    // Zoomed, so the one frame is wide enough for the whole title.
    srv.cmd(&["resize-pane", "-Z", "-t", "f:.1"]).expect("zoom");
    client.wait_for("[u][x]");
    assert!(!client.text().contains("z f x"), "{}", client.text());
    assert!(!client.text().contains('▐'), "{}", client.text());
}

#[test]
fn grofe_identity_with_an_explicit_single_draws_single_frames() {
    let (srv, mut client) = attached("pane-frames = true\npane-frame-title = false\n");
    client.wait_for("╭──");
    srv.cmd(&["set", "-g", "pane-border-lines", "single"])
        .expect("set lines");
    client.wait_for("┌──");
    assert!(!client.text().contains('╭'), "{}", client.text());
}

#[test]
fn a_zoomed_framed_window_draws_one_framed_tile() {
    let (srv, mut client) = attached("pane-frames = true\n");
    client.wait_until("three titled frames", harness::DEFAULT_TIMEOUT, |t| {
        t.matches(": ca").count() == 3 && t.matches("▐z f x▌").count() == 3
    });
    srv.cmd(&["resize-pane", "-Z", "-t", "f:.1"]).expect("zoom");
    client.wait_until("one framed tile", harness::DEFAULT_TIMEOUT, |t| {
        t.matches('╭').count() == 1
    });
    let shown = client.text();
    let rows: Vec<&str> = shown.lines().collect();
    assert!(rows[1].starts_with(" ╭─ 1: cat ─"), "{shown}");
    assert!(rows[1].trim_end().ends_with('╮'), "{shown}");
    assert_eq!(shown.matches('╯').count(), 1, "{shown}");
}

#[test]
fn frames_off_draws_shared_borders_on_an_attached_client() {
    let (_srv, mut client) = attached("pane-frames = false\n");
    client.wait_for("┤");
    assert!(!client.text().contains('╭'), "{}", client.text());
}
