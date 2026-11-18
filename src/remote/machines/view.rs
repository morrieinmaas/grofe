//! The pure parts of the multi-machine client (spec ssh-remote §5): the
//! `M-m` picker's drawing and keys, the `#{client_machines}` text and the
//! `agent list --all-machines` table, with the last-known cache behind its
//! offline rows.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::server::input::{Decoder, Input as Decoded, KeyCode, Mods};

#[cfg(doc)]
use super::LOCAL;

// --- views of a connected machine ------------------------------------------

/// A machine's link, as the multi-machine client sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Connected,
    Reconnecting,
    Offline,
}

impl Link {
    pub fn as_str(self) -> &'static str {
        match self {
            Link::Connected => "connected",
            Link::Reconnecting => "reconnecting",
            Link::Offline => "offline",
        }
    }
}

/// One agent on a machine, from its `agent list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentView {
    /// The agent's kind (`claude`), or its pane id when it reported none.
    pub name: String,
    /// The server's word for it: working, blocked, done, idle or error.
    pub state: String,
}

/// What the picker and `#{client_machines}` know about one machine. For an
/// offline machine the sessions and agents are the last known ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineView {
    pub name: String,
    pub link: Link,
    pub sessions: Vec<String>,
    pub agents: Vec<AgentView>,
    pub unread: bool,
}

impl MachineView {
    /// A blocked agent or an unread notification: the machine wants you.
    pub fn wants_attention(&self) -> bool {
        self.unread || self.agents.iter().any(|a| a.state == "blocked")
    }
}

/// `#{client_machines}`: the machines' names, space separated, each
/// followed by `!` when it [wants attention](MachineView::wants_attention).
pub fn client_machines(machines: &[MachineView]) -> String {
    machines
        .iter()
        .map(|m| {
            let mark = if m.wants_attention() { "!" } else { "" };
            format!("{}{mark}", m.name)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// --- the picker --------------------------------------------------------------

/// What a key in the picker asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerOutcome {
    /// Attach to the machine at this index.
    Choose(usize),
    /// Close the picker without switching.
    Close,
}

/// The `M-m` picker's state: which row is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Picker {
    pub selected: usize,
}

impl Picker {
    /// Feeds the keys in `input` with `count` machines listed. Up and down
    /// move the selection (stopping at the ends), Enter chooses it, Escape
    /// closes, and `M-1`..`M-9` choose that row at once. Keys are decoded
    /// as the server decodes them, so legacy bytes and the kitty keyboard
    /// protocol's both work. The first key that chooses or closes ends the
    /// input; other keys are ignored.
    pub fn handle(&mut self, input: &[u8], count: usize) -> Option<PickerOutcome> {
        let keys = Decoder::new().feed(input, Instant::now(), Duration::ZERO);
        for key in keys {
            let Decoded::Key(key) = key else {
                continue;
            };
            let plain = key.mods == Mods::default();
            match key.code {
                KeyCode::Up if plain => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down if plain && self.selected + 1 < count => self.selected += 1,
                KeyCode::Enter if plain && count > 0 => {
                    return Some(PickerOutcome::Choose(self.selected));
                }
                KeyCode::Escape if plain => return Some(PickerOutcome::Close),
                KeyCode::Char(d @ '1'..='9') if key.mods == alt() => {
                    let n = d as usize - '1' as usize;
                    if n < count {
                        self.selected = n;
                        return Some(PickerOutcome::Choose(n));
                    }
                }
                _ => {}
            }
        }
        None
    }
}

/// Alt and nothing else.
fn alt() -> Mods {
    Mods {
        alt: true,
        ..Mods::default()
    }
}

/// One picker row's text, before it is fitted to the box: the `M-` digit,
/// a `!` when the machine wants attention, its name, link, sessions and
/// agents.
fn row_text(index: usize, m: &MachineView, name_width: usize) -> String {
    let digit = if index < 9 {
        char::from(b'1' + index as u8)
    } else {
        ' '
    };
    let mark = if m.wants_attention() { '!' } else { ' ' };
    let pad = name_width - m.name.width();
    let sessions = if m.sessions.is_empty() {
        "-".to_string()
    } else {
        m.sessions.join(",")
    };
    let agents: Vec<String> = m
        .agents
        .iter()
        .map(|a| format!("{} {}", a.name, a.state))
        .collect();
    let mut text = format!(
        "{digit}{mark}{}{}  {:<12}  {sessions}",
        m.name,
        " ".repeat(pad),
        m.link.as_str()
    );
    if !agents.is_empty() {
        text.push_str("  ");
        text.push_str(&agents.join(", "));
    }
    text
}

/// `s` cut or padded with spaces to exactly `width` columns.
fn fit(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push_str(&" ".repeat(width - used));
    out
}

/// The picker's one row when no machine is saved.
const EMPTY: &str = "no machines saved";

/// The bytes that draw the picker over a `cols` x `rows` terminal: a
/// rounded box centred on the screen, one row per machine, the selected row
/// in reverse video, offline rows dim. With more machines than fit, the
/// rows scroll to keep the selection in view. The cursor is saved first and
/// restored last; closing the picker needs a redraw of what it covered.
pub fn render_picker(machines: &[MachineView], selected: usize, cols: u16, rows: u16) -> Vec<u8> {
    let (cols, rows) = (usize::from(cols), usize::from(rows));
    if cols < 4 || rows < 3 {
        return Vec::new();
    }
    let name_width = machines.iter().map(|m| m.name.width()).max().unwrap_or(0);
    let texts: Vec<String> = machines
        .iter()
        .enumerate()
        .map(|(i, m)| row_text(i, m, name_width))
        .collect();
    let title = " machines ";
    let content = texts
        .iter()
        .map(|t| t.width())
        .max()
        .unwrap_or(0)
        .max(title.width() + 2)
        .max(EMPTY.width());
    let inner = content.min(cols - 4);
    let width = inner + 4;
    let visible = machines.len().max(1).min(rows - 2);
    let height = visible + 2;
    let top = (rows - height) / 2 + 1;
    let left = (cols - width) / 2 + 1;
    let first = selected
        .saturating_sub(visible - 1)
        .min(machines.len().saturating_sub(visible));

    let mut out = String::from("\x1b7");
    let border = |out: &mut String, row: usize, l: char, fill: &str, r: char| {
        out.push_str(&format!("\x1b[{row};{left}H\x1b[0m{l}{fill}{r}"));
    };
    let mut top_fill = format!("─{title}");
    top_fill.push_str(&"─".repeat(width - 2 - title.width() - 1));
    border(&mut out, top, '╭', &top_fill, '╮');
    if machines.is_empty() {
        let text = fit(EMPTY, inner);
        border(&mut out, top + 1, '│', &format!(" {text} "), '│');
    }
    for (line, i) in (first..machines.len()).take(visible).enumerate() {
        let mut style = String::new();
        if machines[i].link == Link::Offline {
            style.push_str("\x1b[2m");
        }
        if i == selected {
            style.push_str("\x1b[7m");
        }
        let text = fit(&texts[i], inner);
        out.push_str(&format!(
            "\x1b[{};{left}H\x1b[0m│{style} {text} \x1b[0m│",
            top + 1 + line
        ));
    }
    border(&mut out, top + height - 1, '╰', &"─".repeat(width - 2), '╯');
    out.push_str("\x1b[0m\x1b8");
    out.into_bytes()
}

// --- the combined agent list -----------------------------------------------

/// One machine's part of `agent list --all-machines`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentListing {
    /// The `agent list` output the machine just gave.
    Online(String),
    /// Unreachable: the last output seen from it, if any.
    Offline(Option<String>),
}

/// `agent list --all-machines`: each machine's `agent list` lines behind a
/// leading `machine` column padded to the longest name, in the order given
/// (the caller puts [`LOCAL`] first). An offline machine's last known lines
/// end in `offline`; with none known it gets one `<name> offline` line.
pub fn combined_agent_list(listings: &[(String, AgentListing)]) -> String {
    let width = listings.iter().map(|(n, _)| n.width()).max().unwrap_or(0);
    let mut out = String::new();
    for (name, listing) in listings {
        let pad = " ".repeat(width - name.width());
        let (text, offline) = match listing {
            AgentListing::Online(t) => (t.as_str(), false),
            AgentListing::Offline(Some(t)) => (t.as_str(), true),
            AgentListing::Offline(None) => {
                out.push_str(&format!("{name}{pad} offline\n"));
                continue;
            }
        };
        for line in text.lines().filter(|l| !l.is_empty()) {
            out.push_str(&format!("{name}{pad} {line}"));
            if offline {
                out.push_str(" offline");
            }
            out.push('\n');
        }
    }
    out
}

/// Where the last `agent list` output of machine `name` is kept, for its
/// offline rows: `<state-dir>/machines/<name>.agents`.
pub fn last_known_path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join("machines").join(format!("{name}.agents"))
}

/// Keeps `text` as machine `name`'s last known agents.
// ponytail: one cache per machine, whatever `-F` the listing used; an
// offline row after a custom format shows the last text as it was.
pub fn save_last_known(state_dir: &Path, name: &str, text: &str) -> Result<(), String> {
    let path = last_known_path(state_dir, name);
    let dir = path.parent().unwrap_or(state_dir);
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// Machine `name`'s last known agents, if any were kept.
pub fn last_known(state_dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(last_known_path(state_dir, name)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::machines::LOCAL;
    use alacritty_terminal::Term;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::vte::ansi::Processor;

    fn view(name: &str, link: Link, agents: &[(&str, &str)]) -> MachineView {
        MachineView {
            name: name.into(),
            link,
            sessions: vec!["main".into()],
            agents: agents
                .iter()
                .map(|(n, s)| AgentView {
                    name: n.to_string(),
                    state: s.to_string(),
                })
                .collect(),
            unread: false,
        }
    }

    #[test]
    fn client_machines_marks_blocked_and_unread() {
        let mut unread = view("b", Link::Connected, &[("claude", "done")]);
        unread.unread = true;
        let list = [
            view("local", Link::Connected, &[("claude", "working")]),
            view("a", Link::Offline, &[("codex", "blocked")]),
            unread,
        ];
        assert_eq!(client_machines(&list), "local a! b!");
        assert_eq!(client_machines(&[]), "");
    }

    #[test]
    fn picker_keys() {
        let mut p = Picker::default();
        assert_eq!(p.handle(b"\x1b[A", 3), None);
        assert_eq!(p.selected, 0, "stops at the top");
        assert_eq!(p.handle(b"\x1b[B\x1bOB\x1b[B", 3), None);
        assert_eq!(p.selected, 2, "stops at the bottom");
        assert_eq!(p.handle(b"\x1b[A\r", 3), Some(PickerOutcome::Choose(1)));
        assert_eq!(p.handle(b"\x1b", 3), Some(PickerOutcome::Close));
        assert_eq!(p.handle(b"x\x1b3", 3), Some(PickerOutcome::Choose(2)));
        assert_eq!(p.selected, 2);
        // A digit past the list does nothing; neither does Enter with none.
        assert_eq!(p.handle(b"\x1b9", 3), None);
        assert_eq!(Picker::default().handle(b"\r", 0), None);
        // An unknown escape sequence is not an Escape.
        assert_eq!(p.handle(b"\x1b[5~", 3), None);
    }

    /// The remote client turns on the kitty keyboard protocol where the
    /// terminal has it: the same keys arrive as `CSI ... u`.
    #[test]
    fn picker_keys_in_the_kitty_protocol() {
        let mut p = Picker::default();
        assert_eq!(p.handle(b"\x1b[27u", 3), Some(PickerOutcome::Close));
        assert_eq!(p.handle(b"\x1b[50;3u", 3), Some(PickerOutcome::Choose(1)));
        assert_eq!(p.selected, 1);
        assert_eq!(p.handle(b"\x1b[13u", 3), Some(PickerOutcome::Choose(1)));
        // Shifted digits are not a jump.
        assert_eq!(p.handle(b"\x1b[50;4u", 3), None);
    }

    struct Size(usize, usize);
    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            self.1
        }
        fn screen_lines(&self) -> usize {
            self.1
        }
        fn columns(&self) -> usize {
            self.0
        }
    }

    /// The screen after `bytes`: each row's text, and per row whether any
    /// cell is reverse and whether any is dim.
    fn draw(bytes: &[u8], cols: usize, rows: usize) -> Vec<(String, bool, bool)> {
        let mut term = Term::new(Config::default(), &Size(cols, rows), VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        (0..rows)
            .map(|r| {
                let cells: Vec<_> = (0..cols)
                    .map(|c| term.grid()[Line(r as i32)][Column(c)].clone())
                    .collect();
                let text: String = cells.iter().map(|c| c.c).collect();
                let inverse = cells.iter().any(|c| c.flags.contains(Flags::INVERSE));
                let dim = cells.iter().any(|c| c.flags.contains(Flags::DIM));
                (text.trim_end().to_string(), inverse, dim)
            })
            .collect()
    }

    #[test]
    fn picker_draws_a_box_with_selection_and_dim_offline() {
        let mut box1 = view("box", Link::Offline, &[("codex", "blocked")]);
        box1.sessions = vec!["main".into(), "dev".into()];
        let list = [
            view("local", Link::Connected, &[("claude", "working")]),
            box1,
        ];
        let screen = draw(&render_picker(&list, 0, 60, 10), 60, 10);
        let lines: Vec<&str> = screen.iter().map(|(t, _, _)| t.as_str()).collect();
        assert_eq!(
            &lines[3..7],
            &[
                "     ╭─ machines ─────────────────────────────────────╮",
                "     │ 1 local  connected     main  claude working    │",
                "     │ 2!box    offline       main,dev  codex blocked │",
                "     ╰────────────────────────────────────────────────╯",
            ],
            "{lines:#?}"
        );
        assert_eq!((screen[4].1, screen[4].2), (true, false), "selected");
        assert_eq!((screen[5].1, screen[5].2), (false, true), "offline dim");
    }

    #[test]
    fn picker_fits_small_screens_and_scrolls() {
        let list: Vec<MachineView> = (0..12)
            .map(|i| view(&format!("m{i}"), Link::Connected, &[]))
            .collect();
        let screen = draw(&render_picker(&list, 11, 20, 6), 20, 6);
        let text: Vec<&str> = screen.iter().map(|(t, _, _)| t.as_str()).collect();
        assert!(text.iter().all(|l| l.chars().count() <= 20), "{text:#?}");
        // Four rows fit; the selected last one is in view and reversed.
        let selected: Vec<_> = screen.iter().filter(|r| r.1).collect();
        assert_eq!(selected.len(), 1);
        assert!(selected[0].0.contains("m11"), "{text:#?}");
        assert!(text.iter().any(|l| l.contains("m8")) && !text.iter().any(|l| l.contains("m7 ")));
        assert!(render_picker(&list, 0, 3, 2).is_empty());
    }

    #[test]
    fn picker_with_no_machines() {
        let screen = draw(&render_picker(&[], 0, 40, 5), 40, 5);
        assert!(screen.iter().any(|r| r.0.contains(EMPTY)));
    }

    #[test]
    fn combined_list_adds_machine_column_and_offline_rows() {
        let listings = [
            (
                LOCAL.to_string(),
                AgentListing::Online("%1 claude working\n%3 codex done\n".into()),
            ),
            (
                "box".to_string(),
                AgentListing::Offline(Some("%2 claude blocked\n".into())),
            ),
            ("far-away".to_string(), AgentListing::Offline(None)),
            ("idle".to_string(), AgentListing::Online(String::new())),
        ];
        assert_eq!(
            combined_agent_list(&listings),
            "local    %1 claude working\n\
             local    %3 codex done\n\
             box      %2 claude blocked offline\n\
             far-away offline\n"
        );
    }

    #[test]
    fn last_known_round_trip() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(last_known(d.path(), "box"), None);
        save_last_known(d.path(), "box", "%1 claude done\n").unwrap();
        assert_eq!(
            last_known(d.path(), "box").as_deref(),
            Some("%1 claude done\n")
        );
        assert_eq!(
            last_known_path(d.path(), "box"),
            d.path().join("machines/box.agents")
        );
    }
}
