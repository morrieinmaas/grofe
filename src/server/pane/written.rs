//! Which cells of a line the program wrote, for `capture-pane -J`.
//!
//! tmux keeps each grid line's used width (`cellused`), and `-J` prints a
//! line up to it with its blanks: `printf 'ab   '` captures as `ab   `.
//! alacritty's grid cannot tell a written blank from an empty cell, so the
//! parser drives the emulator through [`Written`], which marks every cell
//! `input` writes with [`WRITTEN`], a flag bit alacritty does not use. A
//! line's used width is then its last marked cell's end, and the marks
//! move with the cells (scrolling, reflow).
//!
//! The rest follows tmux 3.7b's `grid_clear` and `grid_move_cells`
//! (recorded): a clear that covers a whole line empties it, a partial one
//! keeps the width, a delete (`DCH`) widens it to `columns - n` and an
//! insert (`ICH`) to the whole line.

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::vte::ansi::cursor_icon::CursorIcon;
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Rgb,
    ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};

use super::CellFlags;

/// A cell the program wrote. Bit 15 is free in alacritty 0.26's flags;
/// nothing but `capture-pane -J` reads it.
pub const WRITTEN: CellFlags = CellFlags::from_bits_retain(1 << 15);

/// The used width of `row`: its last written cell's end, 0 if none.
pub fn used(row: &alacritty_terminal::grid::Row<Cell>) -> usize {
    (0..row.len())
        .rev()
        .find(|&c| row[Column(c)].flags.contains(WRITTEN))
        .map_or(0, |c| c + 1)
}

/// Each line's used width, oldest first: a line is the rows a wrap joins,
/// its width counted across them.
fn line_widths(grid: &Grid<Cell>) -> Vec<(usize, Vec<Line>)> {
    let cols = grid.columns();
    let top = -(grid.history_size() as i32);
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    let mut width = 0;
    for l in top..grid.screen_lines() as i32 {
        let row = &grid[Line(l)];
        let used = used(row);
        if used > 0 {
            width = rows.len() * cols + used;
        }
        rows.push(Line(l));
        if !row[Column(cols - 1)].flags.contains(CellFlags::WRAPLINE) {
            lines.push((width, std::mem::take(&mut rows)));
            width = 0;
        }
    }
    if !rows.is_empty() {
        lines.push((width, rows));
    }
    lines
}

/// Used widths a resize kept for lines too short to hold them: a line
/// shrunk to fewer rows than its width needs gets it back when it grows.
pub struct Kept {
    /// `(line, width)`, lines counted from the oldest.
    lost: Vec<(usize, usize)>,
    history_limit: usize,
}

impl Kept {
    pub fn new(history_limit: usize) -> Kept {
        Kept {
            lost: Vec::new(),
            history_limit,
        }
    }

    /// Resizes `term`, keeping each line's used width: alacritty's reflow
    /// drops trailing blanks it counts as empty, written ones too, where
    /// tmux's reflow keeps the width (recorded: `abcdef  ` shrunk to 4
    /// columns and grown back still captures with its blanks). Lines match
    /// from the bottom, the end a reflow keeps.
    pub fn resize<L: EventListener>(&mut self, term: &mut Term<L>, size: impl Dimensions) {
        let mut before = line_widths(term.grid());
        // A full history may have dropped lines since, so the lines no
        // longer count from the same one.
        if term.grid().history_size() < self.history_limit {
            for &(i, width) in &self.lost {
                if let Some(line) = before.get_mut(i) {
                    line.0 = line.0.max(width);
                }
            }
        }
        self.lost.clear();
        term.resize(size);
        let cols = term.columns();
        let after = line_widths(term.grid());
        let shift = after.len() as isize - before.len() as isize;
        let grid = term.grid_mut();
        for (i, (width, _)) in before.iter().enumerate() {
            let Ok(j) = usize::try_from(i as isize + shift) else {
                continue;
            };
            let Some((now, rows)) = after.get(j) else {
                continue;
            };
            if *width == 0 || width <= now {
                continue;
            }
            let (row, col) = ((width - 1) / cols, (width - 1) % cols);
            match rows.get(row) {
                Some(&line) => grid[line][Column(col)].flags.insert(WRITTEN),
                None => self.lost.push((j, *width)),
            }
        }
    }
}

/// The emulator as the parser's handler: [`Term`], marking written cells.
pub struct Written<'a, L: EventListener>(pub &'a mut Term<L>);

impl<L: EventListener> Written<'_, L> {
    fn cursor(&self) -> (usize, usize) {
        let point = self.0.grid().cursor.point;
        (point.column.0, self.0.columns())
    }

    fn line_used(&self) -> usize {
        let grid = self.0.grid();
        used(&grid[grid.cursor.point.line])
    }

    /// Makes the cursor line's used width at least `width`.
    fn widen(&mut self, width: usize) {
        if width == 0 || width <= self.line_used() {
            return;
        }
        let grid = self.0.grid_mut();
        let line = grid.cursor.point.line;
        grid[line][Column(width - 1)].flags.insert(WRITTEN);
    }

    /// Runs `op`, which clears the cursor line's columns `from..to`: a
    /// clear of the whole line empties it, any other keeps its width.
    fn clear(&mut self, from: usize, to: usize, op: impl FnOnce(&mut Term<L>)) {
        let width = self.line_used();
        op(self.0);
        if from > 0 || to < self.0.columns() {
            self.widen(width);
        }
    }
}

macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*);)*) => {
        $(fn $name(&mut self, $($arg: $ty),*) {
            Handler::$name(&mut *self.0, $($arg),*)
        })*
    };
}

impl<L: EventListener> Handler for Written<'_, L> {
    fn input(&mut self, c: char) {
        self.0.grid_mut().cursor.template.flags.insert(WRITTEN);
        Handler::input(&mut *self.0, c);
        self.0.grid_mut().cursor.template.flags.remove(WRITTEN);
        // Insert mode (`IRM`) shifts the line right as `ICH` does, which
        // widens it to the whole line (tmux's `grid_move_cells`, recorded).
        if self.0.mode().contains(TermMode::INSERT) {
            let cols = self.0.columns();
            self.widen(cols);
        }
    }

    fn insert_blank(&mut self, n: usize) {
        let (col, cols) = self.cursor();
        if n.max(1) < cols - col {
            Handler::insert_blank(&mut *self.0, n);
            self.widen(cols);
        } else {
            self.clear(col, cols, |t| Handler::insert_blank(t, n));
        }
    }

    fn delete_chars(&mut self, n: usize) {
        let (col, cols) = self.cursor();
        let n = n.max(1);
        if n < cols - col {
            let width = self.line_used();
            Handler::delete_chars(&mut *self.0, n);
            self.widen(width.max(cols - n));
        } else {
            self.clear(col, cols, |t| Handler::delete_chars(t, n));
        }
    }

    fn erase_chars(&mut self, n: usize) {
        let (col, _) = self.cursor();
        let to = col.saturating_add(n.max(1));
        self.clear(col, to, |t| Handler::erase_chars(t, n));
    }

    fn clear_line(&mut self, mode: LineClearMode) {
        let (col, cols) = self.cursor();
        let (from, to) = match mode {
            LineClearMode::Right => (col, cols),
            LineClearMode::Left => (0, col + 1),
            LineClearMode::All => (0, cols),
        };
        self.clear(from, to, |t| Handler::clear_line(t, mode));
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        let (col, cols) = self.cursor();
        let (from, to) = match mode {
            ClearMode::Below => (col, cols),
            ClearMode::Above => (0, col + 1),
            ClearMode::All | ClearMode::Saved => (0, cols),
        };
        self.clear(from, to, |t| Handler::clear_screen(t, mode));
    }

    forward! {
        set_title(title: Option<String>);
        set_cursor_style(style: Option<CursorStyle>);
        set_cursor_shape(shape: CursorShape);
        goto(line: i32, col: usize);
        goto_line(line: i32);
        goto_col(col: usize);
        move_up(n: usize);
        move_down(n: usize);
        identify_terminal(intermediate: Option<char>);
        device_status(n: usize);
        move_forward(col: usize);
        move_backward(col: usize);
        move_down_and_cr(row: usize);
        move_up_and_cr(row: usize);
        put_tab(count: u16);
        backspace();
        carriage_return();
        linefeed();
        bell();
        substitute();
        newline();
        set_horizontal_tabstop();
        scroll_up(n: usize);
        scroll_down(n: usize);
        insert_blank_lines(n: usize);
        delete_lines(n: usize);
        move_backward_tabs(count: u16);
        move_forward_tabs(count: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_tabs(mode: TabulationClearMode);
        set_tabs(interval: u16);
        reset_state();
        reverse_index();
        terminal_attribute(attr: Attr);
        set_mode(mode: Mode);
        unset_mode(mode: Mode);
        report_mode(mode: Mode);
        set_private_mode(mode: PrivateMode);
        unset_private_mode(mode: PrivateMode);
        report_private_mode(mode: PrivateMode);
        set_scrolling_region(top: usize, bottom: Option<usize>);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        set_active_charset(index: CharsetIndex);
        configure_charset(index: CharsetIndex, charset: StandardCharset);
        set_color(index: usize, rgb: Rgb);
        dynamic_color_sequence(prefix: String, index: usize, terminator: &str);
        reset_color(index: usize);
        clipboard_store(clipboard: u8, data: &[u8]);
        clipboard_load(clipboard: u8, terminator: &str);
        decaln();
        push_title();
        pop_title();
        text_area_size_pixels();
        text_area_size_chars();
        set_hyperlink(link: Option<Hyperlink>);
        set_mouse_cursor_icon(icon: CursorIcon);
        report_keyboard_mode();
        push_keyboard_mode(mode: KeyboardModes);
        pop_keyboard_modes(n: u16);
        set_keyboard_mode(mode: KeyboardModes, behavior: KeyboardModesApplyBehavior);
        set_modify_other_keys(mode: ModifyOtherKeys);
        report_modify_other_keys();
        set_scp(path: ScpCharPath, update: ScpUpdateMode);
    }
}
