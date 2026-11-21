use super::*;

/// Format variables that need the server state, not only the model
/// (`socket_path`, `pane_title`, `pane_tty`, TC.11's `host` and
/// `host_short`, the mouse's, and a running hook's `#{hook*}`), over
/// `inner` for the rest.
/// Built where dispatch holds `&State`, the way `ClientVars` layers the
/// client's variables.
pub struct ServerVars<'a> {
    pub(in crate::server::dispatch::options) state: &'a State,
    pub(in crate::server::dispatch::options) pane: PaneId,
    pub(in crate::server::dispatch::options) inner: Inner<'a>,
    /// Read the foreground process now, not from the last sample.
    pub(in crate::server::dispatch::options) fresh: bool,
    /// What a fresh expansion has read so far: each is read at most once
    /// per expansion, and the members only when a format asks for them.
    pub(in crate::server::dispatch::options) group: OnceCell<Option<i32>>,
    pub(in crate::server::dispatch::options) leader: OnceCell<ProcSample>,
    pub(in crate::server::dispatch::options) members: OnceCell<Vec<Vec<String>>>,
}

impl<'a> ServerVars<'a> {
    /// The status line's and hooks' form: the foreground process comes
    /// from the pane's 1 s sample, so a redraw costs no syscalls for it.
    pub fn new(state: &'a State, pane: PaneId, inner: &'a dyn FormatVars) -> ServerVars<'a> {
        ServerVars {
            state,
            pane,
            inner: Inner::Ref(inner),
            fresh: false,
            group: OnceCell::new(),
            leader: OnceCell::new(),
            members: OnceCell::new(),
        }
    }

    /// A command's form (a binding's conditional, `display-message`, `set
    /// -F`, `list-* -F`): `pane_current_command`, `pane_current_argv` and
    /// `pane_foreground_argv` read the pane's foreground process group at
    /// this moment, with syscalls and no fork, so a key pressed just after
    /// a program starts or exits sees the one in front.
    pub fn fresh(state: &'a State, pane: PaneId, inner: &'a dyn FormatVars) -> ServerVars<'a> {
        ServerVars {
            fresh: true,
            ..ServerVars::new(state, pane, inner)
        }
    }

    pub(in crate::server::dispatch::options) fn foreground(&self, name: &str) -> Option<String> {
        let group = *self.group.get_or_init(|| {
            let p = self.state.model.panes.get(&self.pane)?;
            p.handle.foreground_group()
        });
        let group = group?;
        let leader = || self.leader.get_or_init(|| procinfo::leader(group));
        match name {
            "pane_current_command" => leader().command.clone(),
            "pane_current_argv" => leader().argv.as_ref().map(|a| a.join(" ")),
            _ => {
                let members = self.members.get_or_init(|| procinfo::members(group));
                (!members.is_empty()).then(|| procinfo::argv_lines(members))
            }
        }
    }
}

impl FormatVars for ServerVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        // The clock is the newest mode of a pane showing it.
        if let Some(v) = crate::server::clock::format_var(self.state, self.pane, name) {
            return Some(v);
        }
        // Copy mode's variables while the pane is in it (TM.1).
        if let Some(v) = crate::server::copy::format_var(self.state, self.pane, name) {
            return Some(v);
        }
        // A chooser open over the pane is a mode too (TP.9b).
        if let Some(v) = crate::server::tree::format_var(self.state, self.pane, name) {
            return Some(v);
        }
        // tmux 3.7b reads it from the screen on view, so a pane in any mode
        // (whatever answers `pane_in_mode`) is 0 (TM.7).
        if name == "bracket_paste_flag"
            && (crate::server::clock::format_var(self.state, self.pane, "pane_in_mode").is_some()
                || crate::server::copy::format_var(self.state, self.pane, "pane_in_mode")
                    .as_deref()
                    == Some("1")
                || crate::server::tree::format_var(self.state, self.pane, "pane_in_mode").is_some())
        {
            return Some("0".to_string());
        }
        // The mouse event a mouse key's commands run for, and the view (TC.10).
        if let Some(v) = crate::server::mouse::format_var(self.state, name) {
            return Some(v);
        }
        // The newest automatic buffer's `#{buffer_*}` (TM.4).
        if let Some(v) = crate::server::buffers::format_var(self.state, name) {
            return Some(v);
        }
        // Inside a hook, its `#{hook*}` variables (TC.5).
        let hook = crate::server::hooks::current_formats(self.state);
        if let Some(v) = hook.and_then(|f| f.get(name)) {
            return Some(v);
        }
        match name {
            "socket_path" => Some(self.state.socket.display().to_string()),
            "pane_title" => Some(self.state.passthrough.title(self.pane)),
            "host" => Some(crate::server::passthrough::hostname().to_string()),
            // tmux: the host name up to its first dot.
            "host_short" => {
                let host = crate::server::passthrough::hostname();
                Some(host.split('.').next().unwrap_or(host).to_string())
            }
            "pane_tty" => {
                let p = self.state.model.panes.get(&self.pane)?;
                Some(p.handle.tty_path()?.display().to_string())
            }
            "pane_current_command" | "pane_current_argv" | "pane_foreground_argv" if self.fresh => {
                self.foreground(name)
                    .or_else(|| self.inner.vars().get(name))
            }
            "session_attached_list" => self.attached_list(),
            "agent_kind"
            | "agent_state"
            | "agent_msg"
            | "agent_blocked_kind"
            | "agent_since"
            | "agent_unseen"
            | "agent_attention"
            | "window_agent_state"
            | "session_agent_state"
            | "agents_total"
            | "agents_blocked"
            | "agents_working"
            | "agents_attention"
            | "agents_badge" => Some(self.agent_var(name)),
            "notifications_unread" => Some(self.state.notify.unread().to_string()),
            "session_group_attached"
            | "session_group_attached_list"
            | "session_group_many_attached" => {
                let session = self.session_id()?;
                self.state.model.group_of(session)?;
                let names = self.group_attached()?;
                Some(match name {
                    "session_group_attached" => names.len().to_string(),
                    "session_group_many_attached" => flag(names.len() > 1),
                    _ => names.join(","),
                })
            }
            _ => self.inner.vars().get(name),
        }
    }

    fn loop_items(&self, kind: LoopKind, sort: LoopSort) -> Option<Vec<LoopItem<'_>>> {
        if kind == LoopKind::Clients {
            return self.client_items(sort);
        }
        let items = self.inner.vars().loop_items(kind, sort)?;
        Some(
            items
                .into_iter()
                .map(|it| {
                    let pane = it.pane.unwrap_or(self.pane);
                    LoopItem {
                        vars: Box::new(self.rebuild(Inner::Own(it.vars), pane)),
                        pane: Some(pane),
                        active: it.active,
                        extras: it.extras,
                    }
                })
                .collect(),
        )
    }

    fn name_exists(&self, kind: NameKind, name: &str) -> Option<bool> {
        self.inner.vars().name_exists(kind, name)
    }

    fn pane_search(&self, term: &str, regex: bool, ignore: bool) -> Option<u32> {
        self.inner.vars().pane_search(term, regex, ignore)
    }

    fn reexpand(&self, text: &str) -> String {
        self.inner.vars().reexpand(text)
    }
}

impl<'a> ServerVars<'a> {
    /// This layer over other variables and the pane they read: what a
    /// loop's item sees.
    pub(in crate::server::dispatch::options) fn rebuild<'x>(
        &'x self,
        inner: Inner<'x>,
        pane: PaneId,
    ) -> ServerVars<'x> {
        ServerVars {
            state: self.state,
            pane,
            inner,
            fresh: self.fresh,
            group: OnceCell::new(),
            leader: OnceCell::new(),
            members: OnceCell::new(),
        }
    }

    /// `#{session_attached_list}`: the names of the clients showing the
    /// pane's session, comma-joined.
    pub(in crate::server::dispatch::options) fn attached_list(&self) -> Option<String> {
        let session = self.session_id()?;
        let names: Vec<&str> = self
            .state
            .clients
            .values()
            .filter(|c| c.session == session)
            .map(|c| c.name.as_str())
            .collect();
        Some(names.join(","))
    }

    /// The agent variables of spec §4.1; empty, not missing, when there is
    /// nothing to show.
    pub(in crate::server::dispatch::options) fn agent_var(&self, name: &str) -> String {
        use crate::server::agents::{self, AgentState, Rank};
        let agents = &self.state.agents;
        let needs_you = |a: &agents::Agent| agents::rank(a) <= Rank::Unseen;
        let counts = |keep: &dyn Fn(&agents::Agent) -> bool| {
            agents.iter().filter(|(_, a)| keep(a)).count().to_string()
        };
        match name {
            "agents_total" => return counts(&|_| true),
            "agents_blocked" => return counts(&|a| a.state == AgentState::Blocked),
            "agents_working" => return counts(&|a| a.state == AgentState::Working),
            "agents_attention" => return counts(&needs_you),
            "agents_badge" => {
                let glyph = |n: &str| self.inner.vars().get(n).unwrap_or_default();
                let mut parts: Vec<String> = [
                    (Rank::Blocked, "agent-glyph-blocked"),
                    (Rank::Error, "agent-glyph-error"),
                    (Rank::Unseen, "agent-glyph-unseen"),
                ]
                .iter()
                .filter_map(|(rank, option)| {
                    let n = agents
                        .iter()
                        .filter(|(_, a)| agents::rank(a) == *rank)
                        .count();
                    (n > 0).then(|| format!("{}{n}", glyph(option)))
                })
                .collect();
                if self.state.notify.muted(std::time::Instant::now()) {
                    parts.push(glyph("notify-muted-glyph"));
                }
                return parts.join(" ");
            }
            "window_agent_state" | "session_agent_state" => {
                let model = &self.state.model;
                let Some(window) = model.panes.get(&self.pane).map(|p| p.window) else {
                    return String::new();
                };
                let windows = if name == "window_agent_state" {
                    vec![window]
                } else {
                    let session = self
                        .session_id()
                        .or_else(|| model.windows.get(&window).map(|w| w.session));
                    match session.and_then(|s| model.sessions.get(&s)) {
                        Some(s) => s.windows.clone(),
                        None => vec![window],
                    }
                };
                let best = agents
                    .iter()
                    .filter(|(p, _)| {
                        model
                            .panes
                            .get(p)
                            .is_some_and(|pane| windows.contains(&pane.window))
                    })
                    .map(|(_, a)| agents::rank(a))
                    .min();
                return best.map(|r| r.as_str().to_string()).unwrap_or_default();
            }
            _ => {}
        }
        let Some(a) = agents.get(self.pane) else {
            return String::new();
        };
        match name {
            "agent_kind" => a.kind.clone().unwrap_or_default(),
            "agent_state" => a.state.as_str().to_string(),
            "agent_msg" => a.msg.clone().unwrap_or_default(),
            "agent_blocked_kind" => a.blocked.clone().unwrap_or_default(),
            "agent_since" => std::time::Instant::now()
                .saturating_duration_since(a.since)
                .as_secs()
                .to_string(),
            "agent_unseen" => flag(a.unseen),
            _ => flag(needs_you(a)),
        }
    }

    /// The session the formats are for (`#{session_id}`).
    pub(in crate::server::dispatch::options) fn session_id(&self) -> Option<SessionId> {
        let id = self.inner.vars().get("session_id")?;
        Some(SessionId(id.strip_prefix('$')?.parse().ok()?))
    }

    /// The clients showing any session of the session's group, else of the
    /// session: names, in client order.
    pub(in crate::server::dispatch::options) fn group_attached(&self) -> Option<Vec<&str>> {
        let session = self.session_id()?;
        let m = &self.state.model;
        let mut sessions = m.group_peers(session);
        sessions.insert(0, session);
        Some(
            self.state
                .clients
                .values()
                .filter(|c| sessions.contains(&c.session))
                .map(|c| c.name.as_str())
                .collect(),
        )
    }

    /// `#{L:}`: every client, each over the pane this layer reads.
    pub(in crate::server::dispatch::options) fn client_items(
        &self,
        sort: LoopSort,
    ) -> Option<Vec<LoopItem<'_>>> {
        let model = &self.state.model;
        let mut all: Vec<&clients::ClientEntry> = self.state.clients.values().collect();
        sort.apply(&mut all, |order, a, b| {
            let first = match order {
                SortOrder::Name => a.name.as_bytes().cmp(b.name.as_bytes()),
                SortOrder::Activity => b.activity_time.cmp(&a.activity_time),
                _ => std::cmp::Ordering::Equal,
            };
            first.then_with(|| a.name.as_bytes().cmp(b.name.as_bytes()))
        });
        Some(
            all.into_iter()
                .map(|c| {
                    let client =
                        clients::ClientVars::new(model, c, Some(FormatCtx::new(model, self.pane)));
                    LoopItem {
                        vars: Box::new(self.rebuild(Inner::Own(Box::new(client)), self.pane)),
                        pane: Some(self.pane),
                        active: false,
                        extras: Vec::new(),
                    }
                })
                .collect(),
        )
    }
}
