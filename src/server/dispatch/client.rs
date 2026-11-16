//! Client and session verbs (TC.9, M36-R1): `refresh-client`,
//! `show-messages`, the choosers (their UI in `server::tree`),
//! `suspend-client`, `find-window` and `wait-for`.

use tokio::sync::oneshot;

use crate::cmd::{
    ChooseClient, ChooseTree, Command, CustomizeMode, FindWindow, ParseError, RefreshClient,
    ShowMessages, SuspendClient, WaitFor,
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
    // The terminal's reply to a clipboard query would come back as keys
    // (the input decoder has no OSC 52 reply), so none is asked for.
    if c.clipboard {
        return Err(
            "refresh-client -l: reading the terminal's clipboard is not supported".to_string(),
        );
    }
    for list in &c.flags {
        clients::set_flags(state, id, list);
    }
    if c.report.is_some() {
        crate::compat::note_miss("refresh-client -r: cosmetic, ignored");
    }
    if c.control {
        return Err("not a control client".to_string());
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
        .map(|(time, text)| {
            let secs = time
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
            format!("{}: {text}\n", crate::cmd::format::time::pretty_time(secs))
        })
        .collect())
}

/// `choose-client`: the client chooser over the current pane.
pub fn choose_client(
    state: &mut State,
    ctx: &ClientCtx,
    c: ChooseClient,
) -> Result<String, String> {
    tree::open_clients(state, ctx, c.zoom, &c.opts, ctx.dialect)
}

/// `customize-mode`: tmux's option and key browser over the target pane.
pub fn customize_mode(
    state: &mut State,
    ctx: &ClientCtx,
    c: CustomizeMode,
) -> Result<String, String> {
    tree::open_customize(state, ctx, c.zoom, &c.opts, ctx.dialect)
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
            find: Some(tree::Find {
                pattern,
                content: c.content,
                name: c.name,
                title: c.title,
                regex: c.regex,
                ignore: c.ignore,
            }),
            ..tree::Opts::new(ctx.dialect)
        },
    )
}

/// `choose-tree`: sessions, windows and panes as a tree over the target
/// pane; Enter switches the client to the chosen one.
pub fn choose_tree(state: &mut State, ctx: &ClientCtx, c: ChooseTree) -> Result<String, String> {
    let target = c.opts.target.clone();
    tree::open_tree(
        state,
        ctx,
        &target,
        tree::Opts {
            zoom: c.zoom,
            sessions: c.sessions,
            windows: c.windows,
            chooser: c.opts,
            ..tree::Opts::new(ctx.dialect)
        },
    )
}

/// `wait-for`: blocks the caller until another call signals the channel;
/// with `-S`, signals it.
pub fn wait_for(state: &mut State, _ctx: &ClientCtx, c: WaitFor) -> Result<String, String> {
    let Some(channel) = c.channel else {
        return Err(ParseError::TooFewArguments("wait-for".to_string(), 1).to_string());
    };
    if c.signal {
        signal_channel(state, &channel);
    } else if c.lock {
        lock_channel(state, &channel, dispatch::defer_reply);
    } else if c.unlock {
        unlock_channel(state, &channel)?;
    } else {
        wait_channel(state, &channel, dispatch::defer_reply);
    }
    Ok(String::new())
}

/// The rest of a request held behind a wait: the rest of each command
/// sequence it was in when it stopped, innermost first (a command inside
/// `if -F` stops that body and the request's own commands after it), and
/// what it waits for (a blocked `wait-for`; `None`: a hook's shell).
#[derive(Default)]
pub(crate) struct Held {
    pub frames: Vec<HeldFrame>,
    pub wait: Option<oneshot::Receiver<Result<String, Failure>>>,
    /// The wait is a `run-shell`'s: its status is the request's so far.
    pub shell: bool,
    /// Errors the request already had (a sourced file's earlier lines):
    /// it fails with them at the end, after everything it printed.
    pub errors: Vec<String>,
    /// How many of `errors` a later `run-shell` that succeeded made
    /// harmless to the exit status: in the tmux dialect, as tmux 3.7b, the
    /// last `run-shell`'s status is the request's (recorded: an error on a
    /// line before it prints and exits 0). A native request's, none.
    pub cleared: usize,
}

/// One held command sequence: its context, its commands, and whether an
/// error in it lets the frames after it run (a sourced file's line, as
/// tmux runs a file's next line), else stops them, and the source-file
/// depth it was held at, which it runs under again.
pub(crate) struct HeldFrame {
    pub ctx: ClientCtx,
    pub cmds: Vec<Command>,
    pub keep_going: bool,
    pub depth: u32,
    /// `source-file -v` was on where it was held.
    pub verbose: bool,
}

type Answer = oneshot::Sender<Result<String, Failure>>;

/// Holds `held` until its wait answers, then runs its rest on the actor;
/// the request's answer (`tx`, else a new deferred reply) is everything it
/// printed, what the wait answered included. A CLI caller that hangs up
/// first drops it, as tmux frees a dead client's queue; a binding's rest
/// runs whatever happens to its (dropped) answer.
pub(crate) fn hold_rest(
    state: &mut State,
    ctx: &ClientCtx,
    held: Held,
    out: String,
    failed: Option<Failure>,
    tx: Option<Answer>,
) {
    let wait = held
        .wait
        .or_else(|| hooks::take_request_wait(state))
        .unwrap_or_else(|| {
            let (ready, wait) = oneshot::channel();
            let _ = ready.send(Ok(String::new()));
            wait
        });
    let (frames, shell, errors, cleared) = (held.frames, held.shell, held.errors, held.cleared);
    // A native request keeps its failure, even on a tmux-identity server.
    let tmux = ctx.dialect == crate::protocol::Dialect::Tmux;
    let mut tx = tx.unwrap_or_else(|| dispatch::defer_reply(state));
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
        // A failed command (`run-shell` exiting non-zero) does not stop
        // the rest, as in tmux 3.7b (recorded); the request fails at the end.
        let cleared = match woken {
            Ok(Ok(_)) if shell && tmux => errors.len(),
            _ => cleared,
        };
        let (out, failed) = match woken {
            // The last `run-shell`'s status is the request's (tmux 3.7b).
            Ok(Ok(text)) => (out + &text, failed.filter(|_| !shell)),
            Ok(Err(failure)) => (out + &failure.out, Some(failure)),
            // The server is going away; the dropped answer says so.
            Err(_) => return,
        };
        let job: Job = Box::new(move |state: &mut State| {
            resume(state, &ctx, frames, (errors, cleared), out, failed, tx);
        });
        let _ = jobs.send(job).await;
    });
}

/// Runs a held request's rest, one sequence after another, and answers it
/// once the rest's own hooks are done (as `finish` answers a request), or
/// holds what is left again behind the next wait.
fn resume(
    state: &mut State,
    ctx: &ClientCtx,
    frames: Vec<HeldFrame>,
    (mut errors, cleared): (Vec<String>, usize),
    out: String,
    failed: Option<Failure>,
    tx: Answer,
) {
    let mut frames = std::collections::VecDeque::from(frames);
    let mut more = String::new();
    let mut result = Ok(());
    // The sequences run as the request's own nested commands: a wait in
    // one comes back here rather than holding on its own.
    hooks::enter(state);
    while let Some(frame) = frames.pop_front() {
        let ran =
            dispatch::options::with_source_depth(state, frame.depth, frame.verbose, |state| {
                dispatch::run_commands(state, &frame.ctx, frame.cmds)
            });
        match ran {
            Ok(text) => more.push_str(&text),
            Err(e) if frame.keep_going => {
                more.push_str(&std::mem::take(&mut state.failed_out));
                errors.push(e);
            }
            Err(e) => {
                // What the failing sequence printed first still prints.
                more.push_str(&std::mem::take(&mut state.failed_out));
                result = Err(e);
                break;
            }
        }
        let hook_waits =
            !frames.is_empty() && state.deferred_reply.is_none() && hooks::request_waiting(state);
        if state.held.is_some() || hook_waits {
            state
                .held
                .get_or_insert_default()
                .frames
                .extend(frames.drain(..));
            break;
        }
    }
    hooks::leave(state);
    let held = state.held.take().filter(|_| result.is_ok());
    if let Some(mut held) = held {
        errors.append(&mut held.errors);
        held.errors = errors;
        held.cleared = cleared;
        hold_rest(state, ctx, held, out + &more, failed, Some(tx));
        hooks::run_pending(state);
        state.events.flush(&state.model);
        clients::changed(state);
        return;
    }
    hooks::run_pending(state);
    // A failure keeps what was printed before it, as tmux prints it. The
    // errors held so far come first.
    // Errors a later `run-shell` cleared still print, but the request
    // exits 0 (tmux 3.7b).
    let result = match result {
        Ok(()) if errors.is_empty() => Ok(more),
        Ok(()) if errors.len() == cleared => {
            Err((dispatch::options::join_nonempty(&errors), more, Some(0)))
        }
        Ok(()) => Err((dispatch::options::join_nonempty(&errors), more, None)),
        Err(e) => {
            errors.push(e);
            Err((dispatch::options::join_nonempty(&errors), more, None))
        }
    };
    let later = state.deferred_reply.take().filter(|_| result.is_ok());
    let hold = hooks::take_reply_hold(state);
    state.events.flush(&state.model);
    clients::changed(state);
    // Everything printed, as the request's answer; failed if a held
    // command failed.
    let answer = move |out: String| match failed {
        Some(f) => Err(Failure { out, ..f }),
        None => Ok(out),
    };
    if later.is_none() && hold.is_none() {
        let _ = tx.send(match result {
            Ok(more) => answer(out + &more),
            Err((err, more, status)) => Err(Failure {
                err,
                out: out + &more,
                status,
            }),
        });
        return;
    }
    let mut tx = tx;
    tokio::spawn(async move {
        let answer = async {
            // A command that answered last decides the status, as the
            // last `run-shell` does in tmux.
            let answered_last = later.is_some();
            let last = match later {
                Some(later) => later.await.ok()?,
                None => Ok(String::new()),
            };
            let hooked = match hold {
                Some(hold) => hold.await.unwrap_or_default(),
                None => String::new(),
            };
            Some(match (result, last) {
                (Ok(more), Ok(last)) if answered_last => Ok(out + &more + &last + &hooked),
                (Ok(more), Ok(last)) => answer(out + &more + &last + &hooked),
                (Ok(more), Err(f)) => Err(Failure {
                    out: out + &more + &f.out + &hooked,
                    ..f
                }),
                (Err((err, more, status)), _) => Err(Failure {
                    err,
                    out: out + &more,
                    status,
                }),
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

/// One `wait-for` channel (TC.9, M36-R1): signalled with nobody waiting,
/// it stays woken until the next wait, as in tmux.
#[derive(Default)]
pub(crate) struct WaitChannel {
    woken: bool,
    waiters: Vec<oneshot::Sender<Result<String, Failure>>>,
    /// `wait-for -L` holds the channel.
    locked: bool,
    /// The `-L` callers behind the holder, oldest first.
    lockers: Vec<oneshot::Sender<Result<String, Failure>>>,
}

/// `wait-for -S`: every waiter on `channel` is released; with none, the
/// channel stays woken for the next wait.
pub(crate) fn signal_channel(state: &mut State, channel: &str) {
    prune_waits(state);
    let waits = &mut state.registry.waits;
    let wc = waits.entry(channel.to_string()).or_default();
    if wc.waiters.is_empty() {
        wc.woken = true;
        return;
    }
    for tx in wc.waiters.drain(..) {
        let _ = tx.send(Ok(String::new()));
    }
    if !wc.locked {
        waits.remove(channel);
    }
}

/// `wait-for -L`: takes `channel`'s lock, or waits behind whoever holds it
/// until `wait-for -U` hands it over.
pub(crate) fn lock_channel(
    state: &mut State,
    channel: &str,
    reply: impl FnOnce(&mut State) -> oneshot::Sender<Result<String, Failure>>,
) {
    prune_waits(state);
    if !state
        .registry
        .waits
        .get(channel)
        .is_some_and(|wc| wc.locked)
    {
        state
            .registry
            .waits
            .entry(channel.to_string())
            .or_default()
            .locked = true;
        return;
    }
    let tx = reply(state);
    if let Some(wc) = state.registry.waits.get_mut(channel) {
        wc.lockers.push(tx);
    }
}

/// `wait-for -U`: the lock passes to the first `-L` waiting for it, else
/// the channel is free. An unlocked channel is an error.
pub(crate) fn unlock_channel(state: &mut State, channel: &str) -> Result<(), String> {
    let waits = &mut state.registry.waits;
    let Some(wc) = waits.get_mut(channel).filter(|wc| wc.locked) else {
        return Err(format!("channel {channel} not locked"));
    };
    // A locker that hung up is skipped.
    while !wc.lockers.is_empty() {
        if wc.lockers.remove(0).send(Ok(String::new())).is_ok() {
            return Ok(());
        }
    }
    wc.locked = false;
    if !wc.woken && wc.waiters.is_empty() {
        waits.remove(channel);
    }
    Ok(())
}

/// `wait-for`: returns at once when `channel` was woken; otherwise
/// `reply` answers once it is signalled.
pub(crate) fn wait_channel(
    state: &mut State,
    channel: &str,
    reply: impl FnOnce(&mut State) -> oneshot::Sender<Result<String, Failure>>,
) {
    if let Some(wc) = state.registry.waits.get(channel)
        && wc.woken
    {
        // As tmux's `cmd_wait_for_remove`: a locked channel stays, woken.
        if !wc.locked {
            state.registry.waits.remove(channel);
        }
        return;
    }
    prune_waits(state);
    let tx = reply(state);
    let wc = state.registry.waits.entry(channel.to_string()).or_default();
    wc.waiters.push(tx);
}

/// Drops the waits of callers that hung up, and the channels they leave
/// empty and unwoken.
fn prune_waits(state: &mut State) {
    state.registry.waits.retain(|_, wc| {
        wc.waiters.retain(|tx| !tx.is_closed());
        wc.lockers.retain(|tx| !tx.is_closed());
        wc.woken || wc.locked || !wc.waiters.is_empty()
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
