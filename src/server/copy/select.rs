//! Selection in copy mode (spec §4.11, TM.2): tmux 3.7b's
//! `window-copy.c` selection code, ported, checked against the `tm_2`
//! goldens.
//!
//! A selection is an anchor (`selx`, `sely`, [`Mode::sel_start`]) and an
//! end (`endselx`, `endsely`, [`Mode::sel_end`]) on rows of the copy, one
//! of which follows the cursor ([`Drag`]). Extraction ([`get_selection`])
//! runs over logical lines: a soft-wrapped row joins the next with nothing
//! between, a hard newline stays, trailing blanks go, a wide character and
//! its combining marks are copied whole, a tab comes back as a tab, and a
//! rectangle copies the same columns from each row. A copy is handed to
//! the server through [`Sel::copied`] and stored by [`store`].
//!
//! As in tmux, a resize clears the selection (`window_copy_size_changed`).
// ponytail: the `prefix` argument of the copy commands is accepted and
// ignored (buffers are always named `buffer<N>`), and `set-clipboard`
// (OSC 52) is TM.5's.

use super::{Action, Ctx, LineSel, Mode, WHITESPACE};
use crate::server::State;
use crate::server::buffers;

/// Which end of the selection follows the cursor (tmux's `cursordrag`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Drag {
    #[default]
    None,
    /// The end (`CURSORDRAG_ENDSEL`).
    End,
    /// The start (`CURSORDRAG_SEL`).
    Start,
}

/// What a selection extends by as the cursor moves (tmux's `selflag`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Unit {
    #[default]
    Char,
    Word,
    Line,
}

/// A copy for the server to store (tmux's `window_copy_copy_buffer` and
/// `window_copy_append_selection`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copied {
    pub data: Vec<u8>,
    /// Appends to the newest automatic buffer (`append-selection`).
    pub append: bool,
    /// Stores a buffer (no `-P`).
    pub set_paste: bool,
    /// Also goes to the clipboard (no `-C`); TM.5's.
    pub set_clip: bool,
}

/// The selection state beyond [`Mode`]'s own fields (tmux's
/// `window_copy_mode_data`).
#[derive(Debug, Default)]
pub struct Sel {
    pub drag: Drag,
    pub unit: Unit,
    /// Where a word or line selection started (`dx`, `dy`).
    dx: usize,
    dy: usize,
    /// The first word's or line's start and end (`selrx`, `selry`,
    /// `endselrx`, `endselry`).
    selrx: usize,
    selry: usize,
    endselrx: usize,
    endselry: usize,
    /// `word-separators` when the word selection began.
    separators: String,
    /// The next selection update keeps a word or line selection's ends
    /// (tmux's `no_reset`).
    no_reset: bool,
    /// Copies made by the last command, for the server to store.
    pub copied: Vec<Copied>,
    /// Why the last command's copy failed, for the server to report.
    pub error: Option<String>,
}

// ---- Selection state -------------------------------------------------------

/// tmux's `window_copy_update_selection`: the selection's moving end
/// follows the cursor.
pub fn update_selection(mode: &mut Mode) {
    if !mode.selecting && mode.line_sel == LineSel::None {
        return;
    }
    let no_reset = mode.sel.no_reset;
    set_selection(mode, no_reset);
}

/// Runs `f` with selection updates keeping a word or line selection's
/// ends, as tmux passes `no_reset`.
fn no_reset(mode: &mut Mode, f: impl FnOnce(&mut Mode)) {
    mode.sel.no_reset = true;
    f(mode);
    mode.sel.no_reset = false;
}

/// tmux's `window_copy_set_selection`, without the drawing.
fn set_selection(mode: &mut Mode, no_reset: bool) {
    synchronize_cursor(mode, no_reset);
    let top = mode.grid.hsize - mode.oy;
    let rel = |y: usize| {
        if y < top {
            -1
        } else if y >= top + mode.sy() {
            1
        } else {
            0
        }
    };
    let (start, end) = (rel(mode.sel_start.1), rel(mode.sel_end.1));
    // Both ends off the view on one side: tmux hides the selection.
    if start == end && start != 0 {
        return;
    }
    mode.selecting = true;
}

fn synchronize_cursor(mode: &mut Mode, no_reset: bool) {
    match mode.sel.drag {
        Drag::End => synchronize_cursor_end(mode, false, no_reset),
        Drag::Start => synchronize_cursor_end(mode, true, no_reset),
        Drag::None => {}
    }
}

/// tmux's `window_copy_synchronize_cursor_end`.
fn synchronize_cursor_end(mode: &mut Mode, mut begin: bool, no_reset: bool) {
    let (mut xx, mut yy) = (mode.cx, mode.py());
    match mode.sel.unit {
        Unit::Word if !no_reset => {
            begin = false;
            let (dx, dy) = (mode.sel.dx, mode.sel.dy);
            let seps = mode.sel.separators.clone();
            if dy > yy || (dy == yy && dx > xx) {
                // Right to left.
                let mut r = mode.reader(xx, yy);
                r.cursor_previous_word(&seps, false, true);
                (xx, yy) = (r.cx, r.cy);
                begin = true;
                mode.sel_end = (mode.sel.endselrx, mode.sel.endselry);
            } else {
                if xx >= mode.find_length(yy) || mode.grid.in_set(xx + 1, yy, WHITESPACE) == 0 {
                    (xx, yy) = next_word_end_pos(mode, &seps);
                }
                mode.sel_start = (mode.sel.selrx, mode.sel.selry);
            }
        }
        Unit::Line if !no_reset => {
            begin = false;
            if mode.sel.dy > yy {
                xx = 0;
                begin = true;
                mode.sel_end = (mode.sel.endselrx, mode.sel.endselry);
            } else {
                yy = yy.max(mode.sel.endselry);
                xx = mode.find_length(yy);
                mode.sel_start = (mode.sel.selrx, mode.sel.selry);
            }
        }
        _ => {}
    }
    if begin {
        mode.sel_start = (xx, yy);
    } else {
        mode.sel_end = (xx, yy);
    }
}

/// tmux's `window_copy_cursor_next_word_end_pos`.
fn next_word_end_pos(mode: &Mode, separators: &str) -> (usize, usize) {
    let mut r = mode.reader(mode.cx, mode.py());
    if mode.vi {
        if r.in_set(WHITESPACE) == 0 {
            r.cursor_right(false, false, false);
        }
        r.cursor_next_word_end(separators);
        r.cursor_left(true);
    } else {
        r.cursor_next_word_end(separators);
    }
    (r.cx, r.cy)
}

/// tmux's `window_copy_start_selection`.
fn start_selection(mode: &mut Mode) {
    let py = mode.py();
    mode.sel_start = (mode.cx, py);
    mode.sel_end = (mode.cx, py);
    mode.sel.drag = Drag::End;
    set_selection(mode, false);
}

/// tmux's `window_copy_other_end`.
pub fn other_end(mode: &mut Mode) {
    if !mode.selecting && mode.line_sel == LineSel::None {
        return;
    }
    mode.line_sel = match mode.line_sel {
        LineSel::LeftRight => LineSel::RightLeft,
        LineSel::RightLeft => LineSel::LeftRight,
        LineSel::None => LineSel::None,
    };
    mode.sel.drag = match mode.sel.drag {
        Drag::None | Drag::Start => Drag::End,
        Drag::End => Drag::Start,
    };
    let (selx, sely) = if mode.sel.drag == Drag::Start {
        mode.sel_start
    } else {
        mode.sel_end
    };
    let (cy, yy) = (mode.cy, mode.py());
    let hsize = mode.grid.hsize;
    let sy = mode.sy();
    mode.cx = selx;
    if sely < hsize - mode.oy {
        mode.oy = hsize - sely;
        mode.cy = 0;
    } else if sely > hsize - mode.oy + sy {
        mode.oy = hsize + sy - 1 - sely;
        mode.cy = sy - 1;
    } else {
        mode.cy = cy + sely - yy;
    }
    let limit = mode.cursor_limit(mode.py(), mode.rect);
    mode.cx = mode.cx.min(limit);
    if mode.selecting || mode.line_sel != LineSel::None {
        set_selection(mode, true);
    }
}

/// tmux's `window_copy_clear_selection`.
pub fn clear_selection(mode: &mut Mode) {
    mode.selecting = false;
    mode.sel.drag = Drag::None;
    mode.line_sel = LineSel::None;
    mode.sel.unit = Unit::Char;
    let px = mode.cursor_limit(mode.py(), mode.rect);
    if mode.cx > px {
        mode.update_cursor(px, mode.cy);
    }
}

/// tmux's `window_copy_rectangle_set`.
fn rectangle_set(mode: &mut Mode, rect: bool) {
    mode.rect = rect;
    let px = mode.cursor_limit(mode.py(), rect);
    if mode.cx > px {
        mode.update_cursor(px, mode.cy);
    }
    update_selection(mode);
}

// ---- Extraction ------------------------------------------------------------

/// tmux's `window_copy_get_selection`: the selected text, or with no
/// selection the search match at the cursor (TM.3); `None` when there is
/// neither or it is empty, `Err` when it is larger than a buffer may be
/// ([`buffers::MAX_BUFFER_BYTES`]).
pub fn get_selection(mode: &Mode) -> Result<Option<Vec<u8>>, String> {
    extract(mode, buffers::MAX_BUFFER_BYTES)
}

fn too_large(cap: usize) -> String {
    format!("selection too large (limit {cap} bytes)")
}

/// [`get_selection`], stopping once the text passes `cap` bytes.
fn extract(mode: &Mode, cap: usize) -> Result<Option<Vec<u8>>, String> {
    if !mode.selecting && mode.line_sel == LineSel::None {
        // The match is on view, so far smaller than any cap.
        return Ok(super::search::match_at_cursor(mode).map(String::into_bytes));
    }
    let (xx, yy) = mode.sel_end;
    let (selx, sely) = mode.sel_start;
    let ((sx, sy), (mut ex, ey)) = if yy < sely || (yy == sely && xx < selx) {
        ((xx, yy), (selx, sely))
    } else {
        ((selx, sely), (xx, yy))
    };
    let ey_last = mode.find_length(ey);
    ex = ex.min(ey_last);
    let width = mode.grid.sx;
    let vi = mode.vi;
    let (firstsx, restsx, lastex, restex);
    if mode.rect {
        // The column the cursor is in counts only on its own side.
        let anchor = if mode.sel.drag == Drag::End {
            mode.sel_start.0
        } else {
            mode.sel_end.0
        };
        if anchor < mode.cx {
            let end = if vi { mode.cx + 1 } else { mode.cx };
            (lastex, restex) = (end, end);
            (firstsx, restsx) = (anchor, anchor);
        } else {
            (lastex, restex) = (anchor + 1, anchor + 1);
            (firstsx, restsx) = (mode.cx, mode.cx);
        }
    } else {
        lastex = if vi { ex + 1 } else { ex };
        restex = width;
        firstsx = sx;
        restsx = 0;
    }
    let mut buf = Vec::new();
    for y in sy..=ey {
        let from = if y == sy { firstsx } else { restsx };
        let to = if y == ey { lastex } else { restex };
        copy_line(mode, &mut buf, y, from, to);
        // One byte over: the final newline may still go.
        if buf.len() > cap + 1 {
            return Err(too_large(cap));
        }
    }
    if buf.is_empty() {
        return Ok(None);
    }
    // The final newline goes, unless a vi selection ran past the end.
    if (!vi || lastex <= ey_last) && (!mode.grid.wrapped(ey) || lastex != ey_last) {
        buf.pop();
    }
    if buf.len() > cap {
        return Err(too_large(cap));
    }
    Ok(Some(buf))
}

/// tmux's `window_copy_copy_line`: columns `sx..ex` of row `y`, and a
/// newline unless the row runs on into the next.
fn copy_line(mode: &Mode, buf: &mut Vec<u8>, y: usize, mut sx: usize, mut ex: usize) {
    if sx > ex {
        return;
    }
    let wrapped = mode.grid.wrapped(y);
    // A wrapped row holds all its cells, blanks included (tmux's
    // `cellsize`), even the one a wide character too wide to fit left.
    let xx = if wrapped {
        mode.grid.sx
    } else {
        mode.find_length(y)
    };
    ex = ex.min(xx);
    sx = sx.min(xx);
    let mut utf8 = [0; 4];
    for x in sx..ex {
        let gc = mode.grid.get(x, y);
        if gc.padding {
            continue;
        }
        if gc.tab > 0 {
            buf.push(b'\t');
            continue;
        }
        // One ASCII byte with nothing combined, most cells.
        if gc.single {
            buf.push(gc.c as u8);
            continue;
        }
        buf.extend_from_slice(gc.c.encode_utf8(&mut utf8).as_bytes());
        if let Some((_, look)) = mode.grid.look(x, y) {
            for c in &look.zerowidth {
                buf.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
            }
        }
    }
    if !wrapped || ex != xx {
        buf.push(b'\n');
    }
}

/// Whether copy row `y`, column `x` is drawn selected (tmux's
/// `screen_check_selection`, in the copy's rows).
pub fn highlighted(mode: &Mode, x: usize, y: usize) -> bool {
    if !mode.selecting {
        return false;
    }
    let (s, e) = (mode.sel_start, mode.sel_end);
    if mode.rect {
        let (top, bottom) = (s.1.min(e.1), s.1.max(e.1));
        let (left, right) = (s.0.min(e.0), s.0.max(e.0));
        return (top..=bottom).contains(&y) && (left..=right).contains(&x);
    }
    let (a, b) = if (e.1, e.0) < (s.1, s.0) {
        (e, s)
    } else {
        (s, e)
    };
    (a.1, a.0) <= (y, x) && (y, x) <= (b.1, b.0)
}

// ---- Commands --------------------------------------------------------------

/// A copy-mode command's flags (any of `allowed`) and positionals, as
/// tmux's `args_parse`; `None` when they do not fit.
fn parse<'a>(args: &'a [String], allowed: &str, max: usize) -> Option<(String, &'a [String])> {
    let mut flags = String::new();
    let mut rest = args;
    while let Some(a) = rest.first() {
        if a == "--" {
            rest = &rest[1..];
            break;
        }
        let Some(f) = a.strip_prefix('-').filter(|f| !f.is_empty()) else {
            break;
        };
        if !f.chars().all(|c| allowed.contains(c)) {
            return None;
        }
        flags.push_str(f);
        rest = &rest[1..];
    }
    (rest.len() <= max).then_some((flags, rest))
}

/// Queues the selection as a copy.
fn copy_selection(mode: &mut Mode, flags: &str) {
    queue(mode, false, !flags.contains('P'), !flags.contains('C'));
}

fn append_selection(mode: &mut Mode) {
    queue(mode, true, true, true);
}

fn queue(mode: &mut Mode, append: bool, set_paste: bool, set_clip: bool) {
    match get_selection(mode) {
        Ok(Some(data)) => mode.sel.copied.push(Copied {
            data,
            append,
            set_paste,
            set_clip,
        }),
        Ok(None) => {}
        Err(e) => mode.sel.error = Some(e),
    }
}

/// tmux's `window_copy_cmd_select_line`.
fn select_line(mode: &mut Mode, np: u32) {
    mode.line_sel = LineSel::LeftRight;
    mode.rect = false;
    mode.sel.unit = Unit::Line;
    mode.sel.dx = mode.cx;
    mode.sel.dy = mode.py();
    mode.cursor_start_of_line();
    mode.sel.selrx = mode.cx;
    mode.sel.selry = mode.py();
    mode.sel.endselry = mode.sel.selry;
    start_selection(mode);
    mode.cursor_end_of_line();
    mode.sel.endselry = mode.py();
    mode.sel.endselrx = mode.find_length(mode.sel.endselry);
    for _ in 1..np {
        mode.cursor_down(false);
        mode.cursor_end_of_line();
    }
}

/// tmux's `window_copy_cmd_select_word`.
fn select_word(mode: &mut Mode, separators: &str) {
    mode.line_sel = LineSel::LeftRight;
    mode.rect = false;
    mode.sel.unit = Unit::Word;
    mode.sel.dx = mode.cx;
    mode.sel.dy = mode.py();
    mode.sel.separators = separators.to_string();
    mode.cursor_previous_word(separators, false);
    let (px, py) = (mode.cx, mode.py());
    mode.sel.selrx = px;
    mode.sel.selry = py;
    start_selection(mode);
    // A word of one character.
    let (mut nextx, mut nexty) = (px + 1, py);
    if mode.grid.wrapped(nexty) && nextx > mode.grid.sx.saturating_sub(1) {
        nextx = 0;
        nexty += 1;
    }
    if px >= mode.find_length(py) || mode.grid.in_set(nextx, nexty, WHITESPACE) == 0 {
        no_reset(mode, |m| m.cursor_next_word_end(separators));
    } else {
        mode.update_cursor(px, mode.cy);
        no_reset(mode, update_selection);
    }
    mode.sel.endselrx = mode.cx;
    mode.sel.endselry = mode.py();
    if mode.sel.dy > mode.sel.endselry {
        mode.sel.dy = mode.sel.endselry;
        mode.sel.dx = mode.sel.endselrx;
    } else if mode.sel.dx > mode.sel.endselrx {
        mode.sel.dx = mode.sel.endselrx;
    }
}

/// tmux's `window_copy_do_copy_end_of_line` (`whole` false) and
/// `window_copy_do_copy_line` (`whole` true), without the pipe: copies
/// from the cursor, or the start of its line, to the end of the line `np`
/// lines down, then puts the cursor back.
fn copy_lines(mode: &mut Mode, np: u32, flags: &str, cancel: bool, whole: bool) -> Action {
    let (ocx, ocy, ooy) = (mode.cx, mode.cy, mode.oy);
    if whole {
        mode.sel.unit = Unit::Char;
        mode.cursor_start_of_line();
    }
    start_selection(mode);
    for _ in 1..np {
        mode.cursor_down(false);
    }
    mode.cursor_end_of_line();
    copy_selection(mode, flags);
    if cancel {
        return Action::Cancel;
    }
    clear_selection(mode);
    mode.cx = ocx;
    mode.cy = ocy;
    mode.oy = ooy;
    Action::Redraw
}

/// A selection command, or `None` if `name` is not one (or its arguments
/// do not fit, which tmux ignores).
pub fn command(mode: &mut Mode, name: &str, args: &[String], ctx: &Ctx<'_>) -> Option<Action> {
    use Action::{Cancel, Nothing, Redraw};
    let np = mode.prefix;
    let none = || parse(args, "", 0).map(|_| ());
    let action = match name {
        "begin-selection" => {
            none()?;
            mode.line_sel = LineSel::None;
            mode.sel.unit = Unit::Char;
            start_selection(mode);
            Redraw
        }
        "clear-selection" => {
            none()?;
            clear_selection(mode);
            Redraw
        }
        "select-line" => {
            none()?;
            select_line(mode, np);
            Redraw
        }
        "select-word" => {
            none()?;
            select_word(mode, ctx.separators);
            Redraw
        }
        "other-end" => {
            none()?;
            mode.sel.unit = Unit::Char;
            if !np.is_multiple_of(2) {
                other_end(mode);
            }
            Nothing
        }
        "rectangle-toggle" | "rectangle-on" | "rectangle-off" => {
            none()?;
            mode.line_sel = LineSel::None;
            let rect = match name {
                "rectangle-on" => true,
                "rectangle-off" => false,
                _ => !mode.rect,
            };
            rectangle_set(mode, rect);
            Nothing
        }
        "stop-selection" => {
            none()?;
            mode.sel.drag = Drag::None;
            mode.line_sel = LineSel::None;
            mode.sel.unit = Unit::Char;
            Nothing
        }
        "copy-selection" | "copy-selection-no-clear" | "copy-selection-and-cancel" => {
            let (flags, _) = parse(args, "CP", 1)?;
            copy_selection(mode, &flags);
            match name {
                "copy-selection-no-clear" => Nothing,
                "copy-selection" => {
                    clear_selection(mode);
                    Redraw
                }
                _ => {
                    clear_selection(mode);
                    Cancel
                }
            }
        }
        "copy-end-of-line"
        | "copy-end-of-line-and-cancel"
        | "copy-line"
        | "copy-line-and-cancel" => {
            let (flags, _) = parse(args, "CP", 1)?;
            let cancel = name.ends_with("-and-cancel");
            copy_lines(mode, np, &flags, cancel, name.starts_with("copy-line"))
        }
        "append-selection" | "append-selection-and-cancel" => {
            none()?;
            append_selection(mode);
            clear_selection(mode);
            if name == "append-selection" {
                Redraw
            } else {
                Cancel
            }
        }
        _ => return None,
    };
    Some(action)
}

/// Stores the copies a command made: a new automatic buffer, or appended
/// to the newest automatic one, which tmux then stores by its name
/// (making it a named buffer). `Err` with the first that failed.
pub fn store(state: &mut State, copied: Vec<Copied>) -> Result<(), String> {
    let mut result = Ok(());
    for c in copied {
        let stored = if c.append
            && let Some((name, mut data)) = state
                .buffers
                .top()
                .map(|b| (b.name.clone(), b.data.clone()))
        {
            data.extend_from_slice(&c.data);
            state.buffers.set(&name, data)
        } else if c.set_paste || c.append {
            let limit = buffers::limit(state);
            state.buffers.add(c.data, limit)
        } else {
            Ok(())
        };
        if result.is_ok() {
            result = stored;
        }
    }
    result
}

/// The selection's format variables (tmux's `window_copy_formats`);
/// `resized` when the pane has a resize the mode has not yet applied,
/// which clears the selection.
pub fn format_var(mode: &Mode, name: &str, resized: bool) -> Option<String> {
    let flag = |b: bool| if b { "1" } else { "0" }.to_string();
    let sel = mode.selecting && !resized;
    Some(match name {
        "rectangle_toggle" => flag(mode.rect),
        "selection_active" => flag(sel && mode.sel.drag != Drag::None),
        "selection_present" => flag(sel && mode.sel_start != mode.sel_end),
        "selection_mode" if resized => "char".to_string(),
        "selection_mode" => match mode.sel.unit {
            Unit::Char => "char",
            Unit::Word => "word",
            Unit::Line => "line",
        }
        .to_string(),
        "selection_start_x" if sel => mode.sel_start.0.to_string(),
        "selection_start_y" if sel => mode.sel_start.1.to_string(),
        "selection_end_x" if sel => mode.sel_end.0.to_string(),
        "selection_end_y" if sel => mode.sel_end.1.to_string(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::{Ctx, Grid, Mode, run};
    use super::*;

    const SEPS: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~";

    fn mode(sx: usize, rows: &[&str], cursor: (usize, usize)) -> Mode {
        Mode::new(Grid::from_text(sx, 0, rows), cursor, true)
    }

    fn x(m: &mut Mode, words: &[&str]) -> Action {
        let args: Vec<String> = words.iter().map(|s| s.to_string()).collect();
        run(m, &args, &Ctx { separators: SEPS })
    }

    fn copied(m: &mut Mode) -> Vec<String> {
        std::mem::take(&mut m.sel.copied)
            .into_iter()
            .map(|c| String::from_utf8(c.data).unwrap())
            .collect()
    }

    #[test]
    fn a_stream_selection_trims_and_keeps_hard_newlines() {
        let mut m = mode(10, &["ab   ", "cd", ""], (0, 0));
        x(&mut m, &["begin-selection"]);
        x(&mut m, &["cursor-down"]);
        x(&mut m, &["end-of-line"]);
        x(&mut m, &["copy-selection"]);
        assert_eq!(copied(&mut m), ["ab\ncd"]);
        assert!(!m.selecting);
    }

    #[test]
    fn a_line_selection_keeps_its_newline() {
        let mut m = mode(10, &["ab", "cd", ""], (1, 0));
        x(&mut m, &["select-line"]);
        x(&mut m, &["copy-selection-no-clear"]);
        assert_eq!(copied(&mut m), ["ab\n"]);
        assert!(m.selecting);
    }

    #[test]
    fn select_word_stops_at_separators() {
        let mut m = mode(30, &["see foo/bar-baz.rs", ""], (9, 0));
        x(&mut m, &["select-word"]);
        x(&mut m, &["copy-selection"]);
        assert_eq!(copied(&mut m), ["bar"]);
    }

    #[test]
    fn flags_and_arguments_follow_args_parse() {
        let mut m = mode(10, &["ab", ""], (0, 0));
        x(&mut m, &["begin-selection"]);
        x(&mut m, &["copy-selection", "-x"]);
        x(&mut m, &["copy-selection", "a", "b"]);
        assert!(copied(&mut m).is_empty());
        assert!(m.selecting);
        x(&mut m, &["copy-selection", "-P"]);
        let c = std::mem::take(&mut m.sel.copied);
        assert_eq!(c.len(), 1);
        assert!(!c[0].set_paste);
    }

    /// A copy stops once it passes the cap and says why; one that fits
    /// exactly, its final newline dropped, is kept.
    #[test]
    fn a_copy_past_the_cap_is_refused() {
        let mut m = mode(10, &["abcd", "efgh", ""], (0, 0));
        x(&mut m, &["begin-selection"]);
        x(&mut m, &["cursor-down"]);
        x(&mut m, &["end-of-line"]);
        assert_eq!(extract(&m, 9), Ok(Some(b"abcd\nefgh".to_vec())));
        assert_eq!(extract(&m, 8), Err(too_large(8)));
        x(&mut m, &["copy-selection"]);
        assert_eq!(copied(&mut m), ["abcd\nefgh"]);
        assert_eq!(m.sel.error, None);
    }

    #[test]
    fn copy_line_copies_the_whole_line_and_puts_the_cursor_back() {
        let mut m = mode(10, &["abcd", "efgh", ""], (2, 0));
        m.prefix = 2;
        x(&mut m, &["copy-line"]);
        assert_eq!(copied(&mut m), ["abcd\nefgh"]);
        assert_eq!((m.cx, m.cy, m.selecting), (2, 0, false));
        x(&mut m, &["begin-selection"]);
        x(&mut m, &["cursor-right"]);
        x(&mut m, &["stop-selection"]);
        x(&mut m, &["cursor-right"]);
        assert_eq!(m.sel_end, (3, 0));
        x(&mut m, &["rectangle-on"]);
        x(&mut m, &["rectangle-on"]);
        assert!(m.rect);
        x(&mut m, &["rectangle-off"]);
        assert!(!m.rect);
    }

    #[test]
    fn other_end_swaps_the_moving_end() {
        let mut m = mode(10, &["abcdef", ""], (1, 0));
        x(&mut m, &["begin-selection"]);
        x(&mut m, &["cursor-right"]);
        x(&mut m, &["cursor-right"]);
        x(&mut m, &["other-end"]);
        assert_eq!(m.cx, 1);
        x(&mut m, &["cursor-left"]);
        assert_eq!((m.sel_start, m.sel_end), ((0, 0), (3, 0)));
        x(&mut m, &["copy-selection"]);
        assert_eq!(copied(&mut m), ["abcd"]);
    }
}
