# Trying grofe as tmux on macOS

`scripts/mac-trial.sh` lets you use grofe as your tmux for a few days, with
your own keys, plugins and status bar, in one terminal window. Your live
tmux, canopy and config keep running untouched in every other window.

## What it does

`mac-trial.sh setup` makes a trial root (default `~/.local/share/grofe-trial`,
or `--root <dir>`) and fills it:

| In the root | What it is |
|---|---|
| `bin/grofe`, `bin/tmux -> grofe` | a copy of a release build (or of `--grofe <binary>`), and the name that gives it tmux identity |
| `config/` | `XDG_CONFIG_HOME`: copies of your tmux dir (without TPM's `plugins/`) and canopy's config dir, grofe's config, and a link to every other entry of `~/.config` |
| `data/` | `XDG_DATA_HOME`: a copy of canopy's store, and links to the rest of `~/.local/share` |
| `state/` | `XDG_STATE_HOME`: grofe's logs and `compat-miss.log`, canopy's state and resurrect saves, and links to the rest of `~/.local/state` |
| `run/`, `tmux-tmp/` | a private `XDG_RUNTIME_DIR` (grofe's socket) and `TMUX_TMPDIR` |
| `zdotdir/` | zsh wrappers that source your own dotfiles, then put `bin/` first on `PATH` again |
| `info/`, `reports/` | what setup found, and the reports you collect |

grofe's config there is three layers: a generated `config.toml` that points
canopy at its copies, canopy's fragments from `canopy grofe generate`, and a
`user.toml` (see below). In the copies, the paths to your tmux dir and to
canopy are rewritten to the copies, and a script that sets its own `PATH`
gets `bin/` in front of it. Your trial-only settings go in
`config/grofe/conf.d/99-trial.toml`, which setup never overwrites.

`mac-trial.sh start` runs setup if it has not run yet, then replaces the
current shell with a login shell in the trial's environment that starts or
attaches `tmux -L grofe-trial new-session -A -s trial`.

## What it never touches

- `~/.config` (tmux, canopy, grofe or anything else), `~/.local/share/canopy`,
  `~/.local/state/canopy`, `~/.local/state/grofe`. The script reads them and
  writes only inside the root. It refuses a root that is, contains, or sits
  inside any of them, `$HOME`, `~/Library`, `/tmp` or `$TMPDIR`.
- Your live tmux server and its sockets: grofe's socket is in the root's
  `run/`, and `TMUX_TMPDIR` points into the root too.
- launchd and login items: continuum adds or removes a login agent every
  time it loads, so in the trial's copy of canopy those scripts do nothing.
  The trial installs no agent and no service.
- Your clipboard settings. Copying puts text on the real clipboard; that is
  one of the things to test.

What the links mean: programs you run inside the trial (nvim, gh, mise, the
theme picker on `prefix T`) still use your real config through them, and
write to it as they would outside the trial. `HOME` stays your own.

## Before you start

You need two things the live setup does not have yet:

1. **A canopy with the grofe backend** (`bin/canopy-grofe`). The canopy in
   `~/.local/share/canopy` may predate it; setup says so and stops. Point
   `--canopy` at a checkout at the commit in
   `tests/e2e/alias-day/pins.env` (`CANOPY_SHA`). It is copied, never used
   in place.
2. **A `user.toml`**, your `canopy/user.conf` in grofe's format. The best one
   is the hand-ported file on athome's `grofe-config` branch, rendered with
   your own chezmoi data:

   ```sh
   git -C <athome clone> show grofe-config:home/dot_config/grofe/user.toml.tmpl |
     chezmoi execute-template > ~/grofe-trial-user.toml
   ```

   Pass it with `--user-toml ~/grofe-trial-user.toml`. Without the flag,
   setup imports your `user.conf` with `grofe import-tmux`, and
   `info/user-toml.txt` lists every line that did not come across.

Then, from any shell that is not inside tmux:

```sh
scripts/mac-trial.sh setup --canopy <canopy checkout> --user-toml ~/grofe-trial-user.toml
```

`--grofe <binary>` skips the build; `--offline` skips downloading canopy's
plugin binaries (opensessions and fzf-url stay dormant without them).

## Starting it

**Ghostty.** Open one new window (`Cmd-N`), so the trial is not nested in your
live tmux, and run:

```sh
~/path/to/grofe/scripts/mac-trial.sh start
```

The window is now the trial. `prefix d` detaches and closes it; run `start`
again in a new window to come back. Every other window is your normal setup.

**WezTerm.** The same, in one new WezTerm window. WezTerm speaks the kitty
keyboard protocol only with `config.enable_kitty_keyboard = true` in
`wezterm.lua`; without it, keys like `Ctrl-Enter` arrive in the old encoding.
Both terminals can run the trial at once: they attach to the same session.

`mac-trial.sh status` prints the versions, whether the server is running,
and how many lines `compat-miss.log` has.

## Daily checklist

Note anything that differs from your tmux in `docs/dogfood.md`'s entry
format, with the terminal it happened in.

- [ ] **Your keys:** the prefix, splits, pane moves and resizes, `prefix c`'s
  layout, `prefix r` reload, and your own tools (`g`, `G`, `P`, `T`, `W`, `K`,
  `?`, `a`, `A`, `s`, `u`).
- [ ] **Copy mode and the clipboard:** `prefix [`, select with `v`, copy with
  `y`, then paste in another app with `Cmd-V` (tmux-yank through `pbcopy`). Copy
  in a pane over ssh too, which needs OSC 52 (`set-clipboard on`); Ghostty may
  ask once whether to allow clipboard writes.
- [ ] **Popups and menus:** gh-dash, the pet picker, the network popup, the key
  list, which-key, sessionx.
- [ ] **Resurrect:** `prefix M-s` saves (into the trial's
  `state/canopy/resurrect`); after `tmux kill-server` inside the trial and a
  fresh `start`, continuum should bring the layout back.
- [ ] **Images in Ghostty and kitty:** `kitten icat <image>` works with no
  setup (Unicode placeholders). For direct placements and animation, add
  `graphics = true` to `config/grofe/conf.d/99-trial.toml`, press `prefix r`,
  and try `kitten icat` and an image preview in nvim or yazi again.
- [ ] **Sixel in WezTerm:** with `graphics = true`, `chafa -f sixel <image>` or
  `img2sixel <image>`.
- [ ] **Mouse:** click to focus, drag borders, wheel into copy mode, drag to
  copy, click a window in the status bar and the network pills.
- [ ] **Detach and attach:** `prefix d`, then `start` in a new window; attach
  from Ghostty and WezTerm at the same time.
- [ ] **Sleep and wake:** close the lid with the trial attached; after waking,
  panes still run, the clock and battery update, input still works.
- [ ] **At the end of the day:** `mac-trial.sh status`, and a look at
  `state/grofe/compat-miss.log`.

If something blocks you, detach and carry on in a normal window: your live
tmux was never stopped. Inside the trial, `tmux` is always grofe; a real tmux
started there would use the trial's `TMUX_TMPDIR`, not your live server.

## Reporting

```sh
scripts/mac-trial.sh report
```

writes `reports/report-<time>.txt` in the root: grofe's and canopy's
versions, macOS, the terminals you started the trial in, the config check,
the import report, `compat-miss.log`, grofe's server log and panic reports,
and canopy's plugin log. It holds no environment dump; your home is shown as
`~`, the host name is masked, and values that look like tokens, keys or
passwords are replaced. Read it before you share it.

## Removing everything

```sh
scripts/mac-trial.sh reset
```

asks for `yes`, stops the trial's server if it runs, and deletes the root. It
deletes only a directory setup made (one holding `.grofe-trial`), and nothing
outside it existed to begin with, so that is all there is to remove. Pass the
same `--root` you used for setup.
