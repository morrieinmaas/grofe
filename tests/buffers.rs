//! TM.4: paste buffers against tmux 3.7b's goldens
//! (`tests/fixtures/compat/golden/tm_4/`). Each golden's commands run
//! natively here, in order, and each one's stdout, stderr and exit status
//! must equal what tmux printed; a `capture-pane` is retried until the
//! pane shows what tmux's did (the paste reaches the pane's program
//! asynchronously).

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{CMD_TIMEOUT, TestServer, wait_until};
use harness::Harness;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compat/golden/tm_4")
}

/// One recorded command: its args, and what tmux printed.
struct Step {
    line: String,
    args: Vec<String>,
    stdout: String,
    stderr: String,
    status: i32,
}

/// Splits a scenario line's args as the replayer does (a POSIX shell's
/// quoting: single quotes, double quotes with `\"`, `\\`, `\$`, and a bare
/// backslash).
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
                            Some(n @ ('"' | '\\' | '$')) => w.push(n),
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

fn load(name: &str) -> Vec<Step> {
    let text = fs::read_to_string(golden_dir().join(format!("{name}.golden"))).unwrap();
    let mut steps: Vec<Step> = Vec::new();
    for line in text.lines() {
        if let Some(args) = line.strip_prefix("$ ") {
            steps.push(Step {
                line: line.to_string(),
                args: split_words(args),
                stdout: String::new(),
                stderr: String::new(),
                status: 0,
            });
            continue;
        }
        let step = steps.last_mut().expect("output before a command");
        if let Some(status) = line.strip_prefix("? ") {
            step.status = status.parse().unwrap();
        } else if let Some(err) = line.strip_prefix("! ") {
            step.stderr.push_str(err);
            step.stderr.push('\n');
        } else {
            step.stdout.push_str(line);
            step.stdout.push('\n');
        }
    }
    steps
}

/// Runs a golden's commands on a fresh server, comparing each with tmux.
fn replay(name: &str) -> TestServer {
    let srv = TestServer::start();
    for step in load(name) {
        let args: Vec<&str> = step.args.iter().map(String::as_str).collect();
        let deadline = Instant::now() + CMD_TIMEOUT;
        loop {
            let out = srv.run(&args);
            // The recorder ends unterminated output with a newline.
            let mut stdout = out.stdout;
            if !stdout.is_empty() && !stdout.ends_with('\n') {
                stdout.push('\n');
            }
            let got = (stdout, out.stderr, out.status.code().unwrap_or(-1));
            let want = (step.stdout.clone(), step.stderr.clone(), step.status);
            if got == want {
                break;
            }
            if args[0] != "capture-pane" || Instant::now() > deadline {
                assert_eq!(got, want, "{name}: {}", step.line);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    srv
}

/// Case 1: three `set-buffer`s listed as tmux lists them.
#[test]
fn list_buffers_after_three_set_buffers_equals_the_golden() {
    replay("list-buffers");
}

/// Case 6, with tmux's naming and error text around it.
#[test]
fn set_buffer_append_rename_and_errors_equal_the_golden() {
    replay("append");
}

/// A control character in a name given with `-b` or `-n` is refused with
/// tmux's text.
#[test]
fn buffer_names_with_control_characters_equal_the_golden() {
    replay("names");
}

/// Case 5: the 51st automatic buffer evicts the oldest.
#[test]
fn the_51st_automatic_buffer_evicts_the_oldest() {
    replay("limit");
}

/// Cases 2, 3 and 4: `-p` brackets only for a pane in mode 2004, the
/// separators, sanitizing and `-d`.
#[test]
fn paste_buffer_into_cat_v_equals_the_golden() {
    replay("paste");
}

/// The bytes a pane reads, exactly: `-p` markers for a pane in mode 2004,
/// none without it, and an escape in the data sanitized so it cannot end
/// the paste.
#[test]
fn paste_bytes_reach_the_pane_exactly() {
    let srv = TestServer::start();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let script = format!(
        "printf '\\033[?2004h'; stty raw -echo; exec cat > '{}'",
        out.display()
    );
    srv.cmd(&["new-session", "-d", "-s", "main", &script])
        .unwrap();
    // The shell opens the file after `stty`, just before `cat` reads.
    wait_until(|| out.exists(), CMD_TIMEOUT).unwrap();
    srv.cmd(&["set-buffer", "-b", "x", "a\x1b[201~b\nc\x01"])
        .unwrap();
    srv.cmd(&["paste-buffer", "-p", "-b", "x"]).unwrap();
    let want: &[u8] = b"\x1b[200~a^[[201~b\rc^A\x1b[201~";
    wait_until(|| fs::read(&out).is_ok_and(|b| b == want), CMD_TIMEOUT)
        .unwrap_or_else(|_| panic!("pane read {:?}", fs::read(&out)));
}

/// Hostile sizes are refused, and nothing is stored.
#[test]
fn oversized_buffers_are_refused() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "main", "cat"])
        .unwrap();
    // Over the socket: an argv this long is past the OS's exec limit. A
    // buffer is larger than one request may be, so it is reached by
    // appending thirds.
    let request = |args: &[&str]| {
        let argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        srv.request(&argv, CMD_TIMEOUT)
    };
    let third = "x".repeat(grofe::server::buffers::MAX_BUFFER_BYTES / 3 + 1);
    request(&["set-buffer", "-b", "a", &third]).unwrap();
    request(&["set-buffer", "-a", "-b", "a", &third]).unwrap();
    let err = request(&["set-buffer", "-a", "-b", "a", &third]).unwrap_err();
    assert!(err.contains("buffer too large"), "{err}");
    assert_eq!(
        srv.cmd(&["list-buffers", "-F", "#{buffer_name} #{buffer_size}"]),
        Ok(format!("a {}\n", 2 * third.len()))
    );
    assert!(srv.cmd(&["set", "-g", "buffer-limit", "0"]).is_err());
}

/// The stock `prefix ]`, `#` and `-` bindings run the buffer commands, and
/// `#{buffer_*}` in any format is the newest automatic buffer's.
#[test]
fn stock_keys_and_global_formats() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "main", "cat"])
        .unwrap();
    let keys = srv.cmd(&["list-keys", "-T", "prefix"]).unwrap();
    for (key, cmd) in [
        ("]", "paste-buffer -p"),
        ("\\#", "list-buffers"),
        ("-", "delete-buffer"),
    ] {
        assert!(
            keys.contains(&format!("-T prefix {key} {cmd}  #")),
            "{key}: {keys}"
        );
    }
    assert_eq!(
        srv.cmd(&["display-message", "-p", "[#{buffer_name}]"]),
        Ok("[]\n".to_string())
    );
    srv.cmd(&["set-buffer", "abc"]).unwrap();
    srv.cmd(&["set-buffer", "-b", "named", "zz"]).unwrap();
    assert_eq!(
        srv.cmd(&[
            "display-message",
            "-p",
            "#{buffer_name} #{buffer_size} #{buffer_sample}"
        ]),
        Ok("buffer0 3 abc\n".to_string())
    );
}

/// The verbs parse natively with tmux's flags.
#[test]
fn buffer_verbs_parse_with_their_flags() {
    use grofe::cmd::{self, Command, PasteBuffer, SetBuffer, Target};
    let argv = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        cmd::parse(&argv(&["set-buffer", "-a", "-b", "x", "-n", "y", "data"])),
        Ok(Command::SetBuffer(SetBuffer {
            append: true,
            name: Some("x".into()),
            rename: Some("y".into()),
            data: Some("data".into()),
        }))
    );
    assert_eq!(
        cmd::parse(&argv(&["paste-buffer", "-dpr", "-b", "x", "-s", "XX"])),
        Ok(Command::PasteBuffer(PasteBuffer {
            bracket: true,
            name: Some("x".into()),
            delete: true,
            raw: true,
            separator: Some("XX".into()),
            unsanitized: false,
            target: Target::Current,
        }))
    );
    assert!(cmd::parse(&argv(&["set-buffer", "a", "b"])).is_err());
    assert!(matches!(
        cmd::parse(&argv(&["paste-buffer", "-S"])),
        Ok(Command::PasteBuffer(c)) if c.unsanitized
    ));
    assert!(matches!(
        cmd::parse(&argv(&["choose-buffer", "-Z"])),
        Ok(Command::ChooseBuffer(c)) if c.zoom
    ));
}

/// A server whose `main` pane shows what it reads (`cat -v`, raw), with
/// buffers `a`, `b`, `c` (newest last) and a client attached at 80x24.
fn chooser_server() -> (TestServer, Harness) {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "23",
        "stty raw -echo; exec cat -v",
    ])
    .unwrap();
    for (name, data) in [("a", "AAA"), ("b", "BBB"), ("c", "CCC")] {
        srv.cmd(&["set-buffer", "-b", name, data]).unwrap();
    }
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    a.wait_for("[main]");
    (srv, a)
}

fn pane_text(srv: &TestServer) -> String {
    srv.cmd(&["capture-pane", "-p", "-t", "main"])
        .unwrap_or_default()
}

/// Case 7, as recorded in a pty against tmux 3.7b: the list is newest
/// first, each line `name: time: sample`, titled `(sort: creation)` with
/// the buffer previewed; Enter pastes the chosen buffer (the second line,
/// `b`) into the pane and closes the chooser.
#[test]
fn choose_buffer_then_enter_pastes_the_chosen_buffer() {
    let (srv, mut a) = chooser_server();
    srv.cmd(&["choose-buffer"]).unwrap();
    a.wait_for("(2) a: ");
    let text = a.text();
    assert!(text.contains(": CCC"), "{text}");
    assert!(text.contains("(1) b: "), "{text}");
    assert!(text.contains("┌ c (sort: creation) "), "{text}");
    a.send("\x1b[B");
    a.wait_for("┌ b (sort: creation) ");
    a.send("\r");
    wait_until(|| pane_text(&srv).starts_with("BBB\n"), CMD_TIMEOUT)
        .unwrap_or_else(|_| panic!("pane: {:?}", pane_text(&srv)));
    a.wait_until("the chooser closed", CMD_TIMEOUT, |t| {
        !t.contains("(sort: ")
    });
    assert_eq!(
        srv.cmd(&["list-buffers", "-F", "#{buffer_name}"]),
        Ok("c\nb\na\n".to_string())
    );
}

/// `d` deletes the current buffer and the chooser stays on the next one;
/// the stock `prefix =` opens it zoomed (`choose-buffer -Z`); no buffers,
/// no chooser.
#[test]
fn choose_buffer_d_deletes_and_prefix_equals_opens_it_zoomed() {
    let (srv, mut a) = chooser_server();
    srv.cmd(&["split-window", "-d", "-t", "main", "cat"])
        .unwrap();
    a.send("\x02=");
    a.wait_for("┌ c (sort: creation) ");
    let zoomed = || {
        srv.cmd(&[
            "display-message",
            "-p",
            "-t",
            "main",
            "#{window_zoomed_flag}",
        ])
    };
    assert_eq!(zoomed(), Ok("1\n".to_string()));
    a.send("\x1b[B");
    a.wait_for("┌ b (sort: creation) ");
    a.send("d");
    a.wait_for("┌ a (sort: creation) ");
    assert_eq!(
        srv.cmd(&["list-buffers", "-F", "#{buffer_name}"]),
        Ok("c\na\n".to_string())
    );
    a.send("q");
    a.wait_until("the chooser closed", CMD_TIMEOUT, |t| {
        !t.contains("(sort: ")
    });
    assert_eq!(zoomed(), Ok("0\n".to_string()));
    for name in ["a", "c"] {
        srv.cmd(&["delete-buffer", "-b", name]).unwrap();
    }
    srv.cmd(&["choose-buffer"]).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(!a.text().contains("(sort: "));
}

/// `-S` (tmux 3.7b, recorded): the data goes as it is, control characters
/// and all; the separator still replaces line feeds.
#[test]
fn paste_buffer_s_sends_unsanitized_bytes() {
    let srv = TestServer::start();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let script = format!("stty raw -echo; exec cat > '{}'", out.display());
    srv.cmd(&["new-session", "-d", "-s", "main", &script])
        .unwrap();
    wait_until(|| out.exists(), CMD_TIMEOUT).unwrap();
    srv.cmd(&["set-buffer", "-b", "x", "a\x1b[31mb\nc\x01"])
        .unwrap();
    srv.cmd(&["paste-buffer", "-S", "-b", "x"]).unwrap();
    let want: &[u8] = b"a\x1b[31mb\rc\x01";
    wait_until(|| fs::read(&out).is_ok_and(|b| b == want), CMD_TIMEOUT)
        .unwrap_or_else(|_| panic!("pane read {:?}", fs::read(&out)));
}
