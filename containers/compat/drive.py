"""Drive the owner's tmux setup inside the recording container (TC.0).

Started by scenario.sh with the argv recorder first on PATH. Every `tmux`
this script runs by name goes through the recorder, exactly like the
owner's own calls; every control call it makes for itself (reading the key
tables, typing into a pane) uses the real tmux by absolute path, which the
recorder never sees, so the corpus holds only the setup's own invocations.

The server is started through the recorder, then a client is attached on a
pty (popups, the status line's #() jobs and client hooks need one). The
driver then presses every binding whose command reaches a shell (run-shell,
popups, if-shell, key tables that lead to them), types the owner's scripts
into a pane, and runs smug. Popups and prompts are closed with Escape and
C-c after each key.
"""

import argparse
import fcntl
import os
import pty
import re
import select
import shlex
import struct
import subprocess
import sys
import termios
import time

COLS, ROWS = 200, 50
PREFIX = b"\x13"  # C-s, canopy's prefix

# Bindings that would fetch from the network or delete plugins.
SKIP_COMMANDS = re.compile(
    r"install_plugins|update_plugins|clean_plugins|kill-server|suspend-client|detach-client|lock-"
)
REACHES_SHELL = re.compile(
    r"\b(run-shell|run|display-popup|popup|if-shell|if|new-window|split-window|command-prompt|"
    r"display-menu|menu|source-file|show-wk-menu[\w-]*|switch-client -T)\b"
)


def log(msg):
    print(f"drive: {msg}", flush=True)


class Real:
    """The real tmux, by absolute path: invisible to the recorder."""

    def __init__(self, path):
        self.path = path

    def __call__(self, *args, check=False):
        r = subprocess.run([self.path, *args], capture_output=True, text=True, check=False)
        if check and r.returncode:
            raise RuntimeError(f"{args}: {r.stderr}")
        return r.stdout


class Client:
    """`tmux attach` on a pty, through the recorder."""

    def __init__(self, session):
        pid, fd = pty.fork()
        if pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
            os.execvp("tmux", ["tmux", "attach-session", "-t", session])
        self.pid, self.fd = pid, fd
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.drain(2.0)

    def drain(self, secs):
        end = time.time() + secs
        while True:
            left = end - time.time()
            if left <= 0:
                return
            r, _, _ = select.select([self.fd], [], [], left)
            if r:
                try:
                    os.read(self.fd, 65536)
                except OSError:
                    return

    def send(self, data: bytes, wait=1.5):
        try:
            os.write(self.fd, data)
        except OSError:
            pass
        self.drain(wait)

    def alive(self):
        try:
            pid, _ = os.waitpid(self.pid, os.WNOHANG)
            return pid == 0
        except ChildProcessError:
            return False


def key_bytes(key: str) -> bytes | None:
    named = {
        "Space": b" ",
        "Enter": b"\r",
        "Escape": b"\x1b",
        "BSpace": b"\x7f",
        "Tab": b"\t",
        "Up": b"\x1b[A",
        "Down": b"\x1b[B",
        "Right": b"\x1b[C",
        "Left": b"\x1b[D",
    }
    if key in named:
        return named[key]
    if key.startswith("\\") and len(key) == 2:
        key = key[1]
    if len(key) == 1:
        return key.encode()
    m = re.fullmatch(r"C-(.)", key)
    if m:
        c = m.group(1)
        if c == "Space":
            return b"\x00"
        if c.isalpha():
            return bytes([ord(c.lower()) - 96])
        return {"\\": b"\x1c", "]": b"\x1d", "^": b"\x1e", "_": b"\x1f", "@": b"\x00"}.get(c)
    m = re.fullmatch(r"M-(.)", key)
    if m:
        return b"\x1b" + m.group(1).encode()
    return None


def bindings(real, table):
    """(key, command text) for each binding in a table."""
    out = []
    for line in real("list-keys", "-T", table).splitlines():
        try:
            words = shlex.split(line, posix=True)
        except ValueError:
            continue
        # bind-key [-r] [-N note] -T table key command...
        i = 1
        while i < len(words) and words[i].startswith("-"):
            i += 2 if words[i] in ("-T", "-N") else 1
        if i < len(words):
            out.append((words[i], " ".join(words[i + 1 :])))
    return out


def close_overlays(c):
    c.send(b"\x1b", 0.5)
    c.send(b"\x03", 0.5)
    c.send(b"\x1b", 0.5)


def type_in_pane(real, target, command, wait=3.0):
    log(f"type in {target}: {command}")
    real("send-keys", "-t", target, "-l", command)
    real("send-keys", "-t", target, "Enter")
    time.sleep(wait)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--real", required=True)
    args = ap.parse_args()
    # This starts and drives a tmux server: only inside the recording
    # container, never inside (or next to) a live tmux.
    if not (os.path.exists("/run/.containerenv") or os.path.exists("/.dockerenv")):
        sys.exit("drive: refusing to run outside a container")
    if os.environ.get("TMUX"):
        sys.exit("drive: refusing to run inside tmux (TMUX is set)")
    if not os.environ.get("TMUX_TMPDIR"):
        sys.exit("drive: refusing to run without a private TMUX_TMPDIR")
    real = Real(args.real)
    home = os.environ["HOME"]

    # The owner's shell alias `ta` (tmux attach || tmux new-session) and the
    # systemd unit's `new-session -d` start the server; start it the same way.
    log("start the server through the recorder")
    subprocess.run(["tmux", "new-session", "-d", "-s", "main", "-x", str(COLS), "-y", str(ROWS)], check=True)
    time.sleep(10)  # TPM and every plugin load in run-shell jobs

    c = Client("main")
    c.drain(8)  # the status line runs its #() jobs for the attached client

    # Root table: navigator's C-h/j/k/l/C-\ and the rest that are keyboard keys
    for key, cmd in bindings(real, "root"):
        b = key_bytes(key)
        if b is None or key in ("C-s",) or SKIP_COMMANDS.search(cmd):
            continue
        if not REACHES_SHELL.search(cmd):
            continue
        log(f"root {key}: {cmd[:90]}")
        c.send(b, 1.5)

    # Prefix table, then any table a prefix key switches to
    tables = []
    for key, cmd in bindings(real, "prefix"):
        b = key_bytes(key)
        if b is None or SKIP_COMMANDS.search(cmd) or not REACHES_SHELL.search(cmd):
            continue
        if key in ("M-r",):
            continue  # resurrect restore: last, below
        m = re.search(r"switch-client -T (\S+)", cmd)
        if m:
            tables.append((b, m.group(1)))
            continue
        log(f"prefix {key}: {cmd[:90]}")
        c.send(PREFIX + b, 2.5)
        close_overlays(c)
        if not c.alive():
            log("client exited; attach again")
            c = Client("main")
    for lead, table in tables:
        for key, cmd in bindings(real, table):
            b = key_bytes(key)
            if b is None or SKIP_COMMANDS.search(cmd) or key == "Any":
                continue
            log(f"prefix table {table} {key}: {cmd[:80]}")
            c.send(PREFIX + lead + b, 2.5)
            close_overlays(c)
            if not c.alive():
                c = Client("main")

    # The owner's scripts, typed into a pane as the owner runs them
    real("new-window", "-d", "-t", "main:", "-n", "drive")
    pane = "main:drive"
    time.sleep(1)
    canopy = f"{home}/.local/share/canopy/bin"
    for state in ("working", "blocked", "completed", "idle", "exited"):
        type_in_pane(real, pane, f"{canopy}/canopy-agent report {state} --source claude-code --session-id s1", 1.5)
    for sub in ("status", "doctor", "config", "reboot-check", "version"):
        type_in_pane(real, pane, f"{canopy}/canopy {sub} >/dev/null 2>&1", 3)
    type_in_pane(
        real, pane, "theme --list >/dev/null; theme tokyonight >/dev/null 2>&1; theme gruvbox >/dev/null 2>&1", 4
    )
    type_in_pane(real, pane, "cockpit >/dev/null 2>&1", 4)
    type_in_pane(real, pane, "tsess", 3)
    close_overlays(c)
    real("send-keys", "-t", pane, "C-c")
    type_in_pane(real, pane, f"smug start -f {home}/smug.yaml -d; smug stop -f {home}/smug.yaml", 6)
    type_in_pane(real, pane, "~/.config/tmux/pet-pick.sh </dev/null >/dev/null 2>&1 & sleep 2; kill %1 2>/dev/null", 3)
    type_in_pane(real, pane, "tmux refresh-client -S; tmux info >/dev/null", 2)

    # smug and the shell aliases from outside tmux (cli origin)
    env = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE")}
    for argv in (
        ["smug", "start", "-f", f"{home}/smug.yaml", "-d"],
        ["smug", "stop", "-f", f"{home}/smug.yaml"],
        ["tmux", "list-sessions"],
        ["tmux", "has-session", "-t", "=main"],
        ["bash", "-c", "tmux new-session -A -s side -d"],
        [f"{home}/.config/tmux/net.sh"],
        [f"{home}/.config/tmux/battery.sh"],
        [f"{home}/.config/tmux/pet.sh"],
    ):
        log(f"outside tmux: {' '.join(argv)}")
        subprocess.run(
            argv,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=60,
            check=False,
        )

    # resurrect restore last: it rebuilds sessions from the save made above
    log("prefix M-s, then M-r (resurrect save and restore)")
    c.send(PREFIX + b"\x1bs", 6)
    c.send(PREFIX + b"\x1br", 8)
    close_overlays(c)
    c.drain(6)  # one more status interval of #() jobs
    log("done")


if __name__ == "__main__":
    sys.exit(main())
