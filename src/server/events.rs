//! subscribe, per-subscriber event queues, pane-exited and pane-output.
//!
//! Each control connection owns a bounded channel ([`QUEUE`] events) and
//! hands its sender to the actor with `subscribe`; the connection task
//! writes whatever arrives as JSON lines (spec §4.6). The actor never
//! blocks on a subscriber: [`Events::flush`] uses `try_send`, counts what
//! did not fit, and once the queue has room again sends one
//! `events-dropped` notice with that count before the next event.
//!
//! Commands record their events with [`Events::emit`]; the actor flushes
//! them after the command's reply is sent, so on one connection a reply is
//! written before the events it caused.

use tokio::sync::mpsc;

use crate::event::Event;
use crate::server::model::Removed;
use crate::server::pane;

/// Events one subscriber's queue holds before events are dropped.
// ponytail: 1024 events per subscriber; with `pane-output` (up to 16 KiB
// of output per event) that bounds a stuck subscriber at ~22 MiB.
pub const QUEUE: usize = 1024;

/// Every event name `subscribe` accepts literally.
pub const NAMES: &[&str] = &["pane-exited", "pane-output", "option-changed"];

/// `pane-output` is sent only to subscribers that name it: a bare
/// `subscribe` or a pattern does not match it (volume).
const PANE_OUTPUT: &str = "pane-output";

struct Subscriber {
    /// Event names or `*` patterns; empty means every event but
    /// `pane-output`.
    filters: Vec<String>,
    tx: mpsc::Sender<Event>,
    /// Events that did not fit since the last `events-dropped` notice.
    dropped: u64,
}

impl Subscriber {
    fn wants(&self, name: &str) -> bool {
        if name == PANE_OUTPUT {
            return self.filters.iter().any(|f| f == PANE_OUTPUT);
        }
        self.filters.is_empty() || self.filters.iter().any(|f| glob(f, name))
    }

    /// Queues `event` without waiting, the notice first if events were
    /// dropped. False once the connection is gone.
    fn offer(&mut self, event: &Event) -> bool {
        use mpsc::error::TrySendError;
        if self.dropped > 0 {
            let notice = Event::EventsDropped {
                count: self.dropped,
            };
            match self.tx.try_send(notice) {
                Ok(()) => self.dropped = 0,
                Err(TrySendError::Full(_)) => {
                    self.dropped += 1;
                    return true;
                }
                Err(TrySendError::Closed(_)) => return false,
            }
        }
        match self.tx.try_send(event.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.dropped += 1;
                true
            }
            Err(TrySendError::Closed(_)) => false,
        }
    }
}

/// `*` matches any run of characters; everything else literally.
fn glob(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = name.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

/// The subscriber registry and the events waiting to be flushed.
#[derive(Default)]
pub struct Events {
    subscribers: Vec<Subscriber>,
    pending: Vec<Event>,
}

impl Events {
    /// Sets the subscription of the connection that owns `tx`, replacing
    /// its earlier one. An empty `names` means every event but
    /// `pane-output`.
    pub fn subscribe(&mut self, names: Vec<String>, tx: mpsc::Sender<Event>) -> Result<(), String> {
        if let Some(bad) = names
            .iter()
            .find(|n| !n.contains('*') && !NAMES.contains(&n.as_str()))
        {
            return Err(format!("unknown event: {bad}"));
        }
        self.subscribers.retain(|s| !s.tx.is_closed());
        match self.subscribers.iter_mut().find(|s| s.tx.same_channel(&tx)) {
            Some(s) => s.filters = names,
            None => self.subscribers.push(Subscriber {
                filters: names,
                tx,
                dropped: 0,
            }),
        }
        self.update_tap();
        Ok(())
    }

    /// Records an event, sent on the next [`Events::flush`].
    pub fn emit(&mut self, event: Event) {
        if self.subscribers.iter().any(|s| s.wants(event.name())) {
            self.pending.push(event);
        }
    }

    /// Records `pane-exited` with a null status for panes a kill removed.
    pub fn panes_killed(&mut self, removed: Removed) {
        for (pane, uid) in removed {
            self.emit(Event::PaneExited {
                pane,
                uid,
                status: None,
            });
        }
    }

    /// Queues every pending event to the subscribers that want it, in
    /// order, and forgets subscribers whose connection is gone.
    pub fn flush(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        let before = self.subscribers.len();
        for event in &pending {
            let name = event.name();
            self.subscribers
                .retain_mut(|s| !s.wants(name) || s.offer(event));
        }
        self.subscribers.retain(|s| !s.tx.is_closed());
        if self.subscribers.len() != before {
            self.update_tap();
        }
    }

    /// Pane readers copy their output to the actor only while someone
    /// wants `pane-output`.
    fn update_tap(&self) {
        pane::set_output_tap(self.subscribers.iter().any(|s| s.wants(PANE_OUTPUT)));
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.subscribers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PaneId;

    fn changed(value: &str) -> Event {
        Event::OptionChanged {
            scope: "global".into(),
            name: "@x".into(),
            value: Some(value.into()),
        }
    }

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn globs_match_like_fnmatch_star() {
        assert!(glob("option-*", "option-changed"));
        assert!(glob("*", "pane-exited"));
        assert!(glob("pane-*ed", "pane-exited"));
        assert!(glob("*-exited", "pane-exited"));
        assert!(!glob("window-*", "pane-exited"));
        assert!(!glob("pane-", "pane-exited"));
        assert!(glob("pane-exited", "pane-exited"));
    }

    #[test]
    fn pane_output_only_when_named() {
        let (tx, _rx) = mpsc::channel(1);
        let bare = Subscriber {
            filters: vec![],
            tx: tx.clone(),
            dropped: 0,
        };
        let star = Subscriber {
            filters: names(&["*"]),
            tx: tx.clone(),
            dropped: 0,
        };
        let named = Subscriber {
            filters: names(&["pane-output"]),
            tx,
            dropped: 0,
        };
        assert!(bare.wants("pane-exited") && !bare.wants("pane-output"));
        assert!(star.wants("option-changed") && !star.wants("pane-output"));
        assert!(named.wants("pane-output") && !named.wants("pane-exited"));
    }

    #[tokio::test]
    async fn overflow_sends_one_notice_with_the_count() {
        let mut events = Events::default();
        let (tx, mut rx) = mpsc::channel(2);
        events.subscribe(vec![], tx).unwrap();
        for i in 0..5 {
            events.emit(changed(&i.to_string()));
        }
        events.flush();
        assert_eq!(rx.recv().await, Some(changed("0")));
        assert_eq!(rx.recv().await, Some(changed("1")));
        assert!(rx.try_recv().is_err());
        events.emit(changed("5"));
        events.flush();
        assert_eq!(rx.recv().await, Some(Event::EventsDropped { count: 3 }));
        assert_eq!(rx.recv().await, Some(changed("5")));
    }

    #[test]
    fn closed_subscribers_are_removed_on_the_next_flush() {
        let mut events = Events::default();
        let (live_tx, mut live_rx) = mpsc::channel(8);
        events.subscribe(vec![], live_tx).unwrap();
        for _ in 0..10 {
            let (tx, rx) = mpsc::channel(8);
            events.subscribe(vec![], tx).unwrap();
            drop(rx);
        }
        events.emit(changed("1"));
        events.flush();
        assert_eq!(events.len(), 1);
        assert_eq!(live_rx.try_recv().ok(), Some(changed("1")));
    }

    #[test]
    fn resubscribing_replaces_the_filters() {
        let mut events = Events::default();
        let (tx, mut rx) = mpsc::channel(8);
        events
            .subscribe(names(&["pane-exited"]), tx.clone())
            .unwrap();
        events.subscribe(names(&["option-changed"]), tx).unwrap();
        assert_eq!(events.len(), 1);
        events.emit(Event::PaneExited {
            pane: PaneId(1),
            uid: "u".into(),
            status: None,
        });
        events.emit(changed("1"));
        events.flush();
        assert_eq!(rx.try_recv().ok(), Some(changed("1")));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn unknown_names_are_refused() {
        let mut events = Events::default();
        let (tx, _rx) = mpsc::channel(1);
        let err = events.subscribe(names(&["pane-exitted"]), tx.clone());
        assert_eq!(err, Err("unknown event: pane-exitted".to_string()));
        assert!(events.subscribe(names(&["window-*"]), tx).is_ok());
    }
}
