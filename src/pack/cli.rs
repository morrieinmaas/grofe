//! `grofe pack sync|update [name] [--yes]|list|clean`. Client side only: the
//! server never clones.

use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};

use super::git::{self, Outcome, Status};
use super::lock::Lock;
use super::spec::{Spec, pack_root};
use crate::config::{self, Env};

const USAGE: &str = "usage: grofe pack sync | update [name] [--yes] | list | clean";
const RELOAD: &str = "run `grofe reload` to load";

/// Where a run reads and writes.
struct Paths {
    lock: PathBuf,
    root: PathBuf,
}

pub fn run(args: &[String]) -> i32 {
    let env = Env::from_process();
    let Some(config_dir) = env.config_dir.clone() else {
        return fail("grofe pack: neither XDG_CONFIG_HOME nor HOME is set");
    };
    let specs = match config::load(&env) {
        Ok(r) => r.pack,
        Err(errs) => {
            for e in errs {
                eprintln!("{e}");
            }
            return 1;
        }
    };
    let home = env.home.clone().unwrap_or_default();
    let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    let paths = Paths {
        lock: config_dir.join("pack-lock.toml"),
        root: pack_root(&home, data.as_deref().filter(|p| !p.as_os_str().is_empty())),
    };
    let tty = std::io::stdin().is_terminal();
    execute(args, &specs, &paths, tty, &mut std::io::stdin().lock())
}

fn execute(
    args: &[String],
    specs: &[Spec],
    paths: &Paths,
    tty: bool,
    input: &mut dyn BufRead,
) -> i32 {
    let enabled: Vec<Spec> = specs.iter().filter(|s| s.enabled).cloned().collect();
    match args.first().map(String::as_str) {
        Some("sync") if args.len() == 1 => sync(&enabled, paths),
        Some("update") => update(&enabled, &args[1..], paths, tty, input),
        Some("list") if args.len() == 1 => list(specs, paths),
        Some("clean") if args.len() == 1 => clean(specs, paths),
        _ => fail(USAGE),
    }
}

fn fail(msg: &str) -> i32 {
    eprintln!("{msg}");
    1
}

fn read_lock(path: &Path) -> Result<Lock, i32> {
    Lock::read(path).map_err(|e| fail(&format!("grofe pack: {e}")))
}

fn write_lock(lock: &Lock, path: &Path) -> i32 {
    match lock.write(path) {
        Ok(()) => 0,
        Err(e) => fail(&format!("grofe pack: {e}")),
    }
}

/// Prints each outcome; whether any failed.
fn report(outcomes: &[Outcome]) -> bool {
    let mut failed = false;
    for o in outcomes {
        match &o.status {
            Status::Installed => println!("installed {}", o.name),
            Status::Unchanged => println!("unchanged {}", o.name),
            Status::Updated => println!("updated {}", o.name),
            Status::Failed(e) => {
                failed = true;
                eprintln!("{}: {e}", o.name);
            }
        }
    }
    failed
}

fn sync(specs: &[Spec], paths: &Paths) -> i32 {
    let mut lock = match read_lock(&paths.lock) {
        Ok(l) => l,
        Err(c) => return c,
    };
    let failed = report(&git::sync(specs, &mut lock, &paths.root));
    let code = write_lock(&lock, &paths.lock);
    if code == 0 {
        println!("{RELOAD}");
    }
    if failed { 1 } else { code }
}

fn update(
    specs: &[Spec],
    args: &[String],
    paths: &Paths,
    tty: bool,
    input: &mut dyn BufRead,
) -> i32 {
    let yes = args.iter().any(|a| a == "--yes");
    let names: Vec<&String> = args.iter().filter(|a| *a != "--yes").collect();
    if names.len() > 1 || names.iter().any(|n| n.starts_with('-')) {
        return fail(USAGE);
    }
    if !yes && !tty {
        return fail("grofe pack update: not a terminal, pass --yes to apply the changes");
    }
    let mut lock = match read_lock(&paths.lock) {
        Ok(l) => l,
        Err(c) => return c,
    };
    let changes = git::pending(specs, &lock, &paths.root, names.first().map(|n| n.as_str()));
    if changes.is_empty() {
        println!("nothing to update");
        return 0;
    }
    for c in &changes {
        println!("{}: {} -> {}", c.name, c.from, c.to);
        for l in &c.log {
            println!("  {l}");
        }
    }
    if !yes {
        print!("apply? [y/N] ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut answer = String::new();
        if input.read_line(&mut answer).is_err() || !answer.trim().eq_ignore_ascii_case("y") {
            println!("aborted");
            return 1;
        }
    }
    let failed = report(&git::apply(&changes, &mut lock, &paths.root));
    let code = write_lock(&lock, &paths.lock);
    if code == 0 {
        println!("{RELOAD}");
    }
    if failed { 1 } else { code }
}

fn list(specs: &[Spec], paths: &Paths) -> i32 {
    let lock = match read_lock(&paths.lock) {
        Ok(l) => l,
        Err(c) => return c,
    };
    for s in specs {
        let state = match lock.plugins.get(&s.name) {
            Some(l) => l.rev.chars().take(8).collect::<String>(),
            None => "not locked".to_string(),
        };
        let off = if s.enabled { "" } else { " (disabled)" };
        println!("{}\t{}\t{}{off}", s.name, state, s.src);
    }
    0
}

fn clean(specs: &[Spec], paths: &Paths) -> i32 {
    for name in git::clean(specs, &paths.root) {
        println!("removed {name}");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn paths() -> Paths {
        Paths {
            lock: PathBuf::from("/nonexistent/pack-lock.toml"),
            root: PathBuf::from("/nonexistent/pack"),
        }
    }

    #[test]
    fn test_update_without_tty_needs_yes() {
        let code = execute(&args(&["update"]), &[], &paths(), false, &mut &b""[..]);
        assert_eq!(code, 1);
    }

    #[test]
    fn test_unknown_subcommand_and_extra_args_fail() {
        for a in [
            &["bogus"][..],
            &[][..],
            &["sync", "x"][..],
            &["clean", "x"][..],
        ] {
            assert_eq!(execute(&args(a), &[], &paths(), true, &mut &b""[..]), 1);
        }
    }

    #[test]
    fn test_update_rejects_unknown_flag_and_two_names() {
        for a in [&["update", "--bogus"][..], &["update", "a", "b"][..]] {
            assert_eq!(execute(&args(a), &[], &paths(), true, &mut &b""[..]), 1);
        }
    }
}
