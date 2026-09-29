//! Client and session verbs (TC.9, M36-R1): `refresh-client`,
//! `show-messages`, the choosers (their UI in `server::tree`),
//! `suspend-client`, `find-window` and `wait-for`.

use crate::cmd::{
    ChooseClient, ChooseTree, FindWindow, ParseError, RefreshClient, ShowMessages, SuspendClient,
    WaitFor,
};
use crate::protocol::ClientCtx;
use crate::server::{State, clients, dispatch, tree};

use super::not_implemented;

/// `refresh-client`: the client (`-t`, else the current one) redraws, only
/// its status line with `-S`.
pub fn refresh_client(
    state: &mut State,
    ctx: &ClientCtx,
    c: RefreshClient,
) -> Result<String, String> {
    let id = clients::target_client(state, ctx, c.client.as_deref())?;
    clients::refresh(state, id, c.status);
    Ok(String::new())
}

/// `show-messages`: the message log, newest first, each line as tmux's
/// `#{t/p:message_time}: #{message_text}`.
// ponytail: `-J` (the job list) and `-T` (the terminal descriptions) print
// nothing; grofe keeps neither table.
pub fn show_messages(
    state: &mut State,
    _ctx: &ClientCtx,
    c: ShowMessages,
) -> Result<String, String> {
    if c.jobs || c.terminals {
        return Ok(String::new());
    }
    Ok(clients::messages(state)
        .rev()
        .map(|(time, text)| format!("{}: {text}\n", clients::pretty_time(*time)))
        .collect())
}

/// `choose-client`: the client chooser over the current pane.
pub fn choose_client(
    state: &mut State,
    ctx: &ClientCtx,
    c: ChooseClient,
) -> Result<String, String> {
    tree::open_clients(state, ctx, c.zoom)
}

/// `suspend-client`.
// ponytail: stays unimplemented. Suspending needs the attach client to
// restore its terminal and stop itself (tmux's MSG_SUSPEND), which is a new
// attach frame in `protocol.rs` and its handling in `client.rs`, outside
// TC.9's files (see the TC.9 report).
pub fn suspend_client(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: SuspendClient,
) -> Result<String, String> {
    not_implemented("suspend-client")
}

/// `find-window`: the tree chooser, showing only the windows whose name,
/// a pane's title or a pane's visible text holds the pattern.
pub fn find_window(state: &mut State, ctx: &ClientCtx, c: FindWindow) -> Result<String, String> {
    let Some(pattern) = c.pattern else {
        return Err(ParseError::TooFewArguments("find-window".to_string(), 1).to_string());
    };
    tree::open_tree(
        state,
        ctx,
        &c.target,
        tree::Opts {
            zoom: c.zoom,
            sessions: false,
            windows: false,
            find: Some(pattern),
        },
    )
}

/// `choose-tree`: sessions, windows and panes as a tree over the target
/// pane; Enter switches the client to the chosen one.
pub fn choose_tree(state: &mut State, ctx: &ClientCtx, c: ChooseTree) -> Result<String, String> {
    tree::open_tree(
        state,
        ctx,
        &crate::cmd::Target::Current,
        tree::Opts {
            zoom: c.zoom,
            // ponytail: `-s` (tmux's stock `prefix s`) is not parsed
            // natively yet; `cmd::ChooseTree` has no field for it.
            sessions: false,
            windows: c.windows,
            find: None,
        },
    )
}

/// `wait-for`: blocks the caller until another call signals the channel;
/// with `-S`, signals it.
// ponytail: tmux's `-L`/`-U` (channel locks) are not parsed natively; the
// corpus sends only the wait and the signal (fzf's `--tmux`).
pub fn wait_for(state: &mut State, _ctx: &ClientCtx, c: WaitFor) -> Result<String, String> {
    let Some(channel) = c.channel else {
        return Err(ParseError::TooFewArguments("wait-for".to_string(), 1).to_string());
    };
    if c.signal {
        clients::signal_channel(state, &channel);
    } else {
        clients::wait_channel(state, &channel, dispatch::defer_reply);
    }
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use crate::server::{dispatch, test_ctx, test_state};

    fn run(state: &mut crate::server::State, words: &[&str]) -> Result<String, String> {
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        dispatch::run(state, &test_ctx(), &argv)
    }

    #[test]
    fn suspend_client_is_not_implemented_yet() {
        let mut state = test_state();
        assert_eq!(
            run(&mut state, &["suspend-client"]),
            Err("not implemented: suspend-client".to_string())
        );
    }

    #[test]
    fn bare_verbs_need_their_argument() {
        let mut state = test_state();
        assert_eq!(
            run(&mut state, &["wait-for"]),
            Err("command wait-for: too few arguments (need at least 1)".to_string())
        );
        assert_eq!(
            run(&mut state, &["find-window"]),
            Err("command find-window: too few arguments (need at least 1)".to_string())
        );
    }

    /// A signal with nobody waiting wakes the channel for the next wait
    /// only, as tmux's `wait_channel.woken`.
    #[test]
    fn a_signal_before_the_wait_is_kept_for_one_wait() {
        let mut state = test_state();
        assert_eq!(run(&mut state, &["wait-for", "-S", "c"]), Ok(String::new()));
        assert_eq!(run(&mut state, &["wait-for", "c"]), Ok(String::new()));
        assert!(
            state.deferred_reply.is_none(),
            "the woken wait returns at once"
        );
        assert_eq!(run(&mut state, &["wait-for", "c"]), Ok(String::new()));
        let mut rx = state.deferred_reply.take().expect("the second wait waits");
        assert!(rx.try_recv().is_err());
        assert_eq!(run(&mut state, &["wait-for", "-S", "c"]), Ok(String::new()));
        assert_eq!(rx.try_recv(), Ok(Ok(String::new())));
        // Released waiters leave the channel unwoken.
        assert_eq!(run(&mut state, &["wait-for", "c"]), Ok(String::new()));
        assert!(state.deferred_reply.is_some());
    }

    #[test]
    fn show_messages_with_nothing_logged_prints_nothing() {
        let mut state = test_state();
        assert_eq!(run(&mut state, &["show-messages"]), Ok(String::new()));
        assert_eq!(
            run(&mut state, &["show-messages", "-JT"]),
            Ok(String::new())
        );
    }

    #[test]
    fn client_verbs_without_a_client_fail() {
        let mut state = test_state();
        assert_eq!(
            run(&mut state, &["refresh-client", "-S"]),
            Err("no current client".to_string())
        );
        assert_eq!(
            run(&mut state, &["refresh-client", "-t", "client-9"]),
            Err("can't find client: client-9".to_string())
        );
    }
}
