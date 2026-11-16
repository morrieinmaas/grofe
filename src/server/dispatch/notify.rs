//! The notify commands (agents spec §6).

use std::time::Instant;

use crate::cmd::NotifyCmd;
use crate::config::Resolved;
use crate::protocol::ClientCtx;
use crate::server::State;

pub fn configure(state: &mut State, r: &Resolved) {
    state.notify.cfg = r.notify.clone();
}

pub fn run(state: &mut State, _ctx: &ClientCtx, c: NotifyCmd) -> Result<String, String> {
    match c {
        NotifyCmd::List => Ok(state
            .notify
            .items()
            .iter()
            .map(|n| {
                format!(
                    "{} {} {} {} {}\n",
                    n.id,
                    n.pane,
                    n.kind.as_deref().unwrap_or("-"),
                    n.state,
                    n.msg.as_deref().unwrap_or("")
                )
            })
            .collect()),
        NotifyCmd::Dismiss(id) => {
            if state.notify.dismiss(id) {
                Ok(String::new())
            } else {
                Err(format!("no notification {id}"))
            }
        }
        NotifyCmd::Clear => {
            state.notify.clear();
            Ok(String::new())
        }
        NotifyCmd::Mute(d) => {
            state.notify.mute(d.map(|d| Instant::now() + d));
            Ok(String::new())
        }
        NotifyCmd::Unmute => {
            state.notify.unmute();
            Ok(String::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::server::notify::Notifier;
    use crate::server::{test_ctx, test_state};
    use crate::types::PaneId;

    /// Raise one blocked item per pane id; real `Notifier` by default.
    fn raise(state: &mut State, pane: u32, msg: &str) {
        let n: &mut Notifier = &mut state.notify;
        let r = n.on_change(
            PaneId(pane),
            &format!("u{pane}"),
            Some("claude"),
            Some("working"),
            Some("blocked"),
            Some(msg),
            Duration::from_secs(5),
            false,
            true,
            Instant::now(),
        );
        assert!(r.is_some());
    }

    #[test]
    fn list_newest_first() {
        let mut s = test_state();
        raise(&mut s, 3, "perm");
        raise(&mut s, 4, "ask");
        let out = run(&mut s, &test_ctx(), NotifyCmd::List).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("2 ") && lines[0].ends_with("claude blocked ask"));
        assert!(lines[1].ends_with("claude blocked perm"));
    }

    #[test]
    fn dismiss_then_list_empty() {
        let mut s = test_state();
        raise(&mut s, 3, "perm");
        run(&mut s, &test_ctx(), NotifyCmd::Dismiss(1)).unwrap();
        assert_eq!(run(&mut s, &test_ctx(), NotifyCmd::List).unwrap(), "");
        assert!(run(&mut s, &test_ctx(), NotifyCmd::Dismiss(1)).is_err());
    }

    #[test]
    fn mute_then_unmute() {
        let mut s = test_state();
        let ctx = test_ctx();
        run(
            &mut s,
            &ctx,
            NotifyCmd::Mute(Some(Duration::from_secs(600))),
        )
        .unwrap();
        assert!(s.notify.muted(Instant::now()));
        run(&mut s, &ctx, NotifyCmd::Unmute).unwrap();
        assert!(!s.notify.muted(Instant::now()));
        run(&mut s, &ctx, NotifyCmd::Mute(None)).unwrap();
        assert!(s.notify.muted(Instant::now()));
    }
}
