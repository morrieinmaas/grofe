//! Desktop notification delivery (agents spec §6.4).
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Mac,
    Linux,
}

/// Build the argv for one desktop notification. Agent text is only ever an
/// argv element, never shell or script source.
pub fn argv(
    os: Os,
    title: &str,
    msg: &str,
    sound: bool,
    custom: Option<&[String]>,
    pane: &str,
    have_terminal_notifier: bool,
) -> Vec<String> {
    if let Some(tpl) = custom {
        return tpl
            .iter()
            .map(|e| {
                e.replace("{title}", title)
                    .replace("{message}", msg)
                    .replace("{pane}", pane)
            })
            .collect();
    }
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    match os {
        Os::Linux => {
            let mut a = s(&["notify-send", "-a", "grofe"]);
            if sound {
                a.extend(s(&["-h", "string:sound-name:message-new-instant"]));
            }
            a.push("--".into());
            a.push(title.into());
            a.push(msg.into());
            a
        }
        Os::Mac if have_terminal_notifier => {
            // terminal-notifier reads a leading '-' in a value as an option
            let safe = |v: &str| {
                if v.starts_with('-') {
                    format!(" {v}")
                } else {
                    v.to_string()
                }
            };
            let mut a = vec![
                "terminal-notifier".to_string(),
                "-title".into(),
                safe(title),
                "-message".into(),
                safe(msg),
            ];
            if sound {
                a.extend(s(&["-sound", "default"]));
            }
            a
        }
        Os::Mac => {
            let show = if sound {
                "display notification (item 2 of argv) with title (item 1 of argv) sound name \"default\""
            } else {
                "display notification (item 2 of argv) with title (item 1 of argv)"
            };
            let mut a = s(&[
                "osascript",
                "-e",
                "on run argv",
                "-e",
                show,
                "-e",
                "end run",
                "--",
            ]);
            a.push(title.into());
            a.push(msg.into());
            a
        }
    }
}

/// OSC 9 desktop notification with C0/C1 controls removed from the text.
pub fn osc9(text: &str) -> Vec<u8> {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]9;{clean}\x1b\\").into_bytes()
}

#[derive(Default)]
struct Slot {
    running: bool,
    pending: Option<Vec<String>>,
}

/// Runs argv commands off the actor: one in flight, the newest waiting.
pub struct Runner {
    slot: Arc<Mutex<Slot>>,
    timeout: Duration,
}

impl Default for Runner {
    fn default() -> Self {
        Self::with_timeout(Duration::from_secs(10))
    }
}

impl Runner {
    pub fn with_timeout(timeout: Duration) -> Self {
        Runner {
            slot: Arc::default(),
            timeout,
        }
    }

    /// Outside a tokio runtime this is a no-op.
    pub fn submit(&mut self, argv: Vec<String>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if argv.is_empty() {
            return;
        }
        {
            let Ok(mut slot) = self.slot.lock() else {
                return;
            };
            if slot.running {
                slot.pending = Some(argv);
                return;
            }
            slot.running = true;
        }
        let slot = self.slot.clone();
        let timeout = self.timeout;
        handle.spawn(async move {
            let mut next = argv;
            loop {
                run_one(&next, timeout).await;
                let Ok(mut s) = slot.lock() else { return };
                match s.pending.take() {
                    Some(p) => next = p,
                    None => {
                        s.running = false;
                        return;
                    }
                }
            }
        });
    }
}

async fn run_one(argv: &[String], timeout: Duration) {
    let Ok(mut child) = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    else {
        return;
    };
    // dropping the child on timeout kills it
    let _ = tokio::time::timeout(timeout, child.wait()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn linux_argv_has_dashdash() {
        assert_eq!(
            argv(Os::Linux, "grofe", "-rf $(x)", false, None, "%1", false),
            v(&["notify-send", "-a", "grofe", "--", "grofe", "-rf $(x)"])
        );
    }

    #[test]
    fn linux_sound_before_dashdash() {
        let a = argv(Os::Linux, "t", "m", true, None, "%1", false);
        let h = a.iter().position(|x| x == "-h").unwrap();
        let d = a.iter().position(|x| x == "--").unwrap();
        assert!(h < d);
        assert_eq!(a[h + 1], "string:sound-name:message-new-instant");
    }

    #[test]
    fn mac_osascript_text_is_data() {
        let a = argv(Os::Mac, "t", "m\"q", true, None, "%1", false);
        assert_eq!(a[0], "osascript");
        assert_eq!(a[a.len() - 2..], v(&["t", "m\"q"])[..]);
        assert_eq!(a[a.len() - 3], "--");
        assert!(a.iter().any(|x| x.contains("sound name \"default\"")));
        let quiet = argv(Os::Mac, "t", "m", false, None, "%1", false);
        assert!(!quiet.iter().any(|x| x.contains("sound name")));
    }

    #[test]
    fn terminal_notifier_prefixes_dash_values() {
        let a = argv(Os::Mac, "t", "-rf", false, None, "%1", true);
        assert_eq!(
            a,
            v(&["terminal-notifier", "-title", "t", "-message", " -rf"])
        );
    }

    #[test]
    fn custom_placeholders_single_element() {
        let tpl = v(&["n", "--", "{message}"]);
        assert_eq!(
            argv(Os::Linux, "t", "a b", false, Some(&tpl), "%1", false),
            v(&["n", "--", "a b"])
        );
    }

    #[test]
    fn osc9_bytes() {
        assert_eq!(osc9("hi"), b"\x1b]9;hi\x1b\\");
    }

    #[test]
    fn osc9_strips_controls() {
        assert_eq!(osc9("a\x1bb\x07c\u{9b}d\ne"), b"\x1b]9;abcde\x1b\\");
    }

    #[test]
    fn submit_outside_runtime_is_noop() {
        Runner::default().submit(v(&["true"]));
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("grofe-desktop-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[tokio::test]
    async fn runner_coalesces() {
        let f = tmp("coalesce");
        let mut r = Runner::default();
        let cmd = |n: &str| {
            v(&[
                "sh",
                "-c",
                &format!("echo {n} >> {}; sleep 0.3", f.display()),
            ])
        };
        r.submit(cmd("1"));
        r.submit(cmd("2"));
        r.submit(cmd("3"));
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let out = std::fs::read_to_string(&f).unwrap_or_default();
        let _ = std::fs::remove_file(&f);
        assert_eq!(out, "1\n3\n");
    }

    #[tokio::test]
    async fn runner_timeout_kills() {
        let f = tmp("timeout");
        let mut r = Runner::with_timeout(Duration::from_millis(150));
        r.submit(v(&[
            "sh",
            "-c",
            &format!("sleep 1; echo x > {}", f.display()),
        ]));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!f.exists());
        // the slot is free again
        let g = tmp("after");
        r.submit(v(&["sh", "-c", &format!("echo y > {}", g.display())]));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(g.exists());
        let _ = std::fs::remove_file(&g);
    }
}
