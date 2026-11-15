# Luau scripting and grofe pack: design

Amends docs/specs/2026-09-23-grofe-design.md §5.1 and §7 (owner ruling 2026-11-14). Plan: docs/plans/2026-11-14-lua-plugins.md.


What the owner said (2026-11-14): Lua like Neovim, for plugins and scripting grofe; a built-in plugin manager; option A with guard rails, "guarantee no freeze"; a `grofe-plugin.toml` manifest with an API version; one lockfile that also covers tmux plugins (it replaces TPM). First consumers: status-line widgets and layout/workspace scripts.

- Ruling: Luau, not Lua 5.4 (because sandboxing and the interrupt hook are its defaults, not a stdlib list we maintain).
- Ruling: model on Neovim 0.12 `vim.pack` (because 0.13's changes could not be confirmed).
- Ruling: plugins are declared in TOML `[[pack]]`, not in Lua (because cloning must never run inside the budgeted actor).
- Ruling: status widgets push, not pull. A widget is `grofe.every` plus `grofe.system` plus `set-option @name`, shown with `#{@name}` (because the render path then never calls Lua, as spec D8 has status sources push).

**No-freeze contract.** Every entry into Luau (init, key, event, timer, system callback, the `lua` verb) is one guarded call with a 50 ms budget and a 64 MiB memory limit. Once over budget, every interrupt raises, so even `pcall` loops end within 2× the budget. Luau has no `io` and no `os.execute`; `require` is limited to plugin and config dirs; no native modules. The worst case is a stutter of one budget plus a logged error naming the plugin. Nested entry (Lua running the `lua` verb) is an error, not a deadlock.

**API v1** (`grofe.api == 1`):
- `cmd(argv | string) -> string`
- `fmt(format, target?) -> string`
- `bind(table, key, fn, {desc?, ["repeat"]?}) -> id`
- `on(event, fn) -> id`, with event names from the v1 catalog
- `every(ms, fn) -> id`, at least 100 ms
- `system(argv, fn, {cwd?, timeout_ms?}) -> id`, where `fn` receives `{code, stdout, stderr}`
- `cancel(id)`, `log(msg)`, `plugin(name) -> table`

Errors from `cmd` raise as Lua errors.

**Loading** at start and on `reload`: `lua_host::reset` drops the old runtime with its bindings, timers and subscriptions. Then every enabled, locked and installed pack loads in name order: a Luau pack runs `init.luau` (its return value is what `grofe.plugin(name)` gives), a tmux pack `run-shell`s each `*.tmux`. Then `<config>/init.luau` runs, or `init.lua` if that exists instead. Errors go to the server log and `show-messages`, and never stop startup.

**grofe pack:** `sync` (clone missing, check out the locked revs), `update [name] [--yes]` (fetch, print the commit log of each change, ask, then write the lock), `list`, `clean`. Clones go to `$XDG_DATA_HOME/grofe/pack/<name>` and the lock to `<config>/pack-lock.toml`. The kind comes from `grofe-plugin.toml` (`name`, `api`), else from any `*.tmux` file. An `api` other than 1 fails at sync with the plugin's name.

