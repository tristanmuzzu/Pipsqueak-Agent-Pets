import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { PetRenderer, GREETING_ROW, JUMP_ROW } from './pet.js'
import { sceneSize } from './size.js'
import {
  ACTIVE,
  agentIdentity,
  projectKey,
  sessionVisible,
  cardLayout,
  CELEBRATE_AFTER_MS,
  DONE_LINGER_MS,
  RUNNING,
  SLEEP_AFTER_MS,
  URGENT,
  WAITING_DEBOUNCE_MS,
  WORKING,
  blockedOn,
  displayState,
  dismissable,
  holdState,
  isNewer,
  petState,
  relativeTime,
  runningCount,
  turnElapsed,
  turnOver,
  worthCelebrating
} from './derive.js'

/** `npm run dev` in a plain browser has no IPC; fall back to a demo loop. */
const IS_TAURI = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

/**
 * Two things only Windows can do, and the wording that depends on them.
 *
 * A global hotkey: Wayland has no way for an ordinary application to claim a
 * chord, and the Windows sentence ("every candidate hotkey is already taken")
 * sent people hunting for a program that did not exist. And raising a window
 * by its title, which is what the ↗ arrow falls back to for a chat the
 * desktop app has no record of: elsewhere that click did nothing and said
 * nothing, so the arrow is not offered.
 */
const IS_WINDOWS = typeof navigator !== 'undefined' && /Windows/.test(navigator.userAgent)
const HAS_GLOBAL_HOTKEY = IS_WINDOWS
const CAN_RAISE_BY_TITLE = IS_WINDOWS
const NO_HOTKEY_HERE =
  'No global hotkey on this desktop. Bind "pipsqueak control toggle" to a key in your system keyboard settings.'

/** Replaced at build time from package.json. See vite.config.js. */
const APP_VERSION = typeof __APP_VERSION__ === 'string' ? __APP_VERSION__ : '0.0.0'


const el = {
  stack: document.getElementById('stack'),
  chips: document.getElementById('chips'),
  menu: document.getElementById('menu'),
  panel: document.getElementById('panel'),
  panelTitle: document.getElementById('panel-title'),
  panelBody: document.getElementById('panel-body'),
  panelClose: document.getElementById('panel-close'),
  pet: document.getElementById('pet'),
  hint: document.getElementById('hint'),
  template: document.getElementById('card-template'),
  chipsMore: document.getElementById('chips-more')
}

const appWindow = IS_TAURI ? getCurrentWindow() : null
const renderer = new PetRenderer(el.pet)

let config = {
  pet: 'byte',
  scale: 2,
  clickThrough: false,
  showBubble: true,
  showScratch: false,
  agentFilter: 'all',
  hiddenProjects: [],
  pinnedSession: '',
  alertOnWaiting: false,
  flashOnFinish: true,
  quiet: false,
  /** off | speech | thoughts. See narration.rs. */
  narrate: 'thoughts',
  welcomed: false,
  updateCheck: false,
  updateDismissed: ''
}
/** Keep DOM geometry and native hit rectangles in the same coordinate space. */
function applyScale() {
  const size = sceneSize(config.scale, window.innerWidth, window.innerHeight)
  const stage = document.getElementById('stage')
  stage.style.setProperty('--ui-scale', size.factor)
  stage.style.setProperty('--stage-width', `${size.width}px`)
  stage.style.setProperty('--stage-height', `${size.height}px`)
  renderer.setScale(2)
  renderer.wake()
  syncHitRects()
}

let sessions = []
/** When any project was last doing something, for the doze. */
let lastLiveAt = Date.now()
/**
 * The idle gate.
 *
 * An overlay that is always on screen is always being composited, and
 * anything on it that moves — the sprite loop, the doze, a blinking dot —
 * keeps WebKit repainting at frame rate around the clock. Fifteen seconds
 * after the last thing actually happened (a session event, a notice, the
 * cursor arriving) the page settles: `body.settled` switches the standing
 * CSS animations off, the sprite loop parks itself (see pet.js), and the
 * once-a-second clock render drops to once every ten. The very next event
 * takes it all back instantly. The pet loses nothing by holding still:
 * colour and text carry every state, which is the same judgement the
 * reduced-motion rule in style.css already made.
 */
const SETTLE_UI_AFTER_MS = 15_000
let lastEventAt = Date.now()
let uiSettled = false

/** Something happened: reopen the gate before anyone can see it closed. */
function markActivity() {
  lastEventAt = Date.now()
  if (!uiSettled) return
  uiSettled = false
  document.body.classList.remove('settled')
}

function updateSettled() {
  const idle = Date.now() - lastEventAt > SETTLE_UI_AFTER_MS
  if (idle === uiSettled) return
  uiSettled = idle
  document.body.classList.toggle('settled', idle)
}
let notice = null
let noticeTimer = null
/**
 * How long the cursor has to rest on the pet before it explains itself.
 *
 * The operating system's own tooltips wait about half a second, which is short
 * enough to fire while you are reaching past the pet for something behind it.
 */
const HINT_DELAY_MS = 2500
let hintTimer = null
/** Whether the cursor is on the pet right now. See greetPet. */
let hovering = false
/**
 * How long a departure reported by the window waits to be believed. Long
 * enough to be cancelled by the movement that proves the cursor is still here,
 * short enough that nobody sees the hint linger. See maybeLeftPet.
 */
const LEAVE_GRACE_MS = 150
let leaveTimer = null
/** Where the cursor was last reported, as "x,y". See maybeLeftPet. */
let lastPointerAt = ''
/** Where it was when the window said it had gone, or "" if it has not. */
let leftFrom = ''

let stackHidden = false
let seenSessions = new Set()

/**
 * Card order is a property of the UI, not of the data. Sorting by "most
 * recently updated" on every hook event made the visible set churn every few
 * hundred milliseconds. A chat keeps its slot until it goes quiet.
 */
let slots = []
const views = new Map()
const collapsed = new Set()
const cards = new Map()
const chipNodes = new Map()

/**
 * Completions that have been seen.
 *
 * A finished card has no timer on it: it says "Done" until somebody
 * acknowledges it, and then it goes away entirely rather than shrinking to a
 * chip, because a chip is a thing that is still going on. Keyed by the turn
 * rather than the project, so the *next* completion in the same project is
 * news again.
 */
const acknowledged = new Set()
/**
 * The chat holding the one full card while the stack is collapsed.
 *
 * Null means "whichever is nearest the pet", which is the busiest or most
 * urgent one. Clicking a collapsed line hands it the card, because the thing
 * you just clicked is by definition the thing you want to read.
 */
let promoted = null
const outcomeKey = (session) => `${session.session_id}@${session.outcome_ms || 0}`
const settled = (state) => state === 'done' || state === 'failed'
/**
 * An urgent state that is actually asking for something.
 *
 * The same distinction `dismissable` makes, from the other side. A real
 * permission prompt is a question being asked and takes its card back; the
 * idle notification Claude Code fires sixty seconds after every answer is not,
 * and must not.
 *
 * THE BUG THIS EXISTS FOR
 *
 * The card by the pet could not be closed during a live chat. Closing it
 * collapsed it to a chip correctly, and then about a minute later it was back,
 * and a minute after that, for as long as the conversation went on — because
 * every arrival of that idle "Waiting for your reply" counted as a transition
 * into an urgent state, and the reveal below un-collapses on any of those. It
 * also un-hid the whole stack, so clicking the pet did not stick either.
 */
const demandsAttention = (state, session) => URGENT.has(state) && !dismissable(state, session)
/** Recent enough that finishing is still news rather than a standing fact. */
const isFresh = (session) => Date.now() - (session.outcome_ms || 0) < DONE_LINGER_MS

// --- data ---------------------------------------------------------------
function viewFor(key) {
  let view = views.get(key)
  if (!view) {
    view = {
      expanded: false,
      // What to fall back to before anything is known. "running" was a guess
      // dressed as a fact: a session whose first event is a permission prompt
      // spends the debounce with no derivable state, and seeding this claimed
      // it was working — a card, a slot, and the pet animating as busy, on no
      // evidence at all.
      lastStable: 'idle',
      wasUrgent: false,
      // The narrower edge: urgent AND actually asking for something. Its own
      // field so an idle notification cannot swallow the edge a real
      // permission prompt needs.
      wasDemanding: false,
      // A finished turn nobody has acknowledged yet.
      unread: false,
      // Which completion has already been announced (its outcome key), so a
      // card that sits there finished does not blink the tray on every render
      // tick — and a completion that flickered through "finishing" and back
      // does not blink twice.
      celebrated: '',
      // The state currently on screen, and the earliest it may be replaced.
      heldState: '',
      heldUntil: 0
    }
    views.set(key, view)
  }
  return view
}

/** What to call a session when it has no chat title of its own yet. */
const projectName = (session) => session.project || session.session_id.slice(0, 8)

/**
 * One card per chat.
 *
 * These used to be grouped by project, with the busiest session speaking for
 * the rest. Two chats open on the same repository are both busy, so the card
 * swapped identity every time the other one ran a tool: title, narration,
 * subagent count and elapsed time all belonged to whichever chat had moved
 * last. It read as one card flickering between two chats, or as a chat being
 * removed. A chat is the thing a person has open, so a chat is what gets a
 * card; the project name stays on it as context.
 */
function cardsFor(list) {
  const out = []
  for (const session of list) {
    if (!sessionVisible(session, config)) continue
    const key = session.session_id
    const state = effectiveState(key, session)
    out.push({
      key,
      session,
      state,
      live: ACTIVE.has(state) || key === config.pinnedSession,
      // The chat's own name when the desktop app knows it, and the project
      // otherwise: what a one-line chip has room to say.
      label: session.chat_title || projectName(session)
    })
  }
  return out
}


function effectiveState(key, session) {
  const view = viewFor(key)
  const raw = displayState(session) || view.lastStable
  view.lastStable = RUNNING.has(raw) || raw === 'idle' ? raw : view.lastStable
  // Acknowledged means gone, immediately and without a farewell: holding the
  // state for another two seconds would make the click feel unheard.
  if (dismissable(raw, session) && acknowledged.has(outcomeKey(session))) {
    view.heldState = ''
    view.heldUntil = 0
    return 'idle'
  }
  return holdState(view, session, raw)
}

/**
 * Keeps a line on screen long enough to be read.
 *
 * The tool line can change three times in a second during a burst of reads.
 * Codex's overlay gets away without this because a model narrating itself
 * changes its mind every twenty seconds; ours falls back to tool calls, which
 * do not. A floor of a second is the difference between a line and a flicker.
 */
const LINE_HOLD_MS = 1000

/**
 * What the main line says when the session has said nothing at all.
 *
 * Rare, and worth handling anyway: a session that has only just started, or one
 * whose transcript the overlay cannot read, would otherwise leave the biggest
 * text on the card blank.
 */
function restingLine(state) {
  if (state === 'waiting') return 'Waiting for you'
  if (state === 'finishing') return 'Waiting for the work it started'
  if (state === 'done') return 'Finished'
  if (state === 'failed') return 'The turn failed'
  if (state === 'idle') return 'Nothing running'
  return 'Working…'
}

function holdLine(view, line, now = Date.now()) {
  // Nothing to say is an answer. Returning the last line instead kept a
  // sentence from a previous turn as the biggest text on the card, underneath
  // a status line reading "Stopped responding" — the sweep clears `activity`
  // precisely so the card stops claiming that work is in progress, and this
  // put it straight back.
  if (!line) {
    view.line = ''
    view.lineUntil = 0
    return ''
  }
  if (view.line === line) return line
  if (view.line && now < (view.lineUntil ?? 0)) return view.line
  view.line = line
  view.lineUntil = now + LINE_HOLD_MS
  return line
}

/**
 * Shows or hides the whole stack, and remembers which.
 *
 * `show_bubble` was documented, stored, loaded and saved, and read by nothing:
 * the real switch was a variable that reset on every restart and every
 * watchdog reload, so "hide the cards" quietly came undone. One decision, one
 * place, and the setting means what the guide says it means.
 *
 * `persist` is the second half of that fix. The stack gets revealed for two
 * very different reasons and only one of them is a preference:
 *
 *   - the user chose it (tray menu, pet click)        -> persist
 *   - something urgent took the card back by itself   -> DO NOT persist
 *
 * Without that distinction the auto-reveal below rewrote `show_bubble` to true
 * the first time any session needed attention, which on this machine meant
 * every login: Claude Code starts a turn, the card comes back on its own, and
 * "hide the cards" is undone before the desktop has even finished drawing.
 * Hiding stayed fixed for exactly as long as nothing happened. The runtime
 * reveal still works — it just no longer votes on what the user wants.
 */
function setStackHidden(hidden, { persist = true } = {}) {
  if (stackHidden === hidden) return
  stackHidden = hidden
  if (!persist) return
  config.showBubble = !hidden
  saveConfig()
}

/** Marks a finished turn as seen, which is what removes its card. */
function acknowledge(key) {
  const card = cardsFor(sessions).find((candidate) => candidate.key === key)
  if (!card || !dismissable(card.state, card.session)) return false
  // The display hold can keep "done" on screen after a new turn already
  // cleared the outcome. There is nothing to acknowledge then, and claiming
  // success anyway made the click do nothing at all: not dismissed, not
  // expanded, not collapsed.
  if (!card.session.outcome) return false
  acknowledged.add(outcomeKey(card.session))
  viewFor(key).unread = false
  return true
}

// --- rendering ----------------------------------------------------------
function buildCard(key) {
  const node = el.template.content.firstElementChild.cloneNode(true)
  // The entrance animation rides a one-shot class: kept on the .card rule it
  // replayed from opacity 0 whenever the card was reordered, because moving a
  // connected node re-inserts it and re-insertion restarts its animations.
  node.classList.add('card-enter')
  const entered = () => {
    node.classList.remove('card-enter')
    // The rect measured mid-rise is a few pixels off; measure again now that
    // the card is where it will stay. Not for a card already gone: the 1s
    // fallback below outlives a card dismissed inside its entrance.
    if (node.isConnected) syncHitRects()
  }
  node.addEventListener('animationend', (event) => {
    if (event.target === node) entered()
  })
  // A hidden window pauses animations, so animationend may never come; the
  // class must still die or every queued entrance replays on unhide.
  setTimeout(entered, 1000)
  node.querySelector('.close').addEventListener('click', async (event) => {
    event.stopPropagation()
    const session = sessions.find((candidate) => candidate.session_id === key)
    if (session?.codex_question_pending) {
      try {
        if (IS_TAURI) {
          sessions = await invoke('dismiss_codex_questions', { sessionId: key })
        } else {
          Object.assign(session, { codex_question_pending: false, waiting_since: 0, waiting_reason: '' })
        }
        const view = viewFor(key)
        view.heldState = ''
        view.heldUntil = 0
      } catch {
        showNotice('Could not dismiss this question. Please try again.')
        return
      }
    }
    if (config.pinnedSession === key) { config.pinnedSession = ''; saveConfig() }
    // Closing a finished card dismisses it outright. Leaving a chip behind
    // would keep a project in the row that means "still going on".
    if (!acknowledge(key)) collapsed.add(key)
    render()
  })
  node.querySelector('.pin').addEventListener('click', async (event) => {
    event.stopPropagation()
    config.pinnedSession = config.pinnedSession === key ? '' : key
    collapsed.delete(key)
    await saveConfig()
    render()
  })
  node.querySelector('.focus').addEventListener('click', (event) => {
    event.stopPropagation()
    const card = cardsFor(sessions).find((candidate) => candidate.key === key)
    // The key *is* the session, so this always opens the chat on the card
    // rather than whichever chat in the project happened to move last.
    invoke('focus_session', {
      sessionId: key,
      project: card ? projectName(card.session) : '',
      workspace: card?.session.workspace ?? ''
    }).then((opened) => {
      if (!opened) showNotice('Could not open this task. Check that its desktop app is installed.')
    }).catch(() => showNotice('Could not open this task. Try opening it from its desktop app.'))
  })
  node.addEventListener('click', () => {
    const view = viewFor(key)
    // Three things one click can mean, in order of how sure we are of it.
    //
    // Finished — genuinely finished, with nothing the turn started still
    // running — means you have seen it, so it goes, and it goes completely
    // rather than shrinking to a chip: a chip is a thing that is still going
    // on. Swatting a green card away without aiming at anything is the whole
    // gesture, and it is the one the pet is used with most.
    //
    // Nothing is lost that the card was still holding: the session file keeps
    // its last two dozen actions, and the card comes back on its own the
    // moment that chat starts working again. This deliberately does not apply
    // while a card reads "Finishing", because that turn is not over.
    if (acknowledge(key)) {
      // Nothing else to do; the card is on its way out.
    } else if (node.dataset.density === 'compact') {
      promoted = key
    } else {
      view.expanded = !view.expanded
    }
    view.unread = false
    invoke('clear_attention').catch(() => {})
    render()
  })
  return node
}

function paintCard(node, card, compact = false) {
  const { session, key, state } = card
  const close = node.querySelector('.close')
  close.title = session.codex_question_pending ? 'Dismiss question reminder' : 'Close'
  close.setAttribute('aria-label', close.title)

  const agent = agentIdentity(session)
  node.dataset.provider = agent.id
  const pin = node.querySelector('.pin')
  pin.hidden = key === 'notice'
  const pinned = config.pinnedSession === key
  node.dataset.pinned = String(pinned)
  pin.setAttribute('aria-pressed', String(pinned))
  pin.title = pinned ? 'Unpin conversation' : 'Pin conversation'
  pin.setAttribute('aria-label', pin.title)
  node.querySelector('.agent-label').textContent = agent.label
  node.dataset.state = state
  node.dataset.density = compact ? 'compact' : 'full'
  node.querySelector('.project').textContent = projectName(session)
  node.querySelector('.age').textContent = relativeTime(session.updated_ms)

  const workspace = node.querySelector('.workspace')
  workspace.hidden = !session.workspace
  workspace.textContent = session.workspace || ''

  const view = viewFor(key)
  node.querySelector('.unread').hidden = !view.unread

  // What the app itself calls this chat, when it knows: the title in its own
  // sidebar, written by the thing that has read the whole conversation. A
  // prompt's first line is a fair guess at the subject; this is the answer.
  node.querySelector('.title').textContent =
    session.chat_title || session.headline || ''
  node.querySelector('.focus').hidden = !session.chat_id && (agent.id === 'codex' || !CAN_RAISE_BY_TITLE)
  node.querySelector('.focus').title = `Open this task in ${agent.name}`

  // What it is doing right now, which is the line the card is really for. Its
  // own sentence when it has said one, and the tool line when it has not:
  // "Editing render.js" is the category of the work, "Now the frontend: chat
  // titles and the done card" is the point of it. Written twice because a
  // collapsed card shows it on the head row instead of under it.
  // A stalled session's last sentence is not what it is doing now: the
  // sweep clears the tool line for exactly that reason, and the narration
  // has to go with it or the biggest text on a "Stopped responding" card is
  // Claude mid-thought.
  const line = session.stalled ? '' : session.narration || session.activity || ''
  const said = holdLine(view, line) || restingLine(state)
  node.querySelector('.narration').textContent = said
  node.querySelector('.line').textContent = said

  // What it is blocked on, in its own row. This is the only text on the card
  // that is worth interrupting something else to read.
  const ask = node.querySelector('.ask')
  // The reason is sticky for as long as the card reads "needs you". The
  // minimum-display hold can keep that state up for a couple of seconds after
  // the prompt is answered, and "Needs you" with nothing underneath it is a
  // question the card has stopped being able to answer.
  const live = blockedOn(session)
  if (live) view.lastReason = live
  const reason = state === 'waiting' ? live || view.lastReason || '' : ''
  const risk = node.querySelector('.risk')
  const askWasHidden = ask.hidden
  ask.hidden = !reason
  // The unfold is a one-shot for the same reason as the card entrance: it
  // must play when the row appears, not every time the card moves.
  if (askWasHidden && !ask.hidden) {
    ask.classList.add('ask-enter')
    const entered = () => ask.classList.remove('ask-enter')
    ask.addEventListener('animationend', entered, { once: true })
    setTimeout(entered, 1000)
  }
  node.querySelector('.ask-what').textContent = reason
  // Cleared rather than left behind: a stale warning that reappears with the
  // next prompt would be attached to the wrong command.
  risk.hidden = !reason || !session.pending_risk
  risk.textContent = reason && session.pending_risk ? `⚠ ${session.pending_risk}` : ''

  // Two counts that say how much is going on without any text changing:
  // subagents running, and tools that failed and were worked around.
  // Work this turn started and is still waiting on. Counted from the
  // transcript rather than from the hooks: the hook counter only ever saw
  // subagents, never a background command, and nothing decrements it when a
  // subagent ends without an event, so it drifted upward and then contradicted
  // the status line on the same card. One count, one source.
  //
  // Hidden while the card reads "Finishing", where the status line is already
  // saying the same number in more words.
  //
  // Gated on the same write-off the status word uses, not on the raw number.
  // Reading the number directly is how a card that had been silent for an hour
  // carried "7 running" next to the word "Done".
  const outstanding = runningCount(session)
  const subagentChip = node.querySelector('.subagents')
  subagentChip.hidden = outstanding < 1 || state === 'finishing' || settled(state)
  subagentChip.textContent = `${outstanding} running`
  subagentChip.title = 'Background commands and subagents this turn started and has not been told are finished'
  // Calls auto-mode refused this turn. Tool calls that merely failed used to
  // be counted here instead, which read as "Claude keeps getting things
  // wrong" — noise, and not something anyone can act on. A refusal is: it
  // means the permission rules are the thing in the way.
  const blocked = session.blocked ?? 0
  const blockedChip = node.querySelector('.blocked')
  blockedChip.hidden = blocked < 1
  blockedChip.textContent = `${blocked} blocked`
  // "this turn" is only true while the turn is running. Once it is over the
  // chip is describing something that has finished, and saying so is the
  // difference between a fact and a claim that quietly expired.
  const calls = `tool ${blocked === 1 ? 'call' : 'calls'}`
  blockedChip.title = turnOver(session)
    ? `Auto-mode refused ${blocked} ${calls} in that turn`
    : `Auto-mode refused ${blocked} ${calls} this turn`

  // Status line: a coarse word plus counters. The detail that used to live here
  // moved to the expanded panel, where fast-changing text is fine.
  node.querySelector('.kind').textContent = kindLabel(state, session)
  // Counters only mean something once a turn has actually started; showing
  // "0 actions" before the first prompt is noise.
  const turnStarted = session.turn_started_ms || 0
  const actions = session.turn_tools ?? 0
  const showCounters = turnStarted > 0
  for (const part of node.querySelectorAll('.sep, .actions, .elapsed')) {
    part.hidden = !showCounters
  }
  if (showCounters) {
    node.querySelector('.actions').textContent = `${session.turn_tools_partial ? '≥' : ''}${actions} ${actions === 1 ? 'action' : 'actions'}`
    node.querySelector('.elapsed').textContent = turnElapsed(session)
  }
  // The exact call, and what the turn was asked for, are both still one hover
  // away without occupying a row.
  node.title = [session.headline, session.activity].filter(Boolean).join(' — ')

  const more = node.querySelector('.more')
  more.hidden = !view.expanded
  if (view.expanded) {
    const detail = node.querySelector('.detail')
    detail.hidden = !session.detail
    detail.textContent = session.detail || ''
    node.querySelector('.log').replaceChildren(
      ...(session.recent ?? [])
        .slice()
        .reverse()
        .map((entry) => {
          const item = document.createElement('li')
          item.dataset.state = entry.state
          const when = document.createElement('span')
          when.textContent = relativeTime(entry.ms)
          const what = document.createElement('span')
          what.textContent = entry.text
          item.append(when, what)
          return item
        })
    )
  }
}

function kindLabel(state, session) {
  // "Needs you" outranks "Stopped responding" deliberately. They are both
  // claims about silence, and only one of them can be checked: a prompt that
  // is still pending is a fact, while "stopped responding" is an inference
  // drawn from the same silence a pending prompt causes. A card that shows the
  // question and calls the session dead in the same breath is worse than
  // either alone. The sweep no longer stalls a waiting session either; this is
  // the second lock on the same door.
  if (state === 'waiting') return 'Needs you'
  if (session.stalled) return 'Stopped responding'
  if (state === 'failed') return session.event === 'turn_aborted' ? 'Interrupted' : 'Failed'
  if (state === 'finishing') {
    // The count is the whole point: it is the reason this is not "Done". Kept
    // short because it shares the status line with the counters, and the
    // longer phrasing was the half that got truncated away.
    //
    // Zero is possible and must not be printed: the word is held on screen for
    // a moment after the state that earned it, so the last of the work can
    // drain inside the hold and leave "Finishing · 0 running" — the count
    // contradicting the word it exists to justify.
    const running = runningCount(session)
    return running > 0 ? `Finishing · ${running} running` : 'Finishing'
  }
  if (state === 'done') return 'Done'
  if (state === 'compacting') return 'Compacting'
  // `kind` describes work in progress, so it is only true while there is some.
  // Falling back to it once the session went quiet is how a card that had
  // finished ended up saying "Delegating" with nothing running.
  if (state === 'idle') return session.outcome === 'done' ? 'Done' : 'Idle'
  return session.kind || 'Working'
}

function buildChip(key) {
  const button = document.createElement('button')
  button.type = 'button'
  const dot = document.createElement('span')
  dot.className = 'dot'
  const text = document.createElement('span')
  text.className = 'label'
  const agent = document.createElement('span')
  agent.className = 'agent-label'
  button.append(dot, agent, text)
  button.addEventListener('click', () => {
    // Reveal to show THIS card, not a vote to show cards from now on — and not
    // at all when the user has the cards hidden.
    if (config.showBubble) setStackHidden(false, { persist: false })
    collapsed.delete(key)
    if (!slots.includes(key)) slots.unshift(key)
    render()
  })
  return button
}

/**
 * Re-render the moment a debounce, a settle or a hold expires.
 *
 * Every one of those is a deadline the card is waiting on, and nothing used
 * to fire at it: renders came on emit or on the one-second clock, so "Needs
 * you" appeared up to a second after the debounce it was waiting for, and
 * Done the same after its settle. One timer, re-aimed on every render.
 */
let deadlineTimer = 0
function scheduleDeadlineRender(groups) {
  clearTimeout(deadlineTimer)
  const now = Date.now()
  let next = Infinity
  const consider = (ms) => {
    if (ms > now && ms < next) next = ms
  }
  for (const { key, session } of groups) {
    if (session.waiting_since) consider(session.waiting_since + WAITING_DEBOUNCE_MS)
    if (session.pending_since) consider(session.pending_since + WAITING_DEBOUNCE_MS)
    if (session.settles_ms) consider(session.settles_ms)
    if (session.outcome_ms) consider(session.outcome_ms + CELEBRATE_AFTER_MS)
    const view = views.get(key)
    if (view?.heldUntil) consider(view.heldUntil)
  }
  // The clock renders every second while things move and every ten once
  // settled, so anything inside that window is this timer's job; further
  // out, a later render will re-aim it.
  if (next === Infinity || next - now > 10_000) return
  deadlineTimer = setTimeout(render, next - now + 10)
}

function render() {
  const groups = cardsFor(sessions)
  const byKey = new Map(groups.map((group) => [group.key, group]))
  scheduleDeadlineRender(groups)

  // A chat that is gone is never coming back under the same id, so anything
  // remembered about it is a leak. Keyed by session rather than by project,
  // these maps would otherwise grow for the life of the overlay.
  const known = new Set(sessions.map((session) => session.session_id))
  for (const key of [...views.keys()]) if (!known.has(key)) views.delete(key)
  for (const key of [...collapsed]) if (!known.has(key)) collapsed.delete(key)
  if (promoted && !known.has(promoted)) promoted = null

  // Slots only change when a chat starts or stops being active, or when one
  // starts needing attention. Ordinary progress never reorders anything.
  const liveKeys = groups.filter((group) => group.live).map((group) => group.key)
  slots = slots.filter((key) => liveKeys.includes(key))
  for (const key of liveKeys) if (!slots.includes(key)) slots.push(key)

  for (const group of groups) {
    const view = viewFor(group.key)
    const urgent = URGENT.has(group.state)
    // Two edges, because they answer two different questions. The tray blink is
    // a glance and keeps the old rule: anything urgent, including the idle
    // notification, is worth a flicker in the corner. Taking a put-away card
    // back is the corner of the screen, and only a question actually being
    // asked earns that.
    //
    // Tracked separately on purpose. Sharing one `wasUrgent` edge would mean an
    // idle notification, arriving first, swallowed the edge — and the real
    // permission prompt that followed it without passing through a quiet state
    // would never take its card back at all.
    const demands = demandsAttention(group.state, group.session)
    if (urgent && !view.wasUrgent && group.state === 'waiting') {
      // The state the whole overlay was built for — an agent waiting on you is
      // dead time you are paying for twice — and it was the only state that
      // could not reach you behind a fullscreen window. Done and Failed blink
      // the tray; the one that is costing money in real time now does too. The
      // sound stays opt-in.
      invoke('flash_tray').catch(() => {})
      if (config.alertOnWaiting) invoke('alert').catch(() => {})
    }
    if (demands && !view.wasDemanding) {
      collapsed.delete(group.key)
      slots = [group.key, ...slots.filter((key) => key !== group.key)]
      // "Anything that starts needing you takes the card back on its own" is a
      // promise the README makes — but it is not allowed to overrule an explicit
      // "hide the cards". Hidden means hidden.
      //
      // Two rounds of this. First the auto-reveal PERSISTED, so one urgent
      // session turned "I hid the cards" into "show the cards" for good, which
      // is why it came back every login. persist:false fixed the setting and not
      // the symptom: the cards still appeared on every urgent turn, which with
      // an agent running is more or less always, so the pet looked exactly as
      // un-hidden as before.
      //
      // So the reveal is now conditional. If the user asked for the cards to be
      // hidden, an urgent session gets the tray flash and the pet's own state
      // and nothing else.
      if (config.showBubble) setStackHidden(false, { persist: false })
    }
    view.wasUrgent = urgent
    view.wasDemanding = demands

    // Finishing while you were looking at something else is the thing this
    // whole overlay exists to tell you about. The mark outlives the card.
    // A notice is the overlay talking about itself, not a completion: it
    // neither blinks the tray nor wears an unread dot.
    //
    // `worthCelebrating` is the difference between the card's claim and this
    // one. The card may say Done and take it back; a tray blink cannot be
    // taken back, so it waits until the completion has held, with nothing the
    // session started still running.
    if (
      group.key !== 'notice' &&
      settled(group.state) &&
      view.celebrated !== outcomeKey(group.session) &&
      worthCelebrating(group.session)
    ) {
      // Not gated on freshness: the blink needs a render to land inside the
      // news window, and an occluded window may not get one (WebView2
      // throttles its timers to about one a minute). Stale completions are
      // acknowledged at boot, which is the case freshness was guarding.
      view.celebrated = outcomeKey(group.session)
      view.unread = true
      invoke('flash_tray').catch(() => {})
    }
    // Work restarting answers the question the mark was asking — including
    // work that carried on after the turn ended.
    if (WORKING.has(group.state)) {
      view.unread = false
    }
  }

  // The card waits for you; the pet does not. A completion is worth a little
  // celebration, not an hour of one, so once the news has gone stale the pet
  // settles back even though the card is still sitting there saying "Done".
  renderer.setState(notice ? 'idle' : petState(groups, slots[0]))

  // A pet that visibly dozes is doing real work: it says "nothing is running
  // and I will not interrupt you", which is different from an overlay that has
  // silently stopped receiving events. Do Not Disturb looks the same on
  // purpose, because it is the same promise.
  const busy = groups.some(
    (group) => group.live && (!settled(group.state) || isFresh(group.session))
  )
  if (busy) lastLiveAt = Date.now()
  const dozing = config.quiet || Date.now() - lastLiveAt > SLEEP_AFTER_MS
  el.pet.classList.toggle('asleep', dozing)

  // Three chats fit as cards. Past that the stack would own the screen, so it
  // collapses: one card for the chat being watched, one line each for the rest,
  // which is enough to see what every chat is doing and to close any of them.
  // More chats than that fit *because* they are lines.
  // A notice always shows, at the front, and never counts toward the density
  // flip: letting it take a fourth slot collapsed the whole stack to one-line
  // cards for six seconds and popped it back when the notice expired.
  if (byKey.has('notice')) {
    slots = ['notice', ...slots.filter((key) => key !== 'notice')]
  }
  const wanted = stackHidden ? [] : slots.filter((key) => !collapsed.has(key))
  const { visible, full } = cardLayout(groups, wanted, { pinned: config.pinnedSession, promoted })
  fitStack(visible, full, byKey, groups)
  syncHitRects()
}

/**
 * Paints the stack, then drops cards from the far end until it fits.
 *
 * `cardLayout` decides how many cards are worth showing; this decides how many
 * there is room for, which is a different question on a short screen or at a
 * large UI scale. The stack used to scroll instead, and scrolling a
 * column-reverse box did two things at once that both looked broken: it sliced
 * the far card in half at the top edge, and it clipped the speech tail off the
 * near one, because the tail hangs below the box that was now clipping.
 *
 * Nothing is lost by trimming: a dropped chat is still on screen as a chip, on
 * the row above the cards, with its state dot and its name. The card nearest
 * the pet is the one the pet is "saying", so it is the last to go and is never
 * cut.
 */
function fitStack(visible, full, byKey, groups) {
  // Starting from the count that fitted last time rather than from all of them
  // is what keeps this free in the steady state. Starting from the full set
  // meant building the card that does not fit, measuring, and throwing it away
  // again on every render — three template clones a second, for a card nobody
  // ever saw. It is only a hint: both directions below are measured.
  let shown = visible.slice(0, Math.max(1, Math.min(visible.length, fitted)))
  // Chips are a row of pills and wrap, but enough live chats can still ask for
  // more rows than there are. Cards go first, then chips, then the count of
  // what is left over — which is still the truth, and still not cut off.
  let cap = Infinity
  // The last rung, for the largest pet on a short screen, where the ceiling is
  // thinner than one full card: the one-line form of the same card. A card
  // saying less is better than a card with its head sliced off.
  let compact = false
  let rest = []
  const paint = () => {
    const shownKeys = new Set(shown)
    // Live chats only: a chip for a chat that has gone idle restores nothing
    // when clicked, so it was a button whose only behavior was to vanish.
    //
    // Hiding the cards leaves the chips, deliberately. "Hide the cards" reads
    // like it should leave the pet alone on the desktop, and a pass that went
    // by the wording took the chips away with them (2026-09-12, reverted the
    // same day). They are the point of the click: one small pill per chat, so
    // you can still see at a glance what is running and what has finished
    // without a stack of cards in the corner. The row IS the collapsed state.
    rest = groups.filter((group) => !shownKeys.has(group.key) && group.live)
    paintStack(shown, compact ? EMPTY : full, byKey, rest, cap)
  }
  paint()
  // Grow first, and only into room that is demonstrably there: a card can not
  // be smaller than its one-line form, so that is the test. Without it the
  // stack would never take a card back after a window resize, or after a tall
  // card collapsed.
  while (shown.length < visible.length && stackRoom().slack >= COMPACT_CARD_PX) {
    const before = shown
    shown = visible.slice(0, shown.length + 1)
    paint()
    if (stackRoom().over) {
      shown = before
      paint()
      break
    }
  }
  // Then shrink, down the rungs, until it is under the ceiling.
  // Bounded by the card count plus the chip count, both small.
  for (let guard = 0; stackRoom().over && guard < 16; guard += 1) {
    if (shown.length > 1) shown = shown.slice(0, -1)
    else if (cap > 0) cap = Math.min(cap, rest.length) - 1
    else if (!compact) compact = true
    else break
    paint()
  }
  fitted = shown.length
}

/** `full` for a stack where no card is allowed the room to be full. */
const EMPTY = new Set()
/** How many cards fitted last time. A hint for the next render, never a truth. */
let fitted = Infinity
/** A one-line card, which is the least room another card can possibly want. */
const COMPACT_CARD_PX = 48

function paintStack(visible, full, byKey, rest, chipCap) {
  const visibleKeys = new Set(visible)
  for (const [key, node] of cards) {
    if (!visibleKeys.has(key)) {
      node.remove()
      cards.delete(key)
    }
  }

  // DOM order is bottom-up: #stack is column-reverse, so the first child sits
  // closest to the pet and the chip row ends up on top of the stack.
  let previous = null
  for (const key of visible) {
    const group = byKey.get(key)
    if (!group) continue
    let node = cards.get(key)
    if (!node) {
      node = buildCard(key)
      cards.set(key, node)
    }
    paintCard(node, group, !full.has(key))
    if (previous) {
      if (previous.nextElementSibling !== node) previous.after(node)
    } else if (el.stack.firstElementChild !== node) {
      el.stack.prepend(node)
    }
    previous = node
  }

  renderChips(rest, chipCap)
  // Re-appending an element that is already last still re-inserts it, which
  // restarts any animation in its subtree on every render tick.
  if (el.stack.lastElementChild !== el.chips) el.stack.append(el.chips)
}

/**
 * Is the stack asking for more height than it is allowed?
 *
 * Measured from the children against the computed `max-height` rather than from
 * `scrollHeight`, which stops reporting the overflow once the box no longer
 * scrolls. `offsetHeight` rather than `getBoundingClientRect`, because the
 * stage carries the UI-scale transform and the limit is in unscaled pixels.
 */
function stackRoom() {
  const style = getComputedStyle(el.stack)
  const limit = parseFloat(style.maxHeight)
  // No ceiling to measure against is not an overflow, and not room to grow
  // into either: leave the layout exactly as `cardLayout` asked for it.
  if (!Number.isFinite(limit)) return { over: false, slack: 0 }
  const gap = parseFloat(style.rowGap) || 0
  let used = 0
  let rows = 0
  for (const child of el.stack.children) {
    if (child.hidden) continue
    const height = child.offsetHeight
    if (!height) continue
    used += height
    rows += 1
  }
  used += Math.max(0, rows - 1) * gap
  // One more row would cost its own gap as well as its height.
  return { over: used > limit + 1, slack: limit - used - gap }
}

/** Diffed rather than rebuilt: replacing these every second made them flicker. */
function renderChips(all, cap = Infinity) {
  const groups = Number.isFinite(cap) ? all.slice(0, Math.max(0, cap)) : all
  const overflow = all.length - groups.length
  const wanted = new Set(groups.map((group) => group.key))
  for (const [key, node] of chipNodes) {
    if (!wanted.has(key)) {
      node.remove()
      chipNodes.delete(key)
    }
  }
  for (const group of groups) {
    let node = chipNodes.get(group.key)
    if (!node) {
      node = buildChip(group.key)
      chipNodes.set(group.key, node)
      el.chips.append(node)
    }
    const agent = agentIdentity(group.session)
    node.dataset.provider = agent.id
    node.querySelector('.agent-label').textContent = agent.label
    node.dataset.state = group.state
    // A chat title is longer than a project name was, and a chip is a pill:
    // the row clips it and the tooltip carries the rest.
    node.lastElementChild.textContent = group.label
    node.title = `${agent.name} · ${group.label}`
  }
  // The remainder is a count rather than nothing: "and four more chats are
  // running" is worth a row, and it is the only honest thing to say when there
  // is no room to name them.
  el.chipsMore.hidden = overflow < 1
  el.chipsMore.textContent = `+${overflow}`
  el.chipsMore.title = `${overflow} more ${overflow === 1 ? 'chat' : 'chats'} running, with no room for a chip`
  if (el.chips.lastElementChild !== el.chipsMore) el.chips.append(el.chipsMore)
  el.chips.hidden = chipNodes.size === 0 && overflow < 1
}

/**
 * Report the regions that should swallow clicks. Everything else in this
 * transparent window stays click-through so the overlay never blocks the app
 * underneath it.
 */
let lastHitRects = ''
function syncHitRects() {
  const rects = []
  const add = (node) => {
    if (!node || node.hidden) return
    const r = node.getBoundingClientRect()
    if (r.width && r.height) rects.push({ x: r.x, y: r.y, w: r.width, h: r.height })
  }
  add(el.pet)
  add(el.menu)
  add(el.panel)
  // Per element rather than one rect over the whole stack: the gaps between
  // cards stay click-through, so the window underneath is still reachable
  // between them.
  if (!el.chips.hidden) add(el.chips)
  for (const node of cards.values()) add(node)
  // Every render ended in this IPC, a main-thread hop and an X shape request,
  // for rects that had not moved since the last one.
  const encoded = JSON.stringify(rects)
  if (encoded === lastHitRects) return
  lastHitRects = encoded
  invoke('set_hit_rects', { rects }).catch(() => {})
}

function showNotice(message) {
  markActivity()
  notice = message
  clearTimeout(noticeTimer)
  noticeTimer = setTimeout(() => {
    notice = null
    sessions = sessions.filter((s) => s.session_id !== 'notice')
    render()
  }, 6000)
  sessions = [
    {
      session_id: 'notice',
      project: 'Pipsqueak',
      state: 'idle',
      outcome: 'done',
      outcome_ms: Date.now(),
      settles_ms: 0,
      waiting_since: 0,
      headline: message,
      kind: 'Notice',
      activity: '',
      detail: '',
      updated_ms: Date.now(),
      // A notice is not a turn, so it gets no counters. It used to claim one
      // that had started now and done nothing: "0 actions · 0s", ticking.
      turn_started_ms: 0,
      turn_tools: 0,
      recent: []
    },
    ...sessions.filter((s) => s.session_id !== 'notice')
  ]
  render()
}

// --- updates -------------------------------------------------------------
const RELEASES_API = 'https://api.github.com/repos/tristanmuzzu/Pipsqueak-Agent-Pets/releases/latest'
const RELEASES_PAGE = 'https://github.com/tristanmuzzu/Pipsqueak-Agent-Pets/releases'
/** Never at launch, and never at the same moment on every machine. */
const FIRST_CHECK_MS = 2 * 60_000
const FIRST_CHECK_JITTER_MS = 3 * 60_000
const CHECK_EVERY_MS = 12 * 60 * 60_000

/**
 * Asks GitHub whether there is a newer release, and says so once.
 *
 * Nothing is downloaded and nothing is installed. A desktop pet that can
 * replace its own binary is a much larger promise than this one wants to make,
 * and the release page is one click away.
 */
async function checkForUpdate(manual) {
  try {
    const response = await fetch(RELEASES_API, { headers: { Accept: 'application/vnd.github+json' } })
    if (!response.ok) throw new Error(`GitHub returned ${response.status}`)
    const latest = String((await response.json()).tag_name ?? '')
    if (!isNewer(latest, APP_VERSION)) {
      // Silent unless they asked. A scheduled check that announces "nothing to
      // report" twice a day is just noise.
      if (manual) showNotice(`Pipsqueak ${APP_VERSION} is the latest version.`)
      return
    }
    if (!manual && config.updateDismissed === latest) return
    showNotice(`Pipsqueak ${latest} is available: ${RELEASES_PAGE}`)
    if (!manual) {
      // Told once. Saying it again every twelve hours is how an update prompt
      // becomes something people learn to ignore.
      config.updateDismissed = latest
      await saveConfig()
    }
  } catch (error) {
    // A scheduled check that cannot reach the network is not news.
    if (manual) showNotice(`Could not check for updates: ${error.message}`)
  }
}

let updateTimersStarted = false

/**
 * The timers always run; the setting is checked when they fire. Gating the
 * scheduling on the setting meant enabling it did nothing until the next
 * launch, and disabling it left an already-scheduled check to fire anyway.
 */
function scheduleUpdateChecks() {
  if (updateTimersStarted) return
  updateTimersStarted = true
  const first = FIRST_CHECK_MS + Math.random() * FIRST_CHECK_JITTER_MS
  setTimeout(() => {
    if (config.updateCheck) checkForUpdate(false)
    setInterval(() => {
      if (config.updateCheck) checkForUpdate(false)
    }, CHECK_EVERY_MS)
  }, first)
}

// --- welcome and setup check --------------------------------------------
/**
 * How long the connection test watches for hook traffic.
 *
 * Long enough to type something into Claude Code and hit enter, short enough
 * that nobody wanders off mid-test.
 */
const WATCH_MS = 10_000

function openPanel(title, build) {
  el.panelTitle.textContent = title
  el.panelBody.replaceChildren(...build())
  el.panel.hidden = false
  el.menu.hidden = true
  syncHitRects()
}

/** Timers owned by the doctor's connection test. Without this they outlived
 * the panel, mutating detached nodes and stacking overlapping tests. */
let doctorTimers = []
function clearDoctorTimers() {
  for (const id of doctorTimers) {
    clearInterval(id)
    clearTimeout(id)
  }
  doctorTimers = []
}

function closePanel() {
  clearDoctorTimers()
  el.panel.hidden = true
  syncHitRects()
}

function para(text, className) {
  const node = document.createElement('p')
  node.textContent = text
  if (className) node.className = className
  return node
}

function action(label, onClick, primary) {
  const node = document.createElement('button')
  node.type = 'button'
  node.className = primary ? 'action primary' : 'action'
  node.textContent = label
  node.addEventListener('click', () => onClick(node))
  return node
}

/**
 * Shown once, on the first run that has not been acknowledged.
 *
 * The hooks are the whole product and they do not install themselves, so
 * historically the first thing a new user had to do was find a tray menu and
 * guess what "install hooks" meant. This says what it edits, in the same
 * breath as asking to do it.
 */
/**
 * Tells the welcome panel which chord actually registered.
 *
 * Filled in after the panel is built, because asking the backend is async and
 * a panel that pops up half a frame late is worse than a line that arrives
 * half a frame late.
 */
function hotkeyLine() {
  const node = para('Checking the keyboard shortcut…')
  invoke('hotkey_binding')
    .then((chord) => {
      node.textContent = chord
        ? `Press ${chord} any time to show or hide the pet.`
        : HAS_GLOBAL_HOTKEY
          ? 'No keyboard shortcut was available; set "hotkey" in ~/.pipsqueak/config.json.'
          : NO_HOTKEY_HERE
    })
    .catch(() => {
      node.textContent = ''
    })
  return node
}

/**
 * States the autostart decision, and offers the other one.
 *
 * Built empty and filled in when the backend answers, for the same reason as
 * `hotkeyLine`: a panel that arrives half a frame late is worse than a line
 * that does.
 */
function autostartLine() {
  const row = document.createElement('div')
  const label = para('')
  row.append(label)
  // Windows starts the program with the operating system; an XDG autostart
  // entry starts it when the desktop session begins. The backend owns which
  // sentence is true, and the fallback is the one that is true everywhere.
  let atLogin = 'when you log in'
  const paint = (enabled) => {
    label.textContent = enabled
      ? `It starts ${atLogin}, so it is there when you get back.`
      : `It does not start ${atLogin}.`
    button.textContent = enabled ? `Do not start ${atLogin}` : `Start ${atLogin}`
  }
  const button = action('', async () => {
    const enabled = await invoke('autostart_enabled').catch(() => false)
    const result = await invoke('set_autostart', { enabled: !enabled }).catch((e) => String(e))
    if (typeof result === 'string') showNotice(result)
    paint(!enabled)
  })
  row.append(button)
  Promise.all([
    invoke('at_login').catch(() => atLogin),
    invoke('autostart_enabled').catch(() => false)
  ]).then(([phrase, enabled]) => {
    atLogin = phrase
    paint(enabled)
  })
  return row
}

function showWelcome() {
  openPanel('Pipsqueak', () => {
    const nodes = [
      para('Follow Claude Code and Codex together, with a separate card for every conversation.'),
      para(
        'Codex connects automatically through its local sessions. Blue outlines mean Codex; orange means Claude Code. The status dot tells you whether a task is working, waiting, or done.',
        'tight'
      ),
      para('For Claude Code, install hooks in ~/.claude/settings.json. Your settings are backed up first.', 'tight'),
      action('Install Claude Code hooks', async (button) => {
        button.disabled = true
        button.textContent = 'Installing…'
        const message = await invoke('install_hooks').catch((e) => String(e))
        button.textContent = message
      }, true),
      // On by default, and said out loud rather than left as a button nobody
      // pressed: an overlay that is not running is indistinguishable from an
      // overlay that is broken, and the shortcut that would bring it back
      // belongs to the process that is not there.
      autostartLine(),
      action('Check GitHub for updates occasionally', async (button) => {
        config.updateCheck = !config.updateCheck
        await saveConfig()
        button.textContent = config.updateCheck
          ? 'Will check for updates. Nothing is ever downloaded'
          : 'Will not check for updates'
      }),
      para('Then start a session. Restart Claude Code first, because it reads its hooks at startup.'),
      hotkeyLine(),
      action('Done', () => {
        closePanel()
      })
    ]
    return nodes
  })
}

/**
 * The setup check.
 *
 * The static checks answer "is it configured"; the connection test answers "is
 * it working", which is a different question and the only one worth asking
 * when someone says nothing is happening.
 */
async function showDoctor() {
  clearDoctorTimers()
  const report = await invoke('run_doctor').catch(() => null)
  openPanel('Setup check', () => {
    if (!report) return [para('Could not run the check.')]
    const nodes = []
    for (const check of report.checks) {
      const row = document.createElement('div')
      row.className = 'check'
      row.dataset.status = check.status
      const dot = document.createElement('span')
      dot.className = 'dot'
      const text = document.createElement('span')
      text.className = 'check-text'
      const label = document.createElement('strong')
      label.className = 'check-label'
      label.textContent = check.label
      const detail = document.createElement('span')
      detail.className = 'check-detail'
      detail.textContent = check.detail
      text.append(label, detail)
      row.append(dot, text)
      if (check.fix === 'install') {
        const fix = document.createElement('button')
        fix.type = 'button'
        fix.className = 'fix'
        fix.textContent = 'Fix'
        fix.addEventListener('click', async () => {
          fix.disabled = true
          await invoke('install_hooks').catch(() => {})
          showDoctor()
        })
        row.append(fix)
      }
      nodes.push(row)
    }

    const status = para('')
    nodes.push(
      action('Test the connection', async (button) => {
        button.disabled = true
        const since = await invoke('watch_start').catch(() => Date.now())
        const deadline = Date.now() + WATCH_MS
        status.textContent = 'Start a turn in Codex or Claude Code now…'
        const tick = setInterval(() => {
          const left = Math.ceil((deadline - Date.now()) / 1000)
          if (left > 0) status.textContent = `Start a turn in Codex or Claude Code now… ${left}s`
        }, 250)
        const finish = setTimeout(async () => {
          clearInterval(tick)
          const [, detail] = await invoke('watch_result', { since }).catch(() => ['none', 'Check failed.'])
          status.textContent = detail
          button.disabled = false
          button.textContent = 'Test again'
        }, WATCH_MS)
        doctorTimers.push(tick, finish)
      }),
      status,
      action('Copy report', async (button) => {
        const text = await invoke('doctor_report').catch(() => '')
        try {
          await navigator.clipboard.writeText(text)
          button.textContent = 'Copied. Paste it into an issue'
        } catch {
          button.textContent = 'Could not reach the clipboard'
        }
      })
    )
    return nodes
  })
}

// --- context menu -------------------------------------------------------
/**
 * The right-click menu.
 *
 * Everything used more than once a week is on it; everything used once, when
 * the app is first set up, is behind "More". A menu with twenty items in it is
 * one nobody reads, and it had grown tall enough to run off the top of the
 * screen.
 */
let menuExpanded = false

let connectionTimer
async function updateConnections(node) {
  const state = await invoke('connection_status').catch(() => IS_TAURI ? null : {
    codex: { status: 'demo' }, claude: { status: 'demo' }
  })
  const labels = {
    codex: {
      connected: state?.codex?.observed ? (state.codex.active ? `Live · ${state.codex.active} active` : 'Live · idle') : 'Live · awaiting task',
      fallback: 'Local logs only', missing: 'Not found', demo: 'Preview'
    },
    claude: { receiving: 'Receiving events', ready: 'Hooks ready · quiet', missing: 'Hooks missing', demo: 'Preview' }
  }
  const details = {
    connected: 'Connected to Codex Desktop. Approval and input waits are observed live.',
    fallback: 'Reading local Codex logs. The desktop live connection is unavailable; approval waits may be missed.',
    ready: 'Claude Code hooks are installed. No event in the last five minutes; ready for the next task.',
    receiving: 'Claude Code sent an event in the last five minutes.',
    missing: 'Open Check my setup below for connection details.',
    demo: 'Browser preview; no agents are connected.'
  }
  node.replaceChildren(...['codex', 'claude'].map((id) => {
    const status = state?.[id]?.status
    const row = document.createElement('div')
    row.className = 'connection'
    row.dataset.provider = id
    row.dataset.health = status || 'missing'
    row.title = details[status] || 'Connection check unavailable. Try Check my setup.'
    const name = document.createElement('strong')
    name.textContent = id === 'codex' ? 'Codex' : 'Claude'
    const text = document.createElement('span')
    text.textContent = labels[id][status] || 'Check unavailable'
    row.append(name, text)
    return row
  }))
}

function showProjects() {
  const projects = new Map()
  for (const session of sessions) {
    if (session.session_id !== 'notice') projects.set(projectKey(session), projectName(session))
  }
  for (const key of config.hiddenProjects) {
    if (!projects.has(key)) projects.set(key, key.replace(/^(path|name):/, '').split('/').pop() || key)
  }
  openPanel('Projects', () => {
    const nodes = [para('Hidden projects stay quiet: no cards, sounds or tray alerts. These choices apply to both agents.')]
    for (const [key, name] of [...projects].sort((a, b) => a[1].localeCompare(b[1]))) {
      const shown = !config.hiddenProjects.includes(key)
      const toggle = action(`${shown ? '✓' : '−'} ${name}`, async () => {
        config.hiddenProjects = shown ? [...config.hiddenProjects, key] : config.hiddenProjects.filter((p) => p !== key)
        await saveConfig(); render(); showProjects()
      })
      toggle.classList.add('project-toggle')
      toggle.setAttribute('aria-pressed', String(shown))
      toggle.setAttribute('aria-label', `${shown ? 'Hide' : 'Show'} project ${name}`)
      toggle.title = key.replace(/^(path|name):/, '')
      nodes.push(toggle)
    }
    if (!projects.size) nodes.push(para('Projects appear here when an agent starts working.'))
    if (config.hiddenProjects.length) nodes.push(action('Show all projects', async () => {
      config.hiddenProjects = []; await saveConfig(); render(); showProjects()
    }))
    return nodes
  })
}

async function openMenu() {
  const pets = await invoke('list_pets').catch(() => [])
  const installed = await invoke('hooks_installed').catch(() => false)
  const autostart = await invoke('autostart_enabled').catch(() => false)
  const atLogin = await invoke('at_login').catch(() => 'when you log in')
  const chord = await invoke('hotkey_binding').catch(() => '')
  const children = []

  /** A click closes the menu unless it changed the menu itself. */
  const button = (text, onClick, pressed, keepOpen = false) => {
    const node = document.createElement('button')
    node.type = 'button'
    node.textContent = text
    if (pressed !== undefined) node.setAttribute('aria-pressed', String(pressed))
    node.addEventListener('click', async () => {
      if (!keepOpen) el.menu.hidden = true
      await onClick()
      render()
    })
    return node
  }
  const row = (buttons) => {
    const node = document.createElement('div')
    node.className = 'row'
    node.append(...buttons)
    return node
  }
  const rule = () => {
    const node = document.createElement('div')
    node.className = 'rule'
    return node
  }

  const connections = document.createElement('div')
  connections.className = 'connections'
  connections.setAttribute('aria-label', 'Agent connections')
  children.push(connections)
  updateConnections(connections)
  clearTimeout(connectionTimer)
  const refresh = async () => {
    if (el.menu.hidden || !connections.isConnected) return
    await updateConnections(connections)
    connectionTimer = setTimeout(refresh, 3000)
  }
  connectionTimer = setTimeout(refresh, 3000)
  children.push(rule(), para('Show agents', 'heading'))
  children.push(row(['all', 'codex', 'claude'].map((id) => button(
    id === 'all' ? 'All' : id === 'codex' ? 'Codex' : 'Claude',
    async () => { config.agentFilter = id; await saveConfig(); openMenu() },
    config.agentFilter === id, true
  ))))
  children.push(button(`Projects${config.hiddenProjects.length ? ` · ${config.hiddenProjects.length} hidden` : ''}`, showProjects))
  if (config.pinnedSession) children.push(button('Unpin conversation', async () => {
    config.pinnedSession = ''; await saveConfig()
  }))
  children.push(rule())

  // Pets and sizes are picks from a short list, so they are chips on one line
  // rather than six full-width rows.
  children.push(
    row(
      pets.map((pet) =>
        button(pet.displayName, () => selectPet(pet.id), pet.id === config.pet, true)
      )
    )
  )
  children.push(
    row(
      [['S', 1.5], ['M', 2], ['L', 3]].map(([label, value]) =>
        button(
          label,
          async () => {
            config.scale = value
            applyScale()
            await saveConfig()
            openMenu()
          },
          config.scale === value,
          true
        )
      )
    )
  )
  children.push(rule())

  const narration = {
    off: 'Saying nothing while working',
    speech: 'Showing agent messages',
    thoughts: 'Showing agent thoughts'
  }
  children.push(
    button(
      narration[config.narrate] ?? narration.thoughts,
      async () => {
        const order = ['off', 'speech', 'thoughts']
        config.narrate = order[(order.indexOf(config.narrate) + 1) % order.length]
        await saveConfig()
        openMenu()
      },
      config.narrate !== 'off',
      true
    )
  )
  children.push(
    button(
      'Do not disturb',
      async () => {
        config.quiet = !config.quiet
        if (config.quiet) invoke('clear_attention').catch(() => {})
        await saveConfig()
      },
      config.quiet
    )
  )
  children.push(
    button(stackHidden ? 'Show the cards' : 'Hide the cards', () => {
      setStackHidden(!stackHidden)
      collapsed.clear()
    })
  )

  children.push(rule())
  children.push(
    button(menuExpanded ? 'Less' : 'More…', () => {
      menuExpanded = !menuExpanded
      openMenu()
    })
  )

  if (menuExpanded) {
    children.push(
      button(
        'Include scratch/temp sessions',
        async () => {
          config.showScratch = !config.showScratch
          await saveConfig()
        },
        config.showScratch
      )
    )
    children.push(
      button(
        'Sound when a project needs you',
        async () => {
          config.alertOnWaiting = !config.alertOnWaiting
          await saveConfig()
        },
        config.alertOnWaiting
      )
    )
    children.push(
      button(
        'Blink the tray when a project finishes',
        async () => {
          config.flashOnFinish = !config.flashOnFinish
          await saveConfig()
        },
        config.flashOnFinish
      )
    )
    children.push(
      button(
        'Click through the pet',
        async () => {
          config.clickThrough = !config.clickThrough
          await saveConfig()
        },
        config.clickThrough
      )
    )
    children.push(rule())
    // The chord shown is the one that registered, which is not always the one
    // that was asked for: another program may already own it.
    children.push(
      button(chord ? `Show or hide with ${chord}` : 'No global hotkey available', () =>
        showNotice(
          chord
            ? `${chord} shows and hides the pet. Change it with "hotkey" in ~/.pipsqueak/config.json.`
            : HAS_GLOBAL_HOTKEY
              ? 'Every candidate hotkey is already taken. Set "hotkey" in ~/.pipsqueak/config.json to a free one.'
              : NO_HOTKEY_HERE
        )
      )
    )
    children.push(button('Check my setup…', () => showDoctor()))
    children.push(
      button(
        'Check for updates automatically',
        async () => {
          config.updateCheck = !config.updateCheck
          await saveConfig()
          if (config.updateCheck) checkForUpdate(true)
        },
        config.updateCheck
      )
    )
    children.push(
      button(
        `Start ${atLogin}`,
        async () => {
          const result = await invoke('set_autostart', { enabled: !autostart }).catch((e) =>
            String(e)
          )
          if (typeof result === 'string') showNotice(result)
        },
        autostart
      )
    )
    children.push(
      button(installed ? 'Reinstall Claude Code hooks' : 'Install Claude Code hooks', async () => {
        const message = await invoke('install_hooks').catch((e) => String(e))
        showNotice(message)
      })
    )
    children.push(button('Open pets folder', () => invoke('open_pets_dir').catch(() => {})))
  }

  children.push(rule())
  // Two different doors, and it was worth making the difference obvious. The
  // hotkey and the tray icon belong to this process, so quitting takes them
  // with it: nothing on the keyboard brings the pet back afterwards.
  children.push(
    button(chord ? `Hide the pet (${chord})` : 'Hide the pet', () => invoke('hide_window'))
  )
  const quit = button('Quit Pipsqueak', () => invoke('quit'))
  quit.title = chord
    ? `Stops Pipsqueak. ${chord} will not bring it back; start it again from the Start menu.`
    : 'Stops Pipsqueak. Start it again from the Start menu.'
  children.push(quit)

  el.menu.replaceChildren(...children)
  el.menu.hidden = false
  syncHitRects()
}

async function selectPet(id) {
  try {
    const payload = await loadPetPayload(id)
    await renderer.load(id, payload)
    applyScale()
    config.pet = id
    await saveConfig()
  } catch (error) {
    showNotice(String(error))
  }
}

async function saveConfig() {
  const stored = {
      pet: config.pet,
      scale: config.scale,
      click_through: config.clickThrough,
      show_bubble: config.showBubble,
      show_scratch: config.showScratch,
      agent_filter: config.agentFilter,
      hidden_projects: config.hiddenProjects,
      pinned_session: config.pinnedSession,
      alert_on_waiting: config.alertOnWaiting,
      flash_on_finish: config.flashOnFinish,
      quiet: config.quiet,
      narrate: config.narrate,
      welcomed: config.welcomed,
      update_check: config.updateCheck,
      update_dismissed: config.updateDismissed,
      x: config.x ?? null,
      y: config.y ?? null
  }
  try {
    if (IS_TAURI) await invoke('set_config', { config: stored })
    else localStorage.setItem('pipsqueak-demo-config', JSON.stringify(stored))
  } catch { showNotice('Could not save preferences. Please try again.') }
}

// --- interaction --------------------------------------------------------
/**
 * Take the hint down, and cancel one that has not appeared yet.
 *
 * THE BUG THIS EXISTS FOR
 *
 * The hint used to be dismissed by `pointerleave` alone, and that event does
 * not arrive. The window only accepts the cursor where the pet and its cards
 * are; step off the pet and the compositor routes the pointer to whatever is
 * underneath, so the page is told nothing — no leave, no out, and `:hover`
 * stays stuck on. Measured on GNOME 50.1 / Wayland, 2026-08-18, by counting
 * events while moving the pointer off the pet: `pet-pointerenter:1
 * pet-pointerover:1 hov:1` before, the same after, nothing added by the move.
 *
 * So the hint could appear seconds after the cursor had left, and then sit
 * there. What does know is one layer down: GTK gets the Wayland leave, and
 * `pipsqueak://pointer-left` carries it up. On the platforms that hit-test by
 * polling the cursor instead, `pipsqueak://cursor` answers the same question.
 */
function hideHint() {
  clearTimeout(hintTimer)
  el.hint.hidden = true
}

/**
 * The cursor has arrived on the pet: greet it, and start the hint's wait.
 *
 * THE OTHER HALF OF THE SAME BUG
 *
 * `pointerenter` cannot be trusted to say this either, and for the mirror
 * reason: the page is never told the cursor left, so as far as WebKit is
 * concerned it never did, and coming back produces no crossing event at all.
 * One drag was enough to poison it for good — dragging the pet takes the
 * pointer out of the window under a grab, and afterwards the pet neither
 * jumped nor showed its hint again, however many times the cursor arrived.
 * Movement is the reliable signal, so arrival is read from that, and `hovering`
 * is what keeps it to once per visit.
 */
function greetPet() {
  clearTimeout(leaveTimer)
  leftFrom = ''
  if (hovering) return
  hovering = true
  renderer.playOnce(JUMP_ROW)
  // Long enough that reaching past the pet for something else never brings it
  // up. It is only useful to someone who has stopped and is wondering.
  clearTimeout(hintTimer)
  // The hint is not in the hit rects on purpose: it cannot be clicked, so the
  // window stays click-through underneath it.
  hintTimer = setTimeout(() => {
    el.hint.hidden = false
  }, HINT_DELAY_MS)
}

/** The cursor is off the pet, and the page saw it happen. */
function leftPet() {
  clearTimeout(leaveTimer)
  leftFrom = ''
  hovering = false
  hideHint()
}

/**
 * The window says the pointer has gone. Believe it in a moment, not now.
 *
 * The signal is honest but ambiguous: *arriving* on the pet after a drag
 * produces one too, because the compositor's grab ends as the pointer comes
 * back. Measured on GNOME 50.1 / Wayland, 2026-08-18 — dragging the pet and
 * then returning to it logged `mode=Ungrab detail=Virtual` at the release and
 * `mode=Normal detail=NonlinearVirtual` at the return, the second one at the
 * pet's own position and indistinguishable from a real departure.
 *
 * What tells them apart is what follows: a cursor that is really on the pet
 * goes on moving there. So a departure waits out a grace period that any
 * arrival cancels, which is invisible for a hint that takes 2.5s to appear
 * anyway.
 */
function maybeLeftPet() {
  clearTimeout(leaveTimer)
  leftFrom = lastPointerAt
  leaveTimer = setTimeout(leftPet, LEAVE_GRACE_MS)
}

/** Whether a window-local cursor position is inside the pet. */
function overPet(x, y) {
  const r = el.pet.getBoundingClientRect()
  return x >= r.x && y >= r.y && x <= r.x + r.width && y <= r.y + r.height
}

function wireInteraction() {
  window.addEventListener('resize', () => { applyScale(); render() })
  let origin = null

  el.pet.addEventListener('pointerdown', (event) => {
    if (event.button !== 0) return
    origin = { x: event.clientX, y: event.clientY }
  })

  el.pet.addEventListener('pointermove', (event) => {
    if (!origin) {
      // Arriving counts whether or not the crossing event ever came (see
      // greetPet) — but only if the cursor is really here. Two kinds of move
      // say it is when it is not: one delivered to the pet while the cursor is
      // elsewhere, and the trailing pair WebKit sends *after* a departure, at
      // the position the cursor left from. Both used to cancel the departure
      // the window had just reported, which put the hint back on screen for
      // good. A repeat of the position we were told about is not news.
      const at = `${Math.round(event.clientX)},${Math.round(event.clientY)}`
      if (at === leftFrom) return
      if (overPet(event.clientX, event.clientY)) greetPet()
      return
    }
    if (Math.hypot(event.clientX - origin.x, event.clientY - origin.y) > 3) {
      // The press is the compositor's from here: it moves the window and
      // swallows the release, so the `pointerup` that would normally clear
      // this never comes. Letting it stand meant every later move on the pet
      // was read as "still dragging" — after one drag the pet stopped jumping
      // and stopped ever showing its hint again.
      origin = null
      appWindow?.startDragging().catch(() => {})
    }
  })

  el.pet.addEventListener('pointerup', () => {
    // `origin` is already gone if this turned into a drag, so a drag can never
    // be mistaken for the click that hides the cards.
    if (origin) {
      // A click outside the menu can't reach us, because the window is
      // click-through there, so the pet itself is what dismisses it.
      if (!el.menu.hidden) el.menu.hidden = true
      else setStackHidden(!stackHidden)
      // Looking at the pet is looking at the pet.
      invoke('clear_attention').catch(() => {})
      render()
    }
    origin = null
  })

  // Poking it should do something. Cheap, and the first thing anyone tries.
  el.pet.addEventListener('pointerenter', greetPet)
  el.pet.addEventListener('pointerleave', leftPet)
  el.pet.addEventListener('pointerdown', hideHint)

  // Leaving the pet for a card stays inside the page, so no signal from the
  // window arrives, and `pointerleave` is no more reliable in this direction
  // than the other one. Where the cursor is, is: any movement not on the pet
  // is the cursor not being on the pet.
  document.addEventListener(
    'pointermove',
    (event) => {
      // The cursor can only be here at all because it is over something ours,
      // so movement is activity: it reopens the idle gate.
      markActivity()
      const at = `${Math.round(event.clientX)},${Math.round(event.clientY)}`
      if (at === leftFrom) return
      lastPointerAt = at
      if (!overPet(event.clientX, event.clientY)) leftPet()
    },
    true
  )


  el.pet.addEventListener('contextmenu', (event) => {
    event.preventDefault()
    if (el.menu.hidden) openMenu()
    else {
      el.menu.hidden = true
      syncHitRects()
    }
  })

  document.addEventListener('contextmenu', (event) => event.preventDefault())

  if (!appWindow) return
  appWindow.onMoved(({ payload }) => {
    config.x = payload.x
    config.y = payload.y
    invoke('save_position', { x: payload.x, y: payload.y }).catch(() => {})
  })
}

// --- boot ---------------------------------------------------------------
async function boot() {
  const stored = IS_TAURI ? await invoke('get_config').catch(() => null)
    : (() => { try { return JSON.parse(localStorage.getItem('pipsqueak-demo-config')) } catch { return null } })()
  if (stored) {
    config = {
      pet: stored.pet ?? 'byte',
      scale: stored.scale ?? 2,
      clickThrough: Boolean(stored.click_through),
      showBubble: stored.show_bubble !== false,
      showScratch: Boolean(stored.show_scratch),
      agentFilter: ['all', 'codex', 'claude'].includes(stored.agent_filter) ? stored.agent_filter : 'all',
      hiddenProjects: Array.isArray(stored.hidden_projects) ? stored.hidden_projects : [],
      pinnedSession: stored.pinned_session || '',
      alertOnWaiting: Boolean(stored.alert_on_waiting),
      flashOnFinish: stored.flash_on_finish !== false,
      quiet: Boolean(stored.quiet),
      narrate: stored.narrate || 'thoughts',
      welcomed: Boolean(stored.welcomed),
      updateCheck: Boolean(stored.update_check),
      updateDismissed: stored.update_dismissed ?? '',
      x: stored.x,
      y: stored.y
    }
    // Hiding the cards is a decision, not a mood: it used to come back on
    // every restart, reload and watchdog rescue.
    stackHidden = !config.showBubble
  }

  try {
    await renderer.load(config.pet, await loadPetPayload(config.pet))
  } catch {
    config.pet = 'pip'
    await renderer.load('pip', null)
  }
  applyScale()
  renderer.start()

  wireInteraction()
  el.panelClose.addEventListener('click', closePanel)

  if (!IS_TAURI) {
    startBrowserDemo()
    return
  }

  // Any dismissal counts as acknowledged: Done, the close button, or simply
  // never opening it again. A welcome that keeps coming back is worse than one
  // that is missed.
  if (!config.welcomed) {
    config.welcomed = true
    await saveConfig()
    // An existing install has no `welcomed` flag either, so it would get the
    // panel on its first update. Offering to install hooks that are already
    // registered reads as "something is wrong with your setup", and it
    // interrupts whatever you were doing to say it. If the hooks are there,
    // the only thing worth mentioning is the new shortcut, and a notice card
    // says that without taking over the screen.
    if (await invoke('hooks_installed').catch(() => true)) {
      const chord = await invoke('hotkey_binding').catch(() => '')
      showNotice(
        chord
          ? `Updated to ${APP_VERSION}. Press ${chord} to show or hide the pet.`
          : `Updated to ${APP_VERSION}.`
      )
    } else {
      showWelcome()
    }
  } else if (!(await invoke('hooks_installed').catch(() => true))) {
    // The welcome is shown once and marked seen the moment it appears, even if
    // it was closed without installing anything. After that the pet dozes
    // forever — which is deliberately the same thing a healthy idle pet does,
    // so a deaf install is indistinguishable from a quiet one. Say it, once
    // per launch, only while it is actually true.
    showNotice('Codex connects automatically. To also track Claude Code, install its hooks from the pet menu.')
  }

  // Behind any notice the boot just raised: replacing the array wholesale
  // dropped the "Updated to …" card one IPC round-trip after it was shown.
  sessions = [
    ...sessions.filter((s) => s.session_id === 'notice'),
    ...(await invoke('get_sessions').catch(() => []))
  ]
  sessions.forEach((session) => {
    seenSessions.add(session.session_id)
    // Whatever finished before the overlay started is history, not news. A
    // completion still inside its news window is shown, so restarting the pet
    // in the middle of a turn does not swallow the answer.
    const stale = Date.now() - (session.outcome_ms || 0) > DONE_LINGER_MS
    if (session.outcome && stale) acknowledged.add(outcomeKey(session))
  })

  await listen('pipsqueak://sessions', (event) => {
    // The poller only emits when the data changed, so an emit *is* activity —
    // and it wakes a parked sprite loop even when the display state it maps
    // to is the one already showing.
    markActivity()
    renderer.wake()
    const incoming = event.payload ?? []
    for (const session of incoming) {
      if (!seenSessions.has(session.session_id)) {
        seenSessions.add(session.session_id)
        // Codex's bounded tail can arrive a few polls after startup. An old
        // completion discovered late is still history, not a new result.
        const historical = session.outcome && Date.now() - (session.outcome_ms || 0) > DONE_LINGER_MS
        if (historical) acknowledged.add(outcomeKey(session))
        else if (sessionVisible(session, config)) renderer.playOnce(GREETING_ROW)
      }
    }
    const live = new Set(incoming.map((s) => s.session_id))
    seenSessions = new Set([...seenSessions].filter((id) => live.has(id)))
    // An acknowledgement matters only while its exact outcome is still the
    // one on the session; a gone session or a superseded outcome can never
    // show that card again, so remembering either is just a leak.
    const current = new Set(incoming.map(outcomeKey))
    for (const key of acknowledged) {
      if (!current.has(key)) acknowledged.delete(key)
    }
    const kept = notice ? sessions.filter((s) => s.session_id === 'notice') : []
    sessions = [...kept, ...incoming]
    render()
  })

  await listen('pipsqueak://position', (event) => {
    [config.x, config.y] = event.payload
  })

  await listen('pipsqueak://notice', (event) => showNotice(String(event.payload)))

  // The pointer has left everything this window accepts it on, which is the
  // only notice the page gets of it (see hideHint). Linux only: elsewhere the
  // cursor poll below answers the same question.
  await listen('pipsqueak://pointer-left', maybeLeftPet)

  // The cursor, in this window's own coordinates, while it is somewhere near.
  // Pets drawn with the two look rows turn to face it; the rest never hear
  // about it, because the renderer drops it.
  await listen('pipsqueak://cursor', (event) => {
    markActivity()
    const at = event.payload
    if (!Array.isArray(at)) {
      renderer.lookAt(null, null)
      leftPet()
      return
    }
    // The only honest answer to "is the cursor still on the pet". Off it by so
    // much as a pixel and the hint goes, pending or showing.
    if (!overPet(at[0], at[1])) leftPet()
    else greetPet()
    const rect = el.pet.getBoundingClientRect()
    renderer.lookAt(at[0] - (rect.x + rect.width / 2), at[1] - (rect.y + rect.height / 2))
  })

  // `pipsqueak control <pet>` changes the config from outside the window.
  await listen('pipsqueak://pet', async (event) => {
    const id = String(event.payload)
    try {
      await renderer.load(id, await loadPetPayload(id))
      applyScale()
      config.pet = id
      await saveConfig()
    } catch (error) {
      showNotice(String(error))
    }
  })

  // The backend asking whether the page is alive. Timers are throttled hard
  // for an occluded window, so the render loop can go quiet without the page
  // being broken; event delivery is not throttled, so answer directly and let
  // the watchdog stand down instead of reloading a healthy page.
  await listen('pipsqueak://ping', () => {
    invoke('frontend_pong').catch(() => {})
  })

  // A config change made from the tray. Merge only the fields the tray owns:
  // taking the whole object would clobber an in-flight change on this side,
  // and ignoring it meant the next save here silently reverted the tray's.
  await listen('pipsqueak://config', (event) => {
    const stored = event.payload
    if (!stored || typeof stored !== 'object') return
    config.clickThrough = Boolean(stored.click_through)
    config.quiet = Boolean(stored.quiet)
    render()
  })

  // Listeners are attached: anything emitted before this moment was lost, so
  // ask the poller to send the current state again.
  await invoke('frontend_ready').catch(() => {})
  render()

  scheduleUpdateChecks()

  // Ages and elapsed timers tick even when no event arrives — every second
  // while something is moving, every tenth once the page has settled. The
  // ages a settled page shows are minutes old at best, so a ten-second lag
  // on them is invisible; the render it saves is a full reconcile plus an
  // IPC round-trip for the hit rects, sixty times a minute, around the clock.
  let clockTicks = 0
  setInterval(() => {
    updateSettled()
    clockTicks += 1
    if (!uiSettled || clockTicks % 10 === 0) render()
  }, 1000)
}

/** Built-in pets ship inside the frontend bundle; the rest come from disk. */
async function loadPetPayload(id) {
  if (id === 'byte' || id === 'ember' || id === 'pip') return null
  return invoke('load_pet', { id })
}

/** What each demo chat is "saying", so the main line has real sentences. */
const DEMO_LINES = {
  clockwork: [
    'Reading the formatter to see which clock it trusts.',
    'It parses in local time and compares in UTC — that is the bug.',
    'Rewriting the assertion to fix the timezone at the boundary.',
    'Both suites pass; nothing left to check.'
  ],
  orchestrator: [
    'Adding the twelve tier C scenarios to the eval set.',
    'The runner times out at eight; raising the budget.',
    'Added 12 scenarios; suite passes in 41s.'
  ],
  ledger: [
    'Checking how finance rounds a half cent.',
    'Half-up, and only at the invoice total.',
    'Rewriting the totals to round once, at the end.'
  ],
  atlas: [
    'Tiles are re-fetched every hour for no reason.',
    'Setting a week of cache with a content hash in the path.'
  ],
  migration: [
    'Writing the backfill for the invoices already issued.',
    'Batching it at a thousand rows so the table stays writable.'
  ]
}

/** Cycles states in a browser tab so the UI can be designed without a build. */
function startBrowserDemo() {
  // The desktop app supplies the chat title in a real session; here it is
  // written down so the card can be designed with the line it will actually
  // have to fit.
  const titles = {
    clockwork: 'Timezone test flakiness',
    orchestrator: 'Tier C eval coverage',
    ledger: 'Invoice rounding rules',
    atlas: 'Tile server cache headers',
    migration: 'Backfill for issued invoices'
  }
  // Two chats on the same repository, because that is the case the card layout
  // has to survive: they get a card each, and the project name on both.
  const projects = { migration: 'ledger' }
  const agentsOnly = new URLSearchParams(location.search).get('demo') === 'agents'
  // Five chats rather than two: three is where the stack collapses, and a
  // layout whose rule you cannot see is a layout you cannot design.
  const scripts = {
    clockwork: [
      ['thinking', 'Thinking', 'Fix the flaky timezone test'],
      ['running', 'Reading', 'Fix the flaky timezone test'],
      ['running', 'Editing', 'Fix the flaky timezone test'],
      ['running', 'Running', 'Fix the flaky timezone test'],
      ['waiting', 'Needs you', 'Fix the flaky timezone test'],
      ['done', 'Done', 'Fixed: the formatter used local time.']
    ],
    orchestrator: [
      ['running', 'Editing', 'Add tier C eval scenarios'],
      ['running', 'Running', 'Add tier C eval scenarios'],
      ['failed', 'Failed', 'Add tier C eval scenarios'],
      ['done', 'Done', 'Added 12 scenarios; suite passes in 41s.']
    ],
    ledger: [
      ['running', 'Reading', 'Round invoice totals the way finance does'],
      ['running', 'Editing', 'Round invoice totals the way finance does'],
      ['running', 'Running', 'Round invoice totals the way finance does']
    ],
    atlas: [
      ['running', 'Running', 'Cache tiles for a week, not an hour'],
      ['thinking', 'Thinking', 'Cache tiles for a week, not an hour'],
      ['running', 'Editing', 'Cache tiles for a week, not an hour']
    ],
    migration: [
      ['running', 'Editing', 'Backfill the invoices already issued'],
      ['running', 'Running', 'Backfill the invoices already issued'],
      ['thinking', 'Thinking', 'Backfill the invoices already issued']
    ]
  }
  // In the app the cursor arrives from the window manager, because the overlay
  // is click-through and never sees a pointer event it is not under. In a
  // browser tab the pointer is all there is, so the look poses can still be
  // designed without a build.
  document.addEventListener('pointermove', (event) => {
    const rect = el.pet.getBoundingClientRect()
    renderer.lookAt(event.clientX - (rect.x + rect.width / 2), event.clientY - (rect.y + rect.height / 2))
  })
  // The renderer, for poking at from the console while designing. Only ever in
  // a browser tab: the app takes this branch never.
  window.pet = renderer

  let tick = 0
  const started = Date.now() - 143_000
  const advance = () => {
    const now = Date.now()
    sessions = Object.entries(scripts).filter(([chat]) => !agentsOnly || ['ledger', 'migration'].includes(chat)).map(([chat, steps], i) => {
      const [want, kind, headline] = steps[(tick + i) % steps.length]
      // The demo speaks in display states; the real files speak in the three
      // separate fields, so translate rather than special-case the renderer.
      const outcome = want === 'done' || want === 'failed' ? want : ''
      let durable = want
      if (outcome) durable = 'idle'
      // Being blocked does not change what the session was doing.
      else if (want === 'waiting') durable = 'running'
      return {
        session_id: chat,
        provider: ['orchestrator', 'atlas', 'migration'].includes(chat) ? 'codex' : 'claude',
        project: projects[chat] ?? chat,
        chat_title: titles[chat] ?? '',
        workspace: i === 1 ? 'feature-x' : '',
        scratch: false,
        state: durable,
        outcome,
        outcome_ms: outcome ? now : 0,
        settles_ms: 0,
        waiting_since: 0,
        waiting_reason: '',
        // The demo's one blocked step is a risky command, so the ask row and
        // its warning can be designed without a real session.
        pending_since: want === 'waiting' ? now - WAITING_DEBOUNCE_MS : 0,
        pending_tool: want === 'waiting' ? 'Bash' : '',
        pending_detail: want === 'waiting' ? 'run: git push --force origin main' : '',
        pending_risk: want === 'waiting' ? 'Force-pushes over remote history' : '',
        blocked: i === 1 ? 2 : 0,
        // The card counts work from the transcript now, not from the hooks, so
        // the demo has to speak in the same field or the "3 running" chip
        // silently disappears from every screenshot taken of it.
        outstanding: want === 'running' && i === 0 ? 3 : 0,
        kind,
        headline,
        // In a real session this is whatever Claude last said or thought; the
        // demo needs one so the line can be designed at its real length.
        narration: DEMO_LINES[chat]?.[tick % DEMO_LINES[chat].length] ?? '',
        activity: `${kind} something`,
        detail: want === 'failed' ? 'expected 03:00 to be 02:00' : '',
        updated_ms: now,
        started_ms: started,
        turn_started_ms: started,
        // A finished turn's clock is stopped in a real session, so it has to be
        // stopped here too — a demo that counts upwards over "Done" would be
        // designing against a bug the app no longer has.
        turn_ended_ms: outcome ? now : 0,
        turn_tools: 12 + tick * 3,
        recent: [{ ms: now, state: want, text: `${kind} something` }]
      }
    })
    tick += 1
    render()
  }
  advance()
  setInterval(advance, 2600)
  setInterval(render, 1000)

  // `?panel=welcome` / `?panel=doctor` so both can be designed in a browser
  // without a build, a real install, or a broken machine to point them at.
  const wanted = new URLSearchParams(location.search).get('panel')
  if (wanted === 'welcome') showWelcome()
  if (wanted === 'doctor') {
    openPanel('Setup check', () => [
      para('Rendering with sample results. The real check needs the app.'),
      ...[
        ['ok', 'Claude Code hooks', 'All 15 events registered.'],
        ['fail', 'Hook program', 'The hooks point at a copy that no longer exists.'],
        ['ok', 'Session folder', '~/.pipsqueak/sessions is writable.'],
        ['warn', 'Recent activity', 'No sessions recorded yet.']
      ].map(([status, label, detail]) => {
        const row = document.createElement('div')
        row.className = 'check'
        row.dataset.status = status
        const dot = document.createElement('span')
        dot.className = 'dot'
        const text = document.createElement('span')
        text.className = 'check-text'
        const strong = document.createElement('strong')
        strong.className = 'check-label'
        strong.textContent = label
        const span = document.createElement('span')
        span.className = 'check-detail'
        span.textContent = detail
        text.append(strong, span)
        row.append(dot, text)
        return row
      }),
      action('Test the connection', () => {}),
      action('Copy report', () => {})
    ])
  }
}

boot()
