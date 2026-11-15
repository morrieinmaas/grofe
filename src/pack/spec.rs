use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub name: String,
    pub src: String,
    pub rev: Option<String>,
    pub enabled: bool,
}

/// Turn a user-written source into a clone URL or path, rejecting anything else.
pub fn normalize_src(s: &str) -> Result<String, String> {
    let body = s.strip_prefix("github:").unwrap_or(s);
    if body.split(['/', '\\']).any(|seg| seg == "..") {
        return Err(format!("pack source '{s}' contains '..'"));
    }
    if let Some(rest) = s.strip_prefix("github:") {
        let mut parts = rest.split('/');
        return match (parts.next(), parts.next(), parts.next()) {
            (Some(o), Some(r), None) if !o.is_empty() && !r.is_empty() => {
                Ok(format!("https://github.com/{o}/{r}"))
            }
            _ => Err(format!("pack source '{s}' must be github:owner/repo")),
        };
    }
    let ok = s.starts_with("https://") || s.starts_with("file://") || s.starts_with('/');
    if !ok {
        return Err(format!(
            "pack source '{s}' must be github:owner/repo, https://, file:// or an absolute path"
        ));
    }
    if !valid_name(&name_of(s)) {
        return Err(format!("pack source '{s}' has no usable name"));
    }
    Ok(s.to_string())
}

/// A name is one path component: no separators, not empty, not `.` or `..`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
}

/// The last path segment of a source, without a `.git` suffix.
pub fn name_of(src: &str) -> String {
    let last = src.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    last.strip_suffix(".git").unwrap_or(last).to_string()
}

pub fn pack_root(home: &Path, xdg_data_home: Option<&Path>) -> PathBuf {
    let data = match xdg_data_home {
        Some(x) => x.to_path_buf(),
        None => home.join(".local").join("share"),
    };
    data.join("grofe").join("pack")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_forms() {
        assert_eq!(
            normalize_src("github:x/grofe-git").unwrap(),
            "https://github.com/x/grofe-git"
        );
        assert!(normalize_src("https://h/x/y.git").is_ok());
        assert!(normalize_src("file:///tmp/r").is_ok());
        assert!(normalize_src("/tmp/r").is_ok());
        assert!(normalize_src("ftp://x/y").is_err());
        assert!(normalize_src("rel/path").is_err());
        assert!(normalize_src("github:x").is_err());
    }

    #[test]
    fn test_name_of() {
        assert_eq!(name_of("https://github.com/x/tmux-yank.git"), "tmux-yank");
        assert_eq!(name_of("/tmp/r/"), "r");
    }

    #[test]
    fn test_name_rejects_path_traversal() {
        assert_eq!(name_of("https://x/.."), "..");
        assert!(normalize_src("https://x/..").is_err());
        assert!(normalize_src("/a/../b").is_err());
        assert!(normalize_src("github:../x").is_err());
        assert!(!valid_name("a/b"));
        assert!(!valid_name(".."));
        assert!(!valid_name("."));
        assert!(!valid_name(""));
        assert!(valid_name("tmux-yank"));
    }

    #[test]
    fn test_pack_root() {
        let h = Path::new("/h");
        assert_eq!(pack_root(h, None), Path::new("/h/.local/share/grofe/pack"));
        assert_eq!(
            pack_root(h, Some(Path::new("/x"))),
            Path::new("/x/grofe/pack")
        );
    }
}
