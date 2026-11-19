//! Reflow of cold history rows.

use super::{CellFlags, SnapCell};

/// What a cold row remembers besides its cells.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColdMeta {
    pub wrapped: bool,
    pub used: usize,
}

/// Rewrap `rows` to `cols` the way tmux's `grid_reflow` does: rows joined by
/// `wrapped` form one logical line, which is split again at `cols`. A wide
/// character that would straddle the edge moves whole to the next row.
#[allow(dead_code)]
pub fn reflow_rows(
    rows: &[(Vec<SnapCell>, ColdMeta)],
    cols: usize,
) -> Vec<(Vec<SnapCell>, ColdMeta)> {
    if cols == 0 {
        return rows.to_vec();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let mut line: Vec<SnapCell> = Vec::new();
        while i < rows.len() && rows[i].1.wrapped && i + 1 < rows.len() {
            let cells = &rows[i].0;
            let leading = cells
                .last()
                .is_some_and(|c| c.flags.contains(CellFlags::LEADING_WIDE_CHAR_SPACER));
            line.extend_from_slice(&cells[..cells.len() - usize::from(leading)]);
            i += 1;
        }
        let (cells, meta) = &rows[i];
        line.extend_from_slice(&cells[..meta.used.min(cells.len())]);
        i += 1;
        split_line(line, cols, &mut out);
    }
    out
}

fn split_line(line: Vec<SnapCell>, cols: usize, out: &mut Vec<(Vec<SnapCell>, ColdMeta)>) {
    let mut pos = 0;
    while line.len() - pos > cols {
        let mut take = cols;
        if line[pos + cols - 1].flags.contains(CellFlags::WIDE_CHAR) {
            take = if cols == 1 { 2 } else { cols - 1 };
            if pos + take == line.len() {
                break;
            }
        }
        out.push((
            line[pos..pos + take].to_vec(),
            ColdMeta {
                wrapped: true,
                used: take,
            },
        ));
        pos += take;
    }
    let rest = line[pos..].to_vec();
    let used = rest.len();
    out.push((
        rest,
        ColdMeta {
            wrapped: false,
            used,
        },
    ));
}

#[cfg(test)]
mod tests {
    use super::super::written::WRITTEN;
    use super::*;
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    fn cell(c: char, flags: CellFlags) -> SnapCell {
        SnapCell {
            c,
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags,
            zerowidth: Vec::new(),
            underline_color: None,
            hyperlink: None,
        }
    }

    fn row(s: &str, used: usize, wrapped: bool) -> (Vec<SnapCell>, ColdMeta) {
        let mut cells = Vec::new();
        for c in s.chars() {
            if c == '界' {
                cells.push(cell(c, CellFlags::WIDE_CHAR));
                cells.push(cell(' ', CellFlags::WIDE_CHAR_SPACER));
            } else {
                cells.push(cell(c, CellFlags::empty()));
            }
        }
        (cells, ColdMeta { wrapped, used })
    }

    fn text(r: &(Vec<SnapCell>, ColdMeta)) -> String {
        r.0.iter()
            .filter(|c| !c.flags.contains(CellFlags::WIDE_CHAR_SPACER))
            .map(|c| c.c)
            .collect()
    }

    #[test]
    fn narrower_splits() {
        let out = reflow_rows(&[row("abcdef", 6, false)], 3);
        assert_eq!(out.len(), 2);
        assert_eq!(
            (text(&out[0]), out[0].1),
            (
                "abc".into(),
                ColdMeta {
                    wrapped: true,
                    used: 3
                }
            )
        );
        assert_eq!(
            (text(&out[1]), out[1].1),
            (
                "def".into(),
                ColdMeta {
                    wrapped: false,
                    used: 3
                }
            )
        );
        let out = reflow_rows(&[row("abcd", 4, true), row("ef", 2, false)], 3);
        assert_eq!(out.iter().map(text).collect::<Vec<_>>(), ["abc", "def"]);
    }

    #[test]
    fn one_column() {
        let out = reflow_rows(&[row("ab", 2, false), row("cd", 2, false)], 1);
        assert_eq!(
            out.iter().map(text).collect::<Vec<_>>(),
            ["a", "b", "c", "d"]
        );
        assert_eq!(
            out.iter().map(|r| r.1.wrapped).collect::<Vec<_>>(),
            [true, false, true, false]
        );
    }

    #[test]
    fn wider_joins() {
        let out = reflow_rows(&[row("abc", 3, true), row("d", 1, false)], 8);
        assert_eq!(out.len(), 1);
        assert_eq!(text(&out[0]), "abcd");
        assert_eq!(
            out[0].1,
            ColdMeta {
                wrapped: false,
                used: 4
            }
        );
    }

    #[test]
    fn deep_join_across_many_rows() {
        let mut rows: Vec<_> = (0..50).map(|_| row("abcd", 4, true)).collect();
        rows.push(row("e", 1, false));
        let out = reflow_rows(&rows, 7);
        let all: String = out.iter().map(text).collect();
        assert_eq!(all.len(), 201);
        assert!(
            out[..out.len() - 1]
                .iter()
                .all(|r| r.1.wrapped && r.1.used == 7)
        );
        assert_eq!(out.last().map(|r| r.1.used), Some(5));
    }

    #[test]
    fn wide_char_never_split() {
        let out = reflow_rows(&[row("ab界", 4, false)], 3);
        assert_eq!(out.len(), 2);
        assert_eq!(text(&out[0]), "ab");
        assert_eq!(
            out[0].1,
            ColdMeta {
                wrapped: true,
                used: 2
            }
        );
        assert_eq!(text(&out[1]), "界");
        assert_eq!(
            out[1].1,
            ColdMeta {
                wrapped: false,
                used: 2
            }
        );
    }

    #[test]
    fn wide_char_alone_at_one_column() {
        let out = reflow_rows(&[row("a界", 3, false)], 1);
        assert_eq!(out.iter().map(|r| r.1.used).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn zero_width_stays_on_base_cell() {
        let mut r = row("abcd", 4, false);
        r.0[2].zerowidth.push('\u{301}');
        let out = reflow_rows(&[r], 3);
        assert_eq!(out[0].0[2].zerowidth, ['\u{301}']);
        assert!(out[1].0[0].zerowidth.is_empty());
    }

    #[test]
    fn written_blanks_survive_join() {
        let mut a = row("a ", 2, true);
        a.0[1].flags.insert(WRITTEN);
        let out = reflow_rows(&[a, row("b", 1, false)], 8);
        assert_eq!(out.len(), 1);
        assert!(out[0].0[1].flags.contains(WRITTEN));
        assert_eq!(out[0].1.used, 3);
    }

    #[test]
    fn a_leading_spacer_is_dropped_when_rows_join() {
        let mut a = row("abc", 3, true);
        a.0.push(cell(' ', CellFlags::LEADING_WIDE_CHAR_SPACER));
        let out = reflow_rows(&[a, row("界", 2, false)], 8);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0.len(), 5);
        assert_eq!(out[0].1.used, 5);
    }

    #[test]
    fn empty_row_stays_one_row() {
        let out = reflow_rows(&[row("", 0, false)], 5);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].1,
            ColdMeta {
                wrapped: false,
                used: 0
            }
        );
    }

    #[test]
    fn cols_zero_returns_input() {
        let rows = vec![row("abcd", 4, true), row("ef", 2, false)];
        assert_eq!(reflow_rows(&rows, 0), rows);
    }
}
