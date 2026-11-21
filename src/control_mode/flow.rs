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

/// What a pane's output does for this client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaneState {
    On,
    /// `-A %p:off`: held, and sent at `:on`.
    Off,
    /// `-A %p:pause`: dropped until `:continue`.
    Paused,
}

/// A client's flow control: which pane output is written, and how.
#[derive(Debug, Default)]
pub(super) struct Flow {
    pub(super) panes: BTreeMap<String, PaneState>,
    held: BTreeMap<String, Vec<u8>>,
    no_output: bool,
    pause_after: Option<u64>,
    pub(super) wait_exit: bool,
}

impl Flow {
    /// `refresh-client -f` / `attach -f`: comma-separated flags, `!` clears.
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

    /// `refresh-client -A %p:state`: the lines the command prints, or its
    /// error.
    pub(super) fn set_pane(&mut self, arg: &str) -> Result<Vec<String>, String> {
        let (pane, state) = arg
            .split_once(':')
            .filter(|(p, _)| p.starts_with('%'))
            .ok_or_else(|| format!("bad pane argument: {arg}"))?;
        let mut printed = Vec::new();
        match state {
            "on" | "continue" => {
                self.panes.insert(pane.to_string(), PaneState::On);
                if state == "continue" {
                    printed.push(format!("%continue {pane}"));
                }
            }
            "off" => {
                self.panes.insert(pane.to_string(), PaneState::Off);
            }
            "pause" => {
                self.panes.insert(pane.to_string(), PaneState::Paused);
                self.held.remove(pane);
                printed.push(format!("%pause {pane}"));
            }
            _ => return Err(format!("bad pane argument: {arg}")),
        }
        Ok(printed)
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
        match self.panes.get(pane).copied().unwrap_or(PaneState::On) {
            PaneState::Paused => None,
            PaneState::Off => {
                let held = self.held.entry(pane.to_string()).or_default();
                // ponytail: a pane held off keeps its first HELD_MAX bytes
                // and drops the rest.
                let room = HELD_MAX.saturating_sub(held.len());
                held.extend_from_slice(&data[..data.len().min(room)]);
                None
            }
            PaneState::On if self.pause_after.is_some_and(|s| age_ms / 1000 >= s.max(1)) => {
                self.panes.insert(pane.to_string(), PaneState::Paused);
                Some(format!("%pause {pane}\n").into_bytes())
            }
            PaneState::On => Some(self.frame(pane, data, age_ms)),
        }
    }

    /// The data held for `pane` while it was off, framed.
    pub(super) fn release(&mut self, pane: &str) -> Option<Vec<u8>> {
        let data = self.held.remove(pane)?;
        Some(self.frame(pane, &data, 0))
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

    #[test]
    fn flow_state_follows_live_panes_and_held_output_is_capped() {
        let mut flow = Flow::default();
        flow.set_pane("%0:off").unwrap();
        flow.set_pane("%1:pause").unwrap();
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
    fn pause_drops_off_holds_and_no_output_drops() {
        let mut flow = Flow::default();
        assert_eq!(flow.output("%0", b"a", 0).unwrap(), b"%output %0 a\n");
        flow.set_flags("pause-after=1");
        assert_eq!(
            flow.output("%0", b"a", 7).unwrap(),
            b"%extended-output %0 7 : a\n"
        );
        assert_eq!(flow.set_pane("%0:pause").unwrap(), ["%pause %0"]);
        assert!(flow.output("%0", b"lost", 0).is_none());
        assert_eq!(flow.set_pane("%0:continue").unwrap(), ["%continue %0"]);
        assert!(flow.output("%0", b"b", 0).is_some());
        flow.set_pane("%0:off").unwrap();
        assert!(flow.output("%0", b"held", 0).is_none());
        flow.set_pane("%0:on").unwrap();
        assert_eq!(
            flow.release("%0").unwrap(),
            b"%extended-output %0 0 : held\n"
        );
        flow.set_flags("no-output");
        assert!(flow.output("%0", b"x", 0).is_none());
        flow.set_flags("!no-output");
        assert!(flow.output("%0", b"x", 0).is_some());
    }
}
