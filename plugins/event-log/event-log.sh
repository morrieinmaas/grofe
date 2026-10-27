#!/bin/sh
# event-log: append every grofe event, one JSON line each, to
# $GROFE_STATE/events.log. Run it as a plugin (see README.md); the server
# provides GROFE (the socket), GROFE_STATE (the state directory) and puts it
# in its own process group. Set GROFE_BIN to use a grofe that is not on PATH.
# Events that happen while this script is not running are not replayed.

state=${GROFE_STATE:?GROFE_STATE is not set; run this as a grofe plugin}
log=$state/events.log
mkdir -p "$state" || exit 1

# A bare subscribe takes every event except pane-output. The bridge prints the
# reply to it first, then one line per event; keep only the event lines and
# ignore any field that is not known. The loop ends when the server closes
# the connection, and the supervisor then restarts the script.
printf '%s\n' '{"id":1,"argv":["subscribe"]}' | "${GROFE_BIN:-grofe}" control |
while IFS= read -r line; do
    case $line in
    '{"event":'*) printf '%s\n' "$line" >>"$log" ;;
    esac
done
