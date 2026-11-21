//! Saving customize-mode's items to settings.toml (spec 2026-11-20-settings-menu,
//! PR 2): `P` writes the item under the cursor, `d` and `D` take an option's
//! key out again. Each then reloads the config, and puts the file back when
//! the reload fails.

use std::path::PathBuf;

use super::{Entry, Scope, is_global, key_command_text, value_of};
use crate::config::write::{self, Saved};
use crate::protocol::{ClientCtx, Dialect};
use crate::server::State;
use crate::server::dispatch::options;
use crate::server::input::Key;
use crate::types::ClientId;

/// The TOML key of option `name`; an array element has none.
pub(super) fn key_of(name: &str, idx: Option<u32>) -> Option<&'static str> {
    crate::config::toml_key(name).filter(|_| idx.is_none())
}

/// Why `name` at `scope` cannot be saved, if it cannot: it has no scalar
/// TOML key (a `@user` option, a list option or an array element), or the
/// value is a session's, window's or pane's own rather than the global one.
pub(super) fn refusal(scope: Scope, name: &str, idx: Option<u32>) -> Option<&'static str> {
    if key_of(name, idx).is_none() {
        Some("it has no TOML key")
    } else if !is_global(scope) {
        Some("only global values are saveable")
    } else {
        None
    }
}

/// The settings.toml key and value of option `name` as `scope` shows it, or
/// why it cannot be saved: a user option, an array element and a list
/// option have no scalar TOML key.
// ponytail: list options (`status-format` and the like) are not saved;
// writing them back as a TOML array is not done yet.
pub(crate) fn toml_item(
    state: &State,
    scope: Scope,
    name: &str,
    idx: Option<u32>,
) -> Result<(&'static str, toml_edit::Item), String> {
    let key =
        key_of(name, idx).ok_or_else(|| format!("{name}: not saveable, it has no TOML key"))?;
    let value = value_of(state, scope, name, None).ok_or_else(|| format!("{name} is not set"))?;
    let literal = crate::config::literal(key, &value);
    let v: toml_edit::Value = literal
        .parse()
        .map_err(|e| format!("{name}: {value}: {e}"))?;
    Ok((key, toml_edit::Item::Value(v)))
}

/// The folder settings.toml lives in. Not with `tmux -f`: the server reads
/// only those files then, so a save would change nothing.
fn settings_dir() -> Result<PathBuf, String> {
    if crate::compat::start_config().is_some() {
        return Err(
            "not saved: the server was started with -f, so settings.toml is not read".into(),
        );
    }
    options::config_env()
        .config_dir
        .ok_or_else(|| "no config folder to save to".to_string())
}

/// A binding as `[keys.<table>]` writes it: the command in the config's
/// spelling (a command string spells tmux's ` \; ` as ` ; `), and its note,
/// repeat flag, group and name.
struct Fields {
    cmd: String,
    desc: Option<String>,
    repeat: bool,
    group: Option<String>,
    name: Option<String>,
}

fn binding_fields(state: &State, table: &str, key: &crate::server::input::Key) -> Fields {
    let cmd = key_command_text(state, table, key).replace(" \\; ", " ; ");
    match state.registry.key_tables.exact(table, key) {
        Some(b) => Fields {
            cmd,
            desc: b.desc.clone(),
            repeat: b.repeat,
            group: b.group.clone(),
            name: b.name.clone(),
        },
        None => Fields {
            cmd,
            desc: None,
            repeat: false,
            group: None,
            name: None,
        },
    }
}

/// `P` in customize-mode: writes `entry` to settings.toml, then reloads the
/// config. When the reload fails the file is put back, the runtime marks stay
/// and the error is returned. On success, what was saved, and settings.toml
/// owns it: a hand edit of the file and a `reload` apply.
pub(crate) fn persist(state: &mut State, entry: &Entry) -> Result<String, String> {
    // What the reload is to hand over to the config, until it succeeds.
    let mut option = None;
    let mut binding = None;
    let (what, saved) = match entry {
        Entry::Option { scope, name, idx } => {
            if let Some(why) = refusal(*scope, name, *idx) {
                return Err(format!("{name}: not saveable, {why}"));
            }
            let (key, item) = toml_item(state, *scope, name, *idx)?;
            let saved = write::set(&settings_dir()?, key, item)?;
            option = Some(name.as_str());
            (name.clone(), saved)
        }
        Entry::Key { table, key } | Entry::Field { table, key, .. } => {
            let f = binding_fields(state, table, key);
            let name = key.to_string();
            let saved = write::set_binding(
                &settings_dir()?,
                table,
                &name,
                &f.cmd,
                &write::BindingFields {
                    desc: f.desc.as_deref(),
                    repeat: Some(f.repeat),
                    group: f.group.as_deref(),
                    name: f.name.as_deref(),
                },
            )?;
            // A binding made at runtime would outlive the reload and hide the
            // file's: take it out so the reload builds it from settings.toml.
            let live = state.registry.key_tables.exact(table, key).cloned();
            if let Some(b) = live.filter(|b| !b.source.is_config()) {
                state.registry.key_tables.unbind(table, key);
                binding = Some((table, *key, b));
            }
            (name, saved)
        }
        Entry::Group => return Err("nothing to save here".to_string()),
    };
    match reload_or_roll_back(state, saved) {
        Ok(()) => {
            // settings.toml owns the value now: a reload skips runtime names.
            if let Some(name) = option {
                state.model.options.forget_runtime(name);
            }
            Ok(format!("Saved {what} to settings.toml"))
        }
        Err(e) => {
            if let Some((table, key, b)) = binding {
                state.registry.key_tables.insert(table, key, b);
            }
            Err(e)
        }
    }
}

/// `d` and `D` on options `names` and key bindings `keys`: runs `cmds`,
/// which unset the options at every scope and reset the keys, then hands
/// them back to the config layers: they leave settings.toml, and the reload
/// applies what is below. When the reload fails the file is put back and the
/// options keep their runtime marks. With no settings.toml to change, only
/// `cmds` run.
pub(crate) fn reset(
    state: &mut State,
    client: ClientId,
    cmds: &[Vec<String>],
    names: &[String],
    keys: &[(String, Key)],
) -> Result<(), String> {
    crate::server::clients::run_binding(state, client, cmds, Dialect::Tmux, None);
    let keyed: Vec<(&String, &str)> = names
        .iter()
        .filter_map(|n| Some((n, crate::config::toml_key(n)?)))
        .collect();
    if keyed.is_empty() && keys.is_empty() {
        return Ok(());
    }
    let Ok(dir) = settings_dir() else {
        return Ok(());
    };
    // The first write holds the file as it was, whatever the later ones do.
    let mut first: Option<Saved> = None;
    let writes = keyed.iter().map(|(_, key)| write::unset(&dir, key)).chain(
        keys.iter()
            .map(|(table, key)| write::unset_binding(&dir, table, &key.to_string())),
    );
    for write in writes {
        match write {
            Ok(saved) => {
                first.get_or_insert(saved);
            }
            Err(e) => return Err(failed(e, first.map_or(Ok(()), Saved::rollback))),
        }
    }
    let held: Vec<&String> = keyed
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| state.model.options.forget_runtime(name))
        .collect();
    // A key reset to nothing is unbound at runtime, which a reload keeps;
    // the config below may bind it, so it must not.
    let unbound: Vec<&(String, Key)> = keys
        .iter()
        .filter(|(table, key)| state.registry.key_tables.forget_unbound(table, key))
        .collect();
    let Some(saved) = first else {
        return Ok(());
    };
    let result = reload_or_roll_back(state, saved);
    if result.is_err() {
        for name in held {
            state.model.options.remember_runtime(name);
        }
        for (table, key) in unbound {
            state.registry.key_tables.remember_unbound(table, key);
        }
    }
    result
}

/// Reloads the config; when that fails, puts the file back and returns the
/// reload's errors, and the rollback's too if it failed.
fn reload_or_roll_back(state: &mut State, saved: Saved) -> Result<(), String> {
    let ctx = ClientCtx {
        cwd: PathBuf::from("/"),
        pane: None,
        env: Vec::new(),
        client: None,
        dialect: Dialect::Tmux,
        request: Default::default(),
    };
    match options::reload(state, &ctx, crate::cmd::Reload) {
        Ok(_) => Ok(()),
        Err(e) => Err(failed(e, saved.rollback())),
    }
}

/// `error`, with what went wrong putting settings.toml back, if anything.
fn failed(error: String, rollback: Result<(), String>) -> String {
    match rollback {
        Ok(()) => error,
        Err(r) => format!("{error} (restoring settings.toml failed: {r})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::dispatch::options::Owner;

    fn global_option(state: &mut State, name: &str, value: &str) {
        state
            .model
            .options
            .set(Owner::Global, name, value, false)
            .expect("set");
    }

    /// AC2.9: a user option, a list option and an array element have no TOML
    /// key; `persist` and `toml_item` refuse them and say so.
    #[test]
    fn options_without_a_toml_key_are_refused() {
        let mut state = crate::server::test_state();
        global_option(&mut state, "@mine", "x");
        for name in ["@mine", "status-format"] {
            let e = toml_item(&state, Scope::GlobalSession, name, None).expect_err(name);
            assert!(e.contains("not saveable"), "{name}: {e}");
            let entry = Entry::Option {
                scope: Scope::GlobalSession,
                name: name.to_string(),
                idx: None,
            };
            let e = persist(&mut state, &entry).expect_err(name);
            assert!(e.contains("not saveable"), "{name}: {e}");
        }
        let e = toml_item(&state, Scope::GlobalSession, "status-format", Some(0)).expect_err("idx");
        assert!(e.contains("not saveable"), "{e}");
    }

    /// A session's own value is not the global one: `P` refuses it, and the
    /// preview says so.
    #[test]
    fn a_local_value_is_not_saveable() {
        let mut state = crate::server::test_state();
        let entry = Entry::Option {
            scope: Scope::Session(crate::types::SessionId(1)),
            name: "history-limit".to_string(),
            idx: None,
        };
        let e = persist(&mut state, &entry).expect_err("local");
        assert!(e.contains("only global values are saveable"), "{e}");
        assert_eq!(
            refusal(Scope::GlobalSession, "history-limit", None),
            None,
            "the global value is saveable"
        );
        assert_eq!(
            refusal(
                Scope::Window(crate::types::WindowId(1)),
                "history-limit",
                None
            ),
            Some("only global values are saveable")
        );
    }

    /// AC2.9: the preview of an option with no TOML key says so, and the
    /// preview of one that has a key does not.
    #[test]
    fn the_preview_marks_what_is_not_saveable() {
        let mut state = crate::server::test_state();
        global_option(&mut state, "@mine", "x");
        let fs = super::super::Fs {
            session: crate::types::SessionId(1),
            window: crate::types::WindowId(1),
            pane: crate::types::PaneId(1),
        };
        let preview = |state: &State, name: &str| -> String {
            let entry = Entry::Option {
                scope: Scope::GlobalSession,
                name: name.to_string(),
                idx: None,
            };
            let page = super::super::info(state, fs, &entry, 80, 20, &crate::cmd::NoVars);
            page.lines
                .iter()
                .map(|l| l.iter().map(|(t, _)| t.as_str()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(preview(&state, "@mine").contains("Not saveable"));
        assert!(!preview(&state, "history-limit").contains("Not saveable"));
    }

    /// A reload that fails and a rollback that fails both reach the user.
    #[test]
    fn a_failed_rollback_adds_its_message() {
        assert_eq!(failed("reload".into(), Ok(())), "reload");
        let both = failed("reload".into(), Err("disk".into()));
        assert!(both.contains("reload") && both.contains("disk"), "{both}");
    }
}
