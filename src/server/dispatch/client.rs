//! Client and session verbs (TC.9, M36-R1): `refresh-client`,
//! `show-messages`, the choosers (their UI in `server::tree`),
//! `suspend-client`, `find-window` and `wait-for`.

use tokio::sync::oneshot;

use crate::cmd::{
    ChooseClient, ChooseTree, Command, FindWindow, ParseError, RefreshClient, ShowMessages,
    SuspendClient, WaitFor,
};
use crate::protocol::{ClientCtx, Failure};
use crate::server::{Job, State, clients, dispatch, hooks, tree};

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

/// The rest of a request behind `wait` (a blocked `wait-for`, or a hook
/// waiting on its shell): it runs on the actor once `wait` answers, and
/// the request's answer is everything it printed, what `wait` answered
/// included. A CLI caller that hangs up first drops it, as tmux frees a
/// dead client's queue; a binding's rest runs whatever happens to its
/// (dropped) answer.
pub(crate) fn hold_rest(
    state: &mut State,
    ctx: &ClientCtx,
    rest: Vec<Command>,
    out: String,
    wait: oneshot::Receiver<Result<String, Failure>>,
) {
    let mut tx = dispatch::defer_reply(state);
    let jobs = state.jobs.clone();
    let ctx = ctx.clone();
    let cli = ctx.client.is_none();
    tokio::spawn(async move {
        let woken = if cli {
            tokio::select! {
                woken = wait => woken,
                () = tx.closed() => return,
            }
        } else {
            wait.await
        };
        let out = match woken {
            Ok(Ok(text)) => out + &text,
            Ok(Err(failure)) => {
                let _ = tx.send(Err(failure));
                return;
            }
            // The server is going away; the dropped answer says so.
            Err(_) => return,
        };
        let job: Job = Box::new(move |state: &mut State| resume(state, &ctx, rest, out, tx));
        let _ = jobs.send(job).await;
    });
}

/// Runs a held request's rest and answers it once the rest's own hooks
/// are done (as `finish` answers a request), or holds it again behind the
/// next wait.
fn resume(
    state: &mut State,
    ctx: &ClientCtx,
    rest: Vec<Command>,
    out: String,
    tx: oneshot::Sender<Result<String, Failure>>,
) {
    let result = dispatch::run_commands(state, ctx, rest);
    let later = state.deferred_reply.take().filter(|_| result.is_ok());
    let hold = hooks::take_reply_hold(state);
    state.events.flush();
    clients::changed(state);
    if later.is_none() && hold.is_none() {
        let _ = tx.send(result.map(|more| out + &more).map_err(Failure::from));
        return;
    }
    let mut tx = tx;
    tokio::spawn(async move {
        let answer = async {
            let last = match later {
                Some(later) => later.await.ok()?,
                None => Ok(String::new()),
            };
            let hooked = match hold {
                Some(hold) => hold.await.unwrap_or_default(),
                None => String::new(),
            };
            Some(match (result, last) {
                (Ok(more), Ok(last)) => Ok(out + &more + &last + &hooked),
                (Ok(_), Err(failure)) => Err(failure),
                (Err(e), _) => Err(Failure::from(e)),
            })
        };
        let answer = tokio::select! {
            answer = answer => answer,
            () = tx.closed() => return,
        };
        // `None`: the server is going away; the dropped answer says so.
        if let Some(answer) = answer {
            let _ = tx.send(answer);
        }
    });
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
