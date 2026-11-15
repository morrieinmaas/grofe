use super::*;

/// The session options routing depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeySettings {
    /// `prefix`; `None` is tmux's `None` (no prefix key).
    pub prefix: Option<Key>,
    /// `prefix2`, a second prefix key; `None` is tmux's `None`.
    pub prefix2: Option<Key>,
    /// `repeat-time`; zero turns repeat off.
    pub repeat_time: Duration,
    /// `initial-repeat-time`: the first press of a repeating key waits this
    /// long instead, unless zero.
    pub initial_repeat_time: Duration,
    /// `prefix-timeout`: how long the prefix table waits for a key; zero
    /// waits for ever.
    pub prefix_timeout: Duration,
}

impl KeySettings {
    /// Whether `key` is the prefix or the second prefix.
    pub fn is_prefix(&self, key: &Key) -> bool {
        [self.prefix, self.prefix2]
            .into_iter()
            .flatten()
            .any(|p| p.matches(key))
    }
}

impl Default for KeySettings {
    fn default() -> KeySettings {
        KeySettings {
            prefix: Some(Key::new(KeyCode::Char('b'), ctrl())),
            prefix2: None,
            repeat_time: Duration::from_millis(500),
            initial_repeat_time: Duration::ZERO,
            prefix_timeout: Duration::ZERO,
        }
    }
}

/// One client's place in the key tables.
#[derive(Debug, Clone)]
pub struct KeyState {
    pub(super) table: String,
    /// Set while a repeating binding's `repeat-time` runs.
    pub(super) repeat_until: Option<Instant>,
    /// The key that last started or kept a repeat.
    pub(super) repeat_key: Option<Key>,
    /// Set while the prefix table waits, under `prefix-timeout`.
    pub(super) prefix_until: Option<Instant>,
}

impl Default for KeyState {
    fn default() -> KeyState {
        KeyState {
            table: ROOT_TABLE.to_string(),
            repeat_until: None,
            repeat_key: None,
            prefix_until: None,
        }
    }
}

impl KeyState {
    pub fn new() -> KeyState {
        KeyState::default()
    }

    /// The current table (`client_key_table`).
    pub fn table(&self) -> &str {
        &self.table
    }

    /// Switch table (`switch-client -T`), ending any repeat.
    pub fn set_table(&mut self, name: &str) {
        self.table = name.to_string();
        self.repeat_until = None;
        self.prefix_until = None;
    }

    /// When the running repeat or the wait for a key after the prefix
    /// ends, if one is running.
    pub fn deadline(&self) -> Option<Instant> {
        [self.repeat_until, self.prefix_until]
            .into_iter()
            .flatten()
            .min()
    }

    /// End an expired repeat, or a prefix nobody followed within
    /// `prefix-timeout`; whether the table changed (status redraw).
    pub fn expire(&mut self, now: Instant) -> bool {
        let due = |t: Option<Instant>| t.is_some_and(|d| now >= d);
        if due(self.repeat_until) || due(self.prefix_until) {
            self.set_table(ROOT_TABLE);
            true
        } else {
            false
        }
    }

    /// Route one decoded input for a client whose active pane has `modes`.
    pub fn route(
        &mut self,
        input: &Input,
        now: Instant,
        tables: &KeyTables,
        settings: &KeySettings,
        modes: &InputModes,
    ) -> Routed {
        match input {
            Input::Key(key) => self.route_key(key, now, tables, settings, modes),
            // Never matched; like tmux 3.7b, a paste also ends a pending
            // prefix or repeat (recorded: prefix, then a paste, leaves the
            // client in root).
            Input::Paste { data, start, end } => {
                self.set_table(ROOT_TABLE);
                Routed::ToPane(encode_paste(data, *start, *end, modes))
            }
            // The mouse hook (`server::mouse`) routes the mouse: mouse keys,
            // the built-in model and forwarding. What reaches the tables
            // (`mouse off`) is dropped.
            Input::Mouse(_) => Routed::Consumed,
            Input::Focus(_) if !modes.focus => Routed::Consumed,
            Input::Focus(true) => Routed::ToPane(b"\x1b[I".to_vec()),
            Input::Focus(false) => Routed::ToPane(b"\x1b[O".to_vec()),
        }
    }

    /// tmux 3.7b's `server_client_key_callback` table walk.
    pub(super) fn route_key(
        &mut self,
        key: &Key,
        now: Instant,
        tables: &KeyTables,
        settings: &KeySettings,
        modes: &InputModes,
    ) -> Routed {
        let key = *key;
        self.expire(now);
        let mut repeating = self.repeat_until.is_some();
        let mut first = self.table.clone();
        loop {
            // The prefix always wins, unless already in the prefix table.
            if settings.is_prefix(&key) && self.table != PREFIX_TABLE {
                self.set_table(PREFIX_TABLE);
                self.prefix_until =
                    (!settings.prefix_timeout.is_zero()).then(|| now + settings.prefix_timeout);
                return Routed::Consumed;
            }
            if let Some(b) = tables.lookup(&self.table, &key) {
                if repeating && !b.repeat {
                    // A non-repeating key ends the repeat and is tried
                    // again from root, as if typed fresh.
                    self.set_table(ROOT_TABLE);
                    repeating = false;
                    first = ROOT_TABLE.to_string();
                    continue;
                }
                if b.repeat && !settings.repeat_time.is_zero() {
                    // As `server_client_repeat_time`: the first press of a
                    // key, or a different one, waits `initial-repeat-time`.
                    let first_press = !repeating || self.repeat_key != Some(key);
                    let wait = if first_press && !settings.initial_repeat_time.is_zero() {
                        settings.initial_repeat_time
                    } else {
                        settings.repeat_time
                    };
                    self.repeat_until = Some(now + wait);
                    self.repeat_key = Some(key);
                } else {
                    self.set_table(ROOT_TABLE);
                }
                return Routed::Bound(b.cmds.clone(), b.dialect);
            }
            if self.table != ROOT_TABLE || repeating {
                self.set_table(ROOT_TABLE);
                if repeating {
                    first = ROOT_TABLE.to_string();
                }
                repeating = false;
                continue;
            }
            // Unbound after a prefix or in a custom table: dropped.
            if first != ROOT_TABLE {
                return Routed::Consumed;
            }
            // D4, recorded on tmux 3.7b: back-tab typed with ctrl or meta
            // (kitty `9;4u`, `\e\e[Z`) reaches a legacy pane as nothing,
            // though `send-keys M-BTab` sends `\e[Z`.
            let legacy = modes.kitty_flags & (1 | 8) == 0;
            if legacy && key.code == KeyCode::BackTab && (key.mods.alt || key.mods.ctrl) {
                return Routed::Consumed;
            }
            let bytes = encode(&key, modes);
            return if bytes.is_empty() {
                Routed::Consumed
            } else {
                Routed::ToPane(bytes)
            };
        }
    }
}
