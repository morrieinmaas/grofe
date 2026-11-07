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
