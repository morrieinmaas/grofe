//! TM.4: paste buffers against tmux 3.7b's goldens
//! (`tests/fixtures/compat/golden/tm_4/`). Each golden's commands run
//! natively here, in order, and each one's stdout, stderr and exit status
//! must equal what tmux printed; a `capture-pane` is retried until the
//! pane shows what tmux's did (the paste reaches the pane's program
//! asynchronously).

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{CMD_TIMEOUT, TestServer, wait_until};

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
    let big = "x".repeat(grofe::server::buffers::MAX_BUFFER_BYTES + 1);
    // Over the socket: an argv this long is past the OS's exec limit.
    let request = |args: &[&str]| {
        let argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        srv.request(&argv, CMD_TIMEOUT)
    };
    let err = request(&["set-buffer", &big]).unwrap_err();
    assert!(err.contains("buffer too large"), "{err}");
    let half = "x".repeat(grofe::server::buffers::MAX_BUFFER_BYTES / 2 + 1);
    request(&["set-buffer", "-b", "a", &half]).unwrap();
    let err = request(&["set-buffer", "-a", "-b", "a", &half]).unwrap_err();
    assert!(err.contains("buffer too large"), "{err}");
    assert_eq!(
        srv.cmd(&["list-buffers", "-F", "#{buffer_name} #{buffer_size}"]),
        Ok(format!("a {}\n", half.len()))
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
            target: Target::Current,
        }))
    );
    assert!(cmd::parse(&argv(&["set-buffer", "a", "b"])).is_err());
    assert!(cmd::parse(&argv(&["paste-buffer", "-S"])).is_err());
    assert!(matches!(
        cmd::parse(&argv(&["choose-buffer", "-Z"])),
        Ok(Command::ChooseBuffer(c)) if c.zoom
    ));
}
