//! Menus and confirmations (TC.7): `display-menu`, `display-panes`,
//! `confirm-before`, drawn by `server::menu` and `server::prompt`, and
//! grofe aliases, expanded before a request is parsed ([`expand_aliases`]).

use crate::cmd::{self, ConfirmBefore, DisplayMenu, DisplayPanes, MenuEntry, Resolve};
use crate::protocol::{ClientCtx, Dialect};
use crate::server::input::parse_key;
use crate::server::menu::{self, Look, Menu, MenuItem, PanesArgs, Position, Start};
use crate::server::popup::Lines;
use crate::server::{State, clients, prompt};

/// tmux's wording for a verb given no arguments.
fn too_few_arguments(verb: &str) -> String {
    cmd::ParseError::TooFewArguments(verb.to_string(), 1).to_string()
}

/// `display-menu [-OM] [-b lines] [-c client] [-C row] [-H style] [-s style]
/// [-S style] [-t pane] [-T title] [-x x] [-y y] name key command ...` on
/// the current client, as tmux 3.7b: title, labels and commands are
/// formats; an empty name is a separator, a name starting with `-` a
/// disabled item; a key that is not a key name shows no key. Nothing opens
/// while the client has a menu open, or when the menu does not fit. From
/// the CLI the reply waits until the menu closes.
pub fn display_menu(state: &mut State, ctx: &ClientCtx, c: DisplayMenu) -> Result<String, String> {
    if c.items.is_empty() {
        return Err(too_few_arguments("display-menu"));
    }
    let id = target_client(state, ctx, c.client.as_deref())?;
    let at;
    let ctx = match &c.target {
        Some(target) => {
            at = ClientCtx {
                pane: Some(state.model.resolve_pane(target, ctx)?),
                ..ctx.clone()
            };
            &at
        }
        None => ctx,
    };
    if state.popups.is_open(id) || state.menus.is_open(id) {
        return Ok(String::new());
    }
    let start = match c.starting_choice.as_deref() {
        None => Start::First,
        Some("-") => Start::None,
        Some(n) => Start::At(
            strtonum(n, 0, i64::from(u32::MAX)).map_err(|e| format!("starting choice {e}"))? as u32,
        ),
    };
    let lines = match c.border.as_deref() {
        None => None,
        Some(v) => {
            Some(Lines::parse(v).map_err(|_| format!("menu-border-lines unknown value: {v}"))?)
        }
    };
    let pointer = crate::server::mouse::pointer(state, id);
    let mut x = position(state, ctx, c.x.as_deref(), Axis::X, pointer)?;
    let mut y = position(state, ctx, c.y.as_deref(), Axis::Y, pointer)?;
    if let Some((px, py)) = pointer
        && ctx.dialect == Dialect::Grofe
    {
        if c.x.is_none() {
            x = Position::Pointer(px);
        }
        if c.y.is_none() {
            y = Position::Pointer(py);
        }
    }
    let items = c
        .items
        .into_iter()
        .map(|entry| match entry {
            MenuEntry::Separator => MenuItem::Separator,
            MenuEntry::Item { name, key, command } => MenuItem::Command {
                label: name,
                key: parse_key(&key).ok(),
                command,
            },
        })
        .collect();
    let menu = Menu {
        title: c.title,
        items,
        x,
        y,
        look: Look {
            lines,
            style: c.style,
            selected: c.selected_style,
            border: c.border_style,
            stay_open: c.stay_open,
            mouse: c.mouse,
            start,
        },
    };
    if menu::open(state, ctx, id, menu) {
        menu::wait_for_close(state, id);
    }
    Ok(String::new())
}

/// The client `name` names, else the current one.
fn target_client(
    state: &State,
    ctx: &ClientCtx,
    name: Option<&str>,
) -> Result<crate::types::ClientId, String> {
    match name {
        Some(name) => {
            clients::client_named(state, name).ok_or_else(|| format!("can't find client: {name}"))
        }
        None => clients::current_client(state, ctx).ok_or_else(|| "no current client".to_string()),
    }
}

/// OpenBSD's `strtonum`, as tmux words its failures: `invalid`, `too small`
/// or `too large`.
fn strtonum(value: &str, min: i64, max: i64) -> Result<i64, &'static str> {
    let n: i64 = value.parse().map_err(|_| "invalid")?;
    if n < min {
        Err("too small")
    } else if n > max {
        Err("too large")
    } else {
        Ok(n)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Axis {
    X,
    Y,
}

/// A `-x`/`-y` value: tmux's letters (`C` either way, `P` and `R` for `x`,
/// `P` for `y`), else a format expanded and read as C's `strtol` reads it
/// (a leading number, else 0). `M` is the pointer's cell, or 0 with no mouse
/// event as in tmux; `W`, `S` and a format naming `popup_*` wait for the
/// menu's size ([`Position::Expr`]).
fn position(
    state: &State,
    ctx: &ClientCtx,
    value: Option<&str>,
    axis: Axis,
    pointer: Option<(u16, u16)>,
) -> Result<Position, String> {
    let Some(value) = value else {
        return Ok(Position::Centre);
    };
    match (value, axis) {
        ("C", _) => return Ok(Position::Centre),
        ("P", _) => return Ok(Position::Pane),
        ("R", Axis::X) => return Ok(Position::PaneRight),
        ("M", _) => {
            return Ok(match (pointer, axis) {
                (Some((px, _)), Axis::X) => Position::Pointer(px),
                (Some((_, py)), Axis::Y) => Position::Pointer(py),
                (None, _) => Position::Cells(0),
            });
        }
        ("W", _) | ("S", Axis::Y) => return Ok(Position::Expr(value.to_string())),
        _ if value.contains("popup_") => return Ok(Position::Expr(value.to_string())),
        _ => {}
    }
    Ok(Position::Cells(strtol(&prompt::expand_command(
        state, ctx, value,
    ))))
}

/// C's `strtol(s, NULL, 10)`: blanks, a sign, then digits; 0 without any.
fn strtol(s: &str) -> i64 {
    let s = s.trim_start();
    let (sign, digits) = match s.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    digits[..end].parse::<i64>().map_or(0, |n| sign * n)
}

/// `display-panes [-bN] [-d duration] [-t client] [template]`: each pane
/// of the current window labelled with its index until a key or `-d`
/// milliseconds (`display-panes-time`; 0 keeps them up); a digit (or `a`
/// to `z`) chooses that pane, and the template runs with `%%` for its id
/// (`select-pane -t "%%%"` by default). `-N` takes no keys. From the CLI
/// the reply waits until the labels close, unless `-b`.
pub fn display_panes(
    state: &mut State,
    ctx: &ClientCtx,
    c: DisplayPanes,
) -> Result<String, String> {
    let id = target_client(state, ctx, c.client.as_deref())?;
    if state.menus.is_open(id) || state.popups.is_open(id) {
        return Ok(String::new());
    }
    let duration = match c.duration.as_deref() {
        None => None,
        Some(n) => Some(std::time::Duration::from_millis(
            strtonum(n, 0, i64::from(u32::MAX)).map_err(|e| format!("delay {e}"))? as u64,
        )),
    };
    let args = PanesArgs {
        duration,
        no_keys: c.no_keys,
        template: c.template,
        dialect: ctx.dialect,
    };
    if menu::open_panes(state, id, args) && !c.background {
        menu::wait_for_close(state, id);
    }
    Ok(String::new())
}

/// `confirm-before [-p prompt] command`: a one-key prompt on the current
/// client, `command` run only on `y`. The command must parse now, as
/// tmux parses it when the prompt opens. Without `-p` the prompt names the
/// first command, as tmux 3.7b words it (`Confirm 'kill-pane'? (y/n)`).
pub fn confirm_before(
    state: &mut State,
    ctx: &ClientCtx,
    c: ConfirmBefore,
) -> Result<String, String> {
    let Some(command) = c.command.clone() else {
        return Err(too_few_arguments("confirm-before"));
    };
    let cmds = cmd::split_command_string(&command).map_err(|e| e.to_string())?;
    for argv in &cmds {
        crate::compat::parse_split(ctx, argv)?;
    }
    let verb = cmds
        .first()
        .and_then(|argv| argv.first())
        .map_or("", |v| cmd::parse::canonical_name(v));
    prompt::confirm(state, ctx, &c, verb, command)
}

/// The request's argv with grofe aliases expanded, or `None` to run it as
/// it is. [`super::run`] and [`super::run_split`] call this before
/// parsing, for every dialect. The aliases are the `command-alias` list,
/// each `name=command`, as tmux 3.7b reads it (`cmd_get_alias`): the
/// first entry whose name is exactly a command's verb replaces the verb
/// with its command's words, and the command's own arguments follow. An
/// alias is expanded once, never inside its own expansion. `argv` may hold
/// several commands split at `;` arguments; each one's verb is looked up.
pub fn expand_aliases(state: &State, argv: &[String]) -> Option<Vec<String>> {
    let cmds = cmd::parse::split_argv(argv);
    let expanded: Vec<Option<Vec<Vec<String>>>> = cmds
        .iter()
        .map(|words| expand_alias_command(state, words))
        .collect();
    if expanded.iter().all(Option::is_none) {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    for (words, expanded) in cmds.into_iter().zip(expanded) {
        for command in expanded.unwrap_or_else(|| vec![words]) {
            if !out.is_empty() {
                out.push(";".to_string());
            }
            // A word ending in `;` is a literal one: escape it again so the
            // split that follows reads it back the same.
            out.extend(command.into_iter().map(|w| match w.strip_suffix(';') {
                Some(stem) => format!("{stem}\\;"),
                None => w,
            }));
        }
    }
    Some(out)
}

/// One already-split command (a binding's, the prompt's, a menu item's)
/// with an alias verb expanded: the alias's commands, the command's own
/// arguments after the last of them, or `None` when the verb is no alias
/// (or its value does not split). A `;` word in `argv` stays literal.
/// [`super::run_split`] runs what this returns; `bind-key` stores it, as
/// tmux expands an alias when the binding is made.
pub fn expand_alias_command(state: &State, argv: &[String]) -> Option<Vec<Vec<String>>> {
    let aliases = state.model.options.lookup_list("command-alias", &[])?;
    expand_alias(aliases.values(), argv)
}

/// [`expand_alias_command`] over an explicit `command-alias` list (the
/// config loader's, for its bindings).
pub fn expand_alias<'a>(
    aliases: impl IntoIterator<Item = &'a String>,
    argv: &[String],
) -> Option<Vec<Vec<String>>> {
    let (verb, args) = argv.split_first()?;
    let value = aliases.into_iter().find_map(|entry| {
        let (name, value) = entry.split_once('=')?;
        (name == verb).then_some(value)
    })?;
    let mut cmds = cmd::split_command_string(value).ok()?;
    cmds.last_mut()?.extend(args.iter().cloned());
    Some(cmds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{dispatch, test_ctx, test_state};

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn with_aliases(aliases: &[&str]) -> State {
        let mut state = test_state();
        for alias in aliases {
            dispatch::run(
                &mut state,
                &test_ctx(),
                &argv(&["set", "-ag", "command-alias", alias]),
            )
            .expect("set command-alias");
        }
        state
    }

    #[test]
    fn an_alias_replaces_the_verb_and_keeps_the_arguments() {
        let state = with_aliases(&["mya=display-message hi", "ren=rename-window -t main"]);
        assert_eq!(
            expand_aliases(&state, &argv(&["mya"])),
            Some(argv(&["display-message", "hi"]))
        );
        assert_eq!(
            expand_aliases(&state, &argv(&["ren", "new name"])),
            Some(argv(&["rename-window", "-t", "main", "new name"]))
        );
        // Not an alias: run as it is.
        assert_eq!(expand_aliases(&state, &argv(&["list-sessions"])), None);
        // Exact names only.
        assert_eq!(expand_aliases(&state, &argv(&["my"])), None);
        assert_eq!(expand_aliases(&state, &argv(&["myab"])), None);
        assert_eq!(expand_aliases(&test_state(), &argv(&["mya"])), None);
    }

    #[test]
    fn each_command_of_a_sequence_is_looked_up() {
        let state = with_aliases(&["mya=display-message hi", "two=a ; b x"]);
        assert_eq!(
            expand_aliases(&state, &argv(&["ls", ";", "mya", "a;"])),
            Some(argv(&["ls", ";", "display-message", "hi", "a"]))
        );
        assert_eq!(
            expand_aliases(&state, &argv(&["two", "y"])),
            Some(argv(&["a", ";", "b", "x", "y"]))
        );
        // A literal `;` word survives the round trip.
        let got = expand_aliases(&state, &argv(&["mya", "x\\;"])).expect("expanded");
        assert_eq!(
            cmd::parse::split_argv(&got),
            vec![argv(&["display-message", "hi", "x;"])]
        );
    }

    /// tmux 3.7b: the first entry with the name wins, and an alias is not
    /// expanded inside its own expansion.
    #[test]
    fn the_first_alias_wins_and_expansion_happens_once() {
        let state = with_aliases(&["a=b", "a=c", "b=display-message loop"]);
        assert_eq!(expand_aliases(&state, &argv(&["a"])), Some(argv(&["b"])));
    }

    #[test]
    fn an_alias_runs_as_its_command() {
        let mut state = with_aliases(&["sesh=has-session -t nosuch"]);
        let got = dispatch::run(&mut state, &test_ctx(), &argv(&["sesh"]));
        assert_eq!(got, Err("can't find session: nosuch".to_string()));
    }

    /// tmux 3.7b: an alias standing for a sequence runs every command of
    /// it from a binding, a menu item or the prompt (all split already).
    #[test]
    fn a_split_command_can_be_a_sequence_alias() {
        let state = with_aliases(&["two=rename-window S1 ; set -g @b S2"]);
        assert_eq!(
            expand_alias_command(&state, &argv(&["two", "x"])),
            Some(vec![
                argv(&["rename-window", "S1"]),
                argv(&["set", "-g", "@b", "S2", "x"])
            ])
        );
        // A `;` word of a split command stays literal.
        let state = with_aliases(&["say=send-keys"]);
        assert_eq!(
            expand_alias_command(&state, &argv(&["say", ";"])),
            Some(vec![argv(&["send-keys", ";"])])
        );
        assert_eq!(expand_alias_command(&state, &argv(&["ls"])), None);
    }

    #[test]
    fn strtol_reads_a_leading_number() {
        assert_eq!(strtol("12"), 12);
        assert_eq!(strtol(" -5x"), -5);
        assert_eq!(strtol("+3"), 3);
        assert_eq!(strtol("R"), 0);
        assert_eq!(strtol(""), 0);
    }

    #[test]
    fn bare_verbs_are_too_few_arguments_and_menus_need_a_client() {
        let mut state = test_state();
        let ctx = test_ctx();
        let run = |state: &mut State, words: &[&str]| dispatch::run(state, &ctx, &argv(words));
        assert_eq!(
            run(&mut state, &["display-menu"]),
            Err(too_few_arguments("display-menu"))
        );
        assert_eq!(
            run(&mut state, &["confirm-before"]),
            Err(too_few_arguments("confirm-before"))
        );
        for words in [
            &["display-menu", "a", "b", "c"][..],
            &["display-panes"],
            &["confirm-before", "kill-pane"],
        ] {
            assert_eq!(
                run(&mut state, words),
                Err("no current client".to_string()),
                "{words:?}"
            );
        }
        assert_eq!(
            run(&mut state, &["confirm-before", "no-such-verb"]),
            Err("unknown command: no-such-verb".to_string())
        );
    }
}
