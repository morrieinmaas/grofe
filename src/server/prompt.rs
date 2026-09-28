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

use unicode_width::UnicodeWidthChar;

use crate::cmd::{self, CommandPrompt, RunShell};
use crate::protocol::ClientCtx;
use crate::server::State;
use crate::server::clients::{self, ClientVars};
use crate::server::dispatch::{self, options::FormatCtx};
use crate::server::input::{Input, InputHook, Key, KeyCode, Routed};
use crate::server::pane::{self, CellFlags, CursorState, SnapCell};
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::server::status;
use crate::types::ClientId;

/// tmux's `prompt-history-limit`.
pub const HISTORY_LIMIT: usize = 100;

/// How the prompt row looks: tmux's `message-style`.
// ponytail: fixed until `message-style` is an option (TC.11's message area).
const PROMPT_STYLE: &str = "bg=yellow,fg=black";

/// tmux 3.7b's default `word-separators`, which `C-w` stops at.
// ponytail: fixed until `word-separators` is an option (M5 copy mode).
const WORD_SEPARATORS: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~";

/// Every client's open prompt and the server-wide history.
#[derive(Debug, Default)]
pub struct Prompts {
    open: BTreeMap<ClientId, Prompt>,
    /// Lines entered at any prompt, newest last.
    history: Vec<String>,
}

impl Prompts {
    /// Whether client `id` has a prompt open.
    pub fn is_open(&self, id: ClientId) -> bool {
        self.open.contains_key(&id)
    }

    /// Adds an entered line: empty lines and a repeat of the newest are
    /// skipped, the oldest go past [`HISTORY_LIMIT`].
    fn remember(&mut self, line: &str) {
        if line.is_empty() || self.history.last().is_some_and(|l| l == line) {
            return;
        }
        self.history.push(line.to_string());
        let over = self.history.len().saturating_sub(HISTORY_LIMIT);
        self.history.drain(..over);
    }
}

/// One open prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

/// What a key did to a prompt.
#[derive(Debug, PartialEq, Eq)]
enum Edit {
    Keep,
    Cancel,
    Enter,
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
        }
    }

    fn line(&self) -> String {
        self.text.iter().collect()
    }

    fn set_line(&mut self, line: &str) {
        self.text = line.chars().collect();
        self.cursor = self.text.len();
    }

    fn insert(&mut self, s: &str) {
        for c in s.chars().filter(|c| !c.is_control()) {
            self.text.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    /// tmux's emacs `status-keys`.
    fn key(&mut self, key: &Key, history: &[String]) -> Edit {
        let m = key.mods;
        let ctrl_only = m.ctrl && !m.alt && !m.super_;
        match key.code {
            KeyCode::Enter => return Edit::Enter,
            KeyCode::Escape => return Edit::Cancel,
            KeyCode::Char(c) if ctrl_only => match c.to_ascii_lowercase() {
                'j' | 'm' => return Edit::Enter,
                'c' | 'g' => return Edit::Cancel,
                'b' => self.left(),
                'f' => self.right(),
                'a' => self.cursor = 0,
                'e' => self.cursor = self.text.len(),
                'h' => self.backspace(),
                'd' => self.delete(),
                'k' => self.text.truncate(self.cursor),
                'u' => {
                    self.text.clear();
                    self.cursor = 0;
                }
                'w' => self.delete_word(),
                'p' => self.older(history),
                'n' => self.newer(history),
                _ => {}
            },
            KeyCode::Char(c) if !m.ctrl && !m.alt && !m.super_ => {
                let c = if m.shift { c.to_ascii_uppercase() } else { c };
                self.insert(c.encode_utf8(&mut [0; 4]));
            }
            KeyCode::Left if !m.ctrl && !m.alt => self.left(),
            KeyCode::Right if !m.ctrl && !m.alt => self.right(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Up => self.older(history),
            KeyCode::Down => self.newer(history),
            // ponytail: no Tab completion, no word motions (M-b, M-f), no
            // `status-keys vi`.
            _ => {}
        }
        Edit::Keep
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

    /// `C-w`, as tmux 3.7b: skip whitespace back, then delete the run of
    /// characters that are all separators or all not.
    fn delete_word(&mut self) {
        let is_sep = |c: char| WORD_SEPARATORS.contains(c);
        let mut idx = self.cursor;
        while idx > 0 && self.text[idx - 1].is_whitespace() {
            idx -= 1;
        }
        if idx > 0 {
            let seps = is_sep(self.text[idx - 1]);
            while idx > 0
                && !self.text[idx - 1].is_whitespace()
                && is_sep(self.text[idx - 1]) == seps
            {
                idx -= 1;
            }
        }
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

    /// Down: the next newer entry, then an empty line, as tmux.
    fn newer(&mut self, history: &[String]) {
        match self.back {
            None => {}
            Some(0) => {
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

    /// The prompt row, `cols` cells in `message-style`, and the cursor's
    /// column: the label then the text, scrolled left when the cursor
    /// would be past the edge.
    fn row(&self, cols: usize) -> (Vec<SnapCell>, u16) {
        let style = status::parse_style(PROMPT_STYLE);
        let blank = SnapCell {
            c: ' ',
            fg: style.fg,
            bg: style.bg,
            flags: style.attrs,
            zerowidth: Vec::new(),
            underline_color: style.underline_color,
            hyperlink: None,
        };
        let mut cells: Vec<SnapCell> = Vec::new();
        let mut cursor = None;
        let label_len = self.label.chars().count();
        let chars = self.label.chars().chain(self.text.iter().copied());
        for (i, c) in chars.enumerate() {
            if i == label_len + self.cursor {
                cursor = Some(cells.len());
            }
            let Some(width) = UnicodeWidthChar::width(c) else {
                continue;
            };
            if width == 0 {
                if let Some(last) = cells.last_mut() {
                    last.zerowidth.push(c);
                }
                continue;
            }
            let wide = if width == 2 {
                CellFlags::WIDE_CHAR
            } else {
                CellFlags::empty()
            };
            cells.push(SnapCell {
                c,
                flags: style.attrs | wide,
                ..blank.clone()
            });
            if width == 2 {
                cells.push(SnapCell {
                    flags: style.attrs | CellFlags::WIDE_CHAR_SPACER,
                    ..blank.clone()
                });
            }
        }
        let cursor = cursor.unwrap_or(cells.len());
        // A wide pair cut at the left edge is made whole by the caller's
        // `fix_wide_pairs`.
        let start = (cursor + 1).saturating_sub(cols);
        let row = (start..start + cols)
            .map(|x| cells.get(x).cloned().unwrap_or_else(|| blank.clone()))
            .collect();
        (row, u16::try_from(cursor - start).unwrap_or(u16::MAX))
    }
}

/// tmux's `cmd_template_replace` for the first answer: `%1` is replaced
/// everywhere, `%%` the first time only; `%%%` (and `%1%`) also escapes
/// `"`, `\`, `$`, `;` and `~` in the answer. A template without `%` is
/// kept as it is.
fn replace_template(template: &str, answer: &str) -> String {
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
                Some('1') => true,
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
        let Prompts { open, history } = &mut state.prompts;
        let prompt = open.get_mut(&client)?;
        let edit = match input {
            Input::Key(key) => prompt.key(key, history),
            Input::Paste { data, .. } => {
                prompt.insert(&String::from_utf8_lossy(data));
                Edit::Keep
            }
            Input::Mouse(_) => Edit::Keep,
            Input::Focus(_) => return None,
        };
        let routed = match edit {
            Edit::Keep => Routed::Consumed,
            Edit::Cancel => {
                open.remove(&client);
                Routed::Consumed
            }
            Edit::Enter => {
                let prompt = open.remove(&client)?;
                let line = prompt.line();
                state.prompts.remember(&line);
                let command = replace_template(&prompt.template, &line);
                match cmd::split_command_string(&command) {
                    Ok(cmds) if !cmds.is_empty() => Routed::Command(cmds),
                    Ok(_) => Routed::Consumed,
                    Err(e) => {
                        clients::show_message(state, client, &e.to_string());
                        Routed::Consumed
                    }
                }
            }
        };
        clients::mark_dirty(state, client);
        Some(routed)
    }
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
        if screen.cols == 0 || screen.rows == 0 {
            return;
        }
        let row = cx
            .status_row
            .unwrap_or(screen.rows - 1)
            .min(screen.rows - 1);
        let (cells, col) = prompt.row(usize::from(screen.cols));
        let base = usize::from(row) * usize::from(screen.cols);
        screen.cells[base..base + cells.len()].clone_from_slice(&cells);
        screen.cursor = Some(CursorState {
            row,
            col,
            visible: true,
            ..CursorState::default()
        });
    }
}

/// `command-prompt` (R24: `-p`, `-I`, a `%%` template) on the current
/// client. `-p` and `-I` are formats. A client whose prompt is already open
/// keeps it, as in tmux.
// ponytail: from the CLI tmux waits for the prompt to close unless `-b`;
// grofe answers at once. `-k`, `-1`, `-T`, several `-p` prompts: TC.7.
pub fn command_prompt(
    state: &mut State,
    ctx: &ClientCtx,
    c: CommandPrompt,
) -> Result<String, String> {
    // Prompts of clients that went away.
    let live = &state.clients;
    state.prompts.open.retain(|id, _| live.contains_key(id));
    let Some(id) = clients::current_client(state, ctx) else {
        return Err("no current client".to_string());
    };
    if state.prompts.is_open(id) {
        return Ok(String::new());
    }
    let expand = |s: &str| {
        let Some(client) = state.clients.get(&id) else {
            return s.to_string();
        };
        let model = &state.model;
        let pane = model
            .sessions
            .get(&client.session)
            .and_then(|s| model.windows.get(&s.active))
            .map(|w| FormatCtx::new(model, w.active));
        cmd::expand(s, &ClientVars::new(model, client, pane))
    };
    let label = match (&c.prompt, &c.template) {
        (Some(p), _) => format!("{} ", expand(p)),
        (None, Some(t)) => format!("({}) ", verb(t)),
        (None, None) => ":".to_string(),
    };
    let initial = c.initial.as_deref().map(expand).unwrap_or_default();
    let template = c.template.unwrap_or_else(|| "%1".to_string());
    state
        .prompts
        .open
        .insert(id, Prompt::new(label, &initial, template));
    clients::mark_dirty(state, id);
    Ok(String::new())
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
/// runs in the session's cwd and its output is dropped.
// ponytail: tmux prints the output of a failing command too and exits
// with its status; grofe answers with the error alone and exits 1. From a
// binding the output would go to view mode (M5). No `-b`, `-d`, `-C`
// (`-b` is TC.5's).
pub fn run_shell(state: &mut State, ctx: &ClientCtx, c: RunShell) -> Result<String, String> {
    let session_cwd = ctx
        .client
        .and_then(|id| state.clients.get(&id))
        .map(|client| client.session)
        .and_then(|s| state.model.sessions.get(&s))
        .map(|s| s.cwd.clone());
    let from_binding = session_cwd.is_some();
    let cwd = pane::usable_cwd(session_cwd.unwrap_or_else(|| ctx.cwd.clone()));
    let reply = (!from_binding).then(|| dispatch::defer_reply(state));
    let socket = state.socket.clone();
    tokio::spawn(async move {
        let result = shell(&c.cmd, cwd, socket).await;
        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
    });
    Ok(String::new())
}

/// Runs `script` to completion: its stdout (ending in a newline), or the
/// error tmux words for a nonzero exit or a signal. stderr is discarded,
/// as in tmux.
async fn shell(script: &str, cwd: PathBuf, socket: PathBuf) -> Result<String, String> {
    let child = {
        let _lock = pane::spawn_lock();
        tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .current_dir(cwd)
            // As a pane: a `grofe` run inside talks to this server.
            .env("GROFE", socket)
            .env_remove("GROFE_PANE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    };
    let child = child.map_err(|e| format!("failed to run command: {e}"))?;
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| format!("'{script}' failed: {e}"))?;
    exit_result(script, out.status)?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

fn exit_result(script: &str, status: ExitStatus) -> Result<(), String> {
    match (status.code(), status.signal()) {
        (Some(0), _) => Ok(()),
        (Some(n), _) => Err(format!("'{script}' returned {n}")),
        (None, Some(sig)) => Err(format!("'{script}' terminated by signal {sig}")),
        (None, None) => Err(format!("'{script}' failed")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::input::Mods;

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
        assert_eq!(p.key(&key(KeyCode::Tab), &[]), Edit::Keep);
        assert_eq!(p.line(), "x");
    }

    /// Recorded on tmux 3.7b: Up walks back, Down forward, then an empty
    /// line.
    #[test]
    fn up_and_down_walk_the_history() {
        let history = ["one".to_string(), "two".to_string()];
        let mut p = prompt("draft");
        p.key(&key(KeyCode::Down), &history);
        assert_eq!(p.line(), "draft");
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
        prompts.remember("a");
        prompts.remember("a");
        prompts.remember("");
        prompts.remember("b");
        prompts.remember("a");
        assert_eq!(prompts.history, ["a", "b", "a"]);
        for i in 0..HISTORY_LIMIT + 5 {
            prompts.remember(&i.to_string());
        }
        assert_eq!(prompts.history.len(), HISTORY_LIMIT);
        assert_eq!(prompts.history[0], "5");
        assert_eq!(
            prompts.history.last().map(String::as_str),
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
        let style = status::parse_style(PROMPT_STYLE);
        assert!(p.row(6).0.iter().all(|c| c.bg == style.bg));
        let p = Prompt::new("(rename-window) ".to_string(), "old", String::new());
        assert_eq!(row_text(&p, 20).0, "(rename-window) old ");
    }

    #[test]
    fn a_long_line_scrolls_to_keep_the_cursor_on_screen() {
        let mut p = prompt("abcdefghij");
        // 11 cells and the cursor after them: two scrolled off.
        assert_eq!(row_text(&p, 10), ("bcdefghij ".to_string(), 9));
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

    #[test]
    fn the_label_for_a_template_is_its_verb() {
        assert_eq!(verb("rename-window \"%%\""), "rename-window");
        assert_eq!(verb(""), "");
    }

    #[test]
    fn exit_statuses_word_their_errors_as_tmux() {
        assert_eq!(exit_result("true", ExitStatus::from_raw(0)), Ok(()));
        assert_eq!(
            exit_result("false", ExitStatus::from_raw(1 << 8)),
            Err("'false' returned 1".to_string())
        );
        assert_eq!(
            exit_result("kill", ExitStatus::from_raw(15)),
            Err("'kill' terminated by signal 15".to_string())
        );
    }

    #[tokio::test]
    async fn shell_answers_with_stdout_and_runs_in_its_cwd() {
        let dir = std::env::temp_dir().canonicalize().expect("temp dir");
        let socket = PathBuf::from("/nonexistent.sock");
        assert_eq!(
            shell(
                "echo hi; echo two >&1; echo no >&2",
                dir.clone(),
                socket.clone()
            )
            .await,
            Ok("hi\ntwo\n".to_string())
        );
        assert_eq!(
            shell("printf abc", dir.clone(), socket.clone()).await,
            Ok("abc\n".to_string())
        );
        assert_eq!(
            shell("true", dir.clone(), socket.clone()).await,
            Ok(String::new())
        );
        let pwd = shell("pwd -P", dir.clone(), socket.clone()).await;
        assert_eq!(pwd, Ok(format!("{}\n", dir.display())));
        assert_eq!(
            shell("echo \"$GROFE\"", dir.clone(), socket).await,
            Ok("/nonexistent.sock\n".to_string())
        );
        assert_eq!(
            shell("exit 3", dir, PathBuf::from("/x")).await,
            Err("'exit 3' returned 3".to_string())
        );
    }
}
