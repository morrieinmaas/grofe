use crate::harness::text;

#[test]
fn grofe_runs_over_ssh() {
    let e = e2e_or_skip!();
    let out = e.ssh("grofe -V");
    assert!(out.status.success(), "{out:?}");
    assert!(text(&out).starts_with("grofe "), "{out:?}");
}

#[test]
fn private_socket_session_in_the_container() {
    let e = e2e_or_skip!();
    let script = "d=$(mktemp -d) && grofe -S $d/s new-session -d -s smoke \
        && grofe -S $d/s list-sessions; rc=$?; grofe -S $d/s kill-server; rm -rf $d; exit $rc";
    let out = e.ssh(script);
    assert!(out.status.success(), "{out:?}");
    assert!(text(&out).contains("smoke"), "{out:?}");
}

#[test]
fn bare_host_has_no_grofe() {
    let e = e2e_or_skip!();
    let out = e.ssh_to(&e.bare_host, "command -v grofe");
    assert!(!out.status.success(), "{out:?}");
}

#[test]
fn container_web_server_answers() {
    let e = e2e_or_skip!();
    let out = e.ssh(
        "python3 -c 'import urllib.request as u; print(u.urlopen(\"http://127.0.0.1:8000/\").read().decode())'",
    );
    assert!(
        text(&out).contains("hello from the container web server"),
        "{out:?}"
    );
}

#[test]
fn helpers_reach_the_container() {
    let e = e2e_or_skip!();
    let out = e.helper("e2e_exec", &["whoami"]);
    assert_eq!(text(&out).trim(), "root", "{out:?}");
    // 1 ms of delay is harmless; clear removes it again.
    assert!(e.helper("e2e_netem", &["delay", "1ms"]).status.success());
    assert!(e.helper("e2e_netem", &["clear"]).status.success());
    // Session processes killed from outside: the next login still works.
    assert!(e.helper("e2e_kill_sshd_children", &[]).status.success());
    assert!(e.ssh("true").status.success());
}

#[test]
fn release_dir_is_served() {
    let e = e2e_or_skip!();
    let out = std::process::Command::new("curl")
        .args([
            "-fsS",
            &format!(
                "{}/v{}/SHA256SUMS",
                e.release_url,
                env!("CARGO_PKG_VERSION")
            ),
        ])
        .output()
        .expect("run curl");
    assert!(out.status.success(), "{out:?}");
    assert!(text(&out).contains("-linux-musl.tar.gz"), "{out:?}");
}

#[test]
fn sshd_accepts_the_server_socket_a_scenario_sends() {
    let e = e2e_or_skip!();
    let local = e.local();
    let out = std::process::Command::new(local.root().join("ssh"))
        .args([&e.host, "printf %s \"$GROFE\""])
        .output()
        .expect("run the ssh wrapper");
    assert_eq!(text(&out), local.sock, "{out:?}");
}
