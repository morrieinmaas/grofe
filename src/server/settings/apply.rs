//! Saving the settings menu (spec AC3.9): what C-s writes, the diff it
//! shows, computed in memory, and the real write, which goes through
//! `customize::with_owned_reload`.

use std::fs;
use std::path::{Path, PathBuf};

use toml_edit::DocumentMut;

use crate::config::write::{self, Saved};
use crate::server::customize::{self, Fields, Scope};
use crate::server::input::{Key, Routed};
use crate::server::prompt;
use crate::server::{State, clients};
use crate::types::ClientId;

use super::overlay::{self, Menu};

/// One edit to `settings.toml`.
pub enum Change {
    Set {
        key: String,
        value: toml_edit::Item,
    },
    /// A key's whole entry; an empty `cmd` unbinds it.
    Bind {
        table: String,
        key: String,
        fields: Fields,
    },
}

/// Makes `changes` to the parsed `settings.toml` in `doc`.
fn apply_changes(doc: &mut DocumentMut, changes: &[Change]) -> Result<(), String> {
    for c in changes {
        match c {
            Change::Set { key, value } => write::set_in(doc, key, value.clone())?,
            Change::Bind { table, key, fields } => {
                write::set_binding_in(doc, table, key, &fields.cmd, &fields.as_write())?
            }
        }
    }
    Ok(())
}

/// Writes `changes` in one go, or nothing when one fails. What puts the
/// file back as it was is returned.
pub fn write(dir: &Path, changes: &[Change]) -> Result<Saved, String> {
    write::edit(dir, |doc| apply_changes(doc, changes).map(|()| true))
}

/// `-old` / `+new` lines of `settings.toml` before and after `changes`,
/// which are made to the parsed text only, so the file is untouched.
// ponytail: a line-set diff, not an LCS one; a line moved without change in
// content shows nothing, and duplicate lines are matched by count only.
pub fn diff(dir: &Path, changes: &[Change]) -> Result<String, String> {
    let before = match fs::read_to_string(dir.join(write::FILE)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", write::FILE)),
    };
    let mut doc: DocumentMut = before
        .parse()
        .map_err(|e| format!("{}: {e}", write::FILE))?;
    apply_changes(&mut doc, changes)?;
    Ok(line_diff(&before, &doc.to_string()))
}

fn line_diff(before: &str, after: &str) -> String {
    let mut out = String::new();
    let mut remaining: Vec<&str> = after.lines().collect();
    for l in before.lines() {
        match remaining.iter().position(|a| *a == l) {
            Some(i) => {
                remaining.remove(i);
            }
            None => out.push_str(&format!("-{l}\n")),
        }
    }
    for l in remaining {
        out.push_str(&format!("+{l}\n"));
    }
    out
}

/// What C-s writes: the edits to settings.toml for what this menu set, the
/// options and keys they hand over to it, and the changes that stay live
/// only, with why.
#[derive(Default)]
struct Plan {
    changes: Vec<Change>,
    names: Vec<&'static str>,
    keys: Vec<(String, Key)>,
    refused: Vec<String>,
}

fn collect(state: &State, menu: &Menu) -> Plan {
    let mut plan = Plan::default();
    for &name in menu.set.keys() {
        match customize::toml_item(state, Scope::GlobalSession, name, None) {
            Ok((key, value)) => {
                plan.names.push(name);
                plan.changes.push(Change::Set {
                    key: key.to_string(),
                    value,
                });
            }
            Err(e) => plan.refused.push(e),
        }
    }
    for t in &menu.keys {
        let why = t
            .refusal
            .or_else(|| customize::binding_refusal(state, &t.table, &t.key));
        match why {
            Some(why) => plan
                .refused
                .push(format!("{} {}: not saved, {why}", t.table, t.key)),
            None => {
                plan.changes.push(Change::Bind {
                    table: t.table.clone(),
                    key: t.key.to_string(),
                    fields: customize::binding_fields(state, &t.table, &t.key),
                });
                plan.keys.push((t.table.clone(), t.key));
            }
        }
    }
    plan
}

/// The lines C-s shows before it asks, or why there is nothing to ask
/// about: no folder to save to, or nothing that can be saved.
fn preview(dir: Result<PathBuf, String>, plan: &Plan) -> Result<Vec<String>, String> {
    let dir = dir?;
    if plan.changes.is_empty() {
        return Err(match plan.refused.is_empty() {
            true => "nothing to save".to_string(),
            false => plan.refused.join("; "),
        });
    }
    let mut lines: Vec<String> = diff(&dir, &plan.changes)?
        .lines()
        .map(str::to_string)
        .collect();
    lines.extend(plan.refused.iter().map(|l| format!("! {l}")));
    Ok(lines)
}

/// C-s: shows the diff and asks before writing.
pub(super) fn save(state: &mut State, client: ClientId) {
    let Some(menu) = state.settings_menus.open.get(&client) else {
        return;
    };
    let plan = collect(state, menu);
    let lines = match preview(customize::settings_dir(), &plan) {
        Ok(lines) => lines,
        Err(why) => {
            clients::show_message(state, client, &why);
            return;
        }
    };
    if let Some(menu) = state.settings_menus.open.get_mut(&client) {
        menu.diff = Some(lines);
    }
    prompt::ask(
        state,
        client,
        "write settings.toml? (y/n) ",
        "",
        "command",
        true,
        Box::new(move |state, client, text| {
            if let Some(menu) = state.settings_menus.open.get_mut(&client) {
                menu.diff = None;
            }
            if text == "y" {
                commit(state, client);
            }
            clients::mark_dirty(state, client);
            Routed::Consumed
        }),
    );
    clients::mark_dirty(state, client);
}

/// Writes settings.toml, hands the changed options and keys to it and
/// reloads. A reload that fails puts the file, the marks and the bindings
/// back; what was refused stays live and is not written.
fn commit(state: &mut State, client: ClientId) {
    let dir = match customize::settings_dir() {
        Ok(dir) => dir,
        Err(why) => {
            clients::show_message(state, client, &why);
            return;
        }
    };
    let Some(menu) = state.settings_menus.open.get(&client) else {
        return;
    };
    let plan = collect(state, menu);
    if plan.changes.is_empty() {
        clients::show_message(state, client, "nothing to save");
        return;
    }
    let keys: Vec<(&str, Key)> = plan.keys.iter().map(|(t, k)| (t.as_str(), *k)).collect();
    let result = customize::with_owned_reload(state, &plan.names, &keys, || {
        write(&dir, &plan.changes).map(Some)
    });
    match result {
        Ok(()) => {
            overlay::close(state, client);
            clients::show_message(state, client, "saved settings.toml");
        }
        Err(e) => clients::show_message(state, client, e.trim_end()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(key: &str, v: i64) -> Change {
        Change::Set {
            key: key.into(),
            value: toml_edit::value(v),
        }
    }

    #[test]
    fn diff_shows_one_plus_line_and_leaves_the_file_alone() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join(write::FILE);
        fs::write(&f, "# mine\nhistory-limit = 1\n").unwrap();
        let out = diff(d.path(), &[set("scrollback", 5)]).unwrap();
        assert_eq!(out.lines().filter(|l| l.starts_with('+')).count(), 1);
        assert!(out.contains("+scrollback = 5"), "{out}");
        assert_eq!(fs::read(&f).unwrap(), b"# mine\nhistory-limit = 1\n");
    }

    /// The diff is computed in memory: no file or folder is created, not
    /// even in a config folder that does not exist yet.
    #[test]
    fn diff_creates_nothing() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("missing");
        let out = diff(&dir, &[set("scrollback", 5)]).unwrap();
        assert!(out.contains("+scrollback = 5"), "{out}");
        assert!(!dir.exists());
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 0);
    }

    #[test]
    fn write_then_rollback_restores_bytes() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join(write::FILE);
        fs::write(&f, "a = 1\n").unwrap();
        let saved = write(d.path(), &[set("b", 2), set("c", 3)]).unwrap();
        assert_ne!(fs::read(&f).unwrap(), b"a = 1\n");
        saved.rollback().unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"a = 1\n");
    }

    #[test]
    fn a_bind_writes_the_whole_entry() {
        let d = tempfile::tempdir().unwrap();
        let bind = Change::Bind {
            table: "prefix".into(),
            key: "C-t".into(),
            fields: Fields {
                cmd: "new-window".into(),
                desc: Some("a note".into()),
                repeat: true,
                group: Some("window".into()),
                name: Some("nw".into()),
            },
        };
        let out = diff(d.path(), &[bind]).unwrap();
        for part in ["new-window", "a note", "repeat = true", "window", "nw"] {
            assert!(out.contains(part), "{part}: {out}");
        }
    }

    /// C-s does not ask y/n when there is no folder to save to, or when
    /// everything the menu changed is refused; it says why instead.
    #[test]
    fn nothing_to_ask_about_says_why() {
        let changed = Plan {
            changes: vec![set("scrollback", 5)],
            ..Plan::default()
        };
        let e = preview(Err("no config folder to save to".into()), &changed).unwrap_err();
        assert_eq!(e, "no config folder to save to");
        let refused = Plan {
            refused: vec!["@mine: not saveable, it has no TOML key".into()],
            ..Plan::default()
        };
        let d = tempfile::tempdir().unwrap();
        let e = preview(Ok(d.path().to_path_buf()), &refused).unwrap_err();
        assert!(e.contains("@mine: not saveable"), "{e}");
        let e = preview(Ok(d.path().to_path_buf()), &Plan::default()).unwrap_err();
        assert_eq!(e, "nothing to save");
    }
}
