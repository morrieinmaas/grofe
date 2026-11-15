use std::collections::BTreeMap;
use std::path::Path;

use super::spec::valid_name;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Locked {
    pub src: String,
    pub rev: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Lock {
    pub plugins: BTreeMap<String, Locked>,
}

impl Lock {
    /// A missing file is an empty lock.
    pub fn read(path: &Path) -> Result<Lock, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Lock::default()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let doc: toml::Table = text
            .parse()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let mut lock = Lock::default();
        let Some(plugins) = doc.get("plugins") else {
            return Ok(lock);
        };
        let plugins = plugins
            .as_table()
            .ok_or_else(|| format!("{}: 'plugins' must be a table", path.display()))?;
        for (name, v) in plugins {
            if !valid_name(name) {
                return Err(format!("{}: bad plugin name '{name}'", path.display()));
            }
            let field = |k: &str| {
                v.get(k)
                    .and_then(|x| x.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        format!("{}: plugin '{name}' needs a string '{k}'", path.display())
                    })
            };
            lock.plugins.insert(
                name.clone(),
                Locked {
                    src: field("src")?,
                    rev: field("rev")?,
                },
            );
        }
        Ok(lock)
    }

    /// Write sorted by name through a temp file in the same dir, then rename.
    pub fn write(&self, path: &Path) -> Result<(), String> {
        let mut plugins = toml::Table::new();
        for (name, l) in &self.plugins {
            let mut t = toml::Table::new();
            t.insert("src".into(), l.src.clone().into());
            t.insert("rev".into(), l.rev.clone().into());
            plugins.insert(name.clone(), t.into());
        }
        let mut doc = toml::Table::new();
        doc.insert("plugins".into(), plugins.into());
        let text = toml::to_string(&doc).map_err(|e| e.to_string())?;

        let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
        let dir = dir.unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let file = path.file_name().and_then(|f| f.to_str()).unwrap_or("lock");
        let tmp = dir.join(format!(".{file}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", path.display())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locked(src: &str, rev: &str) -> Locked {
        Locked {
            src: src.into(),
            rev: rev.into(),
        }
    }

    #[test]
    fn test_lock_missing_is_empty() {
        let d = tempfile::tempdir().unwrap();
        let l = Lock::read(&d.path().join("pack-lock.toml")).unwrap();
        assert!(l.plugins.is_empty());
    }

    #[test]
    fn test_lock_write_is_atomic_and_sorted() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("pack-lock.toml");
        let mut l = Lock::default();
        l.plugins.insert("zed".into(), locked("/z", "bbb"));
        l.plugins
            .insert("alpha".into(), locked("https://h/a", "aaa"));
        l.write(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            text.find("alpha").unwrap() < text.find("zed").unwrap(),
            "{text}"
        );
        assert_eq!(Lock::read(&p).unwrap(), l);
        // no temp file left behind
        let names: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        // an overwrite replaces the content whole
        l.plugins.remove("zed");
        l.write(&p).unwrap();
        assert_eq!(Lock::read(&p).unwrap(), l);
    }

    #[test]
    fn test_lock_rejects_bad_name_and_missing_field() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("l.toml");
        std::fs::write(&p, "[plugins.\"a/b\"]\nsrc=\"/x\"\nrev=\"1\"\n").unwrap();
        assert!(Lock::read(&p).is_err());
        std::fs::write(&p, "[plugins.a]\nsrc=\"/x\"\n").unwrap();
        assert!(Lock::read(&p).is_err());
        std::fs::write(&p, "not toml [").unwrap();
        assert!(Lock::read(&p).is_err());
    }
}
