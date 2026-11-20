//! customize-mode `P` saves the selected option to settings.toml
//! (spec 2026-11-20-settings-menu, T17, AC2.8). A private -S socket, a temp
//! HOME and XDG_CONFIG_HOME (the TestServer layout), a pty-attached client.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;
use std::time::Duration;

use common::{TestServer, try_wait_until};
use harness::Harness;

const WAIT: Duration = Duration::from_secs(10);

/// T17: with `status` at 3 lines, customize-mode on `status` and `P` writes
/// settings.toml. The spec writes `status = false` for an off status; the
/// TOML key of the option is `status.lines`, so this checks `status.lines = 3`.
#[test]
fn s_in_customize_mode_saves_the_selected_option() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "s",
        "-x",
        "70",
        "-y",
        "20",
        "cat",
    ])
    .expect("new-session");
    srv.cmd(&["set", "-g", "status", "3"]).expect("set");
    let mut h = Harness::spawn_attach_with_env(srv.command(), "s", &[], 70, 20);
    // The filter keeps only the `status` option.
    srv.cmd(&[
        "bind-key",
        "C",
        "customize-mode",
        "-Z",
        "-f",
        "#{==:#{option_name},status}",
    ])
    .expect("bind-key");
    h.send("\x02C");
    h.wait_until("customize-mode open", WAIT, |t| {
        t.contains("Session Options")
    });
    h.send("j");
    h.send("l");
    h.wait_until("status listed", WAIT, |t| t.contains("status:"));
    h.send("j");
    h.send("P");
    let file = srv.config_home.join("grofe").join("settings.toml");
    let ok = try_wait_until(
        || {
            fs::read_to_string(&file)
                .ok()
                .and_then(|t| t.parse::<toml::Table>().ok())
                .and_then(|t| t.get("status")?.get("lines").cloned())
                .is_some_and(|v| v.as_integer() == Some(3))
        },
        WAIT,
    );
    assert!(
        ok.is_ok(),
        "settings.toml: {:?}\nscreen: {}",
        fs::read_to_string(&file),
        h.text()
    );
    // Nothing else is saved.
    let t: toml::Table = fs::read_to_string(&file)
        .expect("read")
        .parse()
        .expect("toml");
    assert_eq!(t.len(), 1, "{t:?}");
}
