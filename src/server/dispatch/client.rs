//! Client and session verbs (TC.9, M36-R1): `refresh-client`,
//! `show-messages`, the choosers (their UI in `server::tree`),
//! `suspend-client`, `find-window` and `wait-for`.

use crate::cmd::{
    ChooseClient, ChooseTree, FindWindow, ParseError, RefreshClient, ShowMessages, SuspendClient,
    WaitFor,
};
use crate::protocol::ClientCtx;
use crate::server::{State, clients, dispatch, tree};

/// `refresh-client`: the client (`-t`, else the current one) redraws, only
/// its status line with `-S`; `-c/-L/-R/-U/-D [n]` (tmux's stock pan keys)
/// move the visible part of a window larger than the client instead.
pub fn refresh_client(
    state: &mut State,
    ctx: &ClientCtx,
    c: RefreshClient,
) -> Result<String, String> {
    let id = clients::target_client(state, ctx, c.client.as_deref())?;
    if let Some(pan) = c.pan {
        let by = match c.adjustment.as_deref() {
            None => 1,
            Some(s) => match s.parse::<i64>() {
                Ok(n) if n < 1 => return Err("adjustment too small".to_string()),
                Ok(n) if n > i64::from(i32::MAX) => return Err("adjustment too large".to_string()),
                Ok(n) => u16::try_from(n).unwrap_or(u16::MAX),
                Err(_) => return Err("adjustment invalid".to_string()),
            },
        };
        clients::pan(state, id, pan, by);
        return Ok(String::new());
    }
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

/// `suspend-client`: the client (`-t`, else the current one) stops itself
/// with SIGTSTP, as tmux 3.7b's; `fg` in its shell brings it back.
pub fn suspend_client(
    state: &mut State,
    ctx: &ClientCtx,
    c: SuspendClient,
) -> Result<String, String> {
    let id = clients::target_client(state, ctx, c.client.as_deref())?;
    clients::suspend(state, id);
    Ok(String::new())
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
            sessions: c.sessions,
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
