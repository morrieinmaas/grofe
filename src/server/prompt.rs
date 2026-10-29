//! The command prompt (`prefix :`, `command-prompt`) and `run-shell`
//! (T3.4).
//!
//! A prompt belongs to one client ([`Prompts`], keyed by `ClientId`): it is
//! drawn on that client's status row (the last row even with `status off`,
//! as tmux does) by [`PromptOverlay`] and eats that client's input through
//! [`PromptHook`] while open. Editing is tmux's emacs `status-keys`. Enter
//! puts the line into the template at `%%` (tmux's
//! `cmd_template_replace`), splits it with `cmd::split_command_string` and
//! hands it back as [`Routed::Command`], which `clients::route` runs as a
//! binding: the client's context, errors on its status row. History is in
//! memory, server-wide, newest last.
//!
//! `run-shell` runs `/bin/sh -c` in a task of its own, so the actor never
//! waits on it (D8); the CLI's reply comes through
//! [`dispatch::defer_reply`] once the command exits (R23).

use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Instant;

use alacritty_terminal::vte::ansi::{Color, NamedColor};
use tokio::sync::oneshot;
use unicode_width::UnicodeWidthChar;

use crate::cmd::{self, CommandPrompt, Resolve, RunShell};
use crate::protocol::{ClientCtx, Dialect, Failure};
use crate::server::clients::{self, ClientVars};
use crate::server::dispatch::{
    self,
    options::{FormatCtx, ServerVars},
};
use crate::server::input::{Input, InputHook, Key, KeyCode, Routed};
use crate::server::pane::{self, CellFlags, CursorState, SnapCell};
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::server::shell_jobs::ShellJobs;
use crate::server::status::{self, Style};
use crate::server::{State, hooks};
use crate::types::ClientId;

/// tmux's `prompt-history-limit`.
pub const HISTORY_LIMIT: usize = 100;

/// `message-style`'s and `message-command-style`'s defaults, if the options
/// are missing.
const MESSAGE_STYLE: &str = "bg=yellow,fg=black,fill=yellow";
const MESSAGE_COMMAND_STYLE: &str = "bg=black,fg=yellow,fill=black";

/// tmux 3.7b's default `word-separators`, which `C-w` stops at.
const WORD_SEPARATORS: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~";

/// tmux 3.7b's prompt types (`command-prompt -T`), each with a history
/// of its own; a prompt is `command` unless `-T` says otherwise.
const PROMPT_TYPES: [&str; 4] = ["command", "search", "target", "window-target"];

/// Every client's open prompt and the server-wide histories.
#[derive(Debug, Default)]
pub struct Prompts {
    open: BTreeMap<ClientId, Prompt>,
    /// The CLI replies of `command-prompt`s waiting for their prompt to
    /// close.
    waiting: BTreeMap<ClientId, oneshot::Sender<Result<String, Failure>>>,
    /// Lines entered at the prompts of each of [`PROMPT_TYPES`], newest
    /// last.
    history: [Vec<String>; PROMPT_TYPES.len()],
}

impl Prompts {
    /// Whether client `id` has a prompt open.
    pub fn is_open(&self, id: ClientId) -> bool {
        self.open.contains_key(&id)
    }

    /// Closes client `id`'s prompt, answering a CLI that waits on it.
    fn close(&mut self, id: ClientId) -> Option<Prompt> {
        self.close_with(id, Ok(String::new()))
    }

    /// Closes client `id`'s prompt, answering a waiting CLI with `answer`.
    fn close_with(&mut self, id: ClientId, answer: Result<String, Failure>) -> Option<Prompt> {
        if let Some(reply) = self.waiting.remove(&id) {
            let _ = reply.send(answer);
        }
        self.open.remove(&id)
    }

    /// Adds a line entered at a prompt of type `kind`: empty lines and a
    /// repeat of the newest are skipped, the oldest go past
    /// [`HISTORY_LIMIT`].
    fn remember(&mut self, kind: usize, line: &str) {
        let history = &mut self.history[kind];
        if line.is_empty() || history.last().is_some_and(|l| l == line) {
            return;
        }
        history.push(line.to_string());
        let over = history.len().saturating_sub(HISTORY_LIMIT);
        history.drain(..over);
    }
}

/// How a prompt reads its answer (tmux's prompt flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Read {
    /// A line, edited until Enter.
    Line,
    /// `-1` (`PROMPT_SINGLE`): one keystroke, as the character it types.
    Single,
    /// `-k` (`PROMPT_KEY`): one key, as its name.
    Key,
    /// `confirm-before`: one keystroke, read as `-1` reads it; the command
    /// runs only if it is `y`.
    Confirm,
}

/// One open prompt.
#[derive(Debug)]
struct Prompt {
    /// Drawn before the text: `:`, `(verb) ` or `-p`'s text and a space.
    label: String,
    text: Vec<char>,
    /// Insertion point, an index into `text`.
    cursor: usize,
    /// Run on Enter with the text at `%%` (tmux's default `%1`).
    template: String,
    /// While browsing history: how far back, 0 being the newest.
    back: Option<usize>,
    /// What the command runs in (`compat::prompt_dialect`).
    dialect: Dialect,
    read: Read,
    /// `command-prompt -N`: only digits are typed.
    numeric: bool,
    /// `command-prompt -i`: the template runs with every change of the
    /// text, prefixed `=`, `+` or `-`, and Enter runs nothing.
    incremental: bool,
    /// An incremental prompt's `-I` text, which C-r and C-s on an empty
    /// line bring back (tmux's `prompt_last`).
    last: String,
    /// `confirm-before`: the key that confirms, and whether Enter does.
    confirm_key: char,
    default_yes: bool,
    /// Which of [`PROMPT_TYPES`] the prompt is, so which history it walks.
    kind: usize,
    /// `status-keys` is `vi`, and the prompt is in its command mode
    /// (tmux's `PROMPT_COMMAND`); both are read before each key.
    vi: bool,
    command_mode: bool,
    /// `word-separators`, read before each key.
    separators: String,
    /// `C-v`: the next key is typed as it is.
    quote_next: bool,
    /// What `C-w` last cut, which `C-y` brings back; else the newest
    /// buffer (read before the key).
    saved: Option<String>,
    clip: Option<String>,
    /// `command-prompt -e`: Backspace on an empty line cancels.
    bspace_exit: bool,
    /// `command-prompt` with several `-p` prompts: the label and text of
    /// those still to come, and the answers so far.
    pending: std::collections::VecDeque<(String, String)>,
    answers: Vec<String>,
    /// What to do with the answer instead of running `template`.
    then: Then,
}

/// A prompt's own answer handler (`status_prompt_set`'s input callback):
/// called with the line when it is entered.
pub type PromptFn = Box<dyn FnOnce(&mut State, ClientId, &str) -> Routed + Send>;

#[derive(Default)]
struct Then(Option<PromptFn>);

impl std::fmt::Debug for Then {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "Then(fn)" } else { "Then" })
    }
}

/// What `status-keys vi` makes of a key.
enum Translated {
    Drop,
    /// Go on as this emacs key.
    Process(Key),
    /// Type it.
    Append,
}

/// The vi word motions, which tmux gives keys of their own.
enum ViWord {
    Forward(bool),
    End(bool),
    Back(bool),
}

impl ViWord {
    /// Moves `p` (`true`: by `word-separators`, else by spaces only).
    fn apply(self, p: &mut Prompt) {
        let seps = |on: bool| {
            if on {
                p.separators.clone()
            } else {
                String::new()
            }
        };
        match self {
            ViWord::Forward(on) => p.forward_word(true, &seps(on)),
            ViWord::End(on) => p.end_word(&seps(on)),
            ViWord::Back(on) => p.backward_word(&seps(on)),
        }
    }
}

/// What a key did to a prompt.
#[derive(Debug, PartialEq, Eq)]
enum Edit {
    Keep,
    Cancel,
    Enter,
    /// Tab: complete the word at the cursor.
    Complete,
}

impl Prompt {
    fn new(label: String, initial: &str, template: String) -> Prompt {
        let text: Vec<char> = initial.chars().collect();
        Prompt {
            label,
            cursor: text.len(),
            text,
            template,
            back: None,
            dialect: Dialect::Grofe,
            read: Read::Line,
            numeric: false,
            incremental: false,
            last: String::new(),
            confirm_key: 'y',
            default_yes: false,
            kind: 0,
            vi: false,
            command_mode: false,
            separators: WORD_SEPARATORS.to_string(),
            quote_next: false,
            saved: None,
            clip: None,
            bspace_exit: false,
            pending: Default::default(),
            answers: Vec::new(),
            then: Then::default(),
        }
    }

    /// Whether the key a `confirm-before` prompt got confirms.
    fn confirmed(&self) -> bool {
        let key = self.text.first();
        key == Some(&self.confirm_key) || (self.default_yes && key == Some(&'\r'))
    }

    fn line(&self) -> String {
        self.text.iter().collect()
    }

    fn set_line(&mut self, line: &str) {
        self.text = line.chars().collect();
        self.cursor = self.text.len();
    }

    fn insert(&mut self, s: &str) {
        let numeric = self.numeric;
        for c in s
            .chars()
            .filter(|c| !c.is_control() && (!numeric || c.is_ascii_digit()))
        {
            self.text.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    /// tmux's `status_prompt_key` for a line prompt: `status-keys` emacs,
    /// or vi through [`Prompt::translate`].
    fn key(&mut self, key: &Key, history: &[String]) -> Edit {
        if self.quote_next {
            self.quote_next = false;
            if let Some(c) = single_char(key) {
                self.text.insert(self.cursor, c);
                self.cursor += 1;
            }
            return Edit::Keep;
        }
        if self.vi {
            match self.translate(key) {
                Translated::Drop => return Edit::Keep,
                Translated::Process(k) => return self.process(&k, history),
                Translated::Append => return self.append(key),
            }
        }
        self.process(key, history)
    }

    /// Types a printable key.
    fn append(&mut self, key: &Key) -> Edit {
        let m = key.mods;
        if let KeyCode::Char(c) = key.code
            && !m.ctrl
            && !m.alt
            && !m.super_
        {
            let c = if m.shift { c.to_ascii_uppercase() } else { c };
            self.insert(c.encode_utf8(&mut [0; 4]));
        }
        Edit::Keep
    }

    /// tmux's `status_prompt_translate_key`: `status-keys vi`. In its entry
    /// mode most keys type; Escape goes to its command mode, whose keys
    /// are emacs keys under other names.
    fn translate(&mut self, key: &Key) -> Translated {
        use Translated::{Append, Drop, Process};
        let m = key.mods;
        let plain = !m.ctrl && !m.alt && !m.super_;
        let ctrl = |c: char| matches!(key.code, KeyCode::Char(k) if k == c) && m.ctrl && !m.alt;
        let emacs = |code: KeyCode, ctrl: bool, alt: bool| {
            Process(Key {
                code,
                mods: crate::server::input::Mods {
                    ctrl,
                    alt,
                    ..Default::default()
                },
            })
        };
        let c_k = |c: char| emacs(KeyCode::Char(c), true, false);
        let escape = key.code == KeyCode::Escape || ctrl('[');
        if !self.command_mode {
            let passes = match key.code {
                KeyCode::Char(c) if m.ctrl => "acEghknptuvwy".contains(c.to_ascii_lowercase()),
                KeyCode::Tab
                | KeyCode::Enter
                | KeyCode::Backspace
                | KeyCode::Delete
                | KeyCode::Down
                | KeyCode::End
                | KeyCode::Home
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up => true,
                _ => false,
            };
            if escape {
                self.command_mode = true;
                self.cursor = self.cursor.saturating_sub(1);
                return Drop;
            }
            return if passes && !(m.ctrl && matches!(key.code, KeyCode::Char('E'))) {
                Process(*key)
            } else {
                Append
            };
        }
        // Command mode. Some keys switch back to entry first.
        if key.code == KeyCode::Backspace {
            return emacs(KeyCode::Left, false, false);
        }
        let ch = match key.code {
            KeyCode::Char(c) if plain || (m.shift && !m.ctrl && !m.alt) => Some(c),
            _ => None,
        };
        match ch {
            Some('A' | 'I' | 'C' | 's' | 'a') => self.command_mode = false,
            Some('S') => {
                self.command_mode = false;
                return c_k('u');
            }
            Some('i') => {
                self.command_mode = false;
                return Drop;
            }
            _ => {}
        }
        if escape {
            return Drop;
        }
        match (key.code, ch) {
            (_, Some('A' | '$')) | (KeyCode::End, _) => emacs(KeyCode::End, false, false),
            (_, Some('I' | '0' | '^')) | (KeyCode::Home, _) => emacs(KeyCode::Home, false, false),
            (_, Some('C' | 'D')) => c_k('k'),
            (_, Some('X')) => emacs(KeyCode::Backspace, false, false),
            (_, Some('b')) => emacs(KeyCode::Char('b'), false, true),
            (_, Some('B')) => {
                ViWord::Back(false).apply(self);
                Drop
            }
            (_, Some('d')) => c_k('u'),
            (_, Some('e')) => {
                ViWord::End(true).apply(self);
                Drop
            }
            (_, Some('E')) => {
                ViWord::End(false).apply(self);
                Drop
            }
            (_, Some('w')) => {
                ViWord::Forward(true).apply(self);
                Drop
            }
            (_, Some('W')) => {
                ViWord::Forward(false).apply(self);
                Drop
            }
            (_, Some('p')) => c_k('y'),
            (_, Some('q')) => c_k('c'),
            (KeyCode::Delete, _) | (_, Some('s' | 'x')) => emacs(KeyCode::Delete, false, false),
            (KeyCode::Down, _) | (_, Some('j')) => emacs(KeyCode::Down, false, false),
            (KeyCode::Left, _) | (_, Some('h')) => emacs(KeyCode::Left, false, false),
            (KeyCode::Right, _) | (_, Some('a' | 'l')) => emacs(KeyCode::Right, false, false),
            (KeyCode::Up, _) | (_, Some('k')) => emacs(KeyCode::Up, false, false),
            (KeyCode::Enter, _) => Process(*key),
            _ if ctrl('h') || ctrl('c') => Process(*key),
            _ => Drop,
        }
    }

    /// tmux's emacs keys (`status_prompt_key`'s `process_key`).
    fn process(&mut self, key: &Key, history: &[String]) -> Edit {
        let m = key.mods;
        let ctrl_only = m.ctrl && !m.alt && !m.super_;
        match key.code {
            KeyCode::Enter => return Edit::Enter,
            KeyCode::Escape => return Edit::Cancel,
            KeyCode::Tab => return Edit::Complete,
            KeyCode::Char(c) if m.alt && !m.ctrl && !m.super_ => match c.to_ascii_lowercase() {
                'f' => self.forward_word(false, &self.separators.clone()),
                'b' => self.backward_word(&self.separators.clone()),
                _ => {}
            },
            KeyCode::Char(c) if ctrl_only => match c.to_ascii_lowercase() {
                'j' | 'm' => return Edit::Enter,
                'c' | 'g' | '[' => return Edit::Cancel,
                'b' => self.left(),
                'f' => self.right(),
                'a' => self.cursor = 0,
                'e' => self.cursor = self.text.len(),
                'h' => return self.backspace_key(),
                'd' => self.delete(),
                'k' => self.text.truncate(self.cursor),
                'u' => {
                    self.text.clear();
                    self.cursor = 0;
                }
                'w' => self.delete_word(),
                'p' => self.older(history),
                'n' => self.newer(history),
                'y' => self.paste(),
                't' => self.transpose(),
                'v' => self.quote_next = true,
                _ => {}
            },
            KeyCode::Char(c) if !m.ctrl && !m.alt && !m.super_ => {
                let c = if m.shift { c.to_ascii_uppercase() } else { c };
                self.insert(c.encode_utf8(&mut [0; 4]));
            }
            KeyCode::Left if m.ctrl && !m.alt => self.backward_word(&self.separators.clone()),
            KeyCode::Right if m.ctrl && !m.alt => {
                self.forward_word(false, &self.separators.clone())
            }
            KeyCode::Left if !m.alt => self.left(),
            KeyCode::Right if !m.alt => self.right(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            KeyCode::Backspace => return self.backspace_key(),
            KeyCode::Delete => self.delete(),
            KeyCode::Up => self.older(history),
            KeyCode::Down => self.newer(history),
            _ => {}
        }
        Edit::Keep
    }

    /// Backspace; on an empty line `command-prompt -e` cancels.
    fn backspace_key(&mut self) -> Edit {
        if self.bspace_exit && self.text.is_empty() {
            return Edit::Cancel;
        }
        self.backspace();
        Edit::Keep
    }

    /// `C-t`: swaps the two characters before the cursor.
    fn transpose(&mut self) {
        let mut idx = self.cursor;
        if idx < self.text.len() {
            idx += 1;
        }
        if idx >= 2 {
            self.text.swap(idx - 2, idx - 1);
            self.cursor = idx;
        }
    }

    /// `C-y`: what `C-w` cut, else the newest buffer up to its first
    /// character that is not printable ASCII (`status_prompt_paste`).
    fn paste(&mut self) {
        let text = match (&self.saved, &self.clip) {
            (Some(s), _) => s.clone(),
            (None, Some(clip)) => clip
                .chars()
                .take_while(|c| !c.is_ascii() || (' '..'\u{7f}').contains(c))
                .collect(),
            (None, None) => return,
        };
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        self.insert(&text);
    }

    fn is_space(c: char) -> bool {
        c == ' '
    }

    /// `status_prompt_forward_word`: `vi` stops at the start of the next
    /// word, emacs after the end of this one.
    fn forward_word(&mut self, vi: bool, separators: &str) {
        let size = self.text.len();
        let in_list = |c: char| c.is_ascii() && separators.contains(c);
        let mut idx = self.cursor;
        if !vi {
            while idx != size && Self::is_space(self.text[idx]) {
                idx += 1;
            }
        }
        if idx == size {
            self.cursor = idx;
            return;
        }
        let word_is_separators = in_list(self.text[idx]) && !Self::is_space(self.text[idx]);
        loop {
            idx += 1;
            if idx < size && Self::is_space(self.text[idx]) {
                if vi {
                    while idx != size && Self::is_space(self.text[idx]) {
                        idx += 1;
                    }
                }
                break;
            }
            if !(idx != size && word_is_separators == in_list(self.text[idx])) {
                break;
            }
        }
        self.cursor = idx;
    }

    /// `status_prompt_end_word`.
    fn end_word(&mut self, separators: &str) {
        let size = self.text.len();
        let in_list = |c: char| c.is_ascii() && separators.contains(c);
        let mut idx = self.cursor;
        if idx == size {
            return;
        }
        loop {
            idx += 1;
            if idx == size {
                self.cursor = idx;
                return;
            }
            if !Self::is_space(self.text[idx]) {
                break;
            }
        }
        let word_is_separators = in_list(self.text[idx]);
        loop {
            idx += 1;
            if idx == size {
                break;
            }
            if Self::is_space(self.text[idx]) || word_is_separators != in_list(self.text[idx]) {
                break;
            }
        }
        self.cursor = idx - 1;
    }

    /// `status_prompt_backward_word`: where `C-w` would cut to.
    fn word_start(&self, separators: &str) -> usize {
        let in_list = |c: char| c.is_ascii() && separators.contains(c);
        let at = |i: usize| self.text.get(i).copied().unwrap_or(' ');
        let mut idx = self.cursor;
        while idx != 0 {
            idx -= 1;
            if !Self::is_space(at(idx)) {
                break;
            }
        }
        let word_is_separators = in_list(at(idx));
        while idx != 0 {
            idx -= 1;
            if Self::is_space(at(idx)) || word_is_separators != in_list(at(idx)) {
                idx += 1;
                break;
            }
        }
        idx
    }

    fn backward_word(&mut self, separators: &str) {
        self.cursor = self.word_start(separators);
    }

    /// [`Prompt::key`] for an incremental prompt, and the prefix its
    /// template runs with if the key changed what is searched for: `=`,
    /// or with C-r and C-s `-` and `+` (on an empty line they bring back
    /// the `-I` text, with `=`), as tmux's `status_prompt_key`.
    fn incremental_key(&mut self, key: &Key, history: &[String]) -> (Edit, Option<char>) {
        let m = key.mods;
        if let KeyCode::Char(c) = key.code
            && m.ctrl
            && !m.alt
            && !m.super_
            && matches!(c.to_ascii_lowercase(), 'r' | 's')
        {
            if self.text.is_empty() {
                let last = self.last.clone();
                self.set_line(&last);
                return (Edit::Keep, Some('='));
            }
            let prefix = if c.eq_ignore_ascii_case(&'r') {
                '-'
            } else {
                '+'
            };
            return (Edit::Keep, Some(prefix));
        }
        // tmux runs the template again for C-u and C-w, and for a history
        // step that found an entry, even when the text stays the same (a
        // search for the same text steps on to its next match).
        let ctrl = |c: char| {
            m.ctrl
                && !m.alt
                && !m.super_
                && matches!(key.code, KeyCode::Char(k) if k.eq_ignore_ascii_case(&c))
        };
        // Down always lands somewhere, an empty line past the newest.
        let up = key.code == KeyCode::Up || ctrl('p');
        let down = key.code == KeyCode::Down || ctrl('n');
        let at_oldest = self.back == Some(history.len().saturating_sub(1));
        let always = ctrl('u') || ctrl('w') || (up && !history.is_empty() && !at_oldest) || down;
        let before = self.line();
        let edit = self.key(key, history);
        let changed = edit == Edit::Keep && (always || self.line() != before);
        (edit, changed.then_some('='))
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.len());
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.text.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    /// `C-w`, as tmux 3.7b: skip spaces back, then cut the run of
    /// characters that are all separators or all not, keeping it for `C-y`.
    fn delete_word(&mut self) {
        let idx = self.word_start(&self.separators.clone());
        self.saved = Some(self.text[idx..self.cursor].iter().collect());
        self.text.drain(idx..self.cursor);
        self.cursor = idx;
    }

    /// Up: the next older history entry.
    fn older(&mut self, history: &[String]) {
        if history.is_empty() {
            return;
        }
        let back = self.back.map_or(0, |b| (b + 1).min(history.len() - 1));
        self.back = Some(back);
        self.set_line(&history[history.len() - 1 - back]);
    }

    /// Down: the next newer entry, then an empty line, as tmux (whose Down
    /// past the newest entry empties the line too).
    fn newer(&mut self, history: &[String]) {
        match self.back {
            None | Some(0) => {
                self.back = None;
                self.set_line("");
            }
            // The history never gets shorter, so `b` still indexes it.
            Some(b) => {
                self.back = Some(b - 1);
                self.set_line(&history[history.len() - b]);
            }
        }
    }

    /// The prompt row as tests see it: [`Prompt::row_over`] on a label
    /// drawn in `message-style`.
    #[cfg(test)]
    fn row(&self, cols: usize) -> (Vec<SnapCell>, u16) {
        let style = status::parse_style(MESSAGE_STYLE);
        let drawn = status::draw(&self.label, style, cols as u16);
        let start = visible_width(&self.label);
        let (cells, col) = self.row_over(line_cells(&drawn.line, cols), start, &style);
        (cells, col as u16)
    }

    /// The prompt row: `base` (the label already drawn, as many cells as the
    /// prompt area is wide) with the text from column `start` in `style`,
    /// scrolled left when the cursor would be past the edge, as tmux's
    /// `status_prompt_redraw`; and the cursor's column.
    fn row_over(
        &self,
        mut base: Vec<SnapCell>,
        start: usize,
        style: &Style,
    ) -> (Vec<SnapCell>, usize) {
        let aw = base.len();
        let blank = SnapCell {
            c: ' ',
            fg: style.fg,
            bg: style.bg,
            flags: style.attrs,
            zerowidth: Vec::new(),
            underline_color: style.underline_color,
            hyperlink: None,
        };
        let cell = |c: char, extra: CellFlags| SnapCell {
            c,
            flags: style.attrs | extra,
            ..blank.clone()
        };
        // The text, one cell per column; the index each character starts at.
        let mut cells: Vec<SnapCell> = Vec::new();
        let mut starts = Vec::with_capacity(self.text.len() + 1);
        for c in &self.text {
            starts.push(cells.len());
            if (*c as u32) < 0x20 || *c == '\x7f' {
                cells.push(cell('^', CellFlags::empty()));
                cells.push(cell(
                    if *c == '\x7f' {
                        '?'
                    } else {
                        (*c as u8 | 0x40) as char
                    },
                    CellFlags::empty(),
                ));
                continue;
            }
            match UnicodeWidthChar::width(*c) {
                Some(0) | None => {
                    if let Some(last) = cells.last_mut() {
                        last.zerowidth.push(*c);
                    }
                }
                Some(2) => {
                    cells.push(cell(*c, CellFlags::WIDE_CHAR));
                    cells.push(cell(' ', CellFlags::WIDE_CHAR_SPACER));
                }
                Some(_) => cells.push(cell(*c, CellFlags::empty())),
            }
        }
        starts.push(cells.len());
        let pcursor = starts.get(self.cursor).copied().unwrap_or(cells.len());
        if self.quote_next {
            cells.insert(pcursor.min(cells.len()), cell('^', CellFlags::empty()));
        }
        let left = aw.saturating_sub(start);
        if left == 0 {
            return (base, start.min(aw.saturating_sub(1)));
        }
        let offset = if pcursor >= left {
            pcursor - left + 1
        } else {
            0
        };
        for x in 0..left {
            let c = cells
                .get(offset + x)
                .cloned()
                .unwrap_or_else(|| blank.clone());
            base[start + x] = c;
        }
        (base, start + pcursor - offset)
    }
}

/// The columns of `text` once its `#[...]` styles are taken out and `##` is
/// one `#` (tmux's `format_width`).
fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '#' {
            match chars.peek() {
                Some('[') => {
                    for n in chars.by_ref() {
                        if n == ']' {
                            break;
                        }
                    }
                    continue;
                }
                Some('#') => {
                    chars.next();
                }
                _ => {}
            }
        }
        width += UnicodeWidthChar::width(c).unwrap_or(0);
    }
    width
}

/// The cells of a drawn status row, `cols` of them.
fn line_cells(line: &status::StyledLine, cols: usize) -> Vec<SnapCell> {
    let mut out: Vec<SnapCell> = Vec::with_capacity(cols);
    for span in &line.spans {
        for c in span.text.chars() {
            let cell = |c: char, extra: CellFlags| SnapCell {
                c,
                fg: span.style.fg,
                bg: span.style.bg,
                flags: span.style.attrs | extra,
                zerowidth: Vec::new(),
                underline_color: span.style.underline_color,
                hyperlink: None,
            };
            match UnicodeWidthChar::width(c) {
                Some(0) | None => {
                    if let Some(last) = out.last_mut() {
                        last.zerowidth.push(c);
                    }
                }
                Some(2) => {
                    out.push(cell(c, CellFlags::WIDE_CHAR));
                    out.push(cell(' ', CellFlags::WIDE_CHAR_SPACER));
                }
                Some(_) => out.push(cell(c, CellFlags::empty())),
            }
        }
    }
    out.truncate(cols);
    let fill = out.last().cloned().unwrap_or_else(|| SnapCell {
        c: ' ',
        fg: Color::Named(NamedColor::Foreground),
        bg: Color::Named(NamedColor::Background),
        flags: CellFlags::empty(),
        zerowidth: Vec::new(),
        underline_color: None,
        hyperlink: None,
    });
    out.resize(cols, SnapCell { c: ' ', ..fill });
    out
}

/// The character a keystroke types at a one-key prompt (`-1`,
/// `confirm-before`), as tmux 3.7b's `status_prompt_key` reads it: a
/// control chord is its C0 byte, Enter, Tab, Escape and Backspace theirs,
/// Meta is dropped; a key that types nothing (an arrow, a function key) is
/// `None`, and the prompt stays open.
fn single_char(key: &Key) -> Option<char> {
    let m = key.mods;
    match key.code {
        KeyCode::Char(c) if m.ctrl && c.is_ascii() => char::from_u32(u32::from(c) & 0x1f),
        KeyCode::Char(c) if m.shift => Some(c.to_ascii_uppercase()),
        KeyCode::Char(c) => Some(c),
        KeyCode::Enter => Some('\r'),
        KeyCode::Tab => Some('\t'),
        KeyCode::Escape => Some('\x1b'),
        KeyCode::Backspace => Some('\x7f'),
        _ => None,
    }
}

/// tmux's `cmd_template_replace` for the first answer: `%1` is replaced
/// everywhere, `%%` the first time only; `%%%` (and `%1%`) also escapes
/// `"`, `\`, `$`, `;` and `~` in the answer. A template without `%` is
/// kept as it is.
pub(crate) fn replace_template(template: &str, answer: &str) -> String {
    replace_template_idx(template, answer, 1)
}

/// [`replace_template`] for the `idx`th answer of several prompts: `%idx`
/// everywhere, and `%%` the first time only when `idx` is 1 (a template
/// with `%%` is replaced once overall, as `args_make_commands` chains it).
pub(crate) fn replace_template_idx(template: &str, answer: &str, idx: usize) -> String {
    if !template.contains('%') {
        return template.to_string();
    }
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::new();
    let mut replaced = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if c == '%' {
            let next = chars.get(i).copied();
            let matched = match next {
                Some(d) if d.to_digit(10).is_some_and(|d| d as usize == idx) => true,
                Some('%') if !replaced => {
                    replaced = true;
                    true
                }
                _ => false,
            };
            if matched {
                i += 1;
                let quoted = chars.get(i) == Some(&'%');
                if quoted {
                    i += 1;
                }
                for a in answer.chars() {
                    if quoted && "\"\\$;~".contains(a) {
                        out.push('\\');
                    }
                    out.push(a);
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Eats a client's input while its prompt is open. Focus reports pass on.
pub struct PromptHook;

impl InputHook for PromptHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        _now: Instant,
    ) -> Option<Routed> {
        // An open menu takes the keys first, as tmux's overlay does.
        if state.menus.is_open(client) {
            return None;
        }
        if !state.prompts.open.contains_key(&client) {
            return None;
        }
        if let Input::Focus(_) = input {
            return None;
        }
        // `status-keys`, `word-separators` and the buffer `C-y` pastes,
        // read before the key.
        let (vi, separators) = prompt_env(state, client);
        let wants_clip = matches!(input, Input::Key(k)
            if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('p')));
        let clip = wants_clip
            .then(|| {
                state
                    .buffers
                    .top()
                    .map(|b| String::from_utf8_lossy(&b.data).into_owned())
            })
            .flatten();
        let Prompts { open, history, .. } = &mut state.prompts;
        let prompt = open.get_mut(&client)?;
        prompt.vi = vi;
        prompt.separators = separators;
        prompt.clip = clip;
        let history = &history[prompt.kind];
        // `-N` (M5): as tmux 3.7b's `status_prompt_key`, any key but a
        // digit answers the prompt and then goes on as a key (copy mode's
        // counts: `5` then `j`).
        if let (Read::Line, true, Input::Key(key)) = (prompt.read, prompt.numeric, input) {
            let digit = key.mods == Default::default()
                && matches!(key.code, KeyCode::Char(c) if c.is_ascii_digit());
            if !digit {
                let prompt = state.prompts.close(client)?;
                let command = prompt.command(&prompt.line());
                match cmd::split_command_string(&command) {
                    Ok(cmds) if !cmds.is_empty() => {
                        clients::run_binding(state, client, &cmds, prompt.dialect)
                    }
                    Ok(_) => {}
                    Err(e) => clients::show_message(state, client, &e.to_string()),
                }
                clients::mark_dirty(state, client);
                // The key goes on behind whatever holds the client (the
                // binding that opened the prompt), as tmux queues it.
                return clients::hold_behind(state, client, input).then_some(Routed::Consumed);
            }
        }
        // An incremental prompt's template runs again with every change.
        let mut callback: Option<(char, String)> = None;
        let edit = match (prompt.read, input) {
            (Read::Line, Input::Key(key)) if prompt.incremental => {
                let (edit, prefix) = prompt.incremental_key(key, history);
                callback = prefix.map(|p| (p, prompt.line()));
                edit
            }
            (Read::Line, Input::Key(key)) => prompt.key(key, history),
            (Read::Line, Input::Paste { data, .. }) => {
                prompt.insert(&String::from_utf8_lossy(data));
                if prompt.incremental {
                    callback = Some(('=', prompt.line()));
                }
                Edit::Keep
            }
            (Read::Key, Input::Key(key)) => {
                prompt.set_line(&key.to_string());
                Edit::Enter
            }
            (Read::Single | Read::Confirm, Input::Key(key)) => match single_char(key) {
                // As tmux: the keystroke joins the line, and only a line of
                // exactly that one character is an answer.
                Some(c) => {
                    prompt.text.insert(prompt.cursor, c);
                    if prompt.text.len() == 1 {
                        Edit::Enter
                    } else {
                        Edit::Cancel
                    }
                }
                None => Edit::Keep,
            },
            // ponytail: a paste at a one-key prompt is dropped; tmux reads
            // its first character.
            _ => Edit::Keep,
        };
        let routed = match edit {
            Edit::Keep => Routed::Consumed,
            Edit::Complete => {
                complete_word(state, client);
                Routed::Consumed
            }
            Edit::Cancel if prompt.read == Read::Confirm => {
                state.prompts.close_with(client, Err(cancelled()));
                Routed::Consumed
            }
            Edit::Cancel => {
                state.prompts.close(client);
                Routed::Consumed
            }
            Edit::Enter if prompt.read == Read::Confirm && !prompt.confirmed() => {
                state.prompts.close_with(client, Err(cancelled()));
                Routed::Consumed
            }
            // Enter keeps the position the last change found and runs
            // nothing more.
            Edit::Enter if prompt.incremental => {
                let (line, kind) = (prompt.line(), prompt.kind);
                state.prompts.close(client);
                state.prompts.remember(kind, &line);
                Routed::Consumed
            }
            // The next of several prompts.
            Edit::Enter if prompt.read == Read::Line && !prompt.pending.is_empty() => {
                let line = prompt.line();
                prompt.answers.push(line.clone());
                if let Some((label, initial)) = prompt.pending.pop_front() {
                    prompt.label = label;
                    prompt.set_line(&initial);
                    prompt.back = None;
                }
                let kind = prompt.kind;
                state.prompts.remember(kind, &line);
                Routed::Consumed
            }
            Edit::Enter => {
                let mut prompt = state.prompts.close(client)?;
                let line = prompt.line();
                if prompt.read == Read::Line {
                    state.prompts.remember(prompt.kind, &line);
                }
                if let Some(then) = prompt.then.0.take() {
                    let routed = then(state, client, &line);
                    clients::mark_dirty(state, client);
                    return Some(routed);
                }
                let command = match prompt.read {
                    Read::Line | Read::Single | Read::Key => Some(prompt.command(&line)),
                    Read::Confirm => prompt.confirmed().then_some(prompt.template.clone()),
                };
                match command.map(|c| cmd::split_command_string(&c)) {
                    Some(Ok(cmds)) if !cmds.is_empty() => Routed::Command(cmds, prompt.dialect),
                    Some(Err(e)) => {
                        clients::show_message(state, client, &e.to_string());
                        Routed::Consumed
                    }
                    _ => Routed::Consumed,
                }
            }
        };
        if let Some((prefix, line)) = callback {
            run_incremental(state, client, prefix, &line);
        }
        clients::mark_dirty(state, client);
        Some(routed)
    }
}

impl Prompt {
    /// The template with every answer put in: `%1` for the first, `%2` for
    /// the second, `%%` for the first (`args_make_commands`).
    fn command(&self, last: &str) -> String {
        let mut command = self.template.clone();
        for (i, answer) in self
            .answers
            .iter()
            .map(String::as_str)
            .chain([last])
            .enumerate()
        {
            command = replace_template_idx(&command, answer, i + 1);
        }
        command
    }
}

/// Whether client `id`'s session has `status-keys vi`, and its
/// `word-separators`.
fn prompt_env(state: &State, id: ClientId) -> (bool, String) {
    use crate::server::dispatch::options::Owner;
    let Some(c) = state.clients.get(&id) else {
        return (false, WORD_SEPARATORS.to_string());
    };
    let chain = [Owner::Session(c.session)];
    let options = &state.model.options;
    (
        options.lookup("status-keys", &chain) == Some("vi"),
        options
            .lookup("word-separators", &chain)
            .unwrap_or(WORD_SEPARATORS)
            .to_string(),
    )
}

/// Tab: completes the word at the cursor, as tmux's
/// `status_prompt_replace_complete`. The first word completes to a command
/// (or alias), the others to an option or a layout; one candidate
/// completes it (and a command gets a space), several to what they share.
// ponytail: no candidates menu when several share nothing more, and `-t`
// and `-s` words are not completed.
fn complete_word(state: &mut State, client: ClientId) {
    let Some(prompt) = state.prompts.open.get(&client) else {
        return;
    };
    let text = &prompt.text;
    let mut idx = prompt.cursor;
    if idx != 0 {
        idx -= 1;
    }
    let space = |i: usize| text.get(i).is_some_and(|c| *c == ' ');
    let mut first = idx.min(text.len());
    while first > 0 && !space(first) {
        first -= 1;
    }
    while first < text.len() && space(first) {
        first += 1;
    }
    let mut last = idx.min(text.len());
    while last < text.len() && !space(last) {
        last += 1;
    }
    while last > 0 && space(last) {
        last -= 1;
    }
    if last < text.len() {
        last += 1;
    }
    if last < first {
        return;
    }
    let word: String = text[first..last].iter().collect();
    // Only a command prompt completes; an empty word has nothing to.
    if word.is_empty() || prompt.kind != 0 || word.starts_with("-t") || word.starts_with("-s") {
        return;
    }
    let candidates = completions(state, &word, first == 0);
    let out = match candidates.len() {
        0 => return,
        1 => format!("{} ", candidates[0]),
        _ => common_prefix(&candidates),
    };
    if out == word {
        return;
    }
    let Some(prompt) = state.prompts.open.get_mut(&client) else {
        return;
    };
    prompt.text.splice(first..last, out.chars());
    prompt.cursor = first + out.chars().count();
}

/// The candidates for `word` (`status_prompt_complete_list`): commands and
/// their aliases, `command-alias` entries, and for later words options and
/// layouts.
fn completions(state: &State, word: &str, at_start: bool) -> Vec<String> {
    use crate::server::dispatch::options as opts;
    let mut list: Vec<String> = Vec::new();
    let mut add = |s: &str| {
        if s.starts_with(word) && !list.iter().any(|l| l == s) {
            list.push(s.to_string());
        }
    };
    for spec in crate::compat::tmux::specs().iter() {
        add(spec.name);
        if let Some(alias) = spec.alias {
            add(alias);
        }
    }
    if let Some(aliases) = state.model.options.lookup_list("command-alias", &[]) {
        for value in aliases.values() {
            if let Some((name, _)) = value.split_once('=') {
                add(name);
            }
        }
    }
    if !at_start {
        for name in opts::builtin_names().chain(opts::list_names()) {
            add(name);
        }
        for layout in [
            "even-horizontal",
            "even-vertical",
            "main-horizontal",
            "main-horizontal-mirrored",
            "main-vertical",
            "main-vertical-mirrored",
            "tiled",
        ] {
            add(layout);
        }
    }
    list.sort();
    list
}

/// The longest prefix all of `list` share.
fn common_prefix(list: &[String]) -> String {
    let mut out: Vec<char> = list[0].chars().collect();
    for item in &list[1..] {
        let n = out
            .iter()
            .zip(item.chars())
            .take_while(|(a, b)| **a == *b)
            .count();
        out.truncate(n);
    }
    out.into_iter().collect()
}

/// Draws a client's open prompt over its status row, in `message-style`,
/// with the cursor at the insertion point. A line wider than the client
/// scrolls so the cursor stays on screen.
pub struct PromptOverlay;

impl Overlay for PromptOverlay {
    fn draw(&self, state: &State, client: ClientId, cx: &OverlayCtx, screen: &mut Screen) {
        let Some(prompt) = state.prompts.open.get(&client) else {
            return;
        };
        let Some(c) = state.clients.get(&client) else {
            return;
        };
        if screen.cols == 0 || screen.rows == 0 {
            return;
        }
        let row = cx
            .status_row
            .unwrap_or(screen.rows - 1)
            .min(screen.rows - 1);
        let cols = usize::from(screen.cols);
        // `message-style`, or `message-command-style` in vi's command mode,
        // drawn with the label as a message is.
        let model = &state.model;
        let active = model
            .sessions
            .get(&c.session)
            .and_then(|s| model.windows.get(&s.active))
            .map(|w| w.active);
        let chain = active
            .map(|p| crate::server::dispatch::options::chain(model, p))
            .unwrap_or_default();
        let (name, default) = if prompt.command_mode {
            ("message-command-style", MESSAGE_COMMAND_STYLE)
        } else {
            ("message-style", MESSAGE_STYLE)
        };
        let style_text = model.options.lookup(name, &chain).unwrap_or(default);
        let client_vars = ClientVars::new(model, c, active.map(|p| FormatCtx::new(model, p)));
        let server_vars = active.map(|p| ServerVars::new(state, p, &client_vars));
        let vars: &dyn cmd::FormatVars = match &server_vars {
            Some(v) => v,
            None => &client_vars,
        };
        let drawn = status::message_row(&prompt.label, style_text, vars, screen.cols);
        let base = line_cells(&drawn.line, cols);
        let style = status::base_style(style_text, vars);
        let start = visible_width(&prompt.label).min(cols);
        let (cells, col) = prompt.row_over(base, start, &style);
        let at = usize::from(row) * cols;
        screen.cells[at..at + cells.len()].clone_from_slice(&cells);
        screen.cursor = Some(CursorState {
            row,
            col: u16::try_from(col).unwrap_or(u16::MAX),
            visible: true,
            ..CursorState::default()
        });
    }
}

/// Client `id` went away: its prompt closes, and a CLI waiting on it gets
/// an empty success.
// ponytail: tmux 3.7b keeps that CLI waiting until the server exits (then
// exit 0, no output), recorded 2026-09-28 with `detach-client`; grofe
// answers at once instead of leaking it.
pub fn client_gone(state: &mut State, id: ClientId) {
    state.prompts.close(id);
}

/// `command-prompt` (R24: `-p`, `-I`, a `%%` template) on the current
/// client. `-p` and `-I` are formats. A client whose prompt is already open
/// keeps it, as in tmux. From the CLI the reply waits until the prompt
/// closes, as tmux's does, unless `-b`. `-1` answers with the one
/// keystroke typed, `-k` with the name of the one key pressed (TC.7); `-T`
/// picks the history the prompt walks (grofe's prompt has no completion).
pub fn command_prompt(
    state: &mut State,
    ctx: &ClientCtx,
    c: CommandPrompt,
) -> Result<String, String> {
    let id = clients::target_client(state, ctx, c.client.as_deref())?;
    if state.prompts.is_open(id) {
        return Ok(String::new());
    }
    let kind = match c.prompt_type.as_deref() {
        None => 0,
        Some(t) => PROMPT_TYPES
            .iter()
            .position(|k| *k == t)
            .ok_or_else(|| format!("unknown type: {t}"))?,
    };
    let expand = |s: &str| expand_for(state, id, s);
    // `-p a,b` is one prompt each, with the `-I` text of the same place;
    // with none the prompt is `(verb)` or `:`.
    let (raw, space) = match (&c.prompt, &c.template) {
        (Some(p), _) => (p.clone(), true),
        (None, Some(t)) => (format!("({})", verb(t)), true),
        (None, None) => (":".to_string(), false),
    };
    let parts = |text: &str| -> Vec<String> {
        if c.literal {
            vec![text.to_string()]
        } else {
            text.split(',').map(String::from).collect()
        }
    };
    let inputs = c.initial.as_deref().map(parts).unwrap_or_default();
    let mut steps: std::collections::VecDeque<(String, String)> = parts(&raw)
        .into_iter()
        .enumerate()
        .map(|(i, label)| {
            let label = if space { format!("{label} ") } else { label };
            let input = inputs.get(i).map(String::as_str).unwrap_or("");
            (expand(&label), expand(input))
        })
        .collect();
    let (label, initial) = steps.pop_front().unwrap_or_default();
    let dialect = crate::compat::prompt_dialect(state, ctx.dialect, c.template.is_some());
    // `-F` (FE.1): the template is a format, expanded now.
    let template = match c.template {
        Some(t) if c.expand => expand(&t),
        Some(t) => t,
        None => "%1".to_string(),
    };
    // As tmux: `-1` wins over `-N`, which wins over `-i`, then `-k`.
    let incremental = c.incremental && !c.single && !c.numeric;
    let read = match (c.key && !incremental, c.single) {
        (true, _) => Read::Key,
        (false, true) => Read::Single,
        (false, false) => Read::Line,
    };
    let mut prompt = Prompt {
        dialect,
        read,
        numeric: c.numeric && !c.single,
        incremental,
        kind,
        bspace_exit: c.bspace_exit && !c.single && !c.numeric && !incremental && !c.key,
        pending: steps,
        ..Prompt::new(label, &initial, template)
    };
    if incremental {
        prompt.last = initial;
        prompt.set_line("");
    }
    // An incremental prompt never holds its caller, and starts by running
    // its template with a bare `=`.
    open(state, id, prompt, !c.background && !incremental);
    if incremental {
        run_incremental(state, id, '=', "");
    }
    Ok(String::new())
}

/// Opens a prompt on client `id` whose answer goes to `then` instead of a
/// command (a chooser's search, filter, kill confirmation and command): its
/// `label`, starting text, history `kind` ("command" or "search") and
/// whether it takes one keystroke (`single`).
pub fn ask(
    state: &mut State,
    id: ClientId,
    label: &str,
    initial: &str,
    kind: &str,
    single: bool,
    then: PromptFn,
) {
    let kind = PROMPT_TYPES.iter().position(|k| *k == kind).unwrap_or(0);
    let prompt = Prompt {
        read: if single { Read::Single } else { Read::Line },
        kind,
        then: Then(Some(then)),
        ..Prompt::new(label.to_string(), initial, String::new())
    };
    state.prompts.open.insert(id, prompt);
    clients::mark_dirty(state, id);
}

/// An incremental prompt's callback (tmux's `cmd_command_prompt_callback`
/// with `done` 0): its template runs with `prefix` and `line` at `%%`.
fn run_incremental(state: &mut State, client: ClientId, prefix: char, line: &str) {
    let Some(prompt) = state.prompts.open.get(&client) else {
        return;
    };
    let command = replace_template(&prompt.template, &format!("{prefix}{line}"));
    let dialect = prompt.dialect;
    match cmd::split_command_string(&command) {
        Ok(cmds) if !cmds.is_empty() => clients::run_binding(state, client, &cmds, dialect),
        Ok(_) => {}
        Err(e) => clients::show_message(state, client, &e.to_string()),
    }
}

/// `confirm-before`'s prompt on the current client: `-p` (a format,
/// expanded as `command-prompt`'s, then a blank) or `Confirm '<verb>'?
/// (y/n) `, then one keystroke; `command` runs only on the confirm key
/// (`-c`, default `y`; recorded: `n`, `Y` and Enter all cancel), or on
/// Enter with `-y`.
/// From the CLI the reply waits until the prompt closes; a cancelled
/// prompt exits 1 with no message, as tmux's does (recorded).
pub fn confirm(
    state: &mut State,
    ctx: &ClientCtx,
    c: &cmd::ConfirmBefore,
    verb: &str,
    command: String,
) -> Result<String, String> {
    // tmux checks `-c` before it looks for a client.
    let key = match c.confirm_key.as_deref() {
        None => 'y',
        Some(k) => match k.chars().collect::<Vec<_>>()[..] {
            [key] if key.is_ascii() && ('\u{20}'..'\u{7f}').contains(&key) => key,
            _ => return Err("invalid confirm key".to_string()),
        },
    };
    let id = match &c.client {
        Some(name) => clients::client_by_name(state, name)?,
        None => clients::current_client(state, ctx).ok_or("no current client")?,
    };
    if state.prompts.is_open(id) {
        return Ok(String::new());
    }
    let label = match &c.prompt {
        Some(p) => format!("{} ", expand_for(state, id, p)),
        None => format!("Confirm '{verb}'? ({key}/n) "),
    };
    let prompt = Prompt {
        dialect: ctx.dialect,
        read: Read::Confirm,
        confirm_key: key,
        default_yes: c.default_yes,
        ..Prompt::new(label, "", command)
    };
    open(state, id, prompt, !c.background);
    Ok(String::new())
}

/// What a waiting CLI gets when `confirm-before` is cancelled: exit 1,
/// nothing printed.
fn cancelled() -> Failure {
    Failure {
        err: String::new(),
        out: String::new(),
        status: Some(1),
    }
}

/// `text` (`-p`, `-I`) expanded as a format for client `id`, against its
/// current pane, tmux's one-letter aliases (`#W`) included.
fn expand_for(state: &State, id: ClientId, text: &str) -> String {
    let Some(client) = state.clients.get(&id) else {
        return text.to_string();
    };
    let model = &state.model;
    let active = model
        .sessions
        .get(&client.session)
        .and_then(|s| model.windows.get(&s.active))
        .map(|w| w.active);
    let vars = ClientVars::new(model, client, active.map(|p| FormatCtx::new(model, p)));
    let text = long_aliases(text);
    // The pane's mode answers its own variables (`pane_search_string`).
    match active {
        Some(pane) => cmd::expand(&text, &ServerVars::new(state, pane, &vars)),
        None => cmd::expand(&text, &vars),
    }
}

/// Opens `prompt` on client `id`; with `wait` set its caller waits for it
/// to close: a CLI request's reply, and a binding's later commands and its
/// client's input, as tmux 3.7b's command queue waits (recorded,
/// `tm_6/waits`).
fn open(state: &mut State, id: ClientId, prompt: Prompt, wait: bool) {
    state.prompts.open.insert(id, prompt);
    if wait {
        let reply = dispatch::defer_reply(state);
        state.prompts.waiting.insert(id, reply);
    }
    clients::mark_dirty(state, id);
}

/// tmux's one-letter format aliases (`#S`, `#W`, ...) in their `#{...}`
/// form, for the stock `,` and `$` bindings' `-I "#W"` and `-I "#S"`.
// ponytail: belongs in `cmd::expand` (not T3.4's file); only the prompt's
// `-p` and `-I` know the aliases until then.
fn long_aliases(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '#' {
            out.push(c);
            continue;
        }
        let Some(next) = chars.next() else {
            out.push('#');
            break;
        };
        let name = match next {
            'D' => "pane_id",
            'F' => "window_flags",
            'H' => "host",
            'I' => "window_index",
            'P' => "pane_index",
            'S' => "session_name",
            'T' => "pane_title",
            'W' => "window_name",
            'h' => "host_short",
            _ => {
                out.push('#');
                out.push(next);
                continue;
            }
        };
        out.push_str(&format!("#{{{name}}}"));
    }
    out
}

/// The command a template runs, for its prompt label: its first word.
fn verb(template: &str) -> String {
    cmd::split_command_string(template)
        .ok()
        .and_then(|cmds| cmds.into_iter().next()?.into_iter().next())
        .unwrap_or_else(|| template.to_string())
}

/// `run-shell` (R23): `/bin/sh -c` in a task of its own, never waited on
/// by the actor. From the CLI it runs in the CLI's cwd and its stdout is
/// the reply, sent once it exits; from a binding (an attached client) it
/// runs in the session's cwd, its output shows in view mode over the
/// client's pane, and the binding's later commands and the client's later
/// input wait for it, as tmux's client queue does (`clients::run_binding`).
/// With `-b` (TC.5) the reply is empty and immediate, and the output shows
/// in view mode over a pane when it ends (see below). The shell
/// gets the server environment and the session's (`hooks::shell_env`).
/// A failing command's output and tmux's message go to the CLI's stdout,
/// and the CLI exits with the command's status (D20, as tmux 3.7b).
// ponytail: no `-C`.
pub fn run_shell(state: &mut State, ctx: &ClientCtx, c: RunShell) -> Result<String, String> {
    let client_session = ctx
        .client
        .and_then(|id| state.clients.get(&id))
        .map(|client| client.session);
    let session_cwd = client_session
        .and_then(|s| state.model.sessions.get(&s))
        .map(|s| s.cwd.clone());
    let cwd = pane::usable_cwd(session_cwd.unwrap_or_else(|| ctx.cwd.clone()));
    let session =
        client_session.or_else(|| state.model.resolve_session(&cmd::Target::Current, ctx).ok());
    let env = hooks::shell_env(state, session);
    // `-t` (FE.3): the formats expand against that pane, as in tmux.
    let script = match c.target {
        cmd::Target::Current => expand_command(state, ctx, &c.cmd),
        ref target => {
            let pane = state.model.resolve_pane(target, ctx)?;
            let at = ClientCtx {
                pane: Some(pane),
                ..ctx.clone()
            };
            expand_command(state, &at, &c.cmd)
        }
    };
    // Its output is the reply (the CLI prints it, a binding shows it in
    // view mode), except with `-t`, where tmux prints it in that pane's
    // view mode, as it does for `-b` (the pane `cmd_find_from_nothing`
    // finds when there is no `-t`).
    let explicit = (c.target != cmd::Target::Current)
        .then(|| state.model.resolve_pane(&c.target, ctx).ok())
        .flatten();
    let to_pane = explicit.is_some();
    // A binding's output shows over its client's pane, the lines as the
    // job prints them.
    let viewer = ctx.client.filter(|id| state.clients.contains_key(id));
    let reply = (!c.background).then(|| dispatch::defer_reply(state));
    let socket = state.socket.clone();
    let jobs = state.shell_jobs.clone();
    let delay = c.delay;
    let jobs_tx = state.jobs.clone();
    tokio::spawn(async move {
        // `-d` (FE.1): wait first; with no command, only wait.
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        let mut result = match script.is_empty() && delay.is_some() {
            true => Ok(String::new()),
            false => shell(&script, cwd, env, socket, true, &jobs).await,
        };
        if reply.is_none() || to_pane || viewer.is_some() {
            let printed = match &mut result {
                Ok(text) => std::mem::take(text),
                Err(f) => std::mem::take(&mut f.out),
            };
            let job: crate::server::Job = Box::new(move |state: &mut State| {
                crate::server::copy::view::print_shell(state, explicit, viewer, &printed);
            });
            let _ = jobs_tx.send(job).await;
        }
        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
    });
    Ok(String::new())
}

/// `run-shell`'s command, format-expanded as tmux 3.7b expands it: against
/// the current pane and client, and inside a hook its `#{hook*}`
/// variables. Menus expand their items the same way (TC.7).
pub(crate) fn expand_command(state: &State, ctx: &ClientCtx, text: &str) -> String {
    use crate::server::dispatch::options::ServerVars;
    let model = &state.model;
    let hook = hooks::current_formats(state);
    let with_hook = |inner: &dyn cmd::FormatVars| match hook {
        Some(formats) => cmd::expand(
            text,
            &hooks::HookVars {
                formats,
                inner: cmd::Inner::Ref(inner),
            },
        ),
        None => cmd::expand(text, inner),
    };
    let Ok(pane) = model.resolve_pane(&cmd::Target::Current, ctx) else {
        return with_hook(&NoVars);
    };
    match clients::current_client(state, ctx).and_then(|id| state.clients.get(&id)) {
        Some(client) => {
            let vars = ClientVars::new(model, client, Some(FormatCtx::new(model, pane)));
            with_hook(&ServerVars::new(state, pane, &vars))
        }
        None => {
            let vars = FormatCtx::new(model, pane);
            with_hook(&ServerVars::new(state, pane, &vars))
        }
    }
}

struct NoVars;

impl cmd::FormatVars for NoVars {
    fn get(&self, _: &str) -> Option<String> {
        None
    }
}

/// Runs `script` to completion: its stdout (ending in a newline) when
/// `capture` is set, or the error tmux words for a nonzero exit or a
/// signal. stderr is discarded, as in tmux; without `capture` so is
/// stdout, so a chatty bound command costs no server memory. `jobs`
/// tracks the shell while it runs.
pub(crate) async fn shell(
    script: &str,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    socket: PathBuf,
    capture: bool,
    jobs: &ShellJobs,
) -> Result<String, Failure> {
    let stdout = if capture {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let child = {
        let _lock = pane::spawn_lock();
        tokio::process::Command::new("/bin/sh")
            // As tmux's `job_run`: `argv[0]` is `sh`, not the path.
            .arg0("sh")
            .arg("-c")
            .arg(script)
            .current_dir(cwd)
            .env_clear()
            .envs(env)
            // As a pane: a `grofe` run inside talks to this server.
            .env("GROFE", socket)
            .env_remove("GROFE_PANE")
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::null())
            .spawn()
    };
    let child = child.map_err(|e| format!("failed to run command: {e}"))?;
    let tracked = jobs.track(&child);
    let out = child.wait_with_output().await;
    drop(tracked);
    let out = out.map_err(|e| Failure::from(format!("'{script}' failed: {e}")))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    match exit_result(script, out.status) {
        Ok(()) => Ok(text),
        // As tmux 3.7b (recorded): the output, then the message, both on
        // stdout, and the CLI exits with the command's status.
        Err((message, status)) => Err(Failure {
            err: String::new(),
            out: format!("{text}{message}\n"),
            status: Some(status),
        }),
    }
}

/// tmux's message for a failed shell and the status its CLI exits with
/// (128 plus the signal for a killed one).
fn exit_result(script: &str, status: ExitStatus) -> Result<(), (String, i32)> {
    match (status.code(), status.signal()) {
        (Some(0), _) => Ok(()),
        (Some(n), _) => Err((format!("'{script}' returned {n}"), n)),
        (None, Some(sig)) => Err((format!("'{script}' terminated by signal {sig}"), 128 + sig)),
        (None, None) => Err((format!("'{script}' failed"), 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::input::Mods;

    /// `command-prompt -N` (FE.1): only digits reach the text.
    #[test]
    fn a_numeric_prompt_takes_only_digits() {
        let mut p = Prompt {
            numeric: true,
            ..Prompt::new(String::new(), "", "%%".to_string())
        };
        p.insert("a1b2 3");
        assert_eq!(p.line(), "123");
    }

    fn key(code: KeyCode) -> Key {
        Key {
            code,
            mods: Mods::default(),
        }
    }

    fn ctrl(c: char) -> Key {
        Key {
            code: KeyCode::Char(c),
            mods: Mods {
                ctrl: true,
                ..Mods::default()
            },
        }
    }

    fn typed(p: &mut Prompt, s: &str) {
        for c in s.chars() {
            assert_eq!(p.key(&key(KeyCode::Char(c)), &[]), Edit::Keep);
        }
    }

    fn prompt(initial: &str) -> Prompt {
        Prompt::new(":".to_string(), initial, "%1".to_string())
    }

    /// tmux 3.7b's `status_prompt_key` for an incremental prompt: what
    /// runs the template again, and with which prefix.
    #[test]
    fn incremental_keys_run_the_template_as_tmux_does() {
        let mut p = prompt("");
        p.incremental = true;
        p.last = "prev".to_string();
        let history = ["old".to_string(), "new".to_string()];
        let press = |p: &mut Prompt, k: Key| p.incremental_key(&k, &history).1;
        assert_eq!(press(&mut p, ctrl('u')), Some('='), "C-u on an empty line");
        assert_eq!(
            press(&mut p, ctrl('w')),
            Some('='),
            "C-w with nothing to cut"
        );
        assert_eq!(press(&mut p, key(KeyCode::Backspace)), None);
        assert_eq!(press(&mut p, ctrl('r')), Some('='), "brings back -I");
        assert_eq!(p.line(), "prev");
        assert_eq!(press(&mut p, ctrl('r')), Some('-'));
        assert_eq!(press(&mut p, ctrl('s')), Some('+'));
        assert_eq!(press(&mut p, key(KeyCode::Left)), None, "a move only");
        assert_eq!(press(&mut p, key(KeyCode::Char('x'))), Some('='));
        assert_eq!(press(&mut p, key(KeyCode::Down)), Some('='), "empties");
        assert_eq!(p.line(), "");
        assert_eq!(press(&mut p, key(KeyCode::Up)), Some('='));
        assert_eq!(press(&mut p, key(KeyCode::Up)), Some('='));
        assert_eq!(p.line(), "old");
        assert_eq!(press(&mut p, key(KeyCode::Up)), None, "already the oldest");
        assert_eq!(press(&mut p, ctrl('n')), Some('='));
        assert_eq!(p.line(), "new");
        let mut p = prompt("");
        p.incremental = true;
        assert_eq!(p.incremental_key(&key(KeyCode::Up), &[]).1, None);
    }

    #[test]
    fn printable_keys_insert_at_the_cursor() {
        let mut p = prompt("");
        typed(&mut p, "splt");
        p.key(&key(KeyCode::Left), &[]);
        typed(&mut p, "i");
        assert_eq!(p.line(), "split");
        assert_eq!(p.cursor, 4);
        let shifted = Key {
            code: KeyCode::Char('a'),
            mods: Mods {
                shift: true,
                ..Mods::default()
            },
        };
        p.key(&key(KeyCode::End), &[]);
        p.key(&shifted, &[]);
        assert_eq!(p.line(), "splitA");
    }

    #[test]
    fn motion_and_deletion_keys() {
        let mut p = prompt("abcdef");
        p.key(&ctrl('a'), &[]);
        assert_eq!(p.cursor, 0);
        p.key(&ctrl('f'), &[]);
        p.key(&key(KeyCode::Right), &[]);
        assert_eq!(p.cursor, 2);
        p.key(&ctrl('d'), &[]);
        assert_eq!(p.line(), "abdef");
        p.key(&key(KeyCode::Delete), &[]);
        assert_eq!(p.line(), "abef");
        p.key(&key(KeyCode::Backspace), &[]);
        assert_eq!((p.line().as_str(), p.cursor), ("aef", 1));
        p.key(&ctrl('h'), &[]);
        assert_eq!((p.line().as_str(), p.cursor), ("ef", 0));
        p.key(&ctrl('h'), &[]);
        assert_eq!(p.line(), "ef");
        p.key(&ctrl('e'), &[]);
        assert_eq!(p.cursor, 2);
        p.key(&ctrl('b'), &[]);
        p.key(&ctrl('k'), &[]);
        assert_eq!(p.line(), "e");
        p.key(&key(KeyCode::Home), &[]);
        p.key(&key(KeyCode::End), &[]);
        assert_eq!(p.cursor, 1);
        p.key(&ctrl('u'), &[]);
        assert_eq!((p.line().as_str(), p.cursor), ("", 0));
    }

    /// Recorded on tmux 3.7b: `split-window -x` then `C-w` leaves
    /// `split-window -`, a second `C-w` leaves `split-window `.
    #[test]
    fn ctrl_w_stops_at_word_separators_as_tmux() {
        let mut p = prompt("split-window -x");
        p.key(&ctrl('w'), &[]);
        assert_eq!(p.line(), "split-window -");
        p.key(&ctrl('w'), &[]);
        assert_eq!(p.line(), "split-window ");
        p.key(&ctrl('w'), &[]);
        assert_eq!(p.line(), "split-");
        let mut p = prompt("a  b   ");
        p.key(&ctrl('w'), &[]);
        assert_eq!(p.line(), "a  ");
        let mut p = prompt("");
        p.key(&ctrl('w'), &[]);
        assert_eq!(p.line(), "");
    }

    #[test]
    fn enter_and_cancel_keys() {
        let mut p = prompt("x");
        assert_eq!(p.key(&key(KeyCode::Enter), &[]), Edit::Enter);
        assert_eq!(p.key(&ctrl('m'), &[]), Edit::Enter);
        assert_eq!(p.key(&ctrl('j'), &[]), Edit::Enter);
        assert_eq!(p.key(&key(KeyCode::Escape), &[]), Edit::Cancel);
        assert_eq!(p.key(&ctrl('c'), &[]), Edit::Cancel);
        assert_eq!(p.key(&ctrl('g'), &[]), Edit::Cancel);
        // Unhandled keys are eaten and change nothing.
        assert_eq!(p.key(&key(KeyCode::F(1)), &[]), Edit::Keep);
        // Tab asks the hook to complete.
        assert_eq!(p.key(&key(KeyCode::Tab), &[]), Edit::Complete);
        assert_eq!(p.line(), "x");
    }

    /// Recorded on tmux 3.7b: Up walks back, Down forward, then an empty
    /// line.
    #[test]
    fn up_and_down_walk_the_history() {
        let history = ["one".to_string(), "two".to_string()];
        let mut p = prompt("draft");
        p.key(&key(KeyCode::Down), &history);
        assert_eq!(p.line(), "", "Down past the newest empties the line");
        p.key(&key(KeyCode::Up), &history);
        assert_eq!(p.line(), "two");
        p.key(&key(KeyCode::Up), &history);
        assert_eq!(p.line(), "one");
        p.key(&key(KeyCode::Up), &history);
        assert_eq!(p.line(), "one");
        assert_eq!(p.cursor, 3);
        p.key(&key(KeyCode::Down), &history);
        assert_eq!(p.line(), "two");
        p.key(&ctrl('n'), &history);
        assert_eq!(p.line(), "");
        p.key(&ctrl('p'), &history);
        assert_eq!(p.line(), "two");
        let mut p = prompt("");
        p.key(&key(KeyCode::Up), &[]);
        assert_eq!(p.line(), "");
    }

    #[test]
    fn history_skips_empty_lines_and_repeats_and_is_capped() {
        let mut prompts = Prompts::default();
        prompts.remember(0, "a");
        prompts.remember(0, "a");
        prompts.remember(0, "");
        prompts.remember(0, "b");
        prompts.remember(0, "a");
        prompts.remember(1, "search");
        assert_eq!(prompts.history[0], ["a", "b", "a"]);
        assert_eq!(prompts.history[1], ["search"], "a history per type");
        for i in 0..HISTORY_LIMIT + 5 {
            prompts.remember(0, &i.to_string());
        }
        assert_eq!(prompts.history[0].len(), HISTORY_LIMIT);
        assert_eq!(prompts.history[0][0], "5");
        assert_eq!(
            prompts.history[0].last().map(String::as_str),
            Some((HISTORY_LIMIT + 4).to_string().as_str())
        );
    }

    fn row_text(p: &Prompt, cols: usize) -> (String, u16) {
        let (cells, col) = p.row(cols);
        assert_eq!(cells.len(), cols);
        let text = cells
            .iter()
            .filter(|c| !c.flags.contains(CellFlags::WIDE_CHAR_SPACER))
            .map(|c| c.c)
            .collect();
        (text, col)
    }

    #[test]
    fn the_row_is_the_label_then_the_text_with_the_cursor() {
        let mut p = prompt("abc");
        assert_eq!(row_text(&p, 6), (":abc  ".to_string(), 4));
        p.key(&ctrl('a'), &[]);
        assert_eq!(row_text(&p, 6), (":abc  ".to_string(), 1));
        let style = status::parse_style(MESSAGE_STYLE);
        assert!(p.row(6).0.iter().all(|c| c.bg == style.bg));
        let p = Prompt::new("(rename-window) ".to_string(), "old", String::new());
        assert_eq!(row_text(&p, 20).0, "(rename-window) old ");
    }

    #[test]
    fn a_long_line_scrolls_to_keep_the_cursor_on_screen() {
        let mut p = prompt("abcdefghij");
        // The label stays; the text scrolls to keep the cursor in view.
        assert_eq!(row_text(&p, 10), (":cdefghij ".to_string(), 9));
        p.key(&ctrl('a'), &[]);
        assert_eq!(row_text(&p, 10), (":abcdefghi".to_string(), 1));
    }

    #[test]
    fn wide_characters_take_two_cells() {
        let p = prompt("日x");
        let (cells, col) = p.row(6);
        assert!(cells[1].flags.contains(CellFlags::WIDE_CHAR));
        assert!(cells[2].flags.contains(CellFlags::WIDE_CHAR_SPACER));
        assert_eq!(cells[3].c, 'x');
        assert_eq!(col, 4);
    }

    #[test]
    fn template_replacement_follows_tmux() {
        assert_eq!(replace_template("%1", "split-window -h"), "split-window -h");
        assert_eq!(
            replace_template("rename-window \"%%\"", "old"),
            "rename-window \"old\""
        );
        // `%%` only the first time; `%1` every time.
        assert_eq!(replace_template("%% %%", "x"), "x %%");
        assert_eq!(replace_template("%1 %1", "x"), "x x");
        // `%%%` escapes for double quotes.
        assert_eq!(replace_template("\"%%%\"", "a\"$b;"), "\"a\\\"\\$b\\;\"");
        // No `%`: the template as it is; other `%` sequences stay.
        assert_eq!(replace_template("kill-window", "x"), "kill-window");
        assert_eq!(replace_template("%2 %a", "x"), "%2 %a");
    }

    fn meta(c: char) -> Key {
        Key {
            code: KeyCode::Char(c),
            mods: crate::server::input::Mods {
                alt: true,
                ..Default::default()
            },
        }
    }

    /// `M-b` and `M-f` move by words, as tmux's `status_prompt_backward_word`
    /// and `_forward_word` (emacs: past the end of the word).
    #[test]
    fn meta_b_and_f_move_by_words() {
        let mut p = prompt("one two-three  four");
        p.key(&meta('b'), &[]);
        assert_eq!(p.cursor, 15);
        p.key(&meta('b'), &[]);
        assert_eq!(p.cursor, 8);
        p.key(&meta('b'), &[]);
        assert_eq!(p.cursor, 7);
        p.key(&meta('f'), &[]);
        assert_eq!(p.cursor, 8);
        p.key(&meta('f'), &[]);
        assert_eq!(p.cursor, 13);
    }

    /// `status-keys vi`: typing, Escape to command mode, `0`, `w`, `x`, and
    /// `i` back to typing.
    #[test]
    fn vi_keys_edit_in_two_modes() {
        let mut p = prompt("");
        p.vi = true;
        typed(&mut p, "abc def");
        p.key(&key(KeyCode::Escape), &[]);
        assert!(p.command_mode);
        assert_eq!(p.cursor, 6);
        p.key(&plain('0'), &[]);
        assert_eq!(p.cursor, 0);
        p.key(&plain('w'), &[]);
        assert_eq!(p.cursor, 4);
        p.key(&plain('x'), &[]);
        assert_eq!(p.line(), "abc ef");
        p.key(&plain('i'), &[]);
        assert!(!p.command_mode);
        typed(&mut p, "D");
        assert_eq!(p.line(), "abc Def");
        // `d` empties the line, `S` empties it and types.
        p.key(&key(KeyCode::Escape), &[]);
        p.key(&plain('S'), &[]);
        assert_eq!((p.line().as_str(), p.command_mode), ("", false));
    }

    fn plain(c: char) -> Key {
        key(KeyCode::Char(c))
    }

    #[test]
    fn several_answers_fill_percent_one_and_two() {
        let mut p = Prompt::new(String::new(), "", "swap %1 %2 \"%%\"".to_string());
        p.answers.push("a".to_string());
        assert_eq!(p.command("b"), "swap a b \"a\"");
    }

    #[test]
    fn tab_completes_command_names() {
        let list = |w: &str| completions(&crate::server::test_state(), w, true);
        assert_eq!(list("split-w"), ["split-window"]);
        assert_eq!(common_prefix(&list("new-")), "new-");
        assert!(list("new-").contains(&"new-window".to_string()));
        assert!(list("neww").contains(&"neww".to_string()));
    }

    #[test]
    fn one_letter_aliases_become_long_formats() {
        assert_eq!(long_aliases("#W"), "#{window_name}");
        assert_eq!(
            long_aliases("#S:#I.#P"),
            "#{session_name}:#{window_index}.#{pane_index}"
        );
        // `##` and `#{` pass through for `cmd::expand`.
        assert_eq!(long_aliases("##W #{x} #"), "##W #{x} #");
    }

    #[test]
    fn the_label_for_a_template_is_its_verb() {
        assert_eq!(verb("rename-window \"%%\""), "rename-window");
        assert_eq!(verb(""), "");
    }

    /// Runs `input` through the prompt hook for a prompt open on client 1.
    fn feed(state: &mut State, input: Input) -> Option<Routed> {
        PromptHook.on_input(state, ClientId(1), &input, Instant::now())
    }

    fn open_prompt(state: &mut State, read: Read, initial: &str, template: &str) {
        let prompt = Prompt {
            read,
            ..Prompt::new("x ".to_string(), initial, template.to_string())
        };
        state.prompts.open.insert(ClientId(1), prompt);
    }

    fn command(words: &[&str]) -> Option<Routed> {
        let argv = words.iter().map(|w| w.to_string()).collect();
        Some(Routed::Command(vec![argv], Dialect::Grofe))
    }

    /// Recorded on tmux 3.7b: `-1` answers with the character typed, a
    /// control key with its byte; an arrow types nothing and the prompt
    /// stays; with `-I` text the line is longer than one character, so the
    /// prompt closes without running anything.
    #[test]
    fn a_single_key_prompt_answers_with_one_keystroke() {
        let mut state = crate::server::test_state();
        let template = "display-message 'got %%'";
        open_prompt(&mut state, Read::Single, "", template);
        let got = feed(&mut state, Input::Key(key(KeyCode::Char('x'))));
        assert_eq!(got, command(&["display-message", "got x"]));
        assert!(!state.prompts.is_open(ClientId(1)));

        open_prompt(&mut state, Read::Single, "", template);
        let got = feed(&mut state, Input::Key(key(KeyCode::Left)));
        assert_eq!(got, Some(Routed::Consumed));
        assert!(state.prompts.is_open(ClientId(1)));
        let got = feed(&mut state, Input::Key(key(KeyCode::Enter)));
        assert_eq!(got, command(&["display-message", "got \r"]));

        open_prompt(&mut state, Read::Single, "abc", template);
        let got = feed(&mut state, Input::Key(key(KeyCode::Char('z'))));
        assert_eq!(got, Some(Routed::Consumed));
        assert!(!state.prompts.is_open(ClientId(1)));
        // Nothing enters the history.
        assert!(state.prompts.history.iter().all(Vec::is_empty));

        assert_eq!(single_char(&ctrl('x')), Some('\x18'));
        assert_eq!(single_char(&key(KeyCode::Backspace)), Some('\x7f'));
        assert_eq!(single_char(&key(KeyCode::Escape)), Some('\x1b'));
        assert_eq!(single_char(&key(KeyCode::F(1))), None);
        let meta = Key {
            code: KeyCode::Char('q'),
            mods: Mods {
                alt: true,
                ..Mods::default()
            },
        };
        assert_eq!(single_char(&meta), Some('q'));
    }

    /// Recorded on tmux 3.7b: `-k` answers with the key's name, arrows and
    /// function keys included.
    #[test]
    fn a_key_prompt_answers_with_the_keys_name() {
        let mut state = crate::server::test_state();
        let template = "display-message 'got %%'";
        open_prompt(&mut state, Read::Key, "xyz", template);
        let got = feed(&mut state, Input::Key(ctrl('x')));
        assert_eq!(got, command(&["display-message", "got C-x"]));
        open_prompt(&mut state, Read::Key, "", template);
        let got = feed(&mut state, Input::Key(key(KeyCode::Left)));
        assert_eq!(got, command(&["display-message", "got Left"]));
        assert!(!state.prompts.is_open(ClientId(1)));
    }

    /// Recorded on tmux 3.7b: a waiting `confirm-before` CLI exits 0 on `y`
    /// and 1, silently, on anything else.
    #[test]
    fn a_cancelled_confirm_answers_its_cli_with_status_1() {
        let mut state = crate::server::test_state();
        for (k, want) in [
            (key(KeyCode::Char('n')), Err(cancelled())),
            (key(KeyCode::Escape), Err(cancelled())),
            (key(KeyCode::Char('y')), Ok(String::new())),
        ] {
            open_prompt(&mut state, Read::Confirm, "", "kill-pane");
            let (tx, mut rx) = oneshot::channel();
            state.prompts.waiting.insert(ClientId(1), tx);
            feed(&mut state, Input::Key(k));
            assert_eq!(rx.try_recv().expect("answered"), want, "{k:?}");
        }
    }

    /// Recorded on tmux 3.7b: only `y` confirms; `n`, `Y` and Enter cancel;
    /// an arrow leaves the prompt open.
    #[test]
    fn confirm_runs_its_command_only_on_y() {
        let mut state = crate::server::test_state();
        for (k, want) in [
            (key(KeyCode::Char('n')), Some(Routed::Consumed)),
            (key(KeyCode::Char('Y')), Some(Routed::Consumed)),
            (key(KeyCode::Enter), Some(Routed::Consumed)),
            (key(KeyCode::Char('y')), command(&["kill-pane", "-t", "%1"])),
        ] {
            open_prompt(&mut state, Read::Confirm, "", "kill-pane -t %1");
            assert_eq!(feed(&mut state, Input::Key(k)), want, "{k:?}");
            assert!(!state.prompts.is_open(ClientId(1)));
        }
        open_prompt(&mut state, Read::Confirm, "", "kill-pane");
        assert_eq!(
            feed(&mut state, Input::Key(key(KeyCode::Up))),
            Some(Routed::Consumed)
        );
        assert!(state.prompts.is_open(ClientId(1)));
    }

    #[test]
    fn exit_statuses_word_their_errors_as_tmux() {
        assert_eq!(exit_result("true", ExitStatus::from_raw(0)), Ok(()));
        assert_eq!(
            exit_result("false", ExitStatus::from_raw(1 << 8)),
            Err(("'false' returned 1".to_string(), 1))
        );
        assert_eq!(
            exit_result("kill", ExitStatus::from_raw(15)),
            Err(("'kill' terminated by signal 15".to_string(), 143))
        );
    }

    fn failed(out: &str, status: i32) -> Failure {
        Failure {
            err: String::new(),
            out: out.to_string(),
            status: Some(status),
        }
    }

    /// tmux's `job_run` without `JOB_DEFAULTSHELL` (`run-shell`, `if-shell`)
    /// runs `/bin/sh` with `argv[0]` `sh`, whatever `default-shell` is.
    #[tokio::test]
    async fn shell_runs_bin_sh_as_sh() {
        let dir = std::env::temp_dir().canonicalize().expect("temp dir");
        let got = shell(
            "echo \"$0\"",
            dir,
            Vec::new(),
            PathBuf::from("/x"),
            true,
            &ShellJobs::default(),
        )
        .await;
        assert_eq!(got, Ok("sh\n".to_string()));
    }

    #[tokio::test]
    async fn shell_answers_with_stdout_and_runs_in_its_cwd() {
        let dir = std::env::temp_dir().canonicalize().expect("temp dir");
        let socket = PathBuf::from("/nonexistent.sock");
        assert_eq!(
            shell(
                "echo hi; echo two >&1; echo no >&2",
                dir.clone(),
                Vec::new(),
                socket.clone(),
                true,
                &ShellJobs::default()
            )
            .await,
            Ok("hi\ntwo\n".to_string())
        );
        assert_eq!(
            shell(
                "printf abc",
                dir.clone(),
                Vec::new(),
                socket.clone(),
                true,
                &ShellJobs::default()
            )
            .await,
            Ok("abc\n".to_string())
        );
        assert_eq!(
            shell(
                "true",
                dir.clone(),
                Vec::new(),
                socket.clone(),
                true,
                &ShellJobs::default()
            )
            .await,
            Ok(String::new())
        );
        let pwd = shell(
            "pwd -P",
            dir.clone(),
            Vec::new(),
            socket.clone(),
            true,
            &ShellJobs::default(),
        )
        .await;
        assert_eq!(pwd, Ok(format!("{}\n", dir.display())));
        assert_eq!(
            shell(
                "echo \"$GROFE\"",
                dir.clone(),
                Vec::new(),
                socket,
                true,
                &ShellJobs::default()
            )
            .await,
            Ok("/nonexistent.sock\n".to_string())
        );
        assert_eq!(
            shell(
                "exit 3",
                dir.clone(),
                Vec::new(),
                PathBuf::from("/x"),
                true,
                &ShellJobs::default()
            )
            .await,
            Err(failed("'exit 3' returned 3\n", 3))
        );
        assert_eq!(
            shell(
                "echo a; exit 3",
                dir,
                Vec::new(),
                PathBuf::from("/x"),
                true,
                &ShellJobs::default()
            )
            .await,
            Err(failed("a\n'echo a; exit 3' returned 3\n", 3))
        );
        // Without capture (a binding) stdout goes nowhere.
        let dir = std::env::temp_dir();
        assert_eq!(
            shell(
                "echo dropped",
                dir.clone(),
                Vec::new(),
                PathBuf::from("/x"),
                false,
                &ShellJobs::default()
            )
            .await,
            Ok(String::new())
        );
        assert_eq!(
            shell(
                "echo a; exit 2",
                dir,
                Vec::new(),
                PathBuf::from("/x"),
                false,
                &ShellJobs::default()
            )
            .await,
            Err(failed("'echo a; exit 2' returned 2\n", 2))
        );
    }
}
