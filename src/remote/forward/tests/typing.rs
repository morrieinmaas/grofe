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
    let (_d, a, b) = files();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let text = format!("{} {}", escaped(&a), escaped(&b));
    let got = run(
        &mut f,
        Input::Keys(bracketed(text.as_bytes())),
        Instant::now(),
    );
    let [Action::ResolveDir(paste)] = got.as_slice() else {
        panic!("{got:?}");
    };
    let paste = paste.clone();
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
                "grofe: upload of b failed: broken pipe (1 of 2 uploaded, their paths pasted)"
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
            id: 1,
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
    // (A lone Esc is decided once the escape time passes.)
    assert_eq!(f.handle(keys(b"\x1b"), now), []);
    assert_eq!(
        f.handle(Input::Tick, now + HOLD_TIMEOUT),
        [Action::Send(want)]
    );
    assert_eq!(f.handle(keys(b"x"), now), [Action::Send(b"x".to_vec())]);
    // The check answering late changes nothing.
    let late = Input::FilesChecked {
        id: 1,
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

#[test]
fn a_late_check_of_a_paste_given_up_never_counts_for_the_same_paste_again() {
    let (_d, a, _) = files();
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let paste = bracketed(escaped(&a).as_bytes());
    let first = f.handle(Input::Keys(paste.clone()), now);
    let [Action::CheckFiles { id: stale, .. }] = first.as_slice() else {
        panic!("{first:?}");
    };
    let stale = *stale;
    // Esc gives it up; the same file is dropped again.
    f.handle(keys(b"\x1b"), now);
    f.handle(Input::Tick, now + HOLD_TIMEOUT);
    let again = f.handle(Input::Keys(paste), now);
    let [Action::CheckFiles { id: fresh, .. }] = again.as_slice() else {
        panic!("{again:?}");
    };
    assert_ne!(*fresh, stale);
    // The first check answers late, with the same text: nothing happens,
    // and typing still waits for the second.
    let late = Input::FilesChecked {
        id: stale,
        original: escaped(&a).into_bytes(),
        files: Some(vec![(a.clone(), 3_000_000)]),
    };
    assert_eq!(f.handle(late, now), []);
    assert_eq!(f.handle(keys(b"ls"), now), []);
}

#[test]
fn typing_and_c_c_in_one_read_keep_the_typing() {
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let paste = b"\x1b[200~/hung/file\x1b[201~";
    f.handle(keys(paste), now);
    // `ls`, C-c and `pwd` in one read (an Esc there would be M-p): the
    // paste as text, then `ls`, then `pwd` typed as usual.
    let mut want = paste.to_vec();
    want.extend(b"lspwd");
    assert_eq!(f.handle(keys(b"ls\x03pwd"), now), [Action::Send(want)]);
}

#[test]
fn esc_then_a_bracketed_paste_in_one_read_keeps_the_second_paste_whole() {
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let held = b"\x1b[200~/hung/file\x1b[201~";
    f.handle(keys(held), now);
    // Esc gives the held paste up; the paste after it stays a bracketed one.
    let read = b"\x1b\x1b[200~line1\nline2\x1b[201~";
    let mut want = held.to_vec();
    want.extend(b"\x1b[200~line1\nline2\x1b[201~");
    assert_eq!(f.handle(keys(read), now), [Action::Send(want)]);
}

#[test]
fn esc_then_a_bracketed_paste_during_an_upload_waits_whole() {
    let (_d, a, _) = files();
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let paste = uploading(&mut f, &a, now);
    let read = b"\x1b\x1b[200~line1\nline2\x1b[201~";
    assert_eq!(f.handle(keys(read), now), [Action::CancelUpload]);
    let cancelled = Input::Uploaded {
        paste,
        arrived: Vec::new(),
        error: None,
        cancelled: true,
        grew: Vec::new(),
    };
    // Once the upload stops, the second paste goes, still bracketed.
    assert_eq!(
        f.handle(cancelled, now),
        [
            Action::Message("grofe: upload cancelled".into()),
            Action::Send(b"\x1b[200~line1\nline2\x1b[201~".to_vec())
        ]
    );
}

#[test]
fn meta_o_then_c_c_in_one_read_are_two_keys() {
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let held = b"\x1b[200~/hung/file\x1b[201~";
    f.handle(keys(held), now);
    // M-O is held typing; the C-c after it gives the paste up, and nothing
    // of it reaches the shell later.
    let mut want = held.to_vec();
    want.extend(b"\x1bO");
    assert_eq!(f.handle(keys(b"\x1bO\x03"), now), [Action::Send(want)]);
    assert_eq!(f.handle(keys(b"x"), now), [Action::Send(b"x".to_vec())]);
}

/// Cancelled upload of `paste`, as the hook reports it.
fn cancelled(paste: FilePaste) -> Input {
    Input::Uploaded {
        paste,
        arrived: Vec::new(),
        error: None,
        cancelled: true,
        grew: Vec::new(),
    }
}

#[test]
fn a_paste_over_several_reads_during_an_upload_is_text_not_keys() {
    let (_d, a, _) = files();
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let paste = uploading(&mut f, &a, now);
    // An Esc at the end of one read, and C-c, the prefix and a lone Esc in
    // the next, all inside the paste: text, no cancel, nothing through.
    assert_eq!(f.handle(keys(b"\x1b[200~abc\x1b"), now), []);
    assert_eq!(f.handle(keys(b"\x03\x02def"), now), []);
    assert_eq!(f.handle(keys(b"\x1b"), now), []);
    assert_eq!(f.handle(keys(b"ghi\x1b[201~"), now), []);
    // Then the user cancels for real; the held paste comes out whole.
    assert_eq!(f.handle(keys(b"\x03"), now), [Action::CancelUpload]);
    assert_eq!(
        f.handle(cancelled(paste), now),
        [
            Action::Message("grofe: upload cancelled".into()),
            Action::Send(b"\x1b[200~abc\x1b\x03\x02def\x1bghi\x1b[201~".to_vec())
        ]
    );
}

#[test]
fn an_end_marker_split_across_reads_still_ends_the_paste() {
    let (_d, a, _) = files();
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let paste = uploading(&mut f, &a, now);
    assert_eq!(f.handle(keys(b"\x1b[200~text\x1b"), now), []);
    assert_eq!(f.handle(keys(b"[201~"), now), []);
    // After the paste ended, C-c is a key again.
    assert_eq!(f.handle(keys(b"\x03"), now), [Action::CancelUpload]);
    assert_eq!(
        f.handle(cancelled(paste), now),
        [
            Action::Message("grofe: upload cancelled".into()),
            Action::Send(b"\x1b[200~text\x1b[201~".to_vec())
        ]
    );
}

#[test]
fn a_paste_over_several_reads_while_one_is_checked_stays_one_paste() {
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let held = b"\x1b[200~/hung/file\x1b[201~";
    let got = f.handle(keys(held), now);
    let [Action::CheckFiles { id, .. }] = got.as_slice() else {
        panic!("{got:?}");
    };
    let id = *id;
    assert_eq!(f.handle(keys(b"\x1b[200~one"), now), []);
    assert_eq!(f.handle(keys(b"\x03two\x1b[201~"), now), []);
    // The check finds no file: the held paste as text, then the second
    // paste, still one bracketed paste.
    let checked = Input::FilesChecked {
        id,
        original: b"/hung/file".to_vec(),
        files: None,
    };
    let mut want = held.to_vec();
    want.extend(b"\x1b[200~one\x03two\x1b[201~");
    assert_eq!(f.handle(checked, now), [Action::Send(want)]);
}

/// Cancels the upload of `paste` with C-c at `now`; the bytes then sent.
fn cancel_and_release(f: &mut Forwarding, paste: FilePaste, now: Instant) -> Vec<u8> {
    assert_eq!(f.handle(keys(b"\x03"), now), [Action::CancelUpload]);
    let got = f.handle(cancelled(paste), now);
    got.into_iter()
        .filter_map(|a| match a {
            Action::Send(b) => Some(b),
            _ => None,
        })
        .flatten()
        .collect()
}

#[test]
fn a_paste_that_would_pass_the_cap_is_dropped_whole() {
    use crate::remote::forward::typing::MAX_QUEUED;
    let (_d, a, _) = files();
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let paste = uploading(&mut f, &a, now);
    assert_eq!(
        f.handle(Input::Keys(vec![b'k'; MAX_QUEUED - 3000]), now),
        []
    );
    let mut open = b"\x1b[200~".to_vec();
    open.extend(vec![b'p'; 4000]);
    assert_eq!(f.handle(Input::Keys(open), now), []);
    let got = f.handle(keys(b"x\r\x1b[201~"), now);
    assert!(matches!(got.as_slice(), [Action::Message(_)]), "{got:?}");
    // Nothing of the paste comes out later: no bare `x\r`.
    let sent = cancel_and_release(&mut f, paste, now);
    assert_eq!(sent, vec![b'k'; MAX_QUEUED - 3000]);
}

#[test]
fn a_paste_that_never_ends_stops_holding_keys_after_the_paste_timeout() {
    let (_d, a, _) = files();
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let paste = uploading(&mut f, &a, now);
    assert_eq!(f.handle(keys(b"\x1b[200~abc"), now), []);
    // Inside the paste C-c is text; after the decoder's paste timeout it
    // is a key again.
    assert_eq!(f.handle(keys(b"\x03"), now), []);
    let later = now + crate::server::input::PASTE_TIMEOUT;
    assert_eq!(cancel_and_release(&mut f, paste, later), b"");
    // Released, the unended paste goes the usual way: out once its own
    // wait passes.
    assert_eq!(
        f.handle(Input::Tick, later + PASTE_TIMEOUT),
        [Action::Send(b"\x1b[200~abc\x03".to_vec())]
    );
}

#[test]
fn alt_arrow_is_one_held_key_not_a_cancel() {
    let (_d, a, _) = files();
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let paste = uploading(&mut f, &a, now);
    assert_eq!(f.handle(keys(b"\x1b\x1b[A"), now), []);
    assert_eq!(f.handle(Input::Tick, now + HOLD_TIMEOUT), []);
    assert_eq!(cancel_and_release(&mut f, paste, now), b"\x1b\x1b[A");
}

#[test]
fn a_lone_esc_or_a_split_paste_start_waits_for_the_next_read() {
    let (_d, a, _) = files();
    let now = Instant::now();
    let mut f = fw(UploadMode::On, ForwardMode::Ask);
    let paste = uploading(&mut f, &a, now);
    // A lone Esc at a read's end, the rest of an arrow in the next.
    assert_eq!(f.handle(keys(b"ab\x1b"), now), []);
    assert_eq!(f.handle(keys(b"[A"), now), []);
    // A paste start split across reads: its text (C-c included) is text.
    assert_eq!(f.handle(keys(b"\x1b[20"), now), []);
    assert_eq!(f.handle(keys(b"0~x\x03y\x1b[201~"), now), []);
    assert_eq!(
        cancel_and_release(&mut f, paste, now),
        b"ab\x1b[A\x1b[200~x\x03y\x1b[201~"
    );
}
