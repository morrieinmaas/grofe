use super::*;

const UL: &str = "\x1b[0;4m";

fn s(b: Vec<u8>) -> String {
    String::from_utf8_lossy(&b).into_owned()
}

/// A predictor whose mirror shows "$ abc" and which has just confirmed three echoes.
fn confident(t: Instant) -> Predictor {
    let mut p = Predictor::new(20, 5);
    p.feed_output(b"$ ", t);
    for ch in ["a", "b", "c"] {
        assert_eq!(p.on_input(ch.as_bytes(), t), b"", "tentative draws nothing");
        p.feed_output(ch.as_bytes(), t);
    }
    p
}

#[test]
fn tentative_until_confirmed_then_draws_underlined() {
    let t = Instant::now();
    let mut p = confident(t);
    let out = s(p.on_input(b"d", t));
    assert!(out.contains(&format!("\x1b[1;6H{UL}d")), "{out:?}");
    assert!(
        out.ends_with("\x1b[1;7H"),
        "cursor at the prediction: {out:?}"
    );
}

#[test]
fn confirmation_repaints_without_underline() {
    let t = Instant::now();
    let mut p = confident(t);
    p.on_input(b"d", t);
    let out = s(p.feed_output(b"d", t));
    assert!(out.contains("$ abcd\x1b[K"), "{out:?}");
    assert!(!out.contains(UL), "{out:?}");
    assert_eq!(p.tick(t + Duration::from_secs(5)), b"", "nothing pending");
}

#[test]
fn mismatch_rolls_back_and_goes_tentative() {
    let t = Instant::now();
    let mut p = confident(t);
    p.on_input(b"x", t);
    let out = s(p.feed_output(b"y", t));
    assert!(out.contains("$ abcy\x1b[K"), "{out:?}");
    assert!(out.ends_with("\x1b[1;7H"), "mirror cursor last: {out:?}");
    assert_eq!(p.on_input(b"z", t), b"", "tentative again");
}

#[test]
fn alt_screen_switch_erases() {
    let t = Instant::now();
    let mut p = confident(t);
    p.on_input(b"x", t);
    let out = s(p.feed_output(b"\x1b[?1049h", t));
    assert!(
        out.is_empty(),
        "the alt screen hides the stale cell: {out:?}"
    );
    assert_eq!(p.on_input(b"q", t), b"", "no prediction on the alt screen");
    let out = s(p.feed_output(b"\x1b[?1049l", t));
    assert!(
        out.contains("$ abc\x1b[K"),
        "main screen repainted on return: {out:?}"
    );
    assert_eq!(p.on_input(b"q", t), b"", "tentative after leaving");
}

#[test]
fn timeout_erases_after_one_second() {
    let t = Instant::now();
    let mut p = confident(t);
    p.on_input(b"x", t);
    assert_eq!(p.tick(t + Duration::from_millis(999)), b"");
    let out = s(p.tick(t + Duration::from_secs(1)));
    assert!(out.contains("$ abc\x1b[K"), "{out:?}");
    assert_eq!(p.tick(t + Duration::from_secs(2)), b"");
}

#[test]
fn cursor_jump_erases() {
    let t = Instant::now();
    let mut p = confident(t);
    p.on_input(b"x", t);
    let out = s(p.feed_output(b"\x1b[3;3H", t));
    assert!(out.contains("$ abc\x1b[K"), "{out:?}");
    assert!(out.ends_with("\x1b[3;3H"), "{out:?}");
}

#[test]
fn password_prompts_never_predict() {
    let t = Instant::now();
    let mut p = confident(t);
    p.feed_output(b"\r\nuser's Password: ", t);
    assert_eq!(p.on_input(b"s", t), b"");
    assert_eq!(p.tick(t + Duration::from_secs(3)), b"");
    p.feed_output(b"\r\nPassPhrase: ", t);
    p.feed_output(b"\r\npassphrase: ", t);
    assert_eq!(p.on_input(b"s", t), b"");
    let mut q = confident(t);
    q.feed_output(b"\r\nsudo password:", t);
    assert_eq!(q.on_input(b"s", t), b"");
}

#[test]
fn echo_hint_off_stops_prediction() {
    let t = Instant::now();
    let mut p = confident(t);
    p.on_input(b"x", t);
    let out = s(p.set_echo_hint(false));
    assert!(out.contains("$ abc\x1b[K"), "{out:?}");
    assert_eq!(p.on_input(b"y", t), b"");
    p.set_echo_hint(true);
    assert_eq!(p.on_input(b"y", t), b"", "tentative after the hint returns");
}

#[test]
fn disabled_predicts_nothing() {
    let t = Instant::now();
    let mut p = confident(t);
    p.set_enabled(false);
    assert_eq!(p.on_input(b"x", t), b"");
}

#[test]
fn backspace_predicts_and_confirms() {
    let t = Instant::now();
    let mut p = confident(t);
    p.on_input(b"x", t);
    let out = s(p.on_input(b"\x7f", t));
    assert!(out.contains("\x1b[1;6H\x1b[0m "), "{out:?}");
    assert!(out.ends_with("\x1b[1;6H"), "{out:?}");
    // The server echoes the char, then the erase.
    p.feed_output(b"x", t);
    let out = s(p.feed_output(b"\x08 \x08", t));
    assert!(!out.contains(UL), "{out:?}");
    assert_eq!(p.tick(t + Duration::from_secs(5)), b"", "nothing pending");
}

#[test]
fn backspace_at_line_start_flushes() {
    let t = Instant::now();
    let mut p = confident(t);
    p.feed_output(b"\r", t);
    assert_eq!(p.on_input(b"\x7f", t), b"");
}

#[test]
fn left_and_right_move_the_predicted_cursor() {
    let t = Instant::now();
    let mut p = confident(t);
    assert_eq!(s(p.on_input(b"\x1b[D", t)), "\x1b[0m\x1b[1;5H");
    assert_eq!(s(p.on_input(b"\x1bOC", t)), "\x1b[0m\x1b[1;6H");
    // The server moves left only: the first move confirms, the second times out.
    let out = s(p.feed_output(b"\x1b[D", t));
    assert!(
        out.ends_with("\x1b[1;6H"),
        "cursor stays at the unconfirmed move: {out:?}"
    );
    let out = s(p.tick(t + Duration::from_secs(1)));
    assert!(out.ends_with("\x1b[1;5H"), "{out:?}");
    assert_eq!(p.tick(t + Duration::from_secs(2)), b"");
}

#[test]
fn right_at_the_end_of_the_text_is_not_predicted() {
    let t = Instant::now();
    let mut p = confident(t);
    assert_eq!(p.on_input(b"\x1b[C", t), b"");
    assert_eq!(p.tick(t + Duration::from_secs(2)), b"");
}

#[test]
fn left_at_column_zero_is_not_predicted() {
    let t = Instant::now();
    let mut p = confident(t);
    p.feed_output(b"\r", t);
    assert_eq!(p.on_input(b"\x1b[D", t), b"");
}

#[test]
fn enter_and_control_keys_flush() {
    let t = Instant::now();
    let mut p = confident(t);
    p.on_input(b"x", t);
    let out = s(p.on_input(b"\r", t));
    assert!(out.contains("$ abc\x1b[K"), "{out:?}");
    assert_eq!(p.on_input(b"y", t), b"", "tentative after a flush");
    let mut p = confident(t);
    p.on_input(b"x", t);
    assert!(!p.on_input(b"\x03", t).is_empty());
    let mut p = confident(t);
    p.on_input(b"x", t);
    assert!(!p.on_input(b"\t", t).is_empty());
}

#[test]
fn escape_sequences_are_not_typed_text() {
    let t = Instant::now();
    let mut p = confident(t);
    assert_eq!(p.on_input(b"\x1b[A", t), b"");
    assert_eq!(p.on_input(b"\x1b[1;5C", t), b"");
    assert_eq!(p.on_input(b"\x1b[200~", t), b"");
    assert_eq!(
        p.tick(t + Duration::from_secs(3)),
        b"",
        "nothing was predicted"
    );
}

#[test]
fn wide_chars_predict_with_two_cells() {
    let t = Instant::now();
    let mut p = confident(t);
    let out = s(p.on_input("漢".as_bytes(), t));
    assert!(out.contains(&format!("\x1b[1;6H{UL}漢")), "{out:?}");
    assert!(out.ends_with("\x1b[1;8H"), "{out:?}");
    let out = s(p.feed_output("漢".as_bytes(), t));
    assert!(!out.contains(UL), "{out:?}");
    assert_eq!(p.tick(t + Duration::from_secs(5)), b"", "confirmed");
    // The next char lands after both cells.
    let out = s(p.on_input(b"a", t));
    assert!(out.contains(&format!("\x1b[1;8H{UL}a")), "{out:?}");
}

#[test]
fn wide_char_split_across_reads() {
    let t = Instant::now();
    let mut p = confident(t);
    let bytes = "漢".as_bytes();
    assert_eq!(p.on_input(&bytes[..2], t), b"");
    let out = s(p.on_input(&bytes[2..], t));
    assert!(out.contains("漢"), "{out:?}");
}

#[test]
fn zero_width_and_edge_of_screen_are_not_predicted() {
    let t = Instant::now();
    let mut p = confident(t);
    assert!(p.on_input("\u{301}".as_bytes(), t).is_empty());
    p.feed_output(b"\x1b[1;20H", t);
    assert!(p.on_input(b"x", t).is_empty(), "would wrap");
    p.feed_output(b"\x1b[1;19H", t);
    assert!(
        p.on_input("漢".as_bytes(), t).is_empty(),
        "wide char would wrap"
    );
}

#[test]
fn burst_of_typing_while_the_server_lags() {
    let t = Instant::now();
    let mut p = confident(t);
    for (i, ch) in "hello".chars().enumerate() {
        let at = t + Duration::from_millis(10 * i as u64);
        let out = s(p.on_input(ch.to_string().as_bytes(), at));
        assert!(out.contains(&format!("{UL}{ch}")), "{out:?}");
    }
    // 300 ms later the echoes arrive in two chunks.
    let late = t + Duration::from_millis(300);
    let out = s(p.feed_output(b"hel", late));
    assert!(!out.contains(UL), "{out:?}");
    assert!(out.ends_with("\x1b[1;11H"), "{out:?}");
    let out = s(p.feed_output(b"lo", late));
    assert!(!out.contains(UL), "{out:?}");
    assert_eq!(p.tick(t + Duration::from_secs(5)), b"", "all confirmed");
    // Still confident.
    assert!(!p.on_input(b"!", late).is_empty());
}

#[test]
fn partially_confirmed_burst_times_out_from_the_oldest() {
    let t = Instant::now();
    let mut p = confident(t);
    p.on_input(b"a", t);
    p.on_input(b"b", t + Duration::from_millis(500));
    p.feed_output(b"a", t + Duration::from_millis(600));
    assert_eq!(p.tick(t + Duration::from_millis(1400)), b"");
    let out = s(p.tick(t + Duration::from_millis(1500)));
    assert!(out.contains("$ abca\x1b[K"), "{out:?}");
}

#[test]
fn styled_cells_repaint_with_their_style() {
    let t = Instant::now();
    let mut p = confident(t);
    p.on_input(b"x", t);
    let out = s(p.feed_output(b"\x1b[1;31my", t));
    assert!(out.contains("$ abc\x1b[0;1;31my"), "{out:?}");
}
