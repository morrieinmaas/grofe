//! The options reference in the book is generated from the option table,
//! so it cannot drift: this test fails when the committed file differs
//! from what the table produces. Regenerate it with
//! `GROFE_BLESS=1 cargo test --test docs_reference` (or `just docs`).

use std::path::Path;

#[test]
fn options_reference_matches_the_table() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/book/src/reference/options.md");
    let generated = grofe::server::dispatch::options::reference_markdown();
    if std::env::var_os("GROFE_BLESS").is_some() {
        std::fs::write(&path, &generated).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == generated,
        "{} is out of date; run `just docs` (GROFE_BLESS=1 cargo test --test docs_reference)",
        path.display()
    );
}
