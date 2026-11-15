use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Luau,
    Tmux,
}

#[derive(Deserialize)]
struct Manifest {
    name: String,
    api: u32,
}

/// Classify a plugin directory: `grofe-plugin.toml` wins over any `*.tmux` file.
pub fn detect(dir: &Path) -> Result<Kind, String> {
    let path = dir.join("grofe-plugin.toml");
    if path.is_file() {
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let m: Manifest = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if m.api != super::PLUGIN_API {
            return Err(format!(
                "{} needs grofe plugin API {}, this grofe has {}",
                m.name,
                m.api,
                super::PLUGIN_API
            ));
        }
        if !dir.join("init.luau").is_file() && !dir.join("init.lua").is_file() {
            return Err(format!("{}: no init.luau", m.name));
        }
        return Ok(Kind::Luau);
    }
    let has_tmux = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .any(|e| e.path().extension().is_some_and(|x| x == "tmux") && e.path().is_file());
    if has_tmux {
        Ok(Kind::Tmux)
    } else {
        Err("not a grofe or tmux plugin".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (n, c) in files {
            std::fs::write(d.path().join(n), c).unwrap();
        }
        d
    }

    #[test]
    fn test_manifest_wins_over_tmux_file() {
        let d = dir(&[
            ("grofe-plugin.toml", "name = \"p\"\napi = 1\n"),
            ("init.luau", "return {}"),
            ("yank.tmux", ""),
        ]);
        assert_eq!(detect(d.path()), Ok(Kind::Luau));
    }

    #[test]
    fn test_bad_manifest_toml_names_the_file() {
        let d = dir(&[("grofe-plugin.toml", "name = ")]);
        let e = detect(d.path()).unwrap_err();
        assert!(e.contains("grofe-plugin.toml"), "{e}");
    }

    #[test]
    fn test_api_mismatch() {
        let d = dir(&[("grofe-plugin.toml", "name = \"p\"\napi = 2\n")]);
        assert_eq!(
            detect(d.path()),
            Err("p needs grofe plugin API 2, this grofe has 1".into())
        );
    }

    #[test]
    fn test_tmux_file_is_tmux() {
        let d = dir(&[("yank.tmux", "")]);
        assert_eq!(detect(d.path()), Ok(Kind::Tmux));
    }

    #[test]
    fn test_empty_dir_is_not_a_plugin() {
        let d = dir(&[]);
        assert_eq!(detect(d.path()), Err("not a grofe or tmux plugin".into()));
    }

    #[test]
    fn test_luau_needs_init() {
        let d = dir(&[("grofe-plugin.toml", "name = \"p\"\napi = 1\n")]);
        assert_eq!(detect(d.path()), Err("p: no init.luau".into()));
        std::fs::write(d.path().join("init.lua"), "").unwrap();
        assert_eq!(detect(d.path()), Ok(Kind::Luau));
    }
}
