#!/usr/bin/env bats
# One trivial bats test that runs the built binary.

GROFE_BIN="${GROFE_BIN:-target/debug/grofe}"

@test "grofe binary runs" {
    run "$GROFE_BIN"
    [ "$status" -eq 0 ]
}
