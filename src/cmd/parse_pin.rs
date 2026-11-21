//! Pins the parse result of every verb's bad-flag, missing-argument and
//! too-many-arguments cases to `tests/fixtures/parse_pin.txt`, so the
//! command table (WS10) can replace the per-verb parsers without changing
//! an error's wording or the order checks run in. Regenerate with
//! `PARSE_PIN_REGEN=1 cargo test --lib parse_pin`.

use super::parse::parse;

const FIXTURE: &str = "tests/fixtures/parse_pin.txt";

const VERBS: &[&str] = &[
    "__config-file",
    "__register-source",
    "agent",
    "agent-grid",
    "agent-sidebar",
    "attach-session",
    "bind-key",
    "break-pane",
    "capture-pane",
    "choose-agent",
    "choose-buffer",
    "choose-client",
    "choose-notification",
    "choose-scratchpad",
    "choose-tree",
    "clear-history",
    "clear-prompt-history",
    "clock-mode",
    "command-prompt",
    "confirm-before",
    "copy-mode",
    "customize-mode",
    "delete-buffer",
    "detach-client",
    "display-menu",
    "display-message",
    "display-panes",
    "display-popup",
    "drag-pane",
    "find-window",
    "float-pane",
    "has-session",
    "if-shell",
    "join-pane",
    "key-menu",
    "kill-pane",
    "kill-server",
    "kill-session",
    "kill-window",
    "last-pane",
    "last-window",
    "link-window",
    "list-buffers",
    "list-clients",
    "list-keys",
    "list-panes",
    "list-plugins",
    "list-ports",
    "list-scratchpads",
    "list-sessions",
    "list-windows",
    "load-buffer",
    "lock-client",
    "lock-server",
    "lock-session",
    "lua",
    "lua-call",
    "move-pane",
    "move-window",
    "new-pane",
    "new-scratchpad",
    "new-session",
    "new-window",
    "next-layout",
    "next-window",
    "notify",
    "open-url",
    "pane-set-resume",
    "paste-buffer",
    "pipe-pane",
    "place-pane",
    "plugin-restart",
    "previous-layout",
    "previous-window",
    "reboot-check",
    "refresh-client",
    "reload",
    "rename-session",
    "rename-window",
    "resize-pane",
    "resize-window",
    "respawn-pane",
    "respawn-window",
    "rotate-window",
    "run-shell",
    "save-buffer",
    "select-layout",
    "select-pane",
    "select-window",
    "send-keys",
    "settings",
    "send-prefix",
    "server-access",
    "set-buffer",
    "set-client-info",
    "set-environment",
    "set-hook",
    "set-option",
    "show-buffer",
    "show-environment",
    "show-hooks",
    "show-messages",
    "show-options",
    "show-prompt-history",
    "show-scratchpad",
    "source-file",
    "split-window",
    "subscribe",
    "suspend-client",
    "swap-pane",
    "swap-window",
    "switch-client",
    "toggle-scratchpad",
    "unbind-key",
    "unfloat-pane",
    "unlink-window",
    "wait-for",
    // aliases
    "set",
    "show",
    "new",
    "neww",
    "splitw",
    "killp",
    "killw",
    "selectp",
    "selectw",
    "resizep",
    "respawnp",
    "lsp",
    "lsw",
    "send",
    "display",
    "capturep",
    "clearhist",
    "attach",
    "bind",
    "unbind",
    "lsk",
    "source",
    "run",
    "selectl",
    "prevl",
    "nextl",
    "lock",
    "locks",
    "lockc",
    "showphist",
    "clearphist",
];

fn fnv(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

fn run(words: &[&str]) -> String {
    let argv: Vec<String> = words.iter().map(|w| (*w).to_string()).collect();
    match parse(&argv) {
        Ok(cmd) => format!("ok {:08x}", fnv(&format!("{cmd:?}"))),
        Err(e) => format!("err {e}"),
    }
}

fn generate() -> String {
    let many = vec!["w"; 30];
    let mut out = String::new();
    for verb in VERBS {
        let mut case = |label: &str, rest: &[&str]| {
            let mut words = vec![*verb];
            words.extend_from_slice(rest);
            let result = run(&words);
            out.push_str(&format!("{verb} {label} => {result}\n"));
        };
        case("none", &[]);
        case("one", &["x"]);
        case("two", &["x", "y"]);
        case("many", &many);
        case("long", &["--zz"]);
        case("dashdash", &["--", "x"]);
        case("target", &["-t", "%1"]);
        case("target-extra", &["-t", "%1", "x"]);
        case("target-glued", &["-t0"]);
        case("target-num", &["-t", "0"]);
        case("combined-bool", &["-dP"]);
        case("combined-bool-rev", &["-Pd"]);
        case("combined-value", &["-dt", "%1"]);
        case("combined-glued", &["-dtx"]);
        case("repeated-bool", &["-d", "-d"]);
        case("repeated-value", &["-t", "%1", "-t", "%2"]);
        case("flag-after-positional", &["x", "-d"]);
        case("value-after-positional", &["x", "-t", "%1"]);
        case("dashdash-flag", &["--", "-d"]);
        case("flag-dashdash-positional", &["-d", "--", "x", "y"]);
        // One line per verb for the 52 single-letter flags, bare and with a
        // value; an unknown flag is shortened to `U`.
        let mut letters = String::new();
        for c in ('a'..='z').chain('A'..='Z') {
            let flag = format!("-{c}");
            for rest in [vec![flag.as_str()], vec![flag.as_str(), "%1"]] {
                let mut words = vec![*verb];
                words.extend(rest);
                let r = run(&words);
                let unknown = format!("err command {verb}: unknown flag -{c}");
                letters.push_str(&format!("{c}={} ", if r == unknown { "U" } else { &r }));
            }
        }
        out.push_str(&format!("{verb} letters => {letters}\n"));
    }
    out
}

#[test]
fn bad_flag_missing_arg_and_too_many_are_pinned() {
    let got = generate();
    if std::env::var_os("PARSE_PIN_REGEN").is_some() {
        std::fs::write(FIXTURE, &got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(FIXTURE).unwrap();
    for (g, w) in got.lines().zip(want.lines()) {
        assert_eq!(g, w);
    }
    assert_eq!(got.lines().count(), want.lines().count());
}
