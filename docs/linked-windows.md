# Linked windows and session groups (B1e)

tmux: a window can be linked into several sessions (one `winlink` per session,
all pointing at one `window`), and a session group shares one window list among
its members, each keeping its own current and last window. tmux-fzf needs
`link-window`/`unlink-window`, tmux-resurrect needs `new-session -t`.

## Model (smallest change)

Windows stay in `Model.windows`; a session still lists its windows as
`Session.windows` (ordered by that session's index), with its own `active` and
`last_windows`. Two additions, both in `server/links.rs`:

- `Window.links: Vec<(SessionId, u32)>`: every link of the window, session and
  index, in tmux's `window->winlinks` order. The truth for "which index does this
  session know the window by" (`Model::index_in`).
- `Window.session` / `Window.index` stay and mirror `links[0]`. Flows with no
  session of their own (a pane's option chain, events, alert flags) keep using
  the first link. A window in one session is unchanged.
- `Model.groups: Vec<Group { name, members }>`, made by `new-session -t`. A group
  stays with one member (`session_grouped` stays 1, recorded), goes with the last.

A window is linked into a session once. tmux allows the same window twice in
one session; grofe refuses ("window is already in that session") because
`Session.windows` and `active` are keyed by window id.

Links change only through `add_link`, `remove_link`, `set_index` and
`detach_window` (tmux's `session_detach`), which keep `links`, `Session.windows`
and the first-link mirror together. `sync_group` is tmux's
`session_group_synchronize1`: after a change in one member, the others drop their
links and take the source's in its order (so `window_linked_sessions_list`
reorders as tmux's does), keeping current/last windows by index; a member whose
current index is gone first moves to its last window, else the one before it (the
last of all from the first), as `session_detach` would. A command's target resolves to a (session, window) pair
(`Model::resolve_winlink`), because `b:3` and `a:1` can be one window.

## Commands (all recorded in `golden/b1e`)

- `link-window -s SRC -t DST [-abdk]`: no index is the lowest free; `-a`/`-b` make
  room by moving later windows up; an index in use is `index in use: N` unless `-k`
  (the old link goes, its window dies with its last link; replacing the current
  window selects the new one even with `-d`); the same window there is `same index`.
  Without `-d` the window becomes current in DST.
- `unlink-window [-k] -t W`: `window only linked to one session` unless `-k` or it
  has another link. The last link with `-k` closes the window, without
  `renumber-windows` (recorded). An emptied session goes, with its whole group.
- `kill-window`, `kill-window -a`, a pane exit: the window leaves every session
  (with `renumber-windows` in each).
- `kill-session S`: drops S's links; a window with none left dies; the group lives on.
- `new-session -t T [-s name]`: T a session: join its group (made, named after T's
  group or T, if none), take its windows at their indexes, current = lowest. The name
  is `<group>-<id>`. The dropped window takes a window id and a pane id (recorded).
  `-n` or a command with `-t` is `command or window name given with target`; T no
  session: a plain session in a new group named T. `-x`/`-y` are ignored.
- `move-window` links into the new place then unlinks the old (`-abdk`; `-r` numbers
  one session). `swap-window` swaps the places (`-d` makes the destination place
  current, 3.7b's reading of the flag). Both sync the group. `new-window` and
  `respawn-window` in a member show in all.
- Between two sessions of one group `link-window`, `move-window` and `swap-window`
  say `sessions are grouped` (`can't move window, ...` for swap), after `-a`/`-b`
  have made room, as tmux does. `-a`/`-b` against an index with no window use the
  session's current window; with both, `-b` wins.
- `renumber-windows` after a kill numbers every member of the group from its own
  `base-index` (`server_renumber_session`); after `move-window` (without `-s`) it
  numbers the source session alone, and `move-window -r` the target alone, neither
  syncing the group. A group then differs until the next change syncs it from one
  member, as in tmux (the seeded goldens `rand*` record it).
- A mark (`select-pane -m`) names the session and window it was made in; it holds
  while that session has the window, and `M`, the reversed border and the toggle
  follow that link only.
- `unlink-window` without `-k` needs a link outside the session's group
  (`session_is_linked`); `kill-window` renumbers per session as tmux does.

## Formats

`window_linked` is more than one link; `window_linked_sessions` counts distinct
sessions or groups; `_list` the session names in link order. `session_grouped`,
`session_group`, `_size`, `_list`, `session_group_attached(_list)`, `_many_attached`
(clients of all members). The group fields are empty without a group. A format
sees the window as the session in play has it (`FormatCtx::in_session`): a
client's formats use the client's session, `list-windows`/`list-panes` the listed
session, `display-message -t` the target's.

## Events, hooks, persistence, rendering

- M6 events keep the first link's session. The `window-linked` and `window-unlinked`
  hooks do not exist yet. Persistence (M4) is not built; a save of linked windows or
  groups needs links and group names then (resurrect saves groups itself, tested in
  `acceptance/tp_12` case 6).
- A client draws its own session's window list under that session's indexes. A window
  shown to two sessions with clients has the size of the client that was active last
  (`window-size latest`; `apply_sizes` also runs on input when a session shares a
  window), so two clients do not make it flap. `aggressive-resize` is not built.

## Deferred

Same window twice in a session (tmux allows it: `link-window` or `move-window` of a
window the session holds, `swap-window` across sessions; tmux-fzf's link action
excludes the current session's windows by name, so only a window already linked
there under another session's name reaches it; a refactor of `Session.windows` and
`active` to winlink identity would be needed, so grofe refuses it); per-link alert
flags (bell and activity live on the window, so a window current in one session
and not in another shows its flag in both); `window-linked`/`-unlinked` hooks;
choose-tree shows a shared window under both sessions with one tag; a pane's
spawn environment and cwd come from the session the target names, the rest of its
session options from the first link.
