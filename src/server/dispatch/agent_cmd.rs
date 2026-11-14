//! `agent report|list|next|seen` (spec §10): the command handlers over the
//! agent store; the state change itself is `dispatch::agent`.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::cmd::{AgentCmd, AgentReport, Resolve, Target, expand};
use crate::protocol::ClientCtx;
use crate::server::agents::{Rank, Report, ReportState, rank};
use crate::server::dispatch::options::{FormatCtx, ServerVars};
use crate::server::dispatch::{agent, agent_new};
use crate::server::{State, clients};
use crate::types::PaneId;

const LIST_FORMAT: &str = "#{pane_id} #{agent_kind} #{agent_state}";

pub fn run(state: &mut State, ctx: &ClientCtx, c: AgentCmd) -> Result<String, String> {
    match c {
        AgentCmd::Report(r) => report(state, ctx, r),
        AgentCmd::List { format } => Ok(list(state, format.as_deref().unwrap_or(LIST_FORMAT))),
        AgentCmd::Next => next(state, ctx),
        AgentCmd::Seen { target, all } => seen(state, ctx, &target, all),
        AgentCmd::New(c) => agent_new::run(state, ctx, c),
    }
}

fn report(state: &mut State, ctx: &ClientCtx, r: AgentReport) -> Result<String, String> {
    let parsed =
        ReportState::parse(&r.state).ok_or_else(|| format!("unknown agent state: {}", r.state))?;
    let pane = state.model.resolve_pane(&r.target, ctx)?;
    let ts = r.ts.unwrap_or_else(now_micros);
    let report = Report {
        state: parsed,
        kind: r.kind,
        msg: r.msg,
        blocked: r.blocked,
        session_id: r.session_id,
        ts,
    };
    agent::apply(state, pane, report);
    Ok(String::new())
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// One line per agent pane, most urgent first.
fn list(state: &State, format: &str) -> String {
    state
        .agents
        .by_urgency()
        .into_iter()
        .filter(|p| state.model.panes.contains_key(p))
        .map(|p| {
            let vars = FormatCtx::new(&state.model, p);
            format!("{}\n", expand(format, &ServerVars::fresh(state, p, &vars)))
        })
        .collect()
}

/// Jumps to the most urgent pane that needs attention, oldest first
/// within a rank (the order `by_urgency` gives).
fn next(state: &mut State, ctx: &ClientCtx) -> Result<String, String> {
    let target = state.agents.by_urgency().into_iter().find(|p| {
        state
            .agents
            .get(*p)
            .is_some_and(|a| rank(a) <= Rank::Unseen)
    });
    match target {
        Some(pane) => agent::jump(state, ctx, pane).map(|()| String::new()),
        None => Ok("no agent needs you\n".to_string()),
    }
}

fn seen(state: &mut State, ctx: &ClientCtx, target: &Target, all: bool) -> Result<String, String> {
    let panes: Vec<PaneId> = if all {
        state.agents.iter().map(|(p, _)| p).collect()
    } else {
        vec![state.model.resolve_pane(target, ctx)?]
    };
    let mut changed = false;
    for pane in panes {
        changed |= state.agents.mark_seen(pane);
    }
    if changed {
        clients::mark_all_dirty(state);
    }
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::agents::AgentState;

    /// A server with two windows in one session; returns their panes.
    fn two_panes() -> (State, ClientCtx, PaneId, PaneId) {
        let mut state = crate::server::test_state();
        let ctx = crate::server::test_ctx();
        for words in [
            &["new-session", "-d", "-s", "a"][..],
            &["new-window", "-d"][..],
        ] {
            let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            crate::server::dispatch::run(&mut state, &ctx, &argv).expect("setup");
        }
        let mut panes: Vec<PaneId> = state.model.panes.keys().copied().collect();
        panes.sort();
        (state, ctx, panes[0], panes[1])
    }

    fn rep(state: &str, pane: Option<PaneId>, kind: &str) -> AgentCmd {
        AgentCmd::Report(AgentReport {
            state: state.into(),
            target: pane.map_or(Target::Current, |p| {
                Target::Pane(crate::cmd::PaneRef {
                    session: crate::cmd::SessionRef::Current,
                    window: crate::cmd::WindowRef::Current,
                    pane: crate::cmd::PanePart::Id(p),
                })
            }),
            kind: Some(kind.into()),
            msg: None,
            blocked: None,
            session_id: None,
            ts: Some(1),
        })
    }

    #[tokio::test]
    async fn list_urgency_order() {
        let (mut state, ctx, a, b) = two_panes();
        run(&mut state, &ctx, rep("idle", Some(b), "pi")).expect("report");
        run(&mut state, &ctx, rep("blocked", Some(a), "claude")).expect("report");
        let want = format!("{a} claude blocked\n{b} pi idle\n");
        assert_eq!(
            run(&mut state, &ctx, AgentCmd::List { format: None }),
            Ok(want)
        );
    }

    #[tokio::test]
    async fn next_jumps_most_urgent_oldest() {
        let (mut state, ctx, a, b) = two_panes();
        run(&mut state, &ctx, rep("blocked", Some(b), "claude")).expect("report");
        run(&mut state, &ctx, rep("idle", Some(a), "pi")).expect("report");
        assert_eq!(run(&mut state, &ctx, AgentCmd::Next), Ok(String::new()));
        let window = state.model.panes[&b].window;
        let session = state.model.sessions.values().next().expect("session");
        assert_eq!(session.active, window);
    }

    #[tokio::test]
    async fn next_none() {
        let (mut state, ctx, a, _) = two_panes();
        run(&mut state, &ctx, rep("idle", Some(a), "pi")).expect("report");
        assert_eq!(
            run(&mut state, &ctx, AgentCmd::Next),
            Ok("no agent needs you\n".to_string())
        );
    }

    #[tokio::test]
    async fn seen_all() {
        let (mut state, ctx, a, b) = two_panes();
        run(&mut state, &ctx, rep("done", Some(a), "pi")).expect("report");
        run(&mut state, &ctx, rep("done", Some(b), "pi")).expect("report");
        assert!(state.agents.iter().all(|(_, g)| g.unseen));
        let all = AgentCmd::Seen {
            target: Target::Current,
            all: true,
        };
        run(&mut state, &ctx, all).expect("seen");
        assert!(state.agents.iter().all(|(_, g)| !g.unseen));
    }

    #[tokio::test]
    async fn report_default_target_is_ctx_pane() {
        let (mut state, mut ctx, _, b) = two_panes();
        ctx.pane = Some(b);
        run(&mut state, &ctx, rep("working", None, "claude")).expect("report");
        let got: Vec<PaneId> = state.agents.iter().map(|(p, _)| p).collect();
        assert_eq!(got, vec![b]);
        assert_eq!(
            state.agents.get(b).map(|g| g.state),
            Some(AgentState::Working)
        );
    }

    #[tokio::test]
    async fn report_unknown_state_err() {
        let (mut state, ctx, a, _) = two_panes();
        let got = run(&mut state, &ctx, rep("nope", Some(a), "pi"));
        assert!(got.is_err_and(|e| e.contains("nope")));
        assert!(state.agents.get(a).is_none());
    }
}
