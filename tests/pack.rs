//! `grofe pack sync` from a local bare repo, then `reload` loads the plugin.

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use common::{CMD_TIMEOUT, TestServer, run_bounded};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn pack_sync_then_reload_loads_plugin() {
    let srv = TestServer::builder().unstarted();
    let root = srv.home.join("repos");
    let work = root.join("work");
    let bare = root.join("p.git");
    fs::create_dir_all(&work).expect("mkdir work");
    fs::write(work.join("grofe-plugin.toml"), "name = \"p\"\napi = 1\n").expect("write manifest");
    fs::write(work.join("init.luau"), "return { x = 1 }\n").expect("write init");
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "p"]);
    git(&root, &["clone", "-q", "--bare", "work", "p.git"]);

    let config = srv.config_home.join("grofe");
    fs::create_dir_all(&config).expect("mkdir config");
    fs::write(
        config.join("config.toml"),
        format!("[[pack]]\nsrc = \"file://{}\"\n", bare.display()),
    )
    .expect("write config");

    // Every grofe this test runs sees the same private data dir, whatever
    // the host's own XDG_DATA_HOME says.
    let data = srv.home.join("data");
    let run = |args: &[&str]| {
        let mut cmd = srv.command();
        cmd.args(args)
            .env("XDG_DATA_HOME", &data)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0");
        run_bounded(cmd, CMD_TIMEOUT)
    };

    let out = run(&["pack", "sync"]);
    assert!(
        out.status.success(),
        "pack sync: {} {}",
        out.stdout,
        out.stderr
    );
    assert!(config.join("pack-lock.toml").exists(), "no lock written");

    let out = run(&["start-server"]);
    assert!(out.status.success(), "start-server: {}", out.stderr);
    let out = run(&["reload"]);
    assert!(out.status.success(), "reload: {}", out.stderr);
    let out = run(&["lua", "return grofe.plugin(\"p\").x"]);
    assert!(
        out.status.success() && out.stdout.trim() == "1",
        "lua: {:?} {:?}\nlog:\n{}",
        out.stdout,
        out.stderr,
        srv.log()
    );
}
