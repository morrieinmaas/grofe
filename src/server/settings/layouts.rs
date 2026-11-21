//! The settings menu's Layouts section: layout files and their previews.

use crate::layouts::{self, Dirs, schema};
use crate::server::listdraw;
use crate::server::render::Screen;
use crate::server::status::Style;
use crate::types::Rect;

use super::rows::{Act, Entry};
use super::sections::Section;

/// A row per file in the layouts directory: name and the sessions it makes.
pub fn entries() -> Vec<Entry> {
    let Ok(found) = layouts::entries(&Dirs::from_process()) else {
        return Vec::new();
    };
    found
        .into_iter()
        .map(|e| Entry {
            section: Section::Layouts,
            cols: vec![
                e.name.clone(),
                e.sessions.map_or_else(|err| err, |s| s.join(", ")),
            ],
            act: Act::Layout {
                name: e.name,
                toml: std::fs::read_to_string(&e.file).unwrap_or_default(),
            },
        })
        .collect()
}

/// Draws the preview of `layout_toml` into `area`.
pub fn draw_preview(screen: &mut Screen, area: Rect, layout_toml: &str) {
    let lines = preview(layout_toml, area.cols, area.rows.min(12));
    for (i, line) in lines.iter().enumerate() {
        let end = usize::from(area.x + area.cols);
        let y = usize::from(area.y) + i;
        listdraw::put_text(screen, usize::from(area.x), y, end, line, &Style::default());
    }
}

/// `h` rows of at most `w` columns drawing the first window's panes of a
/// layout file: one box per pane, side by side for `split = "h"`, stacked
/// otherwise.
// ponytail: panes are drawn in equal parts; `size` and a `layout` string
// are not read.
pub fn preview(layout_toml: &str, w: u16, h: u16) -> Vec<String> {
    let (w, h) = (usize::from(w), usize::from(h));
    let mut grid = vec![vec![' '; w]; h];
    let window = schema::parse(
        layout_toml,
        std::path::Path::new(""),
        None,
        std::path::Path::new("/"),
    )
    .ok()
    .and_then(|p| p.sessions.into_iter().next())
    .and_then(|s| s.windows.into_iter().next());
    if let Some(win) = window {
        let n = win.panes.len().max(1);
        for i in 0..n {
            let (x0, x1, y0, y1) = if win.side_by_side {
                (w * i / n, w * (i + 1) / n, 0, h)
            } else {
                (0, w, h * i / n, h * (i + 1) / n)
            };
            draw_box(&mut grid, x0, x1, y0, y1);
        }
    }
    grid.into_iter()
        .map(|row| row.into_iter().collect())
        .collect()
}

/// A box over columns `x0..x1` and rows `y0..y1`, when it fits two by two.
fn draw_box(grid: &mut [Vec<char>], x0: usize, x1: usize, y0: usize, y1: usize) {
    if x1 < x0 + 2 || y1 < y0 + 2 {
        return;
    }
    let (r, b) = (x1 - 1, y1 - 1);
    for edge in [y0, b] {
        grid[edge][x0 + 1..r].fill('─');
    }
    for row in &mut grid[y0 + 1..b] {
        row[x0] = '│';
        row[r] = '│';
    }
    grid[y0][x0] = '┌';
    grid[y0][r] = '┐';
    grid[b][x0] = '└';
    grid[b][r] = '┘';
}
