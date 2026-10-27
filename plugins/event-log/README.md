# event-log

A grofe plugin in POSIX sh. It runs `grofe control`, subscribes to every event
in [protocol v1](../../docs/protocol-v1.md) (all but `pane-output`), and
appends each event as one JSON line to `$GROFE_STATE/events.log`, exactly as
the server sent it. It needs no `jq`, but it does need `grofe` on the
server's `PATH` or `GROFE_BIN` set to its path (see below).

```toml
[plugins.event-log]
cmd = ["sh", "event-log/event-log.sh"]
```

The path resolves against the directory of the config file. `GROFE_STATE` is
the state directory the server sets for every plugin. If `grofe` is not on the
server's `PATH`, set `env = { GROFE_BIN = "/path/to/grofe" }`.

Events that happen while the plugin is down are not replayed: the protocol
keeps no history, so a restart (after a crash, `plugin-restart` or a reload)
loses whatever happened in between. The file is never rotated or truncated.
