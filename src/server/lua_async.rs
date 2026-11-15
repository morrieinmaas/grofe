//! Timers, event subscriptions and child processes for Lua callbacks. Each
//! runs off the actor and posts a `Job` that calls `lua_host::call`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};

use crate::lua::api::Payload;
use crate::server::events::QUEUE;
use crate::server::{Job, events::Events, lua_host};

#[derive(Default)]
pub struct Async {
    tasks: HashMap<u32, AbortHandle>,
}

fn post(id: u32, payload: Payload) -> Job {
    Box::new(move |state| lua_host::call(state, id, payload))
}

fn read_all<R: AsyncRead + Unpin + Send + 'static>(pipe: Option<R>) -> JoinHandle<String> {
    tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf).await;
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

async fn run_system(argv: Vec<String>, cwd: Option<PathBuf>, timeout: Duration) -> Payload {
    let failed = |msg: String| Payload::System {
        code: -1,
        stdout: String::new(),
        stderr: msg,
    };
    let Some((prog, args)) = argv.split_first() else {
        return failed("empty argv".into());
    };
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return failed(format!("{prog}: {e}")),
    };
    let out_task = read_all(child.stdout.take());
    let err_task = read_all(child.stderr.take());
    let code = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status.code().unwrap_or(-1),
        Ok(Err(_)) => -1,
        Err(_) => {
            let _ = child.kill().await;
            -1
        }
    };
    let stdout = out_task.await.unwrap_or_default();
    let stderr = err_task.await.unwrap_or_default();
    Payload::System {
        code,
        stdout,
        stderr,
    }
}

impl Async {
    fn track(&mut self, id: u32, handle: AbortHandle) {
        if let Some(old) = self.tasks.insert(id, handle) {
            old.abort();
        }
    }

    pub fn every(&mut self, jobs: &mpsc::Sender<Job>, ms: u64, id: u32) {
        let jobs = jobs.clone();
        let period = Duration::from_millis(ms);
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if jobs.send(post(id, Payload::None)).await.is_err() {
                    break;
                }
            }
        });
        self.track(id, task.abort_handle());
    }

    pub fn subscribe(
        &mut self,
        events: &mut Events,
        jobs: &mpsc::Sender<Job>,
        name: &str,
        id: u32,
    ) -> Result<(), String> {
        let (tx, mut rx) = mpsc::channel(QUEUE);
        events.subscribe(vec![name.to_string()], None, tx)?;
        let jobs = jobs.clone();
        let task = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let value = serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
                if jobs.send(post(id, Payload::Event(value))).await.is_err() {
                    break;
                }
            }
        });
        self.track(id, task.abort_handle());
        Ok(())
    }

    pub fn system(
        &mut self,
        jobs: &mpsc::Sender<Job>,
        argv: Vec<String>,
        cwd: Option<PathBuf>,
        timeout: Duration,
        id: u32,
    ) {
        let jobs = jobs.clone();
        let task = tokio::spawn(async move {
            let payload = run_system(argv, cwd, timeout).await;
            let _ = jobs.send(post(id, payload)).await;
        });
        self.track(id, task.abort_handle());
    }

    pub fn cancel(&mut self, id: u32) {
        if let Some(handle) = self.tasks.remove(&id) {
            handle.abort();
        }
    }

    pub fn cancel_all(&mut self) {
        for (_, handle) in self.tasks.drain() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cancel_stops_timer() {
        let (tx, mut rx) = mpsc::channel::<Job>(16);
        let mut a = Async::default();
        a.every(&tx, 20, 1);
        assert!(rx.recv().await.is_some());
        a.cancel(1);
        while rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_every_posts_repeatedly_and_cancel_all_stops() {
        let (tx, mut rx) = mpsc::channel::<Job>(16);
        let mut a = Async::default();
        a.every(&tx, 20, 1);
        a.every(&tx, 20, 2);
        for _ in 0..3 {
            assert!(rx.recv().await.is_some());
        }
        a.cancel_all();
        while rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_system_timeout_kills() {
        let dir = std::env::temp_dir().join(format!("grofe-t4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        let script = format!("echo $$ > {}; exec sleep 5", pidfile.display());
        let t = std::time::Instant::now();
        let p = run_system(
            vec!["sh".into(), "-c".into(), script],
            None,
            Duration::from_millis(200),
        )
        .await;
        assert!(t.elapsed() < Duration::from_secs(3));
        assert!(matches!(p, Payload::System { code: -1, .. }));
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "child {pid} still running");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_system_captures_output_and_posts_a_job() {
        let p = run_system(
            vec![
                "sh".into(),
                "-c".into(),
                "echo hi; echo e >&2; exit 3".into(),
            ],
            None,
            Duration::from_secs(5),
        )
        .await;
        match p {
            Payload::System {
                code,
                stdout,
                stderr,
            } => assert_eq!((code, stdout.as_str(), stderr.as_str()), (3, "hi\n", "e\n")),
            _ => panic!("not a system payload"),
        }
        let (tx, mut rx) = mpsc::channel::<Job>(1);
        Async::default().system(&tx, vec!["true".into()], None, Duration::from_secs(5), 7);
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn test_unknown_event_errors() {
        let (tx, _rx) = mpsc::channel::<Job>(1);
        let mut events = Events::default();
        let e = Async::default()
            .subscribe(&mut events, &tx, "no-such-event", 1)
            .unwrap_err();
        assert!(e.contains("no-such-event"), "{e}");
        Async::default()
            .subscribe(&mut events, &tx, "pane-exited", 2)
            .unwrap();
    }
}
