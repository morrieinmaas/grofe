use super::*;

/// tmux's `DEFAULT_XPIXEL` and `DEFAULT_YPIXEL`: a window's cell size when
/// no client said its own.
const DEFAULT_CELL_PX: (u16, u16) = (16, 32);

impl ServerVars<'_> {
    /// The variables of the client the formats are for and its view of the
    /// window (issue 4): `None` for a name that is not one of them, or when
    /// tmux 3.7b has no value (no client, a window that fits).
    pub(in crate::server::dispatch::options) fn client_var(&self, name: &str) -> Option<String> {
        let model = &self.state.model;
        let window = model.panes.get(&self.pane)?.window;
        match name {
            // The clients whose session shows the window, in client order.
            "window_active_clients" | "window_active_clients_list" => {
                let names: Vec<&str> = self
                    .state
                    .clients
                    .values()
                    .filter(|c| model.sessions.get(&c.session).map(|s| s.active) == Some(window))
                    .map(|c| c.name.as_str())
                    .collect();
                match name {
                    "window_active_clients" => Some(names.len().to_string()),
                    _ => (!names.is_empty()).then(|| names.join(",")),
                }
            }
            "window_cell_width" | "window_cell_height" => {
                let session = self.session_id()?;
                let (w, h) = clients::cell_px(self.state, session).unwrap_or(DEFAULT_CELL_PX);
                Some(if name == "window_cell_width" { w } else { h }.to_string())
            }
            // The status row under the mouse, on the status line only.
            "mouse_status_line" => {
                let m = self.state.mouse.current()?;
                m.range.as_ref()?;
                Some(m.pane_cell?.1.to_string())
            }
            "copy_cursor_line" | "copy_cursor_hyperlink" => {
                let cursor = |n| {
                    crate::server::copy::format_var(self.state, self.pane, n)?
                        .parse()
                        .ok()
                };
                let (x, y) = (cursor("copy_cursor_x")?, cursor("copy_cursor_y")?);
                let what = match name {
                    "copy_cursor_line" => "mouse_line",
                    _ => "mouse_hyperlink",
                };
                Some(
                    crate::server::copy::mouse_text(self.state, self.pane, x, y, what)
                        .unwrap_or_default(),
                )
            }
            _ => self.attached_var(name),
        }
    }

    /// The variables only a client has: whether it shows the formats'
    /// session, its cell size, and whether the window it shows is bigger
    /// than it and where its view of it starts.
    fn attached_var(&self, name: &str) -> Option<String> {
        let client_name = self.inner.vars().get("client_name")?;
        let (&id, c) = self
            .state
            .clients
            .iter()
            .find(|(_, c)| c.name == client_name)?;
        match name {
            "session_active" => Some(flag(Some(c.session) == self.session_id())),
            "client_cell_width" | "client_cell_height" => {
                let (w, h) = c.caps.cell_px.unwrap_or((0, 0));
                Some(if name == "client_cell_width" { w } else { h }.to_string())
            }
            "window_bigger" | "window_offset_x" | "window_offset_y" => {
                let (_, fits) = clients::window_fits(self.state, id, c)?;
                match name {
                    "window_bigger" => Some(flag(!fits)),
                    _ if fits => None,
                    "window_offset_x" => Some(c.viewport.0.to_string()),
                    _ => Some(c.viewport.1.to_string()),
                }
            }
            _ => None,
        }
    }
}
