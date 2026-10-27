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
//!
//! It also keeps the pane's OSC 133 [`Marks`] in step: the history is held
//! to its limit after every operation, and the clears that reset a tmux
//! line's flags (the same whole-line clears, with the exceptions recorded
//! in `marks.rs` and below) erase the marks of the lines they empty.

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Column;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::vte::ansi::cursor_icon::CursorIcon;
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, Color, CursorShape, CursorStyle, Handler, Hyperlink,
    KeyboardModes, KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, NamedColor,
    PrivateMode, Rgb, ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};

use super::CellFlags;
use super::marks::Marks;

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

/// The emulator as the parser's handler: [`Term`], marking written cells,
/// and the pane's [`Marks`].
pub struct Written<'a, L: EventListener>(pub &'a mut Term<L>, pub &'a mut Marks);

impl<L: EventListener> Written<'_, L> {
    /// Whether marks are being kept up: there are some, on the normal screen.
    fn marked(&self) -> bool {
        !self.1.is_empty() && !self.0.mode().contains(TermMode::ALT_SCREEN)
    }

    /// The cursor's row, counted from the oldest row of the history.
    fn top(&self) -> usize {
        let grid = self.0.grid();
        grid.history_size() + grid.cursor.point.line.0.max(0) as usize
    }

    /// Whether clearing with the pen's background touches a line that has no
    /// cells (tmux's `screen_write_clearline` leaves such a line alone, flags
    /// and all, unless the background is coloured).
    fn clears_empty_lines(&self) -> bool {
        self.0.grid().cursor.template.bg != Color::Named(NamedColor::Background)
    }

    /// Resets the cursor line's marks when `whole` of it is about to be
    /// cleared, as tmux's `grid_clear_lines` resets a line's flags.
    /// `conditional` is `screen_write_clearline`'s early return for a line
    /// with no cells.
    fn reset_line(&mut self, whole: bool, conditional: bool) {
        if whole
            && self.marked()
            && (!conditional || self.line_used() > 0 || self.clears_empty_lines())
        {
            let top = self.top();
            self.1.erase(top);
        }
    }

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
            Handler::$name(&mut *self.0, $($arg),*);
            self.1.sync(&mut *self.0);
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
        self.1.sync(&mut *self.0);
    }

    fn insert_blank(&mut self, n: usize) {
        let (col, cols) = self.cursor();
        if n.max(1) < cols - col {
            Handler::insert_blank(&mut *self.0, n);
            self.widen(cols);
        } else {
            self.reset_line(col == 0, false);
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
            self.reset_line(col == 0, false);
            self.clear(col, cols, |t| Handler::delete_chars(t, n));
        }
    }

    fn erase_chars(&mut self, n: usize) {
        let (col, cols) = self.cursor();
        let to = col.saturating_add(n.max(1));
        self.reset_line(col == 0 && to >= cols, false);
        self.clear(col, to, |t| Handler::erase_chars(t, n));
    }

    fn clear_line(&mut self, mode: LineClearMode) {
        let (col, cols) = self.cursor();
        let (from, to) = match mode {
            LineClearMode::Right => (col, cols),
            LineClearMode::Left => (0, col + 1),
            LineClearMode::All => (0, cols),
        };
        self.reset_line(from == 0 && to >= cols, true);
        self.clear(from, to, |t| Handler::clear_line(t, mode));
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        let (col, cols) = self.cursor();
        let (from, to) = match mode {
            ClearMode::Below => (col, cols),
            ClearMode::Above => (0, col + 1),
            ClearMode::All | ClearMode::Saved => (0, cols),
        };
        let history = self.0.grid().history_size();
        let rows = self.0.screen_lines();
        let top = self.top();
        let normal = !self.0.mode().contains(TermMode::ALT_SCREEN);
        let (all, saved) = (
            matches!(mode, ClearMode::All),
            matches!(mode, ClearMode::Saved),
        );
        match mode {
            ClearMode::Below => {
                self.reset_line(col == 0, false);
                if self.marked() {
                    self.1.erase_range(top + 1, history + rows);
                }
            }
            ClearMode::Above => {
                self.reset_line(col + 1 >= cols, false);
                if self.marked() {
                    self.1.erase_range(history, top);
                }
            }
            ClearMode::All | ClearMode::Saved => {}
        }
        self.clear(from, to, |t| Handler::clear_screen(t, mode));
        if all && normal && !self.1.is_empty() {
            // The rows below the last used one are emptied, the rest
            // scrolled into the history with their marks.
            let pushed = self.0.grid().history_size() - history;
            self.1.erase_range(history + pushed, history + rows);
        } else if saved && normal {
            self.1.trim(history);
        }
        self.1.sync(&mut *self.0);
    }

    fn reset_state(&mut self) {
        Handler::reset_state(&mut *self.0);
        self.1.clear();
        self.1.sync(&mut *self.0);
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
