//! `pipe-pane`: a shell command tied to a pane, as tmux 3.7b's
//! `cmd-pipe-pane.c` ties one.
//!
//! The command runs under `/bin/sh -c` in a process group of its own, with
//! the server's environment and no stderr. With `-O` (the default) it reads
//! what the pane's program writes, the raw bytes the reader took from the
//! pty; with `-I` what it writes goes into the pane as if typed. A pipe
//! ends when its command's end of the socket pair does, as tmux sees it:
//! for `-I`, when the command closes its stdout; for `-O` alone, at the next
//! write the command cannot take. Until then `#{pane_pipe}` stays 1.
// ponytail: tmux buffers a slow command's input without limit; the queue
// here holds 1024 reads, and what comes while it is full is dropped.

use std::io as stdio;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::io::InputQueue;
use super::lock;

/// Reads the pane's output may wait for the command.
const QUEUE_READS: usize = 1024;
/// Most bytes one read of the command's output puts into the pane.
const READ_CHUNK: usize = 4096;

/// Numbers each pipe, so a task that sees its command end closes only its
/// own pipe, not a newer one.
static PIPES: AtomicU64 = AtomicU64::new(0);

/// What a pane keeps of its pipe, shared by the pane's handles across a
/// `respawn-pane`, which keeps the pipe open in tmux.
pub(crate) struct Pipe {
    open: Mutex<Option<Open>>,
    /// Where `-I` output goes: the current handle's input queue.
    input: Mutex<Arc<InputQueue>>,
    /// The generation of the handle that owns the pipe; only it closes the
    /// pipe when it goes.
    owner: AtomicU64,
}

struct Open {
    id: u64,
    pid: u32,
    /// The command's stdin, for `-O`.
    out: Option<mpsc::Sender<Vec<u8>>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Open {
    fn drop(&mut self) {
        // Dropping the tasks drops the command's stdin and stdout: it sees
        // end of file, as it does when tmux closes its end.
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Pipe {
    pub(crate) fn new(input: Arc<InputQueue>, owner: u64) -> Arc<Pipe> {
        Arc::new(Pipe {
            open: Mutex::new(None),
            input: Mutex::new(input),
            owner: AtomicU64::new(owner),
        })
    }

    /// A respawned pane's handle takes the pipe over.
    pub(crate) fn adopt(&self, input: Arc<InputQueue>, owner: u64) {
        *lock(&self.input) = input;
        self.owner.store(owner, Ordering::Relaxed);
    }

    /// The handle `owner` goes away: its pipe closes, unless a respawn
    /// gave the pipe to another.
    pub(crate) fn close_for(&self, owner: u64) {
        if self.owner.load(Ordering::Relaxed) == owner {
            self.close();
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        lock(&self.open).is_some()
    }

    pub(crate) fn pid(&self) -> Option<u32> {
        lock(&self.open).as_ref().map(|o| o.pid)
    }

    pub(crate) fn close(&self) {
        *lock(&self.open) = None;
    }

    fn close_id(&self, id: u64) {
        let mut open = lock(&self.open);
        if open.as_ref().is_some_and(|o| o.id == id) {
            *open = None;
        }
    }

    /// What the pane's program wrote, for the command's stdin.
    pub(crate) fn tee(&self, bytes: &[u8]) {
        if let Some(out) = lock(&self.open).as_ref().and_then(|o| o.out.as_ref()) {
            let _ = out.try_send(bytes.to_vec());
        }
    }

    /// Opens a pipe running `command`, which `-I` writes to the pane and
    /// `-O` reads from it. Must run inside a tokio runtime.
    pub(crate) fn start(
        self: &Arc<Self>,
        command: &str,
        input: bool,
        output: bool,
    ) -> stdio::Result<()> {
        let child = {
            let _lock = super::spawn_lock();
            tokio::process::Command::new("/bin/sh")
                .arg0("sh")
                .arg("-c")
                .arg(command)
                .stdin(if output {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(if input { Stdio::piped() } else { Stdio::null() })
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()?
        };
        let mut child = child;
        let id = PIPES.fetch_add(1, Ordering::Relaxed);
        let pid = child.id().unwrap_or(0);
        let mut tasks = Vec::new();
        let mut out = None;
        if let Some(mut stdin) = child.stdin.take() {
            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(QUEUE_READS);
            out = Some(tx);
            let pipe = Arc::clone(self);
            tasks.push(tokio::spawn(async move {
                while let Some(chunk) = rx.recv().await {
                    if stdin.write_all(&chunk).await.is_err() {
                        break;
                    }
                }
                pipe.close_id(id);
            }));
        }
        if let Some(mut stdout) = child.stdout.take() {
            let pipe = Arc::clone(self);
            tasks.push(tokio::spawn(async move {
                let mut buf = vec![0u8; READ_CHUNK];
                while let Ok(n) = stdout.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let queue = Arc::clone(&lock(&pipe.input));
                    let _ = queue.push(buf[..n].to_vec());
                }
                pipe.close_id(id);
            }));
        }
        // The runtime reaps the command when it ends.
        drop(child);
        *lock(&self.open) = Some(Open {
            id,
            pid,
            out,
            tasks,
        });
        Ok(())
    }
}
