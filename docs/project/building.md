# Building and releasing

[← README](../../README.md)

## Build

Needs Node 22.12+ and a Rust toolchain everywhere, plus the platform's own
prerequisites.

**Windows:** the MSVC C++ build tools.

**Linux (Debian/Ubuntu):** Tauri is a GTK and WebKit application, and none of
that is installed by default.

```bash
sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev \
                 librsvg2-dev libxdo-dev libssl-dev build-essential \
                 patchelf file
```

`libwebkit2gtk-4.1` specifically — Tauri v2 does not use the 4.0 series. Fedora
wants `webkit2gtk4.1-devel`, `libappindicator-gtk3-devel`, `librsvg2-devel` and
`libxdo-devel`. Then, on either:

```bash
npm ci
npm run app:build
```

That produces `.deb`, `.rpm` and `.AppImage` under
`src-tauri/target/release/bundle/`, or the NSIS and MSI installers on Windows.
`--bundles deb` and friends narrow it down when only one is wanted.

`npm run app:dev` runs it with hot reload.

Source edits and passing tests do not update an installed pet. For an authorized
local update, build the frontend and native executable, preserve a backup of the
installed executable, replace it, and restart through the existing launcher.
On Linux, compare the built executable's SHA-256 with `/proc/<pet-pid>/exe` after
restart and check the fresh `~/.pipsqueak/running.json` heartbeat. Matching version
numbers alone do not prove the running copy contains an unversioned fix.

`npm run dev` serves the frontend alone in a browser, with a demo loop standing
in for real sessions. `?panel=welcome` and `?panel=doctor` render those panels
so they can be worked on without a build or a broken machine to point them at.

## The README animation

```bash
npm run demo
```

Paints a flat backdrop over the screen, runs the release build against an
isolated profile driven by `tools/simulate.mjs`, captures frames, and crops to
the pixels that actually changed, so re-recording it needs no judgement about
what happens to be on your desktop, and nothing of yours ends up in a public
README. Windows only; the capture is a `CopyFromScreen` through PowerShell.

`--frames`, `--interval`, `--lead` and `--backdrop` control it. The output is an
animated PNG rather than a GIF: 256 colours band the backdrop badly, and the
encoder is a few chunks on top of the PNG writer the sprite pipeline already
has.

## Tests

```bash
npm test                                   # the pure display logic and the pet manifests
cargo test --manifest-path src-tauri/Cargo.toml
npm run check:sprites                      # the atlases still match their generators
```

`check:sprites` regenerates the atlases and compares them to the committed ones
**by pixel**, then puts the committed bytes back. Not by file: a PNG ends in a
deflate stream, and two zlib builds handed identical pixels emit different
bytes for them, so a file comparison passes on whichever machine generated the
atlas and fails on every other one.

The JavaScript tests run under Node's own runner. No framework, no config
file. `src/derive.js` holds every judgement about whether a card is telling the
truth, kept apart from rendering so it can be tested without a
window, and every function takes `now` so the awkward moments can be examined
rather than waited for.

What is *not* tested: transparent windows, the tray, the hit testing, and
anything that needs a real Claude Code session. Those are checked by hand
against the release checklist below.

## Releasing

1. Bump the version in `package.json`, `src-tauri/Cargo.toml` and
   `src-tauri/tauri.conf.json`. Write `docs/releases/vX.Y.Z.md`.
2. `npm run verify:release` asserts all three agree, that the notes exist, and
   that the tag matches.
3. `npm test && cargo test --manifest-path src-tauri/Cargo.toml`
4. Push the tag. `release` opens the draft, then builds Windows and Linux in
   parallel, running the same tests, clippy and fmt gates on each before either
   is allowed to bundle. The Linux job runs on **ubuntu-22.04** on purpose:
   glibc is forward-compatible and not backward, so the runner's version is the
   floor for everyone who downloads the result, and 22.04 is the oldest image
   that still carries webkit2gtk-4.1.
5. Work through the checklist, then publish the draft. Six assets: `.exe` and
   `.msi` from Windows, `.deb`, `.rpm` and `.AppImage` from Linux.

### Release checklist

Anything not verified on real hardware is recorded as such in the notes rather
than assumed.

**Every platform**

- [ ] Fresh install: the welcome panel appears once, installs the hooks, and
      does not come back after Done, the close button, or a restart.
- [ ] Upgrade from the previous version: the pet is where it was left, and the
      existing `config.json` is neither reset nor rewritten.
- [ ] A session start, a tool call, a permission prompt, and a completion each
      reach the card within about a third of a second.
- [ ] Killing Claude Code mid-turn retires the card within ~10s rather than
      leaving it "running".
- [ ] Setup check: all green, and the connection test reports the session you
      ran during it.
- [ ] Clicks pass through the transparent area to whatever is underneath.
- [ ] Uninstall removes the hooks from `~/.claude/settings.json`.

**Windows**

- [ ] No console window flashes on any menu action or hook.
- [ ] Tray blink on completion, and clicking the tray stops it.
- [ ] Start with Windows survives a reboot.
- [ ] Dragging to a second monitor, then unplugging it, leaves the pet reachable.

**Linux**

- [ ] The `.deb` installs and `pipsqueak` is on `PATH`.
- [ ] The `.AppImage` runs from a fresh download, and the hook command it
      registers in `~/.claude/settings.json` is the path of the AppImage file
      itself — not a `/tmp/.mount_*` path, which is gone the moment it quits.
- [ ] `~/.config/autostart/pipsqueak.desktop` appears on first run and the pet
      is there after a log out and back in.
- [ ] Clicks land on the cards and pass through everywhere else, on Wayland and
      on X11.
- [ ] Clicking a card does not move the keyboard: with something full screen
      behind it, poke a card and check the space bar still reaches what was
      focused before. Needs `GDK_BACKEND=x11`; a Wayland-native client cannot
      refuse focus.
- [ ] The pet survives a display-scale change without shrinking.

**macOS**

- [ ] Builds at all. If there is no hardware to check on, say so in the notes.

### Mixed-agent preview

`npm run dev` demonstrates both provider colors across the dense stack.
Open `http://localhost:1420/?demo=agents` for two full cards on one project,
one Claude Code and one Codex. Collapse either card to verify its labeled chip,
then click the chip to restore it. Check at the native 360 × 640 overlay size.
Codex parser regression fixtures are synthetic and live in `codex.rs`; no
private transcript is checked in.

The live Codex adapter test uses an anonymous Unix socket pair, so restricted
sandboxes must allow local sockets for the Rust suite. It tests passive registration,
fragmented frames, approval flags and disconnect fallback without talking to a real
agent. The browser demo persists its filter/pin preferences in local storage, separate
from the native config. Clear `pipsqueak-demo-config` to reset preview choices.

### Size regression check

S/M/L scale the entire medium layout (360 × 640), including cards, text, menus,
spacing and sprite, by 0.75 / 1 / 1.5. `sceneSize` controls the frontend transform;
the renderer stays at scale 2 to avoid double-scaling the sprite. Hit rectangles
come from the transformed DOM. The native window uses the same ratio and keeps
its bottom-right corner anchored, capped to the monitor's work area. Check Large
on a short viewport too: menus and overflowing stacks must scroll. Verify all
three presets, a reload, and native pointer targets after resizing.

On GTK, `set_size` is asynchronous: coalesce duplicate requests and apply the
saved anchor position only after `inner_size` confirms the new dimensions.
Moving during a shrink lets the compositor clamp against the old larger window.
The display-rescue loop must also wait for pending placement. Native regression:
cycle Medium → Small → Large → Medium and compare the bottom-right corner after
each size settles; frontend screenshots alone cannot detect this race.
