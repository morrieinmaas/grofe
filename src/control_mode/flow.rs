//! A control client's flow control: which pane output is written, and how
//! it is framed (`%output`, `%extended-output`, `%pause`).

use std::collections::{BTreeMap, BTreeSet};

/// The most output held for one pane that is off.
const HELD_MAX: usize = 1 << 20;

/// `data` as `%output` writes it: bytes below 0x20 and `\` as `\ooo` octal,
/// everything else as it is.
pub fn escape_output(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for &b in data {
        if b < b' ' || b == b'\\' {
            out.extend_from_slice(format!("\\{b:03o}").as_bytes());
        } else {
            out.push(b);
        }
    }
    out
}

/// A pane's flow state for this client: control.c's two flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PaneFlags {
    /// `-A %p:off`: output held, and sent at `:on`.
    off: bool,
    /// `-A %p:pause` or `pause-after`: output dropped until `:continue`.
    paused: bool,
}

/// A client's flow control: which pane output is written, and how.
#[derive(Debug, Default)]
pub(super) struct Flow {
    panes: BTreeMap<String, PaneFlags>,
    held: BTreeMap<String, Vec<u8>>,
    no_output: bool,
    pause_after: Option<u64>,
    pub(super) wait_exit: bool,
}

/// `<sigil><n>` read as tmux's `sscanf("%%%d")` reads it: an optional
/// sign, then the leading digits (`%0x` is 0, `%00` is 0).
pub(super) fn scan_id(text: &str, sigil: char) -> Option<i64> {
    let rest = text.strip_prefix(sigil)?;
    let (sign, digits) = match rest.strip_prefix('-') {
        Some(d) => (-1, d),
        None => (1, rest.strip_prefix('+').unwrap_or(rest)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    digits[..end].parse::<i64>().ok().map(|n| sign * n)
}

impl Flow {
    /// `refresh-client -f` / `-F` / `attach -f`: comma-separated flags, `!`
    /// clears.
    pub(super) fn set_flags(&mut self, list: &str) {
        for flag in list.split(',').filter(|f| !f.is_empty()) {
            let (on, name) = match flag.strip_prefix('!') {
                Some(name) => (false, name),
                None => (true, flag),
            };
            match name.split_once('=').unwrap_or((name, "")) {
                ("no-output", _) => self.no_output = on,
                ("wait-exit", _) => self.wait_exit = on,
                ("pause-after", secs) => {
                    self.pause_after = on.then(|| secs.parse().unwrap_or(0));
                }
                _ => {}
            }
        }
    }

    /// `refresh-client -A %p:state` for a pane in `live`: the lines it
    /// prints. As tmux, a bad argument or an unknown pane changes nothing,
    /// and `%pause`/`%continue` are printed only when the state changes.
    pub(super) fn set_pane(&mut self, arg: &str, live: &BTreeSet<String>) -> Vec<String> {
        let Some((pane, state)) = arg.split_once(':') else {
            return Vec::new();
        };
        let Some(pane) = scan_id(pane, '%')
            .filter(|n| *n >= 0)
            .map(|n| format!("%{n}"))
            .filter(|p| live.contains(p))
        else {
            return Vec::new();
        };
        let flags = self.panes.entry(pane.clone()).or_default();
        match state {
            "on" => flags.off = false,
            "off" => {
                flags.off = true;
                self.held.remove(&pane);
            }
            "continue" if flags.paused => {
                flags.paused = false;
                return vec![format!("%continue {pane}")];
            }
            "pause" if !flags.paused => {
                flags.paused = true;
                self.held.remove(&pane);
                return vec![format!("%pause {pane}")];
            }
            _ => {}
        }
        Vec::new()
    }

    /// Forgets the state of panes that are gone, so it cannot outgrow the
    /// server's panes.
    pub(super) fn retain(&mut self, live: &BTreeSet<String>) {
        self.panes.retain(|p, _| live.contains(p));
        self.held.retain(|p, _| live.contains(p));
    }

    /// What to write for `data` printed by `pane`, `age_ms` after it came.
    /// With `pause-after`, output older than that pauses the pane instead,
    /// as tmux does.
    pub(super) fn output(&mut self, pane: &str, data: &[u8], age_ms: u64) -> Option<Vec<u8>> {
        if self.no_output {
            return None;
        }
        let flags = self.panes.get(pane).copied().unwrap_or_default();
        if flags.paused {
            None
        } else if flags.off {
            let held = self.held.entry(pane.to_string()).or_default();
            // ponytail: a pane held off keeps its first HELD_MAX bytes and
            // drops the rest.
            let room = HELD_MAX.saturating_sub(held.len());
            held.extend_from_slice(&data[..data.len().min(room)]);
            None
        } else if self.pause_after.is_some_and(|s| age_ms / 1000 >= s.max(1)) {
            self.panes.entry(pane.to_string()).or_default().paused = true;
            Some(format!("%pause {pane}\n").into_bytes())
        } else {
            Some(self.frame(pane, data, age_ms))
        }
    }

    /// The data held for panes that are no longer off, framed.
    pub(super) fn release(&mut self) -> Vec<Vec<u8>> {
        let ready: Vec<String> = self
            .held
            .keys()
            .filter(|p| self.panes.get(*p).is_none_or(|f| !f.off && !f.paused))
            .cloned()
            .collect();
        ready
            .into_iter()
            .filter_map(|p| self.held.remove(&p).map(|d| self.frame(&p, &d, 0)))
            .collect()
    }

    fn frame(&self, pane: &str, data: &[u8], age_ms: u64) -> Vec<u8> {
        let mut line = if self.pause_after.is_some() {
            format!("%extended-output {pane} {age_ms} : ").into_bytes()
        } else {
            format!("%output {pane} ").into_bytes()
        };
        line.extend(escape_output(data));
        line.push(b'\n');
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live() -> BTreeSet<String> {
        BTreeSet::from(["%0".to_string(), "%1".to_string()])
    }

    #[test]
    fn flow_state_follows_live_panes_and_held_output_is_capped() {
        let mut flow = Flow::default();
        flow.set_pane("%0:off", &live());
        flow.set_pane("%1:pause", &live());
        assert!(flow.output("%0", &vec![b'x'; HELD_MAX + 10], 0).is_none());
        assert_eq!(flow.held["%0"].len(), HELD_MAX);
        flow.retain(&BTreeSet::from(["%1".to_string()]));
        assert!(flow.held.is_empty() && flow.panes.len() == 1);
    }

    #[test]
    fn a_pane_older_than_pause_after_is_paused() {
        let mut flow = Flow::default();
        flow.set_flags("pause-after=2");
        assert!(
            flow.output("%0", b"a", 1999)
                .unwrap()
                .starts_with(b"%extended")
        );
        assert_eq!(flow.output("%0", b"a", 2000).unwrap(), b"%pause %0\n");
        assert!(flow.output("%0", b"a", 0).is_none());
    }

    #[test]
    fn output_escapes_control_bytes_and_backslash_only() {
        let got = escape_output(b"a\r\n\\\x1b[1m\xc3\xa9~");
        assert_eq!(got, b"a\\015\\012\\134\\033[1m\xc3\xa9~");
    }

    #[test]
    fn ids_scan_as_sscanf_does() {
        assert_eq!(scan_id("%0x", '%'), Some(0));
        assert_eq!(scan_id("%00", '%'), Some(0));
        assert_eq!(scan_id("%-1", '%'), Some(-1));
        assert_eq!(scan_id("%+7", '%'), Some(7));
        assert_eq!(scan_id("%x", '%'), None);
        assert_eq!(scan_id("@3", '%'), None);
    }

    /// Recorded on tmux 3.7b: off and paused are two flags; `:on` clears
    /// only off, `:continue` only paused, and a notice only on a change.
    #[test]
    fn off_and_paused_are_separate_flags() {
        let mut flow = Flow::default();
        let none: Vec<String> = Vec::new();
        assert_eq!(flow.set_pane("%0:bogus", &live()), none);
        assert_eq!(flow.set_pane("%9:pause", &live()), none);
        assert_eq!(flow.set_pane("x:pause", &live()), none);
        assert_eq!(flow.set_pane("%0:continue", &live()), none);
        assert_eq!(flow.set_pane("%0:pause", &live()), ["%pause %0"]);
        assert_eq!(flow.set_pane("%00:pause", &live()), none);
        assert_eq!(flow.set_pane("%0:on", &live()), none);
        assert!(flow.output("%0", b"lost", 0).is_none());
        flow.set_pane("%0:off", &live());
        assert_eq!(flow.set_pane("%0:continue", &live()), ["%continue %0"]);
        assert!(flow.output("%0", b"held", 0).is_none());
        assert!(flow.release().is_empty());
        flow.set_pane("%0:on", &live());
        assert_eq!(flow.release(), [b"%output %0 held\n".to_vec()]);
        assert_eq!(flow.output("%0", b"x", 0).unwrap(), b"%output %0 x\n");
    }

    #[test]
    fn no_output_drops_and_pause_after_frames_extended() {
        let mut flow = Flow::default();
        flow.set_flags("pause-after=1");
        assert_eq!(
            flow.output("%0", b"a", 7).unwrap(),
            b"%extended-output %0 7 : a\n"
        );
        flow.set_flags("no-output");
        assert!(flow.output("%0", b"x", 0).is_none());
        flow.set_flags("!no-output");
        assert!(flow.output("%0", b"x", 0).is_some());
    }
}
