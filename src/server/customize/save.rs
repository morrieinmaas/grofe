//! Saving customize-mode's items to settings.toml (spec 2026-11-20-settings-menu,
//! PR 2): `P` writes the item under the cursor, `d` and `D` take a global
//! option's or a key's entry out again. Each then reloads the config, and
//! puts the file and the runtime state back when the reload fails.

use std::path::PathBuf;

use super::{Entry, Scope, is_global, value_of};
use crate::config::write::{self, Saved};
use crate::protocol::{ClientCtx, Dialect};
use crate::server::State;
use crate::server::dispatch::options;
use crate::server::input::{BindingSource, Key};

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
pub(crate) fn settings_dir() -> Result<PathBuf, String> {
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
#[derive(Default)]
pub struct Fields {
    pub(crate) cmd: String,
    pub(crate) desc: Option<String>,
    pub(crate) repeat: bool,
    pub(crate) group: Option<String>,
    pub(crate) name: Option<String>,
}

impl Fields {
    pub fn as_write(&self) -> write::BindingFields<'_> {
        write::BindingFields {
            desc: self.desc.as_deref(),
            repeat: self.repeat,
            group: self.group.as_deref(),
            name: self.name.as_deref(),
        }
    }
}

pub(crate) fn binding_fields(
    state: &State,
    table: &str,
    key: &crate::server::input::Key,
) -> Fields {
    match state.registry.key_tables.exact(table, key) {
        Some(b) => Fields {
            cmd: options::tmux_command_line(&b.cmds).replace(" \\; ", " ; "),
            desc: b.desc.clone(),
            repeat: b.repeat,
            group: b.group.clone(),
            name: b.name.clone(),
        },
        None => Fields::default(),
    }
}

/// Whether a plugin (init.luau or a pack) bound the key: its commands are
/// runtime `lua-call`s, not a config entry.
fn binding_is_plugin(state: &State, table: &str, key: &Key) -> bool {
    state
        .registry
        .key_tables
        .exact(table, key)
        .is_some_and(|b| matches!(b.source, BindingSource::Plugin(_)))
}

/// Whether the binding is grofe-dialect: a tmux-identity stock key or a
/// tmux-client `bind-key` has no `[keys]` spelling to save.
fn binding_is_grofe(state: &State, table: &str, key: &crate::server::input::Key) -> bool {
    state
        .registry
        .key_tables
        .exact(table, key)
        .is_none_or(|b| b.dialect == Dialect::Grofe)
}

/// Why the binding of `key` in `table` cannot be saved, if it cannot.
pub(crate) fn binding_refusal(state: &State, table: &str, key: &Key) -> Option<&'static str> {
    if binding_is_plugin(state, table, key) {
        Some("bound by a plugin, not saveable")
    } else if !binding_is_grofe(state, table, key) {
        Some("only grofe-dialect bindings are saveable")
    } else {
        None
    }
}

/// `P` in customize-mode: writes `entry` to settings.toml, then reloads the
/// config. A key's entry is replaced as a whole: a field its live binding
/// lacks goes. When the reload fails the file is put back, the runtime marks
/// stay and the error is returned. On success, what was saved, and
/// settings.toml owns it: a hand edit of the file and a `reload` apply.
pub(crate) fn persist(state: &mut State, entry: &Entry) -> Result<String, String> {
    match entry {
        Entry::Option { scope, name, idx } => {
            if let Some(why) = refusal(*scope, name, *idx) {
                return Err(format!("{name}: not saveable, {why}"));
            }
            let (key, item) = toml_item(state, *scope, name, *idx)?;
            let dir = settings_dir()?;
            with_owned_reload(state, &[name], &[], || {
                write::set(&dir, key, item).map(Some)
            })?;
            Ok(format!("Saved {name} to settings.toml"))
        }
        Entry::Key { table, key } | Entry::Field { table, key, .. } => {
            if let Some(why) = binding_refusal(state, table, key) {
                return Err(why.to_string());
            }
            let f = binding_fields(state, table, key);
            let name = key.to_string();
            let dir = settings_dir()?;
            with_owned_reload(state, &[], &[(table, *key)], || {
                write::set_binding(&dir, table, &name, &f.cmd, &f.as_write()).map(Some)
            })?;
            Ok(format!("Saved {name} to settings.toml"))
        }
        Entry::Group => Err("nothing to save here".to_string()),
    }
}

/// `d` and `D` on global options `names` and key bindings `keys`: hands
/// them back to the config layers. Their entries leave settings.toml, then
/// [`with_owned_reload`] releases the runtime values and bindings and
/// reloads, so the lower layer or the built-in default applies. With no
/// settings.toml to change (`tmux -f`, no config folder) only the release
/// and the reload run. A name with no TOML key has no entry to remove.
pub(crate) fn reset(
    state: &mut State,
    names: &[String],
    keys: &[(String, Key)],
) -> Result<(), String> {
    let dir = settings_dir().ok();
    let plugin_keys: Vec<bool> = keys
        .iter()
        .map(|(t, k)| binding_is_plugin(state, t, k))
        .collect();
    let option_names: Vec<&str> = names.iter().map(String::as_str).collect();
    let key_names: Vec<(&str, Key)> = keys.iter().map(|(t, k)| (t.as_str(), *k)).collect();
    with_owned_reload(state, &option_names, &key_names, || {
        let Some(dir) = dir else {
            return Ok(None);
        };
        // The first write holds the file as it was, whatever the later ones do.
        let mut first: Option<Saved> = None;
        let writes = names
            .iter()
            .filter_map(|n| crate::config::toml_key(n))
            .map(|key| write::unset(&dir, key))
            .chain(
                keys.iter()
                    .zip(&plugin_keys)
                    .filter(|(_, plugin)| !**plugin)
                    .map(|((table, key), _)| write::unset_binding(&dir, table, &key.to_string())),
            );
        for write in writes {
            match write {
                Ok(saved) => {
                    first.get_or_insert(saved);
                }
                Err(e) => return Err(failed(e, first.map_or(Ok(()), Saved::rollback))),
            }
        }
        Ok(first)
    })
}

/// Hands options `names` and key bindings `keys` over to the config by
/// reloading it, after `write` changed settings.toml (it returns what puts
/// the file back, or nothing when there is no file to change).
///
/// A reload leaves what was set at runtime alone, so first each name's
/// runtime mark goes, and so do each key's runtime binding and runtime
/// unbind; values stay until the reload sets them, so `option-changed`
/// fires only for a value that ends up different. When the reload fails
/// the file is put back and the marks and bindings are as they were; both
/// errors are returned.
pub(crate) fn with_owned_reload(
    state: &mut State,
    names: &[&str],
    keys: &[(&str, Key)],
    write: impl FnOnce() -> Result<Option<Saved>, String>,
) -> Result<(), String> {
    let saved = write()?;
    let released: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| state.model.options.release_runtime(name))
        .collect();
    let tables = state.registry.key_tables.clone();
    for (table, key) in keys {
        state.registry.key_tables.release_runtime(table, key);
    }
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
        Err(e) => {
            let error = failed(e, saved.map_or(Ok(()), Saved::rollback));
            for name in released {
                state.model.options.restore_runtime(name);
            }
            state.registry.key_tables = tables;
            Err(error)
        }
    }
}

/// Hands options `names` and key bindings `keys` back to the config as it
/// is: [`with_owned_reload`] with no file to change.
pub(crate) fn release_and_reload(
    state: &mut State,
    names: &[&str],
    keys: &[(&str, Key)],
) -> Result<(), String> {
    with_owned_reload(state, names, keys, || Ok(None))
}

/// `error`, with what went wrong putting settings.toml back, if anything.
pub(crate) fn failed(error: String, rollback: Result<(), String>) -> String {
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

    /// A tmux-dialect binding (a tmux-identity stock key) has no `[keys]`
    /// spelling: `P` refuses it and writes nothing.
    #[test]
    fn a_plugin_binding_is_refused_and_a_shown_text_is_not_saved() {
        let mut state = crate::server::test_state();
        let key = crate::server::input::parse_key("c").expect("key");
        let mut b = state
            .registry
            .key_tables
            .exact("prefix", &key)
            .cloned()
            .expect("stock c");
        b.source = BindingSource::Plugin("p".into());
        state.registry.key_tables.insert("prefix", key, b.clone());
        let entry = Entry::Key {
            table: "prefix".to_string(),
            key,
        };
        let e = persist(&mut state, &entry).expect_err("plugin");
        assert_eq!(e, "bound by a plugin, not saveable");

        b.source = BindingSource::Runtime;
        b.shown = Some("a description".into());
        state.registry.key_tables.insert("prefix", key, b.clone());
        let f = binding_fields(&state, "prefix", &key);
        assert_eq!(
            f.cmd,
            options::tmux_command_line(&b.cmds).replace(" \\; ", " ; ")
        );
        assert!(!f.cmd.contains("a description"));
    }

    #[test]
    fn a_tmux_dialect_binding_is_not_saveable() {
        let mut state = crate::server::test_state();
        let key = crate::server::input::parse_key("c").expect("key");
        let mut b = state
            .registry
            .key_tables
            .exact("prefix", &key)
            .cloned()
            .expect("stock c");
        b.dialect = Dialect::Tmux;
        state.registry.key_tables.insert("prefix", key, b);
        let entry = Entry::Key {
            table: "prefix".to_string(),
            key,
        };
        let e = persist(&mut state, &entry).expect_err("tmux dialect");
        assert_eq!(e, "only grofe-dialect bindings are saveable");
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
