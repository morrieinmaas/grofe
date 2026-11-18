//! Typing held while a file paste is on its way, and the upload's ends.

use super::*;

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
    f.handle(
        Input::Keys(vec![b'x'; crate::remote::forward::typing::MAX_QUEUED]),
        now,
    );
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
