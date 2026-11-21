use super::*;

pub const ROOT_TABLE: &str = "root";
pub const PREFIX_TABLE: &str = "prefix";

/// Where a binding came from (spec §5.4, key provenance).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingSource {
    /// The built-in default table.
    Default,
    /// A config file layer (spec §5.1), at the key's line.
    File { path: PathBuf, line: usize },
    /// `bind-key` or `source-file` at runtime: outside the config table.
    Runtime,
    /// A plugin, by name (registered by the plugin host).
    Plugin(String),
}

impl BindingSource {
    /// Whether the binding came from the config table (defaults or a
    /// file), which `reload` replaces (R10).
    pub fn is_config(&self) -> bool {
        matches!(self, BindingSource::Default | BindingSource::File { .. })
    }
}

/// A key binding: the commands to run, in order, whether it repeats
/// (`bind -r`), and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// Each command's argv (R12: a binding may be a sequence), run as
    /// already split, so a literal `;` argument stays literal.
    pub cmds: Vec<Vec<String>>,
    pub repeat: bool,
    pub source: BindingSource,
    /// `desc` in the config, `bind-key -N` at runtime.
    pub desc: Option<String>,
    /// The config's stable `name` (spec §5.2).
    pub name: Option<String>,
    /// The language `cmds` are in (spec §4.5: a stored command keeps the
    /// dialect it was bound in). Grofe until the front end binds (FE.1).
    pub dialect: Dialect,
    /// The commands exactly as a tmux-dialect `list-keys` shows them, when
    /// they cannot be printed back from `cmds`: tmux's stock bindings, whose
    /// `{ }` lists `cmds` holds as strings.
    pub shown: Option<String>,
    /// The key menu's section (`group` in the config, `bind-key -G`).
    pub group: Option<String>,
}

impl Binding {
    /// A default-table binding: no description or name, grofe's dialect.
    pub fn new(cmds: Vec<Vec<String>>, repeat: bool, source: BindingSource) -> Binding {
        Binding {
            cmds,
            repeat,
            source,
            desc: None,
            name: None,
            dialect: Dialect::Grofe,
            shown: None,
            group: None,
        }
    }
}

/// Bindings by table name, then key.
#[derive(Debug, Clone, Default)]
pub struct KeyTables {
    pub(super) tables: BTreeMap<String, HashMap<Key, Binding>>,
    /// Keys removed by a runtime `unbind-key`, which `reload` keeps
    /// removed (R10).
    pub(super) runtime_unbound: Vec<(String, Key)>,
    /// Built by [`KeyTables::from_config`] from at least one binding.
    pub(super) configured: bool,
    /// Keys the config unbinds (a binding with no commands), which the
    /// tmux stock table leaves unbound too.
    pub(super) config_unbound: Vec<(String, Key)>,
    /// The default prefix table is tmux 3.7b's stock one (a server with
    /// tmux identity, [`KeyTables::use_tmux_stock`]); `reload` keeps it.
    pub(super) tmux_stock: bool,
}

impl KeyTables {
    /// Binds one command.
    pub fn bind(&mut self, table: &str, key: Key, argv: Vec<String>, repeat: bool) {
        self.bind_seq(table, key, vec![argv], repeat);
    }

    /// Binds a command sequence, run in order, as a default binding.
    pub fn bind_seq(&mut self, table: &str, key: Key, cmds: Vec<Vec<String>>, repeat: bool) {
        self.insert(
            table,
            key,
            Binding::new(cmds, repeat, BindingSource::Default),
        );
    }

    /// Binds `b` as it is. A runtime binding ends a runtime unbind of
    /// the same key.
    pub fn insert(&mut self, table: &str, key: Key, b: Binding) {
        if !b.source.is_config() {
            self.runtime_unbound
                .retain(|(t, k)| !(t == table && *k == key));
        }
        self.tables
            .entry(table.to_string())
            .or_default()
            .insert(key, b);
    }

    /// Remove the binding of exactly `key` (as spelled); whether one was
    /// there.
    pub fn unbind(&mut self, table: &str, key: &Key) -> bool {
        self.tables
            .get_mut(table)
            .is_some_and(|t| t.remove(key).is_some())
    }

    /// `unbind-key` at runtime: removed now and kept removed by `reload`.
    /// A key that was not bound is not recorded, so a later config
    /// binding of it still applies.
    pub fn unbind_runtime(&mut self, table: &str, key: &Key) {
        if self.unbind(table, key)
            && !self
                .runtime_unbound
                .iter()
                .any(|(t, k)| t == table && k == key)
        {
            self.runtime_unbound.push((table.to_string(), *key));
        }
    }

    /// Whether a runtime `unbind-key` holds `key` unbound over a `reload`.
    pub fn is_runtime_unbound(&self, table: &str, key: &Key) -> bool {
        self.runtime_unbound
            .iter()
            .any(|(t, k)| t == table && k == key)
    }

    /// Takes `key` out of the runtime's hands, so the next `reload` binds
    /// it from the config: a runtime binding of it and a runtime unbind go,
    /// and an unbound key gets its default binding until then (a reload that
    /// rebuilds no table keeps that).
    pub fn release_runtime(&mut self, table: &str, key: &Key) {
        if self
            .exact(table, key)
            .is_some_and(|b| !b.source.is_config())
        {
            self.unbind(table, key);
        }
        self.runtime_unbound
            .retain(|(t, k)| !(t == table && k == key));
        if self.exact(table, key).is_none()
            && let Some(b) = self.default_of(table, key)
        {
            self.insert(table, *key, b);
        }
    }

    /// The binding `key` has in `table` on a fresh server: grofe's default
    /// tables, made tmux's stock ones as this server's are.
    pub fn default_of(&self, table: &str, key: &Key) -> Option<Binding> {
        let mut stock = default_bindings();
        if self.tmux_stock {
            stock.use_tmux_stock();
        }
        stock.exact(table, key).cloned()
    }

    /// Every key bound in `table`, sorted by name (stable output).
    pub fn keys(&self, table: &str) -> Vec<Key> {
        let mut keys: Vec<Key> = self
            .tables
            .get(table)
            .map(|t| t.keys().copied().collect())
            .unwrap_or_default();
        keys.sort_by_cached_key(|k| k.to_string());
        keys
    }

    /// Whether a config binding (or unbinding) shaped these tables.
    pub fn is_configured(&self) -> bool {
        self.configured
    }

    /// Every table's name, sorted.
    pub fn table_names(&self) -> impl Iterator<Item = &str> {
        self.tables.keys().map(String::as_str)
    }

    /// The binding of exactly `key`, with no folding.
    pub fn exact(&self, table: &str, key: &Key) -> Option<&Binding> {
        self.tables.get(table)?.get(key)
    }

    /// The default table with the config's bindings over it (spec §5.1):
    /// each binding from `keys` replaces the default one, and one with no
    /// commands unbinds the key (R11).
    pub fn from_config(keys: &[crate::config::KeyBinding]) -> KeyTables {
        let mut t = default_bindings();
        t.configured = !keys.is_empty();
        for k in keys {
            if k.cmds.is_empty() {
                t.unbind(&k.table, &k.key);
                t.config_unbound.push((k.table.clone(), k.key));
                continue;
            }
            let source = match &k.source.file {
                Some(path) => BindingSource::File {
                    path: path.clone(),
                    line: k.source.line,
                },
                None => BindingSource::Default,
            };
            let mut b = Binding::new(k.cmds.clone(), k.repeat, source);
            b.desc = k.desc.clone();
            b.name = k.name.clone();
            b.group = k.group.clone();
            t.insert(&k.table, k.key, b);
        }
        t
    }

    /// `self`'s config bindings replaced by `fresh`'s (R10): runtime and
    /// plugin bindings, and runtime unbinds, carry over.
    pub fn with_config_from(&self, fresh: KeyTables) -> KeyTables {
        let mut t = fresh;
        if self.tmux_stock {
            t.use_tmux_stock();
        }
        for (table, key) in &self.runtime_unbound {
            t.unbind(table, key);
        }
        t.runtime_unbound = self.runtime_unbound.clone();
        for (table, bindings) in &self.tables {
            for (key, b) in bindings.iter().filter(|(_, b)| !b.source.is_config()) {
                t.insert(table, *key, b.clone());
            }
        }
        t
    }

    /// Binds one stock `root` row from [`TMUX_STOCK_ROOT`], in tmux's
    /// dialect and as tmux prints it.
    pub(super) fn insert_stock_root(&mut self, name: &str, text: &str) {
        // `list-keys` joins the commands with ` \; `, which a command
        // string spells `;`.
        let source = text.replace(" \\; ", " ; ");
        let (Ok(key), Ok(cmds)) = (parse_key(name), crate::cmd::split_command_string(&source))
        else {
            return;
        };
        let mut b = Binding::new(cmds, false, BindingSource::Default);
        b.dialect = Dialect::Tmux;
        b.shown = Some(text.to_string());
        self.insert(ROOT_TABLE, key, b);
    }

    /// Makes the default prefix table tmux 3.7b's stock one, for a server
    /// with tmux identity: grofe's default prefix bindings go, and each
    /// stock key the config and the runtime neither bound nor unbound gets
    /// tmux's binding, in tmux's dialect, with its note ([`TMUX_STOCK`]).
    /// A stock command grofe lacks (copy mode, buffers, marking, the clock)
    /// fails with the front end's reason when pressed.
    pub fn use_tmux_stock(&mut self) {
        self.tmux_stock = true;
        if let Some(t) = self.tables.get_mut(PREFIX_TABLE) {
            t.retain(|_, b| b.source != BindingSource::Default);
        }
        let unbound = |key: &Key| {
            self.config_unbound
                .iter()
                .chain(&self.runtime_unbound)
                .any(|(t, k)| t == PREFIX_TABLE && k == key)
        };
        let mut add = Vec::new();
        for (name, repeat, note, text) in TMUX_STOCK {
            let Ok(key) = parse_key(name) else {
                continue;
            };
            if unbound(&key) || self.exact(PREFIX_TABLE, &key).is_some() {
                continue;
            }
            let Ok(cmds) = crate::cmd::split_command_string(text) else {
                continue;
            };
            let mut b = Binding::new(cmds, *repeat, BindingSource::Default);
            b.desc = Some(note.to_string());
            b.dialect = Dialect::Tmux;
            b.shown = Some(text.to_string());
            add.push((key, b));
        }
        for (key, b) in add {
            self.insert(PREFIX_TABLE, key, b);
        }
        // grofe's own default `root` rows ([`GROFE_PANE_MENU`]) give way to
        // tmux's stock ones.
        if let Some(t) = self.tables.get_mut(ROOT_TABLE) {
            t.retain(|_, b| !(b.source == BindingSource::Default && b.dialect == Dialect::Grofe));
        }
        // The rest of the stock `root` rows: those the config and the
        // runtime neither bound nor unbound.
        let unbound_root = |key: &Key| {
            self.config_unbound
                .iter()
                .chain(&self.runtime_unbound)
                .any(|(t, k)| t == ROOT_TABLE && k == key)
        };
        let add: Vec<(&str, &str)> = TMUX_STOCK_ROOT
            .iter()
            .filter(|(name, _)| {
                parse_key(name)
                    .is_ok_and(|key| !unbound_root(&key) && self.exact(ROOT_TABLE, &key).is_none())
            })
            .copied()
            .collect();
        for (name, text) in add {
            self.insert_stock_root(name, text);
        }
        // The stock copy-mode-vi rows run as they are; those whose `{ }`
        // template `cmds` holds as a string show as tmux prints them.
        for (table, rows) in [
            (COPY_MODE_VI_TABLE, TMUX_COPY_MODE_VI_SHOWN),
            (COPY_MODE_TABLE, TMUX_COPY_MODE_EMACS_SHOWN),
        ] {
            let Some(t) = self.tables.get_mut(table) else {
                continue;
            };
            for (name, text) in rows {
                if let Ok(key) = parse_key(name)
                    && let Some(b) = t.get_mut(&key)
                    && b.source == BindingSource::Default
                {
                    b.shown = Some(text.to_string());
                }
            }
        }
    }

    /// Whether `table` exists: `root` always does, as in tmux; any other
    /// while something is bound in it (tmux drops a table whose last
    /// binding goes).
    pub fn exists(&self, table: &str) -> bool {
        table == ROOT_TABLE || self.tables.get(table).is_some_and(|t| !t.is_empty())
    }

    /// The binding for `key`: the one spelled exactly as `key`, else the
    /// one spelled as its folded form, else any whose folded form matches
    /// (the first by name, so the choice is stable).
    pub fn get(&self, table: &str, key: &Key) -> Option<&Binding> {
        let t = self.tables.get(table)?;
        if let Some(b) = t.get(key) {
            return Some(b);
        }
        let folded = key.folded();
        t.get(&folded).or_else(|| {
            t.iter()
                .filter(|(k, _)| k.folded() == folded)
                .min_by_key(|(k, _)| k.to_string())
                .map(|(_, b)| b)
        })
    }

    /// What a key pressed in `table` runs, as tmux 3.7b's key walk finds
    /// it: [`KeyTables::get`], else the table's `Any` binding (recorded:
    /// `Any` takes keys and mouse events alike, the prefix key still wins,
    /// and an exact binding wins over it).
    pub fn lookup(&self, table: &str, key: &Key) -> Option<&Binding> {
        self.get(table, key)
            .or_else(|| self.exact(table, &Key::new(KeyCode::Any, Mods::default())))
    }
}
