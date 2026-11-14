//! Agent status: the per-pane store and the report rules (spec §2).

pub mod kinds;
pub mod osc;
mod rank;
mod text;

use std::collections::HashMap;
use std::time::Instant;

use crate::server::resume::Binding;
use crate::types::PaneId;

pub use rank::{Rank, rank};
pub use text::{sanitize, valid_kind, valid_session_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
    Done,
    Error,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Done => "done",
            AgentState::Error => "error",
        }
    }
}

/// What a report asks for: set a state, or drop the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportState {
    Set(AgentState),
    Clear,
}

impl ReportState {
    pub fn parse(s: &str) -> Option<ReportState> {
        Some(match s {
            "idle" => ReportState::Set(AgentState::Idle),
            "working" => ReportState::Set(AgentState::Working),
            "blocked" => ReportState::Set(AgentState::Blocked),
            "done" | "completed" => ReportState::Set(AgentState::Done),
            "error" => ReportState::Set(AgentState::Error),
            "clear" | "exited" => ReportState::Clear,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub state: ReportState,
    pub kind: Option<String>,
    pub msg: Option<String>,
    pub blocked: Option<String>,
    pub session_id: Option<String>,
    /// The sender's timestamp, microseconds.
    pub ts: u64,
}

#[derive(Debug, Clone)]
pub struct Agent {
    pub kind: Option<String>,
    pub state: AgentState,
    pub blocked: Option<String>,
    pub msg: Option<String>,
    pub since: Instant,
    pub unseen: bool,
    pub session_id: Option<String>,
    pub reported: bool,
    pub last_report: u64,
    pub binding: Option<Binding>,
}

impl Agent {
    fn new(state: AgentState, now: Instant, binding: Option<Binding>) -> Agent {
        Agent {
            kind: None,
            state,
            blocked: None,
            msg: None,
            since: now,
            unseen: false,
            session_id: None,
            reported: false,
            last_report: 0,
            binding,
        }
    }
}

/// What a report changed; `new` is `None` when the record was removed.
/// `kind` is the record's kind at that moment, kept here because a removed
/// record can no longer be asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub old: Option<AgentState>,
    pub new: Option<AgentState>,
    pub kind: Option<String>,
    pub session_changed: bool,
}

#[derive(Debug, Default)]
pub struct Agents {
    map: HashMap<PaneId, Agent>,
}

impl Agents {
    /// Apply a report. `None` when it was dropped or changed nothing.
    pub fn apply(&mut self, pane: PaneId, r: Report, seen: bool, now: Instant) -> Option<Change> {
        if let Some(a) = self.map.get(&pane)
            && r.ts < a.last_report
        {
            return None;
        }
        let state = match r.state {
            ReportState::Clear => {
                let a = self.map.remove(&pane)?;
                return Some(Change {
                    old: Some(a.state),
                    new: None,
                    kind: a.kind,
                    session_changed: false,
                });
            }
            ReportState::Set(s) => s,
        };
        let created = !self.map.contains_key(&pane);
        let a = self
            .map
            .entry(pane)
            .or_insert_with(|| Agent::new(state, now, None));
        let old = (!created).then_some(a.state);
        let state_changed = created || a.state != state;

        a.reported = true;
        a.last_report = r.ts;
        if let Some(k) = r.kind.filter(|k| valid_kind(k)) {
            a.kind = Some(k);
        }
        let blocked = if state == AgentState::Blocked {
            r.blocked
                .or_else(|| a.blocked.clone().filter(|_| !state_changed))
        } else {
            None
        };
        let msg = match r.msg {
            Some(m) => Some(sanitize(&m)),
            None if state_changed => None,
            None => a.msg.clone(),
        };
        let session_changed = r
            .session_id
            .filter(|s| valid_session_id(s))
            .is_some_and(|s| {
                let changed = a.session_id.as_deref() != Some(&s);
                a.session_id = Some(s);
                changed
            });
        let content_changed = blocked != a.blocked || msg != a.msg;
        a.blocked = blocked;
        a.msg = msg;

        if state_changed {
            a.state = state;
            a.since = now;
            match state {
                AgentState::Working | AgentState::Idle => a.unseen = false,
                _ if !seen => a.unseen = true,
                _ => {}
            }
        }
        (state_changed || content_changed || session_changed).then_some(Change {
            old,
            new: Some(state),
            kind: a.kind.clone(),
            session_changed,
        })
    }

    /// Make sure `pane` has a record of `kind`. True when anything changed.
    pub fn ensure(
        &mut self,
        pane: PaneId,
        kind: &str,
        binding: Option<Binding>,
        now: Instant,
    ) -> bool {
        if let Some(a) = self.map.get_mut(&pane) {
            // A record bound to another process is stale; replace it.
            if binding.is_none() || a.binding.is_none() || a.binding == binding {
                if a.kind.is_some() || !valid_kind(kind) {
                    return false;
                }
                a.kind = Some(kind.to_string());
                return true;
            }
        }
        let mut a = Agent::new(AgentState::Idle, now, binding);
        a.kind = valid_kind(kind).then(|| kind.to_string());
        self.map.insert(pane, a);
        true
    }

    /// Clear `unseen`. True when it was set.
    pub fn mark_seen(&mut self, pane: PaneId) -> bool {
        self.map
            .get_mut(&pane)
            .is_some_and(|a| std::mem::take(&mut a.unseen))
    }

    /// Bell or OSC 9/777 on a pane that never reported (spec §3.3). True when
    /// the record changed.
    pub fn attention(&mut self, pane: PaneId, msg: Option<String>) -> bool {
        let Some(a) = self.map.get_mut(&pane).filter(|a| !a.reported) else {
            return false;
        };
        let msg = msg.map(|m| sanitize(&m));
        let changed = !a.unseen || msg.as_ref().is_some_and(|m| a.msg.as_ref() != Some(m));
        a.unseen = true;
        if msg.is_some() {
            a.msg = msg;
        }
        changed
    }

    pub fn remove(&mut self, pane: PaneId) -> Option<Agent> {
        self.map.remove(&pane)
    }

    pub fn get(&self, pane: PaneId) -> Option<&Agent> {
        self.map.get(&pane)
    }

    pub fn iter(&self) -> impl Iterator<Item = (PaneId, &Agent)> {
        self.map.iter().map(|(p, a)| (*p, a))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(state: ReportState, ts: u64) -> Report {
        Report {
            state,
            kind: None,
            msg: None,
            blocked: None,
            session_id: None,
            ts,
        }
    }

    fn set(s: AgentState, ts: u64) -> Report {
        report(ReportState::Set(s), ts)
    }

    const P: PaneId = PaneId(1);

    fn with(state: AgentState, ts: u64) -> (Agents, Instant) {
        let now = Instant::now();
        let mut ag = Agents::default();
        ag.apply(P, set(state, ts), true, now);
        (ag, now)
    }

    #[test]
    fn parse_names() {
        assert_eq!(
            ReportState::parse("completed"),
            Some(ReportState::Set(AgentState::Done))
        );
        assert_eq!(ReportState::parse("exited"), Some(ReportState::Clear));
        assert_eq!(ReportState::parse("nope"), None);
    }

    #[test]
    fn done_while_unseen_sets_unseen() {
        let (mut ag, now) = with(AgentState::Working, 5);
        let c = ag.apply(P, set(AgentState::Done, 10), false, now);
        assert_eq!(
            c,
            Some(Change {
                old: Some(AgentState::Working),
                new: Some(AgentState::Done),
                kind: None,
                session_changed: false
            })
        );
        assert!(ag.get(P).unwrap().unseen);
    }

    #[test]
    fn late_report_dropped() {
        let (mut ag, now) = with(AgentState::Done, 20);
        assert_eq!(ag.apply(P, set(AgentState::Working, 10), true, now), None);
        assert_eq!(ag.get(P).unwrap().state, AgentState::Done);
    }

    #[test]
    fn same_report_noop() {
        let (mut ag, now) = with(AgentState::Working, 1);
        assert_eq!(ag.apply(P, set(AgentState::Working, 2), true, now), None);
        let mut r = set(AgentState::Working, 3);
        r.msg = Some("x".into());
        assert!(ag.apply(P, r.clone(), true, now).is_some());
        r.ts = 4;
        assert_eq!(ag.apply(P, r, true, now), None);
    }

    #[test]
    fn working_clears_unseen() {
        let (mut ag, now) = with(AgentState::Working, 1);
        ag.apply(P, set(AgentState::Blocked, 2), false, now);
        assert!(ag.get(P).unwrap().unseen);
        ag.apply(P, set(AgentState::Working, 3), false, now);
        assert!(!ag.get(P).unwrap().unseen);
    }

    #[test]
    fn clear_removes() {
        let (mut ag, now) = with(AgentState::Working, 1);
        let c = ag.apply(P, report(ReportState::Clear, 2), true, now);
        assert_eq!(
            c,
            Some(Change {
                old: Some(AgentState::Working),
                new: None,
                kind: None,
                session_changed: false
            })
        );
        assert!(ag.get(P).is_none());
    }

    #[test]
    fn invalid_kind_ignored() {
        let now = Instant::now();
        let mut ag = Agents::default();
        let mut r = set(AgentState::Idle, 1);
        r.kind = Some("a b".into());
        ag.apply(P, r, true, now);
        assert_eq!(ag.get(P).unwrap().kind, None);
    }

    #[test]
    fn session_change_reported() {
        let (mut ag, now) = with(AgentState::Working, 1);
        let mut r = set(AgentState::Working, 2);
        r.session_id = Some("abc-1".into());
        let c = ag.apply(P, r, true, now).unwrap();
        assert!(c.session_changed);
    }

    #[test]
    fn attention_on_unreported_only() {
        let now = Instant::now();
        let mut ag = Agents::default();
        assert!(ag.ensure(P, "claude", None, now));
        assert!(ag.attention(P, Some("hi".into())));
        let a = ag.get(P).unwrap();
        assert!(a.unseen);
        assert_eq!(a.msg.as_deref(), Some("hi"));
        assert!(!ag.attention(P, Some("hi".into())));
        assert!(ag.mark_seen(P));
        assert!(!ag.mark_seen(P));

        let (mut ag, _) = with(AgentState::Working, 1);
        assert!(!ag.attention(P, Some("hi".into())));
        assert!(!ag.get(P).unwrap().unseen);
    }

    #[test]
    fn ensure_is_idempotent_and_remove_returns() {
        let now = Instant::now();
        let mut ag = Agents::default();
        assert!(ag.ensure(P, "claude", None, now));
        assert!(!ag.ensure(P, "claude", None, now));
        assert_eq!(ag.iter().count(), 1);
        assert!(ag.remove(P).is_some());
        assert!(ag.remove(P).is_none());
    }
}
