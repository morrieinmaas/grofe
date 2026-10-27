//! Images on a client's screen (G3, `docs/graphics-placement.md`).
//!
//! [`collect`] turns a pane's placements into what one client shows: each
//! clipped to its pane and to the client, as a [`ScreenImage`] on the
//! [`Screen`](crate::server::render::Screen). [`hide_covered`] leaves out
//! the ones that something drawn later (a popup, a menu, a floating pane)
//! covers. [`emit`] writes the escape sequences that turn what the client's
//! terminal shows into the next frame's images.

use std::sync::Arc;

use crate::protocol::Caps;
use crate::server::graphics::Remap;
use crate::server::imgstore::{PaneImages, ViewKitty};
use crate::server::pane::SnapCell;
use crate::server::render::Screen;
use crate::server::sixel::{self, Sixel};
use crate::types::PaneId;

/// What a client's terminal can show of the images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageCaps {
    pub kitty: bool,
    pub sixel: bool,
    /// One cell's pixel size; sixel needs it (tmux draws its text
    /// fallback without).
    pub cell_px: Option<(u16, u16)>,
}

impl ImageCaps {
    pub fn of(caps: &Caps) -> ImageCaps {
        ImageCaps {
            kitty: caps.kitty_graphics,
            sixel: caps.sixel,
            cell_px: caps.cell_px,
        }
    }
}

/// The visible part of a placement: its cells `c0..c1` by `r0..r1`.
pub type Crop = (u32, u32, u32, u32);

/// An image on a client's screen.
#[derive(Debug, Clone)]
pub enum ScreenImage {
    Kitty(KittyShown),
    Sixel(SixelShown),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyShown {
    /// The outer terminal's image id and the placement id.
    pub outer: u32,
    pub pid: u32,
    /// Client cell of the first visible cell.
    pub x: u16,
    pub y: u16,
    /// The placement's size in cells, whole.
    pub full: (u32, u32),
    pub crop: Crop,
    /// The source rectangle of the whole placement: x, y, w, h in pixels.
    pub src: [u32; 4],
    pub off: (u32, u32),
    pub z: i32,
}

#[derive(Debug, Clone)]
pub struct SixelShown {
    pub serial: u32,
    pub x: u16,
    pub y: u16,
    pub crop: Crop,
    pub image: Arc<Sixel>,
    /// The client's cell size in pixels.
    pub px: (u32, u32),
}

impl PartialEq for ScreenImage {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ScreenImage::Kitty(a), ScreenImage::Kitty(b)) => a == b,
            (ScreenImage::Sixel(a), ScreenImage::Sixel(b)) => {
                (a.serial, a.x, a.y, a.crop, a.px) == (b.serial, b.x, b.y, b.crop, b.px)
            }
            _ => false,
        }
    }
}

impl ScreenImage {
    /// The client cells the image covers: x, y, width, height.
    pub fn rect(&self) -> (u16, u16, u16, u16) {
        let (x, y, crop) = match self {
            ScreenImage::Kitty(k) => (k.x, k.y, k.crop),
            ScreenImage::Sixel(s) => (s.x, s.y, s.crop),
        };
        (x, y, (crop.1 - crop.0) as u16, (crop.3 - crop.2) as u16)
    }

    fn kitty_key(&self) -> Option<(u32, u32)> {
        match self {
            ScreenImage::Kitty(k) => Some((k.outer, k.pid)),
            ScreenImage::Sixel(_) => None,
        }
    }
}

/// Where a pane is on a client's screen and how much of the screen there
/// is for it, all in client cells.
#[derive(Debug, Clone, Copy)]
pub struct Place {
    /// The pane's top-left cell (negative where a crop hides it).
    pub origin: (i32, i32),
    pub size: (u16, u16),
    /// The part of the screen the window is drawn on.
    pub area: (u16, u16),
}

/// The part of a `cols` by `rows` placement whose top-left cell is `(x, y)`
/// that lies in `bounds` (`x0, y0, x1, y1`, the end exclusive).
pub fn clip(x: i32, y: i32, cols: u32, rows: u32, bounds: (i32, i32, i32, i32)) -> Option<Crop> {
    let c0 = (bounds.0 - x).max(0);
    let c1 = (bounds.2 - x).min(cols as i32);
    let r0 = (bounds.1 - y).max(0);
    let r1 = (bounds.3 - y).min(rows as i32);
    (c0 < c1 && r0 < r1).then_some((c0 as u32, c1 as u32, r0 as u32, r1 as u32))
}

/// Adds `pane`'s placements to `images`, for a client with `caps`. The
/// sixel images that must be shown as text come back as `(x, y, lines)`.
pub fn collect(
    images: &mut Vec<ScreenImage>,
    fallbacks: &mut Vec<(i32, i32, Crop, (u32, u32))>,
    pane: PaneId,
    view: &PaneImages,
    at: Place,
    remap: Option<&Remap>,
    caps: ImageCaps,
) {
    let bounds = (
        at.origin.0.max(0),
        at.origin.1.max(0),
        (at.origin.0 + i32::from(at.size.0)).min(i32::from(at.area.0)),
        (at.origin.1 + i32::from(at.size.1)).min(i32::from(at.area.1)),
    );
    let mut kitty: Vec<&ViewKitty> = view.kitty.iter().collect();
    // Kitty stacks by z, then by image id.
    kitty.sort_by_key(|k| (k.z, k.image, k.serial));
    if caps.kitty {
        for k in kitty {
            let Some(outer) = remap.and_then(|r| r.get(pane, k.image)) else {
                continue;
            };
            let (x, y) = (at.origin.0 + k.col, at.origin.1 + k.row);
            let Some(crop) = clip(x, y, k.cols, k.rows, bounds) else {
                continue;
            };
            images.push(ScreenImage::Kitty(KittyShown {
                outer,
                pid: k.serial,
                x: (x + crop.0 as i32) as u16,
                y: (y + crop.2 as i32) as u16,
                full: (k.cols, k.rows),
                crop,
                src: k.src,
                off: k.off,
                z: k.z,
            }));
        }
    }
    for s in &view.sixel {
        let (x, y) = (at.origin.0 + s.col, at.origin.1 + s.row);
        let Some(crop) = clip(x, y, s.cells.0, s.cells.1, bounds) else {
            continue;
        };
        let shown = (x + crop.0 as i32, y + crop.2 as i32);
        match (caps.sixel, caps.cell_px) {
            (true, Some((w, h))) if w > 0 && h > 0 => images.push(ScreenImage::Sixel(SixelShown {
                serial: s.serial,
                x: shown.0 as u16,
                y: shown.1 as u16,
                crop,
                image: Arc::clone(&s.image),
                px: (u32::from(w), u32::from(h)),
            })),
            _ => fallbacks.push((shown.0, shown.1, crop, s.cells)),
        }
    }
}

/// Paints tmux's text for a sixel image the client cannot show.
pub fn paint_fallback(screen: &mut Screen, x: i32, y: i32, crop: Crop, cells: (u32, u32)) {
    let text = sixel::fallback(cells.0, cells.1);
    let lines: Vec<&[u8]> = text
        .split(|b| *b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
        .collect();
    let blank = Screen::new(1, 1).cells.remove(0);
    for r in crop.2..crop.3 {
        let Some(line) = lines.get(r as usize) else {
            continue;
        };
        for c in crop.0..crop.1 {
            let (cx, cy) = (x + (c - crop.0) as i32, y + (r - crop.2) as i32);
            if cx < 0 || cy < 0 || cx >= i32::from(screen.cols) || cy >= i32::from(screen.rows) {
                continue;
            }
            let ch = line.get(c as usize).copied().unwrap_or(b' ');
            screen.cells[cy as usize * screen.cols as usize + cx as usize] = SnapCell {
                c: char::from(ch),
                ..blank.clone()
            };
        }
    }
}

/// Leaves out the images that changed cells cover: `before` is the screen
/// as it was when the images were laid out, `screen` as it is now.
pub fn hide_covered(screen: &mut Screen, before: &[SnapCell]) {
    let cols = screen.cols as usize;
    if before.len() != screen.cells.len() {
        return;
    }
    let cells = &screen.cells;
    screen.images.retain(|image| {
        let (x, y, w, h) = image.rect();
        (y..y + h).all(|row| {
            let start = row as usize * cols + x as usize;
            let end = start + w as usize;
            end <= cells.len() && cells[start..end] == before[start..end]
        })
    });
}

/// Shifts every image `by` rows down (a status row above the window).
pub fn shift_down(images: &mut [ScreenImage], by: u16) {
    for image in images {
        match image {
            ScreenImage::Kitty(k) => k.y += by,
            ScreenImage::Sixel(s) => s.y += by,
        }
    }
}

fn cup(out: &mut Vec<u8>, x: u16, y: u16) {
    out.extend_from_slice(format!("\x1b[{};{}H", u32::from(y) + 1, u32::from(x) + 1).as_bytes());
}

/// `a=p` for a placement: at its first visible cell, showing the part of
/// the source its visible cells map to.
fn kitty_put(out: &mut Vec<u8>, k: &KittyShown) {
    let [sx, sy, sw, sh] = k.src.map(u64::from);
    let (cols, rows) = (u64::from(k.full.0), u64::from(k.full.1));
    let (c0, c1, r0, r1) = (
        u64::from(k.crop.0),
        u64::from(k.crop.1),
        u64::from(k.crop.2),
        u64::from(k.crop.3),
    );
    let x = sx + c0 * sw / cols;
    let w = c1 * sw / cols - c0 * sw / cols;
    let y = sy + r0 * sh / rows;
    let h = r1 * sh / rows - r0 * sh / rows;
    if w == 0 || h == 0 {
        return;
    }
    cup(out, k.x, k.y);
    let mut keys = format!(
        "a=p,i={},p={},x={x},y={y},w={w},h={h},c={},r={},z={},C=1,q=2",
        k.outer,
        k.pid,
        c1 - c0,
        r1 - r0,
        k.z
    );
    if k.crop.0 == 0 && k.crop.2 == 0 && k.off != (0, 0) {
        keys.push_str(&format!(",X={},Y={}", k.off.0, k.off.1));
    }
    out.extend_from_slice(format!("\x1b_G{keys}\x1b\\").as_bytes());
}

/// Deletes one placement, keeping the image data.
fn kitty_delete(out: &mut Vec<u8>, outer: u32, pid: u32) {
    out.extend_from_slice(format!("\x1b_Ga=d,d=i,i={outer},p={pid},q=2\x1b\\").as_bytes());
}

/// The sixel image cut to its visible cells and scaled to the client's.
fn sixel_put(out: &mut Vec<u8>, s: &SixelShown) {
    let crop = s.crop;
    let Some(cut) = s
        .image
        .scale(s.px.0, s.px.1, crop.0, crop.2, crop.1 - crop.0, crop.3 - crop.2, false)
    else {
        return;
    };
    let Some(data) = cut.print(Some(&s.image)) else {
        return;
    };
    cup(out, s.x, s.y);
    out.extend_from_slice(&data);
}

/// The cells to rewrite so that a sixel image that is gone or moved is
/// wiped from the terminal.
pub fn wipe_rects(placed: &[ScreenImage], next: &[ScreenImage]) -> Vec<(u16, u16, u16, u16)> {
    placed
        .iter()
        .filter(|p| matches!(p, ScreenImage::Sixel(_)) && !next.contains(p))
        .map(ScreenImage::rect)
        .collect()
}

/// Writes what turns the images `placed` on the client's terminal into
/// `next.images`, after the frame's cells: kitty placements deleted or
/// put again, sixel images drawn again where they are new or changed or
/// where a cell under them was rewritten (`changed`). A `full` redraw
/// starts from nothing.
pub fn emit(
    out: &mut Vec<u8>,
    next: &Screen,
    placed: &[ScreenImage],
    full: bool,
    changed: impl Fn(u16, u16) -> bool,
) {
    for p in placed {
        let Some((outer, pid)) = p.kitty_key() else {
            continue;
        };
        let kept = !full
            && next
                .images
                .iter()
                .any(|n| n.kitty_key() == Some((outer, pid)));
        if !kept {
            kitty_delete(out, outer, pid);
        }
    }
    for image in &next.images {
        match image {
            ScreenImage::Kitty(k) => {
                if full || !placed.contains(image) {
                    kitty_put(out, k);
                }
            }
            ScreenImage::Sixel(s) => {
                let (x, y, w, h) = image.rect();
                let touched = (y..y + h).any(|row| (x..x + w).any(|col| changed(col, row)));
                if full || touched || !placed.contains(image) {
                    sixel_put(out, s);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kitty(outer: u32, pid: u32, at: (u16, u16), crop: Crop) -> ScreenImage {
        ScreenImage::Kitty(KittyShown {
            outer,
            pid,
            x: at.0,
            y: at.1,
            full: (4, 2),
            crop,
            src: [0, 0, 40, 40],
            off: (0, 0),
            z: -1,
        })
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn clipping_math() {
        // A 4x2 placement at (3, 2), inside a 10x10 area.
        let area = (0, 0, 10, 10);
        assert_eq!(clip(3, 2, 4, 2, area), Some((0, 4, 0, 2)));
        // Right edge at column 5: two columns show.
        assert_eq!(clip(3, 2, 4, 2, (0, 0, 5, 10)), Some((0, 2, 0, 2)));
        // Starts left of the area: the first columns are cut.
        assert_eq!(clip(-2, 2, 4, 2, area), Some((2, 4, 0, 2)));
        // Above the area, a row cut; wholly outside, nothing.
        assert_eq!(clip(3, -1, 4, 2, area), Some((0, 4, 1, 2)));
        assert_eq!(clip(3, -2, 4, 2, area), None);
        assert_eq!(clip(10, 2, 4, 2, area), None);
        // Both sides of a pane narrower than the placement.
        assert_eq!(clip(0, 0, 8, 1, (2, 0, 5, 10)), Some((2, 5, 0, 1)));
    }

    #[test]
    fn a_cropped_placement_shows_the_source_its_cells_map_to() {
        let mut out = Vec::new();
        // Columns 1..3 of 4, row 0 of 2, of a 40x40 source.
        kitty_put(&mut out, &match kitty(7, 9, (5, 6), (1, 3, 0, 1)) {
            ScreenImage::Kitty(k) => k,
            ScreenImage::Sixel(_) => unreachable!(),
        });
        assert_eq!(
            text(&out),
            "\x1b[7;6H\x1b_Ga=p,i=7,p=9,x=10,y=0,w=20,h=20,c=2,r=1,z=-1,C=1,q=2\x1b\\"
        );
    }

    #[test]
    fn whole_placements_keep_the_cell_offset() {
        let mut k = match kitty(1, 2, (0, 0), (0, 4, 0, 2)) {
            ScreenImage::Kitty(k) => k,
            ScreenImage::Sixel(_) => unreachable!(),
        };
        k.off = (3, 4);
        let mut out = Vec::new();
        kitty_put(&mut out, &k);
        assert!(text(&out).ends_with(",C=1,q=2,X=3,Y=4\x1b\\"), "{}", text(&out));
        // Cropped at the top: the offset is the cell's, so it goes.
        k.crop = (0, 4, 1, 2);
        let mut out = Vec::new();
        kitty_put(&mut out, &k);
        assert!(!text(&out).contains("X=3"));
    }

    #[test]
    fn emit_puts_new_moves_changed_and_deletes_gone() {
        let a = kitty(1, 10, (0, 0), (0, 4, 0, 2));
        let b = kitty(2, 11, (5, 5), (0, 4, 0, 2));
        let moved = kitty(1, 10, (1, 1), (0, 4, 0, 2));
        let mut next = Screen::new(20, 20);
        let none = |_, _| false;

        // Nothing placed yet: both put.
        next.images = vec![a.clone(), b.clone()];
        let mut out = Vec::new();
        emit(&mut out, &next, &[], false, none);
        assert_eq!(text(&out).matches("a=p,").count(), 2);

        // Same again: nothing.
        let mut out = Vec::new();
        emit(&mut out, &next, &[a.clone(), b.clone()], false, none);
        assert!(out.is_empty());

        // One moved, one gone.
        next.images = vec![moved];
        let mut out = Vec::new();
        emit(&mut out, &next, &[a.clone(), b.clone()], false, none);
        let s = text(&out);
        assert!(s.contains("a=d,d=i,i=2,p=11,q=2"), "{s}");
        assert_eq!(s.matches("a=p,i=1,p=10").count(), 1, "{s}");
        assert!(!s.contains("a=d,d=i,i=1,"), "a move replaces in place: {s}");

        // A full redraw deletes everything placed and puts everything.
        let mut out = Vec::new();
        emit(&mut out, &next, &[a, b], true, none);
        let s = text(&out);
        assert_eq!(s.matches("a=d,d=i").count(), 2, "{s}");
        assert_eq!(s.matches("a=p,").count(), 1, "{s}");
    }

    #[test]
    fn covered_images_are_hidden_whole() {
        let mut screen = Screen::new(10, 5);
        screen.images = vec![kitty(1, 1, (2, 1), (0, 4, 0, 2)), kitty(2, 2, (7, 3), (0, 2, 0, 2))];
        let before = screen.cells.clone();
        // A popup border cell inside the first image's rectangle.
        screen.cells[2 * 10 + 4].c = '|';
        hide_covered(&mut screen, &before);
        assert_eq!(screen.images.len(), 1);
        assert_eq!(screen.images[0].kitty_key(), Some((2, 2)));
        // A cell next to the second does not matter.
        screen.cells[3 * 10 + 6].c = '|';
        hide_covered(&mut screen, &before);
        assert_eq!(screen.images.len(), 1);
    }

    #[test]
    fn the_sixel_text_fallback_is_painted_into_the_cells() {
        let mut screen = Screen::new(30, 4);
        paint_fallback(&mut screen, 2, 1, (0, 20, 0, 2), (20, 2));
        let row = |y: usize| -> String { screen.cells[y * 30..y * 30 + 30].iter().map(|c| c.c).collect() };
        assert_eq!(row(1).trim_end(), format!("{:2}SIXEL IMAGE (20x2)++", ""));
        assert_eq!(row(2).trim_end(), format!("{:2}{}", "", "+".repeat(20)));
        // Cropped at the left by two cells and at the foot.
        let mut screen = Screen::new(30, 4);
        paint_fallback(&mut screen, 0, 0, (2, 4, 1, 2), (20, 2));
        assert_eq!(screen.cells[0].c, '+');
        assert_eq!(screen.cells[2].c, ' ');
    }

    #[test]
    fn sixel_is_drawn_when_new_or_when_cells_under_it_changed() {
        let image = Arc::new(
            Sixel::parse(b"q#0;2;100;0;0!10~-!10~-!10~", 0, 10, 10).expect("parses"),
        );
        let shown = ScreenImage::Sixel(SixelShown {
            serial: 4,
            x: 3,
            y: 2,
            crop: (0, 1, 0, 2),
            image,
            px: (10, 10),
        });
        let mut next = Screen::new(20, 10);
        next.images = vec![shown.clone()];
        let mut out = Vec::new();
        emit(&mut out, &next, &[], false, |_, _| false);
        let s = text(&out);
        assert!(s.starts_with("\x1b[3;4H\x1bP0;0q"), "{s:?}");
        // Unchanged and untouched: nothing.
        let mut out = Vec::new();
        emit(&mut out, &next, std::slice::from_ref(&shown), false, |_, _| false);
        assert!(out.is_empty());
        // A cell inside its rectangle was rewritten: drawn again.
        let mut out = Vec::new();
        emit(&mut out, &next, std::slice::from_ref(&shown), false, |x, y| (x, y) == (3, 3));
        assert!(!out.is_empty());
        // One outside: not.
        let mut out = Vec::new();
        emit(&mut out, &next, std::slice::from_ref(&shown), false, |x, y| (x, y) == (4, 3));
        assert!(out.is_empty());
        // Gone: its cells are to be rewritten.
        next.images.clear();
        assert_eq!(wipe_rects(&[shown], &next.images), vec![(3, 2, 1, 2)]);
    }
}
