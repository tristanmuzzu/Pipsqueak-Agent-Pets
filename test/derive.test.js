// The judgements that decide whether a card is telling the truth.
import test from 'node:test'
import assert from 'node:assert/strict'
import {
  DONE_LINGER_MS,
  WAITING_DEBOUNCE_MS,
  blockedOn,
  displayState,
  dismissable,
  petState,
  holdState,
  isNewer,
  relativeTime,
  runningCount,
  stillWorking,
  turnElapsed,
  worthCelebrating,
  CELEBRATE_AFTER_MS,
  OUTSTANDING_STALE_MS,
  OUTSTANDING_STALE_KNOWN_MS
} from '../src/derive.js'

const NOW = 1_000_000

/** A session file with nothing going on, for tests to bend one field of. */
const quiet = (fields = {}) => ({
  state: 'running',
  outcome: '',
  outcome_ms: 0,
  settles_ms: 0,
  waiting_since: 0,
  pending_since: 0,
  headline: 'Fix the timezone test',
  ...fields
})

test('a prompt inside the debounce is not yet worth showing', () => {
  const session = quiet({ pending_since: NOW - 200, pending_tool: 'Bash' })
  assert.equal(blockedOn(session, NOW), '')
  // '' means "keep showing whatever was already there", not "idle".
  assert.equal(displayState(session, NOW), '')
})

test('a prompt that outlives the debounce means a human is being asked', () => {
  const session = quiet({
    pending_since: NOW - WAITING_DEBOUNCE_MS - 1,
    pending_tool: 'Bash',
    pending_detail: 'run: npm install'
  })
  assert.equal(blockedOn(session, NOW), 'run: npm install')
  assert.equal(displayState(session, NOW), 'waiting')
})

test('an explicit notification outranks a pending permission', () => {
  const session = quiet({
    waiting_since: NOW - 5000,
    waiting_reason: 'Waiting for your reply',
    pending_since: NOW - 5000,
    pending_detail: 'run: npm install'
  })
  assert.equal(blockedOn(session, NOW), 'Waiting for your reply')
})

test('an unanswered Codex question needs you while working and after completion', () => {
  for (const fields of [{ state: 'running' }, { state: 'idle', outcome: 'done' }]) {
    const session = quiet({
      provider: 'codex',
      ...fields,
      waiting_since: NOW - 5000,
      waiting_reason: 'Which folder?'
    })
    assert.equal(blockedOn(session, NOW), 'Which folder?')
    assert.equal(displayState(session, NOW), 'waiting')
  }
})

test('a question keeps the pet orange regardless of card order or other activity', () => {
  const question = { key: 'question', state: 'waiting', session: quiet({ outcome: 'done' }) }
  for (const state of ['running', 'thinking', 'done', 'failed', 'idle']) {
    const other = { key: 'other', state, session: quiet({ outcome_ms: NOW }) }
    assert.equal(petState([other, question], 'other', NOW), 'waiting')
    assert.equal(petState([question, other], 'other', NOW), 'waiting')
  }
  question.state = 'running'
  assert.equal(petState([question], 'question', NOW), 'running')
  question.state = 'done'
  question.session.outcome_ms = NOW
  assert.equal(petState([question], 'question', NOW), 'done')
  assert.equal(petState([question], 'question', NOW + DONE_LINGER_MS), 'idle')
  assert.equal(petState([], undefined, NOW), 'idle')
})

test('completion acknowledgement cannot hide an unanswered async question', () => {
  const session = quiet({ outcome: 'done', codex_question_pending: true })
  assert.equal(dismissable('waiting', session), false)
  session.codex_question_pending = false
  assert.equal(dismissable('done', session), true)
  assert.equal(dismissable('waiting', session), true) // Claude idle notification.
  session.pending_since = NOW
  assert.equal(dismissable('waiting', session), false) // Actual approval prompt.
})

test('a background turn ending is not a card that needs you', () => {
  // The exact shape the hook now writes when a `claude --bg` leg ends a turn:
  // background, the turn over, and deliberately no `waiting_since` — the
  // supervisor that started the session resumes it, so there is nobody to
  // interrupt. This asserts the reader's half of that contract, because the
  // fix is a field the producer stopped writing and this is what would notice
  // if it ever came back.
  const session = quiet({
    background: true,
    state: 'idle',
    kind: 'Done',
    activity: 'Waiting to be resumed',
    outcome: 'done',
    outcome_ms: NOW - 10_000,
    settles_ms: NOW - 8_000,
    waiting_since: 0,
    waiting_reason: ''
  })
  assert.equal(blockedOn(session, NOW), '')
  assert.notEqual(displayState(session, NOW), 'waiting')
  assert.equal(displayState(session, NOW), 'done')
})

test('a background session at a permission prompt still needs you', () => {
  // The other direction, and the reason the fix is gated on *why* a session is
  // waiting rather than on whether it runs in the background. Nothing resumes
  // a background agent sitting on a permission prompt.
  const session = quiet({
    background: true,
    pending_since: NOW - WAITING_DEBOUNCE_MS - 1,
    pending_tool: 'Bash',
    pending_detail: 'run: rm -rf build'
  })
  assert.equal(blockedOn(session, NOW), 'run: rm -rf build')
  assert.equal(displayState(session, NOW), 'waiting')
})

test('a completion that has not settled is still a running turn', () => {
  const session = quiet({ state: 'running', outcome: 'done', outcome_ms: NOW, settles_ms: NOW + 2000 })
  assert.equal(displayState(session, NOW), 'running')
  assert.equal(displayState(session, NOW + 2001), 'done')
})

test('a settling turn stays on screen even though the producer wrote idle', () => {
  // The hook writes `state: "idle"` together with the outcome. Reporting that
  // idle during the settle window took the card out of the DOM for two
  // seconds and popped it back in as "done" — the flicker this window exists
  // to prevent.
  const session = quiet({ state: 'idle', outcome: 'done', outcome_ms: NOW, settles_ms: NOW + 2000 })
  assert.equal(displayState(session, NOW), 'running')
  assert.equal(displayState(session, NOW + 2001), 'done')
})

test('a turn that ended over running work is not done', () => {
  // The complaint this exists for: the assistant yields the floor while five
  // subagents run and its last words are that it is waiting for them. `Stop`
  // fires anyway, and no hook will ever fire for those finishing.
  const session = quiet({
    state: 'idle',
    outcome: 'done',
    outcome_ms: NOW,
    settles_ms: NOW,
    outstanding: 5,
    updated_ms: NOW
  })
  assert.equal(displayState(session, NOW + 3000), 'finishing')
  assert.equal(stillWorking(session, NOW + 3000), true)
  // Not worth a tray blink either: nothing has finished.
  assert.equal(worthCelebrating(session, NOW + 60_000), false)

  // The last one reports back, and only then is it done.
  const drained = { ...session, outstanding: 0 }
  assert.equal(displayState(drained, NOW + 3000), 'done')
})

test('outstanding work is written off once the session goes quiet', () => {
  // The overlay can start watching halfway through and see a launch whose
  // completion it already missed. A card stuck on "finishing" forever would
  // be its own kind of lie.
  const session = quiet({
    state: 'idle',
    outcome: 'done',
    outcome_ms: NOW,
    settles_ms: NOW,
    outstanding: 2,
    updated_ms: NOW
  })
  assert.equal(displayState(session, NOW + OUTSTANDING_STALE_MS - 1000), 'finishing')
  assert.equal(displayState(session, NOW + OUTSTANDING_STALE_MS + 1000), 'done')

  // With the agent process known, the sweep would have retired the session
  // had it died, so the silence is the background task still running. A
  // six-minute build used to turn the card green while it was still going.
  const vouched = quiet({
    state: 'idle',
    outcome: 'done',
    outcome_ms: NOW,
    settles_ms: NOW,
    outstanding: 1,
    updated_ms: NOW,
    agent_pid: 4242
  })
  assert.equal(displayState(vouched, NOW + OUTSTANDING_STALE_MS + 1000), 'finishing')
  assert.equal(runningCount(vouched, NOW + OUTSTANDING_STALE_MS + 1000), 1)
  assert.equal(displayState(vouched, NOW + OUTSTANDING_STALE_KNOWN_MS + 1000), 'done')
})

test('a completion is announced only once it has held', () => {
  // A blocking stop hook can veto a stop seconds later and send the turn back
  // to work. The card can correct itself; a tray blink cannot.
  const session = quiet({ state: 'idle', outcome: 'done', outcome_ms: NOW, updated_ms: NOW })
  assert.equal(worthCelebrating(session, NOW + 2000), false)
  assert.equal(worthCelebrating(session, NOW + CELEBRATE_AFTER_MS + 1), true)
})

test('a finished turn stays finished until somebody looks at it', () => {
  // No timer: a card that removes itself after 30s is a card that finished
  // while you were in a meeting and never told you.
  const session = quiet({ state: 'idle', outcome: 'done', outcome_ms: NOW })
  assert.equal(displayState(session, NOW + 1000), 'done')
  assert.equal(displayState(session, NOW + DONE_LINGER_MS * 10), 'done')
})

test('a failed turn stays visible too', () => {
  const session = quiet({ state: 'idle', outcome: 'failed', outcome_ms: NOW })
  assert.equal(displayState(session, NOW + DONE_LINGER_MS * 10), 'failed')
})

test('a session the sweep gave up on reads as idle, not finished', () => {
  // The sweep sets this when a running session has produced no event for
  // longer than any real turn goes quiet. It is neither a completion nor a
  // failure, and it must not outrank either.
  const session = quiet({ state: 'running', stalled: true })
  assert.equal(displayState(session, NOW), 'idle')
})

test('a question still being asked outranks having given up on the session', () => {
  // Both claims come from the same silence, and only one of them can be
  // checked: the prompt is a fact, "stopped responding" is an inference. A
  // session flagged stalled by an older build — or by any future rule — must
  // still read as waiting while the question is on the card, or the status
  // line contradicts the row underneath it.
  const session = quiet({
    state: 'running',
    stalled: true,
    pending_since: NOW - WAITING_DEBOUNCE_MS * 2,
    pending_tool: 'Bash',
    pending_detail: 'run: npm run deploy'
  })
  assert.equal(displayState(session, NOW), 'waiting')
  assert.equal(blockedOn(session, NOW), 'run: npm run deploy')
})

test('a state holds long enough to be read', () => {
  const view = {}
  const session = quiet()
  assert.equal(holdState(view, session, 'failed', NOW), 'failed')
  // Something less urgent cannot take the card away yet.
  assert.equal(holdState(view, session, 'running', NOW + 100), 'failed')
  // Something more urgent can, immediately.
  assert.equal(holdState(view, session, 'waiting', NOW + 100), 'waiting')
})

test('the hold does not survive the card saying something else', () => {
  const view = {}
  assert.equal(holdState(view, quiet({ headline: 'Add the eval suite' }), 'failed', NOW), 'failed')
  // A new headline is a new situation; holding the old verdict over new text
  // is a worse lie than the flicker the hold prevents.
  assert.equal(
    holdState(view, quiet({ headline: 'Added 12 scenarios; suite passes' }), 'done', NOW + 100),
    'done'
  )
})

test('only a genuinely newer release is worth mentioning', () => {
  assert.ok(isNewer('v0.6.0', '0.5.0'))
  assert.ok(isNewer('0.5.1', '0.5.0'))
  assert.ok(isNewer('1.0.0', '0.9.9'))
  assert.ok(!isNewer('0.5.0', '0.5.0'))
  assert.ok(!isNewer('0.4.9', '0.5.0'))
  // A release beats the prerelease it followed, but never the other way.
  assert.ok(isNewer('0.5.0', '0.5.0-rc.1'))
  assert.ok(!isNewer('0.5.0-rc.2', '0.5.0-rc.1'))
})

test('a nonsense version never triggers a nag', () => {
  // The candidate arrives over the network, so anything unparseable has to
  // fail closed rather than be guessed at.
  for (const junk of ['', null, undefined, 'latest', 'v1', '1.2', '<script>', '99.99.99.99']) {
    assert.equal(isNewer(junk, '0.5.0'), false, `${junk} should not count as newer`)
  }
})

test('ages read the way a glance expects', () => {
  assert.equal(relativeTime(0, NOW), '')
  assert.equal(relativeTime(NOW - 500, NOW), 'now')
  assert.equal(relativeTime(NOW - 90_000, NOW), '1m')
  assert.equal(relativeTime(NOW - 7_200_000, NOW), '2h')
})

test('the turn clock stops when the turn does', () => {
  // A two-minute turn that ended two minutes ago said "4m" and kept climbing,
  // because the elapsed was measured from the turn's start against a live
  // clock with nothing to stop it.
  const ran = quiet({
    state: 'idle',
    outcome: 'done',
    turn_started_ms: NOW - 240_000,
    turn_ended_ms: NOW - 110_000
  })
  assert.equal(turnElapsed(ran, NOW), '2m')
  // Ten minutes later it still took two minutes.
  assert.equal(turnElapsed(ran, NOW + 600_000), '2m')

  // A turn still running keeps counting, which is the whole point of it.
  const running = quiet({ turn_started_ms: NOW - 45_000, turn_ended_ms: 0 })
  assert.equal(turnElapsed(running, NOW), '45s')
  // Nothing to time before the first turn.
  assert.equal(turnElapsed(quiet({ turn_started_ms: 0 }), NOW), '')

  // A stop hook sent the same turn back to work: the outcome cleared, the
  // state is running again, and the stamp from the vetoed Stop is stale.
  const resumed = quiet({
    state: 'running',
    turn_started_ms: NOW - 120_000,
    turn_ended_ms: NOW - 60_000
  })
  assert.equal(turnElapsed(resumed, NOW), '2m')
  // The sweep gave up on it: frozen where the events stopped.
  const stalled = quiet({
    state: 'idle',
    stalled: true,
    turn_started_ms: NOW - 120_000,
    turn_ended_ms: NOW - 60_000
  })
  assert.equal(turnElapsed(stalled, NOW), '1m')
})

test('a count of running work goes quiet when it can no longer be checked', () => {
  const busy = quiet({ outstanding: 3, updated_ms: NOW - 1000 })
  assert.equal(runningCount(busy, NOW), 3)

  // The same claim, from a session that has said nothing for longer than any
  // real turn goes silent. The status word already writes this off; the chip
  // beside it used to read the raw number and sat there saying "7 running"
  // next to the word "Done".
  const quietForAges = quiet({
    outstanding: 7,
    updated_ms: NOW - OUTSTANDING_STALE_MS - 1,
    outcome: 'done',
    outcome_ms: NOW - OUTSTANDING_STALE_MS
  })
  assert.equal(displayState(quietForAges, NOW), 'done')
  assert.equal(runningCount(quietForAges, NOW), 0)
})

test('agent identity survives status changes and defaults old files to Claude', async () => {
  const { agentIdentity } = await import('../src/derive.js')
  assert.equal(agentIdentity({ session_id: 'legacy' }).id, 'claude')
  for (const state of ['thinking', 'running', 'waiting', 'done', 'failed']) {
    assert.deepEqual(agentIdentity({ provider: 'codex', state }), {
      id: 'codex', label: 'Codex', name: 'Codex'
    })
  }
  assert.equal(agentIdentity({ session_id: 'notice' }).label, '')
})
