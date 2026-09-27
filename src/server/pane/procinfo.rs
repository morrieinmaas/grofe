//! foreground command and cwd, per OS.
//!
//! The foreground process group comes from `tcgetpgrp` on the master; its
//! leader's name, argv and cwd come from `/proc` on Linux and from
//! `proc_name`, `proc_pidinfo` and `sysctl(KERN_PROCARGS2)` on macOS.

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::unix::AsyncFd;

use super::lock;

/// How often the foreground process is sampled.
const SAMPLE_EVERY: Duration = Duration::from_secs(1);

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
        if sample.command.is_some() || sample.argv.is_some() || sample.path.is_some() {
            *lock(&out) = sample;
        }
    }
}

/// Name, argv and cwd of `pid`; each is `None` if it could not be read.
pub fn sample(pid: i32) -> ProcSample {
    ProcSample {
        command: os::command(pid),
        argv: os::argv(pid),
        path: os::cwd(pid),
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
