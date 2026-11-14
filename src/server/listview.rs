//! The list widget's state, keys and filter (agents spec §5.1).

use super::input::{Key, KeyCode};

/// Rows a page key moves before `clamp` has seen the list's height.
const DEFAULT_PAGE: usize = 10;

/// Subsequence score of `query` in `text`, case-insensitive; `None` when
/// the query is not a subsequence. Contiguous runs and early matches score
/// higher. An empty query matches everything with 0.
pub fn score(query: &str, text: &str) -> Option<i32> {
    let mut hay = text.chars().flat_map(char::to_lowercase).enumerate();
    let mut total = 0;
    let mut prev: Option<usize> = None;
    for q in query.chars().flat_map(char::to_lowercase) {
        let (at, _) = hay.find(|&(_, c)| c == q)?;
        total += 10;
        match prev {
            Some(p) if p + 1 == at => total += 15,
            None => total -= at.min(9) as i32,
            _ => {}
        }
        prev = Some(at);
    }
    Some(total)
}

/// Indices of `texts` matching `query`, best score first, ties in the
/// caller's order.
pub fn filter(texts: &[String], query: &str) -> Vec<usize> {
    let mut hits: Vec<(usize, i32)> = texts
        .iter()
        .enumerate()
        .filter_map(|(i, t)| score(query, t).map(|s| (i, s)))
        .collect();
    hits.sort_by_key(|&(_, s)| std::cmp::Reverse(s));
    hits.into_iter().map(|(i, _)| i).collect()
}

/// Per-client list state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListState {
    pub query: String,
    pub selected: usize,
    pub top: usize,
    /// The key menu's query is live after `C-f`.
    pub filtering: bool,
    /// Visible rows, as last given to `clamp`; 0 until then.
    pub page: usize,
}

/// What a key did to the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListAction {
    None,
    Moved,
    QueryChanged,
    Enter,
    Close,
    Ctrl(char),
}

impl ListState {
    /// Apply `key` to a list of `rows` visible rows. `typing` says printable
    /// keys go to the query; when false they do nothing until `C-f`.
    pub fn on_key(&mut self, key: &Key, rows: usize, typing: bool) -> ListAction {
        let last = rows.saturating_sub(1);
        let page = if self.page == 0 {
            DEFAULT_PAGE
        } else {
            self.page
        };
        if key.mods.ctrl && !key.mods.alt {
            return match key.code {
                KeyCode::Char('c') => ListAction::Close,
                KeyCode::Char('n') => self.move_to(self.selected.saturating_add(1).min(last)),
                KeyCode::Char('p') => self.move_to(self.selected.saturating_sub(1)),
                KeyCode::Char('f') if !typing => {
                    self.filtering = true;
                    ListAction::QueryChanged
                }
                KeyCode::Char(c) => ListAction::Ctrl(c),
                _ => ListAction::None,
            };
        }
        if key.mods.ctrl || key.mods.alt || key.mods.super_ {
            return ListAction::None;
        }
        match key.code {
            KeyCode::Enter => ListAction::Enter,
            KeyCode::Escape => ListAction::Close,
            KeyCode::Up => self.move_to(self.selected.saturating_sub(1)),
            KeyCode::Down => self.move_to(self.selected.saturating_add(1).min(last)),
            KeyCode::PageUp => self.move_to(self.selected.saturating_sub(page)),
            KeyCode::PageDown => self.move_to(self.selected.saturating_add(page).min(last)),
            KeyCode::Home => self.move_to(0),
            KeyCode::End => self.move_to(last),
            KeyCode::Backspace if (typing || self.filtering) && !self.query.is_empty() => {
                self.query.pop();
                self.reset_selection();
                ListAction::QueryChanged
            }
            KeyCode::Char(c) if typing || self.filtering => {
                self.query.push(c);
                self.reset_selection();
                ListAction::QueryChanged
            }
            _ => ListAction::None,
        }
    }

    /// Keep `selected` inside `rows` and scrolled into a window of `height`.
    pub fn clamp(&mut self, rows: usize, height: usize) {
        self.page = height;
        self.selected = self.selected.min(rows.saturating_sub(1));
        if height == 0 {
            self.top = self.selected;
            return;
        }
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + height {
            self.top = self.selected + 1 - height;
        }
        self.top = self.top.min(rows.saturating_sub(height));
    }

    fn move_to(&mut self, to: usize) -> ListAction {
        self.selected = to;
        ListAction::Moved
    }

    fn reset_selection(&mut self) {
        self.selected = 0;
        self.top = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::input::Mods;

    fn key(code: KeyCode) -> Key {
        Key {
            code,
            mods: Mods::default(),
        }
    }

    fn ctrl(c: char) -> Key {
        Key {
            code: KeyCode::Char(c),
            mods: Mods {
                ctrl: true,
                ..Mods::default()
            },
        }
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn score_case_insensitive() {
        assert!(score("CL", "claude").is_some());
        assert!(score("cl", "CLAUDE").is_some());
        assert_eq!(score("xz", "claude"), None);
    }

    #[test]
    fn score_prefers_contiguous() {
        assert!(score("cl", "claude") > score("cl", "c_l"));
        assert!(score("cl", "claude") > score("cl", "xxclaude"));
    }

    #[test]
    fn ties_keep_order() {
        let rows = strs(&["claude", "codex", "pi"]);
        assert_eq!(filter(&rows, "c"), vec![0, 1]);
        assert_eq!(filter(&rows, ""), vec![0, 1, 2]);
        let same = strs(&["ab", "ab", "ab"]);
        assert_eq!(filter(&same, "ab"), vec![0, 1, 2]);
    }

    #[test]
    fn selected_clamped_after_filter() {
        let mut st = ListState {
            selected: 5,
            top: 3,
            ..ListState::default()
        };
        st.clamp(2, 4);
        assert_eq!((st.selected, st.top), (1, 0));
        st.clamp(0, 4);
        assert_eq!(st.selected, 0);
    }

    #[test]
    fn backspace_on_empty_query_noop() {
        let mut st = ListState::default();
        assert_eq!(
            st.on_key(&key(KeyCode::Backspace), 3, true),
            ListAction::None
        );
        st.on_key(&key(KeyCode::Char('a')), 3, true);
        assert_eq!(
            st.on_key(&key(KeyCode::Backspace), 3, true),
            ListAction::QueryChanged
        );
        assert_eq!(st.query, "");
    }

    #[test]
    fn pageup_pagedown_by_height() {
        let mut st = ListState::default();
        st.clamp(50, 5);
        st.on_key(&key(KeyCode::PageDown), 50, true);
        assert_eq!(st.selected, 5);
        st.on_key(&key(KeyCode::PageDown), 50, true);
        assert_eq!(st.selected, 10);
        st.on_key(&key(KeyCode::PageUp), 50, true);
        assert_eq!(st.selected, 5);
        st.on_key(&key(KeyCode::End), 50, true);
        assert_eq!(st.selected, 49);
        st.on_key(&key(KeyCode::Home), 50, true);
        assert_eq!(st.selected, 0);
    }

    #[test]
    fn ctrl_c_closes() {
        let mut st = ListState::default();
        assert_eq!(st.on_key(&ctrl('c'), 3, true), ListAction::Close);
        assert_eq!(st.on_key(&key(KeyCode::Escape), 3, true), ListAction::Close);
    }

    #[test]
    fn examples() {
        let mut st = ListState::default();
        assert_eq!(st.on_key(&key(KeyCode::Down), 3, true), ListAction::Moved);
        assert_eq!(st.selected, 1);
        assert_eq!(st.on_key(&key(KeyCode::Enter), 3, true), ListAction::Enter);
        assert_eq!(st.on_key(&ctrl('x'), 3, true), ListAction::Ctrl('x'));
        assert_eq!(st.on_key(&ctrl('n'), 3, true), ListAction::Moved);
        assert_eq!(st.selected, 2);
        assert_eq!(st.on_key(&ctrl('n'), 3, true), ListAction::Moved);
        assert_eq!(st.selected, 2);
        assert_eq!(
            st.on_key(&key(KeyCode::Char('a')), 3, true),
            ListAction::QueryChanged
        );
        assert_eq!((st.query.as_str(), st.selected), ("a", 0));
    }

    #[test]
    fn not_typing_until_ctrl_f() {
        let mut st = ListState::default();
        assert_eq!(
            st.on_key(&key(KeyCode::Char('a')), 3, false),
            ListAction::None
        );
        assert_eq!(st.on_key(&ctrl('f'), 3, false), ListAction::QueryChanged);
        assert!(st.filtering);
        assert_eq!(
            st.on_key(&key(KeyCode::Char('a')), 3, false),
            ListAction::QueryChanged
        );
        assert_eq!(st.query, "a");
    }
}
