//! foreground command and cwd, per OS.
//!
//! The foreground process group comes from `tcgetpgrp` on the master; its
//! leader's name, argv and cwd come from `/proc` on Linux and from
//! `proc_name`, `proc_pidinfo` and `sysctl(KERN_PROCARGS2)` on macOS.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::lock;

/// This process's own argv, resolved once. A freshly forked pane child is,
/// until it execs, still byte-for-byte a copy of the server's own image:
/// [`Cache`] uses this to recognise (and discard) a sample taken of that
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

/// How long a sample is reused: a format that reads the command, argv and
/// path in one expansion samples once, not three times.
const SAMPLE_TTL: Duration = Duration::from_millis(20);

/// A pane's foreground process, sampled when asked (a format, the
/// `automatic-rename` check) rather than on a timer, so an idle server
/// wakes for nothing.
#[derive(Default)]
pub(crate) struct Cache(Mutex<(Option<Instant>, ProcSample)>);

impl Cache {
    /// The foreground process of the pty behind `master`, at most
    /// `SAMPLE_TTL` old. A failed sample (the pane is dead, or the process
    /// vanished mid-sample) keeps the previous one.
    pub(crate) fn get(&self, master: impl std::os::fd::AsFd) -> ProcSample {
        let mut cache = lock(&self.0);
        if cache.0.is_some_and(|at| at.elapsed() < SAMPLE_TTL) {
            return cache.1.clone();
        }
        cache.0 = Some(Instant::now());
        if let Ok(pgrp) = nix::unistd::tcgetpgrp(master) {
            let sample = sample(pgrp.as_raw());
            let pre_exec = sample.argv.as_deref() == Some(server_argv());
            if !pre_exec
                && (sample.command.is_some() || sample.argv.is_some() || sample.path.is_some())
            {
                cache.1 = sample;
            }
        }
        cache.1.clone()
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
/// own image), which [`Cache`] leaves out too.
pub fn leader(pgrp: i32) -> ProcSample {
    let leader = sample(pgrp);
    if leader.argv.as_deref() == Some(server_argv()) {
        return ProcSample::default();
    }
    leader
}

/// The cwd of group `pgrp`'s leader, read now with one syscall (or one
/// `readlink`), never a fork: tmux's `osdep_get_cwd`.
pub fn cwd(pgrp: i32) -> Option<PathBuf> {
    os::cwd(pgrp)
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

/// The parent of process `pid`.
pub fn parent(pid: i32) -> Option<i32> {
    os::parent(pid)
}

/// When process `pid` started, in the OS's own units (clock ticks since
/// boot on Linux, microseconds since the epoch on macOS): with the pid,
/// what tells a process from a later one that reuses its pid. `None` once
/// it has exited, a zombie included; a stopped process still has one.
pub fn start_time(pid: i32) -> Option<u64> {
    os::start_time(pid)
}

/// The value of `name` in the environment process `pid` was started (or
/// last exec'd) with, read now: `/proc/<pid>/environ` on Linux, the tail
/// of `KERN_PROCARGS2` on macOS. `None` if it is unset or unreadable (a
/// process of another user). A shell's later `export` is not seen.
pub fn env_var(pid: i32, name: &str) -> Option<String> {
    os::environ(pid)?.into_iter().find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == name).then(|| v.to_string())
    })
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

    /// Field 22 of `/proc/<pid>/stat`, unless field 3 says the process
    /// is a zombie (`Z`) or dead (`X`).
    pub fn start_time(pid: i32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = stat.get(stat.rfind(')')? + 2..)?;
        let mut fields = rest.split(' ');
        if matches!(fields.next()?, "Z" | "X") {
            return None;
        }
        fields.nth(18)?.parse().ok()
    }

    pub fn environ(pid: i32) -> Option<Vec<String>> {
        let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        split_nul(&raw)
    }

    /// Field 4 of `/proc/<pid>/stat`.
    pub fn parent(pid: i32) -> Option<i32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = stat.get(stat.rfind(')')? + 2..)?;
        rest.split(' ').nth(1)?.parse().ok()
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

    /// `proc_pidinfo`'s `PROC_PIDTBSDINFO` for `pid`.
    fn bsdinfo(pid: i32) -> Option<libc::proc_bsdinfo> {
        // SAFETY: an all-zero proc_bsdinfo is a valid value.
        let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
        let size = mem::size_of::<libc::proc_bsdinfo>() as i32;
        // SAFETY: `info` is valid for `size` bytes.
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&raw mut info).cast::<c_void>(),
                size,
            )
        };
        (n == size).then_some(info)
    }

    /// The parent pid, from `proc_pidinfo`'s `PROC_PIDTBSDINFO`.
    pub fn parent(pid: i32) -> Option<i32> {
        bsdinfo(pid).map(|info| info.pbi_ppid as i32)
    }

    /// The start time in microseconds since the epoch, from
    /// `PROC_PIDTBSDINFO`; `None` for a zombie.
    pub fn start_time(pid: i32) -> Option<u64> {
        let info = bsdinfo(pid)?;
        if info.pbi_status == libc::SZOMB {
            return None;
        }
        Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
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

    pub fn argv(pid: i32) -> Option<Vec<String>> {
        parse_procargs2(&procargs2(pid)?).map(|(argv, _)| argv)
    }

    pub fn environ(pid: i32) -> Option<Vec<String>> {
        parse_procargs2(&procargs2(pid)?).map(|(_, env)| env)
    }

    /// The raw `KERN_PROCARGS2` of `pid`: `argc` as an i32, the exec path,
    /// NUL padding, then `argc` NUL-terminated strings, then the
    /// environment's.
    fn procargs2(pid: i32) -> Option<Vec<u8>> {
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
        buf.truncate(len);
        Some(buf)
    }

    /// The argv and the environment in a `KERN_PROCARGS2` buffer.
    pub(super) fn parse_procargs2(buf: &[u8]) -> Option<(Vec<String>, Vec<String>)> {
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
        if argv.is_empty() {
            return None;
        }
        // The environment runs to the first empty string; the padding
        // and the `apple[]` strings after it are not the process's.
        let env = rest
            .split(|&b| b == 0)
            .take_while(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        Some((argv, env))
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

    /// A start time is stable for a live process, and gone with it once it
    /// has exited and been reaped.
    #[test]
    fn start_time_tells_a_live_process_from_a_gone_one() {
        let me = std::process::id() as i32;
        let first = start_time(me);
        assert!(first.is_some());
        assert_eq!(start_time(me), first);
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        assert_eq!(start_time(pid), None);
    }

    /// A zombie (exited, not yet reaped) has no start time.
    #[test]
    fn a_zombie_has_no_start_time() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        let gone = (0..200).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            start_time(pid).is_none()
        });
        child.wait().unwrap();
        assert!(gone, "an exited child still has a start time");
    }

    /// A stopped process keeps its start time.
    #[test]
    fn a_stopped_process_has_a_start_time() {
        use nix::sys::signal::{Signal, kill};
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let pid = nix::unistd::Pid::from_raw(child.id() as i32);
        let before = start_time(pid.as_raw());
        kill(pid, Signal::SIGSTOP).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let stopped = start_time(pid.as_raw());
        let _ = child.kill();
        let _ = child.wait();
        assert!(before.is_some());
        assert_eq!(stopped, before);
    }

    /// Not a test of its own: the process `reads_a_child_s_environment`
    /// reads, so that it does not depend on a system binary.
    #[test]
    #[ignore = "the child process of reads_a_child_s_environment"]
    fn child_that_sleeps() {
        std::thread::sleep(std::time::Duration::from_secs(5));
    }

    /// The child is this test binary, not `sleep`: macOS 27 hides the
    /// environment of Apple's own binaries (/bin/sleep, /bin/zsh) from
    /// other processes, and the test is about grofe's reader.
    #[test]
    fn reads_a_child_s_environment() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "--quiet"])
            .arg("server::pane::procinfo::tests::child_that_sleeps")
            .env_clear()
            .env("PATH", "/x/bin:/usr/bin:/bin")
            .env("GROFE_T42", "a=b")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let read = |name| {
            (0..200).find_map(|_| {
                std::thread::sleep(std::time::Duration::from_millis(5));
                env_var(pid, name)
            })
        };
        let path = read("PATH");
        let value = read("GROFE_T42");
        let unset = env_var(pid, "HOME");
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(path.as_deref(), Some("/x/bin:/usr/bin:/bin"));
        assert_eq!(value.as_deref(), Some("a=b"));
        assert_eq!(unset, None);
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
            Some((
                vec!["sleepy".to_string(), "30".to_string()],
                vec!["HOME=/x".to_string()]
            ))
        );
    }
}
