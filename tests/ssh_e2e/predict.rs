//! Predictive echo over a slow real link (spec ssh-remote §4): with
//! `e2e_netem delay 200ms` and `predictive-echo auto`, a typed key shows at
//! once, underlined, before its echo comes back, which then replaces it.

use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Config;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi::Processor;

use crate::harness::{E2e, REMOTE_TIMEOUT};
use crate::pty::Harness;

const DELAY: Duration = Duration::from_millis(200);

/// The characters drawn underlined on the 80x24 screen `raw` leaves.
fn underlined(raw: &[u8]) -> String {
    let mut term = Term::new(Config::default(), &TermSize::new(80, 24), VoidListener);
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, raw);
    let grid = term.grid();
    let mut out = String::new();
    for row in 0..grid.screen_lines() {
        for col in 0..grid.columns() {
            let cell = &grid[Line(row as i32)][Column(col)];
            if cell.flags.contains(Flags::UNDERLINE) && cell.c != ' ' {
                out.push(cell.c);
            }
        }
    }
    out
}

fn wait_underlined(h: &mut Harness, want: &str) {
    let deadline = Instant::now() + REMOTE_TIMEOUT;
    loop {
        let got = underlined(h.output());
        if got == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "underlined {got:?}, want {want:?}; screen:\n{}",
            h.text()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// `#{client_rtt}` in ms once it is at least `min`.
fn wait_rtt(e: &E2e, min: u64) -> u64 {
    let deadline = Instant::now() + REMOTE_TIMEOUT;
    loop {
        let out = e.ssh("grofe list-clients -F '#{client_rtt}'");
        let said = String::from_utf8_lossy(&out.stdout).into_owned();
        if let Some(ms) = said
            .lines()
            .find_map(|l| l.trim().strip_suffix("ms")?.parse::<u64>().ok())
            && ms >= min
        {
            return ms;
        }
        assert!(Instant::now() < deadline, "client_rtt stayed {said:?}");
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
fn with_200ms_latency_typed_keys_show_underlined_then_the_echo_confirms() {
    let e = e2e_or_skip!();
    let _cleanup = e.server_cleanup(true);
    let local = e.local();
    let mut cmd = local.grofe(&["-r", &e.host]);
    // Pings every second, so the RTT is measured soon.
    cmd.env("GROFE_TEST_PING_MS", "1000");
    let mut h = Harness::spawn(cmd, 80, 24);
    h.wait_for_timeout("$ ", REMOTE_TIMEOUT);
    let out = e.helper("e2e_netem", &["delay", "200ms"]);
    assert!(out.status.success(), "{out:?}");
    // auto predicts from 50 ms.
    wait_rtt(&e, 50);
    // Three echoes confirmed in a row make the predictor confident.
    for (i, key) in ["e", "c", "h"].into_iter().enumerate() {
        h.send(key);
        h.wait_for_timeout(&format!("$ {}", &"ech"[..=i]), REMOTE_TIMEOUT);
    }
    assert_eq!(underlined(h.output()), "");
    // The next key shows at once, underlined (the server never underlines
    // typed text), before its echo can be back...
    let sent = Instant::now();
    h.send("o");
    wait_underlined(&mut h, "o");
    let shown = sent.elapsed();
    assert!(shown < DELAY, "drawn only after {shown:?}");
    // ...and the echo replaces it with the server's own cell.
    wait_underlined(&mut h, "");
    h.wait_for_timeout("$ echo", REMOTE_TIMEOUT);
    h.send("\x15");
    h.send("\x02d");
    h.wait_exit();
}
