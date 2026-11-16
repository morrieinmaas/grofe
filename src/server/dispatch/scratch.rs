//! Scratchpads: floating panes that hide into the model's history instead of
//! closing (`Model::scratch`), and come back with `show-scratchpad`.

use std::path::Path;

use crate::cmd::{
    DisplayMenu, MenuEntry, NewPane, NewScratchpad, Offset, PaneSetup, Resolve, ShowScratchpad,
    ToggleScratchpad,
};
use crate::protocol::ClientCtx;
use crate::server::dispatch::{menu, popup};
use crate::server::model::Model;
use crate::server::{State, clients};
use crate::types::SplitSize;

/// `new-scratchpad`: a floating pane 60% of the window wide and 50% high,
/// centred and active, that is a scratchpad.
pub fn new(state: &mut State, ctx: &ClientCtx, c: NewScratchpad) -> Result<String, String> {
    let model = &state.model;
    let target = model.resolve_pane(&c.target, ctx)?;
    let window = model.panes[&target].window;
    let w = &model.windows[&window];
    // At least one cell, or a 1x1 window's 0x0 pad fails `new-pane`'s checks.
    let pct = |n: u16, p: u32| ((u32::from(n) * p / 100) as u16).max(1);
    let (cols, rows) = (pct(w.cols, 60), pct(w.rows, 50));
    let (x, y) = (
        w.cols.saturating_sub(cols) / 2,
        w.rows.saturating_sub(rows) / 2,
    );
    let float = NewPane {
        print: false,
        format: None,
        target: c.target,
        cwd: c.cwd,
        width: Some(SplitSize::Cells(cols)),
        height: Some(SplitSize::Cells(rows)),
        x: Some(Offset::Cells(i32::from(x))),
        y: Some(Offset::Cells(i32::from(y))),
        command: c.command,
        env: Vec::new(),
        zoom: false,
        detached: false,
        setup: PaneSetup::default(),
    };
    popup::new_pane(state, ctx, float)?;
    // The new pane is the window's active one and its last floating one.
    if let Some(f) = state
        .model
        .windows
        .get_mut(&window)
        .and_then(|w| w.floating.last_mut())
    {
        f.scratch = true;
    }
    Ok(String::new())
}

/// `toggle-scratchpad`: hides a scratchpad target; else shows the newest
/// hidden one in the target's window; with none hidden opens a new one.
pub fn toggle(state: &mut State, ctx: &ClientCtx, c: ToggleScratchpad) -> Result<String, String> {
    let model = &state.model;
    let pane = model.resolve_pane(&c.target, ctx)?;
    let window = model.panes[&pane].window;
    let is_scratch = model
        .windows
        .get(&window)
        .and_then(|w| w.floating(pane))
        .is_some_and(|f| f.scratch);
    if is_scratch {
        let removed = state.model.stash_pane(pane)?;
        state.events.panes_killed(removed);
        return Ok(String::new());
    }
    if let Some(newest) = state.model.scratch.first().map(|s| s.pane.id) {
        state.model.unstash_pane(newest, window)?;
        return Ok(String::new());
    }
    new(
        state,
        ctx,
        NewScratchpad {
            target: c.target,
            cwd: None,
            command: None,
        },
    )
}

/// `show-scratchpad`: shows the hidden scratchpad `c.pane` in the target's
/// window.
pub fn show(state: &mut State, ctx: &ClientCtx, c: ShowScratchpad) -> Result<String, String> {
    let pane = state.model.resolve_pane(&c.target, ctx)?;
    let window = state.model.panes[&pane].window;
    state.model.unstash_pane(c.pane, window)?;
    Ok(String::new())
}

/// What a hidden scratchpad is called: its command, else its shell, then its
/// directory's name.
fn label(pane: &crate::server::model::Pane) -> String {
    let base = |p: &Path| {
        p.file_name().map_or_else(
            || p.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    };
    let what = match &pane.argv {
        Some(argv) if !argv.is_empty() => argv.join(" "),
        _ => base(Path::new(&pane.shell)),
    };
    format!("{what} · {}", base(&pane.cwd))
}

/// A menu label is a format: double each `#` so it shows as written.
fn menu_label(label: &str) -> String {
    label.replace('#', "##")
}

/// `list-scratchpads`: one `<n>: <pane-id> <label>` line per hidden
/// scratchpad, newest first.
pub fn list(state: &mut State, _ctx: &ClientCtx) -> Result<String, String> {
    let mut out = String::new();
    for (n, s) in state.model.scratch.iter().enumerate() {
        out.push_str(&format!("{n}: {} {}\n", s.pane.id, label(&s.pane)));
    }
    Ok(out)
}

/// The menu items of the hidden scratchpads, newest first; the first ten are
/// keyed by their digit.
pub(crate) fn menu_items(model: &Model) -> Vec<MenuEntry> {
    model
        .scratch
        .iter()
        .enumerate()
        .map(|(n, s)| MenuEntry::Item {
            name: menu_label(&label(&s.pane)),
            key: if n < 10 { n.to_string() } else { String::new() },
            command: format!("show-scratchpad {}", s.pane.id),
        })
        .collect()
}

/// `choose-scratchpad`: a menu of the hidden scratchpads.
pub fn choose(state: &mut State, ctx: &ClientCtx) -> Result<String, String> {
    let items = menu_items(&state.model);
    if items.is_empty() {
        return match ctx.client {
            Some(client) => {
                clients::show_message(state, client, "no scratchpads");
                Ok(String::new())
            }
            None => Ok("no scratchpads\n".to_string()),
        };
    }
    let menu = DisplayMenu {
        title: Some("scratchpads".to_string()),
        items,
        ..DisplayMenu::default()
    };
    menu::display_menu(state, ctx, menu)
}

#[cfg(test)]
mod tests {
    use crate::server::dispatch;
    use crate::server::{State, test_ctx, test_state};
    use crate::types::PaneId;

    fn run(state: &mut State, words: &[&str]) -> Result<String, String> {
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        dispatch::run(state, &test_ctx(), &argv)
    }

    /// A state with one 80x24 session `a`, one window, one tiled pane.
    fn state() -> State {
        let mut state = test_state();
        let made = run(
            &mut state,
            &[
                "new-session",
                "-d",
                "-x",
                "80",
                "-y",
                "24",
                "-s",
                "a",
                "sleep 600",
            ],
        );
        assert!(made.is_ok(), "{made:?}");
        state
    }

    fn window_of(state: &State) -> crate::types::WindowId {
        *state.model.windows.keys().next().expect("a window")
    }

    fn hidden(state: &State) -> Vec<PaneId> {
        state.model.scratch.iter().map(|s| s.pane.id).collect()
    }

    fn id(p: PaneId) -> String {
        p.to_string()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn new_is_centred_scratch() {
        let mut state = state();
        assert_eq!(
            run(&mut state, &["new-scratchpad", "sleep", "600"]),
            Ok(String::new())
        );
        let w = &state.model.windows[&window_of(&state)];
        assert_eq!(w.floating.len(), 1);
        let f = w.floating[0];
        assert_eq!((f.cols, f.rows, f.x, f.y), (48, 12, 16, 6));
        assert!(f.scratch);
        assert_eq!(w.active, f.pane);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn toggle_hides_then_shows() {
        let mut state = state();
        run(&mut state, &["new-scratchpad", "sleep", "600"]).expect("new");
        let w = window_of(&state);
        let sp = state.model.windows[&w].floating[0].pane;
        run(&mut state, &["toggle-scratchpad"]).expect("hide");
        assert_eq!(hidden(&state), vec![sp]);
        assert!(!state.model.panes.contains_key(&sp));
        assert!(state.model.windows[&w].floating.is_empty());
        run(&mut state, &["toggle-scratchpad"]).expect("show");
        assert!(state.model.scratch.is_empty());
        let win = &state.model.windows[&w];
        assert_eq!(win.active, sp);
        assert!(win.floating(sp).is_some_and(|f| f.scratch));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn toggle_with_none_opens_one() {
        let mut state = state();
        run(&mut state, &["toggle-scratchpad"]).expect("toggle");
        let w = &state.model.windows[&window_of(&state)];
        assert_eq!(w.floating.len(), 1);
        assert!(w.floating[0].scratch);
        assert!(state.model.scratch.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn toggle_on_a_plain_floating_pane_shows_the_newest_hidden() {
        let mut state = state();
        run(&mut state, &["new-scratchpad", "sleep", "600"]).expect("new");
        let w = window_of(&state);
        let sp = state.model.windows[&w].floating[0].pane;
        run(&mut state, &["toggle-scratchpad"]).expect("hide");
        run(
            &mut state,
            &["new-pane", "-x", "10", "-y", "3", "sleep 600"],
        )
        .expect("float");
        let plain = state.model.windows[&w].floating[0].pane;
        run(&mut state, &["toggle-scratchpad", "-t", &id(plain)]).expect("toggle");
        assert!(state.model.scratch.is_empty());
        let win = &state.model.windows[&w];
        assert!(win.floating(plain).is_some_and(|f| !f.scratch));
        assert!(win.floating(sp).is_some_and(|f| f.scratch));
        assert!(state.model.panes.contains_key(&plain));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn toggle_past_the_history_limit_kills_the_oldest() {
        let mut state = state();
        run(&mut state, &["set", "-g", "scratchpad-history", "1"]).expect("option");
        run(&mut state, &["new-scratchpad", "sleep", "600"]).expect("first");
        run(&mut state, &["new-scratchpad", "sleep", "600"]).expect("second");
        let w = window_of(&state);
        let (a, b) = {
            let f = &state.model.windows[&w].floating;
            (f[0].pane, f[1].pane)
        };
        run(&mut state, &["toggle-scratchpad", "-t", &id(a)]).expect("hide a");
        run(&mut state, &["toggle-scratchpad", "-t", &id(b)]).expect("hide b");
        assert_eq!(hidden(&state), vec![b]);
        assert!(!state.model.panes.contains_key(&a));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn show_unknown_pane_errors() {
        let mut state = state();
        assert_eq!(
            run(&mut state, &["show-scratchpad", "%99"]),
            Err("no such scratchpad: %99".to_string())
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn show_puts_a_hidden_one_in_the_targets_window() {
        let mut state = state();
        run(&mut state, &["new-scratchpad", "sleep", "600"]).expect("new");
        let sp = state.model.windows[&window_of(&state)].floating[0].pane;
        run(&mut state, &["toggle-scratchpad"]).expect("hide");
        run(&mut state, &["new-session", "-d", "-s", "b", "sleep 600"]).expect("b");
        let b = state.model.session_by_name("b").expect("b");
        let bw = state.model.sessions[&b].active;
        run(&mut state, &["show-scratchpad", "-t", "b:", &id(sp)]).expect("show");
        assert_eq!(state.model.panes[&sp].window, bw);
        assert_eq!(state.model.windows[&bw].active, sp);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_is_newest_first_with_labels() {
        let mut state = state();
        assert_eq!(run(&mut state, &["list-scratchpads"]), Ok(String::new()));
        run(
            &mut state,
            &["new-scratchpad", "-c", "/tmp", "sleep", "600"],
        )
        .expect("one");
        run(&mut state, &["toggle-scratchpad"]).expect("hide");
        run(&mut state, &["new-scratchpad", "-c", "/var", "sleep", "7"]).expect("two");
        run(&mut state, &["toggle-scratchpad"]).expect("hide");
        let ids = hidden(&state);
        let want = format!(
            "0: {} sleep 7 · var\n1: {} sleep 600 · tmp\n",
            ids[0], ids[1]
        );
        assert_eq!(run(&mut state, &["list-scratchpads"]), Ok(want));
    }

    fn sized(cols: &str, rows: &str) -> State {
        let mut state = test_state();
        let made = run(
            &mut state,
            &[
                "new-session",
                "-d",
                "-x",
                cols,
                "-y",
                rows,
                "-s",
                "a",
                "sleep 600",
            ],
        );
        assert!(made.is_ok(), "{made:?}");
        state
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn new_in_small_windows_does_not_fail() {
        for (c, r) in [("10", "4"), ("1", "1"), ("2", "1")] {
            let mut state = sized(c, r);
            let got = run(&mut state, &["new-scratchpad", "sleep", "600"]);
            assert_eq!(got, Ok(String::new()), "{c}x{r}");
            let w = &state.model.windows[&window_of(&state)];
            let f = w.floating[0];
            assert!(f.scratch && f.cols >= 1 && f.rows >= 1, "{c}x{r}");
            assert!(f.x >= 0 && f.y >= 0, "{c}x{r}");
            let hide = run(&mut state, &["toggle-scratchpad"]);
            assert_eq!(hide, Ok(String::new()), "{c}x{r}");
            assert_eq!(hidden(&state).len(), 1);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn new_passes_cwd_and_command() {
        let mut state = state();
        run(&mut state, &["new-scratchpad", "-c", "/tmp", "sleep", "9"]).expect("new");
        let sp = state.model.windows[&window_of(&state)].floating[0].pane;
        let p = &state.model.panes[&sp];
        assert_eq!(p.cwd, std::path::PathBuf::from("/tmp"));
        assert_eq!(
            p.argv.as_deref(),
            Some(&["sleep".to_string(), "9".to_string()][..])
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn show_visible_pane_errors() {
        let mut state = state();
        let tiled = state.model.windows[&window_of(&state)].active;
        assert_eq!(
            run(&mut state, &["show-scratchpad", &id(tiled)]),
            Err(format!("no such scratchpad: {tiled}"))
        );
        assert!(state.model.panes.contains_key(&tiled));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn toggle_without_target_uses_the_current_pane() {
        let mut state = state();
        run(&mut state, &["new-scratchpad", "sleep", "600"]).expect("new");
        let sp = state.model.windows[&window_of(&state)].floating[0].pane;
        // The scratchpad is the window's active pane, so it is the default target.
        run(&mut state, &["toggle-scratchpad"]).expect("hide");
        assert_eq!(hidden(&state), vec![sp]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn toggle_from_a_client_pane_acts_on_its_window() {
        let mut state = state();
        run(&mut state, &["new-scratchpad", "sleep", "600"]).expect("new");
        let a = window_of(&state);
        let sp = state.model.windows[&a].floating[0].pane;
        run(&mut state, &["toggle-scratchpad"]).expect("hide");
        run(
            &mut state,
            &[
                "new-session",
                "-d",
                "-x",
                "80",
                "-y",
                "24",
                "-s",
                "b",
                "sleep 600",
            ],
        )
        .expect("b");
        let b = *state
            .model
            .windows
            .keys()
            .find(|w| **w != a)
            .expect("window b");
        let tiled_b = state.model.windows[&b].pane_ids()[0];
        let mut ctx = test_ctx();
        ctx.pane = Some(tiled_b);
        let argv = vec!["toggle-scratchpad".to_string()];
        dispatch::run(&mut state, &ctx, &argv).expect("show");
        assert_eq!(state.model.panes[&sp].window, b);
        assert_eq!(state.model.windows[&b].active, sp);
        assert!(state.model.windows[&a].floating.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hidden_pane_is_unknown_to_everything_else() {
        let mut state = state();
        run(&mut state, &["new-scratchpad", "sleep", "600"]).expect("new");
        run(&mut state, &["toggle-scratchpad"]).expect("hide");
        let sp = id(hidden(&state)[0]);
        for words in [
            vec!["select-pane", "-t", &sp],
            vec!["kill-pane", "-t", &sp],
            vec!["display-message", "-p", "-t", &sp, "#{pane_id}"],
            vec!["send-keys", "-t", &sp, "x"],
            vec!["capture-pane", "-p", "-t", &sp],
            vec!["resize-pane", "-t", &sp, "-x", "10"],
            vec!["swap-pane", "-t", &sp],
            vec!["float-pane", "-t", &sp],
        ] {
            assert!(run(&mut state, &words).is_err(), "{words:?}");
        }
        let listed = run(&mut state, &["list-panes", "-a", "-F", "#{pane_id}"]).expect("list");
        assert!(!listed.contains(&sp), "{listed}");
        assert_eq!(hidden(&state).len(), 1, "no command disturbed the history");
        run(&mut state, &["list-sessions"]).expect("still serving");
        run(&mut state, &["list-windows", "-a"]).expect("still serving");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn labels_fall_back_and_survive_odd_cwds() {
        let mut state = state();
        run(&mut state, &["new-scratchpad", "sleep", "600"]).expect("new");
        run(&mut state, &["toggle-scratchpad"]).expect("hide");
        let pane = &mut state.model.scratch[0].pane;
        pane.argv = Some(Vec::new());
        pane.shell = "/bin/zsh".to_string();
        pane.cwd = std::path::PathBuf::from("/");
        let want = format!("0: {} zsh · /\n", state.model.scratch[0].pane.id);
        assert_eq!(run(&mut state, &["list-scratchpads"]), Ok(want));
        state.model.scratch[0].pane.argv = None;
        let want = format!("0: {} zsh · /\n", state.model.scratch[0].pane.id);
        assert_eq!(run(&mut state, &["list-scratchpads"]), Ok(want));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn menu_labels_survive_format_expansion() {
        let mut state = state();
        run(&mut state, &["new-scratchpad", "sleep", "600"]).expect("new");
        run(&mut state, &["toggle-scratchpad"]).expect("hide");
        let pane = &mut state.model.scratch[0].pane;
        pane.argv = Some(vec!["echo".into(), "#{pane_id}".into(), "\"q\"".into()]);
        pane.cwd = std::path::PathBuf::from("/tmp/#h");
        let items = super::menu_items(&state.model);
        let crate::cmd::MenuEntry::Item { name, command, .. } = &items[0] else {
            panic!("not an item");
        };
        let ctx = test_ctx();
        let shown = crate::server::prompt::expand_command(&state, &ctx, name);
        assert_eq!(shown, "echo #{pane_id} \"q\" · #h");
        let run_it = crate::server::prompt::expand_command(&state, &ctx, command);
        assert_eq!(
            run_it,
            format!("show-scratchpad {}", state.model.scratch[0].pane.id)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn choose_with_none_says_so() {
        let mut state = state();
        assert_eq!(
            run(&mut state, &["choose-scratchpad"]),
            Ok("no scratchpads\n".to_string())
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn choose_menu_keys_the_first_ten() {
        let mut state = state();
        run(&mut state, &["set", "-g", "scratchpad-history", "12"]).expect("option");
        for _ in 0..11 {
            run(
                &mut state,
                &["new-scratchpad", "-c", "/tmp", "sleep", "600"],
            )
            .expect("new");
            run(&mut state, &["toggle-scratchpad"]).expect("hide");
        }
        let items = super::menu_items(&state.model);
        assert_eq!(items.len(), 11);
        let crate::cmd::MenuEntry::Item { name, key, command } = &items[0] else {
            panic!("not an item");
        };
        assert_eq!(name, "sleep 600 · tmp");
        assert_eq!(key, "0");
        assert_eq!(
            command,
            &format!("show-scratchpad {}", state.model.scratch[0].pane.id)
        );
        let crate::cmd::MenuEntry::Item { key, .. } = &items[10] else {
            panic!("not an item");
        };
        assert_eq!(key, "");
    }
}
