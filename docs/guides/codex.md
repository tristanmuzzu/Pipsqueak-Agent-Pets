# Codex and Claude Code together

Pipsqueak automatically follows local Codex Desktop and CLI tasks, alongside
Claude Code sessions. Start the pet and start a turn in either agent. Claude
Code still needs its usual hooks; Codex needs no hooks, plugin, or config edits.

Every conversation has its own card, even when both agents use the same project.
**Codex is blue; Claude Code is orange**, and only the agent's name carries
that colour, on full cards, compact rows, and collapsed chips. The card's edge
says state instead: a bar down the left, the status dot and the status word all
take one colour per state (amber thinking, blue working, orange needs you, red
failed, green done), independently of the agent colour. The ↗ button opens
the card's conversation in its own desktop app when a desktop identity is known.
CLI-only Codex sessions have no desktop open button.

## Filters and pins

Right-click the pet to choose **All**, **Codex**, or **Claude**. **Projects** lets
you hide a repository across both agents. Hidden projects and filtered agents
produce no cards, greeting animations, sounds or tray alerts. Restore individual
projects or use **Show all projects**. These choices survive restarts.

Use the pin button on a card to keep that conversation visible and expanded,
even when it is idle or the stack is crowded. One conversation can be pinned.
Another task that needs attention still gets an expanded card. Closing the pin's
card unpins it; the menu also offers **Unpin conversation**. Filtering its agent
or project temporarily hides the pin without forgetting the choice.

The two connection rows at the top of the menu distinguish a live Codex
connection, local-log fallback, missing setup, and Claude hook traffic. An idle
live connection is different from an unavailable one. Claude hooks are event
based: **Hooks ready · quiet** means installed but no event in five minutes,
not a persistent connection to a Claude process. Hover for details.

## Local discovery

The pet reads `sessions/**/*.jsonl` and `session_index.jsonl` under `CODEX_HOME`,
or `~/.codex` by default. If Codex uses a custom home, launch Pipsqueak with the
same `CODEX_HOME`. Remote/cloud tasks without a local rollout are not tracked.
Internal review and subagent sessions do not get separate cards. Archive a task
in Codex and its card disappears when its rollout moves out of `sessions`.

New files and titles are discovered every three seconds. Existing files are
followed by the overlay's normal 300 ms poll. Only complete JSON lines are
consumed. Reads are bounded: at most 32 recently modified rollouts, 256 KiB per
file per poll, and the last 4 MiB on initial discovery of large logs. Startup can
take several polls while this tail is replayed. Incomplete/oversized records
are skipped safely; no transcript or config is modified.

Resuming a desktop task can create a new rollout file with the same conversation
ID. Only the newest rollout incarnation supplies its card, ordered by the
session metadata timestamp (then activity timestamp and path to break ties).
An older file's interruption cannot override later turns, even after restarting
the pet. While the replacement's bounded replay catches up, the old outcome
stays hidden. File modification times are used for discovery, not ownership.

The optional session index supplies the task title. When a Codex version doesn't
write it, the project and turn's prompt are the fallback. These local file
formats are best-effort integration points and may change with Codex releases.

## What the status means

- A `task_started` / `turn_started` event starts a turn and resets its counters.
- Assistant messages and public reasoning summaries provide narration, using
  the pet's existing Off / Speech / Thoughts setting. Encrypted reasoning and
  tool output are never used as narration.
- Model tool calls count once per call ID. Counts cover the visible tail; a
  partial count is marked `≥` when a start boundary isn't available. Elapsed
  time and counters are hidden if the turn start cannot be established.
- Only `task_complete` / `turn_complete` marks a turn Done. A final-answer
  message alone does not. This describes the Codex turn; detached external jobs
  are not independently tracked.
- An explicit interruption or terminal error is a failure, never Done.
- User-input tool calls show Needs you until the matching result arrives.
  Async questions keep **Needs you** and the question visible while Codex
  continues working, including after a turn completes. The tool's immediate
  acknowledgement does not count as an answer. Each question clears when its
  matching structured reply arrives. Your next ordinary chat message clears
  existing question reminders for that task too. Live Working/Idle status and
  agent progress leave them visible. Failed delivery clears that request's questions.
  An unanswered question keeps the pet orange even when another card is first
  or the cards are hidden. A completed turn alone cannot dismiss a question.
  Close its card with **× (Dismiss question reminder)** to dismiss the task's
  current questions in Pipsqueak, including after restarting the pet. New
  questions still alert. This leaves Codex's question forms available to answer.
  Codex's **Skip** button currently changes only its internal UI state, so
  Pipsqueak cannot observe it; use the card's × or send a chat message instead.
  On Linux and macOS, a passive connection to Codex Desktop's private local
  socket also observes `waitingOnApproval` and `waitingOnUserInput` flags.
  This covers desktop waits that are absent from rollouts. Resolving the wait
  clears it; a pause alone never invents a permission prompt. Windows and CLI
  sessions currently use rollout events only, which can miss approval waits.
- Five minutes without progress retires work as stopped responding; it never
  invents a successful completion. This can also happen during a long silent
  command when no live desktop status is available. A verified active desktop
  task stays active during silent work. Sessions with no rollout activity for
  twelve hours leave the candidate set.

The menu's **Check my setup** checks the Codex session folder separately from
Claude Code hooks. **Test the connection** accepts live activity from either
agent. `pipsqueak sessions` prints combined hook and rollout state for
troubleshooting; live desktop overrides belong to the running overlay.

## Live desktop connection

`src-tauri/src/codex_live.rs` follows up to 32 locally discovered desktop tasks
through `CODEX_HOME/ipc/ipc.sock` on Unix. It registers as a passive follower,
never takes ownership, starts work, approves tools, or answers questions. Only
runtime flags and stream revisions are retained, with a 16 MiB frame limit.
The existing user-private socket and directory are checked before connecting.

The connection is checked every fifteen seconds, with a five-second reply
limit, and reconnects after failures. Missing snapshots retry at most once every
thirty seconds. Disconnects, unsupported stream versions, revision gaps and
invalid status updates discard the live projection immediately; the rollout
adapter remains available. A live Idle status never manufactures Done.

This is an internal desktop protocol checked against the installed app, not a
stable public API. Future Codex versions may require an adapter update. The
menu explicitly shows **Local logs only** when the connection is unavailable.
The running pet's heartbeat includes aggregate connection counts for diagnosis,
without task names or conversation content.

## Implementation

`src-tauri/src/codex.rs` owns discovery, bounded replay, and the Codex state
machine. Its session IDs are prefixed with `codex:` to avoid collisions with
Claude hook files. The overlay merges the sources only after Claude's existing
transcript decoration; each parser sees its own format. Missing provider fields
in older session files continue to mean Claude Code.

The turn event aliases are described in the upstream
[Codex protocol](https://github.com/openai/codex/blob/main/codex-rs/docs/protocol_v1.md).
The desktop deep link (`codex://threads/<id>`) and paginated message shapes were
also checked against the locally installed Codex Desktop build during development.
