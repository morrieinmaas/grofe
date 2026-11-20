//! `grofe show-config [--explain] [--file <path>]... [<key>]` (spec §5.4).
//!
//! File layers only: it never starts or contacts a server, even when one
//! is running (R8).
// ponytail: runtime provenance (a value changed by `set-option`) is not
// shown; this shows what the files resolve to.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::{
    ConfigHook, ConfigKind, ConfigPlugin, ConfigSource, Env, KeyBinding, LIST_NAMES, Resolved,
    closest, list_tmux_name, literal, load, load_with_file, option_names, tmux_name,
};
use crate::server::dispatch::options::has_no_effect;
use crate::server::input::parse_key;
use crate::util::quote;

const USAGE: &str = "usage: show-config [--explain] [--file <path>]... [<key>]";

/// Runs `show-config` with `args` (after the verb) and returns the exit
/// status.
pub fn show_config(args: &[String]) -> ExitCode {
    match run(args) {
        Ok(out) => {
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(out.as_bytes());
            let _ = stdout.flush();
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("{}", msg.trim_end_matches('\n'));
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<String, String> {
    let mut explain = false;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut key: Option<&str> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--explain" => explain = true,
            "--file" => match it.next() {
                Some(path) => files.push(PathBuf::from(path)),
                None => return Err(format!("show-config: --file needs a path\n{USAGE}")),
            },
            s if s.starts_with("--file=") => files.push(PathBuf::from(&s["--file=".len()..])),
            s if s.starts_with('-') && s.len() > 1 => {
                return Err(format!("show-config: unknown flag {s}\n{USAGE}"));
            }
            s if key.is_none() => key = Some(s),
            s => return Err(format!("show-config: unexpected argument {s}\n{USAGE}")),
        }
    }

    let env = Env::from_process();
    let loaded = if files.is_empty() {
        load(&env)
    } else {
        load_with_file(&env, &files)
    };
    let r = loaded.map_err(|errs| {
        errs.iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    for w in &r.warnings {
        eprintln!("grofe: warning: {w}");
    }
    let home = env.home.as_deref();
    let mut out = String::new();
    let option = |out: &mut String, k: &str| {
        let value = r.get(k).unwrap_or_default();
        let no_effect = explain && tmux_name(k).is_some_and(has_no_effect);
        out.push_str(&value_line(k, &literal(k, value), no_effect));
        if explain && let Some(src) = r.provenance.get(k) {
            out.push_str(&format!("  set by {}\n", src.describe(home)));
        }
    };
    let list = |out: &mut String, k: &str| {
        let items = r.lists.get(k).cloned().unwrap_or_default();
        let items = toml::Value::Array(items.into_iter().map(toml::Value::String).collect());
        out.push_str(&format!("{k} = {items}\n"));
        if explain && let Some(src) = r.provenance.get(k) {
            out.push_str(&format!("  set by {}\n", src.describe(home)));
        }
    };
    let binding = |out: &mut String, b: &KeyBinding| {
        out.push_str(&format!(
            "keys.{}.{} = {}\n",
            quote::toml_key(&b.table),
            quote::toml_key(&b.key.to_string()),
            quote::toml_binding(
                &b.command,
                &[
                    ("desc", b.desc.as_deref()),
                    ("group", b.group.as_deref()),
                    ("name", b.name.as_deref()),
                ],
                b.repeat,
            )
        ));
        if explain {
            out.push_str(&format!("  set by {}\n", b.source.describe(home)));
        }
    };
    let hook = |out: &mut String, event: &str, h: &ConfigHook| {
        let command = toml::Value::String(h.command.clone());
        out.push_str(&format!("hooks.{event} = {command}\n"));
        if explain {
            out.push_str(&format!("  set by {}\n", h.source.describe(home)));
        }
    };
    let status_source = |out: &mut String, name: &str, s: &ConfigSource| {
        out.push_str(&format!(
            "status.sources.{} = {}\n",
            quote::toml_key(name),
            source_value(s)
        ));
        if explain {
            out.push_str(&format!("  set by {}\n", s.source.describe(home)));
        }
    };
    let plugin = |out: &mut String, name: &str, p: &ConfigPlugin| {
        out.push_str(&format!(
            "plugins.{} = {}\n",
            quote::toml_key(name),
            plugin_value(p)
        ));
        if explain {
            for (key, src) in &p.sources {
                out.push_str(&format!("  {key} set by {}\n", src.describe(home)));
            }
        }
    };
    let user_option = |out: &mut String, name: &str, value: &str| {
        let name = name.strip_prefix('@').unwrap_or(name);
        out.push_str(&format!(
            "user-options.{} = {}\n",
            quote::toml_key(name),
            quote::toml_str(value)
        ));
    };
    let variable = |out: &mut String, name: &str, value: &str| {
        out.push_str(&format!(
            "environment.{} = {}\n",
            quote::toml_key(name),
            quote::toml_str(value)
        ));
    };
    let load_commands = |out: &mut String| {
        if r.load_commands.is_empty() {
            return;
        }
        let items = r
            .load_commands
            .iter()
            .map(|c| toml::Value::String(c.command.clone()));
        out.push_str(&format!(
            "load-commands = {}\n",
            toml::Value::Array(items.collect())
        ));
        if explain {
            for c in &r.load_commands {
                out.push_str(&format!("  set by {}\n", c.source.describe(home)));
            }
        }
    };
    // ponytail: `--explain` names no file for a user option or variable;
    // the loader keeps no provenance for them.
    match key {
        None => {
            for (k, _) in option_names() {
                option(&mut out, k);
            }
            for (k, _) in LIST_NAMES {
                list(&mut out, k);
            }
            for b in sorted_keys(&r) {
                binding(&mut out, b);
            }
            for (event, h) in &r.hooks {
                hook(&mut out, event, h);
            }
            for (name, s) in &r.sources {
                status_source(&mut out, name, s);
            }
            for (name, p) in &r.plugins {
                plugin(&mut out, name, p);
            }
            out.push_str(&agents_lines(&r, home, explain, "agents")?);
            out.push_str(&notify_lines(&r, home, explain, "notify")?);
            for (name, value) in &r.user_options {
                user_option(&mut out, name, value);
            }
            for (name, value) in &r.environment {
                variable(&mut out, name, value);
            }
            load_commands(&mut out);
        }
        Some(k) if k == "agents" || k.starts_with("agents.") => {
            out.push_str(&agents_lines(&r, home, explain, k)?);
        }
        Some(k) if k == "notify" || k.starts_with("notify.") => {
            out.push_str(&notify_lines(&r, home, explain, k)?);
        }
        Some("plugins") => {
            for (name, p) in &r.plugins {
                plugin(&mut out, name, p);
            }
        }
        Some(k) if k.starts_with("plugins.") => {
            let name = &k["plugins.".len()..];
            let p = r
                .plugins
                .get(name)
                .ok_or_else(|| format!("show-config: no plugin {k}"))?;
            plugin(&mut out, name, p);
        }
        Some("user-options") => {
            for (name, value) in &r.user_options {
                user_option(&mut out, name, value);
            }
        }
        Some(k) if k.starts_with("user-options.") => {
            let name = format!("@{}", &k["user-options.".len()..]);
            let value = r
                .user_options
                .get(&name)
                .ok_or_else(|| format!("show-config: no user option {k}"))?;
            user_option(&mut out, &name, value);
        }
        Some("environment") => {
            for (name, value) in &r.environment {
                variable(&mut out, name, value);
            }
        }
        Some(k) if k.starts_with("environment.") => {
            let name = &k["environment.".len()..];
            let (_, value) = r
                .environment
                .iter()
                .find(|(n, _)| n == name)
                .ok_or_else(|| format!("show-config: no variable {k}"))?;
            variable(&mut out, name, value);
        }
        Some("load-commands") => load_commands(&mut out),
        Some("status.sources") => {
            for (name, s) in &r.sources {
                status_source(&mut out, name, s);
            }
        }
        Some(k) if k.starts_with("status.sources.") => {
            let name = &k["status.sources.".len()..];
            let s = r
                .sources
                .get(name)
                .ok_or_else(|| format!("show-config: no source {k}"))?;
            status_source(&mut out, name, s);
        }
        Some(k) if tmux_name(k).is_some() => option(&mut out, k),
        Some(k) if list_tmux_name(k).is_some() => list(&mut out, k),
        Some(k) if k.starts_with("keys.") => {
            let b = find_binding(&r, &k["keys.".len()..])
                .ok_or_else(|| format!("show-config: no binding {k}"))?;
            binding(&mut out, b);
        }
        Some(k) if k.starts_with("hooks.") => {
            let event = &k["hooks.".len()..];
            let h = r
                .hooks
                .get(event)
                .ok_or_else(|| format!("show-config: no hook {k}"))?;
            hook(&mut out, event, h);
        }
        Some(k) => {
            let mut msg = format!("show-config: unknown key {k}");
            if let Some(s) = closest(
                k,
                option_names()
                    .chain(LIST_NAMES.iter().copied())
                    .map(|(t, _)| t),
            ) {
                msg.push_str(&format!(", did you mean {s}?"));
            }
            return Err(msg);
        }
    }
    Ok(out)
}

/// One option's line; `--explain` marks an option grofe stores but never
/// reads (spec §4.5).
fn value_line(key: &str, literal: &str, no_effect: bool) -> String {
    let marker = if no_effect { "  (no effect)" } else { "" };
    format!("{key} = {literal}{marker}\n")
}

fn sorted_keys(r: &Resolved) -> Vec<&KeyBinding> {
    let mut keys: Vec<&KeyBinding> = r.keys.iter().collect();
    keys.sort_by_key(|b| (b.table.clone(), b.key.to_string()));
    keys
}

/// `<table>.<key>`, the key as `parse_key` reads it (quotes optional).
fn find_binding<'a>(r: &'a Resolved, rest: &str) -> Option<&'a KeyBinding> {
    let (table, name) = rest.split_once('.')?;
    let name = name
        .strip_prefix('"')
        .and_then(|n| n.strip_suffix('"'))
        .unwrap_or(name);
    let key = parse_key(name).ok()?;
    r.keys.iter().find(|b| b.table == table && b.key == key)
}

fn source_value(s: &ConfigSource) -> String {
    let mut fields = vec![format!("cmd = {}", quote::toml_str(&s.cmd))];
    for (k, v) in [("interval", s.interval), ("timeout", s.timeout)] {
        if let Some(v) = v {
            fields.push(format!("{k} = {v}"));
        }
    }
    format!("{{ {} }}", fields.join(", "))
}

fn plugin_value(p: &ConfigPlugin) -> String {
    let cmd = toml::Value::Array(p.cmd.iter().cloned().map(toml::Value::String).collect());
    let mut fields = vec![format!("cmd = {cmd}")];
    if !p.env.is_empty() {
        let vars: Vec<String> = p
            .env
            .iter()
            .map(|(k, v)| format!("{} = {}", quote::toml_key(k), quote::toml_str(v)))
            .collect();
        fields.push(format!("env = {{ {} }}", vars.join(", ")));
    }
    fields.push(format!("enabled = {}", p.enabled));
    fields.push(format!("restart = \"{}\"", p.restart.name()));
    format!("{{ {} }}", fields.join(", "))
}

/// The `[agents]` lines under `key` (`agents`, `agents.enabled`,
/// `agents.kinds` or `agents.kinds.<name>`, ...).
fn agents_lines(
    r: &Resolved,
    home: Option<&Path>,
    explain: bool,
    key: &str,
) -> Result<String, String> {
    let a = &r.agents;
    let mut out = String::new();
    let scalar = |out: &mut String, k: &str, value: String| {
        out.push_str(&format!("{k} = {value}\n"));
        if explain {
            let by = r
                .provenance
                .get(k)
                .map_or_else(|| "built-in defaults".to_string(), |s| s.describe(home));
            out.push_str(&format!("  set by {by}\n"));
        }
    };
    let kind = |out: &mut String, name: &str, k: &ConfigKind| {
        out.push_str(&format!(
            "agents.kinds.{} = {}\n",
            quote::toml_key(name),
            kind_value(k)
        ));
        if explain {
            for field in ["match", "command", "resume", "enabled", "worktree"] {
                let path = format!("agents.kinds.{name}.{field}");
                if let Some(src) = r.provenance.get(&path) {
                    out.push_str(&format!("  {field} set by {}\n", src.describe(home)));
                }
            }
        }
    };
    let want = |k: &str| key == "agents" || key == k;
    if want("agents.enabled") {
        scalar(&mut out, "agents.enabled", a.enabled.to_string());
    }
    if want("agents.scan-interval") {
        let ms = a.scan_interval.as_millis();
        let text = if ms.is_multiple_of(1000) {
            format!("{}s", ms / 1000)
        } else {
            format!("{ms}ms")
        };
        scalar(&mut out, "agents.scan-interval", quote::toml_str(&text));
    }
    let list =
        |v: &[String]| toml::Value::Array(v.iter().cloned().map(toml::Value::String).collect());
    let rows = [
        ("agents.sidebar.width", a.sidebar.width.to_string()),
        (
            "agents.sidebar.show-on-attach",
            a.sidebar.show_on_attach.to_string(),
        ),
        (
            "agents.sidebar.min-client-width",
            a.sidebar.min_client_width.to_string(),
        ),
        ("agents.grid.tile-width", a.grid.tile_width.to_string()),
        (
            "agents.grid.preview-lines",
            a.grid.preview_lines.to_string(),
        ),
        (
            "agents.picker.width",
            quote::toml_str(&format!("{}%", a.picker.width_pct)),
        ),
        (
            "agents.picker.height",
            quote::toml_str(&format!("{}%", a.picker.height_pct)),
        ),
        ("agents.new.worktree", quote::toml_str(&a.new.worktree)),
        (
            "agents.new.worktree-command",
            list(&a.new.worktree_command).to_string(),
        ),
    ];
    let mut shown = false;
    for (k, v) in rows {
        let table = k.rsplit_once('.').map_or("", |(t, _)| t);
        if want(k) || key == table {
            scalar(&mut out, k, v);
            shown = true;
        }
    }
    if key == "agents" || key == "agents.kinds" {
        for (name, k) in &a.kinds {
            kind(&mut out, name, k);
        }
    } else if let Some(name) = key.strip_prefix("agents.kinds.") {
        let k = a
            .kinds
            .get(name)
            .ok_or_else(|| format!("show-config: no kind {key}"))?;
        kind(&mut out, name, k);
    } else if !want("agents.enabled") && !want("agents.scan-interval") && !shown {
        return Err(format!("show-config: unknown key {key}"));
    }
    Ok(out)
}

/// The `[notify]` lines under `key` (`notify` or `notify.<key>`).
fn notify_lines(
    r: &Resolved,
    home: Option<&Path>,
    explain: bool,
    key: &str,
) -> Result<String, String> {
    let n = &r.notify;
    let list =
        |v: &[String]| toml::Value::Array(v.iter().cloned().map(toml::Value::String).collect());
    let secs = |d: std::time::Duration| {
        let ms = d.as_millis();
        quote::toml_str(&if ms.is_multiple_of(1000) {
            format!("{}s", ms / 1000)
        } else {
            format!("{ms}ms")
        })
    };
    let mut rows = vec![
        ("enabled", n.enabled.to_string()),
        ("on", list(&n.on).to_string()),
        ("toast", list(&n.toast).to_string()),
        ("desktop", list(&n.desktop).to_string()),
        ("history", n.history.to_string()),
        ("skip-when-seen", n.skip_when_seen.to_string()),
        ("done-min-work", secs(n.done_min_work)),
        ("min-interval", secs(n.min_interval)),
        ("toast-time", secs(n.toast_time)),
        ("delivery", quote::toml_str(&n.delivery)),
        ("sound", n.sound.to_string()),
    ];
    if let Some(c) = &n.command {
        rows.push(("command", list(c).to_string()));
    }
    let mut out = String::new();
    let mut found = key == "notify";
    for (name, value) in rows {
        let path = format!("notify.{name}");
        if key != "notify" && key != path {
            continue;
        }
        found = true;
        out.push_str(&format!("{path} = {value}\n"));
        if explain {
            let by = r
                .provenance
                .get(&path)
                .map_or_else(|| "built-in defaults".to_string(), |s| s.describe(home));
            out.push_str(&format!("  set by {by}\n"));
        }
    }
    if found {
        Ok(out)
    } else {
        Err(format!("show-config: unknown key {key}"))
    }
}

fn kind_value(k: &ConfigKind) -> String {
    let mut fields = Vec::new();
    if let Some(m) = &k.matches {
        let m = toml::Value::Array(m.iter().cloned().map(toml::Value::String).collect());
        fields.push(format!("match = {m}"));
    }
    for (name, v) in [("command", &k.command), ("resume", &k.resume)] {
        if let Some(v) = v {
            fields.push(format!("{name} = {}", quote::toml_str(v)));
        }
    }
    if let Some(e) = k.enabled {
        fields.push(format!("enabled = {e}"));
    }
    if let Some(w) = &k.worktree {
        fields.push(format!("worktree = {}", quote::toml_str(w)));
    }
    format!("{{ {} }}", fields.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(text: &str) -> Resolved {
        use crate::config::load_one;
        let dir = std::env::temp_dir().join(format!("grofe-agents-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("a.toml");
        std::fs::write(&path, text).expect("write");
        let env = Env::from_vars(None, None);
        load_one(&path, &env).expect("loads")
    }

    #[test]
    fn show_config_lists_agents() {
        let r = resolved(
            "[agents]\nscan-interval = \"500ms\"\n[agents.kinds.claude]\nmatch = [\"claude\"]\nresume = \"claude -r {id}\"",
        );
        let out = agents_lines(&r, None, false, "agents").expect("lines");
        assert_eq!(
            out,
            "agents.enabled = true\nagents.scan-interval = \"500ms\"\n\
             agents.sidebar.width = 32\nagents.sidebar.show-on-attach = false\n\
             agents.sidebar.min-client-width = 120\nagents.grid.tile-width = 60\n\
             agents.grid.preview-lines = 12\nagents.picker.width = \"80%\"\n\
             agents.picker.height = \"70%\"\nagents.new.worktree = \"never\"\n\
             agents.new.worktree-command = [\"git\", \"-C\", \"{dir}\", \"worktree\", \"add\", \"-b\", \"{branch}\", \"{path}\"]\n\
             agents.kinds.claude = { match = [\"claude\"], resume = \"claude -r {id}\" }\n"
        );
        let one = agents_lines(&r, None, true, "agents.kinds.claude").expect("lines");
        assert!(one.contains("  resume set by "), "{one}");
        assert!(agents_lines(&r, None, false, "agents.kinds.nope").is_err());
        assert!(agents_lines(&r, None, false, "agents.bogus").is_err());
        let sc = agents_lines(&r, None, true, "agents.enabled").expect("lines");
        assert_eq!(sc, "agents.enabled = true\n  set by built-in defaults\n");
    }

    #[test]
    fn explain_marks_a_no_effect_option() {
        assert_eq!(
            value_line("default-terminal", "\"tmux-256color\"", true),
            "default-terminal = \"tmux-256color\"  (no effect)\n"
        );
        assert_eq!(value_line("mouse", "true", false), "mouse = true\n");
    }
}
