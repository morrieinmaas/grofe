//! TM.3: copy-mode search and jumps against tmux 3.7b's goldens
//! (`tests/fixtures/compat/golden/tm_3/`), over `search.txt` there printed
//! into an 80x24 pane. Each golden's commands run natively here, in order,
//! and each one's stdout, stderr and exit status must equal what tmux
//! printed; a `styles` step must show the runs of colour tmux drew. A
//! `client` step attaches a pty client (T2.4's harness) and a `keys` step
//! types on it, as the recorder does.
//!
//! The commands come from each `.scenario` and the answers from its
//! `.golden`: the recorder's normalizer rewrites the recording user's
//! name, which is also a directory in the fixture's path.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::fs;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Config;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Processor, Rgb};

use common::{CMD_TIMEOUT, TestServer, wait_until};

use crate::harness::Harness;

const READOUT: &str = "#{copy_cursor_x},#{copy_cursor_y},#{scroll_position} \
    #{search_present} #{search_count} #{search_match}";

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compat")
}

fn read(name: &str, ext: &str) -> String {
    fs::read_to_string(fixtures().join(format!("golden/tm_3/{name}.{ext}"))).unwrap()
}

/// A scenario line's words, split as the replayer splits them.
fn split_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match c {
            ' ' | '\t' if word.is_some() => words.extend(word.take()),
            ' ' | '\t' => {}
            '\'' => {
                let w = word.get_or_insert_default();
                for c in it.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    w.push(c);
                }
            }
            '"' => {
                let w = word.get_or_insert_default();
                while let Some(c) = it.next() {
                    match c {
                        '"' => break,
                        '\\' => match it.next() {
                            Some(n @ ('"' | '\\' | '$' | '`')) => w.push(n),
                            Some(n) => {
                                w.push('\\');
                                w.push(n);
                            }
                            None => w.push('\\'),
                        },
                        c => w.push(c),
                    }
                }
            }
            '\\' => word.get_or_insert_default().extend(it.next()),
            c => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    words
}

/// The bytes a `keys` step types (the recorder's `key_bytes`).
fn key_bytes(words: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for word in words {
        match word.as_str() {
            "Space" => out.push(b' '),
            "Escape" => out.push(0x1b),
            "Enter" => out.push(b'\r'),
            "Tab" => out.push(b'\t'),
            "BSpace" => out.push(0x7f),
            w => match w.strip_prefix("C-").map(str::as_bytes) {
                Some([c]) => out.push(c.to_ascii_lowercase() - b'a' + 1),
                _ => out.extend_from_slice(w.as_bytes()),
            },
        }
    }
    out
}

enum Step {
    Run {
        line: String,
        args: Vec<String>,
        want: (String, String, i32),
    },
    Client(u16, u16),
    Keys(Vec<u8>),
    Styles {
        first: u16,
        last: u16,
        until: Option<String>,
        want: Vec<String>,
    },
}

fn is_step(line: &str) -> bool {
    ["$ ", "client ", "keys ", "styles "]
        .iter()
        .any(|p| line.starts_with(p))
}

/// Golden `name`'s steps: each step line of the scenario, with what the
/// golden recorded after it.
fn load(name: &str) -> Vec<Step> {
    let scenario = read(name, "scenario");
    let mut lines = scenario.lines().filter(|l| is_step(l));
    let mut steps: Vec<Step> = Vec::new();
    for line in read(name, "golden").lines() {
        if is_step(line) {
            let line = lines.next().expect("the golden has more steps");
            if let Some(args) = line.strip_prefix("$ ") {
                let args = split_words(args)
                    .into_iter()
                    .map(|a| a.replace("{fixtures}", &fixtures().display().to_string()))
                    .collect();
                steps.push(Step::Run {
                    line: line.to_string(),
                    args,
                    want: (String::new(), String::new(), 0),
                });
            } else if let Some(rest) = line.strip_prefix("client ") {
                let size = rest.split(' ').next().unwrap();
                let (c, r) = size.split_once('x').unwrap();
                steps.push(Step::Client(c.parse().unwrap(), r.parse().unwrap()));
            } else if let Some(rest) = line.strip_prefix("keys ") {
                steps.push(Step::Keys(key_bytes(&split_words(rest))));
            } else if let Some(rest) = line.strip_prefix("styles ") {
                let (range, until) = match rest.split_once(' ') {
                    Some((r, u)) => (r, Some(u.to_string())),
                    None => (rest, None),
                };
                let (a, b) = range.split_once('-').unwrap();
                steps.push(Step::Styles {
                    first: a.parse().unwrap(),
                    last: b.parse().unwrap(),
                    until,
                    want: Vec::new(),
                });
            }
            continue;
        }
        match steps.last_mut() {
            Some(Step::Styles { want, .. }) => {
                want.push(line.strip_prefix("~ ").unwrap().to_string());
            }
            Some(Step::Run { want, .. }) => {
                if let Some(status) = line.strip_prefix("? ") {
                    want.2 = status.parse().unwrap();
                } else if let Some(err) = line.strip_prefix("! ") {
                    want.1.push_str(err);
                    want.1.push('\n');
                } else {
                    want.0.push_str(line);
                    want.0.push('\n');
                }
            }
            _ => panic!("{name}: output before a command: {line}"),
        }
    }
    assert!(
        lines.next().is_none(),
        "{name}: the scenario has more steps"
    );
    steps
}

/// Runs golden `name` on a fresh server, comparing each command with tmux;
/// `differs` gives grofe's own answer for the commands where grofe is
/// documented to differ, by their line.
fn replay_with(name: &str, differs: &[(&str, &str)]) {
    let srv = TestServer::start();
    let mut client: Option<Harness> = None;
    // Typed keys reach the server in order but after the CLI's commands
    // may: the first command after them is retried until it shows what
    // tmux's did, instead of sleeping.
    let mut typed = false;
    let mut previous = String::new();
    for step in load(name) {
        match step {
            Step::Run {
                line,
                args,
                mut want,
            } => {
                if let Some((_, ours)) = differs.iter().find(|(l, _)| *l == previous) {
                    want.0 = format!("{ours}\n");
                }
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                let run = || {
                    let out = srv.run(&args);
                    let mut stdout = out.stdout;
                    if !stdout.is_empty() && !stdout.ends_with('\n') {
                        stdout.push('\n');
                    }
                    (stdout, out.stderr, out.status.code().unwrap_or(-1))
                };
                if std::mem::take(&mut typed) {
                    let _ = wait_until(|| run() == want, CMD_TIMEOUT);
                }
                assert_eq!(run(), want, "{name}: {line} (after {previous})");
                previous = line;
            }
            Step::Client(cols, rows) => {
                let a = Harness::spawn_attach_with_env(srv.command(), "main", &[], cols, rows);
                wait_until(
                    || !srv.cmd(&["list-clients"]).unwrap_or_default().is_empty(),
                    CMD_TIMEOUT,
                )
                .expect("client attached");
                client = Some(a);
            }
            Step::Keys(bytes) => {
                let a = client.as_mut().expect("keys before a client");
                a.send(&bytes);
                let _ = a.text();
                typed = true;
            }
            Step::Styles {
                first,
                last,
                until,
                want,
            } => {
                let a = client.as_mut().expect("styles before a client");
                let got = styles(a, first, last, until.as_deref());
                assert_eq!(got, want, "{name}: styles {first}-{last} after {previous}");
            }
        }
    }
}

fn replay(name: &str) {
    replay_with(name, &[]);
}

// ---- styles, as the recorder reads them --------------------------------------

const ANSI_NAMES: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "brightblack",
    "brightred",
    "brightgreen",
    "brightyellow",
    "brightblue",
    "brightmagenta",
    "brightcyan",
    "brightwhite",
];

/// How a cell looks, as the recorder's `style_of` prints it.
fn style_of(cell: &Cell) -> Option<String> {
    let blank = cell.c == ' ' || cell.c == '\0';
    let colour = |c: &AnsiColor, default: NamedColor| match c {
        AnsiColor::Named(n) if *n == default => None,
        AnsiColor::Named(n) => Some(format!("{n:?}").to_lowercase()),
        AnsiColor::Indexed(i @ 0..16) => Some(ANSI_NAMES[usize::from(*i)].to_string()),
        AnsiColor::Indexed(i) => Some(format!("idx:{i}")),
        AnsiColor::Spec(Rgb { r, g, b }) => Some(format!("rgb:{r:02x}/{g:02x}/{b:02x}")),
    };
    let fg = colour(&cell.fg, NamedColor::Foreground).filter(|_| !blank);
    let bg = colour(&cell.bg, NamedColor::Background);
    let attrs = [
        (Flags::BOLD, "bold"),
        (Flags::DIM, "dim"),
        (Flags::ITALIC, "italic"),
        (Flags::UNDERLINE, "underline"),
        (Flags::INVERSE, "reverse"),
        (Flags::STRIKEOUT, "strikeout"),
    ]
    .into_iter()
    .filter(|(f, _)| cell.flags.contains(*f))
    .filter(|(f, _)| !blank || !matches!(*f, Flags::BOLD | Flags::DIM | Flags::ITALIC))
    .map(|(_, name)| name)
    .collect::<Vec<_>>();
    if fg.is_none() && bg.is_none() && attrs.is_empty() {
        return None;
    }
    let mut look = format!(
        "fg={} bg={}",
        fg.as_deref().unwrap_or("default"),
        bg.as_deref().unwrap_or("default")
    );
    for a in attrs {
        look.push(' ');
        look.push_str(a);
    }
    Some(look)
}

/// The runs of colour on rows `first..=last` once the client shows
/// `until` and its output has been quiet a moment.
fn styles(a: &mut Harness, first: u16, last: u16, until: Option<&str>) -> Vec<String> {
    let deadline = Instant::now() + CMD_TIMEOUT;
    let mut seen = 0;
    let mut quiet_since = Instant::now();
    loop {
        let text = a.text();
        let len = a.output().len();
        if len != seen {
            seen = len;
            quiet_since = Instant::now();
        }
        if quiet_since.elapsed() >= Duration::from_millis(300)
            && until.is_none_or(|t| text.contains(t))
        {
            break;
        }
        assert!(Instant::now() < deadline, "the client never settled");
        sleep(Duration::from_millis(20));
    }
    let size = TermSize::new(80, 25);
    let mut term = Term::new(Config::default(), &size, VoidListener);
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, a.output());
    let grid = term.grid();
    let mut runs = Vec::new();
    for y in first..=last {
        let row = &grid[Line(i32::from(y))];
        let mut x = 0;
        while x < grid.columns() {
            let look = style_of(&row[Column(x)]);
            let start = x;
            while x < grid.columns() && style_of(&row[Column(x)]) == look {
                x += 1;
            }
            if let Some(look) = look {
                runs.push(format!("{y} {start}-{} {look}", x - 1));
            }
        }
    }
    runs
}

// ---- The goldens -------------------------------------------------------------

/// Case 1 and 3: `fo+[0-9]` lands on each match in order; `n` past the
/// last match stays with `wrap-search` off and wraps with it on; `N`,
/// `search-backward`, counts, a pattern that does not compile, the marks
/// going on a selection command, and the pane's last search in its next
/// copy mode.
#[test]
fn regex_search_equals_the_golden() {
    replay("regex");
}

/// Case 2: `-text` takes `a.b` literally; the regex `a.b` does not.
#[test]
fn text_search_equals_the_golden() {
    replay("text");
}

/// Case 4: matches split by a soft wrap, both ways.
#[test]
fn soft_wrapped_matches_equal_the_golden() {
    replay("softwrap");
}

/// Case 6: each jump, `;` and `,` after it, counts, across a wrap, a tab.
#[test]
fn jumps_equal_the_golden() {
    replay("jumps");
}

/// `/`, `?`, `n`, `N`, `#`, `*` and the one-key jump prompts through a
/// client.
#[test]
fn keys_through_a_client_equal_the_golden() {
    replay("keys");
}

/// A copy with no selection copies the search match at the cursor
/// (tmux's `window_copy_get_selection`); with none there, nothing.
#[test]
fn a_copy_with_no_selection_copies_the_match() {
    replay("copy-match");
}

/// Case 5, where grofe matches glibc: escapes, brackets, classes,
/// boundaries, intervals, anchors and refused patterns.
#[test]
fn the_regex_dialect_equals_the_golden() {
    replay("dialect");
}

/// `copy-mode-match-style` and `copy-mode-current-match-style`, their
/// defaults and changed, and the marks going.
#[test]
fn match_styles_equal_the_golden() {
    replay("styles");
}

/// Case 5, alternation and back-references: alternation takes the
/// longest match at the leftmost position, as POSIX does (`d(e|ee)` matches
/// all of `dee`); a back-reference, which the crate cannot match, finds
/// nothing (`docs/copy-mode.md`). Every other line equals tmux's.
#[test]
fn alternation_is_longest_and_back_references_find_nothing() {
    let differs = [
        (
            "$ send-keys -t main -X search-forward '(ab)\\1'",
            "0,0,36 0  ",
        ),
        (
            "$ send-keys -t main -X search-backward '(ab)\\1'",
            "0,23,0 0  ",
        ),
    ];
    replay_with("differs", &differs);
}

/// Classes, case and ranges outside ASCII, as glibc reads them in
/// `C.UTF-8` (`docs/copy-mode.md` lists where grofe's Unicode properties
/// only approximate glibc's tables).
#[test]
fn unicode_classes_equal_the_golden() {
    replay("unicode");
}

// ---- Off the actor ---------------------------------------------------------------

/// A forced frame every 1/60 s.
const FRAME: Duration = Duration::from_micros(16_667);
/// The longest frame interval the acceptance text allows.
const FRAME_LIMIT: Duration = Duration::from_millis(50);

fn display(srv: &TestServer, target: &str, fmt: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, fmt])
        .unwrap()
        .trim_end_matches('\n')
        .to_string()
}

/// Types into `a` at 60 Hz and reads both clients until `done` holds or
/// `max` passes: the longest gap between two reads that brought a client
/// output (TC.4's method), and how many there were.
fn force_frames(
    a: &mut Harness,
    b: &mut Harness,
    max: Duration,
    mut done: impl FnMut() -> bool,
) -> (Duration, usize) {
    let start = Instant::now();
    let mut next_key = start;
    let mut typed = 0usize;
    let mut seen = [a.output().len(), b.output().len()];
    let mut last = [start, start];
    let mut longest = Duration::ZERO;
    let mut frames = 0;
    loop {
        let now = Instant::now();
        if now >= next_key {
            a.send(if typed % 40 == 39 { "\r" } else { "x" });
            typed += 1;
            next_key += FRAME;
        }
        for (i, h) in [&mut *a, &mut *b].into_iter().enumerate() {
            let len = h.output().len();
            if len > seen[i] {
                seen[i] = len;
                longest = longest.max(now - last[i]);
                last[i] = now;
                frames += 1;
            }
        }
        if done() || now - start >= max {
            break;
        }
        sleep(Duration::from_millis(1));
    }
    let end = Instant::now();
    for at in last {
        longest = longest.max(end - at);
    }
    (longest, frames)
}

/// Case 7: two clients attached to a session running `cat`, frames forced
/// at 60 Hz; meanwhile a 50 000-line history in another session is
/// searched end to end for a pattern it does not hold, ten times over
/// (`-N 10`, each wrapping once). No frame waits more than 50 ms (or twice
/// the machine's own baseline, on a loaded machine, as TC.4 allows), and
/// the request waits for the whole search.
#[test]
fn a_search_over_a_full_history_never_delays_a_frame() {
    let srv = TestServer::start();
    srv.cmd(&["set-option", "-g", "history-limit", "50000"])
        .unwrap();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "s",
        "-x",
        "80",
        "-y",
        "24",
        "cat",
    ])
    .unwrap();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "big",
        "-x",
        "80",
        "-y",
        "24",
        "seq -f 'line %g of the history, some text to search' 1 50100; exec sleep 100000",
    ])
    .unwrap();
    wait_until(
        || display(&srv, "big", "#{history_size}") == "50000",
        Duration::from_secs(30),
    )
    .expect("a full history");
    srv.cmd(&["copy-mode", "-t", "big"]).unwrap();
    let mut a = Harness::spawn_attach_with_env(srv.command(), "s", &[], 80, 24);
    let mut b = Harness::spawn_attach_with_env(srv.command(), "s", &[], 80, 24);
    for h in [&mut a, &mut b] {
        h.wait_until("the status row", CMD_TIMEOUT, |text| {
            text.lines().last().unwrap_or_default().starts_with("[s]")
        });
    }
    let (baseline, _) = force_frames(&mut a, &mut b, Duration::from_secs(3), || false);

    let search = {
        let mut cmd = srv.command();
        cmd.args([
            "send-keys",
            "-t",
            "big",
            "-X",
            "-N",
            "10",
            "search-backward",
            "no(such|line)[x-z]+",
        ]);
        std::thread::spawn(move || {
            let started = Instant::now();
            let out = cmd.output().expect("search");
            (out.status.success(), started.elapsed())
        })
    };
    let (longest, frames) = force_frames(&mut a, &mut b, Duration::from_secs(60), || {
        search.is_finished()
    });
    let (ok, took) = search.join().unwrap();
    assert!(ok, "the search failed");
    let bound = FRAME_LIMIT.max(baseline * 2);
    eprintln!(
        "search took {took:?}; longest frame gap {longest:?}, baseline {baseline:?}, {frames} frame reads"
    );
    assert!(
        took > Duration::from_millis(100),
        "the search was too short to show anything: {took:?}"
    );
    assert!(frames > 5, "frames were not forced: {frames}");
    assert!(
        longest < bound,
        "a frame waited {longest:?} (baseline {baseline:?}, bound {bound:?})"
    );
    // Nothing found: the cursor stays at the bottom.
    assert_eq!(
        display(
            &srv,
            "big",
            "#{copy_cursor_y},#{scroll_position} #{search_present}"
        ),
        "23,0 0"
    );
}

/// While a search runs, the mode's next commands wait in order, and so
/// does the request that sent them: `n` three times in one `send-keys`
/// lands where three separate ones do, and a `display` after a search in
/// the same request shows its result.
#[test]
fn commands_behind_a_search_run_in_order() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "24",
        "seq -f 'foo%g' 1 3000; exec sleep 100000",
    ])
    .unwrap();
    wait_until(
        || display(&srv, "main", "#{history_size}") != "0",
        CMD_TIMEOUT,
    )
    .unwrap();
    wait_until(|| srv.capture("main").contains("foo3000"), CMD_TIMEOUT).unwrap();
    srv.cmd(&["copy-mode", "-t", "main"]).unwrap();
    let out = srv
        .cmd(&[
            "send-keys",
            "-t",
            "main",
            "-X",
            "search-backward",
            "foo[0-9]+0",
            ";",
            "display-message",
            "-p",
            "-t",
            "main",
            READOUT,
        ])
        .unwrap();
    assert_eq!(out.trim_end(), "0,22,0 1 723 foo3000");
    srv.cmd(&["send-keys", "-t", "main", "n", "n", "n"])
        .unwrap();
    let three = display(&srv, "main", READOUT);
    assert_eq!(three, "0,18,26 1 723 foo2970");
    srv.cmd(&["send-keys", "-t", "main", "N", "N", "N"])
        .unwrap();
    for _ in 0..3 {
        srv.cmd(&["send-keys", "-t", "main", "n"]).unwrap();
    }
    assert_eq!(display(&srv, "main", READOUT), three);
    // A held `cancel` leaves the mode once the search ends.
    srv.cmd(&[
        "send-keys",
        "-t",
        "main",
        "-X",
        "search-again",
        ";",
        "send-keys",
        "-t",
        "main",
        "-X",
        "cancel",
    ])
    .unwrap();
    assert_eq!(display(&srv, "main", "#{pane_in_mode}"), "0");
}
