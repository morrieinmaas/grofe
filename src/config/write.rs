//! Write-back to `settings.toml` (spec 2026-11-20-settings-menu, PR 2): the
//! file grofe itself edits, loaded after every other layer. Each call
//! re-reads the file, changes one key with `toml_edit` so comments, blank
//! lines and other keys stay byte for byte, and replaces the file by a
//! temp file plus rename.

use std::fs;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, value};

/// The file name inside the config folder.
pub const FILE: &str = "settings.toml";

/// What a write replaced, for [`Saved::rollback`].
#[derive(Debug)]
pub struct Saved {
    path: PathBuf,
    /// `None`: the file did not exist.
    old: Option<Vec<u8>>,
}

impl Saved {
    /// Puts the old bytes back, or removes a file the write created.
    pub fn rollback(self) -> Result<(), String> {
        match self.old {
            Some(bytes) => write_atomic(&self.path, &bytes),
            None => match fs::remove_file(&self.path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    Err(format!("{}: {e}", self.path.display()))
                }
                _ => Ok(()),
            },
        }
    }
}

/// Sets option `key` (a TOML key, `status.lines` for a nested one) to `item`.
pub fn set(dir: &Path, key: &str, item: Item) -> Result<Saved, String> {
    if key.starts_with('@') || key.is_empty() {
        return Err(format!("{key}: not saveable, it has no TOML key"));
    }
    edit(dir, |doc| {
        let parts: Vec<&str> = key.split('.').collect();
        let (last, parents) = parts.split_last().ok_or("empty key")?;
        let mut table = doc.as_table_mut();
        for p in parents {
            if !table.contains_key(p) {
                let mut t = Table::new();
                t.set_dotted(true);
                table.insert(p, Item::Table(t));
            }
            table = table
                .get_mut(p)
                .and_then(Item::as_table_mut)
                .ok_or_else(|| format!("{key}: {p} in settings.toml is not a table"))?;
        }
        put(table, last, item);
        Ok(true)
    })
}

/// Removes `key` from settings.toml, so the lower layers apply again. A key
/// that is not there is not an error.
pub fn unset(dir: &Path, key: &str) -> Result<Saved, String> {
    edit(dir, |doc| {
        let parts: Vec<&str> = key.split('.').collect();
        Ok(remove(doc.as_table_mut(), &parts))
    })
}

/// Binds `key` in `[keys.<table>]` to `cmd`; an empty `cmd` unbinds it
/// (`key = ""`).
pub fn set_binding(dir: &Path, table: &str, key: &str, cmd: &str) -> Result<Saved, String> {
    edit(dir, |doc| {
        let root = doc.as_table_mut();
        let keys = sub_table(root, "keys", true)?;
        let t = sub_table(keys, table, false)?;
        let item = if cmd.is_empty() {
            value("")
        } else {
            let mut inline = toml_edit::InlineTable::new();
            inline.insert("cmd", cmd.into());
            value(inline)
        };
        put(t, key, item);
        Ok(true)
    })
}

/// The table `name` of `parent`, created when missing; `implicit` leaves out
/// its own `[header]` line.
fn sub_table<'a>(
    parent: &'a mut Table,
    name: &str,
    implicit: bool,
) -> Result<&'a mut Table, String> {
    if !parent.contains_key(name) {
        let mut t = Table::new();
        t.set_implicit(implicit);
        parent.insert(name, Item::Table(t));
    }
    parent
        .get_mut(name)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| format!("{name} in settings.toml is not a table"))
}

/// Replaces or adds `key`, keeping the old key's comments above it and the
/// old value's spacing and trailing comment.
fn put(table: &mut Table, key: &str, mut item: Item) {
    let Some(old) = table.get_mut(key) else {
        table.insert(key, item);
        return;
    };
    if let (Some(o), Item::Value(new)) = (old.as_value(), &mut item) {
        *new.decor_mut() = o.decor().clone();
    }
    *old = item;
}

/// Removes the key at `path`; empty tables this leaves behind that were
/// dotted or implicit go too. Whether anything was removed.
fn remove(table: &mut Table, path: &[&str]) -> bool {
    let Some((first, rest)) = path.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return table.remove(first).is_some();
    }
    let Some(child) = table.get_mut(first).and_then(Item::as_table_mut) else {
        return false;
    };
    let removed = remove(child, rest);
    if removed && child.is_empty() && (child.is_dotted() || child.is_implicit()) {
        table.remove(first);
    }
    removed
}

/// Reads the file, applies `f`, and writes when `f` says it changed
/// something.
fn edit(
    dir: &Path,
    f: impl FnOnce(&mut DocumentMut) -> Result<bool, String>,
) -> Result<Saved, String> {
    let path = dir.join(FILE);
    let old = match fs::read(&path) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let text = match &old {
        Some(b) => std::str::from_utf8(b)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .to_string(),
        None => String::new(),
    };
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if f(&mut doc)? {
        let mut out = doc.to_string();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        write_atomic(&path, out.as_bytes())?;
    }
    Ok(Saved { path, old })
}

/// Temp file in the same folder, then rename over the target.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(err)?;
    }
    let tmp = path.with_extension(format!("toml.tmp{}", std::process::id()));
    fs::write(&tmp, bytes).map_err(err)?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        err(e)
    })
}
