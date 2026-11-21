use super::*;

/// The tmux version a server with tmux identity reports (`#{version}`,
/// `tmux -V`'s number): the release the compat suite is recorded against.
pub(in crate::server::dispatch::options) const TMUX_COMPAT_VERSION: &str = "3.7b";

impl FormatVars for FormatCtx<'_> {
    fn get(&self, name: &str) -> Option<String> {
        use crate::compat::Identity;
        use crate::types::MouseMode;
        let m = self.model;
        if name.starts_with('@') || builtin(name).is_some() {
            let value = m.options.lookup(name, &self.option_chain())?;
            // A flag is 1 or 0 in a format (tmux's `options_to_string`
            // with `numeric`), so `#{?synchronize-panes,...}` is false off.
            return Some(match (builtin(name).map(|b| b.kind), value) {
                (Some(Kind::Bool), "on") => "1".to_string(),
                (Some(Kind::Bool), "off") => "0".to_string(),
                _ => value.to_string(),
            });
        }
        match name {
            "version" => {
                return Some(
                    match m.identity {
                        Identity::Grofe => env!("CARGO_PKG_VERSION"),
                        Identity::Tmux => TMUX_COMPAT_VERSION,
                    }
                    .to_string(),
                );
            }
            "grofe_version" => return Some(env!("CARGO_PKG_VERSION").to_string()),
            // Outside a loop; the loop's items say otherwise.
            "loop_last_flag" => return Some("0".to_string()),
            "server_sessions" => return Some(m.sessions.len().to_string()),
            "uid" => return Some(nix::unistd::getuid().as_raw().to_string()),
            "user" => {
                let user = nix::unistd::User::from_uid(nix::unistd::getuid()).ok()??;
                return Some(user.name);
            }
            "next_session_id" => return Some(m.next_session_id().to_string()),
            "tree_mode_format" => return Some(TREE_MODE_FORMAT.to_string()),
            "buffer_mode_format" => return Some(BUFFER_MODE_FORMAT.to_string()),
            "client_mode_format" => return Some(CLIENT_MODE_FORMAT.to_string()),
            "pid" => return Some(std::process::id().to_string()),
            "start_time" => {
                let since = m.started?.duration_since(std::time::UNIX_EPOCH).ok()?;
                return Some(since.as_secs().to_string());
            }
            // ponytail: stubs until the features exist: the status range under
            // the mouse, which only a mouse-bound command sees (TC.10).
            // `client_tty` is a client's (`ClientVars`), empty with none.
            "copy_cursor_word" | "pane_mode" | "mouse_status_range" | "client_tty"
            | "client_pid" => return Some(String::new()),
            "pane_in_mode" => return Some("0".to_string()),
            "pane_marked" => return Some(flag(m.marked_pane() == Some(self.pane))),
            "pane_marked_set" => return Some(flag(m.marked_pane().is_some())),
            "window_marked_flag" | "session_marked" => {
                let here = m.panes.get(&self.pane)?.window;
                let in_session = self.session_for(m.windows.get(&here)?);
                // The mark names the session and window link it was made in.
                let same = match m.marked_pane().and(m.marked) {
                    Some((session, _, _)) if name == "session_marked" => session == in_session,
                    Some((session, window, _)) => session == in_session && window == here,
                    None => false,
                };
                return Some(flag(same));
            }
            // The search string of a copy mode, which answers first.
            "pane_search_string" => return Some(String::new()),
            _ => {}
        }
        let p = m.panes.get(&self.pane)?;
        let w = m.windows.get(&p.window)?;
        let s = m.sessions.get(&self.session_for(w))?;
        let index_of = |id: &WindowId| {
            m.index_in(s.id, *id)
                .or_else(|| Some(m.windows.get(id)?.index))
        };
        Some(match name {
            "session_name" => s.name.clone(),
            "session_id" => s.id.to_string(),
            "window_index" => index_of(&w.id)?.to_string(),
            "window_name" => w.name.clone(),
            "window_id" => w.id.to_string(),
            "window_active" => flag(s.active == w.id),
            "window_zoomed_flag" => flag(w.zoomed),
            "window_width" => w.cols.to_string(),
            "window_bell_flag" => flag(w.bell_flag),
            "window_activity_flag" => flag(w.activity_flag),
            "window_silence_flag" => flag(w.silence_flag),
            "window_height" => w.rows.to_string(),
            "window_panes" => w.pane_ids().len().to_string(),
            "pane_floating_flag" => flag(w.floating(p.id).is_some()),
            "session_windows" => s.windows.len().to_string(),
            "session_created" => s
                .created
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
                .to_string(),
            "session_attached" => s.attached.to_string(),
            "session_path" => s.cwd.display().to_string(),
            "window_layout" => window_layout(w),
            "window_visible_layout" => window_visible_layout(w),
            "window_raw_flags" => window_flags(m, s, w),
            "pane_id" => p.id.to_string(),
            "pane_uid" => p.uid.clone(),
            "pane_index" => {
                let pos = w.pane_ids().iter().position(|id| *id == p.id)?;
                (pos as u32 + m.pane_base_index(w.id)).to_string()
            }
            "pane_active" => flag(w.active == p.id),
            "pane_pid" => p.handle.pid()?.to_string(),
            // The TCP ports the pane's processes listen on, comma-separated.
            "pane_ports" => crate::server::remote::join(&p.handle.ports()),
            "pane_current_command" => p.handle.current_command()?,
            "pane_current_argv" => p.handle.current_argv()?.join(" "),
            // Every member of the foreground group, one argv per line; the
            // sample holds only the leader's (a command reads them all,
            // `ServerVars::fresh`).
            "pane_foreground_argv" => procinfo::argv_lines(&[p.handle.current_argv()?]),
            "pane_current_path" => p
                .handle
                .current_path()
                .unwrap_or_else(|| p.cwd.clone())
                .display()
                .to_string(),
            // What the pane reported with OSC 7, as tmux shows it.
            "pane_path" => p.handle.reported_path().unwrap_or_default(),
            "pane_left" => match w.floating(p.id) {
                Some(f) => f.x.to_string(),
                None => pane_origin(w, p.id)?.0.to_string(),
            },
            "pane_top" => match w.floating(p.id) {
                Some(f) => f.y.to_string(),
                None => pane_origin(w, p.id)?.1.to_string(),
            },
            // tmux's names for the same corner, and the far one.
            "pane_x" => self.get("pane_left")?,
            "pane_y" => self.get("pane_top")?,
            "pane_right" => {
                let left: u32 = self.get("pane_left")?.parse().ok()?;
                (left + u32::from(p.cols)).saturating_sub(1).to_string()
            }
            "pane_bottom" => {
                let top: u32 = self.get("pane_top")?.parse().ok()?;
                (top + u32::from(p.rows)).saturating_sub(1).to_string()
            }
            // Whether the pane touches that edge of the window, as tmux's
            // format.c decides: a border status moves the top and bottom
            // edges in by the row it takes.
            "pane_at_left" => flag(self.get("pane_left")? == "0"),
            "pane_at_top" => {
                let top = if w.pane_status == PaneStatus::Top {
                    "1"
                } else {
                    "0"
                };
                flag(self.get("pane_top")? == top)
            }
            "pane_at_right" => {
                let left: u32 = self.get("pane_left")?.parse().ok()?;
                flag(left + u32::from(p.cols) >= u32::from(w.cols))
            }
            "pane_at_bottom" => {
                let top: u32 = self.get("pane_top")?.parse().ok()?;
                let edge = u32::from(w.rows) - u32::from(w.pane_status == PaneStatus::Bottom);
                flag(top + u32::from(p.rows) >= edge)
            }
            // The place in the stack, 0 on top: the floating panes, then
            // every tiled pane one below them all (tmux's z_index).
            "pane_z" => match w.floating(p.id) {
                Some(f) => w.floating.iter().filter(|o| o.z > f.z).count(),
                None => w.floating.len() + 1,
            }
            .to_string(),
            "pane_width" => p.cols.to_string(),
            "pane_height" => p.rows.to_string(),
            "pane_dead" => flag(p.dead.is_some()),
            // Spec §6: the pane's resume argv, quoted as
            // `pane_start_command`; the reason a resume is expected and
            // none is known; whether the pane came from a snapshot.
            "pane_resume" => p
                .resume
                .as_deref()
                .map(|argv| {
                    argv.iter()
                        .map(|a| quote::tmux_arg(a))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default(),
            "pane_resume_expected" => p.expect.clone().unwrap_or_default(),
            "pane_restored" => flag(p.restored),
            "pane_pipe" => flag(p.handle.pipe_open()),
            "pane_pipe_pid" => p.handle.pipe_pid()?.to_string(),
            // Empty for a live pane and for one a signal killed, as tmux.
            "pane_dead_status" => p.dead.flatten()?.to_string(),
            "pane_dead_time" => epoch_secs(p.dead_time?),
            "pane_start_command" => p
                .argv
                .as_deref()
                .map(|argv| {
                    argv.iter()
                        .map(|a| quote::tmux_arg(a))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default(),
            "alternate_on" => flag(p.handle.modes().alt_screen),
            "mouse_any_flag" => flag(p.handle.modes().mouse != MouseMode::Off),
            // 0 in a mode: `ServerVars::get`.
            "bracket_paste_flag" => flag(p.handle.modes().bracketed_paste),
            "history_size" => p.handle.history_size().to_string(),
            "history_limit" => m.history_limit(s.id).to_string(),
            // tmux's grid: 40 bytes a line, history and screen.
            // ponytail: tmux adds 5 bytes for each cell a line has
            // allocated, which depends on how its grid grew; left out.
            "history_bytes" => ((p.handle.history_size() + usize::from(p.rows)) * 40).to_string(),
            "window_flags" => window_flags(m, s, w),
            "pane_format" => flag(self.level == Level::Pane),
            "window_format" => flag(self.level == Level::Window),
            "session_format" => flag(self.level == Level::Session),
            "window_last_flag" => flag(s.last_windows.last() == Some(&w.id)),
            "pane_last" => flag(w.last_pane() == Some(p.id)),
            // tmux's `window_pane_printable_flags`.
            "pane_flags" => [
                (w.active == p.id, '*'),
                (w.last_pane() == Some(p.id), '-'),
                (w.zoomed && w.active == p.id, 'Z'),
                (w.floating(p.id).is_some(), 'F'),
            ]
            .into_iter()
            .filter_map(|(on, c)| on.then_some(c))
            .collect(),
            "window_start_flag" => flag(s.windows.first() == Some(&w.id)),
            "window_end_flag" => flag(s.windows.last() == Some(&w.id)),
            "window_activity" => epoch_secs(w.activity),
            "session_activity" => epoch_secs(std::time::SystemTime::now() - s.activity.elapsed()),
            "active_window_index" => index_of(&s.active)?.to_string(),
            "last_window_index" => s.windows.iter().filter_map(index_of).max()?.to_string(),
            "session_many_attached" => flag(s.attached > 1),
            "pane_zoomed_flag" => flag(w.zoomed && w.active == p.id),
            // The sessions (or groups, once each) whose current window it is.
            "window_active_sessions" => self.active_sessions(w.id).len().to_string(),
            "window_active_sessions_list" => self.active_sessions(w.id).join(","),
            "window_linked_sessions_list" => w
                .links
                .iter()
                .filter_map(|(l, _)| m.sessions.get(l).map(|s| s.name.as_str()))
                .collect::<Vec<_>>()
                .join(","),
            "session_grouped" => flag(m.group_of(s.id).is_some()),
            "session_group" => m.group_of(s.id)?.name.clone(),
            "session_group_size" => m.group_of(s.id)?.members.len().to_string(),
            "session_group_list" => m
                .group_of(s.id)?
                .members
                .iter()
                .filter_map(|l| m.sessions.get(l).map(|s| s.name.as_str()))
                .collect::<Vec<_>>()
                .join(","),
            // Each kind of alert once, windows in index order.
            "session_alert" => {
                let windows: Vec<_> = s
                    .windows
                    .iter()
                    .filter_map(|id| m.windows.get(id))
                    .collect();
                [
                    ('#', windows.iter().any(|w| w.activity_flag)),
                    ('!', windows.iter().any(|w| w.bell_flag)),
                    ('~', windows.iter().any(|w| w.silence_flag)),
                ]
                .into_iter()
                .filter_map(|(c, on)| on.then_some(c))
                .collect()
            }
            "session_activity_flag" | "session_bell_flag" | "session_silence_flag" => {
                let any = s
                    .windows
                    .iter()
                    .filter_map(|id| m.windows.get(id))
                    .any(|w| match name {
                        "session_activity_flag" => w.activity_flag,
                        "session_bell_flag" => w.bell_flag,
                        _ => w.silence_flag,
                    });
                flag(any)
            }
            // `idx` and its alerts, for each window that has any.
            "session_alerts" => s
                .windows
                .iter()
                .filter_map(|id| m.windows.get(id))
                .filter(|w| w.activity_flag || w.bell_flag || w.silence_flag)
                .map(|w| {
                    format!(
                        "{}{}{}{}",
                        index_of(&w.id).unwrap_or(w.index),
                        if w.activity_flag { "#" } else { "" },
                        if w.bell_flag { "!" } else { "" },
                        if w.silence_flag { "~" } else { "" },
                    )
                })
                .collect::<Vec<_>>()
                .join(","),
            // The windows before this one, most recent first.
            "session_stack" => std::iter::once(index_of(&s.active)?)
                .chain(s.last_windows.iter().rev().filter_map(index_of))
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(","),
            "window_stack_index" => s
                .last_windows
                .iter()
                .rev()
                .position(|id| *id == w.id)
                .map_or(0, |at| at + 1)
                .to_string(),
            "pane_input_off" => {
                return Some(flag(m.panes.get(&self.pane)?.input_off));
            }
            "pane_synchronized" => {
                flag(m.options.lookup("synchronize-panes", &self.option_chain()) == Some("on"))
            }
            "pane_start_path" => p.cwd.display().to_string(),
            "window_linked" => flag(w.links.len() > 1),
            "window_linked_sessions" => m.linked_units(w.id).to_string(),
            // `client_width`/`client_height` are `ClientVars`', `host` and
            // `host_short` are `ServerVars`'.
            // The emulator's screen state (`cursor_x`, `wrap_flag`, ...).
            _ => return p.handle.term_var(name),
        })
    }

    fn loop_items(&self, kind: LoopKind, sort: LoopSort) -> Option<Vec<LoopItem<'_>>> {
        let specs = loop_specs(self.model, self.pane, self.session, None, kind, sort)?;
        Some(
            specs
                .into_iter()
                .map(|s| LoopItem {
                    vars: Box::new(in_session(
                        FormatCtx::at(self.model, s.pane, s.level),
                        s.session,
                    )),
                    pane: Some(s.pane),
                    active: s.active,
                    extras: s.extras,
                })
                .collect(),
        )
    }

    fn name_exists(&self, kind: NameKind, name: &str) -> Option<bool> {
        let m = self.model;
        match kind {
            NameKind::Session => Some(m.sessions.values().any(|s| s.name == name)),
            NameKind::Window => {
                let p = m.panes.get(&self.pane)?;
                let s = m.sessions.get(&m.windows.get(&p.window)?.session)?;
                Some(
                    s.windows
                        .iter()
                        .filter_map(|id| m.windows.get(id))
                        .any(|w| w.name == name),
                )
            }
        }
    }

    fn pane_search(&self, term: &str, regex: bool, ignore: bool) -> Option<u32> {
        let p = self.model.panes.get(&self.pane)?;
        let visible = crate::types::CaptureOpts {
            start: None,
            end: None,
            escapes: false,
        };
        let text = p.handle.capture(&visible);
        Some(crate::cmd::format::search_lines(
            text.lines(),
            term,
            regex,
            ignore,
        ))
    }

    fn reexpand(&self, text: &str) -> String {
        use crate::compat::Identity;
        match self.model.identity {
            Identity::Tmux => {
                crate::compat::tmux::formats::to_native(
                    text,
                    crate::compat::tmux::formats::Jobs::Empty,
                )
                .value
            }
            Identity::Grofe => text.to_string(),
        }
    }
}

/// tmux 3.7b's `window_tree_mode.default_format`, `window_buffer_mode`'s and
/// `window_client_mode`'s (`window-tree.c`, `window-buffer.c`,
/// `window-client.c`): what `#{tree_mode_format}` and the like are.
pub(in crate::server::dispatch::options) const TREE_MODE_FORMAT: &str = concat!(
    "#{?pane_format,",
    "#{?pane_marked,#[reverse],}#{?pane_floating_flag,#[italics],}",
    "#{pane_current_command}#{pane_flags}",
    "#{?#{&&:#{pane_title},#{!=:#{pane_title},#{host_short}}},: \"#{pane_title}\",}",
    ",window_format,",
    "#{?window_marked_flag,#[reverse],}",
    "#{window_name}#{window_flags}",
    "#{?#{&&:#{==:#{window_panes},1},#{&&:#{pane_title},#{!=:#{pane_title},#{host_short}}}},: \"#{pane_title}\",}",
    ",",
    "#{session_windows} windows",
    "#{?session_grouped, (group #{session_group}: #{session_group_list}),}",
    "#{?session_attached, (attached),}",
    "}"
);
pub(in crate::server::dispatch::options) const BUFFER_MODE_FORMAT: &str =
    "#{t/p:buffer_created}: #{buffer_sample}";
pub(in crate::server::dispatch::options) const CLIENT_MODE_FORMAT: &str =
    "#{t/p:client_activity}: session #{session_name}";
