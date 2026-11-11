//! Sessions made without `-s` are named, at once, after their git
//! repository or folder, with `-2`, `-3` for more in the same place; one
//! given `-s` keeps its name.

mod common;

use std::fs;

use common::TestServer;

#[test]
fn an_unnamed_session_is_named_after_its_place_at_once() {
    let srv = TestServer::start();
    let repo = srv.home.join("proj");
    let plain = srv.home.join("notes");
    fs::create_dir_all(repo.join(".git")).expect("mkdir");
    fs::create_dir_all(&plain).expect("mkdir");
    let (repo, plain) = (repo.to_str().expect("utf8"), plain.to_str().expect("utf8"));
    let new = |dir: &str| {
        srv.cmd(&[
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{session_name}",
            "-c",
            dir,
        ])
        .expect("new-session")
    };
    assert_eq!(new(repo), "proj\n");
    assert_eq!(new(repo), "proj-2\n");
    assert_eq!(new(plain), "notes\n");
    srv.cmd(&["new-session", "-d", "-s", "mine", "-c", repo])
        .expect("new-session -s");
    let names = srv
        .cmd(&["list-sessions", "-F", "#{session_name}"])
        .expect("list-sessions");
    assert_eq!(names, "mine\nnotes\nproj\nproj-2\n");
}
