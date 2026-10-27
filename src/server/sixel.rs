//! Sixel images, a port of tmux 3.7b's `image-sixel.c` (G3).
//!
//! [`Sixel::parse`] reads the body of a `DCS q ... ST` into palette-indexed
//! pixels (index 0 is transparent, colour register `c` is index `c + 1`),
//! [`Sixel::scale`] cuts a cell rectangle out of it and maps it onto a
//! terminal's own cell size, and [`Sixel::print`] writes it as sixel again.
//! The quirks of the C are kept on purpose (the 10000 pixel limits, the
//! `ra` handling in `scale`, the run-length rules) so that what a terminal
//! is sent is what tmux would send.

use std::fmt::Write as _;

const WIDTH_LIMIT: u32 = 10_000;
const HEIGHT_LIMIT: u32 = 10_000;
/// Most pixels one image may hold. tmux's two limits alone allow 10000 by
/// 10000, which is 200 MB of palette indexes for one string; grofe's server
/// holds every session's, so it stops at 16 Mpixel (32 MB).
const PIXEL_LIMIT: u64 = 16 * 1024 * 1024;
/// `SIXEL_COLOUR_REGISTERS`: the highest colour register number.
const COLOUR_REGISTERS: u32 = 1024;
/// Most sixel characters (a repeat counts its length) one image may spend
/// writing pixels. `!10000~$` repeated fits 130000 times in tmux's 1 MiB
/// input limit: over a billion writes, which would hold the pane's lock for
/// seconds. Real encoders spend a few tens of millions.
const WORK_LIMIT: u64 = 64 * 1024 * 1024;

/// tmux's `DEFAULT_XPIXEL` and `DEFAULT_YPIXEL`: a cell's size in pixels
/// until a terminal says otherwise.
pub const DEFAULT_CELL_PX: (u32, u32) = (16, 32);

/// A parsed sixel image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sixel {
    /// Width and height in pixels.
    x: u32,
    y: u32,
    /// The cell size the image is measured in cells of.
    xpixel: u32,
    ypixel: u32,
    set_ra: bool,
    ra_x: u32,
    ra_y: u32,
    /// Colour register values: `type << 25 | c1 << 16 | c2 << 8 | c3`.
    colours: Vec<u32>,
    used_colours: u32,
    p2: u32,
    /// Parse cursor: pixel column, pixel row of the current band, colour.
    dx: u32,
    dy: u32,
    dc: u32,
    /// One row of pixels each, as wide as the image once written to.
    lines: Vec<Vec<u16>>,
    /// Characters written so far, against [`WORK_LIMIT`].
    work: u64,
    work_limit: u64,
}

impl Sixel {
    fn new(xpixel: u32, ypixel: u32, p2: u32) -> Sixel {
        Sixel {
            x: 0,
            y: 0,
            xpixel,
            ypixel,
            set_ra: false,
            ra_x: 0,
            ra_y: 0,
            colours: Vec::new(),
            used_colours: 0,
            p2,
            dx: 0,
            dy: 0,
            dc: 0,
            lines: Vec::new(),
            work: 0,
            work_limit: WORK_LIMIT,
        }
    }

    /// Bytes held by the pixels, for the store's cap.
    pub fn bytes(&self) -> usize {
        self.lines.iter().map(|l| l.len() * 2).sum::<usize>() + self.colours.len() * 4
    }

    /// The image's size in pixels.
    pub fn size_px(&self) -> (u32, u32) {
        (self.x, self.y)
    }

    fn expand_lines(&mut self, y: u32) -> bool {
        if y <= self.y {
            return false;
        }
        if y > HEIGHT_LIMIT || u64::from(y) * u64::from(self.x) > PIXEL_LIMIT {
            return true;
        }
        self.lines.resize(y as usize, Vec::new());
        self.y = y;
        false
    }

    fn expand_line(&mut self, line: usize, x: u32) -> bool {
        if x <= self.lines[line].len() as u32 {
            return false;
        }
        if x > WIDTH_LIMIT || u64::from(self.y) * u64::from(x.max(self.x)) > PIXEL_LIMIT {
            return true;
        }
        if x > self.x {
            self.x = x;
        }
        let width = self.x as usize;
        self.lines[line].resize(width, 0);
        false
    }

    fn get_pixel(&self, x: u32, y: u32) -> u16 {
        self.lines
            .get(y as usize)
            .and_then(|l| l.get(x as usize))
            .copied()
            .unwrap_or(0)
    }

    fn set_pixel(&mut self, x: u32, y: u32, c: u32) -> bool {
        if self.expand_lines(y + 1) || self.expand_line(y as usize, x + 1) {
            return true;
        }
        self.lines[y as usize][x as usize] = c as u16;
        false
    }

    fn write(&mut self, ch: u32) -> bool {
        self.work += 1;
        if self.work > self.work_limit {
            return true;
        }
        for i in 0..6 {
            if ch & (1 << i) != 0 && self.set_pixel(self.dx, self.dy + i, self.dc) {
                return true;
            }
        }
        false
    }

    /// The image's size in cells of `xpixel` by `ypixel` pixels.
    pub fn size_in_cells(&self) -> (u32, u32) {
        (
            self.x.div_ceil(self.xpixel.max(1)),
            self.y.div_ceil(self.ypixel.max(1)),
        )
    }

    /// Parses the body of a `DCS` whose final byte was `q` (`body` starts
    /// with that `q`), as `sixel_parse` does. `p2` is the DCS's second
    /// parameter; the pixel sizes are the cell size the image is measured
    /// in. `None` for an empty or malformed image.
    pub fn parse(body: &[u8], p2: u32, xpixel: u32, ypixel: u32) -> Option<Sixel> {
        Sixel::parse_limited(body, p2, xpixel, ypixel, WORK_LIMIT)
    }

    fn parse_limited(
        body: &[u8],
        p2: u32,
        xpixel: u32,
        ypixel: u32,
        work_limit: u64,
    ) -> Option<Sixel> {
        let (&first, rest) = body.split_first()?;
        if first != b'q' || rest.is_empty() {
            return None;
        }
        let mut si = Sixel::new(xpixel.max(1), ypixel.max(1), p2);
        si.work_limit = work_limit;
        let mut cp = 0;
        while cp < rest.len() {
            let ch = rest[cp];
            cp += 1;
            match ch {
                b'"' => cp = si.parse_attributes(rest, cp)?,
                b'#' => cp = si.parse_colour(rest, cp)?,
                b'!' => cp = si.parse_repeat(rest, cp)?,
                b'-' => {
                    si.dx = 0;
                    si.dy += 6;
                }
                b'$' => si.dx = 0,
                _ => {
                    if ch < 0x20 {
                        continue;
                    }
                    if !(0x3f..=0x7e).contains(&ch) {
                        return None;
                    }
                    if si.write(u32::from(ch - 0x3f)) {
                        return None;
                    }
                    si.dx += 1;
                }
            }
        }
        (si.x != 0 && si.y != 0).then_some(si)
    }

    fn parse_attributes(&mut self, s: &[u8], cp: usize) -> Option<usize> {
        let last = field_end(s, cp);
        let (_, p) = strtoul(s, cp);
        if p == last || s.get(p) != Some(&b';') {
            return Some(last);
        }
        let (_, p) = strtoul(s, p + 1);
        if p == last {
            return Some(last);
        }
        if s.get(p) != Some(&b';') {
            return None;
        }
        let (x, p) = strtoul(s, p + 1);
        if p == last || s.get(p) != Some(&b';') || x > u64::from(WIDTH_LIMIT) {
            return None;
        }
        let (y, p) = strtoul(s, p + 1);
        if p != last || y > u64::from(HEIGHT_LIMIT) {
            return None;
        }
        self.x = x as u32;
        self.expand_lines(y as u32);
        self.set_ra = true;
        self.ra_x = x as u32;
        self.ra_y = y as u32;
        Some(last)
    }

    fn parse_colour(&mut self, s: &[u8], cp: usize) -> Option<usize> {
        let last = field_end(s, cp);
        let (c, p) = strtoul(s, cp);
        if c > u64::from(COLOUR_REGISTERS) {
            return None;
        }
        let c = c as u32;
        if self.used_colours <= c {
            self.used_colours = c + 1;
        }
        self.dc = c + 1;
        if p == last || s.get(p) != Some(&b';') {
            return Some(last);
        }
        let mut vals = [0u64; 4];
        let mut p = p;
        for (i, v) in vals.iter_mut().enumerate() {
            let (n, q) = strtoul(s, p + 1);
            let end_ok = if i < 3 {
                q != last && s.get(q) == Some(&b';')
            } else {
                q == last
            };
            if !end_ok {
                return None;
            }
            *v = n;
            p = q;
        }
        let [ty, c1, c2, c3] = vals;
        let bad = (ty != 1 && ty != 2)
            || (ty == 1 && (c1 > 360 || c2 > 100 || c3 > 100))
            || (ty == 2 && (c1 > 100 || c2 > 100 || c3 > 100));
        if bad {
            return None;
        }
        if c as usize + 1 > self.colours.len() {
            self.colours.resize(c as usize + 1, 0);
        }
        self.colours[c as usize] =
            ((ty as u32) << 25) | ((c1 as u32) << 16) | ((c2 as u32) << 8) | c3 as u32;
        Some(last)
    }

    fn parse_repeat(&mut self, s: &[u8], cp: usize) -> Option<usize> {
        let mut last = cp;
        while last < s.len() && s[last].is_ascii_digit() {
            last += 1;
            if last - cp == 31 {
                return None;
            }
        }
        if last == cp || last == s.len() {
            return None;
        }
        let n: u64 = std::str::from_utf8(&s[cp..last]).ok()?.parse().ok()?;
        if n == 0 || n > u64::from(WIDTH_LIMIT) {
            return None;
        }
        let ch = u32::from(s[last].wrapping_sub(0x3f));
        for _ in 0..n {
            if self.write(ch) {
                return None;
            }
            self.dx += 1;
        }
        Some(last + 1)
    }

    /// The part of the image at cell `ox,oy` and `sx` by `sy` cells big
    /// (the second tuple), mapped onto cells of `xpixel` by `ypixel` pixels
    /// (the first; 0 keeps the image's own), as `sixel_scale`. With `colours` the palette is kept.
    pub fn scale(
        &self,
        (xpixel, ypixel): (u32, u32),
        (ox, oy, mut sx, mut sy): (u32, u32, u32, u32),
        colours: bool,
    ) -> Option<Sixel> {
        let (cx, cy) = self.size_in_cells();
        if ox >= cx || oy >= cy {
            return None;
        }
        if ox + sx >= cx {
            sx = cx - ox;
        }
        if oy + sy >= cy {
            sy = cy - oy;
        }
        let xpixel = if xpixel == 0 { self.xpixel } else { xpixel };
        let ypixel = if ypixel == 0 { self.ypixel } else { ypixel };
        let (pox, poy) = (ox * self.xpixel, oy * self.ypixel);
        let (psx, psy) = (sx * self.xpixel, sy * self.ypixel);
        let (tsx, tsy) = (sx * xpixel, sy * ypixel);

        let mut new = Sixel::new(xpixel, ypixel, self.p2);
        new.set_ra = self.set_ra;
        new.ra_x = self.ra_x.min(psx);
        new.ra_y = self.ra_y.min(psy);
        new.ra_x = new.ra_x * xpixel / self.xpixel;
        new.ra_y = new.ra_y * ypixel / self.ypixel;
        new.used_colours = self.used_colours;
        for y in 0..tsy {
            let py = (f64::from(poy) + f64::from(y) * f64::from(psy) / f64::from(tsy)) as u32;
            for x in 0..tsx {
                let px = (f64::from(pox) + f64::from(x) * f64::from(psx) / f64::from(tsx)) as u32;
                new.set_pixel(x, y, u32::from(self.get_pixel(px, py)));
            }
        }
        if colours && !self.colours.is_empty() {
            new.colours = self.colours.clone();
        }
        Some(new)
    }

    /// The image as a `DCS ... ST` string, with the palette of `map` (the
    /// image it was scaled from) when given, as `sixel_print`. `None` when
    /// the image uses no colour.
    pub fn print(&self, map: Option<&Sixel>) -> Option<Vec<u8>> {
        let palette = map.map_or(&self.colours, |m| &m.colours);
        let used = self.used_colours as usize;
        if used == 0 {
            return None;
        }
        let mut buf = format!("\x1bP0;{}q", self.p2);
        if self.set_ra {
            let _ = write!(buf, "\"1;1;{};{}", self.ra_x, self.ra_y);
        }
        for (i, c) in palette.iter().enumerate() {
            let _ = write!(
                buf,
                "#{i};{};{};{};{}",
                c >> 25,
                (c >> 16) & 0x1ff,
                (c >> 8) & 0xff,
                c & 0xff
            );
        }
        let mut out = buf.into_bytes();
        let mut chunks = vec![Chunk::default(); used];
        let mut active: Vec<usize> = Vec::new();
        let mut y = 0;
        while y < self.y {
            active.clear();
            self.compress_colours(&mut chunks, y, &mut active);
            for &c in &active {
                let chunk = &mut chunks[c];
                out.extend_from_slice(format!("#{c}").as_bytes());
                out.extend_from_slice(&chunk.data);
                repeat(&mut out, chunk.count, chunk.pattern + 0x3f);
                out.push(b'$');
                chunk.data.clear();
                chunk.next_x = 0;
                chunk.count = 0;
            }
            if out.last() == Some(&b'$') {
                out.pop();
            }
            out.push(b'-');
            y += 6;
        }
        if out.last() == Some(&b'-') {
            out.pop();
        }
        out.extend_from_slice(b"\x1b\\");
        Some(out)
    }

    fn compress_colours(&self, chunks: &mut [Chunk], y: u32, active: &mut Vec<usize>) {
        for x in 0..self.x {
            let mut colours = [0u16; 6];
            for (i, slot) in colours.iter_mut().enumerate() {
                let row = (y as usize) + i;
                let Some(line) = self.lines.get(row) else {
                    continue;
                };
                if let Some(&p) = line.get(x as usize)
                    && p != 0
                {
                    *slot = p;
                    if let Some(chunk) = chunks.get_mut(p as usize - 1) {
                        chunk.next_pattern |= 1 << i;
                    }
                }
            }
            for &p in &colours {
                if p == 0 {
                    continue;
                }
                let c = p as usize - 1;
                let Some(chunk) = chunks.get_mut(c) else {
                    continue;
                };
                if chunk.next_x == x + 1 {
                    continue;
                }
                if chunk.next_y < y + 1 {
                    chunk.next_y = y + 1;
                    active.push(c);
                }
                let dx = x - chunk.next_x;
                if chunk.pattern != chunk.next_pattern || dx != 0 {
                    repeat(&mut chunk.data, chunk.count, chunk.pattern + 0x3f);
                    repeat(&mut chunk.data, dx, b'?');
                    chunk.pattern = chunk.next_pattern;
                    chunk.count = 0;
                }
                chunk.count += 1;
                chunk.next_pattern = 0;
                chunk.next_x = x + 1;
            }
        }
    }
}

/// The run of one colour in the band being printed.
#[derive(Debug, Clone, Default)]
struct Chunk {
    next_x: u32,
    next_y: u32,
    count: u32,
    pattern: u8,
    next_pattern: u8,
    data: Vec<u8>,
}

/// `count` of `ch`, run-length coded from four on (`sixel_print_repeat`).
fn repeat(out: &mut Vec<u8>, count: u32, ch: u8) {
    match count {
        0 => {}
        1..=3 => out.extend(std::iter::repeat_n(ch, count as usize)),
        _ => {
            out.extend_from_slice(format!("!{count}").as_bytes());
            out.push(ch);
        }
    }
}

/// The end of the digits-and-semicolons run at `from`.
fn field_end(s: &[u8], from: usize) -> usize {
    let mut last = from;
    while last < s.len() && (s[last].is_ascii_digit() || s[last] == b';') {
        last += 1;
    }
    last
}

/// C's `strtoul(s + at, &end, 10)`: the number (0 if there are no digits,
/// saturating) and where it ended.
fn strtoul(s: &[u8], at: usize) -> (u64, usize) {
    let mut n: u64 = 0;
    let mut p = at;
    while p < s.len() && s[p].is_ascii_digit() {
        n = n.saturating_mul(10).saturating_add(u64::from(s[p] - b'0'));
        p += 1;
    }
    (n, p)
}

/// tmux's text stand-in for an image a terminal cannot show
/// (`image_fallback`): `sy` rows of `sx` plus signs, the first headed
/// `SIXEL IMAGE (SXxSY)` when that fits, rows split by `\r\n`.
pub fn fallback(sx: u32, sy: u32) -> Vec<u8> {
    let label = format!("SIXEL IMAGE ({sx}x{sy})\r\n");
    let lsize = label.len() as u32 + 1;
    let mut out = Vec::new();
    if sx < lsize - 3 {
        out.extend_from_slice(label.as_bytes());
    } else {
        out.extend_from_slice(&label.as_bytes()[..(lsize - 3) as usize]);
        out.extend(std::iter::repeat_n(b'+', (sx + 3 - lsize) as usize));
        out.extend_from_slice(b"\r\n");
    }
    for _ in 1..sy {
        out.extend(std::iter::repeat_n(b'+', sx as usize));
        out.extend_from_slice(b"\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2x6 image: colour 0 red on the left column, colour 1 on the right.
    const SMALL: &[u8] = b"q\"1;1;2;6#0;2;100;0;0#1;2;0;100;0#0~$#1?~";

    fn small() -> Sixel {
        Sixel::parse(SMALL, 0, 1, 1).expect("parses")
    }

    /// Repeats that rewrite the same pixels cost a bounded amount of work:
    /// `!10000~$` over and over would otherwise take seconds.
    #[test]
    fn a_repeat_flood_is_refused_after_a_bounded_amount_of_work() {
        let mut body = b"q#0;2;100;0;0".to_vec();
        for _ in 0..200 {
            body.extend_from_slice(b"!10000~$");
        }
        // 2 million characters against a budget of one million.
        assert!(Sixel::parse_limited(&body, 0, 10, 20, 1_000_000).is_none());
        assert!(Sixel::parse_limited(&body, 0, 10, 20, 3_000_000).is_some());
        // The real budget is far above an honest image's spend.
        assert!(Sixel::parse(b"q#0;2;100;0;0!10000~", 0, 10, 20).is_some());
    }

    #[test]
    fn parses_pixels_palette_and_size() {
        let si = small();
        assert_eq!(si.size_px(), (2, 6));
        assert_eq!(si.used_colours, 2);
        assert_eq!(si.get_pixel(0, 0), 1);
        assert_eq!(si.get_pixel(1, 5), 2);
        assert_eq!(si.get_pixel(0, 6), 0);
        assert_eq!(si.size_in_cells(), (2, 6));
        let big = Sixel::parse(SMALL, 0, 16, 32).expect("parses");
        assert_eq!(big.size_in_cells(), (1, 1));
    }

    #[test]
    fn rejects_what_tmux_rejects() {
        assert!(Sixel::parse(b"", 0, 1, 1).is_none());
        assert!(Sixel::parse(b"q", 0, 1, 1).is_none());
        assert!(Sixel::parse(b"x~", 0, 1, 1).is_none());
        // A space is not a sixel data byte.
        assert!(Sixel::parse(b"q#0;2;1;1;1 ~", 0, 1, 1).is_none());
        // Colour register past 1024, colour type 3, too wide.
        assert!(Sixel::parse(b"q#1025;2;1;1;1~", 0, 1, 1).is_none());
        assert!(Sixel::parse(b"q#0;3;1;1;1~", 0, 1, 1).is_none());
        assert!(Sixel::parse(b"q\"1;1;10001;6~", 0, 1, 1).is_none());
        // Nothing drawn.
        assert!(Sixel::parse(b"q#0;2;1;1;1", 0, 1, 1).is_none());
        // A repeat with no data byte.
        assert!(Sixel::parse(b"q!5", 0, 1, 1).is_none());
    }

    #[test]
    fn an_image_past_the_pixel_limit_is_refused() {
        // 10000 wide and 2400 tall is 24 Mpixel: tmux would hold it.
        let mut body = b"q#0;2;1;1;1".to_vec();
        for _ in 0..400 {
            body.extend_from_slice(b"!10000~-");
        }
        assert!(Sixel::parse(&body, 0, 16, 32).is_none());
        // Half of that is fine.
        body.truncate(b"q#0;2;1;1;1".len() + 200 * b"!10000~-".len());
        assert!(Sixel::parse(&body, 0, 16, 32).is_some());
    }

    #[test]
    fn repeat_and_band_moves() {
        let si = Sixel::parse(b"q#0;2;0;0;0!3~-!2~", 0, 1, 1).expect("parses");
        assert_eq!(si.size_px(), (3, 12));
        assert_eq!(si.get_pixel(2, 5), 1);
        assert_eq!(si.get_pixel(2, 6), 0);
        assert_eq!(si.get_pixel(1, 11), 1);
    }

    #[test]
    fn prints_what_it_parsed() {
        let si = small();
        let out = si.print(None).expect("prints");
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(text.starts_with("\x1bP0;0q\"1;1;2;6#0;2;100;0;0#1;2;0;100;0"));
        assert!(text.ends_with("\x1b\\"));
        // Parsing the print gives the same pixels back.
        let body = &out[b"\x1bP0;".len() + 1..out.len() - 2];
        let again = Sixel::parse(body, 0, 1, 1).expect("reparses");
        assert_eq!(again.lines, si.lines);
    }

    #[test]
    fn scale_crops_to_cells() {
        // 4 pixels wide, 12 tall, 2x6 pixel cells: a 2x2 cell image.
        let si = Sixel::parse(b"q#0;2;1;1;1!4~-!4~", 0, 2, 6).expect("parses");
        assert_eq!(si.size_in_cells(), (2, 2));
        let top = si.scale((0, 0), (0, 0, 2, 1), true).expect("scales");
        assert_eq!(top.size_px(), (4, 6));
        let bottom = si.scale((0, 0), (0, 1, 2, 1), true).expect("scales");
        assert_eq!(bottom.size_px(), (4, 6));
        let right = si.scale((0, 0), (1, 0, 1, 2), false).expect("scales");
        assert_eq!(right.size_px(), (2, 12));
        // Past the image.
        assert!(si.scale((0, 0), (2, 0, 1, 1), false).is_none());
        // Onto bigger cells: pixels are repeated.
        let wide = si.scale((4, 12), (0, 0, 2, 2), true).expect("scales");
        assert_eq!(wide.size_px(), (8, 24));
    }

    /// `sixel-3x2.six` (48 pixels wide, 60 tall, four colours, runs of every
    /// length) shown by a tmux 3.7b built with `--enable-sixel` on a terminal
    /// with 16x32 pixel cells and the `sixel` feature: its output is the
    /// fixture, from the `DCS` to the `ST`. Recorded in the golden container
    /// (`tmux-sixel`) with a pty client that answers DA1 with attribute 4.
    /// `sixel-3x2.tmux-scrolled.bin` is what it printed once the image had
    /// scrolled up until only its second row was left.
    #[test]
    fn printing_is_byte_for_byte_what_tmux_sends() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/graphics");
        let input = std::fs::read(dir.join("sixel-3x2.six")).expect("input");
        let want = std::fs::read(dir.join("sixel-3x2.tmux-out.bin")).expect("tmux's output");
        let body = &input[2..input.len() - 2];
        let si = Sixel::parse(body, 0, 16, 32).expect("parses");
        assert_eq!(si.size_in_cells(), (3, 2));
        // tmux scales the whole image onto the client's cells to print it.
        let whole = si.scale((16, 32), (0, 0, 3, 2), false).expect("scales");
        let got = whole.print(Some(&si)).expect("prints");
        assert_eq!(
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(&want)
        );
        // Scrolled up by a row: tmux cut it to its second row.
        let want = std::fs::read(dir.join("sixel-3x2.tmux-scrolled.bin")).expect("tmux's output");
        let low = si.scale((16, 32), (0, 1, 3, 1), false).expect("scales");
        let got = low.print(Some(&si)).expect("prints");
        assert_eq!(
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(&want)
        );
    }

    #[test]
    fn fallback_matches_tmux() {
        assert_eq!(fallback(3, 1), b"SIXEL IMAGE (3x1)\r\n");
        let wide = String::from_utf8(fallback(20, 2)).expect("ascii");
        assert_eq!(wide, "SIXEL IMAGE (20x2)++\r\n++++++++++++++++++++\r\n");
    }
}
