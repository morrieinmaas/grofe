//! The shell jobs still running (`run-shell`, `if-shell`, `copy-pipe`): as
//! tmux 3.7b's `job_kill_all` (job.c), run as its server exits, each one
//! gets SIGTERM, the shell's own process and not its group.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// The pids of the shells started and not reaped yet. Clones share one set.
#[derive(Clone, Default)]
pub struct ShellJobs(Arc<Mutex<BTreeSet<i32>>>);

impl ShellJobs {
    /// Tracks `child` until the guard drops; drop it only once the child
    /// is reaped, so its pid stays its own while it is tracked.
    pub fn track(&self, child: &tokio::process::Child) -> Tracked {
        let pid = child.id().and_then(|id| i32::try_from(id).ok());
        if let (Some(pid), Ok(mut set)) = (pid, self.0.lock()) {
            set.insert(pid);
        }
        Tracked {
            jobs: self.clone(),
            pid,
        }
    }

    /// SIGTERM to every tracked shell.
    // ponytail: a shell reaped on a worker thread at this very moment is
    // still in the set, so its pid gets the signal; that pid being reused
    // in between would take a whole pid-space cycle.
    pub fn kill_all(&self) {
        let Ok(set) = self.0.lock() else {
            return;
        };
        for pid in set.iter() {
            let _ = kill(Pid::from_raw(*pid), Signal::SIGTERM);
        }
    }

    /// How many shells are tracked.
    pub fn len(&self) -> usize {
        self.0.lock().map_or(0, |set| set.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Removes its shell from the set when dropped.
pub struct Tracked {
    jobs: ShellJobs,
    pid: Option<i32>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        if let (Some(pid), Ok(mut set)) = (self.pid, self.jobs.0.lock()) {
            set.remove(&pid);
        }
    }
}

/// Stops the server's shell jobs when dropped: on the way out of `serve`,
/// whether it returns or unwinds from a panic.
pub struct KillOnExit(pub ShellJobs);

impl Drop for KillOnExit {
    fn drop(&mut self) {
        self.0.kill_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn kill_all_ends_a_tracked_shell_and_reaping_untracks_it() {
        let jobs = ShellJobs::default();
        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "while :; do sleep 0.05; done"])
            .spawn()
            .expect("spawn sh");
        let tracked = jobs.track(&child);
        assert_eq!(jobs.len(), 1);
        jobs.kill_all();
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("the shell ended")
            .expect("wait");
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(Signal::SIGTERM as i32));
        drop(tracked);
        assert!(jobs.is_empty());
    }
}
