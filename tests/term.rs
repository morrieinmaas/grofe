//! Tests for inner-`TERM` resolution (D7 / T0.4): the terminfo-dir lookup in
//! isolation, plus expectrl evidence for the decision to stay on
//! `screen-256color` rather than `tmux-256color`. See the module doc on
//! `grofe::term` and docs/spike-notes.md's "TERM decision" section.

use std::fs;

use grofe::term;

fn fake_entry(root: &std::path::Path, subdir: &str, term_name: &str) {
    let dir = root.join(subdir);
    fs::create_dir_all(&dir).expect("create fake terminfo dir");
    fs::write(dir.join(term_name), b"").expect("write fake terminfo entry");
}

#[test]
fn entry_found_in_letter_layout() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fake_entry(tmp.path(), "s", "screen-256color");
    assert!(term::entry_exists(
        &[tmp.path().to_path_buf()],
        "screen-256color"
    ));
}

#[test]
fn entry_found_in_hex_layout() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // 's' is 0x73; macOS's ncurses shards by the two-digit hex byte, not the
    // literal letter.
    fake_entry(tmp.path(), "73", "screen-256color");
    assert!(term::entry_exists(
        &[tmp.path().to_path_buf()],
        "screen-256color"
    ));
}

#[test]
fn missing_default_falls_back() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // No screen-256color entry anywhere in the candidate dirs: resolution
    // must skip straight to the last resort.
    fake_entry(tmp.path(), "x", "xterm-256color");
    assert_eq!(
        term::resolve_from(&[tmp.path().to_path_buf()]),
        "xterm-256color"
    );
}

#[test]
fn resolves_on_real_host() {
    let resolved = term::resolve_term();
    assert!(!resolved.is_empty());
    assert!(
        ["screen-256color", "xterm-256color"].contains(&resolved),
        "unexpected TERM candidate: {resolved}"
    );
    eprintln!("resolved TERM on this host: {resolved}");
    assert_eq!(term::COLORTERM, "truecolor");
}

/// Live evidence for the D7 decision: spawn the T0.3 spike, run three of the
/// SGR extras `tmux-256color`'s terminfo advertises over `screen-256color`'s
/// through it (italic `sitm`/`ritm`, strikeout `smxx`/`rmxx`, and curly
/// underline, one of `Smulx`'s extended styles), and check what actually
/// reaches a second `Term` reconstructing the outer screen.
///
/// Italic and strikeout survive the round trip: `alacritty_terminal` parses
/// them into `Flags::ITALIC` / `Flags::STRIKEOUT`, and the spike's `sgr()`
/// re-emits plain SGR `3` / `9` for them. The curly underline does not: the
/// spike's `render()` collapses every `Flags::ALL_UNDERLINES` variant to
/// plain SGR `4`, so `Flags::UNDERCURL` is set internally but never reaches
/// the outer terminal as anything but a regular underline. That gap is why
/// D7's "every extra capability works end to end" bar is not met.
#[test]
fn smulx_underline_style_does_not_survive_the_spike() {
    use std::io::{self, Write};
    use std::process::Command;
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use alacritty_terminal::Term;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::Processor;
    use expectrl::Session;
    use expectrl::session::OsSession;

    const COLS: usize = 100;
    const ROWS: usize = 30;
    const TIMEOUT: Duration = Duration::from_secs(10);

    fn pump(
        session: &mut OsSession,
        term: &mut Term<VoidListener>,
        parser: &mut Processor,
    ) -> bool {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match session.try_read(&mut buf) {
                Ok(0) => return false,
                Ok(n) => parser.advance(term, &buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
    }

    fn screen_text(term: &Term<VoidListener>) -> String {
        let grid = term.grid();
        let mut out = String::new();
        for line in 0..grid.screen_lines() {
            for col in 0..grid.columns() {
                out.push(grid[Line(line as i32)][Column(col)].c);
            }
            out.push('\n');
        }
        out
    }

    fn wait_for(
        session: &mut OsSession,
        term: &mut Term<VoidListener>,
        parser: &mut Processor,
        needle: &str,
        deadline: Instant,
    ) {
        loop {
            let open = pump(session, term, parser);
            if screen_text(term).contains(needle) {
                return;
            }
            assert!(
                open && Instant::now() < deadline,
                "timed out waiting for {needle:?}; screen:\n{}",
                screen_text(term)
            );
            sleep(Duration::from_millis(20));
        }
    }

    fn find_flags(term: &Term<VoidListener>, needle: &str) -> Flags {
        let grid = term.grid();
        for line in 0..grid.screen_lines() {
            let row: String = (0..grid.columns())
                .map(|col| grid[Line(line as i32)][Column(col)].c)
                .collect();
            if let Some(col) = row.find(needle) {
                return grid[Line(line as i32)][Column(col)].flags;
            }
        }
        panic!("{needle:?} not found on screen:\n{}", screen_text(term));
    }

    let status = Command::new(env!("CARGO"))
        .args(["build", "--example", "spike"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("run cargo build");
    assert!(status.success(), "cargo build --example spike failed");
    let exe = std::env::current_exe().expect("current_exe");
    let spike_bin = exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("target profile dir")
        .join("examples/spike");

    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(format!(
            "stty cols {COLS} rows {ROWS} && exec \"$0\" -- \"$@\""
        ))
        .arg(spike_bin)
        .arg("/bin/sh");
    let mut session: OsSession = Session::spawn(cmd).expect("spawn spike");
    let mut term = Term::new(Config::default(), &TermSize::new(COLS, ROWS), VoidListener);
    let mut parser = Processor::new();
    let deadline = Instant::now() + TIMEOUT;

    // Turn off tty echo first, on its own line, so the *next* line's raw
    // bytes (which look like escape codes as plain typed text) are not
    // echoed back verbatim. Only `printf`'s real output should appear.
    session.write_all(b"stty -echo\r").expect("write");
    session.flush().expect("flush");
    sleep(Duration::from_millis(300));

    session
        .write_all(b"printf '\\033[3mBB\\033[23m \\033[9mCC\\033[29m \\033[4:3mDD\\033[24m\\n'\r")
        .expect("write");
    session.flush().expect("flush");
    wait_for(&mut session, &mut term, &mut parser, "DD", deadline);

    assert!(
        find_flags(&term, "BB").contains(Flags::ITALIC),
        "sitm/ritm (italic) should render end to end"
    );
    assert!(
        find_flags(&term, "CC").contains(Flags::STRIKEOUT),
        "smxx/rmxx (strikeout) should render end to end"
    );
    let underline_flags = find_flags(&term, "DD");
    assert!(
        underline_flags.contains(Flags::UNDERLINE),
        "outer screen should still see a plain underline"
    );
    assert!(
        !underline_flags.contains(Flags::UNDERCURL),
        "the curly underline style is lost by the spike's renderer (the D7 gap)"
    );

    // Let the spike exit cleanly (Ctrl-]).
    session.write_all(b"\x1d").expect("write");
    session.flush().expect("flush");
    let exit_deadline = Instant::now() + TIMEOUT;
    loop {
        pump(&mut session, &mut term, &mut parser);
        if !session.get_process().is_alive().unwrap_or(false) {
            break;
        }
        assert!(Instant::now() < exit_deadline, "spike did not exit");
        sleep(Duration::from_millis(20));
    }
}
