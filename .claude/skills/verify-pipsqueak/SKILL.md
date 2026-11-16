---
name: verify-pipsqueak
description: Drive the real Pipsqueak UI in a browser and prove a user-visible behaviour with captured evidence. Use before claiming any change to the pet, the hint, the cards, or the menu works, and whenever a fix concerns something that only shows up while the app is running.
---

# Verify Pipsqueak

The unit tests under `test/` cover sprite decoding, derivation and pet data.
They cannot see the pet on screen, so every bug about the hint, the cards, the
menu or the drag lands outside them. That is where the last three hint fixes
lived (`74f7262`, `bb55af0`, `e242576`), and it is what this skill covers.

The app runs outside Tauri. `src/main.js:24` sets `IS_TAURI` from
`__TAURI_INTERNALS__`, and the browser path is a supported degradation, so the
whole UI can be driven in plain Chrome with no desktop, no compositor and no
window management. Use that for browser-verifiable behavior. Use the currently available native
computer-use workflow (Deskwright on this workstation) when the behavior needs
the real Tauri window, such as click-through, always-on-top or multi-monitor
placement. Choose the desktop surface from the task and current personal guidance.

## Launch

```bash
npm run dev        # vite, port 1420, ready when it prints "ready in"
```

Ready check: `curl -sf http://localhost:1420/` returns 200. The drive script
waits for this itself and tells you to start the server if it is missing. Leave
the server running between drives; it is not what the script tears down.

## Doctor

Run this first whenever anything looks off, before debugging the app:

```bash
node .claude/skills/verify-pipsqueak/drive.mjs doctor
```

It checks the page is Pipsqueak, `#pet` and `#hint` exist, and the viewport is
non-zero. That last one matters more than it looks: **the pet parks itself at
-104,-104 while the viewport is 0x0**, so every hit test silently misses and
every interaction appears to do nothing. A doctor failure means the harness is
wrong, not the app.

## Drive

```bash
node .claude/skills/verify-pipsqueak/drive.mjs all     # doctor plus every mapped feature
node .claude/skills/verify-pipsqueak/drive.mjs hint    # one feature
```

Zero dependencies. It launches its own headless Chrome on port 9333 with a
private profile in a temp dir, talks CDP over the `WebSocket` that Node 22
ships, and exits non-zero when any check fails. Overridable by environment:
`APP_PORT`, `CDP_PORT`, `CHROME_BIN`, `EVIDENCE_DIR`.

### The gotcha that will bite the next probe

`src/main.js:1466` registers a **capture-phase** `pointermove` listener on
`document`, and any move that is not over the pet calls `leftPet()`, which hides
the hint and cancels the pending one. A probe that dispatches one `pointerenter`
and then waits quietly is not a hovering user, it is a user who left: a single
stray move from the browser kills the hint and the check fails for a reason that
has nothing to do with the app. Dwell the way a hand does, re-dispatching
`pointermove` on the pet every 150ms. `hintLifecycle()` already does this.

This cost a false failure the first time the script ran. When a check fails,
suspect the observation before the app.

## Evidence

Written to `.verify/evidence/` (gitignored, survives cleanup):

- `run.json`: doctor result, every check with its verdict, and the raw observed
  values behind them, so a reviewer can re-derive the verdict rather than trust
  it.
- `hint-after-run.png`: full-page screenshot at the end of the run.

Proof standards: drive the real user path through real events on real elements,
never an internal setter or a test-only hook. Capture the state at each moment
the behaviour is supposed to change, not only the final screen. Where a check
asserts something is hidden, assert the element's own `hidden`, not its absence.

Evidence produced by this script sits at rung 4 of the `docs/engineering.md`
ladder: real code ran and would have failed loudly. Rung 5, reproduced in the
running Tauri window with a real cursor, needs the available native computer-use
workflow and is not what this script does. Say which rung you got to.

## Cleanup

The script kills the Chrome it started and removes its temp profile, in a
`finally`, so a failed run cleans up too. It never kills by process name and it
never touches the dev server or any other Chrome. Evidence is not removed.

Verify with `ls -d /tmp/pipsqueak-verify-*`, which should match nothing.

## Coverage

`features/README.md` is the map. One feature is proven today, and a drive that
exercises only the convenient one is incomplete while the map lists others.
Adding a feature means a map entry and a function in `drive.mjs` with its own
named checks.
