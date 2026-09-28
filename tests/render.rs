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
use grofe::server::render::{FrameInputs, Screen, compose, diff, fix_wide_pairs, write_sgr};
use grofe::server::status::{Span, Style, StyledLine, parse_style};
use grofe::types::{Dir, PaneId, Rect, SplitSize};

const RICH: Caps = Caps {
    truecolor: true,
    styled_underline: true,
    sync: true,
    kitty_keys: false,
    kitty_graphics: false,
    cell_px: None,
};
const PLAIN: Caps = Caps {
    truecolor: false,
    styled_underline: false,
    sync: false,
    kitty_keys: false,
    kitty_graphics: false,
    cell_px: None,
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
                        hyperlink: cell.hyperlink(),
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
        border_style: Style::default(),
        active_border_style: parse_style("fg=green"),
        active,
        status: None,
        cursor,
        graphics: None,
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

/// Overlays draw over the composed screen (the server's clients code does
/// it, T3.0), then the wide-pair fix-up runs.
#[test]
fn overlays_draw_last() {
    let inputs = single((6, 2), "abc");
    let mut screen = compose(&inputs);
    let cell = &mut screen.cells[1];
    cell.c = '#';
    cell.bg = Color::Named(NamedColor::Blue);
    fix_wide_pairs(&mut screen);
    let term = reconstruct(&screen, &diff(None, &screen, &RICH));
    assert_shows(&term, &screen);
    insta::assert_snapshot!(dump(&term));
}

/// The border style options reach the border cells (T3.0): the defaults
/// draw as M2 did, other values recolour inactive and active borders.
#[test]
fn border_styles_colour_the_borders() {
    let tree = three_panes();
    let contents = [(PaneId(1), "one"), (PaneId(2), "two"), (PaneId(3), "three")];
    let mut inputs = inputs((30, 10), area(30, 10), &tree, &contents, PaneId(1));
    let m2 = compose(&inputs);
    let border = |s: &Screen| {
        s.cells
            .iter()
            .filter(|c| "│─┼├┤┬┴┌┐└┘".contains(c.c))
            .map(|c| c.fg)
            .collect::<Vec<_>>()
    };
    let green = Color::Named(NamedColor::Green);
    let plain = Color::Named(NamedColor::Foreground);
    assert!(border(&m2).contains(&green) && border(&m2).contains(&plain));
    inputs.border_style = parse_style("fg=red");
    inputs.active_border_style = parse_style("fg=blue");
    let styled = compose(&inputs);
    let red = Color::Named(NamedColor::Red);
    let blue = Color::Named(NamedColor::Blue);
    let recoloured: Vec<Color> = border(&m2)
        .into_iter()
        .map(|c| if c == green { blue } else { red })
        .collect();
    assert_eq!(border(&styled), recoloured);
    // `default` is the terminal's default style.
    assert_eq!(parse_style("default"), Style::default());
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

// ---- randomized round trips ----------------------------------------------

/// Property tests: random screens through sequences of diffs, and random
/// pane text through compose with crop, letterbox, splits and a status row.
/// Case count defaults to 64 (debug builds are slow); set PROPTEST_CASES.
mod randomized {
    use alacritty_terminal::Term;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::term::{Config, TermMode};
    use alacritty_terminal::vte::ansi::{
        Color, CursorShape as VteShape, NamedColor, Processor, Rgb,
    };
    use grofe::protocol::Caps;
    use grofe::server::pane::{CellFlags, CursorShape, CursorState, SnapCell};
    use grofe::server::render::{Screen, diff};
    use proptest::prelude::*;

    const RICH: Caps = Caps {
        truecolor: true,
        styled_underline: true,
        sync: true,
        kitty_keys: false,
        kitty_graphics: false,
        cell_px: None,
    };

    const KEEP: CellFlags = CellFlags::BOLD
        .union(CellFlags::DIM)
        .union(CellFlags::ITALIC)
        .union(CellFlags::ALL_UNDERLINES)
        .union(CellFlags::INVERSE)
        .union(CellFlags::HIDDEN)
        .union(CellFlags::STRIKEOUT)
        .union(CellFlags::WIDE_CHAR)
        .union(CellFlags::WIDE_CHAR_SPACER);

    fn color() -> impl Strategy<Value = Color> {
        prop_oneof![
            Just(Color::Named(NamedColor::Foreground)),
            Just(Color::Named(NamedColor::Background)),
            (0usize..16).prop_map(|i| Color::Named(match i {
                0 => NamedColor::Black,
                1 => NamedColor::Red,
                2 => NamedColor::Green,
                3 => NamedColor::Yellow,
                4 => NamedColor::Blue,
                5 => NamedColor::Magenta,
                6 => NamedColor::Cyan,
                7 => NamedColor::White,
                8 => NamedColor::BrightBlack,
                9 => NamedColor::BrightRed,
                10 => NamedColor::BrightGreen,
                11 => NamedColor::BrightYellow,
                12 => NamedColor::BrightBlue,
                13 => NamedColor::BrightMagenta,
                14 => NamedColor::BrightCyan,
                _ => NamedColor::BrightWhite,
            })),
            (16u8..=255).prop_map(Color::Indexed),
            any::<(u8, u8, u8)>().prop_map(|(r, g, b)| Color::Spec(Rgb { r, g, b })),
        ]
    }

    #[derive(Debug, Clone)]
    struct Proto {
        kind: u8,
        fg: Color,
        bg: Color,
        flags: u8,
        ul: Option<(u8, u8, u8)>,
    }

    fn proto() -> impl Strategy<Value = Proto> {
        (
            0u8..8,
            color(),
            color(),
            any::<u8>(),
            proptest::option::of(any::<(u8, u8, u8)>()),
        )
            .prop_map(|(kind, fg, bg, flags, ul)| Proto {
                kind,
                fg,
                bg,
                flags,
                ul,
            })
    }

    fn flags_of(bits: u8) -> CellFlags {
        let mut f = CellFlags::empty();
        let table = [
            CellFlags::BOLD,
            CellFlags::DIM,
            CellFlags::ITALIC,
            CellFlags::UNDERLINE,
            CellFlags::UNDERCURL,
            CellFlags::INVERSE,
            CellFlags::HIDDEN,
            CellFlags::STRIKEOUT,
        ];
        if bits & 0x80 == 0 {
            return f;
        }
        for (i, fl) in table.iter().enumerate().take(7) {
            if bits & (1 << i) != 0 {
                f |= *fl;
            }
        }
        if f.contains(CellFlags::UNDERLINE) && f.contains(CellFlags::UNDERCURL) {
            f.remove(CellFlags::UNDERLINE);
        }
        f
    }

    fn build(
        cols: u16,
        rows: u16,
        protos: &[Proto],
        cursor: Option<(u16, u16, u8, bool)>,
    ) -> Screen {
        let mut s = Screen::new(cols, rows);
        let mut i = 0;
        for y in 0..rows as usize {
            let mut x = 0usize;
            while x < cols as usize {
                let p = &protos[i % protos.len()];
                i += 1;
                let base = y * cols as usize;
                let flags = flags_of(p.flags);
                let ul =
                    p.ul.filter(|_| flags.intersects(CellFlags::ALL_UNDERLINES))
                        .map(|(r, g, b)| Color::Spec(Rgb { r, g, b }));
                let mut cell = SnapCell {
                    c: ' ',
                    fg: if p.fg == Color::Named(NamedColor::Background) {
                        Color::Named(NamedColor::Foreground)
                    } else {
                        p.fg
                    },
                    bg: if p.bg == Color::Named(NamedColor::Foreground) {
                        Color::Named(NamedColor::Background)
                    } else {
                        p.bg
                    },
                    flags,
                    zerowidth: Vec::new(),
                    underline_color: ul,
                    hyperlink: None,
                };
                match p.kind {
                    0 => {
                        x += 1;
                        continue;
                    }
                    1 | 2 => cell.c = (b'a' + (p.flags % 26)) as char,
                    3 => cell.c = ' ',
                    4 => {
                        cell.c = 'e';
                        cell.zerowidth = vec!['\u{301}'];
                    }
                    5 => {
                        cell.c = '\u{2764}';
                        cell.zerowidth = vec!['\u{FE0F}'];
                    }
                    _ => {
                        if x + 1 < cols as usize {
                            cell.c = if p.kind == 6 { '\u{6F22}' } else { '\u{1F44D}' };
                            cell.flags |= CellFlags::WIDE_CHAR;
                            let mut sp = cell.clone();
                            sp.c = ' ';
                            sp.flags =
                                (sp.flags - CellFlags::WIDE_CHAR) | CellFlags::WIDE_CHAR_SPACER;
                            s.cells[base + x] = cell;
                            s.cells[base + x + 1] = sp;
                            x += 2;
                            continue;
                        }
                        cell.c = 'w';
                    }
                }
                s.cells[base + x] = cell;
                x += 1;
            }
        }
        s.cursor = cursor.map(|(r, c, sh, blink)| CursorState {
            row: r % rows,
            col: c % cols,
            visible: true,
            shape: match sh % 3 {
                0 => CursorShape::Block,
                1 => CursorShape::Underline,
                _ => CursorShape::Bar,
            },
            blink,
        });
        s
    }

    fn check(term: &Term<VoidListener>, screen: &Screen) -> Result<(), String> {
        let grid = term.grid();
        for r in 0..screen.rows as usize {
            for c in 0..screen.cols as usize {
                let got = &grid[Line(r as i32)][Column(c)];
                let want = &screen.cells[r * screen.cols as usize + c];
                let g = (
                    got.c,
                    got.fg,
                    got.bg,
                    got.flags & KEEP,
                    got.zerowidth().map(<[char]>::to_vec).unwrap_or_default(),
                    got.underline_color(),
                );
                let w = (
                    want.c,
                    want.fg,
                    want.bg,
                    want.flags & KEEP,
                    want.zerowidth.clone(),
                    want.underline_color,
                );
                if g != w {
                    return Err(format!("cell r{r} c{c}: got {g:?} want {w:?}"));
                }
            }
        }
        let visible = term.mode().contains(TermMode::SHOW_CURSOR);
        match screen.cursor {
            Some(cur) => {
                if !visible {
                    return Err("cursor hidden".into());
                }
                let p = grid.cursor.point;
                if (p.line.0 as u16, p.column.0 as u16) != (cur.row, cur.col) {
                    return Err(format!("cursor at {:?} want {:?}", p, (cur.row, cur.col)));
                }
                let st = term.cursor_style();
                let shape = match st.shape {
                    VteShape::Beam => CursorShape::Bar,
                    VteShape::Underline => CursorShape::Underline,
                    _ => CursorShape::Block,
                };
                if (shape, st.blinking) != (cur.shape, cur.blink) {
                    return Err(format!(
                        "shape {:?} {} want {:?} {}",
                        shape, st.blinking, cur.shape, cur.blink
                    ));
                }
            }
            None => {
                if visible {
                    return Err("cursor visible".into());
                }
            }
        }
        Ok(())
    }

    /// Cursor as (row, col, shape, blink), reduced modulo the screen size.
    type CursorSeed = Option<(u16, u16, u8, bool)>;

    fn frame() -> impl Strategy<Value = (Vec<Proto>, CursorSeed)> {
        (
            proptest::collection::vec(proto(), 1..40),
            proptest::option::of((any::<u16>(), any::<u16>(), any::<u8>(), any::<bool>())),
        )
    }

    proptest! {
        #![proptest_config(config())]
        #[test]
        fn diff_sequence_reconstructs(
            cols in 1u16..14, rows in 1u16..6,
            frames in proptest::collection::vec(frame(), 1..6),
            sync in any::<bool>(),
        ) {
            let caps = Caps { sync, ..RICH };
            let mut term = Term::new(Config::default(), &TermSize::new(cols as usize, rows as usize), VoidListener);
            let mut parser: Processor = Processor::new();
            let mut prev: Option<Screen> = None;
            for (i, (protos, cur)) in frames.iter().enumerate() {
                let next = build(cols, rows, protos, *cur);
                let bytes = diff(prev.as_ref(), &next, &caps);
                if prev.as_ref() == Some(&next) {
                    prop_assert!(bytes.is_empty());
                }
                parser.advance(&mut term, &bytes);
                if let Err(e) = check(&term, &next) {
                    return Err(TestCaseError::fail(format!("frame {i}: {e}\nbytes {:?}", String::from_utf8_lossy(&bytes))));
                }
                prop_assert!(diff(Some(&next), &next, &caps).is_empty());
                prev = Some(next);
            }
        }
    }

    mod compose_prop {
        use super::*;
        use grofe::server::layout::LayoutTree;
        use grofe::server::pane::RowsSnapshot;
        use grofe::server::render::{FrameInputs, compose, fix_wide_pairs};
        use grofe::server::status::{Span, Style, StyledLine, parse_style};
        use grofe::types::{Dir, PaneId, Rect, SplitSize};

        fn pane(cols: u16, rows: u16, bytes: &str) -> RowsSnapshot {
            let mut term = Term::new(
                Config::default(),
                &TermSize::new(cols as usize, rows as usize),
                VoidListener,
            );
            let mut parser: Processor = Processor::new();
            parser.advance(&mut term, bytes.as_bytes());
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
                                zerowidth: cell
                                    .zerowidth()
                                    .map(<[char]>::to_vec)
                                    .unwrap_or_default(),
                                underline_color: cell.underline_color(),
                                hyperlink: cell.hyperlink(),
                            }
                        })
                        .collect()
                })
                .collect();
            let p = grid.cursor.point;
            RowsSnapshot {
                rows: rows_out,
                cursor: CursorState {
                    row: p.line.0 as u16,
                    col: p.column.0 as u16,
                    visible: term.mode().contains(TermMode::SHOW_CURSOR),
                    shape: CursorShape::Block,
                    blink: false,
                },
            }
        }

        fn text() -> impl Strategy<Value = String> {
            proptest::collection::vec(
                prop_oneof![
                    Just("a".to_string()),
                    Just("\u{6F22}".to_string()),
                    Just("\u{1F44D}".to_string()),
                    Just("e\u{301}".to_string()),
                    Just("\u{2764}\u{FE0F}".to_string()),
                    Just("\r\n".to_string()),
                    Just("\x1b[31m".to_string()),
                    Just("\x1b[0m".to_string()),
                    Just("\x1b[48;2;1;2;3m".to_string()),
                    Just(" ".to_string()),
                ],
                0..80,
            )
            .prop_map(|v| v.concat())
        }

        proptest! {
            #![proptest_config(config())]
            #[test]
            fn compose_then_diff(
                wc in 5u16..24, wr in 2u16..8, cc in 1u16..24, cr in 1u16..9,
                vx in 0u16..12, vy in 0u16..6, split in any::<bool>(), status in any::<bool>(),
                t1 in text(), t2 in text(), t3 in text(), st in text(),
            ) {
                let area = Rect { x: 0, y: 0, cols: wc, rows: wr };
                let mut tree = LayoutTree::new(PaneId(1));
                if split { let _ = tree.split(area, PaneId(1), Dir::Horizontal, PaneId(2), SplitSize::Half); }
                let texts = [t1.clone(), t2.clone()];
                let panes: Vec<_> = tree.compute(area).into_iter().enumerate()
                    .map(|(i, (id, r))| (id, r, pane(r.cols.max(2), r.rows.max(1), &texts[i % 2]))).collect();
                let cursor = panes.first().map(|(_, r, s)| CursorState { row: s.cursor.row + r.y, col: s.cursor.col + r.x, ..s.cursor });
                let vx = if cc < wc { vx.min(wc - cc) } else { 0 };
                let vy = if cr < wr { vy.min(wr - cr) } else { 0 };
                let status_line = status.then(|| StyledLine { spans: vec![Span { text: st.replace("\r\n", "x").replace('\x1b', "E"), style: Style::default() }, Span { text: t3.replace("\r\n","y").replace('\x1b', "E"), style: Style { fg: Color::Named(NamedColor::Red), ..Style::default() } }] });
                let inputs = FrameInputs { client: (cc, cr), viewport: (vx, vy), area, borders: tree.borders(area),
                    border_style: Style::default(), active_border_style: parse_style("fg=green"),
                    panes, active: PaneId(1), status: status_line, cursor, graphics: None };
                let screen = compose(&inputs);
                // The wide-pair fix-up the server runs after overlays is a
                // no-op on what compose made (T3.0: no overlay, no change).
                let mut fixed = screen.clone();
                fix_wide_pairs(&mut fixed);
                prop_assert_eq!(&fixed, &screen);
                for y in 0..cr as usize { for x in 0..cc as usize {
                    let c = &screen.cells[y * cc as usize + x];
                    if c.flags.contains(CellFlags::WIDE_CHAR) {
                        prop_assert!(x + 1 < cc as usize, "wide at last col r{y} c{x}");
                        prop_assert!(screen.cells[y * cc as usize + x + 1].flags.contains(CellFlags::WIDE_CHAR_SPACER), "wide without spacer r{y} c{x}");
                    }
                    if c.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                        prop_assert!(x > 0 && screen.cells[y * cc as usize + x - 1].flags.contains(CellFlags::WIDE_CHAR), "orphan spacer r{y} c{x}");
                    }
                }}
                let mut term = Term::new(Config::default(), &TermSize::new(cc as usize, cr as usize), VoidListener);
                let mut parser: Processor = Processor::new();
                parser.advance(&mut term, &diff(None, &screen, &RICH));
                if let Err(e) = check(&term, &screen) { return Err(TestCaseError::fail(e)); }
            }
        }
    }

    fn config() -> ProptestConfig {
        let cases = std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(64);
        ProptestConfig::with_cases(cases)
    }
}
