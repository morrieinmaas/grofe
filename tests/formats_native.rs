//! TC.3: grofe's own format language, expanded by a real server against a
//! scripted model (fixed session, window and pane names and commands).
//! Modifier semantics and the variables' string forms were checked
//! against tmux 3.7b on a private `-L` socket.

mod common;

use std::process::Command;
use std::time::Duration;

use common::TestServer;

const WAIT: Duration = Duration::from_secs(5);

/// A server with session `alpha` (window 1 runs `sleep 30`, pane
/// `alpha:1.1`) and a second window `alpha:2`, which is the current one,
/// so window 1 is the session's last window.
fn scripted() -> TestServer {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-x",
        "80",
        "-y",
        "24",
        "sleep",
        "30",
    ])
    .expect("new-session");
    srv.cmd(&["new-window", "-t", "alpha", "sleep", "31"])
        .expect("new-window");
    srv
}

fn display(srv: &TestServer, target: &str, fmt: &str) -> String {
    let out = srv
        .cmd(&["display-message", "-p", "-t", target, fmt])
        .unwrap_or_else(|e| panic!("display-message {fmt}: {e}"));
    out.strip_suffix('\n').unwrap_or(&out).to_string()
}

fn wait_display(srv: &TestServer, target: &str, fmt: &str, want: &str) {
    if common::wait_until(|| display(srv, target, fmt) == want, WAIT).is_err() {
        panic!(
            "{fmt} never became {want:?}: {:?}",
            display(srv, target, fmt)
        );
    }
}

#[test]
fn each_modifier_has_a_true_and_a_false_case() {
    let srv = scripted();
    srv.cmd(&["set", "-g", "@pat", "al*"]).expect("set");
    // (format, expected); comparisons are string comparisons, as in tmux
    // (`10` sorts before `9`).
    let table = [
        ("#{!=:a,b}", "1"),
        ("#{!=:a,a}", "0"),
        ("#{!=:#{session_name},alpha}", "0"),
        ("#{>:b,a}", "1"),
        ("#{>:a,a}", "0"),
        ("#{>:10,9}", "0"),
        ("#{>:#{session_windows},1}", "1"),
        ("#{>=:a,a}", "1"),
        ("#{>=:a,b}", "0"),
        ("#{&&:1,x}", "1"),
        ("#{&&:1,0}", "0"),
        ("#{&&:#{window_active},}", "0"),
        ("#{||:0,x}", "1"),
        ("#{||:0,}", "0"),
        ("#{||:#{@nope},#{session_windows}}", "1"),
        ("#{<:a,b}", "1"),
        ("#{<:9,10}", "0"),
        ("#{<=:a,a}", "1"),
        ("#{<=:b,a}", "0"),
        ("#{!:#{@nope}}", "1"),
        ("#{!:#{session_name}}", "0"),
        ("#{m:al*,alpha}", "1"),
        ("#{m:be?a,alpha}", "0"),
        ("#{m:[!x]lpha,alpha}", "1"),
        ("#{m:#{@pat},#{session_name}}", "1"),
        ("#{m/r:^al.ha$,alpha}", "1"),
        ("#{m/r:(copy|view)-mode,#{pane_mode}}", "0"),
        ("#{m/r:(,alpha}", "0"),
        ("#{?#{m:al*,#{session_name}},yes,no}", "yes"),
    ];
    for (fmt, want) in table {
        assert_eq!(display(&srv, "alpha:1", fmt), want, "{fmt}");
    }
}

/// `date`'s rendering of `secs` in ctime's shape (GNU, then BSD flags), or
/// `None` when neither `date` understands the request.
fn date_ctime(secs: i64) -> Option<String> {
    let fmt = "+%a %b %e %H:%M:%S %Y";
    [
        vec!["-d".to_string(), format!("@{secs}"), fmt.to_string()],
        vec!["-r".to_string(), secs.to_string(), fmt.to_string()],
    ]
    .into_iter()
    .find_map(|args| {
        let out = Command::new("date").args(&args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    })
}

#[test]
fn time_modifier_renders_a_variable_as_ctime() {
    let srv = scripted();
    srv.cmd(&["set", "-g", "@stamp", "1700000000"])
        .expect("set");
    let got = display(&srv, "alpha:1", "#{t:@stamp}");
    if let Some(want) = date_ctime(1_700_000_000) {
        assert_eq!(got, want);
    }
    assert!(got.ends_with(" 2023"), "{got}");
    // Not a time: empty, as in tmux. A nested argument expands unchanged.
    srv.cmd(&["set", "-g", "@word", "soon"]).expect("set");
    assert_eq!(display(&srv, "alpha:1", "[#{t:@word}]"), "[]");
    assert_eq!(display(&srv, "alpha:1", "[#{t:@unset}]"), "[]");
    assert_eq!(display(&srv, "alpha:1", "[#{t:#{@stamp}}]"), "[1700000000]");
}

#[test]
fn bare_name_conditions_look_the_name_up() {
    let srv = scripted();
    let fmt = "#{?@custom_opt,yes,no}";
    assert_eq!(display(&srv, "alpha:1", fmt), "no");
    srv.cmd(&["set", "-g", "@custom_opt", "1"]).expect("set");
    assert_eq!(display(&srv, "alpha:1", fmt), "yes");
    srv.cmd(&["set", "-g", "@custom_opt", "0"]).expect("set");
    assert_eq!(display(&srv, "alpha:1", fmt), "no");
    assert_eq!(
        display(&srv, "alpha:1", "#{?session_name,named,anon}"),
        "named"
    );
}

/// TC.4 fills `@src-<hash>`; to the format language it is any user option.
#[test]
fn a_source_reference_is_an_ordinary_user_option() {
    let srv = scripted();
    assert_eq!(display(&srv, "alpha:1", "[#{@src-0123abcd}]"), "[]");
    srv.cmd(&["set", "-g", "@src-0123abcd", "42%"])
        .expect("set");
    assert_eq!(display(&srv, "alpha:1", "[#{@src-0123abcd}]"), "[42%]");
}

/// `#(cmd)` is the tmux dialect's; FE.3 rewrites it before expansion.
#[test]
fn a_job_in_a_native_format_is_literal_text() {
    let srv = scripted();
    assert_eq!(display(&srv, "alpha:1", "#(date)"), "#(date)");
    assert_eq!(
        display(&srv, "alpha:1", "[#(echo #{session_name})]"),
        "[#(echo alpha)]"
    );
}

#[test]
fn version_is_grofes_under_the_grofe_identity() {
    let srv = scripted();
    let v = env!("CARGO_PKG_VERSION");
    assert_eq!(display(&srv, "alpha:1", "#{version}"), v);
    assert_eq!(display(&srv, "alpha:1", "#{grofe_version}"), v);
}

/// The server's pid, from its own start line in the log.
fn server_pid(srv: &TestServer) -> String {
    let log = srv.log();
    let line = log
        .lines()
        .rfind(|l| l.contains("server starting: pid "))
        .unwrap_or_else(|| panic!("no start line in the log:\n{log}"));
    let rest = &line[line.find("pid ").expect("pid") + 4..];
    rest.split(',').next().unwrap_or_default().to_string()
}

#[test]
fn server_and_session_variables() {
    let srv = scripted();
    let t = "alpha:1";
    assert_eq!(display(&srv, t, "#{pid}"), server_pid(&srv));
    assert_eq!(display(&srv, t, "#{session_grouped}"), "0");
    assert_eq!(display(&srv, t, "#{session_windows}"), "2");
    assert_eq!(display(&srv, t, "#{window_panes}"), "1");
    // FE.2 owns tmux's layout string; natively it is a stub.
    assert_eq!(display(&srv, t, "[#{window_layout}]"), "[]");
}

#[test]
fn pane_variables_of_a_plain_pane() {
    let srv = scripted();
    let t = "alpha:1";
    let table = [
        ("#{pane_start_command}", "sleep 30"),
        ("#{pane_left},#{pane_top}", "0,0"),
        ("[#{pane_mode}]", "[]"),
        ("#{pane_in_mode}", "0"),
        ("#{pane_floating_flag}", "0"),
        ("#{alternate_on}", "0"),
        ("#{mouse_any_flag}", "0"),
        ("#{history_size}", "0"),
        ("[#{pane_dead_status}]", "[]"),
        ("[#{copy_cursor_word}]", "[]"),
        ("[#{mouse_status_range}]", "[]"),
        ("[#{session_group}]", "[]"),
        ("[#{client_tty}]", "[]"),
    ];
    for (fmt, want) in table {
        assert_eq!(display(&srv, t, fmt), want, "{fmt}");
    }
}

#[test]
fn start_command_is_quoted_as_tmux_quotes_it() {
    let srv = scripted();
    srv.cmd(&[
        "new-window",
        "-t",
        "alpha",
        "sh",
        "-c",
        "sleep 30",
        "a\"b",
        "c$d",
        "e'f",
        "g\\h",
        "",
        "~x",
        "#{y}",
        "a\tb",
        "a$1",
        "$",
    ])
    .expect("new-window");
    assert_eq!(
        display(&srv, "alpha:3", "#{pane_start_command}"),
        r##"sh -c "sleep 30" 'a"b' "c\$d" "e'f" g\\h '' \~x "#{y}" a\tb "a$1" \$"##
    );
    // A shell pane has no start command.
    srv.cmd(&["new-window", "-t", "alpha"]).expect("new-window");
    assert_eq!(display(&srv, "alpha:4", "[#{pane_start_command}]"), "[]");
}

#[test]
fn pane_modes_follow_what_the_pane_asked_for() {
    let srv = scripted();
    srv.cmd(&[
        "new-window",
        "-t",
        "alpha",
        "sh",
        "-c",
        "printf '\\033[?1049h\\033[?1000h'; sleep 30",
    ])
    .expect("new-window");
    wait_display(&srv, "alpha:3", "#{alternate_on}", "1");
    wait_display(&srv, "alpha:3", "#{mouse_any_flag}", "1");
}

#[test]
fn history_size_counts_scrolled_off_lines() {
    let srv = scripted();
    srv.cmd(&[
        "new-window",
        "-t",
        "alpha",
        "sh",
        "-c",
        "seq 1 100; sleep 30",
    ])
    .expect("new-window");
    let height: usize = display(&srv, "alpha:3", "#{pane_height}")
        .parse()
        .expect("pane_height");
    // 100 lines and the cursor's empty line below them.
    let want = (101 - height).to_string();
    wait_display(&srv, "alpha:3", "#{history_size}", &want);
}

#[test]
fn pane_dead_status_is_the_exit_status_of_a_dead_pane() {
    let srv = scripted();
    srv.cmd(&["new-window", "-t", "alpha", "sh", "-c", "exit 1"])
        .expect("new-window");
    wait_display(&srv, "alpha:3", "#{pane_dead}", "1");
    assert_eq!(display(&srv, "alpha:3", "#{pane_dead_status}"), "1");
    assert_eq!(display(&srv, "alpha:1", "[#{pane_dead_status}]"), "[]");
}

#[test]
fn window_flags_mark_current_last_zoom_and_bell() {
    let srv = scripted();
    assert_eq!(display(&srv, "alpha:2", "#{window_flags}"), "*");
    assert_eq!(display(&srv, "alpha:1", "#{window_flags}"), "-");
    srv.cmd(&["split-window", "-t", "alpha:2", "sleep", "30"])
        .expect("split");
    srv.cmd(&["resize-pane", "-Z", "-t", "alpha:2"])
        .expect("zoom");
    assert_eq!(display(&srv, "alpha:2", "#{window_flags}"), "*Z");
    srv.cmd(&[
        "new-window",
        "-d",
        "-t",
        "alpha",
        "sh",
        "-c",
        "printf '\\a'; sleep 30",
    ])
    .expect("new-window");
    wait_display(&srv, "alpha:3", "[#{window_flags}]", "[!]");
    assert_eq!(display(&srv, "alpha:2", "#{window_flags}"), "*Z");
}

#[test]
fn socket_path_is_the_servers_socket() {
    let srv = scripted();
    let want = srv.socket.display().to_string();
    assert_eq!(display(&srv, "alpha:1", "#{socket_path}"), want);
    let listed = srv
        .cmd(&["list-sessions", "-F", "#{socket_path}"])
        .expect("list-sessions");
    assert_eq!(listed, format!("{want}\n"));
}

/// `hostname`'s answer, which is what tmux titles a new pane with.
fn host() -> String {
    let out = Command::new("hostname").output().expect("hostname");
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

#[test]
fn pane_title_is_the_host_until_the_pane_sets_one() {
    let srv = scripted();
    assert_eq!(display(&srv, "alpha:1", "#{pane_title}"), host());
    srv.cmd(&[
        "new-window",
        "-t",
        "alpha",
        "sh",
        "-c",
        "printf '\\033]2;my title\\007'; sleep 30",
    ])
    .expect("new-window");
    wait_display(&srv, "alpha:3", "#{pane_title}", "my title");
    let listed = srv
        .cmd(&["list-panes", "-t", "alpha:3", "-F", "#{pane_title}"])
        .expect("list-panes");
    assert_eq!(listed, "my title\n");
}

#[test]
fn pane_tty_is_the_panes_own_terminal() {
    let srv = scripted();
    let file = srv.home.join("tty.txt");
    let script = format!("tty > '{}'; sleep 30", file.display());
    srv.cmd(&["new-window", "-t", "alpha", "sh", "-c", &script])
        .expect("new-window");
    common::wait_until(
        || std::fs::read_to_string(&file).is_ok_and(|s| s.ends_with('\n')),
        WAIT,
    )
    .expect("the pane wrote its tty");
    let want = std::fs::read_to_string(&file).unwrap_or_default();
    assert!(want.starts_with("/dev/"), "{want}");
    assert_eq!(display(&srv, "alpha:3", "#{pane_tty}"), want.trim_end());
}
