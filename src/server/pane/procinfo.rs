//! foreground command and cwd, per OS.
//!
//! The foreground process group comes from `tcgetpgrp` on the master; its
//! leader's name, argv and cwd come from `/proc` on Linux and from
//! `proc_name`, `proc_pidinfo` and `sysctl(KERN_PROCARGS2)` on macOS.

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::io::unix::AsyncFd;

use super::lock;

/// How often the foreground process is sampled.
const SAMPLE_EVERY: Duration = Duration::from_secs(1);

/// This process's own argv, resolved once. A freshly forked pane child is,
/// until it execs, still byte-for-byte a copy of the server's own image:
/// [`sampler`] uses this to recognise (and discard) a sample taken of that
/// pre-exec image, rather than report the server's own argv as the pane's.
fn server_argv() -> &'static [String] {
    static ARGV: OnceLock<Vec<String>> = OnceLock::new();
    ARGV.get_or_init(|| std::env::args().collect())
}

/// The last sample of a pane's foreground process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcSample {
    pub command: Option<String>,
    pub argv: Option<Vec<String>>,
    pub path: Option<PathBuf>,
}

/// Sampler task: refresh `out` every `SAMPLE_EVERY`. A failed sample (the
/// pane is dead, or the process vanished mid-sample) keeps the previous one.
pub(crate) async fn sampler(master: Arc<AsyncFd<OwnedFd>>, out: Arc<Mutex<ProcSample>>) {
    let mut tick = tokio::time::interval(SAMPLE_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Ok(pgrp) = nix::unistd::tcgetpgrp(master.get_ref()) else {
            continue;
        };
        let sample = sample(pgrp.as_raw());
        // The pty's foreground process group is the child's own pid as
        // soon as it calls setsid()+TIOCSCTTY, before it execs; on macOS
        // the introspection APIs below can keep reporting that pre-exec
        // image (still literally the server's own argv) for close to a
        // second afterwards. Skip such a sample rather than store it, so a
        // caller sees either nothing yet or the real command, never ours.
        if sample.argv.as_deref() == Some(server_argv()) {
            continue;
        }
        if sample.command.is_some() || sample.argv.is_some() || sample.path.is_some() {
            *lock(&out) = sample;
        }
    }
}

/// The foreground process group of the pty behind `master`, read now
/// (`tcgetpgrp`): what a key binding's conditional tests, as
/// vim-tmux-navigator's `ps -t <tty>` does on every keypress. The reads
/// below use syscalls alone, never a fork.
pub fn foreground_group(master: impl std::os::fd::AsFd) -> Option<i32> {
    Some(nix::unistd::tcgetpgrp(master).ok()?.as_raw())
}

/// The name, argv and cwd of group `pgrp`'s leader, read now; empty once
/// the leader has exited (the first command of a pipeline often has), or
/// while it is a forked child that has not exec'd yet (still the server's
/// own image), which [`sampler`] leaves out too.
pub fn leader(pgrp: i32) -> ProcSample {
    let leader = sample(pgrp);
    if leader.argv.as_deref() == Some(server_argv()) {
        return ProcSample::default();
    }
    leader
}

/// The argv of every live member of group `pgrp`, the leader first, then
/// by pid, read now.
pub fn members(pgrp: i32) -> Vec<Vec<String>> {
    let mut pids = os::group(pgrp);
    pids.sort_unstable();
    pids.dedup();
    if let Some(at) = pids.iter().position(|&p| p == pgrp) {
        pids.remove(at);
        pids.insert(0, pgrp);
    }
    pids.into_iter()
        .filter_map(os::argv)
        .filter(|argv| argv != server_argv())
        .collect()
}

/// `members` as `#{pane_foreground_argv}` shows them: one argv per line,
/// its words joined by spaces. A newline inside a word becomes a space, so
/// no argument can pass for a member of its own.
pub fn argv_lines(members: &[Vec<String>]) -> String {
    members
        .iter()
        .map(|argv| {
            argv.iter()
                .map(|w| w.replace(['\n', '\r'], " "))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// How many times [`sample`] reads again after the name changed under it.
const SAMPLE_TRIES: usize = 4;

/// Name, argv and cwd of `pid`; each is `None` if it could not be read.
/// The three reads are separate calls, so a process that execs between
/// them would give its old name with its new argv; the name is read again
/// after the rest, and a changed one takes the sample again.
pub fn sample(pid: i32) -> ProcSample {
    let mut command = os::command(pid);
    let mut tries = 0;
    loop {
        let argv = os::argv(pid);
        let path = os::cwd(pid);
        let after = os::command(pid);
        tries += 1;
        // A process that keeps exec'ing is read at most SAMPLE_TRIES times;
        // the last read stands.
        if after == command || tries == SAMPLE_TRIES {
            return ProcSample {
                command: after,
                argv,
                path,
            };
        }
        command = after;
    }
}

#[cfg(target_os = "linux")]
mod os {
    use std::path::PathBuf;

    pub fn command(pid: i32) -> Option<String> {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        Some(comm.trim_end_matches('\n').to_string())
    }

    pub fn argv(pid: i32) -> Option<Vec<String>> {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        split_nul(&raw)
    }

    pub fn cwd(pid: i32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    /// Every pid in process group `pgrp`, from each `/proc/<pid>/stat`.
    pub fn group(pgrp: i32) -> Vec<i32> {
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
            .filter(|pid| stat_pgrp(*pid) == Some(pgrp))
            .collect()
    }

    /// Field 5 of `/proc/<pid>/stat`, after the parenthesised name, which
    /// may itself hold spaces and parentheses.
    fn stat_pgrp(pid: i32) -> Option<i32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = stat.get(stat.rfind(')')? + 2..)?;
        rest.split(' ').nth(2)?.parse().ok()
    }

    fn split_nul(raw: &[u8]) -> Option<Vec<String>> {
        let raw = raw.strip_suffix(&[0]).unwrap_or(raw);
        if raw.is_empty() {
            return None;
        }
        Some(
            raw.split(|&b| b == 0)
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect(),
        )
    }
}

#[cfg(target_os = "macos")]
mod os {
    use std::ffi::{CStr, c_void};
    use std::mem;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    use nix::libc;

    pub fn command(pid: i32) -> Option<String> {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for its full length.
        let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if n <= 0 {
            return None;
        }
        Some(String::from_utf8_lossy(&buf[..n as usize]).into_owned())
    }

    pub fn cwd(pid: i32) -> Option<PathBuf> {
        // SAFETY: an all-zero proc_vnodepathinfo is a valid value.
        let mut info: libc::proc_vnodepathinfo = unsafe { mem::zeroed() };
        let size = mem::size_of::<libc::proc_vnodepathinfo>() as i32;
        // SAFETY: `info` is valid for `size` bytes.
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&raw mut info).cast::<c_void>(),
                size,
            )
        };
        if n != size {
            return None;
        }
        // vip_path is a MAXPATHLEN char array declared as [[c_char; 32]; 32].
        // SAFETY: the array is contiguous and at least one byte long.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                info.pvi_cdir.vip_path.as_ptr().cast::<u8>(),
                mem::size_of_val(&info.pvi_cdir.vip_path),
            )
        };
        let path = CStr::from_bytes_until_nul(bytes).ok()?.to_bytes();
        if path.is_empty() {
            return None;
        }
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(path)))
    }

    /// `proc_listpids`'s type for "the members of one process group"
    /// (libproc.h); the libc crate does not name it.
    const PROC_PGRP_ONLY: u32 = 2;

    /// Every pid in process group `pgrp`.
    pub fn group(pgrp: i32) -> Vec<i32> {
        let Ok(pgrp) = u32::try_from(pgrp) else {
            return Vec::new();
        };
        // A group larger than the buffer is cut short; a foreground group
        // of a thousand processes is not one a pane holds.
        let mut pids = vec![0 as libc::c_int; 1024];
        let size = (pids.len() * mem::size_of::<libc::c_int>()) as libc::c_int;
        // SAFETY: `pids` is valid for `size` bytes.
        let n = unsafe {
            libc::proc_listpids(
                PROC_PGRP_ONLY,
                pgrp,
                pids.as_mut_ptr().cast::<c_void>(),
                size,
            )
        };
        let n = usize::try_from(n).unwrap_or(0) / mem::size_of::<libc::c_int>();
        pids.truncate(n.min(pids.len()));
        pids.retain(|&p| p > 0);
        pids
    }

    /// The full argv from `KERN_PROCARGS2`: `argc` as an i32, the exec path,
    /// NUL padding, then `argc` NUL-terminated strings (then the environment).
    pub fn argv(pid: i32) -> Option<Vec<String>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        let mut argmax: libc::c_int = 0;
        let mut len = mem::size_of::<libc::c_int>();
        // SAFETY: mib and the output buffer are valid for the lengths given.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&raw mut argmax).cast::<c_void>(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || argmax <= 0 {
            return None;
        }
        let mut buf = vec![0u8; argmax as usize];
        let mut len = buf.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        // SAFETY: mib and buf are valid for the lengths given.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast::<c_void>(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return None;
        }
        parse_procargs2(&buf[..len])
    }

    pub(super) fn parse_procargs2(buf: &[u8]) -> Option<Vec<String>> {
        let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
        let rest = &buf[4..];
        // Skip the exec path, then the NUL padding after it.
        let path_end = rest.iter().position(|&b| b == 0)?;
        let mut rest = &rest[path_end..];
        let start = rest.iter().position(|&b| b != 0)?;
        rest = &rest[start..];
        let mut argv = Vec::with_capacity(argc.max(0) as usize);
        for _ in 0..argc {
            let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
            argv.push(String::from_utf8_lossy(&rest[..end]).into_owned());
            rest = rest.get(end + 1..).unwrap_or(&[]);
        }
        (!argv.is_empty()).then_some(argv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_own_process() {
        let s = sample(std::process::id() as i32);
        assert!(s.command.is_some());
        assert!(!s.argv.unwrap_or_default().is_empty());
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert_eq!(s.path.map(|p| p.canonicalize().unwrap()), Some(cwd));
    }

    /// [`os::group`] lists this process's own group: this process, and a
    /// child spawned into it.
    #[test]
    fn lists_a_process_group() {
        let me = std::process::id() as i32;
        let pgrp = nix::unistd::getpgrp().as_raw();
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let members = os::group(pgrp);
        let _ = child.kill();
        let _ = child.wait();
        assert!(members.contains(&me), "{members:?}");
        assert!(members.contains(&(child.id() as i32)), "{members:?}");
    }

    #[test]
    fn a_newline_in_an_argument_is_not_a_member_of_its_own() {
        let members = vec![
            vec!["sh".to_string(), "-c".to_string(), "x\nnvim".to_string()],
            vec!["cat".to_string(), "a\r\nb".to_string()],
        ];
        assert_eq!(argv_lines(&members), "sh -c x nvim\ncat a  b");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parses_procargs2() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/sleep\0\0\0\0sleepy\x0030\0HOME=/x\0");
        assert_eq!(
            os::parse_procargs2(&buf),
            Some(vec!["sleepy".to_string(), "30".to_string()])
        );
    }
}
