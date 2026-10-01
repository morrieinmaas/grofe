//! TC.13 case 6: every line of the six corpus files through the front end,
//! on a live grofe run as `tmux` with the port loaded, and each line's
//! reply read. tmux's own replies to these lines are not recorded (they
//! hit targets and files that exist only on the machine they came from),
//! so this asserts structure: no reply says `unknown command` or `not
//! implemented`, and `compat-miss.log` holds nothing but the
//! [`MISS_ALLOWED`] lines.
//!
//! Placeholders are made concrete as FE.1's replay makes them (`<...>` is
//! `1`, `%N` is `%1`, `@N` is `@1`), against a session `1` holding panes
//! `%0` and `%1` and windows `@0` and `@1`, rebuilt whenever a line killed
//! it. A CLI line is sent as its argv, without the corpus's own `-L`, `-S`
//! or `-f` (they name other servers); a config line (`static-conf`,
//! `container-stored`) is split as the config language splits it and each
//! command is sent as one request, `\;` kept literal, since grofe's
//! `source-file` reads grofe's TOML and not a tmux config. A request still
//! running after [`BLOCK`] was accepted and is waiting (a popup, a menu,
//! `wait-for`, an attach), and is killed.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use grofe::compat::tmux;

use super::{Checkouts, tc13_cli};
use crate::normalize::fixtures_dir;
use crate::replay::{Cli, split_words};

/// How long a request may take before it counts as accepted and waiting.
const BLOCK: Duration = Duration::from_secs(2);

/// Corpus lines that cannot be sent as they stand, each with why.
const SKIPPED: &[(&str, &str)] = &[
    (
        "<$window_filter>",
        "stands for `-a` or `-t <session>`, chosen at run time (FE.1's artifact list)",
    ),
    (
        "new-session -A -s",
        "a shell alias; the caller appends the name (FE.1's artifact list)",
    ),
    (
        "<$tmux_cmd>",
        "stands for a whole command (tmux-yank builds it), so `1` would be a verb",
    ),
    (
        "<$note_flag>",
        "stands for `-N <note>` or nothing, so `1` would be the key and the key the verb",
    ),
    (
        "<$join_flag>",
        "stands for `-h` or `-v`, so `1` would end the flags early",
    ),
    (
        "<$@>",
        "stands for every item of a which-key menu, so `1` alone is too few",
    ),
    (
        "set-hook -g <",
        "the hook name is a placeholder, and `1` is no hook (FE.3's hook-forms golden has real ones)",
    ),
    (
        "set-hook -g -u <",
        "the hook name is a placeholder, and `1` is no hook (FE.3's hook-forms golden has real ones)",
    ),
    (
        "bind-key -T prefix Space show-wk-menu-root",
        "names which-key's alias, whose command the corpus has only as a placeholder \
         (`set -g -F @wk_cmd_show <value>`), so it expands to `1`; FE.4's aliases golden \
         binds through a real one",
    ),
    (
        "%if",
        "a tmux config-file directive, not a command: it reaches grofe only as \
         import-tmux input (TP.0), never through the front end",
    ),
    ("%else", "a tmux config-file directive (see %if)"),
    ("%endif", "a tmux config-file directive (see %if)"),
];

/// `compat-miss.log` lines the corpus is expected to leave (the time
/// stamp dropped), each with the row that marks it: a cosmetic flag the
/// front end ignores, or a form its row refuses on purpose.
pub const MISS_ALLOWED: &[(&str, &str)] = &[
    // Copy mode and paste buffers are M5's (TM.1 to TM.7; M36-R8).
    ("choose-buffer -Z", M5),
    ("copy-mode", M5),
    ("copy-mode -u", M5),
    ("delete-buffer -b", M5),
    ("list-buffers", M5),
    ("load-buffer", M5),
    ("paste-buffer -b", M5),
    ("paste-buffer -p", M5),
    ("send-keys -F -X", M5),
    ("send-keys -X", M5),
    ("send-keys -X -N", M5),
    ("set-buffer", M5),
    ("set-buffer -b", M5),
    ("show-buffer -b", M5),
    // tmux's stock mouse bindings, which grofe's built-in mouse model
    // replaces (spec 4.5 out of scope, "Mouse argv forms"; M36-R7).
    ("display-menu -O -T -t -x -y", MOUSE),
    ("display-menu -T -t -x -y", MOUSE),
    ("display-menu -T -x -y", MOUSE),
    ("resize-pane -M: cosmetic, ignored", MOUSE),
    ("select-pane -M", MOUSE),
    ("send-keys -M", MOUSE),
    // Spec 4.5 out of scope: marks, linked windows and session groups
    // (M36-R9).
    ("select-pane -m", "marked panes (spec 4.5 out of scope)"),
    ("link-window -a -s -t", GROUPS),
    ("unlink-window -k -t", GROUPS),
    ("new-session -d -s -t", GROUPS),
    // Spec 4.5 out of scope until a ported script needs it: sessionx's
    // preview sends it, and M3.7 runs sessionx unchanged, so M3.7 needs it.
    (
        "format modifier T",
        "`#{T:}` (spec 4.5 out of scope until needed; tmux-sessionx's preview.sh)",
    ),
];

const M5: &str = "copy mode and buffers, M5 (M36-R8)";
const MOUSE: &str = "tmux's stock mouse bindings (spec 4.5 out of scope, M36-R7)";
const GROUPS: &str = "linked windows and session groups (spec 4.5 out of scope, M36-R9)";

/// Corpus lines that still fail the replay, each with the task whose claim
/// it contradicts. A listed line must still fail, so a fix shows up.
const KNOWN_FAILING: &[(&str, &str)] = &[];

/// Words in a reply that mean the front end does not know a form.
const FORBIDDEN: &[&str] = &["unknown command", "not implemented"];

/// `line` with its placeholders made concrete, as FE.1's replay does.
fn concrete(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(start) = rest.find('<') {
        let end = rest[start..].find('>').map(|e| start + e);
        match end {
            Some(end) if !rest[start + 1..end].contains([' ', '<']) && end > start + 1 => {
                out.push_str(&rest[..start]);
                out.push('1');
                rest = &rest[end + 1..];
            }
            _ => {
                out.push_str(&rest[..=start]);
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out.replace("%N", "%1").replace("@N", "@1")
}

/// A config command as CLI argv: a `;` word stays one argument, as `\;`.
fn as_cli(cmd: Vec<String>) -> Vec<String> {
    cmd.into_iter()
        .map(|w| match w.strip_suffix(';') {
            Some(stem) => format!("{stem}\\;"),
            None => w,
        })
        .collect()
}

/// The requests one corpus line sends, or why it sends none.
fn requests(line: &str, conf: bool) -> Result<Vec<Vec<String>>, String> {
    let line = concrete(line);
    if conf {
        // `list-keys` prints a `{` or `}` key bare, which the config
        // language reads as a block; such a line goes as the argv it is.
        if let Ok(cmds) = grofe::cmd::split_command_string(&line) {
            return Ok(cmds.into_iter().map(as_cli).collect());
        }
        return Ok(vec![split_words(&line)?]);
    }
    let words = split_words(&line)?;
    let cli = tmux::cli(&words)?;
    if cli.version {
        return Ok(vec![vec!["-V".into()]]);
    }
    // `-c` runs its command with the default shell, no server request: it
    // goes as written, with [`SHELL_RAN`] after it to prove a shell ran.
    if let Some(command) = cli.shell_command {
        return Ok(vec![vec![
            "-c".into(),
            format!("{command}\necho {SHELL_RAN}$?"),
        ]]);
    }
    Ok(vec![cli.argv])
}

/// What a `-c` request's shell prints last, with the command's status.
const SHELL_RAN: &str = "tc13-shell-ran-";

/// Runs `argv` through `cli`, killing it once [`BLOCK`] passes: its exit
/// code (`None` when it was still waiting) and what it printed.
fn run_bounded(cli: &Cli, argv: &[String]) -> (Option<i32>, String) {
    let mut child: Child = cli
        .command(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawning {argv:?}: {e}"));
    // Read both pipes as the command runs, so a long reply cannot stall it.
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|mut pipe| {
        thread::spawn(move || {
            let mut text = String::new();
            let _ = pipe.read_to_string(&mut text);
            text
        })
    })
    .collect();
    let deadline = Instant::now() + BLOCK;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().or(Some(-1)),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let text = readers
        .into_iter()
        .map(|r| r.join().unwrap_or_default())
        .collect();
    (code, text)
}

fn words(w: &[&str]) -> Vec<String> {
    w.iter().map(|s| s.to_string()).collect()
}

/// Session `1` with panes `%0`/`%1` and windows `@0`/`@1`, unless it is
/// there already.
fn ensure_target(cli: &Cli) {
    if cli.run(&words(&["has-session", "-t", "=1"])).0.success() {
        return;
    }
    let _ = cli.run(&words(&["kill-server"]));
    for argv in [
        &[
            "new-session",
            "-d",
            "-s",
            "1",
            "-n",
            "1",
            "-x",
            "200",
            "-y",
            "50",
            "cat",
        ][..],
        &["split-window", "-d", "-t", "1:1", "cat"][..],
        &["new-window", "-d", "-t", "1:", "-n", "2", "cat"][..],
    ] {
        let (status, _, err) = cli.run(&words(argv));
        assert!(status.success(), "{argv:?}: {err}");
    }
}

/// The miss log's lines past byte `from`, time stamps dropped, and the
/// log's length.
fn miss_lines(log: &Path, from: usize) -> (Vec<String>, usize) {
    let text = fs::read_to_string(log).unwrap_or_default();
    let lines = text
        .get(from..)
        .unwrap_or_default()
        .lines()
        .map(|l| l.split_once(' ').map_or(l, |(_, what)| what).to_string())
        .collect();
    (lines, text.len())
}

/// Case 6. Returns every failure.
pub fn run(co: &Checkouts) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let cli = tc13_cli(dir.path(), co);
    let log = dir.path().join("state/grofe/compat-miss.log");
    let corpus = fixtures_dir().join("../../compat/corpus");
    let mut failures = Vec::new();
    let mut failing: BTreeMap<&str, String> = BTreeMap::new();
    let mut skipped: BTreeMap<&str, usize> = BTreeMap::new();
    // Each miss-log line with the first corpus line that wrote it.
    let mut logged: BTreeMap<String, String> = BTreeMap::new();
    let mut log_len = 0;
    let (mut sent, mut waited, mut refused) = (0usize, 0usize, 0usize);
    for file in [
        "static-cli",
        "static-conf",
        "container-cli",
        "container-pane",
        "container-job",
        "container-stored",
    ] {
        let conf = matches!(file, "static-conf" | "container-stored");
        let text = fs::read_to_string(corpus.join(format!("{file}.argv"))).unwrap();
        for (n, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            if let Some((what, _)) = SKIPPED.iter().find(|(what, _)| {
                if what.starts_with('%') {
                    line.trim_start().starts_with(what)
                } else {
                    line.contains(what)
                }
            }) {
                *skipped.entry(what).or_default() += 1;
                continue;
            }
            let at = format!("{file}:{}: {line}", n + 1);
            let mut problem = None;
            match requests(line, conf) {
                Err(e) => problem = Some(format!("not sendable: {e}")),
                Ok(reqs) => {
                    for req in reqs {
                        ensure_target(&cli);
                        sent += 1;
                        let (code, reply) = run_bounded(&cli, &req);
                        match code {
                            None => waited += 1,
                            Some(0) => {}
                            Some(_) => refused += 1,
                        }
                        if req.first().is_some_and(|w| w == "-c")
                            && (code != Some(0) || !reply.contains(SHELL_RAN))
                        {
                            problem = Some(format!(
                                "sent {req:?}, no shell ran it ({code:?}): {}",
                                reply.trim()
                            ));
                        }
                        if let Some(bad) = FORBIDDEN.iter().find(|f| reply.contains(*f)) {
                            problem = Some(format!(
                                "sent {req:?}, reply says {bad:?}: {}",
                                reply.trim()
                            ));
                        }
                        let (new, len) = miss_lines(&log, log_len);
                        log_len = len;
                        for what in new {
                            logged.entry(what).or_insert_with(|| at.clone());
                        }
                    }
                }
            }
            if let Some(problem) = problem {
                match KNOWN_FAILING.iter().find(|(l, _)| *l == line) {
                    Some((l, _)) => {
                        failing.insert(l, problem);
                    }
                    None => failures.push(format!("{at}\n  {problem}")),
                }
            }
        }
    }
    let _ = cli.run(&words(&["kill-server"]));
    if let Err(e) = super::tripwire(&cli) {
        failures.push(e);
    }

    for (line, why) in KNOWN_FAILING {
        match failing.get(line) {
            Some(problem) => eprintln!("KNOWN tc_13 corpus failure ({why}): {line}: {problem}"),
            None => failures.push(format!("KNOWN_FAILING entry now passes, remove it: {line}")),
        }
    }
    let allowed: BTreeMap<&str, &str> = MISS_ALLOWED.iter().copied().collect();
    for (what, first) in &logged {
        if !allowed.contains_key(what.as_str()) {
            failures.push(format!(
                "compat-miss.log: {what:?} is not in MISS_ALLOWED (first from {first})"
            ));
        }
    }
    for what in allowed.keys() {
        if !logged.contains_key(*what) {
            failures.push(format!("MISS_ALLOWED: {what:?} was not logged; remove it"));
        }
    }
    eprintln!(
        "tc_13 corpus replay: {sent} requests, {waited} waiting (killed after {BLOCK:?}), \
         {refused} refused with an error, skipped {skipped:?}, {} distinct miss-log lines",
        logged.len()
    );
    failures
}

#[test]
fn placeholders_become_concrete() {
    assert_eq!(
        concrete("kill-pane -t %N \\; select -t @N:<window>"),
        "kill-pane -t %1 \\; select -t @1:1"
    );
    assert_eq!(concrete("a <b c> d"), "a <b c> d");
}

/// tmux-fzf's `-c {}` goes as `-c`, its command and the marker the
/// replay looks for, so the line cannot pass without a shell running.
#[test]
fn a_shell_command_line_goes_with_its_marker() {
    assert_eq!(
        requests("-c {}", false).unwrap(),
        vec![words(&["-c", &format!("{{}}\necho {SHELL_RAN}$?")])]
    );
}

#[test]
fn a_config_semicolon_stays_one_argument() {
    assert_eq!(
        as_cli(words(&["bind", "c", "a", ";", "b"])),
        words(&["bind", "c", "a", "\\;", "b"])
    );
}
