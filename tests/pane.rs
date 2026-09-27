//! T1.4 pane tests: spawn, emulate, read and write, no server.

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use grofe::server::pane::{PaneEvent, PaneEventKind, PaneHandle, PaneSpawn};
use grofe::types::{CaptureBound, CaptureOpts, PaneId};
use tokio::sync::{Notify, mpsc};

const WAIT: Duration = Duration::from_secs(5);

struct Pane {
    handle: PaneHandle,
    events: mpsc::Receiver<PaneEvent>,
    wake: Arc<Notify>,
}

fn spec(id: u32, argv: &[&str]) -> PaneSpawn {
    PaneSpawn {
        id: PaneId(id),
        argv: Some(argv.iter().map(|s| s.to_string()).collect()),
        cwd: std::env::temp_dir(),
        env: Vec::new(),
        cols: 80,
        rows: 24,
        history_limit: 1000,
        socket: PathBuf::from("/tmp/grofe-test.sock"),
        term: grofe::server::pane::inner_term(),
    }
}

fn start(spec: PaneSpawn) -> Pane {
    let (tx, events) = mpsc::channel(64);
    let wake = Arc::new(Notify::new());
    let handle = PaneHandle::spawn(spec, tx, wake.clone()).expect("spawn");
    Pane {
        handle,
        events,
        wake,
    }
}

fn sh(id: u32, script: &str) -> Pane {
    start(spec(id, &["/bin/sh", "-c", script]))
}

fn screen(p: &PaneHandle) -> String {
    p.capture(&CaptureOpts {
        start: None,
        end: None,
        escapes: false,
    })
}

/// Poll `cond` every 20 ms until it holds or `WAIT` passes.
async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_screen(p: &PaneHandle, needle: &str) {
    wait_for(needle, || screen(p).contains(needle)).await;
}

async fn next_exit(p: &mut Pane) -> Option<i32> {
    let fut = async {
        loop {
            match p.events.recv().await {
                Some(PaneEvent {
                    kind: PaneEventKind::Exited(code),
                    ..
                }) => return code,
                Some(_) => continue,
                None => panic!("event channel closed without Exited"),
            }
        }
    };
    tokio::time::timeout(WAIT, fut)
        .await
        .expect("timed out waiting for Exited")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_output_is_captured_before_exited() {
    let mut p = sh(1, "echo hi");
    let code = next_exit(&mut p).await;
    assert_eq!(code, Some(0));
    assert!(screen(&p.handle).contains("hi"), "{}", screen(&p.handle));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_code_and_signal() {
    let mut p = sh(1, "exit 3");
    assert_eq!(next_exit(&mut p).await, Some(3));
    let mut p = sh(2, "kill -9 $$");
    assert_eq!(next_exit(&mut p).await, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_sets_dirty_and_wakes() {
    let p = sh(1, "echo wake; sleep 5");
    tokio::time::timeout(WAIT, p.wake.notified())
        .await
        .expect("no wake");
    wait_screen(&p.handle, "wake").await;
    assert!(p.handle.take_dirty());
    assert!(!p.handle.take_dirty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpr_query_gets_a_reply() {
    let p = sh(
        1,
        concat!(
            "stty raw -echo; printf '\\033[6n'; x=; ",
            "while c=$(dd bs=1 count=1 2>/dev/null); [ \"$c\" != R ]; do x=\"$x$c\"; done; ",
            "stty sane; case \"$x\" in *\\[*\\;*) echo got-cpr-$((6*7));; esac; sleep 5"
        ),
    );
    wait_screen(&p.handle, "got-cpr-42").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resize_changes_tput_cols() {
    let p = sh(
        1,
        "echo A=$(tput cols); read x; echo B=$(tput cols); sleep 5",
    );
    wait_screen(&p.handle, "A=80").await;
    p.handle.resize(100, 30).expect("resize");
    p.handle.enqueue_input(b"\n".to_vec()).expect("enqueue");
    wait_screen(&p.handle, "B=100").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_input_does_not_block_and_other_panes_stay_responsive() {
    let stuck = start(spec(1, &["sleep", "10"]));
    let t = Instant::now();
    stuck
        .handle
        .enqueue_input(vec![b'x'; 1024 * 1024])
        .expect("enqueue 1 MiB");
    assert!(
        t.elapsed() < Duration::from_millis(100),
        "{:?}",
        t.elapsed()
    );

    let other = start(spec(2, &["cat"]));
    other
        .handle
        .enqueue_input(b"ping-pong\n".to_vec())
        .expect("enqueue");
    wait_for("cat echo", || {
        screen(&other.handle).matches("ping-pong").count() >= 2
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_queue_is_bounded() {
    // Raw mode without echo: the kernel stops accepting input once its queue
    // is full, instead of discarding it (and ringing a bell per byte) as a
    // canonical-mode tty does on macOS, which is slow for megabytes.
    let stuck = sh(1, "stty raw -echo; echo ready; sleep 10");
    wait_screen(&stuck.handle, "ready").await;
    let mut full = false;
    for _ in 0..64 {
        if stuck.handle.enqueue_input(vec![b'x'; 1024 * 1024]).is_err() {
            full = true;
            break;
        }
    }
    assert!(full, "64 MiB were accepted into a pane that never reads");
}

/// The master of one pane must not be open in another pane's child.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_master_fd_leaks_into_a_sibling() {
    let _first = start(spec(1, &["sleep", "10"]));
    // Count ptmx fds in the sibling shell itself and, as a positive control,
    // in its parent (this test process, which holds both masters).
    let count = if cfg!(target_os = "linux") {
        "ls -l /proc/$1/fd 2>/dev/null | grep -c ptmx"
    } else {
        "lsof -p $1 2>/dev/null | grep -c ptmx"
    };
    let script = format!("n() {{ {count}; }}; echo SELF=$(n $$) PARENT=$(n $PPID)=END; sleep 5");
    let sibling = sh(2, &script);
    wait_screen(&sibling.handle, "=END").await;
    let out = screen(&sibling.handle);
    assert!(out.contains("SELF=0 "), "{out}");
    assert!(!out.contains("PARENT=0="), "positive control failed: {out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_path_follows_cd() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let mut s = spec(1, &["/bin/sh"]);
    s.cwd = a.path().to_path_buf();
    let p = start(s);
    let canon = |d: &Path| d.canonicalize().unwrap();
    wait_for("initial cwd", || {
        p.handle.current_path().map(|d| canon(&d)) == Some(canon(a.path()))
    })
    .await;
    let cd = format!("cd '{}'\n", b.path().display());
    p.handle.enqueue_input(cd.into_bytes()).expect("enqueue");
    wait_for("cwd after cd", || {
        p.handle.current_path().map(|d| canon(&d)) == Some(canon(b.path()))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_argv_is_not_comm() {
    let p = start(spec(
        1,
        &["/bin/bash", "-c", "exec -a grofe-fake-name sleep 30"],
    ));
    wait_for("argv of exec -a sleep", || {
        p.handle.current_argv() == Some(vec!["grofe-fake-name".to_string(), "30".to_string()])
    })
    .await;
    // comm is the binary's name (`sleep`, or `gsleep` from coreutils).
    let comm = p.handle.current_command().expect("comm");
    assert!(comm.contains("sleep"), "{comm}");
    assert!(p.handle.pid().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_synchronized_update_still_renders() {
    let p = sh(1, "printf 'before\\033[?2026hSYNCED'; sleep 5");
    wait_screen(&p.handle, "SYNCED").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn env_term_overrides_spawn_env() {
    let mut s = spec(
        7,
        &[
            "/bin/sh",
            "-c",
            "echo T=$TERM C=$COLORTERM P=$GROFE_PANE G=$GROFE; sleep 5",
        ],
    );
    s.env = vec![
        ("TERM".into(), "xterm-kitty".into()),
        ("COLORTERM".into(), "nope".into()),
    ];
    let p = start(s);
    let want = format!(
        "T={} C=truecolor P=%7 G=/tmp/grofe-test.sock",
        grofe::term::resolve_term()
    );
    wait_screen(&p.handle, &want).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_grandchild_does_not_keep_pane_alive() {
    // The grandchild holds the slave open, so the master never reports EOF;
    // the quiet grace period must still let the pane die.
    // It ignores SIGHUP, which the kernel sends when the session leader exits.
    let mut p = sh(1, "(trap '' HUP; exec sleep 3) & echo BG=$!=");
    let t = Instant::now();
    let code = next_exit(&mut p).await;
    assert_eq!(code, Some(0));
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    let out = screen(&p.handle);
    if let Some(pid) = out
        .split("BG=")
        .nth(1)
        .and_then(|s| s.split('=').next())
        .and_then(|s| s.parse::<i32>().ok())
    {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_history_respects_bounds_and_limit() {
    let mut s = spec(
        1,
        &[
            "/bin/sh",
            "-c",
            "i=1; while [ $i -le 60 ]; do echo line$i; i=$((i+1)); done",
        ],
    );
    s.rows = 10;
    s.history_limit = 20;
    let mut p = start(s);
    next_exit(&mut p).await;
    let all = p.handle.capture(&CaptureOpts {
        start: Some(CaptureBound::HistoryStart),
        end: Some(CaptureBound::VisibleEnd),
        escapes: false,
    });
    assert_eq!(all.lines().count(), 30, "{all}");
    assert!(
        !all.contains("line30\n"),
        "history limit not applied: {all}"
    );
    let one = p.handle.capture(&CaptureOpts {
        start: Some(CaptureBound::Line(-1)),
        end: Some(CaptureBound::Line(-1)),
        escapes: false,
    });
    assert_eq!(one.trim_end(), "line51");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_rows_copies_cells() {
    let p = sh(1, "printf '\\033[1mAB\\033[0m'; sleep 5");
    wait_screen(&p.handle, "AB").await;
    let snap = p.handle.snapshot_rows(0..1);
    assert_eq!(snap.rows.len(), 1);
    let row = &snap.rows[0];
    assert_eq!(row.len(), 80);
    assert_eq!(row[0].c, 'A');
    assert!(row[0].flags.contains(grofe::server::pane::CellFlags::BOLD));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn modes_reflect_terminal_state() {
    let p = sh(1, "printf '\\033[?1h\\033[?2004hM'; sleep 5");
    wait_screen(&p.handle, "M").await;
    let m = p.handle.modes();
    assert!(m.app_cursor);
    assert!(m.bracketed_paste);
    assert!(!m.alt_screen);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn title_and_bell_reach_the_actor() {
    let mut p = sh(3, "printf '\\033]0;hello-title\\007\\a\\a\\a'; sleep 5");
    let mut title = None;
    let mut bells = 0;
    let deadline = Instant::now() + WAIT;
    while title.is_none() || bells == 0 {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, p.events.recv()).await {
            Ok(Some(PaneEvent {
                pane: PaneId(3),
                kind: PaneEventKind::Title(t),
                ..
            })) => title = Some(t),
            Ok(Some(PaneEvent {
                pane: PaneId(3),
                kind: PaneEventKind::Bell,
                ..
            })) => bells += 1,
            Ok(Some(other)) => panic!("unexpected {other:?}"),
            Ok(None) | Err(_) => panic!("title={title:?} bells={bells}"),
        }
    }
    assert_eq!(title.as_deref(), Some("hello-title"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synchronized_update_is_flushed_before_exited() {
    // The child begins a synchronized update and exits well inside the
    // 150 ms sync timeout; its text must be on screen when Exited arrives.
    let mut p = sh(1, "printf 'before\\033[?2026hSYNCED'");
    next_exit(&mut p).await;
    let out = screen(&p.handle);
    assert!(out.contains("SYNCED"), "{out}");
}

/// A fd opened in the test process without `CLOEXEC` must not survive into a
/// spawned pane's child (spec §4.8, T1.4b). This is the fd a non-atomic
/// `accept()` + `fcntl(FD_CLOEXEC)` elsewhere in the process (macOS has no
/// `accept4`/`SOCK_CLOEXEC`) can otherwise leak into a pane forked mid-race.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inherited_fd_without_cloexec_is_closed_in_the_child() {
    let (read_fd, write_fd) = nix::unistd::pipe().expect("pipe");
    // Clear CLOEXEC explicitly, rather than relying on the pair's default.
    nix::fcntl::fcntl(
        &read_fd,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
    )
    .expect("clear cloexec");
    let target = read_fd.as_raw_fd();

    let script = format!(
        "if [ -e /dev/fd/{target} ]; then echo STATE=open; else echo STATE=closed; fi; echo DONE; sleep 5"
    );
    let p = sh(1, &script);
    wait_screen(&p.handle, "DONE").await;
    let out = screen(&p.handle);
    assert!(out.contains("STATE=closed"), "{out}");

    drop(write_fd);
    drop(read_fd);
}
