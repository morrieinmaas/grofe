use super::*;
use crate::cmd::{ListSessions, SessionRef, SetOption, ShowMessages, ShowOptions, WindowRef};

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

fn ctx() -> ClientCtx {
    ClientCtx {
        dialect: Dialect::Tmux,
        ..crate::server::test_ctx()
    }
}

fn one_or_none(words: &[&str]) -> Result<Vec<Command>, TmuxError> {
    dispatch(&args(words), &ctx())
}

fn one(words: &[&str]) -> Result<Command, TmuxError> {
    let mut got = dispatch(&args(words), &ctx())?;
    assert_eq!(got.len(), 1, "{got:?}");
    Ok(got.remove(0))
}

#[test]
fn the_command_table_is_tmux_s_91_plus_the_hidden_one() {
    let specs = specs();
    assert_eq!(specs.iter().filter(|s| !s.hidden).count(), 91);
    let attach = resolve("attach-session").unwrap();
    assert_eq!(attach.alias, Some("attach"));
    assert!(attach.takes_value('t') && attach.takes_value('c'));
    assert!(!attach.takes_value('d'));
    assert!(resolve("wait-for").unwrap().usage.starts_with("[-L|-S|-U]"));
    assert!(!resolve("wait-for").unwrap().takes_value('L'));
    assert!(resolve("select-pane").unwrap().takes_value('P'));
}

/// Every alias in the golden resolves to its command, as does every
/// full name.
#[test]
fn every_alias_and_name_resolves() {
    for spec in specs() {
        assert_eq!(resolve(spec.name).unwrap().name, spec.name);
        if let Some(alias) = spec.alias {
            assert_eq!(resolve(alias).unwrap().name, spec.name, "{alias}");
        }
    }
    assert_eq!(resolve("info").unwrap().name, "server-info");
}

/// Recorded on tmux 3.7b (`-L grofe-fe1 -f /dev/null`).
#[test]
fn prefixes_resolve_as_tmux_s() {
    assert_eq!(resolve("a").unwrap().name, "attach-session");
    assert_eq!(resolve("new-s").unwrap().name, "new-session");
    assert_eq!(resolve("last").unwrap().name, "last-window");
    assert_eq!(resolve("lastp").unwrap().name, "last-pane");
    assert_eq!(
        resolve("kill-s").unwrap_err().0,
        "ambiguous command: kill-s, could be: kill-server, kill-session"
    );
    assert_eq!(
        resolve("list").unwrap_err().0,
        "ambiguous command: list, could be: list-buffers, list-clients, list-commands, \
         list-keys, list-panes, list-sessions, list-windows"
    );
    assert!(resolve("se").unwrap_err().0.starts_with(
        "ambiguous command: se, could be: select-layout, select-pane, select-window, \
         send-keys, send-prefix, server-access, set-buffer,"
    ));
    assert_eq!(resolve("nope").unwrap_err().0, "unknown command: nope");
}

#[test]
fn getopt_reads_as_tmux_s() {
    let spec = resolve("new-session").unwrap();
    let a = args(&["-ds", "x", "-c/tmp", "--", "-t", "cmd"]);
    let inv = Invocation {
        verb: spec.name,
        args: &a,
        ctx: &ctx(),
        spec,
    };
    let got = inv.getopt();
    assert_eq!(
        got.flags,
        vec![('d', None), ('s', Some("x")), ('c', Some("/tmp"))]
    );
    assert_eq!(inv.positionals(), &a[4..]);
}

#[test]
fn rows_are_well_formed_and_every_form_verb_is_tmux_s() {
    for row in table() {
        let verb = row.verb();
        if !row.tmux_form.starts_with("tmux") && !verb.is_empty() {
            assert_eq!(resolve(verb).map(|s| s.name), Ok(verb), "{}", row.tmux_form);
        }
        assert_eq!(
            row.form_flags().len(),
            row.flags.len(),
            "{}: one mark per flag",
            row.tmux_form
        );
    }
    let r = table().iter().find(|r| r.tmux_form == "-t[=]").unwrap();
    assert_eq!(r.form_flags(), vec![('t', Some("="))]);
}

#[test]
fn m1_to_m3_verbs_pass_through_by_alias_and_prefix() {
    assert_eq!(
        one(&["ls"]).unwrap(),
        Command::ListSessions(ListSessions::default())
    );
    assert_eq!(
        one(&["list-s"]).unwrap(),
        Command::ListSessions(ListSessions::default())
    );
    let got = dispatch(&args(&["ls", ";", "kill-server"]), &ctx()).unwrap();
    assert_eq!(got.len(), 2);
}

#[test]
fn exact_targets_and_offsets_reach_the_native_command() {
    let Command::HasSession(c) = one(&["has", "-t", "=x"]).unwrap() else {
        panic!()
    };
    assert_eq!(c.target, Target::Session(SessionRef::Bare("=x".into())));
    let Command::SelectWindow(c) = one(&["selectw", "-t", "=x:2"]).unwrap() else {
        panic!()
    };
    assert_eq!(
        c.target,
        Target::Window(SessionRef::Exact("x".into()), WindowRef::Index(2))
    );
    // A bare `=` binds, and fails when it resolves (tmux's order).
    let Command::HasSession(c) = one(&["has-session", "-t", "="]).unwrap() else {
        panic!()
    };
    assert_eq!(c.target, Target::Session(SessionRef::Mouse));
    assert!(one(&["select-pane", "-t", ":.+"]).is_ok());
}

#[test]
fn set_and_show_take_tmux_names_and_window_forms() {
    // `setw -g` is native `set -w -g` (`set -gw`).
    assert_eq!(
        one(&["setw", "-g", "mode-keys", "vi"]).unwrap(),
        cmd::parse(&args(&["set-option", "-gw", "mode-keys", "vi"])).unwrap()
    );
    let Command::ShowOptions(ShowOptions {
        value_only, quiet, ..
    }) = one(&["show", "-gqv", "base-index"]).unwrap()
    else {
        panic!()
    };
    assert!(value_only && quiet);
    assert_eq!(
        one(&["showw", "-v", "remain-on-exit"]).unwrap(),
        cmd::parse(&args(&["show-options", "-wv", "remain-on-exit"])).unwrap()
    );
    assert!(matches!(
        one(&["set", "-a", "status-format[1]", "x"]).unwrap(),
        Command::SetOption(SetOption { append: true, .. })
    ));
    assert_eq!(
        one(&["set-window-option"]).unwrap_err().0,
        "command set-window-option: too few arguments (need at least 1)"
    );
    // tmux's server scope is grofe's global one.
    assert_eq!(
        one(&["set", "-as", "terminal-features", ",x:RGB"]).unwrap(),
        cmd::parse(&args(&[
            "set-option",
            "-a",
            "-g",
            "terminal-features",
            ",x:RGB"
        ]))
        .unwrap()
    );
    assert_eq!(
        one(&["show", "-sv", "escape-time"]).unwrap(),
        cmd::parse(&args(&["show-options", "-gv", "escape-time"])).unwrap()
    );
    assert_eq!(resolve("set-option").unwrap().min_args(), 1);
    assert_eq!(resolve("if-shell").unwrap().min_args(), 2);
    assert_eq!(resolve("command-prompt").unwrap().min_args(), 0);
    assert_eq!(store_name("status-format[2]"), "status-format[2]");
    assert_eq!(store_name("@x"), "@x");
    assert_eq!(store_name("nope"), "nope");
}

/// The tmux dialect knows every option grofe has that tmux's CLI knows,
/// and only those: grofe's own (`pane-frames` and the rest) are refused.
#[test]
fn the_tmux_dialect_knows_the_option_store_s_names() {
    use crate::server::dispatch::options::{grofe_only_list_names, grofe_only_names};
    for name in grofe_only_names().chain(grofe_only_list_names()) {
        assert!(option_name(name).is_none(), "{name} is grofe's own");
    }
    for name in builtin_names().chain(list_names()) {
        assert_eq!(option_name(name), Some(name));
    }
}

#[test]
fn list_commands_answers_from_the_golden() {
    let Command::DisplayMessage(c) = one(&["lscm", "has"]).unwrap() else {
        panic!()
    };
    assert!(c.print);
    assert_eq!(
        c.text.as_deref(),
        Some("has-session (has) [-t target-session]")
    );
    let Command::DisplayMessage(c) = one(&[
        "list-commands",
        "-F",
        "#{command_list_name}|#{command_list_alias}|#{session_name}",
        "attach",
    ])
    .unwrap() else {
        panic!()
    };
    assert_eq!(
        c.text.as_deref(),
        Some("attach-session|attach|#{session_name}")
    );
    let Command::DisplayMessage(c) = one(&["list-commands"]).unwrap() else {
        panic!()
    };
    assert_eq!(c.text.unwrap().lines().count(), 91);
    assert_eq!(
        one(&["lscm", "nope"]).unwrap_err().0,
        "unknown command: nope"
    );
    assert_eq!(
        one(&["lscm", "a", "b"]).unwrap_err().0,
        "command list-commands: too many arguments (need at most 1)"
    );
}

#[test]
fn info_is_show_messages_jt() {
    assert_eq!(
        one(&["info"]).unwrap(),
        Command::ShowMessages(ShowMessages {
            client: None,
            jobs: true,
            terminals: true,
        })
    );
}

#[test]
fn select_pane_style_selects_and_sets_the_pane_styles() {
    let got = dispatch(
        &args(&["select-pane", "-P", "bg=colour238", "-t", "%1"]),
        &ctx(),
    )
    .unwrap();
    let [
        Command::SelectPane(select),
        Command::SetOption(style),
        Command::SetOption(active),
    ] = &got[..]
    else {
        panic!("{got:?}")
    };
    let pane = cmd::target::parse_target("%1").unwrap();
    assert_eq!(select.target, pane);
    for (set, name) in [(style, "window-style"), (active, "window-active-style")] {
        assert_eq!(
            (
                set.scope,
                &set.target,
                set.name.as_str(),
                set.value.as_deref()
            ),
            (
                Some(cmd::OptionScope::Pane),
                &pane,
                name,
                Some("bg=colour238")
            )
        );
    }
    assert_eq!(
        one(&["select-pane", "-t", "%1", "-P"]).unwrap_err().0,
        "command select-pane: -P expects an argument"
    );
}

/// The most specific row decides, wherever it is registered: a
/// flagless `new-session` row ahead of the core rows (as an area row
/// would be) does not swallow the refusal of `new-session -t`.
#[test]
fn the_most_specific_row_decides() {
    let mut rows = vec![pass("new-session", "M1")];
    rows.push(unsupported("new-session -t", "test", SEMANTIC));
    let got = translate_with(&args(&["new-session", "-t", "x"]), &ctx(), &rows, &[]);
    assert!(got.unwrap_err().0.contains("not supported"));
    assert!(translate_with(&args(&["new-session", "-d"]), &ctx(), &rows, &[]).is_ok());
    // Between rows as specific, the earlier one.
    let rows = vec![
        unsupported("new-session", "first", &[]),
        pass("new-session", "M1"),
    ];
    let err = translate_with(&args(&["new-session"]), &ctx(), &rows, &[]).unwrap_err();
    assert_eq!(err.0, "new-session: not supported (first)");
    // A pinned positional counts as much as a flag.
    let rows = vec![
        pass("list-commands", "M3.6"),
        unsupported("list-commands new-pane", "pinned", &[]),
    ];
    let err = translate_with(&args(&["lscm", "new-pane"]), &ctx(), &rows, &[]);
    assert!(err.unwrap_err().0.contains("pinned"));
}

/// A registered rewrite applies before any row matches, so a row
/// that pins the rewritten value sees it.
#[test]
fn rewrites_apply_before_the_rows() {
    fn short_forms(_: &Invocation<'_>, v: &str) -> Result<Rewritten, TmuxError> {
        Ok(Rewritten::value(v.replace("#S", "#{session_name}")))
    }
    let registered = [
        Rewrite {
            verb: "display-message",
            arg: Arg::Positional(0),
            rewrite: short_forms,
        },
        Rewrite {
            verb: "list-sessions",
            arg: Arg::Flag('F'),
            rewrite: short_forms,
        },
    ];
    let mut rows = vec![unsupported(
        "list-sessions -F[#{session_name}]",
        "pinned after the rewrite",
        SEMANTIC,
    )];
    rows.extend(core_rows());
    let got = translate_with(&args(&["display", "-p", "#S!"]), &ctx(), &rows, &registered).unwrap();
    let [Command::DisplayMessage(d)] = &got[..] else {
        panic!("{got:?}")
    };
    assert_eq!(d.text.as_deref(), Some("#{session_name}!"));
    let err = translate_with(&args(&["ls", "-F#S"]), &ctx(), &rows, &registered).unwrap_err();
    assert!(err.0.contains("pinned after the rewrite"), "{err}");
    // With no rewrite for its verb, a command is left as sent.
    let got = translate_with(&args(&["lsw", "-F", "#S"]), &ctx(), &rows, &registered).unwrap();
    let [Command::ListWindows(w)] = &got[..] else {
        panic!("{got:?}")
    };
    assert_eq!(w.format.as_deref(), Some("#S"));
}

/// FE.3's `#(cmd)`: a rewrite of `set`'s value, seeing the option it
/// is for, registers the source first and leaves the option reading
/// it.
#[test]
fn a_rewrite_registers_a_source_before_its_set() {
    fn jobs(inv: &Invocation<'_>, v: &str) -> Result<Rewritten, TmuxError> {
        assert_eq!(inv.option_name(), Some("status-right"));
        let Some(job) = v.strip_prefix("#(").and_then(|j| j.strip_suffix(')')) else {
            return Ok(Rewritten::value(v));
        };
        let key = crate::server::sources::inline_key(job);
        Ok(Rewritten {
            value: format!("#{{@{key}}}"),
            before: vec![Command::RegisterSource(cmd::RegisterSource {
                name: None,
                command: job.to_string(),
            })],
        })
    }
    let registered = [Rewrite {
        verb: "set-option",
        arg: Arg::Positional(1),
        rewrite: jobs,
    }];
    let got = translate_with(
        &args(&["set", "-g", "status-right", "#(date)"]),
        &ctx(),
        &core_rows(),
        &registered,
    )
    .unwrap();
    let [Command::RegisterSource(source), Command::SetOption(set)] = &got[..] else {
        panic!("{got:?}")
    };
    assert_eq!(
        (source.name.as_deref(), source.command.as_str()),
        (None, "date")
    );
    let key = crate::server::sources::inline_key("date");
    assert_eq!(set.value.as_deref(), Some(format!("#{{@{key}}}").as_str()));
    assert_eq!(set.name, "status-right");
    assert_eq!(
        cmd::parse(&args(&["__register-source", "-n", "batt", "acpi"])).unwrap(),
        Command::RegisterSource(cmd::RegisterSource {
            name: Some("batt".into()),
            command: "acpi".into(),
        })
    );
}

/// A refusal for good wins over any mapping for its verb, however many
/// flags the mapping names and wherever it is registered; a pinned
/// value outranks a bare flag.
#[test]
fn refusals_and_pinned_values_come_first() {
    let two_flags = row(
        "new-session -d -s",
        GrofeTarget::Native("new-session"),
        &[FlagMark::Semantic, FlagMark::Semantic],
        "M3.6",
        "",
    );
    let mut rows = vec![two_flags];
    rows.push(unsupported("new-session -t", "a refusal", SEMANTIC));
    let err = translate_with(
        &args(&["new", "-d", "-s", "x", "-t", "y"]),
        &ctx(),
        &rows,
        &[],
    );
    assert!(err.unwrap_err().0.contains("not supported"));
    assert!(translate_with(&args(&["new", "-d", "-s", "x"]), &ctx(), &rows, &[]).is_ok());

    // A mouse refusal, in either registration order.
    let map = row(
        "resize-pane -M -Z",
        GrofeTarget::Native("resize-pane"),
        &[FlagMark::Semantic, FlagMark::Semantic],
        "M3.6",
        "",
    );
    let refuse = unsupported("resize-pane -M", "a mouse form", SEMANTIC);
    for rows in [vec![map.clone(), refuse.clone()], vec![refuse, map]] {
        let err = translate_with(&args(&["resize-pane", "-M", "-Z"]), &ctx(), &rows, &[]);
        assert!(err.unwrap_err().0.contains("mouse"), "{rows:?}");
    }

    // `-x[M]` over a bare `-x` with more flags.
    let bare = unsupported("display-menu -x -y", "bare", &[]);
    let bare = Row {
        milestone: "M3.6",
        ..bare
    };
    let pinned = Row {
        milestone: "M3.6",
        ..unsupported("display-menu -x[M]", "pinned", &[])
    };
    let err = translate_with(
        &args(&["menu", "-x", "M", "-y", "1", "a", "b", "c"]),
        &ctx(),
        &[bare, pinned],
        &[],
    );
    assert!(err.unwrap_err().0.contains("pinned"));
    // A refusal for a milestone ranks as a mapping, so a later row can take it over.
    let m5 = row(
        "copy-mode",
        GrofeTarget::Unsupported("mapped by M5"),
        &[],
        "M5",
        "",
    );
    let mut rows = vec![m5];
    rows.extend(core_rows());
    let err = translate_with(&args(&["copy-mode"]), &ctx(), &rows, &[]).unwrap_err();
    assert!(err.0.contains("mapped by M5"), "{err}");
}

/// A mapping never replaces a refusal for good.
#[test]
fn a_mapping_never_replaces_a_refusal() {
    let map = row(
        "detach-client -E",
        GrofeTarget::Native("detach-client"),
        SEMANTIC,
        "M3.6",
        "",
    );
    let mut rows = vec![map];
    rows.extend(core_rows());
    let err = translate_with(&args(&["detach-client", "-E", "x"]), &ctx(), &rows, &[]);
    assert!(err.unwrap_err().0.contains("not supported"));
    // tmux 3.7b's own answer to these outside a mouse event: nothing.
    assert_eq!(one_or_none(&["resize-pane", "-M"]), Ok(Vec::new()));
    // TM.7 maps `copy-mode -M` onto the native flag, which does
    // nothing outside a mouse key, as in tmux.
    assert!(matches!(
        one(&["copy-mode", "-M"]),
        Ok(Command::CopyMode(cmd::CopyMode { mouse: true, .. }))
    ));
    // `send -M` is native: the server answers `no mouse target` when
    // no mouse key's commands are running.
    assert!(matches!(
        one(&["send", "-M"]),
        Ok(Command::SendKeys(cmd::SendKeys { mouse: true, .. }))
    ));
}

/// Recorded on tmux 3.7b: flags are checked against tmux's usage.
#[test]
fn flags_tmux_lacks_are_refused_as_tmux_refuses_them() {
    assert_eq!(
        one(&["showw", "-p"]).unwrap_err().0,
        "command show-window-options: unknown flag -p"
    );
    assert_eq!(
        one(&["start-server", "-x"]).unwrap_err().0,
        "command start-server: unknown flag -x"
    );
    assert_eq!(
        one(&["kill-session", "-t"]).unwrap_err().0,
        "command kill-session: -t expects an argument"
    );
    assert_eq!(
        one(&["lscm", "info"]).unwrap_err().0,
        "unknown command: info"
    );
    assert_eq!(dispatch(&args(&[";"]), &ctx()), Ok(Vec::new()));
}

/// tmux keeps server, global session and global window user options
/// apart; builtins stay in grofe's one global scope.
#[test]
fn user_options_keep_their_tmux_scope() {
    let scope = |words: &[&str]| match one(words).unwrap() {
        Command::SetOption(c) => c.scope,
        Command::ShowOptions(c) => c.scope,
        other => panic!("{other:?}"),
    };
    use cmd::OptionScope::{Global, GlobalWindow, Server};
    assert_eq!(scope(&["set", "-s", "@s", "1"]), Some(Server));
    assert_eq!(scope(&["show", "-sv", "@s"]), Some(Server));
    assert_eq!(scope(&["set", "-gw", "@w", "1"]), Some(GlobalWindow));
    assert_eq!(scope(&["setw", "-g", "@w", "1"]), Some(GlobalWindow));
    assert_eq!(scope(&["showw", "-gv", "@w"]), Some(GlobalWindow));
    assert_eq!(scope(&["set", "-g", "@g", "1"]), Some(Global));
    assert_eq!(scope(&["set", "-s", "escape-time", "0"]), Some(Global));
    assert_eq!(scope(&["setw", "-g", "mode-keys", "vi"]), Some(Global));
}

#[test]
fn a_socket_name_has_no_path_separator() {
    let err = cli(&args(&["-L", "a/b", "ls"])).unwrap_err();
    assert!(err.contains("refusing -L a/b"), "{err}");
    assert!(cli(&args(&["-L../x", "ls"])).is_err());
}

#[test]
fn cli_reads_sockets_and_leaves_the_request_for_the_transport() {
    let got = cli(&args(&["-L", "work", "new-session", "-d", "-L", "x"])).unwrap();
    assert_eq!(got.name.as_deref(), Some("work"));
    assert_eq!(got.socket, None);
    assert_eq!(got.argv, args(&["new-session", "-d", "-L", "x"]));
    assert_eq!(got.dialect, Dialect::Tmux);
    assert!(!got.version);

    let got = cli(&args(&["-S/tmp/s.sock", "-Lname", "ls"])).unwrap();
    assert_eq!(got.socket, Some(PathBuf::from("/tmp/s.sock")));
    assert_eq!(got.name.as_deref(), Some("name"));
    assert_eq!(got.argv, args(&["list-sessions"]));

    let got = cli(&args(&["--", "-x"])).unwrap();
    assert_eq!(got.argv, args(&["-x"]));
    let got = cli(&args(&["-L", "x"])).unwrap();
    assert_eq!(got.argv, args(&["new-session"]));
}

#[test]
fn cli_names_the_first_verb_in_full() {
    let got = cli(&args(&["a", "-t", "x"])).unwrap();
    assert_eq!(got.argv, args(&["attach-session", "-t", "x"]));
    let got = cli(&args(&["new", "-d"])).unwrap();
    assert_eq!(got.argv, args(&["new-session", "-d"]));
    // An unknown or ambiguous verb is the server's to refuse.
    assert_eq!(cli(&args(&["s"])).unwrap().argv, args(&["s"]));
}

#[test]
fn cli_accepts_2_and_u_with_no_effect() {
    let got = cli(&args(&["-2", "-u", "-L", "x", "ls"])).unwrap();
    assert_eq!(got.name.as_deref(), Some("x"));
    assert_eq!(got.argv, args(&["list-sessions"]));
    let got = cli(&args(&["-2uLx", "ls"])).unwrap();
    assert_eq!(got.name.as_deref(), Some("x"));
    assert!(cli(&args(&["-2V"])).unwrap().version);
}

#[test]
fn cli_version_wins_as_soon_as_it_is_read() {
    for argv in [
        &["-V"][..],
        &["-V", "-Z"],
        &["-L", "x", "-V"],
        &["-SV", "-V"],
    ] {
        let got = cli(&args(argv)).unwrap();
        assert!(got.version, "{argv:?}");
    }
    // `-V` as `-L`'s value is a socket name.
    assert!(!cli(&args(&["-L", "-V"])).unwrap().version);
}

/// The texts are tmux 3.7b's, recorded on a private socket.
#[test]
fn cli_errors_are_tmux_s() {
    let err = cli(&args(&["-Z"])).unwrap_err();
    assert!(err.starts_with("tmux: unknown option -- Z\nusage: tmux [-2CDhlNuVv]"));
    let err = cli(&args(&["-L"])).unwrap_err();
    assert!(err.starts_with("tmux: option requires an argument -- L\nusage: tmux"));
    assert_eq!(cli(&args(&["-C"])).unwrap().control, 1);
    assert_eq!(cli(&args(&["-CC", "attach"])).unwrap().control, 2);
    let got = cli(&args(&["-c", "echo a", "-cecho b"])).unwrap();
    assert_eq!(got.shell_command.as_deref(), Some("echo b"));
    assert!(got.argv.is_empty());
    let err = cli(&args(&["-c", "true", "ls"])).unwrap_err();
    assert!(err.starts_with("usage: tmux [-2CDhlNuVv]"), "{err}");
    let got = cli(&args(&["-L", "x", "-f", "/dev/null", "ls"])).unwrap();
    assert_eq!(got.files, [PathBuf::from("/dev/null")]);
    let got = cli(&args(&["-f", "a.toml", "-fb.toml", "ls"])).unwrap();
    assert_eq!(
        got.files,
        [PathBuf::from("a.toml"), PathBuf::from("b.toml")]
    );
    assert_eq!(got.argv, args(&["list-sessions"]));
    assert_eq!(
        cli(&args(&["-f/a.toml"])).unwrap().files,
        [PathBuf::from("/a.toml")]
    );
}

/// B2b: `pipe-pane`'s command and a copy-mode pipe's are tmux formats, so
/// the short forms reach the native expander as names.
#[test]
fn pipe_commands_are_rewritten_as_formats() {
    let Ok(Command::PipePane(p)) = one(&["pipe-pane", "-o", "cat >>out.#I-#P"]) else {
        panic!("pipe-pane")
    };
    assert_eq!(
        p.command.as_deref(),
        Some("cat >>out.#{window_index}-#{pane_index}")
    );
    let words = |c: Command| match c {
        Command::SendKeys(k) => k
            .items
            .iter()
            .map(|i| match i {
                crate::cmd::SendKeysItem::Key(s) | crate::cmd::SendKeysItem::Literal(s) => {
                    s.clone()
                }
                crate::cmd::SendKeysItem::Hex(b) => b.to_string(),
            })
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    for (sent, want) in [
        (
            &["copy-pipe", "x #S"][..],
            &["copy-pipe", "x #{session_name}"][..],
        ),
        (
            &["copy-pipe", "-C", "x #S", "#S"],
            &["copy-pipe", "-C", "x #{session_name}", "#S"],
        ),
        (&["pipe", "--", "#S"], &["pipe", "--", "#{session_name}"]),
        (&["search-forward", "#S"], &["search-forward", "#S"]),
    ] {
        let mut argv = vec!["send-keys", "-X"];
        argv.extend_from_slice(sent);
        assert_eq!(words(one(&argv).unwrap()), want, "{sent:?}");
    }
    let plain = one(&["send-keys", "copy-pipe", "#S"]).unwrap();
    assert_eq!(words(plain), ["copy-pipe", "#S"], "no -X");
}
