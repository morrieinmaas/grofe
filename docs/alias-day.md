# Alias day

Alias day is the day grofe becomes your `tmux` for good: on the release
tag, and only after `scripts/release-gate.sh` is green. These are the
owner's steps, for a machine set up with athome (chezmoi dotfiles that
install grofe and canopy). Every step has a stand-in that the release gate
runs in a container; the commands marked for it below are run there by
`tests/e2e/scenarios/readme.sh`, exactly as written here.

**1. Pin and apply.** In athome, set the `ref=` line (and the `ref:`
comment above it) to the release commit in
`run_onchange_17-install-grofe.sh.tmpl`, and to canopy's release tag in
`run_onchange_18-install-canopy.sh.tmpl`. Then apply athome the way its
`AGENTS.md` says: read the diff, copy anything precious out of the way, and
apply.

```sh
chezmoi diff
chezmoi apply
```

The apply links `~/.local/lib/grofe/bin/tmux` to grofe, puts that directory
first on `PATH`, installs grofe's config and canopy's grofe fragments, and
enables the login unit, `canopy-tmux.service`. Open a new shell so it picks
up the new `PATH`.

**2. Check the login.** At your next login the unit starts grofe as `tmux`.
Check that it did, and that canopy and grofe agree on what a reboot would
bring back:

<!-- alias-day: begin -->
```sh
systemctl --user is-active canopy-tmux.service
command -v tmux
tmux display -p 'grofe #{grofe_version}, as tmux #{version}'
~/.local/share/canopy/bin/canopy doctor
grofe reboot-check
```
<!-- alias-day: end -->

`command -v tmux` names `~/.local/lib/grofe/bin/tmux`, and `display` prints
a grofe version: only grofe knows `#{grofe_version}`. athome does not put
canopy on `PATH`, so these steps call it by its full path.

**3. Reboot for real**, on each machine. Every session comes back with its
panes' `uid`s, and every Claude Code pane is back in its own conversation.

**4. Work under the alias for a week.** Write each difference from tmux in
`docs/dogfood.md`, tagged `compat` or `copy`; each one becomes a scenario
and then gets fixed. Read `compat-miss.log` in grofe's state directory each
day.

**5. Keep real tmux installed** until that week ends with no open entry.
It stays reachable by its full path; inside a grofe pane run it as
`env -u TMUX /usr/bin/tmux`, because `$TMUX` there points at grofe.
Removing it is your call.

## Rolling back

At any point you can go back to real tmux. Stop the login unit, which ends
the grofe server and every session in it, remove the `tmux` symlink (grofe
itself and its state stay where they are), and switch canopy back to its
tmux backend:

<!-- alias-day: begin -->
```sh
systemctl --user stop canopy-tmux.service
rm ~/.local/lib/grofe/bin/tmux
hash -r
tmux -V
~/.local/share/canopy/bin/canopy install --yes
systemctl --user start canopy-tmux.service
tmux display -p 'tmux #{version}'
~/.local/share/canopy/bin/canopy doctor
```
<!-- alias-day: end -->

`tmux -V` now prints the real tmux's version. `canopy install --yes` takes
over `~/.config/tmux/tmux.conf`: athome's copy is moved into
`~/.config/canopy/user.conf`, and the original is kept in a restore point.
Revert the athome change too, so the next machine you apply it to is not
switched. grofe's state lives in its own directory and touches nothing of
tmux's, so nothing else needs cleaning up.

To undo `canopy install` later (before going back to grofe, or to hand
`tmux.conf` back to chezmoi), stop the server and restore. Every file canopy
changed comes back byte for byte:

<!-- alias-day: begin -->
```sh
systemctl --user stop canopy-tmux.service
~/.local/share/canopy/bin/canopy restore --all
```
<!-- alias-day: end -->
