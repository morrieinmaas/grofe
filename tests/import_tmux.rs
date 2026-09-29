//! `grofe import-tmux` (TI.1, spec 5.5): golden-file tests over the two
//! fixtures under `tests/fixtures/import-tmux/`, plus the concrete cases
//! from the task brief. Every test copies a fixture's `home/` into a
//! tempdir and runs the built binary with `HOME`/`XDG_CONFIG_HOME` pointed
//! there; nothing ever touches the real state dir or `~/.config/grofe`, and
//! no server is started anywhere (`import-tmux` never starts one).

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::{CmdOutput, run_bounded};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(5);

/// A fixture's `home/` tree copied into a private tempdir.
struct Fixture {
    tmp: TempDir,
    home: PathBuf,
    xdg_empty: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let src = fixture_dir(name).join("home");
        let tmp = tempfile::Builder::new()
            .prefix("grofe-import-tmux")
            .tempdir()
            .expect("tempdir");
        let home = tmp.path().join("home");
        copy_dir(&src, &home);
        let xdg_empty = tmp.path().join("xdg-empty");
        fs::create_dir_all(&xdg_empty).expect("xdg-empty dir");
        Fixture {
            tmp,
            home,
            xdg_empty,
        }
    }

    /// `grofe import-tmux <args>`, isolated: `HOME` and `XDG_CONFIG_HOME`
    /// point at this fixture's tempdir, and every `CANOPY_*`/`TMUX`/
    /// `GROFE*` variable is removed from the child's environment.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_grofe"));
        cmd.arg("import-tmux")
            .args(args)
            .current_dir(&self.home)
            .env_remove("CANOPY_STORE")
            .env_remove("CANOPY_STATE")
            .env_remove("CANOPY_CONFIG")
            .env_remove("TMUX")
            .env_remove("GROFE")
            .env_remove("GROFE_PANE")
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.xdg_empty);
        cmd
    }

    fn run(&self, args: &[&str]) -> CmdOutput {
        run_bounded(self.command(args), TIMEOUT)
    }

    /// Every file under the copied `home/`, with its contents hashed
    /// (path relative to `home/` -> a cheap content digest), for the
    /// byte-identical-inputs check.
    fn hash_all(&self) -> BTreeMap<PathBuf, u64> {
        let mut out = BTreeMap::new();
        hash_dir(&self.home, &self.home, &mut out);
        out
    }
}

fn hash_dir(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, u64>) {
    for entry in fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let file_type = entry.file_type().expect("file_type");
        if file_type.is_dir() {
            hash_dir(root, &path, out);
        } else if file_type.is_file() {
            let bytes = fs::read(&path).expect("read fixture file");
            let rel = path.strip_prefix(root).expect("under root").to_path_buf();
            out.insert(rel, fnv1a(&bytes));
        }
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn fixture_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/import-tmux")
        .join(name)
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("mkdir");
    for entry in fs::read_dir(src).expect("read_dir src") {
        let entry = entry.expect("dir entry");
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type().expect("file_type").is_dir() {
            copy_dir(&from, &to);
        } else {
            fs::copy(&from, &to).expect("copy fixture file");
        }
    }
}

/// Reads a golden file, or panics with instructions when `GROFE_BLESS=1`
/// hasn't been run yet to create it.
fn read_golden(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "read golden {}: {e} (run with GROFE_BLESS=1 first)",
            path.display()
        )
    })
}

/// Compares `actual` to the golden at `path`, or (with `GROFE_BLESS=1` in
/// the test's own environment) overwrites it. Never blessed in CI (spec).
fn assert_golden(path: &Path, actual: &str) {
    if std::env::var("GROFE_BLESS").as_deref() == Ok("1") {
        fs::write(path, actual).unwrap_or_else(|e| panic!("write golden {}: {e}", path.display()));
        return;
    }
    let expected = read_golden(path);
    assert_eq!(
        actual,
        expected,
        "golden mismatch at {} (rerun with GROFE_BLESS=1 to update, then inspect the diff before committing)",
        path.display()
    );
}

// ---- golden-file tests (cases 1 and 2) ----

#[test]
fn golden_canopy() {
    let f = Fixture::new("canopy");
    let out = f.run(&[]);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{}", out.stderr);
    assert_golden(&fixture_dir("canopy").join("expected.toml"), &out.stdout);
    assert_golden(&fixture_dir("canopy").join("expected.report"), &out.stderr);
}

#[test]
fn golden_dotfiles() {
    let f = Fixture::new("dotfiles");
    let out = f.run(&[]);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{}", out.stderr);
    assert_golden(&fixture_dir("dotfiles").join("expected.toml"), &out.stdout);
    assert_golden(
        &fixture_dir("dotfiles").join("expected.report"),
        &out.stderr,
    );
}

// ---- case 3: the output loads with `show-config --file` ----

#[test]
fn case3_output_loads_with_show_config() {
    for name in ["canopy", "dotfiles"] {
        let f = Fixture::new(name);
        let out_path = f.tmp.path().join("out.toml");
        let out = f.run(&["-o", out_path.to_str().expect("utf8 path")]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "[{name}] stderr:\n{}",
            out.stderr
        );
        assert!(out_path.is_file(), "[{name}] -o file was not written");

        let none_dir = f.tmp.path().join("xdg-empty2");
        fs::create_dir_all(&none_dir).expect("mkdir");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_grofe"));
        cmd.arg("-S")
            .arg(f.tmp.path().join("none.sock"))
            .arg("show-config")
            .arg("--file")
            .arg(&out_path)
            .current_dir(&f.home)
            .env_remove("CANOPY_STORE")
            .env_remove("CANOPY_STATE")
            .env_remove("CANOPY_CONFIG")
            .env("HOME", &f.home)
            .env("XDG_CONFIG_HOME", &none_dir);
        let check = run_bounded(cmd, TIMEOUT);
        assert_eq!(
            check.status.code(),
            Some(0),
            "[{name}] show-config --file on the generated TOML failed:\n{}",
            check.stderr
        );
        assert!(
            check.stderr.is_empty(),
            "[{name}] show-config --file printed to stderr:\n{}",
            check.stderr
        );
        assert!(
            !f.tmp.path().join("none.sock").exists(),
            "[{name}] show-config --file must never start a server"
        );
    }
}

// ---- case 4: report completeness ----

/// Every non-blank, non-comment physical line of every input file is
/// either the start of an emitted `# <path>:<line>` entry, or named by a
/// report line at that file and line (including a note's own line, and a
/// `source-file`/`set-environment` line, which is always reported even
/// when also consumed for path resolution). A silently dropped line fails
/// this.
#[test]
fn case4_report_completeness() {
    for name in ["canopy", "dotfiles"] {
        let f = Fixture::new(name);
        let out = f.run(&[]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "[{name}] stderr:\n{}",
            out.stderr
        );

        let mut emitted_or_reported: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        for cap in line_refs(&out.stdout) {
            emitted_or_reported.entry(cap.0).or_default().push(cap.1);
        }
        for cap in line_refs(&out.stderr) {
            emitted_or_reported.entry(cap.0).or_default().push(cap.1);
        }

        for (path, starts) in directive_start_lines(&f.home) {
            let rel = path.strip_prefix(&f.home).unwrap_or(&path).to_path_buf();
            let covered = emitted_or_reported
                .iter()
                .find(|(p, _)| p.ends_with(&rel))
                .map(|(_, lines)| lines.clone())
                .unwrap_or_default();
            for (line, is_source_file) in starts {
                // Rule 8: "the source-file line itself emits nothing: the
                // output is flat" - it is exempt by design, not a drop.
                if is_source_file {
                    continue;
                }
                assert!(
                    covered.contains(&line),
                    "[{name}] {}:{line} is not accounted for (no emitted comment and no report line)",
                    rel.display()
                );
            }
        }
    }
}

/// `~path:line` (or `path:line`) references from either stream: an
/// emitted `# <path>:<line>` comment, or a `<path>:<line>: ...` report
/// line. The leading `~/` is stripped so it compares against a fixture
/// path relative to `home/`.
fn line_refs(text: &str) -> Vec<(PathBuf, usize)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.trim_start_matches('#').trim_start();
        let Some((path, rest)) = l.split_once(':') else {
            continue;
        };
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            continue;
        }
        let Ok(n) = digits.parse::<usize>() else {
            continue;
        };
        let path = path.strip_prefix("~/").unwrap_or(path);
        out.push((PathBuf::from(path), n));
    }
    out
}

/// The 1-based physical line numbers, per file under `home/`, that start
/// a non-blank, non-comment logical directive (not itself blank/comment,
/// and not a continuation of the previous physical line, which does not
/// end in a lone trailing `\`), paired with whether that directive is a
/// `source-file`/`source` (rule 8: it emits nothing of its own, by
/// design, so it is exempt from the completeness check below).
fn directive_start_lines(home: &Path) -> BTreeMap<PathBuf, Vec<(usize, bool)>> {
    let mut out = BTreeMap::new();
    collect_directive_starts(home, home, &mut out);
    out
}

fn collect_directive_starts(
    root: &Path,
    dir: &Path,
    out: &mut BTreeMap<PathBuf, Vec<(usize, bool)>>,
) {
    for entry in fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("entry");
        let path = entry.path();
        let ft = entry.file_type().expect("file_type");
        if ft.is_dir() {
            collect_directive_starts(root, &path, out);
        } else if ft.is_file() {
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let physical: Vec<&str> = text.lines().collect();
            let mut starts = Vec::new();
            let mut i = 0;
            let mut prev_continued = false;
            while i < physical.len() {
                let trimmed = physical[i].trim_start();
                let is_blank_or_comment = trimmed.is_empty() || trimmed.starts_with('#');
                if !prev_continued && !is_blank_or_comment {
                    let word = trimmed.split_whitespace().next().unwrap_or("");
                    let is_source_file = word == "source-file" || word == "source";
                    starts.push((i + 1, is_source_file));
                }
                prev_continued = !is_blank_or_comment && physical[i].ends_with('\\');
                i += 1;
            }
            if !starts.is_empty() {
                let rel = path.strip_prefix(root).expect("under root").to_path_buf();
                out.insert(rel, starts);
            }
        }
    }
}

// ---- case 5: byte-identical inputs ----

#[test]
fn case5_inputs_are_byte_identical_after_stdout_and_o_runs() {
    for name in ["canopy", "dotfiles"] {
        let f = Fixture::new(name);
        let before = f.hash_all();
        let out = f.run(&[]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "[{name}] stderr:\n{}",
            out.stderr
        );
        assert_eq!(
            f.hash_all(),
            before,
            "[{name}] stdout run modified an input file"
        );

        let out_path = f.tmp.path().join("out.toml");
        let out = f.run(&["-o", out_path.to_str().expect("utf8 path")]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "[{name}] stderr:\n{}",
            out.stderr
        );
        assert_eq!(
            f.hash_all(),
            before,
            "[{name}] -o run modified an input file"
        );
    }
}

// ---- case 6: `-o` refusal ----

#[test]
fn case6_o_refuses_an_input_file() {
    let f = Fixture::new("dotfiles");
    let before = fs::read(f.home.join(".tmux.conf")).expect("read entry");
    let out = f.run(&["-o", f.home.join(".tmux.conf").to_str().expect("utf8")]);
    assert_eq!(out.status.code(), Some(2), "stderr:\n{}", out.stderr);
    let after = fs::read(f.home.join(".tmux.conf")).expect("read entry");
    assert_eq!(before, after, "-o must never write to an input file");
}

#[test]
fn case6_o_refuses_a_symlink_to_an_input_file() {
    let f = Fixture::new("canopy");
    let target = f.home.join(".local/state/canopy/10-keys.conf");
    let link = f.tmp.path().join("out-link.toml");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let before = fs::read(&target).expect("read target");
    let out = f.run(&["-o", link.to_str().expect("utf8")]);
    assert_eq!(out.status.code(), Some(2), "stderr:\n{}", out.stderr);
    let after = fs::read(&target).expect("read target");
    assert_eq!(
        before, after,
        "-o must never write through a symlink to an input file"
    );
}

// ---- case 8: $CANOPY_STORE never expands in a command ----

#[test]
fn case8_canopy_store_in_the_importers_env_never_changes_stdout() {
    let f = Fixture::new("canopy");
    let out = f.run(&[]);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{}", out.stderr);
    assert!(out.stdout.contains("$CANOPY_STORE"));

    let mut cmd = f.command(&[]);
    cmd.env("CANOPY_STORE", "/nowhere");
    let out2 = run_bounded(cmd, TIMEOUT);
    assert_eq!(out2.status.code(), Some(0), "stderr:\n{}", out2.stderr);
    assert_eq!(
        out.stdout, out2.stdout,
        "CANOPY_STORE in the importer's own env must not change a binding's command text"
    );
}

// ---- case 12: publishability ----

#[test]
fn case12_fixtures_hold_no_personal_identifiers() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/import-tmux");
    let hostname = hostname_lossy();
    let git_email = git_user_email();
    let mut offenders = Vec::new();
    walk_files(&root, &mut |path| {
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        if text.contains("/Users/") || text.contains("/home/") {
            offenders.push(format!("{}: absolute home path", path.display()));
        }
        if contains_email(&text) {
            offenders.push(format!("{}: email address", path.display()));
        }
        if let Some(h) = &hostname
            && !h.is_empty()
            && text.contains(h.as_str())
        {
            offenders.push(format!("{}: this machine's hostname", path.display()));
        }
        if let Some(e) = &git_email
            && !e.is_empty()
            && text.contains(e.as_str())
        {
            offenders.push(format!("{}: git config user.email", path.display()));
        }
    });
    assert!(
        offenders.is_empty(),
        "personal identifiers found:\n{}",
        offenders.join("\n")
    );
}

fn walk_files(dir: &Path, f: &mut impl FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            walk_files(&path, f);
        } else {
            f(&path);
        }
    }
}

fn contains_email(text: &str) -> bool {
    for word in text.split(|c: char| c.is_whitespace() || "'\"<>()".contains(c)) {
        if let Some((local, domain)) = word.split_once('@')
            && !local.is_empty()
            && domain.contains('.')
            && domain
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            return true;
        }
    }
    false
}

fn hostname_lossy() -> Option<String> {
    let out = Command::new("hostname").output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_user_email() -> Option<String> {
    let out = Command::new("git")
        .args(["config", "user.email"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
