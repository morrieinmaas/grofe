# tmux compatibility coverage

The argv corpus (plan M3.6, task TC.0) is every tmux invocation the owner's
setup makes, one line each. It bounds the tmux front end (spec D1, section
4.5): a form that is in the corpus needs a row in the front end's table, and a
form that is not needs a dogfooding reason first. This page maps each distinct
form to the spec 4.5 row that owns it, or to the out-of-scope row that
excludes it. `scripts/check-corpus.sh` fails when a form maps to no row.

## How the corpus is built

The owner's live tmux is never touched: no recorder on the owner's `PATH`, no
restart of the owner's server, and canopy's live use of tmux is left alone
(plan addendum, owner constraint of 2026-09-28). So the corpus comes from two
places instead.

**Static extraction** (`scripts/corpus.py extract`) reads the owner's sources
and never runs anything. It scans shell scripts, tmux config files, canopy's
key table and the string literals of Rust, Lua, Vim script and Python code for
tmux invocations, follows stored command strings (bind bodies, `if-shell`
branches, hooks, menu items, `command-alias` values) and the shell strings
they carry (`run-shell`, popups, `#()` jobs) to the calls inside them, and
keeps a candidate only when its verb is a tmux 3.7b command or alias. Call
sites that build argv in code the scanner cannot follow are transcribed by
hand in `scripts/corpus-manual.txt`, each block naming its file. Sources:

- canopy: `tmux/` (config layers and `keys.tsv`), `bin/`, `lib/`,
  `adapters/`, and the vendored resurrect and continuum it loads;
- athome: `dot_config/tmux/`, `dot_config/canopy/user.conf.tmpl`,
  `dot_config/themes/`, `dot_local/bin/{tsess,cockpit,theme}`, the nvim
  navigation config, the zsh aliases and functions, gh-dash's config, the
  systemd unit, and the chezmoi scripts that install the plugins and canopy;
- the loaded TPM plugins: tpm, sensible, yank, tmux-fzf, vim-tmux-navigator,
  sessionx, which-key, opensessions, fzf-url, claude-session-manager (the
  other clones in the plugin directory are not loaded and are not read);
- Neovim: vim-tmux-navigator's Vim side and snacks.nvim's image terminal
  support, both of which call `tmux` themselves.

A branch that only runs on a tmux older than 3.7b is dropped (tmux-yank's
`bind -t vi-copy` for tmux before 2.4). A flag that 3.7b lacks but the
owner's setup still sends is kept and marked `!` in its form.

**Container recording** (`scripts/record-container-corpus.sh`) builds
`containers/compat/Containerfile`: a Debian base pinned by digest, tmux 3.7b
from source (checked by hash and `tmux -V`), off the `PATH`, with the owner's
fzf, smug and opensessions versions (each download checked by sha256). It
copies canopy's runtime files, the athome files and the loaded plugins into a
staging directory (a git tree's own files only), refuses to continue if the
copy holds a denylisted string (`scripts/corpus.py screen`, the same list the
check uses, below), and mounts it read-only in a container with no network.
`scenario.sh` and `drive.py` refuse to run outside a container, inside tmux
(`TMUX` set) or without a private `TMUX_TMPDIR`. Inside, `containers/compat/scenario.sh` lays the setup out under a
container user's home as chezmoi and canopy do, installs
`scripts/tmux-argv-recorder.sh` as the only `tmux` on `PATH`, starts the
server through it, and `containers/compat/drive.py` exercises the setup
through an attached client on a pty: every root, prefix and key-table binding
whose command reaches a shell (popups and prompts closed after each), the
owner's scripts typed into a pane (canopy-agent's five states, canopy's
read-only commands, `theme`, `cockpit`, `tsess`, smug), the status line's
`#()` jobs, and the calls the owner makes from outside tmux. At the end it
dumps the server's key tables and hooks, since those stored commands are tmux
argv too.

The recorder classifies each call by where it came from: `job` (the server is
an ancestor and there is no terminal: `run-shell`, hooks, `if-shell`, `#()`),
`pane` (a pane or popup process), or `cli` (anything else, including a
daemon a job started that has since detached from the server, such as
opensessions' server). A non-empty
`container-job.argv` is the proof that the server's own calls went through the
recorder.

## Files

| File | What it holds |
|---|---|
| `tests/compat/corpus/static-cli.argv` | invocations of the `tmux` binary found statically, with the global flags |
| `tests/compat/corpus/static-conf.argv` | commands from tmux config files and canopy's key table (no global flags) |
| `tests/compat/corpus/container-cli.argv` | recorded calls from outside tmux |
| `tests/compat/corpus/container-pane.argv` | recorded calls from processes in panes and popups |
| `tests/compat/corpus/container-job.argv` | recorded calls from `run-shell`, hooks, `if-shell` and `#()` jobs |
| `tests/compat/corpus/container-stored.argv` | the recorded server's key bindings (`list-keys`) and hooks (`show-hooks -g`, as `set-hook`) |
| `tests/compat/corpus/sources.tsv` | for every line, where it came from (`<repo>:<path>`, `manual:<path>` or `container:<origin>`) |
| `tests/compat/corpus/tmux-list-commands.txt` | tmux 3.7b's `list-commands`, recorded in the container: the flag table forms are parsed with |

A line is one invocation: the argv after `tmux`, shell-quoted (`shlex`
rules), with a lone `\;` as tmux's command separator. An embedded newline or
tab is written as `\n` or `\t`. Files are sorted and deduplicated.

## Normalization

`scripts/normalize-corpus.sh` (recordings) and the extractor share one set of
rules (`normalize_argv` in `scripts/corpus.py`), so both halves dedupe
against each other:

- the recording's home directory, and any `/Users/<name>` or `/home/<name>`,
  become `~`; this machine's hostnames become `<host>`, and its user and
  ssh host names and private denylist terms `<redacted>`; email addresses
  become `<email>`; mktemp suffixes under `/tmp` become `XXXXXX` and digit
  runs of seven or more (timestamps) `<n>`; pane and window ids inside
  nested command and shell strings become `%N` and `@N`;
- `-L` and `-S` values become `<socket>` and `<socket-path>` unless they are a
  format (`-S #{socket_path}`), and `-f` values `<file>` unless `/dev/null`;
- targets keep their shape and lose their names: `=<session>`,
  `<session>:<window>.<pane>`, `%N`, `@N`, `$N`, a bare `=`, indices;
- session, window and buffer names, start directories and client names
  become `<name>`, `<dir>` and `<client>`; `-e VAR=value` keeps `VAR`;
- the value of a user option (`@name`) and of `set-environment` becomes
  `<value>`; `source-file` and buffer paths become `<file>`;
- a shell variable or command substitution found statically becomes
  `<$name>` or `<$(...)>`, since its value is only known at run time;
- a stored command string (an `if-shell` branch, a hook, a
  `confirm-before` command, a one-string bind body) has its targets
  normalized like a top-level command, and is rewritten only when that
  changes something;
- flag clusters are written out one flag each (`-gqv` is `-g -q -v`), which
  tmux parses identically.

Formats, option names, hook names, key names and shell strings are kept (with
the substitutions above), since the front end's tasks need them.

## Regenerating

```sh
GROFE_COMPAT_CANOPY=<canopy checkout> GROFE_COMPAT_ATHOME=<athome checkout> \
GROFE_COMPAT_NVIM_PACK=<nvim pack dir> uv run scripts/corpus.py extract
GROFE_COMPAT_CANOPY=<canopy checkout> GROFE_COMPAT_ATHOME=<athome checkout> \
  scripts/record-container-corpus.sh
uv run scripts/corpus.py coverage     # rewrites the generated section below
scripts/check-corpus.sh
```

## Publishability

`scripts/check-corpus.sh` scans every file under `tests/compat/corpus/`,
`scripts/compat-rows.tsv`, `scripts/corpus-manual.txt`, everything under
`containers/compat/` and this page, and fails on any denylisted string. The
list is built on the machine running the check, never stored:

- this machine's home directory (anywhere, not only under `/Users` or
  `/home`) and git email, matched as substrings;
- its hostnames, user name (`$USER`, `id -un`), git user name, and the
  `Host`/`HostName` values of `~/.ssh/config` and the files it includes
  (public forges and loopback excepted), matched as whole words in any case;
  a found name equal to a word the corpus uses on its own (`main`, `rec`,
  `tmux` and the like) is skipped, since it cannot identify a machine;
- the terms in `GROFE_COMPAT_DENYLIST`, a file outside the repository with
  one term per line (for example `~/.secrets/grofe/denylist`, holding client
  and employer names), matched as whole words; the terms themselves are
  never committed.

It also fails on any email address and any `/Users/<name>` or
`/home/<name>` path (the container user `rec` excepted). Normalization
scrubs the same names out of recordings (`<host>`, `<redacted>`), and the
staging copy for the container is screened with the same list.

A form the corpus gains needs a line in `scripts/compat-rows.tsv`, which says
which spec 4.5 row owns it; the check refuses a row id that names nothing in
the spec.

## The tmux-live-guard hook

`~/.claude/hooks/tmux-live-guard.sh` blocks an agent's shell command that
matches `tmux ... kill-(server|session|window|pane)` without `-L` or `-S`. The
match is on the command text, so once `tmux` on `PATH` resolves to grofe
(alias day, plan M3.6) it keeps matching and guards the owner's grofe server
exactly as it guards tmux today. It does not match the native spelling
(`grofe kill-server`), nor the owner's shell alias `tk` (`tmux kill-server`,
in the corpus from athome's `aliases.zsh`), which never reaches an agent's
command text in that form.

## What the corpus says about spec 4.5

Every form maps to a row. The corpus recorded on 2026-09-28 (tmux 3.7b, fzf
0.74.4) raised the questions below; each one now has an owner, and M3.6
settled all of them (the M3.6 addendum's rulings M36-R1 to M36-R10).

- **fzf `--tmux` no longer uses `display-popup`.** fzf 0.74.4 probes with
  `list-commands new-pane`, opens its window as a tmux 3.7 floating pane
  (`new-pane -P -F #{pane_id} -t %N -c <dir> -x -y -X -Y sh -c ...`, inside
  an `if -F #{window_zoomed_flag}`), and waits on it with `wait-for` and
  `wait-for -S`. These shipped: `new-pane` as a real floating pane (TC.6),
  `wait-for` and `wait-for -S` (TC.9), the `list-commands` answer (FE.1),
  and `pane_floating_flag` (TC.3). A real `fzf --tmux` run works end to end
  (FE.4).
- **The owner's own binding sends a bare `=` target.** `user.conf`'s
  `MouseDown1Status` else branch is `switch-client -t =`, on purpose (it
  reproduces tmux's default). Inside a mouse key's commands a bare `=` now
  means the target under the pointer (M36-R3, TC.10); elsewhere it is still
  refused.
- **Popup and menu positions beyond `M`, `W`, `S`.** which-key opens its
  menus with `-x R -y P`, tmux-fzf its popup with `-x C -y C`, and tmux's
  stock menus use `-x W -y W`. TC.6 and TC.7 parse each letter the corpus
  sends, pinned by goldens recorded from tmux 3.7b (M36-R5).
- **Verbs no row named at first:** `move-pane` (tmux-fzf), `next-layout` and
  `respawn-window -k` (tmux's stock bindings and menus) shipped with TC.8
  (M36-R6). `set-window-option` and `show-window-options` resolve to `setw`
  and `showw` through the front end's alias table (FE.1, M36-R10). The
  copy-mode and buffer verbs beyond the three the M5 row names (`copy-mode`
  and its flags, `send-keys -X`, `delete-buffer`, `list-buffers`,
  `load-buffer`, `show-buffer`) belong to M5 (M36-R8). TM.7 maps them all,
  `load-buffer` included, and `save-buffer`, which the corpus does not
  send, and refuses `copy-mode -d -q -s -S` (`docs/copy-mode.md`).
- **`clear-history` and the screen-state formats (F1).** `clear-history`
  (`clearhist`, `-H`, `-t`) is a native row with a golden
  (`golden/f1/clear-history`): the pane leaves its modes, the scrollback and
  its prompt marks go, and `-H` forgets the pane's hyperlinks; on the
  alternate screen it does nothing, as alacritty keeps the normal screen's
  scrollback out of reach. `#{cursor_x}`, `#{cursor_y}` (resurrect's
  `save.sh` reads it), `#{cursor_character}`, `#{cursor_flag}`,
  `#{insert_flag}`, `#{keypad_cursor_flag}`, `#{keypad_flag}`,
  `#{wrap_flag}`, `#{origin_flag}` and the `#{mouse_*_flag}` family come
  from the emulator (`golden/f1/cursor`). Not given: `scroll_region_*` and
  `alternate_saved_*` (alacritty keeps them private), `cursor_shape`,
  `cursor_colour` and `cursor_blinking`. tmux keeps the last of mouse modes
  1000, 1002 and 1003; grofe reads the widest one set. `respawn-pane` keeps
  the scrollback and resets the screen, modes and tab stops, as tmux's
  `screen_reinit` does (`golden/f1/respawn`).
- **A hidden flag, not a rejected one:** opensessions' pane flash sends
  `select-pane -P bg=colour238`. The usage no longer lists `-P`, but tmux
  3.7b still takes it (recorded by FE.1): it selects the pane and sets the
  pane's `window-style` and `window-active-style`, with no check on the
  style. The front end does the same; grofe draws no pane background, so
  the two options have no effect.
- **Session groups and linked windows** are out of scope (spec), and sessionx
  sends both: `new-session -d -s <name> -t <session>` and `link-window`.
  M3.7's native sessionx port (TP.4) replaces them (M36-R9).
- **Options and format variables** the spec does not name are listed in the
  generated tables below (a `no` in the last column).

## Key parity

`scripts/key-parity.sh` (plan M3.7, TP.9a and TP.9b) loads the owner's
whole config with every plugin on real tmux 3.7b and on grofe, and compares
`list-keys` per table, every table either side has, `copy-mode` and
`copy-mode-vi` included; tmux's `copy-mode-vi` must also be TM.0's 95-row
golden. It runs two grofe servers: `grofe-tmux`, grofe answering to the
tmux name as it will from alias day, and `grofe`, the same native config
with the server started under grofe's own name. A key that differs fails
the script unless a row below names it. *Servers* says which grofe server
the row is for. A row whose key is `(all)` covers every key of its table
that has no row of its own. The classes:

- **oos**: out of scope in the spec (§4.5). Reported, the difference excused.
- **mouse**: a stock tmux mouse binding. grofe leaves it unbound, and its
  built-in mouse model (TC.10 and TM.6, `src/server/mouse.rs`) does the
  job, or nothing does, as the reason says. Where the model does the job,
  the script's third part clicks, wheels or drags it on an attached client
  and reads back what tmux did.
- **native**: grofe binds the key in its own form on purpose.
- **gap**: a known gap outside grofe. Reported, not fixed here.

No row is deferred: since TP.9b every key is compared, and M5's copy mode,
buffers and mouse rows are checked like the rest.

The table cannot go stale. A row fails the script when its key matches tmux
anyway (whatever its class), when neither side binds its key, or when
neither side has its table; an `(all)` row fails when no key of its table
differs.

Under the tmux name, grofe shows tmux's own stock binding for every stock
key (`[ ] = PPage m` and the `#` buffer list included), so those
`list-keys` rows match tmux there and the rows below name only `grofe`.
`m` still fails with the front end's reason when pressed (out of scope).

The third part presses `prefix [`, `PPage`, `=` and `]` and tmux-yank's
`y` on an attached client, and sends the root mouse rows as SGR reports
(`WheelUpPane` twice, a drag ending in tmux-yank's `MouseDragEnd1Pane`,
`DoubleClick1Pane`, `TripleClick1Pane`, `MouseDown2Pane`). After each step
it reads back the pane's mode and position, the newest buffer, what the
clipboard stubs received and what was pasted, and each grofe server must
read back what tmux did.

| Table | Key | Class | Servers | Reason |
|---|---|---|---|---|
| `prefix` | `m` | oos | grofe | out of scope: marks unused by the owner's scripts (spec §4.5, marked panes) |
| `root` | `WheelUpPane` | mouse | both | built-in (TM.6): over a pane in no mode, not on the alternate screen and not asking for the mouse, `copy-mode -e`; otherwise the pane gets the wheel. Read back against tmux |
| `root` | `MouseDrag1Pane` | mouse | both | built-in (TM.6): over such a pane, `copy-mode -M`, a selection from the press; the release runs `copy-mode-vi`'s `MouseDragEnd1Pane` (tmux-yank's copy). Read back against tmux |
| `root` | `DoubleClick1Pane` | mouse | both | built-in (TM.6): `select-pane`, `copy-mode -H`, `select-word`, `run-shell -d 0.3`, `copy-pipe-and-cancel`. Read back against tmux |
| `root` | `TripleClick1Pane` | mouse | both | built-in (TM.6): the same with `select-line`. Read back against tmux |
| `root` | `MouseDrag1ScrollbarSlider` | mouse | both | never fires: grofe draws no pane scrollbar (copy-mode.md, out of scope) |
| `root` | `MouseDown1Pane` | mouse | both | built-in: a press focuses the pane under the pointer and reaches it if it asked for the mouse |
| `root` | `MouseDrag1Border` | mouse | both | built-in: the border under the pointer follows the drag until the release |
| `root` | `WheelUpStatus` | mouse | both | built-in: the previous window |
| `root` | `WheelDownStatus` | mouse | both | built-in: the next window |
| `root` | `MouseDown2Pane` | mouse | both | built-in: focus, then `paste-buffer -p` over a pane in no mode that did not ask for the mouse; otherwise the press reaches the pane. Read back against tmux |
| `root` | `MouseDown1Border` | mouse | both | `select-pane -M` clears the mark, which is out of scope (`m`); the built-in drag starts here |
| `root` | `MouseDown3Pane` | mouse | both | no replacement: tmux's pane menu opens at `-x M -y M`, a mouse argv form (spec §4.5); the press reaches a pane that asked for the mouse |
| `root` | `M-MouseDown3Pane` | mouse | both | no replacement: the same pane menu, at `-x M -y M` |
| `root` | `MouseDown3Status` | mouse | both | no replacement: tmux's window menu opens at `-x W -y W`, a mouse argv form |
| `root` | `M-MouseDown3Status` | mouse | both | no replacement: the same window menu, at `-x W -y W` |
| `root` | `MouseDown3StatusLeft` | mouse | both | no replacement: tmux's session menu opens at `-x M -y W`, a mouse argv form |
| `root` | `M-MouseDown3StatusLeft` | mouse | both | no replacement: the same session menu |
| `root` | `C-MouseDown1Pane` | mouse | both | no replacement: `swap-pane -s @` takes the pane under the pointer as a mouse target |
| `root` | `C-MouseDown1Status` | mouse | both | no replacement: `swap-window -t @`, the same for the window clicked |
| `root` | `MouseDown1Control8` | mouse | both | never fires: tmux 3.7's pane border controls, which grofe does not draw |
| `root` | `MouseDown1Control9` | mouse | both | never fires: the same, and its menu opens at `-x M -y M` |
| `root` | `MouseDown1ScrollbarUp` | mouse | both | never fires: grofe draws no pane scrollbar |
| `root` | `MouseDown1ScrollbarDown` | mouse | both | never fires: grofe draws no pane scrollbar |
| `root` | `C-h` | native | both | vim-tmux-navigator, which athome loads through TPM on tmux, is TP.2's native `40-canopy-nav.toml` on grofe (no `ps` fork per key) |
| `root` | `C-j` | native | both | the same |
| `root` | `C-k` | native | both | the same |
| `root` | `C-l` | native | both | the same |
| `root` | `C-\\` | native | both | the same |
| `prefix` | `C-l` | native | both | the same fragment gives clearing the screen to the prefix |
| `prefix` | `r` | native | both | canopy-reload runs grofe's `reload`, which validates every layer first (canopy `grofe/keys.tsv`) |
| `prefix` | `M-s` | native | both | the grofe fragment runs `"$CANOPY_STORE/..."`, which the shell expands from the server's environment; tmux expanded the same path when it read the config |
| `prefix` | `\"` | native | grofe | grofe's own default table splits in the active pane's directory (spec §5.3); under the tmux name it is tmux's stock `split-window` |
| `prefix` | `\%` | native | grofe | the same, `split-window -h` |
| `prefix` | `&` | native | grofe | grofe's own default table spells tmux's `#W` as `#{window_name}`, the same text |
| `prefix` | `x` | native | grofe | the same, `#P` as `#{pane_index}` |
| `prefix` | `,` | native | grofe | grofe's own default table writes the prompt's template as a quoted string, not a `{ }` block: the same command |
| `prefix` | `\$` | native | grofe | the same |
| `prefix` | `\'` | native | grofe | the same |
| `prefix` | `.` | native | grofe | the same |
| `prefix` | `/` | native | grofe | the same |
| `prefix` | `f` | native | grofe | the same |
| `prefix` | `0` | native | grofe | grofe's own default table targets `:0`, which the native grammar reads as exactly window 0, as tmux reads `:=0` |
| `prefix` | `1` | native | grofe | the same |
| `prefix` | `2` | native | grofe | the same |
| `prefix` | `3` | native | grofe | the same |
| `prefix` | `4` | native | grofe | the same |
| `prefix` | `5` | native | grofe | the same |
| `prefix` | `6` | native | grofe | the same |
| `prefix` | `7` | native | grofe | the same |
| `prefix` | `8` | native | grofe | the same |
| `prefix` | `9` | native | grofe | the same |
| `prefix` | `*` | gap | grofe | a gap, not a choice: grofe's own default table leaves out tmux 3.7's stock `* new-pane`, though the verb exists (TC.6, built to fzf's recorded form). Under the tmux name the stock binding is there; what a bare `new-pane` does on grofe is not checked here |
| `prefix` | `<` | oos | grofe | out of scope: tmux's window menu opens at `-x W -y W`, a mouse position form (spec §4.5, mouse argv forms), so grofe's own default table leaves it unbound |
| `prefix` | `>` | oos | grofe | out of scope: tmux's pane menu opens at `-x P -y P`, but its items are mouse argv forms (`copy-mode -t=`, `send-keys -X -t =`, `#{mouse_word}`; spec §4.5), so grofe's own default table leaves it unbound |
| `copy-mode` | `(all)` | oos | both | the emacs table: `mode-keys` stays `vi` and grofe ships no emacs table (copy-mode.md, out of scope). tmux-yank's five rows there match |
| `copy-mode-vi` | `C-h` | gap | both | vim-tmux-navigator's copy-mode-vi rows (`select-pane -L`, and `-D -U -R -l` below): canopy's `40-canopy-nav.toml` still defers them to M5, so grofe keeps the stock row. A canopy change, outside grofe |
| `copy-mode-vi` | `C-j` | gap | both | the same (`select-pane -D`; grofe keeps the stock `copy-pipe-and-cancel`) |
| `copy-mode-vi` | `C-k` | gap | both | the same (`select-pane -U`; unbound on grofe) |
| `copy-mode-vi` | `C-l` | gap | both | the same (`select-pane -R`; unbound on grofe) |
| `copy-mode-vi` | `C-\\` | gap | both | the same (`select-pane -l`; unbound on grofe) |
| `copy-mode-vi` | `/` | native | grofe | grofe's own default table writes the prompt's template as a quoted string, not a `{ }` block: the same command. Under the tmux name it is printed as tmux prints it |
| `copy-mode-vi` | `?` | native | grofe | the same |
| `copy-mode-vi` | `:` | native | grofe | the same |
| `copy-mode-vi` | `F` | native | grofe | the same |
| `copy-mode-vi` | `T` | native | grofe | the same |
| `copy-mode-vi` | `f` | native | grofe | the same |
| `copy-mode-vi` | `t` | native | grofe | the same |
| `copy-mode-vi` | `1` | native | grofe | the same |
| `copy-mode-vi` | `2` | native | grofe | the same |
| `copy-mode-vi` | `3` | native | grofe | the same |
| `copy-mode-vi` | `4` | native | grofe | the same |
| `copy-mode-vi` | `5` | native | grofe | the same |
| `copy-mode-vi` | `6` | native | grofe | the same |
| `copy-mode-vi` | `7` | native | grofe | the same |
| `copy-mode-vi` | `8` | native | grofe | the same |
| `copy-mode-vi` | `9` | native | grofe | the same |

## Coverage

The tables below are generated from the corpus by `scripts/corpus.py
coverage`; do not edit them by hand.

<!-- generated by scripts/corpus.py coverage: start -->

### Corpus size

| File | Lines |
|---|---|
| `tests/compat/corpus/static-cli.argv` | 494 |
| `tests/compat/corpus/static-conf.argv` | 154 |
| `tests/compat/corpus/container-cli.argv` | 62 |
| `tests/compat/corpus/container-pane.argv` | 47 |
| `tests/compat/corpus/container-job.argv` | 332 |
| `tests/compat/corpus/container-stored.argv` | 343 |
| total | 1432 |

Distinct forms (verb plus flag set, stored commands included): **287**.

### Forms

One row per distinct form. *Lines* counts corpus lines holding it; *stored* counts those where it sits inside a stored command (a bind body, hook, `if-shell` branch, menu item). *Verb named* says whether the mapped row mentions the verb at all, and *unnamed flags* lists flags it does not mention (both an approximate text match): a **no** or a listed flag is a question for the task that owns the row, not an error. A flag marked `!` is one tmux 3.7b's `list-commands` usage leaves out; it may still be a hidden flag tmux accepts (`select-pane -P` is).

| Form | Lines | Stored | Files | Spec 4.5 row | Verb named | Unnamed flags | Sources |
|---|---|---|---|---|---|---|---|
| `%endif` | 1 | 0 | sf1 | oos:`%if` |  |  | canopy:tmux/conf.d/00-core.conf |
| `%if` | 2 | 0 | sf2 | oos:`%if` |  |  | canopy:tmux/conf.d/00-core.conf |
| `(alias) show-wk-menu-root` | 3 | 3 | sc3 | area:Bindings |  |  | athome:home/dot_config/tmux/executable_wk-keys-fix.sh, plugin:tmux-which-key/plugin/build.py, plugin:tmux-which-key/plugin/init.example.tmux (+1) |
| `attach-session` | 1 | 0 | sc1 | cmd:M2 | yes |  | athome:home/dot_zsh/aliases.zsh |
| `attach-session -d -t` | 1 | 0 | cc1 | cmd:M2 | yes | `-d`, `-t` | container:cli |
| `attach-session -t` | 5 | 0 | cc1 cp1 sc3 | cmd:M2 | yes | `-t` | athome:home/dot_zsh/functions.zsh, container:cli, container:pane (+2) |
| `bind-key` | 71 | 0 | cj25 sc22 sf24 | area:Bindings | yes |  | athome:home/dot_config/canopy/user.conf.tmpl, canopy:plugins/tmux-resurrect/resurrect.tmux, canopy:tmux/keys.tsv (+10) |
| `bind-key -N` | 1 | 0 | cj1 | area:Bindings | yes |  | container:job |
| `bind-key -T` | 356 | 0 | cj24 cs311 sc21 | area:Bindings | yes |  | athome:home/dot_config/tmux/executable_wk-keys-fix.sh, container:job, container:list-keys (+6) |
| `bind-key -n` | 10 | 0 | cj5 sc4 sf1 | area:Bindings | yes | `-n` | athome:home/dot_config/canopy/user.conf.tmpl, container:job, plugin:opensessions/opensessions.tmux (+2) |
| `bind-key -r` | 6 | 0 | sf6 | area:Bindings | yes |  | canopy:tmux/keys.tsv |
| `bind-key -r -T` | 22 | 0 | cs22 | area:Bindings | yes |  | container:list-keys |
| `break-pane` | 1 | 1 | cs1 | area:Windows and panes | yes |  | container:list-keys |
| `break-pane -s -t` | 1 | 0 | sc1 | area:Windows and panes | yes |  | plugin:tmux-fzf/scripts/pane.sh |
| `capture-pane -J -p -t` | 1 | 0 | sc1 | area:Listing and inspection | yes |  | canopy:plugins/tmux-resurrect/scripts/save.sh |
| `capture-pane -e -J -p` | 1 | 0 | sc1 | area:Listing and inspection | yes |  | plugin:tmux-fzf-url/fzf-url.sh |
| `capture-pane -e -J -p -S` | 1 | 0 | sc1 | area:Listing and inspection | yes |  | plugin:tmux-fzf-url/fzf-url.sh |
| `capture-pane -e -J -p -S -t` | 1 | 0 | sc1 | area:Listing and inspection | yes |  | canopy:plugins/tmux-resurrect/scripts/save.sh |
| `capture-pane -e -p -t` | 8 | 0 | cp2 sc6 | area:Listing and inspection | yes |  | athome:home/dot_local/bin/executable_tsess, container:pane, plugin:tmux-claude-session-manager/scripts/picker.sh (+3) |
| `choose-buffer -Z` | 1 | 1 | cs1 | area:Buffers and copy | yes | `-Z` | container:list-keys |
| `choose-client -Z` | 1 | 1 | cs1 | area:Sessions and clients | yes |  | container:list-keys |
| `choose-tree -w -Z` | 1 | 1 | cs1 | area:Overlays | yes |  | container:list-keys |
| `command-prompt` | 2 | 2 | cs2 | area:Overlays | yes |  | container:list-keys |
| `command-prompt -1 -p` | 8 | 8 | cs8 | area:Overlays | yes | `-p` | container:list-keys |
| `command-prompt -F -I` | 3 | 3 | cs3 | area:Bindings | yes |  | container:list-keys |
| `command-prompt -I` | 5 | 4 | cs4 sc1 | area:Bindings | yes |  | container:list-keys, plugin:tmux-fzf/scripts/command.sh |
| `command-prompt -I -N -p` | 18 | 18 | cs18 | area:Bindings | yes |  | container:list-keys |
| `command-prompt -T` | 1 | 1 | cs1 | area:Overlays | yes |  | container:list-keys |
| `command-prompt -i -I -p -T` | 2 | 2 | cs2 | area:Bindings | yes | `-i` | container:list-keys |
| `command-prompt -k -p` | 1 | 1 | cs1 | area:Overlays | yes | `-p` | container:list-keys |
| `command-prompt -p` | 3 | 2 | cs2 sc1 | area:Overlays | yes | `-p` | container:list-keys, plugin:tpm/bindings/update_plugins |
| `command-prompt -p -T` | 3 | 3 | cs3 | area:Overlays | yes | `-p` | container:list-keys |
| `confirm-before -p` | 2 | 2 | cs2 | area:Bindings | yes |  | container:list-keys |
| `copy-mode` | 2 | 1 | cs1 sc1 | area:Buffers and copy | yes |  | container:list-keys, plugin:tmux-fzf/scripts/keybinding.sh, plugin:tmux-yank/scripts/copy_line.sh |
| `copy-mode -H` | 2 | 2 | cs2 | area:Buffers and copy | yes |  | container:list-keys |
| `copy-mode -M` | 1 | 1 | cs1 | area:Buffers and copy | yes |  | container:list-keys |
| `copy-mode -S` | 1 | 1 | cs1 | area:Buffers and copy | yes |  | container:list-keys |
| `copy-mode -d` | 1 | 1 | cs1 | area:Buffers and copy | yes |  | container:list-keys |
| `copy-mode -e` | 1 | 1 | cs1 | area:Buffers and copy | yes |  | container:list-keys |
| `copy-mode -q` | 15 | 15 | cs15 | area:Buffers and copy | yes |  | container:list-keys |
| `copy-mode -t[=]` | 3 | 3 | cs3 | oos:Mouse argv forms |  |  | container:list-keys |
| `copy-mode -u` | 2 | 2 | cs2 | area:Buffers and copy | yes |  | container:list-keys |
| `delete-buffer -b` | 1 | 0 | sc1 | area:Buffers and copy | yes |  | plugin:tmux-fzf/scripts/clipboard.sh |
| `detach-client` | 3 | 3 | cs3 | cmd:M2 | yes |  | container:list-keys |
| `detach-client -s` | 2 | 0 | sc2 | area:Sessions and clients | yes |  | plugin:tmux-claude-session-manager/scripts/list.sh, plugin:tmux-fzf/scripts/session.sh |
| `display-menu -O -t[=] -T -x[M] -y[M]` | 1 | 1 | cs1 | oos:Mouse argv forms |  |  | container:list-keys |
| `display-menu -T -x[P] -y[P]` | 1 | 1 | cs1 | area:Overlays | yes |  | container:list-keys |
| `display-menu -T -x[R] -y[P]` | 1 | 0 | sc1 | area:Overlays | yes |  | plugin:tmux-which-key/which-key.sh |
| `display-menu -T -x[W] -y[W]` | 1 | 1 | cs1 | oos:Mouse argv forms |  |  | container:list-keys |
| `display-menu -t[=] -T -x[M] -y[M]` | 2 | 2 | cs2 | oos:Mouse argv forms |  |  | container:list-keys |
| `display-menu -t[=] -T -x[M] -y[W]` | 2 | 2 | cs2 | oos:Mouse argv forms |  |  | container:list-keys |
| `display-menu -t[=] -T -x[W] -y[W]` | 2 | 2 | cs2 | oos:Mouse argv forms |  |  | container:list-keys |
| `display-message` | 31 | 3 | cc2 cj13 cp1 cs2 sc10 sf3 | area:Listing and inspection | yes |  | canopy:plugins/tmux-resurrect/scripts/check_tmux_version.sh, canopy:plugins/tmux-resurrect/scripts/helpers.sh, canopy:plugins/tmux-resurrect/scripts/tmux_spinner.sh (+14) |
| `display-message -F -p` | 3 | 0 | cj1 sc2 | area:Listing and inspection | yes |  | canopy:plugins/tmux-continuum/continuum.tmux, container:job, plugin:tmux-yank/scripts/copy_pane_pwd.sh |
| `display-message -F -p -t` | 3 | 0 | sc3 | area:Listing and inspection | yes |  | canopy:plugins/tmux-resurrect/scripts/restore.sh, canopy:plugins/tmux-resurrect/scripts/save.sh |
| `display-message -c -p` | 2 | 0 | cc1 sc1 | area:Listing and inspection | yes | `-c` | container:cli, manual:plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs |
| `display-message -p` | 43 | 1 | cc3 cj9 cp3 sc24 sf4 | area:Listing and inspection | yes |  | canopy:bin/canopy-config, canopy:bin/canopy-reboot-check, canopy:bin/canopy-status (+26) |
| `display-message -p -t` | 23 | 0 | cc2 cj3 cp4 sc14 | area:Listing and inspection | yes |  | canopy:bin/canopy-adopt, canopy:bin/canopy-agent, canopy:bin/canopy-reboot-check (+12) |
| `display-panes` | 1 | 1 | cs1 | area:Overlays | yes |  | container:list-keys |
| `display-popup -B -d -E -h -w` | 2 | 0 | cj2 | area:Overlays | yes | `-B` | container:job |
| `display-popup -B -d -E -h -w -x[C] -y[C]` | 2 | 0 | cj2 | area:Overlays | yes | `-B` | container:job |
| `display-popup -E -h -w` | 5 | 2 | cj1 cs1 sc2 sf1 | area:Overlays | yes |  | athome:home/dot_config/canopy/user.conf.tmpl, container:job, container:list-keys (+2) |
| `display-popup -b -E -h -T -w` | 4 | 4 | cs2 sf2 | area:Overlays | yes |  | athome:home/dot_config/canopy/user.conf.tmpl, container:list-keys |
| `display-popup -b -E -h -w` | 4 | 4 | cs2 sf2 | area:Overlays | yes |  | athome:home/dot_config/canopy/user.conf.tmpl, container:list-keys |
| `display-popup -c -E -h -w` | 2 | 0 | cj1 sc1 | area:Overlays | yes |  | container:job, plugin:tmux-claude-session-manager/scripts/list.sh |
| `display-popup -d` | 1 | 0 | sc1 | area:Overlays | yes |  | plugin:tmux-fzf/scripts/.fzf-tmux |
| `display-popup -d -E -h -w` | 3 | 2 | cs1 sc1 sf1 | area:Overlays | yes |  | athome:home/dot_config/canopy/user.conf.tmpl, container:list-keys, manual:plugin:opensessions/apps/tui-rs/src/main.rs |
| `display-popup -h -w -x[C] -y[C]` | 1 | 0 | sc1 | area:Overlays | yes |  | plugin:tmux-fzf/scripts/menu.sh |
| `find-window -Z` | 1 | 1 | cs1 | area:Windows and panes | yes |  | container:list-keys |
| `has-session -t` | 14 | 0 | cc1 cj1 cp2 sc10 | area:Sessions and clients | yes |  | athome:home/dot_local/bin/executable_cockpit, athome:home/dot_local/bin/executable_tsess, canopy:plugins/tmux-resurrect/scripts/restore.sh (+8) |
| `if-shell` | 16 | 11 | cj5 cs5 sc1 sf5 | area:Bindings | yes |  | athome:home/dot_config/canopy/user.conf.tmpl, canopy:tmux/conf.d/30-plugins.conf, container:job (+2) |
| `if-shell -F` | 11 | 7 | cs9 sc1 sf1 | area:Bindings | yes |  | athome:home/dot_config/canopy/user.conf.tmpl, container:list-keys, manual:nvim:vim-tmux-navigator/plugin/tmux_navigator.vim |
| `if-shell -F -t` | 1 | 0 | cp1 | area:Bindings | yes |  | container:pane |
| `if-shell -F -t[=]` | 4 | 4 | cs4 | oos:Mouse argv forms |  |  | container:list-keys |
| `join-pane` | 1 | 0 | sc1 | area:Windows and panes | yes |  | plugin:opensessions/integrations/tmux-plugin/scripts/sidebar-common.sh |
| `join-pane -b -d -f -h -l -s -t` | 1 | 0 | cj1 | area:Windows and panes | yes | `-b`, `-f`, `-l` | container:job |
| `join-pane -d -s -t` | 2 | 0 | cj1 sc1 | area:Windows and panes | yes |  | container:job, plugin:opensessions/integrations/tmux-plugin/scripts/sidebar-common.sh |
| `kill-pane` | 4 | 4 | cs4 | cmd:M1 | yes |  | container:list-keys |
| `kill-pane -t` | 8 | 0 | cc1 cj1 sc6 | cmd:M1 | yes |  | canopy:plugins/tmux-resurrect/scripts/restore.sh, container:cli, container:job (+5) |
| `kill-pane -t[=]` | 1 | 1 | cs1 | oos:Mouse argv forms |  |  | container:list-keys |
| `kill-server` | 3 | 0 | cp1 sc2 | cmd:M1 | yes |  | athome:home/.chezmoiscripts/run_onchange_18-install-canopy.sh.tmpl, athome:home/dot_config/systemd/user/tmux.service.tmpl, athome:home/dot_zsh/aliases.zsh (+2) |
| `kill-session -t` | 4 | 0 | cc1 sc3 | cmd:M1 | yes | `-t` | athome:home/.chezmoiscripts/run_onchange_after_14-install-tmux-plugins.sh.tmpl, canopy:plugins/tmux-resurrect/scripts/restore.sh, container:cli (+4) |
| `kill-window` | 4 | 4 | cs4 | cmd:M1 | yes |  | container:list-keys |
| `kill-window -t` | 3 | 0 | cc1 sc2 | cmd:M1 | yes | `-t` | container:cli, plugin:opensessions/packages/runtime-rs/src/tmux_scripting.rs, plugin:tmux-fzf/scripts/.fzf-tmux |
| `last-pane` | 1 | 0 | cs1 | area:Windows and panes | yes |  | container:list-keys |
| `last-window` | 1 | 1 | sc1 | cmd:M2 | yes |  | plugin:tmux-sensible/sensible.tmux |
| `link-window -a -s -t` | 1 | 0 | sc1 | oos:`link-window` |  |  | plugin:tmux-fzf/scripts/window.sh |
| `list-buffers` | 2 | 1 | cs1 sc1 | area:Buffers and copy | yes |  | container:list-keys, plugin:tmux-fzf/scripts/clipboard.sh |
| `list-clients` | 3 | 0 | cj1 cp1 sc1 | cmd:M2 | yes |  | canopy:bin/canopy-reboot-check, canopy:plugins/tmux-continuum/scripts/helpers.sh, container:job (+1) |
| `list-clients -F` | 4 | 0 | cj1 sc3 | area:Sessions and clients | yes |  | container:job, plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs, plugin:tmux-claude-session-manager/scripts/list.sh |
| `list-clients -F -t` | 1 | 0 | sc1 | area:Sessions and clients | yes |  | plugin:opensessions/packages/runtime-rs/src/tmux_scripting.rs |
| `list-commands` | 2 | 0 | cp1 sc1 | area:Overlays | yes |  | container:pane, plugin:tmux-fzf/scripts/command.sh |
| `list-keys` | 2 | 0 | cj1 sc1 | cmd:M3 | yes |  | athome:home/dot_config/tmux/executable_keys-popup.sh, container:job, plugin:tmux-fzf/scripts/keybinding.sh (+1) |
| `list-keys -1 -N` | 1 | 1 | cs1 | area:Overlays | yes | `-N` | container:list-keys |
| `list-panes -F` | 1 | 0 | sc1 | area:Listing and inspection | yes |  | plugin:tmux-fzf/scripts/.fzf-tmux |
| `list-panes -F -s -t` | 3 | 0 | cc1 cj1 sc1 | area:Listing and inspection | yes | `-s` | container:cli, container:job, manual:plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs |
| `list-panes -F -t` | 18 | 0 | cc1 cj7 cp1 sc9 | area:Listing and inspection | yes |  | canopy:plugins/tmux-resurrect/scripts/restore.sh, container:cli, container:job (+9) |
| `list-panes -a` | 2 | 0 | cj1 sc1 | area:Listing and inspection | yes |  | canopy:plugins/tmux-resurrect/scripts/restore.sh, container:job |
| `list-panes -a -F` | 17 | 0 | cc3 cj4 cp1 sc9 | area:Listing and inspection | yes |  | canopy:bin/canopy-adopt, canopy:bin/canopy-reboot-check, canopy:lib/pane.sh (+10) |
| `list-panes -a -f -F` | 6 | 0 | cc1 cj4 sc1 | area:Listing and inspection | yes |  | container:cli, container:job, plugin:opensessions/packages/runtime-rs/src/tmux_scripting.rs |
| `list-sessions` | 5 | 0 | cc1 cj1 cp1 sc2 | area:Listing and inspection | yes |  | athome:home/.chezmoiscripts/run_onchange_18-install-canopy.sh.tmpl, athome:home/dot_zsh/aliases.zsh, athome:home/dot_zsh/functions.zsh (+9) |
| `list-sessions -F` | 14 | 0 | cc3 cj3 cp1 sc7 | area:Listing and inspection | yes |  | athome:home/dot_local/bin/executable_tsess, canopy:plugins/tmux-resurrect/scripts/save.sh, container:cli (+6) |
| `list-sessions -f -F` | 1 | 0 | sc1 | area:Listing and inspection | yes |  | plugin:tmux-sessionx/scripts/preview.sh |
| `list-windows` | 3 | 0 | sc3 | area:Listing and inspection | yes |  | plugin:tmux-fzf/scripts/pane.sh, plugin:tmux-fzf/scripts/window.sh |
| `list-windows -F -t` | 10 | 0 | cj1 cp1 sc8 | area:Listing and inspection | yes |  | athome:home/dot_local/bin/executable_cockpit, athome:home/dot_local/bin/executable_tsess, canopy:plugins/tmux-resurrect/scripts/restore.sh (+5) |
| `list-windows -a -F` | 8 | 0 | cc2 cj2 sc4 | area:Listing and inspection | yes |  | athome:home/dot_local/bin/executable_tsess, canopy:plugins/tmux-resurrect/scripts/save.sh, container:cli (+4) |
| `load-buffer` | 1 | 0 | sc1 | area:Buffers and copy | yes |  | plugin:tmux-fzf-url/fzf-url.sh |
| `move-pane -s` | 1 | 0 | sc1 | area:Windows and panes | **no** |  | plugin:tmux-fzf/scripts/pane.sh |
| `move-window -a -s -t` | 1 | 0 | sc1 | area:Windows and panes | yes |  | plugin:tmux-fzf/scripts/window.sh |
| `move-window -r` | 2 | 2 | cs2 | area:Windows and panes | yes | `-r` | container:list-keys |
| `move-window -r -s -t` | 1 | 0 | cc1 | area:Windows and panes | yes | `-r` | container:cli |
| `move-window -s -t` | 1 | 0 | sc1 | area:Windows and panes | yes |  | canopy:plugins/tmux-resurrect/scripts/restore.sh |
| `move-window -t` | 1 | 1 | cs1 | area:Windows and panes | yes |  | container:list-keys |
| `new-pane` | 1 | 1 | cs1 | area:Overlays | yes |  | container:list-keys |
| `new-pane -c -F -P -t -x -X -y -Y` | 1 | 0 | cp1 | area:Overlays | yes |  | container:pane |
| `new-session` | 3 | 2 | cs2 sc1 | cmd:M1 | yes |  | athome:home/dot_zsh/aliases.zsh, container:list-keys |
| `new-session -A -d -s` | 1 | 0 | cc1 | area:Sessions and clients | yes |  | container:cli |
| `new-session -A -s` | 1 | 0 | sc1 | area:Sessions and clients | yes |  | athome:home/dot_zsh/aliases.zsh |
| `new-session -c -d -F -P` | 1 | 0 | sc1 | cmd:M1 | yes | `-F`, `-P` | manual:plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs |
| `new-session -c -d -F -P -s` | 1 | 0 | sc1 | cmd:M1 | yes | `-F`, `-P` | manual:plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs |
| `new-session -c -d -n -P -s` | 1 | 0 | cc1 | cmd:M1 | yes | `-n`, `-P` | container:cli |
| `new-session -c -d -n -s` | 1 | 0 | sc1 | cmd:M1 | yes | `-n` | plugin:tmux-sessionx/scripts/sessionx.sh |
| `new-session -c -d -s` | 8 | 0 | cj1 sc7 | cmd:M1 | yes |  | athome:home/dot_local/bin/executable_tsess, canopy:plugins/tmux-resurrect/scripts/restore.sh, container:job (+3) |
| `new-session -d` | 3 | 0 | cp1 sc2 | cmd:M1 | yes |  | athome:home/dot_config/systemd/user/tmux.service.tmpl, canopy:lib/tmux.sh, container:pane (+1) |
| `new-session -d -F -P` | 1 | 0 | sc1 | cmd:M1 | yes | `-F`, `-P` | manual:plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs |
| `new-session -d -F -P -s` | 1 | 0 | sc1 | cmd:M1 | yes | `-F`, `-P` | manual:plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs |
| `new-session -d -s` | 3 | 0 | sc3 | cmd:M1 | yes |  | athome:home/.chezmoiscripts/run_onchange_after_14-install-tmux-plugins.sh.tmpl, plugin:tmux-fzf/scripts/session.sh, plugin:tmux-sessionx/scripts/sessionx.sh |
| `new-session -d -s -t` | 1 | 0 | sc1 | oos:`link-window` |  |  | canopy:plugins/tmux-resurrect/scripts/restore.sh |
| `new-session -d -s -x -y` | 3 | 0 | cc1 cj1 sc1 | cmd:M1 | yes |  | container:cli, container:job, plugin:opensessions/integrations/tmux-plugin/scripts/sidebar-common.sh |
| `new-window` | 6 | 5 | cs5 sc1 | cmd:M1 | yes |  | athome:home/.chezmoitemplates/gh-dash/config.yml, container:list-keys |
| `new-window -a` | 3 | 3 | cs3 | cmd:M1 | yes | `-a` | container:list-keys |
| `new-window -c` | 4 | 4 | cs2 sf2 | cmd:M1 | yes | `-c` | canopy:tmux/keys.tsv, container:list-keys |
| `new-window -c -d -F -n -P -t` | 1 | 0 | cc1 | area:Windows and panes | yes | `-c`, `-F`, `-P` | container:cli |
| `new-window -c -d -t` | 2 | 0 | sc2 | cmd:M1 | yes | `-c`, `-d`, `-t` | canopy:plugins/tmux-resurrect/scripts/restore.sh |
| `new-window -d -F -P` | 1 | 0 | sc1 | cmd:M1 | yes | `-d`, `-F`, `-P` | plugin:tmux-fzf/scripts/.fzf-tmux |
| `new-window -d -t` | 1 | 0 | sc1 | cmd:M1 | yes | `-d`, `-t` | plugin:opensessions/integrations/tmux-plugin/scripts/sidebar-common.sh |
| `new-window -n` | 4 | 4 | cs2 sf2 | area:Windows and panes | yes |  | athome:home/dot_config/canopy/user.conf.tmpl, container:list-keys |
| `next-layout` | 1 | 1 | cs1 | area:Windows and panes | **no** |  | container:list-keys |
| `next-window` | 5 | 5 | cj1 cs3 sc1 | cmd:M2 | yes |  | container:job, container:list-keys, plugin:tmux-sensible/sensible.tmux |
| `next-window -a` | 1 | 1 | cs1 | area:Windows and panes | yes |  | container:list-keys |
| `paste-buffer` | 3 | 3 | cs3 | area:Buffers and copy | yes |  | container:list-keys |
| `paste-buffer -b` | 1 | 0 | sc1 | area:Buffers and copy | yes |  | plugin:tmux-fzf/scripts/clipboard.sh |
| `paste-buffer -p` | 3 | 2 | cs2 sc1 | area:Buffers and copy | yes |  | container:list-keys, plugin:tmux-yank/yank.tmux |
| `previous-window` | 5 | 5 | cj1 cs3 sc1 | cmd:M2 | yes |  | container:job, container:list-keys, plugin:tmux-sensible/sensible.tmux |
| `previous-window -a` | 1 | 1 | cs1 | area:Windows and panes | yes |  | container:list-keys |
| `refresh-client -D` | 1 | 1 | cs1 | area:Sessions and clients | yes | `-D` | container:list-keys |
| `refresh-client -L` | 1 | 1 | cs1 | area:Sessions and clients | yes | `-L` | container:list-keys |
| `refresh-client -R` | 1 | 1 | cs1 | area:Sessions and clients | yes | `-R` | container:list-keys |
| `refresh-client -S` | 2 | 0 | cp1 sc1 | area:Sessions and clients | yes |  | athome:home/dot_config/themes/executable_apply-tmux.sh, athome:home/dot_config/tmux/executable_pet-pick.sh, container:pane |
| `refresh-client -U` | 1 | 1 | cs1 | area:Sessions and clients | yes | `-U` | container:list-keys |
| `refresh-client -c` | 1 | 1 | cs1 | area:Sessions and clients | yes |  | container:list-keys |
| `rename-session` | 3 | 3 | cs3 | cmd:M2 | yes |  | container:list-keys |
| `rename-session -t` | 2 | 0 | sc2 | cmd:M2 | yes | `-t` | plugin:tmux-fzf/scripts/session.sh, plugin:tmux-sessionx/sessionx.tmux |
| `rename-window` | 1 | 1 | cs1 | cmd:M2 | yes |  | container:list-keys |
| `rename-window -t` | 9 | 3 | cj4 cs3 sc2 | cmd:M2 | yes | `-t` | canopy:plugins/tmux-resurrect/scripts/restore.sh, container:job, container:list-keys (+1) |
| `resize-pane -D` | 5 | 4 | cs3 sc1 sf1 | cmd:M1 | yes |  | canopy:tmux/keys.tsv, container:list-keys, plugin:tmux-fzf/scripts/pane.sh |
| `resize-pane -L` | 5 | 4 | cs3 sc1 sf1 | cmd:M1 | yes |  | canopy:tmux/keys.tsv, container:list-keys, plugin:tmux-fzf/scripts/pane.sh |
| `resize-pane -M` | 1 | 1 | cs1 | oos:Mouse argv forms |  |  | container:list-keys |
| `resize-pane -R` | 5 | 4 | cs3 sc1 sf1 | cmd:M1 | yes |  | canopy:tmux/keys.tsv, container:list-keys, plugin:tmux-fzf/scripts/pane.sh |
| `resize-pane -U` | 4 | 3 | cs2 sc1 sf1 | cmd:M1 | yes |  | canopy:tmux/keys.tsv, container:list-keys, plugin:tmux-fzf/scripts/pane.sh |
| `resize-pane -Z` | 6 | 5 | cs5 sc1 | cmd:M1 | yes |  | container:list-keys, plugin:tmux-fzf/scripts/.fzf-tmux |
| `resize-pane -t -U` | 1 | 0 | sc1 | cmd:M1 | yes |  | canopy:plugins/tmux-resurrect/scripts/restore.sh |
| `resize-pane -t -Z` | 3 | 1 | cp1 sc2 | cmd:M1 | yes |  | canopy:plugins/tmux-resurrect/scripts/restore.sh, canopy:plugins/tmux-resurrect/scripts/save.sh, container:pane |
| `resize-pane -t -x` | 6 | 0 | cc1 cj1 sc4 | area:Windows and panes | yes |  | container:cli, container:job, manual:claude-code/TmuxBackend (agent teams, as tuios's TestClaudeCodeTeammateSequence pins it) (+3) |
| `resize-window -t -x -y` | 2 | 0 | cj1 sc1 | area:Windows and panes | yes |  | container:job, plugin:opensessions/integrations/tmux-plugin/scripts/sidebar-common.sh |
| `respawn-pane -c -k` | 1 | 1 | sf1 | area:Windows and panes | yes | `-c` | plugin:tmux-which-key/plugin/init.tmux |
| `respawn-pane -k` | 3 | 3 | cs3 | area:Windows and panes | yes |  | container:list-keys |
| `respawn-pane -k -t` | 2 | 0 | sc2 | area:Windows and panes | yes |  | canopy:bin/canopy-adopt, manual:claude-code/TmuxBackend (agent teams, as tuios's TestClaudeCodeTeammateSequence pins it) |
| `respawn-window -k` | 3 | 3 | cs3 | area:Windows and panes | **no** |  | container:list-keys |
| `rotate-window` | 1 | 1 | cs1 | area:Windows and panes | yes |  | container:list-keys |
| `rotate-window -D` | 1 | 1 | cs1 | area:Windows and panes | yes |  | container:list-keys |
| `run-shell` | 108 | 104 | cj38 cs36 sc22 sf12 | cmd:M3 | yes |  | athome:home/dot_config/canopy/user.conf.tmpl, canopy:plugins/tmux-resurrect/resurrect.tmux, canopy:tmux/conf.d/30-plugins.conf (+13) |
| `run-shell -b` | 38 | 35 | cj17 cs15 sc5 sf1 | area:Hooks and environment | yes |  | athome:home/dot_config/canopy/user.conf.tmpl, container:job, container:list-keys (+5) |
| `run-shell -d` | 6 | 2 | cs6 | cmd:M3 | yes | `-d` | container:list-keys |
| `select-layout` | 5 | 2 | cs2 sc1 sf2 | cmd:M3 | yes |  | canopy:tmux/keys.tsv, container:list-keys, plugin:tmux-fzf/scripts/pane.sh |
| `select-layout -E` | 1 | 1 | cs1 | area:Windows and panes | yes |  | container:list-keys |
| `select-layout -t` | 12 | 0 | cc2 cj7 sc3 | cmd:M3 | yes | `-t` | canopy:plugins/tmux-resurrect/scripts/restore.sh, container:cli, container:job (+2) |
| `select-pane` | 12 | 12 | cs12 | cmd:M1 | yes |  | container:list-keys |
| `select-pane -D` | 7 | 7 | cj2 cs4 sf1 | cmd:M1 | yes |  | canopy:tmux/keys.tsv, container:job, container:list-keys |
| `select-pane -D -t` | 1 | 0 | sc1 | cmd:M1 | yes |  | manual:nvim:vim-tmux-navigator/plugin/tmux_navigator.vim |
| `select-pane -L` | 7 | 7 | cj2 cs4 sf1 | cmd:M1 | yes |  | canopy:tmux/keys.tsv, container:job, container:list-keys |
| `select-pane -L -t` | 2 | 1 | sc2 | cmd:M1 | yes |  | manual:nvim:vim-tmux-navigator/plugin/tmux_navigator.vim |
| `select-pane -L -t -Z` | 1 | 0 | sc1 | cmd:M1 | yes |  | manual:nvim:vim-tmux-navigator/plugin/tmux_navigator.vim |
| `select-pane -M` | 1 | 1 | cs1 | oos:Marked panes |  |  | container:list-keys |
| `select-pane -P! -t` | 2 | 0 | sc2 | cmd:M1 | yes | `-P!` | plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs |
| `select-pane -R` | 7 | 7 | cj2 cs4 sf1 | cmd:M1 | yes |  | canopy:tmux/keys.tsv, container:job, container:list-keys |
| `select-pane -R -t` | 1 | 0 | sc1 | cmd:M1 | yes |  | manual:nvim:vim-tmux-navigator/plugin/tmux_navigator.vim |
| `select-pane -U` | 7 | 7 | cj2 cs4 sf1 | cmd:M1 | yes |  | canopy:tmux/keys.tsv, container:job, container:list-keys |
| `select-pane -l` | 4 | 4 | cj2 cs2 | cmd:M1 | yes |  | container:job, container:list-keys |
| `select-pane -l -t` | 1 | 0 | sc1 | cmd:M1 | yes |  | manual:nvim:vim-tmux-navigator/plugin/tmux_navigator.vim |
| `select-pane -m` | 7 | 7 | cs7 | oos:Marked panes |  |  | container:list-keys |
| `select-pane -t` | 14 | 0 | cj2 cp1 cs1 sc9 sf1 | cmd:M1 | yes |  | canopy:plugins/tmux-resurrect/scripts/process_restore_helpers.sh, canopy:plugins/tmux-resurrect/scripts/restore.sh, canopy:tmux/keys.tsv (+10) |
| `select-pane -t -T` | 13 | 0 | cc1 cj9 sc3 | cmd:M1 | yes | `-T` | canopy:plugins/tmux-resurrect/scripts/restore.sh, container:cli, container:job (+2) |
| `select-pane -t -U` | 1 | 0 | sc1 | cmd:M1 | yes |  | manual:nvim:vim-tmux-navigator/plugin/tmux_navigator.vim |
| `select-pane -t[=]` | 5 | 5 | cs5 | oos:Mouse argv forms |  |  | container:list-keys |
| `select-window -t` | 17 | 11 | cs11 sc6 | area:Windows and panes | yes |  | athome:home/dot_local/bin/executable_tsess, container:list-keys, plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs (+4) |
| `send-keys` | 31 | 15 | cj6 cs6 sc19 | cmd:M1 | yes |  | container:job, container:list-keys, plugin:tmux-yank/scripts/copy_line.sh (+2) |
| `send-keys -F -X` | 2 | 2 | cs2 | area:Buffers and copy | yes |  | container:list-keys |
| `send-keys -M` | 7 | 6 | cs7 | oos:Mouse argv forms |  |  | container:list-keys |
| `send-keys -N` | 18 | 18 | cs18 | cmd:M1 | yes | `-N` | container:list-keys |
| `send-keys -N -X` | 5 | 0 | cs4 sc1 | area:Buffers and copy | yes |  | container:list-keys, plugin:tmux-yank/scripts/copy_line.sh |
| `send-keys -X` | 182 | 164 | cj6 cs158 sc18 | area:Buffers and copy | yes |  | container:job, container:list-keys, plugin:tmux-fzf/scripts/copy-mode.sh (+2) |
| `send-keys -l` | 6 | 6 | cs6 | cmd:M1 | yes |  | container:list-keys |
| `send-keys -t` | 1 | 0 | sc1 | cmd:M1 | yes | `-t` | canopy:plugins/tmux-resurrect/scripts/process_restore_helpers.sh |
| `send-keys -t[=] -X` | 3 | 3 | cs3 | oos:Mouse argv forms |  |  | container:list-keys |
| `send-prefix` | 3 | 3 | cs1 sc1 sf1 | cmd:M2 | yes |  | athome:home/dot_config/canopy/user.conf.tmpl, canopy:tmux/keys.tsv, container:list-keys (+1) |
| `set-buffer` | 10 | 9 | cs9 sc1 | area:Buffers and copy | yes |  | container:list-keys, plugin:tmux-yank/scripts/copy_pane_pwd.sh |
| `set-buffer -b` | 1 | 0 | sc1 | area:Buffers and copy | yes |  | plugin:tmux-fzf/scripts/clipboard.sh |
| `set-environment -g` | 7 | 0 | cj1 sc2 sf4 | area:Hooks and environment | yes |  | athome:home/dot_config/canopy/user.conf.tmpl, athome:home/dot_config/tmux/tmux.conf.tmpl, container:job (+2) |
| `set-environment -g -u` | 6 | 0 | cj1 sc5 | area:Hooks and environment | yes |  | container:job, plugin:opensessions/integrations/tmux-plugin/scripts/uninstall.sh, plugin:opensessions/opensessions.tmux |
| `set-environment -h` | 1 | 0 | sf1 | area:Hooks and environment | yes |  | plugin:tmux-which-key/plugin/init.tmux |
| `set-environment -t` | 2 | 0 | cc2 | area:Hooks and environment | yes | `-t` | container:cli |
| `set-hook -g` | 24 | 0 | cj12 cs10 sc2 | area:Hooks and environment | yes |  | container:job, container:show-hooks, manual:plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs (+1) |
| `set-hook -g -u` | 3 | 0 | sc3 | area:Hooks and environment | yes |  | manual:plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs, plugin:opensessions/integrations/tmux-plugin/scripts/uninstall.sh, plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs |
| `set-option -F -g` | 5 | 0 | sf5 | area:Options by tmux name | yes |  | plugin:tmux-which-key/plugin/init.tmux |
| `set-option -a -g` | 2 | 0 | sf2 | area:Options by tmux name | yes |  | canopy:tmux/conf.d/00-core.conf |
| `set-option -a -s` | 4 | 0 | sf4 | area:Options by tmux name | yes |  | canopy:tmux/conf.d/00-core.conf |
| `set-option -g` | 133 | 0 | cj15 cp8 sc28 sf82 | area:Options by tmux name | yes |  | athome:home/dot_config/canopy/user.conf.tmpl, athome:home/dot_config/themes/executable_apply-tmux.sh, athome:home/dot_config/tmux/executable_wk-keys-fix.sh (+11) |
| `set-option -g -q` | 21 | 0 | cc2 cj10 sc9 | area:Options by tmux name | yes |  | canopy:plugins/tmux-continuum/scripts/helpers.sh, canopy:plugins/tmux-resurrect/resurrect.tmux, canopy:plugins/tmux-resurrect/scripts/check_tmux_version.sh (+7) |
| `set-option -g -u` | 2 | 0 | sc2 | area:Options by tmux name | yes |  | manual:plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs, plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs |
| `set-option -p` | 1 | 0 | sc1 | area:Options by tmux name | yes |  | nvim:snacks.nvim/lua/snacks/image/terminal.lua |
| `set-option -p -t` | 16 | 0 | cp7 sc9 | area:Options by tmux name | yes |  | canopy:bin/canopy-agent, container:pane, manual:claude-code/TmuxBackend (agent teams, as tuios's TestClaudeCodeTeammateSequence pins it) |
| `set-option -s` | 2 | 0 | sc2 | area:Options by tmux name | yes |  | plugin:tmux-sensible/sensible.tmux |
| `set-option -t` | 4 | 0 | cj2 sc2 | area:Options by tmux name | yes |  | canopy:plugins/tmux-resurrect/scripts/restore.sh, container:job, plugin:tmux-claude-session-manager/scripts/launch.sh |
| `set-option -t -u` | 4 | 0 | cj3 sc1 | area:Options by tmux name | yes |  | canopy:plugins/tmux-resurrect/scripts/restore.sh, container:job |
| `set-window-option -g` | 6 | 0 | cj1 sc1 sf4 | area:Options by tmux name | **no** |  | athome:home/dot_config/canopy/user.conf.tmpl, canopy:tmux/conf.d/00-core.conf, container:job (+1) |
| `set-window-option -t` | 4 | 0 | cc1 sc3 | area:Options by tmux name | **no** |  | athome:home/dot_local/bin/executable_cockpit, container:cli, manual:plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs |
| `show-buffer -b` | 1 | 0 | sc1 | area:Buffers and copy | yes |  | plugin:tmux-fzf/scripts/clipboard.sh |
| `show-environment -g` | 15 | 0 | cj6 cp1 sc8 | area:Hooks and environment | yes |  | container:job, container:pane, plugin:opensessions/apps/tui/scripts/sessionizer.sh (+4) |
| `show-messages` | 1 | 1 | cs1 | area:Sessions and clients | yes |  | container:list-keys |
| `show-messages -J -T` | 2 | 0 | cp1 sc1 | area:Sessions and clients | yes |  | athome:home/dot_local/bin/executable_theme, container:pane |
| `show-options -g` | 3 | 0 | sc3 | area:Listing and inspection | yes |  | athome:home/dot_config/tmux/executable_net.sh, athome:home/dot_config/tmux/executable_wk-keys-fix.sh, nvim:snacks.nvim/lua/snacks/image/terminal.lua |
| `show-options -g -q` | 10 | 0 | cj9 sc1 | area:Listing and inspection | yes |  | container:job, plugin:vim-tmux-navigator/vim-tmux-navigator.tmux |
| `show-options -g -q -v` | 142 | 0 | cc9 cj101 cp6 sc26 | area:Listing and inspection | yes |  | canopy:bin/canopy-reboot-check, canopy:lib/tmux.sh, canopy:plugins/strategies/canopy_save_command.sh (+25) |
| `show-options -g -v` | 22 | 0 | cc5 cj10 cp1 sc6 | area:Listing and inspection | yes |  | athome:home/dot_config/tmux/executable_battery.sh, athome:home/dot_config/tmux/executable_wk-keys-fix.sh, canopy:bin/canopy-doctor (+5) |
| `show-options -g -v -w` | 1 | 0 | sc1 | area:Listing and inspection | yes | `-w` | plugin:tmux-yank/scripts/copy_line.sh |
| `show-options -g -w` | 1 | 0 | sc1 | area:Listing and inspection | yes | `-w` | plugin:tpm/scripts/helpers/tmux_echo_functions.sh |
| `show-options -p` | 4 | 0 | cj2 sc2 | area:Listing and inspection | yes |  | container:job, plugin:tmux-fzf/scripts/.fzf-tmux |
| `show-options -q -t -v` | 1 | 0 | sc1 | area:Listing and inspection | yes |  | plugin:tmux-claude-session-manager/scripts/picker.sh |
| `show-options -s -v` | 3 | 0 | cj2 sc1 | area:Listing and inspection | yes | `-s` | container:job, plugin:tmux-sensible/sensible.tmux |
| `show-window-options` | 2 | 0 | sc2 | area:Listing and inspection | **no** |  | plugin:tmux-fzf/scripts/.fzf-tmux |
| `show-window-options -t -v` | 9 | 0 | cc4 cj4 sc1 | area:Listing and inspection | **no** |  | canopy:plugins/tmux-resurrect/scripts/save.sh, container:cli, container:job |
| `source-file` | 10 | 3 | cj1 cp1 cs1 sc4 sf3 | cmd:M3 | yes |  | athome:home/.chezmoiscripts/run_onchange_18-install-canopy.sh.tmpl, canopy:lib/tmux.sh, canopy:tmux/keys.tsv (+8) |
| `source-file -q` | 5 | 0 | sf5 | cmd:M3 | yes | `-q` | athome:home/dot_config/tmux/tmux.conf.tmpl, canopy:tmux/tmux.conf |
| `split-window` | 1 | 1 | cs1 | cmd:M1 | yes |  | container:list-keys |
| `split-window -b -c -l -v` | 4 | 0 | sc4 | cmd:M1 | yes | `-b` | plugin:tmux-fzf/scripts/process.sh |
| `split-window -b -f -F -h -l -P -t` | 2 | 0 | cc1 sc1 | cmd:M1 | yes | `-b`, `-f`, `-F`, `-P` | container:cli, manual:plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs |
| `split-window -b -l -v` | 2 | 0 | sc2 | cmd:M1 | yes | `-b` | plugin:tmux-fzf/scripts/session.sh, plugin:tmux-fzf/scripts/window.sh |
| `split-window -c` | 1 | 0 | sc1 | cmd:M1 | yes |  | plugin:tmux-fzf/scripts/.fzf-tmux |
| `split-window -c -d -F -P -t` | 1 | 0 | cc1 | cmd:M1 | yes | `-d`, `-F`, `-P` | container:cli |
| `split-window -c -h` | 4 | 2 | cs2 sf2 | cmd:M1 | yes |  | canopy:tmux/keys.tsv, container:list-keys |
| `split-window -c -t` | 2 | 0 | sc2 | cmd:M1 | yes |  | canopy:plugins/tmux-resurrect/scripts/restore.sh |
| `split-window -c -v` | 4 | 2 | cs2 sf2 | cmd:M1 | yes |  | canopy:tmux/keys.tsv, container:list-keys |
| `split-window -d -F -h -l -P -t` | 1 | 0 | sc1 | cmd:M1 | yes | `-d`, `-F`, `-P` | manual:claude-code/TmuxBackend (agent teams, as tuios's TestClaudeCodeTeammateSequence pins it) |
| `split-window -f -F -h -l -P -t` | 1 | 0 | sc1 | cmd:M1 | yes | `-f`, `-F`, `-P` | manual:plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs |
| `split-window -h` | 4 | 4 | cs4 | cmd:M1 | yes |  | container:list-keys |
| `split-window -v` | 3 | 3 | cs3 | cmd:M1 | yes |  | container:list-keys |
| `start-server` | 5 | 0 | cj2 sc3 | area:Sessions and clients | yes |  | athome:home/.chezmoiscripts/run_onchange_after_14-install-tmux-plugins.sh.tmpl, container:job, plugin:tpm/scripts/helpers/plugin_functions.sh |
| `suspend-client` | 1 | 1 | cs1 | area:Sessions and clients | yes |  | container:list-keys |
| `swap-pane` | 3 | 3 | cs3 | area:Windows and panes | yes |  | container:list-keys |
| `swap-pane -D` | 5 | 4 | cs4 sc1 | area:Windows and panes | yes |  | container:list-keys, plugin:tmux-fzf/scripts/.fzf-tmux |
| `swap-pane -U` | 4 | 4 | cs4 | area:Windows and panes | yes |  | container:list-keys |
| `swap-pane -s` | 1 | 1 | cs1 | area:Windows and panes | yes |  | container:list-keys |
| `swap-pane -s -t` | 1 | 0 | sc1 | area:Windows and panes | yes |  | plugin:tmux-fzf/scripts/pane.sh |
| `swap-pane -t` | 2 | 0 | sc2 | area:Windows and panes | yes |  | plugin:tmux-fzf/scripts/.fzf-tmux |
| `swap-window` | 3 | 3 | cs3 | area:Windows and panes | yes |  | container:list-keys |
| `swap-window -s -t` | 1 | 0 | sc1 | area:Windows and panes | yes |  | plugin:tmux-fzf/scripts/window.sh |
| `swap-window -t` | 7 | 7 | cs7 | area:Windows and panes | yes |  | container:list-keys |
| `switch-client -T` | 8 | 6 | cj3 cs2 sc3 | area:Sessions and clients | yes |  | container:job, container:list-keys, plugin:opensessions/integrations/tmux-plugin/scripts/even-horizontal.sh (+4) |
| `switch-client -c -t` | 6 | 0 | cc1 cj1 sc4 | area:Sessions and clients | yes |  | container:cli, container:job, manual:plugin:opensessions/packages/runtime-rs/src/tmux_provider.rs (+2) |
| `switch-client -l` | 2 | 2 | cs1 sf1 | cmd:M2 | yes |  | canopy:tmux/keys.tsv, container:list-keys |
| `switch-client -n` | 5 | 4 | cs3 sc1 sf1 | cmd:M2 | yes |  | canopy:plugins/tmux-resurrect/scripts/restore.sh, canopy:tmux/keys.tsv, container:list-keys |
| `switch-client -p` | 4 | 4 | cs3 sf1 | cmd:M2 | yes |  | canopy:tmux/keys.tsv, container:list-keys |
| `switch-client -t` | 19 | 0 | cj6 sc13 | cmd:M2 | yes | `-t` | athome:home/dot_local/bin/executable_tsess, canopy:plugins/tmux-resurrect/scripts/process_restore_helpers.sh, canopy:plugins/tmux-resurrect/scripts/restore.sh (+8) |
| `switch-client -t[=]` | 2 | 2 | cs1 sf1 | oos:Mouse argv forms |  |  | athome:home/dot_config/canopy/user.conf.tmpl, container:list-keys |
| `tmux (bare)` | 1 | 0 | sc1 | area:Invocation |  |  | canopy:bin/canopy-reboot-check, nvim:snacks.nvim/lua/snacks/image/terminal.lua, nvim:vim-tmux-navigator/plugin/tmux_navigator.vim (+8) |
| `tmux -L` | 4 | 0 | cp2 sc2 | area:Invocation |  |  | canopy:lib/tmux.sh, container:pane |
| `tmux -L -f` | 2 | 0 | cp1 sc1 | area:Invocation |  |  | canopy:lib/tmux.sh, container:pane |
| `tmux -S` | 27 | 0 | cj6 sc21 | area:Invocation |  |  | canopy:plugins/tmux-resurrect/scripts/restore.sh, container:job, manual:nvim:vim-tmux-navigator/plugin/tmux_navigator.vim (+1) |
| `tmux -V` | 4 | 0 | cc1 cj1 cp1 sc1 | area:Invocation |  |  | canopy:bin/canopy-caps, canopy:bin/canopy-doctor, canopy:lib/tmux.sh (+10) |
| `tmux -c` | 1 | 0 | sc1 | area:Invocation |  | `-c` | plugin:tmux-fzf/scripts/menu.sh |
| `unbind-key` | 6 | 0 | sc5 sf1 | cmd:M3 | yes |  | canopy:tmux/conf.d/00-core.conf, plugin:opensessions/integrations/tmux-plugin/scripts/uninstall.sh, plugin:tmux-sensible/sensible.tmux |
| `unbind-key -T` | 5 | 0 | sc5 | cmd:M3 | yes | `-T` | plugin:opensessions/integrations/tmux-plugin/scripts/uninstall.sh |
| `unbind-key -n` | 2 | 0 | sc2 | cmd:M3 | yes | `-n` | plugin:opensessions/integrations/tmux-plugin/scripts/uninstall.sh |
| `unlink-window -k -t` | 1 | 0 | sc1 | oos:`link-window` |  |  | plugin:tmux-fzf/scripts/window.sh |
| `wait-for` | 1 | 0 | cp1 | area:Overlays | yes |  | container:pane |
| `wait-for -S` | 1 | 0 | cp1 | area:Overlays | yes |  | container:pane |

File codes: `sc` static-cli, `sf` static-conf, `cc` container-cli, `cp` container-pane, `cj` container-job, `cs` container-stored.

### Options by tmux name

Server, session, window and pane options the corpus sets or reads (`set`, `setw`, `show`, `showw`), with whether the spec names them anywhere (a text match: options live in section 5 as well as 4.5).

| Option | Uses | Named in the spec |
|---|---|---|
| `aggressive-resize` | 2 | yes |
| `allow-passthrough` | 2 | yes |
| `allow-rename` | 1 | yes |
| `automatic-rename` | 17 | yes |
| `automatic-rename-format` | 1 | no |
| `base-index` | 2 | yes |
| `command-alias` | 4 | yes |
| `command-alias[N]` | 5 | yes |
| `default-command` | 1 | no |
| `default-shell` | 1 | no |
| `default-terminal` | 5 | yes |
| `display-time` | 11 | yes |
| `escape-time` | 3 | yes |
| `extended-keys` | 2 | yes |
| `extended-keys-format` | 1 | yes |
| `focus-events` | 3 | no |
| `history-limit` | 3 | yes |
| `main-pane-width` | 1 | yes |
| `mode-keys` | 3 | yes |
| `mouse` | 1 | yes |
| `pane-active-border-style` | 1 | no |
| `pane-base-index` | 1 | yes |
| `pane-border-format` | 2 | no |
| `pane-border-style` | 1 | no |
| `prefix` | 3 | yes |
| `remain-on-exit` | 8 | yes |
| `renumber-windows` | 1 | yes |
| `set-clipboard` | 1 | yes |
| `status` | 1 | yes |
| `status-format[N]` | 1 | yes |
| `status-interval` | 4 | yes |
| `status-justify` | 1 | yes |
| `status-keys` | 2 | yes |
| `status-left` | 4 | yes |
| `status-left-length` | 2 | yes |
| `status-position` | 1 | yes |
| `status-right` | 6 | yes |
| `status-right-length` | 2 | yes |
| `status-style` | 1 | no |
| `synchronize-panes` | 3 | no |
| `terminal-features` | 4 | yes |
| `update-environment` | 2 | yes |
| `window-status-current-format` | 2 | yes |
| `window-status-format` | 2 | yes |
| `window-status-separator` | 2 | yes |
| `window-style` | 1 | no |

User options, grouped by prefix: `@batt*` (1), `@canopy_*` (15), `@claude_*` (15), `@continuum-*` (14), `@copy_*` (3), `@custom_*` (1), `@fzf-*` (9), `@net*` (1), `@opensessions-*` (6), `@opensessions_*` (3), `@override_*` (1), `@pet*` (1), `@pill_*` (6), `@resurrect-*` (33), `@sessionx-*` (84), `@theme_*` (26), `@tmux-*` (5), `@tmux_*` (1), `@tpm-*` (3), `@tpm_*` (2), `@vim_*` (8), `@wk_*` (17).

### Hooks

| Hook | Uses | Named in 4.5 |
|---|---|---|
| `after-kill-pane` | 2 | yes |
| `after-new-window` | 2 | yes |
| `after-resize-pane` | 2 | yes |
| `after-resize-window` | 2 | yes |
| `after-select-pane` | 2 | yes |
| `after-select-window` | 2 | yes |
| `client-resized` | 2 | yes |
| `client-session-changed` | 2 | yes |
| `pane-died` | 1 | yes |
| `pane-exited` | 1 | yes |
| `session-closed` | 2 | yes |
| `session-created` | 2 | yes |

### Format vocabulary

Variables, modifiers, short forms and styles found in corpus tokens (a regex scan, so nested conditions are counted per occurrence). *Named in 4.5* is a text match over the section.

| Token | Uses | Named in 4.5 |
|---|---|---|
| `#{@user_option}` | 146 | yes |
| `#{alternate_on}` | 1 | yes |
| `#{buffer_size}` | 3 | no |
| `#{client_last_session}` | 4 | yes |
| `#{client_name}` | 5 | yes |
| `#{client_prefix}` | 3 | yes |
| `#{client_session}` | 3 | yes |
| `#{client_termname}` | 1 | no |
| `#{client_tty}` | 22 | yes |
| `#{copy_cursor_word}` | 2 | yes |
| `#{cursor_y}` | 1 | yes |
| `#{history_bytes}` | 1 | no |
| `#{history_limit}` | 1 | no |
| `#{history_size}` | 4 | yes |
| `#{log_info}` | 2 | no |
| `#{mouse_any_flag}` | 6 | yes |
| `#{mouse_hyperlink}` | 6 | no |
| `#{mouse_line}` | 3 | no |
| `#{mouse_status_range}` | 4 | yes |
| `#{mouse_word}` | 9 | no |
| `#{pane_active}` | 24 | yes |
| `#{pane_at_left}` | 1 | no |
| `#{pane_current_command}` | 10 | yes |
| `#{pane_current_path}` | 36 | yes |
| `#{pane_dead}` | 4 | yes |
| `#{pane_floating_flag}` | 12 | no |
| `#{pane_height}` | 8 | yes |
| `#{pane_id}` | 67 | yes |
| `#{pane_in_mode}` | 14 | yes |
| `#{pane_index}` | 24 | yes |
| `#{pane_left}` | 10 | yes |
| `#{pane_marked_set}` | 6 | no |
| `#{pane_marked}` | 6 | no |
| `#{pane_mode}` | 10 | yes |
| `#{pane_pid}` | 15 | yes |
| `#{pane_right}` | 10 | no |
| `#{pane_search_string}` | 2 | no |
| `#{pane_start_command}` | 4 | yes |
| `#{pane_title}` | 36 | yes |
| `#{pane_tty}` | 18 | yes |
| `#{pane_width}` | 23 | yes |
| `#{pid}` | 2 | yes |
| `#{session_attached}` | 2 | no |
| `#{session_created}` | 2 | no |
| `#{session_grouped}` | 2 | yes |
| `#{session_group}` | 2 | yes |
| `#{session_id}` | 8 | yes |
| `#{session_name}` | 67 | yes |
| `#{session_path}` | 2 | no |
| `#{session_windows}` | 14 | yes |
| `#{socket_path}` | 61 | yes |
| `#{start_time}` | 2 | no |
| `#{version}` | 1 | yes |
| `#{window_active}` | 9 | yes |
| `#{window_flags}` | 5 | yes |
| `#{window_height}` | 1 | no |
| `#{window_id}` | 47 | yes |
| `#{window_index}` | 30 | yes |
| `#{window_layout}` | 2 | yes |
| `#{window_name}` | 15 | yes |
| `#{window_panes}` | 29 | yes |
| `#{window_width}` | 1 | no |
| `#{window_zoomed_flag}` | 7 | yes |
| `conditions on bare names` | 46 | yes |
| `escape ##` | 103 | yes |
| `job #(cmd)` | 10 | yes |
| `modifier !=:` | 13 | yes |
| `modifier &&:` | 38 | yes |
| `modifier ==:` | 23 | yes |
| `modifier >:` | 26 | yes |
| `modifier >=:` | 1 | yes |
| `modifier ?` | 80 | yes |
| `modifier b:` | 1 | yes |
| `modifier m/r:` | 10 | yes |
| `modifier m:` | 2 | yes |
| `modifier q:` | 18 | yes |
| `modifier \|\|:` | 6 | yes |
| `short #F` | 1 | no |
| `short #I` | 6 | yes |
| `short #P` | 1 | yes |
| `short #S` | 19 | yes |
| `short #W` | 15 | yes |
| `short #h` | 1 | yes |
| `style #[align` | 9 | yes |
| `style #[norange` | 2 | yes |
| `style #[range` | 2 | yes |

<!-- generated by scripts/corpus.py coverage: end -->
