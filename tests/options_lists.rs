//! TC.2b: list-valued options (tmux's array options), each case recorded
//! against tmux 3.7b on a private socket.

mod common;

use common::TestServer;

fn server() -> TestServer {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "s", "sleep", "30"])
        .expect("new-session");
    srv
}

fn err(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args)
        .expect_err("should fail")
        .trim_end()
        .to_string()
}

fn ends(e: &str, want: &str) {
    assert!(e.ends_with(want), "{e:?} should end with {want:?}");
}

#[test]
fn setting_index_1_leaves_index_0_at_its_default() {
    let srv = server();
    let before = srv.cmd(&["show", "-gv", "status-format[0]"]).unwrap();
    srv.cmd(&["set", "-g", "status-format[1]", "X"]).unwrap();
    assert_eq!(
        srv.cmd(&["show", "-gv", "status-format[0]"]).unwrap(),
        before
    );
    assert_eq!(
        srv.cmd(&["show", "-gv", "status-format[1]"]).unwrap(),
        "X\n"
    );
    assert_eq!(
        srv.cmd(&["show", "-g", "status-format"]).unwrap(),
        "status-format[0] ''\nstatus-format[1] X\n"
    );
}

#[test]
fn append_fills_the_first_free_index() {
    let srv = server();
    assert_eq!(
        srv.cmd(&["show", "-g", "command-alias"]).unwrap(),
        "command-alias\n"
    );
    srv.cmd(&["set", "-ag", "command-alias", "a=list-sessions"])
        .unwrap();
    srv.cmd(&["set", "-ag", "command-alias", "b=list-windows"])
        .unwrap();
    assert_eq!(
        srv.cmd(&["show", "-g", "command-alias"]).unwrap(),
        "command-alias[0] a=list-sessions\ncommand-alias[1] b=list-windows\n"
    );
    srv.cmd(&["set", "-ag", "command-alias", "c=x"]).unwrap();
    assert_eq!(
        srv.cmd(&["show", "-gv", "command-alias"]).unwrap(),
        "a=list-sessions\nb=list-windows\nc=x\n"
    );
    // As tmux: an append refills a hole before growing the list, and an
    // append at an index appends to that element's text.
    srv.cmd(&["set", "-gu", "command-alias[0]"]).unwrap();
    srv.cmd(&["set", "-ag", "command-alias", "d=y"]).unwrap();
    srv.cmd(&["set", "-ag", "command-alias[2]", "z"]).unwrap();
    assert_eq!(
        srv.cmd(&["show", "-g", "command-alias"]).unwrap(),
        "command-alias[0] d=y\ncommand-alias[1] b=list-windows\ncommand-alias[2] c=xz\n"
    );
}

#[test]
fn unset_one_index_leaves_a_hole_and_unset_all_restores_the_default() {
    let srv = server();
    for (i, v) in ["a=b", "c=d", "e=f"].iter().enumerate() {
        let name = format!("command-alias[{i}]");
        srv.cmd(&["set", "-g", &name, v]).unwrap();
    }
    srv.cmd(&["set", "-gu", "command-alias[1]"]).unwrap();
    assert_eq!(
        srv.cmd(&["show", "-g", "command-alias"]).unwrap(),
        "command-alias[0] a=b\ncommand-alias[2] e=f\n"
    );
    assert_eq!(
        srv.cmd(&["show", "-g", "command-alias[1]"]).unwrap(),
        "command-alias[1] ''\n"
    );
    assert_eq!(srv.cmd(&["show", "-gv", "command-alias[1]"]).unwrap(), "\n");
    // A whole-list set replaces every element.
    srv.cmd(&["set", "-g", "command-alias", "only=this"])
        .unwrap();
    assert_eq!(
        srv.cmd(&["show", "-g", "command-alias"]).unwrap(),
        "command-alias[0] only=this\n"
    );
    srv.cmd(&["set", "-gu", "command-alias"]).unwrap();
    assert_eq!(
        srv.cmd(&["show", "-g", "command-alias"]).unwrap(),
        "command-alias\n"
    );
}

#[test]
fn a_session_list_holds_only_what_the_session_set() {
    let srv = server();
    srv.cmd(&["set", "-g", "status-format[1]", "global"])
        .unwrap();
    assert_eq!(srv.cmd(&["show", "status-format"]).unwrap(), "");
    srv.cmd(&["set", "status-format[1]", "sess"]).unwrap();
    assert_eq!(
        srv.cmd(&["show", "status-format"]).unwrap(),
        "status-format[1] sess\n"
    );
    assert_eq!(
        srv.cmd(&["show", "-g", "status-format[1]"]).unwrap(),
        "status-format[1] global\n"
    );
    // As tmux: removing the last element keeps an empty list at that scope.
    srv.cmd(&["set", "-u", "status-format[1]"]).unwrap();
    assert_eq!(
        srv.cmd(&["show", "status-format"]).unwrap(),
        "status-format\n"
    );
    srv.cmd(&["set", "-u", "status-format"]).unwrap();
    assert_eq!(srv.cmd(&["show", "status-format"]).unwrap(), "");
}

#[test]
fn show_all_lists_every_element_in_name_order() {
    let srv = server();
    srv.cmd(&["set", "-g", "status-format[1]", "one"]).unwrap();
    let all = srv.cmd(&["show", "-g"]).unwrap();
    let lines: Vec<&str> = all.lines().collect();
    let at = |l: &str| lines.iter().position(|x| *x == l).unwrap_or(usize::MAX);
    assert!(
        at("status-format[0] ''") < at("status-format[1] one"),
        "{all}"
    );
    assert!(
        at("status-format[1] one") < at("status-interval 15"),
        "{all}"
    );
    assert!(lines.contains(&"command-alias"), "{all}");
}

#[test]
fn bad_indices_are_refused_with_tmuxs_errors() {
    let srv = server();
    for name in [
        "status-format[-1]",
        "status-format[abc]",
        "status-format[]",
        "status-format[1",
        "status-format[1]x",
        "status-format[1a]",
        "status-format[2147483648]",
        "nosuch[-1]",
        "[1]",
    ] {
        ends(
            &err(&srv, &["set", "-g", name, "x"]),
            &format!("ambiguous option: {name}"),
        );
    }
    ends(
        &err(&srv, &["show", "-g", "status-format[-1]"]),
        "ambiguous option: status-format[-1]",
    );
    ends(
        &err(&srv, &["set", "-gu", "status-format[-1]"]),
        "ambiguous option: status-format[-1]",
    );
    // sscanf's `%u`, as tmux reads it: leading zeros, a plus sign and a
    // blank are fine; digits past 2^32 wrap.
    srv.cmd(&["set", "-g", "status-format[+3]", "p3"]).unwrap();
    srv.cmd(&["set", "-g", "status-format[04]", "z4"]).unwrap();
    srv.cmd(&["set", "-g", "status-format[4294967296]", "wrapped"])
        .unwrap();
    assert_eq!(
        srv.cmd(&["show", "-gv", "status-format"]).unwrap(),
        "wrapped\np3\nz4\n"
    );
    // A large index is only a key, not a length.
    srv.cmd(&["set", "-g", "status-format[2147483647]", "far"])
        .unwrap();
    assert_eq!(
        srv.cmd(&["show", "-g", "status-format[2147483647]"])
            .unwrap(),
        "status-format[2147483647] far\n"
    );
}

#[test]
fn indices_on_scalar_and_unknown_options() {
    let srv = server();
    ends(
        &err(&srv, &["set", "-g", "status-left[0]", "x"]),
        "not an array: status-left[0]",
    );
    ends(
        &err(&srv, &["set", "-gu", "status-left[0]"]),
        "not an array: status-left[0]",
    );
    ends(
        &err(&srv, &["set", "-g", "@u[0]", "x"]),
        "not an array: @u[0]",
    );
    ends(
        &err(&srv, &["set", "-g", "nosuch[0]", "x"]),
        "invalid option: nosuch[0]",
    );
    ends(
        &err(&srv, &["set", "-gu", "nosuch[1]"]),
        "invalid option: nosuch[1]",
    );
    ends(
        &err(&srv, &["show", "-g", "nosuch[0]"]),
        "invalid option: nosuch[0]",
    );
    ends(
        &err(&srv, &["show", "-g", "@u[0]"]),
        "invalid option: @u[0]",
    );
    // As tmux: showing a scalar at an index shows its value.
    assert_eq!(
        srv.cmd(&["show", "-g", "status-left[5]"]).unwrap(),
        "status-left[5] \"[#{session_name}] \"\n"
    );
    srv.cmd(&["set", "-g", "@u", "val"]).unwrap();
    assert_eq!(srv.cmd(&["show", "-g", "@u[3]"]).unwrap(), "@u[3] val\n");
    ends(
        &err(&srv, &["set", "-g", "status-format[1]"]),
        "empty value",
    );
    ends(&err(&srv, &["set", "-g", "command-alias"]), "empty value");
}

/// M1's errors for an unknown option are unchanged.
#[test]
fn an_unknown_option_still_errors_as_in_m1() {
    let srv = server();
    ends(
        &err(&srv, &["set", "-g", "no-such", "1"]),
        "invalid option: no-such",
    );
    ends(
        &err(&srv, &["show", "-g", "no-such"]),
        "invalid option: no-such",
    );
    ends(
        &err(&srv, &["set", "-gu", "no-such"]),
        "invalid option: no-such",
    );
}
