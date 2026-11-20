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
    "notify",
];

/// `pane-output` is sent only to subscribers that name it: a bare
/// `subscribe` or a pattern does not match it (volume).
const PANE_OUTPUT: &str = "pane-output";

/// Events added after v1.0 (protocol-v1 §8) reach only a subscription that
/// names them, as `pane-output` does. The last two are also what the poller
/// in `server::remote` runs for.
const NAMED_ONLY: &[&str] = &[
    PANE_OUTPUT,
    "open-url",
    "ports-changed",
    "pane-echo",
    "notify",
];

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
            // An `open-url` must come from a pane of the client's session: one
            // from no pane (cron, another login) is not this client's to open.
            Scope::Client { .. }
                if at.pane.is_none() && at.window.is_none() && at.session.is_none() =>
            {
                !matches!(event, Event::OpenUrl { .. })
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
    /// scope it lies in. A `subscribe -c` reaches only while its client is
    /// `attached`: a client whose link dropped keeps its event channel open
    /// until the proxy gives up on it, and nothing would open what it gets.
    pub fn reaches(
        &self,
        event: &Event,
        model: &Model,
        attached: impl Fn(ClientId) -> bool,
    ) -> bool {
        self.subscribers.iter().any(|s| {
            let present = match s.scope {
                Some(Scope::Client { client, .. }) => attached(client),
                _ => true,
            };
            present
                && !s.tx.is_closed()
                && s.wants(event.name())
                && s.passes(event, Place::default(), model)
        })
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
mod tests;
