//! Drawing a list box (agents spec §5.1).
//!
//! Pure drawing: the caller owns the rows, the filter result and the
//! [`ListState`]. The box has a rounded border with the title in the top
//! edge, an optional `> query` line, then one line per visible row. The
//! selected row is reversed and the hint is dim; no other colours.

use unicode_width::UnicodeWidthChar;

use crate::server::listview::ListState;
use crate::server::pane::{CellFlags, SnapCell};
use crate::server::render::Screen;
use crate::server::status::Style;
use crate::types::Rect;

/// The text of every row, split into columns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rows {
    pub cols: Vec<Vec<String>>,
}

/// A box `w_pct` by `h_pct` percent of the client, centred.
pub fn centred(client: (u16, u16), w_pct: u16, h_pct: u16) -> Rect {
    let scale = |n: u16, pct: u16| (u32::from(n) * u32::from(pct.min(100)) / 100) as u16;
    let (cols, rows) = (scale(client.0, w_pct), scale(client.1, h_pct));
    Rect {
        x: (client.0 - cols) / 2,
        y: (client.1 - rows) / 2,
        cols,
        rows,
    }
}

fn width(text: &str) -> usize {
    text.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

fn put_cell(screen: &mut Screen, x: usize, y: usize, c: char, style: &Style, flags: CellFlags) {
    if x >= usize::from(screen.cols) || y >= usize::from(screen.rows) {
        return;
    }
    let at = y * usize::from(screen.cols) + x;
    screen.cells[at] = SnapCell {
        c,
        fg: style.fg,
        bg: style.bg,
        flags: style.attrs | flags,
        zerowidth: Vec::new(),
        underline_color: style.underline_color,
        hyperlink: None,
    };
}

/// Draws `text` from `x` and never past column `end` (exclusive). A wide
/// character that does not fit whole is dropped, not split. Returns the
/// column after the last cell drawn.
fn put_text(
    screen: &mut Screen,
    mut x: usize,
    y: usize,
    end: usize,
    text: &str,
    style: &Style,
) -> usize {
    for c in text.chars() {
        let w = match UnicodeWidthChar::width(c) {
            Some(w) if w > 0 => w,
            _ => continue,
        };
        if x + w > end {
            break;
        }
        put_cell(screen, x, y, c, style, CellFlags::empty());
        if w == 2 {
            put_cell(screen, x + 1, y, ' ', style, CellFlags::WIDE_CHAR_SPACER);
            if let Some(cell) = cell_mut(screen, x, y) {
                cell.flags |= CellFlags::WIDE_CHAR;
            }
        }
        x += w;
    }
    x
}

fn cell_mut(screen: &mut Screen, x: usize, y: usize) -> Option<&mut SnapCell> {
    if x >= usize::from(screen.cols) || y >= usize::from(screen.rows) {
        return None;
    }
    let cols = usize::from(screen.cols);
    screen.cells.get_mut(y * cols + x)
}

fn fill(screen: &mut Screen, x0: usize, x1: usize, y: usize, style: &Style) {
    for x in x0..x1 {
        put_cell(screen, x, y, ' ', style, CellFlags::empty());
    }
}

/// Draws the list into `area`. `visible` indexes `rows` in display order;
/// `st.selected` is a position in `visible`, `st.top` the first position
/// shown (moved if needed to keep the selection on screen).
pub fn draw_list(
    screen: &mut Screen,
    area: Rect,
    title: &str,
    rows: &Rows,
    visible: &[usize],
    st: &ListState,
    show_query: bool,
) {
    let plain = Style::default();
    let (x0, y0) = (usize::from(area.x), usize::from(area.y));
    let (w, h) = (usize::from(area.cols), usize::from(area.rows));
    if w < 2 || h < 2 {
        return;
    }
    let (x1, y1) = (x0 + w - 1, y0 + h - 1);

    for y in y0..=y1 {
        fill(screen, x0, x1 + 1, y, &plain);
    }
    for x in x0 + 1..x1 {
        put_cell(screen, x, y0, '─', &plain, CellFlags::empty());
        put_cell(screen, x, y1, '─', &plain, CellFlags::empty());
    }
    for y in y0 + 1..y1 {
        put_cell(screen, x0, y, '│', &plain, CellFlags::empty());
        put_cell(screen, x1, y, '│', &plain, CellFlags::empty());
    }
    put_cell(screen, x0, y0, '╭', &plain, CellFlags::empty());
    put_cell(screen, x1, y0, '╮', &plain, CellFlags::empty());
    put_cell(screen, x0, y1, '╰', &plain, CellFlags::empty());
    put_cell(screen, x1, y1, '╯', &plain, CellFlags::empty());
    if !title.is_empty() && w > 4 {
        let x = put_text(screen, x0 + 2, y0, x1 - 1, " ", &plain);
        let x = put_text(screen, x, y0, x1 - 1, title, &plain);
        put_text(screen, x, y0, x1 - 1, " ", &plain);
    }

    let (ix0, ix1) = (x0 + 1, x1);
    let mut y = y0 + 1;
    if show_query && y < y1 {
        let x = put_text(screen, ix0, y, ix1, "> ", &plain);
        let x = put_text(screen, x, y, ix1, &st.query, &plain);
        if st.query.is_empty() {
            let mut dim = plain;
            dim.attrs |= CellFlags::DIM;
            put_text(screen, x, y, ix1, "type to filter", &dim);
        }
        y += 1;
    }

    let height = y1.saturating_sub(y);
    if height == 0 || visible.is_empty() {
        return;
    }
    let selected = st.selected.min(visible.len() - 1);
    let mut top = st.top.min(visible.len() - 1);
    if selected < top {
        top = selected;
    } else if selected >= top + height {
        top = selected + 1 - height;
    }
    let shown = &visible[top..visible.len().min(top + height)];

    let ncols = rows.cols.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..ncols)
        .map(|c| {
            visible
                .iter()
                .filter_map(|&i| rows.cols.get(i)?.get(c))
                .map(|t| width(t))
                .max()
                .unwrap_or(0)
        })
        .collect();

    for (n, &i) in shown.iter().enumerate() {
        let row_y = y + n;
        let mut style = plain;
        if top + n == selected {
            style.attrs |= CellFlags::INVERSE;
        }
        fill(screen, ix0, ix1, row_y, &style);
        let Some(cells) = rows.cols.get(i) else {
            continue;
        };
        let mut x = ix0;
        for (c, text) in cells.iter().enumerate() {
            if x >= ix1 {
                break;
            }
            put_text(screen, x, row_y, ix1, text, &style);
            x += widths[c] + 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(data: &[&[&str]]) -> Rows {
        Rows {
            cols: data
                .iter()
                .map(|r| r.iter().map(|s| s.to_string()).collect())
                .collect(),
        }
    }

    fn line(s: &Screen, y: u16) -> String {
        (0..s.cols)
            .map(|x| &s.cells[usize::from(y) * usize::from(s.cols) + usize::from(x)])
            .filter(|c| !c.flags.contains(CellFlags::WIDE_CHAR_SPACER))
            .map(|c| c.c)
            .collect()
    }

    fn area(cols: u16, rows: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            cols,
            rows,
        }
    }

    #[test]
    fn centred_example() {
        assert_eq!(
            centred((100, 50), 80, 70),
            Rect {
                x: 10,
                y: 7,
                cols: 80,
                rows: 35
            }
        );
    }

    #[test]
    fn wide_chars_cut_by_cell_width() {
        let r = rows(&[&["日本語日本語"]]);
        let mut s = Screen::new(10, 5);
        // inner width 6 holds three wide chars; with 5 the third would be
        // split, so it is dropped.
        draw_list(
            &mut s,
            area(8, 5),
            "t",
            &r,
            &[0],
            &ListState::default(),
            false,
        );
        assert_eq!(line(&s, 1).trim_end(), "│日本語│");
        let mut s = Screen::new(10, 5);
        draw_list(
            &mut s,
            area(7, 5),
            "t",
            &r,
            &[0],
            &ListState::default(),
            false,
        );
        assert_eq!(line(&s, 1).trim_end(), "│日本 │");
        let cell = &s.cells[10 + 1];
        assert!(cell.flags.contains(CellFlags::WIDE_CHAR));
        assert!(s.cells[10 + 2].flags.contains(CellFlags::WIDE_CHAR_SPACER));
    }

    #[test]
    fn columns_aligned() {
        let r = rows(&[&["a", "bb", "x"], &["ccc", "d", "y"], &["hidden", "h", "h"]]);
        let mut s = Screen::new(30, 6);
        draw_list(
            &mut s,
            area(30, 6),
            "t",
            &r,
            &[0, 1],
            &ListState::default(),
            false,
        );
        assert_eq!(line(&s, 1).trim_end(), "│a    bb  x                  │");
        assert_eq!(line(&s, 2).trim_end(), "│ccc  d   y                  │");
    }

    #[test]
    fn last_column_takes_the_rest() {
        let r = rows(&[&["a", "long text here"]]);
        let mut s = Screen::new(12, 4);
        draw_list(
            &mut s,
            area(12, 4),
            "t",
            &r,
            &[0],
            &ListState::default(),
            false,
        );
        assert_eq!(line(&s, 1).trim_end(), "│a  long te│");
    }

    #[test]
    fn selected_row_reversed() {
        let r = rows(&[&["one"], &["two"]]);
        let mut s = Screen::new(12, 6);
        let st = ListState {
            selected: 1,
            ..ListState::default()
        };
        draw_list(&mut s, area(12, 6), "t", &r, &[0, 1], &st, false);
        let at = |x: usize, y: usize| &s.cells[y * 12 + x];
        assert!(!at(2, 1).flags.contains(CellFlags::INVERSE));
        assert!(at(2, 2).flags.contains(CellFlags::INVERSE));
        assert!(at(10, 2).flags.contains(CellFlags::INVERSE));
        assert!(!at(0, 2).flags.contains(CellFlags::INVERSE));
    }

    #[test]
    fn query_line_and_title() {
        let r = rows(&[&["one"]]);
        let mut s = Screen::new(20, 6);
        let st = ListState {
            query: "o".into(),
            ..ListState::default()
        };
        draw_list(&mut s, area(20, 6), "Agents", &r, &[0], &st, true);
        assert!(line(&s, 0).starts_with("╭─ Agents "));
        assert_eq!(line(&s, 1), "│> o               │");
        assert_eq!(line(&s, 2).trim_end(), "│one               │".trim_end());
    }

    #[test]
    fn scrolls_to_keep_selection_visible() {
        let r = rows(&[&["a"], &["b"], &["c"], &["d"]]);
        let mut s = Screen::new(10, 4);
        let st = ListState {
            selected: 3,
            ..ListState::default()
        };
        draw_list(&mut s, area(10, 4), "", &r, &[0, 1, 2, 3], &st, false);
        assert!(line(&s, 1).starts_with("│c"));
        assert!(line(&s, 2).starts_with("│d"));
    }

    #[test]
    fn tiny_area_no_panic() {
        let r = rows(&[&["日本語"]]);
        let mut s = Screen::new(10, 5);
        draw_list(
            &mut s,
            area(3, 2),
            "title",
            &r,
            &[0],
            &ListState::default(),
            true,
        );
        draw_list(
            &mut s,
            area(1, 1),
            "title",
            &r,
            &[0],
            &ListState::default(),
            true,
        );
        draw_list(
            &mut s,
            area(0, 0),
            "title",
            &r,
            &[0],
            &ListState::default(),
            true,
        );
        // beyond the screen
        draw_list(
            &mut s,
            Rect {
                x: 8,
                y: 4,
                cols: 20,
                rows: 9,
            },
            "t",
            &r,
            &[0],
            &ListState::default(),
            true,
        );
    }
}
