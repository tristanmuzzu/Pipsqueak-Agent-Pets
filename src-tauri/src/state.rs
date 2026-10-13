//! Shared paths, on-disk types, and small filesystem helpers.
//!
//! The hook writer and the overlay never talk to each other directly: hooks
//! write one small JSON file per Claude Code session, the overlay polls the
//! directory. That keeps the hook a fire-and-forget process with no port to
//! collide with and no daemon to be running first.

use crate::process;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const RECENT_LIMIT: usize = 24;

/// A session nobody has touched in this long is gone, whatever it last claimed.
/// Generous on purpose: a real prompt left waiting overnight is still true.
const STALE_AFTER_MS: u64 = 12 * 60 * 60 * 1000;

/// Work that has produced no event in this long is not happening. Claude Code
/// fires a hook for every tool call, so silence this long means the turn died
/// without a `Stop`, and the card must stop claiming otherwise.
const WORKING_STALE_MS: u64 = 5 * 60 * 1000;

/// How far before the recorded boot a session's last write must fall before
/// the reboot is taken as its death. The boot time is read back from the
/// kernel and the file's stamp from the wall clock, and the two are not the
/// same clock: an NTP step after boot moves one and not the other.
const BOOT_MARGIN_MS: u64 = 60 * 1000;

/// How long a turn that yielded the floor with background work still running
/// may go without an event. Nothing fires while a background command runs,
/// and it reports back into the same turn when it finishes, so this silence
/// is the wait itself. Bounded, because a task the overlay never hears from
/// again should not hold a card open all night.
const BACKGROUND_STALE_MS: u64 = 60 * 60 * 1000;

pub fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let raw = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let raw = std::env::var_os("HOME");
    raw.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn root() -> PathBuf {
    home_dir().join(".pipsqueak")
}

pub fn sessions_dir() -> PathBuf {
    root().join("sessions")
}

pub fn pets_dir() -> PathBuf {
    root().join("pets")
}

pub fn codex_pets_dir() -> PathBuf {
    home_dir().join(".codex").join("pets")
}

pub fn config_path() -> PathBuf {
    root().join("config.json")
}

/// One-shot instruction for a running overlay, written by `pipsqueak control`.
pub fn command_path() -> PathBuf {
    root().join("command.json")
}

/// Written every time the global hotkey fires.
///
/// "The shortcut does nothing" is otherwise unanswerable: registering the
/// chord and receiving the keypress are separate things, and from outside the
/// process they look identical. This makes the second one observable.
pub fn hotkey_log_path() -> PathBuf {
    root().join("hotkey.json")
}

/// Touched by the running overlay so other invocations can tell it is alive.
pub fn heartbeat_path() -> PathBuf {
    root().join("running.json")
}

pub fn claude_settings_path() -> PathBuf {
    home_dir().join(".claude").join("settings.json")
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Session ids come from an external process, so never let one escape the
/// sessions directory.
pub fn sanitize(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(96)
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub ms: u64,
    pub state: String,
    pub text: String,
}

/// Durable states: what the session *is* doing.
///
/// Excludes "waiting", "done" and "failed". Those answer different questions
/// (is a human blocking this, and how did the last turn end), and storing them
/// here is what lets a card get stuck on an alert forever when the resolving
/// event never arrives.
pub const RUNNING_STATES: [&str; 3] = ["thinking", "running", "compacting"];

/// Keeps a transient, overlay-computed count out of the file hooks write.
fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    Claude,
    Codex,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Session {
    pub session_id: String,
    /// Missing in older hook files, which all belong to Claude Code.
    pub provider: Provider,
    /// idle | thinking | running | compacting. Only forward progress moves it.
    pub state: String,
    /// What this turn is *about*. Set when the turn starts and replaced when it
    /// ends, never by tool events, so it stays readable while tools churn.
    pub headline: String,
    /// The live line: "Editing render.js", "Running: npm test".
    pub activity: String,
    /// The *category* of the current work: Reading, Editing, Running… This
    /// changes far less often than `activity`, which is the point.
    pub kind: String,
    /// Longer text for the expanded panel (assistant summary, error body).
    pub detail: String,
    /// The last thing the session said out loud, in its own words. Read from
    /// the transcript by the overlay, never by a hook: a hook only ever runs
    /// between tool calls, which is exactly when nothing is being said.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub narration: String,
    /// Where Claude Code is writing this session's transcript. Straight from
    /// the hook payload, because guessing the path from the working directory
    /// is right until somebody's project has an em dash in its name.
    pub transcript: String,
    /// What the Claude Code desktop app calls this chat, and the id it opens it
    /// by. Filled in by the overlay from the app's own records, never by a
    /// hook, so both are absent for a session running anywhere else.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub chat_title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub chat_id: String,
    /// Display name of the owning project: the git repository, not the folder
    /// the session happens to be sitting in.
    pub project: String,
    /// Set only when the session runs somewhere other than the project root
    /// (a worktree, a subdirectory), for a secondary label.
    pub workspace: String,
    pub project_root: String,
    /// True when the project root is a temp directory: throwaway sessions
    /// from scripted runs, which should not compete with real work.
    pub scratch: bool,
    /// True when nobody is sitting in front of this session: a background
    /// agent (`claude --bg`), whose turns are ended and restarted by a
    /// supervisor rather than by a person.
    ///
    /// The distinction matters because Claude Code fires the same idle
    /// `Notification` at the end of every turn, foreground or not. Read as a
    /// human being asked a question it is the pet's worst lie: the card says
    /// "Needs you" about a session that is about to be resumed by a daemon,
    /// and the one alert that is supposed to mean "go and look" starts firing
    /// at nothing.
    pub background: bool,
    pub cwd: String,
    pub event: String,
    pub updated_ms: u64,
    pub started_ms: u64,
    /// Start of the current turn, for the "3m" on the status line.
    pub turn_started_ms: u64,
    /// Tool calls in the current turn.
    pub turn_tools: u64,
    /// The Codex tail started mid-turn; this count is a lower bound.
    pub turn_tools_partial: bool,
    pub tools: u64,
    pub recent: Vec<Entry>,

    /// How the last turn ended: "" | done | failed. Cleared the moment the
    /// next turn starts, so it can never outlive the thing it describes.
    pub outcome: String,
    pub outcome_ms: u64,
    /// A `done` is only real once the clock passes this. Claude Code sends
    /// `Stop` while background tasks are still finishing, so an immediate
    /// celebration is sometimes premature; holding it briefly lets the next
    /// event cancel it instead.
    pub settles_ms: u64,
    /// Non-zero while Claude Code is genuinely blocked on a human. Cleared by
    /// any forward progress.
    pub waiting_since: u64,
    pub waiting_reason: String,
    /// Rollout-backed async input survives live Working/Idle status and cannot
    /// be dismissed as a completed turn while its question is unanswered.
    pub codex_question_pending: bool,

    /// A permission prompt Claude Code raised and has not resolved.
    ///
    /// Recorded rather than believed. Auto-mode and permission hooks settle
    /// most of these in a couple of hundred milliseconds without anyone being
    /// asked, so this only becomes a visible "needs you" once it outlives the
    /// debounce in the frontend.
    pub pending_tool: String,
    pub pending_detail: String,
    pub pending_since: u64,
    /// Why the pending command looks irreversible. A hint for the card only.
    /// Nothing here approves, denies, or blocks anything.
    pub pending_risk: String,
    /// Which agent raised the prompt: empty for the session's own agent, the
    /// `agent_id` for a subagent's. Only that agent's next event, or any
    /// event of the main agent, may clear it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pending_agent: String,
    /// The latest moment a tool call now in flight may still be running
    /// without any further event having fired. The sweep does not read
    /// silence as death before it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tool_deadline_ms: u64,
    /// Tool calls the auto-mode classifier refused inside the current turn.
    ///
    /// Policy, not failure, and the one number here a person can act on: it
    /// says the permission rules are getting in this turn's way. Tool calls
    /// that merely errored are deliberately not counted — Claude retries
    /// something else and nobody needs a tally of that.
    pub blocked: u64,
    /// Subagents started but not yet finished, as the hooks saw them.
    /// Derived from [`Session::agents`]; kept as a number because that is what
    /// every reader of this file wants.
    pub subagents: u64,
    /// The ids of those subagents, which is what makes the count safe.
    ///
    /// Claude Code stamps `agent_id` on every event a subagent causes, so a
    /// subagent can be counted once no matter how many events announce it —
    /// the tally this replaces was fed by two sources and drained by one, and
    /// never came back to zero.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    /// Work the turn started and has not been told is over: background shell
    /// commands, monitors and subagents, counted from the transcript by the
    /// overlay rather than by a hook.
    ///
    /// A hook fires when the assistant yields the floor, which is not the same
    /// moment the work ends, and no hook ever fires for a background command
    /// finishing. Without this the card called a turn Done while five agents
    /// were still running and the assistant's last words were that it was
    /// waiting for them.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub outstanding: u64,
    /// What Claude Code itself last said was still running.
    ///
    /// The `Stop` family of hook payloads carries a `background_tasks` array —
    /// id, type, status and description for every asynchronous thing the
    /// session has in flight. That is the answer, from the only process that
    /// actually knows it, arriving at exactly the moment the card is deciding
    /// between "Done" and "Finishing". Reading the transcript is still how the
    /// count moves *between* those moments; this is what it gets corrected
    /// against, so a shape this build has never seen cannot strand a card on
    /// "still working" for more than one turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<String>,
    /// When [`Session::tasks`] was last replaced, so the overlay applies each
    /// correction once rather than on every poll.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tasks_ms: u64,
    /// The turn this file is describing, straight from the hook payload.
    ///
    /// Claude Code stamps every hook event with the prompt it belongs to. That
    /// is a far better turn boundary than "a `UserPromptSubmit` arrived":
    /// turns also begin when a stop hook sends one back to work and when a
    /// session is resumed, and a prompt queued mid-turn fires
    /// `UserPromptSubmit` without ending the turn that is running.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt_id: String,
    /// The turn before it, so a turn is never rotated backwards.
    ///
    /// A prompt typed while a turn is running is submitted then and finishes
    /// later, so events from both turns interleave. Without this the counters
    /// reset every time the two took it in turns to arrive.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prev_prompt_id: String,
    /// How long the current turn has been going, frozen once it ends.
    ///
    /// The card used to compute this live from `turn_started_ms`, which is
    /// only ever cleared by the next prompt — so a finished card counted
    /// upwards for as long as it was on screen and read "4m" over a turn that
    /// took two minutes and had been over for another two.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub turn_ended_ms: u64,
    /// The sweep gave up on this session: running, but silent for longer than
    /// any real turn goes without firing a hook.
    ///
    /// A flag rather than a message written into the headline. The headline
    /// only changes at turn boundaries, so a sentence dropped into it by the
    /// sweep outlives the situation it describes and sits there contradicting
    /// the status line once work resumes.
    pub stalled: bool,

    /// The agent process that owns this session, as `(pid, creation time)`.
    /// The creation time is what makes it safe: pids get reused, and a reused
    /// pid would otherwise report a dead session as alive.
    pub agent_pid: u32,
    pub agent_created: u64,
    /// Timestamp of the event this file describes. Claude Code runs matching
    /// hooks in parallel, so two writers can race; the older one must lose.
    pub event_ms: u64,
}

impl Session {
    pub fn push_recent(&mut self, state: &str, text: &str, now: u64) {
        if let Some(last) = self.recent.last() {
            if last.state == state && last.text == text {
                return;
            }
        }
        self.recent.push(Entry {
            ms: now,
            state: state.to_string(),
            text: text.to_string(),
        });
        let overflow = self.recent.len().saturating_sub(RECENT_LIMIT);
        if overflow > 0 {
            self.recent.drain(0..overflow);
        }
    }

    pub fn is_running(&self) -> bool {
        RUNNING_STATES.contains(&self.state.as_str())
    }

    /// Forward progress: whatever the session was blocked on or had finished
    /// is no longer true.
    pub fn clear_pending(&mut self) {
        self.outcome.clear();
        self.outcome_ms = 0;
        self.settles_ms = 0;
        self.waiting_since = 0;
        self.waiting_reason.clear();
    }

    pub fn clear_permission(&mut self) {
        self.pending_tool.clear();
        self.pending_detail.clear();
        self.pending_risk.clear();
        self.pending_agent.clear();
        self.pending_since = 0;
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// Which build's idea of this file it is. See [`CONFIG_VERSION`].
    pub version: u32,
    /// False until the first run has been acknowledged. *Not* backfilled for
    /// existing installs, because an upgrade should get the welcome
    /// once too, since it is the only place the setup is explained.
    pub welcomed: bool,
    /// Whether the once-only autostart decision has been made.
    ///
    /// An overlay whose entire job is to be in the corner of your eye is
    /// worthless if it is not running, and "it did not come back after a
    /// reboot" is indistinguishable from "it is broken". So the first run
    /// registers it to start with Windows and says so. Recorded here rather
    /// than inferred from the registry, so turning it *off* stays off instead
    /// of being helpfully restored on the next launch.
    pub autostart_initialised: bool,
    pub pet: String,
    pub scale: f64,
    pub click_through: bool,
    pub show_bubble: bool,
    /// Sessions rooted in a temp directory (scripted runs, evals) are hidden
    /// unless asked for. They are not projects and drown out real work.
    pub show_scratch: bool,
    /// All agents, or just `claude` / `codex`. Presentation only.
    pub agent_filter: String,
    pub hidden_projects: Vec<String>,
    /// One preferred conversation; provider-qualified session ID.
    pub pinned_session: String,
    /// Beep when a project starts waiting on you.
    pub alert_on_waiting: bool,
    /// Blink the tray icon when a project finishes. The only channel that
    /// reaches you when the overlay is behind a fullscreen window.
    pub flash_on_finish: bool,
    /// Do not disturb: no sound, no tray flash. The pet still updates.
    pub quiet: bool,
    /// How much of itself a session narrates on its card: `off`, `speech`
    /// (only what Claude said to you) or `thoughts` (and the line it was
    /// thinking, which is most of what makes the card feel live).
    pub narrate: String,
    /// The chord that shows and hides the pet, e.g. "Ctrl+Alt+P". "off"
    /// disables it. If the chord is already claimed by something else,
    /// Pipsqueak falls back to the next one it can get and says which.
    pub hotkey: String,
    /// Ask GitHub whether there is a newer release.
    ///
    /// Off until asked for. This is the only thing in the app that talks to
    /// the network at all, and a status overlay reaching out on its own
    /// without being asked is a surprise nobody signed up for.
    pub update_check: bool,
    /// A version the user has already been told about and did not want.
    pub update_dismissed: String,
    pub x: Option<i32>,
    pub y: Option<i32>,
}

impl Config {
    /// Pulls any out-of-range value back to something usable.
    ///
    /// Per field rather than all-or-nothing: one bad number in a hand-edited
    /// file should cost that one setting, not every other one alongside it.
    fn clamp(&mut self) {
        if !self.scale.is_finite() || self.scale <= 0.0 {
            self.scale = 2.0;
        }
        self.scale = self.scale.clamp(1.0, 6.0);
        if self.pet.trim().is_empty() {
            self.pet = "byte".to_string();
        }
        if !["all", "claude", "codex"].contains(&self.agent_filter.as_str()) {
            self.agent_filter = "all".into();
        }
        self.hidden_projects
            .retain(|key| !key.is_empty() && key.len() <= 4096);
        self.hidden_projects.sort();
        self.hidden_projects.dedup();
        self.hidden_projects.truncate(256);
        if self.pinned_session.len() > 128 {
            self.pinned_session.clear();
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            welcomed: false,
            autostart_initialised: false,
            pet: "byte".to_string(),
            scale: 2.0,
            click_through: false,
            show_bubble: true,
            show_scratch: false,
            agent_filter: "all".into(),
            hidden_projects: Vec::new(),
            pinned_session: String::new(),
            alert_on_waiting: false,
            flash_on_finish: true,
            quiet: false,
            narrate: "thoughts".to_string(),
            hotkey: "Ctrl+Alt+P".to_string(),
            update_check: false,
            update_dismissed: String::new(),
            x: None,
            y: None,
        }
    }
}

/// The shape this build understands. Bumped when a field changes meaning,
/// not when one is merely added, which `serde(default)` already handles.
///
/// 2: the window grew to fit the setup panel, which moved the pet (drawn at
///    the bottom of it) down by the difference.
pub const CONFIG_VERSION: u32 = 2;

/// Reads the config, surviving anything that might have happened to the file.
///
/// It is a plain JSON file in the user's home directory, which means it gets
/// hand-edited, half-written by a crash, and occasionally written by a newer
/// build than this one. None of those may lose the whole config or, worse,
/// silently overwrite a newer file with older defaults.
pub fn load_config() -> Config {
    let path = config_path();
    let Ok(raw) = fs::read_to_string(&path) else {
        // No file yet: a first run, not a problem.
        return Config::default();
    };
    match serde_json::from_str::<Config>(&raw) {
        Ok(mut config) => {
            migrate(&mut config);
            config.clamp();
            config
        }
        Err(_) => {
            // Keep whatever they had. Overwriting an unparseable config with
            // defaults destroys the only copy of a setting they may have been
            // hand-editing when something went wrong.
            let _ = fs::rename(&path, path.with_extension("json.bak"));
            Config::default()
        }
    }
}

/// Brings a config written by an older build up to date.
///
/// Each step moves one version forward and nothing else, so a config that has
/// been sitting on disk through several releases arrives correct rather than
/// having the newest step applied to the oldest shape.
fn migrate(config: &mut Config) {
    if config.version < 2 {
        // Version 2 grew the window, which pushed the pet below the bottom of
        // the screen for anyone with a saved position. Correcting the
        // coordinate here was the obvious fix and the wrong one: the saved
        // value is in physical pixels and the window's size is declared in
        // logical ones, so on a scaled display the correction was short by the
        // scale factor and the pet stayed off screen.
        //
        // `app::clamp_to_display` handles it instead, and handles every other
        // way a saved position can stop fitting: a resolution change, a
        // scaling change, a taskbar that moved. Nothing to do here but record
        // that we looked.
        config.version = 2;
    }
}

pub fn save_config(config: &Config) -> std::io::Result<()> {
    if config.version > CONFIG_VERSION {
        // Written by a newer build. Rolling it back to fields this one happens
        // to know about would quietly delete their settings.
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "config.json was written by a newer version of Pipsqueak",
        ));
    }
    let mut stored = config.clone();
    stored.clamp();
    stored.version = CONFIG_VERSION;
    let bytes = serde_json::to_vec_pretty(&stored).unwrap_or_else(|_| b"{}".to_vec());
    write_atomic(&config_path(), &bytes)
}

/// Removes `*.tmp` files a hard stop left behind.
///
/// `write_atomic` writes a temp file and renames it; a machine that loses
/// power between the two leaves an empty `running.json.<pid>.tmp` that
/// nothing else ever looks at. Two of them were the tell that a phantom card
/// had come from an unclean stop, which is worth knowing once and not worth
/// keeping. Anything older than a minute is nobody's in-flight write.
pub fn remove_stale_temp_files() -> usize {
    let mut removed = 0;
    for dir in [root(), sessions_dir()] {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("tmp") {
                continue;
            }
            let old = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| at.elapsed().ok())
                .map(|age| age.as_secs() >= 60)
                .unwrap_or(false);
            if old && fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// Write via temp file + rename so a reader never sees a half-written file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_file_name(format!(
        "{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("out"),
        std::process::id()
    ));
    fs::write(&tmp, bytes)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            Err(err)
        }
    }
}

/// Held for the read-modify-write of one session file.
///
/// Claude Code runs every matching hook for an event in parallel, so two
/// `pipsqueak hook` processes routinely touch the same file at the same
/// moment. Without this, both read the same "before", and whichever renames
/// last silently discards the other's event.
pub struct FileLock {
    path: PathBuf,
    held: bool,
}

impl FileLock {
    /// Waits up to ~200ms, then proceeds anyway.
    ///
    /// Losing an event is worse than an interleaved write: hooks are the only
    /// source of truth here, and a hook that gives up writes nothing at all.
    pub fn acquire(target: &Path) -> Self {
        Self::acquire_within(target, 200)
    }

    /// The same lock, with a caller-chosen patience.
    pub fn acquire_within(target: &Path, wait_ms: u64) -> Self {
        let path = target.with_extension("json.lock");
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        for attempt in 0..(wait_ms / 5).max(1) {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Self { path, held: true },
                Err(_) => {
                    // A hook killed mid-write would otherwise block every later
                    // event for this session forever.
                    if attempt == 0 {
                        if let Ok(meta) = fs::metadata(&path) {
                            // Only steal a lock that is *provably* old. An
                            // unreadable age (clock skew, metadata failure)
                            // used to count as expired, which deleted a live
                            // holder's lock and let two writers interleave.
                            let expired = meta
                                .modified()
                                .ok()
                                .and_then(|t| t.elapsed().ok())
                                .map(|age| age.as_secs() >= 2)
                                .unwrap_or(false);
                            if expired {
                                let _ = fs::remove_file(&path);
                                continue;
                            }
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
        Self { path, held: false }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if self.held {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn read_sessions() -> Vec<Session> {
    let mut out: Vec<Session> = Vec::new();
    let Ok(entries) = fs::read_dir(sessions_dir()) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // One transient read failure (an AV scanner holding the file for a
        // moment) must not drop the session from this emit: the frontend
        // tears a missing card all the way down and rebuilds it on the next
        // tick, which reads as a flash.
        let raw = fs::read_to_string(&path).or_else(|_| {
            std::thread::sleep(std::time::Duration::from_millis(5));
            fs::read_to_string(&path)
        });
        if let Some(session) = raw
            .ok()
            .and_then(|raw| serde_json::from_str::<Session>(&raw).ok())
        {
            out.push(session);
        }
    }
    // Anything needing a human wins, then most recently active.
    out.sort_by(|a, b| {
        attention_rank(b)
            .cmp(&attention_rank(a))
            .then(b.updated_ms.cmp(&a.updated_ms))
    });
    out
}

fn attention_rank(session: &Session) -> u8 {
    if session.waiting_since > 0 {
        3
    } else if session.outcome == "failed" {
        2
    } else {
        0
    }
}

/// Retires sessions that are no longer telling the truth.
///
/// Four rules, in order of how confident each is:
///
/// 1. The agent process is gone. Certain, so delete the session outright.
/// 2. The file was last written before this machine booted. Whatever process
///    wrote it did not survive the reboot, whether or not the hook ever
///    learned its pid. Certain, so delete. This is the rule that catches a
///    session recorded by a build that knew no process identity on this
///    platform, and it is why a "Needs you" card no longer outlives the
///    machine that asked.
/// 3. Nothing has happened for [`WORKING_STALE_MS`] while the card claims work
///    is in progress. Claude Code fires a hook per tool call, so that silence
///    means the turn died without a `Stop`. Downgrade rather than delete, since
///    the session may still be resumed. A session genuinely *waiting* on a
///    human is left alone: that claim stays true no matter how long it takes.
/// 4. Nothing at all for [`STALE_AFTER_MS`]. Delete.
///
/// Every retirement is logged with the rule that made it, because the one
/// question a stale card raises — "why is this still here?" — is otherwise
/// unanswerable after the fact.
///
/// Returns the number of files it changed, so callers can skip a redraw.
pub fn sweep() -> usize {
    let now = now_ms();
    let Ok(entries) = fs::read_dir(sessions_dir()) else {
        return 0;
    };
    let boot_cutoff = process::boot_time_ms().map(|boot| boot.saturating_sub(BOOT_MARGIN_MS));
    let retire = |path: &Path, session: &Session, why: &str| {
        let _ = fs::remove_file(path);
        crate::log::write(&format!(
            "retired session {} ({}, {}, last event {}s ago): {why}",
            session.session_id.chars().take(8).collect::<String>(),
            session.project,
            session.kind,
            now.saturating_sub(session.updated_ms) / 1000,
        ));
    };
    let mut changed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // The same lock the hooks hold for their read-modify-write. Without
        // it, the sweep could read a snapshot, lose the race to a fresh
        // event, and then write its stale verdict (or a deletion) over the
        // new turn — a card flashing "Stopped responding" seconds after work
        // began.
        let _lock = FileLock::acquire(&path);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            // Could not read: an AV scanner or indexer briefly holding the
            // file. Deleting on that verdict removed live sessions, and the
            // next hook event recreated them as brand-new cards. Leave it for
            // the next sweep to look at again.
            Err(_) => continue,
        };
        let Some(mut session) = serde_json::from_str::<Session>(&raw).ok() else {
            // Read fine but does not parse: genuinely corrupt or from an
            // incompatible build. It can only mislead.
            let _ = fs::remove_file(&path);
            changed += 1;
            continue;
        };

        let age = now.saturating_sub(session.updated_ms);

        if session.agent_pid != 0 && !process::is_alive(session.agent_pid, session.agent_created) {
            retire(
                &path,
                &session,
                &format!("agent process {} is gone", session.agent_pid),
            );
            changed += 1;
            continue;
        }
        if boot_cutoff.is_some_and(|cutoff| session.updated_ms < cutoff) {
            retire(&path, &session, "written before this boot");
            changed += 1;
            continue;
        }
        // A prompt the agent is still sitting at is exempt from age, as the
        // README promises — but only once the agent is known to be there.
        // An unknown process (no pid recorded) waiting for twelve hours is
        // the phantom card this sweep exists to remove.
        let known_to_be_waiting = session.waiting_since > 0 && session.agent_pid != 0;
        if age > STALE_AFTER_MS && !known_to_be_waiting {
            retire(&path, &session, "no event for twelve hours");
            changed += 1;
            continue;
        }
        if has_stopped_responding(&session, age, now) {
            session.state = "idle".to_string();
            session.kind = "Idle".to_string();
            session.activity.clear();
            session.subagents = 0;
            session.agents.clear();
            // The turn is not going to end now, so its clock has to be stopped
            // by something. Left running, a "Stopped responding" card counted
            // upwards for as long as it was on screen.
            if session.turn_ended_ms == 0 {
                session.turn_ended_ms = session.updated_ms;
            }
            // Not a completion. Saying "Done" here would be the same lie in a
            // friendlier voice. The headline is left alone: it still describes
            // what the turn was about, which is true whatever happened next.
            session.stalled = true;
            if let Ok(bytes) = serde_json::to_vec(&session) {
                let _ = write_atomic(&path, &bytes);
                changed += 1;
            }
        }
    }
    changed
}

/// Clears a permission prompt the human has already answered.
///
/// No hook reports the answer, so a "yes" to a long command left the card
/// saying "Needs you" for as long as the command ran: measured 2026-09-18, a
/// store upload approved after half a minute still showed the question four
/// minutes later. The shell the answer starts is the evidence instead; see
/// `process::shell_started_since`. Only a Bash prompt has that tell, and only
/// once the agent's pid is known.
pub fn settle_answered_prompts() -> usize {
    let now = now_ms();
    let Ok(entries) = fs::read_dir(sessions_dir()) else {
        return 0;
    };
    let mut changed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // Nearly every session, nearly all the time, is not at a prompt, so
        // look before taking the lock the hooks contend for.
        let Some(session) = read_session(&path) else {
            continue;
        };
        if !at_shell_prompt(&session) {
            continue;
        }
        let _lock = FileLock::acquire(&path);
        let Some(mut session) = read_session(&path) else {
            continue;
        };
        if !at_shell_prompt(&session)
            || process::shell_started_since(session.agent_pid, session.pending_since) != Some(true)
        {
            continue;
        }
        mark_answered(&mut session, now);
        if let Ok(bytes) = serde_json::to_vec(&session) {
            let _ = write_atomic(&path, &bytes);
            changed += 1;
        }
    }
    changed
}

fn read_session(path: &Path) -> Option<Session> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn at_shell_prompt(session: &Session) -> bool {
    session.pending_since > 0 && session.pending_tool == "Bash" && session.agent_pid != 0
}

/// The prompt was answered and the command is running: say so, the way the
/// hooks would have if one fired at the answer.
fn mark_answered(session: &mut Session, now: u64) {
    let what = session.pending_detail.clone();
    session.clear_permission();
    session.waiting_since = 0;
    session.waiting_reason.clear();
    session.event = "Approved".to_string();
    session.updated_ms = now;
    let text = if what.is_empty() {
        "Approved".to_string()
    } else {
        format!("Approved · {what}")
    };
    session.push_recent("running", &text, now);
}

/// Has this session died without saying so?
///
/// Silence is the only evidence available, and it means different things. A
/// turn that reported how it ended is not stuck. Neither is one blocked on a
/// human: being asked a question produces no further events *by definition*,
/// so a real permission prompt looks exactly like death to this rule. It used
/// to be read as death, and five minutes after walking away from a prompt the
/// card said "Stopped responding" while still showing the question — the pet
/// contradicting itself in the one situation it exists for.
///
/// A session whose agent process is genuinely gone is retired before this is
/// ever asked, which is the honest test.
fn has_stopped_responding(session: &Session, age: u64, now: u64) -> bool {
    if age <= WORKING_STALE_MS || !session.is_running() {
        return false;
    }
    if !session.outcome.is_empty() {
        return false;
    }
    if session.waiting_since > 0 || session.pending_since > 0 {
        return false;
    }
    // A tool call that declared a long budget is allowed its silence.
    if session.tool_deadline_ms > now {
        return false;
    }
    // So is a turn that yielded the floor with background work outstanding:
    // the next event is that work reporting back, and it may be a while.
    if session.event == "Stop" && !session.tasks.is_empty() && age <= BACKGROUND_STALE_MS {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet(state: &str) -> Session {
        Session {
            state: state.to_string(),
            ..Default::default()
        }
    }

    /// The README promise: "a session that is genuinely waiting on you is
    /// never retired by age. That claim stays true no matter how long you
    /// take." It was not true — a prompt left unanswered for five minutes was
    /// relabelled "Stopped responding" with the question still on the card.
    #[test]
    fn a_session_waiting_on_a_human_never_stops_responding() {
        let long = WORKING_STALE_MS * 2;

        let mut asked = quiet("running");
        asked.waiting_since = 1;
        assert!(!has_stopped_responding(&asked, long, NOW));

        let mut pending = quiet("running");
        pending.pending_since = 1;
        assert!(!has_stopped_responding(&pending, long, NOW));

        // Silence with nobody being asked anything is the case this is for.
        assert!(has_stopped_responding(&quiet("running"), long, NOW));
        // And it takes more than a pause between tool calls.
        assert!(!has_stopped_responding(&quiet("running"), 1000, NOW));
    }

    /// A "yes" to a Bash prompt fires no hook. Once the shell it starts is
    /// seen, the card has to stop asking. Only a shell can be seen starting,
    /// so an Edit prompt has no such tell and keeps waiting for its hook.
    #[test]
    fn an_answered_shell_prompt_is_cleared_without_a_hook() {
        let mut asked = quiet("running");
        asked.pending_tool = "Bash".into();
        asked.pending_detail = "run: cargo build".into();
        asked.pending_since = 5;
        asked.waiting_since = 9;
        asked.waiting_reason = "Claude needs your permission to use Bash".into();
        asked.agent_pid = 1;
        assert!(at_shell_prompt(&asked));

        mark_answered(&mut asked, NOW);
        assert_eq!(asked.pending_since, 0);
        assert_eq!(asked.waiting_since, 0);
        assert!(asked.waiting_reason.is_empty());
        assert!(asked.pending_detail.is_empty());
        assert_eq!(asked.updated_ms, NOW);
        assert_eq!(
            asked.recent.last().map(|entry| entry.text.as_str()),
            Some("Approved · run: cargo build")
        );
        assert!(!at_shell_prompt(&asked));

        let mut edit = quiet("running");
        edit.pending_tool = "Edit".into();
        edit.pending_since = 5;
        edit.agent_pid = 1;
        assert!(!at_shell_prompt(&edit));

        let mut unknown = quiet("running");
        unknown.pending_tool = "Bash".into();
        unknown.pending_since = 5;
        assert!(!at_shell_prompt(&unknown), "no pid, nothing to look at");
    }

    const NOW: u64 = 1_000_000_000;

    /// A seven-minute build is one tool call and one long silence. The call
    /// said how long it might take; the sweep has to believe it.
    #[test]
    fn a_tool_call_inside_its_budget_is_not_dead() {
        let long = WORKING_STALE_MS * 2;
        let mut building = quiet("running");
        building.tool_deadline_ms = NOW + 1;
        assert!(!has_stopped_responding(&building, long, NOW));
        building.tool_deadline_ms = NOW - 1;
        assert!(has_stopped_responding(&building, long, NOW));
    }

    /// The floor was yielded while background work ran. The silence is the
    /// wait, for a while.
    #[test]
    fn waiting_on_background_work_is_not_dead_for_an_hour() {
        let mut parked = quiet("running");
        parked.event = "Stop".into();
        parked.tasks = vec!["b1".into()];
        assert!(!has_stopped_responding(&parked, WORKING_STALE_MS * 2, NOW));
        assert!(has_stopped_responding(
            &parked,
            BACKGROUND_STALE_MS + 1,
            NOW
        ));
        // Without anything outstanding, silence after a Stop that kept the
        // state running is the usual rule.
        parked.tasks.clear();
        assert!(has_stopped_responding(&parked, WORKING_STALE_MS * 2, NOW));
    }

    /// A turn that said how it ended is not stuck, however long ago that was.
    #[test]
    fn a_finished_turn_is_not_a_dead_one() {
        let mut done = quiet("running");
        done.outcome = "done".into();
        assert!(!has_stopped_responding(&done, WORKING_STALE_MS * 100, NOW));
        assert!(!has_stopped_responding(
            &quiet("idle"),
            WORKING_STALE_MS * 100,
            NOW
        ));
    }

    /// Migration leaves the saved position alone on purpose. Correcting it
    /// here needs the display scale, which this layer does not have, and
    /// getting it wrong is what put the pet below the bottom of the screen.
    /// Placement clamps it to the work area instead.
    #[test]
    fn migration_does_not_touch_the_saved_position() {
        let mut config = Config {
            version: 1,
            x: Some(1326),
            y: Some(649),
            ..Config::default()
        };
        migrate(&mut config);
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!((config.x, config.y), (Some(1326), Some(649)));
    }

    #[test]
    fn a_hand_edited_value_costs_only_itself() {
        let mut config = Config {
            scale: -12.0,
            pet: "  ".into(),
            show_scratch: true,
            ..Config::default()
        };
        config.clamp();
        assert_eq!(config.scale, 2.0);
        assert_eq!(config.pet, "byte");
        assert!(config.show_scratch, "an unrelated setting must survive");
    }

    #[test]
    fn a_newer_config_is_never_written_over() {
        let config = Config {
            version: CONFIG_VERSION + 1,
            ..Config::default()
        };
        assert!(save_config(&config).is_err());
    }
    #[test]
    fn tracking_preferences_round_trip_and_old_configs_default_to_all() {
        let mut config: Config = serde_json::from_str(r#"{"pet":"byte","agent_filter":"codex","hidden_projects":["path:/a","path:/a"],"pinned_session":"codex:123"}"#).unwrap();
        config.clamp();
        let again: Config = serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(again.agent_filter, "codex");
        assert_eq!(again.hidden_projects, vec!["path:/a"]);
        assert_eq!(again.pinned_session, "codex:123");
        let old: Config = serde_json::from_str(r#"{"pet":"byte"}"#).unwrap();
        assert_eq!(old.agent_filter, "all");
        assert!(old.hidden_projects.is_empty());
        assert!(old.pinned_session.is_empty());
    }
}
