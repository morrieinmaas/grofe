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
//! `hooks::run_command`, `pane-focus` in [`track_focus`], and `agent-changed`
//! in [`emit_agent_changed`].
//!
//! **`subscribe -t`.** A subscription may be limited to a [`Scope`], a pane,
//! window or session resolved when it was made. An event passes when the
//! pane, window or session it names (its own fields, filled in from the
//! model, or from its [`Place`] for a pane that has gone) lies inside the
//! scope. A scope whose target has gone passes nothing more once the batch
//! that removed it is delivered.

use std::collections::BTreeMap;

use tokio::sync::mpsc;

use crate::cmd::format::fnmatch;
use crate::cmd::{Resolve, Target};
use crate::event::{Event, Place};
use crate::protocol::ClientCtx;
use crate::server::State;
use crate::server::agents;
use crate::server::dispatch;
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
    "agent-changed",
    "open-url",
    "ports-changed",
    "pane-echo",
];

/// `pane-output` is sent only to subscribers that name it: a bare
/// `subscribe` or a pattern does not match it (volume).
const PANE_OUTPUT: &str = "pane-output";

/// Events added after v1.0 (protocol-v1 §8) reach only a subscription that
/// names them, as `pane-output` does. The last two are also what the poller
/// in `server::remote` runs for.
const NAMED_ONLY: &[&str] = &[PANE_OUTPUT, "open-url", "ports-changed", "pane-echo"];

/// What a `subscribe -t` is limited to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Pane(PaneId),
    Window(WindowId),
    Session(SessionId),
    /// `subscribe -c`: the session `client` is attached to, moved along
    /// when it switches ([`Events::client_moved`]). Unlike [`Scope::Session`],
    /// an event of no pane, window or session passes.
    Client {
        client: ClientId,
        session: SessionId,
    },
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
            Scope::Session(s) | Scope::Client { session: s, .. } => model.sessions.contains_key(&s),
        }
    }

    /// Whether `event`, which happened at `place`, lies inside the scope.
    fn passes(self, event: &Event, place: Place, model: &Model) -> bool {
        let mut at = event.subject();
        // An event that names no session of its own is one of the window,
        // which every session holding it sees.
        let windowed = at.session.is_none();
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
            Scope::Client { .. }
                if at.pane.is_none() && at.window.is_none() && at.session.is_none() =>
            {
                true
            }
            Scope::Session(s) | Scope::Client { session: s, .. } => {
                at.session == Some(s) || windowed && at.window.is_some_and(|w| model.has_link(s, w))
            }
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
        if NAMED_ONLY.contains(&name) {
            return self.filters.iter().any(|f| f == name);
        }
        self.filters.is_empty()
            || self
                .filters
                .iter()
                .any(|f| fnmatch(f.as_bytes(), name.as_bytes()))
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

    /// Whether some subscription names event `name` (a [`NAMED_ONLY`] one):
    /// what makes `open-url` deliverable and the poller worth running.
    pub fn named(&self, name: &str) -> bool {
        self.subscribers
            .iter()
            .any(|s| !s.tx.is_closed() && s.filters.iter().any(|f| f == name))
    }

    /// Client `client` is attached to `session` now: its `subscribe -c`
    /// subscriptions follow it.
    pub fn client_moved(&mut self, client: ClientId, session: SessionId) {
        for s in &mut self.subscribers {
            if let Some(Scope::Client { client: c, .. }) = s.scope
                && c == client
            {
                s.scope = Some(Scope::Client { client, session });
                s.gone = false;
            }
        }
    }

    /// Whether `event` would reach a subscriber: one that names it, whose
    /// scope it lies in.
    pub fn reaches(&self, event: &Event, model: &Model) -> bool {
        self.subscribers.iter().any(|s| {
            !s.tx.is_closed() && s.wants(event.name()) && s.passes(event, Place::default(), model)
        })
    }

    /// How many live subscriptions name event `name`.
    pub fn named_count(&self, name: &str) -> usize {
        self.subscribers
            .iter()
            .filter(|s| !s.tx.is_closed() && s.filters.iter().any(|f| f == name))
            .count()
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
    if state.clients.is_empty() {
        state.events.seen.clear();
        return;
    }
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
    mark_agents_seen(state);
}

/// Marks each attached client's active pane seen when
/// `dispatch::agent::is_seen` says it is; clients redraw if that changed
/// anything (spec §2.4).
fn mark_agents_seen(state: &mut State) {
    let panes: Vec<PaneId> = state
        .clients
        .values()
        .filter_map(|c| {
            let window = state.model.sessions.get(&c.session)?.active;
            Some(state.model.windows.get(&window)?.active)
        })
        .collect();
    let mut changed = false;
    for pane in panes {
        if dispatch::agent::is_seen(state, pane) {
            changed |= state.agents.mark_seen(pane);
            changed |= state.notify.seen(pane);
        }
    }
    if changed {
        crate::server::clients::mark_all_dirty(state);
    }
}

/// `agent-changed` for `pane` (spec §10): `old` and `new` are the state
/// names, null for no record.
pub fn emit_agent_changed(state: &mut State, pane: PaneId, change: &agents::Change) {
    let uid = state
        .model
        .panes
        .get(&pane)
        .map(|p| p.uid.clone())
        .unwrap_or_default();
    state.events.emit(Event::AgentChanged {
        pane,
        uid,
        kind: change.kind.clone(),
        old: change.old.map(|s| s.as_str().to_string()),
        new: change.new.map(|s| s.as_str().to_string()),
    });
    crate::server::hooks::agent_changed(state, pane);
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

    /// A window linked into two sessions is in both: an event of the window
    /// reaches a subscriber scoped to either session, not a third.
    #[tokio::test]
    async fn a_session_scope_sees_the_events_of_a_window_it_holds() {
        let mut state = crate::server::test_state();
        let ctx = crate::server::test_ctx();
        let run = |state: &mut State, words: &[&str]| {
            let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            crate::server::dispatch::run(state, &ctx, &argv).expect("command")
        };
        for name in ["a", "b", "c"] {
            run(&mut state, &["new-session", "-d", "-s", name, "sleep 30"]);
        }
        let model = &state.model;
        let id = |name: &str| model.session_by_name(name).expect("session");
        let (a, b, c) = (id("a"), id("b"), id("c"));
        let window = model.sessions[&a].active;
        let src = window.to_string();
        run(&mut state, &["link-window", "-d", "-s", &src, "-t", "b:7"]);
        let model = &state.model;
        let place = Place {
            window: Some(window),
            session: Some(a),
        };
        let event = exited(9);
        assert!(Scope::Session(a).passes(&event, place, model));
        assert!(Scope::Session(b).passes(&event, place, model));
        assert!(!Scope::Session(c).passes(&event, place, model));
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

    /// `subscribe -c`: the client's session's events, then the next one's
    /// after it switches; an event of no place (a global option) passes.
    #[test]
    fn a_client_scope_follows_its_client() {
        let model = Model::default();
        let place = |s: u32| Place {
            window: Some(WindowId(s)),
            session: Some(SessionId(s)),
        };
        let mut events = Events::default();
        let (tx, mut rx) = mpsc::channel(8);
        let scope = Scope::Client {
            client: ClientId(7),
            session: SessionId(1),
        };
        events.subscribe(vec![], Some(scope), tx).unwrap();
        let global = Event::OptionChanged {
            scope: "global".into(),
            name: "".into(),
            value: None,
        };
        assert!(scope.passes(&global, Place::default(), &model));
        events.emit_at(exited(1), place(1));
        events.emit_at(exited(2), place(2));
        events.flush(&model);
        assert_eq!(rx.try_recv().ok(), Some(exited(1)));
        assert!(rx.try_recv().is_err());
        // Another client moving changes nothing; this one moving does.
        events.client_moved(ClientId(8), SessionId(1));
        events.client_moved(ClientId(7), SessionId(2));
        events.emit_at(exited(1), place(1));
        events.emit_at(exited(2), place(2));
        events.flush(&model);
        assert_eq!(rx.try_recv().ok(), Some(exited(2)));
        assert!(rx.try_recv().is_err());
    }

    /// Events added after v1.0 reach only a subscription that names them,
    /// not a bare one or a pattern (protocol-v1 §8).
    #[test]
    fn remote_events_need_their_own_name() {
        let (tx, _rx) = mpsc::channel(4);
        let mut events = Events::default();
        events.subscribe(vec![], None, tx).unwrap();
        for name in ["open-url", "ports-changed", "pane-echo"] {
            assert!(!events.named(name), "{name}");
        }
        let (tx, _rx2) = mpsc::channel(4);
        events
            .subscribe(vec!["*".to_string()], None, tx.clone())
            .unwrap();
        let sub = &events.subscribers[1];
        assert!(!sub.wants("open-url") && !sub.wants("ports-changed") && !sub.wants("pane-echo"));
        assert!(sub.wants("pane-exited"));
        events
            .subscribe(vec!["pane-echo".to_string()], None, tx)
            .unwrap();
        assert!(events.named("pane-echo") && !events.named("ports-changed"));
    }

    #[test]
    fn agent_changed_in_names() {
        assert!(NAMES.contains(&"agent-changed"));
        let mut events = Events::default();
        let (tx, _rx) = mpsc::channel(4);
        events
            .subscribe(names(&["agent-changed"]), None, tx)
            .expect("subscribe");
    }

    #[test]
    fn set_hook_agent_changed_accepted() {
        let mut state = crate::server::test_state();
        let ctx = crate::server::test_ctx();
        let argv: Vec<String> = ["set-hook", "-g", "agent-changed", "list-sessions"]
            .iter()
            .map(|w| w.to_string())
            .collect();
        crate::server::dispatch::run(&mut state, &ctx, &argv).expect("set-hook");
    }

    /// A state with one session `main`, one attached client, and an agent
    /// record on its active pane.
    fn agent_state() -> (State, PaneId, ClientId) {
        use crate::server::agents::{AgentState, Report, ReportState};
        use crate::server::clients::{AttachRequest, attach};
        let mut state = crate::server::test_state();
        let ctx = crate::server::test_ctx();
        let argv: Vec<String> = ["new-session", "-d", "-s", "main", "sleep 30"]
            .iter()
            .map(|w| w.to_string())
            .collect();
        crate::server::dispatch::run(&mut state, &ctx, &argv).expect("new-session");
        let accepted = attach(
            &mut state,
            AttachRequest {
                ctx: ctx.clone(),
                size: (80, 24),
                term: "xterm".to_string(),
                caps: Default::default(),
                pid: None,
                target: Some("main".to_string()),
                detach_others: false,
                nest: true,
                tty: None,
                created: false,
                extras: Default::default(),
            },
        )
        .expect("attach");
        let session = state.model.session_by_name("main").expect("session");
        let window = state.model.sessions[&session].active;
        let pane = state.model.windows[&window].active;
        let report = Report {
            state: ReportState::Set(AgentState::Done),
            kind: None,
            msg: None,
            blocked: None,
            session_id: None,
            ts: 1,
        };
        state
            .agents
            .apply(pane, report, false, std::time::Instant::now());
        (state, pane, accepted.id)
    }

    #[tokio::test]
    async fn seen_clears_unseen_focused() {
        let (mut state, pane, _) = agent_state();
        assert!(state.agents.get(pane).expect("record").unseen);
        track_focus(&mut state);
        assert!(!state.agents.get(pane).expect("record").unseen);
    }

    #[tokio::test]
    async fn unfocused_client_not_seen() {
        let (mut state, pane, id) = agent_state();
        state.clients.get_mut(&id).expect("client").focused = false;
        track_focus(&mut state);
        assert!(state.agents.get(pane).expect("record").unseen);
    }

    #[tokio::test]
    async fn focus_unknown_counts_seen() {
        // A client that never reported focus starts focused, as in tmux.
        let (mut state, pane, id) = agent_state();
        assert!(state.clients[&id].focused);
        track_focus(&mut state);
        assert!(!state.agents.get(pane).expect("record").unseen);
    }

    #[tokio::test]
    async fn emit_agent_changed_names_the_pane_and_states() {
        use crate::server::agents::{AgentState, Change};
        let (mut state, pane, _) = agent_state();
        let (tx, mut rx) = mpsc::channel(4);
        state
            .events
            .subscribe(names(&["agent-changed"]), None, tx)
            .expect("subscribe");
        let change = Change {
            old: None,
            new: Some(AgentState::Idle),
            kind: None,
            session_changed: false,
        };
        emit_agent_changed(&mut state, pane, &change);
        state.events.flush(&state.model);
        let event = rx.try_recv().expect("event");
        let uid = state.model.panes[&pane].uid.clone();
        assert_eq!(event.name(), "agent-changed");
        assert!(matches!(
            event,
            Event::AgentChanged { pane: p, uid: u, old: None, new: Some(n), .. }
                if p == pane && u == uid && n == "idle"
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seen_marks_read() {
        use crate::server::{dispatch, test_ctx, test_state};
        let mut state = test_state();
        let argv = ["new-session", "-d", "-s", "a", "sleep 600"].map(String::from);
        dispatch::run(&mut state, &test_ctx(), &argv).expect("new-session");
        let pane = *state.model.panes.keys().next().expect("pane");
        state.notify.cfg.delivery = "off".into();
        let accepted = crate::server::clients::attach(
            &mut state,
            crate::server::clients::AttachRequest {
                ctx: crate::protocol::ClientCtx {
                    client: None,
                    ..test_ctx()
                },
                size: (80, 24),
                term: "xterm".into(),
                caps: Default::default(),
                pid: None,
                target: Some("a".into()),
                detach_others: false,
                nest: false,
                tty: None,
                created: false,
                extras: Default::default(),
            },
        )
        .expect("attach");
        state.clients.get_mut(&accepted.id).expect("client").focused = false;
        let report = crate::server::agents::Report {
            state: crate::server::agents::ReportState::Set(
                crate::server::agents::AgentState::Blocked,
            ),
            kind: Some("claude".into()),
            msg: Some("perm".into()),
            blocked: None,
            session_id: None,
            ts: 1,
        };
        dispatch::agent::apply(&mut state, pane, report);
        assert!(!state.notify.items()[0].read);
        state.clients.get_mut(&accepted.id).expect("client").focused = true;
        mark_agents_seen(&mut state);
        assert!(state.notify.items()[0].read);
    }
}
