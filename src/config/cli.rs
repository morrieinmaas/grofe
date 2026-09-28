//! `grofe show-config [--explain] [--file <path>]... [<key>]` (spec §5.4).
//!
//! File layers only: it never starts or contacts a server, even when one
//! is running (R8).
// ponytail: runtime provenance (a value changed by `set-option`, and the
// no-effect marker) is TC.2b's; this shows what the files resolve to.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use super::{
    Env, KeyBinding, OPTION_NAMES, Resolved, Ty, closest, load, load_with_file, tmux_name,
    toml_type,
};
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
        out.push_str(&format!("{k} = {}\n", literal(k, value)));
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
    match key {
        None => {
            for (k, _) in OPTION_NAMES {
                option(&mut out, k);
            }
            for b in sorted_keys(&r) {
                binding(&mut out, b);
            }
        }
        Some(k) if tmux_name(k).is_some() => option(&mut out, k),
        Some(k) if k.starts_with("keys.") => {
            let b = find_binding(&r, &k["keys.".len()..])
                .ok_or_else(|| format!("show-config: no binding {k}"))?;
            binding(&mut out, b);
        }
        Some(k) => {
            let mut msg = format!("show-config: unknown key {k}");
            if let Some(s) = closest(k, OPTION_NAMES.iter().map(|(t, _)| *t)) {
                msg.push_str(&format!(", did you mean {s}?"));
            }
            return Err(msg);
        }
    }
    Ok(out)
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
