//! Winning-source provenance per option (spec §5.4): which layer and file
//! set each resolved value.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A config layer (spec §5.1), in load order. Later layers win.
// ponytail: layer 5 (`init.lua`) is T6.4's and layer 6 (runtime
// `set-option`) is T3.5's; neither is a file layer T3.1 loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    /// Compiled-in defaults.
    Defaults,
    /// `config.toml`, or the `--file` layers that replace it (R5).
    Config,
    /// `conf.d/*.toml`, in lexical order.
    ConfD,
    /// `user.toml`, the user's final word.
    User,
}

/// Where a resolved value came from. An included file (`source-file`)
/// keeps the layer of the file that included it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub layer: Layer,
    /// `None` for the built-in defaults.
    pub file: Option<PathBuf>,
    /// 1-based line of the key; 0 for the built-in defaults.
    pub line: usize,
}

impl Source {
    pub fn defaults() -> Source {
        Source {
            layer: Layer::Defaults,
            file: None,
            line: 0,
        }
    }

    /// The source as `show-config --explain` prints it: the file with
    /// `$HOME` written as `~`, or `built-in defaults` (R8).
    pub fn describe(&self, home: Option<&Path>) -> String {
        match &self.file {
            None => "built-in defaults".to_string(),
            Some(file) => tilde(file, home),
        }
    }
}

/// `path` with a leading `home` written as `~`.
pub fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The winning source of every resolved option, by TOML key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    options: BTreeMap<String, Source>,
}

impl Provenance {
    pub fn get(&self, toml_key: &str) -> Option<&Source> {
        self.options.get(toml_key)
    }

    pub fn set(&mut self, toml_key: &str, source: Source) {
        self.options.insert(toml_key.to_string(), source);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Source)> {
        self.options.iter().map(|(k, s)| (k.as_str(), s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_contracts_only_under_home() {
        let home = Path::new("/h/me");
        assert_eq!(
            tilde(Path::new("/h/me/.config/x"), Some(home)),
            "~/.config/x"
        );
        assert_eq!(tilde(Path::new("/h/meow/x"), Some(home)), "/h/meow/x");
        assert_eq!(tilde(Path::new("/h/me/x"), None), "/h/me/x");
    }
}
