//! Toasts (agents spec §6.3).
//!
//! The newest unread notification, when its state is one that toasts and it
//! is younger than `notify.toast-time`, is drawn as a small box in the top
//! right of a client's view, unless that client is looking at the pane.
//! Nothing is stored: [`shown`] derives the toast from `State.notify` on
//! every draw, and [`deadline`] says when it stops being shown.

use std::time::Instant;

use unicode_width::UnicodeWidthChar;

use super::dispatch::agent;
use super::dispatch::options::{FormatCtx, ServerVars};
use super::input::{Input, InputHook, MouseButton, MouseKind, Routed};
use super::listdraw::{Rows, draw_list};
use super::listview::ListState;
use super::notify::Notification;
use super::pane::{CellFlags, SnapCell};
use super::render::{Overlay, OverlayCtx, Screen};
use super::status::Style;
use super::{State, clients};
use crate::cmd::expand;
use crate::types::{ClientId, Rect};

/// Widest toast, in cells.
const MAX_WIDTH: u16 = 48;
/// Border, two text lines, border.
const HEIGHT: u16 = 4;

/// The notification to toast on `client` now, if any.
fn shown(state: &State, client: ClientId, now: Instant) -> Option<&Notification> {
    let n = state.notify.newest_unread()?;
    let cfg = &state.notify.cfg;
    if !cfg.toast.iter().any(|s| *s == n.state)
        || now.saturating_duration_since(n.at) >= cfg.toast_time
        || state.notify.muted(now)
    {
        return None;
    }
    let session = state
        .model
        .sessions
        .get(&state.clients.get(&client)?.session)?;
    let looking = state.model.windows.get(&session.active)?.active == n.pane;
    (!looking).then_some(n)
}

/// Where the toast box goes on a client of `size`, below `top` rows.
fn area(size: (u16, u16), top: u16) -> Option<Rect> {
    let cols = size.0.saturating_sub(2).min(MAX_WIDTH);
    if cols < 6 || size.1 < top + HEIGHT {
        return None;
    }
    Some(Rect {
        x: size.0 - cols - 1,
        y: top,
        cols,
        rows: HEIGHT,
    })
}

/// When the toast shown to `client` expires, so the server redraws then.
pub fn deadline(state: &State, client: ClientId) -> Option<Instant> {
    let n = shown(state, client, Instant::now())?;
    Some(n.at + state.notify.cfg.toast_time)
}

fn glyph(state: &str) -> &'static str {
    match state {
        "blocked" => "⏸",
        "error" => "✗",
        "done" => "✓",
        _ => "●",
    }
}

fn target_label(state: &State, n: &Notification) -> String {
    let vars = FormatCtx::new(&state.model, n.pane);
    expand(
        "#{session_name}:#{window_index}.#{pane_index}",
        &ServerVars::fresh(state, n.pane, &vars),
    )
}

/// Writes `text` from `x`, never past column `end` (exclusive). A wide
/// character that does not fit whole is dropped.
fn put_text(screen: &mut Screen, mut x: usize, y: usize, end: usize, text: &str) {
    let style = Style::default();
    let cols = usize::from(screen.cols);
    let put = |screen: &mut Screen, x: usize, c: char, flags: CellFlags| {
        if x < cols && y < usize::from(screen.rows) {
            screen.cells[y * cols + x] = SnapCell {
                c,
                fg: style.fg,
                bg: style.bg,
                flags: style.attrs | flags,
                zerowidth: Vec::new(),
                underline_color: style.underline_color,
                hyperlink: None,
            };
        }
    };
    for c in text.chars() {
        let w = match UnicodeWidthChar::width(c) {
            Some(w) if w > 0 => w,
            _ => continue,
        };
        if x + w > end {
            break;
        }
        if w == 2 {
            put(screen, x, c, CellFlags::WIDE_CHAR);
            put(screen, x + 1, ' ', CellFlags::WIDE_CHAR_SPACER);
        } else {
            put(screen, x, c, CellFlags::empty());
        }
        x += w;
    }
}

pub struct ToastOverlay;

impl Overlay for ToastOverlay {
    fn draw(&self, state: &State, client: ClientId, cx: &OverlayCtx, screen: &mut Screen) {
        let Some(n) = shown(state, client, Instant::now()) else {
            return;
        };
        let Some(a) = area((screen.cols, screen.rows), cx.window_top) else {
            return;
        };
        draw_list(
            screen,
            a,
            "",
            &Rows::default(),
            &[],
            &ListState::default(),
            false,
        );
        let (x0, y0) = (usize::from(a.x), usize::from(a.y));
        let end = x0 + usize::from(a.cols) - 1;
        let head = format!(
            "{} {} {} {}",
            glyph(&n.state),
            target_label(state, n),
            n.kind.as_deref().unwrap_or(""),
            n.state
        );
        put_text(screen, x0 + 1, y0 + 1, end - 2, &head);
        put_text(screen, end - 1, y0 + 1, end, "x");
        put_text(screen, x0 + 1, y0 + 2, end, n.msg.as_deref().unwrap_or(""));
    }
}

/// A left click in the toast jumps to the pane and marks it read; on the
/// `x` it dismisses.
pub struct ToastHook;

impl InputHook for ToastHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        now: Instant,
    ) -> Option<Routed> {
        let Input::Mouse(m) = input else {
            return None;
        };
        if m.kind != MouseKind::Down(MouseButton::Left) {
            return None;
        }
        let n = shown(state, client, now)?;
        let (id, pane) = (n.id, n.pane);
        let size = state.clients.get(&client)?.size;
        let a = area(size, clients::overlay_ctx(state, client)?.window_top)?;
        let inside = (a.x..a.x + a.cols).contains(&m.col) && (a.y..a.y + a.rows).contains(&m.row);
        if !inside {
            return None;
        }
        if m.row == a.y + 1 && m.col == a.x + a.cols - 2 {
            state.notify.dismiss(id);
        } else {
            let ctx = state.clients.get(&client)?.ctx.clone();
            if let Err(e) = agent::jump(state, &ctx, pane) {
                clients::show_message(state, client, &e);
            }
            state.notify.seen(pane);
        }
        clients::mark_all_dirty(state);
        Some(Routed::Consumed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ClientCtx;
    use crate::server::clients::AttachRequest;
    use crate::server::input::{Mods, Mouse};
    use crate::server::{dispatch, test_ctx, test_state};
    use crate::types::PaneId;
    use std::time::Duration;

    // The notifier is the real T1 code: its raise rules are not under test,
    // and a fake would only restate its two lines.

    fn run(state: &mut State, args: &[&str]) {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        dispatch::run(state, &test_ctx(), &argv).expect("command");
    }

    /// Panes %1 (window 1) and %2 (window 2, current); a 100x40 client.
    fn setup() -> (
        State,
        ClientId,
        PaneId,
        PaneId,
        tokio::sync::mpsc::Receiver<Vec<u8>>,
    ) {
        let mut state = test_state();
        run(&mut state, &["new-session", "-d", "-s", "a", "sleep 600"]);
        run(&mut state, &["new-window", "-t", "a", "sleep 600"]);
        let panes: Vec<PaneId> = state.model.panes.keys().copied().collect();
        let accepted = clients::attach(
            &mut state,
            AttachRequest {
                ctx: ClientCtx {
                    client: None,
                    ..test_ctx()
                },
                size: (100, 40),
                term: "xterm".into(),
                caps: Default::default(),
                pid: None,
                target: Some("a".into()),
                detach_others: false,
                nest: false,
                tty: None,
                created: false,
                extras: Default::default(),
            },
        )
        .expect("attach");
        (state, accepted.id, panes[0], panes[1], accepted.output)
    }

    fn raise(state: &mut State, pane: PaneId, to: &str, msg: &str) {
        state.notify.on_change(
            pane,
            "u1",
            Some("claude"),
            Some("working"),
            Some(to),
            Some(msg),
            Duration::from_secs(60),
            false,
            true,
            Instant::now(),
        );
    }

    fn lines(state: &State, client: ClientId) -> Vec<String> {
        let mut screen = Screen::new(100, 40);
        let cx = clients::overlay_ctx(state, client).expect("ctx");
        ToastOverlay.draw(state, client, &cx, &mut screen);
        screen
            .cells
            .chunks(100)
            .map(|r| r.iter().map(|c| c.c).collect::<String>())
            .collect()
    }

    fn click(state: &mut State, client: ClientId, col: u16, row: u16) -> Option<Routed> {
        let m = Mouse {
            kind: MouseKind::Down(MouseButton::Left),
            col,
            row,
            mods: Mods::default(),
        };
        ToastHook.on_input(state, client, &Input::Mouse(m), Instant::now())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shown_only_for_toast_states_within_toast_time() {
        let (mut state, client, p1, _p2, _out) = setup();
        raise(&mut state, p1, "done", "all finished");
        assert!(lines(&state, client).iter().all(|l| l.trim().is_empty()));
        assert!(deadline(&state, client).is_none());

        raise(&mut state, p1, "blocked", "needs perm");
        let text = lines(&state, client).join("\n");
        assert!(text.contains("blocked"), "{text}");
        assert!(text.contains("needs perm"), "{text}");
        assert!(deadline(&state, client).is_some());

        state.notify.cfg.toast_time = Duration::ZERO;
        assert!(lines(&state, client).iter().all(|l| l.trim().is_empty()));
        assert!(deadline(&state, client).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn not_shown_to_client_looking_at_pane() {
        let (mut state, client, p1, p2, _out) = setup();
        raise(&mut state, p2, "blocked", "here");
        assert!(lines(&state, client).iter().all(|l| l.trim().is_empty()));
        assert!(deadline(&state, client).is_none());
        raise(&mut state, p1, "blocked", "elsewhere");
        assert!(lines(&state, client).join("\n").contains("elsewhere"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn newer_replaces() {
        let (mut state, client, p1, _p2, _out) = setup();
        raise(&mut state, p1, "blocked", "first one");
        raise(&mut state, p1, "blocked", "second one");
        let text = lines(&state, client).join("\n");
        assert!(text.contains("second one") && !text.contains("first one"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn long_msg_cut_by_width() {
        let (mut state, client, p1, _p2, _out) = setup();
        raise(&mut state, p1, "blocked", &"漢".repeat(40));
        let rows = lines(&state, client);
        let at = rows.iter().position(|l| l.contains('漢')).expect("msg row");
        let row: Vec<char> = rows[at].chars().collect();
        // The box spans 48 cells from column 51; the message stays inside it.
        assert_eq!(row.len(), 100);
        assert_eq!(row[98], '│', "right border intact");
        let wide = row.iter().filter(|c| **c == '漢').count();
        assert_eq!(wide, 23, "46 inner cells hold 23 wide characters");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn click_jumps_and_x_dismisses() {
        let (mut state, client, p1, _p2, _out) = setup();
        raise(&mut state, p1, "blocked", "perm");
        // The `x` is one cell left of the right border.
        assert!(matches!(
            click(&mut state, client, 97, 1),
            Some(Routed::Consumed)
        ));
        assert!(state.notify.items().is_empty() || state.notify.unread() == 0);
        assert!(click(&mut state, client, 97, 1).is_none());

        raise(&mut state, p1, "blocked", "again");
        assert!(click(&mut state, client, 0, 0).is_none(), "outside");
        assert!(matches!(
            click(&mut state, client, 60, 2),
            Some(Routed::Consumed)
        ));
        assert_eq!(state.notify.unread(), 0);
        let window = state.model.panes[&p1].window;
        let session = state.clients[&client].session;
        assert_eq!(state.model.sessions[&session].active, window);
    }
}
