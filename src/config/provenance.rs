//! Winning-source provenance per option (spec §5.4): which layer and file
//! set each resolved value.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A config layer (spec §5.1), in load order. Later layers win.
// ponytail: layer 5 (runtime `set-option`) is T3.5's; it is not a file
// layer T3.1 loads. There is no scripting layer (spec §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    /// Compiled-in defaults.
    Defaults,
    /// `config.toml`, or the `--file` layers that replace it (R5).
    Config,
    /// `conf.d/*.toml`, in lexical order.
    ConfD,
    /// `user.toml`, the last file the user writes by hand.
    User,
    /// `settings.toml`, written by grofe itself; the last word.
    Settings,
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
    /// The highest non-default source each winner replaced.
    replaced: BTreeMap<String, Source>,
}

impl Provenance {
    pub fn get(&self, toml_key: &str) -> Option<&Source> {
        self.options.get(toml_key)
    }

    pub fn set(&mut self, toml_key: &str, source: Source) {
        if let Some(old) = self.options.insert(toml_key.to_string(), source)
            && old.layer != Layer::Defaults
        {
            self.replaced.insert(toml_key.to_string(), old);
        }
    }

    /// The source `settings.toml` replaced for `toml_key`, if the winner is
    /// a settings value that hides a non-default lower one.
    pub fn overridden(&self, toml_key: &str) -> Option<&Source> {
        match self.options.get(toml_key) {
            Some(w) if w.layer == Layer::Settings => self.replaced.get(toml_key),
            _ => None,
        }
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

    fn src(layer: Layer, line: usize) -> Source {
        Source {
            layer,
            file: Some(PathBuf::from("f.toml")),
            line,
        }
    }

    #[test]
    fn overridden_reports_the_replaced_source_under_settings() {
        let mut p = Provenance::default();
        p.set("k", Source::defaults());
        p.set("k", src(Layer::ConfD, 7));
        p.set("k", src(Layer::Settings, 2));
        assert_eq!(p.overridden("k"), Some(&src(Layer::ConfD, 7)));
    }

    #[test]
    fn overridden_is_none_without_a_replaced_source_or_settings_winner() {
        let mut p = Provenance::default();
        p.set("a", Source::defaults());
        p.set("a", src(Layer::Settings, 1));
        assert_eq!(p.overridden("a"), None);
        p.set("b", Source::defaults());
        p.set("b", src(Layer::ConfD, 3));
        p.set("b", src(Layer::User, 4));
        assert_eq!(p.overridden("b"), None);
    }
}
