//! Events (spec §4.6): `subscribe`, per-subscriber bounded queues with an
//! `events-dropped` notice, `pane-exited` on every pane removal,
//! `pane-output`, `option-changed`, plus the two model fixes that landed
//! with them: a respawned pane ignores its old process's exit (spawn
//! generation) and a pane leaving a zoomed window unzooms it (tmux 3.7b).

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use common::{Subscription, TestServer};
use grofe::event::Event;
use grofe::protocol::{Hello, Reply, Request, encode_hello};
use grofe::types::PaneId;

const WAIT: Duration = Duration::from_secs(5);
/// How long a test waits to be sure an event did not come.
const QUIET: Duration = Duration::from_millis(300);

fn display(srv: &TestServer, target: &str, fmt: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, fmt])
        .unwrap_or_else(|e| panic!("display-message {fmt}: {e}"))
        .trim_end()
        .to_string()
}

fn panes(srv: &TestServer, target: &str, fmt: &str) -> Vec<String> {
    srv.cmd(&["list-panes", "-t", target, "-F", fmt])
        .unwrap_or_else(|e| panic!("list-panes: {e}"))
        .lines()
        .map(str::to_string)
        .collect()
}

fn pane_id(srv: &TestServer, target: &str) -> PaneId {
    display(srv, target, "#{pane_id}").parse().unwrap()
}

fn next(sub: &mut Subscription) -> Event {
    sub.next(WAIT).unwrap_or_else(|e| panic!("no event: {e}"))
}

fn option_changed(name: &str, value: Option<&str>) -> Event {
    Event::OptionChanged {
        scope: "global".to_string(),
        name: name.to_string(),
        value: value.map(str::to_string),
    }
}

/// A raw control connection that can pipeline many requests.
struct Control {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Control {
    fn open(srv: &TestServer) -> Control {
        let writer = UnixStream::connect(&srv.socket).expect("connect");
        let reader = BufReader::new(writer.try_clone().unwrap());
        let mut c = Control { writer, reader };
        let hello = encode_hello(&Hello::Control(srv.ctx())).unwrap();
        writeln!(c.writer, "{hello}").unwrap();
        c
    }

    fn line(id: u64, argv: &[String]) -> String {
        serde_json::to_string(&Request {
            id,
            argv: argv.to_vec(),
            ..Default::default()
        })
        .unwrap()
    }

    /// Sends every request from a writer thread while reading the replies
    /// here, so neither side's socket buffer can deadlock the other.
    fn pipeline(self, requests: Vec<Vec<String>>) -> Vec<Reply> {
        let Control { mut writer, reader } = self;
        // A server that stops answering fails the test instead of hanging it.
        reader.get_ref().set_read_timeout(Some(WAIT)).unwrap();
        let n = requests.len();
        let sender = thread::spawn(move || {
            for (id, argv) in requests.iter().enumerate() {
                writeln!(writer, "{}", Control::line(id as u64, argv)).unwrap();
            }
            writer
        });
        let replies: Vec<Reply> = reader
            .lines()
            .take(n)
            .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
            .collect();
        drop(sender.join().unwrap());
        replies
    }
}

/// A pane command that waits for `flag` to exist, then exits with
/// `status`, so a test decides when the pane exits.
fn exit_on(flag: &std::path::Path, status: i32) -> String {
    format!(
        "while [ ! -e {} ]; do sleep 0.01; done; exit {status}",
        flag.display()
    )
}

fn touch(flag: &std::path::Path) {
    std::fs::write(flag, "").unwrap();
}

fn argv(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

// --- pane-exited ---------------------------------------------------------

#[test]
fn pane_exited_fires_once_with_the_exit_status() {
    let srv = TestServer::start();
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("go");
    let mut sub = srv.subscribe(&["pane-exited"]).unwrap();
    srv.cmd(&["new-session", "-d", "-s", "e", &exit_on(&flag, 3)])
        .unwrap();
    let pane = pane_id(&srv, "e");
    let uid = display(&srv, "e", "#{pane_uid}");
    touch(&flag);

    assert_eq!(
        next(&mut sub),
        Event::PaneExited {
            pane,
            uid,
            status: Some(3)
        }
    );
    // remain-on-exit (on in tests) keeps the dead pane; removing it later
    // must not report the same exit again.
    assert_eq!(display(&srv, "e", "#{pane_dead}"), "1");
    srv.cmd(&["new-window", "-d", "-t", "e", "sleep 30"])
        .unwrap();
    srv.cmd(&["kill-pane", "-t", &pane.to_string()]).unwrap();
    assert!(sub.next(QUIET).is_err(), "a second pane-exited for {pane}");
}

#[test]
fn natural_exit_with_remain_on_exit_off_still_reports_the_status() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "keep", "sleep 30"])
        .unwrap();
    srv.cmd(&["set", "-g", "remain-on-exit", "off"]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("go");
    let mut sub = srv.subscribe(&["pane-exited"]).unwrap();
    srv.cmd(&["new-session", "-d", "-s", "gone", &exit_on(&flag, 5)])
        .unwrap();
    let pane = pane_id(&srv, "gone");
    let uid = display(&srv, "gone", "#{pane_uid}");
    touch(&flag);
    assert_eq!(
        next(&mut sub),
        Event::PaneExited {
            pane,
            uid,
            status: Some(5)
        }
    );
    srv.wait_for(|| srv.cmd(&["has-session", "-t", "gone"]).is_err(), WAIT)
        .unwrap();
}

#[test]
fn kill_pane_reports_pane_exited_with_null_status() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "k", "sleep 30"])
        .unwrap();
    srv.cmd(&["split-window", "-t", "k", "sleep 30"]).unwrap();
    let pane = pane_id(&srv, "k:1.2");
    let uid = display(&srv, "k:1.2", "#{pane_uid}");
    let mut sub = srv.subscribe(&["pane-exited"]).unwrap();

    srv.cmd(&["kill-pane", "-t", &pane.to_string()]).unwrap();
    assert_eq!(
        next(&mut sub),
        Event::PaneExited {
            pane,
            uid,
            status: None
        }
    );
    assert!(sub.next(QUIET).is_err(), "only the killed pane is reported");
}

/// kill-window, kill-session and kill-server each report every live pane
/// they remove, once, with a null status.
#[test]
fn kill_window_session_and_server_report_their_panes() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "a", "sleep 30"])
        .unwrap();
    srv.cmd(&["new-window", "-d", "-t", "a", "sleep 30"])
        .unwrap();
    srv.cmd(&["split-window", "-t", "a:2", "sleep 30"]).unwrap();
    srv.cmd(&["new-session", "-d", "-s", "b", "sleep 30"])
        .unwrap();
    srv.cmd(&["new-session", "-d", "-s", "c", "sleep 30"])
        .unwrap();
    srv.cmd(&["split-window", "-t", "c", "sleep 30"]).unwrap();
    let ids = |t: &str| -> Vec<(PaneId, String)> {
        panes(&srv, t, "#{pane_id} #{pane_uid}")
            .iter()
            .map(|l| {
                let (id, uid) = l.split_once(' ').unwrap();
                (id.parse().unwrap(), uid.to_string())
            })
            .collect()
    };
    let window = ids("a:2");
    let session = ids("b");
    let rest = [ids("a:1"), ids("c")].concat();
    let mut sub = srv.subscribe(&["pane-exited"]).unwrap();

    let expect = |killed: Vec<(PaneId, String)>, sub: &mut Subscription| {
        let mut got: Vec<Event> = killed.iter().map(|_| next(sub)).collect();
        let mut want: Vec<Event> = killed
            .into_iter()
            .map(|(pane, uid)| Event::PaneExited {
                pane,
                uid,
                status: None,
            })
            .collect();
        got.sort_by_key(|e| format!("{e:?}"));
        want.sort_by_key(|e| format!("{e:?}"));
        assert_eq!(got, want);
    };

    srv.cmd(&["kill-window", "-t", "a:2"]).unwrap();
    expect(window, &mut sub);
    srv.cmd(&["kill-session", "-t", "b"]).unwrap();
    expect(session, &mut sub);
    srv.cmd(&["kill-server"]).unwrap();
    expect(rest, &mut sub);
    match sub.next(WAIT) {
        Err(e) => assert!(e.contains("closed"), "{e}"),
        Ok(ev) => panic!("unexpected event after kill-server: {ev:?}"),
    }
}

// --- pane-output ---------------------------------------------------------

#[test]
fn pane_output_delivers_the_panes_bytes_base64() {
    let srv = TestServer::start();
    let mut sub = srv.subscribe(&["pane-output"]).unwrap();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "o",
        "printf 'out-\\001-%s\\n' marker; sleep 30",
    ])
    .unwrap();
    let pane = pane_id(&srv, "o");

    let mut bytes = Vec::new();
    let deadline = Instant::now() + WAIT;
    while !bytes.windows(12).any(|w| w == b"out-\x01-marker") {
        assert!(Instant::now() < deadline, "no marker in {bytes:?}");
        match next(&mut sub) {
            Event::PaneOutput { pane: p, data } => {
                assert_eq!(p, pane);
                bytes.extend(
                    base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .unwrap(),
                );
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
}

// --- option-changed ------------------------------------------------------

#[test]
fn option_changed_on_set_and_not_on_a_same_value_set() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "s", "sleep 30"])
        .unwrap();
    let mut sub = srv.subscribe(&["option-changed"]).unwrap();

    srv.cmd(&["set", "-g", "@x", "1"]).unwrap();
    assert_eq!(next(&mut sub), option_changed("@x", Some("1")));
    // The same value again is a no-op: the next event is the real change.
    srv.cmd(&["set", "-g", "@x", "1"]).unwrap();
    srv.cmd(&["set", "-g", "@x", "2"]).unwrap();
    assert_eq!(next(&mut sub), option_changed("@x", Some("2")));
    srv.cmd(&["set", "-gu", "@x"]).unwrap();
    assert_eq!(next(&mut sub), option_changed("@x", None));
    // Unsetting what is not set changes nothing.
    srv.cmd(&["set", "-gu", "@x"]).unwrap();

    let session = display(&srv, "s", "#{session_id}");
    srv.cmd(&["set", "-t", "s", "@y", "v"]).unwrap();
    assert_eq!(
        next(&mut sub),
        Event::OptionChanged {
            scope: session,
            name: "@y".to_string(),
            value: Some("v".to_string()),
        }
    );
    // A built-in set to the value it already has changes nothing.
    srv.cmd(&["set", "-g", "remain-on-exit", "on"]).unwrap();
    srv.cmd(&["set", "-g", "remain-on-exit", "off"]).unwrap();
    assert_eq!(
        next(&mut sub),
        option_changed("remain-on-exit", Some("off"))
    );
}

// --- filters -------------------------------------------------------------

#[test]
fn unsubscribed_events_are_not_sent() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "u", "sleep 30"])
        .unwrap();
    srv.cmd(&["split-window", "-t", "u", "sleep 30"]).unwrap();
    let doomed = pane_id(&srv, "u:1.2");
    let mut exits = srv.subscribe(&["pane-exited"]).unwrap();
    let mut options = srv.subscribe(&["option-*"]).unwrap();

    srv.cmd(&["set", "-g", "@first", "1"]).unwrap();
    srv.cmd(&["kill-pane", "-t", &doomed.to_string()]).unwrap();
    srv.cmd(&["set", "-g", "@last", "1"]).unwrap();

    // Each sees only its own kind, in order, with nothing in between.
    assert!(matches!(next(&mut exits), Event::PaneExited { pane, .. } if pane == doomed));
    assert_eq!(next(&mut options), option_changed("@first", Some("1")));
    assert_eq!(next(&mut options), option_changed("@last", Some("1")));
    assert!(exits.next(QUIET).is_err());
}

/// A bare `subscribe` gets every event but `pane-output`, which is only
/// sent when named.
#[test]
fn bare_subscribe_gets_everything_but_pane_output() {
    let srv = TestServer::start();
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("go");
    let mut all = srv.subscribe(&[]).unwrap();
    let script = format!("printf 'visible-output\\n'; {}", exit_on(&flag, 4));
    srv.cmd(&["new-session", "-d", "-s", "b", &script]).unwrap();
    srv.wait_for(|| srv.capture("b").contains("visible-output"), WAIT)
        .unwrap();
    // The v1 catalog (spec 7.2) puts session-created on a bare subscribe.
    assert!(matches!(next(&mut all), Event::SessionCreated { .. }));
    srv.cmd(&["set", "-g", "@after-output", "1"]).unwrap();
    assert_eq!(next(&mut all), option_changed("@after-output", Some("1")));
    touch(&flag);
    assert!(matches!(
        next(&mut all),
        Event::PaneExited {
            status: Some(4),
            ..
        }
    ));
}

#[test]
fn subscribe_rejects_an_unknown_event_name() {
    let srv = TestServer::start();
    let err = srv.subscribe(&["pane-exitted"]).map(|_| ()).unwrap_err();
    assert!(err.contains("unknown event: pane-exitted"), "{err}");
}

/// Events reach a subscriber in the order they happened, whatever their
/// kind.
#[test]
fn a_subscriber_sees_events_in_order() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "o", "sleep 30"])
        .unwrap();
    srv.cmd(&["split-window", "-t", "o", "sleep 30"]).unwrap();
    let doomed = pane_id(&srv, "o:1.2");
    let mut sub = srv.subscribe(&["pane-exited", "option-changed"]).unwrap();

    let requests = vec![
        argv(&["set", "-g", "@a", "1"]),
        argv(&["kill-pane", "-t", &doomed.to_string()]),
        argv(&["set", "-g", "@b", "1"]),
    ];
    let replies = Control::open(&srv).pipeline(requests);
    assert!(
        replies.iter().all(|r| matches!(r, Reply::Ok { .. })),
        "{replies:?}"
    );

    assert_eq!(next(&mut sub), option_changed("@a", Some("1")));
    assert!(matches!(next(&mut sub), Event::PaneExited { pane, .. } if pane == doomed));
    assert_eq!(next(&mut sub), option_changed("@b", Some("1")));
}

// --- bounded queues ------------------------------------------------------

/// A subscriber that never reads overflows its queue: the server keeps
/// answering (a `new-session` is not delayed), drops what does not fit,
/// and once the subscriber reads again sends one `events-dropped` with the
/// exact count before resuming.
#[test]
fn a_stuck_subscriber_gets_events_dropped_and_delays_nothing() {
    const N: usize = 4000;
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "q", "sleep 30"])
        .unwrap();
    let mut stuck = srv.subscribe(&["option-changed"]).unwrap();

    // Big values fill the socket buffers quickly on every OS.
    let pad = "p".repeat(512);
    let requests = (0..N)
        .map(|i| argv(&["set", "-g", "@n", &format!("{i}-{pad}")]))
        .collect();
    let replies = Control::open(&srv).pipeline(requests);
    assert!(replies.iter().all(|r| matches!(r, Reply::Ok { .. })));

    let started = Instant::now();
    srv.cmd(&["new-session", "-d", "-s", "late", "sleep 30"])
        .unwrap();
    let took = started.elapsed();
    assert!(took < Duration::from_secs(2), "new-session took {took:?}");

    // Read everything that was queued, then trigger one more event.
    let mut received = Vec::new();
    while let Ok(ev) = stuck.next(QUIET) {
        received.push(ev);
    }
    srv.cmd(&["set", "-g", "@done", "1"]).unwrap();
    let notice = next(&mut stuck);
    let dropped = match notice {
        Event::EventsDropped { count } => count as usize,
        other => panic!("expected events-dropped, got {other:?}"),
    };
    assert_eq!(next(&mut stuck), option_changed("@done", Some("1")));

    assert!(dropped > 0, "nothing was dropped");
    let values: Vec<String> = received
        .iter()
        .map(|ev| match ev {
            Event::OptionChanged { name, value, .. } if name == "@n" => value.clone().unwrap(),
            other => panic!("unexpected event {other:?}"),
        })
        .collect();
    for (i, v) in values.iter().enumerate() {
        assert_eq!(v, &format!("{i}-{pad}"), "event {i} out of order");
    }
    assert_eq!(values.len() + dropped, N);
}

/// Subscribers that go away are forgotten: many subscribe/disconnect
/// cycles later, events still reach a live subscriber promptly and the
/// server still answers.
#[test]
fn disconnected_subscribers_are_cleaned_up() {
    let srv = TestServer::start();
    for i in 0..200 {
        let sub = srv.subscribe(&["option-changed", "pane-output"]).unwrap();
        drop(sub);
        if i % 20 == 0 {
            srv.cmd(&["set", "-g", "@cycle", &i.to_string()]).unwrap();
        }
    }
    let mut live = srv.subscribe(&["option-changed"]).unwrap();
    srv.cmd(&["set", "-g", "@after", "1"]).unwrap();
    assert_eq!(next(&mut live), option_changed("@after", Some("1")));
    srv.cmd(&["new-session", "-d", "-s", "still", "sleep 30"])
        .unwrap();
}

// --- respawn generation --------------------------------------------------

const RACE_PANES: usize = 8;

/// Sets up the respawn race: a pane with a long history (so capturing it
/// keeps the actor busy) and [`RACE_PANES`] windows running `old(flag)`.
/// Returns their pane ids and the flag dir.
fn race_panes(
    srv: &TestServer,
    old: impl Fn(&std::path::Path) -> String,
) -> (Vec<PaneId>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    srv.cmd(&["new-session", "-d", "-s", "big", "seq 1 60000; sleep 30"])
        .unwrap();
    srv.wait_for(|| srv.capture("big").contains("60000"), WAIT)
        .unwrap();
    let ids = (0..RACE_PANES)
        .map(|i| {
            let script = old(&dir.path().join(format!("go{i}")));
            srv.cmd(&["new-window", "-d", "-t", "big", &script])
                .unwrap();
            pane_id(srv, &format!("big:{}", i + 2))
        })
        .collect();
    (ids, dir)
}

/// Keeps the actor busy with two slow captures, sets every flag while it
/// is (so each old process acts and its pane events queue up), then sends
/// each group of requests behind the captures, pipelined on a connection
/// of its own. A connection reads its next request only after the reply
/// to the previous one, so a group runs in order; the groups' first
/// requests wait in the actor's queue together. Stale pane events and the
/// requests are then handled in a random interleaving. Returns the replies,
/// group by group.
fn race(srv: &TestServer, dir: &std::path::Path, groups: Vec<Vec<Vec<String>>>) -> Vec<Reply> {
    let mut conns: Vec<Control> = groups.iter().map(|_| Control::open(srv)).collect();
    let slow = Control::open(srv);
    let busy = thread::spawn(move || {
        slow.pipeline(vec![
            argv(&["capture-pane", "-p", "-S", "-", "-t", "big:1"]),
            argv(&["capture-pane", "-p", "-S", "-", "-t", "big:1"]),
        ])
    });
    thread::sleep(Duration::from_millis(5));
    for i in 0..RACE_PANES {
        touch(&dir.join(format!("go{i}")));
    }
    thread::sleep(Duration::from_millis(40));
    for (c, group) in conns.iter_mut().zip(&groups) {
        for (id, req) in group.iter().enumerate() {
            writeln!(c.writer, "{}", Control::line(id as u64, req)).unwrap();
        }
    }
    let mut replies = Vec::new();
    for (c, group) in conns.iter_mut().zip(&groups) {
        c.writer.set_read_timeout(Some(WAIT)).unwrap();
        for _ in group {
            let mut line = String::new();
            c.reader.read_line(&mut line).unwrap();
            replies.push(serde_json::from_str(&line).unwrap());
        }
    }
    busy.join().unwrap();
    replies
}

/// `respawn-pane -k` racing the old process's exit: the old process's
/// `Exited`, already queued when the respawn runs, must not be applied to
/// the respawned pane. The bug is caught by the `pane_dead` checks: with
/// remain-on-exit on, a stale exit marks the new, running process dead.
/// (A `pane-exited` for the old process is legitimate when its exit was
/// handled before the respawn, so events cannot tell the two apart.)
#[test]
fn respawn_ignores_the_old_processes_exit() {
    let srv = TestServer::start();
    let (ids, dir) = race_panes(&srv, |flag| exit_on(flag, 7));
    // `exec`, so the foreground argv is `sleep 30` whatever `/bin/sh` is:
    // not every shell execs the last command of `sh -c` itself.
    let respawns = ids
        .iter()
        .map(|id| {
            vec![argv(&[
                "respawn-pane",
                "-k",
                "-t",
                &id.to_string(),
                "exec sleep 30",
            ])]
        })
        .collect();
    let replies = race(&srv, dir.path(), respawns);
    assert!(
        replies.iter().all(|r| matches!(r, Reply::Ok { .. })),
        "{replies:?}"
    );

    // Every stale exit has been handled once a few more round trips went
    // through the actor.
    thread::sleep(QUIET);
    for id in &ids {
        let t = id.to_string();
        assert_eq!(display(&srv, &t, "#{pane_dead}"), "0", "{t} marked dead");
        srv.wait_for(
            || display(&srv, &t, "#{pane_current_argv}") == "sleep 30",
            WAIT,
        )
        .unwrap_or_else(|e| panic!("{t} is not running the new command: {e}"));
    }
}

/// Output the old process wrote, queued across `respawn-pane -k`, is not
/// delivered as the respawned pane's `pane-output`. Each respawn is
/// pipelined with a `set @after-%N` fence on its own connection, so the
/// fence runs after its respawn and its `option-changed` marks that point:
/// any old output for that pane seen after it came from a stale event.
#[test]
fn respawn_ignores_the_old_processes_output() {
    const OLD: &[u8] = b"OLD-OUTPUT-MARK";
    let srv = TestServer::start();
    let (ids, dir) = race_panes(&srv, |flag| {
        format!(
            "while [ ! -e {} ]; do sleep 0.01; done; printf OLD-OUTPUT-MARK; sleep 30",
            flag.display()
        )
    });
    let mut sub = srv.subscribe(&["pane-output", "option-changed"]).unwrap();
    let requests = ids
        .iter()
        .map(|id| {
            let t = id.to_string();
            vec![
                argv(&["respawn-pane", "-k", "-t", &t, "exec sleep 30"]),
                argv(&["set", "-g", &format!("@after-{t}"), "1"]),
            ]
        })
        .collect();
    let replies = race(&srv, dir.path(), requests);
    assert!(
        replies.iter().all(|r| matches!(r, Reply::Ok { .. })),
        "{replies:?}"
    );

    let mut fenced = std::collections::BTreeSet::new();
    while let Ok(ev) = sub.next(QUIET) {
        match ev {
            Event::OptionChanged { name, .. } => {
                fenced.insert(name.trim_start_matches("@after-").to_string());
            }
            Event::PaneOutput { pane, data } => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap();
                let old = bytes.windows(OLD.len()).any(|w| w == OLD);
                assert!(
                    !(old && fenced.contains(&pane.to_string())),
                    "{pane}: old output delivered after its respawn"
                );
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert_eq!(fenced.len(), RACE_PANES, "fences seen: {fenced:?}");
}

// --- zoom on natural removal ---------------------------------------------

fn zoom_state(srv: &TestServer, t: &str) -> Vec<String> {
    panes(
        srv,
        t,
        "#{pane_index} #{pane_width} z=#{window_zoomed_flag} a=#{pane_active}",
    )
}

/// tmux 3.7b unzooms a window whenever one of its panes goes away: a
/// non-zoomed pane exiting from a three-pane zoomed window leaves the
/// other two laid out, not one of them stretched with the flag still set.
#[test]
fn a_pane_exiting_from_a_zoomed_window_unzooms_it() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "z",
        "-x",
        "80",
        "-y",
        "24",
        "sleep 30",
    ])
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("go");
    srv.cmd(&["set", "-g", "remain-on-exit", "off"]).unwrap();
    srv.cmd(&["split-window", "-h", "-t", "z", "sleep 30"])
        .unwrap();
    srv.cmd(&["split-window", "-h", "-t", "z", &exit_on(&flag, 0)])
        .unwrap();
    srv.cmd(&["resize-pane", "-Z", "-t", "z:1.1"]).unwrap();
    assert_eq!(display(&srv, "z:1.1", "#{window_zoomed_flag}"), "1");
    touch(&flag);

    srv.wait_for(|| panes(&srv, "z", "#{pane_id}").len() == 2, WAIT)
        .unwrap();
    assert_eq!(zoom_state(&srv, "z"), ["1 40 z=0 a=1", "2 39 z=0 a=0"]);
}

/// The zoomed pane itself exiting, in a three-pane window and in a
/// two-pane window (which drops to one pane): unzoomed both times.
#[test]
fn the_zoomed_pane_exiting_unzooms_its_window() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "z",
        "-x",
        "80",
        "-y",
        "24",
        "sleep 30",
    ])
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (first, second) = (dir.path().join("go1"), dir.path().join("go2"));
    srv.cmd(&["set", "-g", "remain-on-exit", "off"]).unwrap();
    srv.cmd(&["split-window", "-h", "-t", "z", "sleep 30"])
        .unwrap();
    srv.cmd(&["split-window", "-h", "-t", "z", &exit_on(&first, 0)])
        .unwrap();
    srv.cmd(&["resize-pane", "-Z", "-t", "z:1.3"]).unwrap();
    assert_eq!(display(&srv, "z:1.3", "#{pane_width}"), "80");
    touch(&first);
    srv.wait_for(|| panes(&srv, "z", "#{pane_id}").len() == 2, WAIT)
        .unwrap();
    assert_eq!(zoom_state(&srv, "z"), ["1 40 z=0 a=0", "2 39 z=0 a=1"]);

    srv.cmd(&["new-window", "-t", "z", "sleep 30"]).unwrap();
    srv.cmd(&["split-window", "-h", "-t", "z:2", &exit_on(&second, 0)])
        .unwrap();
    srv.cmd(&["resize-pane", "-Z", "-t", "z:2.2"]).unwrap();
    assert_eq!(display(&srv, "z:2.2", "#{window_zoomed_flag}"), "1");
    touch(&second);
    srv.wait_for(|| panes(&srv, "z:2", "#{pane_id}").len() == 1, WAIT)
        .unwrap();
    assert_eq!(zoom_state(&srv, "z:2"), ["1 80 z=0 a=1"]);
}

// --- connection counting -------------------------------------------------

/// A subscriber is an open connection like any other: it keeps an empty
/// auto-started server up, and its close lets the server exit.
#[test]
fn a_subscriber_keeps_an_empty_auto_started_server_until_it_closes() {
    let srv = TestServer::unstarted();
    let mut cmd = srv.command();
    cmd.args(["__server", "--exit-when-empty"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let server = cmd.spawn().expect("spawn server");
    srv.wait_listening(WAIT).expect("server comes up");
    let mut sub = srv.subscribe(&["option-changed"]).unwrap();

    // A first request that leaves no session arms the exit-when-empty rule.
    let out = srv.run(&["new-session", "-d", "-x", "not-a-width"]);
    assert!(!out.status.success(), "{out:?}");
    thread::sleep(QUIET);
    assert!(
        srv.is_running(),
        "exited under a subscriber; log:\n{}",
        srv.log()
    );
    srv.cmd(&["set", "-g", "@still", "1"]).unwrap();
    assert_eq!(next(&mut sub), option_changed("@still", Some("1")));

    drop(sub);
    let status = common::wait_child(server, WAIT).expect("server exits once the subscriber closes");
    assert!(status.success(), "{status:?}");
    assert!(!common::exists(&srv.socket), "socket left behind");
}
