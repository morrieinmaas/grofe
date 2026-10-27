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
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Column;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::vte::ansi::cursor_icon::CursorIcon;
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Rgb,
    ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};

use super::CellFlags;
use crate::server::imgstore::{Cursor, Erase, Probe, ScrollOp, Store};

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

/// The emulator as the parser's handler: [`Term`], marking written cells.
/// With a [`Store`] (the pane's images are on) it also tells it what the
/// parser does to the screen: scrolls, erases and writes over its images.
pub struct Written<'a, L: EventListener>(pub &'a mut Term<L>, pub Option<&'a mut Store>);

impl<L: EventListener> Written<'_, L> {
    fn point(&self) -> Cursor {
        let point = self.0.grid().cursor.point;
        Cursor {
            line: point.line.0.max(0) as u32,
            col: point.column.0 as u32,
        }
    }

    /// How the parser stands, when there is a store to tell.
    fn probe(&self) -> Option<Probe> {
        self.1.as_ref()?;
        let grid = self.0.grid();
        Some(Probe {
            history: grid.history_size(),
            line: grid.cursor.point.line.0.max(0) as u32,
            rows: grid.screen_lines() as u32,
            needs_wrap: grid.cursor.input_needs_wrap && self.0.mode().contains(TermMode::LINE_WRAP),
        })
    }

    /// An operation that may have scrolled the viewport ran.
    fn scrolled(&mut self, kind: ScrollOp, before: Option<Probe>) {
        let history = self.0.grid().history_size();
        if let (Some(store), Some(before)) = (self.1.as_deref_mut(), before) {
            store.scroll_op(kind, before, history);
        }
    }

    /// Runs `op` and tells the store if the alternate screen came or went.
    fn swapping(&mut self, op: impl FnOnce(&mut Term<L>)) {
        let was = self.0.mode().contains(TermMode::ALT_SCREEN);
        op(self.0);
        let now = self.0.mode().contains(TermMode::ALT_SCREEN);
        if let Some(store) = self.1.as_deref_mut()
            && was != now
        {
            store.alt_changed(now);
        }
    }

    /// An operation changes the cursor's row (`to_foot`: and all below it).
    fn line_touched(&mut self, to_foot: bool) {
        let at = self.point();
        let rows = self.0.screen_lines() as u32;
        if let Some(store) = self.1.as_deref_mut() {
            store.check_line(at.line, if to_foot { rows - at.line.min(rows) } else { 1 });
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
            Handler::$name(&mut *self.0, $($arg),*)
        })*
    };
}

impl<L: EventListener> Handler for Written<'_, L> {
    fn input(&mut self, c: char) {
        // Where the character lands, for a sixel it would overwrite.
        let target = self.1.as_ref().filter(|s| s.has_sixel()).and_then(|_| {
            let (p, at) = (self.probe()?, self.point());
            Some(if p.needs_wrap {
                (0, (at.line + 1).min(p.rows - 1))
            } else {
                (at.col, at.line)
            })
        });
        let before = self.probe();
        self.0.grid_mut().cursor.template.flags.insert(WRITTEN);
        Handler::input(&mut *self.0, c);
        self.0.grid_mut().cursor.template.flags.remove(WRITTEN);
        self.scrolled(ScrollOp::Wrap, before);
        if let (Some((col, line)), Some(store)) = (target, self.1.as_deref_mut()) {
            let width = unicode_width::UnicodeWidthChar::width(c)
                .unwrap_or(1)
                .max(1) as u32;
            store.check_area(col, line, width, 1);
        }
        // Insert mode (`IRM`) shifts the line right as `ICH` does, which
        // widens it to the whole line (tmux's `grid_move_cells`, recorded).
        if self.0.mode().contains(TermMode::INSERT) {
            let cols = self.0.columns();
            self.widen(cols);
        }
    }

    fn insert_blank(&mut self, n: usize) {
        self.line_touched(false);
        let (col, cols) = self.cursor();
        if n.max(1) < cols - col {
            Handler::insert_blank(&mut *self.0, n);
            self.widen(cols);
        } else {
            self.clear(col, cols, |t| Handler::insert_blank(t, n));
        }
    }

    fn delete_chars(&mut self, n: usize) {
        self.line_touched(false);
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
        self.line_touched(false);
        let (col, _) = self.cursor();
        let to = col.saturating_add(n.max(1));
        self.clear(col, to, |t| Handler::erase_chars(t, n));
    }

    fn clear_line(&mut self, mode: LineClearMode) {
        self.line_touched(false);
        let (col, cols) = self.cursor();
        let (from, to) = match mode {
            LineClearMode::Right => (col, cols),
            LineClearMode::Left => (0, col + 1),
            LineClearMode::All => (0, cols),
        };
        self.clear(from, to, |t| Handler::clear_line(t, mode));
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        let at = self.point();
        let before = self.probe();
        if let Some(store) = self.1.as_deref_mut() {
            store.erase(
                match mode {
                    ClearMode::Below => Erase::Below,
                    ClearMode::Above => Erase::Above,
                    ClearMode::All => Erase::All,
                    ClearMode::Saved => Erase::Saved,
                },
                at,
            );
        }
        let (col, cols) = self.cursor();
        let (from, to) = match mode {
            ClearMode::Below => (col, cols),
            ClearMode::Above => (0, col + 1),
            ClearMode::All | ClearMode::Saved => (0, cols),
        };
        let all = matches!(mode, ClearMode::All);
        self.clear(from, to, |t| Handler::clear_screen(t, mode));
        if all {
            self.scrolled(ScrollOp::Clear, before);
        }
    }

    fn linefeed(&mut self) {
        let before = self.probe();
        Handler::linefeed(&mut *self.0);
        self.scrolled(ScrollOp::Feed, before);
    }

    fn newline(&mut self) {
        let before = self.probe();
        Handler::newline(&mut *self.0);
        self.scrolled(ScrollOp::Feed, before);
    }

    fn scroll_up(&mut self, n: usize) {
        let before = self.probe();
        Handler::scroll_up(&mut *self.0, n);
        self.scrolled(ScrollOp::Up(n), before);
    }

    fn scroll_down(&mut self, n: usize) {
        Handler::scroll_down(&mut *self.0, n);
        if let Some(store) = self.1.as_deref_mut() {
            store.free_sixels();
        }
    }

    fn reverse_index(&mut self) {
        let at = self.point();
        if let Some(store) = self.1.as_deref_mut() {
            store.reverse_index(at.line);
        }
        Handler::reverse_index(&mut *self.0);
    }

    fn insert_blank_lines(&mut self, n: usize) {
        self.line_touched(true);
        Handler::insert_blank_lines(&mut *self.0, n);
    }

    fn delete_lines(&mut self, n: usize) {
        self.line_touched(true);
        let before = self.probe();
        Handler::delete_lines(&mut *self.0, n);
        self.scrolled(ScrollOp::Delete(n), before);
    }

    fn decaln(&mut self) {
        Handler::decaln(&mut *self.0);
        if let Some(store) = self.1.as_deref_mut() {
            store.free_sixels();
        }
    }

    fn reset_state(&mut self) {
        Handler::reset_state(&mut *self.0);
        if let Some(store) = self.1.as_deref_mut() {
            store.reset();
        }
    }

    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        Handler::set_scrolling_region(&mut *self.0, top, bottom);
        if let Some(store) = self.1.as_deref_mut() {
            store.set_region(top, bottom);
        }
    }

    fn set_private_mode(&mut self, mode: PrivateMode) {
        self.swapping(|t| Handler::set_private_mode(t, mode));
    }

    fn unset_private_mode(&mut self, mode: PrivateMode) {
        self.swapping(|t| Handler::unset_private_mode(t, mode));
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
        bell();
        substitute();
        set_horizontal_tabstop();
        move_backward_tabs(count: u16);
        move_forward_tabs(count: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_tabs(mode: TabulationClearMode);
        set_tabs(interval: u16);
        terminal_attribute(attr: Attr);
        set_mode(mode: Mode);
        unset_mode(mode: Mode);
        report_mode(mode: Mode);
        report_private_mode(mode: PrivateMode);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        set_active_charset(index: CharsetIndex);
        configure_charset(index: CharsetIndex, charset: StandardCharset);
        set_color(index: usize, rgb: Rgb);
        dynamic_color_sequence(prefix: String, index: usize, terminator: &str);
        reset_color(index: usize);
        clipboard_store(clipboard: u8, data: &[u8]);
        clipboard_load(clipboard: u8, terminator: &str);
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

#[cfg(test)]
mod tests {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    use super::*;

    const COLS: usize = 5;
    const ROWS: usize = 5;

    /// A parser, emulator and store, `history` lines of scrollback.
    struct Rig {
        term: Term<VoidListener>,
        parser: Processor<StdSyncHandler>,
        store: Store,
    }

    impl Rig {
        fn new(history: usize) -> Rig {
            let config = Config {
                scrolling_history: history,
                ..Config::default()
            };
            let mut store = Store::new(COLS as u32, ROWS as u32, history);
            store.set_cell_px(Some((10, 20)));
            Rig {
                term: Term::new(config, &TermSize::new(COLS, ROWS), VoidListener),
                parser: Processor::new(),
                store,
            }
        }

        fn feed(&mut self, bytes: &str) {
            self.parser.advance(
                &mut Written(&mut self.term, Some(&mut self.store)),
                bytes.as_bytes(),
            );
        }

        /// A 10x20 pixel image placed at the cursor, one cell, no movement.
        fn place(&mut self) {
            let point = self.term.grid().cursor.point;
            let cur = Cursor {
                line: point.line.0 as u32,
                col: point.column.0 as u32,
            };
            let apc = b"\x1b_Ga=T,i=1,f=24,s=10,v=20,C=1;AAAA\x1b\\";
            self.store.on_apc(apc, cur);
        }

        /// The screen row the one placement is on, if it is visible.
        fn row(&self) -> Option<i32> {
            self.store.view()?.kitty.first().map(|p| p.row)
        }
    }

    /// The first row of the image after `feed`ing something that scrolls.
    fn scroll_case(history: usize, fill: &str, scroll: &str) -> Option<i32> {
        let mut rig = Rig::new(history);
        rig.feed(fill);
        rig.feed("\x1b[5;1H");
        rig.place();
        assert_eq!(rig.row(), Some(4));
        rig.feed(scroll);
        rig.row()
    }

    #[test]
    fn linefeeds_at_the_foot_move_the_placement_up() {
        assert_eq!(scroll_case(100, "", "\n\n"), Some(2));
        assert_eq!(scroll_case(100, "", "\x1bD\x1bE"), Some(2), "IND and NEL");
    }

    #[test]
    fn a_full_history_scrolls_all_the_same() {
        // Ten lines already printed: the three of history are full.
        let fill = "1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8\r\n9";
        assert_eq!(scroll_case(3, fill, "\n\n"), Some(2));
        // No history at all.
        assert_eq!(scroll_case(0, "", "\n\n\n"), Some(1));
    }

    #[test]
    fn a_character_that_wraps_at_the_foot_scrolls_too() {
        // Six characters in five columns: the sixth wraps below the foot.
        assert_eq!(scroll_case(100, "", "abcdef"), Some(3));
        assert_eq!(scroll_case(0, "", "abcdef"), Some(3));
    }

    #[test]
    fn su_and_dl_scroll_and_a_region_does_not() {
        assert_eq!(scroll_case(100, "", "\x1b[2S"), Some(2));
        assert_eq!(scroll_case(0, "", "\x1b[2S"), Some(2));
        assert_eq!(
            scroll_case(100, "", "\x1b[1;1H\x1b[2M"),
            Some(2),
            "DL at the top"
        );
        // A scroll region that is not the whole screen scrolls what is in
        // it and leaves the viewport be.
        assert_eq!(scroll_case(100, "", "\x1b[1;4r\x1b[4;1H\n\n"), Some(4));
    }

    #[test]
    fn erasing_the_screen_deletes_what_is_on_it() {
        assert_eq!(scroll_case(100, "", "\x1b[2J"), None);
        assert_eq!(scroll_case(100, "", "\x1bc"), None, "RIS");
    }

    #[test]
    fn the_alternate_screen_has_its_own() {
        let mut rig = Rig::new(100);
        rig.feed("\x1b[5;1H");
        rig.place();
        rig.feed("\x1b[?1049h");
        assert_eq!(rig.row(), None);
        rig.feed("\x1b[?1049l");
        assert_eq!(rig.row(), Some(4));
    }

    #[test]
    fn text_over_a_sixel_image_frees_it() {
        let sixel = b"q#0;2;100;0;0!20~-!20~";
        let mut rig = Rig::new(100);
        let place = |rig: &mut Rig| {
            let inject = rig.store.on_sixel(sixel, 0, Cursor { line: 1, col: 1 });
            rig.feed(std::str::from_utf8(&inject).expect("ascii"));
        };
        place(&mut rig);
        assert!(rig.store.has_sixel());
        rig.feed("\x1b[1;1Hx");
        assert!(rig.store.has_sixel(), "beside it");
        rig.feed("\x1b[2;3Hy");
        assert!(!rig.store.has_sixel(), "a character in it");
        place(&mut rig);
        rig.feed("\x1b[2;1H\x1b[K");
        assert!(!rig.store.has_sixel(), "its row cleared");
        place(&mut rig);
        rig.feed("\x1b[5;1H\n");
        assert!(rig.store.has_sixel(), "a scroll by one only moves it up");
        rig.feed("\n\n\n");
        assert!(!rig.store.has_sixel() || rig.store.view().is_none());
    }
}
