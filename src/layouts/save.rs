//! `layout save`: a running session written back as a layout file.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde::Serialize;

use super::{Dirs, Server, words};
use crate::server::layout::tmux_checksum;

#[derive(Serialize)]
struct OutSession {
    name: String,
    root: String,
    window: Vec<OutWindow>,
}

#[derive(Serialize)]
struct OutWindow {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    layout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    focus: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pane: Vec<OutPane>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    float: Vec<OutFloat>,
}

#[derive(Serialize)]
struct OutPane {
    cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cmd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    focus: Option<bool>,
}

#[derive(Serialize)]
struct OutFloat {
    cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cmd: Option<String>,
    x: i64,
    y: i64,
    width: i64,
    height: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    border: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    focus: Option<bool>,
}

/// Programs that are a shell, not a command to start again.
const SHELLS: [&str; 11] = [
    "sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "nu", "elvish", "xonsh",
];

/// The layout string of the tiled panes only: `layout` is a window's
/// `#{window_layout}`, which lists floating panes (`floats` are their
/// `WxH,x,y,id` cells) after the tiled ones and again in a `<...>` stack,
/// and which `select-layout` does not take back.
pub fn tiled_layout(layout: &str, floats: &[String]) -> Option<String> {
    if floats.is_empty() {
        return Some(layout.to_string());
    }
    let (_, body) = layout.split_once(',')?;
    let body = match body.rfind('<') {
        Some(i) if body.ends_with('>') => &body[..i],
        _ => body,
    };
    let mut body = body.to_string();
    for cell in floats {
        let needle = format!(",{cell}");
        let at = body.match_indices(&needle).find(|(i, _)| {
            matches!(
                body.as_bytes().get(i + needle.len()),
                Some(b',' | b']' | b'}')
            )
        })?;
        body.replace_range(at.0..at.0 + needle.len(), "");
    }
    // A lone tiled pane was wrapped in a stack with the floats.
    let lone = regex::Regex::new(r"^\d+x\d+,\d+,\d+\[(\d+x\d+,\d+,\d+,\d+)\]$").ok()?;
    if let Some(c) = lone.captures(&body) {
        body = c[1].to_string();
    }
    Some(format!("{:04x},{body}", tmux_checksum(&body)))
}

fn fields(line: &str, n: usize) -> Result<Vec<String>, String> {
    let f: Vec<String> = line.split('\t').map(str::to_string).collect();
    if f.len() == n {
        Ok(f)
    } else {
        Err(format!("unexpected server output {line:?}"))
    }
}

pub fn save(
    server: &mut dyn Server,
    dirs: &Dirs,
    session: &str,
    dest: Option<&str>,
    force: bool,
) -> Result<PathBuf, String> {
    let (sid, name) = server
        .sessions()?
        .into_iter()
        .find(|(id, n)| n == session || id == session)
        .ok_or_else(|| format!("can't find session: {session}"))?;
    let path = match dest {
        Some(d) => dirs.resolve(d)?,
        None => dirs.resolve(&name).map_err(|_| {
            format!("session name {name:?} is not a layout name: give one to save as")
        })?,
    };
    if path.exists() && !force {
        return Err(format!(
            "{} exists: use --force to replace it",
            path.display()
        ));
    }
    let shell = server
        .run(&words(&["show-options", "-gv", "default-shell"]))?
        .trim()
        .to_string();
    let host = server
        .run(&words(&["display-message", "-p", "#{host}"]))?
        .trim()
        .to_string();
    let root = server
        .run(&words(&[
            "display-message",
            "-p",
            "-t",
            &sid,
            "#{session_path}",
        ]))?
        .trim()
        .to_string();
    let wins = server.run(&words(&[
        "list-windows",
        "-t",
        &sid,
        "-F",
        "#{window_id}\t#{window_name}\t#{window_active}\t#{window_layout}",
    ]))?;
    let mut windows = Vec::new();
    for line in wins.lines().filter(|l| !l.is_empty()) {
        let [wid, wname, wactive, wlayout] = fields(line, 4)?.try_into().unwrap_or_default();
        let panes = server.run(&words(&[
            "list-panes",
            "-t",
            &wid,
            "-F",
            "#{pane_id}\t#{pane_floating_flag}\t#{pane_active}\t#{pane_current_path}\t#{pane_current_command}\t#{pane_x}\t#{pane_y}\t#{pane_width}\t#{pane_height}\t#{pane_title}",
        ]))?;
        let mut tiled = Vec::new();
        let mut float = Vec::new();
        let mut cells = Vec::new();
        for pl in panes.lines().filter(|l| !l.is_empty()) {
            let f = fields(pl, 10)?;
            let focus = (f[2] == "1").then_some(true);
            let cmd = foreground(&f[4], &shell);
            if f[1] == "1" {
                let num = f[0].trim_start_matches('%');
                cells.push(format!("{}x{},{},{},{num}", f[7], f[8], f[5], f[6]));
                let border = server
                    .run(&words(&["show-options", "-p", "-t", &f[0]]))?
                    .lines()
                    .find_map(|l| l.strip_prefix("pane-border-lines ").map(str::to_string));
                float.push(OutFloat {
                    cwd: f[3].clone(),
                    cmd,
                    x: f[5].parse().unwrap_or(0),
                    y: f[6].parse().unwrap_or(0),
                    width: f[7].parse().unwrap_or(1),
                    height: f[8].parse().unwrap_or(1),
                    border,
                    title: (f[9] != host && !f[9].is_empty()).then(|| f[9].clone()),
                    focus,
                });
            } else {
                tiled.push(OutPane {
                    cwd: f[3].clone(),
                    cmd,
                    focus,
                });
            }
        }
        let layout = if tiled.len() > 1 {
            tiled_layout(&wlayout, &cells)
        } else {
            None
        };
        windows.push(OutWindow {
            name: wname,
            layout,
            focus: (wactive == "1").then_some(true),
            pane: tiled,
            float,
        });
    }
    let text = toml::to_string(&OutSession {
        name,
        root,
        window: windows,
    })
    .map_err(|e| e.to_string())?;
    write_file(&path, &text, force)?;
    Ok(path)
}

/// The command to save for a pane: the foreground program unless it is a shell.
fn foreground(command: &str, default_shell: &str) -> Option<String> {
    let command = command.trim_start_matches('-');
    let default = default_shell.rsplit('/').next().unwrap_or("");
    (!command.is_empty() && command != default && !SHELLS.contains(&command))
        .then(|| command.to_string())
}

/// Creates the file (and its directory); without `force` it must not exist.
fn write_file(path: &std::path::Path, text: &str, force: bool) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut open = fs::OpenOptions::new();
    open.write(true);
    if force {
        open.create(true).truncate(true);
    } else {
        open.create_new(true);
    }
    let mut f = open
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(text.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_floats_from_a_stack() {
        let tiled = tiled_layout(
            "50ca,80x24,0,0[80x12,0,0,0,80x11,0,13,1,20x5,3,4,2]<20x5,3,4,2>",
            &["20x5,3,4,2".to_string()],
        )
        .unwrap();
        let body = "80x24,0,0[80x12,0,0,0,80x11,0,13,1]";
        assert_eq!(tiled, format!("{:04x},{body}", tmux_checksum(body)));
        assert!(crate::server::layout::parse_tmux_layout(&tiled).is_some());
    }

    #[test]
    fn strips_floats_around_a_lone_pane() {
        let body = "80x24,0,0[80x24,0,0,0,20x5,3,4,2]<20x5,3,4,2>";
        let layout = format!("{:04x},{body}", tmux_checksum(body));
        let tiled = tiled_layout(&layout, &["20x5,3,4,2".to_string()]).unwrap();
        let want = "80x24,0,0,0";
        assert_eq!(tiled, format!("{:04x},{want}", tmux_checksum(want)));
    }

    #[test]
    fn leaves_a_floatless_layout_alone() {
        assert_eq!(
            tiled_layout("abcd,1x1,0,0,0", &[]).unwrap(),
            "abcd,1x1,0,0,0"
        );
    }

    #[test]
    fn a_shell_is_not_a_command() {
        assert_eq!(foreground("zsh", "/bin/zsh"), None);
        assert_eq!(foreground("-bash", "/bin/sh"), None);
        assert_eq!(foreground("nvim", "/bin/zsh"), Some("nvim".into()));
    }
}
