//! `grofe layout`: the binary's side of `grofe::layouts`, which sends its
//! work to the server as ordinary commands.

use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use grofe::event::Event;
use grofe::layouts::{self, Dirs};
use grofe::protocol::{Reply, Request};

use super::{
    Globals, attach_to, connect, connect_or_start, control_hello, fail, hello_for, no_server,
    request, request_with,
};

pub fn run(globals: &Globals, args: &[String]) -> ExitCode {
    let (socket, own_dir) = globals.resolve();
    let mut server = Host { socket, own_dir };
    let dirs = Dirs::from_process();
    let mut out = io::stdout().lock();
    match layouts::run(args, &mut server, &dirs, &mut out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

struct Host {
    socket: PathBuf,
    own_dir: bool,
}

fn reply(r: Result<Reply, String>) -> Result<String, String> {
    match r? {
        Reply::Ok { out, .. } => Ok(out),
        Reply::Err { err, .. } => Err(err.trim_end().to_string()),
        Reply::Event(_) => Err("unexpected event reply".to_string()),
    }
}

impl layouts::Server for Host {
    fn run(&mut self, argv: &[String]) -> Result<String, String> {
        let stream = connect(&self.socket)?;
        reply(request(stream, &self.socket, argv))
    }

    fn create_session(&mut self, argv: &[String]) -> Result<String, String> {
        // No pane in the request, as for any new session.
        let hello = hello_for(&self.socket, false)?;
        let stream = connect_or_start(&self.socket, self.own_dir, true)?;
        reply(request_with(stream, &hello, argv))
    }

    fn sessions(&mut self) -> Result<Vec<(String, String)>, String> {
        let argv = ["list-sessions", "-F", "#{session_id}\t#{session_name}"].map(String::from);
        let out = match self.run(&argv) {
            Ok(out) => out,
            Err(e) if no_server(&e) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(out
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(i, n)| (i.to_string(), n.to_string()))
            .collect())
    }

    fn host_shell(&mut self, command: &str, cwd: &Path, session: &str) -> Result<(), String> {
        // The shell run-shell uses: the server's default-shell, else $SHELL.
        let ask = ["show-options", "-gv", "default-shell"].map(String::from);
        let set = self
            .run(&ask)
            .ok()
            .map(|s| s.trim_end().to_string())
            .filter(|s| !s.is_empty());
        let shell = grofe::server::pane::pick_shell(set.or_else(|| std::env::var("SHELL").ok()));
        let status = Command::new(&shell)
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .env("SHELL", &shell)
            .env("GROFE_LAYOUT_SESSION", session)
            .status()
            .map_err(|e| format!("{shell}: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{command:?} failed with {status}"))
        }
    }

    fn attach(&mut self, session_id: &str) -> Result<(), String> {
        // `$GROFE` names the server of the pane (or `run-shell`) this runs in.
        let inside = std::env::var_os("GROFE")
            .is_some_and(|g| grofe::paths::same_socket(Path::new(&g), &self.socket));
        if inside
            && self
                .run(&["switch-client", "-t", session_id].map(String::from))
                .is_ok()
        {
            return Ok(());
        }
        if !io::stdin().is_terminal() {
            println!("not attaching: no terminal");
            return Ok(());
        }
        control_hello(&self.socket)?;
        let argv = ["attach-session", "-t", session_id].map(String::from);
        let reason = attach_to(&self.socket, &argv)?;
        println!("[{reason}]");
        Ok(())
    }

    fn wait_closed(&mut self, id: &str) {
        // A subscription, not polling: the event comes as the session closes,
        // and the connection ends with the server, so this never outlives it.
        let Ok(hello) = hello_for(&self.socket, false) else {
            return;
        };
        let Ok(stream) = connect(&self.socket) else {
            return;
        };
        let req = Request {
            id: 1,
            argv: ["subscribe", "session-closed"].map(String::from).to_vec(),
            stdin: None,
            umask: None,
        };
        let Ok(req) = serde_json::to_string(&req) else {
            return;
        };
        let mut writer = &stream;
        if writeln!(writer, "{hello}\n{req}").is_err() {
            return;
        }
        let mut lines = BufReader::new(&stream).lines();
        while let Some(Ok(line)) = lines.next() {
            match serde_json::from_str::<Reply>(&line) {
                // Subscribed: a session that ended before this is gone now.
                Ok(Reply::Ok { .. }) => {
                    let ask = ["has-session", "-t", id].map(String::from);
                    if self.run(&ask).is_err() {
                        return;
                    }
                }
                Ok(Reply::Event(Event::SessionClosed { session, .. }))
                    if session.to_string() == id =>
                {
                    return;
                }
                Ok(Reply::Err { .. }) => return,
                _ => {}
            }
        }
    }

    fn spawn_watch(
        &mut self,
        session_id: &str,
        name: &str,
        root: &Path,
        command: &str,
    ) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        Command::new(exe)
            .arg("-S")
            .arg(&self.socket)
            .args(["layout", "__watch", session_id, name])
            .arg(root)
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("starting the on_stop watcher: {e}"))
    }

    fn self_command(&self) -> Vec<String> {
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "grofe".to_string());
        vec![exe, "-S".to_string(), self.socket.display().to_string()]
    }
}
