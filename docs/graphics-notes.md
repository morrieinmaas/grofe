# Kitty graphics placeholder survey (T3.8, step 0)

Recorded 2026-09-28, before any T3.8 code (m3 addendum R19). Owner review:
**reported to owner, not blocking**.

**Headline.** Placeholder mode (`U=1` plus U+10EEEE cells) is not a
`kitten`-only feature among the owner's tools. The file manager (superfile),
the Neovim picker preview (snacks.nvim) and the yazi adapter all use it, and
chafa switches to it as soon as it thinks it runs under a multiplexer. Stock
Claude Code shows images as text, although its bundle carries a
placeholder-mode image element. The catch is that every tool decides from its
environment (TERM, TERM_PROGRAM, XTVERSION, `$TMUX`) whether to draw images at
all, and several transmit by file or shared memory rather than inline data.
Both points change the T3.8 design; see "Risks" below.

## Per-tool findings

"Capture" means the tool ran in a disposable Debian trixie container
(`podman run --rm --name grofe-t38s ...`, kitten 0.49.1 from kitty's release
binaries, chafa and timg from Debian) with its output piped to a file or
through a private pty. Nothing was installed on the host.

| Tool | Mode | How established | Source |
|---|---|---|---|
| `kitten icat --unicode-placeholder` (0.49.1) | Placeholder. One `a=T,U=1,q=2,f=100` APC per image (not chunked, even at 16.5 KB of base64), then a `c`x`r` grid of U+10EEEE cells with three diacritics each (row, column, id high byte) and the id as a truecolor foreground in colon form `38:2:R:G:B`. Random 32-bit ids unless `--image-id` is given | Capture (fixture below) | [kitty tools/cmd/icat](https://github.com/kovidgoyal/kitty/tree/master/tools/cmd/icat) |
| `kitten icat` without the flag | Direct placement: `a=T` without `U=1`, drawn at the cursor | Capture | same |
| `kitten icat --clear` | `a=d,d=A` (delete all, free data), sent twice | Capture | same |
| Claude Code 2.1.283 (stock) | No inline images: pasted and read images show as text. The bundle does contain a kitty image element in placeholder mode: `a=T,U=1,q=2` with inline chunks, or `a=T,U=1,q=1` with `t=f` (file) or `t=s` (shared memory), delete `a=d,d=I,i=N,q=2`, probes `a=q` for `t=d` (id 31) and `t=f`, with DA1 as the end marker. It is gated on the environment ("inside tmux or screen", "TMUX (tmux answers for itself)", `CLAUDE_CODE_FORCE_TERMINAL_IMAGES`). A pty capture of the first 8 s under four environments (screen TERM, Ghostty, forced, fake `$TMUX`) showed no `_G` bytes at all, so the image probe is lazy. **[owner]**: capture a session that shows an image, on a private grofe or tmux socket | Strings of the shipped binary (the image module is bytecode, so only its constants and one plaintext encoder chunk are readable), pty capture, public feature requests | [anthropics/claude-code#79706](https://github.com/anthropics/claude-code/issues/79706), [#80479](https://github.com/anthropics/claude-code/issues/80479), [claude-mod-images](https://github.com/adamNewell/claude-mod-images) (drives that element) |
| chafa 1.14.5, `-f kitty`, kitty-like TERM | Direct placement: `a=T,f=32,...,m=1` chunks, no `U=1` | Capture | [hpjansson/chafa](https://github.com/hpjansson/chafa) |
| chafa 1.14.5, `-f kitty`, `$TMUX` set or TERM `screen*` | Placeholder: `a=T,U=1,q=2,f=32,c,r,i=1,m=1` in chunks, id as **256-colour** foreground `38;5;1`, two diacritics per cell, rows joined by `CSI 4 D` + `ESC D`. Wrapped in `DCS tmux;` under `$TMUX`, and in plain screen-style `ESC P ... ESC \` pieces when TERM starts with `screen` (grofe's pane TERM today) | Capture | same |
| chafa 1.14.5, auto format under `$TMUX` | Falls back to symbols (text) | Capture | same |
| timg 1.6.0 (Debian) `-pk` | Direct placement: `a=T,i=<random>,q=2,f=100` with no `U=1`, also with `$TMUX` set | Capture | [hzeller/timg](https://github.com/hzeller/timg) |
| timg upstream main | Placeholder (`U=1,c,r` and U+10EEEE tiles) only when its own terminal query decides it is inside tmux | Source reading | [src/kitty-canvas.cc](https://github.com/hzeller/timg/blob/main/src/kitty-canvas.cc) |
| Neovim: snacks.nvim (pinned rev `882c996`), picker and explorer preview | Placeholder when its terminal table says so (Ghostty and kitty yes, WezTerm no). Transmit without `a=` (so `a=t`) using `t=f` and a file path locally, `t=d` chunks only when it detects ssh; then `a=p,U=1,i,p,C=1,c,r`; delete `a=d,d=i,i,p`; default `q=2`. Detects the terminal from the XTVERSION reply (`CSI > q`), under tmux from `tmux display-message -p "#{client_termname}"`, and runs `tmux set -p allow-passthrough all`. The owner's config does not list `image`, but the picker previews images whenever `Snacks.image.config.enabled ~= false` | Source reading of the pinned revision | [image/terminal.lua](https://github.com/folke/snacks.nvim/blob/882c996cf28183f4d63640de0b4c02ec886d01f2/lua/snacks/image/terminal.lua), [image/image.lua](https://github.com/folke/snacks.nvim/blob/882c996cf28183f4d63640de0b4c02ec886d01f2/lua/snacks/image/image.lua), [picker/preview.lua](https://github.com/folke/snacks.nvim/blob/882c996cf28183f4d63640de0b4c02ec886d01f2/lua/snacks/picker/preview.lua) |
| superfile (`show_image_preview = true`) | Placeholder: `TransmitAndPut`, `Direct`, `RGBA`, `VirtualPlacement: true`, chunked, quiet, i.e. `a=T,U=1,t=d,f=32,q=2,m=...`; delete `a=d,d=i,i=N`. Enabled only when TERM or TERM_PROGRAM is on an allowlist (ghostty, WezTerm, iTerm2, kitty, ...); `tmux` is not on it (upstream issue 1169, open) | Source reading | [src/pkg/file_preview/kitty.go](https://github.com/yorukot/superfile/blob/main/src/pkg/file_preview/kitty.go), [issue 1169](https://github.com/yorukot/superfile/issues/1169) |
| yazi (still installed, replaced by superfile in the config) | Placeholder: `q=2,a=T,C=1,U=1,t=s` (shared memory) locally or inline chunks; id as truecolor RGB; delete `a=d,d=A` | Source reading | [yazi-adapter/src/drivers/kgp.rs](https://github.com/sxyazi/yazi/blob/main/yazi-adapter/src/drivers/kgp.rs) |
| fastfetch, glow | No graphics: fastfetch's logo is `type: small` (text); glow renders image references as text | Owner's dotfiles | n/a |

Not installed on the dev Mac: `kitten`, chafa, timg. **[owner]**: which of
these the owner actually runs inside the multiplexer (superfile and the Neovim
picker are the likely daily ones).

## What each tool sends first

- `kitten icat` reads the cell pixel size from `TIOCGWINSZ` and exits with
  "Terminal does not support reporting screen sizes in pixels" when
  `ws_xpixel`/`ws_ypixel` are 0; it sends no escape to ask. With pixels known
  and `--transfer-mode` left on detect, it sends three `a=q` queries (inline
  `t=d`, temp file `t=t`, shared memory `t=s`, ids 1, 2, 3) and then DA1
  `CSI c`, and picks the first medium answered `OK`. Under `$TMUX` it runs
  `tmux show -Ap allow-passthrough` and `tmux set -p allow-passthrough on`
  (captured with a fake `tmux` on PATH), and with `--passthrough=tmux` wraps
  in `DCS tmux;`.
- Claude Code queries `a=q,t=d` with id 31 (the same query grofe's client
  probe uses) and an `a=q,t=f` file query, then DA1.
- snacks.nvim sends XTVERSION and waits up to 1.5 s; no `a=q`.
- superfile decides from environment variables only.
- chafa and timg (1.6.0) do not query before a forced `-f kitty`/`-pk`.

## Escapes grofe must forward or answer

Byte examples are from the captures unless marked as source. `ESC` is 0x1b,
`ST` is `ESC \` (0x1b 0x5c).

**Transmit plus virtual placement in one command** (icat, superfile, yazi,
chafa, Claude Code):

```
ESC _Ga=T,q=2,f=100,U=1,s=4,v=4,c=2,r=2,i=1;iVBORw0KGgoAAAANSUhEUgAAAAQAAAAECAIAAAAmkwkpAAAAFklEQVR42mP4zwAE/yEkgvUfysUqAwCeow/x3VZc7gAAAABJRU5ErkJggg ST
```

**Chunked transmit** (chafa; superfile and Claude Code alike). Only the first
chunk carries the keys; Claude Code repeats `q=2` on later chunks:

```
ESC _Ga=T,U=1,q=2,f=32,s=40,v=40,c=4,r=2,i=1,m=1 ST
ESC _Gm=1;/gAA//YATP/sAGz/... ST
...
ESC _Gm=0 ST
```

**Transmit, then place** (snacks.nvim, from source; no `a=` means `a=t`):

```
ESC _Gt=f,i=<id>,f=100,q=2;<base64 of a file path> ST
ESC _Ga=p,U=1,i=<id>,p=<placement>,C=1,c=<cols>,r=<rows>,q=2 ST
```

**Delete:**

```
ESC _Ga=d,d=I,i=<id>,q=2 ST             Claude Code (source): one image, free data
ESC _Ga=d,d=i,i=<id>,p=<placement>,q=2 ST   snacks.nvim (source): keep data
ESC _Ga=d,d=A ST                        icat --clear, yazi: every image
```

**Query and reply.** Kitty answers `a=q` without storing the image, and
honours `q` (1 suppresses `OK`, 2 suppresses errors too):

```
ESC _Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA ST            Claude Code, grofe's probe
ESC _Ga=q,f=24,s=1,v=1,S=3,i=1;MTIz ST             icat, inline
ESC _Ga=q,f=24,t=t,s=1,v=1,S=47,i=2;<b64 path> ST  icat, temp file
ESC _Ga=q,f=24,t=s,s=1,v=1,S=19,i=3;<b64 name> ST  icat, shared memory
CSI c                                               DA1 end marker
reply, success:  ESC _Gi=1;OK ST
reply, failure:  ESC _Gi=2;<ECODE>:<message> ST     (anything but OK)
```

**Placeholder cells.** U+10EEEE is `f4 8e bb ae`; the diacritics are from
kitty's table (index 0 is U+0305 `cc 85`, 1 is U+030D `cc 8d`, 2 is U+030E,
...). The icat 2x2 grid, image id 1:

```
ESC [38:2:0:0:1m
f4 8e bb ae  cc 85  cc 85  cc 85      row 0, col 0, id high byte 0
f4 8e bb ae  cc 85  cc 8d  cc 85      row 0, col 1
CR LF (after padding spaces)
f4 8e bb ae  cc 8d  cc 85  cc 85      row 1, col 0
f4 8e bb ae  cc 8d  cc 8d  cc 85      row 1, col 1
ESC [39m
```

The foreground comes in three forms: `38:2:R:G:B` (icat), `38;2;R;G;B`
(truecolor RGB elsewhere) and `38;5;N` for ids below 256 (chafa). A random
icat id 0x862033e2 was sent as `38:2:32:51:226` plus a third diacritic for
0x86. The placement id is the underline colour (`58:...`, snacks' `sp`).

**Multiplexer wrappers** (TC.12's, recorded here for reference):

```
ESC Ptmux; ESC ESC _Ga=T,U=1,... ESC ESC \ ESC \      chafa, icat, snacks under $TMUX
ESC P ESC _Ga=T,U=1,...,m=1 ESC \ ESC P ESC ESC \ ...  chafa when TERM is screen*
```

## Fixtures

- `tests/fixtures/graphics/red-blue-4x4.png`: a 79-byte 4x4 red and blue
  checkerboard PNG, generated by a script (no personal data).
- `tests/fixtures/graphics/icat-placeholder-2x2.bin`: the exact output of
  `kitten icat --unicode-placeholder --transfer-mode=stream
  --use-window-size=80,24,160,48 --image-id=1 --stdin=no red-blue-4x4.png`
  (kitten 0.49.1): a leading CR, one `a=T,q=2,f=100,U=1,s=4,v=4,c=2,r=2,i=1`
  transmit, and a 2x2 placeholder grid in colour `38:2:0:0:1`, 289 bytes. It
  serves as the scripted fake program's output for acceptance case 5.

## Risks to the T3.8 design

1. **File and shared-memory transmits.** snacks.nvim sends `t=f` (a path),
   yazi `t=s`, and Claude Code and icat use `t=f`/`t=t`/`t=s` once a query
   says they work. Forwarding them as-is breaks the cache and late attach
   (`t=t` and `t=s` are deleted by the first terminal that reads them), and
   breaks any client whose terminal is on another host. The brief forwards
   transmits unchanged.
2. **`a=q` answer ordering.** icat and Claude Code send DA1 right after their
   queries and treat the DA1 reply as "no more answers". grofe's emulator
   (alacritty_terminal) answers DA1 while parsing the same bytes, so the `a=q` reply must reach the pane
   before that DA1 reply, or both tools conclude there is no support. A
   reply queued through the actor after the emulator already answered is too
   late.
3. **Id encoding is wider than a foreground rewrite.** The remap has to cope
   with `38;5;N`, `38;2`, `38:2` and the third (high byte) diacritic. icat's
   default ids are random 32-bit numbers, so the high byte is routine, not an
   edge case. A remapped id that needs a high byte also needs that diacritic
   rewritten, which the brief does not list.
4. **Delete-all.** `a=d,d=a`/`d=A` from one pane (icat `--clear`, yazi) would
   wipe every pane's images on the outer terminal if forwarded.
5. **Pixel size.** grofe sets `ws_xpixel`/`ws_ypixel` to 0 in the pane's
   winsize (`src/server/pane/mod.rs`), and `kitten icat` refuses to run in
   that case. Acceptance case 6 fails unless grofe learns the cell size from
   the client (for example a `CSI 16 t` probe) and passes it on, which is
   outside T3.8's files. icat's `--use-window-size` gets a test past it but
   not a user.
6. **Whether tools draw at all inside grofe.** superfile checks
   TERM/TERM_PROGRAM against an allowlist, snacks asks XTVERSION (whether
   grofe's emulator answers it was not checked) or tmux, Claude Code has its own
   tmux/screen verdicts, and chafa's auto mode falls back to text under a
   multiplexer. With TERM `screen-256color`, and from M3.6 `TERM_PROGRAM=tmux`
   plus `$TMUX` (D12), most of these either stop drawing images or switch to
   passthrough wrappers. chafa's screen-style `ESC P` wrapping is not
   `DCS tmux;` and the spec only unwraps the latter.
7. **tmux commands.** Under `$TMUX`, icat runs `show -Ap allow-passthrough`
   and `set -p allow-passthrough on`, snacks runs `set -p allow-passthrough
   all` and `display-message -p "#{client_termname}"`. M3.6's compat inventory
   needs these (they will otherwise land in `compat-miss.log`).
8. **Large single APCs.** icat sends a whole image in one APC (16.5 KB of
   base64 for a 12 KB PNG). The scanner must frame APCs of any size under a
   cap, not assume 4096-byte chunks.

Lower risk: transmits without an `a=` key mean `a=t` and must be treated as
such; Claude Code's file-medium transmit uses `q=1` and expects error replies,
which is moot once risk 1 is handled.

## Recommendation

Proceed with T3.8; the mode is real and used by the owner's daily tools. Amend
the brief before coding:

- On a transmit with `t=f`, `t=t` or `t=s`, read the data on the server (the
  pane runs on the same host), delete `t=t`/`t=s` sources as kitty would, and
  forward and cache it as `t=d` chunks of at most 4096 bytes. Answer `a=q`
  with `OK` only for `t=d`, and an error for the other media, so tools that
  query fall back to inline data.
- Answer `a=q` synchronously in the pane's output path, ahead of any DA1 reply
  from the same read.
- Specify the remap over all three colour forms and the high-byte diacritic.
- Translate a pane's `d=a`/`d=A` into per-image deletes for that pane's ids.
- Treat a missing `a=` as `a=t`.
- Raise the pixel-size gap with the orchestrator as a contract edit outside
  T3.8 (client probe and pane winsize); until then the kitten acceptance test
  passes `--use-window-size`.
- Leave risks 6 and 7 to T3.6/M3.6 and TC.12, and log in the dogfood week
  which tools draw images inside grofe in Ghostty and WezTerm (**[owner]**).
