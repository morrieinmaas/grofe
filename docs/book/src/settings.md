# Settings menu

The settings menu lists every option, key binding and layout in one place.
Open it with `prefix C` (in the tmux identity `prefix C` is still
customize-mode) or run `grofe settings` from a shell, which starts a server
when none is running. It needs a window of at least 50 by 10; a smaller one
says `window too small`.

## Sections

Press `Tab` and `Shift-Tab` to move between the sections: Appearance, Status
line, Keys, Layouts, Agents, Sessions and restore, Remote and Other. Every
option is in exactly one.

Typing searches all sections at once, by fuzzy match on a row's name,
description and value, best match first. `stl` finds `status-left` before
`status-left-length`.

Each option row shows its name, current value, where the config sets it and
its default. A row changed in this menu session is marked with `*`.

## Keys

| Key | What it does |
| --- | --- |
| `Enter` | Toggles a flag, cycles a choice, or asks for a number, string or colour. An invalid value shows the option's own error and changes nothing. |
| `Tab` | Next section. |
| `Esc` | Closes. With changes it asks `discard N changes? (y/n)`. |
| `C-s` | Shows the diff of `settings.toml` and asks before writing. |
| `C-d` | Unbinds the selected key in the Keys section. |

Typing goes to the search, so the actions use control keys.

## Preview and cancel

Every change applies to the running server at once. `Esc` then `y` hands each
option and key binding the menu changed back to the config: the config's value
or binding as it is now applies again, also when a `reload` changed it while
the menu was open. One that a runtime `set-option` or `bind-key` held before
the menu changed it goes back to that runtime value instead. Something another
client, another menu or a reload changed after the menu did is left alone.
A menu closed any other way than `C-s` does the same: when the client
detaches, or is resized below 50 by 10. A value or `replaces` prompt the menu
opened does nothing once the menu has closed.

## Saving

`C-s` shows the exact change to `settings.toml`. `y` writes it and reloads
the config; `n` goes back to the menu and writes nothing. If the reload
fails, the file is put back as it was and the error is shown.

`settings.toml` is the last layer: it overrides your dotfiles, including
`config.toml`, `conf.d/` and `user.toml`, which grofe never writes. When it
sets an option that a lower layer also sets, the row says
`overrides <file>:<line>`, so you can see what the saved value hides. To drop
an override, reset the setting in customize-mode (`d`); the key leaves
`settings.toml` and the lower layer's value applies again. In customize-mode,
`P` saves the selected option or key to `settings.toml`; see
[Configuration](configuration.md).

## Keys section

The Keys section lists bindings of the prefix and root tables with the table,
key, command and source. `Enter` waits for one key press and moves the
selected command to that key. If the key is bound, the menu shows
`replaces <command>` and needs `y`. `C-d` unbinds. `Esc` cancels the wait.

## Layouts section

The Layouts section lists the files in `layouts/` next to your config, with a
drawing of the first window's panes beside the list. `Enter` sets
`default-layout` to the selected layout; save it with `C-s`.

With `default-layout` set, `new-session` without a command builds that
layout from the `layouts/` folder next to the server's config file. An
unknown name makes a plain session; the `grofe` command line
prints `grofe: default-layout: no layout <name>` and logs it for
`show-messages`. The command line builds the layout, not the server, so a
session the server makes by itself, such as from a key binding, stays plain. `-P` and `-A`
also make a plain session.
