//! Agent notifications: records, tray, raise rules (agents spec §6).
pub mod desktop;

use std::time::{Duration, Instant};

use crate::types::PaneId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyConfig {
    pub enabled: bool,
    pub on: Vec<String>,
    pub toast: Vec<String>,
    pub desktop: Vec<String>,
    pub history: usize,
    pub skip_when_seen: bool,
    pub done_min_work: Duration,
    pub min_interval: Duration,
    pub toast_time: Duration,
    pub delivery: String,
    pub sound: bool,
    pub command: Option<Vec<String>>,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
        NotifyConfig {
            enabled: true,
            on: s(&["blocked", "error", "done"]),
            toast: s(&["blocked"]),
            desktop: s(&["blocked", "error", "done"]),
            history: 10,
            skip_when_seen: true,
            done_min_work: Duration::from_secs(30),
            min_interval: Duration::from_secs(10),
            toast_time: Duration::from_secs(5),
            delivery: "auto".into(),
            sound: false,
            command: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub id: u64,
    pub pane: PaneId,
    pub uid: String,
    pub kind: Option<String>,
    pub state: String,
    pub msg: Option<String>,
    pub at: Instant,
    pub read: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Raised {
    pub id: u64,
    pub toast: bool,
    pub desktop: bool,
}

#[derive(Debug, Default)]
pub struct Notifier {
    pub cfg: NotifyConfig,
    items: Vec<Notification>,
    next_id: u64,
    /// `Some(None)` is muted until unmuted.
    muted: Option<Option<Instant>>,
}

impl Notifier {
    #[allow(clippy::too_many_arguments)]
    pub fn on_change(
        &mut self,
        pane: PaneId,
        uid: &str,
        kind: Option<&str>,
        _old: Option<&str>,
        new: Option<&str>,
        msg: Option<&str>,
        worked: Duration,
        seen: bool,
        configured_kind: bool,
        now: Instant,
    ) -> Option<Raised> {
        // Leaving the state an item was raised for reads it.
        for it in &mut self.items {
            if it.pane == pane && Some(it.state.as_str()) != new {
                it.read = true;
            }
        }
        let new = new?;
        let cfg = &self.cfg;
        if !cfg.enabled || !cfg.on.iter().any(|s| s == new) {
            return None;
        }
        if seen && cfg.skip_when_seen {
            return None;
        }
        if new == "done" && worked < cfg.done_min_work {
            return None;
        }
        let muted = self.muted(now);
        let toast = cfg.toast.iter().any(|s| s == new) && !muted;
        let desktop = cfg.desktop.iter().any(|s| s == new) && !muted && configured_kind;
        let min_interval = cfg.min_interval;
        if let Some(it) = self
            .items
            .iter_mut()
            .find(|it| it.pane == pane && now.saturating_duration_since(it.at) < min_interval)
        {
            it.state = new.to_string();
            it.msg = msg.map(str::to_string);
            it.read = false;
            return None;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.items.insert(
            0,
            Notification {
                id,
                pane,
                uid: uid.to_string(),
                kind: kind.map(str::to_string),
                state: new.to_string(),
                msg: msg.map(str::to_string),
                at: now,
                read: false,
            },
        );
        self.items.truncate(self.cfg.history);
        Some(Raised { id, toast, desktop })
    }

    /// Marks the pane's notifications read; true if any changed.
    pub fn seen(&mut self, pane: PaneId) -> bool {
        let mut changed = false;
        for it in self.items.iter_mut().filter(|i| i.pane == pane && !i.read) {
            it.read = true;
            changed = true;
        }
        changed
    }

    pub fn dismiss(&mut self, id: u64) -> bool {
        let before = self.items.len();
        self.items.retain(|i| i.id != id);
        self.items.len() != before
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn mute(&mut self, until: Option<Instant>) {
        self.muted = Some(until);
    }

    pub fn unmute(&mut self) {
        self.muted = None;
    }

    pub fn muted(&self, now: Instant) -> bool {
        match self.muted {
            None => false,
            Some(None) => true,
            Some(Some(t)) => now < t,
        }
    }

    pub fn unread(&self) -> usize {
        self.items.iter().filter(|i| !i.read).count()
    }

    /// Newest first.
    pub fn items(&self) -> &[Notification] {
        &self.items
    }

    pub fn newest_unread(&self) -> Option<&Notification> {
        self.items.iter().find(|i| !i.read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn raise(
        n: &mut Notifier,
        pane: u32,
        new: &str,
        worked: u64,
        seen: bool,
        t: Instant,
    ) -> Option<Raised> {
        n.on_change(
            PaneId(pane),
            "u1",
            Some("claude"),
            Some("working"),
            Some(new),
            Some("perm"),
            s(worked),
            seen,
            true,
            t,
        )
    }

    #[test]
    fn blocked_raises_toast_and_desktop() {
        let mut n = Notifier::default();
        let r = raise(&mut n, 1, "blocked", 5, false, Instant::now());
        assert_eq!(
            r,
            Some(Raised {
                id: 1,
                toast: true,
                desktop: true
            })
        );
    }

    #[test]
    fn seen_and_short_done_raise_nothing() {
        let mut n = Notifier::default();
        let t = Instant::now();
        assert_eq!(raise(&mut n, 1, "blocked", 5, true, t), None);
        assert_eq!(raise(&mut n, 1, "done", 3, false, t), None);
        assert_eq!(n.items().len(), 0);
    }

    #[test]
    fn second_within_interval_replaces_text() {
        let mut n = Notifier::default();
        let t = Instant::now();
        raise(&mut n, 1, "blocked", 5, false, t);
        let r = n.on_change(
            PaneId(1),
            "u1",
            None,
            None,
            Some("blocked"),
            Some("again"),
            s(5),
            false,
            true,
            t + s(2),
        );
        assert_eq!(r, None);
        assert_eq!(n.items().len(), 1);
        assert_eq!(n.items()[0].msg.as_deref(), Some("again"));
    }

    #[test]
    fn history_capped() {
        let mut n = Notifier::default();
        let t = Instant::now();
        for i in 1..=11 {
            raise(&mut n, i, "blocked", 5, false, t);
        }
        assert_eq!(n.items().len(), 10);
        assert_eq!(n.items()[0].id, 11);
        assert!(n.items().iter().all(|i| i.id != 1));
    }

    #[test]
    fn read_on_seen() {
        let mut n = Notifier::default();
        raise(&mut n, 1, "blocked", 5, false, Instant::now());
        assert_eq!(n.unread(), 1);
        assert!(n.seen(PaneId(1)));
        assert!(!n.seen(PaneId(1)));
        assert_eq!(n.unread(), 0);
    }

    #[test]
    fn read_when_state_left() {
        let mut n = Notifier::default();
        let t = Instant::now();
        raise(&mut n, 1, "blocked", 5, false, t);
        raise(&mut n, 1, "working", 0, false, t + s(1));
        assert_eq!(n.unread(), 0);
        assert!(n.newest_unread().is_none());
    }

    #[test]
    fn muted_raises_tray_only() {
        let mut n = Notifier::default();
        let t = Instant::now();
        n.mute(Some(t + s(60)));
        let r = raise(&mut n, 1, "blocked", 5, false, t).unwrap();
        assert!(!r.toast && !r.desktop);
        assert_eq!(n.unread(), 1);
        assert!(!n.muted(t + s(61)));
        n.mute(None);
        assert!(n.muted(t + s(9999)));
        n.unmute();
        assert!(!n.muted(t));
    }

    #[test]
    fn disabled_raises_nothing() {
        let mut n = Notifier::default();
        n.cfg.enabled = false;
        assert_eq!(raise(&mut n, 1, "blocked", 5, false, Instant::now()), None);
        assert!(n.items().is_empty());
    }

    #[test]
    fn unconfigured_kind_no_desktop() {
        let mut n = Notifier::default();
        let r = n
            .on_change(
                PaneId(1),
                "u",
                None,
                None,
                Some("blocked"),
                None,
                s(5),
                false,
                false,
                Instant::now(),
            )
            .unwrap();
        assert!(r.toast && !r.desktop);
    }

    #[test]
    fn dismiss_unknown_false() {
        let mut n = Notifier::default();
        raise(&mut n, 1, "blocked", 5, false, Instant::now());
        assert!(!n.dismiss(99));
        assert!(n.dismiss(1));
        n.clear();
        assert!(n.items().is_empty());
    }
}
