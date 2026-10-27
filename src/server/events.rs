//! subscribe, per-subscriber event queues, and the v1 catalog's emission
//! (spec §7.2).
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
//!
//! **Where each event is emitted.** One site per event: `pane-exited` in
//! `handle_pane_event` (a process exit) and [`Events::panes_killed`] (a
//! kill), `pane-output` in `handle_pane_event`, `option-changed` in the
//! option and status-source code, `pane-died`, `session-created`,
//! `session-closed`, `client-session-changed`, `client-resized` and the
//! alerts in `server::hooks` beside the hook each fires, `pane-resized`,
//! `window-focused`, `window-created` and `window-resized` in
//! `hooks::run_command`, and `pane-focus` in [`track_focus`].
//!
//! **`subscribe -t`.** A subscription may be limited to a [`Scope`], a pane,
//! window or session resolved when it was made. An event passes when the
//! pane, window or session it names (its own fields, filled in from the
//! model, or from its [`Place`] for a pane that has gone) lies inside the
//! scope. A scope whose target has gone passes nothing more once the batch
//! that removed it is delivered.

use std::collections::BTreeMap;

use tokio::sync::mpsc;

use crate::cmd::{Resolve, Target};
use crate::event::{Event, Place};
use crate::protocol::ClientCtx;
use crate::server::State;
use crate::server::model::{Model, Removed};
use crate::server::pane;
use crate::types::{ClientId, PaneId, SessionId, WindowId};

/// Events one subscriber's queue holds before events are dropped.
// ponytail: 1024 events per subscriber; with `pane-output` (up to 16 KiB
// of output per event) that bounds a stuck subscriber at ~22 MiB.
pub const QUEUE: usize = 1024;

/// Every event name `subscribe` accepts literally. `events-dropped` is a
/// notice every subscriber can receive, not a subscription.
pub const NAMES: &[&str] = &[
    "pane-exited",
    "pane-died",
    "pane-output",
    "pane-focus",
    "pane-resized",
    "window-focused",
    "window-created",
    "window-resized",
    "session-created",
    "session-closed",
    "client-session-changed",
    "client-resized",
    "alert-bell",
    "alert-activity",
    "alert-silence",
    "option-changed",
];

/// `pane-output` is sent only to subscribers that name it: a bare
/// `subscribe` or a pattern does not match it (volume).
const PANE_OUTPUT: &str = "pane-output";

/// What a `subscribe -t` is limited to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Pane(PaneId),
    Window(WindowId),
    Session(SessionId),
}

impl Scope {
    /// Resolves `subscribe -t`: a pane target is a pane, a window target a
    /// window, anything else a session (a bare word names a session).
    pub fn resolve(model: &Model, ctx: &ClientCtx, target: &Target) -> Result<Scope, String> {
        match target {
            Target::Pane(_) | Target::Current => model.resolve_pane(target, ctx).map(Scope::Pane),
            Target::Window(..) => model.resolve_window(target, ctx).map(Scope::Window),
            Target::Session(_) => model.resolve_session(target, ctx).map(Scope::Session),
        }
    }

    fn alive(self, model: &Model) -> bool {
        match self {
            Scope::Pane(p) => model.panes.contains_key(&p),
            Scope::Window(w) => model.windows.contains_key(&w),
            Scope::Session(s) => model.sessions.contains_key(&s),
        }
    }

    /// Whether `event`, which happened at `place`, lies inside the scope.
    fn passes(self, event: &Event, place: Place, model: &Model) -> bool {
        let mut at = event.subject();
        if at.window.is_none() {
            at.window = place
                .window
                .or_else(|| at.pane.and_then(|p| model.panes.get(&p).map(|p| p.window)));
        }
        if at.session.is_none() {
            at.session = place.session.or_else(|| {
                at.window
                    .and_then(|w| model.windows.get(&w).map(|w| w.session))
            });
        }
        match self {
            Scope::Pane(p) => at.pane == Some(p),
            Scope::Window(w) => at.window == Some(w),
            Scope::Session(s) => at.session == Some(s),
        }
    }
}

struct Subscriber {
    /// Event names or `*` patterns; empty means every event but
    /// `pane-output`.
    filters: Vec<String>,
    /// `subscribe -t`: only events inside it pass.
    scope: Option<Scope>,
    /// The scope's target has gone: nothing passes any more.
    gone: bool,
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

    fn passes(&self, event: &Event, place: Place, model: &Model) -> bool {
        match self.scope {
            None => true,
            Some(_) if self.gone => false,
            Some(scope) => scope.passes(event, place, model),
        }
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

/// What a client's view was when `pane-focus` last looked at it.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Seen {
    window: WindowId,
    pane: PaneId,
    focused: bool,
}

/// The subscriber registry and the events waiting to be flushed.
#[derive(Default)]
pub struct Events {
    subscribers: Vec<Subscriber>,
    pending: Vec<(Event, Place)>,
    /// Each attached client's view as `pane-focus` last saw it.
    seen: BTreeMap<ClientId, Seen>,
}

impl Events {
    /// Sets the subscription of the connection that owns `tx`, replacing
    /// its earlier one. An empty `names` means every event but
    /// `pane-output`.
    pub fn subscribe(
        &mut self,
        names: Vec<String>,
        scope: Option<Scope>,
        tx: mpsc::Sender<Event>,
    ) -> Result<(), String> {
        if let Some(bad) = names
            .iter()
            .find(|n| !n.contains('*') && !NAMES.contains(&n.as_str()))
        {
            return Err(format!("unknown event: {bad}"));
        }
        self.subscribers.retain(|s| !s.tx.is_closed());
        match self.subscribers.iter_mut().find(|s| s.tx.same_channel(&tx)) {
            Some(s) => {
                s.filters = names;
                s.scope = scope;
                s.gone = false;
            }
            None => self.subscribers.push(Subscriber {
                filters: names,
                scope,
                gone: false,
                tx,
                dropped: 0,
            }),
        }
        self.update_tap();
        Ok(())
    }

    /// Records an event, sent on the next [`Events::flush`].
    pub fn emit(&mut self, event: Event) {
        self.emit_at(event, Place::default());
    }

    /// As [`Events::emit`], for an event about a pane that is no longer in
    /// the model: `place` is where it was.
    pub fn emit_at(&mut self, event: Event, place: Place) {
        if self.subscribers.iter().any(|s| s.wants(event.name())) {
            self.pending.push((event, place));
        }
    }

    /// Records `pane-exited` with a null status for panes a kill removed.
    pub fn panes_killed(&mut self, removed: Removed) {
        for (pane, uid, place) in removed {
            let event = Event::PaneExited {
                pane,
                uid,
                status: None,
            };
            self.emit_at(event, place);
        }
    }

    /// Queues every pending event to the subscribers that want it, in
    /// order, and forgets subscribers whose connection is gone. `model` is
    /// what `subscribe -t` limits are measured against.
    pub fn flush(&mut self, model: &Model) {
        let pending = std::mem::take(&mut self.pending);
        let before = self.subscribers.len();
        for (event, place) in &pending {
            let name = event.name();
            self.subscribers.retain_mut(|s| {
                !s.wants(name) || !s.passes(event, *place, model) || s.offer(event)
            });
        }
        for s in &mut self.subscribers {
            s.gone |= s.scope.is_some_and(|scope| !scope.alive(model));
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

/// `pane-focus`, the one emission site for it (spec §7.2): compares each
/// attached client's current window, active pane and terminal focus with
/// what it showed last time, and emits for the clients where the window or
/// pane changed, or where the terminal regained focus (while `focus-events`
/// is on, as for hooks). Run by the actor after each message, so one action
/// gives one event per client whatever number of verbs it took. A new
/// client counts as changed (an attach). A session with no attached client
/// has no clients here, so it never emits.
pub fn track_focus(state: &mut State) {
    let focus_events = state.model.options.lookup("focus-events", &[]) == Some("on");
    let State {
        events,
        clients,
        model,
        ..
    } = state;
    events.seen.retain(|id, _| clients.contains_key(id));
    for (id, client) in clients.iter() {
        let Some(session) = model.sessions.get(&client.session) else {
            continue;
        };
        let window = session.active;
        let Some(pane) = model.windows.get(&window).map(|w| w.active) else {
            continue;
        };
        let now = Seen {
            window,
            pane,
            focused: client.focused,
        };
        let regained = |before: Seen| !before.focused && now.focused && focus_events;
        let changed = match events.seen.insert(*id, now) {
            None => true,
            Some(before) => {
                before.window != now.window || before.pane != now.pane || regained(before)
            }
        };
        if changed {
            events.emit(Event::PaneFocus {
                pane,
                window,
                session: client.session,
                client: client.name.clone(),
            });
        }
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
            scope: None,
            gone: false,
            tx: tx.clone(),
            dropped: 0,
        };
        let star = Subscriber {
            filters: names(&["*"]),
            scope: None,
            gone: false,
            tx: tx.clone(),
            dropped: 0,
        };
        let named = Subscriber {
            filters: names(&["pane-output"]),
            scope: None,
            gone: false,
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
        events.subscribe(vec![], None, tx).unwrap();
        for i in 0..5 {
            events.emit(changed(&i.to_string()));
        }
        events.flush(&Model::default());
        assert_eq!(rx.recv().await, Some(changed("0")));
        assert_eq!(rx.recv().await, Some(changed("1")));
        assert!(rx.try_recv().is_err());
        events.emit(changed("5"));
        events.flush(&Model::default());
        assert_eq!(rx.recv().await, Some(Event::EventsDropped { count: 3 }));
        assert_eq!(rx.recv().await, Some(changed("5")));
    }

    #[test]
    fn closed_subscribers_are_removed_on_the_next_flush() {
        let mut events = Events::default();
        let (live_tx, mut live_rx) = mpsc::channel(8);
        events.subscribe(vec![], None, live_tx).unwrap();
        for _ in 0..10 {
            let (tx, rx) = mpsc::channel(8);
            events.subscribe(vec![], None, tx).unwrap();
            drop(rx);
        }
        events.emit(changed("1"));
        events.flush(&Model::default());
        assert_eq!(events.len(), 1);
        assert_eq!(live_rx.try_recv().ok(), Some(changed("1")));
    }

    #[test]
    fn resubscribing_replaces_the_filters() {
        let mut events = Events::default();
        let (tx, mut rx) = mpsc::channel(8);
        events
            .subscribe(names(&["pane-exited"]), None, tx.clone())
            .unwrap();
        events
            .subscribe(names(&["option-changed"]), None, tx)
            .unwrap();
        assert_eq!(events.len(), 1);
        events.emit(Event::PaneExited {
            pane: PaneId(1),
            uid: "u".into(),
            status: None,
        });
        events.emit(changed("1"));
        events.flush(&Model::default());
        assert_eq!(rx.try_recv().ok(), Some(changed("1")));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn unknown_names_are_refused() {
        let mut events = Events::default();
        let (tx, _rx) = mpsc::channel(1);
        let err = events.subscribe(names(&["pane-exitted"]), None, tx.clone());
        assert_eq!(err, Err("unknown event: pane-exitted".to_string()));
        assert!(events.subscribe(names(&["window-*"]), None, tx).is_ok());
    }

    fn exited(pane: u32) -> Event {
        Event::PaneExited {
            pane: PaneId(pane),
            uid: "u".into(),
            status: None,
        }
    }

    #[test]
    fn a_scope_whose_target_has_gone_passes_nothing_more() {
        let model = Model::default();
        let mut events = Events::default();
        let (tx, mut rx) = mpsc::channel(8);
        let scope = Some(Scope::Pane(PaneId(1)));
        events.subscribe(vec![], scope, tx).unwrap();
        // The target is not in the model: its own last events still pass,
        // and the other pane's do not.
        events.emit(exited(1));
        events.emit(exited(2));
        events.flush(&model);
        assert_eq!(rx.try_recv().ok(), Some(exited(1)));
        assert!(rx.try_recv().is_err());
        events.emit(exited(1));
        events.flush(&model);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_killed_panes_place_decides_the_window_and_session_scopes() {
        let model = Model::default();
        let place = Place {
            window: Some(WindowId(4)),
            session: Some(SessionId(2)),
        };
        let event = exited(9);
        assert!(Scope::Window(WindowId(4)).passes(&event, place, &model));
        assert!(Scope::Session(SessionId(2)).passes(&event, place, &model));
        assert!(Scope::Pane(PaneId(9)).passes(&event, place, &model));
        assert!(!Scope::Window(WindowId(5)).passes(&event, place, &model));
        assert!(!Scope::Session(SessionId(3)).passes(&event, place, &model));
        // Without a place and a model that lacks the pane, it names no window.
        assert!(!Scope::Window(WindowId(4)).passes(&event, Place::default(), &model));
    }

    #[test]
    fn option_changed_passes_by_its_scope() {
        let model = Model::default();
        let changed = |scope: &str| Event::OptionChanged {
            scope: scope.into(),
            name: "@x".into(),
            value: None,
        };
        let none = Place::default();
        assert!(Scope::Session(SessionId(1)).passes(&changed("$1"), none, &model));
        assert!(!Scope::Session(SessionId(1)).passes(&changed("global"), none, &model));
        assert!(Scope::Window(WindowId(2)).passes(&changed("@2"), none, &model));
        assert!(Scope::Pane(PaneId(3)).passes(&changed("%3"), none, &model));
        assert!(!Scope::Pane(PaneId(3)).passes(&changed("%4"), none, &model));
    }
}
