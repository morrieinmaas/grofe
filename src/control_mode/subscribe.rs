//! `refresh-client -B` subscriptions: formats the client watches, checked
//! once a second, each change written as `%subscription-changed`
//! (tmux 3.7b's control.c).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::scan::scan_id;

/// How often tmux checks subscriptions.
const EVERY: Duration = Duration::from_secs(1);

/// What a subscription watches, from its `what` field.
#[derive(Debug, Clone, PartialEq, Eq)]
enum What {
    Session,
    Pane(String),
    Window(String),
    AllPanes,
    AllWindows,
}

impl What {
    /// tmux's reading: `%*`, `@*`, else `%<n>` or `@<n>` as
    /// `sscanf("%%%d")` reads it (`%0x` is `%0`), and anything else, a
    /// negative id included, is the session.
    fn parse(what: &str) -> What {
        let id = |sigil: char| scan_id(what, sigil).filter(|n| *n >= 0);
        match what {
            "%*" => What::AllPanes,
            "@*" => What::AllWindows,
            _ => match (id('%'), id('@')) {
                (Some(n), _) => What::Pane(format!("%{n}")),
                (_, Some(n)) => What::Window(format!("@{n}")),
                _ => What::Session,
            },
        }
    }

    /// The order tmux checks in: session subscriptions, then single panes
    /// and windows, then all panes, then all windows.
    fn class(&self) -> u8 {
        match self {
            What::Session => 0,
            What::Pane(_) | What::Window(_) => 1,
            What::AllPanes => 2,
            What::AllWindows => 3,
        }
    }
}

#[derive(Debug)]
struct Sub {
    what: What,
    format: String,
    /// The last value written, by row (`@w idx %p`, `- - -` for the session).
    last: BTreeMap<String, String>,
}

/// A client's subscriptions, by name.
#[derive(Debug, Default)]
pub(super) struct Subs {
    subs: BTreeMap<String, Sub>,
    /// When the next check is due; `None` with no subscriptions.
    next: Option<Instant>,
}

impl Subs {
    /// `-B name:what:format` adds or replaces `name`; `-B name` removes it.
    /// A value with one `:` is ignored, as tmux does.
    pub(super) fn update(&mut self, value: &str, now: Instant) {
        let Some((name, rest)) = value.split_once(':') else {
            self.subs.remove(value);
            if self.subs.is_empty() {
                self.next = None;
            }
            return;
        };
        let Some((what, format)) = rest.split_once(':') else {
            return;
        };
        let sub = Sub {
            what: What::parse(what),
            format: format.to_string(),
            last: BTreeMap::new(),
        };
        self.subs.insert(name.to_string(), sub);
        self.next.get_or_insert(now + EVERY);
    }

    /// Whether a check is due at `now`; if so the next is a second on.
    pub(super) fn due(&mut self, now: Instant) -> bool {
        match self.next {
            Some(at) if now >= at => {
                self.next = Some(now + EVERY);
                true
            }
            _ => false,
        }
    }

    /// The `%subscription-changed` lines for `session`, asking the server
    /// with `ask` (an argv, its output).
    pub(super) fn check(
        &mut self,
        session: &str,
        ask: impl Fn(&[&str]) -> Result<String, String>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        for class in 0..4 {
            // (row index, line): the all-panes and all-windows checks go
            // row by row across their subscriptions.
            let mut lines: Vec<(usize, String)> = Vec::new();
            for (name, sub) in self
                .subs
                .iter_mut()
                .filter(|(_, s)| s.what.class() == class)
            {
                // A failed query skips the subscription this time, keeping
                // what it last wrote.
                let Some(rows) = rows(sub, session, &ask) else {
                    continue;
                };
                sub.last.retain(|k, _| rows.iter().any(|(r, _)| r == k));
                for (i, (row, value)) in rows.into_iter().enumerate() {
                    if sub.last.get(&row) == Some(&value) {
                        continue;
                    }
                    lines.push((
                        i,
                        format!("%subscription-changed {name} {session} {row} : {value}"),
                    ));
                    sub.last.insert(row, value);
                }
            }
            if class >= 2 {
                lines.sort_by_key(|(i, _)| *i);
            }
            out.extend(lines.into_iter().map(|(_, l)| l));
        }
        out
    }
}

/// The rows `sub` covers in `session` and its format's value in each;
/// `None` when the server did not answer.
fn rows(
    sub: &Sub,
    session: &str,
    ask: impl Fn(&[&str]) -> Result<String, String>,
) -> Option<Vec<(String, String)>> {
    let target = format!("{session}:");
    // `list-panes -s` and `list-windows` list the session's panes and
    // windows, each row keyed as the notice names it.
    let listed = |verb: &str, key: &str| -> Option<Vec<(String, String)>> {
        let format = format!("{key}\t{}", sub.format);
        let mut argv = vec![verb, "-t", session, "-F", &format];
        if verb == "list-panes" {
            argv.insert(1, "-s");
        }
        let text = ask(&argv).ok()?;
        let rows = text
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(k, v)| (k.to_string(), v.to_string()));
        Some(rows.collect())
    };
    let panes = || listed("list-panes", "#{window_id} #{window_index} #{pane_id}");
    let windows = || listed("list-windows", "#{window_id} #{window_index} -");
    let only = |rows: Option<Vec<(String, String)>>, keep: &dyn Fn(&str) -> bool| {
        rows.map(|r| r.into_iter().filter(|(k, _)| keep(k)).collect())
    };
    match &sub.what {
        // `--`: a format may start with `-`.
        What::Session => ask(&["display-message", "-p", "-t", &target, "--", &sub.format])
            .ok()
            .map(|v| vec![("- - -".to_string(), v.trim_end_matches('\n').to_string())]),
        What::Pane(id) => only(panes(), &|k| k.ends_with(&format!(" {id}"))),
        What::AllPanes => panes(),
        What::Window(id) => only(windows(), &|k| k.starts_with(&format!("{id} "))),
        What::AllWindows => windows(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server with session `$0`: windows @0 (panes %0, %1) and @1 (%2).
    fn fake(argv: &[&str]) -> Result<String, String> {
        let format = argv.last().unwrap();
        let value = format.split_once('\t').map_or(*format, |(_, v)| v);
        Ok(match argv[0] {
            "display-message" => format!("{value}\n"),
            "list-panes" => format!("@0 0 %0\t{value}\n@0 0 %1\t{value}\n@1 1 %2\t{value}\n"),
            "list-windows" => format!("@0 0 -\t{value}\n@1 1 -\t{value}\n"),
            _ => return Err("unknown".into()),
        })
    }

    #[test]
    fn what_reads_as_tmux_reads_it() {
        assert_eq!(What::parse("%*"), What::AllPanes);
        assert_eq!(What::parse("%3"), What::Pane("%3".into()));
        assert_eq!(What::parse("@*"), What::AllWindows);
        assert_eq!(What::parse("@12"), What::Window("@12".into()));
        assert_eq!(What::parse(""), What::Session);
        assert_eq!(What::parse("%x"), What::Session);
        assert_eq!(What::parse("%0x"), What::Pane("%0".into()));
        assert_eq!(What::parse("%00"), What::Pane("%0".into()));
        assert_eq!(What::parse("%-1"), What::Session);
    }

    #[test]
    fn changes_are_written_once_in_tmux_order() {
        let now = Instant::now();
        let mut subs = Subs::default();
        for value in ["w:@1:W", "s::S", "p:%1:P", "all:%*:A", "allw:@*:X"] {
            subs.update(value, now);
        }
        assert!(!subs.due(now));
        assert!(subs.due(now + EVERY));
        assert_eq!(
            subs.check("$0", fake),
            [
                "%subscription-changed s $0 - - - : S",
                "%subscription-changed p $0 @0 0 %1 : P",
                "%subscription-changed w $0 @1 1 - : W",
                "%subscription-changed all $0 @0 0 %0 : A",
                "%subscription-changed all $0 @0 0 %1 : A",
                "%subscription-changed all $0 @1 1 %2 : A",
                "%subscription-changed allw $0 @0 0 - : X",
                "%subscription-changed allw $0 @1 1 - : X",
            ]
        );
        assert!(subs.check("$0", fake).is_empty());
    }

    #[test]
    fn a_failed_query_keeps_what_was_written() {
        let now = Instant::now();
        let mut subs = Subs::default();
        subs.update("all:%*:A", now);
        assert_eq!(subs.check("$0", fake).len(), 3);
        assert!(subs.check("$0", |_| Err("gone".into())).is_empty());
        assert!(subs.check("$0", fake).is_empty());
    }

    #[test]
    fn a_name_alone_removes_and_one_colon_is_ignored() {
        let now = Instant::now();
        let mut subs = Subs::default();
        subs.update("s::S", now);
        subs.update("bad:%*", now);
        assert_eq!(subs.subs.len(), 1);
        subs.update("s", now);
        assert!(subs.subs.is_empty() && !subs.due(now + EVERY));
    }
}
