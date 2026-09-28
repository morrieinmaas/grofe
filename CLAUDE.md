# grofe: agent conventions

Conventions for agents implementing tasks from `docs/plans/2026-09-23-milestones.md`.

## Before starting a task

- Read the spec (`docs/specs/2026-09-23-grofe-design.md`) and the task's row in the
  plan before writing code.
- File ownership per the plan's "Owns" column is a hard boundary: a task edits
  only the files it owns. If you need a dependency that isn't already in
  `Cargo.toml`, report it as missing rather than editing `Cargo.toml` yourself.
- Contract names in the plan are binding; to change one, stop and report.

## Checks that must pass

- `cargo fmt` (CI checks `cargo fmt --check`)
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `bats tests/cli.bats`
- `scripts/ci-local.sh` runs the whole CI workflow locally (Linux jobs in a
  container via `act`, then the macOS leg natively). It is the CI gate while
  GitHub Actions is unavailable.

## Testing

- Test first.
- Integration tests start a server on a unique `-S` socket; use the harness in
  `tests/common`.

## Real tmux

Never run `tmux` against the default server. Your shell is inside the user's live
tmux session, and `tmux kill-session` with no target kills it along with every agent
running there. To compare against real tmux, always pass a private socket,
`tmux -L grofe-<task> ...`, and clean up with `tmux -L grofe-<task> kill-server`. A
global hook blocks `tmux kill-*` without `-L`/`-S`, but `set-option`, `new-session`
and the rest would still change the live server.

## Code

- No `unwrap()`/`expect()` on socket or pty paths in non-test code.
- Keep it simple: no speculative abstractions. If a deliberate simplification
  is made, mark it with a `// ponytail:` comment naming the ceiling it hits.

## Commits

- Conventional commits: `type(scope): description`.
- Never an em dash as a sentence break, in prose, comments or commits.

## Publishability

- No hostnames, emails, or machine specifics in shipped files.
