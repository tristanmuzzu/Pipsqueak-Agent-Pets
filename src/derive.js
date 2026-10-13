// Turning a session file into the one word a card shows.
//
// Kept apart from the rendering because this is where every judgement about
// truthfulness lives: whether a prompt is real, whether a turn is finished,
// which of several sessions speaks for a project. Those are worth being able
// to test without a window.
//
// Every function takes `now` so the awkward moments (a debounce that has not
// elapsed, a completion that has not settled) can be examined directly rather
// than waited for.

/**
 * A turn can enter and leave "blocked" in a few hundred milliseconds when
 * auto-mode or a permission hook answers the prompt. Only a wait that persists
 * is worth telling anyone about.
 */
export const WAITING_DEBOUNCE_MS = 800
/**
 * How recently a turn must have ended for its card to count as news.
 *
 * A finished card is not on a timer: it stays until it is acknowledged, which
 * is the only thing that proves anyone saw it. This is the separate question of
 * whether a *newly seen* completion is worth a tray blink, so that starting the
 * overlay does not announce everything that finished this morning.
 */
export const DONE_LINGER_MS = 30_000
/** Quiet for this long and the pet visibly dozes off. */
export const SLEEP_AFTER_MS = 60_000

export const URGENT = new Set(['waiting', 'failed'])
export const ACTIVE = new Set([
  'thinking',
  'running',
  'waiting',
  'failed',
  'compacting',
  'finishing',
  'done'
])
/** The durable states a session file can report. */
export const RUNNING = new Set(['thinking', 'running', 'compacting'])
/**
 * States that mean work is still happening, whoever is doing it.
 *
 * `finishing` is the turn being over while the things it started are not:
 * subagents, a background command, a monitor. The assistant has stopped
 * talking, so nothing a hook can see is running, and the session is still
 * busy.
 */
export const WORKING = new Set(['thinking', 'running', 'compacting', 'finishing'])
/**
 * How long a session must be silent before its outstanding work is written
 * off.
 *
 * Every launch is recorded in the transcript and so is every completion, but
 * the overlay can start watching halfway through and see only one half. This
 * is the backstop for a session whose agent process is *not* known: the same
 * five minutes after which such a session is assumed dead, applied to the
 * work it started.
 */
export const OUTSTANDING_STALE_MS = 5 * 60_000
/**
 * The same backstop when the agent process is known and alive.
 *
 * A background build or a monitor produces no event for as long as it runs,
 * and five minutes of that turned "Finishing · 1 running" into "Done" with
 * the task still going — the card guessing the work had finished because it
 * had heard nothing, which is what a running background task sounds like. A
 * known process cannot be a ghost: the sweep retires the session within
 * seconds of it dying. So the only thing left to guard against is a missed
 * completion, and an hour bounds that.
 */
export const OUTSTANDING_STALE_KNOWN_MS = 60 * 60_000
/**
 * How long a completion must hold before it is worth interrupting anyone.
 *
 * A blocking stop hook — a review gate, a completion loop — can veto a stop
 * seconds after the fact and send the turn straight back to work. The card can
 * afford to be wrong for a moment and correct itself; a tray blink and an
 * unread dot cannot, because they are claims made to someone who is looking
 * somewhere else.
 */
export const CELEBRATE_AFTER_MS = 8_000

/**
 * Which state wins when a project has several, and how long each one holds the
 * card once shown.
 *
 * The hold is what makes the stack readable. Hook events arrive in bursts, and
 * without a floor the card can flick through three states faster than you can
 * read one, and "needs you" can vanish under a later "running" before you
 * ever look up.
 */
export const DISPLAY = {
  waiting: { rank: 5, hold: 2500 },
  failed: { rank: 4, hold: 4000 },
  done: { rank: 3, hold: 2500 },
  compacting: { rank: 2, hold: 1500 },
  running: { rank: 2, hold: 900 },
  thinking: { rank: 2, hold: 900 },
  finishing: { rank: 2, hold: 900 },
  idle: { rank: 0, hold: 0 }
}

export const priority = (state) => DISPLAY[state]?.rank ?? 1

/** An unanswered question owns the pet even when another card is first. */
export function petState(groups, preferredKey, now = Date.now()) {
  if (groups.some((group) => group.state === 'waiting')) return 'waiting'
  const leader = groups.find((group) => group.key === preferredKey)
  if (!leader) return 'idle'
  if (['done', 'failed'].includes(leader.state) &&
      now - (leader.session.outcome_ms || 0) >= DONE_LINGER_MS) return 'idle'
  return leader.state
}

/** Async questions remain actionable even after their turn finishes. */
export function dismissable(state, session) {
  if (session.codex_question_pending) return false
  return state === 'done' || state === 'failed' ||
    (state === 'waiting' && Boolean(session.outcome) && !session.pending_since)
}

/**
 * What the session is blocked on, once it has been blocked long enough to be
 * worth saying. Empty while the prompt is still inside the debounce.
 */
export function blockedOn(session, now = Date.now()) {
  if (session.waiting_since && now - session.waiting_since >= WAITING_DEBOUNCE_MS) {
    return session.waiting_reason || 'Waiting for you'
  }
  // A permission prompt Claude Code raised and nothing has resolved. Most are
  // answered by auto-mode within a few hundred milliseconds and never reach a
  // human, which is exactly what the debounce filters out.
  if (session.pending_since && now - session.pending_since >= WAITING_DEBOUNCE_MS) {
    return session.pending_detail || `Permission for ${session.pending_tool}`
  }
  return ''
}

/**
 * Collapse the three questions a session file answers (what is it doing, how
 * did the last turn end, is a human blocking it) into a single word, in that
 * order of urgency.
 *
 * Returns '' while a prompt is inside its debounce, meaning "keep showing
 * whatever was already there".
 */
export function displayState(session, now = Date.now()) {
  if (blockedOn(session, now)) return 'waiting'
  if (session.waiting_since || session.pending_since) return ''

  // Gave up waiting on it. Not a failure and not a completion: the turn just
  // stopped producing events.
  if (session.stalled) return 'idle'

  const outcome = session.outcome || ''
  if (outcome === 'done' && stillWorking(session, now)) {
    // The turn is over and the work is not. Claude Code fires `Stop` when the
    // assistant yields the floor, which it does while subagents run and while
    // a background command it is waiting on has not finished — and no hook
    // ever fires for either of those ending. Saying "Done" here is the pet's
    // loudest claim made at the one moment it cannot support it.
    return 'finishing'
  }
  if (outcome === 'done') {
    // Claude Code sends Stop while background work is still finishing. Until
    // the result settles it is still a running turn. The producer writes
    // `state: "idle"` alongside the outcome, so falling back to the stored
    // state here made the card leave the screen for the whole settle window
    // and pop back in as "done" — report a running turn instead.
    if (now < (session.settles_ms || 0)) {
      return RUNNING.has(session.state) ? session.state : 'running'
    }
    return 'done'
  }
  if (outcome === 'failed') return 'failed'
  return session.state || 'idle'
}

/**
 * Is work the turn started still running?
 *
 * Counted from the transcript, where every background command, monitor and
 * subagent is given an id when it starts and named again when it finishes.
 * Written off after a stretch of total silence — [`OUTSTANDING_STALE_MS`]
 * when nothing vouches for the agent process, [`OUTSTANDING_STALE_KNOWN_MS`]
 * when the hooks recorded it and the sweep would have retired the session
 * had it died — because a session that has said nothing for that long with
 * nobody checking is not waiting on anything, and a card stuck on
 * "finishing" would be its own kind of lie.
 */
export function stillWorking(session, now = Date.now()) {
  if (!(session.outstanding > 0)) return false
  const patience = session.agent_pid ? OUTSTANDING_STALE_KNOWN_MS : OUTSTANDING_STALE_MS
  return now - (session.updated_ms || 0) < patience
}

/**
 * Is this completion worth interrupting someone for yet?
 *
 * Separate from whether the card may show it. The card is allowed to be
 * provisional — it is in front of you and it corrects itself. A tray blink is
 * a tap on the shoulder, and taking one back is not possible.
 */
export function worthCelebrating(session, now = Date.now()) {
  if (stillWorking(session, now)) return false
  return now - (session.outcome_ms || 0) >= CELEBRATE_AFTER_MS
}

/**
 * Apply the minimum-display hold to a freshly derived state.
 *
 * `view` is mutated: it carries the state currently on screen, the earliest it
 * may be replaced, and the headline it belongs to. The headline matters:
 * holding "failed" over text that already says the turn succeeded is a worse
 * lie than the flicker the hold was preventing.
 */
export function holdState(view, session, raw, now = Date.now()) {
  const subject = session.headline || ''
  if (view.heldSubject !== subject) {
    view.heldSubject = subject
    view.heldState = ''
    view.heldUntil = 0
  }
  if (view.heldState && now < view.heldUntil && priority(raw) <= priority(view.heldState)) {
    return view.heldState
  }
  if (raw !== view.heldState) {
    view.heldState = raw
    view.heldUntil = now + (DISPLAY[raw]?.hold ?? 0)
  }
  return raw
}

/**
 * Is `candidate` a later release than `current`?
 *
 * Strict about what it accepts. This decides whether to tell
 * someone there is an update, based on a string from a network response, and
 * "probably newer" is not a good enough reason to nag.
 */
export function isNewer(candidate, current) {
  const parse = (value) => {
    const match = /^v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?$/.exec(String(value ?? '').trim())
    return match ? { parts: match.slice(1, 4).map(Number), pre: match[4] ?? '' } : null
  }
  const a = parse(candidate)
  const b = parse(current)
  if (!a || !b) return false
  for (let i = 0; i < 3; i += 1) {
    if (a.parts[i] !== b.parts[i]) return a.parts[i] > b.parts[i]
  }
  // Same numbers: a prerelease is older than the release it precedes, and one
  // prerelease is never worth interrupting anyone about.
  return Boolean(b.pre) && !a.pre
}

export function relativeTime(ms, now = Date.now()) {
  if (!ms) return ''
  const delta = Math.max(0, now - ms)
  if (delta < 1000) return 'now'
  if (delta < 60_000) return `${Math.floor(delta / 1000)}s`
  if (delta < 3_600_000) return `${Math.floor(delta / 60_000)}m`
  return `${Math.floor(delta / 3_600_000)}h`
}

export function duration(ms, now = Date.now()) {
  if (!ms) return '0s'
  const seconds = Math.max(0, Math.floor((now - ms) / 1000))
  if (seconds < 60) return `${seconds}s`
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`
  return `${Math.floor(seconds / 3600)}h`
}

/**
 * How long the turn on this card has been going, or how long it took.
 *
 * The card reads "3 actions · 4m", and both halves describe the turn. The
 * elapsed used to be computed live from `turn_started_ms` with nothing to stop
 * it, so a turn that took two minutes and finished two minutes ago said "4m",
 * and went on climbing for as long as anyone was looking at it. A number that
 * grows while nothing is happening is the pet inventing work.
 */
export function turnElapsed(session, now = Date.now()) {
  const started = session.turn_started_ms || 0
  if (!started) return ''
  return duration(started, turnOver(session) ? session.turn_ended_ms : now)
}

/**
 * Is the turn on this card over, so that its clock and counters are history?
 *
 * `turn_ended_ms` alone used to decide it, and it lied on a continued turn: a
 * stop hook sends the same turn back to work, the outcome clears, the state
 * goes back to running, and the stamp from the vetoed `Stop` stayed. The card
 * read "12m" over a turn forty-six minutes in. A turn is over when it said how
 * it ended, when the sweep gave up on it, or when it is simply not running.
 */
export function turnOver(session) {
  if (!session.turn_ended_ms) return false
  return Boolean(session.outcome) || Boolean(session.stalled) || !RUNNING.has(session.state)
}

/**
 * Is the "N running" count worth showing at all?
 *
 * The count is only ever a claim about *now*, so it has to be silent the
 * moment it stops being checkable. [`stillWorking`] already decides that for
 * the status word; the chip beside it was reading the raw number instead, so a
 * session that had been silent for an hour still carried "7 running" next to
 * the word "Done".
 */
export function runningCount(session, now = Date.now()) {
  return stillWorking(session, now) ? session.outstanding || 0 : 0
}

/** Identity is independent of status, and legacy hook files are Claude Code. */
export function agentIdentity(session) {
  if (session.session_id === 'notice') return { id: 'pipsqueak', label: '', name: 'Pipsqueak' }
  return session.provider === 'codex'
    ? { id: 'codex', label: 'Codex', name: 'Codex' }
    : { id: 'claude', label: 'Claude', name: 'Claude Code' }
}

/** Stable across agents and worktree sessions. Names are a legacy fallback. */
export function projectKey(session) {
  let root = (session.project_root || session.cwd || '').replaceAll('\\', '/').replace(/\/+$/, '')
  if (/^[a-z]:/i.test(root)) root = root.toLowerCase()
  return root ? `path:${root}` : `name:${session.project || session.session_id}`
}

export function sessionVisible(session, prefs) {
  if (session.session_id === 'notice') return true
  if (session.scratch && !prefs.showScratch) return false
  if (prefs.agentFilter && prefs.agentFilter !== 'all' && agentIdentity(session).id !== prefs.agentFilter) return false
  return !(prefs.hiddenProjects || []).includes(projectKey(session))
}

/** Reserve space for a pin and urgent work, even beyond the six-card limit. */
export function cardLayout(groups, wanted, { pinned = '', promoted = null } = {}) {
  const byKey = new Map(groups.map((g) => [g.key, g]))
  const pin = wanted.includes(pinned) ? pinned : null
  const urgent = wanted.find((key) => URGENT.has(byKey.get(key)?.state))
  const reserved = [...new Set([wanted.includes('notice') ? 'notice' : null, pin, urgent].filter(Boolean))]
  const ordered = [...reserved, ...wanted.filter((key) => !reserved.includes(key))]
  const dense = wanted.filter((key) => key !== 'notice').length > 3 || Boolean(pin && wanted.length > 2)
  const visible = ordered.slice(0, dense ? 6 : 3)
  const full = dense
    ? new Set(['notice', pin, urgent || (visible.includes(promoted) ? promoted : null) || pin || visible[0]].filter(Boolean))
    : new Set(visible)
  return { visible, full }
}
