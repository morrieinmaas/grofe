use super::*;

const ET: Duration = Duration::from_millis(25);

fn k(name: &str) -> Key {
    parse_key(name).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn key(name: &str) -> Input {
    Input::Key(k(name))
}

fn decode(bytes: &[u8]) -> Vec<Input> {
    let t = Instant::now();
    let mut d = Decoder::new();
    let mut out = d.feed(bytes, t, ET);
    out.extend(d.flush(t + ET, ET));
    out
}

fn whole(data: &[u8]) -> Input {
    Input::Paste {
        data: data.to_vec(),
        start: true,
        end: true,
    }
}

fn modes(kitty_flags: u8) -> InputModes {
    InputModes {
        kitty_flags,
        ..InputModes::default()
    }
}

// ---- key names ----

/// Recorded on tmux 3.7b (`tmux -L grofe-t23 -f /dev/null`,
/// `set -g prefix X; show -gv prefix`).
#[test]
fn key_names_parse_and_print_as_tmux_does() {
    let cases = [
        ("C-b", "C-b"),
        ("c-B", "C-B"),
        ("PgUp", "PPage"),
        ("pageup", "PPage"),
        ("ppage", "PPage"),
        ("IC", "IC"),
        ("dc", "DC"),
        ("BTAB", "BTab"),
        ("bspace", "BSpace"),
        ("M-C-x", "C-M-x"),
        ("S-Up", "S-Up"),
        ("^a", "C-a"),
        ("C-?", "C-?"),
        ("F12", "F12"),
        ("Escape", "Escape"),
        ("space", "Space"),
        ("%", "%"),
        ("M--", "M--"),
        ("-", "-"),
        ("C-Space", "C-Space"),
        ("M-Enter", "M-Enter"),
        ("é", "é"),
        // tmux keeps shift as written: these are keys of their own.
        ("C-S-a", "C-S-a"),
        ("S-a", "S-a"),
        ("M-S-a", "M-S-a"),
        ("S-Tab", "S-Tab"),
        ("C-S-A", "C-S-A"),
        ("S-BTab", "S-BTab"),
        ("C-BSpace", "C-BSpace"),
    ];
    for (input, shown) in cases {
        assert_eq!(k(input).to_string(), shown, "{input}");
        assert_eq!(k(shown), k(input), "{shown} round-trips");
    }
}

#[test]
fn bad_key_names_are_refused_with_tmux_wording() {
    for bad in ["foo", "F13", "", "C-", "Nonsense", "ab"] {
        assert_eq!(parse_key(bad), Err(format!("bad key: {bad}")), "{bad}");
    }
}

#[test]
fn prefix_accepts_none() {
    assert_eq!(parse_prefix("None"), Ok(None));
    assert_eq!(parse_prefix("none"), Ok(None));
    assert_eq!(parse_prefix("C-a"), Ok(Some(k("C-a"))));
    assert_eq!(parse_prefix("foo"), Err("bad key: foo".to_string()));
}

/// tmux 3.7b lists C-A, C-S-a, S-a and A as four bindings, and S-Tab
/// and BTab as two, so they stay distinct keys; only matching folds.
#[test]
fn shift_spellings_are_distinct_keys_that_fold_alike() {
    let distinct = [
        ("C-A", "C-S-a"),
        ("S-a", "A"),
        ("M-S-a", "M-A"),
        ("S-Tab", "BTab"),
    ];
    for (x, y) in distinct {
        assert_ne!(k(x), k(y), "{x} vs {y}");
        assert_eq!(k(x).folded(), k(y).folded(), "{x} folds like {y}");
        assert!(k(x).matches(&k(y)));
    }
    assert!(!k("S-Up").matches(&k("Up")));
    assert!(!k("a").matches(&k("A")));

    let mut t = KeyTables::default();
    t.bind(ROOT_TABLE, k("C-A"), argv(&["one"]), false);
    t.bind(ROOT_TABLE, k("C-S-a"), argv(&["two"]), false);
    assert_eq!(
        t.get(ROOT_TABLE, &k("C-A")).map(|b| &b.cmds[0][0][..]),
        Some("one")
    );
    assert_eq!(
        t.get(ROOT_TABLE, &k("C-S-a")).map(|b| &b.cmds[0][0][..]),
        Some("two")
    );
    t.bind(ROOT_TABLE, k("S-Tab"), argv(&["three"]), false);
    t.bind(ROOT_TABLE, k("BTab"), argv(&["four"]), false);
    assert_eq!(
        t.get(ROOT_TABLE, &k("S-Tab")).map(|b| &b.cmds[0][0][..]),
        Some("three")
    );
    assert_eq!(
        t.get(ROOT_TABLE, &k("BTab")).map(|b| &b.cmds[0][0][..]),
        Some("four")
    );
    assert!(t.unbind(ROOT_TABLE, &k("C-S-a")));
    assert_eq!(
        t.get(ROOT_TABLE, &k("C-A")).map(|b| &b.cmds[0][0][..]),
        Some("one")
    );
}

/// A binding matches whichever spelling the terminal sends.
#[test]
fn bindings_match_the_folded_spelling() {
    let mut t = KeyTables::default();
    t.bind(ROOT_TABLE, k("C-A"), argv(&["ca"]), false);
    t.bind(ROOT_TABLE, k("S-Tab"), argv(&["stab"]), false);
    t.bind(ROOT_TABLE, k("M-S-a"), argv(&["msa"]), false);
    t.bind(PREFIX_TABLE, k("BTab"), argv(&["btab"]), false);
    let got = |table, bytes: &[u8]| {
        let inputs = decode(bytes);
        let [Input::Key(key)] = inputs.as_slice() else {
            panic!("{bytes:?}");
        };
        t.get(table, key).map(|b| b.cmds[0][0].clone())
    };
    // kitty ctrl+shift+a (C-S-a) finds C-A.
    assert_eq!(got(ROOT_TABLE, b"\x1b[97;6u"), Some("ca".into()));
    // legacy back-tab (BTab) finds S-Tab; kitty shift+Tab finds BTab.
    assert_eq!(got(ROOT_TABLE, b"\x1b[Z"), Some("stab".into()));
    assert_eq!(got(PREFIX_TABLE, b"\x1b[9;2u"), Some("btab".into()));
    // A binding written S-Tab still catches kitty shift+Tab.
    assert_eq!(got(ROOT_TABLE, b"\x1b[9;2u"), Some("stab".into()));
    // legacy meta-A (M-A) finds M-S-a.
    assert_eq!(got(ROOT_TABLE, b"\x1bA"), Some("msa".into()));
}

// ---- decoding ----

#[test]
fn decodes_text_and_controls() {
    let cases: &[(&[u8], &[&str])] = &[
        (b"a", &["a"]),
        (b"A", &["A"]),
        (b"%", &["%"]),
        (b" ", &["Space"]),
        (b"\r", &["Enter"]),
        (b"\t", &["Tab"]),
        (b"\x7f", &["BSpace"]),
        (b"\x02", &["C-b"]),
        (b"\x01\x1a", &["C-a", "C-z"]),
        (b"\x08", &["C-h"]),
        (b"\x0a", &["C-j"]),
        (b"\x00", &["C-Space"]),
        (b"\x1c\x1d\x1e\x1f", &["C-\\", "C-]", "C-^", "C-_"]),
        ("é€😀".as_bytes(), &["é", "€", "😀"]),
        (b"ab", &["a", "b"]),
    ];
    for (bytes, want) in cases {
        let want: Vec<Input> = want.iter().map(|n| key(n)).collect();
        assert_eq!(decode(bytes), want, "{bytes:?}");
    }
}

#[test]
fn decodes_legacy_csi_and_ss3_keys() {
    let cases: &[(&[u8], &str)] = &[
        (b"\x1b[A", "Up"),
        (b"\x1b[B", "Down"),
        (b"\x1b[C", "Right"),
        (b"\x1b[D", "Left"),
        (b"\x1bOA", "Up"),
        (b"\x1bOD", "Left"),
        (b"\x1b[H", "Home"),
        (b"\x1b[F", "End"),
        (b"\x1bOH", "Home"),
        (b"\x1bOF", "End"),
        (b"\x1b[1~", "Home"),
        (b"\x1b[7~", "Home"),
        (b"\x1b[4~", "End"),
        (b"\x1b[8~", "End"),
        (b"\x1b[2~", "IC"),
        (b"\x1b[3~", "DC"),
        (b"\x1b[5~", "PPage"),
        (b"\x1b[6~", "NPage"),
        (b"\x1bOP", "F1"),
        (b"\x1bOQ", "F2"),
        (b"\x1bOR", "F3"),
        (b"\x1bOS", "F4"),
        (b"\x1b[P", "F1"),
        (b"\x1b[13~", "F3"),
        (b"\x1b[15~", "F5"),
        (b"\x1b[17~", "F6"),
        (b"\x1b[21~", "F10"),
        (b"\x1b[24~", "F12"),
        (b"\x1b[Z", "BTab"),
        // A terminal in application keypad mode: tmux's keypad keys.
        (b"\x1bOM", "KPEnter"),
        (b"\x1bOp", "KP0"),
        (b"\x1bOy", "KP9"),
        (b"\x1bOj", "KP*"),
        (b"\x1bOn", "KP."),
        // xterm modifier parameter: 2 shift, 3 alt, 5 ctrl, 6 C-S, 7 C-M.
        (b"\x1b[1;2A", "S-Up"),
        (b"\x1b[1;3A", "M-Up"),
        (b"\x1b[1;5D", "C-Left"),
        (b"\x1b[1;6A", "C-S-Up"),
        (b"\x1b[1;7A", "C-M-Up"),
        (b"\x1b[1;5H", "C-Home"),
        (b"\x1b[3;5~", "C-DC"),
        (b"\x1b[5;2~", "S-PPage"),
        (b"\x1b[1;2P", "S-F1"),
        (b"\x1b[15;5~", "C-F5"),
    ];
    for (bytes, want) in cases {
        assert_eq!(decode(bytes), vec![key(want)], "{bytes:?}");
    }
}

#[test]
fn decodes_meta_as_an_escape_prefix() {
    let cases: &[(&[u8], &str)] = &[
        (b"\x1ba", "M-a"),
        (b"\x1bA", "M-A"),
        (b"\x1b\x01", "C-M-a"),
        (b"\x1b\r", "M-Enter"),
        (b"\x1b\x7f", "M-BSpace"),
        (b"\x1b\x1b", "M-Escape"),
        (b"\x1b\x1b[A", "M-Up"),
        ("\x1bé".as_bytes(), "M-é"),
    ];
    for (bytes, want) in cases {
        assert_eq!(decode(bytes), vec![key(want)], "{bytes:?}");
    }
}

/// The forms a terminal sends with kitty flags 1 pushed (what T2.1
/// asks for), plus the richer forms flags 2, 4, 8 and 16 add.
#[test]
fn decodes_kitty_csi_u() {
    let cases: &[(&[u8], &str)] = &[
        (b"\x1b[27u", "Escape"),
        (b"\x1b[13u", "Enter"),
        (b"\x1b[9u", "Tab"),
        (b"\x1b[127u", "BSpace"),
        (b"\x1b[97;5u", "C-a"),
        (b"\x1b[98;5u", "C-b"),
        (b"\x1b[97;3u", "M-a"),
        (b"\x1b[97;7u", "C-M-a"),
        (b"\x1b[97;4u", "M-S-a"),
        (b"\x1b[97;6u", "C-S-a"),
        (b"\x1b[13;2u", "S-Enter"),
        (b"\x1b[13;5u", "C-Enter"),
        (b"\x1b[9;2u", "BTab"),
        (b"\x1b[9;6u", "C-BTab"),
        (b"\x1b[27;2;9~", "BTab"),
        (b"\x1b[27;3u", "M-Escape"),
        (b"\x1b[32;5u", "C-Space"),
        (b"\x1b[37;5u", "C-%"),
        // flag 4: shifted key; flag 8/16: all keys, with text.
        (b"\x1b[97:65;2u", "A"),
        (b"\x1b[49:33;2u", "!"),
        (b"\x1b[113u", "q"),
        (b"\x1b[97;;97u", "a"),
        (b"\x1b[97;2;65u", "S-a"),
        (b"\x1b[0;;229u", "å"),
        // flag 2: press and repeat are presses.
        (b"\x1b[97;5:1u", "C-a"),
        (b"\x1b[97;5:2u", "C-a"),
        // lock bits are ignored.
        (b"\x1b[97;69u", "C-a"),
        // functional keys in the private use area.
        (b"\x1b[57399u", "0"),
        (b"\x1b[57414u", "Enter"),
        (b"\x1b[57419u", "Up"),
    ];
    for (bytes, want) in cases {
        assert_eq!(decode(bytes), vec![key(want)], "{bytes:?}");
    }
    // F13 has no tmux name but decodes.
    assert_eq!(
        decode(b"\x1b[57376u"),
        vec![Input::Key(Key::plain(KeyCode::F(13)))]
    );
}

#[test]
fn kitty_releases_and_modifier_keys_are_dropped() {
    assert_eq!(decode(b"\x1b[97;5:3u"), vec![]);
    // Releases of functional keys use the legacy finals.
    assert_eq!(decode(b"\x1b[1;5:3A"), vec![]);
    assert_eq!(decode(b"\x1b[3;1:3~"), vec![]);
    assert_eq!(decode(b"\x1b[1;1:3P"), vec![]);
    assert_eq!(decode(b"\x1b[1;5:2A"), vec![key("C-Up")]);
    assert_eq!(decode(b"\x1b[57441;2u"), vec![]); // left shift
    assert_eq!(decode(b"\x1b[57358u"), vec![]); // caps lock
}

#[test]
fn decodes_modify_other_keys() {
    assert_eq!(decode(b"\x1b[27;5;97~"), vec![key("C-a")]);
    assert_eq!(decode(b"\x1b[27;2;13~"), vec![key("S-Enter")]);
}

#[test]
fn decodes_sgr_mouse() {
    let m = |kind, col, row, mods| {
        Input::Mouse(Mouse {
            kind,
            col,
            row,
            mods,
        })
    };
    let none = Mods::default();
    let ctrl = Mods {
        ctrl: true,
        ..Mods::default()
    };
    assert_eq!(
        decode(b"\x1b[<0;10;5M"),
        vec![m(MouseKind::Down(MouseButton::Left), 9, 4, none)]
    );
    assert_eq!(
        decode(b"\x1b[<2;1;1m"),
        vec![m(MouseKind::Up(MouseButton::Right), 0, 0, none)]
    );
    assert_eq!(
        decode(b"\x1b[<32;3;4M"),
        vec![m(MouseKind::Drag(MouseButton::Left), 2, 3, none)]
    );
    assert_eq!(
        decode(b"\x1b[<35;3;4M"),
        vec![m(MouseKind::Move, 2, 3, none)]
    );
    assert_eq!(
        decode(b"\x1b[<64;1;1M"),
        vec![m(MouseKind::WheelUp, 0, 0, none)]
    );
    assert_eq!(
        decode(b"\x1b[<81;1;1M"),
        vec![m(MouseKind::WheelDown, 0, 0, ctrl)]
    );
    assert_eq!(
        decode(b"\x1b[<129;2;2M"),
        vec![m(MouseKind::Down(MouseButton::Other(9)), 1, 1, none)]
    );
}

#[test]
fn decodes_focus_reports() {
    assert_eq!(
        decode(b"\x1b[I\x1b[O"),
        vec![Input::Focus(true), Input::Focus(false)]
    );
}

#[test]
fn unknown_sequences_and_replies_are_dropped() {
    // A stray cursor position report, DA1 reply, kitty flags reply.
    assert_eq!(decode(b"\x1b[12;40R"), vec![]);
    assert_eq!(decode(b"\x1b[?62;22c"), vec![]);
    assert_eq!(decode(b"\x1b[?1u"), vec![]);
    assert_eq!(decode(b"\x1b[99x"), vec![]);
    assert_eq!(decode(b"\x1b[5xa"), vec![key("a")]);
}

#[test]
fn unknown_ss3_is_meta_o_and_the_next_byte_stays_a_key() {
    assert_eq!(decode(b"\x1bOz"), vec![key("M-O"), key("z")]);
    assert_eq!(decode(b"\x1bO\x1b[A"), vec![key("M-O"), key("Up")]);
}

#[test]
fn broken_utf8_never_swallows_the_next_key() {
    assert_eq!(decode(b"\xc3a"), vec![key("a")]);
    assert_eq!(decode(b"\xe2\x82a"), vec![key("a")]);
    assert_eq!(decode(b"\xf0\x9f\x1b[A"), vec![key("Up")]);
    assert_eq!(decode(b"\xc3\xa9\xff\x01"), vec![key("é"), key("C-a")]);
    // Incomplete at the deadline: the lead and its continuations go.
    assert_eq!(decode(b"\xe2\x82"), vec![]);
}

#[test]
fn paste_is_one_opaque_blob() {
    let got = decode(b"a\x1b[200~\x02%\x1b[A\x1b\x1b[201~b");
    assert_eq!(got, vec![key("a"), whole(b"\x02%\x1b[A\x1b"), key("b")]);
}

#[test]
fn paste_split_across_reads_waits_for_its_end_marker() {
    let t = Instant::now();
    let mut d = Decoder::new();
    assert_eq!(d.feed(b"\x1b[200~hello \x1b[20", t, ET), vec![]);
    // A paste does not end at escape-time; it waits 5 s for data.
    assert_eq!(d.deadline(ET), Some(t + PASTE_TIMEOUT));
    assert_eq!(d.flush(t + Duration::from_secs(1), ET), vec![]);
    let t1 = t + Duration::from_secs(4);
    assert_eq!(d.feed(b"1~x", t1, ET), vec![whole(b"hello "), key("x")]);
}

#[test]
fn a_paste_without_its_end_marker_ends_after_5s_without_data() {
    let t = Instant::now();
    let mut d = Decoder::new();
    assert_eq!(d.feed(b"\x1b[200~abc", t, ET), vec![]);
    let t1 = t + Duration::from_secs(3);
    assert_eq!(d.feed(b"def", t1, ET), vec![]);
    // Each read restarts the wait.
    assert_eq!(d.deadline(ET), Some(t1 + PASTE_TIMEOUT));
    assert_eq!(d.flush(t + PASTE_TIMEOUT, ET), vec![]);
    assert_eq!(d.flush(t1 + PASTE_TIMEOUT, ET), vec![whole(b"abcdef")]);
    assert_eq!(d.deadline(ET), None);
    // Keys are keys again.
    assert_eq!(d.feed(b"\x02", t1 + PASTE_TIMEOUT, ET), vec![key("C-b")]);
}

/// Feeds 2.5 MiB of paste in 64 KiB reads with `end` as the last read
/// (split across two reads), and returns the pane stream for a
/// bracketed-paste pane.
fn stream_long_paste(tail: &[&[u8]], flush_at: Option<Instant>) -> (Vec<u8>, Vec<u8>, Decoder) {
    let t = Instant::now();
    let mut d = Decoder::new();
    let mut state = KeyState::new();
    let bracketed = InputModes {
        bracketed_paste: true,
        ..InputModes::default()
    };
    let mut stream = Vec::new();
    let route = |out: Vec<Input>, state: &mut KeyState, stream: &mut Vec<u8>| {
        for i in out {
            assert!(matches!(i, Input::Paste { .. }), "{i:?}");
            let settings = KeySettings::default();
            match state.route(&i, t, &KeyTables::default(), &settings, &bracketed) {
                Routed::ToPane(b) => stream.extend(b),
                r => panic!("{r:?}"),
            }
        }
    };
    route(d.feed(PASTE_START, t, ET), &mut state, &mut stream);
    let chunk: Vec<u8> = (0..64 * 1024).map(|i| b'a' + (i % 26) as u8).collect();
    let mut sent = Vec::new();
    for _ in 0..40 {
        route(d.feed(&chunk, t, ET), &mut state, &mut stream);
        sent.extend_from_slice(&chunk);
        assert!(d.buf.len() <= PASTE_CAP + chunk.len(), "{}", d.buf.len());
    }
    for part in tail {
        route(d.feed(part, t, ET), &mut state, &mut stream);
    }
    if let Some(at) = flush_at {
        route(d.flush(at, ET), &mut state, &mut stream);
    }
    (stream, sent, d)
}

fn count(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

#[test]
fn a_long_paste_streams_as_one_bracketed_paste() {
    // The end marker is split across two reads.
    let (stream, mut sent, d) = stream_long_paste(&[b"tail\x1b[2", b"01~"], None);
    sent.extend_from_slice(b"tail");
    assert!(!d.in_paste);
    assert!(d.buf.is_empty());
    assert_eq!(count(&stream, PASTE_START), 1);
    assert_eq!(count(&stream, PASTE_END), 1);
    assert!(stream.starts_with(PASTE_START));
    assert!(stream.ends_with(PASTE_END));
    let body = &stream[PASTE_START.len()..stream.len() - PASTE_END.len()];
    assert_eq!(body.len(), sent.len());
    assert!(body == sent.as_slice());
}

#[test]
fn a_long_paste_that_times_out_still_closes_its_bracket() {
    let late = Instant::now() + Duration::from_secs(60);
    let (stream, sent, d) = stream_long_paste(&[], Some(late));
    assert!(!d.in_paste);
    assert_eq!(count(&stream, PASTE_START), 1);
    assert_eq!(count(&stream, PASTE_END), 1);
    assert!(stream.ends_with(PASTE_END));
    let body = &stream[PASTE_START.len()..stream.len() - PASTE_END.len()];
    assert!(body == sent.as_slice());
}

#[test]
fn sequences_split_across_reads_join_within_escape_time() {
    let t = Instant::now();
    let mut d = Decoder::new();
    assert_eq!(d.feed(b"\x1b[1;", t, ET), vec![]);
    assert_eq!(
        d.feed(b"5A", t + Duration::from_millis(10), ET),
        vec![key("C-Up")]
    );
    assert_eq!(d.feed(&[0xc3], t, ET), vec![]);
    assert_eq!(d.feed(&[0xa9], t, ET), vec![key("é")]);
}

// ---- escape-time (injected clock) ----

#[test]
fn escape_time_resolves_a_lone_esc() {
    let t = Instant::now();
    let mut d = Decoder::new();
    assert_eq!(d.feed(b"\x1b", t, ET), vec![]);
    assert_eq!(d.deadline(ET), Some(t + ET));
    assert_eq!(d.flush(t + Duration::from_millis(24), ET), vec![]);
    assert_eq!(d.flush(t + ET, ET), vec![key("Escape")]);
    assert_eq!(d.deadline(ET), None);
    assert_eq!(d.flush(t + ET * 2, ET), vec![]);
}

#[test]
fn esc_then_a_key_within_escape_time_is_meta() {
    let t = Instant::now();
    let mut d = Decoder::new();
    assert_eq!(d.feed(b"\x1b", t, ET), vec![]);
    assert_eq!(
        d.feed(b"x", t + Duration::from_millis(20), ET),
        vec![key("M-x")]
    );
}

#[test]
fn esc_then_a_key_after_escape_time_is_two_keys() {
    let t = Instant::now();
    let mut d = Decoder::new();
    assert_eq!(d.feed(b"\x1b", t, ET), vec![]);
    // The timer did not fire before the next read: feed resolves it.
    assert_eq!(
        d.feed(b"x", t + Duration::from_millis(30), ET),
        vec![key("Escape"), key("x")]
    );
}

#[test]
fn escape_time_resolves_an_incomplete_sequence_to_keys() {
    let t = Instant::now();
    let mut d = Decoder::new();
    assert_eq!(d.feed(b"\x1b[", t, ET), vec![]);
    assert_eq!(d.flush(t + ET, ET), vec![key("M-[")]);
    assert_eq!(d.feed(b"\x1bO", t, ET), vec![]);
    assert_eq!(d.flush(t + ET, ET), vec![key("M-O")]);
    assert_eq!(d.feed(b"\x1b\x1b", t, ET), vec![]);
    assert_eq!(d.flush(t + ET, ET), vec![key("M-Escape")]);
}

#[test]
fn zero_escape_time_resolves_at_once() {
    let t = Instant::now();
    let mut d = Decoder::new();
    assert_eq!(d.feed(b"\x1b", t, Duration::ZERO), vec![key("Escape")]);
}

#[test]
fn the_wait_starts_when_the_bytes_arrive() {
    let t = Instant::now();
    let mut d = Decoder::new();
    assert_eq!(d.feed(b"a", t, ET), vec![key("a")]);
    let later = t + Duration::from_secs(1);
    assert_eq!(d.feed(b"\x1b", later, ET), vec![]);
    assert_eq!(d.deadline(ET), Some(later + ET));
}

// ---- routing ----

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

fn tables() -> KeyTables {
    let mut t = KeyTables::default();
    t.bind(PREFIX_TABLE, k("%"), argv(&["split-window", "-h"]), false);
    t.bind(PREFIX_TABLE, k("C-b"), argv(&["send-prefix"]), false);
    t.bind(PREFIX_TABLE, k("Up"), argv(&["select-pane", "-U"]), true);
    t.bind(
        PREFIX_TABLE,
        k("M-Up"),
        argv(&["resize-pane", "-U", "5"]),
        true,
    );
    t.bind(ROOT_TABLE, k("M-h"), argv(&["select-pane", "-L"]), false);
    t
}

struct Client {
    dec: Decoder,
    state: KeyState,
    tables: KeyTables,
    settings: KeySettings,
    modes: InputModes,
}

impl Client {
    fn new() -> Client {
        Client {
            dec: Decoder::new(),
            state: KeyState::new(),
            tables: tables(),
            settings: KeySettings::default(),
            modes: InputModes::default(),
        }
    }

    fn send(&mut self, bytes: &[u8], now: Instant) -> Vec<Routed> {
        let inputs = self.dec.feed(bytes, now, ET);
        inputs
            .iter()
            .map(|i| {
                self.state
                    .route(i, now, &self.tables, &self.settings, &self.modes)
            })
            .collect()
    }
}

fn cmd(words: &[&str]) -> Routed {
    Routed::Bound(vec![argv(words)], Dialect::Grofe)
}

fn pane(bytes: &[u8]) -> Routed {
    Routed::ToPane(bytes.to_vec())
}

#[test]
fn prefix_then_percent_routes_to_the_bound_command() {
    let t = Instant::now();
    let mut c = Client::new();
    assert_eq!(c.send(b"\x02", t), vec![Routed::Consumed]);
    assert_eq!(c.state.table(), PREFIX_TABLE);
    assert_eq!(c.send(b"%", t), vec![cmd(&["split-window", "-h"])]);
    assert_eq!(c.state.table(), ROOT_TABLE);
    // Back in root, `%` is typing.
    assert_eq!(c.send(b"%", t), vec![pane(b"%")]);
}

/// tmux 3.7b, recorded through an attached client: a table's `Any`
/// takes every key it does not bind, ahead of the retry in root; the
/// prefix key still wins; root's `Any` takes what root leaves, an
/// unbound key after the prefix included.
#[test]
fn any_takes_every_key_its_table_leaves_unbound() {
    let t = Instant::now();
    let mut c = Client::new();
    c.tables.bind("kp", k("Any"), argv(&["any-kp"]), false);
    c.tables
        .bind(ROOT_TABLE, k("C-h"), argv(&["root-ch"]), false);
    c.tables
        .bind(ROOT_TABLE, k("C-q"), argv(&["root-cq"]), false);
    c.tables.bind("kp", k("C-q"), argv(&["kp-cq"]), false);
    for (key, want) in [
        (&b"\x08"[..], "any-kp"),
        (b"q", "any-kp"),
        (b"\x11", "kp-cq"),
    ] {
        c.state.set_table("kp");
        assert_eq!(c.send(key, t), vec![cmd(&[want])], "{key:?}");
        assert_eq!(c.state.table(), ROOT_TABLE);
    }
    c.state.set_table("kp");
    assert_eq!(c.send(b"\x02", t), vec![Routed::Consumed]);
    assert_eq!(c.state.table(), PREFIX_TABLE);
    assert_eq!(k("Any").to_string(), "Any");

    c.state.set_table(ROOT_TABLE);
    c.tables
        .bind(ROOT_TABLE, k("Any"), argv(&["any-root"]), false);
    assert_eq!(c.send(b"z", t), vec![cmd(&["any-root"])]);
    assert_eq!(c.send(b"\x08", t), vec![cmd(&["root-ch"])]);
    assert_eq!(
        c.send(b"\x02Q", t),
        vec![Routed::Consumed, cmd(&["any-root"])]
    );
}

#[test]
fn prefix_and_key_in_one_read_route_the_same() {
    let t = Instant::now();
    let mut c = Client::new();
    assert_eq!(
        c.send(b"\x02%", t),
        vec![Routed::Consumed, cmd(&["split-window", "-h"])]
    );
}

#[test]
fn kitty_spelled_prefix_matches_too() {
    let t = Instant::now();
    let mut c = Client::new();
    assert_eq!(
        c.send(b"\x1b[98;5u%", t),
        vec![Routed::Consumed, cmd(&["split-window", "-h"])]
    );
}

#[test]
fn paste_containing_the_prefix_key_is_never_matched() {
    let t = Instant::now();
    let mut c = Client::new();
    assert_eq!(
        c.send(b"\x1b[200~x\x02%y\x1b[201~", t),
        vec![pane(b"x\x02%y")]
    );
    assert_eq!(c.state.table(), ROOT_TABLE);
    // A pane in mode 2004 gets the markers back.
    c.modes.bracketed_paste = true;
    assert_eq!(
        c.send(b"\x1b[200~\x02%\x1b[201~", t),
        vec![pane(b"\x1b[200~\x02%\x1b[201~")]
    );
    assert_eq!(c.state.table(), ROOT_TABLE);
}

#[test]
fn paste_after_the_prefix_is_not_matched_either() {
    let t = Instant::now();
    let mut c = Client::new();
    assert_eq!(
        c.send(b"\x02\x1b[200~%\x1b[201~", t),
        vec![Routed::Consumed, pane(b"%")]
    );
    // tmux 3.7b (recorded through an attached client on a private
    // socket): the paste goes to the pane and the client is back in
    // root, so the next key is typing.
    assert_eq!(c.state.table(), ROOT_TABLE);
    assert_eq!(c.send(b"%", t), vec![pane(b"%")]);
    // A paste also ends a repeat.
    c.send(b"\x02\x1b[A", t);
    assert!(c.state.deadline().is_some());
    c.send(b"\x1b[200~x\x1b[201~", t);
    assert_eq!(c.state.deadline(), None);
    assert_eq!(c.send(b"\x1b[A", t), vec![pane(b"\x1b[A")]);
}

/// tmux 3.7b, through an attached client on a private socket: with both
/// S-Tab and BTab bound, kitty `9;2u` fires BTab; unbound, it puts
/// `\e[Z` on the pane.
#[test]
fn kitty_shift_tab_is_btab() {
    let t = Instant::now();
    let mut c = Client::new();
    c.tables
        .bind(ROOT_TABLE, k("S-Tab"), argv(&["stab"]), false);
    c.tables.bind(ROOT_TABLE, k("BTab"), argv(&["btab"]), false);
    assert_eq!(c.send(b"\x1b[9;2u", t), vec![cmd(&["btab"])]);
    assert_eq!(c.send(b"\x1b[27;2;9~", t), vec![cmd(&["btab"])]);
    let mut c = Client::new();
    assert_eq!(c.send(b"\x1b[9;2u", t), vec![pane(b"\x1b[Z")]);
}

#[test]
fn kitty_ctrl_shift_letter_hits_a_prefix_written_c_s() {
    let t = Instant::now();
    let mut c = Client::new();
    c.settings.prefix = Some(k("C-S-a"));
    assert_eq!(
        c.send(b"\x1b[97;6u%", t),
        vec![Routed::Consumed, cmd(&["split-window", "-h"])]
    );
    c.settings.prefix = Some(k("C-A"));
    assert_eq!(c.send(b"\x1b[97;6u", t), vec![Routed::Consumed]);
}

#[test]
fn unbound_key_after_the_prefix_is_dropped() {
    let t = Instant::now();
    let mut c = Client::new();
    assert_eq!(
        c.send(b"\x02z", t),
        vec![Routed::Consumed, Routed::Consumed]
    );
    assert_eq!(c.state.table(), ROOT_TABLE);
    assert_eq!(c.send(b"z", t), vec![pane(b"z")]);
}

#[test]
fn prefix_twice_runs_the_prefix_tables_binding() {
    let t = Instant::now();
    let mut c = Client::new();
    assert_eq!(
        c.send(b"\x02\x02", t),
        vec![Routed::Consumed, cmd(&["send-prefix"])]
    );
}

#[test]
fn root_bindings_run_without_the_prefix() {
    let t = Instant::now();
    let mut c = Client::new();
    assert_eq!(c.send(b"\x1bh", t), vec![cmd(&["select-pane", "-L"])]);
    assert_eq!(c.send(b"\x1bj", t), vec![pane(b"\x1bj")]);
}

#[test]
fn repeat_keys_run_again_without_the_prefix_within_repeat_time() {
    let t = Instant::now();
    let mut c = Client::new();
    let up = cmd(&["select-pane", "-U"]);
    assert_eq!(c.send(b"\x02\x1b[A", t), vec![Routed::Consumed, up.clone()]);
    assert_eq!(c.state.deadline(), Some(t + Duration::from_millis(500)));
    let t1 = t + Duration::from_millis(400);
    assert_eq!(c.send(b"\x1b[A", t1), vec![up.clone()]);
    // Each repeat restarts the timer.
    let t2 = t1 + Duration::from_millis(400);
    assert_eq!(
        c.send(b"\x1b[1;3A", t2),
        vec![cmd(&["resize-pane", "-U", "5"])]
    );
    // After repeat-time the key is the pane's again.
    let t3 = t2 + Duration::from_millis(600);
    assert_eq!(c.send(b"\x1b[A", t3), vec![pane(b"\x1b[A")]);
    assert_eq!(c.state.table(), ROOT_TABLE);
}

#[test]
fn repeat_ends_on_a_non_repeating_or_unbound_key() {
    let t = Instant::now();
    let mut c = Client::new();
    c.send(b"\x02\x1b[A", t);
    // `%` is bound but does not repeat: it is not run, and goes to the
    // pane from root, as tmux does.
    assert_eq!(c.send(b"%", t), vec![pane(b"%")]);
    assert_eq!(c.state.table(), ROOT_TABLE);

    c.send(b"\x02\x1b[A", t);
    assert_eq!(c.send(b"z", t), vec![pane(b"z")]);
    assert_eq!(c.state.deadline(), None);

    // The prefix during a repeat starts a new prefix.
    c.send(b"\x02\x1b[A", t);
    assert_eq!(
        c.send(b"\x02%", t),
        vec![Routed::Consumed, cmd(&["split-window", "-h"])]
    );
}

#[test]
fn expire_reports_the_end_of_a_repeat() {
    let t = Instant::now();
    let mut c = Client::new();
    c.send(b"\x02\x1b[A", t);
    assert!(!c.state.expire(t + Duration::from_millis(499)));
    assert_eq!(c.state.table(), PREFIX_TABLE);
    assert!(c.state.expire(t + Duration::from_millis(500)));
    assert_eq!(c.state.table(), ROOT_TABLE);
}

/// `initial-repeat-time` replaces `repeat-time` for the first press of
/// a repeating key, or a different one, and not for the presses that
/// keep it going.
#[test]
fn initial_repeat_time_is_for_the_first_press_only() {
    let t = Instant::now();
    let mut c = Client::new();
    c.settings.initial_repeat_time = Duration::from_millis(100);
    c.send(b"\x02\x1b[A", t);
    assert_eq!(c.state.deadline(), Some(t + Duration::from_millis(100)));
    let later = t + Duration::from_millis(50);
    c.send(b"\x1b[A", later);
    assert_eq!(c.state.deadline(), Some(later + Duration::from_millis(500)));
    // After the repeat ran out, the next press is a first one again.
    let after = later + Duration::from_secs(1);
    c.state.expire(after);
    c.send(b"\x02\x1b[A", after);
    assert_eq!(c.state.deadline(), Some(after + Duration::from_millis(100)));
}

/// `prefix-timeout` ends the wait for the key after the prefix.
#[test]
fn prefix_timeout_returns_to_root() {
    let t = Instant::now();
    let mut c = Client::new();
    c.settings.prefix_timeout = Duration::from_millis(200);
    c.send(b"\x02", t);
    assert_eq!(c.state.table(), PREFIX_TABLE);
    assert_eq!(c.state.deadline(), Some(t + Duration::from_millis(200)));
    assert!(!c.state.expire(t + Duration::from_millis(199)));
    assert!(c.state.expire(t + Duration::from_millis(200)));
    assert_eq!(c.state.table(), ROOT_TABLE);
    // Without it the prefix waits for ever.
    c.settings.prefix_timeout = Duration::ZERO;
    c.send(b"\x02", t);
    assert_eq!(c.state.deadline(), None);
}

#[test]
fn zero_repeat_time_turns_repeat_off() {
    let t = Instant::now();
    let mut c = Client::new();
    c.settings.repeat_time = Duration::ZERO;
    c.send(b"\x02\x1b[A", t);
    assert_eq!(c.state.table(), ROOT_TABLE);
    assert_eq!(c.send(b"\x1b[A", t), vec![pane(b"\x1b[A")]);
}

#[test]
fn no_prefix_key_means_none_is_special() {
    let t = Instant::now();
    let mut c = Client::new();
    c.settings.prefix = None;
    assert_eq!(c.send(b"\x02", t), vec![pane(b"\x02")]);
    c.settings.prefix = Some(k("C-a"));
    assert_eq!(c.send(b"\x02", t), vec![pane(b"\x02")]);
    assert_eq!(
        c.send(b"\x01%", t),
        vec![Routed::Consumed, cmd(&["split-window", "-h"])]
    );
}

#[test]
fn custom_table_from_switch_client_t() {
    let t = Instant::now();
    let mut c = Client::new();
    c.tables
        .bind("resize", k("h"), argv(&["resize-pane", "-L"]), false);
    c.state.set_table("resize");
    assert_eq!(c.send(b"h", t), vec![cmd(&["resize-pane", "-L"])]);
    assert_eq!(c.state.table(), ROOT_TABLE);
    c.state.set_table("resize");
    assert_eq!(c.send(b"q", t), vec![Routed::Consumed]);
}

#[test]
fn keys_are_encoded_for_the_active_panes_modes() {
    let t = Instant::now();
    let mut c = Client::new();
    c.modes.app_cursor = true;
    assert_eq!(c.send(b"\x1b[A", t), vec![pane(b"\x1bOA")]);
    c.modes = modes(1);
    assert_eq!(c.send(b"\x1b[27u", t), vec![pane(b"\x1b[27u")]);
    c.modes = modes(8);
    assert_eq!(c.send(b"q", t), vec![pane(b"\x1b[113u")]);
}

#[test]
fn mouse_is_consumed_and_focus_follows_mode_1004() {
    let t = Instant::now();
    let mut c = Client::new();
    assert_eq!(c.send(b"\x1b[<0;1;1M", t), vec![Routed::Consumed]);
    assert_eq!(c.send(b"\x1b[I", t), vec![Routed::Consumed]);
    c.modes.focus = true;
    assert_eq!(
        c.send(b"\x1b[I\x1b[O", t),
        vec![pane(b"\x1b[I"), pane(b"\x1b[O")]
    );
}

#[test]
fn unbind_removes_a_binding() {
    let mut t = tables();
    assert!(t.unbind(PREFIX_TABLE, &k("%")));
    assert!(!t.unbind(PREFIX_TABLE, &k("%")));
    assert_eq!(t.get(PREFIX_TABLE, &k("%")), None);
    assert!(t.get(PREFIX_TABLE, &k("C-b")).is_some());
    assert_eq!(t.get("nosuch", &k("C-b")), None);
}

// ---- encoding ----

fn enc(name: &str, m: &InputModes) -> Vec<u8> {
    encode(&k(name), m)
}

/// Legacy bytes, as tmux 3.7b sends them into a pane (recorded with
/// `send-keys` into a raw pty on `tmux -L grofe-t23 -f /dev/null`), and
/// tmux-256color's terminfo, the same as screen-256color's for these
/// (`infocmp -1x`: kcuu1 `\EOA` in keypad
/// transmit mode, khome `\E[1~`, kend `\E[4~`, kf1 `\EOP`, kcbt `\E[Z`).
#[test]
fn legacy_encoding_matches_tmux() {
    let m = InputModes::default();
    let cases: &[(&str, &[u8])] = &[
        ("a", b"a"),
        ("A", b"A"),
        ("é", "é".as_bytes()),
        ("Space", b" "),
        ("Enter", b"\r"),
        ("Tab", b"\t"),
        ("BSpace", b"\x7f"),
        ("Escape", b"\x1b"),
        ("C-a", b"\x01"),
        ("C-b", b"\x02"),
        ("C-A", b"\x01"),
        ("C-Space", b"\x00"),
        ("C-@", b"\x00"),
        ("C-/", b"\x1f"),
        ("C-?", b"\x7f"),
        ("C-[", b"\x1b"),
        ("C-1", b"1"),
        ("C-i", b"\t"),
        ("C-m", b"\r"),
        ("C-h", b"\x08"),
        ("M-a", b"\x1ba"),
        ("M-A", b"\x1bA"),
        ("C-M-a", b"\x1b\x01"),
        ("M-Space", b"\x1b "),
        ("M-Enter", b"\x1b\r"),
        ("M-BSpace", b"\x1b\x7f"),
        ("M-Escape", b"\x1b\x1b"),
        ("S-Enter", b"\r"),
        ("C-Enter", b"\r"),
        ("C-Tab", b"\t"),
        ("BTab", b"\x1b[Z"),
        ("M-BTab", b"\x1b[Z"),
        ("S-Space", b" "),
        ("Up", b"\x1b[A"),
        ("Left", b"\x1b[D"),
        ("M-Up", b"\x1b[1;3A"),
        ("C-S-Up", b"\x1b[1;6A"),
        ("Home", b"\x1b[1~"),
        ("End", b"\x1b[4~"),
        ("C-Home", b"\x1b[1;5H"),
        ("S-End", b"\x1b[1;2F"),
        ("IC", b"\x1b[2~"),
        ("DC", b"\x1b[3~"),
        ("C-DC", b"\x1b[3;5~"),
        ("PPage", b"\x1b[5~"),
        ("NPage", b"\x1b[6~"),
        ("F1", b"\x1bOP"),
        ("F4", b"\x1bOS"),
        ("S-F1", b"\x1b[1;2P"),
        ("F5", b"\x1b[15~"),
        ("M-F5", b"\x1b[15;3~"),
        ("F12", b"\x1b[24~"),
        // Shift spellings, recorded the same way.
        ("S-a", b"a"),
        ("S-A", b"A"),
        ("M-S-a", b"\x1ba"),
        ("C-S-a", b"\x01"),
        ("C-S-A", b"\x01"),
        ("S-1", b"1"),
        ("S-Tab", b"\t"),
        ("C-S-Tab", b"\t"),
        ("M-S-Tab", b"\x1b\t"),
        ("S-BTab", b"\x1b[Z"),
        ("C-BTab", b"\x1b[Z"),
        ("S-BSpace", b"\x7f"),
        ("S-Escape", b"\x1b"),
        ("C-S-Enter", b"\r"),
    ];
    for (name, want) in cases {
        assert_eq!(enc(name, &m), want.to_vec(), "{name}");
    }
    // tmux cannot send these (its send-keys types them as text).
    for name in ["C-BSpace", "C-Escape", "C-M-BSpace", "C-M-Escape"] {
        assert_eq!(enc(name, &m), Vec::<u8>::new(), "{name}");
    }
    // Keys with no legacy form send nothing.
    assert_eq!(encode(&Key::plain(KeyCode::F(13)), &m), Vec::<u8>::new());
}

#[test]
fn app_cursor_mode_uses_ss3_for_unmodified_arrows() {
    let m = InputModes {
        app_cursor: true,
        ..InputModes::default()
    };
    assert_eq!(enc("Up", &m), b"\x1bOA");
    assert_eq!(enc("Right", &m), b"\x1bOC");
    assert_eq!(enc("C-Up", &m), b"\x1b[1;5A");
    assert_eq!(enc("Home", &m), b"\x1b[1~");
}

/// Kitty flag 1 (disambiguate), per the protocol spec: Escape, and
/// ctrl/alt with any key, become `CSI code;mods u`; text keys stay text;
/// unmodified Enter, Tab and Backspace keep their legacy bytes;
/// functional keys keep their legacy forms.
#[test]
fn kitty_disambiguate_encoding() {
    let m = modes(1);
    let cases: &[(&str, &[u8])] = &[
        ("a", b"a"),
        ("A", b"A"),
        ("é", "é".as_bytes()),
        ("Space", b" "),
        ("Escape", b"\x1b[27u"),
        ("M-Escape", b"\x1b[27;3u"),
        ("C-a", b"\x1b[97;5u"),
        ("C-A", b"\x1b[97;6u"),
        ("M-a", b"\x1b[97;3u"),
        ("M-A", b"\x1b[97;4u"),
        ("C-M-a", b"\x1b[97;7u"),
        ("C-Space", b"\x1b[32;5u"),
        ("C-i", b"\x1b[105;5u"),
        ("C-[", b"\x1b[91;5u"),
        ("Enter", b"\r"),
        ("Tab", b"\t"),
        ("BSpace", b"\x7f"),
        ("S-Enter", b"\x1b[13;2u"),
        ("C-Enter", b"\x1b[13;5u"),
        ("M-Enter", b"\x1b[13;3u"),
        ("BTab", b"\x1b[9;2u"),
        ("S-Tab", b"\x1b[9;2u"),
        ("S-a", b"A"),
        ("C-S-a", b"\x1b[97;6u"),
        ("M-S-a", b"\x1b[97;4u"),
        ("C-BSpace", b"\x1b[127;5u"),
        ("Up", b"\x1b[A"),
        ("C-Up", b"\x1b[1;5A"),
        ("Home", b"\x1b[1~"),
        ("C-Home", b"\x1b[1;5H"),
        ("DC", b"\x1b[3~"),
        ("S-DC", b"\x1b[3;2~"),
        ("F1", b"\x1bOP"),
        ("C-F1", b"\x1b[1;5P"),
        ("C-F3", b"\x1b[13;5~"),
        ("F5", b"\x1b[15~"),
    ];
    for (name, want) in cases {
        assert_eq!(enc(name, &m), want.to_vec(), "{name}");
    }
    assert_eq!(encode(&Key::plain(KeyCode::F(13)), &m), b"\x1b[57376u");
    let super_a = Key::new(
        KeyCode::Char('a'),
        Mods {
            super_: true,
            ..Mods::default()
        },
    );
    assert_eq!(encode(&super_a, &m), b"\x1b[97;9u");
}

/// Flag 8 (report all keys as escape codes): text keys too, and
/// Enter/Tab/Backspace, and functional keys in their CSI forms.
#[test]
fn kitty_report_all_keys_encoding() {
    let m = InputModes {
        kitty_flags: 1 | 8,
        app_cursor: true,
        ..InputModes::default()
    };
    let cases: &[(&str, &[u8])] = &[
        ("q", b"\x1b[113u"),
        ("A", b"\x1b[97;2u"),
        ("S-a", b"\x1b[97;2u"),
        ("!", b"\x1b[33u"),
        ("Space", b"\x1b[32u"),
        ("Enter", b"\x1b[13u"),
        ("Tab", b"\x1b[9u"),
        ("BTab", b"\x1b[9;2u"),
        ("BSpace", b"\x1b[127u"),
        ("Escape", b"\x1b[27u"),
        ("C-a", b"\x1b[97;5u"),
        ("Up", b"\x1b[A"),
        ("S-Up", b"\x1b[1;2A"),
        ("Home", b"\x1b[H"),
        ("End", b"\x1b[F"),
        ("IC", b"\x1b[2~"),
        ("F1", b"\x1b[P"),
        ("F3", b"\x1b[13~"),
        ("F4", b"\x1b[S"),
        ("F5", b"\x1b[15~"),
    ];
    for (name, want) in cases {
        assert_eq!(enc(name, &m), want.to_vec(), "{name}");
    }
    // Flag 8 alone behaves the same.
    assert_eq!(enc("q", &modes(8)), b"\x1b[113u");
}

#[test]
fn kitty_alternate_keys_and_associated_text() {
    // Flag 4: the shifted key after a colon, only with shift.
    assert_eq!(enc("A", &modes(8 | 4)), b"\x1b[97:65;2u");
    assert_eq!(enc("a", &modes(8 | 4)), b"\x1b[97u");
    assert_eq!(enc("C-A", &modes(1 | 4)), b"\x1b[97:65;6u");
    // Flag 16 with 8: the text as code points.
    assert_eq!(enc("a", &modes(8 | 16)), b"\x1b[97;;97u");
    assert_eq!(enc("A", &modes(8 | 16)), b"\x1b[97;2;65u");
    assert_eq!(enc("A", &modes(8 | 4 | 16)), b"\x1b[97:65;2;65u");
    assert_eq!(enc("S-a", &modes(8 | 4 | 16)), b"\x1b[97:65;2;65u");
    // Keys that make no text carry none.
    assert_eq!(enc("C-a", &modes(8 | 16)), b"\x1b[97;5u");
    assert_eq!(enc("Enter", &modes(8 | 16)), b"\x1b[13u");
    // Flag 2 adds nothing for presses; flag 16 without 8 is undefined
    // and is ignored.
    assert_eq!(enc("C-a", &modes(1 | 2)), b"\x1b[97;5u");
    assert_eq!(enc("a", &modes(1 | 16)), b"a");
}

#[test]
fn decode_then_encode_round_trips_legacy_bytes() {
    let m = InputModes::default();
    let samples: &[&[u8]] = &[
        b"a",
        b"\x01",
        b"\r",
        b"\t",
        b"\x7f",
        b"\x1b[A",
        b"\x1b[1;5C",
        b"\x1b[3~",
        b"\x1b[15;2~",
        b"\x1bOP",
        b"\x1bx",
        b"\x1b[Z",
    ];
    for bytes in samples {
        let inputs = decode(bytes);
        let [Input::Key(key)] = inputs.as_slice() else {
            panic!("{bytes:?}: {inputs:?}");
        };
        assert_eq!(encode(key, &m), bytes.to_vec(), "{bytes:?}");
    }
}

#[test]
fn paste_encoding() {
    let plain = InputModes::default();
    let bracketed = InputModes {
        bracketed_paste: true,
        ..InputModes::default()
    };
    assert_eq!(encode_paste(b"hi", true, true, &plain), b"hi");
    assert_eq!(
        encode_paste(b"hi", true, true, &bracketed),
        b"\x1b[200~hi\x1b[201~"
    );
    assert_eq!(encode_paste(b"hi", true, false, &bracketed), b"\x1b[200~hi");
    assert_eq!(encode_paste(b"hi", false, false, &bracketed), b"hi");
    assert_eq!(encode_paste(b"hi", false, true, &bracketed), b"hi\x1b[201~");
}

/// Under tmux identity the prefix table is tmux 3.7b's stock one, in
/// tmux's dialect with its notes; a key bound at runtime stays, and
/// `reload` keeps the stock table.
#[test]
fn the_tmux_stock_table_replaces_grofe_s_defaults_and_survives_reload() {
    let mut t = default_bindings();
    t.insert(
        PREFIX_TABLE,
        k("y"),
        Binding::new(
            vec![vec!["list-sessions".into()]],
            false,
            BindingSource::Runtime,
        ),
    );
    t.use_tmux_stock();
    let split = t.exact(PREFIX_TABLE, &k("\"")).expect("stock \"");
    assert_eq!(split.cmds, vec![vec!["split-window".to_string()]]);
    assert_eq!(split.dialect, Dialect::Tmux);
    assert_eq!(split.desc.as_deref(), Some("Split window vertically"));
    let rename = t.exact(PREFIX_TABLE, &k("$")).expect("stock $");
    assert_eq!(
        rename.cmds,
        vec![vec![
            "command-prompt".to_string(),
            "-I".into(),
            "#S".into(),
            "'rename-session' '%%'".into(),
        ]]
    );
    assert!(
        t.exact(PREFIX_TABLE, &k("[")).is_some(),
        "tmux's copy-mode key"
    );
    assert!(
        t.exact(PREFIX_TABLE, &k("y")).is_some(),
        "runtime binding kept"
    );
    assert_eq!(t.keys(PREFIX_TABLE).len(), TMUX_STOCK.len() + 1);
    // TM.7: the stock copy-mode-vi templates show as tmux prints them.
    let slash = t.exact(COPY_MODE_VI_TABLE, &k("/")).expect("stock /");
    assert_eq!(
        slash.shown.as_deref(),
        Some(
            r#"command-prompt -T search -p "(search down)" { send-keys -X search-forward -- "%%" }"#
        )
    );
    assert_eq!(slash.dialect, Dialect::Grofe);
    assert_eq!(t.exact(COPY_MODE_VI_TABLE, &k("j")).unwrap().shown, None);
    let reloaded = t.with_config_from(KeyTables::from_config(&[]));
    assert!(
        reloaded
            .exact(COPY_MODE_VI_TABLE, &k("7"))
            .unwrap()
            .shown
            .is_some()
    );
    assert_eq!(reloaded.keys(PREFIX_TABLE).len(), TMUX_STOCK.len() + 1);
    assert_eq!(
        reloaded.exact(PREFIX_TABLE, &k("\"")).map(|b| b.dialect),
        Some(Dialect::Tmux)
    );
}

/// FR.4: grofe identity binds `prefix f` and `prefix F` to the float
/// toggle (`f` in place of tmux's `find-window` prompt, owner review
/// 2026-11-11); a tmux-identity table has tmux 3.7b's `f` and no `F`.
#[test]
fn prefix_f_toggles_floating_under_grofe_identity_only() {
    let mut t = default_bindings();
    for key in ["f", "F"] {
        let f = t.exact(PREFIX_TABLE, &k(key)).expect("grofe's float key");
        assert_eq!(
            words(f),
            [
                "if-shell",
                "-F",
                "#{pane_floating_flag}",
                "unfloat-pane",
                "float-pane"
            ],
            "{key}"
        );
        assert_eq!(f.dialect, Dialect::Grofe, "{key}");
    }
    t.use_tmux_stock();
    assert!(t.exact(PREFIX_TABLE, &k("F")).is_none());
    let f = t.exact(PREFIX_TABLE, &k("f")).expect("tmux's f");
    assert!(words(f)[1].contains("find-window"), "{:?}", words(f));
}

/// WS13: grofe identity binds `prefix G` to copy mode's hints; a
/// tmux-identity table leaves it unbound, as tmux 3.7b does.
#[test]
fn prefix_upper_g_starts_copy_hints_under_grofe_identity_only() {
    let mut t = default_bindings();
    let g = t.exact(PREFIX_TABLE, &k("G")).expect("grofe's hint key");
    assert_eq!(words(g), ["copy-mode", "-h"]);
    assert_eq!(g.dialect, Dialect::Grofe);
    t.use_tmux_stock();
    assert!(t.exact(PREFIX_TABLE, &k("G")).is_none());
}

/// grofe identity binds `prefix a`, `S` and `A` to the scratchpad
/// commands; they are grofe's own, so tmux identity drops them.
#[test]
fn grofe_binds_scratchpad_keys() {
    let t = default_bindings();
    for (key, cmd, note) in [
        ("a", "new-scratchpad", "Open a scratchpad"),
        ("S", "toggle-scratchpad", "Hide or show a scratchpad"),
        ("A", "choose-scratchpad", "Choose a scratchpad from a list"),
    ] {
        let b = t.exact(PREFIX_TABLE, &k(key)).expect(key);
        assert_eq!(words(b), [cmd], "{key}");
        assert_eq!(b.desc.as_deref(), Some(note), "{key}");
        assert_eq!(b.dialect, Dialect::Grofe, "{key}");
    }
}

/// The bound argv must parse as the scratchpad commands, so a press runs
/// the handlers and not an unknown-command error.
#[test]
fn scratchpad_keys_parse_as_their_commands() {
    use crate::cmd::{Command, parse};
    let t = default_bindings();
    for (key, want) in [("a", "new"), ("S", "toggle"), ("A", "choose")] {
        let b = t.exact(PREFIX_TABLE, &k(key)).expect(key);
        let argv: Vec<String> = b.cmds[0].clone();
        let got = parse(&argv).unwrap_or_else(|e| panic!("{key}: {e}"));
        let ok = matches!(
            (want, &got),
            ("new", Command::NewScratchpad(_))
                | ("toggle", Command::ToggleScratchpad(_))
                | ("choose", Command::ChooseScratchpad)
        );
        assert!(ok, "{key} parsed as {got:?}");
    }
}

#[test]
fn tmux_identity_drops_scratchpad_keys() {
    let mut t = default_bindings();
    t.use_tmux_stock();
    for key in ["a", "S", "A"] {
        assert!(t.exact(PREFIX_TABLE, &k(key)).is_none(), "{key}");
    }
}

/// grofe identity binds `prefix Q` to stop the server after asking
/// (owner review 2026-11-11); tmux 3.7b has no key for `kill-server`.
#[test]
fn prefix_q_kills_the_server_after_asking_under_grofe_identity_only() {
    let mut t = default_bindings();
    let q = t.exact(PREFIX_TABLE, &k("Q")).expect("grofe's Q");
    let w = words(q);
    assert_eq!(w[0], "confirm-before");
    assert_eq!(w.last(), Some(&"kill-server"));
    t.use_tmux_stock();
    assert!(t.exact(PREFIX_TABLE, &k("Q")).is_none());
}

/// grofe identity binds `prefix u` to jump to the agent that needs you;
/// tmux 3.7b has no `u` binding, so a tmux-identity table has none.
#[test]
fn grofe_binds_u_agent_next() {
    let t = default_bindings();
    let u = t.exact(PREFIX_TABLE, &k("u")).expect("grofe's u");
    assert_eq!(words(u), ["agent", "next"]);
    assert_eq!(u.desc.as_deref(), Some("Jump to the agent that needs you"));
}

#[test]
fn tmux_identity_has_no_u_binding() {
    let mut t = default_bindings();
    t.use_tmux_stock();
    assert!(t.exact(PREFIX_TABLE, &k("u")).is_none());
}

// ---- default key table (T2.5b) ----

fn words(b: &Binding) -> Vec<&str> {
    assert_eq!(b.cmds.len(), 1, "one command");
    b.cmds[0].iter().map(String::as_str).collect()
}

/// customize-mode's `d` on a key releases it before the reload: a rebound
/// stock key has tmux's binding until then, a key with no default goes, and
/// a runtime unbind ends.
#[test]
fn a_released_key_has_its_default_or_goes() {
    let mut t = default_bindings();
    t.use_tmux_stock();
    let cz = parse_key("C-z").unwrap();
    let stock = t.exact(PREFIX_TABLE, &cz).unwrap().cmds.clone();
    let mut b = Binding::new(
        vec![vec!["display-message".into(), "zz".into()]],
        false,
        BindingSource::Runtime,
    );
    t.insert(PREFIX_TABLE, cz, b.clone());
    t.release_runtime(PREFIX_TABLE, &cz);
    assert_eq!(t.exact(PREFIX_TABLE, &cz).unwrap().cmds, stock);
    let f5 = parse_key("F5").unwrap();
    b.cmds = vec![vec!["display-message".into()]];
    t.insert("mytab", f5, b);
    t.release_runtime("mytab", &f5);
    assert!(t.exact("mytab", &f5).is_none());
    t.unbind_runtime(PREFIX_TABLE, &cz);
    t.release_runtime(PREFIX_TABLE, &cz);
    assert_eq!(t.exact(PREFIX_TABLE, &cz).unwrap().cmds, stock);
    assert!(t.runtime_unbound.is_empty());
}

/// tmux 3.7b's stock prefix bindings (`list-keys -T prefix` on a
/// private server) for the commands grofe has, with new panes and windows
/// in the active pane's cwd (spec §5.3).
#[test]
fn default_bindings_are_tmux_stock_for_the_m2_commands() {
    let t = default_bindings();
    let cases: &[(&str, &[&str], bool)] = &[
        (
            "%",
            &["split-window", "-h", "-c", "#{pane_current_path}"],
            false,
        ),
        ("\"", &["split-window", "-c", "#{pane_current_path}"], false),
        ("c", &["new-window", "-c", "#{pane_current_path}"], false),
        ("d", &["detach-client"], false),
        ("n", &["next-window"], false),
        ("p", &["previous-window"], false),
        ("l", &["last-window"], false),
        ("(", &["switch-client", "-p"], false),
        (")", &["switch-client", "-n"], false),
        ("L", &["switch-client", "-l"], false),
        ("z", &["resize-pane", "-Z"], false),
        ("0", &["select-window", "-t", ":0"], false),
        ("9", &["select-window", "-t", ":9"], false),
        ("C-b", &["send-prefix"], false),
        (":", &["command-prompt"], false),
        (
            ",",
            &["command-prompt", "-I", "#W", "rename-window \"%%\""],
            false,
        ),
        (
            "$",
            &["command-prompt", "-I", "#S", "rename-session \"%%\""],
            false,
        ),
        ("Up", &["select-pane", "-U"], true),
        ("Down", &["select-pane", "-D"], true),
        ("Left", &["select-pane", "-L"], true),
        ("Right", &["select-pane", "-R"], true),
        ("M-Up", &["resize-pane", "-U", "5"], true),
        ("M-Right", &["resize-pane", "-R", "5"], true),
        ("C-Down", &["resize-pane", "-D"], true),
        ("C-Left", &["resize-pane", "-L"], true),
    ];
    for (name, argv, repeat) in cases {
        let b = t
            .get(PREFIX_TABLE, &k(name))
            .unwrap_or_else(|| panic!("{name} unbound"));
        assert_eq!(words(b), *argv, "{name}");
        assert_eq!(b.repeat, *repeat, "{name} repeat");
    }
    // Nothing in root: every key there goes to the pane.
    assert!(t.get(ROOT_TABLE, &k("q")).is_none());
    // Stock keys grofe leaves unbound (out of scope).
    for name in ["m", "M", "t", "<", ">", "*"] {
        assert!(t.get(PREFIX_TABLE, &k(name)).is_none(), "{name}");
    }
}

/// D10, recorded on tmux 3.7b (`list-keys -T prefix`, `list-keys -N`
/// on a private `-f /dev/null` server): the stock keys whose verbs
/// the structure commands add, with tmux's notes. Formats tmux spells short (`#P`, `#W`)
/// are native (`#{pane_index}`).
#[test]
fn default_bindings_add_the_m36_stock_keys_with_notes() {
    let t = default_bindings();
    let cases: &[(&str, &[&str], &str)] = &[
        (";", &["last-pane"], "Move to the previously active pane"),
        ("C-o", &["rotate-window"], "Rotate through the panes"),
        (
            "M-o",
            &["rotate-window", "-D"],
            "Rotate through the panes in reverse",
        ),
        ("q", &["display-panes"], "Display pane numbers"),
        (
            ".",
            &["command-prompt", "-T", "target", "move-window -t \"%%\""],
            "Move the current window",
        ),
        (
            "'",
            &[
                "command-prompt",
                "-T",
                "window-target",
                "-p",
                "index",
                "select-window -t \":%%\"",
            ],
            "Prompt for window index to select",
        ),
        ("C-z", &["suspend-client"], "Suspend the current client"),
        (
            "D",
            &["choose-client", "-Z"],
            "Choose and detach a client from a list",
        ),
        ("~", &["show-messages"], "Show messages"),
        ("E", &["select-layout", "-E"], "Spread panes out evenly"),
        (
            "M-n",
            &["next-window", "-a"],
            "Select the next window with an alert",
        ),
        (
            "M-p",
            &["previous-window", "-a"],
            "Select the previous window with an alert",
        ),
        (
            "/",
            &["command-prompt", "-k", "-p", "key", "list-keys -1N \"%%\""],
            "Describe key binding",
        ),
        (
            "x",
            &[
                "confirm-before",
                "-p",
                "kill-pane #{pane_index}? (y/n)",
                "kill-pane",
            ],
            "Kill the active pane",
        ),
        ("!", &["break-pane"], "Break pane to a new window"),
        (
            "}",
            &["swap-pane", "-D"],
            "Swap the active pane with the pane below",
        ),
        ("w", &["choose-tree", "-Zw"], "Choose a window from a list"),
        ("Space", &["key-menu", "-T", "prefix"], "Show the key menu"),
        ("M-Space", &["next-layout"], "Select next layout"),
        ("U", &["choose-agent"], "Pick an agent"),
        ("?", &["list-keys", "-N"], "List key bindings"),
        (
            "M-5",
            &["select-layout", "tiled"],
            "Select the tiled layout",
        ),
        ("C-Left", &["resize-pane", "-L"], "Resize the pane left"),
        (
            "M-Up",
            &["resize-pane", "-U", "5"],
            "Resize the pane up by 5",
        ),
        (
            "Right",
            &["select-pane", "-R"],
            "Select the pane to the right of the active pane",
        ),
    ];
    for (name, argv, note) in cases {
        let b = t
            .get(PREFIX_TABLE, &k(name))
            .unwrap_or_else(|| panic!("{name} unbound"));
        assert_eq!(words(b), *argv, "{name}");
        assert_eq!(b.desc.as_deref(), Some(*note), "{name}");
    }
    // Stock `o` is tmux dialect: its offset target has no native form.
    let o = t.get(PREFIX_TABLE, &k("o")).expect("o bound");
    assert_eq!(words(o), ["select-pane", "-t", ":.+"]);
    assert_eq!(o.desc.as_deref(), Some("Select the next pane"));
    assert_eq!(o.dialect, Dialect::Tmux);
    // Every default carries a note, as every tmux stock binding does.
    let unnoted: Vec<String> = t
        .keys(PREFIX_TABLE)
        .into_iter()
        .filter(|key| t.exact(PREFIX_TABLE, key).is_some_and(|b| b.desc.is_none()))
        .map(|key| key.to_string())
        .collect();
    assert!(unnoted.is_empty(), "{unnoted:?}");
}

/// tmux 3.7b's mouse key names parse, case-insensitively, with
/// modifiers, and print back in tmux's spelling.
#[test]
fn mouse_key_names_parse_and_print_as_tmux_does() {
    for (name, shown) in [
        ("MouseDown1Pane", "MouseDown1Pane"),
        ("mousedown1status", "MouseDown1Status"),
        ("MouseDown3Pane", "MouseDown3Pane"),
        ("MouseDrag1Border", "MouseDrag1Border"),
        ("MouseDragEnd1Pane", "MouseDragEnd1Pane"),
        ("MouseUp2StatusLeft", "MouseUp2StatusLeft"),
        ("MouseDown1StatusRight", "MouseDown1StatusRight"),
        ("MouseDown1StatusDefault", "MouseDown1StatusDefault"),
        ("WheelUpPane", "WheelUpPane"),
        ("WheelDownStatus", "WheelDownStatus"),
        ("DoubleClick1Pane", "DoubleClick1Pane"),
        ("TripleClick1Pane", "TripleClick1Pane"),
        ("SecondClick1Pane", "SecondClick1Pane"),
        ("M-MouseDown3Pane", "M-MouseDown3Pane"),
        ("C-MouseDown1Status", "C-MouseDown1Status"),
        // tmux 3.7's names, recorded binding on a private server.
        ("MouseDown1ScrollbarUp", "MouseDown1ScrollbarUp"),
        ("MouseDrag1ScrollbarSlider", "MouseDrag1ScrollbarSlider"),
        ("MouseDown1ScrollbarDown", "MouseDown1ScrollbarDown"),
        ("WheelUpScrollbarUp", "WheelUpScrollbarUp"),
        ("MouseDown1Control0", "MouseDown1Control0"),
        ("mousedown1control9", "MouseDown1Control9"),
        ("MouseUp3Control5", "MouseUp3Control5"),
    ] {
        let key = parse_key(name).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(matches!(key.code, KeyCode::Mouse(_)), "{name}");
        assert_eq!(key.to_string(), shown, "{name}");
    }
    assert_eq!(
        k("MouseDown1Pane").code,
        KeyCode::Mouse(MouseKey {
            action: MouseAction::Down,
            button: 1,
            place: MousePlace::Pane,
        })
    );
    for bad in [
        "MouseDown1",
        "MouseDownPane",
        "MouseDown0Pane",
        "MouseDown12Pane",
        "WheelUp1Pane",
        "MouseDown1Window",
        // tmux 3.7b: `unknown key: MouseDown1Control10`.
        "MouseDown1Control10",
        "MouseDown1Control",
    ] {
        assert_eq!(parse_key(bad), Err(format!("bad key: {bad}")), "{bad}");
    }
    // A mouse key is never bytes for a pane.
    let key = k("MouseDown1Pane");
    assert!(encode(&key, &InputModes::default()).is_empty());
    assert!(encode(&key, &modes(1 | 8)).is_empty());
}

/// D4, recorded on tmux 3.7b through an attached client: kitty `9;4u`
/// (M-S-Tab), `9;6u` (C-S-Tab) and `\e\e[Z` (M-BTab) reach a legacy pane
/// as nothing; `9;2u` (S-Tab) and `\e[Z` as `\e[Z`.
#[test]
fn back_tab_with_ctrl_or_meta_sends_a_legacy_pane_nothing() {
    let t = Instant::now();
    let mut c = Client::new();
    assert_eq!(c.send(b"\x1b[9;4u", t), vec![Routed::Consumed]);
    assert_eq!(c.send(b"\x1b[9;6u", t), vec![Routed::Consumed]);
    assert_eq!(c.send(b"\x1b[9;2u", t), vec![pane(b"\x1b[Z")]);
    assert_eq!(c.send(b"\x1b[Z", t), vec![pane(b"\x1b[Z")]);
    // `send-keys M-BTab` still sends `\e[Z`, as tmux's does.
    assert_eq!(
        encode(&k("M-BTab"), &InputModes::default()),
        b"\x1b[Z".to_vec()
    );
}

/// tmux 3.7b's whole stock `root` table is bound on a server with tmux
/// identity, as tmux prints it, and parses as commands; the rows a
/// config or the runtime unbound stay unbound.
#[test]
fn the_stock_root_rows_are_bindings() {
    let k = |name: &str| parse_key(name).unwrap();
    let ctx = crate::protocol::ClientCtx {
        dialect: Dialect::Tmux,
        ..crate::server::test_ctx()
    };
    let mut t = default_bindings();
    for name in NATIVE_STOCK_ROOT {
        assert!(t.exact(ROOT_TABLE, &k(name)).is_some(), "{name}");
    }
    // FR.4: grofe's own pane menu, which tmux's stock one replaces.
    let menu = t.exact(ROOT_TABLE, &k("MouseDown3Pane")).expect("menu");
    assert_eq!(menu.dialect, Dialect::Grofe);
    assert!(menu.shown.is_none());
    assert_eq!(menu.cmds[0][0], "if-shell");
    assert!(
        menu.cmds[0][4].contains("'unfloat-pane' '-t' '='"),
        "{menu:?}"
    );
    assert!(t.exact(ROOT_TABLE, &k("M-MouseDown3Pane")).is_none());
    t.use_tmux_stock();
    assert_eq!(t.keys(ROOT_TABLE).len(), TMUX_STOCK_ROOT.len());
    let menu = t.exact(ROOT_TABLE, &k("MouseDown3Pane")).expect("menu");
    assert_eq!(menu.dialect, Dialect::Tmux);
    // `unbind -n` sticks through a reload, which binds the stock again.
    t.unbind_runtime(ROOT_TABLE, &k("MouseDown3Status"));
    t.use_tmux_stock();
    assert_eq!(t.keys(ROOT_TABLE).len(), TMUX_STOCK_ROOT.len() - 1);
    for (name, text) in TMUX_STOCK_ROOT {
        let Some(b) = t.exact(ROOT_TABLE, &k(name)) else {
            assert_eq!(*name, "MouseDown3Status");
            continue;
        };
        assert_eq!(b.shown.as_deref(), Some(*text), "{name}");
        for argv in &b.cmds {
            // A mouse key's `=` and `@` are targets by the time it runs.
            let argv: Vec<String> = argv
                .iter()
                .map(|w| match w.as_str() {
                    "=" | "@" => "%0".to_string(),
                    w => w.replace("-t=", "-t%0"),
                })
                .collect();
            if let Err(e) = crate::compat::parse_split(&ctx, &argv) {
                panic!("{name}: {argv:?}: {e}");
            }
        }
    }
    let click = t.exact(ROOT_TABLE, &k("MouseDown1Pane")).unwrap();
    assert_eq!(click.cmds.len(), 2, "select-pane, then send-keys -M");
}

#[test]
fn every_default_binding_parses_as_a_command() {
    let t = default_bindings();
    let ctx = crate::server::test_ctx();
    for b in t.tables.values().flat_map(|t| t.values()) {
        // Each in its own dialect: stock `o` is tmux's.
        let ctx = crate::protocol::ClientCtx {
            dialect: b.dialect,
            ..ctx.clone()
        };
        for argv in &b.cmds {
            if let Err(e) = crate::compat::parse_split(&ctx, argv) {
                panic!("{argv:?}: {e}");
            }
        }
    }
}

/// R12: a binding may be a sequence; `bind` is the one-command case.
#[test]
fn a_sequence_binding_routes_every_command() {
    let t = Instant::now();
    let mut c = Client::new();
    c.tables.bind_seq(
        PREFIX_TABLE,
        k("Z"),
        vec![argv(&["split-window"]), argv(&["select-pane", "-L"])],
        false,
    );
    assert_eq!(c.send(b"\x02", t), vec![Routed::Consumed]);
    assert_eq!(
        c.send(b"Z", t),
        vec![Routed::Bound(
            vec![argv(&["split-window"]), argv(&["select-pane", "-L"])],
            Dialect::Grofe
        )]
    );
    let b = c.tables.get(PREFIX_TABLE, &k("%")).expect("bound");
    assert_eq!(b.cmds, vec![argv(&["split-window", "-h"])]);
}

/// The stub edit: a binding's commands route in its own dialect.
#[test]
fn a_binding_routes_in_its_own_dialect() {
    let t = Instant::now();
    let mut c = Client::new();
    let mut b = Binding::new(vec![argv(&["neww"])], false, BindingSource::Runtime);
    b.dialect = Dialect::Tmux;
    c.tables.insert(ROOT_TABLE, k("F5"), b);
    assert_eq!(
        c.send(b"\x1b[15~", t),
        vec![Routed::Bound(vec![argv(&["neww"])], Dialect::Tmux)]
    );
}

fn config_key(table: &str, key: &str, cmds: &[&[&str]], line: usize) -> crate::config::KeyBinding {
    crate::config::KeyBinding {
        table: table.to_string(),
        key: k(key),
        command: String::new(),
        cmds: cmds.iter().map(|c| argv(c)).collect(),
        desc: None,
        group: None,
        name: Some(format!("n-{key}")),
        repeat: false,
        source: crate::config::Source {
            layer: crate::config::Layer::Config,
            file: Some(PathBuf::from("/c/config.toml")),
            line,
        },
    }
}

/// The config's bindings go over the defaults; an empty one unbinds
/// (R11); `reload` replaces config bindings and keeps runtime ones and
/// runtime unbinds (R10).
#[test]
fn config_tables_layer_over_defaults_and_reload_keeps_runtime() {
    let t = KeyTables::from_config(&[
        config_key(PREFIX_TABLE, "X", &[&["split-window", "-h"]], 3),
        config_key(PREFIX_TABLE, "%", &[], 4),
    ]);
    let x = t.exact(PREFIX_TABLE, &k("X")).expect("X");
    assert_eq!(
        x.source,
        BindingSource::File {
            path: PathBuf::from("/c/config.toml"),
            line: 3
        }
    );
    assert_eq!(x.name.as_deref(), Some("n-X"));
    assert!(t.exact(PREFIX_TABLE, &k("%")).is_none());
    assert_eq!(
        t.exact(PREFIX_TABLE, &k("c")).map(|b| &b.source),
        Some(&BindingSource::Default)
    );

    let mut live = t;
    live.insert(
        PREFIX_TABLE,
        k("y"),
        Binding::new(vec![argv(&["new-window"])], false, BindingSource::Runtime),
    );
    live.unbind_runtime(PREFIX_TABLE, &k("c"));
    let fresh = KeyTables::from_config(&[]);
    let t = live.with_config_from(fresh);
    assert!(t.exact(PREFIX_TABLE, &k("X")).is_none(), "config X gone");
    assert!(t.exact(PREFIX_TABLE, &k("%")).is_some(), "stock % back");
    assert!(t.exact(PREFIX_TABLE, &k("y")).is_some(), "runtime y kept");
    assert!(
        t.exact(PREFIX_TABLE, &k("c")).is_none(),
        "runtime unbind kept"
    );
}

#[test]
fn default_arrows_repeat_without_the_prefix() {
    let t = Instant::now();
    let mut c = Client::new();
    c.tables = default_bindings();
    assert_eq!(c.send(b"\x02", t), vec![Routed::Consumed]);
    assert_eq!(c.send(b"\x1b[D", t), vec![cmd(&["select-pane", "-L"])]);
    // Within repeat-time, another arrow needs no prefix.
    assert_eq!(c.send(b"\x1b[A", t), vec![cmd(&["select-pane", "-U"])]);
    // A key that does not repeat ends the repeat and goes to the pane.
    assert_eq!(c.send(b"x", t), vec![pane(b"x")]);
}

/// Every command a stock binding runs, the menu items and prompt
/// templates inside it too, is one the tmux front end maps: pressing
/// the key fails at nothing but a missing target or state (B1d).
fn unmapped(text: &str, out: &mut Vec<String>) {
    use crate::protocol::{ClientCtx, Dialect};
    let ctx = ClientCtx {
        cwd: std::path::PathBuf::from("/"),
        pane: None,
        env: Vec::new(),
        client: None,
        dialect: Dialect::Tmux,
        request: Default::default(),
    };
    let Ok(cmds) = crate::cmd::split_command_string(text) else {
        out.push(format!("does not split: {text}"));
        return;
    };
    for argv in cmds {
        if let Err(e) = crate::compat::parse_split(&ctx, &argv) {
            out.push(format!("{}: {e}", argv.join(" ")));
        }
        // The commands a verb holds as arguments.
        let inner: Vec<&String> = match argv.first().map(String::as_str) {
            Some("display-menu") => {
                let mut words = argv[1..].iter().peekable();
                while let Some(&w) = words.peek() {
                    if !w.starts_with('-') || w.len() < 2 {
                        break;
                    }
                    let takes = "TxytbsSHC".contains(w.chars().nth(1).unwrap_or(' '));
                    words.next();
                    if takes && w.len() == 2 {
                        words.next();
                    }
                }
                let rest: Vec<&String> = words.collect();
                let mut commands = Vec::new();
                let mut i = 0;
                while i < rest.len() {
                    if rest[i].is_empty() {
                        i += 1;
                    } else {
                        commands.extend(rest.get(i + 2).copied());
                        i += 3;
                    }
                }
                commands
            }
            Some("command-prompt" | "confirm-before") => argv.last().into_iter().collect(),
            Some("if-shell") => argv.iter().skip(2).collect(),
            _ => Vec::new(),
        };
        for text in inner {
            if text.contains(' ') || crate::cmd::parse::canonical_name(text) != text {
                unmapped(&text.replace("%%", "x"), out);
            }
        }
    }
}

#[test]
fn every_stock_prefix_binding_maps() {
    let mut bad = Vec::new();
    for (key, _, _, text) in TMUX_STOCK.iter() {
        let mut errors = Vec::new();
        unmapped(text, &mut errors);
        bad.extend(errors.into_iter().map(|e| format!("prefix {key}: {e}")));
    }
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
fn every_default_has_group() {
    let t = default_bindings();
    let mut bad = Vec::new();
    for (table, bindings) in &t.tables {
        for (key, b) in bindings {
            if b.group.is_none() {
                bad.push(format!("{table} {key}"));
            }
        }
    }
    assert!(bad.is_empty(), "{bad:?}");
    let get = |k: &str| t.exact(PREFIX_TABLE, &parse_key(k).unwrap()).unwrap();
    assert_eq!(get("U").group.as_deref(), Some("agents"));
    assert_eq!(get("u").group.as_deref(), Some("agents"));
    assert_eq!(get("Space").group.as_deref(), Some("other"));
    assert_eq!(get("M-Space").group.as_deref(), Some("layout"));
    assert_eq!(get("c").group.as_deref(), Some("window"));
    assert_eq!(get("Space").cmds, vec![argv(&["key-menu", "-T", "prefix"])]);
}

#[test]
fn toml_group_kept() {
    let mut kb = config_key("prefix", "x", &[&["neww"]], 1);
    kb.group = Some("window".to_string());
    let t = KeyTables::from_config(&[kb]);
    let b = t.exact(PREFIX_TABLE, &parse_key("x").unwrap()).unwrap();
    assert_eq!(b.group.as_deref(), Some("window"));
}

#[test]
fn y_binding_grofe_only() {
    let mut t = default_bindings();
    let y = t.exact(PREFIX_TABLE, &parse_key("y").unwrap()).unwrap();
    assert_eq!(y.cmds, vec![argv(&["choose-notification"])]);
    assert_eq!(y.desc.as_deref(), Some("Show notifications"));
    assert_eq!(y.group.as_deref(), Some("notifications"));
    t.use_tmux_stock();
    assert!(t.exact(PREFIX_TABLE, &parse_key("y").unwrap()).is_none());
}

#[test]
fn b_and_o_grofe_only() {
    let mut t = default_bindings();
    for (key, cmd, note) in [
        (
            "b",
            argv(&["agent-sidebar", "toggle"]),
            "Toggle the agent sidebar",
        ),
        ("O", argv(&["agent-grid"]), "Show all agents"),
    ] {
        let b = t.exact(PREFIX_TABLE, &parse_key(key).unwrap()).unwrap();
        assert_eq!(b.cmds, vec![cmd]);
        assert_eq!(b.desc.as_deref(), Some(note));
        assert_eq!(b.group.as_deref(), Some("agents"));
    }
    t.use_tmux_stock();
    for key in ["b", "O"] {
        assert!(t.exact(PREFIX_TABLE, &parse_key(key).unwrap()).is_none());
    }
}

#[test]
fn badge_click_opens_tray() {
    let key = parse_key("MouseDown1Status").unwrap();
    let mut t = default_bindings();
    let text = |t: &KeyTables| format!("{:?}", t.exact(ROOT_TABLE, &key).unwrap().cmds);
    let grofe = text(&t);
    assert!(grofe.contains("mouse_status_range"), "{grofe}");
    assert!(grofe.contains("choose-notification"), "{grofe}");
    assert!(grofe.contains("switch-client"), "{grofe}");
    t.use_tmux_stock();
    assert_eq!(
        t.exact(ROOT_TABLE, &key).unwrap().cmds,
        vec![argv(&["switch-client", "-t", "="])]
    );
}

#[test]
fn tmux_stock_unchanged() {
    let mut t = default_bindings();
    t.use_tmux_stock();
    let b = t.exact(PREFIX_TABLE, &parse_key("Space").unwrap()).unwrap();
    assert_eq!(b.cmds, vec![argv(&["next-layout"])]);
    assert!(t.exact(PREFIX_TABLE, &parse_key("U").unwrap()).is_none());
}
