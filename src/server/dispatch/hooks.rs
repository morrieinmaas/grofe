//! Event hooks and the server environment (TC.5): `set-hook`,
//! `show-hooks`, `set-environment`, `show-environment`. The hooks
//! themselves run from `server::hooks`, which also keeps the environment.

use crate::cmd::{ParseError, Resolve, SetEnvironment, SetHook, ShowEnvironment, ShowHooks};
use crate::protocol::ClientCtx;
use crate::server::{State, hooks};

fn too_few(verb: &str) -> String {
    ParseError::TooFewArguments(verb.to_string(), 1).to_string()
}

/// A hook name as tmux's `options_match` reads it: the name and an
/// optional `[N]` array index. Anything else after the name is the text
/// tmux calls `ambiguous option`.
fn split_index(name: &str) -> Result<(&str, Option<u32>), String> {
    let Some((base, rest)) = name.split_once('[') else {
        return Ok((name, None));
    };
    rest.strip_suffix(']')
        .and_then(|n| n.parse::<u32>().ok())
        .map(|n| (base, Some(n)))
        .ok_or_else(|| format!("ambiguous option: {name}"))
}

/// `set-hook [-agpRu] [-t target] <event>[N] [command]`: tmux's hooks are
/// array options. `-g` sets the global commands, else the target's own
/// session, window (`-w` is implied for the window hooks) or, with `-p`,
/// pane; a firing hook runs the nearest scope that has any. `-a` adds a
/// command, `[N]` sets one element, `-u` removes a hook or an element, and
/// `-R` runs the hook now.
pub fn set_hook(state: &mut State, ctx: &ClientCtx, c: SetHook) -> Result<String, String> {
    let name = c.hook.clone().ok_or_else(|| too_few("set-hook"))?;
    if c.run {
        // tmux runs whatever the name finds and says nothing of the rest.
        let Some(spec) = hooks::event(&name) else {
            return Ok(String::new());
        };
        let pane = state.model.resolve_pane(&c.target, ctx).ok();
        return Ok(hooks::run_now(state, ctx, spec, pane));
    }
    let (base, index) = split_index(&name)?;
    let spec = hooks::event(base).ok_or_else(|| format!("unknown hook: {name}"))?;
    let table = hooks::table_of(spec, c.pane);
    let text = c.target_text.as_deref();
    let scope = hooks::scope_in(state, ctx, table, c.global, &c.target, text)?;
    if c.unset {
        hooks::unbind(state, scope, spec, index);
        return Ok(String::new());
    }
    let command = c.command.ok_or_else(|| "empty value".to_string())?;
    hooks::assign(state, ctx, scope, spec, index, c.append, &command)?;
    Ok(String::new())
}

/// `show-hooks [-gpw] [-t target] [event]`: the hooks of a table, in the
/// scope the flags pick, or one hook by name (`<event>[N]` for one
/// element).
pub fn show_hooks(state: &mut State, ctx: &ClientCtx, c: ShowHooks) -> Result<String, String> {
    let named = match &c.hook {
        Some(name) => {
            let (base, index) = split_index(name)?;
            let spec = hooks::event(base).ok_or_else(|| format!("unknown hook: {name}"))?;
            Some((spec, index))
        }
        None => None,
    };
    let (spec, index) = match named {
        Some((spec, index)) => (Some(spec), index),
        None => (None, None),
    };
    hooks::show(state, ctx, &c, index, spec)
}

/// `set-environment [-g] [-h] [-r] [-F] [-t session] name value`, or `-u
/// name`, or `-r name`: the global environment with `-g`, else the target
/// session's. `-u` removes the variable, `-r` marks it removed (`-NAME`),
/// `-F` expands the value as a format. Texts are tmux 3.7b's (recorded).
pub fn set_environment(
    state: &mut State,
    ctx: &ClientCtx,
    c: SetEnvironment,
) -> Result<String, String> {
    let name = c.name.ok_or_else(|| too_few("set-environment"))?;
    if name.contains('=') {
        return Err("variable name contains =".to_string());
    }
    let session = if c.global {
        None
    } else {
        // tmux finds `-t` as a pane (cmd-set-option.c).
        let target = state.model.pane_type(&c.target, ctx)?;
        Some(state.model.resolve_session(&target, ctx)?)
    };
    let value = if c.unset {
        if c.value.is_some() {
            return Err("can't specify a value with -u".to_string());
        }
        None
    } else if c.clear {
        if c.value.is_some() {
            return Err("can't specify a value with -r".to_string());
        }
        match session {
            None => hooks::clear_global(state, &name),
            Some(s) => hooks::clear_session(state, s, &name),
        }
        return Ok(String::new());
    } else {
        let value = c.value.ok_or_else(|| "no value specified".to_string())?;
        Some(if c.format {
            let pane = state.model.resolve_pane(&c.target, ctx).ok();
            super::bindings::expand(state, ctx, pane, &value)
        } else {
            value
        })
    };
    match session {
        None => hooks::set_global(state, &name, value, c.hidden),
        Some(s) => hooks::set_session(state, s, &name, value, c.hidden),
    }
    Ok(String::new())
}

/// `show-environment [-g] [-h] [-s] [-t session] [name]`: `NAME=value`
/// lines, by name, of the visible variables (the hidden ones with `-h`),
/// and `-NAME` for a variable marked removed. With `-s`, shell commands
/// instead: `NAME="value"; export NAME;`, and `unset NAME;` for a removed
/// one. A named variable that does not exist is an error; one of the other
/// kind shows nothing, as in tmux 3.7b.
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
    let line = |v: &hooks::Variable| match (&v.value, c.shell) {
        (Some(value), false) => format!("{}={value}\n", v.name),
        (None, false) => format!("-{}\n", v.name),
        (Some(value), true) => {
            format!(
                "{}=\"{}\"; export {};\n",
                v.name,
                shell_escape(value),
                v.name
            )
        }
        (None, true) => format!("unset {};\n", v.name),
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

/// A value for inside double quotes in `sh`: `$`, `` ` ``, `"` and `\` get
/// a backslash, as tmux's `cmd_show_environment_escape`.
fn shell_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '$' | '`' | '"' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
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
        assert_eq!(
            run(&mut state, &["show-hooks"]),
            Err("no current session".to_string())
        );
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
            Err("no current session".to_string())
        );
        assert_eq!(
            run(
                &mut state,
                &["set-hook", "-g", "pane-focused[x]", "list-sessions"]
            ),
            Err("ambiguous option: pane-focused[x]".to_string())
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

    #[test]
    fn show_environment_s_prints_shell_lines() {
        let mut state = test_state();
        run(&mut state, &["set-environment", "-g", "A", "a b"]).unwrap();
        run(&mut state, &["set-environment", "-g", "Q", "x\"$y`z\\w"]).unwrap();
        run(&mut state, &["set-environment", "-gr", "R"]).unwrap();
        assert_eq!(
            run(&mut state, &["show-environment", "-gs"]),
            Ok("A=\"a b\"; export A;\nQ=\"x\\\"\\$y\\`z\\\\w\"; export Q;\nunset R;\n".to_string())
        );
        assert_eq!(
            run(&mut state, &["show-environment", "-gs", "R"]),
            Ok("unset R;\n".to_string())
        );
    }

    #[test]
    fn set_environment_r_marks_a_variable_removed() {
        let mut state = test_state();
        run(&mut state, &["set-environment", "-g", "A", "1"]).unwrap();
        run(&mut state, &["set-environment", "-gr", "A"]).unwrap();
        run(&mut state, &["set-environment", "-gr", "NEW"]).unwrap();
        assert_eq!(
            run(&mut state, &["show-environment", "-g"]),
            Ok("-A\n-NEW\n".to_string())
        );
        assert_eq!(
            run(&mut state, &["set-environment", "-gr", "A", "x"]),
            Err("can't specify a value with -r".to_string())
        );
        // `-u` removes it altogether.
        run(&mut state, &["set-environment", "-gu", "NEW"]).unwrap();
        assert_eq!(
            run(&mut state, &["show-environment", "-g", "NEW"]),
            Err("unknown variable: NEW".to_string())
        );
        // A hidden variable stays hidden when removed; a set value revives it.
        run(&mut state, &["set-environment", "-gh", "H", "1"]).unwrap();
        run(&mut state, &["set-environment", "-gr", "H"]).unwrap();
        assert_eq!(
            run(&mut state, &["show-environment", "-gh"]),
            Ok("-H\n".to_string())
        );
        run(&mut state, &["set-environment", "-g", "A", "2"]).unwrap();
        assert_eq!(
            run(&mut state, &["show-environment", "-g"]),
            Ok("A=2\n".to_string())
        );
    }

    #[test]
    fn a_removed_global_variable_is_not_given_to_panes() {
        let mut state = test_state();
        run(&mut state, &["set-environment", "-g", "KEEP", "1"]).unwrap();
        run(&mut state, &["set-environment", "-g", "DROP", "1"]).unwrap();
        run(&mut state, &["set-environment", "-gr", "DROP"]).unwrap();
        let env = crate::server::hooks::shell_env(&state, None);
        assert!(env.contains(&("KEEP".to_string(), "1".to_string())));
        assert!(!env.iter().any(|(k, _)| k == "DROP"));
    }

    #[tokio::test]
    async fn set_environment_f_expands_the_value() {
        let mut state = test_state();
        run(&mut state, &["new-session", "-d", "-s", "work"]).unwrap();
        run(&mut state, &["set-option", "-g", "@d", "/x"]).unwrap();
        run(&mut state, &["set-environment", "-gF", "A", "#{@d}/y"]).unwrap();
        run(&mut state, &["set-environment", "-g", "B", "#{@d}/y"]).unwrap();
        assert_eq!(
            run(&mut state, &["show-environment", "-g"]),
            Ok("A=/x/y\nB=#{@d}/y\n".to_string())
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
