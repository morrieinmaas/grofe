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
- **`-f` through the `tmux` shim** loads a tmux.conf, as tmux does; the native
  `grofe -f` keeps taking TOML.
- **Control mode** (`-C` and `-CC`), the largest remaining gap for editor
  plugins and terminal integrations.

## Features

- **Copy-mode hints.** Label URLs, paths, hashes and addresses on screen and
  copy one with two keystrokes.
