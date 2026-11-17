//! The TCP ports the processes of a tree listen on: the listening sockets
//! from `/proc/net/tcp{,6}` and `/proc/<pid>/fd` on Linux, and from
//! `proc_pidinfo` and `proc_pidfdinfo` on macOS.

/// The TCP ports the processes below each of `roots` (the roots included)
/// listen on, one sorted list per root, read now. Reads every process, so
/// it belongs off the actor (`spawn_blocking`).
pub fn listening_ports(roots: &[i32]) -> Vec<Vec<u16>> {
    let mut children: std::collections::HashMap<i32, Vec<i32>> = std::collections::HashMap::new();
    for (pid, ppid) in super::os::all_parents() {
        children.entry(ppid).or_default().push(pid);
    }
    let sockets = Sockets::new();
    roots
        .iter()
        .map(|&root| {
            let mut seen = std::collections::HashSet::from([root]);
            let mut todo = vec![root];
            let mut ports = Vec::new();
            while let Some(pid) = todo.pop() {
                ports.extend(sockets.listening(pid));
                for &child in children.get(&pid).into_iter().flatten() {
                    if seen.insert(child) {
                        todo.push(child);
                    }
                }
            }
            ports.sort_unstable();
            ports.dedup();
            ports
        })
        .collect()
}

/// The inode and local port of each listening socket (state `0A`) in a
/// `/proc/net/tcp` or `/proc/net/tcp6` table; header and malformed lines
/// are skipped.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn parse_proc_net_tcp(table: &str) -> Vec<(u64, u16)> {
    table
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let local = f.nth(1)?;
            let state = f.nth(1)?;
            let inode = f.nth(5)?;
            if state != "0A" {
                return None;
            }
            let port = u16::from_str_radix(local.rsplit_once(':')?.1, 16).ok()?;
            Some((inode.parse().ok()?, port))
        })
        .collect()
}

#[cfg(target_os = "linux")]
mod sockets {
    /// The listening sockets of this network namespace, by inode.
    pub struct Sockets(std::collections::HashMap<u64, u16>);

    impl Sockets {
        pub fn new() -> Sockets {
            let mut inodes = std::collections::HashMap::new();
            for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
                if let Ok(text) = std::fs::read_to_string(table) {
                    inodes.extend(super::parse_proc_net_tcp(&text));
                }
            }
            Sockets(inodes)
        }

        /// The ports of the listening sockets `pid` holds open.
        pub fn listening(&self, pid: i32) -> Vec<u16> {
            let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
                return Vec::new();
            };
            fds.filter_map(|fd| {
                let link = std::fs::read_link(fd.ok()?.path()).ok()?;
                let inode = link
                    .to_str()?
                    .strip_prefix("socket:[")?
                    .strip_suffix(']')?
                    .parse()
                    .ok()?;
                self.0.get(&inode).copied()
            })
            .collect()
        }
    }
}

#[cfg(target_os = "macos")]
mod sockets {
    use std::ffi::c_void;
    use std::mem;

    use nix::libc;

    /// `PROC_PIDFDSOCKETINFO` (sys/proc_info.h); the libc crate has no
    /// `socket_fdinfo`, so the reply is read at its documented offsets.
    const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
    /// `sizeof(struct socket_fdinfo)`.
    const SOCKET_FDINFO_SIZE: usize = 792;
    /// `proc_fileinfo` (24 bytes) then `socket_info`: its `soi_kind` sits
    /// 232 bytes in, and its protocol union 240.
    const SOI_KIND: usize = 24 + 232;
    const SOCKINFO_TCP: i32 = 2;
    /// `tcp_sockinfo`: `tcpsi_ini.insi_lport` is its second int, and
    /// `tcpsi_state` follows the 80-byte `in_sockinfo`.
    const TCP_LPORT: usize = 24 + 240 + 4;
    const TCP_STATE: usize = 24 + 240 + 80;
    const TSI_S_LISTEN: i32 = 1;

    pub struct Sockets;

    impl Sockets {
        pub fn new() -> Sockets {
            Sockets
        }

        /// The ports of the listening TCP sockets `pid` holds open.
        pub fn listening(&self, pid: i32) -> Vec<u16> {
            // SAFETY: a null buffer asks for the size only.
            let need = unsafe {
                libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0)
            };
            if need <= 0 {
                return Vec::new();
            }
            let mut fds: Vec<libc::proc_fdinfo> =
                Vec::with_capacity(need as usize / mem::size_of::<libc::proc_fdinfo>() + 8);
            let cap = (fds.capacity() * mem::size_of::<libc::proc_fdinfo>()) as libc::c_int;
            // SAFETY: the buffer is valid for `cap` bytes; only the entries
            // the call reports are exposed.
            let got = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDLISTFDS,
                    0,
                    fds.as_mut_ptr().cast::<c_void>(),
                    cap,
                )
            };
            if got <= 0 {
                return Vec::new();
            }
            // SAFETY: `got` bytes were written, whole entries of plain ints.
            unsafe { fds.set_len(got as usize / mem::size_of::<libc::proc_fdinfo>()) };
            fds.iter()
                .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32)
                .filter_map(|fd| listening_port(pid, fd.proc_fd))
                .collect()
        }
    }

    fn listening_port(pid: i32, fd: i32) -> Option<u16> {
        let mut buf = [0u8; SOCKET_FDINFO_SIZE];
        // SAFETY: the buffer is valid for its full length.
        let n = unsafe {
            libc::proc_pidfdinfo(
                pid,
                fd,
                PROC_PIDFDSOCKETINFO,
                buf.as_mut_ptr().cast::<c_void>(),
                buf.len() as libc::c_int,
            )
        };
        if n < (TCP_STATE + 4) as libc::c_int {
            return None;
        }
        let int = |at: usize| i32::from_ne_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
        if int(SOI_KIND) != SOCKINFO_TCP || int(TCP_STATE) != TSI_S_LISTEN {
            return None;
        }
        // The port is in network byte order in the int's low half.
        Some(u16::from_be(int(TCP_LPORT) as u16))
    }
}

use sockets::Sockets;
