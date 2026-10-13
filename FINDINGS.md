# Findings ledger

Deep audit, 2026-08-06 (Fable 5, five parallel audit lenses + hook-API doc
verification). Each finding: mechanism at the line, status. Fixed items name
the commit subject; parked items say why.

## The symptom that started this

"Cards pop up and then disappear again immediately; strange transient visuals."
Root causes found, in order of contribution:

| # | Mechanism | Status |
| --- | --- | --- |
| P-1 | Sprite-loop settle clock anchored to `frame === 0`, which a cycling loop leaves within 300ms — the 5s threshold was unreachable, so the canvas repainted at 60fps around the clock (measured 41.9% of a core sustained, 9h18m CPU over 31h incl. idle nights); fixed by anchoring the clock to loop entry + a 15s stale gate for working loops, `body.settled` stops standing CSS animations, clock render drops to 1/10s when settled | FIXED |
| S-1 | `Stop` with background tasks writes `state:"idle"` + 2s settle; `displayState` returns `idle` during the settle window, card leaves the DOM, returns 1–3s later as done | FIXED |
| H-2 | First `Stop` gets `settle_ms: 0`; a blocking stop hook (this user runs one) forces continuation and the just-shown Done card is wiped by the next event | FIXED |
| W-1 | Frontend-liveness watchdog trusts a throttleable JS timer; WebView2 clamps timers for occluded windows to ~1/min, so the overlay hard-reloads every 60s while occluded | FIXED |
| F-3 | Reordering a live card node (`prepend`/`after`) restarts the 160ms `rise` entrance animation from `opacity:0` — the card blinks when promoted | FIXED |
| F-5 | A notice injects a synthetic done-session that takes a slot: 4th slot flips dense mode, whole stack collapses for 6s, tray flashes | FIXED |
| H-3/I-4/S-2 | `SessionEnd` deletes the session file and its lock without holding the lock; an in-flight hook resurrects it as a zombie card that the sweep later kills | FIXED |
| S-5/F-2 | Sweep deletes a session file on a *transient read failure* (AV/indexer sharing violation), next hook recreates it bare — delete/recreate flap | FIXED |
| S-3/W-6 | Sweep does lock-free read-modify-write against files hooks are writing; can write back a stale snapshot ("Stopped responding" flash on a fresh turn) | FIXED |
| S-4 | `CreateToolhelp32Snapshot` transient failure reads as "every process is dead" → live session deleted, recreated on next hook | FIXED |
| W-2 | `pipsqueak control show` unconditionally reloads a healthy page — every `/pet` invocation blanks and repaints the overlay | FIXED |
| F-1 | Frontend tears down all card state (view, acknowledged, seen, slot) the instant a session misses one emit; combined with any transient dropout above, full flash + re-greet | FIXED (root causes above + read retry) |

## Fixed — backend truthfulness

- **Codex resumed-rollout collision (2026-09-22)** `codex.rs` — a resumed
  desktop task can leave multiple rollouts with the same conversation ID.
  Publishing each file independently let an old interruption outrank a later
  successful turn; live Active temporarily hid it, and Idle exposed it again.
  Select the newest session-metadata incarnation before publishing, including
  during bounded startup replay. Regression tests cover subsequent turns,
  restart, obsolete-file updates, deterministic ties and genuine interruptions.
- **S-1 / H-2** `hook.rs:503`, `derive.js:93` — every `Stop::Finished` now
  settles for 2000ms, and `displayState` reports `running` (not the stored
  `idle`) inside the settle window, so the card never leaves the DOM at turn
  end and a stop-hook-forced continuation cancels the outcome before it was
  ever shown.
- **S-7** `hook.rs:123` — `Stop`/`StopFailure` now clear `waiting_since`; a
  denied permission followed by turn end no longer leaves "Needs you" up for
  hours.
- **H-9** `hook.rs:671` — a `SubagentStop` when no subagents are outstanding is
  bookkeeping even when the outcome was already cleared by a new prompt.
- **H-5** `hook.rs:142` — a new permission prompt for the same tool but a
  different command refreshes detail, risk and the debounce clock instead of
  showing the previous command's text.
- **H-6** `hook.rs:280` — `SessionStart` reads `source`; `"compact"` no longer
  resets a working card to "Session started" mid-turn (doc-verified: the event
  fires mid-session after auto-compaction).
- **H-7** `hook.rs:354` — `Notification` without the undocumented
  `notification_type` field falls back to classifying by message text instead
  of being dropped (doc-verified: the field is not in the payload schema).
- **H-3/I-4/S-2** `hook.rs:49` — `SessionEnd` acquires the file lock before
  deleting and leaves the lock file to expire; events other than
  `SessionStart`/`UserPromptSubmit` no longer create a session file from
  nothing, so stragglers cannot resurrect an ended session.
- **H-8** `narration.rs` — transcript entries with `isSidechain: true` are
  skipped; a subagent's inner monologue no longer flickers onto the parent
  card.

## Fixed — sweep and liveness

- **S-3/S-5/W-6/F-2** `state.rs:542` — the sweep takes the per-file lock for
  its read-modify-write; an IO read failure skips the file (retry next sweep)
  instead of deleting it; only a successfully read but unparseable file is
  removed.
- **S-4** `process.rs` — a failed process snapshot now reads as "unknown, treat
  as alive" instead of "everything is dead".
- **S-11** `state.rs:470` — a lock file whose age cannot be read is no longer
  stolen instantly; stealing requires a provably expired lock.
- **F-1** `state.rs:503` — `read_sessions` retries a failed read once before
  dropping a session from the emit.

## Fixed — window layer

- **W-1** `app.rs`, `main.js` — liveness no longer trusts a throttleable
  timer: when the page has been silent the backend emits a ping and only
  reloads if the page fails to answer it; a throttled-but-alive page answers
  events immediately.
- **W-2** `app.rs:820` — `control show` reloads only a page that is actually
  silent.
- **W-3** `app.rs` — the heartbeat is written at setup, before the first poll
  tick, and a second instance that finds a live heartbeat at startup defers to
  it (shows the running overlay and exits) instead of stacking a duplicate pet.
- **W-5/F-14/W-11** `app.rs`, `main.js` — the frontend calls `frontend_ready`
  after its listeners attach; the poller re-emits its state, so changes that
  land in the boot/reload gap (including pet-switch commands) are not lost.
- **W-8** `control.rs:27` — `pipsqueak control stop` quits; only
  `off`/`hide` hide (matches the "put it away" vs "stop it" split).
- **W-4** `app.rs`, `main.js` — tray toggles emit the full config, the
  frontend listens and merges, and the tray checkboxes are updated on every
  config change; the two sides no longer overwrite each other's settings.
- **W-12** `app.rs:488` — default first-launch placement uses the monitor work
  area instead of a hard-coded taskbar guess.

## Fixed — frontend

- **F-3/F-12** `main.js`, `style.css` — the entrance animation is a one-shot
  class applied only to freshly built cards (removed on `animationend`, hit
  rects re-synced then); moving a card no longer replays it.
- **F-4** `main.js:498` — the chip row is only re-appended when out of place.
- **F-5** `main.js` — a notice always shows at the front of the stack but no
  longer counts toward the density flip, flashes the tray, or marks itself
  unread; with three chats up, one chat temporarily becomes a chip instead of
  every card collapsing for six seconds.
- **F-9** `main.js:776` — the doctor's countdown and completion timers are
  cancelled when the panel closes or rebuilds.
- **F-10** `main.js` — chips are only offered for live chats; the chip for an
  idle chat restored nothing and its only behavior was to vanish when clicked.
- **F-11** `main.js:215` — `acknowledge` only reports success when it recorded
  one, so clicks during a hold window expand/close instead of doing nothing.
- **F-8** `main.js` — the update-check toggle starts and stops the scheduler
  at the moment it is flipped.
- **F-13** `pet.js` — one-shots start at frame 0 (not the previous row's
  frame) and a state change during a one-shot crossfades from the state row.

## Fixed — installer, doctor, log, misc

- **I-1** `Cargo.toml`, `install.rs` — serde_json `preserve_order`; the
  installer keeps key order, skips the write when nothing changed, and no
  longer plants an empty `hooks` object on uninstall.
- **I-2/I-13** `log.rs` — rotation cuts at a char boundary (no more abort on a
  multibyte midpoint under `panic = "abort"`) and reads lossily so one bad
  byte cannot disable rotation forever.
- **I-5** `install.rs` — settings backups are pruned to the newest five.
- **I-7** `install.rs` — install strips our hooks from every event key (not
  just the current list) before re-inserting, so renamed events cannot orphan.
- **I-9** `doctor.rs:174` — path comparison is case-insensitive and
  separator-insensitive on Windows.
- **I-10** `doctor.rs` — stale traffic (no events for over an hour) warns
  instead of reading green.
- **S-8** `chats.rs` — duplicate chat records for one session resolve to the
  most recently modified, via one shared function, so the title cannot flap
  between rescans and the open button opens the chat the title names.
- **S-9** `chats.rs:243` — surrogate-pair `\u` escapes decode; an emoji title
  no longer silently drops the whole title.
- **S-10** `project.rs` — scratch detection anchors at real OS temp roots
  (`D:\Temp\project` is a project); an unparseable `.git` file continues the
  ancestor walk instead of aborting it.
- **S-12** `risk.rs` — `git branch -D` and `-d` are distinguished (flag case
  is preserved); `bash -c` / `powershell -Command` / `cmd /c` are unwrapped
  one level so a dangerous inner command is still seen.
- **I-17** `.gitignore` — `.harness/` ignored.

## After v0.7.2 — the counting the last release introduced

- **N-1** `narration.rs:245` — only `<status>completed</status>` drained the
  outstanding set, and a real transcript ends work four ways. Across this
  machine's transcripts: 1156 `completed`, 65 `failed`, 22 `stopped`, 10
  `killed`. One failed subagent therefore left the count permanently one too
  high, so every later turn in that session read **Finishing · 1 running**,
  never went green, and never blinked the tray — the five-minute write-off only
  starts once the session goes *entirely* quiet, which a working session never
  does. All four terminal statuses now drain; `running` deliberately does not.
  FIXED, with a test per status.
- **N-2** `main.js:1588` — the browser demo still populated `subagents`, which
  nothing has read since the count moved to the transcript, so the "3 running"
  chip was missing from every demo run and from anything recorded off it.
  FIXED (`outstanding`).
- **N-3** the red chip counted `PostToolUseFailure` and read "N retried". A
  tool that errored is not something anyone can act on, and Claude usually
  just tries something else. It now counts `PermissionDenied` — documented as
  "denied by the auto mode classifier" — and reads "N blocked". FIXED.

## After v0.7.3 — the claim the whole overlay is for

- **N-4** `state.rs:630` — the sweep downgraded any running session silent for
  five minutes to `stalled`, *including one blocked on a human*. Being asked a
  question produces no further hook events by definition, so a real permission
  prompt is indistinguishable from death by that rule. Five minutes after
  walking away from a prompt, the card read **Stopped responding** with the
  question still on it, and `how-it-works.md` had been claiming the opposite
  ("a session genuinely waiting on you is never retired by age") since it was
  written. Reproduced live against the installed 0.7.3 build: a session with
  `waiting_since` six minutes old came back `state: idle, stalled: true`.
  FIXED — the decision is now `has_stopped_responding`, a pure function with
  tests, and a session whose agent really died is still retired by the process
  check that runs before it.
- **N-5** `main.js:439` — `kindLabel` checked `stalled` before `waiting`, so
  even with the backend fixed, any session flagged by an older build showed
  "Stopped responding" over its own question. The checkable claim now wins.
- **N-6** `.github/workflows/release.yml` — the release workflow ran `npm test`
  and `cargo test` but not `clippy -D warnings` or `cargo fmt --check`, the two
  gates `ci` enforces. That is how v0.7.2 shipped green from a commit whose
  `ci` run was red. Both added. The release body also said "See
  docs/releases/vX.Y.Z.md" instead of containing the notes; it now takes the
  file.

## After v0.7.4 — the numbers around the sentence

Reported as "the pet is bullshitting": the quoted line is what Claude really
said, but everything around it — **Finishing · 7 running · 1 action · 4m** on a
turn that was over and had nothing running — was not. Three parallel read-only
audits plus a replay of the real matcher over 250 transcripts and 376 captured
hook payloads.

- **P-1** `narration.rs:230` — `"taskId"` is overwhelmingly the *to-do list's*
  field, not an async one: 694 of 723 occurrences across 233 transcripts were
  `TaskUpdate` ticking a checkbox, ids `"1"`, `"2"`, `"3"`. Each added a
  background task that could never finish, because no completion notification
  for a checkbox exists. Measured on 250 real transcripts: **38 sessions ended
  holding 298 phantom runners**; the screenshot's own session held 14, of which
  7 had accumulated by the moment it was taken. FIXED — `toolUseResult` is read
  as an object, only the four shapes Claude Code actually launches work with
  count, and all-numeric ids are rejected. Same 250 transcripts after: 7
  leftovers in 7 sessions, every one a background shell still alive at session
  end.
- **P-2** `narration.rs:262` — a resumed session emits one notification listing
  every task the previous session abandoned, up to ten `<task-id>` tags under a
  single `<status>stopped</status>`. `split_once` read the first and discarded
  the other nine, which then sat in the count for the rest of the session.
  FIXED — every id in the notification is retired, `__orphan_summary__` markers
  skipped.
- **P-3** `narration.rs:234` — an `attachment` echoing a finished task's id on a
  later line *re-added* it, by the very line saying it had completed. FIXED — a
  result or attachment carrying a finished status retires its id instead.
- **P-4** `main.js:423` — the elapsed was `duration(turn_started_ms)` against a
  live clock, and `turn_started_ms` is cleared only by the next prompt. A
  two-minute turn that finished two minutes ago read **4m** and went on
  climbing for as long as anyone looked at it. FIXED — the hook stamps
  `turn_ended_ms` and `turnElapsed` freezes there; the sweep stamps it too, so
  a "Stopped responding" card stops counting as well.
- **P-5** `hook.rs:133` — a subagent's tool calls arrive on the *parent's*
  session id with `agent_id` set, and were counted as the turn's own actions
  and allowed to rename its status line. Verified live: of 131 tool events in
  one session, 21 were subagents'. The card said "Editing spiralplan.py" while
  the main agent sat waiting for three agents to report. FIXED — `turn_tools`
  counts the main chain, and a delegated call reads "Delegating".
- **P-6** `hook.rs:137` — `UserPromptSubmit` was the only turn boundary, so a
  turn begun by a stop hook sending one back to work, or by a resume, inherited
  the previous turn's counters and clock. FIXED — `prompt_id`, which is in
  every payload, is the boundary; stragglers cannot rotate it back.
- **P-7** `hook.rs:227` — `event_ms` was documented as the race guard ("the
  older one must lose") and was written by one line and read by none. Hooks are
  installed `async: true`, so a `PreToolUse` that started before a `Stop` and
  landed after it cleared the outcome and set the card back to "running" with
  nothing running. FIXED — the stamp is taken before the lock (taking it after
  ordered the events backwards) and an out-of-order event may still count its
  tool call but may not un-finish a turn.
- **P-8** `hook.rs:166` — "anything other than the prompt itself means the
  prompt is over" was written as an exclusion list of two, so `SubagentStop`,
  `PreCompact` and `SessionStart` cleared a permission prompt nobody had
  answered — and a subagent finishing while the main agent waits at a prompt is
  routine. "Needs you" vanished while Claude Code was still asking, and the
  sweep, which reads a cleared `pending_since` as silence, was then free to
  call the session dead. FIXED — only an event proving the tool ran, was
  refused, or that the turn ended may clear it, and never a subagent's.
- **P-9** `hook.rs:383`/`hook.rs:530` — a `Task` call added a subagent and the
  `SubagentStart` for that same subagent added another, against one
  `SubagentStop` removing one. Measured live: three agents launched, count read
  six, and it never returned to zero — which is the value `is_trailing_subagent`
  tests. FIXED — a set of the `agent_id`s in the payload.
- **P-10** `main.js:397` — the "N running" chip read the raw number while the
  status word used the five-minute write-off, so a session silent for an hour
  carried "7 running" next to the word "Done". `kindLabel` could also print
  "Finishing · 0 running" when the work drained inside the state's display
  hold. FIXED — both go through `runningCount`, and zero prints as "Finishing".
- **P-11** `main.js:212` — `holdLine` returned the previous line whenever the
  new one was empty, and the sweep clears `activity` precisely to stop the card
  claiming work is in progress. So the biggest text on a "Stopped responding"
  card was a sentence from the turn before. FIXED.
- **P-12** `main.js:120` — a new view seeded `lastStable: 'running'`, so a
  session whose first event is a permission prompt spent the debounce claiming
  to be working, with a card, a slot and the pet animating as busy, on no
  evidence. FIXED — seeded `idle`.

## After v0.8.0 — the click that took the keyboard

Reported from the desktop it runs on (GNOME 50.1, Ubuntu 26.04, pet under
XWayland): clicking a card while a video was full screen left space and escape
going nowhere until the video was clicked again. The overlay had become the
focused window, which is the one thing it should never be.

- **L-1** `tauri.conf.json` — the window was created focusable, so mutter gave
  it the keyboard on the first click. `focus: false` only covers the *initial
  map*, and setting the GTK input hint from `setup()` does not survive either:
  tao restores `accept_focus(true)` on the first draw of a window built
  `focusable && !focused`
  (`tao-0.35.3/src/platform_impl/linux/window.rs:221`). FIXED — `focusable:
  false` on the window, which tao turns into the ICCCM input hint on X11,
  `WS_EX_NOACTIVATE` on Windows and `canBecomeKeyWindow: NO` on macOS, and
  which also stops the draw-time restore from running. Measured before and
  after with a click driven onto the pet sprite: before, focus moved from
  Chrome to Pipsqueak; after, the pet collapsed its cards (so the click landed)
  while a separate window kept receiving every keystroke typed either side of
  the click.
- **L-2** — a Wayland-native run drops the request entirely: the window takes
  focus on map and ignores its saved position. Not a regression and not
  fixable from the client side (xdg-shell has no way to refuse keyboard focus);
  the README now says so and says to use `GDK_BACKEND=x11`.
- **L-3** `app.rs:1033`, `desktop.rs:124` — found while testing L-1, three
  times over: `ensure_autostart` rewrote the XDG entry whenever its `Exec` was
  not literally this binary, and "not this binary" is not "dead". An entry that
  launches the pet through a wrapper script — to set the `GDK_BACKEND=x11` L-2
  needs, or to wait for the shell's tray support — was replaced by a bare path
  on every single start, silently undoing both, and the doctor told the owner
  of that entry to reinstall and finish the job. FIXED — an entry is corrected
  only when the program it names is gone (`program_is_present`: a file that
  exists and can be executed, a bare name resolved against `PATH`), and the
  doctor says the entry is being left alone rather than advising a reinstall.
  Verified on both branches: a wrapper entry came through a start byte for byte
  identical, and an entry pointing at a path that does not exist was still
  corrected.

## After v0.8.0 — audit 2026-08-22

- **A-1** `app.rs:88` — `e242576` compiled the cursor-polling loop out on
  Linux, which left `Rect::contains` with no caller there; its doc comment
  claimed "used on every platform" and was wrong the day it was written. The
  release gates caught it as designed: `clippy -D warnings` failed the
  ubuntu job on the last two pushes to `main` while the Windows job stayed
  green, so `main` has been unreleasable for three days. FIXED — both helpers
  carry the same `cfg` as the loop they serve, comment now says so.
- **A-2** `main.js:1592` — pruning `acknowledged` kept every key whose session
  was still live, so a long-lived chat accumulated one dead key per dismissed
  completion; a superseded key can never match again (suppression compares
  the current `outcome_ms`), so this was a bounded leak, not wrong behavior.
  FIXED — keys are pruned against the incoming sessions' current outcome
  keys, which also covers the gone-session case the old check was for. The
  prune runs only in the Tauri event path, which the browser demo does not
  drive; verified by the hint suite passing and reasoning at the line, not by
  executing the branch.

## After v0.8.0 — background sessions, 2026-08-29

- **B-1** `hook.rs:678` — Claude Code fires the idle `Notification` at the end
  of every turn, background or not, so a `claude --bg` leg ending a turn
  produced `waiting_reason: "Waiting for your reply"` and a "Needs you" card
  for a session `claude agents --json` was calling `"busy"/"working"` at the
  same moment. FIXED — `CLAUDE_JOB_DIR` (the one background marker that
  actually reaches a hook; `CLAUDE_CODE_SESSION_KIND` is set on the agent
  process and is *not* passed down, measured from `/proc`) flags the session,
  and an idle notification for one is relabelled rather than believed. Gated
  on why the session is waiting, not on whether it is a background one: a
  permission prompt still alerts.
- **B-2** `hook.rs:150` — found while verifying B-1, on a live mission leg
  halted by a model safeguard. A background leg that *failed* is not waiting
  for a supervisor and will not be resumed by one, so B-1's "Waiting to be
  resumed" label was the same lie pointing the other way. FIXED — the
  relabel applies to turns that ended, not turns that died; a failed one is
  recorded silently and the failure stays on the card.
- **B-3** `hook.rs:406` — a `Notification` arriving after a `StopFailure`
  blanked `session.detail`, so the error body the failure carried was wiped
  from the expanded panel and the card said a turn had failed without being
  able to say how. `apply` wrote `session.detail = update.detail`
  unconditionally for every non-silent update, and the events that carry no
  long text — notifications, tool calls, permission requests — all supply an
  empty one. Observed 2026-08-29 on session `88b752ad` (`detail: ""` beside
  `kind: "Failed"`). FIXED — `Update::detail` is now `Option<String>`, the
  same idiom `headline` already used in this file: `None` leaves the panel
  standing, `Some(text)` replaces it, and `Some("")` still clears, so a turn
  that ends with nothing worth reading cannot leave a stale tool error under
  the word "Done". Side effect, and an improvement: the prompt no longer
  vanishes from the expanded card the moment the first tool runs.

## After v0.8.0 — the overlay measured against a "it eats my clicks" report, 2026-08-29

> **Superseded in part — read the next section first.** The verdict below
> ("not the cause") was measured with the card stack COLLAPSED, which is the
> state it is in most of the time and the only state this pass looked at.
> Expanded, the same overlay claimed a 322x304 block. Everything below about
> focus, passthrough and the other overlay still holds; the "click area is
> honest" bullet did not.

Reported from the same desktop as L-1 (GNOME 50.1, Ubuntu 26.04, pet under
XWayland): "the pet is at the front but its click takes up the entire screen" —
space not reaching a playing video until the video is clicked again, and a left
click on a link doing nothing where a right-click menu still works. The pet was
the suspect. It was measured and it is not the cause; the measurements are here
so nobody re-derives them.

- **Click area is honest.** `XShapeGetRectangles(ShapeInput)` on the live
  overlay returned exactly what is painted: `256,536 96x96` (the sprite) plus
  the chip row, `29,511 321x23` at the time of measurement. Overlaid on a
  screenshot of the same window, the rectangles hug the visible chips. There
  is no invisible region, and nothing outside the 360x640 window can be
  affected either way.
- **Clicks outside those rectangles reach the window below.** Reproduced on the
  headless session: the real binary over a Wayland-native GTK window, and the
  same window fullscreen. A click in the transparent part of the overlay was
  logged by the window underneath, and moved focus to it. XWayland does
  translate the X input shape into the surface's Wayland input region.
- **The overlay cannot take the keyboard.** With another window focused,
  clicking the sprite, clicking a button inside its own panel, hiding and
  re-showing the window, and a click over a *fullscreen* focused window all
  left focus where it was. An explicit `org.gnome.Mutter` ActivateWindow on it
  was accepted and then ignored: focus stayed on the other window. L-1's fix
  holds.
- **The other always-on-top overlay on that desktop is innocent too.**
  `claude-brainrot` is `_NET_WM_WINDOW_TYPE_UTILITY`, input hint False, no
  `WM_TAKE_FOCUS`, and an *empty* input region (0 rectangles — an unshaped
  window returns 1 covering itself, checked against a control), so it can
  neither be clicked nor focused.
- **L-4** `tauri.conf.json` / GTK — LATENT, not the reported bug. The overlay
  advertises `WM_TAKE_FOCUS` in `WM_PROTOCOLS` while its ICCCM input hint is
  False. GTK sets that protocol unconditionally at realize and neither tao nor
  Tauri removes it, so the window is ICCCM "globally active" rather than "no
  input": mutter is entitled to make it the focus window and hand it
  `WM_TAKE_FOCUS`, which GTK then declines because `accept_focus` is false —
  the keyboard would land nowhere. That is exactly the L-1 symptom by a second
  route. Not observed: every path tried above left focus alone. Fix if it ever
  is observed — drop `WM_TAKE_FOCUS` from `WM_PROTOCOLS` after realize, which
  is what makes `claude-brainrot` unfocusable by construction.

Method note for the next person: **XTEST does not work on this desktop.**
`xdotool click` and `wmctrl` are not routed to the compositor on GNOME 50 and
asking pops a Remote Desktop consent dialog; a click driven that way lands
nowhere and looks exactly like a click that was swallowed. Two hours of this
investigation were spent on measurements taken that way before the readout was
checked against a control. Use `org.gnome.Mutter.RemoteDesktop` (the
`wayland-computer-use` / `wcu-headless` pointer tools). `xdotool
getmouselocation` is equally untrustworthy: it reports the root window and a
stale position whenever the pointer is over a Wayland surface.

## After v0.8.0 — the block the cards were claiming, 2026-08-29

Same report, second pass, after the owner sharpened it: opening a video full
screen leaves space and the arrow keys dead until the video is clicked again,
and a click on a link or a button does nothing where the right-click menu still
works.

- **L-5** `main.js:762` — every card claimed clicks for as long as it was on
  screen, and the cards are the biggest thing the overlay draws. Caught by a
  watcher logging the live window's X input shape, three cards deep:
  `28,230 322x98`, `28,333 322x98`, `28,436 322x98` — a **322x304 block** of
  always-on-top clickable area, screen `x 1553-1875, y 611-915`, sitting over a
  full-screen video. A click aimed at the video landed on a card
  (`main.js:361`, dismiss/expand), so the video never got the click, never took
  the keyboard, and every keystroke went to whatever had focus before. Clicking
  the video somewhere the pet was not fixed it, which is exactly what was
  reported. STILL OPEN — a fix was written, measured and then reverted at the
  owner's request (`7778c42`, reverted by `1dcc1ed`); the block described above
  is what the overlay does today.

  What was tried, so the next attempt starts further along: at rest the overlay
  claimed only the 96x96 pet, and touching the pet armed the cards and chips,
  which stayed armed while the cursor was on anything the window accepts (the
  existing 150ms leave grace covering the few pixels between two cards, so the
  reach from pet to top card was unbroken; menu and setup panel never gated).
  It worked — measured on the real binary, three chips up: cursor away
  `256,536 96x96`; cursor on the pet `28,484 322x50` + the sprite; cursor moved
  from the pet onto a chip, unchanged; cursor far again, back to the sprite
  alone. With the cursor parked on one chip, disarmed it did not light up and
  armed it did — the same pixel, owned by the window below and then by the pet.
  No reason was given for the revert and none is guessed here. The one cost
  the approach is known to carry, recorded so a later attempt can weigh it:
  every click on a chip or a card began with a detour to the pet, which is a
  tax on the gesture the overlay is used with most.
- **Why the first pass missed it.** It sampled the shape by hand, twice, and
  both times the stack happened to be collapsed to chips (`322x50`). The
  expanded state only showed up once the shape was logged continuously. A
  measurement of a state machine taken at one moment is a measurement of one
  state.
- **What is NOT reachable from a client here, checked layer by layer.** The
  owner asked for "get out of the way when the thing underneath is full
  screen". `org.gnome.Shell.Introspect.GetWindows` → `AccessDenied` (portals
  only). The session's Wayland globals (40 of them, enumerated over the socket)
  contain no `foreign_toplevel` and no `layer_shell`. XWayland sees only X
  windows and the browser is a Wayland client. So on GNOME an ordinary app
  cannot know another window went full screen; that needs a shell extension.
  `org.gnome.SessionManager.IsInhibited(12)` is the one standard signal in the
  area — true while a video is playing — but it means "something is inhibiting
  idle", not "full screen". Arming on touch was chosen instead because it needs
  none of this and is not GNOME-specific.

## After v0.8.0 — the card that survived a reboot, and the audit of 2026-09-02

Reported from the desktop it runs on (Ubuntu 26.04 / GNOME 50.1): three cards
up, two chats actually working, the third reading "Needs you · Waiting for your
reply" for a worktree session nobody could find. It was `91b7b58e`, written
last at 22:03 and the machine rebooted at 22:57; the card was six hours old when
reported. Root cause in one line: on Linux the pet never knew which process
owned a session, so nothing but the twelve-hour rule could ever retire one.
Fixed first, then a four-lens audit (Fable 5.1: lifecycle, overlay cost,
frontend, platform) whose findings follow. IDs: `L-` Linux, `T-` lifecycle,
`R-` running cost, `FE-` frontend, `X-` platform.

- **L-6** `process.rs:25`, `state.rs:679` — `process::owner()` and
  `is_alive()` were Windows-only; every Linux and macOS session ran with
  `agent_pid: 0`, "unknown" read as alive, and rule 1 of the sweep never
  fired. A waiting session is exempt from every other rule but twelve hours,
  so a reboot left "Needs you" up until then. FIXED — `/proc` on Linux (parent
  walk past shells to the first non-shell ancestor, identity `(pid, start
  ticks since boot)`, zombies count as gone, an unreadable `/proc/<pid>` that
  exists still counts as alive), `proc_pidinfo` on macOS by inspection only
  (the kernel's echo of the pid is the layout check; a mismatch degrades to
  "unknown"), and a fourth rule on every platform: a file written before the
  current boot (`btime`, `GetTickCount64`, `kern.boottime`, with a 60s margin
  for clock steps) is dead whatever it says. Every retirement is now logged
  with its rule. Verified live: the new binary retired seven pre-boot files at
  startup including the reported one, and the three live chats carry the pids
  of their real `claude` processes.
- **T-1** `hook.rs:275` — a subagent that outlives the answer keeps making
  tool calls on the parent's session id, and they counted as progress: the
  outcome cleared, the card went "Done" → "Delegating" with nothing that would
  ever set it right, and a permission prompt the *main* agent was sitting at
  was cleared by a subagent's `PreToolUse`. FIXED — progress is the main
  agent's own events; any subagent event after the outcome is bookkeeping.
- **T-2** `state.rs:23`, `hook.rs` — the five-minute rule assumed a hook per
  tool call; one `Bash` call may legally run ten minutes, and a live
  `claude --bg` session running a seven-minute loop was called "Stopped
  responding" twice while its process was alive. FIXED — `PreToolUse` records
  a `tool_deadline_ms` from the call's own `timeout` (ten minutes when none is
  declared, plus a minute for the hook after it; extends, never shortens), and
  the sweep does not read silence as death before it. Same for a `Stop` that
  kept working with background tasks listed, for up to an hour.
- **T-3** `hook.rs:254` — `turn_ended_ms` was zeroed only on a prompt-id
  change, so a stop-hook continuation froze the card's clock at the vetoed
  `Stop`: "12m" over a turn forty-six minutes in (live, `570dd5ed`). FIXED —
  main-agent progress inside the same turn restarts the clock, and
  `derive.js` reads the stamp only when the turn is actually over
  (`turnOver`), which also covers files written by older builds.
- **T-4** `hook.rs:415` — `event_ms = now` was written for a straggler too, so
  the high-water mark moved *backwards* and the ordering guard held for
  exactly one straggler; the second one un-finished the turn. FIXED — both
  stamps are monotonic.
- **T-5** `hook.rs:318` — a `PermissionRequest` landing after `Stop` raised a
  prompt on a finished card that nothing in that turn would ever clear.
  FIXED — not from a straggler.
- **T-6** `hook.rs:312` — a subagent's own prompt was recorded but only a
  main-agent event could clear it, so an answered prompt kept "Needs you" up
  for as long as the subagent ran. FIXED — `pending_agent` records who asked;
  that agent's next event, or any main-agent event, clears it.
- **T-7** `hook.rs:64` — `SessionEnd` deleted after a 200ms lock wait, which
  is the wait an event should give up after and not the one a delete should:
  a holder mid-write renamed the whole session back. FIXED — `SessionEnd`
  waits up to three seconds; it is async and nothing is waiting on it.
- **T-10** `state.rs:684` vs `how-it-works.md` — the doc said a waiting
  session is never retired by age; the code deleted at twelve hours
  regardless. FIXED both ways — exempt when the agent process is known and
  alive, aged out when it is not, and the doc says so.
- **T-11/T-14/T-15** — `background_tasks` also arrives on `SubagentStop`
  (comment fixed), a duplicate match arm, and `recent` entries stamped after
  the lock wait instead of with the event's own time. FIXED.
- **FE-1** `main.js:1606` — the boot notice ("Updated to …", "No hooks
  installed") was replaced by the first `get_sessions` one IPC round-trip
  after it was shown. FIXED — the fetch keeps the notice in front.
- **FE-3** `main.js:415` — the sweep clears the tool line on a stalled
  session but narration was shown regardless, so a "Stopped responding" card
  narrated Claude mid-thought (live, `59c3026f`). FIXED.
- **FE-4/FE-7** `main.js:635` — the tray blink needed a render inside a 30s
  freshness window, which an occluded window may never get; and a completion
  that flickered done → finishing → done blinked twice. FIXED — no freshness
  gate (stale completions are acknowledged at boot, which is what it
  guarded), and `celebrated` is keyed by outcome.
- **FE-5** `main.js:311` — Claude Code fires `idle_prompt` sixty seconds after
  every answer, so every finished chat became "Needs you", sorted above the
  working ones, and `acknowledge` refused it because the state was not
  settled: a card no click could dismiss, and the close button turned it into
  a chip. This is the live half of the reported card. FIXED — a finished turn
  whose only blocker is the idle notification is dismissable (and pre-acknowledged
  at boot when stale); a real permission prompt still is not. The state
  itself is unchanged: the card says what Claude Code said.
- **FE-6** `main.js:462` — "Done · 2 running" during the 2.5s display hold
  when the outstanding count arrived after the outcome showed. FIXED — the
  chip hides on a settled state.
- **FE-11/FE-13** — the 1s entrance fallback fired `syncHitRects` for a card
  already gone; `view.lastShown` written and never read. FIXED.
- **FE-16** `derive.js:60` — "Finishing · 1 running" turned green after five
  minutes while the background task was still running: the write-off for a
  count the overlay might have missed the completion of, and a running
  background task is exactly five minutes of silence. Reported by the owner
  2026-09-03. FIXED — an hour of patience when the hooks recorded the agent's
  process (a dead one is retired by the sweep within seconds, so the silence
  is the work), five minutes when nothing vouches for it. Measured live with a
  400s background task: sampled every 30s, the card read `finishing · 1` at
  323s, 354s and 384s, and drained to 0 on completion.
- **R-1** `pet.js:311` — `draw()` on every animation frame while frames change
  ~5×/s, through a `drop-shadow` filter; the WebKit process averaged 8.6% of
  a core over its lifetime against 1.3% at rest. FIXED — repaint only when
  the row/frame changed; a crossfade still paints every frame.
- **R-2** `narration.rs:118` — `decorate` opened, stat'ed and closed every
  session's transcript three times a second, three quarters of them for idle
  chats, while the docs said "only for sessions that are on screen". FIXED —
  a session that is not running, has nothing outstanding and has had no event
  since the last read is not reopened.
- **R-4** `app.rs:1217` — the heartbeat was an atomic write (create, write,
  rename) on every 300ms poll: 85% of the app's disk writes, for readers that
  allow six seconds. FIXED — every two seconds.
- **R-5** `main.js:762` — `set_hit_rects` (IPC, main-thread hop, cairo region,
  XShape request) on every render even when nothing moved. FIXED — diffed.
- **R-6** `derive.js:99`, `main.js` — the 800ms prompt debounce, the 2s settle
  and the display holds were only re-evaluated on the next emit or the 1s
  clock, so "Needs you" appeared up to a second late. FIXED — one timer aimed
  at the nearest deadline on every render.
- **R-7** `chats.rs:101` — the by-session map was rebuilt over ~275 records on
  every tick between 3s rescans. FIXED — built at scan time.
- **X-1** `doctor.rs:254`, `main.js:941,1261` — on Linux the hotkey row, the
  welcome line and the menu button all showed the Windows sentence ("every
  candidate chord is already taken"), and the doctor stayed amber on a correct
  install. FIXED — the platform's honest sentence, and `ok`.
- **X-2** `doctor.rs:335`, `desktop.rs:230` — a wrapper-script autostart entry
  (this desktop's, and the one L-3 stopped the pet from overwriting) was still
  a permanent warning whose advice, `pipsqueak autostart on`, replaced the
  wrapper with the bare binary. FIXED — reported as `ok` and left alone, and
  `set_autostart(true)` refuses to overwrite a present launcher.
- **X-3** `main.rs` — `pipsqueak doctor` prints the same report as the tray.
- **X-4** `desktop.rs:90` — `alert_on_waiting` was silent off Windows. FIXED —
  `canberra-gtk-play -i message`, falling back to `paplay` on the freedesktop
  file; `afplay` on macOS (untested).
- **X-5** `control.rs:110` — `pipsqueak control on` spawned the bare binary
  with inherited stdio and no session of its own: a Wayland-native pet (no
  always-on-top, blank tray menu) that died with the shell and held the `/pet`
  skill's stdout open. FIXED — through the login launcher when it is present
  and not this binary, `Stdio::null()` and its own process group on Unix.
- **X-6** `app.rs:1226` — `command.json` was drained without looking at its
  age, so a `quit` written in the last moment before a logout ran on the next
  login's first tick; and read and delete were separate, so a second command
  landing between them was deleted unread. FIXED — claimed by rename, ignored
  and logged past ten seconds.
- **X-7** `state.rs` — two 0-byte `running.json.<pid>.tmp` files were the
  tell of two hard stops (ext4 delayed allocation between create and rename).
  FIXED — swept at startup, with a log line saying the previous stop was not
  clean.
- **X-8** `app.rs:586` — 118 "started" lines and zero "quit" lines: a crash, a
  logout and a click on Quit were indistinguishable afterwards. FIXED — quits
  are logged with their source, drained commands too.
- **X-9** `main.rs:115` — `last-cli-result.txt` held 54 KB of headlines and
  answer text after `pipsqueak sessions`, on every platform. FIXED — not
  recorded for that command.
- **X-11** `desktop.rs:205` — an autostart entry disabled the XDG way
  (`Hidden=true`, what KDE writes) read as enabled. FIXED.
- **X-12** `project.rs:89` — a relative `gitdir:` pointer
  (`worktree.useRelativePaths`, git ≥ 2.48) read as a project called "..", and
  a bare repository's worktrees never matched. FIXED — resolved against the
  marker, and `<repo>.git/worktrees/` recognised.
- **X-13** `main.js:355` — the ↗ arrow was offered for a chat the desktop app
  had no record of, where on Linux the click did nothing and said nothing.
  FIXED — hidden unless there is a chat id or a title to raise by.

## After v0.8.0 — the stack that scrolled, and the arrow that did nothing, 2026-09-12

Reported from the desktop it runs on (Ubuntu 26.04 / GNOME 50.1, Wayland): the
cards "look very cut off at the bottom of the top", and the ↗ arrow opens a
Codex task but does nothing for a Claude Code one. Both reproduced, both fixed.

- **F-16 (FIXED)** `style.css` `#stack.scrolling` — the stack scrolled once it
  ran out of room, and a `column-reverse` box with `overflow-y: auto` is cut at
  both ends: the far card is sliced through the middle of its text at the top
  edge, and the near card loses its speech tail, because the tail is an
  `::after` hanging 7px *below* the box that is now clipping. Reproduced at
  `--stage-height: 360px` with five chats — which is what the largest pet size
  on a laptop actually produces, since the window is clamped to the work area
  and the scene is divided by the scale factor. Fixed by deleting the scroll:
  `fitStack` measures the children against the computed `max-height` and drops
  cards from the far end until they are under it, so the card nearest the pet
  is the last to go and is never cut. Dropped chats are still on screen as
  chips, and below that as a `+N` count. Ladder: cards, then chips, then the
  one-line form of the last card.
- **F-17 (FIXED)** `main.js` `fitStack` — the first cut of F-16 started from
  the full set on every render, so in the overflow case it built the card that
  did not fit, measured, and threw it away again three times a second. The
  fitted count is now a starting hint, and growth is gated on measured slack of
  at least one one-line card.
- **F-18 (WRONG — raised and reverted the same day)** `main.js` `render` — the
  pass above read "Hide the cards" literally, saw a chip left on screen for
  every live chat, and took them away so that hiding left the pet alone. That
  was a feature, not a leftover: the chip row IS the collapsed state, and
  clicking the pet to get one small pill per chat — still running, or finished —
  without a stack of cards in the corner is what the click is for. Reported
  within the hour by the person who uses it, reverted. The lesson is the cheap
  one: a label is not a specification, and a behaviour that looks redundant on a
  demo dataset may be the behaviour someone relies on. Ask before removing.
  What remains open from that observation, unchanged and untouched: a chip
  click does nothing while the cards are hidden, because the reveal it performs
  is gated on `config.showBubble`, which hiding has just set false. The comment
  on the handler says that is intended ("not at all when the user has the cards
  hidden"), so it stays a question for the owner rather than a fix.
- **L-7 (FIXED)** `desktop.rs::open_url` — on Linux the arrow handed the URL to
  `xdg-open` spawned from a worker thread. The deep link arrived and the app
  switched chats, but the window stayed behind whatever was in front and GNOME
  posted "Claude is ready" instead of raising it. That notification is the
  tell: it is what the compositor shows when a focus request carries no
  activation token. Measured both ways against a throwaway scheme handler that
  printed its environment: `xdg-open` → `XDG_ACTIVATION_TOKEN=<unset>`; a URI
  opened through a GDK launch context taken from the display →
  `XDG_ACTIVATION_TOKEN=da6dc733-…_TIME0`. Codex was unaffected because its
  window was usually being opened rather than raised, and a window being mapped
  for the first time competes with nothing for focus. Now opened through
  `gio::AppInfo::launch_default_for_uri` with that context, on the main thread,
  with a claim flag so a slow main thread cannot launch the URL twice.
- **L-8 (FIXED)** `desktop.rs::open_url` — the same call reported
  `spawn().is_ok()`, which is true whenever `xdg-open` itself starts, so the
  arrow could never report a failure and the frontend's "Could not open this
  task" notice was unreachable on Linux. `launch_default_for_uri` fails when
  nothing is registered for the scheme, which is the case worth telling: the
  desktop app is not installed.

## After v0.8.0 — the card that counted a task nobody was running, 2026-09-12

Reported live: a finished turn reading `Finishing · 1 running · 12 actions · 5m`
with nothing running. Checked against the machine rather than the card — no
shell, no subagent, no build; the one process that session had launched was the
overlay itself, reparented to systemd and not a child of anything.

- **L-9 (FIXED)** `narration.rs::decorate` — the session record held
  `tasks: ['slhnflide']`, an id that appears nowhere else: not in the
  transcript, not in any file under `~/.claude`, and with no process behind it.
  The count it produced could never fall, so every finished turn in that session
  read "Finishing · N running" instead of "Done", for as long as the session
  lived.

  **First diagnosis, and why it was wrong.** It looked like the hook had stopped
  mentioning the task: `tasks_ms` was half an hour stale across several
  completed turns, which would mean the correction simply never re-ran. Watching
  it longer disproved that — `tasks_ms` does advance, on every stop, still
  carrying the same id with `status: "running"`. Claude Code is not dropping the
  key; it is actively re-asserting a task that no longer exists. A one-turn
  expiry built on the first reading would therefore have changed nothing anyone
  could see: the count falls during the turn and is restored by the stop, which
  is the exact moment the card is read. Caught before shipping, but only because
  the numbers were re-read rather than assumed.

  **Fix.** The pet's own evidence outranks the report. The transcript is a file
  this process read; the list is something another process said. An id the
  transcript has never named gets one turn — the bound this file already
  claimed in prose, "cannot strand a card on 'still working' for more than one
  turn" — and is then not counted again unless the transcript does name it, at
  which point it is ordinary outstanding work with the normal lifecycle. The
  turn boundary is evaluated before the hook answer so the result cannot depend
  on whether a poll landed between a prompt and the stop after it.

  **The trade, stated plainly.** Async work of a shape the transcript does not
  record in a form this build recognises now counts for one turn instead of
  forever. That is the case the hook answer was added for. It is a real loss,
  taken deliberately: the alternative on the evidence is a card that is
  permanently wrong, and any shape the transcript does record — background
  shells, monitors, async subagents — is unaffected.

  Four regression tests, each failing with the expiry removed.
- **T-1 (FIXED, same change)** `narration.rs` tests — the new tests were flaky,
  4 failures in 12 runs, and only under the full module. `decorate` ends by
  dropping every watch whose path it was not given, which is correct in
  production, where one call sees every session on screen, and means two
  parallel tests wipe each other's state so a re-seed passes or fails on the
  scheduler. They take a shared lock now. 0 failures in 12 runs after.

## Parked (recorded, not fixed — reasons given)
- **T-9 (idle-prompt "Needs you" rank)** — a finished chat still sorts above
  working ones for the minute after its idle notification. Dismissable now
  (FE-5); ranking it below "running" is a product call the owner should make
  with the card in front of them.
- **R-3 (stat-gated session reads)** `state.rs:598` — 12 file reads and
  parses per 300ms tick, ~0.35% of a core. A `(mtime, len)` gate is unsafe:
  kernel timestamps are coarse (ms), two hook writes inside one tick with the
  same byte length would leave the second unread until the next change. Not
  worth a stale card for a third of a percent.
- **R-8/R-9** — any emit wakes the sprite loop for 15s even when the leader
  is unchanged (cheap now that R-1 repaints only on change); a hidden window
  still receives emits. Revisit if measured.
- **FE-8/FE-9/FE-12/FE-14/FE-15** — reorder restarts a card's standing dot
  animations; the expanded log rebuilds 24 `li` per tick; `cardsFor` mutates
  hold state when called from a click; two unreachable `idle` labels; the
  menu awaits five IPCs serially. Cosmetic.
- **T-12/T-13/T-16** — the Task-call `subagents: 1` has no consumer;
  `PROGRESS_EVENTS` and `ANSWERS_PROMPT` differ by the two compaction events
  only; the `event_ms` doc says "older loses" while counters from older
  events are deliberately kept. Cleanups.
- **X-10** — `write_settings` takes the umask mode rather than the original
  file's. **X-14** — `claude://resume` on Wayland selects the chat but cannot
  raise the app (no activation token through `gio open`); untested by hand.
  **X-15** — macOS: `autostart on` errors, the welcome button fails the same
  way; no macOS CI job. **X-16** — `tauri-action@v0` floats a major.

- **P-13 (counter increments rest on a best-effort lock)** `hook.rs:134`,
  `state.rs:477` — `FileLock::acquire` gives up after ~200ms and writes
  unlocked, and it will steal a lock older than 2s from a holder that is still
  alive, whose `Drop` then deletes the thief's lock. N parallel `PreToolUse`
  processes can therefore collapse N increments into one. Not seen in the
  captured payloads (376 events, no lost `tool_use_id`), and the fix — refusing
  to write rather than writing unlocked — trades a wrong count for a missing
  event, which needs its own measurement first.
- **P-14 (the hook does not retry a transient read)** `hook.rs:74` — `sweep`
  retries once for exactly this reason and the hook does not, so a file briefly
  held by an indexer loses that event's increment, or its whole outcome.
  One-line fix, deliberately separated from this pass so it can be measured.
- **P-15 (`PermissionDenied` is labelled "Auto-mode blocked")** `hook.rs:439` —
  the payload carries a `reason`, observed as `"Blocked by classifier"`. Since
  it is present, the label should quote it rather than assume the classifier;
  as it stands a denial from a `deny` rule or a permission hook would be
  attributed to auto-mode. Needs a sample of a non-classifier denial first.
- **P-16 (`Notification` fallback misses wordings)** `hook.rs:469` — with
  `notification_type` absent, only "permission", "waiting for your" and "input"
  are matched; "approval", "approve" and "confirm" are not recorded at all.

- **H-1/S-6 (general event-ordering guard)** — hook payloads carry no fire
  timestamp and process-spawn skew means arrival time cannot reconstruct fire
  order. The concrete harms are individually fixed (settle absorption, strict
  file creation, waiting cleared on Stop). A correct general guard needs a
  timestamp in the payload, which Claude Code does not provide. Residual: a
  straggler `PostToolUse` landing >2s after `Stop` briefly revives "running";
  self-limiting via the sweep.
- **H-4 (permission-prompt straggler)** — same missing-timestamp problem;
  bounded by the 800ms debounce plus clear-on-next-event. Rare, cosmetic.
- **W-3b (named mutex single-instance)** — heartbeat-at-setup plus the startup
  check covers the observed races; a named-mutex/plugin approach is the
  durable fix but adds a dependency and touches startup semantics. Do with the
  next dependency bump.
- **W-7 (show before first paint)** — plausible one-frame flash on some GPU
  paths only; deferring `show()` interacts with the watchdog's visibility
  check. Revisit if reported.
- **W-9 (hotkey late-success after 3s timeout)** — cosmetic mismatch between
  log and reality on very slow machines.
- **W-10 (`ensure_on_screen` vs live drag; config write races)** — needs a
  drag-state signal or a config-writer mutex; small blast radius.
- **F-7 (dense-mode hysteresis)** — main triggers (F-1 flap, F-5 notice) are
  fixed; add hysteresis only if the 3↔4 boundary still convulses in practice.
- **F-6 (`acknowledged` keyed on `outcome_ms`)** — with settle and ordering
  fixes `outcome_ms` is stable per completion; a genuine second completion is
  a new event and should reappear.
- **H-12 (grapheme-cluster truncation)** — needs a segmentation dependency for
  a cosmetic edge (emoji cut before the ellipsis).
- **H-13 (missing `session_id` funnels to `unknown.json`)** — version-skew
  only; harmless bucket.
- **S-13 (sanitize collisions)** — adversarial ids only; suffixing a hash
  changes on-disk filenames mid-upgrade.
- **I-3 (undocumented Stop extras)** — `background_tasks`/`session_crons`/
  `last_assistant_message` are not in the docs; the code already degrades
  gracefully when absent. Re-verify each Claude Code release.
- **I-6 (`is_ours` substring match)** — changing the match rule strands hooks
  installed by older builds; needs a migration story.
- **I-8 (settings.json read-modify-write race with Claude Code)** — needs
  retry-compare; rare, and the backup limits the damage.
- **I-11/I-12 (doctor wording nits)** — cosmetic.
- **I-14 (main.js has zero tests)** — the render/reconciliation layer needs a
  DOM test harness; out of scope for this pass, first candidate for the next.
- **I-15 (simulate.mjs payload drift)** — capture-and-replay real hook
  payloads; needs a recording session.
- **F-15 (browser-demo fallback)** — if `__TAURI_INTERNALS__` is ever absent
  the app silently runs the demo loop; worth an explicit banner eventually.
