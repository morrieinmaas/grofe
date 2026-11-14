//! `grofe agent setup <kind>`: installs or removes grofe's agent hooks in an agent's settings.

use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// (event, matcher, state arguments) for each Claude hook grofe installs.
const CLAUDE_HOOKS: &[(&str, Option<&str>, &str)] = &[
    ("SessionStart", None, "idle"),
    ("UserPromptSubmit", None, "working"),
    ("PreToolUse", None, "working"),
    ("PostToolUse", None, "working"),
    (
        "Notification",
        Some("permission_prompt"),
        "blocked --blocked permission",
    ),
    ("Notification", Some("idle_prompt"), "done"),
    ("Stop", None, "done"),
    ("SessionEnd", None, "exited"),
];

const KINDS: &[&str] = &["claude", "codex", "opencode", "pi", "gemini"];

fn prefix(grofe: &str) -> String {
    format!("{grofe} agent report")
}

fn command(grofe: &str, args: &str) -> String {
    format!("{} {args} --hook claude", prefix(grofe))
}

fn hooks_of(group: &Value) -> &[Value] {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn is_ours(hook: &Value, grofe: &str) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.starts_with(&prefix(grofe)))
}

/// Adds grofe's hooks to Claude settings, leaving everything else and the key order alone.
pub fn merge_claude(mut settings: Value, grofe: &str) -> Value {
    if !settings.is_object() {
        settings = json!({});
    }
    let Some(root) = settings.as_object_mut() else {
        return settings;
    };
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let Some(hooks) = hooks.as_object_mut() else {
        return settings;
    };
    for (event, matcher, args) in CLAUDE_HOOKS {
        let cmd = command(grofe, args);
        let groups = hooks.entry(*event).or_insert_with(|| json!([]));
        if !groups.is_array() {
            *groups = json!([]);
        }
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        let present = groups
            .iter()
            .any(|g| hooks_of(g).iter().any(|h| h["command"] == cmd.as_str()));
        if present {
            continue;
        }
        let mut group = json!({});
        if let Some(m) = matcher {
            group["matcher"] = json!(m);
        }
        group["hooks"] = json!([{ "type": "command", "command": cmd }]);
        groups.push(group);
    }
    settings
}

/// Removes exactly the hooks whose command starts with `<grofe> agent report`.
pub fn remove_claude(mut settings: Value, grofe: &str) -> Value {
    let Some(root) = settings.as_object_mut() else {
        return settings;
    };
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return settings;
    };
    let mut emptied = Vec::new();
    for (event, groups) in hooks.iter_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        let before = groups.len();
        groups.retain_mut(|g| {
            let Some(list) = g.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let n = list.len();
            list.retain(|h| !is_ours(h, grofe));
            list.len() == n || !list.is_empty()
        });
        if groups.is_empty() && before > 0 {
            emptied.push(event.clone());
        }
    }
    let touched = !emptied.is_empty();
    for event in emptied {
        hooks.shift_remove(&event);
    }
    if touched && hooks.is_empty() {
        root.shift_remove("hooks");
    }
    settings
}

/// Every grofe command in the settings, as "event: command".
fn ours(settings: &Value, grofe: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    if let Some(hooks) = settings.get("hooks").and_then(Value::as_object) {
        for (event, groups) in hooks {
            for h in groups.as_array().into_iter().flatten().flat_map(hooks_of) {
                if is_ours(h, grofe) {
                    found.insert(format!("{event}: {}", h["command"].as_str().unwrap_or("")));
                }
            }
        }
    }
    found
}

fn env_settings_path() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Some(Path::new(&d).join("settings.json"));
    }
    std::env::var_os("HOME").map(|h| Path::new(&h).join(".claude/settings.json"))
}

fn read_settings(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(json!({})),
        Ok(s) => serde_json::from_str(&s).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[derive(Default)]
struct Opts {
    dry_run: bool,
    remove: bool,
    yes: bool,
}

/// Applies or removes the claude hooks in `path`, asking on `input` unless `opts.yes`.
fn setup_claude(
    path: &Path,
    grofe: &str,
    opts: &Opts,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    // Resolve symlinks so the edit lands in the real file and the link survives.
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let before = read_settings(&target)?;
    let after = if opts.remove {
        remove_claude(before.clone(), grofe)
    } else {
        merge_claude(before.clone(), grofe)
    };
    if after == before {
        let msg = if opts.remove {
            "nothing to remove"
        } else {
            "already installed"
        };
        writeln!(out, "{}: {msg}", target.display()).map_err(io)?;
        return Ok(());
    }
    let (old, new) = (ours(&before, grofe), ours(&after, grofe));
    writeln!(out, "{}", target.display()).map_err(io)?;
    for line in new.difference(&old) {
        writeln!(out, "+ {line}").map_err(io)?;
    }
    for line in old.difference(&new) {
        writeln!(out, "- {line}").map_err(io)?;
    }
    if opts.dry_run {
        return Ok(());
    }
    if !opts.yes {
        write!(out, "Apply? [y/N] ").map_err(io)?;
        out.flush().map_err(io)?;
        let mut answer = String::new();
        input.read_line(&mut answer).map_err(io)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return Err("aborted".into());
        }
    }
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let mut bak = target.clone().into_os_string();
    bak.push(".grofe-bak");
    if target.exists() && !Path::new(&bak).exists() {
        std::fs::copy(&target, &bak).map_err(io)?;
    }
    let mut text = serde_json::to_string_pretty(&after).map_err(|e| e.to_string())?;
    text.push('\n');
    std::fs::write(&target, text).map_err(io)?;
    writeln!(out, "done").map_err(io)
}

fn status(path: Option<&Path>, grofe: &str, on_path: bool) -> String {
    let mut s = String::new();
    for kind in KINDS {
        let state = if *kind != "claude" {
            "not supported yet"
        } else {
            let settings = path.and_then(|p| read_settings(p).ok());
            let found = path.is_some_and(|p| p.exists()) || on_path;
            match settings {
                Some(v) if ours(&v, grofe).len() == CLAUDE_HOOKS.len() => "installed",
                _ if found => "not installed",
                _ => "agent not found",
            }
        };
        s.push_str(&format!("{kind}: {state}\n"));
    }
    s
}

fn claude_on_path() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("claude").is_file()))
}

pub fn run(args: &[String]) -> ExitCode {
    let grofe = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            eprintln!("grofe: cannot find own path: {e}");
            return ExitCode::FAILURE;
        }
    };
    let usage = "grofe: usage: agent setup <kind> [--dry-run] [--remove] [--yes] | --status";
    let mut opts = Opts::default();
    let mut kind = None;
    let mut want_status = false;
    for a in args {
        match a.as_str() {
            "--dry-run" => opts.dry_run = true,
            "--remove" => opts.remove = true,
            "--yes" | "-y" => opts.yes = true,
            "--status" => want_status = true,
            f if f.starts_with('-') => {
                eprintln!("grofe: unknown flag {f}");
                return ExitCode::FAILURE;
            }
            k if kind.is_none() => kind = Some(k.to_string()),
            _ => {
                eprintln!("{usage}");
                return ExitCode::FAILURE;
            }
        }
    }
    let path = env_settings_path();
    if want_status {
        print!("{}", status(path.as_deref(), &grofe, claude_on_path()));
        return ExitCode::SUCCESS;
    }
    match kind.as_deref() {
        Some("claude") => {}
        Some(k) if KINDS.contains(&k) => {
            eprintln!("grofe: agent setup {k}: not supported yet");
            return ExitCode::FAILURE;
        }
        _ => {
            eprintln!("{usage}");
            return ExitCode::FAILURE;
        }
    }
    let Some(path) = path else {
        eprintln!("grofe: no HOME or CLAUDE_CONFIG_DIR");
        return ExitCode::FAILURE;
    };
    let stdin = std::io::stdin();
    match setup_claude(
        &path,
        &grofe,
        &opts,
        &mut stdin.lock(),
        &mut std::io::stdout(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("grofe: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: &str = "/b/grofe";

    fn go(path: &Path, opts: &Opts) -> String {
        let mut out = Vec::new();
        setup_claude(path, G, opts, &mut std::io::empty(), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn yes() -> Opts {
        Opts {
            yes: true,
            ..Opts::default()
        }
    }

    #[test]
    fn example_stop_command() {
        let v = merge_claude(json!({}), G);
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/b/grofe agent report done --hook claude"
        );
        assert_eq!(
            v["hooks"]["Notification"][0]["matcher"],
            "permission_prompt"
        );
    }

    #[test]
    fn keeps_key_order() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, r#"{"zeta":1,"alpha":{"b":1,"a":2},"model":"x"}"#).unwrap();
        go(&p, &yes());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["zeta", "alpha", "model", "hooks"]);
        let inner: Vec<_> = v["alpha"].as_object().unwrap().keys().cloned().collect();
        assert_eq!(inner, ["b", "a"]);
    }

    #[test]
    fn symlink_stays_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.json");
        let link = dir.path().join("settings.json");
        std::fs::write(&real, "{}").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        go(&link, &yes());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            std::fs::read_to_string(&real)
                .unwrap()
                .contains("agent report")
        );
    }

    #[test]
    fn backup_once() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, r#"{"a":1}"#).unwrap();
        go(&p, &yes());
        let bak = dir.path().join("settings.json.grofe-bak");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), r#"{"a":1}"#);
        go(
            &p,
            &Opts {
                remove: true,
                yes: true,
                ..Opts::default()
            },
        );
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), r#"{"a":1}"#);
    }

    #[test]
    fn idempotent() {
        let once = merge_claude(json!({"a": 1}), G);
        assert_eq!(merge_claude(once.clone(), G), once);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        go(&p, &yes());
        let first = std::fs::read_to_string(&p).unwrap();
        assert!(go(&p, &yes()).contains("already installed"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), first);
    }

    #[test]
    fn remove_exact() {
        let foreign = json!({"matcher": "x", "hooks": [{"type": "command", "command": "/other agent report done"}]});
        let start = json!({"theme": "dark", "hooks": {"Stop": [foreign]}});
        let merged = merge_claude(start.clone(), G);
        assert_eq!(remove_claude(merged, G), start);
        // a foreign hook sharing a group with ours survives
        let mut shared = merge_claude(json!({}), G);
        shared["hooks"]["Stop"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "command", "command": "echo hi"}));
        let left = remove_claude(shared, G);
        assert_eq!(left["hooks"]["Stop"][0]["hooks"][0]["command"], "echo hi");
        assert!(left["hooks"].get("SessionEnd").is_none());
        assert_eq!(remove_claude(json!({"a": 1}), G), json!({"a": 1}));
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let out = go(
            &p,
            &Opts {
                dry_run: true,
                ..Opts::default()
            },
        );
        assert!(out.contains("+ Stop: /b/grofe agent report done --hook claude"));
        assert!(!p.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn prompt_declined_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let mut out = Vec::new();
        let r = setup_claude(&p, G, &Opts::default(), &mut &b"n\n"[..], &mut out);
        assert!(r.is_err());
        assert!(!p.exists());
    }

    #[test]
    fn status_lists_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let s = status(Some(&p), G, false);
        assert!(s.contains("claude: agent not found"));
        assert!(s.contains("codex: not supported yet"));
        go(&p, &yes());
        assert!(status(Some(&p), G, false).contains("claude: installed"));
        std::fs::write(&p, "{}").unwrap();
        assert!(status(Some(&p), G, false).contains("claude: not installed"));
    }
}
