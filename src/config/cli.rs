//! `grofe show-config [--explain] [--file <path>]... [<key>]` (spec §5.4).
//!
//! File layers only: it never starts or contacts a server, even when one
//! is running (R8).
// ponytail: runtime provenance (a value changed by `set-option`) is not
// shown; this shows what the files resolve to.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use super::{
    ConfigHook, ConfigPlugin, ConfigSource, Env, KeyBinding, LIST_NAMES, OPTION_NAMES, Resolved,
    Ty, closest, list_tmux_name, load, load_with_file, tmux_name, toml_type,
};
use crate::server::dispatch::options::has_no_effect;
use crate::server::input::parse_key;

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
            bare_or_quoted(&b.table),
            bare_or_quoted(&b.key.to_string()),
            binding_value(b)
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
            bare_or_quoted(name),
            source_value(s)
        ));
        if explain {
            out.push_str(&format!("  set by {}\n", s.source.describe(home)));
        }
    };
    let plugin = |out: &mut String, name: &str, p: &ConfigPlugin| {
        out.push_str(&format!(
            "plugins.{} = {}\n",
            bare_or_quoted(name),
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
            bare_or_quoted(name),
            quote(value)
        ));
    };
    let variable = |out: &mut String, name: &str, value: &str| {
        out.push_str(&format!(
            "environment.{} = {}\n",
            bare_or_quoted(name),
            quote(value)
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
            for (k, _) in OPTION_NAMES {
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
            for (name, value) in &r.user_options {
                user_option(&mut out, name, value);
            }
            for (name, value) in &r.environment {
                variable(&mut out, name, value);
            }
            load_commands(&mut out);
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
            if let Some(s) = closest(k, OPTION_NAMES.iter().chain(LIST_NAMES).map(|(t, _)| *t)) {
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

/// An option value (tmux form) as the TOML literal a config would write.
fn literal(toml: &str, value: &str) -> String {
    match (toml_type(toml), toml, value) {
        (Ty::Bool, _, v) => (v == "on").to_string(),
        (Ty::Int, "status.lines", "off") => "0".to_string(),
        (Ty::Int, "status.lines", "on") => "1".to_string(),
        (Ty::Int, _, v) => v.to_string(),
        (Ty::Str, _, v) => quote(v),
    }
}

fn quote(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn bare_or_quoted(s: &str) -> String {
    let bare = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if bare { s.to_string() } else { quote(s) }
}

fn source_value(s: &ConfigSource) -> String {
    let mut fields = vec![format!("cmd = {}", quote(&s.cmd))];
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
            .map(|(k, v)| format!("{} = {}", bare_or_quoted(k), quote(v)))
            .collect();
        fields.push(format!("env = {{ {} }}", vars.join(", ")));
    }
    fields.push(format!("enabled = {}", p.enabled));
    fields.push(format!("restart = \"{}\"", p.restart.name()));
    format!("{{ {} }}", fields.join(", "))
}

fn binding_value(b: &KeyBinding) -> String {
    if b.desc.is_none() && b.group.is_none() && b.name.is_none() && !b.repeat {
        return quote(&b.command);
    }
    let mut fields = vec![format!("cmd = {}", quote(&b.command))];
    for (k, v) in [("desc", &b.desc), ("group", &b.group), ("name", &b.name)] {
        if let Some(v) = v {
            fields.push(format!("{k} = {}", quote(v)));
        }
    }
    if b.repeat {
        fields.push("repeat = true".to_string());
    }
    format!("{{ {} }}", fields.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explain_marks_a_no_effect_option() {
        assert_eq!(
            value_line("default-terminal", "\"tmux-256color\"", true),
            "default-terminal = \"tmux-256color\"  (no effect)\n"
        );
        assert_eq!(value_line("mouse", "true", false), "mouse = true\n");
    }
}
