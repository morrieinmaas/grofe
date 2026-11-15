//! Every relative markdown link in README.md and docs/ resolves to a file,
//! and every backticked `docs/...md` path does too.

use std::fs;
use std::path::{Path, PathBuf};

fn markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            markdown_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

/// The text of every `](target)` in `text`.
fn link_targets(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("](") {
        rest = &rest[i + 2..];
        if let Some(end) = rest.find(')') {
            found.push(&rest[..end]);
        }
    }
    found
}

/// Backticked `docs/...md` paths in `text`.
fn doc_paths(text: &str) -> Vec<&str> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter(|s| s.starts_with("docs/") && s.ends_with(".md") && !s.contains(' '))
        .collect()
}

#[test]
fn relative_links_and_doc_paths_resolve() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![root.join("README.md")];
    markdown_files(&root.join("docs"), &mut files);
    let mut broken = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).unwrap();
        let dir = file.parent().unwrap();
        for target in link_targets(&text) {
            let path = target.split('#').next().unwrap_or("");
            if path.is_empty() || path.contains("://") || path.starts_with("mailto:") {
                continue;
            }
            if !dir.join(path).exists() {
                broken.push(format!(
                    "{}: link ({target})",
                    file.strip_prefix(root).unwrap().display()
                ));
            }
        }
        for path in doc_paths(&text) {
            if path.contains('<') || path.contains('*') {
                continue;
            }
            if !root.join(path).exists() {
                broken.push(format!(
                    "{}: path `{path}`",
                    file.strip_prefix(root).unwrap().display()
                ));
            }
        }
    }
    assert!(
        broken.is_empty(),
        "broken doc references:\n{}",
        broken.join("\n")
    );
}
