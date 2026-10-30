//! B4 rest: the leftover gaps, against goldens recorded from tmux 3.7b
//! (`golden/b4rest/`).

use crate::tp::check;

/// A click, a drag or the wheel on a pane in clock mode ends it.
#[test]
fn the_mouse_ends_clock_mode() {
    check("b4rest", "clock-mouse", &[]);
}

/// `source-file -t`: the format and command context of the file.
#[test]
fn source_file_target_is_the_context() {
    check("b4rest", "source-file-target", &[]);
}

/// `source-file -` from a key, and from a CLI whose stdin is empty.
#[test]
fn source_file_dash_is_the_clients_stdin() {
    check("b4rest", "source-file-stdin", &[]);
}

/// The CLI's stdin is sourced: its commands, a `-v` trace, the path `-` in
/// an error and `-n` parsing only; a failing line refuses the file.
#[test]
fn source_file_dash_reads_what_the_cli_pipes() {
    use std::io::Write as _;
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let cli = crate::fe_1::grofe_cli(dir.path());
    let argv = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
    let pipe = |words: &[&str], text: &str| {
        let mut child = cli
            .command(&argv(words))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let show = |name: &str| cli.run(&argv(&["show-options", "-gv", name])).1;
    cli.run(&argv(&["new-session", "-d", "-s", "main", "cat"]));

    assert_eq!(
        pipe(&["source-file", "-"], "set -g @a one\nset -g @b two\n"),
        (Some(0), String::new(), String::new())
    );
    assert_eq!((show("@a").as_str(), show("@b").as_str()), ("one\n", "two\n"));
    let (code, out, _) = pipe(&["source-file", "-v", "-"], "set -g @c three\n");
    assert_eq!((code, out.as_str()), (Some(0), "-:1: set-option -g @c three\n"));
    assert_eq!(show("@c"), "three\n");
    let (code, out, _) = pipe(&["source-file", "-n", "-"], "set -g @d four\n");
    assert_eq!((code, out.as_str()), (Some(0), ""));
    assert_eq!(show("@d"), "");
    let (code, out, _) = pipe(&["source-file", "-"], "set -g @e five\nbogus\n");
    assert_eq!(code, Some(1));
    assert_eq!(out, "-:2: unknown command: bogus\n");
    assert_eq!(show("@e"), "");
    cli.run(&argv(&["kill-server"]));
}

/// The items of a key's `display-menu -t` run for that pane.
#[test]
fn menu_items_run_for_the_pane_the_menu_was_opened_for() {
    check("b4rest", "menu-target", &[]);
}

/// `display-popup -EE` closes the popup only after a zero exit.
#[test]
fn popup_ee_closes_on_a_zero_exit_only() {
    check("b4rest", "popup-ee", &[]);
}

/// A popup's drag, resize and menu.
#[test]
fn a_popup_takes_the_mouse() {
    check("b4rest", "popup-mouse", &[]);
}

/// `popup-style` and `popup-border-style` expand formats.
#[test]
fn popup_styles_are_formats() {
    check("b4rest", "popup-style-format", &[]);
}

/// `switch-client -Z`.
#[test]
fn switch_client_z_keeps_the_zoom() {
    check("b4rest", "switch-client-z", &[]);
}

/// The commands a key runs are logged for `show-messages`.
#[test]
fn show_messages_logs_a_keys_commands() {
    check("b4rest", "messages-key", &[]);
}

/// `capture-pane -p` run by a key shows in the pane's view mode.
#[test]
fn capture_pane_from_a_binding_shows_in_view_mode() {
    check("b4rest", "capture-binding", &[]);
}

/// `source-file -v` prints arguments as `args_escape` does.
#[test]
fn source_file_v_quotes_as_args_escape() {
    crate::tp::check_with("b4rest", "source-v-quoting", &[], |s| s.to_string(), crate::b3wpd::real_tmpdir);
}

/// `source-file -v` across a `run-shell` that holds the queue.
#[test]
fn source_file_v_goes_on_after_a_hold() {
    crate::tp::check_with("b4rest", "source-v-hold", &[], |s| s.to_string(), crate::b3wpd::real_tmpdir);
}

/// A burst of bells rings the terminal once.
#[test]
fn a_burst_of_bells_rings_once() {
    check("b4rest", "bell-burst", &[]);
}
