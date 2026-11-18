//! Predictive local echo in the remote client (spec ssh-remote §4) over a
//! slow link: `grofe -r` against a real server through a fake ssh (as in
//! `remote_client.rs`) whose output to the client goes through a pipe
//! that holds every chunk back for [`DELAY`]. The RTT the client measures
//! is then well over `predictive-echo auto`'s 50 ms.

mod common;
#[path = "acceptance/harness.rs"]
#[allow(dead_code)]
mod harness;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Config;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi::Processor;
use common::TestServer;
use harness::Harness;
use tempfile::TempDir;

/// How long the link holds back the remote's output.
const DELAY: Duration = Duration::from_millis(300);
const WAIT: Duration = Duration::from_secs(20);
/// The pen the predictor draws a shown key with.
const PREDICTED: &[u8] = b"\x1b[0;4m";

/// Holds every chunk read from stdin back for `$1` seconds, in order.
const DELAY_PIPE: &str = r#"
use strict; use Time::HiRes qw(time);
binmode STDIN; binmode STDOUT;
my $d = $ARGV[0]; my @q; my $eof = 0;
while (!$eof || @q) {
    my $now = time;
    while (@q && $q[0][0] <= $now) {
        my $b = (shift @q)->[1]; my $off = 0;
        while ($off < length $b) {
            my $n = syswrite(STDOUT, $b, length($b) - $off, $off);
            exit 0 unless defined $n;
            $off += $n;
        }
    }
    my $wait = @q ? $q[0][0] - time : undef;
    $wait = 0 if defined $wait && $wait < 0;
    if ($eof) { select(undef, undef, undef, $wait); next; }
    my $rin = ''; vec($rin, fileno(STDIN), 1) = 1;
    if (select(my $rout = $rin, undef, undef, $wait) > 0) {
        my $n = sysread(STDIN, my $buf, 65536);
        if (!$n) { $eof = 1 } else { push @q, [time + $d, $buf] }
    }
}
"#;

struct Remote {
    srv: TestServer,
    dir: TempDir,
}

impl Remote {
    fn new() -> Remote {
        let srv = TestServer::start();
        let dir = tempfile::Builder::new()
            .prefix("grofe-rp")
            .tempdir()
            .expect("tempdir");
        let root = dir.path();
        for d in ["home", "bin", "local-home", "local-config"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_grofe"), root.join("bin/grofe")).unwrap();
        fs::write(root.join("delay.pl"), DELAY_PIPE).unwrap();
        let script = format!(
            r#"#!/bin/sh
for a; do [ "$a" = -G ] && {{ echo "serveraliveinterval 0"; exit 0; }}; done
while [ $# -gt 0 ] && [ "$1" != -- ]; do shift; done
shift
# The destination may follow `--` (ssh -T [opts] -- <dest> <cmd>).
[ "$1" = box ] && shift
export HOME='{root}/home' PATH='{root}/bin:/usr/bin:/bin' GROFE='{sock}'
export GROFE_NOTIFY_DESKTOP=off
export XDG_STATE_HOME='{state}' XDG_RUNTIME_DIR='{run}' XDG_CONFIG_HOME='{cfg}'
unset GROFE_PANE TMUX
/bin/sh -c "$*" | perl '{root}/delay.pl' {delay}
"#,
            root = root.display(),
            sock = srv.socket.display(),
            state = srv.state_home.display(),
            run = srv.runtime_dir.display(),
            cfg = srv.config_home.display(),
            delay = DELAY.as_secs_f64(),
        );
        let ssh = root.join("ssh");
        fs::write(&ssh, script).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        Remote { srv, dir }
    }

    /// `grofe -r box`, attached to `main`, with pings often enough to
    /// measure the RTT soon (but slower than the delay).
    fn attach(&self) -> Harness {
        let root = self.dir.path();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_grofe"));
        cmd.env("GROFE_NOTIFY_DESKTOP", "off");
        cmd.args(["-r", "box"])
            .current_dir(root)
            .env_remove("GROFE")
            .env_remove("GROFE_PANE")
            .env_remove("TMUX")
            .env("GROFE_SSH", root.join("ssh"))
            .env("GROFE_TEST_PING_MS", "1000")
            .env("HOME", root.join("local-home"))
            .env("XDG_CONFIG_HOME", root.join("local-config"))
            .env("TERM", "xterm-256color")
            .env_remove("COLORTERM");
        Harness::spawn(cmd, 80, 24)
    }

    /// `#{client_rtt}` of the one client, once the client reported it.
    fn client_rtt(&self) -> u64 {
        let deadline = Instant::now() + WAIT;
        loop {
            let out = self
                .srv
                .cmd(&["list-clients", "-F", "#{client_rtt}"])
                .unwrap_or_default();
            if let Some(ms) = out.trim().strip_suffix("ms") {
                return ms.parse().expect("client_rtt in ms");
            }
            assert!(Instant::now() < deadline, "no client_rtt: {out:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// The characters drawn underlined on the screen `raw` leaves.
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

/// Waits until the screen `raw` leaves has exactly `want` underlined.
fn wait_underlined(h: &mut Harness, want: &str) {
    let deadline = Instant::now() + WAIT;
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

#[test]
fn typed_keys_show_underlined_at_once_and_the_echo_confirms_them() {
    let r = Remote::new();
    r.srv.cmd(&["new-session", "-d", "-s", "main"]).unwrap();
    let mut h = r.attach();
    h.wait_for(common::PROMPT.trim());
    let rtt = r.client_rtt();
    assert!(rtt >= 50, "the delayed link measured {rtt} ms");

    // Three echoes confirmed in a row make the predictor confident.
    for (i, key) in ["e", "c", "h"].into_iter().enumerate() {
        h.send(key);
        h.wait_for(&format!("$ {}", &"ech"[..=i]));
    }
    assert_eq!(underlined(h.output()), "");

    // The next key shows at once, underlined (the server never underlines
    // typed text), long before its echo can come back...
    let sent = Instant::now();
    h.send("o");
    wait_underlined(&mut h, "o");
    assert!(
        sent.elapsed() < DELAY,
        "drawn only after {:?}",
        sent.elapsed()
    );
    // ...and the echo replaces it with the server's own cell.
    wait_underlined(&mut h, "");
    h.wait_for("$ echo");

    h.send("\x02d");
    h.wait_exit();
}

#[test]
fn a_password_read_with_echo_off_gets_no_prediction() {
    let r = Remote::new();
    r.srv.cmd(&["new-session", "-d", "-s", "main"]).unwrap();
    let mut h = r.attach();
    h.wait_for(common::PROMPT.trim());
    r.client_rtt();
    h.send("stty -echo; read pw; stty echo; echo got-$pw\r");
    // The server polls the pty's echo flag every 2 s.
    std::thread::sleep(Duration::from_millis(2500));
    let from = h.output().len();
    for key in ["s", "e", "c", "r", "e", "t"] {
        h.send(key);
        std::thread::sleep(DELAY);
    }
    std::thread::sleep(DELAY * 2);
    assert!(!h.output()[from..].windows(6).any(|w| w == PREDICTED));
    assert!(
        !h.output()[from..].windows(6).any(|w| w == b"secret"),
        "the password reached the screen"
    );
    assert!(!h.text().contains("secret"));
    h.send("\r");
    h.wait_for("got-secret");

    h.send("\x02d");
    h.wait_exit();
}

#[test]
fn predictive_echo_off_in_the_local_config_draws_nothing_ahead() {
    let r = Remote::new();
    r.srv.cmd(&["new-session", "-d", "-s", "main"]).unwrap();
    // The remote's own setting does not count: the option is this machine's.
    r.srv
        .cmd(&["set-option", "-g", "predictive-echo", "on"])
        .unwrap();
    let config = r.dir.path().join("local-config/grofe");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("config.toml"), "predictive-echo = \"off\"\n").unwrap();
    let mut h = r.attach();
    h.wait_for(common::PROMPT.trim());
    r.client_rtt();
    for (i, key) in ["e", "c", "h", "o", " ", "x"].into_iter().enumerate() {
        h.send(key);
        h.wait_for(&format!("$ {}", &"echo x"[..=i]));
    }
    // What the predictor writes for a shown key, at any point.
    assert!(!h.output().windows(6).any(|w| w == PREDICTED));

    h.send("\x02d");
    h.wait_exit();
}
