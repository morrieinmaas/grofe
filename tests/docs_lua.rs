//! The scripting docs and the editor types must mention every `grofe.*` function.

use std::path::Path;

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Names set on the `grofe` table: `g.set("name", ...)` or the `g.set(\n "name",` form.
fn registered_names(api: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = api;
    while let Some(i) = rest.find("g.set(") {
        rest = &rest[i + "g.set(".len()..];
        let after = rest.trim_start();
        if let Some(q) = after.strip_prefix('"')
            && let Some(end) = q.find('"')
        {
            names.push(q[..end].to_string());
        }
    }
    names
}

#[test]
fn test_types_cover_api() {
    let names = registered_names(&read("src/lua/api.rs"));
    assert!(names.len() >= 10, "found only {names:?}");
    let types = read("types/grofe.d.luau");
    let docs = read("docs/lua.md");
    for name in &names {
        assert!(
            types.contains(&format!("{name}:")),
            "{name} missing from types/grofe.d.luau"
        );
        assert!(
            docs.contains(&format!("grofe.{name}")),
            "{name} missing from docs/lua.md"
        );
    }
}
