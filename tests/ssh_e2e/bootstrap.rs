//! The bootstrap install: `grofe -r <bare host> --install` against the
//! container without grofe installs the release from the local release
//! dir, then attaches; the next connect finds it installed.

use crate::harness::{Cleanup, REMOTE_TIMEOUT, run, text};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[test]
fn install_from_the_release_dir_then_attach_and_no_second_install() {
    let e = e2e_or_skip!();
    let installed = format!("$HOME/.local/share/grofe/bin/{VERSION}/grofe");
    let wipe = format!(
        "{installed} kill-server 2>/dev/null; rm -rf $HOME/.local/share/grofe $HOME/.local/state/grofe"
    );
    // A clean slate, and the same again afterwards, pass or fail.
    let out = e.ssh_to(&e.bare_host, &wipe);
    assert!(out.status.success(), "{out:?}");
    let _cleanup = Cleanup {
        e: &e,
        host: e.bare_host.clone(),
        command: wipe.clone(),
        netem: false,
    };
    let local = e.local();

    let mut cmd = local.grofe(&["-r", &e.bare_host, "--install"]);
    cmd.env("GROFE_REMOTE_RELEASE_URL", &e.release_url);
    let mut h = local.attach(cmd);
    h.wait_for_timeout("$ ", REMOTE_TIMEOUT);
    h.send("echo e2e-$((6*7))-installed\r");
    h.wait_for_timeout("e2e-42-installed", REMOTE_TIMEOUT);
    h.send("\x02d");
    h.wait_exit();
    let said = String::from_utf8_lossy(h.output()).into_owned();
    assert!(said.contains(&format!("installed v{VERSION}")), "{said}");

    let out = e.ssh_to(&e.bare_host, &format!("{installed} -V"));
    assert!(text(&out).starts_with("grofe "), "{out:?}");

    // The second connect: no --install, no release URL, and no question.
    let out = run(local.grofe(&["-r", &e.bare_host, "list-sessions"]));
    assert!(out.status.success(), "{out:?}");
    assert!(text(&out).contains("main:"), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("install"), "{err}");
}
