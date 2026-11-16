//! The hook side: read one Claude Code hook payload from stdin, translate it
//! into a pet state plus a human-readable activity line, persist it.
//!
//! Invoked as `pipsqueak hook <EventName>` once per hook event, so it must be
//! cheap, silent, and incapable of failing loudly. Anything printed to stdout
//! would be parsed by Claude Code as hook output.

use crate::process;
use crate::project;
use crate::state::{now_ms, sanitize, sessions_dir, write_atomic, FileLock, Session};
use crate::text::{basename_or, first_line, summary_of, truncate};
use serde_json::Value;
use std::fs;
use std::io::Read;

/// Events that mean the session moved forward, and so cannot still be blocked
/// on a human or finished.
/// Subagent events are deliberately absent. They say something about work the
/// *turn* delegated, not about the turn itself, so a subagent finishing must
/// never clear an outcome or resolve a prompt — which is what used to relabel
/// a finished card "Delegating" with nothing running.
const PROGRESS_EVENTS: [&str; 7] = [
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionDenied",
    "PreCompact",
    "PostCompact",
];

pub fn run(fallback_event: Option<String>) {
    // Stamped before anything that can block, and in particular before the
    // file lock. Read after it instead — which is where this used to be — and
    // a hook that waited 200ms for the lock gets a *later* timestamp than the
    // one that made it wait, which is precisely backwards and makes ordering
    // by this value worse than not ordering at all.
    let now = now_ms();
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    capture(&raw);
    let payload: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);

    let event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(fallback_event)
        .unwrap_or_default();
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    let path = sessions_dir().join(format!("{}.json", sanitize(&session_id)));

    if event == "SessionEnd" {
        // Hold the same lock every hook holds for its read-modify-write, so an
        // in-flight event cannot rename the file back into existence after
        // this delete. The lock file itself is left alone: removing it out
        // from under a live holder lets a third writer into the critical
        // section, and an orphaned lock expires on its own.
        //
        // Patiently: `acquire` gives up after 200ms and proceeds unlocked,
        // which is right for an event (losing it is worse than an interleaved
        // write) and wrong here, where deleting under a holder mid-write lets
        // its rename bring the whole session back as a zombie card. This
        // hook is async and nothing is waiting on it, so it can afford to
        // wait for a slow `project::resolve` to finish.
        let _lock = FileLock::acquire_within(&path, 3_000);
        let _ = fs::remove_file(&path);
        return;
    }

    // Held across the read and the write. Claude Code runs the hooks matching
    // one event in parallel, and without this two of them read the same
    // "before" and whichever renames last discards the other's event.
    let _lock = FileLock::acquire(&path);

    let mut session: Session = match fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
    {
        Some(session) => session,
        None => {
            // Only the events that genuinely begin work may create the file.
            // Hooks run detached and in parallel, so a straggler can arrive
            // after `SessionEnd` deleted the session; recreating it from
            // nothing resurrects a zombie card that sits there until the
            // sweep notices.
            if event != "SessionStart" && event != "UserPromptSubmit" {
                return;
            }
            Session::default()
        }
    };

    // Asked again on every event, and allowed to go both ways.
    //
    // The tempting version of this is sticky — decide once, on the theory that
    // a session cannot change what it is. It can: `claude attach <id>` opens a
    // background session in a terminal, and from that moment a person really
    // is sitting in front of it and really does want to be told when it needs
    // them. A flag that only ever went true would have silenced the alert on
    // exactly the session somebody had just walked over to. Every event of a
    // background session carries the marker (measured, see `is_background`),
    // so asking each time costs an environment lookup and stays true.
    session.background = is_background();

    let Some(update) = classify(&event, &payload) else {
        return;
    };
    apply(&mut session, &event, &payload, update, now);
    session.session_id = session_id;

    if let Ok(bytes) = serde_json::to_vec(&session) {
        let _ = write_atomic(&path, &bytes);
    }
}

/// Folds one classified event into the session file's state.
///
/// Split out from [`run`] so the judgements that decide whether a card is
/// telling the truth — which turn this is, whether the event is a straggler,
/// whether a subagent or the session itself did the work — can be tested
/// without a filesystem or a running Claude Code.
fn apply(session: &mut Session, event: &str, payload: &Value, mut update: Update, now: u64) {
    // A subagent reporting in after the turn ended is bookkeeping, not new
    // work. Taking it at face value cleared the outcome and relabelled a
    // finished card "Delegating", so the turn that had just been announced as
    // done went back to looking busy with nothing running.
    // Which agent this event belongs to. A subagent's tool calls arrive on
    // the *parent's* session id with `agent_id` set; see below.
    let by_subagent = payload
        .get("agent_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    let trailing_subagent = is_trailing_subagent(event, &by_subagent, session);
    if trailing_subagent {
        update.state = "";
        update.kind.clear();
        update.activity.clear();
        update.detail = None;
        update.headline = None;
        update.silent = true;
    }

    // Nobody is waiting on this one.
    //
    // A background agent ends a turn exactly as a chat does, and Claude Code
    // raises the same idle `Notification` for it — but what answers that
    // notification is the supervisor that started the session, not a person.
    // Taken at face value the card said "Needs you" about a session
    // `claude agents --json` was calling `busy`/`working` at the same moment,
    // and the one alert that is supposed to mean "go and look" started firing
    // at sessions running unattended by design.
    //
    // Suppressed here rather than in the frontend so the claim never reaches
    // the file at all: one writer, one rule, and nothing for a second reader
    // to disagree with. A permission prompt is deliberately not `resumable`
    // and still alerts — a background agent stopped on one is stuck for good,
    // which is the case the alert exists for.
    if session.background && update.resumable {
        // Never the words "Waiting for your reply", whichever way this goes:
        // no reply is what either of these is waiting on.
        update.waiting.clear();
        if session.outcome == "failed" {
            // Except when the turn did not end but broke. A leg that died on
            // an API error is not waiting for a supervisor and will not be
            // resumed by one: the chain is halted until a person looks at it,
            // and "Waiting to be resumed" written across that is the same lie
            // as "Needs you", pointing the other way. Seen live — a mission
            // leg stopped by a model safeguard, its card reading Failed with
            // this sentence underneath.
            //
            // Silent, so what stays on screen is the failure `StopFailure`
            // already recorded: the word, the headline, and the error body,
            // which this event carries none of and would otherwise blank.
            update.silent = true;
        } else {
            // The turn is over, so it is not running tools; it has not been
            // abandoned either, and the sweep leaves an idle session alone.
            update.state = "idle";
            update.activity = "Waiting to be resumed".to_string();
        }
    }

    // An event that reached this file after a later one already did.
    //
    // Hooks are installed with `async: true`, so Claude Code neither waits for
    // them nor delivers them in order. A `PreToolUse` that started before a
    // `Stop` and lands after it used to clear the outcome and set the state
    // back to "running" — the card came off "Done" with nothing running, which
    // is the shape of lie this whole file exists to avoid. Counters are
    // deliberately still applied: they are increments under a lock, and
    // dropping one loses a tool call that really did happen.
    let out_of_order = now < session.event_ms;
    if session.started_ms == 0 {
        session.started_ms = now;
    }
    if session.agent_pid == 0 {
        // Once per session: walking the process table is not free, and the
        // answer cannot change while the session lives.
        if let Some((pid, created)) = process::owner() {
            session.agent_pid = pid;
            session.agent_created = created;
        }
    }
    if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
        // Resolving walks the filesystem, so only redo it when the session
        // actually moves.
        if !cwd.is_empty() && cwd != session.cwd {
            let resolved = project::resolve(cwd);
            session.cwd = cwd.to_string();
            session.project = resolved.name;
            session.workspace = resolved.workspace;
            session.project_root = resolved.root.to_string_lossy().to_string();
            session.scratch = resolved.scratch;
        }
    }
    // Where the overlay can read what this session is saying while it works.
    if let Some(transcript) = payload.get("transcript_path").and_then(Value::as_str) {
        if !transcript.is_empty() {
            session.transcript = transcript.to_string();
        }
    }
    // Which turn this event belongs to.
    //
    // `agent_id` and `prompt_id` are in every hook payload and neither was
    // being read. Without the first, the card counted three subagents' work
    // as the main agent's actions and flipped its own status line to whatever
    // a subagent happened to be doing — "Editing spiralplan.py" while the
    // main agent was sitting still waiting for them. Narration already
    // refuses sidechain voices for exactly this reason; the tool line had no
    // equivalent.
    let prompt_id = payload
        .get("prompt_id")
        .and_then(Value::as_str)
        .unwrap_or_default();

    // A new turn, however it began.
    //
    // `UserPromptSubmit` is not the only way one starts: a stop hook can send
    // a turn back to work, and a resumed session carries on without a prompt.
    // Both left the counters and the clock running from the previous turn.
    // `prompt_id` is Claude Code's own answer to "which turn is this", so ask
    // it. Stragglers are excluded — an out-of-order event from the turn that
    // just ended would otherwise rotate the turn back and forth.
    let turn_changed = !out_of_order
        && !prompt_id.is_empty()
        && prompt_id != session.prompt_id
        // Never backwards. A prompt typed while a turn is running is submitted
        // then and answered later, so the two turns' events interleave, and
        // rotating on every change would reset the counters each time they
        // took it in turns to arrive.
        && prompt_id != session.prev_prompt_id;
    if turn_changed {
        session.prev_prompt_id = std::mem::take(&mut session.prompt_id);
        session.prompt_id = prompt_id.to_string();
    }
    if turn_changed || (prompt_id.is_empty() && event == "UserPromptSubmit") {
        session.turn_started_ms = now;
        session.turn_ended_ms = 0;
        session.turn_tools = 0;
        session.blocked = 0;
        session.agents.clear();
    }
    if event == "PreToolUse" {
        // The longest this call may legitimately run without any further
        // event. Extends, never shortens: two calls in flight are covered by
        // the later deadline, and a finished call simply lets it expire.
        session.tool_deadline_ms = session.tool_deadline_ms.max(now + tool_budget_ms(payload));
        session.tools += 1;
        // The status line reads "N actions · 4m", and the clock beside it is
        // the *turn's*. So the count has to be the turn's own work, not the
        // sum of everything its subagents did in parallel.
        if by_subagent.is_empty() {
            session.turn_tools += 1;
        }
    }

    // `Stop` and `StopFailure` clear it too: the turn ending resolves whatever
    // it was blocked on. Without this, a denied permission followed by the
    // turn ending left "Needs you" on the card for hours — the outcome the
    // same event carries is applied just below, after the slate is clean.
    //
    // Only the main agent's own events count. A subagent that outlives the
    // answer keeps making tool calls on the parent's session id, and taking
    // those as progress cleared the outcome — a finished card went back to
    // "Delegating" with nothing running — and cleared a permission prompt the
    // main agent was still sitting at.
    let resolves = PROGRESS_EVENTS.contains(&event) || event == "Stop" || event == "StopFailure";
    if resolves && !trailing_subagent && !out_of_order && by_subagent.is_empty() {
        session.clear_pending();
        // Work resuming inside the same turn — a stop hook sent it back, or
        // it carried on after a `Stop` the sweep had read as the end — is the
        // turn's clock starting again. Left frozen at the vetoed `Stop`, a
        // card read "12m" over a turn forty-six minutes in.
        if PROGRESS_EVENTS.contains(&event) {
            session.turn_ended_ms = 0;
        }
    }
    // Anything other than the prompt itself means the prompt is over.
    //
    // Including a `PreToolUse` for the very tool being asked about, which took
    // a wrong turn to arrive at. Claude Code runs `PreToolUse` first, where it
    // may answer the permission itself, and only then raises
    // `PermissionRequest`, so a `PreToolUse` *after* a pending prompt means
    // the tool is running and the prompt was answered. Trying to protect the
    // pending state from it instead left the card saying "needs you" about a
    // command that had already been approved and run.
    //
    // It is also the rule that survives being wrong about that order: if
    // `PermissionRequest` ever came first, the `PreToolUse` would clear it and
    // the next `PermissionRequest` would simply set it again.
    //
    // "Anything other than the prompt itself" was too broad, though. It was
    // written as an exclusion list of two, so a `SubagentStop`, a `PreCompact`
    // or a `SessionStart` cleared a prompt nobody had answered — and a
    // subagent finishing while the main agent sits at a permission prompt is
    // routine, not exotic. The card stopped saying "Needs you" while Claude
    // Code was still asking, and the sweep, which reads a cleared
    // `pending_since` as silence, was then free to call the session dead.
    // Only an event that proves the tool ran, was refused, or that the turn is
    // over may clear it now.
    const ANSWERS_PROMPT: [&str; 7] = [
        "PreToolUse",
        "PostToolUse",
        "PostToolUseFailure",
        "PermissionDenied",
        "UserPromptSubmit",
        "Stop",
        "StopFailure",
    ];
    //
    // ...and only by the agent that was asked. A subagent's prompt used to be
    // recorded and then unclearable: only a main-agent event could clear it,
    // so an answered prompt kept "Needs you" on the card for as long as the
    // subagent ran. The main agent's own events still clear anything, since
    // the main agent moving means Claude Code is no longer blocked.
    let answers_prompt = ANSWERS_PROMPT.contains(&event);
    let same_asker = by_subagent.is_empty() || by_subagent == session.pending_agent;
    if !session.pending_tool.is_empty() && answers_prompt && same_asker {
        session.clear_permission();
    }
    // Compare the detail too: two consecutive prompts for the same tool but
    // different commands must not leave the previous command's text, risk
    // label, and an already-elapsed debounce clock on the new prompt.
    //
    // Never from a straggler: a `PermissionRequest` for the last tool of a
    // turn can land after the `Stop` (auto-mode answers in milliseconds, and
    // three hook processes race), and nothing in that turn will ever arrive
    // to clear a prompt raised on a finished card.
    if !update.permission.is_empty()
        && !out_of_order
        && (session.pending_tool != update.permission
            || session.pending_detail != update.permission_detail)
    {
        session.pending_tool = update.permission.clone();
        session.pending_detail = update.permission_detail.clone();
        session.pending_risk = update.permission_risk.to_string();
        session.pending_agent = by_subagent.clone();
        session.pending_since = now;
    }
    if !update.state.is_empty() && !out_of_order {
        session.state = update.state.to_string();
    }
    if !update.outcome.is_empty() && !out_of_order {
        session.outcome = update.outcome.to_string();
        session.outcome_ms = now;
        session.settles_ms = now + update.settle_ms;
        // Stop the turn's clock. The card shows "N actions · 4m" beside the
        // word, and both belong to the turn — so once the turn is over they
        // are history, and history does not keep counting upward.
        session.turn_ended_ms = now;
        // The subagent count deliberately survives the outcome. Zeroing it
        // here destroyed the one piece of evidence that contradicts "Done":
        // the turn yielded the floor, but the work it delegated is still
        // running, and the card went green anyway. It drains on the
        // `SubagentStop` events that are already arriving.
    }
    if !update.waiting.is_empty() {
        // Keep the first timestamp. A second Notification about the same
        // prompt must not restart the debounce and hide the card again.
        if session.waiting_since == 0 {
            session.waiting_since = now;
        }
        session.waiting_reason = update.waiting.clone();
    }
    if update.blocked {
        session.blocked += 1;
    }
    // Subagents by name rather than by tally.
    //
    // It used to be a counter with two sources — a `Task` call adding one and
    // the `SubagentStart` for that same subagent adding another — against a
    // single `SubagentStop` taking one away. Three agents launched read as
    // six, and the count never came back to zero, which is the value
    // `is_trailing_subagent` tests to decide whether a straggling
    // `SubagentStop` is allowed to relabel a card. A set of the ids Claude
    // Code puts in the payload cannot double-count and cannot drift.
    if update.subagents > 0
        && !by_subagent.is_empty()
        && !session.agents.contains(&by_subagent)
        && session.agents.len() < 64
    {
        session.agents.push(by_subagent.clone());
    }
    if update.subagents < 0 {
        session.agents.retain(|id| *id != by_subagent);
    }
    session.subagents = session.agents.len() as u64;

    // What Claude Code itself says is still in flight. Carried by the `Stop`
    // family, which is where it is needed — the moment the card chooses
    // between "Done" and "Finishing" — and, as it turns out, by
    // `SubagentStop` too (observed live: two background shells listed on
    // one). Whoever carries it, the newest answer wins.
    if let Some(tasks) = payload.get("background_tasks").and_then(Value::as_array) {
        session.tasks = tasks
            .iter()
            .filter(|task| task.get("status").and_then(Value::as_str).unwrap_or("") == "running")
            .filter_map(|task| task.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        session.tasks_ms = now;
    }

    // Any event at all is proof of life, so whatever the sweep concluded is
    // no longer true.
    session.stalled = false;
    if !update.silent {
        // An event that does not describe work leaves the last word about work
        // standing, rather than blanking it or replacing it with its own.
        if !update.kind.is_empty() {
            // A subagent's tool call is the session delegating, not the
            // session reading a file. Letting its category through made the
            // status line say "Editing" over a turn whose own work was to sit
            // and wait for three agents to report back.
            session.kind = if by_subagent.is_empty() {
                update.kind
            } else {
                "Delegating".to_string()
            };
        }
        session.activity = update.activity.clone();
        if let Some(detail) = update.detail {
            session.detail = detail;
        }
        if let Some(headline) = update.headline {
            session.headline = headline;
        }
    }
    // High-water marks. A straggler used to move `event_ms` *backwards* to
    // its own stamp, so the ordering guard held for exactly one straggler and
    // the second one un-finished the turn.
    session.event = event.to_string();
    session.event_ms = session.event_ms.max(now);
    session.updated_ms = session.updated_ms.max(now);
    if !update.silent && !update.activity.is_empty() {
        let tag = if !update.outcome.is_empty() {
            update.outcome
        } else if !update.waiting.is_empty() {
            "waiting"
        } else if update.blocked {
            "blocked"
        } else if update.hiccup {
            "failed"
        } else if update.state.is_empty() {
            "running"
        } else {
            update.state
        };
        session.push_recent(tag, &update.activity, now);
    }
}

/// Is this hook running inside a background agent (`claude --bg`)?
///
/// The authoritative answer is `claude agents --json`, which reports
/// `"kind":"background"` per session — and disagreeing with it is the bug this
/// exists to fix. It is also an answer this hook cannot afford: it runs on
/// every event with a 5s budget, and paying a whole CLI startup for a fact
/// that cannot change would be the pet making the machine slower in order to
/// describe it.
///
/// So it is read from the environment instead, which costs nothing. **Not**
/// from `CLAUDE_CODE_SESSION_KIND`: that is set on the agent process and is
/// *not* passed down to hooks — measured, by catching live
/// `pipsqueak hook` processes in `/proc` and reading their environment while a
/// real `claude --bg` session ran. What hooks do get is `CLAUDE_JOB_DIR`, the
/// background job's own directory, and only background sessions have one.
/// Both are checked because the first is free and the day it starts being
/// forwarded is a day this gets more reliable, not less.
///
/// Measured 2026-08-29 against Claude Code 2.1.251:
///   background `SessionStart` hook -> CLAUDE_JOB_DIR=~/.claude/jobs/d14b29f0
///                                     CLAUDE_CODE_SESSION_KIND absent
///   foreground hooks (any event)   -> neither
fn is_background() -> bool {
    reads_as_background(|key| std::env::var(key).ok())
}

/// The rule itself, with the environment handed to it so it can be examined.
fn reads_as_background(get: impl Fn(&str) -> Option<String>) -> bool {
    if get("CLAUDE_JOB_DIR").is_some_and(|dir| !dir.trim().is_empty()) {
        return true;
    }
    matches!(
        get("CLAUDE_CODE_SESSION_KIND").as_deref(),
        Some("bg") | Some("background")
    )
}

/// Keeps a copy of a raw hook payload, when asked to.
///
/// Off unless `~/.pipsqueak/payloads` exists, which makes turning it on a
/// `mkdir` and turning it off a delete — no setting, no restart, and nothing
/// to leave switched on by accident. It exists because the hook payload is
/// the one contract here that is not ours: fields the pet leans on are absent
/// from the published schema, and "does this event actually carry that?" is
/// otherwise unanswerable without guessing.
///
/// Payloads contain prompts and assistant text, so this is deliberately
/// awkward to enable and never on by default.
fn capture(raw: &str) {
    let dir = crate::state::root().join("payloads");
    if !dir.is_dir() {
        return;
    }
    let event = serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| {
            v.get("hook_event_name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".into());
    let name = format!(
        "{}-{}-{}.json",
        now_ms(),
        sanitize(&event),
        std::process::id()
    );
    let _ = fs::write(dir.join(name), raw);
}

/// One state change.
///
/// Every field is optional. Most events answer only one of "what is it doing",
/// "how did the turn end", and "is a human blocking it", and an event that is
/// silent on a question must leave the existing answer alone.
/// Collapsing all three into a single `state` field is what let a card get
/// stuck on an alert that nothing ever cleared.
#[derive(Default)]
struct Update {
    /// New durable state. Empty leaves it unchanged.
    state: &'static str,
    /// The word on the status line. Kept coarse.
    kind: String,
    activity: String,
    /// The expanded panel's longer text: the prompt, an error body, the
    /// answer. `None` for the many events that carry none, which must leave
    /// the last one standing rather than blank it — a `Notification` landing
    /// after a `StopFailure` used to wipe the error out from under the word
    /// "Failed", leaving a card that said a turn had failed and could not say
    /// how. `Some("")` still clears, which is what a turn ending with nothing
    /// worth reading should do.
    detail: Option<String>,
    /// `None` for the many events that should not disturb what the card says
    /// the turn is about.
    headline: Option<String>,
    /// How the turn ended, and how long to treat that as provisional.
    outcome: &'static str,
    settle_ms: u64,
    /// Why a human is blocking. Empty means "not waiting".
    waiting: String,
    /// True when the only thing this wait needs is another turn — the idle
    /// prompt Claude Code raises at the end of every turn, foreground or not.
    /// A background agent's supervisor answers that itself, so for those
    /// sessions it is not a wait on anybody. A permission prompt is never
    /// this: nothing but a decision resolves one, and a background agent stuck
    /// on it is stuck for good.
    resumable: bool,
    /// Change to the live subagent count.
    subagents: i64,
    /// A tool failed mid-turn. Not a failed turn, and not counted anywhere:
    /// it only decides how this line is coloured in the log.
    hiccup: bool,
    /// The auto-mode classifier refused a tool call. Counted on the card.
    blocked: bool,
    /// A permission prompt was raised for this tool. Empty means "no change".
    permission: String,
    permission_detail: String,
    permission_risk: &'static str,
    /// Record the event, but leave everything the card displays alone.
    silent: bool,
}

/// The command a Bash-shaped tool call would run, if it is one.
fn bash_command(tool: &str, input: Option<&Value>) -> Option<String> {
    if tool != "Bash" && tool != "PowerShell" {
        return None;
    }
    input
        .and_then(|v| v.get("command"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn live(state: &'static str, kind: &str, activity: String) -> Update {
    Update {
        state,
        kind: kind.to_string(),
        activity,
        ..Default::default()
    }
}

/// A tool call the host app made on a plugin's behalf, not one the agent chose.
///
/// The Claude desktop app lets plugins and mods call tools inside every open
/// chat, and those calls fire the same Pre/PostToolUse hooks as the agent's
/// own. A side-pane mod polling `ccd_session_mgmt` every twenty seconds made
/// every idle chat on the machine read as Working, and wiped the Done its last
/// turn had earned (measured 2026-11-16, seven cards, two real turns). The
/// app marks these calls in the id it hands the hook: the agent's are
/// `toolu_01…`, the host's are `toolu_plugin_…`.
fn is_host_tool_call(payload: &Value) -> bool {
    payload
        .get("tool_use_id")
        .and_then(Value::as_str)
        .is_some_and(|id| id.starts_with("toolu_plugin_"))
}

/// Returns `None` for events the pet ignores.
fn classify(event: &str, payload: &Value) -> Option<Update> {
    // Not progress, not a turn, not the agent: the session file must not move
    // at all, or `updated_ms` alone would keep a dead chat looking alive.
    if matches!(event, "PreToolUse" | "PostToolUse" | "PostToolUseFailure")
        && is_host_tool_call(payload)
    {
        return None;
    }
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let tool = text("tool_name");
    let input = payload.get("tool_input");

    Some(match event {
        // Auto-compaction fires `SessionStart` again mid-session with
        // `source: "compact"` (doc-verified). Treating it as a fresh session
        // flipped a working card to an idle "Session started" and stole the
        // headline for the rest of the turn.
        "SessionStart" if text("source") == "compact" => {
            live("thinking", "Thinking", "Context compacted".into())
        }
        "SessionStart" => Update {
            state: "idle",
            kind: "Idle".into(),
            headline: Some("Session started".into()),
            ..Default::default()
        },
        "UserPromptSubmit" => {
            let prompt = text("prompt");
            Update {
                state: "thinking",
                kind: "Thinking".into(),
                activity: "Thinking…".into(),
                detail: Some(truncate(&prompt, 400)),
                headline: Some(
                    summary_of(&prompt).unwrap_or_else(|| truncate(&first_line(&prompt), 90)),
                ),
                ..Default::default()
            }
        }
        // A Task call *is* a subagent launch, and is often the only signal one
        // happened, since Claude Code does not always send a matching SubagentStart.
        "PreToolUse" if tool == "Task" || tool == "Agent" => Update {
            state: "running",
            kind: "Delegating".into(),
            activity: phrase(&tool, input).present,
            subagents: 1,
            ..Default::default()
        },
        "PreToolUse" => live("running", &kind_of(&tool), phrase(&tool, input).present),
        // The matching PostToolUse is *not* a subagent stop: the
        // dedicated SubagentStop event owns that, and counting both would
        // decrement twice.
        "PostToolUse" => live("running", &kind_of(&tool), phrase(&tool, input).past),
        // One tool failing is not the turn failing. Claude nearly always
        // tries something else. Recorded as a mark on the turn, not a state.
        "PostToolUseFailure" => Update {
            state: "running",
            kind: "Recovering".into(),
            activity: format!("{} failed", pretty_tool(&tool)),
            detail: Some(truncate(&first_line(&text("error")), 400)),
            hiccup: true,
            ..Default::default()
        },
        // PermissionRequest fires *before* anyone is asked. Auto-mode and
        // permission hooks resolve most of them in milliseconds, so believing
        // it outright made the pet cry wolf constantly. Recording it costs
        // nothing: the frontend only surfaces a prompt that outlives the
        // debounce, by which point a human really is being asked.
        //
        // Note what this hook does *not* do. It writes a file and exits 0 with
        // empty stdout. It never prints a decision, so it cannot approve or
        // deny a tool call, and Claude Code's own prompt appears exactly as it
        // would if we were not installed at all.
        "PermissionRequest" => Update {
            // Nothing visible changes yet. The card keeps showing whatever the
            // session was doing until the prompt outlives the debounce.
            silent: true,
            permission: if tool.is_empty() {
                "a tool".into()
            } else {
                tool.clone()
            },
            permission_detail: phrase(&tool, input).infinitive,
            permission_risk: bash_command(&tool, input)
                .and_then(|command| crate::risk::irreversible(&command))
                .unwrap_or_default(),
            ..Default::default()
        },
        // Auto-mode declining a call is routine policy, not a failure — the
        // documented meaning of this event is "denied by the auto mode
        // classifier", so it is exactly the thing worth counting: it says the
        // permission rules, not Claude, are what this turn is fighting.
        "PermissionDenied" => Update {
            blocked: true,
            ..live(
                "running",
                &kind_of(&tool),
                format!("Auto-mode blocked: {}", phrase(&tool, input).infinitive),
            )
        },
        "Notification" => {
            let notification = text("notification_type");
            let message = truncate(&text("message"), 200);
            // `state` stays empty: being blocked does not change what the
            // session was doing, and overwriting it here is what used to leave
            // a card stuck on "Needs you" when the answer arrived elsewhere.
            // `kind` is deliberately left alone. The card says "Needs you" off
            // the *state*, which only becomes `waiting` once the prompt has
            // outlived the debounce; writing the word here as well put it on
            // cards that were still plainly running, which is the pet claiming
            // something that was not true yet.
            let waiting = |reason: &str| Update {
                activity: reason.to_string(),
                waiting: reason.to_string(),
                ..Default::default()
            };
            // The turn simply ended. For a foreground chat that means the
            // person is up; for a background agent it means the supervisor is.
            let turn_over = |reason: &str| Update {
                resumable: true,
                ..waiting(reason)
            };
            match notification.as_str() {
                "permission_prompt" => waiting(if message.is_empty() {
                    "Waiting for permission"
                } else {
                    &message
                }),
                "idle_prompt" => turn_over("Waiting for your reply"),
                "agent_needs_input" => waiting("A teammate needs input"),
                // `notification_type` is not in the documented payload schema,
                // so it may simply be absent. Falling back to the message text
                // keeps "needs you" working instead of silently never firing.
                "" if !message.is_empty() => {
                    let lower = message.to_lowercase();
                    if lower.contains("permission") {
                        waiting(&message)
                    } else if lower.contains("waiting for your") || lower.contains("input") {
                        turn_over("Waiting for your reply")
                    } else {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        "Stop" => {
            let message = text("last_assistant_message");
            let summary = summary_of(&message);
            match stop_disposition(payload, &message) {
                // Claude is not finished; it is about to carry on. Announcing
                // "Done" here is the single easiest way for the pet to lie,
                // and the 30s the card lingers makes the lie outlast the turn.
                Stop::KeepWorking(reason) => Update {
                    state: "running",
                    kind: "Working".into(),
                    activity: reason.to_string(),
                    ..Default::default()
                },
                Stop::Finished { settle_ms } => Update {
                    // The turn is over, so the session is no longer *doing*
                    // anything. Leaving this as "running" would have the sweep
                    // decide five minutes later that a finished session had
                    // stopped responding, and say so over the answer.
                    state: "idle",
                    kind: "Done".into(),
                    // The turn is over: a live line would only show a stale
                    // tool call.
                    detail: Some(truncate(&message, 600)),
                    // Nothing worth reading in the answer leaves the headline
                    // alone, and what is already there is what the turn was
                    // asked to do, which is still true.
                    headline: summary,
                    outcome: "done",
                    settle_ms,
                    ..Default::default()
                },
            }
        }
        "StopFailure" => {
            let error_kind = text("error_type");
            Update {
                state: "idle",
                kind: "Failed".into(),
                detail: Some(truncate(&first_line(&text("error")), 400)),
                headline: Some(if error_kind.is_empty() {
                    "Turn failed".into()
                } else {
                    format!("Turn failed: {}", error_kind.replace('_', " "))
                }),
                outcome: "failed",
                ..Default::default()
            }
        }
        "SubagentStart" => Update {
            state: "running",
            kind: "Delegating".into(),
            activity: format!("Subagent: {}", short(&text("agent_type"), "agent")),
            subagents: 1,
            ..Default::default()
        },
        "SubagentStop" => Update {
            state: "running",
            kind: "Delegating".into(),
            activity: "Subagent finished".into(),
            subagents: -1,
            ..Default::default()
        },
        "PreCompact" => live("compacting", "Compacting", "Compacting context".into()),
        // Compaction finishing is not the *turn* finishing. An automatic
        // compact happens mid-turn and work resumes straight after; only a
        // manual one leaves the session genuinely idle.
        "PostCompact" => {
            if text("trigger") == "manual" {
                live("idle", "Idle", "Context compacted".into())
            } else {
                live("thinking", "Thinking", "Context compacted".into())
            }
        }
        _ => return None,
    })
}

/// What a `Stop` event actually means.
enum Stop {
    /// Claude will keep going. There is nothing to celebrate yet.
    KeepWorking(&'static str),
    /// The turn is over. `settle_ms` holds the result provisional for a
    /// moment so a follow-up event can cancel it.
    Finished { settle_ms: u64 },
}

/// `Stop` does not reliably mean "turn finished".
///
/// Claude Code fires it whenever the assistant yields the floor, which
/// includes cases where it is about to immediately continue. Three payload
/// fields say so, and taking them at face value is the difference between a
/// pet that reports completions and a pet that guesses at them.
fn stop_disposition(payload: &Value, last_message: &str) -> Stop {
    let non_empty_list = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_array)
            .map(|items| !items.is_empty())
            .unwrap_or(false)
    };

    if non_empty_list("session_crons") {
        return Stop::KeepWorking("Scheduled work still running");
    }
    // `stop_hook_active` is set on the Stop that *ends* a turn a stop hook
    // extended, not on the one it vetoed — the veto has already happened and
    // Claude has already done the extra work. Reading it as "still working"
    // meant that anyone with a blocking stop hook installed (a completion
    // gate, a journal prompt, a review loop) never saw a finished card at all:
    // every turn ended on this branch and stayed grey. Measured 2026-08-18
    // with the installed 0.8.0 build, one synthetic Stop per case:
    //   stop_hook_active=false -> state=idle kind=Done outcome=done
    //   stop_hook_active=true  -> state=running kind=Working outcome=""
    //
    // It is still a stop that can be vetoed again, so it takes the same hold
    // as the rest: a further veto means more work, and work clears a pending
    // outcome before it is ever shown.
    if payload
        .get("stop_hook_active")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Stop::Finished { settle_ms: 2_000 };
    }
    if non_empty_list("background_tasks") {
        return if last_message.trim().is_empty() {
            Stop::KeepWorking("Background tasks still running")
        } else {
            // It said its piece, but something is still finishing behind it.
            // Hold the result briefly rather than suppress it.
            Stop::Finished { settle_ms: 2_000 }
        };
    }
    // Even a clean-looking end stays provisional for a moment. A blocking
    // stop hook (a review gate, a completion loop) can veto this stop and
    // have the turn carry straight on — `stop_hook_active` only says so on
    // the *second* Stop, after the fact. Without the hold, every vetoed stop
    // flashed a Done card that the next tool event immediately wiped.
    Stop::Finished { settle_ms: 2_000 }
}

/// The coarse category behind the status line. Several different tools map to
/// one word: it should survive a whole stretch of work, so the only thing
/// moving is the counter next to it.
fn kind_of(tool: &str) -> String {
    if let Some(rest) = tool.strip_prefix("mcp__") {
        let server = rest.split("__").next().unwrap_or("MCP");
        return format!("Calling {server}");
    }
    match tool {
        "Read" | "Glob" | "Grep" | "NotebookRead" => "Reading",
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => "Editing",
        "Bash" | "PowerShell" => "Running",
        "WebFetch" | "WebSearch" => "Browsing",
        "Task" | "Agent" => "Delegating",
        "Skill" => "Running a skill",
        "TodoWrite" | "TaskCreate" | "TaskUpdate" | "TaskList" | "TaskGet" => "Planning",
        _ => "Working",
    }
    .to_string()
}

/// One tool call, in the three tenses the bubble needs: "Editing clock.js",
/// "Edited clock.js", "…permission to edit clock.js".
struct Phrase {
    present: String,
    past: String,
    infinitive: String,
}

fn forms(ing: &str, ed: &str, base: &str, object: &str) -> Phrase {
    let object = object.trim();
    let join = |verb: &str| {
        if object.is_empty() {
            verb.trim_end_matches(':').to_string()
        } else {
            format!("{verb} {object}")
        }
    };
    Phrase {
        present: join(ing),
        past: join(ed),
        infinitive: join(base),
    }
}

fn phrase(tool: &str, input: Option<&Value>) -> Phrase {
    let field = |key: &str| {
        input
            .and_then(|v| v.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    if let Some(rest) = tool.strip_prefix("mcp__") {
        let mut parts = rest.splitn(2, "__");
        let server = parts.next().unwrap_or("mcp");
        let name = parts.next().unwrap_or("call").replace('_', " ");
        return forms("Calling", "Called", "call", &format!("{server}: {name}"));
    }

    match tool {
        "Bash" | "PowerShell" => {
            let description = field("description");
            let command = field("command");
            let shown = if description.is_empty() {
                command
            } else {
                description
            };
            forms("Running:", "Ran:", "run:", &truncate(&shown, 64))
        }
        "Read" => forms(
            "Reading",
            "Read",
            "read",
            &basename_or(&field("file_path"), "a file"),
        ),
        "Edit" => forms(
            "Editing",
            "Edited",
            "edit",
            &basename_or(&field("file_path"), "a file"),
        ),
        "Write" => forms(
            "Writing",
            "Wrote",
            "write",
            &basename_or(&field("file_path"), "a file"),
        ),
        "NotebookEdit" => forms(
            "Editing",
            "Edited",
            "edit",
            &basename_or(&field("notebook_path"), "a notebook"),
        ),
        "Glob" => forms(
            "Finding",
            "Found",
            "find",
            &short(&field("pattern"), "files"),
        ),
        "Grep" => forms(
            "Searching for",
            "Searched for",
            "search for",
            &truncate(&field("pattern"), 40),
        ),
        "WebFetch" => forms("Fetching", "Fetched", "fetch", &host_of(&field("url"))),
        "WebSearch" => forms(
            "Searching the web for",
            "Searched the web for",
            "search the web for",
            &truncate(&field("query"), 40),
        ),
        "Task" | "Agent" => forms(
            "Delegating to",
            "Heard back from",
            "delegate to",
            &short(&field("subagent_type"), "a subagent"),
        ),
        "Skill" => forms("Running skill", "Ran skill", "run skill", &field("skill")),
        "TodoWrite" | "TaskCreate" | "TaskUpdate" => {
            forms("Updating", "Updated", "update", "tasks")
        }
        "" => forms("Working", "Worked", "work", ""),
        other => Phrase {
            present: other.to_string(),
            past: format!("{other} finished"),
            infinitive: format!("use {other}"),
        },
    }
}

fn pretty_tool(tool: &str) -> String {
    if tool.is_empty() {
        "A tool".to_string()
    } else {
        tool.to_string()
    }
}

fn host_of(url: &str) -> String {
    let without_scheme = url.split("//").last().unwrap_or(url);
    let host = without_scheme.split('/').next().unwrap_or(without_scheme);
    if host.is_empty() {
        "a page".to_string()
    } else {
        host.to_string()
    }
}

fn short(value: &str, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        truncate(value, 40)
    }
}

/// A subagent event that arrives after the turn it belonged to has ended.
///
/// Claude Code sends `SubagentStop` for work that outlives the answer, so this
/// is routine rather than exceptional, and it must not restart the turn. The
/// same goes for every tool call that subagent makes in the meantime: they
/// arrive stamped with its `agent_id` on the parent's session, and the turn
/// that announced itself done is still done while they happen.
fn is_trailing_subagent(event: &str, by_subagent: &str, session: &Session) -> bool {
    match event {
        "SubagentStart" => !session.outcome.is_empty(),
        // A `SubagentStop` with nothing outstanding is also bookkeeping when
        // the next prompt has already cleared the outcome: without the count
        // check, a straggler from the previous turn relabelled a fresh
        // "Thinking…" card as "Delegating" with nothing running.
        "SubagentStop" => !session.outcome.is_empty() || session.subagents == 0,
        _ => !by_subagent.is_empty() && !session.outcome.is_empty(),
    }
}

/// How long a tool call may run before silence means something is wrong.
///
/// The sweep reads five minutes without an event as a turn that died, on the
/// grounds that Claude Code fires a hook per tool call. One tool call can
/// legally take longer than that: `Bash` accepts a timeout of up to ten
/// minutes, and a seven-minute build was twice called "Stopped responding"
/// while its process was alive and working. The call's own timeout is the
/// honest budget when the payload carries one; otherwise the longest budget
/// Claude Code allows, plus a moment for the hook after it to arrive.
fn tool_budget_ms(payload: &Value) -> u64 {
    const DEFAULT_MS: u64 = 10 * 60 * 1000;
    const CEILING_MS: u64 = 60 * 60 * 1000;
    const GRACE_MS: u64 = 60 * 1000;
    let declared = payload
        .get("tool_input")
        .and_then(|input| input.get("timeout"))
        .and_then(Value::as_u64)
        // Bash declares milliseconds; a tool that declares seconds would
        // read as a value no millisecond timeout ever takes.
        .map(|timeout| {
            if timeout < 1000 {
                timeout * 1000
            } else {
                timeout
            }
        });
    declared.unwrap_or(DEFAULT_MS).min(CEILING_MS) + GRACE_MS
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn activity_of(event: &str, payload: serde_json::Value) -> String {
        classify(event, &payload)
            .expect("event should be classified")
            .activity
    }

    #[test]
    fn bash_prefers_the_description_over_the_raw_command() {
        let payload = json!({
            "tool_name": "Bash",
            "tool_input": { "command": "npm test -- --run", "description": "Run unit tests" }
        });
        assert_eq!(
            activity_of("PreToolUse", payload),
            "Running: Run unit tests"
        );
    }

    #[test]
    fn file_tools_show_only_the_basename() {
        let payload = json!({
            "tool_name": "Edit",
            "tool_input": { "file_path": "C:\\code\\app\\src\\render.js" }
        });
        assert_eq!(activity_of("PreToolUse", payload), "Editing render.js");
    }

    #[test]
    fn finished_tools_are_reported_in_the_past_tense() {
        let payload = json!({
            "tool_name": "Edit",
            "tool_input": { "file_path": "/code/render.js" }
        });
        assert_eq!(activity_of("PostToolUse", payload), "Edited render.js");
    }

    #[test]
    fn mcp_tools_are_split_into_server_and_call() {
        let payload = json!({ "tool_name": "mcp__github__create_issue" });
        assert_eq!(
            activity_of("PreToolUse", payload),
            "Calling github: create issue"
        );
    }

    /// PermissionRequest fires before anyone is asked, and auto-mode resolves
    /// most of them instantly. Reacting to it made the pet claim to be blocked
    /// on turns that were never interrupted, so it is recorded and the card
    /// only surfaces prompts that outlive the debounce.
    #[test]
    fn permission_requests_alone_do_not_mean_blocked() {
        let update = classify(
            "PermissionRequest",
            &json!({ "tool_name": "Bash", "tool_input": { "command": "rm -rf build" } }),
        )
        .unwrap();
        assert!(update.silent, "nothing on the card may change yet");
        assert!(update.waiting.is_empty());
        assert!(update.state.is_empty());
        assert_eq!(update.permission, "Bash");
        assert_eq!(update.permission_risk, "Deletes a directory tree");
    }

    #[test]
    fn an_ordinary_prompt_carries_no_warning() {
        let update = classify(
            "PermissionRequest",
            &json!({ "tool_name": "Bash", "tool_input": { "command": "npm test" } }),
        )
        .unwrap();
        assert_eq!(update.permission, "Bash");
        assert!(update.permission_risk.is_empty());
    }

    #[test]
    fn only_a_real_prompt_means_blocked() {
        let update = classify(
            "Notification",
            &json!({ "notification_type": "permission_prompt", "message": "Allow Bash?" }),
        )
        .unwrap();
        assert_eq!(update.waiting, "Allow Bash?");
        // Not even the word: "Needs you" is the card's name for the *state*,
        // and the state is only blocked once the prompt outlives the debounce.
        // Writing it here put it on cards whose dot was still plainly running.
        assert!(update.kind.is_empty());
        // Being blocked says nothing about what the session was doing, so the
        // durable state is left alone. Overwriting it here is what used to
        // strand a card on "Needs you" when the prompt was answered elsewhere.
        assert!(update.state.is_empty());
    }

    #[test]
    fn related_tools_share_one_status_word() {
        for tool in ["Read", "Glob", "Grep"] {
            assert_eq!(kind_of(tool), "Reading");
        }
        for tool in ["Edit", "Write", "NotebookEdit"] {
            assert_eq!(kind_of(tool), "Editing");
        }
        assert_eq!(kind_of("mcp__github__create_issue"), "Calling github");
    }

    #[test]
    fn stop_reports_the_first_line_of_the_answer_as_the_headline() {
        let payload = json!({
            "last_assistant_message": "\n\nFixed the off-by-one.\nDetails follow."
        });
        let update = classify("Stop", &payload).unwrap();
        assert_eq!(update.outcome, "done");
        // Provisional even when clean: a blocking stop hook can veto this
        // stop, and zero settle time made every vetoed stop flash a Done card.
        assert_eq!(update.settle_ms, 2_000);
        // The turn is over, so the session is not running any more. Leaving it
        // running would have the staleness sweep decide five minutes later
        // that a finished session had stopped responding, and say so over the
        // answer it had already given.
        assert_eq!(update.state, "idle");
        assert_eq!(update.headline.as_deref(), Some("Fixed the off-by-one."));
        // The turn is over, so the live line clears rather than showing a stale tool.
        assert!(update.activity.is_empty());
        assert!(update
            .detail
            .as_deref()
            .unwrap()
            .contains("Details follow."));
    }

    /// The ways `Stop` arrives while Claude is about to carry on. Each one used
    /// to produce a "Done" card that then sat there for 30 seconds being wrong.
    #[test]
    fn stop_while_still_working_is_not_a_completion() {
        let cases = [
            json!({ "session_crons": [{ "id": "nightly" }] }),
            json!({ "background_tasks": [{ "id": "build" }] }),
        ];
        for payload in cases {
            let update = classify("Stop", &payload).unwrap();
            assert!(
                update.outcome.is_empty(),
                "{payload} should not report a completion"
            );
            assert_eq!(update.state, "running");
        }
    }

    /// A turn a stop hook extended still ends, and the Stop carrying
    /// `stop_hook_active` is that ending. Treating the flag as "still working"
    /// is why a session with any blocking stop hook installed never went green.
    #[test]
    fn stop_after_a_hook_extended_the_turn_is_a_completion() {
        let update = classify(
            "Stop",
            &json!({
                "stop_hook_active": true,
                "last_assistant_message": "Wrote the journal entry."
            }),
        )
        .unwrap();
        assert_eq!(update.outcome, "done");
        assert_eq!(update.state, "idle");
        // Held, not announced: another veto is still possible, and any work
        // that follows clears a pending outcome before it is ever shown.
        assert_eq!(update.settle_ms, 2_000);
    }

    /// Claude said its piece but something is still finishing behind it. That
    /// is a real completion, just one worth holding briefly in case a follow
    /// up event cancels it.
    #[test]
    fn stop_with_background_work_and_an_answer_settles_late() {
        let update = classify(
            "Stop",
            &json!({
                "background_tasks": [{ "id": "build" }],
                "last_assistant_message": "Kicked off the build."
            }),
        )
        .unwrap();
        assert_eq!(update.outcome, "done");
        assert_eq!(update.settle_ms, 2_000);
    }

    /// An automatic compaction happens *inside* a turn; work resumes straight
    /// after. Reporting it as a finished turn is a false completion.
    #[test]
    fn compaction_finishing_is_not_the_turn_finishing() {
        let auto = classify("PostCompact", &json!({ "trigger": "auto" })).unwrap();
        assert_eq!(auto.state, "thinking");
        assert!(auto.outcome.is_empty());

        let manual = classify("PostCompact", &json!({ "trigger": "manual" })).unwrap();
        assert_eq!(manual.state, "idle");
        assert!(manual.outcome.is_empty());
    }

    /// Claude Code reports many subagent launches only as a `Task` tool call,
    /// so counting `SubagentStart` alone undercounts them.
    #[test]
    fn a_task_call_counts_as_a_subagent() {
        let update = classify(
            "PreToolUse",
            &json!({ "tool_name": "Task", "tool_input": { "subagent_type": "explore" } }),
        )
        .unwrap();
        assert_eq!(update.subagents, 1);
        assert_eq!(update.kind, "Delegating");

        // The paired PostToolUse must not decrement: SubagentStop owns that,
        // and counting both would take the same subagent away twice.
        let done = classify("PostToolUse", &json!({ "tool_name": "Task" })).unwrap();
        assert_eq!(done.subagents, 0);
    }

    /// A mod polling through the desktop app is not the agent working. Payload
    /// shape copied from a real idle chat, 2026-11-16.
    #[test]
    fn a_tool_call_the_host_made_for_a_plugin_is_ignored() {
        let polled = |event: &str, id: &str| {
            classify(
                event,
                &json!({
                    "hook_event_name": event,
                    "tool_name": "mcp__ccd_session_mgmt__list_sessions",
                    "tool_input": { "limit": 30 },
                    "tool_use_id": id,
                    "mcp_server": { "name": "ccd_session_mgmt", "source": "sdk" }
                }),
            )
        };
        for event in ["PreToolUse", "PostToolUse", "PostToolUseFailure"] {
            assert!(
                polled(event, "toolu_plugin_11a034a7c1bc40068faba2f99bec6261").is_none(),
                "{event} from a plugin must not touch the card"
            );
            // The same tool, chosen by the agent, is real work.
            assert!(polled(event, "toolu_01AGxB4N7Y7NKJxXJJLyxeG8").is_some());
        }
    }

    // --- what one event does to the file ---------------------------------
    //
    // Every number the card shows comes out of `apply`, so these are the tests
    // that decide whether the status line is telling the truth.

    /// Runs one event against a session, the way `run` would.
    fn feed(session: &mut Session, event: &str, payload: serde_json::Value, now: u64) {
        let update = classify(event, &payload).expect("event should be classified");
        apply(session, event, &payload, update, now);
    }

    fn tool_call(prompt: &str) -> serde_json::Value {
        json!({ "tool_name": "Read", "prompt_id": prompt, "tool_input": { "file_path": "a.rs" } })
    }

    fn prompt(session: &mut Session) {
        feed(
            session,
            "UserPromptSubmit",
            json!({ "prompt_id": "p1", "prompt": "go" }),
            1000,
        );
    }

    fn stop(session: &mut Session, now: u64) {
        feed(
            session,
            "Stop",
            json!({ "prompt_id": "p1", "last_assistant_message": "Done." }),
            now,
        );
    }

    /// A subagent that outlives the answer keeps making tool calls, and they
    /// arrive on the parent's session id. They used to un-finish the turn:
    /// the card went from "Done" back to "Delegating" with nothing that would
    /// ever set it right again.
    #[test]
    fn a_subagents_tool_call_after_stop_does_not_unfinish_the_turn() {
        let mut session = Session::default();
        prompt(&mut session);
        feed(
            &mut session,
            "SubagentStart",
            json!({ "prompt_id": "p1", "agent_id": "a1", "agent_type": "x" }),
            1100,
        );
        stop(&mut session, 9000);
        assert_eq!(session.outcome, "done");

        let mut delegated = tool_call("p1");
        delegated["agent_id"] = json!("a1");
        feed(&mut session, "PreToolUse", delegated, 9500);
        assert_eq!(session.outcome, "done");
        assert_eq!(session.state, "idle");
        assert_eq!(session.kind, "Done");
        // The call still happened and is still counted.
        assert_eq!(session.tools, 1);

        // The main agent picking the work back up is a different matter.
        feed(&mut session, "PreToolUse", tool_call("p1"), 9600);
        assert_eq!(session.outcome, "");
        assert_eq!(session.state, "running");
    }

    /// The main agent is at a permission prompt while a subagent works on.
    #[test]
    fn a_subagents_tool_call_does_not_answer_the_main_agents_prompt() {
        let mut session = Session::default();
        prompt(&mut session);
        feed(
            &mut session,
            "PermissionRequest",
            json!({ "prompt_id": "p1", "tool_name": "Bash", "tool_input": { "command": "rm -rf build" } }),
            2000,
        );
        feed(
            &mut session,
            "Notification",
            json!({ "prompt_id": "p1", "notification_type": "permission_prompt", "message": "Claude needs your permission to use Bash" }),
            2100,
        );
        assert_eq!(session.waiting_since, 2100);

        let mut delegated = tool_call("p1");
        delegated["agent_id"] = json!("a1");
        feed(&mut session, "PreToolUse", delegated, 2500);
        assert!(session.pending_since > 0);
        assert_eq!(session.waiting_since, 2100);
    }

    /// A stop-hook continuation keeps the prompt id. The turn's clock has to
    /// start again when the work does, or the card freezes at "12m" over a
    /// turn that is forty minutes in.
    #[test]
    fn the_turn_clock_restarts_when_the_same_turn_resumes() {
        let mut session = Session::default();
        prompt(&mut session);
        stop(&mut session, 9000);
        assert_eq!(session.turn_ended_ms, 9000);

        feed(&mut session, "PreToolUse", tool_call("p1"), 12000);
        assert_eq!(session.outcome, "");
        assert_eq!(session.state, "running");
        assert_eq!(session.turn_ended_ms, 0);

        stop(&mut session, 15000);
        assert_eq!(session.turn_ended_ms, 15000);
    }

    /// The ordering guard used to hold for exactly one straggler: the first
    /// moved the high-water mark backwards, and the second walked straight
    /// past it.
    #[test]
    fn the_high_water_mark_survives_two_stragglers() {
        let mut session = Session::default();
        prompt(&mut session);
        stop(&mut session, 9000);
        feed(&mut session, "PreToolUse", tool_call("p1"), 8500);
        assert_eq!(session.outcome, "done");
        assert_eq!(session.event_ms, 9000);
        feed(&mut session, "PostToolUse", tool_call("p1"), 8700);
        assert_eq!(session.outcome, "done");
        assert_eq!(session.state, "idle");
    }

    /// A `PermissionRequest` that lands after the turn ended is asking nobody
    /// anything, and nothing in that turn will ever arrive to clear it.
    #[test]
    fn a_straggling_permission_request_raises_no_prompt() {
        let mut session = Session::default();
        prompt(&mut session);
        stop(&mut session, 9000);
        feed(
            &mut session,
            "PermissionRequest",
            json!({ "prompt_id": "p1", "tool_name": "Bash", "tool_input": { "command": "ls" } }),
            8900,
        );
        assert_eq!(session.pending_since, 0);
    }

    /// A subagent's own prompt, answered, is cleared by that subagent's tool
    /// running — and not by another subagent's.
    #[test]
    fn a_subagents_prompt_is_cleared_by_the_subagent_that_asked() {
        let mut session = Session::default();
        prompt(&mut session);
        feed(
            &mut session,
            "PermissionRequest",
            json!({ "prompt_id": "p1", "agent_id": "a1", "tool_name": "Bash", "tool_input": { "command": "ls" } }),
            2000,
        );
        assert_eq!(session.pending_since, 2000);
        assert_eq!(session.pending_agent, "a1");

        let mut other = tool_call("p1");
        other["agent_id"] = json!("a2");
        feed(&mut session, "PostToolUse", other, 2200);
        assert_eq!(
            session.pending_since, 2000,
            "another agent's work is not an answer"
        );

        let mut own = tool_call("p1");
        own["agent_id"] = json!("a1");
        feed(&mut session, "PostToolUse", own, 2500);
        assert_eq!(session.pending_since, 0);
        assert_eq!(session.pending_agent, "");
    }

    /// One `Bash` call may run for ten minutes. The session file has to say
    /// so, or the sweep reads the silence as death.
    #[test]
    fn a_tool_call_declares_how_long_it_may_be_silent() {
        let mut session = Session::default();
        prompt(&mut session);
        let mut long = json!({ "tool_name": "Bash", "prompt_id": "p1", "tool_input": { "command": "make", "timeout": 600_000 } });
        feed(&mut session, "PreToolUse", long.clone(), 2000);
        assert_eq!(session.tool_deadline_ms, 2000 + 600_000 + 60_000);

        // A shorter call in parallel does not shorten the budget.
        long["tool_input"]["timeout"] = json!(5_000);
        feed(&mut session, "PreToolUse", long, 2100);
        assert_eq!(session.tool_deadline_ms, 2000 + 600_000 + 60_000);

        // No timeout declared: the longest Claude Code allows.
        let mut fresh = Session::default();
        prompt(&mut fresh);
        feed(&mut fresh, "PreToolUse", tool_call("p1"), 3000);
        assert_eq!(fresh.tool_deadline_ms, 3000 + 600_000 + 60_000);
    }

    /// A subagent's tool calls arrive on the *parent's* session id, so without
    /// reading `agent_id` the card counted three agents' work as the turn's
    /// own and renamed itself after whatever they were doing — "Editing
    /// spiralplan.py" while the main agent sat waiting for them to report.
    #[test]
    fn a_subagents_work_is_not_the_turns_work() {
        let mut session = Session::default();
        feed(
            &mut session,
            "UserPromptSubmit",
            json!({ "prompt_id": "p1", "prompt": "go" }),
            1000,
        );

        feed(&mut session, "PreToolUse", tool_call("p1"), 1100);
        assert_eq!(session.turn_tools, 1);
        assert_eq!(session.kind, "Reading");

        let mut delegated = tool_call("p1");
        delegated["agent_id"] = json!("a12345");
        feed(&mut session, "PreToolUse", delegated, 1200);
        assert_eq!(
            session.turn_tools, 1,
            "a subagent's call is not the turn's action"
        );
        assert_eq!(
            session.kind, "Delegating",
            "and it is not what the turn is doing"
        );
        // Still counted somewhere: it is real work the session did.
        assert_eq!(session.tools, 2);
    }

    /// `UserPromptSubmit` is not the only way a turn begins. A stop hook can
    /// send one back to work and a resumed session simply carries on, and both
    /// used to inherit the previous turn's counters and its clock.
    #[test]
    fn a_new_turn_starts_wherever_claude_code_says_it_does() {
        let mut session = Session::default();
        feed(
            &mut session,
            "UserPromptSubmit",
            json!({ "prompt_id": "p1", "prompt": "go" }),
            1000,
        );
        feed(&mut session, "PreToolUse", tool_call("p1"), 1100);
        feed(&mut session, "PreToolUse", tool_call("p1"), 1200);
        assert_eq!(session.turn_tools, 2);

        // No prompt event at all, just work stamped with a different turn.
        feed(&mut session, "PreToolUse", tool_call("p2"), 5000);
        assert_eq!(session.turn_tools, 1);
        assert_eq!(session.turn_started_ms, 5000);
        assert_eq!(session.turn_ended_ms, 0);

        // And a straggler from the turn just ended does not start it again.
        // A prompt typed mid-turn is submitted then and answered later, so the
        // two turns' events interleave for a while.
        feed(&mut session, "PreToolUse", tool_call("p1"), 5100);
        assert_eq!(session.turn_started_ms, 5000, "the turn is still p2");
        assert_eq!(session.turn_tools, 2);
    }

    /// The card reads "N actions · 4m", and both belong to the turn. The clock
    /// used to run on after the turn ended, so a two-minute turn that finished
    /// two minutes ago said "4m" — and went on climbing for as long as anyone
    /// looked at it.
    #[test]
    fn the_turns_clock_stops_when_the_turn_does() {
        let mut session = Session::default();
        feed(
            &mut session,
            "UserPromptSubmit",
            json!({ "prompt_id": "p1", "prompt": "go" }),
            1000,
        );
        feed(
            &mut session,
            "Stop",
            json!({ "prompt_id": "p1", "last_assistant_message": "Done." }),
            131_000,
        );
        assert_eq!(session.outcome, "done");
        assert_eq!(session.turn_ended_ms, 131_000);
        // And starts again with the next turn.
        feed(&mut session, "PreToolUse", tool_call("p2"), 200_000);
        assert_eq!(session.turn_ended_ms, 0);
    }

    /// Hooks are installed `async: true`, so Claude Code neither waits for them
    /// nor orders them. A `PreToolUse` that started before the `Stop` and
    /// landed after it took the card back off "Done" with nothing running.
    #[test]
    fn a_straggler_does_not_un_finish_a_finished_turn() {
        let mut session = Session::default();
        feed(
            &mut session,
            "UserPromptSubmit",
            json!({ "prompt_id": "p1", "prompt": "go" }),
            1000,
        );
        feed(
            &mut session,
            "Stop",
            json!({ "prompt_id": "p1", "last_assistant_message": "Done." }),
            9000,
        );
        assert_eq!(session.outcome, "done");

        // Same turn, earlier timestamp: it left before the Stop did.
        feed(&mut session, "PreToolUse", tool_call("p1"), 8500);
        assert_eq!(session.outcome, "done", "the turn is still over");
        assert_eq!(session.state, "idle");
        // The tool call itself really happened, so it is still counted.
        assert_eq!(session.turn_tools, 1);
    }

    /// A subagent finishing while the main agent sits at a permission prompt is
    /// routine. It used to clear the prompt, so the card stopped saying "Needs
    /// you" while Claude Code was still asking — and the sweep, which reads a
    /// cleared prompt as silence, was then free to call the session dead.
    #[test]
    fn a_prompt_nobody_answered_survives_the_bookkeeping() {
        let mut session = Session::default();
        feed(
            &mut session,
            "UserPromptSubmit",
            json!({ "prompt_id": "p1", "prompt": "go" }),
            1000,
        );
        feed(
            &mut session,
            "PermissionRequest",
            json!({ "prompt_id": "p1", "tool_name": "Bash", "tool_input": { "command": "rm -rf build" } }),
            2000,
        );
        assert!(session.pending_since > 0);

        for event in ["SubagentStop", "PreCompact", "SessionStart"] {
            feed(&mut session, event, json!({ "prompt_id": "p1" }), 3000);
            assert!(
                session.pending_since > 0,
                "{event} must not answer a prompt on the user's behalf"
            );
        }
        // The tool actually running is what proves the prompt was answered.
        feed(
            &mut session,
            "PreToolUse",
            json!({ "prompt_id": "p1", "tool_name": "Bash", "tool_input": { "command": "rm -rf build" } }),
            4000,
        );
        assert_eq!(session.pending_since, 0);
    }

    /// Two sources added a subagent and one took it away, so three launches
    /// read as six and the count never came back to zero.
    #[test]
    fn a_subagent_is_counted_once_however_it_is_announced() {
        let mut session = Session::default();
        feed(
            &mut session,
            "UserPromptSubmit",
            json!({ "prompt_id": "p1", "prompt": "go" }),
            1000,
        );
        // The Task call, which cannot name an agent that does not exist yet.
        feed(
            &mut session,
            "PreToolUse",
            json!({ "prompt_id": "p1", "tool_name": "Task", "tool_input": { "description": "audit" } }),
            1100,
        );
        // And the subagent announcing itself, twice for good measure.
        for at in [1200, 1300] {
            feed(
                &mut session,
                "SubagentStart",
                json!({ "prompt_id": "p1", "agent_id": "a1", "agent_type": "general-purpose" }),
                at,
            );
        }
        assert_eq!(session.subagents, 1);
        feed(
            &mut session,
            "SubagentStop",
            json!({ "prompt_id": "p1", "agent_id": "a1" }),
            1400,
        );
        assert_eq!(session.subagents, 0);
    }

    /// The `Stop` payload carries Claude Code's own list of what is still
    /// running. It arrives at exactly the moment the card is choosing between
    /// "Done" and "Finishing", so it is the answer, and anything the overlay
    /// inferred from the transcript gives way to it.
    #[test]
    fn claude_codes_own_list_of_running_work_is_taken_as_the_answer() {
        let mut session = Session::default();
        feed(
            &mut session,
            "UserPromptSubmit",
            json!({ "prompt_id": "p1", "prompt": "go" }),
            1000,
        );
        feed(
            &mut session,
            "Stop",
            json!({
                "prompt_id": "p1",
                "last_assistant_message": "Handing off.",
                "background_tasks": [
                    { "id": "a111", "type": "subagent", "status": "running" },
                    { "id": "b222", "type": "shell", "status": "completed" }
                ]
            }),
            9000,
        );
        assert_eq!(session.tasks, vec!["a111"], "only what is actually running");
        assert_eq!(session.tasks_ms, 9000);
    }

    /// One tool failing is not the turn failing. Claude nearly always tries
    /// something else, and a red card for a failed grep is noise.
    #[test]
    fn a_failed_tool_marks_the_turn_without_failing_it() {
        let update = classify(
            "PostToolUseFailure",
            &json!({ "tool_name": "Grep", "error": "no matches" }),
        )
        .unwrap();
        assert!(update.hiccup);
        assert!(update.outcome.is_empty());
        assert_eq!(update.state, "running");
        // A tool that merely failed is not a tool that was refused. Counting
        // both under one number is what made the card's red chip meaningless.
        assert!(!update.blocked);
    }

    /// The only thing the red chip counts: the auto-mode classifier refusing a
    /// call. It is policy pushing back, which is worth a number, unlike a
    /// command that simply errored.
    #[test]
    fn an_auto_mode_refusal_is_counted_and_named() {
        let update = classify(
            "PermissionDenied",
            &json!({
                "tool_name": "Bash",
                "tool_input": { "command": "git push --force" }
            }),
        )
        .unwrap();
        assert!(update.blocked);
        assert!(update.activity.starts_with("Auto-mode blocked:"));
        // Still running: a refused call is not a failed turn, and the card
        // must not go red over one.
        assert_eq!(update.state, "running");
        assert!(update.outcome.is_empty());
    }

    #[test]
    fn tool_events_never_disturb_the_headline() {
        let payload = json!({
            "tool_name": "Read",
            "tool_input": { "file_path": "/code/clock.js" }
        });
        for event in [
            "PreToolUse",
            "PostToolUse",
            "PostToolUseFailure",
            "PermissionDenied",
        ] {
            let update = classify(event, &payload).unwrap();
            assert!(
                update.headline.is_none(),
                "{event} should leave the headline alone"
            );
        }
    }

    /// The opening line of a reply is often a greeting, and a card that says
    /// "Tristan," has spent its only line saying nothing.
    #[test]
    fn a_greeting_is_not_a_summary() {
        let update = classify(
            "Stop",
            &json!({ "last_assistant_message": "Tristan,\n\nFixed the timezone bug." }),
        )
        .unwrap();
        assert_eq!(update.headline.as_deref(), Some("Fixed the timezone bug."));
    }

    /// An answer with nothing quotable in it leaves the headline alone rather
    /// than replacing what the turn was about with "Finished".
    #[test]
    fn an_answer_with_nothing_to_say_keeps_the_question() {
        let update = classify("Stop", &json!({ "last_assistant_message": "Tristan," })).unwrap();
        assert_eq!(update.outcome, "done");
        assert!(update.headline.is_none());
    }

    /// A dragged-in file arrives as an absolute path, and the path is the
    /// least useful part of it.
    #[test]
    fn a_dropped_file_is_named_not_pathed() {
        let update = classify(
            "UserPromptSubmit",
            &json!({ "prompt": "@\"C:\\Users\\acer\\Downloads\\CLAUDE 1.md\"" }),
        )
        .unwrap();
        assert_eq!(update.headline.as_deref(), Some("CLAUDE 1.md"));

        let mentioned = classify(
            "UserPromptSubmit",
            &json!({ "prompt": "compare @src/derive.js with ./tools/pixel.mjs please" }),
        )
        .unwrap();
        assert_eq!(
            mentioned.headline.as_deref(),
            Some("compare derive.js with pixel.mjs please")
        );
    }

    /// The `SubagentStop` for work that outlived the answer used to clear the
    /// outcome, which turned a card that had just said "Done" back into
    /// "Delegating" with nothing running.
    #[test]
    fn a_late_subagent_does_not_unfinish_a_turn() {
        let with = |outcome: &str, subagents: u64| Session {
            outcome: outcome.to_string(),
            subagents,
            ..Session::default()
        };
        assert!(is_trailing_subagent("SubagentStop", "", &with("done", 1)));
        assert!(is_trailing_subagent(
            "SubagentStart",
            "",
            &with("failed", 0)
        ));
        assert!(!is_trailing_subagent("SubagentStop", "", &with("", 2)));
        assert!(!is_trailing_subagent("PreToolUse", "", &with("done", 0)));
        // A new prompt cleared the outcome, but nothing is outstanding: this
        // stop belongs to the previous turn and must stay bookkeeping.
        assert!(is_trailing_subagent("SubagentStop", "", &with("", 0)));
        // A genuine stop for a subagent this turn started is progress.
        assert!(!is_trailing_subagent("SubagentStop", "", &with("", 1)));
    }

    /// A blocking stop hook can veto this stop and have the turn carry on;
    /// the payload only admits that on the *second* Stop. The hold is what
    /// keeps the Done card from flashing and vanishing when that happens.
    #[test]
    fn even_a_clean_stop_is_held_provisional() {
        let update = classify("Stop", &json!({ "last_assistant_message": "Fixed." })).unwrap();
        assert_eq!(update.outcome, "done");
        assert!(
            update.settle_ms >= 1_000,
            "settle_ms was {}",
            update.settle_ms
        );
    }

    /// Auto-compaction fires `SessionStart` again mid-session with
    /// `source: "compact"`. That is a resumption, not a fresh session: the
    /// card must not reset to idle or lose its headline mid-turn.
    #[test]
    fn a_compaction_restart_is_not_a_new_session() {
        let update = classify("SessionStart", &json!({ "source": "compact" })).unwrap();
        assert_eq!(update.state, "thinking");
        assert!(update.headline.is_none());

        let fresh = classify("SessionStart", &json!({ "source": "startup" })).unwrap();
        assert_eq!(fresh.state, "idle");
        assert_eq!(fresh.headline.as_deref(), Some("Session started"));
    }

    /// `notification_type` is not in the documented payload schema. When it is
    /// missing, the message text still has to be able to say "needs you".
    #[test]
    fn a_notification_without_a_type_still_counts_as_waiting() {
        let payload = json!({ "message": "Claude needs your permission to use Bash" });
        let update = classify("Notification", &payload).unwrap();
        assert!(!update.waiting.is_empty());

        let idle = json!({ "message": "Claude is waiting for your input" });
        assert!(!classify("Notification", &idle).unwrap().waiting.is_empty());

        // A typeless notification that reads like neither stays ignored.
        assert!(classify("Notification", &json!({ "message": "Signed in." })).is_none());
    }

    #[test]
    fn the_prompt_becomes_the_headline() {
        let payload = json!({ "prompt": "Fix the flaky timezone test\n\nIt fails on CI only." });
        let update = classify("UserPromptSubmit", &payload).unwrap();
        assert_eq!(
            update.headline.as_deref(),
            Some("Fix the flaky timezone test")
        );
        assert_eq!(update.activity, "Thinking…");
    }

    #[test]
    fn uninteresting_notifications_are_ignored() {
        let payload = json!({ "notification_type": "auth_success" });
        assert!(classify("Notification", &payload).is_none());
        assert!(classify("MessageDisplay", &json!({})).is_none());
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let truncated = truncate("ααααα", 3);
        assert_eq!(truncated.chars().count(), 3);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn session_ids_cannot_escape_the_sessions_directory() {
        assert_eq!(crate::state::sanitize("../../etc/passwd"), "etcpasswd");
        assert_eq!(crate::state::sanitize(""), "unknown");
    }

    /// The bug this whole `background` flag exists for.
    ///
    /// A `claude --bg` leg ends a turn and Claude Code raises the same idle
    /// notification a chat gets. Nobody is being asked anything — the
    /// supervisor that started the leg resumes it — and
    /// `claude agents --json` said `busy`/`working` for this very session at
    /// the moment the card was saying "Needs you".
    #[test]
    fn a_background_turn_ending_is_not_a_person_being_asked() {
        let mut session = Session {
            background: true,
            ..Default::default()
        };
        feed(
            &mut session,
            "UserPromptSubmit",
            json!({ "prompt_id": "p1", "prompt": "work milestone M1" }),
            1000,
        );
        feed(
            &mut session,
            "Stop",
            json!({ "prompt_id": "p1", "last_assistant_message": "Handed off." }),
            9000,
        );
        feed(
            &mut session,
            "Notification",
            json!({ "notification_type": "idle_prompt", "prompt_id": "p1" }),
            9100,
        );
        assert_eq!(session.waiting_reason, "", "nobody is waiting on this");
        assert_eq!(session.waiting_since, 0, "so the debounce never starts");
        assert_eq!(session.activity, "Waiting to be resumed");
        assert_eq!(session.state, "idle");
        // The alert the pet does raise is counted somewhere else entirely, and
        // this must not have quietly invented one.
        assert_eq!(session.blocked, 0);
    }

    /// The regression the fix has to not cause: an ordinary chat.
    #[test]
    fn a_foreground_turn_ending_still_asks_for_you() {
        let mut session = Session::default();
        feed(
            &mut session,
            "UserPromptSubmit",
            json!({ "prompt_id": "p1", "prompt": "fix the test" }),
            1000,
        );
        feed(
            &mut session,
            "Stop",
            json!({ "prompt_id": "p1", "last_assistant_message": "Fixed it." }),
            9000,
        );
        feed(
            &mut session,
            "Notification",
            json!({ "notification_type": "idle_prompt", "prompt_id": "p1" }),
            9100,
        );
        assert_eq!(session.waiting_reason, "Waiting for your reply");
        assert_eq!(session.waiting_since, 9100);
    }

    /// The other direction, and the reason this is gated on *why* the session
    /// is waiting rather than on whether it is a background one. A background
    /// agent stopped on a permission prompt is stopped for good: no supervisor
    /// answers that, and it is exactly the case the alert exists for.
    #[test]
    fn a_background_session_at_a_permission_prompt_still_alerts() {
        let mut session = Session {
            background: true,
            ..Default::default()
        };
        feed(
            &mut session,
            "UserPromptSubmit",
            json!({ "prompt_id": "p1", "prompt": "deploy it" }),
            1000,
        );
        feed(
            &mut session,
            "PermissionRequest",
            json!({
                "prompt_id": "p1",
                "tool_name": "Bash",
                "tool_input": { "command": "rm -rf build" }
            }),
            5000,
        );
        feed(
            &mut session,
            "Notification",
            json!({
                "notification_type": "permission_prompt",
                "prompt_id": "p1",
                "message": "Claude needs your permission to run rm -rf build"
            }),
            5100,
        );
        assert_eq!(session.pending_tool, "Bash", "the prompt is on the record");
        assert_eq!(session.pending_since, 5000);
        assert!(
            session.waiting_reason.contains("permission"),
            "a permission prompt is never resumable: {}",
            session.waiting_reason
        );
        assert_eq!(session.waiting_since, 5100);
    }

    /// `notification_type` is absent from the published schema, so the message
    /// text is the fallback path — and it has to make the same distinction, or
    /// the fix has a hole in it the width of one missing field.
    #[test]
    fn the_message_fallback_knows_the_difference_too() {
        let idle = classify(
            "Notification",
            &json!({ "message": "Claude is waiting for your input" }),
        )
        .unwrap();
        assert!(idle.resumable, "a turn simply ending");

        let prompt = classify(
            "Notification",
            &json!({ "message": "Claude needs your permission to use Bash" }),
        )
        .unwrap();
        assert!(!prompt.resumable, "a decision, which only a person makes");
    }

    /// What a hook actually gets handed, both ways, as measured.
    ///
    /// The obvious field — `CLAUDE_CODE_SESSION_KIND=bg` — is on the *agent*
    /// process and does not reach its hooks, which is why reading it alone
    /// left every background session looking like a chat. `CLAUDE_JOB_DIR` is
    /// what does come through, on every event of a background session and on
    /// none of a foreground one.
    #[test]
    fn a_background_hook_is_recognised_by_what_it_is_actually_given() {
        let env = |pairs: Vec<(&'static str, &'static str)>| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| (*value).to_string())
            }
        };

        // Measured 2026-08-29, Claude Code 2.1.251, every event of a
        // `claude --bg` session: SessionStart, UserPromptSubmit, PreToolUse,
        // PostToolUse, Stop and Notification all carried this and nothing else
        // that named the session's kind.
        assert!(reads_as_background(env(vec![
            ("CLAUDE_JOB_DIR", "/home/tristan/.claude/jobs/959c74a7"),
            (
                "CLAUDE_CODE_SESSION_ID",
                "959c74a7-8f7c-49e4-a782-42645920b8d2"
            ),
        ])));

        // The same events in a foreground chat, which is the regression this
        // whole change has to not cause.
        assert!(!reads_as_background(env(vec![
            (
                "CLAUDE_CODE_SESSION_ID",
                "7a958c6e-dfb0-4e0c-9cd4-f308894c4615"
            ),
            ("CLAUDE_PID", "230772"),
        ])));

        // Kept for the day it is forwarded, and because it is the name
        // `claude agents --json` uses.
        assert!(reads_as_background(env(vec![(
            "CLAUDE_CODE_SESSION_KIND",
            "bg"
        )])));
        assert!(!reads_as_background(env(vec![(
            "CLAUDE_CODE_SESSION_KIND",
            "interactive"
        )])));

        // An empty variable is not a job directory. Shells export those freely.
        assert!(!reads_as_background(env(vec![("CLAUDE_JOB_DIR", "")])));
    }

    /// A background leg that broke rather than ended.
    ///
    /// Found live: a mission leg stopped dead by a model safeguard, halted
    /// until a person looks at it, and the idle notification that followed
    /// wrote "Waiting to be resumed" over the failure. Nothing was going to
    /// resume it. The suppression is for turns that ended, not turns that
    /// died.
    #[test]
    fn a_background_leg_that_failed_is_not_waiting_to_be_resumed() {
        let mut session = Session {
            background: true,
            ..Default::default()
        };
        feed(
            &mut session,
            "UserPromptSubmit",
            json!({ "prompt_id": "p1", "prompt": "work milestone M1" }),
            1000,
        );
        feed(
            &mut session,
            "StopFailure",
            json!({
                "prompt_id": "p1",
                "error_type": "api_error",
                "error": "API Error: this message was flagged"
            }),
            9000,
        );
        feed(
            &mut session,
            "Notification",
            json!({ "notification_type": "idle_prompt", "prompt_id": "p1" }),
            9100,
        );
        assert_eq!(session.outcome, "failed", "the failure still stands");
        assert_eq!(session.kind, "Failed");
        assert_eq!(session.headline, "Turn failed: api error");
        assert_ne!(
            session.activity, "Waiting to be resumed",
            "nothing is going to resume a halted leg"
        );
        assert!(
            session.detail.contains("flagged"),
            "the error body survives the notification: {}",
            session.detail
        );
        // Still not a person being asked a question, either.
        assert_eq!(session.waiting_reason, "");
    }

    /// FINDINGS B-3, and the foreground half of it.
    ///
    /// A `Notification` carries no long text, and used to write that emptiness
    /// over whatever the panel was holding. Landing after a `StopFailure` it
    /// wiped the error body, so the card said a turn had failed and could not
    /// say how — the one moment the detail is the whole point.
    #[test]
    fn a_notification_does_not_wipe_the_error_it_arrives_after() {
        let mut session = Session::default();
        feed(
            &mut session,
            "UserPromptSubmit",
            json!({ "prompt_id": "p1", "prompt": "run the migration" }),
            1000,
        );
        feed(
            &mut session,
            "StopFailure",
            json!({
                "prompt_id": "p1",
                "error_type": "api_error",
                "error": "API Error: the upstream call timed out"
            }),
            9000,
        );
        feed(
            &mut session,
            "Notification",
            json!({ "notification_type": "idle_prompt", "prompt_id": "p1" }),
            9100,
        );
        assert_eq!(session.kind, "Failed");
        assert!(
            session.detail.contains("timed out"),
            "the card must still be able to say how: {:?}",
            session.detail
        );
        // Foreground, so the person really is being asked. Unchanged.
        assert_eq!(session.waiting_reason, "Waiting for your reply");
    }

    /// The other half of making `detail` optional: `Some("")` still clears.
    ///
    /// Without it the rule would only ever accumulate, and a turn that ended
    /// with nothing worth reading would leave an earlier tool's error sitting
    /// under the word "Done" — the same lie the other way round.
    #[test]
    fn a_turn_ending_with_nothing_to_say_clears_the_panel() {
        let mut session = Session::default();
        feed(
            &mut session,
            "UserPromptSubmit",
            json!({ "prompt_id": "p1", "prompt": "tidy up" }),
            1000,
        );
        feed(
            &mut session,
            "PostToolUseFailure",
            json!({ "tool_name": "Grep", "prompt_id": "p1", "error": "no matches found" }),
            2000,
        );
        assert!(session.detail.contains("no matches"));
        feed(
            &mut session,
            "Stop",
            json!({ "prompt_id": "p1", "last_assistant_message": "" }),
            9000,
        );
        assert_eq!(session.outcome, "done");
        assert_eq!(
            session.detail, "",
            "a stale tool error must not sit under the word Done"
        );
    }

    /// A tool call says nothing long, so it leaves the panel alone. It used to
    /// blank it, which meant the prompt vanished from the expanded card the
    /// moment any work started.
    #[test]
    fn ordinary_work_leaves_the_panel_standing() {
        let mut session = Session::default();
        feed(
            &mut session,
            "UserPromptSubmit",
            json!({ "prompt_id": "p1", "prompt": "fix the timezone test" }),
            1000,
        );
        assert!(session.detail.contains("timezone"));
        feed(&mut session, "PreToolUse", tool_call("p1"), 2000);
        assert!(
            session.detail.contains("timezone"),
            "a tool call is not a reason to empty the panel: {:?}",
            session.detail
        );
    }
}
