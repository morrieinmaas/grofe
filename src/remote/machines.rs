//! Saved machines (spec ssh-remote §5): the `<config-dir>/machines.toml`
//! store and `grofe machine add|ls|rm`. The pure views of connected
//! machines (picker, `#{client_machines}`, combined agent list) are in
//! [`view`]. Nothing here does network or terminal I/O.

mod view;

pub use view::*;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthStr;

/// The name the combined views give this machine; a saved machine cannot
/// take it.
pub const LOCAL: &str = "local";

const USAGE: &str = "usage: grofe machine add <name> <ssh-target> [--session s] [--transport ssh|quic] | ls | rm <name>";

// --- the store ---------------------------------------------------------------

/// How the client reaches a machine; `None` in a [`Machine`] means ssh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Ssh,
    Quic,
}

impl Transport {
    pub fn parse(s: &str) -> Option<Transport> {
        match s {
            "ssh" => Some(Transport::Ssh),
            "quic" => Some(Transport::Quic),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Ssh => "ssh",
            Transport::Quic => "quic",
        }
    }
}

/// One saved machine. Never holds a secret: the target is whatever `ssh`
/// accepts, and ssh does the auth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Machine {
    pub name: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}

fn enabled_default() -> bool {
    true
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default, rename = "machine")]
    machines: Vec<Machine>,
}

/// `<config-dir>/machines.toml`.
pub fn machines_path(config_dir: &Path) -> PathBuf {
    config_dir.join("machines.toml")
}

/// A machine name: non-empty, no whitespace, no `/`, not [`LOCAL`].
pub fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a machine name cannot be empty".into());
    }
    if name.chars().any(|c| c.is_whitespace() || c == '/') {
        return Err(format!("bad machine name '{name}': no whitespace or '/'"));
    }
    if name == LOCAL {
        return Err(format!("'{LOCAL}' is this machine's name; pick another"));
    }
    Ok(())
}

/// A target or session as `machine ls` prints it: one word, so the columns
/// stay parseable.
fn check_word(what: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return Err(format!("bad {what} '{value}': it must be one word"));
    }
    Ok(())
}

fn check(m: &Machine) -> Result<(), String> {
    check_name(&m.name)?;
    check_word("target", &m.target)?;
    if let Some(s) = &m.session {
        check_word("session", s)?;
    }
    Ok(())
}

/// The saved machines, in file order. A missing file is none.
pub fn load(path: &Path) -> Result<Vec<Machine>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let file: File = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    for (i, m) in file.machines.iter().enumerate() {
        check(m).map_err(|e| format!("{}: {e}", path.display()))?;
        if file.machines[..i].iter().any(|o| o.name == m.name) {
            return Err(format!(
                "{}: machine '{}' is listed twice",
                path.display(),
                m.name
            ));
        }
    }
    Ok(file.machines)
}

/// Writes `machines` in order through a temp file in the same directory,
/// then renames it over `path`.
pub fn save(path: &Path, machines: &[Machine]) -> Result<(), String> {
    let file = File {
        machines: machines.to_vec(),
    };
    let text = toml::to_string(&file).map_err(|e| e.to_string())?;
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".machines.toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// Appends `m`; a name already saved is an error naming that entry.
pub fn add(machines: &mut Vec<Machine>, m: Machine) -> Result<(), String> {
    check(&m)?;
    if let Some(old) = machines.iter().find(|o| o.name == m.name) {
        return Err(format!(
            "machine '{}' already exists (target {}); rm it first",
            old.name, old.target
        ));
    }
    machines.push(m);
    Ok(())
}

/// Removes and returns the machine called `name`.
pub fn remove(machines: &mut Vec<Machine>, name: &str) -> Result<Machine, String> {
    let i = machines
        .iter()
        .position(|m| m.name == name)
        .ok_or_else(|| format!("no machine named '{name}'"))?;
    Ok(machines.remove(i))
}

/// `machine ls`: one line per machine, columns name, target, session,
/// transport, enabled, padded with spaces. An unset field is `-`, enabled
/// is `on` or `off`. No header, so a script reads it with `awk`.
pub fn ls_text(machines: &[Machine]) -> String {
    let rows: Vec<[&str; 5]> = machines
        .iter()
        .map(|m| {
            [
                m.name.as_str(),
                m.target.as_str(),
                m.session.as_deref().unwrap_or("-"),
                m.transport.map_or("-", Transport::as_str),
                if m.enabled { "on" } else { "off" },
            ]
        })
        .collect();
    let mut widths = [0usize; 5];
    for r in &rows {
        for (w, c) in widths.iter_mut().zip(r) {
            *w = (*w).max(c.width());
        }
    }
    let mut out = String::new();
    for r in &rows {
        let mut line = String::new();
        for (i, c) in r.iter().enumerate() {
            line.push_str(c);
            if i + 1 < r.len() {
                line.push_str(&" ".repeat(widths[i] - c.width() + 2));
            }
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// `grofe machine ...`, `args` after `machine`. Exits 1 with a message on
/// any error.
pub fn run(args: &[String]) -> i32 {
    let env = crate::config::Env::from_process();
    let Some(dir) = env.config_dir else {
        return fail("neither XDG_CONFIG_HOME nor HOME is set");
    };
    match execute(args, &machines_path(&dir)) {
        Ok(out) => {
            print!("{out}");
            0
        }
        Err(e) => fail(&e),
    }
}

fn fail(msg: &str) -> i32 {
    eprintln!("grofe machine: {msg}");
    1
}

/// The command against the file at `path`; the text to print on success.
fn execute(args: &[String], path: &Path) -> Result<String, String> {
    match args.first().map(String::as_str) {
        Some("add") => {
            let m = parse_add(&args[1..])?;
            let mut machines = load(path)?;
            add(&mut machines, m)?;
            save(path, &machines)?;
            Ok(String::new())
        }
        Some("ls") if args.len() == 1 => Ok(ls_text(&load(path)?)),
        Some("rm") if args.len() == 2 => {
            let mut machines = load(path)?;
            remove(&mut machines, &args[1])?;
            save(path, &machines)?;
            Ok(String::new())
        }
        _ => Err(USAGE.into()),
    }
}

fn parse_add(args: &[String]) -> Result<Machine, String> {
    let mut words = Vec::new();
    let mut session = None;
    let mut transport = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session" => {
                session = Some(it.next().ok_or("--session needs a value")?.clone());
            }
            "--transport" => {
                let t = it.next().ok_or("--transport needs a value")?;
                transport = Some(
                    Transport::parse(t)
                        .ok_or_else(|| format!("unknown transport '{t}': ssh or quic"))?,
                );
            }
            f if f.starts_with("--") => return Err(format!("unknown flag {f}\n{USAGE}")),
            _ => words.push(a.clone()),
        }
    }
    let [name, target] = <[String; 2]>::try_from(words).map_err(|_| USAGE.to_string())?;
    Ok(Machine {
        name,
        target,
        session,
        transport,
        enabled: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(name: &str, target: &str) -> Machine {
        Machine {
            name: name.into(),
            target: target.into(),
            session: None,
            transport: None,
            enabled: true,
        }
    }

    #[test]
    fn round_trip_keeps_order_and_fields() {
        let d = tempfile::tempdir().unwrap();
        let p = machines_path(d.path());
        let mut list = vec![machine("zed", "z.example"), machine("alpha", "dev@a:2222")];
        list[1].session = Some("work".into());
        list[1].transport = Some(Transport::Quic);
        list[0].enabled = false;
        save(&p, &list).unwrap();
        assert_eq!(load(&p).unwrap(), list);
        let names: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec!["machines.toml"], "no temp file left");
    }

    #[test]
    fn file_format_sample() {
        let d = tempfile::tempdir().unwrap();
        let p = machines_path(d.path());
        let mut box1 = machine("box", "box");
        box1.session = Some("dev".into());
        box1.transport = Some(Transport::Ssh);
        save(&p, &[box1, machine("b2", "b2")]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "[[machine]]\nname = \"box\"\ntarget = \"box\"\nsession = \"dev\"\ntransport = \"ssh\"\nenabled = true\n\n[[machine]]\nname = \"b2\"\ntarget = \"b2\"\nenabled = true\n"
        );
    }

    #[test]
    fn missing_file_is_empty_and_defaults_apply() {
        let d = tempfile::tempdir().unwrap();
        let p = machines_path(d.path());
        assert!(load(&p).unwrap().is_empty());
        std::fs::write(&p, "[[machine]]\nname = \"a\"\ntarget = \"t\"\n").unwrap();
        assert_eq!(load(&p).unwrap(), vec![machine("a", "t")]);
    }

    #[test]
    fn load_rejects_bad_files() {
        let d = tempfile::tempdir().unwrap();
        let p = machines_path(d.path());
        for bad in [
            "[[machine]]\nname = \"a b\"\ntarget = \"t\"\n",
            "[[machine]]\nname = \"a\"\ntarget = \"t\"\n[[machine]]\nname = \"a\"\ntarget = \"u\"\n",
            "[[machine]]\nname = \"a\"\ntarget = \"t\"\npassword = \"x\"\n",
            "[[machine]]\nname = \"a\"\ntarget = \"t\"\ntransport = \"mosh\"\n",
        ] {
            std::fs::write(&p, bad).unwrap();
            let e = load(&p).unwrap_err();
            assert!(e.contains("machines.toml"), "{bad}: {e}");
        }
    }

    #[test]
    fn names_are_checked() {
        assert!(check_name("box-1.dev").is_ok());
        for bad in ["", "a b", "a/b", "a\tb", "local"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn add_duplicate_names_the_existing_one() {
        let mut list = vec![machine("box", "old.example")];
        let e = add(&mut list, machine("box", "new")).unwrap_err();
        assert!(e.contains("'box'") && e.contains("old.example"), "{e}");
        assert_eq!(list.len(), 1);
        assert!(add(&mut list, machine("x", "two words")).is_err());
    }

    #[test]
    fn remove_unknown_is_an_error() {
        let mut list = vec![machine("a", "t"), machine("b", "u")];
        assert_eq!(remove(&mut list, "a").unwrap().name, "a");
        assert!(remove(&mut list, "a").is_err());
        assert_eq!(list, vec![machine("b", "u")]);
    }

    #[test]
    fn ls_columns() {
        let mut q = machine("quick", "q");
        q.session = Some("work".into());
        q.transport = Some(Transport::Quic);
        q.enabled = false;
        assert_eq!(
            ls_text(&[machine("a", "dev@long.example"), q]),
            "a      dev@long.example  -     -     on\nquick  q                 work  quic  off\n"
        );
        assert_eq!(ls_text(&[]), "");
    }

    #[test]
    fn parse_add_flags_anywhere() {
        let args: Vec<String> = ["--session", "s", "n", "t", "--transport", "ssh"]
            .map(String::from)
            .to_vec();
        let m = parse_add(&args).unwrap();
        assert_eq!(
            (m.name.as_str(), m.target.as_str(), m.session.as_deref()),
            ("n", "t", Some("s"))
        );
        assert_eq!(m.transport, Some(Transport::Ssh));
        let extra: Vec<String> = ["n", "t", "u"].map(String::from).to_vec();
        assert!(parse_add(&extra).is_err());
        let flag: Vec<String> = ["n", "t", "--force"].map(String::from).to_vec();
        assert!(parse_add(&flag).unwrap_err().contains("--force"));
    }
}
