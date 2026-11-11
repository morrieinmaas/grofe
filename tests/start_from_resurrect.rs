//! `start-from-tmux` with no tmux running: the server takes the sessions
//! of tmux-resurrect's newest save in `resurrect-dir`, and a pane saved
//! with a command runs it again. Its own file, as it points `TMUX_TMPDIR`
//! at an empty directory for the whole test process.

mod common;

use std::fs;
use std::time::Duration;

use common::{TestServer, wait_until};

#[test]
fn with_no_tmux_running_the_newest_resurrect_save_is_restored() {
    let empty = tempfile::tempdir().expect("tempdir");
    // SAFETY: this file's only test; nothing else reads the environment.
    unsafe { std::env::set_var("TMUX_TMPDIR", empty.path()) };
    // tmux's sessions go only to the default server (`-L default`).
    let mut srv = TestServer::builder().restore_on_start(true).unstarted();
    srv.socket = srv.runtime_dir.join("default.sock");
    let save_dir = srv.home.join("saves");
    fs::create_dir_all(&save_dir).expect("mkdir");
    let cwd = srv.home.display().to_string();
    let marker = srv.home.join("ran");
    let save = format!(
        "pane\twork\t1\t1\t:*\t1\tt\t:{cwd}\t1\tsh\t:\n\
         pane\twork\t1\t1\t:*\t2\tt\t:{cwd}\t0\ttouch\t:touch {m}; exec sleep 600\n\
         window\twork\t1\t:editor\t1\t:*\t{{\"V\":2,\"L\":{{\"t\":\"h\",\"w\":80,\"h\":24,\"x\":0,\"y\":0,\"c\":[{{\"t\":\"p\",\"w\":40,\"h\":24,\"x\":0,\"y\":0,\"i\":1}},{{\"t\":\"p\",\"w\":39,\"h\":24,\"x\":41,\"y\":0,\"i\":2}}]}}}}\t:\n\
         state\twork\twork\n",
        m = marker.display()
    );
    fs::write(save_dir.join("tmux_resurrect_1.txt"), save).expect("write save");
    std::os::unix::fs::symlink("tmux_resurrect_1.txt", save_dir.join("last")).expect("link");
    fs::write(
        srv.config_home.join("grofe").join("config.toml"),
        format!(
            "start-from-tmux = true\nresurrect-dir = \"{}\"\n",
            save_dir.display()
        ),
    )
    .expect("write config");
    srv.cmd(&["start-server"]).expect("start-server");

    let windows = srv
        .cmd(&[
            "list-windows",
            "-a",
            "-F",
            "#{session_name}:#{window_name} #{window_panes}",
        ])
        .unwrap_or_default();
    assert_eq!(windows, "work:editor 2\n", "server log:\n{}", srv.log());
    let ran = wait_until(|| marker.exists(), Duration::from_secs(10));
    assert!(
        ran.is_ok(),
        "the saved command did not run; log:\n{}",
        srv.log()
    );
}
