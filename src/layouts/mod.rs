//! TOML layouts (`docs/layouts.md`): `grofe layout up|save|list|pick`.
//!
//! The work is done by ordinary server commands sent through [`Server`], so
//! the server learns no new verb and the tmux identity is untouched. The
//! binary supplies the [`Server`]; tests can supply a fake.

pub mod save;
pub mod schema;
mod up;

use std::fs;
use std::path::{Path, PathBuf};

use schema::Plan;

pub use up::{UpOptions, UpReport, up, watch};

/// What the layout commands need from the running program and server.
pub trait Server {
    /// Runs a server command (a grofe-dialect argv) and returns its output.
    fn run(&mut self, argv: &[String]) -> Result<String, String>;
    /// Runs `new-session`, starting a server first when none runs.
    fn create_session(&mut self, argv: &[String]) -> Result<String, String>;
    /// `(id, name)` of every session; no server means none.
    fn sessions(&mut self) -> Result<Vec<(String, String)>, String>;
    /// Runs `command` as `run-shell` would, on the host, in `cwd`.
    fn host_shell(&mut self, command: &str, cwd: &Path, session: &str) -> Result<(), String>;
    /// Attaches this terminal to the session, or switches the client this
    /// command runs in. Returns a line to print.
    fn attach(&mut self, session_id: &str) -> Result<(), String>;
    /// Starts the detached `layout __watch` helper for `on_stop`.
    fn spawn_watch(
        &mut self,
        session_id: &str,
        name: &str,
        root: &Path,
        command: &str,
    ) -> Result<(), String>;
    /// The words that run this grofe against this server again, for a menu.
    fn self_command(&self) -> Vec<String>;
}

/// Where layouts live and how paths resolve.
pub struct Dirs {
    /// `<config dir>/layouts`; `None` when there is no config dir.
    pub layouts: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub cwd: PathBuf,
}

impl Dirs {
    pub fn from_process() -> Dirs {
        Dirs {
            layouts: crate::config::Env::from_process()
                .config_dir
                .map(|d| d.join("layouts")),
            home: std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(PathBuf::from),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        }
    }

    fn layouts_dir(&self) -> Result<&Path, String> {
        self.layouts
            .as_deref()
            .ok_or_else(|| "no config directory: set HOME or XDG_CONFIG_HOME".to_string())
    }

    /// The file `arg` names: a path when it starts with `/`, `./` or `~/`,
    /// else a name inside the layouts directory.
    pub fn resolve(&self, arg: &str) -> Result<PathBuf, String> {
        if let Some(rest) = arg.strip_prefix("~/") {
            let home = self.home.as_deref().ok_or("~ needs HOME")?;
            return Ok(home.join(rest));
        }
        if arg.starts_with('/') || arg.starts_with("./") {
            return Ok(self.cwd.join(arg));
        }
        let name = arg.strip_suffix(".toml").unwrap_or(arg);
        if !valid_name(name) {
            return Err(format!("invalid layout name: {arg:?}"));
        }
        Ok(self.layouts_dir()?.join(format!("{name}.toml")))
    }
}

/// `[A-Za-z0-9][A-Za-z0-9._-]*` without `..`: nothing that leaves a directory.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !name.contains("..")
}

/// Reads and validates a layout file.
pub fn load(path: &Path, dirs: &Dirs) -> Result<Plan, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    schema::parse(&text, path, dirs.home.as_deref(), &dirs.cwd).map_err(|e| e.to_string())
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

/// `grofe layout <args>`: prints to `out`; the error is the message to show.
pub fn run(
    args: &[String],
    server: &mut dyn Server,
    dirs: &Dirs,
    out: &mut dyn std::io::Write,
) -> Result<(), String> {
    let usage = "usage: grofe layout up <name|path> [-d] [--session NAME]\n       \
                 grofe layout save <session> [name|path] [--force]\n       \
                 grofe layout list\n       grofe layout pick";
    let Some((verb, rest)) = args.split_first() else {
        return Err(usage.to_string());
    };
    let mut say = |line: &str| writeln!(out, "{line}").map_err(|e| e.to_string());
    match verb.as_str() {
        "up" => {
            let (flags, pos) = split_flags(rest, &["-d"], &["--session"])?;
            let [target] = pos.as_slice() else {
                return Err(usage.to_string());
            };
            let plan = load(&dirs.resolve(target)?, dirs)?;
            let opts = UpOptions {
                detach: flags.has("-d"),
                session: flags.value("--session").map(str::to_string),
            };
            let report = up(server, &plan, &opts)?;
            for l in &report.lines {
                say(l)?;
            }
            for w in &report.warnings {
                eprintln!("grofe: warning: {w}");
            }
            if let Some(id) = report.attach {
                server.attach(&id)?;
            }
            Ok(())
        }
        "save" => {
            let (flags, pos) = split_flags(rest, &["--force"], &[])?;
            let (session, dest) = match pos.as_slice() {
                [s] => (s.as_str(), None),
                [s, d] => (s.as_str(), Some(d.as_str())),
                _ => return Err(usage.to_string()),
            };
            let path = save::save(server, dirs, session, dest, flags.has("--force"))?;
            say(&format!("saved {}", path.display()))
        }
        "list" => {
            if !rest.is_empty() {
                return Err(usage.to_string());
            }
            for e in entries(dirs)? {
                say(&format!(
                    "{}\t{}\t{}",
                    e.name,
                    e.file.display(),
                    match &e.sessions {
                        Ok(s) => s.join(","),
                        Err(why) => format!("! {why}"),
                    }
                ))?;
            }
            Ok(())
        }
        "pick" => {
            if !rest.is_empty() {
                return Err(usage.to_string());
            }
            let argv = pick_menu(&entries(dirs)?, &server.self_command(), dirs)?;
            server.run(&argv).map(|_| ())
        }
        "__watch" => {
            let [id, name, root, command] = rest else {
                return Err("layout __watch: internal verb".to_string());
            };
            watch(server, id, name, Path::new(root), command)
        }
        other => Err(format!("layout: unknown subcommand {other:?}\n{usage}")),
    }
}

struct Flags {
    set: Vec<String>,
    values: Vec<(String, String)>,
}

impl Flags {
    fn has(&self, f: &str) -> bool {
        self.set.iter().any(|s| s == f)
    }
    fn value(&self, f: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(k, _)| k == f)
            .map(|(_, v)| v.as_str())
    }
}

/// Flags may come anywhere; `--` ends them.
fn split_flags(
    args: &[String],
    bools: &[&str],
    valued: &[&str],
) -> Result<(Flags, Vec<String>), String> {
    let mut flags = Flags {
        set: Vec::new(),
        values: Vec::new(),
    };
    let mut pos = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            pos.extend(it.by_ref().cloned());
        } else if bools.contains(&a.as_str()) {
            flags.set.push(a.clone());
        } else if valued.contains(&a.as_str()) {
            let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
            flags.values.push((a.clone(), v.clone()));
        } else if a.starts_with('-') && a.len() > 1 {
            return Err(format!("layout: unknown flag {a}"));
        } else {
            pos.push(a.clone());
        }
    }
    Ok((flags, pos))
}

/// One file in the layouts directory.
pub struct Entry {
    pub name: String,
    pub file: PathBuf,
    pub sessions: Result<Vec<String>, String>,
}

/// Every `*.toml` in the layouts directory, by name; none if it is missing.
pub fn entries(dirs: &Dirs) -> Result<Vec<Entry>, String> {
    let dir = dirs.layouts_dir()?;
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    let mut found = Vec::new();
    for entry in read.flatten() {
        let file = entry.path();
        let Some(name) = file
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".toml"))
            .filter(|n| valid_name(n))
        else {
            continue;
        };
        if !file.is_file() {
            continue;
        }
        let sessions = load(&file, dirs).map(|p| p.sessions.into_iter().map(|s| s.name).collect());
        found.push(Entry {
            name: name.to_string(),
            file,
            sessions,
        });
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

/// Single-quotes `s` for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The `display-menu` request for the valid layouts: each item runs `up`.
fn pick_menu(entries: &[Entry], me: &[String], dirs: &Dirs) -> Result<Vec<String>, String> {
    let usable: Vec<&Entry> = entries.iter().filter(|e| e.sessions.is_ok()).collect();
    if usable.is_empty() {
        return Err(format!("no layouts in {}", dirs.layouts_dir()?.display()));
    }
    let keys = "123456789abcdefghijklmnopqrstuvwxyz";
    let mut argv = words(&["display-menu", "-T", "Layouts", "-x", "C", "-y", "C"]);
    let me: Vec<String> = me.iter().map(|w| shell_quote(w)).collect();
    for (e, key) in usable.iter().zip(keys.chars()) {
        let sessions = e
            .sessions
            .as_ref()
            .map(|s| s.join(", "))
            .unwrap_or_default();
        argv.push(format!("{} ({sessions})", e.name).replace('#', "##"));
        argv.push(key.to_string());
        let up = format!("{} layout up -- {}", me.join(" "), shell_quote(&e.name));
        argv.push(format!(
            "run-shell -b {}",
            shell_quote(&up.replace('#', "##"))
        ));
    }
    Ok(argv)
}
