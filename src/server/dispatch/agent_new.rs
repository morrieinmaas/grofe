//! `agent new <kind>` (spec §7): a window or split running the kind's
//! command, optionally in a fresh git worktree, with its agent record made.
//! The worktree command runs off the actor and finishes in a job; nothing
//! goes through a shell.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command as Process;

use crate::cmd::{AgentNew, Command, NewWindow, Resolve, SplitWindow, Target};
use crate::protocol::ClientCtx;
use crate::server::input::Routed;
use crate::server::{State, clients, prompt};
use crate::types::ClientId;

const WORKTREE_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether a worktree is wanted, from the flags, the kind and the config.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Policy {
    Never,
    Always,
    Ask,
}

/// Branch names: no whitespace, no `..`, no leading `-`, no control
/// characters, at most 100 chars.
pub fn valid_branch(b: &str) -> bool {
    !b.is_empty()
        && b.chars().count() <= 100
        && !b.starts_with('-')
        && !b.contains("..")
        && !b.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// `<repo root>/../worktrees/<branch, / as ->`.
pub fn worktree_path(repo_root: &Path, branch: &str) -> PathBuf {
    let parent = repo_root.parent().unwrap_or(repo_root);
    parent.join("worktrees").join(branch.replace('/', "-"))
}

/// The worktree command's argv with `{dir}`, `{branch}` and `{path}` filled.
pub fn expand(argv: &[String], dir: &str, branch: &str, path: &str) -> Vec<String> {
    argv.iter()
        .map(|a| {
            a.replace("{dir}", dir)
                .replace("{branch}", branch)
                .replace("{path}", path)
        })
        .collect()
}

fn policy(word: &str) -> Policy {
    match word {
        "always" => Policy::Always,
        "ask" => Policy::Ask,
        _ => Policy::Never,
    }
}

/// `agent new`: opens the window or split now, or once the worktree exists.
pub fn run(state: &mut State, ctx: &ClientCtx, c: AgentNew) -> Result<String, String> {
    let kind = state
        .agent_kinds
        .iter()
        .find(|k| k.name == c.kind)
        .ok_or_else(|| format!("unknown agent kind: {}", c.kind))?;
    let argv: Vec<String> = kind
        .command
        .as_deref()
        .unwrap_or_default()
        .split_whitespace()
        .map(String::from)
        .collect();
    if argv.is_empty() {
        return Err(format!("agent kind {} has no command", c.kind));
    }
    let dir = start_dir(state, ctx, &c)?;
    let wanted = match c.worktree {
        Some(true) => Policy::Always,
        Some(false) => Policy::Never,
        None => policy(
            kind.worktree
                .as_deref()
                .unwrap_or(&state.agents_new.worktree),
        ),
    };
    let launch = Launch {
        ctx: ctx.clone(),
        c,
        argv,
        dir,
    };
    match (wanted, launch.c.branch.clone()) {
        (Policy::Never, _) => launch.open(state, None),
        (_, Some(branch)) => launch.worktree(state, branch),
        (_, None) => match ctx.client {
            Some(client) => {
                launch.ask(state, client);
                Ok(String::new())
            }
            None => Err("a worktree needs a branch (-b)".to_string()),
        },
    }
}

/// `-c`, else the current pane's path, else the session's directory.
fn start_dir(state: &State, ctx: &ClientCtx, c: &AgentNew) -> Result<PathBuf, String> {
    if let Some(dir) = &c.cwd {
        return Ok(ctx.cwd.join(dir));
    }
    let model = &state.model;
    let pane = model.resolve_pane(&Target::Current, ctx)?;
    Ok(model
        .panes
        .get(&pane)
        .and_then(|p| p.handle.current_path())
        .unwrap_or_else(|| ctx.cwd.clone()))
}

struct Launch {
    ctx: ClientCtx,
    c: AgentNew,
    argv: Vec<String>,
    dir: PathBuf,
}

impl Launch {
    /// Prompts for the branch; empty means no worktree.
    fn ask(self, state: &mut State, client: ClientId) {
        prompt::ask(
            state,
            client,
            "Worktree branch (empty for none): ",
            "",
            "command",
            false,
            Box::new(move |state, client, text| {
                let text = text.trim().to_string();
                let result = if text.is_empty() {
                    self.open(state, None)
                } else {
                    self.worktree(state, text)
                };
                if let Err(e) = result {
                    clients::display_message(state, client, &e, None, false);
                }
                Routed::Consumed
            }),
        );
    }

    /// Makes the worktree off the actor, then opens in it from a job.
    fn worktree(self, state: &mut State, branch: String) -> Result<String, String> {
        if !valid_branch(&branch) {
            return Err(format!("invalid branch name: {branch}"));
        }
        let template = state.agents_new.worktree_command.clone();
        // From a client the failure is shown as a message; a CLI waits for it.
        let reply = self.ctx.client.is_none().then(|| super::defer_reply(state));
        let jobs = state.jobs.clone();
        let dir = self.dir.clone();
        tokio::spawn(async move {
            let made = make_worktree(&template, &dir, &branch).await;
            let job: crate::server::Job = Box::new(move |state: &mut State| {
                let result = match made {
                    Ok(path) => self.open(state, Some(path)),
                    Err(e) => {
                        if let Some(id) = self.ctx.client {
                            clients::display_message(state, id, &e, None, false);
                        }
                        Err(e)
                    }
                };
                if let Some(reply) = reply {
                    let _ = reply.send(result.map_err(Into::into));
                }
            });
            let _ = jobs.send(job).await;
        });
        Ok(String::new())
    }

    /// The window (or split) in the worktree, else in the start directory,
    /// and the agent record, without a binding: the scan binds it.
    fn open(&self, state: &mut State, worktree: Option<PathBuf>) -> Result<String, String> {
        let dir = worktree.unwrap_or_else(|| self.dir.clone());
        // `-c` is a format; keep a `#` in the directory literal.
        let cwd = Some(dir.display().to_string().replace('#', "##"));
        let command = Some(self.argv.clone());
        let before: Vec<_> = state.model.panes.keys().copied().collect();
        let cmd = if self.c.split {
            Command::SplitWindow(SplitWindow {
                cwd,
                command,
                detached: self.c.detached,
                ..SplitWindow::default()
            })
        } else {
            Command::NewWindow(NewWindow {
                detached: self.c.detached,
                target: Target::Current,
                name: self.c.name.clone(),
                cwd,
                command,
                ..NewWindow::default()
            })
        };
        super::route(state, &self.ctx, cmd)?;
        if let Some(pane) = state
            .model
            .panes
            .keys()
            .copied()
            .find(|p| !before.contains(p))
        {
            state
                .agents
                .ensure(pane, &self.c.kind, None, std::time::Instant::now());
        }
        Ok(String::new())
    }
}

/// `git rev-parse --show-toplevel` in `dir`, then the worktree command.
async fn make_worktree(template: &[String], dir: &Path, branch: &str) -> Result<PathBuf, String> {
    let top = capture(
        Process::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "--show-toplevel"]),
    )
    .await?;
    let root = PathBuf::from(top.lines().next().unwrap_or("").trim());
    let path = worktree_path(&root, branch);
    let words = expand(
        template,
        &dir.display().to_string(),
        branch,
        &path.display().to_string(),
    );
    let (program, args) = words.split_first().ok_or("worktree-command is empty")?;
    let out = capture(Process::new(program).args(args).current_dir(dir)).await?;
    let last = out.lines().rev().find(|l| !l.trim().is_empty());
    Ok(match last.map(|l| PathBuf::from(l.trim())) {
        Some(p) if p.is_dir() => p,
        _ => path,
    })
}

/// Runs `cmd` with a timeout; its stdout, or its stderr as the error.
async fn capture(cmd: &mut Process) -> Result<String, String> {
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("{e}"))?;
    let out = tokio::time::timeout(WORKTREE_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "worktree command timed out after 30s".to_string())?
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if err.is_empty() {
        format!("worktree command failed ({})", out.status)
    } else {
        err
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ClientCtx;
    use crate::server::agents::kinds::Kind;
    use crate::server::input::{Input, InputHook};
    use crate::server::{Job, test_ctx, test_state_at};
    use tokio::sync::mpsc;

    fn setup() -> (State, mpsc::Receiver<Job>, ClientCtx) {
        let (mut state, rx) = test_state_at(PathBuf::from("/nonexistent/agent-new.sock"));
        let ctx = test_ctx();
        let argv = ["new-session", "-d", "-s", "a"].map(String::from);
        crate::server::dispatch::run(&mut state, &ctx, &argv).expect("session");
        state.agent_kinds.push(Kind {
            name: "demo".into(),
            matches: vec!["demo".into()],
            command: Some("sleep 60".into()),
            resume: None,
            worktree: None,
        });
        (state, rx, ctx)
    }

    fn new(kind: &str) -> AgentNew {
        AgentNew {
            kind: kind.into(),
            split: false,
            cwd: Some("/tmp".into()),
            name: None,
            worktree: None,
            branch: None,
            detached: true,
        }
    }

    fn newest(state: &State, before: &[crate::types::PaneId]) -> crate::types::PaneId {
        state
            .model
            .panes
            .keys()
            .copied()
            .find(|p| !before.contains(p))
            .expect("a new pane")
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git")
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().canonicalize().expect("canon").join("proj");
        std::fs::create_dir(&root).expect("mkdir");
        git(&root, &["init", "-q"]);
        git(
            &root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "--no-verify",
                "-m",
                "i",
            ],
        );
        (tmp, root)
    }

    #[test]
    fn helpers() {
        assert_eq!(
            worktree_path(Path::new("/r/grofe"), "feat/x"),
            PathBuf::from("/r/worktrees/feat-x")
        );
        assert!(!valid_branch("a b"));
        assert!(!valid_branch("-x"));
        assert!(!valid_branch("a..b"));
        assert!(!valid_branch(""));
        assert!(valid_branch("feat/ok"));
        let argv = ["x", "{dir}", "{branch}", "{path}"].map(String::from);
        assert_eq!(expand(&argv, "d", "b", "p"), ["x", "d", "b", "p"]);
    }

    #[tokio::test]
    async fn opens_window_running_kind_command_and_creates_record() {
        let (mut state, _rx, ctx) = setup();
        let before: Vec<_> = state.model.panes.keys().copied().collect();
        let mut c = new("demo");
        c.name = Some("work".into());
        assert_eq!(run(&mut state, &ctx, c), Ok(String::new()));
        let pane = newest(&state, &before);
        assert_eq!(
            state.model.panes[&pane].argv.as_deref(),
            Some(&["sleep".to_string(), "60".to_string()][..])
        );
        assert!(state.agents.get(pane).is_some());
        let win = state.model.panes[&pane].window;
        assert_eq!(state.model.windows[&win].name, "work");
    }

    #[tokio::test]
    async fn split_with_p() {
        let (mut state, _rx, mut ctx) = setup();
        let first = *state.model.panes.keys().next().expect("pane");
        ctx.pane = Some(first);
        let mut c = new("demo");
        c.split = true;
        assert_eq!(run(&mut state, &ctx, c), Ok(String::new()));
        let second = newest(&state, &[first]);
        assert_eq!(
            state.model.panes[&first].window,
            state.model.panes[&second].window
        );
        assert!(state.agents.get(second).is_some());
    }

    #[tokio::test]
    async fn unknown_kind_err() {
        let (mut state, _rx, ctx) = setup();
        assert_eq!(
            run(&mut state, &ctx, new("nope")),
            Err("unknown agent kind: nope".to_string())
        );
    }

    #[tokio::test]
    async fn worktree_command_failure_shows_stderr_opens_nothing() {
        let (mut state, mut rx, ctx) = setup();
        let (_tmp, root) = repo();
        state.agents_new.worktree_command = ["sh", "-c", "echo boom >&2; exit 3"]
            .map(String::from)
            .to_vec();
        let before = state.model.panes.len();
        let mut c = new("demo");
        c.cwd = Some(root.display().to_string());
        c.worktree = Some(true);
        c.branch = Some("feat/x".into());
        let _ = run(&mut state, &ctx, c);
        let job = rx.recv().await.expect("job");
        job(&mut state);
        assert_eq!(state.model.panes.len(), before);
        let reply = state.deferred_reply.take().expect("reply");
        let failure = reply.await.expect("sent").expect_err("failed");
        assert_eq!(failure.err, "boom");
    }

    #[tokio::test]
    async fn last_stdout_line_dir_wins() {
        let (mut state, mut rx, ctx) = setup();
        let (tmp, root) = repo();
        let chosen = tmp.path().canonicalize().expect("canon").join("chosen");
        std::fs::create_dir(&chosen).expect("mkdir");
        state.agents_new.worktree_command = vec![
            "sh".into(),
            "-c".into(),
            format!("echo noise; echo {}", chosen.display()),
        ];
        let before: Vec<_> = state.model.panes.keys().copied().collect();
        let mut c = new("demo");
        c.cwd = Some(root.display().to_string());
        c.worktree = Some(true);
        c.branch = Some("feat/x".into());
        let _ = run(&mut state, &ctx, c);
        let job = rx.recv().await.expect("job");
        job(&mut state);
        let pane = newest(&state, &before);
        assert_eq!(state.model.panes[&pane].cwd, chosen);
    }

    #[tokio::test]
    async fn default_command_creates_worktree_at_path() {
        let (mut state, mut rx, ctx) = setup();
        let (tmp, root) = repo();
        let before: Vec<_> = state.model.panes.keys().copied().collect();
        let mut c = new("demo");
        c.cwd = Some(root.display().to_string());
        c.worktree = Some(true);
        c.branch = Some("feat/x".into());
        let _ = run(&mut state, &ctx, c);
        let job = rx.recv().await.expect("job");
        job(&mut state);
        let want = tmp
            .path()
            .canonicalize()
            .expect("c")
            .join("worktrees/feat-x");
        assert!(want.is_dir());
        let pane = newest(&state, &before);
        assert_eq!(state.model.panes[&pane].cwd, want);
    }

    #[tokio::test]
    async fn ask_prompts_empty_means_none() {
        let (mut state, _rx, mut ctx) = setup();
        let accepted = clients::attach(
            &mut state,
            clients::AttachRequest {
                ctx: ClientCtx {
                    client: None,
                    ..test_ctx()
                },
                size: (100, 40),
                term: "xterm".into(),
                caps: Default::default(),
                pid: None,
                target: Some("a".into()),
                detach_others: false,
                nest: false,
                tty: None,
                created: false,
                extras: Default::default(),
            },
        )
        .expect("attach");
        let id = accepted.id;
        ctx.client = Some(id);
        state.agents_new.worktree = "ask".into();
        let before = state.model.panes.len();
        assert_eq!(run(&mut state, &ctx, new("demo")), Ok(String::new()));
        assert!(state.prompts.is_open(id), "prompt is open");
        assert_eq!(state.model.panes.len(), before);
        let enter = crate::server::input::parse_key("Enter").expect("key");
        let routed = crate::server::prompt::PromptHook.on_input(
            &mut state,
            id,
            &Input::Key(enter),
            std::time::Instant::now(),
        );
        assert!(routed.is_some());
        assert_eq!(state.model.panes.len(), before + 1);
    }

    /// The kind's own `worktree` wins over `[agents.new] worktree`.
    #[tokio::test]
    async fn kind_worktree_beats_config() {
        let (mut state, _rx, mut ctx) = setup();
        ctx.client = None;
        state.agents_new.worktree = "never".into();
        let kind = state
            .agent_kinds
            .iter_mut()
            .find(|k| k.name == "demo")
            .expect("kind");
        kind.worktree = Some("always".into());
        // `always` without a branch and without a client is the error;
        // `never` would have opened a window.
        assert_eq!(
            run(&mut state, &ctx, new("demo")),
            Err("a worktree needs a branch (-b)".to_string())
        );
    }
}
