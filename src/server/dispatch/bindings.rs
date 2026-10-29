//! grofe's conditional (TC.10, spec §4.5 bindings row): `if-shell`, the
//! native form tmux's `if-shell` and `if -F` translate onto.
//!
//! - The format form (`-F`) expands its format on the actor, against the
//!   `-t` pane (the current pane by default, which in a mouse key's
//!   commands is the pane under the pointer), and is true when the result
//!   is neither empty nor `0`. The chosen branch runs at once.
//! - The shell form expands its command the same way and runs it off the
//!   actor, through `run-shell`'s spawn (`prompt::shell`); it is true when
//!   the shell exits 0. The branch runs on the actor once the shell is
//!   done (a [`Job`]). Without `-b` the request's reply waits for the
//!   branch, as tmux's CLI waits, and from a binding that client's later
//!   input waits for it too, in order, as tmux's per-client queue does;
//!   with `-b` nothing waits. No other client stalls behind the shell.
//!   A failing branch's error shows on a binding's client's status line.
//!
//! The branches are stored command strings, split when they run and run
//! in the request's dialect with the request's context, as tmux runs them
//! with the `if-shell` item's own state; `-t` only picks what the format
//! expands against. In a mouse key's commands a branch keeps the mouse
//! event, so `mouse_status_range` and a bare `=` target (M36-R3) work in
//! it, a shell-form branch too.

use crate::cmd::{
    self, Condition, FormatVars, If, Inner, LoopItem, LoopKind, LoopSort, NameKind, Resolve, Target,
};
use crate::protocol::{ClientCtx, Failure};
use crate::server::clients::{self, ClientVars};
use crate::server::dispatch::options::{FormatCtx, ServerVars};
use crate::server::mouse::{self, MouseState};
use crate::server::{Job, State, hooks, pane, prompt};
use crate::types::PaneId;

use super::{parse_split, run_commands};

/// `if-shell`.
pub fn if_shell(state: &mut State, ctx: &ClientCtx, c: If) -> Result<String, String> {
    let (Some(condition), Some(then)) = (c.condition, c.then) else {
        return Err("command if-shell: too few arguments (need at least 2)".to_string());
    };
    let pane = match (&c.target, state.model.resolve_pane(&c.target, ctx)) {
        (_, Ok(pane)) => Some(pane),
        // With no pane at all (no session yet), formats expand to nothing.
        (Target::Current, Err(_)) => None,
        (_, Err(e)) => return Err(e),
    };
    match condition {
        Condition::Format(format) => {
            let value = expand(state, ctx, pane, &format);
            let branch = if value.is_empty() || value == "0" {
                c.otherwise
            } else {
                Some(then)
            };
            branch.map_or(Ok(String::new()), |b| run_branch(state, ctx, &b))
        }
        Condition::Shell(command) => {
            let script = expand(state, ctx, pane, &command);
            shell_form(state, ctx, script, then, c.otherwise, c.background);
            Ok(String::new())
        }
    }
}

/// Runs `script` off the actor, then the branch it picks on the actor.
fn shell_form(
    state: &mut State,
    ctx: &ClientCtx,
    script: String,
    then: String,
    otherwise: Option<String>,
    background: bool,
) {
    // Where and with what `run-shell` runs its shell.
    let client_session = ctx
        .client
        .and_then(|id| state.clients.get(&id))
        .map(|client| client.session);
    let cwd = pane::usable_cwd(
        client_session
            .and_then(|s| state.model.sessions.get(&s))
            .map_or_else(|| ctx.cwd.clone(), |s| s.cwd.clone()),
    );
    let session =
        client_session.or_else(|| state.model.resolve_session(&Target::Current, ctx).ok());
    let env = hooks::shell_env(state, session);
    let reply = (!background).then(|| super::defer_reply(state));
    let socket = state.socket.clone();
    let jobs = state.jobs.clone();
    let shell_jobs = state.shell_jobs.clone();
    let event = state.mouse.current().cloned();
    // A binding's client types on behind the condition, as tmux's
    // per-client queue waits: its input is held until the branch has run.
    let held = ctx
        .client
        .filter(|id| !background && state.clients.contains_key(id) && !hooks::in_hook(state));
    if let Some(id) = held {
        clients::hold_input(state, id);
    }
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let yes = prompt::shell(&script, cwd, env, socket, false, &shell_jobs)
            .await
            .is_ok();
        let job: Job = Box::new(move |state: &mut State| {
            let branch = if yes { Some(then) } else { otherwise };
            let result = MouseState::with_current(state, event, |state| {
                branch.map_or(Ok(String::new()), |b| run_branch(state, &ctx, &b))
            });
            let inner = state.deferred_reply.take();
            state.events.flush(&state.model);
            clients::changed(state);
            // As tmux shows a binding's error: on the client's status line.
            if let (Err(e), Some(id)) = (&result, ctx.client) {
                clients::show_message(state, id, e.trim_end());
            }
            if let Some(id) = held {
                clients::release_input(state, id);
            }
            match (reply, inner) {
                // A branch that answers later (`run-shell`) answers for us.
                (Some(reply), Some(inner)) if result.is_ok() => {
                    tokio::spawn(async move {
                        let answer = inner.await.unwrap_or_else(|_| Ok(String::new()));
                        let _ = reply.send(answer);
                    });
                }
                (Some(reply), _) => {
                    let _ = reply.send(result.map_err(Failure::from));
                }
                (None, _) => {}
            }
        });
        let _ = jobs.send(job).await;
    });
}

/// How deep `if-shell` branches (and the commands that run them) may nest.
const BRANCH_DEPTH_LIMIT: u32 = 100;

/// Runs a branch's command string in `ctx`, parsed whole first as tmux
/// parses it, then each command in turn as one sequence, so a hook's
/// shell or a `wait-for` holds the rest of it (and of the request); the
/// first error stops the rest.
///
/// Branches nest at most [`BRANCH_DEPTH_LIMIT`] deep. tmux 3.7b has no
/// limit (recorded: `if -F` nested 1990 deep runs, in 16 s, since each
/// level re-parses the rest); grofe runs a branch on the actor's stack and
/// re-splits its text, so it stops there with an error instead.
fn run_branch(state: &mut State, ctx: &ClientCtx, text: &str) -> Result<String, String> {
    if hooks::depth(state) >= BRANCH_DEPTH_LIMIT {
        return Err(format!(
            "if-shell: nested more than {BRANCH_DEPTH_LIMIT} deep"
        ));
    }
    let mut cmds = Vec::new();
    for argv in cmd::split_command_string(text).map_err(|e| e.to_string())? {
        let argv = mouse::resolve_targets(state, &argv);
        cmds.extend(parse_split(state, ctx, &argv)?);
    }
    run_commands(state, ctx, cmds)
}

/// `text` expanded against `pane`, with the client's and the server's
/// variables, a running hook's, and the mouse's over them.
pub(super) fn expand(state: &State, ctx: &ClientCtx, pane: Option<PaneId>, text: &str) -> String {
    let model = &state.model;
    let with = |inner: &dyn FormatVars| {
        let vars = MouseVars {
            state,
            inner: Inner::Ref(inner),
        };
        match hooks::current_formats(state) {
            Some(formats) => cmd::expand(
                text,
                &hooks::HookVars {
                    formats,
                    inner: Inner::Ref(&vars),
                },
            ),
            None => cmd::expand(text, &vars),
        }
    };
    let Some(pane) = pane else {
        return with(&NoVars);
    };
    match clients::current_client(state, ctx).and_then(|id| state.clients.get(&id)) {
        Some(client) => {
            let vars = ClientVars::new(model, client, Some(FormatCtx::new(model, pane)));
            with(&ServerVars::fresh(state, pane, &vars))
        }
        None => {
            let vars = FormatCtx::new(model, pane);
            with(&ServerVars::fresh(state, pane, &vars))
        }
    }
}

/// The mouse's variables ([`mouse::format_var`]) over `inner`.
struct MouseVars<'a> {
    state: &'a State,
    inner: Inner<'a>,
}

impl FormatVars for MouseVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        mouse::format_var(self.state, name).or_else(|| self.inner.vars().get(name))
    }

    fn loop_items(&self, kind: LoopKind, sort: LoopSort) -> Option<Vec<LoopItem<'_>>> {
        let items = self.inner.vars().loop_items(kind, sort)?;
        Some(
            items
                .into_iter()
                .map(|it| LoopItem {
                    vars: Box::new(MouseVars {
                        state: self.state,
                        inner: Inner::Own(it.vars),
                    }),
                    ..it
                })
                .collect(),
        )
    }

    fn name_exists(&self, kind: NameKind, name: &str) -> Option<bool> {
        self.inner.vars().name_exists(kind, name)
    }

    fn pane_search(&self, term: &str, regex: bool, ignore: bool) -> Option<u32> {
        self.inner.vars().pane_search(term, regex, ignore)
    }

    fn reexpand(&self, text: &str) -> String {
        self.inner.vars().reexpand(text)
    }
}

struct NoVars;

impl FormatVars for NoVars {
    fn get(&self, _: &str) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::server::{dispatch, test_ctx, test_state};

    fn run(state: &mut crate::server::State, argv: &[&str]) -> Result<String, String> {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        dispatch::run(state, &test_ctx(), &argv)
    }

    /// tmux 3.7b's words for a bare `if-shell`.
    #[test]
    fn a_bare_if_shell_needs_two_arguments() {
        let mut state = test_state();
        assert_eq!(
            run(&mut state, &["if-shell"]),
            Err("command if-shell: too few arguments (need at least 2)".to_string())
        );
    }

    /// The format form picks its branch by the expanded value: empty and
    /// `0` are false, anything else true; a missing else branch does
    /// nothing. With no session yet, variables expand to nothing.
    #[test]
    fn the_format_form_picks_a_branch_by_its_value() {
        let mut state = test_state();
        let cases = [
            ("1", "yes"),
            ("0", "no"),
            ("", "no"),
            ("00", "yes"),
            ("#{session_name}", "no"),
            ("#{==:a,a}", "yes"),
            ("#{||:0,#{m:*x*,axb}}", "yes"),
        ];
        for (format, want) in cases {
            run(
                &mut state,
                &["if-shell", "-F", format, "set -g @r yes", "set -g @r no"],
            )
            .unwrap_or_else(|e| panic!("{format}: {e}"));
            assert_eq!(
                run(&mut state, &["show-options", "-gv", "@r"]),
                Ok(format!("{want}\n")),
                "{format}"
            );
        }
        run(&mut state, &["set", "-g", "@r", "none"]).unwrap();
        run(&mut state, &["if-shell", "-F", "0", "set -g @r yes"]).unwrap();
        assert_eq!(
            run(&mut state, &["show-options", "-gv", "@r"]),
            Ok("none\n".to_string())
        );
        // A branch is a command sequence, and its first error stops it.
        run(
            &mut state,
            &["if-shell", "-F", "1", "set -g @a 1 ; set -g @b 2"],
        )
        .unwrap();
        assert_eq!(
            run(&mut state, &["show-options", "-gv", "@b"]),
            Ok("2\n".to_string())
        );
        assert_eq!(
            run(
                &mut state,
                &["if-shell", "-F", "1", "splat-window ; set -g @c 3"]
            ),
            Err("unknown command: splat-window".to_string())
        );
        let c = run(&mut state, &["show-options", "-gv", "@c"]);
        assert!(!matches!(&c, Ok(v) if !v.is_empty()), "{c:?}");
    }
}
