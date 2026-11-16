use std::path::Path;
use std::process::Command;

use super::{
    lock::{Lock, Locked},
    manifest,
    spec::Spec,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Installed,
    Unchanged,
    Updated,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub name: String,
    pub status: Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub name: String,
    pub from: String,
    pub to: String,
    pub log: Vec<String>,
}

fn git(dir: Option<&Path>, args: &[&str]) -> Result<String, String> {
    let mut c = Command::new("git");
    if let Some(d) = dir {
        c.arg("-C").arg(d);
    }
    let out = c
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!("git {}: {}", args.join(" "), err.trim()))
    }
}

fn safe_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
}

fn head(dir: &Path) -> Result<String, String> {
    git(Some(dir), &["rev-parse", "HEAD"])
}

fn resolve(dir: &Path, rev: &str) -> Result<String, String> {
    let direct = format!("{rev}^{{commit}}");
    let remote = format!("origin/{rev}^{{commit}}");
    git(Some(dir), &["rev-parse", "--verify", "--quiet", &remote])
        .or_else(|_| git(Some(dir), &["rev-parse", "--verify", "--quiet", &direct]))
        .map_err(|_| format!("unknown revision {rev}"))
}

fn checkout(dir: &Path, rev: &str) -> Result<(), String> {
    git(Some(dir), &["checkout", "--quiet", "--detach", rev]).map(|_| ())
}

fn sync_one(spec: &Spec, lock: &mut Lock, root: &Path) -> Result<Status, String> {
    if !safe_name(&spec.name) {
        return Err(format!("unsafe plugin name {:?}", spec.name));
    }
    let dir = root.join(&spec.name);
    let cloned = !dir.exists();
    if cloned {
        std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
        git(
            None,
            &["clone", "--quiet", &spec.src, &dir.to_string_lossy()],
        )?;
    }
    let locked = lock.plugins.get(&spec.name).filter(|l| l.src == spec.src);
    let before = head(&dir)?;
    let target = match locked {
        Some(l) => {
            if git(
                Some(&dir),
                &["cat-file", "-e", &format!("{}^{{commit}}", l.rev)],
            )
            .is_err()
            {
                git(Some(&dir), &["fetch", "--quiet", "--tags", "origin"])?;
            }
            l.rev.clone()
        }
        None => match &spec.rev {
            Some(r) => resolve(&dir, r)?,
            None => before.clone(),
        },
    };
    if target != before {
        checkout(&dir, &target)?;
    }
    manifest::detect(&dir)?;
    let now = head(&dir)?;
    lock.plugins.insert(
        spec.name.clone(),
        Locked {
            src: spec.src.clone(),
            rev: now.clone(),
        },
    );
    Ok(if cloned {
        Status::Installed
    } else if now == before {
        Status::Unchanged
    } else {
        Status::Updated
    })
}

pub fn sync(specs: &[Spec], lock: &mut Lock, root: &Path) -> Vec<Outcome> {
    specs
        .iter()
        .filter(|s| s.enabled)
        .map(|s| Outcome {
            name: s.name.clone(),
            status: sync_one(s, lock, root).unwrap_or_else(Status::Failed),
        })
        .collect()
}

fn pending_one(spec: &Spec, locked: &Locked, root: &Path) -> Result<Option<Change>, String> {
    let dir = root.join(&spec.name);
    git(Some(&dir), &["fetch", "--quiet", "--tags", "origin"])?;
    let to = match &spec.rev {
        Some(r) => resolve(&dir, r)?,
        None => resolve(&dir, "HEAD")
            .or_else(|_| git(Some(&dir), &["rev-parse", "origin/HEAD^{commit}"]))?,
    };
    if to == locked.rev {
        return Ok(None);
    }
    let log = git(
        Some(&dir),
        &["log", "--oneline", &format!("{}..{}", locked.rev, to)],
    )
    .unwrap_or_default()
    .lines()
    .map(str::to_string)
    .collect();
    Ok(Some(Change {
        name: spec.name.clone(),
        from: locked.rev.clone(),
        to,
        log,
    }))
}

pub fn pending(specs: &[Spec], lock: &Lock, root: &Path, only: Option<&str>) -> Vec<Change> {
    specs
        .iter()
        .filter(|s| s.enabled && only.is_none_or(|o| o == s.name) && safe_name(&s.name))
        .filter_map(|s| {
            let locked = lock.plugins.get(&s.name)?;
            pending_one(s, locked, root).ok().flatten()
        })
        .collect()
}

pub fn apply(changes: &[Change], lock: &mut Lock, root: &Path) -> Vec<Outcome> {
    changes
        .iter()
        .map(|c| {
            let res = if safe_name(&c.name) {
                checkout(&root.join(&c.name), &c.to)
            } else {
                Err(format!("unsafe plugin name {:?}", c.name))
            };
            let status = match res {
                Ok(()) => {
                    if let Some(l) = lock.plugins.get_mut(&c.name) {
                        l.rev = c.to.clone();
                    }
                    Status::Updated
                }
                Err(e) => Status::Failed(e),
            };
            Outcome {
                name: c.name.clone(),
                status,
            }
        })
        .collect()
}

/// Removes entries of `root` that no spec names. Symlinks are unlinked, never followed.
pub fn clean(specs: &[Spec], root: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if specs.iter().any(|s| s.name == name) {
            continue;
        }
        let path = e.path();
        let is_real_dir = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
        let ok = if is_real_dir {
            std::fs::remove_dir_all(&path).is_ok()
        } else {
            std::fs::remove_file(&path).is_ok()
        };
        if ok {
            removed.push(name);
        }
    }
    removed.sort();
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn g(dir: &Path, args: &[&str]) -> String {
        let mut a = vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ];
        a.extend_from_slice(args);
        git(Some(dir), &a).unwrap()
    }

    /// A bare upstream with one commit holding a valid manifest; returns (bare path, work dir).
    fn upstream(tmp: &Path, api: u32) -> (PathBuf, PathBuf) {
        let bare = tmp.join("up.git");
        let work = tmp.join("work");
        fs::create_dir_all(&work).unwrap();
        git(
            None,
            &[
                "init",
                "--quiet",
                "--bare",
                "-b",
                "main",
                &bare.to_string_lossy(),
            ],
        )
        .unwrap();
        g(&work, &["init", "--quiet", "-b", "main"]);
        fs::write(
            work.join("grofe-plugin.toml"),
            format!("name = \"p\"\napi = {api}\n"),
        )
        .unwrap();
        fs::write(work.join("init.luau"), "").unwrap();
        commit(&work, "one");
        g(&work, &["push", "--quiet", &bare.to_string_lossy(), "main"]);
        (bare, work)
    }

    fn commit(work: &Path, msg: &str) {
        fs::write(work.join("f"), msg).unwrap();
        g(work, &["add", "-A"]);
        g(work, &["commit", "--quiet", "--no-verify", "-m", msg]);
    }

    fn spec(bare: &Path) -> Spec {
        Spec {
            name: "p".into(),
            src: bare.to_string_lossy().into(),
            rev: None,
            enabled: true,
        }
    }

    #[test]
    fn test_sync_installs_and_locks_head() {
        let t = tempfile::tempdir().unwrap();
        let (bare, work) = upstream(t.path(), 1);
        let root = t.path().join("root");
        let mut lock = Lock::default();
        let out = sync(&[spec(&bare)], &mut lock, &root);
        assert_eq!(
            out,
            vec![Outcome {
                name: "p".into(),
                status: Status::Installed
            }]
        );
        assert_eq!(lock.plugins["p"].rev, g(&work, &["rev-parse", "HEAD"]));
    }

    #[test]
    fn test_sync_checks_out_locked_rev() {
        let t = tempfile::tempdir().unwrap();
        let (bare, work) = upstream(t.path(), 1);
        let root = t.path().join("root");
        let mut lock = Lock::default();
        sync(&[spec(&bare)], &mut lock, &root);
        let locked = lock.plugins["p"].rev.clone();
        commit(&work, "two");
        g(&work, &["push", "--quiet", &bare.to_string_lossy(), "main"]);
        let out = sync(&[spec(&bare)], &mut lock, &root);
        assert_eq!(out[0].status, Status::Unchanged);
        assert_eq!(head(&root.join("p")).unwrap(), locked);
        assert_eq!(lock.plugins["p"].rev, locked);
    }

    #[test]
    fn test_api_mismatch_fails_sync() {
        let t = tempfile::tempdir().unwrap();
        let (bare, _work) = upstream(t.path(), 2);
        let mut lock = Lock::default();
        let out = sync(&[spec(&bare)], &mut lock, &t.path().join("root"));
        assert!(
            matches!(&out[0].status, Status::Failed(m) if m.contains("API 2")),
            "{out:?}"
        );
        assert!(lock.plugins.is_empty());
    }

    #[test]
    fn test_pending_and_apply_move_the_lock() {
        let t = tempfile::tempdir().unwrap();
        let (bare, work) = upstream(t.path(), 1);
        let root = t.path().join("root");
        let mut lock = Lock::default();
        let specs = [spec(&bare)];
        sync(&specs, &mut lock, &root);
        assert!(pending(&specs, &lock, &root, None).is_empty());
        commit(&work, "two");
        g(&work, &["push", "--quiet", &bare.to_string_lossy(), "main"]);
        let ch = pending(&specs, &lock, &root, Some("p"));
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].log.len(), 1);
        assert!(ch[0].log[0].contains("two"));
        let out = apply(&ch, &mut lock, &root);
        assert_eq!(out[0].status, Status::Updated);
        assert_eq!(lock.plugins["p"].rev, ch[0].to);
        assert_eq!(head(&root.join("p")).unwrap(), ch[0].to);
    }

    #[test]
    fn test_sync_rejects_traversal_name() {
        let t = tempfile::tempdir().unwrap();
        let mut s = spec(Path::new("/nonexistent"));
        s.name = "../evil".into();
        let out = sync(&[s], &mut Lock::default(), &t.path().join("root"));
        assert!(matches!(out[0].status, Status::Failed(_)));
    }

    #[cfg(unix)]
    #[test]
    fn test_clean_never_leaves_root() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("root");
        let outside = t.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        fs::create_dir_all(root.join("stale/sub")).unwrap();
        fs::create_dir_all(root.join("p")).unwrap();
        let removed = clean(&[spec(Path::new("x"))], &root);
        assert_eq!(removed, vec!["link".to_string(), "stale".to_string()]);
        assert!(outside.join("keep").exists());
        assert!(root.join("p").exists());
        assert!(!root.join("link").exists());
    }
}
