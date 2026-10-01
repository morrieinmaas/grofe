//! TC.2: native parsing of the M3.6 verbs and of the new flags on existing
//! verbs, in grofe's own grammar (spec §4.5). tmux-only forms stay unknown
//! natively (FE.1's alias table and `=name` targets translate them), and
//! the two tokenizer divergences D17 and D24 match tmux 3.7b, recorded on
//! `tmux -L grofe-tc2 -f /dev/null`.

mod common;

use common::TestServer;
use grofe::cmd::target::parse_target;
use grofe::cmd::*;
use grofe::types::{Edge, SplitSize};

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

fn p(words: &[&str]) -> Result<Command, ParseError> {
    parse(&argv(words))
}

fn t(s: &str) -> Target {
    parse_target(s).expect("target")
}

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

fn err(words: &[&str]) -> String {
    match p(words) {
        Ok(c) => panic!("{words:?} parsed: {c:?}"),
        Err(e) => e.to_string(),
    }
}

/// Every M3.6 verb parsed with every flag its §4.5 row names, in the
/// combinations `docs/compat-coverage.md`'s Forms table shows the corpus
/// sending.
#[test]
fn every_new_verb_parses_with_its_rows_flags() {
    let cases: Vec<(Vec<&str>, Command)> = vec![
        (
            vec![
                "join-pane",
                "-b",
                "-d",
                "-f",
                "-h",
                "-l",
                "26",
                "-s",
                "%1",
                "-t",
                "%2",
            ],
            Command::JoinPane(JoinPane {
                src: t("%1"),
                dst: t("%2"),
                before: true,
                detach: true,
                full: true,
                horizontal: true,
                vertical: false,
                size: Some(SplitSize::Cells(26)),
            }),
        ),
        (
            vec!["join-pane", "-v", "-l", "30%", "-s", "%3"],
            Command::JoinPane(JoinPane {
                src: t("%3"),
                dst: Target::Current,
                before: false,
                detach: false,
                full: false,
                horizontal: false,
                vertical: true,
                size: Some(SplitSize::Percent(30)),
            }),
        ),
        (
            vec!["move-pane", "-s", "work:2.1"],
            Command::MovePane(JoinPane {
                src: t("work:2.1"),
                dst: Target::Current,
                before: false,
                detach: false,
                full: false,
                horizontal: false,
                vertical: false,
                size: None,
            }),
        ),
        (
            vec!["swap-pane", "-D"],
            Command::SwapPane(SwapPane {
                src: None,
                dst: Target::Current,
                up: false,
                down: true,
            }),
        ),
        (
            vec!["swap-pane", "-U", "-t", "%4"],
            Command::SwapPane(SwapPane {
                src: None,
                dst: t("%4"),
                up: true,
                down: false,
            }),
        ),
        (
            vec!["swap-pane", "-s", "%1", "-t", "%2"],
            Command::SwapPane(SwapPane {
                src: Some(t("%1")),
                dst: t("%2"),
                up: false,
                down: false,
            }),
        ),
        (
            vec!["swap-window", "-s", "work:1", "-t", "work:3"],
            Command::SwapWindow(SwapWindow {
                src: Some(t("work:1")),
                dst: t("work:3"),
            }),
        ),
        (
            vec!["break-pane", "-d", "-n", "logs", "-s", "%5", "-t", "work:"],
            Command::BreakPane(BreakPane {
                src: t("%5"),
                dst: Some(t("work:")),
                detach: true,
                name: s("logs"),
            }),
        ),
        (
            vec!["break-pane"],
            Command::BreakPane(BreakPane {
                src: Target::Current,
                dst: None,
                detach: false,
                name: None,
            }),
        ),
        (
            vec!["resize-window", "-t", "@1", "-x", "200", "-y", "50"],
            Command::ResizeWindow(ResizeWindow {
                target: t("@1"),
                width: Some(200),
                height: Some(50),
                largest: false,
                smallest: false,
            }),
        ),
        (
            vec!["resize-window", "-A"],
            Command::ResizeWindow(ResizeWindow {
                target: Target::Current,
                width: None,
                height: None,
                largest: true,
                smallest: false,
            }),
        ),
        (
            vec!["resize-window", "-a"],
            Command::ResizeWindow(ResizeWindow {
                target: Target::Current,
                width: None,
                height: None,
                largest: false,
                smallest: true,
            }),
        ),
        (
            vec!["last-pane", "-t", "@2"],
            Command::LastPane(LastPane { target: t("@2") }),
        ),
        (
            vec!["rotate-window", "-D"],
            Command::RotateWindow(RotateWindow {
                target: Target::Current,
                up: false,
                down: true,
            }),
        ),
        (
            vec!["rotate-window", "-U", "-t", "@1"],
            Command::RotateWindow(RotateWindow {
                target: t("@1"),
                up: true,
                down: false,
            }),
        ),
        (
            vec!["move-window", "-a", "-s", "a:1", "-t", "b:2"],
            Command::MoveWindow(MoveWindow {
                src: t("a:1"),
                dst: Some(t("b:2")),
                after: true,
                renumber: false,
            }),
        ),
        (
            vec!["move-window", "-r", "-s", "a:", "-t", "a:"],
            Command::MoveWindow(MoveWindow {
                src: t("a:"),
                dst: Some(t("a:")),
                after: false,
                renumber: true,
            }),
        ),
        (
            vec!["next-layout", "-t", "@3"],
            Command::NextLayout(NextLayout { target: t("@3") }),
        ),
        (
            vec!["respawn-window", "-k", "-t", "@1", "htop"],
            Command::RespawnWindow(RespawnPane {
                kill: true,
                target: t("@1"),
                command: Some(argv(&["htop"])),
                cwd: None,
            }),
        ),
        (
            vec!["refresh-client", "-S"],
            Command::RefreshClient(RefreshClient {
                client: None,
                status: true,
                pan: None,
                adjustment: None,
            }),
        ),
        (
            vec!["refresh-client", "-t", "/dev/ttys001"],
            Command::RefreshClient(RefreshClient {
                client: s("/dev/ttys001"),
                status: false,
                pan: None,
                adjustment: None,
            }),
        ),
        (
            vec!["refresh-client", "-R", "-L", "10"],
            Command::RefreshClient(RefreshClient {
                client: None,
                status: false,
                pan: Some(Pan::Left),
                adjustment: s("10"),
            }),
        ),
        (
            vec!["show-messages", "-J", "-T"],
            Command::ShowMessages(ShowMessages {
                client: None,
                jobs: true,
                terminals: true,
            }),
        ),
        (
            vec!["choose-client", "-Z"],
            Command::ChooseClient(ChooseClient { zoom: true }),
        ),
        (
            vec!["suspend-client", "-t", "c1"],
            Command::SuspendClient(SuspendClient { client: s("c1") }),
        ),
        (
            vec!["find-window", "-Z", "foo"],
            Command::FindWindow(FindWindow {
                target: Target::Current,
                zoom: true,
                pattern: s("foo"),
            }),
        ),
        (
            vec!["choose-tree", "-Zw"],
            Command::ChooseTree(ChooseTree {
                zoom: true,
                sessions: false,
                windows: true,
            }),
        ),
        (
            vec!["wait-for", "fzf-1"],
            Command::WaitFor(WaitFor {
                channel: s("fzf-1"),
                signal: false,
            }),
        ),
        (
            vec!["wait-for", "-S", "fzf-1"],
            Command::WaitFor(WaitFor {
                channel: s("fzf-1"),
                signal: true,
            }),
        ),
        (
            vec![
                "display-popup",
                "-E",
                "-b",
                "rounded",
                "-T",
                " network ",
                "-w",
                "66",
                "-h",
                "26",
                "-d",
                "#{pane_current_path}",
                "-c",
                "/dev/ttys002",
                "-e",
                "A=1",
                "-e",
                "NOEQ",
                "-eB=2",
                "net.sh",
                "#{mouse_status_range}",
            ],
            Command::DisplayPopup(DisplayPopup {
                close_on_exit: true,
                no_border: false,
                border: s("rounded"),
                client: s("/dev/ttys002"),
                cwd: s("#{pane_current_path}"),
                env: vec![("A".into(), "1".into()), ("B".into(), "2".into())],
                title: s(" network "),
                width: Some(SplitSize::Cells(66)),
                height: Some(SplitSize::Cells(26)),
                x: None,
                y: None,
                command: Some(argv(&["net.sh", "#{mouse_status_range}"])),
            }),
        ),
        (
            vec![
                "display-popup",
                "-E",
                "-d",
                "/tmp",
                "-B",
                "-x",
                "C",
                "-y",
                "C",
                "-w",
                "70%",
                "-h",
                "68%",
                "sh",
                "/tmp/fzf",
            ],
            Command::DisplayPopup(DisplayPopup {
                close_on_exit: true,
                no_border: true,
                cwd: s("/tmp"),
                width: Some(SplitSize::Percent(70)),
                height: Some(SplitSize::Percent(68)),
                x: s("C"),
                y: s("C"),
                command: Some(argv(&["sh", "/tmp/fzf"])),
                ..Default::default()
            }),
        ),
        (
            vec![
                "new-pane",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                "%1",
                "-c",
                "/tmp",
                "-x",
                "138",
                "-y",
                "30",
                "-X",
                "31",
                "-Y",
                "9",
                "sh",
                "-c",
                "fzf",
            ],
            Command::NewPane(NewPane {
                print: true,
                format: s("#{pane_id}"),
                target: t("%1"),
                cwd: s("/tmp"),
                width: Some(SplitSize::Cells(138)),
                height: Some(SplitSize::Cells(30)),
                x: Some(Offset::Cells(31)),
                y: Some(Offset::Cells(9)),
                command: Some(argv(&["sh", "-c", "fzf"])),
            }),
        ),
        (
            vec!["new-pane"],
            Command::NewPane(NewPane {
                print: false,
                format: None,
                target: Target::Current,
                cwd: None,
                width: None,
                height: None,
                x: None,
                y: None,
                command: None,
            }),
        ),
        (
            vec![
                "display-menu",
                "-T",
                "#[align=centre]menu",
                "-x",
                "R",
                "-y",
                "P",
                "Split",
                "s",
                "split-window",
                "",
                "-Disabled",
                "d",
                "",
                "More",
                "m",
                "display-menu -T sub Kill k kill-pane",
            ],
            Command::DisplayMenu(DisplayMenu {
                title: s("#[align=centre]menu"),
                x: s("R"),
                y: s("P"),
                items: vec![
                    MenuEntry::Item {
                        name: "Split".to_string(),
                        key: "s".to_string(),
                        command: "split-window".to_string(),
                    },
                    MenuEntry::Separator,
                    MenuEntry::Item {
                        name: "-Disabled".to_string(),
                        key: "d".to_string(),
                        command: String::new(),
                    },
                    MenuEntry::Item {
                        name: "More".to_string(),
                        key: "m".to_string(),
                        command: "display-menu -T sub Kill k kill-pane".to_string(),
                    },
                ],
            }),
        ),
        (
            vec!["display-panes"],
            Command::DisplayPanes(DisplayPanes {}),
        ),
        (
            vec!["confirm-before", "-p", "kill-pane #P? (y/n)", "kill-pane"],
            Command::ConfirmBefore(ConfirmBefore {
                prompt: s("kill-pane #P? (y/n)"),
                command: s("kill-pane"),
            }),
        ),
        (
            vec![
                "if-shell",
                "-F",
                "-t",
                "%1",
                "#{window_zoomed_flag}",
                "resize-pane -Z -t %1",
            ],
            Command::If(If {
                condition: Some(Condition::Format("#{window_zoomed_flag}".to_string())),
                then: s("resize-pane -Z -t %1"),
                otherwise: None,
                background: false,
                target: t("%1"),
            }),
        ),
        (
            vec![
                "if-shell",
                "-b",
                "test -f x",
                "send-keys C-h",
                "select-pane -L",
            ],
            Command::If(If {
                condition: Some(Condition::Shell("test -f x".to_string())),
                then: s("send-keys C-h"),
                otherwise: s("select-pane -L"),
                background: true,
                target: Target::Current,
            }),
        ),
        (
            vec!["set-hook", "-g", "pane-focused", "run-shell -b 'true'"],
            Command::SetHook(SetHook {
                global: true,
                unset: false,
                hook: s("pane-focused"),
                command: s("run-shell -b 'true'"),
            }),
        ),
        (
            vec!["set-hook", "-gu", "pane-focused"],
            Command::SetHook(SetHook {
                global: true,
                unset: true,
                hook: s("pane-focused"),
                command: None,
            }),
        ),
        (
            vec!["show-hooks", "-g"],
            Command::ShowHooks(ShowHooks {
                global: true,
                window: false,
                hook: None,
            }),
        ),
        (
            vec!["set-environment", "-g", "OPENSESSIONS_DIR", "/x"],
            Command::SetEnvironment(SetEnvironment {
                global: true,
                unset: false,
                hidden: false,
                target: Target::Current,
                name: s("OPENSESSIONS_DIR"),
                value: s("/x"),
            }),
        ),
        (
            vec!["set-environment", "-gu", "OPENSESSIONS_WIDTH"],
            Command::SetEnvironment(SetEnvironment {
                global: true,
                unset: true,
                hidden: false,
                target: Target::Current,
                name: s("OPENSESSIONS_WIDTH"),
                value: None,
            }),
        ),
        (
            vec!["set-environment", "-h", "-t", "work", "SECRET", "1"],
            Command::SetEnvironment(SetEnvironment {
                global: false,
                unset: false,
                hidden: true,
                target: t("work"),
                name: s("SECRET"),
                value: s("1"),
            }),
        ),
        (
            vec!["show-environment", "-g", "-h", "SECRET"],
            Command::ShowEnvironment(ShowEnvironment {
                global: true,
                hidden: true,
                target: Target::Current,
                name: s("SECRET"),
            }),
        ),
        (
            vec!["show-environment", "-t", "work"],
            Command::ShowEnvironment(ShowEnvironment {
                global: false,
                hidden: false,
                target: t("work"),
                name: None,
            }),
        ),
    ];
    for (words, want) in cases {
        assert_eq!(p(&words), Ok(want), "{words:?}");
    }
}

/// New flags on verbs M1 to M3 already had (spec §4.5's M3.6 rows).
#[test]
fn existing_verbs_parse_their_new_flags() {
    let Ok(Command::NewSession(c)) = p(&["new-session", "-A", "-d", "-s", "x"]) else {
        panic!("new-session -A");
    };
    assert!(c.attach && c.detached);
    assert_eq!(c.name.as_deref(), Some("x"));

    let Ok(Command::SwitchClient(c)) = p(&["switch-client", "-c", "/dev/ttys003", "-t", "s"])
    else {
        panic!("switch-client -c");
    };
    assert_eq!(c.client.as_deref(), Some("/dev/ttys003"));
    assert_eq!(c.target, Some(t("s")));

    // `resize-pane -x/-y` (TC.8): a size alone, or with a direction.
    for (words, width, height, edges) in [
        (
            &["resize-pane", "-t", "%1", "-x", "26"][..],
            Some(SplitSize::Cells(26)),
            None,
            vec![],
        ),
        (
            &["resize-pane", "-y", "50%"][..],
            None,
            Some(SplitSize::Percent(50)),
            vec![],
        ),
        (
            &["resize-pane", "-L", "-x", "10"][..],
            Some(SplitSize::Cells(10)),
            None,
            vec![Edge::Left],
        ),
    ] {
        let Ok(Command::ResizePane(c)) = p(words) else {
            panic!("{words:?}");
        };
        assert_eq!((c.width, c.height), (width, height), "{words:?}");
        assert_eq!(c.action, ResizeAction::Edges(edges, 1), "{words:?}");
    }

    // TC.8's corpus flags.
    let Ok(Command::SplitWindow(c)) = p(&[
        "split-window",
        "-b",
        "-f",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-h",
        "-l",
        "30",
    ]) else {
        panic!("split-window -b -f -d -P -F");
    };
    assert!(c.before && c.full && c.detached && c.print);
    assert_eq!(c.format.as_deref(), Some("#{pane_id}"));
    let Ok(Command::NewWindow(c)) = p(&["new-window", "-a", "-P", "-F", "#{window_id}"]) else {
        panic!("new-window -a -P -F");
    };
    assert!(c.after && c.print);
    assert_eq!(c.format.as_deref(), Some("#{window_id}"));
    let Ok(Command::SelectPane(c)) = p(&["select-pane", "-l", "-Z"]) else {
        panic!("select-pane -l -Z");
    };
    assert!(c.last && c.keep_zoom);
    let Ok(Command::RespawnPane(c)) = p(&["respawn-pane", "-c", "/tmp", "-k"]) else {
        panic!("respawn-pane -c");
    };
    assert_eq!(c.cwd.as_deref(), Some("/tmp"));
    assert!(c.kill);

    let Ok(Command::SelectLayout(c)) = p(&["select-layout", "-E"]) else {
        panic!("select-layout -E");
    };
    assert!(c.spread);
    assert_eq!(c.name, "");

    for verb in ["next-window", "previous-window"] {
        let parsed = p(&[verb, "-a"]);
        let (Ok(Command::NextWindow(c)) | Ok(Command::PreviousWindow(c))) = parsed else {
            panic!("{verb} -a: {parsed:?}");
        };
        assert!(c.alert, "{verb}");
    }
    assert_eq!(
        err(&["last-window", "-a"]),
        "command last-window: unknown flag -a"
    );

    let Ok(Command::ListSessions(c)) = p(&["list-sessions", "-f", "#{==:#{session_name},a}"])
    else {
        panic!("list-sessions -f");
    };
    assert_eq!(c.filter.as_deref(), Some("#{==:#{session_name},a}"));
    let Ok(Command::ListWindows(c)) = p(&["list-windows", "-a", "-F", "#{window_id}"]) else {
        panic!("list-windows -a");
    };
    assert!(c.all);
    let Ok(Command::ListPanes(c)) = p(&["list-panes", "-a", "-f", "#{pane_active}", "-F", "x"])
    else {
        panic!("list-panes -a -f");
    };
    assert!(c.all && !c.session);
    assert_eq!(c.filter.as_deref(), Some("#{pane_active}"));
    let Ok(Command::ListPanes(c)) = p(&["list-panes", "-s", "-t", "work"]) else {
        panic!("list-panes -s");
    };
    assert!(c.session);

    let Ok(Command::ShowOptions(c)) = p(&["show-options", "-gqv", "@x"]) else {
        panic!("show -gqv");
    };
    assert!(c.value_only && c.quiet);
    assert_eq!(c.scope, Some(OptionScope::Global));

    let Ok(Command::SetOption(c)) = p(&["set-option", "-agFqo", "@x", "#{pane_id}"]) else {
        panic!("set -agFqo");
    };
    assert!(c.append && c.expand && c.quiet && c.only_if_unset);
    assert_eq!(c.scope, Some(OptionScope::Global));

    let Ok(Command::CapturePane(c)) = p(&["capture-pane", "-e", "-J", "-p", "-S", "-10"]) else {
        panic!("capture-pane -J");
    };
    assert!(c.join && c.print && c.opts.escapes);

    let Ok(Command::RunShell(c)) = p(&["run-shell", "-b", "sleep 5"]) else {
        panic!("run-shell -b");
    };
    assert!(c.background);
    assert_eq!(c.cmd, "sleep 5");

    let Ok(Command::ListKeys(c)) = p(&["list-keys", "-1N", "-T", "prefix", "C-o"]) else {
        panic!("list-keys -1N");
    };
    assert!(c.first && c.notes);
    assert_eq!(c.key.as_deref(), Some("C-o"));
    assert_eq!(c.table.as_deref(), Some("prefix"));
}

/// D21: `command-prompt`'s background flag, and TC.7's `-k -1 -T`.
#[test]
fn command_prompt_parses_background_and_the_prompt_flags() {
    let Ok(Command::CommandPrompt(c)) = p(&["command-prompt", "-b", "-p", "x", "display %%"])
    else {
        panic!("command-prompt -b");
    };
    assert!(c.background);
    assert_eq!(c.template.as_deref(), Some("display %%"));
    let Ok(Command::CommandPrompt(c)) = p(&["command-prompt", "-k", "-p", "key", "list-keys"])
    else {
        panic!("command-prompt -k");
    };
    assert!(c.key && !c.background);
    let Ok(Command::CommandPrompt(c)) = p(&[
        "command-prompt",
        "-T",
        "window-target",
        "-p",
        "index",
        "select-window -t ':%%'",
    ]) else {
        panic!("command-prompt -T");
    };
    assert_eq!(c.prompt_type.as_deref(), Some("window-target"));
    let Ok(Command::CommandPrompt(c)) = p(&["command-prompt", "-1", "-p", "(jump)", "x"]) else {
        panic!("command-prompt -1");
    };
    assert!(c.single);
}

/// `new-pane` takes what tmux 3.7b takes: sizes in percent, and signed
/// or percent positions (`-x 50% -y 50%`, `-X -1`, `-X 50%`).
#[test]
fn new_pane_takes_percent_sizes_and_signed_positions() {
    let Ok(Command::NewPane(c)) = p(&[
        "new-pane", "-x", "50%", "-y", "50%", "-X", "-1", "-Y", "25%",
    ]) else {
        panic!("new-pane percent");
    };
    assert_eq!(c.width, Some(SplitSize::Percent(50)));
    assert_eq!(c.height, Some(SplitSize::Percent(50)));
    assert_eq!(c.x, Some(Offset::Cells(-1)));
    assert_eq!(c.y, Some(Offset::Percent(25)));
    let Ok(Command::NewPane(c)) = p(&["new-pane", "-x", "0", "-X", "-99999"]) else {
        panic!("new-pane zero");
    };
    assert_eq!(c.width, Some(SplitSize::Cells(0)));
    assert_eq!(c.x, Some(Offset::Cells(-99999)));
}

/// Values the flags above reject, each text recorded from tmux 3.7b on
/// `tmux -L grofe-tc2 -f /dev/null`.
#[test]
fn bad_values_and_argument_counts_are_errors() {
    for (words, want) in [
        (
            &["join-pane", "-l", "abc"][..],
            "size or position invalid tiled geometry",
        ),
        (
            &["join-pane", "-l", "200%"][..],
            "size or position invalid tiled geometry",
        ),
        (
            &["join-pane", "-l", "-3"][..],
            "size or position invalid tiled geometry",
        ),
        (&["resize-pane", "-x", "abc"][..], "width invalid"),
        (&["resize-pane", "-x", "200%"][..], "width too large"),
        (&["resize-window", "-x", "abc"][..], "width invalid"),
        (&["display-popup", "-w", "5x"][..], "width invalid"),
        (&["display-popup", "-w", "0"][..], "width too small"),
        (&["display-popup", "-w", "-1%"][..], "width too small"),
        (&["display-popup", "-h", "200%"][..], "height too large"),
        (&["display-popup", "-h", "101%"][..], "height too large"),
        (&["new-pane", "-x", "abc"][..], "size or position invalid"),
        (&["new-pane", "-X", "abc"][..], "size or position invalid"),
        (&["new-pane", "-x", "-5"][..], "size or position too small"),
        (
            &["new-pane", "-x", "101%"][..],
            "size or position too large",
        ),
    ] {
        assert_eq!(err(words), want, "{words:?}");
    }
    assert_eq!(
        err(&["if-shell", "-F", "1", "a", "b", "c"]),
        "command if-shell: too many arguments (need at most 3)"
    );
    assert_eq!(
        err(&["set-environment", "-g", "A", "b", "c"]),
        "command set-environment: too many arguments (need at most 2)"
    );
    assert_eq!(err(&["display-menu", "a", "b"]), "not enough arguments");
    assert_eq!(
        err(&["wait-for", "a", "b"]),
        "command wait-for: too many arguments (need at most 1)"
    );
    // Given any argument, a required positional must be there (tmux 3.7b).
    for (words, want) in [
        (
            &["wait-for", "-S"][..],
            "command wait-for: too few arguments (need at least 1)",
        ),
        (
            &["set-hook", "-g"][..],
            "command set-hook: too few arguments (need at least 1)",
        ),
        (
            &["if-shell", "true"][..],
            "command if-shell: too few arguments (need at least 2)",
        ),
        (
            &["find-window", "-Z"][..],
            "command find-window: too few arguments (need at least 1)",
        ),
        (
            &["confirm-before", "-p", "x"][..],
            "command confirm-before: too few arguments (need at least 1)",
        ),
        (
            &["set-environment", "-g"][..],
            "command set-environment: too few arguments (need at least 1)",
        ),
        (
            &["display-menu", "-T", "t"][..],
            "command display-menu: too few arguments (need at least 1)",
        ),
    ] {
        assert_eq!(err(words), want, "{words:?}");
    }
    // Flags no §4.5 row names stay unknown natively.
    assert_eq!(
        err(&["wait-for", "-L", "a"]),
        "command wait-for: unknown flag -L"
    );
    assert_eq!(
        err(&["refresh-client", "-A", "%1:on"]),
        "command refresh-client: unknown flag -A"
    );
}

/// A bare verb whose tmux form needs a positional still parses (its
/// handler refuses it), so the stub-phase dispatch tests keep passing.
#[test]
fn a_bare_verb_parses_with_its_positional_missing() {
    assert_eq!(
        p(&["wait-for"]),
        Ok(Command::WaitFor(WaitFor {
            channel: None,
            signal: false
        }))
    );
    assert_eq!(p(&["set-hook"]), Ok(Command::SetHook(SetHook::default())));
    assert_eq!(
        p(&["confirm-before"]),
        Ok(Command::ConfirmBefore(ConfirmBefore::default()))
    );
    assert_eq!(
        p(&["display-menu"]),
        Ok(Command::DisplayMenu(DisplayMenu::default()))
    );
    let Ok(Command::If(c)) = p(&["if-shell"]) else {
        panic!("bare if-shell");
    };
    assert_eq!((c.condition, c.then), (None, None));
    let Ok(Command::FindWindow(c)) = p(&["find-window"]) else {
        panic!("bare find-window");
    };
    assert_eq!(c.pattern, None);
    let Ok(Command::SetEnvironment(c)) = p(&["set-environment"]) else {
        panic!("bare set-environment");
    };
    assert_eq!(c.name, None);
    assert_eq!(
        ParseError::TooFewArguments("wait-for".to_string(), 1).to_string(),
        "command wait-for: too few arguments (need at least 1)"
    );
}

/// tmux-only forms M1 lacks are unknown natively; FE.1's alias table and
/// prefix rule resolve them in the tmux dialect only.
#[test]
fn tmux_only_names_are_unknown_natively() {
    for name in [
        "ls",
        "setw",
        "showw",
        "a",
        "at",
        "if",
        "setenv",
        "showenv",
        "joinp",
        "swapp",
        "breakp",
        "popup",
        "menu",
        "movew",
        "rotatew",
        "lastp",
        "nextl",
        "newp",
        "wait",
        "confirm",
        "displayp",
        "refresh",
        "showmsgs",
        "findw",
        "resizew",
        "respawnw",
        "swapw",
        "movep",
        "info",
        "set-window-option",
        "show-window-options",
        "list-commands",
    ] {
        assert_eq!(
            p(&[name]),
            Err(ParseError::UnknownCommand(name.to_string())),
            "{name}"
        );
    }
}

/// Natively `=x` is the literal word `=x`, looked up as any bare word is
/// (a pane target: a window, then a session); `=name` as an exact match is
/// FE.1's front-end form.
#[test]
fn equals_targets_are_literal_session_names() {
    for (given, name) in [("=", "="), ("=foo", "=foo")] {
        assert_eq!(
            p(&["select-pane", "-t", given]),
            Ok(Command::SelectPane(SelectPane {
                edge: None,
                target: Target::Session(SessionRef::Bare(name.to_string())),
                last: false,
                keep_zoom: false,
                title: None,
            })),
            "{given}"
        );
    }
}

#[test]
fn equals_targets_find_no_session_at_run_time() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "x", "sleep", "30"])
        .expect("new-session");
    let e = srv.cmd(&["select-pane", "-t", "=x"]).unwrap_err();
    assert_eq!(e.trim_end(), "can't find pane: =x");
    let e = srv.cmd(&["select-pane", "-t", "="]).unwrap_err();
    assert_eq!(e.trim_end(), "can't find pane: =");
}

/// D17: a bare `$` in a stored command string, as tmux 3.7b's lexer reads
/// it: at the end of the string it is a syntax error, before anything that
/// can't start a name it is a literal `$`; `${` must close over a name.
#[test]
fn bare_dollar_follows_tmux() {
    let errors: &[(&str, &str)] = &[
        ("split-window $", "syntax error"),
        ("display a$", "syntax error"),
        ("display $x$", "syntax error"),
        ("display $$", "syntax error"),
        ("display $;", "syntax error"),
        ("display ${", "invalid environment variable"),
        ("display ${x", "invalid environment variable"),
        ("display ${x-y}", "invalid environment variable"),
        ("display \"${\"", "invalid environment variable"),
    ];
    for (input, want) in errors {
        let got = split_command_string(input).map_err(|e| e.to_string());
        assert_eq!(got, Err(want.to_string()), "{input:?}");
    }
    let ok: &[(&str, &[&[&str]])] = &[
        ("display $ ;", &[&["display", "$"]]),
        ("display $; ", &[&["display", "$"]]),
        ("display $;x", &[&["display", "$"], &["x"]]),
        ("display $1", &[&["display", "$1"]]),
        ("display $-", &[&["display", "$-"]]),
        ("display $.x", &[&["display", "$.x"]]),
        ("display \"a$\"", &[&["display", "a$"]]),
        ("display \"$$\"", &[&["display", "$$"]]),
        ("display '$'", &[&["display", "$"]]),
        ("display \\$", &[&["display", "$"]]),
        ("display ${}", &[&["display", ""]]),
        ("display ${GROFE_TC2_UNSET}y", &[&["display", "y"]]),
        ("display a$$GROFE_TC2_UNSET", &[&["display", "a$"]]),
    ];
    for (input, want) in ok {
        let want: Vec<Vec<String>> = want.iter().map(|c| argv(c)).collect();
        assert_eq!(split_command_string(input), Ok(want), "{input:?}");
    }
}

/// D24: a stored `a \; b` is one command whose arguments include a
/// literal `;`, never two commands; tmux 3.7b then fails `a` with too many
/// arguments.
#[test]
fn a_stored_escaped_semicolon_is_an_argument_not_a_separator() {
    let cmds = split_command_string("select-pane \\; kill-pane").expect("split");
    assert_eq!(cmds, vec![argv(&["select-pane", ";", "kill-pane"])]);
    assert_eq!(
        parse(&cmds[0]).map_err(|e| e.to_string()),
        Err("command select-pane: too many arguments (need at most 0)".to_string())
    );
    let cmds = split_command_string("kill-server \\; list-sessions").expect("split");
    assert_eq!(
        parse(&cmds[0]).map_err(|e| e.to_string()),
        Err("command kill-server: too many arguments (need at most 0)".to_string())
    );
}

#[test]
fn binding_a_stored_escaped_semicolon_fails_like_tmux() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "b", "sleep", "30"])
        .expect("new-session");
    let e = srv
        .cmd(&["bind-key", "F5", "select-pane \\; kill-pane"])
        .unwrap_err();
    assert_eq!(
        e.trim_end(),
        "command select-pane: too many arguments (need at most 0)"
    );
    let e = srv.cmd(&["bind-key", "F5", "split-window $"]).unwrap_err();
    assert_eq!(e.trim_end(), "syntax error");
    // Nothing was bound.
    let keys = srv.cmd(&["list-keys", "-T", "prefix"]).expect("list-keys");
    assert!(!keys.contains("F5"), "{keys}");
}

/// M1's short names and `-g` scope merges stay native (T1.2, approved).
#[test]
fn m1_short_names_and_scope_merges_still_parse() {
    let short: &[(&str, &str, &[&str])] = &[
        ("new", "new-session", &["-d", "-s", "x"]),
        ("neww", "new-window", &["-n", "w"]),
        ("splitw", "split-window", &["-h"]),
        ("lsp", "list-panes", &["-a"]),
        ("lsw", "list-windows", &[]),
        ("send", "send-keys", &["Enter"]),
        ("display", "display-message", &["-p", "#{pane_id}"]),
        ("set", "set-option", &["-g", "@x", "1"]),
        ("show", "show-options", &["-gv", "@x"]),
        ("attach", "attach-session", &["-t", "x"]),
        ("selectp", "select-pane", &["-L"]),
        ("killp", "kill-pane", &[]),
    ];
    for (name, full, rest) in short {
        let a = p(&[&[*name], *rest].concat());
        let b = p(&[&[*full], *rest].concat());
        assert!(a.is_ok(), "{name}: {a:?}");
        assert_eq!(a, b, "{name}");
    }
    let Ok(Command::SetOption(c)) = p(&["set", "-gw", "automatic-rename", "on"]) else {
        panic!("set -gw");
    };
    assert_eq!(c.scope, Some(OptionScope::Global));
    let Ok(Command::ShowOptions(c)) = p(&["show", "-gw"]) else {
        panic!("show -gw");
    };
    assert_eq!(c.scope, Some(OptionScope::Global));
}
