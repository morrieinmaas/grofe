# Lua scripting and packs

grofe runs Luau (a sandboxed Lua) inside the server. Use it to bind keys, react to events, run
timers and fill the status line. `grofe pack` installs plugins written in Luau, and tmux plugins too.

## Where your script lives

`<config dir>/init.luau` (or `init.lua` if that is the file you have). It runs when the server
starts and again on every `reload`. Each time, grofe drops the old Lua state first, with all its
key bindings, timers and event subscriptions, so a reload never doubles anything up.

The order is: installed packs in name order, then your `init.luau`. An error in any of them is
logged, shown by `show-messages` with the name of the script, and never stops startup.

## The no-freeze contract

Every entry into Lua (the init file, a key, an event, a timer, a `system` callback, the `lua`
command) is one call with its own limits:

- 50 ms of run time. After that every step raises an error, so even a `pcall` loop ends within
  twice the budget.
- 64 MiB of memory.
- No `io`, no `os.execute`, no `loadstring`, no native modules. `require` only reaches plugin and
  config directories.
- Calling the `lua` command from Lua is an error, not a deadlock.

When a call goes over, grofe aborts it, logs `<script>: over its 50 ms budget` and carries on. The
worst case is a stutter of one budget. Anything slow (a `git` call, a network fetch) belongs in
`grofe.system`, which runs outside the server and calls you back with the result.

## The API (`grofe.api == 1`)

Callback ids are small numbers starting at 1. `cmd` raises a Lua error when the command fails, so
wrap it in `pcall` if you expect that.

```lua
grofe.cmd({ "split-window", "-h" })        -- argv form, returns the output
grofe.cmd("new-window -n logs; split-window") -- string form, commands run in order
local s = grofe.fmt("#{session_name}", "%1") -- expand a format, target optional
local id = grofe.bind("prefix", "g", function() grofe.cmd("display-message hi") end,
  { desc = "Say hi", ["repeat"] = false })   -- bind a key, returns an id
grofe.on("window-created", function(e) grofe.log("new window") end)
grofe.every(2000, function() end)            -- timer, at least 100 ms
grofe.system({ "git", "status" }, function(r) grofe.log(r.stdout) end,
  { cwd = "/tmp", timeout_ms = 5000 })       -- r has code, stdout, stderr
grofe.cancel(id)                             -- stop a timer, binding, event or process
grofe.log("text")                            -- write to the server log
local cfg = grofe.plugin("my-plugin")        -- what that plugin's init.luau returned
```

`grofe.every` and `grofe.system` are the only way to run things over time. `system` kills the
process at `timeout_ms` (default 10000) and reports `code = -1`. Event names are the ones
`grofe control` can subscribe to (see [protocol v1](https://github.com/termage/grofe/blob/main/docs/protocol-v1.md)); the callback receives the
event as a table.

## Example: a status widget

The status line never calls Lua. A widget pushes a value into an option, and the format reads it.

```lua
local function refresh()
  grofe.system({ "git", "rev-parse", "--abbrev-ref", "HEAD" }, function(r)
    local branch = r.code == 0 and r.stdout:gsub("%s+$", "") or ""
    grofe.cmd({ "set-option", "-g", "@git", branch })
  end)
end

refresh()
grofe.every(2000, refresh)
```

Show it in the status line with `#{@git}`, for example in `config.toml`:

```toml
[status]
right = "#{@git} #{host_short}"
```

## Example: a layout script on a key

```lua
grofe.bind("prefix", "D", function()
  grofe.cmd({ "new-window", "-n", "dev" })
  grofe.cmd({ "split-window", "-h" })
  grofe.cmd({ "send-keys", "-t", "dev.1", "nvim .", "Enter" })
  grofe.cmd({ "send-keys", "-t", "dev.2", "cargo watch -x test", "Enter" })
end, { desc = "Dev layout" })
```

## grofe pack

Declare plugins in `config.toml`. Cloning never happens inside the server, so a plugin is a table
of data, not a Lua call:

```toml
[[pack]]
src = "github:someone/grofe-git"   # or https://, file:// or an absolute path
rev = "v1.2"                       # optional: branch, tag or commit to track
name = "git"                       # optional: defaults to the repo name
enabled = true                     # optional
```

| Command | What it does |
| --- | --- |
| `grofe pack sync` | clone what is missing and check out the revisions in the lock |
| `grofe pack update [name] [--yes]` | fetch, print the commits each plugin would gain, ask, then write the lock |
| `grofe pack list` | show every plugin and its state |
| `grofe pack clean` | remove clones that no `[[pack]]` declares |

Clones live in `$XDG_DATA_HOME/grofe/pack/<name>`. The lock is `<config dir>/pack-lock.toml`: one
entry per plugin with the source and the exact commit. Commit it to keep machines in step.
`update` without a terminal needs `--yes`. Run `reload` after `sync` to load what was installed.

The kind of plugin comes from the repo. A `grofe-plugin.toml` with `name` and `api` marks a Luau
plugin, and an `api` other than 1 fails `sync` with the plugin's name. A repo with `*.tmux`
files and no manifest is a tmux plugin: each `*.tmux` runs through `run-shell` at load, so this
replaces TPM.

## Writing a plugin

```
my-plugin/
  grofe-plugin.toml   -- name = "my-plugin"  and  api = 1
  init.luau
```

`init.luau` runs at load, and what it returns is stored for `grofe.plugin("my-plugin")`:

```lua
local M = { prefix_key = "g" }
grofe.bind("prefix", M.prefix_key, function() grofe.cmd("display-message plugin") end)
return M
```

The user can then read or tune it from their own `init.luau`, which loads after every pack.

## Editor setup

Install `luau-lsp` and point it at the declarations so `grofe.*` completes and type-checks.
In a luau-lsp client, map a name to the file in `luau-lsp.types.definitionFiles`:

```json
{ "luau-lsp.types.definitionFiles": { "grofe": "types/grofe.d.luau" } }
```

From the command line the same is `luau-lsp lsp --definitions:@grofe=types/grofe.d.luau`.
