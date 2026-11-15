# Config reference

The options reference now lives in the book, generated from the option table
so it cannot go stale: every option with its TOML key, type, scope, allowed
values, default and description.

- [Options reference](book/src/reference/options.md)
  ([on the site](https://termage.github.io/grofe/docs/reference/options.html))
- [Configuration](book/src/configuration.md): layers, `show-config --explain`
  and `reload`
- [Getting started](book/src/getting-started.md): `reboot-check`

Regenerate the reference after changing an option with `just docs`.
