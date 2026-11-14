//! `grofe agent setup <kind>`: installs or removes grofe's agent hooks in an agent's settings.

use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// (event, matcher, state arguments) for one hook grofe installs.
type HookDef = (&'static str, Option<&'static str>, &'static str);

/// A JSON-configured agent: where its hooks live and which ones grofe installs.
struct Kind {
    name: &'static str,
    /// The `--hook` value, also the end of the command tail we recognise our entries by.
    hook: &'static str,
    hooks: &'static [HookDef],
    /// Whether the file may be a bare event map instead of `{"hooks": {...}}`.
    bare_ok: bool,
}

const CLAUDE: Kind = Kind {
    name: "claude",
    hook: "claude",
    bare_ok: false,
    hooks: &[
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
    ],
};

const CODEX: Kind = Kind {
    name: "codex",
    hook: "codex",
    bare_ok: true,
    hooks: &[
        ("SessionStart", None, "idle"),
        ("UserPromptSubmit", None, "working"),
        ("PreToolUse", None, "working"),
        ("PostToolUse", None, "working"),
        ("Stop", None, "done"),
    ],
};

const GEMINI: Kind = Kind {
    name: "gemini",
    hook: "gemini",
    bare_ok: false,
    hooks: &[
        ("SessionStart", None, "idle"),
        ("BeforeTool", Some("*"), "working"),
        ("AfterTool", Some("*"), "working"),
        ("AfterAgent", None, "done"),
        ("Notification", None, "blocked --blocked permission"),
    ],
};

const KINDS: &[&str] = &["claude", "codex", "opencode", "pi", "gemini"];

const PLUGIN_MARKER: &str = "// grofe: managed by `grofe agent setup opencode`";

/// Single-quotes `s` for a shell, since the agents run hook commands through one.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn command(kind: &Kind, grofe: &str, args: &str) -> String {
    format!(
        "{} agent report {args} --hook {}",
        shell_quote(grofe),
        kind.hook
    )
}

fn hooks_of(group: &Value) -> &[Value] {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Whether `hook` is a grofe hook for `args`, found by the tail of its command so
/// a hook installed from another path, quoted or not, still counts.
fn is_ours_with(kind: &Kind, hook: &Value, args: &str) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .and_then(|c| c.strip_suffix(&format!(" agent report {args} --hook {}", kind.hook)))
        .is_some_and(|head| !head.trim().is_empty())
}

fn is_ours(kind: &Kind, hook: &Value) -> bool {
    kind.hooks
        .iter()
        .any(|(_, _, a)| is_ours_with(kind, hook, a))
}

/// Whether the file is a bare event map (codex only): no `hooks` key, but event names at the top.
fn is_bare(root: &Map<String, Value>, kind: &Kind) -> bool {
    kind.bare_ok
        && !root.contains_key("hooks")
        && root.keys().any(|k| kind.hooks.iter().any(|(e, ..)| e == k))
}

/// The event map of `root`: the root itself when bare, else `hooks`, made if `create`.
fn event_map<'a>(
    root: &'a mut Map<String, Value>,
    kind: &Kind,
    create: bool,
) -> Option<&'a mut Map<String, Value>> {
    if is_bare(root, kind) {
        return Some(root);
    }
    if create {
        let hooks = root.entry("hooks").or_insert_with(|| json!({}));
        if !hooks.is_object() {
            *hooks = json!({});
        }
    }
    root.get_mut("hooks").and_then(Value::as_object_mut)
}

/// Adds grofe's hooks, leaving everything else and the key order alone.
/// A grofe hook from another path is pointed at `grofe` in place.
fn merge(kind: &Kind, mut settings: Value, grofe: &str) -> Value {
    if !settings.is_object() {
        settings = json!({});
    }
    let Some(root) = settings.as_object_mut() else {
        return settings;
    };
    let Some(hooks) = event_map(root, kind, true) else {
        return settings;
    };
    for (event, matcher, args) in kind.hooks {
        let cmd = command(kind, grofe, args);
        let groups = hooks.entry(*event).or_insert_with(|| json!([]));
        if !groups.is_array() {
            *groups = json!([]);
        }
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        let mut found = false;
        groups.retain_mut(|g| {
            let Some(list) = g.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let n = list.len();
            list.retain_mut(|h| {
                if !is_ours_with(kind, h, args) {
                    return true;
                }
                // The first one is updated, any duplicate from an older install goes.
                let keep = !found;
                found = true;
                if keep {
                    h["command"] = json!(cmd);
                }
                keep
            });
            list.len() == n || !list.is_empty()
        });
        if found {
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

/// Removes grofe's hooks, whatever path they were installed from.
fn remove(kind: &Kind, mut settings: Value) -> Value {
    let Some(root) = settings.as_object_mut() else {
        return settings;
    };
    let bare = is_bare(root, kind);
    let Some(hooks) = event_map(root, kind, false) else {
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
            list.retain(|h| !is_ours(kind, h));
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
    if touched && !bare && hooks.is_empty() {
        root.shift_remove("hooks");
    }
    settings
}

pub fn merge_claude(settings: Value, grofe: &str) -> Value {
    merge(&CLAUDE, settings, grofe)
}

pub fn remove_claude(settings: Value) -> Value {
    remove(&CLAUDE, settings)
}

/// Every grofe command in the settings, as "event: command".
fn ours(kind: &Kind, settings: &Value) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut copy = settings.clone();
    let Some(hooks) = copy.as_object_mut().and_then(|r| event_map(r, kind, false)) else {
        return found;
    };
    for (event, groups) in hooks.iter() {
        for h in groups.as_array().into_iter().flatten().flat_map(hooks_of) {
            if is_ours(kind, h) {
                found.insert(format!("{event}: {}", h["command"].as_str().unwrap_or("")));
            }
        }
    }
    found
}

fn home_path(rel: &str) -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| Path::new(&h).join(rel))
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Where each kind's config lives: settings for the JSON kinds, the plugin for opencode.
fn kind_path(kind: &str) -> Option<PathBuf> {
    match kind {
        "claude" => match env_dir("CLAUDE_CONFIG_DIR") {
            Some(d) => Some(d.join("settings.json")),
            None => home_path(".claude/settings.json"),
        },
        "codex" => match env_dir("CODEX_HOME") {
            Some(d) => Some(d.join("hooks.json")),
            None => home_path(".codex/hooks.json"),
        },
        "gemini" => home_path(".gemini/settings.json"),
        "opencode" => home_path(".config/opencode/plugins/grofe.js"),
        _ => None,
    }
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

/// Resolves symlinks so the edit lands in the real file and the link survives.
fn real_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Dry run stops here (false); otherwise asks unless `opts.yes`. True means go ahead.
fn confirm(opts: &Opts, input: &mut dyn BufRead, out: &mut dyn Write) -> Result<bool, String> {
    let io = |e: std::io::Error| e.to_string();
    if opts.dry_run {
        return Ok(false);
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
    Ok(true)
}

/// Applies or removes `kind`'s hooks in `path`, asking on `input` unless `opts.yes`.
fn setup_json(
    kind: &Kind,
    path: &Path,
    grofe: &str,
    opts: &Opts,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    let target = real_path(path);
    let before = read_settings(&target)?;
    let after = if opts.remove {
        remove(kind, before.clone())
    } else {
        merge(kind, before.clone(), grofe)
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
    let (old, new) = (ours(kind, &before), ours(kind, &after));
    writeln!(out, "{}", target.display()).map_err(io)?;
    for line in new.difference(&old) {
        writeln!(out, "+ {line}").map_err(io)?;
    }
    for line in old.difference(&new) {
        writeln!(out, "- {line}").map_err(io)?;
    }
    if !confirm(opts, input, out)? {
        return Ok(());
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

/// The opencode plugin source. It spawns grofe with an argv array, never a shell string,
/// fire and forget, so a slow or missing grofe cannot stall or crash opencode.
fn plugin_source(grofe: &str) -> String {
    let path = serde_json::to_string(grofe).unwrap_or_else(|_| "\"grofe\"".into());
    format!(
        r#"{PLUGIN_MARKER}
import {{ spawn }} from "node:child_process";

const grofe = {path};

function report(state, payload, extra = []) {{
  try {{
    const child = spawn(grofe, ["agent", "report", state, ...extra, "--hook", "opencode"], {{
      stdio: ["pipe", "ignore", "ignore"],
      detached: true,
    }});
    child.on("error", () => {{}});
    child.stdin.on("error", () => {{}});
    child.stdin.end(JSON.stringify(payload ?? {{}}));
    child.unref();
  }} catch (_) {{}}
}}

export const GrofePlugin = async () => ({{
  event: async ({{ event }}) => {{
    if (event.type === "session.idle") report("done", event);
    else if (event.type === "permission.asked") report("blocked", event, ["--blocked", "permission"]);
  }},
  "tool.execute.before": async (input) => {{
    report("working", input);
  }},
}});

export default GrofePlugin;
"#
    )
}

fn has_marker(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|s| s.lines().next() == Some(PLUGIN_MARKER))
}

/// Writes or deletes the opencode plugin file.
fn setup_opencode(
    path: &Path,
    grofe: &str,
    opts: &Opts,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    let target = real_path(path);
    let existing = std::fs::read_to_string(&target).ok();
    if opts.remove {
        if !has_marker(&target) {
            writeln!(out, "{}: nothing to remove", target.display()).map_err(io)?;
            return Ok(());
        }
        writeln!(out, "{}\n- plugin file", target.display()).map_err(io)?;
        if !confirm(opts, input, out)? {
            return Ok(());
        }
        std::fs::remove_file(&target).map_err(io)?;
        return writeln!(out, "done").map_err(io);
    }
    let source = plugin_source(grofe);
    if existing.as_deref() == Some(source.as_str()) {
        writeln!(out, "{}: already installed", target.display()).map_err(io)?;
        return Ok(());
    }
    if existing.is_some() && !has_marker(&target) {
        return Err(format!("{} exists and is not grofe's", target.display()));
    }
    writeln!(out, "{}\n+ plugin file", target.display()).map_err(io)?;
    if !confirm(opts, input, out)? {
        return Ok(());
    }
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    std::fs::write(&target, source).map_err(io)?;
    writeln!(out, "done").map_err(io)
}

fn json_kind(name: &str) -> Option<&'static Kind> {
    [&CLAUDE, &CODEX, &GEMINI]
        .into_iter()
        .find(|k| k.name == name)
}

/// One line per kind. `path_of` gives each kind's config path, `on_path` whether its binary exists.
fn status(path_of: &dyn Fn(&str) -> Option<PathBuf>, on_path: &dyn Fn(&str) -> bool) -> String {
    let mut s = String::new();
    for name in KINDS {
        let state = if *name == "pi" {
            "no hooks needed"
        } else {
            let path = path_of(name);
            let found = path.as_ref().is_some_and(|p| p.exists()) || on_path(name);
            let installed = match (json_kind(name), &path) {
                (Some(k), Some(p)) => {
                    read_settings(p).is_ok_and(|v| ours(k, &v).len() == k.hooks.len())
                }
                (None, Some(p)) => has_marker(p),
                _ => false,
            };
            if installed {
                "installed"
            } else if found {
                "not installed"
            } else {
                "agent not found"
            }
        };
        s.push_str(&format!("{name}: {state}\n"));
    }
    s
}

fn binary_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
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
    if want_status {
        print!("{}", status(&kind_path, &binary_on_path));
        return ExitCode::SUCCESS;
    }
    let kind = match kind.as_deref() {
        Some("pi") => {
            println!("pi has no hooks; grofe recognises it by name");
            return ExitCode::SUCCESS;
        }
        Some(k) if KINDS.contains(&k) => k.to_string(),
        _ => {
            eprintln!("{usage}");
            return ExitCode::FAILURE;
        }
    };
    let Some(path) = kind_path(&kind) else {
        eprintln!("grofe: no HOME or config dir for {kind}");
        return ExitCode::FAILURE;
    };
    let stdin = std::io::stdin();
    let (input, out) = (&mut stdin.lock(), &mut std::io::stdout());
    let result = match json_kind(&kind) {
        Some(k) => setup_json(k, &path, &grofe, &opts, input, out),
        None => setup_opencode(&path, &grofe, &opts, input, out),
    };
    match result {
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
        setup_json(&CLAUDE, path, G, opts, &mut std::io::empty(), &mut out).unwrap();
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
            "'/b/grofe' agent report done --hook claude"
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
        assert_eq!(remove_claude(merged), start);
        // a foreign hook sharing a group with ours survives
        let mut shared = merge_claude(json!({}), G);
        shared["hooks"]["Stop"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "command", "command": "echo hi"}));
        let left = remove_claude(shared);
        assert_eq!(left["hooks"]["Stop"][0]["hooks"][0]["command"], "echo hi");
        assert!(left["hooks"].get("SessionEnd").is_none());
        assert_eq!(remove_claude(json!({"a": 1})), json!({"a": 1}));
    }

    fn commands(v: &Value, event: &str) -> Vec<String> {
        v["hooks"][event]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(hooks_of)
            .map(|h| h["command"].as_str().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn path_is_shell_quoted() {
        let v = merge_claude(json!({}), "/my bin/grofe");
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0]["command"],
            "'/my bin/grofe' agent report done --hook claude"
        );
        let v = merge_claude(json!({}), "/it's/grofe");
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0]["command"],
            "'/it'\\''s/grofe' agent report done --hook claude"
        );
    }

    #[test]
    fn quoted_command_runs_in_a_shell() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("a b's");
        std::fs::create_dir(&bin).unwrap();
        let exe = bin.join("grofe");
        std::fs::write(&exe, "#!/bin/sh\necho \"$@\"\n").unwrap();
        std::fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let v = merge_claude(json!({}), &exe.to_string_lossy());
        let cmd = v["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        let out = std::process::Command::new("sh")
            .args(["-c", cmd])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "agent report done --hook claude"
        );
    }

    #[test]
    fn rerun_with_new_path_replaces_in_place() {
        let old = merge_claude(
            json!({"hooks": {"Stop": [
                {"hooks": [{"type": "command", "command": "/old/grofe agent report done --hook claude"}]}
            ]}}),
            "/older/grofe",
        );
        let moved = merge_claude(old, "/new bin/grofe");
        for (event, ..) in CLAUDE.hooks {
            let n = CLAUDE.hooks.iter().filter(|(e, ..)| e == event).count();
            let cmds = commands(&moved, event);
            assert_eq!(cmds.len(), n, "{event}: {cmds:?}");
            assert!(
                cmds.iter().all(|c| c.starts_with("'/new bin/grofe' ")),
                "{cmds:?}"
            );
        }
        // idempotent after the move, and a doubled install from the old bug collapses
        assert_eq!(merge_claude(moved.clone(), "/new bin/grofe"), moved);
        let mut doubled = moved.clone();
        doubled["hooks"]["Stop"].as_array_mut().unwrap().push(
            json!({"hooks": [{"type": "command", "command": "/old/grofe agent report done --hook claude"}]}),
        );
        assert_eq!(merge_claude(doubled, "/new bin/grofe"), moved);
    }

    #[test]
    fn remove_after_move_removes_old_entries() {
        let installed = merge_claude(json!({"theme": "dark"}), "/old/grofe");
        assert_eq!(remove_claude(installed), json!({"theme": "dark"}));
        let quoted = merge_claude(json!({}), "/it's/grofe");
        assert_eq!(remove_claude(quoted), json!({}));
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
        assert!(out.contains("+ Stop: '/b/grofe' agent report done --hook claude"));
        assert!(!p.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn prompt_declined_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let mut out = Vec::new();
        let r = setup_json(&CLAUDE, &p, G, &Opts::default(), &mut &b"n\n"[..], &mut out);
        assert!(r.is_err());
        assert!(!p.exists());
    }

    fn paths(dir: &Path) -> impl Fn(&str) -> Option<PathBuf> + '_ {
        move |k| Some(dir.join(k))
    }

    #[test]
    fn status_lists_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("claude");
        let s = status(&paths(dir.path()), &|_| false);
        assert!(s.contains("claude: agent not found"));
        assert!(s.contains("codex: agent not found"));
        assert!(s.contains("pi: no hooks needed"));
        go(&p, &yes());
        assert!(status(&paths(dir.path()), &|_| false).contains("claude: installed"));
        std::fs::write(&p, "{}").unwrap();
        assert!(status(&paths(dir.path()), &|_| false).contains("claude: not installed"));
        assert!(status(&paths(dir.path()), &|k| k == "gemini").contains("gemini: not installed"));
    }

    fn go_kind(kind: &Kind, path: &Path, opts: &Opts) -> String {
        let mut out = Vec::new();
        setup_json(kind, path, G, opts, &mut std::io::empty(), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn rm() -> Opts {
        Opts {
            remove: true,
            yes: true,
            ..Opts::default()
        }
    }

    #[test]
    fn codex_hooks_json_merge() {
        let v = merge(&CODEX, json!({}), G);
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0]["command"],
            "'/b/grofe' agent report done --hook codex"
        );
        assert_eq!(v["hooks"].as_object().unwrap().len(), 5);
        assert_eq!(remove(&CODEX, v), json!({}));
        // a bare event map is merged into as it is
        let bare = json!({"Stop": [{"hooks": [{"type": "command", "command": "echo hi"}]}]});
        let m = merge(&CODEX, bare.clone(), G);
        assert!(m.get("hooks").is_none());
        assert_eq!(commands_in(&m["Stop"]).len(), 2);
        assert_eq!(
            m["PreToolUse"][0]["hooks"][0]["command"],
            "'/b/grofe' agent report working --hook codex"
        );
        assert_eq!(merge(&CODEX, m.clone(), G), m);
        assert_eq!(remove(&CODEX, m), bare);
        // files: absent is created as {"hooks":{}}-shaped, CODEX_HOME style path
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x/hooks.json");
        go_kind(&CODEX, &p, &yes());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(v["hooks"]["SessionStart"].is_array());
        assert!(go_kind(&CODEX, &p, &yes()).contains("already installed"));
    }

    fn commands_in(groups: &Value) -> Vec<String> {
        groups
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(hooks_of)
            .map(|h| h["command"].as_str().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn gemini_settings_merge_keeps_other_keys() {
        let start = json!({"zeta": 1, "theme": "dark", "hooks": {"BeforeTool": [
            {"matcher": "x", "hooks": [{"type": "command", "command": "echo hi"}]}
        ]}, "alpha": [1]});
        let v = merge(&GEMINI, start.clone(), G);
        let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["zeta", "theme", "hooks", "alpha"]);
        assert_eq!(
            v["hooks"]["AfterAgent"][0]["hooks"][0]["command"],
            "'/b/grofe' agent report done --hook gemini"
        );
        assert_eq!(v["hooks"]["BeforeTool"][1]["matcher"], "*");
        assert_eq!(
            v["hooks"]["BeforeTool"][0]["hooks"][0]["command"],
            "echo hi"
        );
        assert_eq!(
            v["hooks"]["Notification"][0]["hooks"][0]["command"],
            "'/b/grofe' agent report blocked --blocked permission --hook gemini"
        );
        assert_eq!(remove(&GEMINI, v), start);
    }

    #[test]
    fn opencode_plugin_written_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a/b/plugins/grofe.js");
        let mut out = Vec::new();
        setup_opencode(&p, G, &yes(), &mut std::io::empty(), &mut out).unwrap();
        let src = std::fs::read_to_string(&p).unwrap();
        assert_eq!(src.lines().next(), Some(PLUGIN_MARKER));
        assert!(src.contains(r#"const grofe = "/b/grofe";"#));
        assert!(src.contains("session.idle") && src.contains("permission.asked"));
        assert!(src.contains("tool.execute.before"));
        assert!(has_marker(&p));
        // a foreign file is neither overwritten nor removed
        let other = dir.path().join("other.js");
        std::fs::write(&other, "// mine\n").unwrap();
        assert!(setup_opencode(&other, G, &yes(), &mut std::io::empty(), &mut out).is_err());
        setup_opencode(&other, G, &rm(), &mut std::io::empty(), &mut out).unwrap();
        assert!(other.exists());
        setup_opencode(&p, G, &rm(), &mut std::io::empty(), &mut out).unwrap();
        assert!(!p.exists());
    }

    #[test]
    fn opencode_plugin_passes_argv_not_shell() {
        let src = plugin_source("/my \"bin/grofe");
        assert!(src.contains(r#"const grofe = "/my \"bin/grofe";"#));
        assert!(src.contains(r#"spawn(grofe, ["agent", "report", state"#));
        assert!(!src.contains("shell: true"));
        assert!(!src.contains("exec("));
        assert!(!src.lines().skip(1).any(|l| l.contains('`')));
        assert!(src.contains("child.on(\"error\""));
    }

    #[test]
    fn rerun_idempotent_each_kind() {
        let dir = tempfile::tempdir().unwrap();
        for kind in [&CLAUDE, &CODEX, &GEMINI] {
            let p = dir.path().join(kind.name);
            go_kind(kind, &p, &yes());
            let first = std::fs::read_to_string(&p).unwrap();
            assert!(go_kind(kind, &p, &yes()).contains("already installed"));
            assert_eq!(std::fs::read_to_string(&p).unwrap(), first);
            assert!(go_kind(kind, &p, &rm()).contains("done"));
            assert!(go_kind(kind, &p, &rm()).contains("nothing to remove"));
        }
        let p = dir.path().join("grofe.js");
        let mut out = Vec::new();
        setup_opencode(&p, G, &yes(), &mut std::io::empty(), &mut out).unwrap();
        let first = std::fs::read_to_string(&p).unwrap();
        out.clear();
        setup_opencode(&p, G, &yes(), &mut std::io::empty(), &mut out).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("already installed")
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), first);
    }
}
