//! Clock mode (`clock-mode`, prefix `t`), as tmux 3.7b's `window-clock.c`:
//! the pane shows the time in big digits until a key is typed. The time
//! shows as `clock-mode-style` says (12 or 24 hours, with or without
//! seconds) in `clock-mode-colour`; a pane too small for the digits shows
//! it as plain text, and one too small for that shows nothing.
//!
//! tmux redraws the clock from a timer on each second boundary while the
//! mode is up; grofe does the same, and only marks a client for drawing
//! when the text changed, so a clock without seconds draws once a minute.
//! With no pane in the mode there is no timer.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alacritty_terminal::vte::ansi::{Color, NamedColor};

use crate::cmd::{ClockMode, FormatVars, Resolve, expand_time};
use crate::protocol::ClientCtx;
use crate::server::dispatch::options;
use crate::server::input::{Input, InputHook, ROOT_TABLE, Routed};
use crate::server::pane::{CellFlags, SnapCell};
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::server::status;
use crate::server::{State, clients};
use crate::types::{ClientId, PaneId, Rect};

/// `pane_mode` for a pane showing the clock.
pub const MODE_NAME: &str = "clock-mode";

/// `window_clock_table`: each of `0` to `9`, `:`, `A`, `P` and `M` as five
/// rows of five cells.
const TABLE: [[&str; 5]; 14] = [
    ["#####", "#...#", "#...#", "#...#", "#####"],
    ["....#", "....#", "....#", "....#", "....#"],
    ["#####", "....#", "#####", "#....", "#####"],
    ["#####", "....#", "#####", "....#", "#####"],
    ["#...#", "#...#", "#####", "....#", "....#"],
    ["#####", "#....", "#####", "....#", "#####"],
    ["#####", "#....", "#####", "#...#", "#####"],
    ["#####", "....#", "....#", "....#", "....#"],
    ["#####", "#...#", "#####", "#...#", "#####"],
    ["#####", "#...#", "#####", "....#", "#####"],
    [".....", "..#..", ".....", "..#..", "....."],
    ["#####", "#...#", "#####", "#...#", "#...#"],
    ["#####", "#...#", "#####", "#....", "#...."],
    ["#...#", "##.##", "#.#.#", "#...#", "#...#"],
];

/// The panes in clock mode. Lives on `State.clocks`.
#[derive(Default)]
pub struct Clocks {
    /// Each pane in the mode, with the text last drawn for it.
    active: BTreeMap<PaneId, String>,
    /// Whether a timer is waiting for the next second.
    ticking: bool,
}

impl Clocks {
    pub fn is_active(&self, pane: PaneId) -> bool {
        self.active.contains_key(&pane)
    }

    /// Drops the panes that are gone.
    pub fn retain(&mut self, live: impl Fn(PaneId) -> bool) {
        self.active.retain(|p, _| live(*p));
    }
}

/// No format variables: the clock's `strftime` formats name none.
struct NoVars;

impl FormatVars for NoVars {
    fn get(&self, _: &str) -> Option<String> {
        None
    }
}

fn strftime(fmt: &str) -> String {
    expand_time(fmt, &NoVars)
}

/// The text the clock shows for `style` (0 and 2 are 12 hours, 1 and 3 24;
/// 2 and 3 have seconds), `time` being `strftime` at the current time:
/// tmux's `window_clock_draw_screen`.
fn text(style: u8, time: &dyn Fn(&str) -> String) -> String {
    match style {
        0 | 2 => {
            let fmt = if style == 2 { "%l:%M:%S " } else { "%l:%M " };
            let pm = time("%H").trim().parse::<u32>().is_ok_and(|h| h >= 12);
            format!("{}{}", time(fmt), if pm { "PM" } else { "AM" })
        }
        3 => time("%H:%M:%S"),
        _ => time("%H:%M"),
    }
}

/// `clock-mode-style` as tmux numbers its choices.
fn style_of(state: &State, pane: PaneId) -> u8 {
    let chain = options::chain(&state.model, pane);
    match state.model.options.lookup("clock-mode-style", &chain) {
        Some("12") => 0,
        Some("12-with-seconds") => 2,
        Some("24-with-seconds") => 3,
        _ => 1,
    }
}

fn now_text(state: &State, pane: PaneId) -> String {
    text(style_of(state, pane), &strftime)
}

/// `clock-mode [-t target-pane]`: puts the pane in clock mode. A pane
/// already showing the clock is left as it is.
pub fn clock_mode(state: &mut State, ctx: &ClientCtx, c: ClockMode) -> Result<String, String> {
    let pane = state.model.resolve_pane(&c.target, ctx)?;
    enter(state, pane);
    Ok(String::new())
}

/// Puts `pane` in clock mode and starts the timer if none runs.
pub fn enter(state: &mut State, pane: PaneId) {
    if state.clocks.is_active(pane) {
        return;
    }
    let drawn = now_text(state, pane);
    state.clocks.active.insert(pane, drawn);
    clients::mark_all_dirty(state);
    arm(state);
}

/// Takes `pane` out of clock mode; whether it was in it.
pub fn leave(state: &mut State, pane: PaneId) -> bool {
    let was = state.clocks.active.remove(&pane).is_some();
    if was {
        clients::mark_all_dirty(state);
    }
    was
}

/// Waits for the next second boundary, as `window_clock_start_timer`, then
/// redraws the clocks whose text changed and waits again. One timer serves
/// every clock, and none runs without one.
fn arm(state: &mut State) {
    if state.clocks.ticking || state.clocks.active.is_empty() {
        return;
    }
    // Without a runtime (unit tests) nothing ticks.
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    state.clocks.ticking = true;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let delay = Duration::from_nanos(1_000_000_000 - u64::from(nanos));
    let jobs = state.jobs.clone();
    rt.spawn(async move {
        tokio::time::sleep(delay).await;
        let job: crate::server::Job = Box::new(tick);
        let _ = jobs.send(job).await;
    });
}

fn tick(state: &mut State) {
    state.clocks.ticking = false;
    let model = &state.model;
    state.clocks.retain(|p| model.panes.contains_key(&p));
    let panes: Vec<PaneId> = state.clocks.active.keys().copied().collect();
    let mut changed = false;
    for pane in panes {
        let now = now_text(state, pane);
        if state.clocks.active.get(&pane) != Some(&now) {
            state.clocks.active.insert(pane, now);
            changed = true;
        }
    }
    if changed {
        clients::mark_all_dirty(state);
    }
    arm(state);
}

/// `pane_in_mode` and `pane_mode` for a pane showing the clock.
pub fn format_var(state: &State, pane: PaneId, name: &str) -> Option<String> {
    if !state.clocks.is_active(pane) {
        return None;
    }
    match name {
        // tmux counts the modes stacked on the pane, the clock on top.
        "pane_in_mode" => {
            let below = usize::from(state.copy.is_active(pane))
                + usize::from(crate::server::tree::is_over(state, pane));
            Some((1 + below).to_string())
        }
        "pane_mode" => Some(MODE_NAME.to_string()),
        _ => None,
    }
}

// ---- Keys --------------------------------------------------------------------

/// Routes a client's keys while its active pane shows the clock, as tmux
/// 3.7b's `server_client_key_callback` does: the prefix, and a key bound in
/// the root table, run as usual; any other key takes the pane out of the
/// mode and goes no further (`window_clock_key`).
pub struct ClockHook;

impl InputHook for ClockHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        _now: std::time::Instant,
    ) -> Option<Routed> {
        let model = &state.model;
        state.clocks.retain(|p| model.panes.contains_key(&p));
        let c = state.clients.get(&client)?;
        let session = model.sessions.get(&c.session)?;
        let pane = model.windows.get(&session.active)?.active;
        if !state.clocks.is_active(pane) || c.keys.table() != ROOT_TABLE {
            return None;
        }
        match input {
            Input::Key(key) => {
                if clients::key_settings(model, c.session).is_prefix(key)
                    || state.registry.key_tables.get(ROOT_TABLE, key).is_some()
                {
                    return None;
                }
                leave(state, pane);
                Some(Routed::Consumed)
            }
            Input::Paste { .. } => Some(Routed::Consumed),
            Input::Mouse(_) | Input::Focus(_) => None,
        }
    }
}

// ---- Drawing -------------------------------------------------------------------

/// Draws each visible pane showing the clock.
pub struct ClockOverlay;

impl Overlay for ClockOverlay {
    fn draw(&self, state: &State, client: ClientId, cx: &OverlayCtx, screen: &mut Screen) {
        if state.clocks.active.is_empty() {
            return;
        }
        let Some(w) = state
            .clients
            .get(&client)
            .and_then(|c| state.model.sessions.get(&c.session))
            .and_then(|s| state.model.windows.get(&s.active))
        else {
            return;
        };
        let rects: Vec<(PaneId, Rect)> = if w.zoomed {
            vec![(w.active, w.area())]
        } else {
            w.layout.compute(w.area())
        };
        for (pane, rect) in rects {
            if !state.clocks.is_active(pane) {
                continue;
            }
            let Some(&(_, clip)) = cx.panes.iter().find(|(id, _)| *id == pane) else {
                continue;
            };
            let chain = options::chain(&state.model, pane);
            let colour = state
                .model
                .options
                .lookup("clock-mode-colour", &chain)
                .unwrap_or("blue");
            let colour = status::parse_style(&format!("fg={colour}")).fg;
            let cells = layout(
                &now_text(state, pane),
                (usize::from(rect.cols), usize::from(rect.rows)),
                colour,
            );
            let inside = |v: i64, lo: u16, len: u16| {
                v >= i64::from(lo) && v < i64::from(lo) + i64::from(len)
            };
            for y in 0..usize::from(rect.rows) {
                for x in 0..usize::from(rect.cols) {
                    let col = i64::from(rect.x) + x as i64 - i64::from(cx.viewport.0);
                    let row = i64::from(rect.y) + y as i64 - i64::from(cx.viewport.1)
                        + i64::from(cx.window_top);
                    if !(inside(col, clip.x, clip.cols) && inside(row, clip.y, clip.rows)) {
                        continue;
                    }
                    let i = row as usize * usize::from(screen.cols) + col as usize;
                    if let Some(slot) = screen.cells.get_mut(i) {
                        *slot = cells.get(&(x, y)).cloned().unwrap_or_else(blank);
                    }
                }
            }
            if pane == w.active {
                screen.cursor = None;
            }
        }
    }
}

fn blank() -> SnapCell {
    SnapCell {
        c: ' ',
        fg: Color::Named(NamedColor::Foreground),
        bg: Color::Named(NamedColor::Background),
        flags: CellFlags::empty(),
        zerowidth: Vec::new(),
        underline_color: None,
        hyperlink: None,
    }
}

/// The cells of the clock in a `size` pane, by `(column, row)`: every
/// other cell is blank. A pane at least six cells wide for each character
/// and six rows high gets the big digits, centred; a smaller one the text
/// on its middle row; one that cannot hold the text, nothing.
fn layout(
    time: &str,
    (sx, sy): (usize, usize),
    colour: Color,
) -> BTreeMap<(usize, usize), SnapCell> {
    let mut cells = BTreeMap::new();
    let len = time.len();
    if sx < 6 * len || sy < 6 {
        if sx >= len && sy != 0 {
            let x = sx / 2 - len / 2;
            let y = sy / 2;
            for (i, c) in time.chars().enumerate() {
                let cell = SnapCell {
                    c,
                    fg: colour,
                    ..blank()
                };
                cells.insert((x + i, y), cell);
            }
        }
        return cells;
    }
    let mut x = sx / 2 - 3 * len;
    let y = sy / 2 - 3;
    for c in time.chars() {
        let index = match c {
            '0'..='9' => c as usize - '0' as usize,
            ':' => 10,
            'A' => 11,
            'P' => 12,
            'M' => 13,
            _ => {
                x += 6;
                continue;
            }
        };
        for (j, row) in TABLE[index].iter().enumerate() {
            for (i, bit) in row.chars().enumerate() {
                if bit == '#' {
                    let cell = SnapCell {
                        c: '#',
                        fg: colour,
                        bg: colour,
                        ..blank()
                    };
                    cells.insert((x + i, y + j), cell);
                }
            }
        }
        x += 6;
    }
    cells
}

/// Registers the clock's input hook and overlay (`server::register_builtins`).
pub fn register(state: &mut State) {
    crate::server::register_input_hook(state, Arc::new(ClockHook));
    crate::server::register_overlay(state, Arc::new(ClockOverlay));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(hour: &str, min: &str, sec: &str) -> impl Fn(&str) -> String {
        let (hour, min, sec) = (hour.to_string(), min.to_string(), sec.to_string());
        move |fmt: &str| {
            let h12 = match hour.parse::<u32>().unwrap_or(0) % 12 {
                0 => 12,
                h => h,
            };
            fmt.replace("%H", &hour)
                .replace("%l", &format!("{h12:>2}"))
                .replace("%M", &min)
                .replace("%S", &sec)
        }
    }

    #[test]
    fn the_style_picks_the_text() {
        let t = fixed("21", "07", "09");
        assert_eq!(text(0, &t), " 9:07 PM");
        assert_eq!(text(1, &t), "21:07");
        assert_eq!(text(2, &t), " 9:07:09 PM");
        assert_eq!(text(3, &t), "21:07:09");
        let t = fixed("00", "30", "00");
        assert_eq!(text(0, &t), "12:30 AM");
        let t = fixed("12", "00", "00");
        assert_eq!(text(0, &t), "12:00 PM");
    }

    fn rows(cells: &BTreeMap<(usize, usize), SnapCell>, (sx, sy): (usize, usize)) -> Vec<String> {
        (0..sy)
            .map(|y| {
                (0..sx)
                    .map(|x| cells.get(&(x, y)).map_or(' ', |c| c.c))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn big_digits_are_centred_as_tmux_centres_them() {
        let blue = Color::Named(NamedColor::Blue);
        // "10:07" is five characters: 30 columns, so a 40 by 7 pane starts
        // at column 5 of 40 (20 - 15) and row 0 (3 - 3).
        let got = rows(&layout("10:07", (40, 7), blue), (40, 7));
        assert_eq!(
            got,
            [
                "     ....# ##### ..... ##### #####",
                "     ....# #...# ..#.. #...# ....#",
                "     ....# #...# ..... #...# ....#",
                "     ....# #...# ..#.. #...# ....#",
                "     ....# ##### ..... ##### ....#",
                "",
                "",
            ]
            .map(|r: &str| r.replace('.', " ")),
        );
    }

    #[test]
    fn a_small_pane_shows_plain_text_and_a_tiny_one_nothing() {
        let blue = Color::Named(NamedColor::Blue);
        let got = rows(&layout("10:07", (20, 5), blue), (20, 5));
        assert_eq!(got, ["", "", "        10:07", "", ""]);
        // Wide enough for the digits but not tall: the text.
        let got = rows(&layout("10:07", (40, 5), blue), (40, 5));
        assert_eq!(got[2].trim(), "10:07");
        assert!(layout("10:07", (4, 9), blue).is_empty());
        assert!(layout("10:07", (40, 0), blue).is_empty());
    }

    #[test]
    fn digits_are_drawn_in_the_colour_on_the_colour() {
        let blue = Color::Named(NamedColor::Blue);
        let cells = layout("1", (6, 6), blue);
        let cell = cells.get(&(4, 0)).expect("a lit cell");
        assert_eq!((cell.c, cell.fg, cell.bg), ('#', blue, blue));
        assert!(!cells.contains_key(&(3, 0)));
    }
}
