# grofe: agent conventions

Conventions for agents implementing tasks from `docs/plans/2026-09-23-milestones.md`.

## Before starting a task

- Read the spec (`docs/specs/2026-09-23-grofe-design.md`) and the task's row in the
  plan before writing code.
- File ownership per the plan's "Owns" column is a hard boundary: a task edits
  only the files it owns. If you need a dependency that isn't already in
  `Cargo.toml`, report it as missing rather than editing `Cargo.toml` yourself.

## Checks that must pass

- `cargo fmt`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `bats tests/cli.bats`

## Testing

- Test first.
- Integration tests start a server on a unique `-S` socket; use the harness in
  `tests/common`.

## Code

- No `unwrap()`/`expect()` on socket or pty paths in non-test code.
- Keep it simple: no speculative abstractions. If a deliberate simplification
  is made, mark it with a `// ponytail:` comment naming the ceiling it hits.

## Commits

- Conventional commits: `type(scope): description`.
- Never an em dash as a sentence break, in prose, comments or commits.

## Publishability

- No hostnames, emails, or machine specifics in shipped files.
