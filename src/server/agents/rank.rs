//! Urgency ranking (spec §2.5).

use super::{Agent, AgentState, Agents};
use crate::types::PaneId;

/// Most urgent first; derived `Ord` follows declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rank {
    Blocked,
    Error,
    Unseen,
    Working,
    Idle,
    Done,
}

impl Rank {
    pub fn as_str(self) -> &'static str {
        match self {
            Rank::Blocked => "blocked",
            Rank::Error => "error",
            Rank::Unseen => "unseen",
            Rank::Working => "working",
            Rank::Idle => "idle",
            Rank::Done => "done",
        }
    }
}

pub fn rank(a: &Agent) -> Rank {
    match a.state {
        AgentState::Blocked => Rank::Blocked,
        AgentState::Error => Rank::Error,
        _ if a.unseen => Rank::Unseen,
        AgentState::Working => Rank::Working,
        AgentState::Idle => Rank::Idle,
        AgentState::Done => Rank::Done,
    }
}

impl Agents {
    /// Panes ordered by rank, then oldest `since` first.
    pub fn by_urgency(&self) -> Vec<PaneId> {
        let mut v: Vec<_> = self.iter().map(|(p, a)| (rank(a), a.since, p)).collect();
        v.sort_by_key(|&(r, s, _)| (r, s));
        v.into_iter().map(|(_, _, p)| p).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::agents::{Report, ReportState};
    use std::time::{Duration, Instant};

    fn report(state: &str) -> Report {
        Report {
            state: ReportState::parse(state).unwrap(),
            kind: Some("claude".into()),
            msg: None,
            blocked: None,
            session_id: None,
            ts: 0,
        }
    }

    fn put(agents: &mut Agents, pane: u32, state: &str, seen: bool, at: Instant) {
        agents.apply(PaneId(pane), report(state), seen, at);
    }

    #[test]
    fn rank_order() {
        let t = Instant::now();
        let mut a = Agents::default();
        put(&mut a, 1, "done", true, t); // seen done
        put(&mut a, 2, "idle", true, t);
        put(&mut a, 3, "working", true, t);
        put(&mut a, 4, "done", false, t); // done but unseen
        put(&mut a, 5, "error", true, t);
        put(&mut a, 6, "blocked", true, t);
        let ranks: Vec<_> = a
            .by_urgency()
            .iter()
            .map(|p| rank(a.get(*p).unwrap()))
            .collect();
        assert_eq!(
            ranks,
            [
                Rank::Blocked,
                Rank::Error,
                Rank::Unseen,
                Rank::Working,
                Rank::Idle,
                Rank::Done
            ]
        );
        assert_eq!(Rank::Unseen.as_str(), "unseen");
    }

    #[test]
    fn by_urgency_oldest_first_within_rank() {
        let t = Instant::now();
        let mut a = Agents::default();
        put(&mut a, 1, "blocked", true, t + Duration::from_secs(5));
        put(&mut a, 2, "blocked", true, t);
        put(&mut a, 3, "blocked", true, t + Duration::from_secs(2));
        assert_eq!(a.by_urgency(), [PaneId(2), PaneId(3), PaneId(1)]);
    }
}
