# Roadmap

What is done lives in the README's Status section; the finished milestone
plans and specs are in `docs/archive/`. This page lists what is open, from the
codebase-health review of 2026-11-15. Each item is one branch and one PR.

## Quality

- **Test reliability.** Replace fixed sleeps in tests with a polling
  `wait_until` helper; fix the two known flaky tests at their root cause.
- **Shared helpers.** One implementation each of strftime, glob matching,
  `/bin/sh -c` spawning, shell quoting and character width.
- **Dead code and stale comments.** Remove what nothing calls; fix comments
  that name tasks instead of behaviour.
- **Safety on the actor.** No blocking calls on the server's single thread.
- **Fuzzing the parsers.** Config, command, format and key parsers.
- **Smaller files.** Split the largest modules along their seams.
- **One source of truth for options.** One table drives the schema, the
  defaults, the tmux names and the config reference.
- **Table-driven command parser.** One row per verb instead of a hand-written
  parser each.
- **Explicit per-command context.** Per-request state moves off the shared
  server state.

## tmux compatibility

- **Keys, targets, format variables and inert options** that scripts rely on
  and grofe lacks.
- **Control mode, the rest.** `-C` and `-CC` work as a client-side bridge
  (`docs/specs/2026-11-15-control-mode.md`) with every notification and
  `refresh-client -B`; left are a server-side client kind (which removes the
  divergences the spec lists) and an iTerm2 end-to-end check.

## Features

- **Copy-mode hints.** Label URLs, paths, hashes and addresses on screen and
  copy one with two keystrokes.

## Follow-ups from the health work

Found while landing the items above; each is small.

- **Line numbers in `source-file` runtime errors.** Parse errors carry
  `file:line`; errors raised while a parsed command runs do not.
- **Shared-memory SIGBUS.** A pane that truncates its graphics shm file under
  the server can fault the reader; map defensively or validate the size first.
- **`:{last}` with no previous window** should resolve to the current window,
  as tmux does, instead of failing.
- **Process info off the actor.** `list-panes` still reads per-pane process
  info on the server thread.
- **Control-mode notifications.** `%subscription-changed`,
  `%pane-mode-changed`, `%paste-buffer-*`, `%message` and `%client-*`; send
  the closing ST on SIGHUP and SIGTERM too.
- **Glob edge case.** An unterminated `[` matches literally; macOS fnmatch
  differs, so pick one and test it.
- **Word splitting.** The splitter treats form feed as whitespace; tmux splits
  on space and tab only.
- **`compat_corpus.bats` test 8** clones a corpus repo from a personal
  account; make the source configurable or vendor the corpus.
