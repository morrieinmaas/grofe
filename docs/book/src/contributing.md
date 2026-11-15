# Contributing

grofe welcomes issues, ideas and pull requests. How to build it, the checks a
change must pass, how tests and commits work, and how these docs are made are
all in
[CONTRIBUTING.md](https://github.com/termage/grofe/blob/main/CONTRIBUTING.md).

- Found a bug? [Open an issue](https://github.com/termage/grofe/issues/new/choose)
  with `grofe -V`, your OS and terminal, and the steps to reproduce.
- Found a security problem? Report it privately, as
  [SECURITY.md](https://github.com/termage/grofe/blob/main/SECURITY.md)
  describes.
- Want to change these docs? Every page has an edit link at the top; the
  options reference is generated from the code, so change the option's `doc`
  line in `src/server/dispatch/options/table.rs` (lists in `lists.rs`) and run `just docs`.
