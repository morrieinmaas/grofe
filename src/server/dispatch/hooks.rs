//! Event hooks and the server environment (TC.5): `set-hook`,
//! `show-hooks`, `set-environment`, `show-environment`. The hooks
//! themselves run from `server::hooks`, which also keeps the environment.

use crate::cmd::{ParseError, Resolve, SetEnvironment, SetHook, ShowEnvironment, ShowHooks};
use crate::protocol::ClientCtx;
use crate::server::{State, hooks};

fn too_few(verb: &str) -> String {
    ParseError::TooFewArguments(verb.to_string(), 1).to_string()
}

/// `set-hook -g <event> <command>`: binds a command string to one of
/// grofe's events (`server::hooks::EVENTS`), replacing what was bound;
/// `-gu <event>` removes it.
// ponytail: global hooks only; tmux's session, window and pane hooks
// (`set-hook` without `-g`, `-t`, `-w`, `-p`) and arrays (`-a`, `[N]`) are
// not kept.
pub fn set_hook(state: &mut State, ctx: &ClientCtx, c: SetHook) -> Result<String, String> {
    let name = c.hook.ok_or_else(|| too_few("set-hook"))?;
    let event = hooks::event(&name).ok_or_else(|| format!("unknown hook: {name}"))?;
    if !c.global {
        return Err("only global hooks are supported (set-hook -g)".to_string());
    }
    if c.unset {
        hooks::unbind(state, event);
        return Ok(String::new());
    }
    let command = c.command.ok_or_else(|| "empty value".to_string())?;
    hooks::bind(state, ctx, event, &command)?;
    Ok(String::new())
}

/// `show-hooks -g [event]`: `<event> <command>` for each bound hook.
/// Without `-g` it shows the session's hooks, of which there are none.
pub fn show_hooks(state: &mut State, ctx: &ClientCtx, c: ShowHooks) -> Result<String, String> {
    if !c.global {
        return Ok(String::new());
    }
    hooks::show(state, c.hook.as_deref(), ctx.dialect, c.window)
}

/// `set-environment [-g] [-h] [-t session] name value`, or `-u name`: the
/// global environment with `-g`, else the target session's. Texts are
/// tmux 3.7b's (recorded).
pub fn set_environment(
    state: &mut State,
    ctx: &ClientCtx,
    c: SetEnvironment,
) -> Result<String, String> {
    let name = c.name.ok_or_else(|| too_few("set-environment"))?;
    if name.contains('=') {
        return Err("variable name contains =".to_string());
    }
    let value = match (c.unset, c.value) {
        (true, Some(_)) => return Err("can't specify a value with -u".to_string()),
        (true, None) => None,
        (false, None) => return Err("no value specified".to_string()),
        (false, Some(value)) => Some(value),
    };
    if c.global {
        hooks::set_global(state, &name, value, c.hidden);
    } else {
        let session = state.model.resolve_session(&c.target, ctx)?;
        hooks::set_session(state, session, &name, value, c.hidden);
    }
    Ok(String::new())
}

/// `show-environment [-g] [-h] [-t session] [name]`: `NAME=value` lines,
/// by name, of the visible variables (the hidden ones with `-h`), and
/// `-NAME` for a session variable marked removed. A named
/// variable that does not exist is an error; one of the other kind shows
/// nothing, as in tmux 3.7b.
pub fn show_environment(
    state: &mut State,
    ctx: &ClientCtx,
    c: ShowEnvironment,
) -> Result<String, String> {
    let session = if c.global {
        None
    } else {
        Some(state.model.resolve_session(&c.target, ctx)?)
    };
    let vars = hooks::variables(state, session);
    let line = |v: &hooks::Variable| match &v.value {
        Some(value) => format!("{}={value}\n", v.name),
        None => format!("-{}\n", v.name),
    };
    if let Some(name) = &c.name {
        let v = vars
            .iter()
            .find(|v| v.name == *name)
            .ok_or_else(|| format!("unknown variable: {name}"))?;
        return Ok(if v.hidden == c.hidden {
            line(v)
        } else {
            String::new()
        });
    }
    Ok(vars
        .iter()
        .filter(|v| v.hidden == c.hidden)
        .map(line)
        .collect())
}

#[cfg(test)]
mod tests {
    use crate::server::{State, dispatch, hooks, test_ctx};

    fn test_state() -> State {
        let mut state = crate::server::test_state();
        hooks::clear_environment(&mut state);
        state
    }

    fn run(state: &mut State, words: &[&str]) -> Result<String, String> {
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        dispatch::run(state, &test_ctx(), &argv)
    }

    #[test]
    fn bare_verbs_need_their_argument() {
        let mut state = test_state();
        assert_eq!(
            run(&mut state, &["set-hook"]),
            Err("command set-hook: too few arguments (need at least 1)".to_string())
        );
        assert_eq!(
            run(&mut state, &["set-environment"]),
            Err("command set-environment: too few arguments (need at least 1)".to_string())
        );
        assert_eq!(run(&mut state, &["show-hooks"]), Ok(String::new()));
        assert_eq!(
            run(&mut state, &["show-environment", "-g"]),
            Ok(String::new())
        );
    }

    #[test]
    fn set_hook_errors() {
        let mut state = test_state();
        assert_eq!(
            run(&mut state, &["set-hook", "-g", "after-select-pane", "x"]),
            Err("unknown hook: after-select-pane".to_string())
        );
        assert_eq!(
            run(&mut state, &["set-hook", "pane-focused", "list-sessions"]),
            Err("only global hooks are supported (set-hook -g)".to_string())
        );
        assert_eq!(
            run(&mut state, &["set-hook", "-g", "pane-focused"]),
            Err("empty value".to_string())
        );
        // Unsetting what is not set is fine, as in tmux.
        assert_eq!(
            run(&mut state, &["set-hook", "-gu", "pane-focused"]),
            Ok(String::new())
        );
    }

    #[test]
    fn show_hooks_lists_what_is_bound() {
        let mut state = test_state();
        run(
            &mut state,
            &["set-hook", "-g", "pane-focused", "list-sessions"],
        )
        .unwrap();
        run(
            &mut state,
            &["set-hook", "-g", "client-resized", "list-clients"],
        )
        .unwrap();
        assert_eq!(
            run(&mut state, &["show-hooks", "-g"]),
            Ok("client-resized list-clients\npane-focused list-sessions\n".to_string())
        );
        assert_eq!(
            run(&mut state, &["show-hooks", "-g", "pane-died"]),
            Ok("pane-died\n".to_string())
        );
        assert_eq!(
            run(&mut state, &["show-hooks", "-g", "nope"]),
            Err("unknown hook: nope".to_string())
        );
        run(&mut state, &["set-hook", "-gu", "pane-focused"]).unwrap();
        assert_eq!(
            run(&mut state, &["show-hooks", "-g"]),
            Ok("client-resized list-clients\n".to_string())
        );
    }

    #[test]
    fn set_environment_errors_are_tmuxs() {
        let mut state = test_state();
        for (words, err) in [
            (
                &["set-environment", "-g", "A=B", "c"][..],
                "variable name contains =",
            ),
            (
                &["set-environment", "-gu", "A", "b"],
                "can't specify a value with -u",
            ),
            (&["set-environment", "-g", "A"], "no value specified"),
            (
                &["show-environment", "-g", "NOPE"],
                "unknown variable: NOPE",
            ),
        ] {
            assert_eq!(run(&mut state, words), Err(err.to_string()), "{words:?}");
        }
        // Unsetting what is not set is fine.
        assert_eq!(
            run(&mut state, &["set-environment", "-gu", "NOPE"]),
            Ok(String::new())
        );
    }

    #[test]
    fn global_variables_show_by_kind() {
        let mut state = test_state();
        run(&mut state, &["set-environment", "-g", "B", "2"]).unwrap();
        run(&mut state, &["set-environment", "-g", "A", "1"]).unwrap();
        run(&mut state, &["set-environment", "-gh", "S", "x"]).unwrap();
        assert_eq!(
            run(&mut state, &["show-environment", "-g"]),
            Ok("A=1\nB=2\n".to_string())
        );
        assert_eq!(
            run(&mut state, &["show-environment", "-gh"]),
            Ok("S=x\n".to_string())
        );
        assert_eq!(
            run(&mut state, &["show-environment", "-g", "S"]),
            Ok(String::new())
        );
        assert_eq!(
            run(&mut state, &["show-environment", "-gh", "S"]),
            Ok("S=x\n".to_string())
        );
        // Setting without -h makes it visible again.
        run(&mut state, &["set-environment", "-g", "S", "y"]).unwrap();
        assert_eq!(
            run(&mut state, &["show-environment", "-gh"]),
            Ok(String::new())
        );
        run(&mut state, &["set-environment", "-gu", "A"]).unwrap();
        assert_eq!(
            run(&mut state, &["show-environment", "-g"]),
            Ok("B=2\nS=y\n".to_string())
        );
    }

    #[tokio::test]
    async fn session_variables_live_in_the_session() {
        let mut state = test_state();
        run(&mut state, &["new-session", "-d", "-s", "work"]).unwrap();
        run(&mut state, &["set-environment", "-t", "work", "V", "1"]).unwrap();
        run(&mut state, &["set-environment", "-t", "work", "V", "2"]).unwrap();
        run(
            &mut state,
            &["set-environment", "-h", "-t", "work", "H", "3"],
        )
        .unwrap();
        let session = &state.model.sessions.values().next().unwrap().env;
        assert_eq!(
            session.iter().filter(|(k, _)| k == "V").collect::<Vec<_>>(),
            vec![&("V".to_string(), "2".to_string())]
        );
        assert!(!session.iter().any(|(k, _)| k == "H"));
        assert_eq!(
            run(&mut state, &["show-environment", "-t", "work", "V"]),
            Ok("V=2\n".to_string())
        );
        assert_eq!(
            run(&mut state, &["show-environment", "-h", "-t", "work"]),
            Ok("H=3\n".to_string())
        );
        run(&mut state, &["set-environment", "-u", "-t", "work", "V"]).unwrap();
        assert_eq!(
            run(&mut state, &["show-environment", "-t", "work", "V"]),
            Err("unknown variable: V".to_string())
        );
        // The global environment is separate.
        assert_eq!(
            run(&mut state, &["show-environment", "-g"]),
            Ok(String::new())
        );
    }
}
