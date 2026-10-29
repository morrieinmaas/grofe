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
//!
//! What `-I` reads waits while the pane's input queue is full, so a command
//! that writes faster than the pane takes it is held back, as a pipe holds
//! back its writer; nothing it wrote is lost, and neither the pane nor the
//! server waits for it.
// ponytail: tmux buffers what the pane writes for a slow `-O` command
// without limit; the queue here holds 1024 reads, and what comes while it
// is full is dropped, so a command that stops reading never holds the pane.

use std::io as stdio;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::io::InputQueue;
use super::lock;

/// Reads the pane's output may wait for the command.
const QUEUE_READS: usize = 1024;
/// Most bytes one read of the command's output puts into the pane.
const READ_CHUNK: usize = 4096;
/// How long `-I` output waits before it tries a full input queue again.
const FULL_WAIT: Duration = Duration::from_millis(10);
/// How long a closed pane's command may take to read what is queued for it.
// ponytail: tmux keeps such a pane until its pipe drains, however long;
// grofe gives a command that stopped reading this long, then closes.
const DRAIN_LIMIT: Duration = Duration::from_secs(30);

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
    /// The task writing the queue to the command's stdin (`-O`).
    writer: Option<JoinHandle<()>>,
    /// The task reading the command's stdout into the pane (`-I`).
    reader: Option<JoinHandle<()>>,
}

impl Open {
    /// Closes the pipe once the command has what the pane wrote, as tmux
    /// destroys a pane only when its pipe's output has drained: the queue
    /// ends here and the writer runs on to its end, for at most
    /// [`DRAIN_LIMIT`].
    fn drain(mut self) {
        self.out = None;
        if let (Some(writer), Ok(runtime)) =
            (self.writer.take(), tokio::runtime::Handle::try_current())
        {
            let stop = writer.abort_handle();
            runtime.spawn(async move {
                if tokio::time::timeout(DRAIN_LIMIT, writer).await.is_err() {
                    stop.abort();
                }
            });
        }
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // Dropping the tasks drops the command's stdin and stdout: it sees
        // end of file, as it does when tmux closes its end.
        for task in self.writer.iter().chain(&self.reader) {
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

    /// The handle `owner` goes away: its pipe closes once the command has
    /// read what the pane wrote, unless a respawn gave the pipe to another.
    pub(crate) fn close_for(&self, owner: u64) {
        if self.owner.load(Ordering::Relaxed) != owner {
            return;
        }
        let open = lock(&self.open).take();
        if let Some(open) = open {
            open.drain();
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
        let (mut writer, mut reader, mut out) = (None, None, None);
        if let Some(mut stdin) = child.stdin.take() {
            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(QUEUE_READS);
            out = Some(tx);
            let pipe = Arc::clone(self);
            writer = Some(tokio::spawn(async move {
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
            reader = Some(tokio::spawn(async move {
                let mut buf = vec![0u8; READ_CHUNK];
                while let Ok(n) = stdout.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    // The handle (so the queue) can change under a respawn.
                    while Arc::clone(&lock(&pipe.input))
                        .push(buf[..n].to_vec())
                        .is_err()
                    {
                        tokio::time::sleep(FULL_WAIT).await;
                    }
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
            writer,
            reader,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `-I` from a command that writes far more than the pane's input
    /// queue holds: the command waits for room, and every byte arrives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_waits_for_a_full_queue_and_loses_nothing() {
        const BYTES: usize = 12 * 1024 * 1024;
        let (queue, mut rx) = InputQueue::new();
        let pipe = Pipe::new(Arc::clone(&queue), 0);
        pipe.start(&format!("head -c {BYTES} /dev/zero"), true, false)
            .expect("start");
        // Let the command fill the queue before anything drains it.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut got = 0;
        while got < BYTES {
            let chunk = tokio::time::timeout(Duration::from_secs(20), rx.recv())
                .await
                .expect("the rest of the output")
                .expect("queue open");
            // As the pane's writer does once the bytes are written.
            queue.written(chunk.len());
            got += chunk.len();
        }
        assert_eq!(got, BYTES);
        pipe.close();
    }

    /// A pane that goes while its command is still reading: the command
    /// gets everything the pane wrote, then end of file, as tmux holds the
    /// pane until its pipe has drained.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_closed_pane_s_command_reads_all_that_was_queued() {
        const CHUNK: usize = 8 * 1024;
        const CHUNKS: usize = 500;
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("out");
        let (queue, _rx) = InputQueue::new();
        let pipe = Pipe::new(queue, 7);
        // A slow reader: the queue is still full when the pane goes.
        let command = format!("sleep 0.3; cat > '{}'", file.display());
        pipe.start(&command, false, true).expect("start");
        for _ in 0..CHUNKS {
            pipe.tee(&[b'x'; CHUNK]);
        }
        pipe.close_for(7);
        assert!(!pipe.is_open(), "closed for the pane at once");
        let want = (CHUNK * CHUNKS) as u64;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut len = 0;
        while std::time::Instant::now() < deadline {
            len = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            if len == want {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(len, want);
    }
}
