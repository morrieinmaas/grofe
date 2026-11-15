# Building the private corpus

How the argv corpus behind `docs/compat-coverage.md` was built and how it is
regenerated. The sources are a private corpus of real configs, scripts and
plugins, so the inputs are not in the repository; only the recorded result is
(`tests/compat/corpus/`). This page is kept for the maintainer.

## How the corpus is built

A maintainer's live tmux is never touched: no recorder on their `PATH`, no
restart of their server. The corpus comes from two places instead.

**Static extraction** (`scripts/corpus.py extract`) reads the private sources
and never runs anything. It scans shell scripts, tmux config files, a key
table and the string literals of Rust, Lua, Vim script and Python code for
tmux invocations, follows stored command strings (bind bodies, `if-shell`
branches, hooks, menu items, `command-alias` values) and the shell strings
they carry (`run-shell`, popups, `#()` jobs) to the calls inside them, and
keeps a candidate only when its verb is a tmux 3.7b command or alias. Call
sites that build argv in code the scanner cannot follow are transcribed by
hand in `scripts/corpus-manual.txt`, each block naming its file. Sources:

- a set of dotfile repositories: tmux config layers, helper scripts, shell
  aliases and functions, and the scripts that install the plugins;
- the loaded TPM plugins: tpm, sensible, yank, tmux-fzf, vim-tmux-navigator,
  sessionx, which-key, opensessions, fzf-url, claude-session-manager (the
  other clones in the plugin directory are not loaded and are not read);
- Neovim: vim-tmux-navigator's Vim side and snacks.nvim's image terminal
  support, both of which call `tmux` themselves.

A branch that only runs on a tmux older than 3.7b is dropped. A flag that
3.7b lacks but the setup still sends is kept and marked `!` in its form.

**Container recording** (`scripts/record-container-corpus.sh`) builds
`containers/compat/Containerfile`: a Debian base pinned by digest, tmux 3.7b
from source (checked by hash and `tmux -V`), off the `PATH`, with the fzf,
smug and opensessions versions the setup uses (each download checked by
sha256). It copies the private files and the loaded plugins into a staging
directory (a git tree's own files only), refuses to continue if the copy
holds a denylisted string (`scripts/corpus.py screen`, the same list the
check uses, below), and mounts it read-only in a container with no network.
`scenario.sh` and `drive.py` refuse to run outside a container, inside tmux
(`TMUX` set) or without a private `TMUX_TMPDIR`. Inside,
`containers/compat/scenario.sh` lays the setup out under a container user's
home, installs `scripts/tmux-argv-recorder.sh` as the only `tmux` on `PATH`,
starts the server through it, and `containers/compat/drive.py` exercises the
setup through an attached client on a pty: every root, prefix and key-table
binding whose command reaches a shell (popups and prompts closed after
each), the setup's scripts typed into a pane, the status line's `#()` jobs,
and the calls made from outside tmux. At the end it dumps the server's key
tables and hooks, since those stored commands are tmux argv too.

The recorder classifies each call by where it came from: `job` (the server is
an ancestor and there is no terminal: `run-shell`, hooks, `if-shell`, `#()`),
`pane` (a pane or popup process), or `cli` (anything else, including a
daemon a job started that has since detached from the server). A non-empty
`container-job.argv` is the proof that the server's own calls went through the
recorder.

## Regenerating

`extract` and `record-container-corpus.sh` read the checkout locations from
`GROFE_COMPAT_*` environment variables (the scripts name them in their usage
text and error messages).

```sh
uv run scripts/corpus.py extract
scripts/record-container-corpus.sh
uv run scripts/corpus.py coverage     # rewrites the generated section of docs/compat-coverage.md
scripts/check-corpus.sh
```

## Publishability

`scripts/check-corpus.sh` scans every file under `tests/compat/corpus/`,
`scripts/compat-rows.tsv`, `scripts/corpus-manual.txt`, everything under
`containers/compat/` and the coverage page, and fails on any denylisted
string. The list is built on the machine running the check, never stored:

- this machine's home directory (anywhere, not only under `/Users` or
  `/home`) and git email, matched as substrings;
- its hostnames, user name (`$USER`, `id -un`), git user name, and the
  `Host`/`HostName` values of `~/.ssh/config` and the files it includes
  (public forges and loopback excepted), matched as whole words in any case;
  a found name equal to a word the corpus uses on its own (`main`, `rec`,
  `tmux` and the like) is skipped, since it cannot identify a machine;
- the terms in `GROFE_COMPAT_DENYLIST`, a file outside the repository with
  one term per line, matched as whole words; the terms themselves are never
  committed.

It also fails on any email address and any `/Users/<name>` or
`/home/<name>` path (the container user `rec` excepted). Normalization
scrubs the same names out of recordings (`<host>`, `<redacted>`), and the
staging copy for the container is screened with the same list.

A form the corpus gains needs a line in `scripts/compat-rows.tsv`, which says
which spec 4.5 row owns it; the check refuses a row id that names nothing in
the spec.

## Guarding a live server

A local agent hook can block a shell command that runs tmux's kill verbs
(server, session, window, pane) without `-L` or `-S`. The match is on the
command text, so once `tmux` on `PATH` resolves to grofe it keeps matching and
guards a grofe server exactly as it guards tmux. It does not match the native
spelling (`grofe kill-server`).
