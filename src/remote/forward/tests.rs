//! The state machine's tests.

use std::path::Path;

use super::*;

fn pane(n: u32) -> PaneId {
    format!("%{n}").parse().unwrap()
}

/// [`Forwarding::handle`], with the worker's path checks run and fed back
/// as [`ForwardHook`] does.
fn run(f: &mut Forwarding, input: Input, now: Instant) -> Vec<Action> {
    let mut out = Vec::new();
    let mut inputs = vec![input];
    while let Some(input) = inputs.pop() {
        for action in f.handle(input, now) {
            match action {
                Action::CheckFiles { paths, original } => inputs.push(Input::FilesChecked {
                    original,
                    files: check_files(paths),
                }),
                Action::CheckOpen(url) => {
                    inputs.push(Input::OpenChecked(text::local_file_to_open(&url)))
                }
                other => out.push(other),
            }
        }
    }
    out
}

fn keys(s: &[u8]) -> Input {
    Input::Keys(s.to_vec())
}

fn ports(p: u32, list: &[u16]) -> Input {
    Input::Event(Event::PortsChanged {
        pane: pane(p),
        ports: list.to_vec(),
    })
}

fn fw(upload: UploadMode, ports: ForwardMode) -> Forwarding {
    Forwarding::new(
        "host",
        Settings {
            upload_paste: upload,
            forward_ports: ports,
            ..Settings::default()
        },
    )
}

/// Two temp files and their paths.
fn files() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a b.txt");
    let b = dir.path().join("it's.png");
    std::fs::write(&a, vec![0u8; 3_000_000]).unwrap();
    std::fs::write(&b, vec![0u8; 100_000]).unwrap();
    (dir, a, b)
}

fn escaped(p: &Path) -> String {
    p.display()
        .to_string()
        .replace(' ', "\\ ")
        .replace('\'', "\\'")
}

#[test]
fn keys_pass_through() {
    let mut f = fw(UploadMode::Ask, ForwardMode::Ask);
    let now = Instant::now();
    assert_eq!(
        run(&mut f, keys(b"ls\r"), now),
        [Action::Send(b"ls\r".to_vec())]
    );
    // A lone escape and an arrow key are not held.
    assert_eq!(
        run(&mut f, keys(b"\x1b"), now),
        [Action::Send(b"\x1b".to_vec())]
    );
    assert_eq!(
        run(&mut f, keys(b"\x1b[A"), now),
        [Action::Send(b"\x1b[A".to_vec())]
    );
}

#[test]
fn a_text_paste_passes_through_whole() {
    let mut f = fw(UploadMode::Ask, ForwardMode::Ask);
    let now = Instant::now();
    let paste = b"x\x1b[200~hello world\x1b[201~y";
    assert_eq!(
        run(&mut f, keys(paste), now),
        [Action::Send(paste.to_vec())]
    );
}

#[test]
fn a_file_paste_split_across_reads_asks_and_uploads() {
    let (_d, a, b) = files();
    let mut f = fw(UploadMode::Ask, ForwardMode::Ask);
    let now = Instant::now();
    let text = format!("{} {}", escaped(&a), escaped(&b));
    let mut all = b"\x1b[200~".to_vec();
    all.extend(text.as_bytes());
    all.extend(b"\x1b[201~");
    // Cut inside the start marker, the content and the end marker.
    let cuts = [3, 10, all.len() - 2];
    let mut got = Vec::new();
    let mut from = 0;
    for &cut in cuts.iter().chain([all.len()].iter()) {
        got.extend(run(&mut f, keys(&all[from..cut]), now));
        from = cut;
    }
    let paste = FilePaste {
        files: vec![a.clone(), b.clone()],
        sizes: vec![3_000_000, 100_000],
        total: 3_100_000,
        original: text.clone().into_bytes(),
    };
    assert_eq!(got, [Action::ResolveDir(paste.clone())]);
    let dir = UploadDir {
        path: "/home/u/src".into(),
        shown: "~/src".into(),
    };
    assert_eq!(
        run(&mut f, Input::DirResolved(paste.clone(), dir), now),
        [Action::Prompt(
            "upload 2 files (3.1 MB) to host:~/src? [y/N]".into()
        )]
    );
    assert_eq!(
        run(&mut f, Input::Answer("y".into()), now),
        [Action::Upload {
            dir: "/home/u/src".into(),
            paste: paste.clone()
        }]
    );
    assert_eq!(
        run(
            &mut f,
            Input::Uploaded {
                paste,
                arrived: vec!["/r/a b.txt".into(), "/r/it's.png".into()],
                error: None,
                cancelled: false,
                grew: Vec::new()
            },
            now
        ),
        [Action::Send(
            b"\x1b[200~'/r/a b.txt' '/r/it'\\''s.png'\x1b[201~".to_vec()
        )]
    );
}

#[test]
fn declining_an_upload_pastes_the_text() {
    let (_d, a, _) = files();
    let mut f = fw(UploadMode::Ask, ForwardMode::Ask);
    let now = Instant::now();
    let got = run(&mut f, Input::Keys(bracketed(escaped(&a).as_bytes())), now);
    let [Action::ResolveDir(paste)] = got.as_slice() else {
        panic!("{got:?}");
    };
    let dir = UploadDir {
        path: "/x".into(),
        shown: "/x".into(),
    };
    run(&mut f, Input::DirResolved(paste.clone(), dir), now);
    let want = bracketed(escaped(&a).as_bytes());
    assert_eq!(
        run(&mut f, Input::Answer("".into()), now),
        [Action::Send(want)]
    );
}

#[test]
fn upload_on_skips_the_prompt_and_off_passes_through() {
    let (_d, a, _) = files();
    let now = Instant::now();
    let paste = bracketed(escaped(&a).as_bytes());
    let mut on = fw(UploadMode::On, ForwardMode::Ask);
    let got = run(&mut on, Input::Keys(paste.clone()), now);
    let [Action::ResolveDir(p)] = got.as_slice() else {
        panic!("no resolve");
    };
    let dir = UploadDir {
        path: "/x".into(),
        shown: "/x".into(),
    };
    assert_eq!(
        run(&mut on, Input::DirResolved(p.clone(), dir), now),
        [Action::Upload {
            dir: "/x".into(),
            paste: p.clone()
        }]
    );
    assert_eq!(p.files, std::slice::from_ref(&a));
    let mut off = fw(UploadMode::Off, ForwardMode::Ask);
    assert_eq!(
        run(&mut off, Input::Keys(paste.clone()), now),
        [Action::Send(paste)]
    );
}

#[test]
fn missing_relative_and_directory_paths_pass_through() {
    let (d, a, _) = files();
    let now = Instant::now();
    for text in [
        format!("{} /no/such/file", escaped(&a)),
        "relative/path".to_string(),
        d.path().display().to_string(),
    ] {
        let mut f = fw(UploadMode::On, ForwardMode::Ask);
        let paste = bracketed(text.as_bytes());
        assert_eq!(
            run(&mut f, Input::Keys(paste.clone()), now),
            [Action::Send(paste)],
            "{text}"
        );
    }
}

#[test]
fn over_the_cap_says_so_and_pastes_the_text() {
    let (_d, a, b) = files();
    let mut f = Forwarding::new(
        "host",
        Settings {
            upload_max: 1 << 20,
            ..Settings::default()
        },
    );
    let paste = bracketed(format!("{} {}", escaped(&a), escaped(&b)).as_bytes());
    assert_eq!(
        run(&mut f, Input::Keys(paste.clone()), Instant::now()),
        [
            Action::Message(
                "grofe: not uploading 2 files (3.1 MB): over remote-upload-max (1 MiB)".into()
            ),
            Action::Send(paste)
        ]
    );
}

#[test]
fn a_failed_upload_says_so_and_pastes_the_text() {
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let paste = FilePaste {
        files: vec!["/a".into()],
        sizes: vec![1],
        total: 1,
        original: b"/a".to_vec(),
    };
    let failed = Input::Uploaded {
        paste,
        arrived: Vec::new(),
        error: Some("upload of a failed: broken pipe".into()),
        cancelled: false,
        grew: Vec::new(),
    };
    assert_eq!(
        run(&mut f, failed, Instant::now()),
        [
            Action::Message("grofe: upload of a failed: broken pipe".into()),
            Action::Send(b"\x1b[200~/a\x1b[201~".to_vec())
        ]
    );
}

#[test]
fn a_paste_with_no_end_goes_out_after_a_while_and_a_long_one_at_once() {
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let t0 = Instant::now();
    assert_eq!(run(&mut f, keys(b"\x1b[200~/a"), t0), []);
    assert_eq!(
        run(&mut f, Input::Tick, t0 + Duration::from_millis(100)),
        []
    );
    assert_eq!(
        run(&mut f, Input::Tick, t0 + PASTE_TIMEOUT),
        [Action::Send(b"\x1b[200~/a".to_vec())]
    );
    let long = [b"\x1b[200~".as_slice(), &vec![b'x'; MAX_PASTE + 1]].concat();
    assert_eq!(
        run(&mut f, Input::Keys(long.clone()), t0),
        [Action::Send(long)]
    );
    // A split start marker is held briefly.
    assert_eq!(
        run(&mut f, keys(b"a\x1b["), t0),
        [Action::Send(b"a".to_vec())]
    );
    assert_eq!(
        run(&mut f, Input::Tick, t0 + HOLD_TIMEOUT),
        [Action::Send(b"\x1b[".to_vec())]
    );
}

#[test]
fn ports_ask_and_a_yes_forwards() {
    let mut f = fw(UploadMode::Ask, ForwardMode::Ask);
    let now = Instant::now();
    assert_eq!(
        run(&mut f, ports(1, &[5173]), now),
        [Action::Offer(forward_prompt("host", 5173))]
    );
    assert_eq!(
        run(&mut f, Input::Answer("y".into()), now),
        [Action::StartForward(5173)]
    );
    assert_eq!(
        run(
            &mut f,
            Input::Forwarded {
                remote: 5173,
                local: Ok(5174)
            },
            now
        ),
        [Action::Message(
            "forwarding host:5173 to localhost:5174".into()
        )]
    );
    // The same port again, from another pane, asks nothing.
    assert_eq!(run(&mut f, ports(2, &[5173]), now), []);
    // Gone from one pane, still open in the other.
    assert_eq!(run(&mut f, ports(1, &[]), now), []);
    assert_eq!(run(&mut f, ports(2, &[]), now), [Action::StopForward(5173)]);
    assert!(f.forwarded().is_empty());
}

#[test]
fn ports_queue_their_prompts_and_always_switches_to_auto() {
    let mut f = fw(UploadMode::Ask, ForwardMode::Ask);
    let now = Instant::now();
    assert_eq!(
        run(&mut f, ports(1, &[3000, 4000]), now),
        [Action::Offer(forward_prompt("host", 3000))]
    );
    assert_eq!(
        run(&mut f, Input::Answer("always".into()), now),
        [Action::StartForward(3000), Action::StartForward(4000)]
    );
    assert_eq!(f.settings().forward_ports, ForwardMode::Auto);
    assert_eq!(
        run(&mut f, ports(1, &[3000, 4000, 5000]), now),
        [Action::StartForward(5000)]
    );
}

#[test]
fn a_no_is_remembered_until_the_port_closes() {
    let mut f = fw(UploadMode::Ask, ForwardMode::Ask);
    let now = Instant::now();
    run(&mut f, ports(1, &[8080]), now);
    assert_eq!(run(&mut f, Input::Answer("n".into()), now), []);
    assert_eq!(run(&mut f, ports(2, &[8080]), now), []);
    run(&mut f, ports(1, &[]), now);
    run(&mut f, ports(2, &[]), now);
    assert_eq!(
        run(&mut f, ports(1, &[8080]), now),
        [Action::Offer(forward_prompt("host", 8080))]
    );
}

#[test]
fn a_port_closing_while_asked_takes_its_prompt_away() {
    let mut f = fw(UploadMode::Ask, ForwardMode::Ask);
    let now = Instant::now();
    run(&mut f, ports(1, &[1, 2]), now);
    assert_eq!(
        run(&mut f, ports(1, &[2]), now),
        [
            Action::ClearPrompt,
            Action::Offer(forward_prompt("host", 2))
        ]
    );
    // A pane exiting closes its ports.
    let exited = Input::Event(Event::PaneExited {
        pane: pane(1),
        uid: String::new(),
        status: None,
    });
    assert_eq!(run(&mut f, exited, now), [Action::ClearPrompt]);
}

#[test]
fn auto_forwards_off_ignores_and_a_failed_start_is_reported() {
    let now = Instant::now();
    let mut off = fw(UploadMode::Ask, ForwardMode::Off);
    assert_eq!(run(&mut off, ports(1, &[80]), now), []);
    let mut auto = fw(UploadMode::Ask, ForwardMode::Auto);
    assert_eq!(
        run(&mut auto, ports(1, &[80]), now),
        [Action::StartForward(80)]
    );
    assert_eq!(
        run(
            &mut auto,
            Input::Forwarded {
                remote: 80,
                local: Err("no free local port from 80".into())
            },
            now
        ),
        [Action::Message(
            "grofe: cannot forward host:80: no free local port from 80".into()
        )]
    );
    assert!(auto.forwarded().is_empty());
}

#[test]
fn open_url_events_open_and_a_refusal_is_a_message() {
    let mut f = fw(UploadMode::Ask, ForwardMode::Ask);
    let now = Instant::now();
    let ev = Input::Event(Event::OpenUrl {
        url: "https://a/".into(),
        pane: None,
    });
    assert_eq!(run(&mut f, ev, now), [Action::OpenUrl("https://a/".into())]);
    assert_eq!(
        run(&mut f, Input::Opened(Err("bad".into())), now),
        [Action::Message("grofe: not opening a url: bad".into())]
    );
}

#[test]
fn a_file_url_from_the_remote_asks_first_and_a_runnable_one_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let doc = dir.path().join("report.pdf");
    std::fs::write(&doc, "x").unwrap();
    // The offer names the real path (macOS: /tmp is a link).
    let doc = std::fs::canonicalize(&doc).unwrap();
    let exe = dir.path().join("tool");
    std::fs::write(&exe, "x").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
    let url = |p: &Path| format!("file://{}", p.display());
    let open = |u: String| Input::Event(Event::OpenUrl { url: u, pane: None });
    let mut f = fw(UploadMode::Ask, ForwardMode::Ask);
    let now = Instant::now();
    assert_eq!(
        run(&mut f, open(url(&doc)), now),
        [Action::Offer(format!("open {}? [y/N]", url(&doc)))]
    );
    assert_eq!(
        run(&mut f, Input::Answer("y".into()), now),
        [Action::OpenUrl(url(&doc))]
    );
    run(&mut f, open(url(&doc)), now);
    assert_eq!(run(&mut f, Input::Answer("".into()), now), []);
    assert_eq!(
        run(&mut f, open(url(&exe)), now),
        [Action::Message(
            "grofe: not opening a url: an executable file is not opened".into()
        )]
    );
    // http and https open without asking, as before.
    assert_eq!(
        run(&mut f, open("https://a/".into()), now),
        [Action::OpenUrl("https://a/".into())]
    );
}

#[test]
fn a_resync_replaces_what_the_events_said() {
    let mut f = fw(UploadMode::Ask, ForwardMode::Auto);
    let now = Instant::now();
    assert_eq!(
        run(&mut f, ports(1, &[3000]), now),
        [Action::StartForward(3000)]
    );
    // Another session now: pane 1 is not in it, pane 2 listens on 4000.
    assert_eq!(
        run(&mut f, Input::Resync(vec![(pane(2), vec![4000])]), now),
        [Action::StopForward(3000), Action::StartForward(4000)]
    );
    assert_eq!(f.forwarded(), [4000]);
    // The same again changes nothing.
    assert_eq!(
        run(&mut f, Input::Resync(vec![(pane(2), vec![4000])]), now),
        []
    );
}

#[test]
fn settings_come_from_the_options_given_with_defaults_for_the_rest() {
    let local = |name: &str| match name {
        "remote-upload-paste" => Some("on".to_string()),
        "remote-upload-max" => Some("5".to_string()),
        "remote-forward-ports" => Some("bogus".to_string()),
        _ => None,
    };
    assert_eq!(
        Settings::from_options(local),
        Settings {
            upload_paste: UploadMode::On,
            upload_max: 5 << 20,
            forward_ports: ForwardMode::Ask,
        }
    );
    assert_eq!(Settings::from_options(|_| None), Settings::default());
}

#[test]
fn keys_typed_during_a_file_paste_go_after_its_text() {
    let (_d, a, _) = files();
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    // The paste and an Enter in one read, then more keys while it uploads.
    let mut read = bracketed(escaped(&a).as_bytes());
    read.extend(b"\r");
    let got = run(&mut f, Input::Keys(read), now);
    let [Action::ResolveDir(paste)] = got.as_slice() else {
        panic!("{got:?}");
    };
    let paste = paste.clone();
    assert_eq!(run(&mut f, keys(b" x"), now), []);
    let dir = UploadDir {
        path: "/r".into(),
        shown: "/r".into(),
    };
    let got = run(&mut f, Input::DirResolved(paste.clone(), dir), now);
    assert!(matches!(got.as_slice(), [Action::Upload { .. }]), "{got:?}");
    assert_eq!(run(&mut f, keys(b"y"), now), []);
    assert_eq!(
        run(
            &mut f,
            Input::Uploaded {
                paste,
                arrived: vec!["/r/a b.txt".into()],
                error: None,
                cancelled: false,
                grew: Vec::new()
            },
            now
        ),
        [Action::Send(
            b"\x1b[200~'/r/a b.txt'\x1b[201~\r xy".to_vec()
        )]
    );
    // Then keys go straight through again.
    assert_eq!(run(&mut f, keys(b"z"), now), [Action::Send(b"z".to_vec())]);
}

#[test]
fn keys_wait_for_a_declined_upload_too() {
    let (_d, a, _) = files();
    let now = Instant::now();
    let mut f = fw(UploadMode::Ask, ForwardMode::Ask);
    let got = run(&mut f, Input::Keys(bracketed(escaped(&a).as_bytes())), now);
    let [Action::ResolveDir(paste)] = got.as_slice() else {
        panic!("{got:?}");
    };
    let dir = UploadDir {
        path: "/r".into(),
        shown: "/r".into(),
    };
    run(&mut f, Input::DirResolved(paste.clone(), dir), now);
    // (While the prompt is up the client sends keys to it; these came
    // before it showed.)
    assert_eq!(run(&mut f, keys(b"ls"), now), []);
    let mut want = bracketed(escaped(&a).as_bytes());
    want.extend(b"ls");
    assert_eq!(
        run(&mut f, Input::Answer("n".into()), now),
        [Action::Send(want)]
    );
}

#[test]
fn a_prompt_that_went_unanswered_is_shown_again() {
    let (_d, a, _) = files();
    let now = Instant::now();
    let mut f = fw(UploadMode::Ask, ForwardMode::Ask);
    let got = run(&mut f, Input::Keys(bracketed(escaped(&a).as_bytes())), now);
    let [Action::ResolveDir(paste)] = got.as_slice() else {
        panic!("{got:?}");
    };
    let dir = UploadDir {
        path: "/r".into(),
        shown: "/r".into(),
    };
    let shown = run(&mut f, Input::DirResolved(paste.clone(), dir), now);
    let [Action::Prompt(text)] = shown.as_slice() else {
        panic!("{shown:?}");
    };
    // The link drops with the prompt up: nothing shows until asked again.
    f.dropped();
    assert_eq!(run(&mut f, keys(b"ls"), now), [], "held for the paste");
    assert_eq!(f.reshow(), [Action::Prompt(text.clone())]);
    assert_eq!(f.reshow(), [], "once");
    let mut want = bracketed(escaped(&a).as_bytes());
    want.extend(b"ls");
    assert_eq!(
        run(&mut f, Input::Answer("n".into()), now),
        [Action::Send(want)]
    );
    assert_eq!(run(&mut f, keys(b"x"), now), [Action::Send(b"x".to_vec())]);
}

#[test]
fn a_new_question_after_a_dropped_one_shows_the_first_again() {
    let mut f = fw(UploadMode::Ask, ForwardMode::Ask);
    let now = Instant::now();
    run(&mut f, ports(1, &[3000]), now);
    f.dropped();
    // Not stuck behind a prompt nobody sees: the first comes back up.
    assert_eq!(
        run(&mut f, ports(1, &[3000, 4000]), now),
        [Action::Offer(forward_prompt("host", 3000))]
    );
    assert_eq!(
        run(&mut f, Input::Answer("n".into()), now),
        [Action::Offer(forward_prompt("host", 4000))]
    );
}

#[test]
fn an_upload_that_fails_part_way_pastes_what_arrived() {
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let paste = FilePaste {
        files: vec!["/a".into(), "/b".into(), "/c".into()],
        sizes: vec![1, 1, 1],
        total: 3,
        original: b"/a /b /c".to_vec(),
    };
    let part = Input::Uploaded {
        paste,
        arrived: vec!["/r/a".into()],
        error: Some("upload of b failed: broken pipe".into()),
        cancelled: false,
        grew: Vec::new(),
    };
    assert_eq!(
        run(&mut f, part, Instant::now()),
        [
            Action::Message(
                "grofe: upload of b failed: broken pipe (1 of 3 uploaded, their paths pasted)"
                    .into()
            ),
            Action::Send(b"\x1b[200~'/r/a'\x1b[201~".to_vec())
        ]
    );
}

#[test]
fn path_checks_go_to_the_worker_not_the_input_thread() {
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let now = Instant::now();
    // A path on a hung mount would block a metadata call: none is made here.
    let got = f.handle(keys(b"\x1b[200~/hung/mount/file\x1b[201~"), now);
    assert_eq!(
        got,
        [Action::CheckFiles {
            paths: vec!["/hung/mount/file".into()],
            original: b"/hung/mount/file".to_vec()
        }]
    );
    let open = Input::Event(Event::OpenUrl {
        url: "file:///hung/mount/doc.pdf".into(),
        pane: None,
    });
    assert_eq!(
        f.handle(open, now),
        [Action::CheckOpen("file:///hung/mount/doc.pdf".into())]
    );
}

/// A paste of `a` taken as far as its upload, with `f` in mode on.
fn uploading(f: &mut Forwarding, a: &Path, now: Instant) -> FilePaste {
    let got = run(f, Input::Keys(bracketed(escaped(a).as_bytes())), now);
    let [Action::ResolveDir(paste)] = got.as_slice() else {
        panic!("{got:?}");
    };
    let paste = paste.clone();
    let dir = UploadDir {
        path: "/r".into(),
        shown: "/r".into(),
    };
    let got = run(f, Input::DirResolved(paste.clone(), dir), now);
    assert!(matches!(got.as_slice(), [Action::Upload { .. }]), "{got:?}");
    paste
}

#[test]
fn the_prefix_goes_through_and_c_c_cancels_while_an_upload_runs() {
    let (_d, a, _) = files();
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let paste = uploading(&mut f, &a, now);
    assert_eq!(run(&mut f, keys(b"ls"), now), []);
    // prefix+d: on to the server, so the client detaches.
    assert_eq!(
        run(&mut f, keys(b"\x02"), now),
        [Action::Send(b"\x02".to_vec())]
    );
    assert_eq!(run(&mut f, keys(b"d"), now), [Action::Send(b"d".to_vec())]);
    assert_eq!(run(&mut f, keys(b"\x03"), now), [Action::CancelUpload]);
    let cancelled = Input::Uploaded {
        paste,
        arrived: Vec::new(),
        error: Some("upload cancelled".into()),
        cancelled: true,
        grew: Vec::new(),
    };
    // Nothing pasted (the user said stop); the held typing goes on.
    assert_eq!(
        run(&mut f, cancelled, now),
        [
            Action::Message("grofe: upload cancelled".into()),
            Action::Send(b"ls".to_vec())
        ]
    );
}

#[test]
fn a_file_that_grew_while_uploading_is_named() {
    let (_d, a, _) = files();
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let paste = uploading(&mut f, &a, now);
    let done = Input::Uploaded {
        paste,
        arrived: vec!["/r/a b.txt".into()],
        error: None,
        cancelled: false,
        grew: vec!["a b.txt".into()],
    };
    assert_eq!(
        run(&mut f, done, now),
        [
            Action::Message(
                "grofe: a b.txt grew while uploading; only its size when pasted was sent".into()
            ),
            Action::Send(b"\x1b[200~'/r/a b.txt'\x1b[201~".to_vec())
        ]
    );
}

#[test]
fn esc_while_a_paste_is_looked_at_gives_it_up_as_text() {
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let paste = b"\x1b[200~/hung/mount/file\x1b[201~";
    // The check runs elsewhere (and may never come back).
    let got = f.handle(keys(paste), now);
    assert!(
        matches!(got.as_slice(), [Action::CheckFiles { .. }]),
        "{got:?}"
    );
    assert_eq!(f.handle(keys(b"ls"), now), []);
    // Esc: the paste as text, then what was typed, and typing flows again.
    let mut want = paste.to_vec();
    want.extend(b"ls");
    assert_eq!(f.handle(keys(b"\x1b"), now), [Action::Send(want)]);
    assert_eq!(f.handle(keys(b"x"), now), [Action::Send(b"x".to_vec())]);
    // The check answering late changes nothing.
    let late = Input::FilesChecked {
        original: b"/hung/mount/file".to_vec(),
        files: Some(vec![("/hung/mount/file".into(), 1)]),
    };
    assert_eq!(f.handle(late, now), []);
}

#[test]
fn c_c_while_the_directory_is_looked_up_gives_the_paste_up_too() {
    let (_d, a, _) = files();
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let got = run(&mut f, Input::Keys(bracketed(escaped(&a).as_bytes())), now);
    let [Action::ResolveDir(paste)] = got.as_slice() else {
        panic!("{got:?}");
    };
    let paste = paste.clone();
    assert_eq!(
        f.handle(keys(b"\x03"), now),
        [Action::Send(bracketed(escaped(&a).as_bytes()))]
    );
    let dir = UploadDir {
        path: "/r".into(),
        shown: "/r".into(),
    };
    assert_eq!(
        f.handle(Input::DirResolved(paste, dir), now),
        [],
        "no upload"
    );
}

#[test]
fn the_typing_cap_message_says_what_esc_does_now() {
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    f.handle(keys(b"\x1b[200~/hung/file\x1b[201~"), now);
    f.handle(Input::Keys(vec![b'x'; typing::MAX_QUEUED]), now);
    assert_eq!(
        f.handle(keys(b"y"), now),
        [Action::Message(
            "grofe: typing dropped while the paste is looked at (Esc pastes it as text)".into()
        )]
    );
}

#[test]
fn a_known_prefix_is_kept() {
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let c_a = crate::server::input::parse_prefix("C-a").unwrap().unwrap();
    f.handle(Input::Prefix(c_a), now);
    f.handle(keys(b"\x1b[200~/hung/file\x1b[201~"), now);
    // The session's C-a goes through; C-b is just typing now.
    assert_eq!(
        f.handle(keys(b"\x01"), now),
        [Action::Send(b"\x01".to_vec())]
    );
    assert_eq!(f.handle(keys(b"d"), now), [Action::Send(b"d".to_vec())]);
    assert_eq!(f.handle(keys(b"\x02"), now), []);
}
