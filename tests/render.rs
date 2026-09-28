//! Round-trip tests for the renderer (spec §4.4): pane content is fed through
//! an inner `alacritty_terminal` to get real `SnapCell`s, composed into a
//! client `Screen`, diffed to an escape stream, and that stream is fed into a
//! fresh outer `Term` whose screen is snapshot-tested.

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape as VteShape, NamedColor, Processor, Rgb};
use grofe::protocol::Caps;
use grofe::server::layout::{BorderSegment, LayoutTree};
use grofe::server::pane::{CellFlags, CursorShape, CursorState, RowsSnapshot, SnapCell};
use grofe::server::render::{FrameInputs, Overlay, Screen, compose, diff, write_sgr};
use grofe::server::status::{Span, Style, StyledLine};
use grofe::types::{Dir, PaneId, Rect, SplitSize};

const RICH: Caps = Caps {
    truecolor: true,
    styled_underline: true,
    sync: true,
    kitty_keys: false,
    kitty_graphics: false,
};
const PLAIN: Caps = Caps {
    truecolor: false,
    styled_underline: false,
    sync: false,
    kitty_keys: false,
    kitty_graphics: false,
};

// ---- helpers ---------------------------------------------------------------

/// Run `bytes` through a pane-sized emulator and snapshot it as the pane
/// would (`PaneHandle::snapshot_rows`).
fn pane(cols: u16, rows: u16, bytes: &str) -> RowsSnapshot {
    let mut term = Term::new(
        Config::default(),
        &TermSize::new(cols as usize, rows as usize),
        VoidListener,
    );
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, bytes.replace('\n', "\r\n").as_bytes());
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
                        zerowidth: cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default(),
                        underline_color: cell.underline_color(),
                    }
                })
                .collect()
        })
        .collect();
    let point = grid.cursor.point;
    let style = term.cursor_style();
    RowsSnapshot {
        rows: rows_out,
        cursor: CursorState {
            row: point.line.0 as u16,
            col: point.column.0 as u16,
            visible: term.mode().contains(TermMode::SHOW_CURSOR),
            shape: match style.shape {
                VteShape::Beam => CursorShape::Bar,
                VteShape::Underline => CursorShape::Underline,
                _ => CursorShape::Block,
            },
            blink: style.blinking,
        },
    }
}

/// A window of `panes` laid out by `tree` over `area`, with the active
/// pane's cursor translated into window coordinates.
fn inputs<'a>(
    client: (u16, u16),
    area: Rect,
    tree: &LayoutTree,
    contents: &[(PaneId, &str)],
    active: PaneId,
) -> FrameInputs<'a> {
    let panes: Vec<(PaneId, Rect, RowsSnapshot)> = tree
        .compute(area)
        .into_iter()
        .map(|(id, rect)| {
            let text = contents
                .iter()
                .find(|(p, _)| *p == id)
                .map_or("", |(_, t)| t);
            (id, rect, pane(rect.cols, rect.rows, text))
        })
        .collect();
    let cursor = panes
        .iter()
        .find(|(id, _, _)| *id == active)
        .map(|(_, rect, snap)| CursorState {
            row: snap.cursor.row + rect.y,
            col: snap.cursor.col + rect.x,
            ..snap.cursor
        });
    FrameInputs {
        client,
        viewport: (0, 0),
        area,
        panes,
        borders: tree.borders(area),
        active,
        status: None,
        overlays: &[],
        cursor,
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

fn single(client: (u16, u16), text: &str) -> FrameInputs<'static> {
    let a = area(client.0, client.1);
    inputs(
        client,
        a,
        &LayoutTree::new(PaneId(1)),
        &[(PaneId(1), text)],
        PaneId(1),
    )
}

fn outer(cols: u16, rows: u16) -> (Term<VoidListener>, Processor) {
    (
        Term::new(
            Config::default(),
            &TermSize::new(cols as usize, rows as usize),
            VoidListener,
        ),
        Processor::new(),
    )
}

/// Feed `bytes` into a fresh outer terminal the size of `screen`.
fn reconstruct(screen: &Screen, bytes: &[u8]) -> Term<VoidListener> {
    let (mut term, mut parser) = outer(screen.cols, screen.rows);
    parser.advance(&mut term, bytes);
    term
}

fn color(c: Color) -> String {
    match c {
        Color::Named(n) => format!("{n:?}"),
        Color::Indexed(i) => format!("idx{i}"),
        Color::Spec(Rgb { r, g, b }) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

const STYLE_FLAGS: CellFlags = CellFlags::BOLD
    .union(CellFlags::DIM)
    .union(CellFlags::ITALIC)
    .union(CellFlags::ALL_UNDERLINES)
    .union(CellFlags::INVERSE)
    .union(CellFlags::HIDDEN)
    .union(CellFlags::STRIKEOUT);

/// The outer terminal's screen as text, then its cursor, then every styled
/// run (cells whose colours or attributes are not the defaults).
fn dump(term: &Term<VoidListener>) -> String {
    let grid = term.grid();
    let mut out = String::new();
    for r in 0..grid.screen_lines() {
        let mut line = String::new();
        for c in 0..grid.columns() {
            let cell = &grid[Line(r as i32)][Column(c)];
            if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                continue;
            }
            line.push(cell.c);
            if let Some(zw) = cell.zerowidth() {
                line.extend(zw);
            }
        }
        out.push_str(&format!("|{}\n", line.trim_end()));
    }
    let point = grid.cursor.point;
    let style = term.cursor_style();
    // Where a hidden cursor sits is not observable, so it is not dumped.
    if term.mode().contains(TermMode::SHOW_CURSOR) {
        out.push_str(&format!(
            "cursor: row {} col {} shape {:?} blink {}\n",
            point.line.0, point.column.0, style.shape, style.blinking,
        ));
    } else {
        out.push_str("cursor: hidden\n");
    }
    for r in 0..grid.screen_lines() {
        let mut runs: Vec<(usize, usize, String)> = Vec::new();
        for c in 0..grid.columns() {
            let cell = &grid[Line(r as i32)][Column(c)];
            let flags = cell.flags & STYLE_FLAGS;
            let default = cell.fg == Color::Named(NamedColor::Foreground)
                && cell.bg == Color::Named(NamedColor::Background)
                && flags.is_empty()
                && cell.underline_color().is_none();
            if default {
                continue;
            }
            let mut desc = format!("fg={} bg={}", color(cell.fg), color(cell.bg));
            if !flags.is_empty() {
                desc.push_str(&format!(" {flags:?}"));
            }
            if let Some(u) = cell.underline_color() {
                desc.push_str(&format!(" ul={}", color(u)));
            }
            match runs.last_mut() {
                Some((_, end, d)) if *end + 1 == c && *d == desc => *end = c,
                _ => runs.push((c, c, desc)),
            }
        }
        for (start, end, desc) in runs {
            out.push_str(&format!("r{r} c{start}-{end}: {desc}\n"));
        }
    }
    out
}

/// Assert the outer terminal shows exactly `screen`: every cell's glyph,
/// colours, style flags, underline colour, and the cursor.
fn assert_shows(term: &Term<VoidListener>, screen: &Screen) {
    let grid = term.grid();
    for r in 0..screen.rows as usize {
        for c in 0..screen.cols as usize {
            let got = &grid[Line(r as i32)][Column(c)];
            let want = &screen.cells[r * screen.cols as usize + c];
            let keep = STYLE_FLAGS | CellFlags::WIDE_CHAR | CellFlags::WIDE_CHAR_SPACER;
            assert_eq!(
                (
                    got.c,
                    got.fg,
                    got.bg,
                    got.flags & keep,
                    got.zerowidth().map(<[char]>::to_vec).unwrap_or_default(),
                    got.underline_color(),
                ),
                (
                    want.c,
                    want.fg,
                    want.bg,
                    want.flags & keep,
                    want.zerowidth.clone(),
                    want.underline_color,
                ),
                "cell r{r} c{c}\n{}",
                dump(term)
            );
        }
    }
    let visible = term.mode().contains(TermMode::SHOW_CURSOR);
    match screen.cursor {
        Some(cur) => {
            assert!(visible, "cursor should be visible");
            let point = grid.cursor.point;
            assert_eq!(
                (point.line.0 as u16, point.column.0 as u16),
                (cur.row, cur.col)
            );
        }
        None => assert!(!visible, "cursor should be hidden"),
    }
}

fn round_trip(inputs: &FrameInputs, caps: &Caps) -> String {
    let screen = compose(inputs);
    let bytes = diff(None, &screen, caps);
    dump(&reconstruct(&screen, &bytes))
}

/// Byte offsets right after each `needle` in `bytes`.
fn ends_of(bytes: &[u8], needle: &[u8]) -> Vec<usize> {
    bytes
        .windows(needle.len())
        .enumerate()
        .filter(|(_, w)| *w == needle)
        .map(|(i, _)| i + needle.len())
        .collect()
}

/// Does `bytes` start with an absolute cursor move, `ESC [ row ; col H`?
fn starts_with_cup(bytes: &[u8]) -> bool {
    let Some(rest) = bytes.strip_prefix(b"\x1b[") else {
        return false;
    };
    let end = rest
        .iter()
        .position(|b| !(b.is_ascii_digit() || *b == b';'));
    matches!(end, Some(n) if n > 0 && rest[n] == b'H' && rest[..n].contains(&b';'))
}

// ---- compose + round trip --------------------------------------------------

#[test]
fn single_pane() {
    let inputs = single((20, 5), "$ echo hi\nhi\n$ ");
    let screen = compose(&inputs);
    let bytes = diff(None, &screen, &RICH);
    let term = reconstruct(&screen, &bytes);
    assert_shows(&term, &screen);
    insta::assert_snapshot!(dump(&term));
}

fn three_panes() -> LayoutTree {
    let a = area(30, 10);
    let mut tree = LayoutTree::new(PaneId(1));
    tree.split(a, PaneId(1), Dir::Horizontal, PaneId(2), SplitSize::Half)
        .expect("split h");
    tree.split(a, PaneId(2), Dir::Vertical, PaneId(3), SplitSize::Half)
        .expect("split v");
    tree
}

#[test]
fn splits_with_borders() {
    let tree = three_panes();
    let inputs = inputs(
        (30, 10),
        area(30, 10),
        &tree,
        &[
            (PaneId(1), "left\n$ "),
            (PaneId(2), "top right"),
            (PaneId(3), "bottom"),
        ],
        PaneId(1),
    );
    let screen = compose(&inputs);
    let term = reconstruct(&screen, &diff(None, &screen, &RICH));
    assert_shows(&term, &screen);
    insta::assert_snapshot!(dump(&term));
}

#[test]
fn active_border_follows_the_active_pane() {
    let tree = three_panes();
    let contents = [(PaneId(1), "one"), (PaneId(2), "two"), (PaneId(3), "three")];
    let bottom = inputs((30, 10), area(30, 10), &tree, &contents, PaneId(3));
    let top = inputs((30, 10), area(30, 10), &tree, &contents, PaneId(2));
    insta::assert_snapshot!("active_bottom_right", round_trip(&bottom, &RICH));
    insta::assert_snapshot!("active_top_right", round_trip(&top, &RICH));
}

const COLOURS: &str = "\x1b[31mred\x1b[0m \x1b[38;5;208midx\x1b[0m \
                       \x1b[38;2;10;200;30mrgb\x1b[0m \x1b[48;2;128;128;128mgry\x1b[0m\n\
                       \x1b[1;3;7mbiv\x1b[0m \x1b[2;9mds\x1b[0m \x1b[94;101mbright\x1b[0m";

#[test]
fn colours_truecolor() {
    insta::assert_snapshot!(round_trip(&single((30, 3), COLOURS), &RICH));
}

#[test]
fn colours_256() {
    let caps = Caps {
        truecolor: false,
        ..RICH
    };
    insta::assert_snapshot!(round_trip(&single((30, 3), COLOURS), &caps));
}

#[test]
fn undercurl_and_underline_colour_survive_with_styled_underline() {
    let text = "\x1b[4:3;58:2::255:0:0mcurl\x1b[0m \x1b[4:2mdbl\x1b[0m \x1b[4:4mdot\x1b[0m \x1b[4mplain\x1b[0m";
    let inputs = single((30, 2), text);
    let styled = round_trip(&inputs, &RICH);
    insta::assert_snapshot!("underline_styled", styled);
    // Without the cap every style degrades to a plain underline and the
    // colour is dropped.
    insta::assert_snapshot!("underline_plain", round_trip(&inputs, &PLAIN));
}

const WIDE: &str = "a\u{1F44D}b \u{2764}\u{FE0F}c e\u{301}f \u{6F22}\u{5B57}g";

#[test]
fn emoji_vs16_and_combining_marks() {
    let inputs = single((24, 2), WIDE);
    let screen = compose(&inputs);
    let bytes = diff(None, &screen, &RICH);
    let term = reconstruct(&screen, &bytes);
    assert_shows(&term, &screen);
    insta::assert_snapshot!(dump(&term));
}

#[test]
fn absolute_cursor_move_follows_every_wide_or_zero_width_cell() {
    let inputs = single((24, 2), WIDE);
    let screen = compose(&inputs);
    let bytes = diff(None, &screen, &RICH);
    for glyph in [
        "\u{1F44D}",
        "\u{2764}\u{FE0F}",
        "e\u{301}",
        "\u{6F22}",
        "\u{5B57}",
    ] {
        let ends = ends_of(&bytes, glyph.as_bytes());
        assert_eq!(ends.len(), 1, "{glyph:?} drawn once");
        assert!(
            starts_with_cup(&bytes[ends[0]..]),
            "no absolute move after {glyph:?}: {:?}",
            String::from_utf8_lossy(&bytes[ends[0]..(ends[0] + 12).min(bytes.len())])
        );
    }
}

#[test]
fn cursor_shape_position_and_visibility() {
    let bar = single((20, 3), "ab\x1b[5 q");
    insta::assert_snapshot!("cursor_bar_blink", round_trip(&bar, &RICH));
    let hidden = single((20, 3), "ab\x1b[?25l");
    insta::assert_snapshot!("cursor_hidden", round_trip(&hidden, &RICH));
}

#[test]
fn status_line_on_the_last_row() {
    let mut inputs = single((20, 4), "$ ");
    inputs.area = area(20, 3);
    inputs.panes[0].1 = area(20, 3);
    let green = Style {
        bg: Color::Named(NamedColor::Green),
        fg: Color::Named(NamedColor::Black),
        ..Style::default()
    };
    inputs.status = Some(StyledLine {
        spans: vec![
            Span {
                text: "[t] 1:sh*".into(),
                style: green,
            },
            Span {
                text: " \u{1F600}".into(),
                style: Style::default(),
            },
        ],
    });
    let screen = compose(&inputs);
    let term = reconstruct(&screen, &diff(None, &screen, &RICH));
    assert_shows(&term, &screen);
    insta::assert_snapshot!(dump(&term));
}

#[test]
fn smaller_window_is_letterboxed() {
    let mut inputs = single((10, 3), "hi\n$ ");
    inputs.client = (14, 5);
    insta::assert_snapshot!(round_trip(&inputs, &RICH));
}

#[test]
fn larger_window_is_cropped_at_the_viewport() {
    // Row 1 has wide chars straddling both crop edges (cols 4-5 and 13-14,
    // for a viewport from col 5, 9 columns wide): each cut half is a space.
    // The cursor ends at window (8, 3), client (3, 2).
    let text = "row zero\n0123\u{6F22}6789abc\u{5B57}ef\nline two\n      xy";
    let mut inputs = single((20, 5), text);
    inputs.client = (9, 3);
    inputs.viewport = (5, 1);
    let screen = compose(&inputs);
    let term = reconstruct(&screen, &diff(None, &screen, &RICH));
    assert_shows(&term, &screen);
    insta::assert_snapshot!(dump(&term));
}

struct Box1;
impl Overlay for Box1 {
    fn draw(&self, screen: &mut Screen) {
        let cell = &mut screen.cells[1];
        cell.c = '#';
        cell.bg = Color::Named(NamedColor::Blue);
    }
}

#[test]
fn overlays_draw_last() {
    let overlays: [&dyn Overlay; 1] = [&Box1];
    let mut inputs = single((6, 2), "abc");
    inputs.overlays = &overlays;
    insta::assert_snapshot!(round_trip(&inputs, &RICH));
}

// ---- diff ------------------------------------------------------------------

#[test]
fn unchanged_frame_diffs_to_nothing() {
    let tree = three_panes();
    let contents = [(PaneId(1), "one"), (PaneId(2), WIDE), (PaneId(3), COLOURS)];
    let screen = compose(&inputs((30, 10), area(30, 10), &tree, &contents, PaneId(1)));
    for caps in [RICH, PLAIN] {
        assert!(diff(Some(&screen), &screen, &caps).is_empty());
    }
}

#[test]
fn one_cell_change_is_a_short_sequence() {
    let before = compose(&single((80, 24), "$ ls\nfile\n$ "));
    let after = compose(&single((80, 24), "$ ls\nfile\n$ x"));
    let bytes = diff(Some(&before), &after, &RICH);
    let inner = bytes
        .strip_prefix(b"\x1b[?2026h".as_slice())
        .and_then(|b| b.strip_suffix(b"\x1b[?2026l".as_slice()))
        .expect("wrapped in synchronized output");
    assert!(
        inner.len() <= 32,
        "{} bytes: {:?}",
        inner.len(),
        String::from_utf8_lossy(inner)
    );
    let plain = diff(Some(&before), &after, &PLAIN);
    assert!(
        !plain.starts_with(b"\x1b[?2026h"),
        "no sync without the cap"
    );

    let (mut term, mut parser) = outer(80, 24);
    parser.advance(&mut term, &diff(None, &before, &RICH));
    parser.advance(&mut term, &bytes);
    assert_shows(&term, &after);
}

#[test]
fn a_sequence_of_diffs_tracks_every_frame() {
    let tree = three_panes();
    let frames = [
        vec![(PaneId(1), "one"), (PaneId(2), "two"), (PaneId(3), "three")],
        vec![(PaneId(1), WIDE), (PaneId(2), "two"), (PaneId(3), "three")],
        vec![
            (PaneId(1), "a\u{1F44D}"),
            (PaneId(2), COLOURS),
            (PaneId(3), "x"),
        ],
        vec![
            (PaneId(1), "ab c\x1b[?25l"),
            (PaneId(2), ""),
            (PaneId(3), WIDE),
        ],
        vec![
            (PaneId(1), "\x1b[3 q\u{6F22}"),
            (PaneId(2), "e\u{301}"),
            (PaneId(3), ""),
        ],
    ];
    for caps in [RICH, PLAIN] {
        let (mut term, mut parser) = outer(30, 10);
        let mut prev: Option<Screen> = None;
        for (i, contents) in frames.iter().enumerate() {
            let active = PaneId(1 + (i as u32 % 3));
            let next = compose(&inputs((30, 10), area(30, 10), &tree, contents, active));
            parser.advance(&mut term, &diff(prev.as_ref(), &next, &caps));
            if caps == RICH {
                assert_shows(&term, &next);
            } else {
                // Colours downgrade, so compare only the text.
                let want = reconstruct(&next, &diff(None, &next, &caps));
                assert_eq!(dump(&term), dump(&want), "frame {i}");
            }
            prev = Some(next);
        }
    }
}

#[test]
fn size_change_is_a_full_redraw() {
    let small = compose(&single((10, 3), "abc"));
    let big = compose(&single((12, 4), "abcd"));
    let (mut term, mut parser) = outer(12, 4);
    parser.advance(&mut term, b"garbage everywhere garbage");
    parser.advance(&mut term, &diff(Some(&small), &big, &RICH));
    assert_shows(&term, &big);
}

// ---- SGR writer ------------------------------------------------------------

#[test]
fn sgr_writer_downgrades_by_caps() {
    let style = Style {
        fg: Color::Spec(Rgb { r: 255, g: 0, b: 0 }),
        bg: Color::Indexed(17),
        attrs: CellFlags::BOLD | CellFlags::UNDERCURL,
        underline_color: Some(Color::Spec(Rgb { r: 0, g: 0, b: 255 })),
    };
    let mut rich = Vec::new();
    write_sgr(&mut rich, &style, &RICH);
    let mut plain = Vec::new();
    write_sgr(&mut plain, &style, &PLAIN);
    insta::assert_snapshot!(format!(
        "rich:  {}\nplain: {}",
        String::from_utf8_lossy(&rich).replace('\x1b', "ESC"),
        String::from_utf8_lossy(&plain).replace('\x1b', "ESC")
    ));
}

#[test]
fn border_segments_join_into_box_drawing() {
    // A hand-built T junction: the renderer picks glyphs from neighbours.
    let mut inputs = single((7, 3), "");
    inputs.panes.clear();
    inputs.borders = vec![
        BorderSegment::Vertical {
            col: 3,
            row: 0,
            len: 3,
        },
        BorderSegment::Horizontal {
            row: 1,
            col: 4,
            len: 3,
        },
    ];
    inputs.cursor = None;
    insta::assert_snapshot!(round_trip(&inputs, &RICH));
}
