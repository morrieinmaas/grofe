//! `[[pack]]` (the plugin manager's specs): `src` is required; `name`,
//! `rev` and `enabled` are optional. Specs from every layer are collected
//! in order, and a later spec of the same name replaces an earlier one.

use toml::de::{DeTable, DeValue};

use super::{Loader, closest, entries};
use crate::pack::spec::{Spec, name_of, normalize_src};

use super::FileCtx;

impl Loader {
    pub(super) fn pack(&mut self, f: &FileCtx, line: usize, v: &DeValue) {
        let DeValue::Array(items) = v else {
            let msg = format!("expected an array of tables, found {}", v.type_str());
            return self.error(f.path, line, "pack", msg);
        };
        for item in items {
            let at = f.line(item.span().start);
            match item.get_ref() {
                DeValue::Table(t) => self.pack_spec(f, at, t),
                other => {
                    let msg = format!("expected a table, found {}", other.type_str());
                    self.error(f.path, at, "pack", msg);
                }
            }
        }
    }

    fn pack_spec(&mut self, f: &FileCtx, line: usize, t: &DeTable) {
        let (mut src, mut name, mut rev, mut enabled) = (None, None, None, true);
        let mut src_line = line;
        let mut failed = false;
        for (k, v) in entries(t) {
            let field = k.get_ref().as_ref();
            let at = f.line(k.span().start);
            let key = format!("pack.{field}");
            let result: Result<(), String> = match (field, v.get_ref()) {
                ("src", DeValue::String(s)) => {
                    src_line = at;
                    normalize_src(s).map(|s| src = Some(s))
                }
                ("name", DeValue::String(s)) => valid_name(s).map(|()| name = Some(s.to_string())),
                ("rev", DeValue::String(s)) if !s.is_empty() => {
                    rev = Some(s.to_string());
                    Ok(())
                }
                ("enabled", DeValue::Boolean(b)) => {
                    enabled = *b;
                    Ok(())
                }
                ("src" | "name" | "rev" | "enabled", other) => {
                    Err(format!("wrong type, found {}", other.type_str()))
                }
                (other, _) => {
                    let mut msg = "unknown key".to_string();
                    if let Some(s) = closest(other, ["src", "name", "rev", "enabled"]) {
                        msg.push_str(&format!(", did you mean {s}?"));
                    }
                    Err(msg)
                }
            };
            if let Err(msg) = result {
                failed = true;
                self.error(f.path, at, &key, msg);
            }
        }
        let Some(src) = src else {
            if !failed {
                self.error(f.path, line, "pack.src", "missing src".to_string());
            }
            return;
        };
        let name = name.unwrap_or_else(|| name_of(&src));
        if let Err(msg) = valid_name(&name) {
            return self.error(f.path, src_line, "pack.src", msg);
        }
        let spec = Spec {
            name,
            src,
            rev,
            enabled,
        };
        match self.pack.iter_mut().find(|s| s.name == spec.name) {
            Some(old) => *old = spec,
            None => self.pack.push(spec),
        }
    }
}

/// A name is one path component: it becomes a directory under the pack root.
fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err(format!("{name:?} is not a usable plugin name"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::config::{Env, load::load_one};

    fn load(text: &str) -> Result<Vec<crate::pack::spec::Spec>, Vec<String>> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        let env = Env::from_vars(None, None);
        load_one(&path, &env)
            .map(|r| r.pack)
            .map_err(|e| e.iter().map(|e| e.to_string()).collect())
    }

    #[test]
    fn test_pack_table_parses() {
        let specs = load("[[pack]]\nsrc = \"github:a/b\"\n").unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "b");
        assert_eq!(specs[0].src, "https://github.com/a/b");
        assert!(specs[0].enabled);
        assert_eq!(specs[0].rev, None);
    }

    #[test]
    fn test_pack_options_parse() {
        let text = "[[pack]]\nsrc = \"github:a/b\"\nname = \"x\"\nrev = \"v1\"\nenabled = false\n";
        let s = &load(text).unwrap()[0];
        assert_eq!(
            (s.name.as_str(), s.rev.as_deref(), s.enabled),
            ("x", Some("v1"), false)
        );
    }

    #[test]
    fn test_pack_bad_src_errors_with_line() {
        let errs = load("\n\n[[pack]]\nsrc = \"ftp://x\"\n").unwrap_err();
        assert!(errs[0].contains("config.toml:4: pack.src"), "{errs:?}");
    }

    #[test]
    fn test_pack_missing_src_errors() {
        let errs = load("\n[[pack]]\nname = \"x\"\n").unwrap_err();
        assert!(
            errs[0].contains("config.toml:2: pack.src: missing src"),
            "{errs:?}"
        );
    }

    #[test]
    fn test_pack_must_be_an_array_of_tables() {
        let errs = load("pack = \"x\"\n").unwrap_err();
        assert!(errs[0].contains("config.toml:1: pack"), "{errs:?}");
    }

    #[test]
    fn test_pack_unknown_key_and_wrong_type() {
        let errs = load("[[pack]]\nsrcc = \"x\"\nenabled = 1\n").unwrap_err();
        assert!(errs[0].contains("did you mean src"), "{errs:?}");
        assert!(errs[1].contains("config.toml:3: pack.enabled"), "{errs:?}");
    }

    #[test]
    fn test_pack_name_rejects_path_traversal() {
        let errs = load("[[pack]]\nsrc = \"x\"\nname = \"../evil\"\n").unwrap_err();
        assert!(errs.iter().any(|e| e.contains("pack.name")), "{errs:?}");
    }
}
