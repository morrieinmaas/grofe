use crate::pack::lock::Lock;
use crate::pack::manifest::{Kind, detect};
use crate::pack::spec::{Spec, pack_root};
use crate::server::State;
use std::path::{Path, PathBuf};

/// Loads every enabled, locked and installed pack in name order. A failing
/// pack goes to the log and never stops the others.
pub fn load_all(state: &mut State, specs: &[Spec], config_dir: &Path) {
    let lock = match Lock::read(&config_dir.join("pack-lock.toml")) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("grofe pack: {e}");
            return;
        }
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let xdg = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    let root = pack_root(&home, xdg.as_deref());
    for (name, dir) in selected(specs, &lock, &root) {
        match detect(&dir) {
            Ok(Kind::Luau) => {
                if let Err(e) = crate::server::lua_host::load_pack(state, &name, &dir) {
                    eprintln!("grofe pack {name}: {e}");
                }
            }
            Ok(Kind::Tmux) => {
                crate::server::hooks::run_at_load(state, tmux_cmds(&dir));
            }
            Err(e) => eprintln!("grofe pack {name}: {e}"),
        }
    }
}

/// The enabled specs that are locked and cloned under `root`, by name.
fn selected(specs: &[Spec], lock: &Lock, root: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = specs
        .iter()
        .filter(|s| s.enabled && lock.plugins.contains_key(&s.name))
        .map(|s| (s.name.clone(), root.join(&s.name)))
        .filter(|(_, dir)| dir.is_dir())
        .collect();
    out.sort();
    out
}

/// One `run-shell` per `*.tmux` file in `dir`, sorted by file name.
fn tmux_cmds(dir: &Path) -> Vec<Vec<String>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "tmux"))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|p| vec!["run-shell".to_string(), p.to_string_lossy().into_owned()])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::lock::Locked;

    fn spec(name: &str, enabled: bool) -> Spec {
        Spec {
            name: name.into(),
            src: format!("https://x/{name}"),
            rev: None,
            enabled,
        }
    }

    #[test]
    fn test_only_locked_installed_enabled_load() {
        let root = tempfile::tempdir().unwrap();
        for n in ["b", "a", "off", "unlocked"] {
            std::fs::create_dir(root.path().join(n)).unwrap();
        }
        let mut lock = Lock::default();
        for n in ["a", "b", "off", "missing"] {
            lock.plugins.insert(n.into(), Locked::default());
        }
        let specs = [
            spec("b", true),
            spec("a", true),
            spec("off", false),
            spec("unlocked", true),
            spec("missing", true),
        ];
        let got: Vec<String> = selected(&specs, &lock, root.path())
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(got, ["a", "b"]);
    }

    #[test]
    fn test_tmux_pack_queues_run_shell_sorted() {
        let d = tempfile::tempdir().unwrap();
        for f in ["z.tmux", "a.tmux", "readme.md"] {
            std::fs::write(d.path().join(f), "").unwrap();
        }
        let cmds = tmux_cmds(d.path());
        let want: Vec<Vec<String>> = ["a.tmux", "z.tmux"]
            .iter()
            .map(|f| {
                vec![
                    "run-shell".into(),
                    d.path().join(f).to_string_lossy().into_owned(),
                ]
            })
            .collect();
        assert_eq!(cmds, want);
    }
}
