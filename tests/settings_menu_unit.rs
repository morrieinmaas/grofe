//! Pure pieces of the settings menu (spec 2026-11-20-settings-menu, PR 3).
//! These need `grofe::server::settings` to be public; they live apart from
//! the e2e file so that file compiles while the module is missing.

mod common;

use std::collections::BTreeSet;

use common::TestServer;
use grofe::server::settings::{layouts::preview, section_of};

/// T21: every option lists into one section; the sections in use are the
/// eight of the spec at most, and related options share one.
#[test]
fn every_option_maps_to_exactly_one_section() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "s", "cat"]).unwrap();
    let listing = srv.cmd(&["show-options", "-g"]).expect("show-options");
    let names: Vec<&str> = listing
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|n| !n.starts_with('@'))
        .collect();
    assert!(
        names.len() > 100,
        "expected the full table, got {}",
        names.len()
    );
    const SECTIONS: [&str; 8] = [
        "Appearance",
        "StatusLine",
        "Keys",
        "Layouts",
        "Agents",
        "Sessions",
        "Remote",
        "Other",
    ];
    let mut used = BTreeSet::new();
    for n in &names {
        let section = format!("{:?}", section_of(n));
        assert!(
            SECTIONS.contains(&section.as_str()),
            "{n} maps to {section}, not one of the eight sections"
        );
        used.insert(section);
    }
    assert!(used.len() <= 8, "more than eight sections: {used:?}");
    assert!(
        used.len() >= 6,
        "options bunch into too few sections: {used:?}"
    );
    assert_eq!(
        format!("{:?}", section_of("status-left")),
        format!("{:?}", section_of("status-right"))
    );
    assert_ne!(
        format!("{:?}", section_of("status-left")),
        format!("{:?}", section_of("history-limit"))
    );
}

const TWO_SIDE_BY_SIDE: &str = r#"
[[window]]
name = "dev"
split = "h"
[[window.pane]]
[[window.pane]]
"#;

const TWO_STACKED: &str = r#"
[[window]]
name = "dev"
split = "v"
[[window.pane]]
[[window.pane]]
"#;

fn corners(line: &str) -> usize {
    line.chars().filter(|c| "┌╭+".contains(*c)).count()
}

/// T28: two panes side by side draw two boxes on the first row; stacked
/// panes draw one box per row band. Every line fits the box given.
#[test]
fn preview_of_a_side_by_side_split_draws_two_boxes() {
    let (w, h) = (24u16, 6u16);
    let side = preview(TWO_SIDE_BY_SIDE, w, h);
    assert_eq!(side.len(), usize::from(h), "{side:#?}");
    assert!(
        side.iter().all(|l| l.chars().count() <= usize::from(w)),
        "{side:#?}"
    );
    assert_eq!(corners(&side[0]), 2, "{side:#?}");

    let stacked = preview(TWO_STACKED, w, h);
    assert_eq!(corners(&stacked[0]), 1, "{stacked:#?}");
    let rows_with_corner = stacked.iter().filter(|l| corners(l) > 0).count();
    assert_eq!(rows_with_corner, 2, "{stacked:#?}");
}
