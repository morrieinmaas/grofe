# Day-to-day tasks. `just` lists them.

state_dir := env("XDG_STATE_HOME", env("HOME") / ".local/state") / "grofe"
config_dir := env("XDG_CONFIG_HOME", env("HOME") / ".config") / "grofe"

default:
    @just --list

# Install a release build of grofe on PATH (~/.cargo/bin), under its own name
install:
    cargo install --path . --locked

# Convert your tmux.conf into grofe's user.toml (refuses to overwrite one)
import-tmux tmux_conf=(env("HOME") / ".config/tmux/tmux.conf"):
    @test ! -e "{{ config_dir }}/user.toml" || { echo "{{ config_dir }}/user.toml exists; move it first" >&2; exit 1; }
    mkdir -p "{{ config_dir }}"
    grofe import-tmux "{{ tmux_conf }}" > "{{ config_dir }}/user.toml"

# Attach to the running grofe server, or start one (run outside tmux)
try:
    @test -z "${TMUX:-}" || echo "note: you are inside tmux; open a fresh terminal window for a clean trial" >&2
    grofe attach || grofe

# Follow the server log of the default instance
log instance="default":
    tail -F "{{ state_dir }}/{{ instance }}.log"

# List panic reports and today's compat misses
reports:
    -ls -1t "{{ state_dir }}"/*.panic.*.log 2>/dev/null
    -tail -n 20 "{{ state_dir }}/compat-miss.log" 2>/dev/null

# The checks CLAUDE.md requires before a commit
check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test
    bats tests/cli.bats

# The whole CI workflow locally (Linux in a container, then macOS natively)
ci *legs:
    scripts/ci-local.sh {{ legs }}

# The release gate (TE.3); needs canopy and athome clones
gate canopy athome:
    GROFE_E2E_CANOPY_REPO="{{ canopy }}" GROFE_E2E_ATHOME_REPO="{{ athome }}" scripts/release-gate.sh

# Regenerate CHANGELOG.md, naming the unreleased changes <version> (git-cliff)
changelog version:
    git cliff --config cliff.toml v0.1.0..HEAD --tag "v{{ trim_start_match(version, "v") }}" -o CHANGELOG.md

# Bump to <version>, regenerate the changelog, commit and tag locally (never pushes)
release version:
    #!/usr/bin/env bash
    set -euo pipefail
    v="{{ trim_start_match(version, "v") }}"
    [[ "$v" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "release: $v is not x.y.z" >&2; exit 1; }
    [ -z "$(git status --porcelain)" ] || { echo "release: the working tree is not clean" >&2; exit 1; }
    ! git rev-parse -q --verify "refs/tags/v$v" >/dev/null || { echo "release: tag v$v exists" >&2; exit 1; }
    awk -v v="$v" '!done && /^version = "/ { $0 = "version = \"" v "\""; done = 1 } 1' Cargo.toml >Cargo.toml.new
    mv Cargo.toml.new Cargo.toml
    cargo update --workspace --offline
    just changelog "$v"
    git add Cargo.toml Cargo.lock CHANGELOG.md
    git commit -m "chore(release): v$v"
    git tag -a "v$v" -m "grofe v$v"
    echo "Tagged v$v. Publish it with: git push origin HEAD \"v$v\""

# Build a .deb for a target triple into target/dist (cargo-deb)
deb target="x86_64-unknown-linux-musl":
    cargo build --release --locked --target {{ target }}
    cargo deb --target {{ target }} --no-build -o target/dist/

# Build a .rpm for a target triple into target/dist (cargo-generate-rpm)
rpm target="x86_64-unknown-linux-musl":
    cargo build --release --locked --target {{ target }}
    cargo generate-rpm --target {{ target }} -o target/dist/

# Render the Homebrew formula and AUR PKGBUILD for an existing release into target/pkg
pkg-render version:
    packaging/render.sh {{ version }} target/pkg

# The ssh remote-session scenarios against sshd containers (podman or docker)
ssh-e2e *filter:
    scripts/ssh-e2e.sh {{ filter }}
