//! Copy-mode hints (`copy-mode -h`, WS13): the visible text is scanned with
//! `copy-hint-patterns`, each match gets a one- or two-letter label drawn
//! over it, and typing a label copies the match (an uppercase letter also
//! pastes it into the pane).

use regex::Regex;

use super::Grid;

/// The label letters: the home row.
pub const ALPHABET: &str = "asdfghjkl";

/// One match on the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub label: String,
    pub text: String,
    /// The row of the copy, and the first column and width of the match.
    pub row: usize,
    pub x: usize,
    pub width: usize,
}

/// A running hint mode: the labelled matches and the letters typed so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hints {
    pub hits: Vec<Hit>,
    pub typed: String,
    /// An uppercase letter was typed: paste as well as copy.
    pub paste: bool,
    /// The mode was entered by the hint command itself, so cancelling the
    /// hints leaves it as well.
    pub entered: bool,
}

/// What one typed letter did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// A prefix of a longer label.
    Pending,
    /// Not a label: the hints end.
    Cancel,
    /// A label: its match and whether to paste it.
    Pick { text: String, paste: bool },
}

impl Hints {
    /// The labelled matches of rows `top..top + rows` of `grid`, nearest
    /// the cursor (`cursor`, a column and a row of the copy) first. `None`
    /// when nothing matches.
    pub fn scan(
        grid: &Grid,
        top: usize,
        rows: usize,
        cursor: (usize, usize),
        patterns: &[Regex],
        entered: bool,
    ) -> Option<Hints> {
        let mut found = Vec::new();
        for row in top..(top + rows).min(grid.hsize + grid.sy) {
            found.extend(scan_row(grid, row, patterns));
        }
        if found.is_empty() {
            return None;
        }
        let (cx, cy) = cursor;
        let distance = |h: &Hit| {
            let dx = if cx < h.x {
                h.x - cx
            } else {
                cx.saturating_sub(h.x + h.width - 1)
            };
            (h.row.abs_diff(cy), dx)
        };
        found.sort_by_key(|h| (distance(h), h.row, h.x));
        let labels = labels(found.len());
        found.truncate(labels.len());
        for (h, label) in found.iter_mut().zip(labels) {
            h.label = label;
        }
        Some(Hints {
            hits: found,
            typed: String::new(),
            paste: false,
            entered,
        })
    }

    /// Takes one typed character.
    pub fn press(&mut self, c: char) -> Step {
        let lower = c.to_ascii_lowercase();
        if !ALPHABET.contains(lower) {
            return Step::Cancel;
        }
        self.typed.push(lower);
        self.paste |= c.is_ascii_uppercase();
        if let Some(h) = self.hits.iter().find(|h| h.label == self.typed) {
            return Step::Pick {
                text: h.text.clone(),
                paste: self.paste,
            };
        }
        if self.hits.iter().any(|h| h.label.starts_with(&self.typed)) {
            Step::Pending
        } else {
            Step::Cancel
        }
    }
}

/// The matches in one row, earlier patterns first; a match overlapping
/// an earlier one is dropped.
// ponytail: a match never spans a wrapped line, so a URL wrapped by the
// pane's width is two partial matches.
fn scan_row(grid: &Grid, row: usize, patterns: &[Regex]) -> Vec<Hit> {
    let mut text = String::new();
    // The column of each byte of `text`, and one past the last.
    let mut col = Vec::new();
    let len = grid.line_length(row);
    let mut x = 0;
    while x < len {
        let gc = grid.get(x, row);
        if gc.padding {
            x += 1;
            continue;
        }
        let c = if gc.tab > 0 { ' ' } else { gc.c };
        text.push(c);
        col.resize(text.len(), x);
        x += 1;
    }
    col.push(len);
    let mut taken: Vec<(usize, usize)> = Vec::new();
    let mut hits = Vec::new();
    for re in patterns {
        for caps in re.captures_iter(&text) {
            let Some(m) = caps.get(1).or_else(|| caps.get(0)) else {
                continue;
            };
            let (s, e) = (m.start(), m.end());
            if s == e || taken.iter().any(|&(a, b)| s < b && a < e) {
                continue;
            }
            taken.push((s, e));
            hits.push(Hit {
                label: String::new(),
                text: m.as_str().to_string(),
                row,
                x: col[s],
                width: col[e] - col[s],
            });
        }
    }
    hits
}

/// `n` labels, shortest first: single letters while they suffice, else the
/// last letters of the alphabet become prefixes of two-letter labels, as
/// few as it takes. At most `ALPHABET.len()` squared come back.
pub fn labels(n: usize) -> Vec<String> {
    let letters: Vec<char> = ALPHABET.chars().collect();
    let k = letters.len();
    let prefixes = (0..=k).find(|p| k - p + p * k >= n).unwrap_or(k);
    let mut out: Vec<String> = letters[..k - prefixes]
        .iter()
        .map(|c| c.to_string())
        .collect();
    for p in &letters[k - prefixes..] {
        for s in &letters {
            out.push(format!("{p}{s}"));
        }
    }
    out.truncate(n);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(pats: &[&str]) -> Vec<Regex> {
        pats.iter().map(|p| Regex::new(p).unwrap()).collect()
    }

    fn defaults() -> Vec<Regex> {
        crate::server::dispatch::options::default_hint_patterns()
            .iter()
            .map(|p| Regex::new(p).unwrap())
            .collect()
    }

    fn grid(rows: &[&str]) -> Grid {
        Grid::from_text(80, 0, rows)
    }

    fn scan(rows: &[&str], cursor: (usize, usize), pats: &[Regex]) -> Option<Hints> {
        Hints::scan(&grid(rows), 0, rows.len(), cursor, pats, false)
    }

    fn texts(h: &Hints) -> Vec<(&str, &str)> {
        h.hits
            .iter()
            .map(|h| (h.label.as_str(), h.text.as_str()))
            .collect()
    }

    #[test]
    fn labels_are_single_letters_until_they_do_not_fit() {
        assert_eq!(labels(3), ["a", "s", "d"]);
        assert_eq!(labels(9).len(), 9);
    }

    #[test]
    fn two_letter_labels_are_prefix_free_and_shortest_first() {
        let ten = labels(10);
        assert_eq!(ten[..8], ["a", "s", "d", "f", "g", "h", "j", "k"]);
        assert_eq!(ten[8..], ["la", "ls"]);
        for n in [10, 30, 81] {
            let l = labels(n);
            assert_eq!(l.len(), n);
            for a in &l {
                assert!(l.iter().all(|b| a == b || !b.starts_with(a.as_str())));
            }
        }
        assert_eq!(labels(500).len(), 81);
    }

    #[test]
    fn nearest_match_gets_the_first_label_and_labels_are_stable() {
        let rows = ["see 1111 and", "", "then 2222 here", "", "3333"];
        let h = scan(&rows, (5, 2), &defaults()).unwrap();
        assert_eq!(texts(&h), [("a", "2222"), ("s", "1111"), ("d", "3333")]);
        assert_eq!(h, scan(&rows, (5, 2), &defaults()).unwrap());
        // From the top the order follows.
        let h = scan(&rows, (0, 0), &defaults()).unwrap();
        assert_eq!(texts(&h), [("a", "1111"), ("s", "2222"), ("d", "3333")]);
    }

    #[test]
    fn default_patterns_find_urls_paths_hashes_ips_and_numbers() {
        let rows = [
            "open https://example.com/a?b=1, then",
            "edit ~/src/main.rs and /etc/hosts",
            "commit 4f3c2a1b9d is at 10.0.0.12",
            "v6 fe80::1 and ::1 and 2001:db8:0:0:0:0:0:1",
            "port 12345 (1.2) 12:30:45",
        ];
        let h = scan(&rows, (0, 0), &defaults()).unwrap();
        let all: Vec<&str> = h.hits.iter().map(|h| h.text.as_str()).collect();
        for want in [
            "https://example.com/a?b=1",
            "~/src/main.rs",
            "/etc/hosts",
            "4f3c2a1b9d",
            "10.0.0.12",
            "fe80::1",
            "::1",
            "2001:db8:0:0:0:0:0:1",
            "12345",
        ] {
            assert!(all.contains(&want), "{want} in {all:?}");
        }
        assert!(!all.contains(&"12:30:45"), "{all:?}");
    }

    #[test]
    fn a_capture_group_picks_the_text_and_overlaps_are_dropped() {
        let h = scan(&["id=77 77"], (0, 0), &res(&[r"id=(\d+)", r"\d+"])).unwrap();
        assert_eq!(texts(&h), [("a", "77"), ("s", "77")]);
        assert_eq!((h.hits[0].x, h.hits[0].width), (3, 2));
        assert_eq!(h.hits[1].x, 6);
    }

    #[test]
    fn typing_a_label_picks_and_an_uppercase_letter_pastes() {
        let mut h = scan(&["1111 2222"], (0, 0), &defaults()).unwrap();
        assert_eq!(
            h.clone().press('s'),
            Step::Pick {
                text: "2222".into(),
                paste: false
            }
        );
        assert_eq!(
            h.press('S'),
            Step::Pick {
                text: "2222".into(),
                paste: true
            }
        );
        assert_eq!(h.clone().press('x'), Step::Cancel);
    }

    #[test]
    fn a_two_letter_label_waits_for_its_second_letter() {
        let rows = [
            "1001 1002 1003 1004 1005 1006",
            "1007 1008 1009 1010 1011 1012",
        ];
        let mut h = scan(&rows, (0, 0), &defaults()).unwrap();
        assert_eq!(h.hits.len(), 12);
        assert_eq!(h.press('l'), Step::Pending);
        match h.press('A') {
            Step::Pick { paste, .. } => assert!(paste),
            s => panic!("{s:?}"),
        }
    }

    #[test]
    fn a_screen_with_no_matches_has_no_hints() {
        assert!(scan(&["nothing here", "at all"], (0, 0), &defaults()).is_none());
    }
}
