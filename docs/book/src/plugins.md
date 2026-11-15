# Plugins

Your tmux plugins load as they are (see
[Switching from tmux](switching-from-tmux.md)). grofe also has plugins of its
own: separate programs, in any language, that the server supervises.

## Declaring a plugin

```toml
[plugins.event-log]
cmd = ["event-log/event-log.sh"]
env = { LC_ALL = "C" }
restart = "always"
```

The server starts each plugin with itself, restarts it when it exits
(backing off, and disabling it after five crashes in a minute), and stops it
on `kill-server`. `list-plugins` shows them and `plugin-restart <name>`
restarts one.

Only `cmd[0]` is a path: a relative one containing a `/` resolves against the
directory of the config file that sets it, and every later word is passed as
written. A script that needs an interpreter should either be `cmd[0]` itself
(with a shebang line and the exec bit) or be given by an absolute path.

A plugin gets `GROFE` (the socket), `GROFE_PLUGIN` (its name) and
`GROFE_STATE` (the state directory) in its environment; its output goes to
`<state>/plugins/<name>.log`.

## Talking to the server

`grofe control` is a stdio bridge that speaks protocol v1: one JSON request
per line in, replies and events out, so a plugin needs no socket code.

```sh
printf '{"id":1,"argv":["subscribe"]}\n' | grofe control
```

The full protocol is in
[`docs/protocol-v1.md`](https://github.com/morrieinmaas/grofe/blob/main/docs/protocol-v1.md).
[`plugins/event-log`](https://github.com/morrieinmaas/grofe/tree/main/plugins/event-log)
is a working example in POSIX sh: it subscribes to every event and appends
each as a JSON line to `<state>/events.log`. Copy it next to your config and
use the snippet above.
