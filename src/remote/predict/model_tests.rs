use super::*;
use alacritty_terminal::vte::ansi::Color;
use proptest::prelude::*;

/// The user's terminal: fed the server bytes and everything the predictor returns.
struct Model {
    term: Term<VoidListener>,
    parser: Processor<NoSync>,
}

impl Model {
    fn new(cols: usize, rows: usize) -> Self {
        let config = Config {
            scrolling_history: 0,
            ..Config::default()
        };
        Model {
            term: Term::new(config, &Size { cols, rows }, VoidListener),
            parser: Processor::new(),
        }
    }
    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }
}

const MASK: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT)
    .union(Flags::UNDERLINE);

fn snapshot(t: &Term<VoidListener>, cols: usize, rows: usize) -> String {
    let mut s = String::new();
    for r in 0..rows {
        for c in 0..cols {
            let cell = &t.grid()[Line(r as i32)][Column(c)];
            s.push(cell.c);
            let f = cell.flags & MASK;
            if f.bits() != 0
                || cell.fg != Color::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground)
                || cell.bg != Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background)
            {
                s += &format!("{{{:?}{:?}{:?}}}", f, cell.fg, cell.bg);
            }
        }
        s.push('\n');
    }
    let p = t.grid().cursor.point;
    s += &format!(
        "cursor {} {} wrap={} show={} alt={}",
        p.line.0,
        p.column.0,
        t.grid().cursor.input_needs_wrap,
        t.mode().contains(TermMode::SHOW_CURSOR),
        t.mode().contains(TermMode::ALT_SCREEN)
    );
    s
}

#[derive(Clone, Debug)]
enum Op {
    Input(&'static str),
    Server(Vec<&'static str>, usize),
    Tick(u64),
}

const INPUTS: &[&str] = &[
    "a",
    "b",
    "z",
    " ",
    "漢",
    "\x7f",
    "\x1b[D",
    "\x1b[C",
    "\x1bOD",
    "\r",
    "\x03",
    "\x1b",
    "\x1b[200~",
    "\x1b[201~",
    "\u{301}",
    "\x1b[A",
];
const ATOMS: &[&str] = &[
    "a",
    "b",
    "z",
    "ab",
    "漢",
    "e\u{301}",
    "\r\n",
    "\n",
    "\x08",
    "\x08 \x08",
    "\x1b[D",
    "\x1b[C",
    "\x1b[2K",
    "\x1b[K",
    "\x1b[1;31m",
    "\x1b[0m",
    "\x1b[1m",
    "\x1b[3;5H",
    "\x1b[1;9H",
    "\x1b[?25l",
    "\x1b[?25h",
    "\x1b[?1049h",
    "\x1b[?1049l",
    "\x1b[J",
    "$ ",
    "Password: ",
    "\x1b[?2026h",
    "\x1b[?2026l",
    "\x1b]0;title\x07",
    "\x1b7",
    "\x1b8",
    "0123456789012345678901234567",
    "\x1b[4;2r",
    "\x1b[r",
    "\x1bM",
    "\x1b[2J\x1b[H",
    "\x1b[38;5;200m",
    "\x1b[48;2;1;2;3m",
    "\x1b[4m",
    "\x1b[7m",
];

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0..INPUTS.len()).prop_map(|i| Op::Input(INPUTS[i])),
        4 => (prop::collection::vec(0..ATOMS.len(), 1..4), 1usize..40)
            .prop_map(|(v, cut)| Op::Server(v.into_iter().map(|i| ATOMS[i]).collect(), cut)),
        1 => (0u64..1500).prop_map(Op::Tick),
    ]
}

const COLS: usize = 12;
const ROWS: usize = 5;

fn run(ops: &[Op], preamble: &str) -> Result<(), String> {
    let t0 = Instant::now();
    let mut now = t0;
    let mut p = Predictor::new(COLS, ROWS);
    let mut m = Model::new(COLS, ROWS);
    let server = |p: &mut Predictor, m: &mut Model, chunk: &[u8], now: Instant| {
        m.feed(&p.before_output());
        m.feed(chunk);
        m.feed(&p.feed_output(chunk, now));
    };
    // Get confident first.
    server(&mut p, &mut m, preamble.as_bytes(), now);
    for ch in ["a", "b", "c", "d"] {
        m.feed(&p.on_input(ch.as_bytes(), now));
        server(&mut p, &mut m, ch.as_bytes(), now);
    }
    // The server stream is only ever cut, so the last chunk must end at ground.
    for o in ops {
        now += Duration::from_millis(20);
        match o {
            Op::Input(s) => m.feed(&p.on_input(s.as_bytes(), now)),
            Op::Server(atoms, cut) => {
                let all: Vec<u8> = atoms.concat().into_bytes();
                for chunk in all.chunks(*cut) {
                    server(&mut p, &mut m, chunk, now);
                }
            }
            Op::Tick(ms) => {
                now += Duration::from_millis(*ms);
                m.feed(&p.tick(now));
            }
        }
    }
    // Make the stream end at ground, then let everything time out.
    server(&mut p, &mut m, b"\x1b\\\x07\x18", now);
    now += Duration::from_secs(2);
    m.feed(&p.tick(now));
    m.feed(&p.tick(now));
    let (a, b) = (snapshot(&m.term, COLS, ROWS), snapshot(&p.term, COLS, ROWS));
    if a == b {
        Ok(())
    } else {
        Err(format!("terminal:\n{a}\nmirror:\n{b}\nops: {ops:?}"))
    }
}

/// The user sees what the caller writes: before_output, the server bytes, feed_output.
fn server(p: &mut Predictor, m: &mut Model, chunk: &[u8], now: Instant) -> Vec<u8> {
    m.feed(&p.before_output());
    m.feed(chunk);
    let after = p.feed_output(chunk, now);
    m.feed(&after);
    after
}

fn confident(cols: usize, t: Instant) -> (Predictor, Model) {
    let mut p = Predictor::new(cols, ROWS);
    let mut m = Model::new(cols, ROWS);
    server(&mut p, &mut m, b"$ ", t);
    for ch in ["a", "b", "c"] {
        m.feed(&p.on_input(ch.as_bytes(), t));
        server(&mut p, &mut m, ch.as_bytes(), t);
    }
    (p, m)
}

#[test]
fn the_server_echo_lands_at_the_mirror_cursor_not_the_predicted_one() {
    let t = Instant::now();
    let (mut p, mut m) = confident(COLS, t);
    m.feed(&p.on_input(b"d", t));
    server(&mut p, &mut m, b"d", t);
    assert_eq!(snapshot(&m.term, COLS, ROWS), snapshot(&p.term, COLS, ROWS));
}

#[test]
fn the_servers_saved_cursor_and_pen_survive_a_prediction() {
    let t = Instant::now();
    let (mut p, mut m) = confident(COLS, t);
    server(&mut p, &mut m, b"\x1b7\x1b[1;31m", t);
    m.feed(&p.on_input(b"d", t));
    server(&mut p, &mut m, b"d\x1b[3;3H\x1b8Z", t);
    m.feed(&p.tick(t + Duration::from_secs(2)));
    assert_eq!(snapshot(&m.term, COLS, ROWS), snapshot(&p.term, COLS, ROWS));
}

#[test]
fn nothing_is_written_inside_a_split_escape_sequence() {
    let t = Instant::now();
    let (mut p, mut m) = confident(COLS, t);
    m.feed(&p.on_input(b"d", t));
    for chunk in [&b"\x1b"[..], b"[", b"3", b";3", b"H"] {
        let after = server(&mut p, &mut m, chunk, t);
        if chunk != b"H" {
            assert!(after.is_empty(), "mid-sequence: {after:?}");
        }
    }
    m.feed(&p.tick(t + Duration::from_secs(2)));
    assert_eq!(snapshot(&m.term, COLS, ROWS), snapshot(&p.term, COLS, ROWS));
}

#[test]
fn a_split_utf8_char_from_the_server_is_not_interrupted() {
    let t = Instant::now();
    let (mut p, mut m) = confident(COLS, t);
    m.feed(&p.on_input("漢".as_bytes(), t));
    let bytes = "漢".as_bytes();
    assert!(server(&mut p, &mut m, &bytes[..1], t).is_empty());
    assert!(server(&mut p, &mut m, &bytes[1..2], t).is_empty());
    server(&mut p, &mut m, &bytes[2..], t);
    assert_eq!(snapshot(&m.term, COLS, ROWS), snapshot(&p.term, COLS, ROWS));
}

#[test]
fn a_scroll_does_not_leave_a_stale_underlined_cell() {
    let t = Instant::now();
    let (mut p, mut m) = confident(COLS, t);
    server(&mut p, &mut m, b"\x1b[5;1Hxy", t);
    for ch in ["1", "2", "3"] {
        m.feed(&p.on_input(ch.as_bytes(), t));
        server(&mut p, &mut m, ch.as_bytes(), t);
    }
    m.feed(&p.on_input(b"4", t));
    server(&mut p, &mut m, b"\r\n\r\n", t);
    m.feed(&p.tick(t + Duration::from_secs(2)));
    assert_eq!(snapshot(&m.term, COLS, ROWS), snapshot(&p.term, COLS, ROWS));
}

#[test]
fn leaving_the_alt_screen_repaints_the_main_one() {
    let t = Instant::now();
    let (mut p, mut m) = confident(COLS, t);
    m.feed(&p.on_input(b"d", t));
    server(&mut p, &mut m, b"\x1b[?1049hvim", t);
    server(&mut p, &mut m, b"\x1b[?1049l", t);
    assert_eq!(snapshot(&m.term, COLS, ROWS), snapshot(&p.term, COLS, ROWS));
}

#[test]
fn an_arrow_key_split_across_reads_is_not_typed_text() {
    let t = Instant::now();
    let (mut p, _) = confident(COLS, t);
    p.on_input(b"\x1b", t);
    p.on_input(b"[D", t);
    assert_eq!(p.pending.len(), 1, "one Left, not the chars [ and D");
    assert!(matches!(p.pending[0].kind, Kind::Move));
}

#[test]
fn pasted_text_is_not_predicted() {
    let t = Instant::now();
    let (mut p, _) = confident(COLS, t);
    p.on_input(b"\x1b[200~", t);
    assert!(p.on_input(b"hi", t).is_empty());
    assert!(p.pending.is_empty());
    p.on_input(b"\x1b[20", t);
    p.on_input(b"1~", t);
    p.on_input(b"x", t);
    assert_eq!(
        p.pending.len(),
        1,
        "typing is predicted again after the paste"
    );
}

#[test]
fn a_wrapped_password_prompt_is_detected() {
    let t = Instant::now();
    let (mut p, _) = confident(COLS, t);
    p.feed_output(b"\r\nEnter passphrase for key abc: ", t);
    assert!(p.on_input(b"s", t).is_empty());
    assert!(p.pending.is_empty());
    let (mut p, _) = confident(COLS, t);
    p.feed_output(b"\r\n[sudo] password for me: ", t);
    p.on_input(b"s", t);
    assert!(p.pending.is_empty());
    let (mut p, _) = confident(COLS, t);
    p.feed_output(b"\r\nENTER PASSWORD:", t);
    p.on_input(b"s", t);
    assert!(p.pending.is_empty());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]
    #[test]
    fn rollback_leaves_the_terminal_equal_to_the_mirror(
        ops in prop::collection::vec(op(), 1..25)
    ) {
        run(&ops, "$ ").map_err(TestCaseError::fail)?;
    }
}

/// Proptest's shrunk case: an underlined wide char, backspaced over after
/// a reverse index, then cleared to the end of the screen.
#[test]
fn a_repaint_after_a_wide_char_and_a_reverse_index_matches_the_mirror() {
    let ops = [
        Op::Server(vec!["\u{1b}[4m", "漢"], 1),
        Op::Server(vec!["\n"], 1),
        Op::Server(vec!["\u{1b}M"], 1),
        Op::Server(vec!["$ "], 1),
        Op::Server(vec!["\u{8}", "漢", "\u{8}"], 1),
        Op::Server(vec!["\u{1b}[J"], 1),
        Op::Input("a"),
    ];
    run(&ops, "$ ").unwrap();
}

/// Proptest's shrunk case: an erase in a colour from a wide char's spacer
/// column, then a repaint.
#[test]
fn a_repaint_after_a_coloured_erase_from_a_spacer_matches_the_mirror() {
    let ops = [
        Op::Server(vec!["漢"], 1),
        Op::Server(vec!["\u{8}", "\u{1b}[48;2;1;2;3m", "\u{1b}[K"], 1),
        Op::Input("a"),
        Op::Input("a"),
        Op::Input("a"),
    ];
    run(&ops, "$ ").unwrap();
}
