# Settings menu, config path and write-back

## Goal

- Let people change every grofe option, key and default layout from an fzf-style menu, with the dotfiles staying the source of truth.

## Decisions

- Three PRs, in order: (1) config path, (2) write-back, (3) settings menu.
- `--config <file>` and `GROFE_CONFIG` replace `config.toml`. The config dir becomes that file's folder, so `conf.d/`, `user.toml`, `settings.toml`, `layouts/`, packs and `init.luau` come from there.
- The menu writes only `settings.toml`, a new last layer after `user.toml`. Hand-written and chezmoi-managed files are never written.
- The override is documented in the book and shown in the menu: a row that settings.toml sets over a lower layer says `overrides <file>:<line>`; reset removes the override.
- `toml_edit` is added, so writes keep comments and key order.
- The menu is a native server overlay that reuses `listview` (fuzzy), the key menu's drawing and customize-mode's option and key data. `grofe settings` starts a server when none runs.
- Preview applies live. Apply shows the TOML diff, writes on `y` and reloads. Cancel restores the values captured at open. Customize-mode gets the same save.
- `prefix C` opens settings in grofe identity; tmux identity keeps customize-mode on `C`.
- canopy moves its plain new-window from `C` to `M-c` (a canopy PR).
- New `default-layout` option: `new-session` without a command builds that layout. The menu's layout picker draws a preview.
- The book gets a Settings page covering the menu and customize-mode.
- Minimise bloat: shared code is refactored and reused, not copied; each PR states its net line change.

## Acceptance criteria

### PR 1: config path

- AC1.1 `grofe --config <file> <cmd>` makes a server it starts load `<file>` in place of `config.toml`.
- AC1.2 `GROFE_CONFIG=<file>` does the same; `--config` wins over `GROFE_CONFIG`, which wins over the XDG default.
- AC1.3 With a custom file, `conf.d/`, `user.toml` and `layouts/` are read from the custom file's folder, not `~/.config/grofe`.
- AC1.4 A custom file that does not exist loads as empty (built-in defaults), with no error.
- AC1.5 A relative path is resolved against the client's working directory, and a leading `~/` against `$HOME`; the server stores the absolute path.
- AC1.6 `reload` re-reads the same custom file the server started with.
- AC1.7 `grofe show-config` and `show-config --explain` honour `--config` and `GROFE_CONFIG` and name the custom file as the source.
- AC1.8 A client with `--config` that reaches a running server started with another config prints `grofe: server uses <path>` on stderr and carries on.
- AC1.9 `tmux -f` keeps its current behaviour.

### PR 2: write-back

- AC2.1 `settings.toml` loads after `user.toml`; a key set in both takes the settings.toml value.
- AC2.2 `show-config --explain` names `settings.toml` for keys it sets.
- AC2.3 Saving an option writes its TOML key into settings.toml and keeps every comment, blank line and other key byte for byte.
- AC2.4 Saving a binding writes `[keys.<table>] <key> = { cmd = ... }`; saving an unbind writes `<key> = ""`.
- AC2.5 A missing settings.toml or config folder is created on the first save.
- AC2.6 The write is atomic (temp file plus rename). When the reload after it fails, the old bytes are restored and the error is shown.
- AC2.7 Resetting a key removes it from settings.toml, and the lower layer's value applies again after the reload.
- AC2.8 In customize-mode, `P` (persist; `S` is tmux's set-global) saves the selected option or key to settings.toml.
- AC2.9 An option with no TOML key is shown as not saveable and `P` says so.

### PR 3: settings menu

- AC3.1 The `settings` command opens the menu overlay on the client; `prefix C` runs it in grofe identity; tmux identity's `C` is still customize-mode.
- AC3.2 `grofe settings` from a shell attaches to the server, starting one when none runs, and opens the menu.
- AC3.3 Sections are Appearance, Status line, Keys, Layouts, Agents, Sessions and restore, Remote and Other. Every option is in exactly one section.
- AC3.4 Typing filters rows across all sections by fuzzy match on name, description and value, best match first.
- AC3.5 Each row shows name, current value, default and source file; a row changed in this menu session is marked; a settings.toml value over a lower layer shows `overrides <file>:<line>`.
- AC3.6 Enter on a flag toggles it, on a choice cycles it, and on a number, string or colour opens a prompt; an invalid value is refused with the option's error and nothing changes.
- AC3.7 Every change applies to the running server at once (preview).
- AC3.8 Esc with no changes closes. Esc with changes asks `discard N changes? (y/n)`; `y` restores every value and binding captured at open.
- AC3.9 `C-s` shows the exact settings.toml diff; `y` writes and reloads, `n` returns to the menu.
- AC3.10 In Keys, Enter waits for a key press and rebinds the selected command to it; a key that is already bound shows `replaces <command>` and needs `y`; `C-d` unbinds (the query is typed live, so a plain `d` would search).
- AC3.11 In Layouts, each file in `layouts/` is a row with a drawn preview of its first window's panes; Enter sets `default-layout`.
- AC3.12 With `default-layout` set, `new-session` without a command builds that layout; an unknown name creates a plain session and shows `default-layout: no layout <name>`.
- AC3.13 The book has a Settings page, the configuration page covers `--config`, `GROFE_CONFIG` and the settings.toml layer, and the options reference lists `default-layout`.

## Edge cases

- Empty: no config files at all; an empty settings.toml; a layouts folder with no files; a query that matches nothing shows `no matches`.
- Max: all 153 options plus every binding in the list scroll without lag; a 200-line hand-written config.toml keeps every byte when settings.toml is written.
- Invalid: `--config` names a directory; settings.toml has a syntax error at start (defaults plus a logged error, as for other layers); a colour like `#zzz`; a key name that does not parse.
- Concurrent: two clients open the menu and both apply; settings.toml edited by hand while the menu is open (the write re-reads the file first); a reload during an open menu.
- Unicode: a query and option values with non-ASCII text (`世界`); a path with spaces and non-ASCII characters.
- Small window: the menu on an 80x24 client, and on a client too small to draw it (it says so and closes).

## Test list

- T1 (e2e) `--config /tmp/x/a.toml` with `status-left = "A"`: `display -p '#{status-left}'` prints `A`. AC1.1
- T2 (e2e) `GROFE_CONFIG` names a file with `status-left = "B"` and `--config` one with `"C"`: prints `C`; without `--config` prints `B`. AC1.2
- T3 (e2e) custom dir holds `conf.d/10.toml` with `status-right = "D"`: prints `D`; `~/.config/grofe/conf.d` is ignored. AC1.3
- T4 (e2e) `--config /tmp/none/x.toml`: the server starts and `status-left` is the built-in default. AC1.4
- T5 (unit) `resolve("cfg/a.toml", cwd="/w", home="/h")` gives `/w/cfg/a.toml`; `"~/a.toml"` gives `/h/a.toml`. AC1.5
- T6 (e2e) edit the custom file, run `reload`: the new value shows. AC1.6
- T7 (e2e) `show-config --explain status-left` with `--config` names the custom file. AC1.7
- T8 (e2e) a server started with file A, then a client with `--config B`: stderr has `server uses A`. AC1.8
- T9 (e2e) the existing `tmux -f` tests pass unchanged. AC1.9
- T10 (unit) user.toml `status-left="U"` and settings.toml `"S"`: the loaded value is `S`. AC2.1
- T11 (unit) the explain output for that key names settings.toml. AC2.2
- T12 (unit) a fixture with comments, blank lines and three keys: `set("status-left","X")` changes one line, and every other byte is equal. AC2.3
- T13 (unit) save a binding `prefix C-t` to `new-window`, then an unbind of `prefix %`: the file has both lines in `[keys.prefix]`. AC2.4
- T14 (unit) save into a missing dir: the dir and file are created. AC2.5
- T15 (unit) a reload that fails after a write: the file bytes equal the old bytes and the error is returned. AC2.6
- T16 (e2e) set over user.toml, then reset: the key is gone from settings.toml and the user.toml value shows. AC2.7
- T17 (e2e) customize-mode: change `status`, press `P`: settings.toml has `status.lines = 3`. AC2.8
- T18 (unit) `P` on an option with no TOML key returns `not saveable`. AC2.9
- T19 (e2e) `prefix C` in grofe identity opens the menu; in tmux identity it opens customize-mode. AC3.1
- T20 (e2e) `grofe settings` with no server running starts one and the menu is drawn. AC3.2
- T21 (unit) every option in the table maps to exactly one section. AC3.3
- T22 (unit) query `stl` ranks `status-left` above `status-left-length`. AC3.4
- T23 (unit) a row's render for an option set in settings.toml over conf.d shows `overrides conf.d/10.toml:3`. AC3.5
- T24 (e2e) Enter on `status` toggles it; entering `abc` for `history-limit` shows the error and the value is unchanged. AC3.6, AC3.7
- T25 (e2e) change two options, Esc, `y`: both values equal the values at open. AC3.8
- T26 (e2e) change one option, `C-s`: the diff names it; `y` writes settings.toml and the value survives `reload`. AC3.9
- T27 (e2e) Keys: rebind `new-window` to `C-t`; then rebind to `%` shows `replaces split-window -h` and needs `y`; `C-d` unbinds it. AC3.10
- T28 (unit) the layout preview of a two-pane vertical split draws two boxes side by side. AC3.11
- T29 (e2e) `default-layout = "dev"` with `layouts/dev.toml` of two panes: `new-session -d` makes a session with two panes; `"nope"` makes one pane and the message. AC3.12
- T30 (e2e) the existing docs test (`just docs` options reference) passes with `default-layout`. AC3.13
- T31 (unit) a query that matches nothing shows `no matches`. Edge: empty.
- T32 (unit) a 200-line fixture keeps every untouched byte after a save. Edge: max.
- T33 (unit) `--config` on a directory exits 1 with `grofe: --config: <path> is a directory`. Edge: invalid.
- T34 (unit) settings.toml edited between open and apply: apply re-reads it and keeps the hand edit. Edge: concurrent.
- T35 (unit) query `世界` matches a value with `世界`; a config path with spaces and `ü` loads. Edge: unicode.
- T36 (e2e) the menu on a 20x5 client shows `window too small` and closes. Edge: small window.

## Out of scope

- Editing Lua, packs or `init.luau` from the menu.
- Syncing settings.toml to chezmoi, or a "copy as TOML" action.
- Mouse input in the menu.
- Editing remote machines' configs.
- Moving canopy's `C`: a separate canopy PR, tracked here only.
