//! What a scenario needs from `scripts/ssh-e2e.sh`'s environment.

use std::process::{Command, Output};

pub struct E2e {
    pub config: String,
    pub host: String,
    pub bare_host: String,
    pub release_url: String,
    script: String,
}

impl E2e {
    /// The environment of a `scripts/ssh-e2e.sh` run, or None (skip).
    pub fn get() -> Option<E2e> {
        let config = std::env::var("GROFE_E2E_SSH_CONFIG").ok()?;
        let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
        Some(E2e {
            config,
            host: var("GROFE_E2E_HOST"),
            bare_host: var("GROFE_E2E_BARE_HOST"),
            release_url: var("GROFE_E2E_RELEASE_URL"),
            script: var("GROFE_E2E_SCRIPT"),
        })
    }

    /// `ssh -F config <host> <command>` on the container with grofe.
    pub fn ssh(&self, command: &str) -> Output {
        self.ssh_to(&self.host, command)
    }

    pub fn ssh_to(&self, host: &str, command: &str) -> Output {
        Command::new("ssh")
            .args(["-F", &self.config, host, command])
            .output()
            .expect("run ssh")
    }

    /// The script's helpers: `e2e_exec`, `e2e_netem`, `e2e_kill_sshd_children`.
    pub fn helper(&self, helper: &str, args: &[&str]) -> Output {
        Command::new(&self.script)
            .arg(helper)
            .args(args)
            .output()
            .expect("run the e2e script")
    }
}

pub fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}
