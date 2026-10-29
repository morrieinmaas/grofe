//! Module declarations.

pub mod client;
pub mod cmd;
pub mod compat;
pub mod config;
pub mod event;
pub mod import_tmux;
pub mod layouts;
pub mod paths;
pub mod protocol;
pub mod server;
pub mod term;
pub mod types;

/// The calling thread's CPU time, for tests that bound the work something
/// takes: unlike wall time, a loaded host does not stretch it.
#[cfg(test)]
pub(crate) fn thread_cpu_time() -> std::time::Duration {
    use nix::libc;
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid timespec to write to.
    let ok = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0;
    assert!(ok, "clock_gettime");
    std::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}
