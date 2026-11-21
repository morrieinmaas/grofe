//! Write-back to `settings.toml` (spec 2026-11-20-settings-menu, PR 2): the
//! file grofe itself edits, loaded after every other layer. Each call
//! re-reads the file, changes one key with `toml_edit` so comments, blank
//! lines and other keys stay byte for byte, and replaces the file by a
//! temp file plus rename.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike, Value, value};

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
    edit(dir, |doc| set_in(doc, key, item).map(|()| true))
}

/// What [`set`] does to the file, done to `doc`.
pub fn set_in(doc: &mut DocumentMut, key: &str, item: Item) -> Result<(), String> {
    if key.starts_with('@') || key.is_empty() {
        return Err(format!("{key}: not saveable, it has no TOML key"));
    }
    let parts: Vec<&str> = key.split('.').collect();
    let (last, parents) = parts.split_last().ok_or("empty key")?;
    let mut table: &mut dyn TableLike = doc.as_table_mut();
    for p in parents {
        if !table.contains_key(p) {
            let mut t = Table::new();
            t.set_dotted(true);
            table.insert(p, Item::Table(t));
        }
        table = table
            .get_mut(p)
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| format!("{key}: {p} in settings.toml is not a table"))?;
    }
    put(table, last, item);
    Ok(())
}

/// Removes `key` from settings.toml, so the lower layers apply again. A key
/// that is not there is not an error.
pub fn unset(dir: &Path, key: &str) -> Result<Saved, String> {
    edit(dir, |doc| {
        let parts: Vec<&str> = key.split('.').collect();
        Ok(remove(doc.as_table_mut(), &parts))
    })
}

/// The fields of a binding besides its command, for [`set_binding`].
#[derive(Debug, Default, Clone, Copy)]
pub struct BindingFields<'a> {
    pub desc: Option<&'a str>,
    pub repeat: bool,
    pub group: Option<&'a str>,
    pub name: Option<&'a str>,
}

/// Binds `key` in `[keys.<table>]` to `cmd`; an empty `cmd` unbinds it
/// (`key = ""`). An entry already there keeps its place and comments, and
/// its fields become `fields`: one that is `None` (or a `false` repeat
/// flag) is removed.
pub fn set_binding(
    dir: &Path,
    table: &str,
    key: &str,
    cmd: &str,
    fields: &BindingFields,
) -> Result<Saved, String> {
    edit(dir, |doc| {
        set_binding_in(doc, table, key, cmd, fields).map(|()| true)
    })
}

/// What [`set_binding`] does to the file, done to `doc`.
pub fn set_binding_in(
    doc: &mut DocumentMut,
    table: &str,
    key: &str,
    cmd: &str,
    fields: &BindingFields,
) -> Result<(), String> {
    let root = doc.as_table_mut();
    let keys = sub_table(root, "keys", true)?;
    let t = sub_table(keys, table, false)?;
    if cmd.is_empty() {
        put(t, key, value(""));
        return Ok(());
    }
    let texts = [
        ("desc", fields.desc),
        ("group", fields.group),
        ("name", fields.name),
    ];
    if let Some(entry) = t.get_mut(key).and_then(Item::as_table_like_mut) {
        put(entry, "cmd", value(cmd));
        for (field, text) in texts {
            match text {
                Some(text) => put(entry, field, value(text)),
                None => {
                    entry.remove(field);
                }
            }
        }
        if fields.repeat {
            put(entry, "repeat", value(true));
        } else {
            entry.remove("repeat");
        }
        return Ok(());
    }
    let mut inline = InlineTable::new();
    inline.insert("cmd", cmd.into());
    for (field, text) in texts {
        if let Some(text) = text {
            inline.insert(field, text.into());
        }
    }
    if fields.repeat {
        inline.insert("repeat", Value::from(true));
    }
    put(t, key, value(inline));
    Ok(())
}

/// Removes `key` of `[keys.<table>]` from settings.toml, so the lower layers
/// bind it again. A key that is not there is not an error.
pub fn unset_binding(dir: &Path, table: &str, key: &str) -> Result<Saved, String> {
    edit(dir, |doc| {
        Ok(remove(doc.as_table_mut(), &["keys", table, key]))
    })
}

/// The table `name` of `parent`, created when missing; `implicit` leaves out
/// its own `[header]` line.
fn sub_table<'a>(
    parent: &'a mut dyn TableLike,
    name: &str,
    implicit: bool,
) -> Result<&'a mut dyn TableLike, String> {
    if !parent.contains_key(name) {
        let mut t = Table::new();
        t.set_implicit(implicit);
        parent.insert(name, Item::Table(t));
    }
    parent
        .get_mut(name)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| format!("{name} in settings.toml is not a table"))
}

/// Replaces or adds `key`, keeping the old key's comments above it and the
/// old value's spacing and trailing comment.
fn put(table: &mut dyn TableLike, key: &str, mut item: Item) {
    let Some(old) = table.get_mut(key) else {
        table.insert(key, item);
        return;
    };
    if let (Some(o), Item::Value(new)) = (old.as_value(), &mut item) {
        *new.decor_mut() = o.decor().clone();
    }
    *old = item;
}

/// Removes the key at `path`; tables this leaves empty that were dotted,
/// implicit or inline go too. Whether anything was removed.
fn remove(table: &mut dyn TableLike, path: &[&str]) -> bool {
    let Some((first, rest)) = path.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return table.remove(first).is_some();
    }
    let Some(item) = table.get_mut(first) else {
        return false;
    };
    let droppable = match item {
        Item::Table(t) => t.is_dotted() || t.is_implicit(),
        _ => true,
    };
    let Some(child) = item.as_table_like_mut() else {
        return false;
    };
    let removed = remove(child, rest);
    if removed && droppable && child.is_empty() {
        table.remove(first);
    }
    removed
}

/// Where a write to `path` lands: its target when it is a symlink, also a
/// dangling one, which the write then creates and a rollback removes, so the
/// link itself is never replaced.
fn resolve(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    for _ in 0..40 {
        if let Ok(c) = fs::canonicalize(&p) {
            return c;
        }
        let is_link = fs::symlink_metadata(&p).is_ok_and(|m| m.is_symlink());
        let Some(target) = is_link.then(|| fs::read_link(&p).ok()).flatten() else {
            break;
        };
        p = p.parent().unwrap_or(Path::new("")).join(target);
    }
    p
}

/// Reads the file, applies `f`, and writes when `f` says it changed
/// something.
pub(crate) fn edit(
    dir: &Path,
    f: impl FnOnce(&mut DocumentMut) -> Result<bool, String>,
) -> Result<Saved, String> {
    let path = resolve(&dir.join(FILE));
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

/// Temp file in the same folder, then rename over the target. A symlinked
/// settings.toml is written through to its target and the target keeps its
/// permissions; the temp file is flushed to disk before the rename and is
/// removed again when anything fails.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir).map_err(err)?;
    }
    let tmp = target.with_extension(format!("toml.tmp{}", std::process::id()));
    let perms = fs::metadata(&target).ok().map(|m| m.permissions());
    let write = || -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        if let Some(p) = perms {
            f.set_permissions(p)?;
        }
        f.sync_all()?;
        fs::rename(&tmp, &target)
    };
    write().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        err(e)
    })
}
