# Linked windows and session groups (B1e)

tmux: a window can be linked into several sessions (one `winlink` per session,
all pointing at one `window`), and a session group shares one window list among
its members, each keeping its own current and last window. tmux-fzf needs
`link-window`/`unlink-window`, tmux-resurrect needs `new-session -t`.

## Model (smallest change)

Windows stay in `Model.windows`; a session still lists its windows as
`Session.windows` (ordered by that session's index), with its own `active` and
`last_windows`. Two additions:

- `Window.links: Vec<(SessionId, u32)>`: every link of the window, session and
  index, in tmux's `window->winlinks` order. It is the truth for "which index
  does this session know the window by".
- `Window.session` / `Window.index` stay, and mirror `links[0]` (the "first"
  link). Every flow that has no session of its own (a pane's options chain,
  hooks, events) keeps working on the first link. `Model::index_in(session,
  window)` is the per-session index; a window in one session is unchanged.
- `Model.groups: Vec<Group { name, members }>`. Created by `new-session -t`;
  kept with one member left (tmux: `session_grouped` stays 1), removed when empty.

A window may be linked to a session once. tmux allows the same window twice in
one session (`link-window -k` games); grofe refuses it ("window is already in
session"), because `Session.windows` and `active` are keyed by window id.

Mutation goes through four `Model` primitives that keep `links`, `Session.windows`
and the first-link mirror together: `add_link`, `remove_link`, `set_index`,
`sync_group`. Group sync copies tmux's `session_group_synchronize1`: the member's
links are dropped and re-added in the source's order (so `window_linked_sessions_list`
reorders as tmux's does), its current/last windows are kept by index, else the
lowest index becomes current.

## Commands

- `link-window -s SRC -t DST [-abdk]`: DST may have no index (lowest free).
  `-a`/`-b`: after/before DST's window, shuffling later windows up. An index in use
  is `index in use: N` unless `-k` (the old window is unlinked, destroyed if it was its
  last link). Same window at the index: `same index: N`. Without `-d` the window
  becomes current in DST's session. Fires `window-linked` once for DST's session.
- `unlink-window [-k] -t W`: `window only linked to one session` unless `-k` or the
  window has more links (a group counts once, like tmux's `references`). The last
  link with `-k` destroys the window and its panes. An emptied session is destroyed,
  with its whole group. Fires `window-unlinked`.
- `kill-window`, a pane exit: the window leaves every session (all links).
- `kill-session S`: drops S's links; a window with no link left dies. In a group the
  other members keep their windows. The group lives on with fewer members.
- `new-session -t T [-s name]`: T a session: join its group (made, named after T,
  when T has none); the new session gets T's windows (same indexes, no new window),
  current = lowest index. Name without `-s`: `<group>-<session id>`. `-n`/command with
  `-t` is `command or window name given with target`. T not a session: a normal new
  session in a new group named T. `-x/-y` are ignored.
- `new-window`, `link-window`, `unlink-window`, `move-window`, `swap-window`,
  `move-window -r`, `renumber-windows` end with `sync_group`.
- `move-window` = link into DST then unlink from the source *link*, so a linked
  window moved out of one session stays in the others. `-k` as for link-window.
- The source/target of these commands resolves to a (session, window) pair
  (`Model::resolve_winlink`), because `b:3` and `a:1` can be one window.

## Formats

`window_linked` = window has more than one link. `window_linked_sessions` = distinct
sessions-or-groups among the links (a group counts once, as tmux). `_list` = session
names in link order, comma separated. `session_grouped`, `session_group` (group name,
fixed at creation), `session_group_size`, `session_group_list` (member order),
`session_group_attached` (sum of members' clients), `_attached_list`,
`session_group_many_attached`. The group fields are empty for an ungrouped session.

## Events, hooks, persistence, rendering

- M6 events keep the first link's session for `window-*` events; `session-closed`
  fires per destroyed session. `window-linked`/`-unlinked` hooks carry `hook_window`
  and `hook_session` of the session acted on.
- Persistence (resurrect-style state): a group is saved by name and members; restore
  recreates it with `new-session -t`. Linked windows save once per link.
- Rendering: a client draws its own session's window list with `index_in`, so the
  status line of b shows the index b knows. Window size: `window-size latest` among
  sessions that have a client; the session whose client was active last sizes the
  window (so two clients of different sizes do not make a shared window flap).
  `aggressive-resize` is not implemented (spec: deferred).

## Deferred

Same window twice in one session; per-link alert flags (bell/activity live on the
window, so every session sees a flag); `aggressive-resize`; hooks per synced member.
