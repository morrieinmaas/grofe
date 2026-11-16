#!/usr/bin/env bats
# CLI smoke suite (spec §9): drives the built binary with a temp HOME and no
# GROFE* variables in the environment, as a canopy-style shell script would.

BIN="${GROFE_BIN:-$(cd "$(dirname "$BATS_TEST_FILENAME")/.." && pwd)/target/debug/grofe}"

setup() {
    # A short path: unix socket paths are limited to ~104 bytes on macOS.
    TMP="$(mktemp -d /tmp/grofe-bats.XXXXXX)"
    export HOME="$TMP/home"
    mkdir -p "$HOME"
    unset XDG_CONFIG_HOME XDG_STATE_HOME XDG_RUNTIME_DIR
    for v in $(compgen -e | grep '^GROFE' || true); do
        unset "$v"
    done
    SOCK="$TMP/s.sock"
    cd "$HOME"
}

teardown() {
    "$BIN" -S "$SOCK" kill-server >/dev/null 2>&1 || true
    rm -rf "$TMP"
}

g() {
    "$BIN" -S "$SOCK" "$@"
}

# Polls until pane $1's capture has a line exactly equal to $2 (5 s max).
wait_for_line() {
    for _ in $(seq 50); do
        g capture-pane -p -t "$1" | grep -qx -- "$2" && return 0
        sleep 0.1
    done
    echo "no line '$2' in pane $1:" >&2
    g capture-pane -p -t "$1" >&2
    return 1
}

@test "bare grofe means new-session, as bare tmux does" {
    # No terminal here, so new-session stops where tmux's does.
    run "$BIN" -S "$SOCK" </dev/null
    [ "$status" -eq 1 ]
    [ "$output" = "open terminal failed: not a terminal" ]
}

@test "--version and -V print grofe's version" {
    version="$(grep -m1 '^version' "$(dirname "$BATS_TEST_FILENAME")/../Cargo.toml" | cut -d'"' -f2)"
    for flag in --version -V; do
        run "$BIN" "$flag"
        [ "$status" -eq 0 ]
        [ "$output" = "grofe $version" ]
    done
    run "$BIN" -L x --version
    [ "$status" -eq 0 ]
    [ "$output" = "grofe $version" ]
}

@test "--help and -h print usage with the common commands" {
    for flag in --help -h; do
        run "$BIN" "$flag"
        [ "$status" -eq 0 ]
        [[ "${lines[0]}" == usage:\ grofe* ]]
        for word in -L -S --version --help new-session attach-session \
            list-sessions split-window send-keys kill-server; do
            grep -q -- "$word" <<<"$output"
        done
    done
    # Nothing starts a server.
    [ ! -e "$SOCK" ]
}

@test "through a tmux symlink -V and -h stay tmux's" {
    mkdir -p "$TMP/bin"
    ln -s "$BIN" "$TMP/bin/tmux"
    run "$TMP/bin/tmux" -V
    [ "$status" -eq 0 ]
    [ "$output" = "tmux 3.7b" ]
    run "$TMP/bin/tmux" -h
    [ "$status" -eq 0 ]
    [[ "${lines[0]}" == usage:\ tmux* ]]
}

@test "no GROFE variables reach the binary" {
    run env
    [ "$status" -eq 0 ]
    ! grep -q '^GROFE' <<<"$output"
}

@test "M1 scenario: new-session, split-window, send-keys echo hi, capture-pane" {
    g new-session -d -s m /bin/sh
    g split-window -t m /bin/sh

    run g list-panes -t m
    [ "$status" -eq 0 ]
    [ "${#lines[@]}" -eq 2 ]

    g send-keys -t m:1.2 'echo hi' Enter
    wait_for_line m:1.2 hi

    run g display-message -p -t m:1.2 '#{pane_uid}'
    [ "$status" -eq 0 ]
    [[ "$output" =~ ^[0-9A-HJKMNP-TV-Z]{26}$ ]]
    uid="$output"
    run g display-message -p -t m:1.2 '#{pane_uid}'
    [ "$output" = "$uid" ]

    g kill-server
    run g has-session -t m
    [ "$status" -ne 0 ]
}

@test "scratchpad: new, hide, list, show" {
    g new-session -d -s m /bin/sh
    g new-scratchpad -t m /bin/sh
    run g display-message -p -t m '#{pane_id}'
    [ "$status" -eq 0 ]
    pad="$output"
    run g display-message -p -t "$pad" '#{pane_floating_flag}'
    [ "$output" = "1" ]

    g toggle-scratchpad -t "$pad"
    run g list-panes -t m
    [ "${#lines[@]}" -eq 1 ]
    run g list-scratchpads
    [ "$status" -eq 0 ]
    [ "$output" = "0: $pad /bin/sh · home" ]

    g toggle-scratchpad -t m
    run g list-scratchpads
    [ -z "$output" ]
    run g list-panes -t m
    [ "${#lines[@]}" -eq 2 ]
}
