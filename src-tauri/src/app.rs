//! The overlay: a transparent, always-on-top window that polls session state
//! and animates the pet.

use crate::attention::{self, Attention};
use crate::chats;
use crate::desktop;
use crate::doctor;
use crate::hotkey::{self, Binding};
use crate::install;
use crate::log;
use crate::narration;
use crate::state::{
    self, codex_pets_dir, load_config, pets_dir, read_sessions, root, sessions_dir, Config, Session,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, State, WindowEvent};

const POLL_INTERVAL: Duration = Duration::from_millis(300);
/// Hit-test rates, by how close the cursor is to something interactive. Only
/// the first one has to be quick, and only while the cursor is right there.
/// Polling this fast all the time would keep a core busy for an overlay you
/// are not even pointing at.
///
/// All of this is compiled out on Linux, where the cursor is not knowable and
/// `spawn_hit_test` does nothing (see there).
#[cfg(not(target_os = "linux"))]
const HIT_TEST_NEAR: Duration = Duration::from_millis(12);
#[cfg(not(target_os = "linux"))]
const HIT_TEST_APPROACHING: Duration = Duration::from_millis(50);
#[cfg(not(target_os = "linux"))]
const HIT_TEST_FAR: Duration = Duration::from_millis(220);
/// Roughly a card's own height: far enough out that a fast flick is caught
/// before it lands.
#[cfg(not(target_os = "linux"))]
const NEAR_PX: f64 = 90.0;
#[cfg(not(target_os = "linux"))]
const APPROACHING_PX: f64 = 320.0;
/// How far outside the window a cursor is still worth telling the pet about,
/// and how often to say so. A pet that can look at things wants this often
/// enough to track a moving cursor and rarely enough to be free.
#[cfg(not(target_os = "linux"))]
const LOOK_RADIUS_PX: f64 = 420.0;
#[cfg(not(target_os = "linux"))]
const LOOK_INTERVAL: Duration = Duration::from_millis(60);
/// Often enough that a crashed session disappears while you are still looking
/// at the card, rare enough that it costs nothing.
const SWEEP_INTERVAL: Duration = Duration::from_secs(10);
/// The heartbeat other invocations read to tell an overlay is up. Its
/// readers allow six seconds of silence, so writing it on every 300ms poll
/// was an atomic file write — create, write, rename — three times a second,
/// around the clock, for a three-times-a-second answer nobody asked for. This
/// keeps a three-fold margin under the readers' timeout.
const BEAT_INTERVAL: Duration = Duration::from_secs(2);
/// How often to look for a permission prompt that has been answered. A "yes"
/// fires no hook; the card should stop asking within the second, while you
/// are still looking at it.
const PROMPT_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// A `pipsqueak control` command older than this was meant for a pet that is
/// no longer the one reading it.
const COMMAND_STALE_MS: u64 = 10_000;
/// How long the frontend may go quiet before we assume the page is dead.
///
/// It reports its clickable regions at least once a second. Generous anyway,
/// because Windows throttles timers hard for an occluded window and a needless
/// reload is still a visible flicker.
const FRONTEND_SILENT_MS: u64 = 45_000;
/// How long a silent page gets to answer a ping before it is declared dead.
///
/// Timers are throttled hard for an occluded window — down to once a minute —
/// so silence alone only proves the page is not *rendering*. Event delivery is
/// not throttled: a live page answers the ping immediately, and one that does
/// not is actually gone. Checked on the sweep cadence, so the effective grace
/// is one sweep interval.
const PING_GRACE_MS: u64 = 3_000;
/// A rescue that fails leaves the page just as dead, so retrying in a tight
/// loop only fills the log.
const RESCUE_INTERVAL_MS: u64 = 60_000;
/// The floor between two corrections of an unwanted resize. Fast enough that a
/// scale change is repaired within a frame or two, slow enough that arguing
/// with a compositor that wants a different size costs nothing measurable.
const RESIZE_FIX_INTERVAL: Duration = Duration::from_millis(250);
const WINDOW_LABEL: &str = "pet";

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    /// Both methods serve only the cursor-polling hit test, and that loop
    /// does not exist on Linux (the input shape does its job there).
    #[cfg(not(target_os = "linux"))]
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }

    /// Distance from the point to the nearest edge; zero inside.
    #[cfg(not(target_os = "linux"))]
    fn distance_to(&self, x: f64, y: f64) -> f64 {
        let dx = (self.x - x).max(0.0).max(x - (self.x + self.w));
        let dy = (self.y - y).max(0.0).max(y - (self.y + self.h));
        (dx * dx + dy * dy).sqrt()
    }
}

#[derive(Default)]
struct Interactive(Mutex<Vec<Rect>>);

/// When the frontend last spoke to us.
///
/// It reports its clickable regions on every render, and it renders at least
/// once a second, so silence means the page has stopped running. A window can
/// be present, visible and correctly placed while painting nothing, and this
/// is the only thing that can tell the difference.
#[derive(Default)]
struct Frontend {
    last_seen_ms: AtomicU64,
    last_ping_ms: AtomicU64,
    last_rescue_ms: AtomicU64,
}

/// The last session list the poller emitted, so the frontend can ask for a
/// re-send by clearing it. An emit that fires before the page's listeners
/// attach (first boot, any reload) is otherwise lost until the next change.
#[derive(Default)]
struct Emitted(Mutex<String>);

/// The tray's two checkboxes, so a config change made anywhere else can move
/// them. Set once at build time they drifted: toggle quiet from the pet menu
/// and the tray kept claiming the old value.
#[derive(Default)]
struct TrayToggles(Mutex<Option<(CheckMenuItem<tauri::Wry>, CheckMenuItem<tauri::Wry>)>>);

/// Mirrors `config.click_through` so the hit-test loop never touches the disk.
#[derive(Default)]
struct ClickThrough(AtomicBool);

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct PetInfo {
    id: String,
    display_name: String,
    description: String,
    source: &'static str,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct PetPayload {
    manifest: Value,
    /// `None` for the bundled pet, which the frontend loads from its own assets.
    image_data_url: Option<String>,
}

// --- commands ----------------------------------------------------------
#[tauri::command]
fn get_sessions() -> Vec<Session> {
    current_sessions()
}

#[tauri::command]
fn dismiss_codex_questions(session_id: String) -> Result<Vec<Session>, String> {
    crate::codex::dismiss_questions(&session_id)?;
    Ok(current_sessions())
}

/// Everything the overlay shows, with each session's desktop chat attached.
///
/// The hooks cannot supply the title: they run inside the session and know
/// nothing about the window it is displayed in. It is joined on here instead,
/// where a missing answer costs a nicer label and nothing else.
fn current_sessions() -> Vec<Session> {
    let mut sessions = read_sessions();
    chats::decorate(&mut sessions);
    narration::decorate(&mut sessions, narration::mode());
    sessions.extend(crate::codex::sessions(narration::mode()));
    sessions
}

#[tauri::command]
fn connection_status() -> Value {
    let live = crate::codex_live::summary();
    let latest = read_sessions()
        .iter()
        .map(|s| s.updated_ms)
        .max()
        .unwrap_or(0);
    let recent = latest > 0 && state::now_ms().saturating_sub(latest) < 5 * 60 * 1000;
    let hooks = install::installed();
    let codex_logs = std::fs::read_dir(crate::codex::home().join("sessions")).is_ok();
    serde_json::json!({
        "codex": {"status": if live["connected"] == true && (live["desired_tasks"] == 0 || live["observed_tasks"] != 0) { "connected" } else if codex_logs { "fallback" } else { "missing" },
            "observed": live["observed_tasks"], "active": live["active_tasks"]},
        "claude": {"status": if !hooks { "missing" } else if recent { "receiving" } else { "ready" }, "last_event_ms": latest}
    })
}

#[tauri::command]
fn get_config() -> Config {
    load_config()
}

#[tauri::command]
fn set_config(app: AppHandle, config: Config) -> Result<(), String> {
    app.state::<ClickThrough>()
        .0
        .store(config.click_through, Ordering::Relaxed);
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.set_ignore_cursor_events(config.click_through);
    }
    narration::set_mode(narration::Mode::parse(&config.narrate));
    sync_tray_toggles(&app, &config);
    state::save_config(&config).map_err(|e| e.to_string())?;
    ensure_size(&app);
    Ok(())
}

/// Moves the tray checkboxes to match a config changed anywhere else.
fn sync_tray_toggles(app: &AppHandle, config: &Config) {
    if let Ok(guard) = app.state::<TrayToggles>().0.lock() {
        if let Some((click_through, quiet)) = guard.as_ref() {
            let _ = click_through.set_checked(config.click_through);
            let _ = quiet.set_checked(config.quiet);
        }
    }
}

/// The page answering a ping: alive, whatever its throttled timers are doing.
#[tauri::command]
fn frontend_pong(frontend: State<'_, Frontend>) {
    frontend
        .last_seen_ms
        .store(state::now_ms(), Ordering::Relaxed);
}

/// The page has (re)attached its listeners. Anything emitted before this
/// moment was shouted into an empty room, so forget what was last sent and
/// let the poller send it again.
#[tauri::command]
fn frontend_ready(frontend: State<'_, Frontend>, emitted: State<'_, Emitted>) {
    frontend
        .last_seen_ms
        .store(state::now_ms(), Ordering::Relaxed);
    if let Ok(mut last) = emitted.0.lock() {
        last.clear();
    }
}

#[tauri::command]
fn set_hit_rects(
    app: AppHandle,
    rects: Vec<Rect>,
    interactive: State<'_, Interactive>,
    frontend: State<'_, Frontend>,
) {
    frontend
        .last_seen_ms
        .store(state::now_ms(), Ordering::Relaxed);
    #[cfg(target_os = "linux")]
    apply_input_shape(&app, rects.clone());
    #[cfg(not(target_os = "linux"))]
    let _ = &app;
    if let Ok(mut guard) = interactive.0.lock() {
        *guard = rects;
    }
}

/// Restricts the clickable area of the overlay to `rects`; every other pixel
/// falls through to whatever is underneath.
///
/// This replaces the cursor-polling hit test on Linux, which cannot work there.
/// Under Wayland no client may ask where the pointer is, and running through
/// XWayland does not rescue it: X only learns the pointer position while it is
/// over an X surface, and a click-through window never is. Measured on GNOME
/// 50.1 / Wayland, 2026-08-08 — `cursor_position()` returned
/// `Ok((960.0, 540.0))`, the exact centre of the 1920x1080 screen, on every
/// single poll no matter where the mouse actually was:
///
///     hittest: click_through=false rects=2 cursor=Ok((960.0, 540.0)) origin=Ok((67, 32))
///
/// With the pointer permanently "not over" the pet, the poll concluded
/// not-over forever and left the window click-through for its entire life. The
/// pet rendered correctly and could never be clicked or dragged.
///
/// An input shape needs no pointer position at all — the compositor does the
/// routing — so it is both correct and cheaper than polling.
#[cfg(target_os = "linux")]
fn apply_input_shape(app: &AppHandle, rects: Vec<Rect>) {
    use gtk::prelude::*;

    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    // GTK is not thread-safe and this arrives on a command worker thread.
    let _ = app.run_on_main_thread(move || {
        let Ok(gtk_window) = window.gtk_window() else {
            return;
        };
        let Some(gdk_window) = gtk_window.window() else {
            return;
        };
        let region = cairo::Region::create();
        for r in &rects {
            // Outward rounding: a click on the boundary pixel of a card should
            // hit the card, not the desktop behind it.
            let rect = cairo::RectangleInt::new(
                r.x.floor() as i32,
                r.y.floor() as i32,
                r.w.ceil() as i32,
                r.h.ceil() as i32,
            );
            let _ = region.union_rectangle(&rect);
        }
        gdk_window.input_shape_combine_region(&region, 0, 0);
    });
}

/// Tells the page when the pointer has left everything the window accepts it
/// on, because the page itself is never told.
///
/// Outside the input shape the compositor routes the pointer to whatever is
/// underneath, and WebKit sees no crossing event at all: no `pointerleave`, no
/// `pointerout`, and `:hover` stuck on whatever the cursor was last over. That
/// is what left the pet's hint on screen after the cursor had gone. GTK does
/// get the Wayland leave, one layer down, so that is where it has to be read.
///
/// Enter needs no equivalent: inside the shape the page gets its own events.
#[cfg(target_os = "linux")]
fn watch_pointer_crossing(app: &AppHandle) {
    use gtk::prelude::*;

    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    let app = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        let Ok(gtk_window) = window.gtk_window() else {
            return;
        };
        gtk_window.add_events(gdk::EventMask::LEAVE_NOTIFY_MASK);
        gtk_window.connect_leave_notify_event(move |_, event| {
            // Only a crossing the pointer itself made. A drag ends with the
            // compositor dropping its grab, and that arrives here as a leave
            // too — `mode=Ungrab detail=Virtual`, measured on GNOME 50.1 /
            // Wayland, 2026-08-18, immediately after dragging the pet.
            //
            // Nothing here can tell where the pointer went: GTK reports the
            // position it left *from*, which for a departure straight off the
            // pet is the pet. The page settles that instead — it re-reads the
            // cursor from movement, so a leave that turns out to be wrong
            // costs one re-arm and nothing else.
            if event.mode() == gdk::CrossingMode::Normal {
                let _ = app.emit("pipsqueak://pointer-left", ());
            }
            gtk::glib::Propagation::Proceed
        });
    });
}

#[tauri::command]
fn save_position(app: AppHandle, x: i32, y: i32) -> Result<(), String> {
    let _ = app;
    let mut config = load_config();
    config.x = Some(x);
    config.y = Some(y);
    state::save_config(&config).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_pets() -> Vec<PetInfo> {
    let mut pets = vec![
        // Byte is the default, and was missing from this list, so the menu
        // could switch away from it and never back.
        PetInfo {
            id: "byte".into(),
            display_name: "Byte".into(),
            description:
                "A CRT-headed bot. Its screen shows the state, so the pet reads on its own.".into(),
            source: "builtin",
        },
        PetInfo {
            id: "pip".into(),
            display_name: "Pip".into(),
            description: "An ember-fox with a status wisp.".into(),
            source: "builtin",
        },
        PetInfo {
            id: "ember".into(),
            display_name: "Ember".into(),
            description: "A clay pebble with a spark for a status light.".into(),
            source: "builtin",
        },
    ];
    for (dir, source) in [(pets_dir(), "user"), (codex_pets_dir(), "codex")] {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let manifest_path = entry.path().join("pet.json");
            let Some(manifest) = fs::read_to_string(&manifest_path)
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            else {
                continue;
            };
            let folder = entry.file_name().to_string_lossy().to_string();
            let id = manifest
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(&folder)
                .to_string();
            if pets.iter().any(|p| p.id == id) {
                continue;
            }
            pets.push(PetInfo {
                display_name: manifest
                    .get("displayName")
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                    .to_string(),
                description: manifest
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                id,
                source,
            });
        }
    }
    pets
}

#[tauri::command]
fn load_pet(id: String) -> Result<PetPayload, String> {
    if id == "byte" || id == "ember" || id == "pip" {
        return Ok(PetPayload {
            manifest: Value::Null,
            image_data_url: None,
        });
    }
    let folder = find_pet_folder(&id).ok_or_else(|| format!("pet '{id}' not found"))?;
    let manifest: Value = fs::read_to_string(folder.join("pet.json"))
        .map_err(|e| e.to_string())
        .and_then(|raw| serde_json::from_str(&raw).map_err(|e| e.to_string()))?;
    let sprite_name = manifest
        .get("spritesheetPath")
        .and_then(Value::as_str)
        .unwrap_or("spritesheet.webp");
    let sprite_path = folder.join(sprite_name);
    let bytes = fs::read(&sprite_path)
        .map_err(|e| format!("cannot read {}: {e}", sprite_path.display()))?;
    let mime = match sprite_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "gif" => "image/gif",
        _ => "image/webp",
    };
    Ok(PetPayload {
        manifest,
        image_data_url: Some(format!("data:{mime};base64,{}", base64(&bytes))),
    })
}

#[tauri::command]
fn install_hooks() -> Result<String, String> {
    install::install()
}

#[tauri::command]
fn hooks_installed() -> bool {
    install::installed()
}

#[tauri::command]
fn open_pets_dir() -> Result<(), String> {
    let dir = pets_dir();
    let _ = fs::create_dir_all(&dir);
    open_path(&dir)
}

/// Opens the chat a card is about.
///
/// The desktop app is a single window with the chats inside it, so "bring this
/// forward" is two different actions: ask the app to show *that chat*, then
/// raise the window. The first needs the id the app routes by, which only its
/// own records have; without one there is nothing to aim at but a window title,
/// and every chat shares the same one.
#[tauri::command]
fn focus_session(app: AppHandle, session_id: String, project: String, workspace: String) -> bool {
    if session_id.starts_with("codex:") {
        return crate::codex::open(&app, &session_id);
    }
    if let Some(chat) = chats::lookup(&session_id) {
        if chats::open(&app, &chat.id) {
            // The app raises itself when it handles the link. Matching a title
            // as well would only find the same window and risk fighting it.
            return true;
        }
    }
    focus_project(project, workspace)
}

/// Brings the window for a project forward. Matching is by window title, so it
/// is best-effort: `false` means nothing recognisable was open.
fn focus_project(project: String, workspace: String) -> bool {
    let mut fragments = Vec::new();
    if !workspace.is_empty() {
        fragments.push(workspace);
    }
    if !project.is_empty() {
        fragments.push(project);
    }
    fragments.push("claude".to_string());
    desktop::focus_window_titled(&fragments)
}

#[tauri::command]
fn alert() {
    // Do Not Disturb has to be checked here rather than in the frontend: the
    // frontend's copy of the config can be a moment stale, and a beep that
    // escapes a quiet mode is the one failure nobody forgives.
    if load_config().quiet {
        return;
    }
    desktop::alert();
}

#[tauri::command]
fn run_doctor(app: AppHandle) -> doctor::Report {
    doctor::run(&hotkey_binding(app))
}

/// Marks the start of the live connection test. The frontend tells the user to
/// go and run something, then asks what arrived.
#[tauri::command]
fn watch_start() -> u64 {
    doctor::watch_start()
}

#[tauri::command]
fn watch_result(since: u64) -> (String, String) {
    let (status, detail) = doctor::watch_result(since);
    (status.to_string(), detail)
}

#[tauri::command]
fn doctor_report(app: AppHandle) -> String {
    doctor::markdown(&doctor::run(&hotkey_binding(app)))
}

/// Blink the tray icon: a project finished while you were looking elsewhere.
#[tauri::command]
fn flash_tray(app: AppHandle) {
    attention::flash(&app);
}

/// The chord that shows and hides the pet, for the menu to display.
#[tauri::command]
fn hotkey_binding(app: AppHandle) -> String {
    app.state::<Binding>()
        .0
        .lock()
        .map(|held| held.clone())
        .unwrap_or_default()
}

/// You have seen it. Stop blinking.
#[tauri::command]
fn clear_attention(app: AppHandle) {
    attention::clear(&app);
}

#[tauri::command]
fn autostart_enabled() -> bool {
    desktop::autostart_enabled()
}

/// The platform's own words for starting with the machine.
///
/// The panel used to say "Start with Windows" on every platform, which on
/// Linux is both wrong and the kind of wrong that tells a reader the Linux
/// build was an afterthought.
#[tauri::command]
fn at_login() -> &'static str {
    desktop::AT_LOGIN
}

#[tauri::command]
fn set_autostart(enabled: bool) -> Result<(), String> {
    // Turning it off is a decision, and `ensure_autostart` must not helpfully
    // switch it back on at the next launch.
    let mut config = load_config();
    if !config.autostart_initialised {
        config.autostart_initialised = true;
        let _ = state::save_config(&config);
    }
    desktop::set_autostart(enabled)
}

#[tauri::command]
fn quit(app: AppHandle) {
    shutdown(&app, "the panel");
}

/// Stops the overlay, and stops it claiming to be running.
///
/// The heartbeat is what tells the next launch somebody is already home, and
/// it is believed for a few seconds after it was last written. Leaving it
/// behind meant quitting and immediately starting again did nothing at all,
/// silently — the single-instance guard causing the complaint it exists to
/// prevent.
fn shutdown(app: &AppHandle, why: &str) {
    // Logged, because a log with a hundred "started" lines and no "quit"
    // lines cannot tell a crash from a logout from somebody clicking Quit —
    // which was the question a card surviving a reboot raised.
    log::write(&format!("quit via {why}"));
    let _ = fs::remove_file(state::heartbeat_path());
    app.exit(0);
}

/// Puts the pet away without stopping it.
///
/// The distinction matters more than it sounds: the global hotkey and the tray
/// icon belong to this process, so quitting takes them with it and nothing on
/// the keyboard can bring the pet back. Hiding leaves both alive.
#[tauri::command]
fn hide_window(app: AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.hide();
    }
}

// --- helpers -----------------------------------------------------------
fn find_pet_folder(id: &str) -> Option<PathBuf> {
    for dir in [pets_dir(), codex_pets_dir()] {
        let direct = dir.join(id);
        if direct.join("pet.json").exists() {
            return Some(direct);
        }
        let entries = fs::read_dir(&dir).ok()?;
        for entry in entries.flatten() {
            let manifest = entry.path().join("pet.json");
            let matches = fs::read_to_string(&manifest)
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_string))
                .map(|found| found == id)
                .unwrap_or(false);
            if matches {
                return Some(entry.path());
            }
        }
    }
    None
}

fn open_path(path: &PathBuf) -> Result<(), String> {
    #[cfg(windows)]
    let opener = "explorer";
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let opener = "xdg-open";

    desktop::quiet_command(opener)
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn place_window(app: &AppHandle, config: &Config) {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    if let (Some(x), Some(y)) = (config.x, config.y) {
        let (x, y) = clamp_to_display(&window, x, y);
        let _ = window.set_position(PhysicalPosition::new(x, y));
        if (x, y) != (config.x.unwrap_or(x), config.y.unwrap_or(y)) {
            // Remember the corrected position, or every launch pays the same
            // correction and a drag near the edge keeps snapping back.
            let mut stored = config.clone();
            stored.x = Some(x);
            stored.y = Some(y);
            let _ = state::save_config(&stored);
        }
        return;
    }
    // Default: bottom-right of the work area, which already excludes the
    // taskbar wherever it is and however tall it is. The old hard-coded 72px
    // allowance undershot a scaled or stacked taskbar and the pet's feet
    // started behind it until ensure_on_screen corrected it seconds later.
    // Inner size for the same reason clamp_to_display uses it: outer is the
    // shadow, not the pet.
    if let (Ok(Some(monitor)), Ok(size)) = (window.primary_monitor(), window.inner_size()) {
        let area = monitor.work_area();
        let x = area.position.x + (area.size.width as i32 - size.width as i32 - 24).max(0);
        let y = area.position.y + (area.size.height as i32 - size.height as i32 - 24).max(0);
        let _ = window.set_position(PhysicalPosition::new(x, y));
        let mut stored = config.clone();
        stored.x = Some(x);
        stored.y = Some(y);
        let _ = state::save_config(&stored);
    }
}

/// Pulls a position back until the whole window sits inside a display.
///
/// A midpoint check is not enough, which is how the pet ended up below the
/// bottom of the screen: the window is 640 tall, the pet is drawn at the
/// *bottom* of it, and a window whose middle is comfortably on screen can
/// still have its bottom corner hanging off. The part that matters is the part
/// you can see.
///
/// Clamping rather than re-placing also means a saved position that is merely
/// slightly wrong, after a resolution change, a scaling change, or a window
/// that grew between versions, gets nudged back instead of thrown away.
fn clamp_to_display(window: &tauri::WebviewWindow, x: i32, y: i32) -> (i32, i32) {
    clamp_to_display_for_size(window, x, y, window.inner_size().unwrap_or_default())
}

fn clamp_to_display_for_size(
    window: &tauri::WebviewWindow,
    x: i32,
    y: i32,
    size: tauri::PhysicalSize<u32>,
) -> (i32, i32) {
    let Ok(monitors) = window.available_monitors() else {
        return (x, y);
    };
    if monitors.is_empty() {
        return (x, y);
    }
    // Inner, not outer. `outer_size` is unusable here on GTK for two reasons:
    // tao initialises it from `root_origin()` — a *position*, so before the
    // first configure-event it is not a size at all — and then tracks
    // `frame_extents()`, which on GNOME includes the invisible CSD shadow.
    // Feeding either into the clamp made the pet walk: every launch clamped
    // against a window fatter than the one on screen, saved the tightened
    // position, and clamped that again next time. Measured 2026-08-10 across
    // three launches at a fixed 1920x1080: x went 1674 -> 1449 -> 1254 with
    // nobody touching it. The window is undecorated, so the part that has to
    // fit on screen is exactly the inner size.
    let (w, h) = (size.width as i32, size.height as i32);
    let mid_x = x + w / 2;
    let mid_y = y + h / 2;

    // The display the window is mostly on, or the nearest one if it is adrift.
    let contains = |m: &tauri::Monitor| {
        let o = m.position();
        let s = m.size();
        mid_x >= o.x
            && mid_y >= o.y
            && mid_x < o.x + s.width as i32
            && mid_y < o.y + s.height as i32
    };
    let monitor = monitors
        .iter()
        .find(|m| contains(m))
        .or_else(|| {
            monitors.iter().min_by_key(|m| {
                let o = m.position();
                let s = m.size();
                let cx = o.x + s.width as i32 / 2;
                let cy = o.y + s.height as i32 / 2;
                (mid_x - cx).pow(2) + (mid_y - cy).pow(2)
            })
        })
        .unwrap_or(&monitors[0]);

    // Work area rather than the whole screen, so the pet does not end up
    // behind the taskbar.
    let area = monitor.work_area();
    let left = area.position.x;
    let top = area.position.y;
    let right = left + area.size.width as i32 - w;
    let bottom = top + area.size.height as i32 - h;
    // A window taller than the work area has no valid position; pin it to the
    // top-left of that display rather than inverting the range.
    let out = (
        x.clamp(left, right.max(left)),
        y.clamp(top, bottom.max(top)),
    );
    if out != (x, y) {
        // Moving the pet is the visible part; the numbers behind it are the
        // only way to tell a real rescue from a clamp against a bad size.
        let outer = window.outer_size().unwrap_or_default();
        log::write(&format!(
            "clamped ({x},{y}) -> ({},{}) with window inner {w}x{h} outer {}x{} \
             in work area {}x{}+{}+{}",
            out.0,
            out.1,
            outer.width,
            outer.height,
            area.size.width,
            area.size.height,
            area.position.x,
            area.position.y
        ));
    }
    out
}

/// Medium is the declared layout; user size scales the whole window. Cap it
/// to this display's work area so large menus remain reachable on laptops.
fn configured_size(app: &AppHandle) -> Option<LogicalSize<f64>> {
    let base = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == WINDOW_LABEL)?;
    let window = app.get_webview_window(WINDOW_LABEL)?;
    let monitor = window
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| window.primary_monitor().ok().flatten());
    let available = monitor.map(|m| {
        let area = m.work_area();
        (
            area.size.width as f64 / m.scale_factor(),
            area.size.height as f64 / m.scale_factor(),
        )
    });
    let (width, height) =
        scaled_window_size(base.width, base.height, load_config().scale, available);
    Some(LogicalSize::new(width, height))
}

fn scaled_window_size(
    width: f64,
    height: f64,
    scale: f64,
    available: Option<(f64, f64)>,
) -> (f64, f64) {
    let factor = scale / 2.0;
    let (w, h) = (width * factor, height * factor);
    available.map_or((w, h), |(aw, ah)| (w.min(aw), h.min(ah)))
}

/// Puts the overlay back to its declared size after something else changed it.
///
/// THE BUG THIS EXISTS FOR
///
/// Changing the display scale under Wayland shrank the running overlay and left
/// it that way. Measured 2026-08-10, GNOME 50.1, monitor scale 1.25 -> 1.0 with
/// `xwayland-native-scaling` on: the process had been up since before the
/// change, and its X window had gone from the configured 360x640 to 200x320
/// while the page inside still laid out a 322px-wide card. The card was sliced
/// down the middle. A restart fixed it, which is the tell: nothing was wrong
/// with the config, only with the live window.
///
/// The window is `resizable: false`; the selected pet size determines its
/// expected dimensions. An outside resize must not override that preference. The compositor does it anyway when the
/// GDK scale factor changes underneath a running client. Since every such resize
/// is wrong by definition, the repair is simply to ask for the declared size
/// back whenever the observed one differs.
///
/// It cannot key on a scale-factor event, because on Linux there is none: tao's
/// GTK backend emits `Resized` from GTK's configure-event and never emits
/// `ScaleFactorChanged` at all (tao 0.35 platform_impl/linux/event_loop.rs).
/// `Resized` is the better signal anyway — it catches an unwanted resize
/// whatever caused it, not just the one cause we know about.
///
/// Returns whether it had to correct anything.
struct Resizing {
    requested: Option<(u32, u32, Instant)>,
    destination: Option<(u32, u32, i32, i32)>,
}
static RESIZING: Mutex<Resizing> = Mutex::new(Resizing {
    requested: None,
    destination: None,
});

fn settled_position(
    pending: &mut Option<(u32, u32, i32, i32)>,
    have: (u32, u32),
) -> Option<(i32, i32)> {
    let (w, h, x, y) = (*pending)?;
    if w.abs_diff(have.0) > 1 || h.abs_diff(have.1) > 1 {
        return None;
    }
    *pending = None;
    Some((x, y))
}

fn ensure_size(app: &AppHandle) -> bool {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return false;
    };
    let Some(want) = configured_size(app) else {
        return false;
    };
    let (Ok(scale), Ok(have)) = (window.scale_factor(), window.inner_size()) else {
        return false;
    };
    let want = want.to_physical::<u32>(scale);
    let Ok(mut resize) = RESIZING.lock() else {
        return false;
    };
    if have.width.abs_diff(want.width) <= 1 && have.height.abs_diff(want.height) <= 1 {
        let position = settled_position(&mut resize.destination, (have.width, have.height));
        if resize.destination.is_none() {
            resize.requested = None;
        }
        drop(resize);
        if let Some((x, y)) = position {
            // Move only AFTER GTK confirms the new dimensions. Moving while
            // shrinking otherwise gets clamped to the previous, larger bounds.
            let _ = window.set_position(PhysicalPosition::new(x, y));
            let mut config = load_config();
            config.x = Some(x);
            config.y = Some(y);
            let _ = state::save_config(&config);
            let _ = app.emit("pipsqueak://position", (x, y));
            return true;
        }
        return false;
    }
    // The poller and Resized callback can both see the previous dimensions.
    // Keep one destination through duplicate callbacks and rapid S/M/L changes.
    if !request_resize(
        &mut resize.requested,
        (want.width, want.height),
        Instant::now(),
    ) {
        return false;
    }
    let anchor = resize
        .destination
        .map(|(w, h, x, y)| (x + w as i32, y + h as i32))
        .or_else(|| {
            window
                .outer_position()
                .ok()
                .map(|p| (p.x + have.width as i32, p.y + have.height as i32))
        });
    if let Some((right, bottom)) = anchor {
        let (x, y) = clamp_to_display_for_size(
            &window,
            right - want.width as i32,
            bottom - want.height as i32,
            want,
        );
        resize.destination = Some((want.width, want.height, x, y));
    }
    drop(resize);
    log::write(&format!(
        "window was {}x{} at scale {scale}, restoring {}x{}",
        have.width, have.height, want.width, want.height
    ));
    let _ = window.set_size(want);
    false
}

fn request_resize(
    pending: &mut Option<(u32, u32, Instant)>,
    want: (u32, u32),
    now: Instant,
) -> bool {
    if pending
        .is_some_and(|(w, h, at)| (w, h) == want && now.duration_since(at) < Duration::from_secs(2))
    {
        return false;
    }
    *pending = Some((want.0, want.1, now));
    true
}

/// Rescues the overlay when the display it was on disappears while running.
fn ensure_on_screen(app: &AppHandle) {
    // A pending resize owns placement until the compositor confirms its size.
    if RESIZING.lock().map_or(true, |r| r.destination.is_some()) {
        return;
    }
    // A hidden window has no position worth trusting and nothing to rescue:
    // moving it would only overwrite the spot the pet comes back to.
    if !app
        .get_webview_window(WINDOW_LABEL)
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false)
    {
        return;
    }
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    let Ok(position) = window.outer_position() else {
        return;
    };
    let (x, y) = clamp_to_display(&window, position.x, position.y);
    if (x, y) == (position.x, position.y) {
        return;
    }
    let _ = window.set_position(PhysicalPosition::new(x, y));
    let mut config = load_config();
    config.x = Some(x);
    config.y = Some(y);
    let _ = state::save_config(&config);
}

/// Keeps the window click-through except over the pet and its cards.
///
/// A transparent overlay otherwise swallows every click inside its rectangle,
/// which would make the pet actively hostile to the thing it sits on top of.
///
/// This has to be a poll rather than ordinary pointer events, because a window
/// that is ignoring the cursor receives no events to tell it the cursor has
/// arrived. The cost of polling is the gap: a click landing inside it goes to
/// the wrong window. So the rate follows the cursor: fast when it is near
/// something interactive, idle when it is nowhere near the overlay.
///
/// (The alternative is a second, always-interactive window shaped to the hit
/// area. That works for a single small sprite; here the interactive area is a
/// stack of separate cards, and one window covering their union would swallow
/// clicks in the gaps between them.)
fn spawn_hit_test(app: AppHandle) {
    // On Linux the pointer position is not knowable (see apply_input_shape),
    // so this poll can only ever conclude "not over" and would fight the input
    // shape that actually works there. The cost is that the pet does not follow
    // the cursor with its eyes on Linux; the alternative is a pet nobody can
    // click.
    // No `return` here: on Linux the block below is compiled out, so this is
    // already the end of the function, and clippy is right that returning from
    // it is noise.
    #[cfg(target_os = "linux")]
    let _ = app;
    #[cfg(not(target_os = "linux"))]
    std::thread::spawn(move || {
        let mut last: Option<bool> = None;
        let mut interval = HIT_TEST_FAR;
        let mut last_look: Option<Instant> = None;
        let mut watching = false;
        loop {
            std::thread::sleep(interval);
            let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
                continue;
            };
            let click_through = app.state::<ClickThrough>().0.load(Ordering::Relaxed);
            let (Ok(cursor), Ok(origin), Ok(scale)) = (
                window.cursor_position(),
                window.outer_position(),
                window.scale_factor(),
            ) else {
                continue;
            };
            let local_x = (cursor.x - origin.x as f64) / scale;
            let local_y = (cursor.y - origin.y as f64) / scale;

            // Where the cursor is, for a pet that was drawn able to look at it.
            // Rate-limited, and dropped entirely once the cursor is far away,
            // because this is the one message in the app that could otherwise
            // fire every frame.
            let size = window
                .inner_size()
                .map(|size| (size.width as f64 / scale, size.height as f64 / scale))
                .unwrap_or((0.0, 0.0));
            let near = local_x > -LOOK_RADIUS_PX
                && local_y > -LOOK_RADIUS_PX
                && local_x < size.0 + LOOK_RADIUS_PX
                && local_y < size.1 + LOOK_RADIUS_PX;
            if last_look
                .map(|at: Instant| at.elapsed() >= LOOK_INTERVAL)
                .unwrap_or(true)
                && (near || watching)
            {
                // `None` on the way out, once: the pet faces front again rather
                // than staring at wherever the cursor was last seen.
                let payload = near.then_some((local_x, local_y));
                let _ = app.emit("pipsqueak://cursor", payload);
                last_look = Some(Instant::now());
                watching = near;
            }

            if click_through {
                if last != Some(true) {
                    let _ = window.set_ignore_cursor_events(true);
                    last = Some(true);
                }
                interval = HIT_TEST_FAR;
                continue;
            }
            let (over, distance) = app
                .state::<Interactive>()
                .0
                .lock()
                .map(|rects| {
                    let over = rects.iter().any(|r| r.contains(local_x, local_y));
                    let distance = rects
                        .iter()
                        .map(|r| r.distance_to(local_x, local_y))
                        .fold(f64::INFINITY, f64::min);
                    (over, distance)
                })
                .unwrap_or((false, f64::INFINITY));
            // Approaching a card, or just left one: this is the window in which
            // being wrong costs a misdirected click.
            interval = if distance <= NEAR_PX {
                HIT_TEST_NEAR
            } else if distance <= APPROACHING_PX {
                HIT_TEST_APPROACHING
            } else {
                HIT_TEST_FAR
            };
            if last != Some(!over) {
                let _ = window.set_ignore_cursor_events(!over);
                last = Some(!over);
            }
        }
    });
}

fn spawn_poller(app: AppHandle) {
    std::thread::spawn(move || {
        let mut next_sweep = Instant::now();
        // The window rescue, unlike the session sweep, must not run on the
        // first tick.
        //
        // THE BUG THIS EXISTS FOR
        //
        // `place_window` has just put the window exactly where it belongs, so
        // there is nothing to rescue yet — and asking GTK where the window is
        // before it has delivered its first configure-event does not say "I do
        // not know", it says (0,0). The clamp duly pulled that onto the work
        // area and *saved* it, so every launch moved the pet to the top-left
        // corner and forgot where the owner had put it. Measured 2026-08-10
        // with a config reading 1514,425:
        //   clamped (0,0) -> (69,33) with window inner 360x640 ...
        let mut next_rescue = Instant::now() + SWEEP_INTERVAL;
        let mut next_beat = Instant::now();
        let mut next_prompt_check = Instant::now();
        loop {
            if Instant::now() >= next_prompt_check {
                state::settle_answered_prompts();
                next_prompt_check = Instant::now() + PROMPT_CHECK_INTERVAL;
            }
            // Hooks can only ever say what happened; nothing writes a file to
            // report that Claude Code died. The sweep is the only thing that
            // can retire a session, so it has to run on a clock of its own.
            if Instant::now() >= next_sweep {
                state::sweep();
                watch_frontend(&app);
                check_own_binary(&app);
                next_sweep = Instant::now() + SWEEP_INTERVAL;
            }
            // Same cadence, different job: catch an undock, an unplugged
            // monitor, or a display-scale change that left the overlay the
            // wrong size or somewhere you cannot reach it. Size first — a
            // window of the wrong size cannot be clamped to the right place,
            // and a scale change breaks both at once.
            if Instant::now() >= next_rescue {
                ensure_size(&app);
                ensure_on_screen(&app);
                next_rescue = Instant::now() + SWEEP_INTERVAL;
            }
            let sessions = current_sessions();
            let encoded = serde_json::to_string(&sessions).unwrap_or_default();
            // The dedupe string lives in shared state so `frontend_ready` can
            // clear it: an emit fired before the page's listeners attached
            // needs sending again, and nothing else would change the data.
            let changed = app
                .state::<Emitted>()
                .0
                .lock()
                .map(|mut last| {
                    if *last == encoded {
                        false
                    } else {
                        *last = encoded;
                        true
                    }
                })
                .unwrap_or(false);
            if changed {
                let _ = app.emit("pipsqueak://sessions", &sessions);
            }
            if Instant::now() >= next_beat {
                beat();
                next_beat = Instant::now() + BEAT_INTERVAL;
            }
            drain_commands(&app);
            std::thread::sleep(POLL_INTERVAL);
        }
    });
}

/// Starts with Windows by default, and keeps pointing at the right program.
///
/// Two separate failures, both of which look identical from the outside — the
/// pet is simply not there, and the hotkey that would bring it back belongs to
/// a process that is not running:
///
/// 1. Nobody ever turned autostart on. It was a button in a panel shown once,
///    which is a poor way to decide something an overlay depends on entirely.
///    The first run now registers it and says so; turning it off keeps it off.
/// 2. The entry pointed at a path the program no longer lives at, after an
///    install to a different location. Present, plausible, and inert.
///
/// "Not this binary" is not the same question as "dead", which is the mistake
/// this used to make: it rewrote an entry that launched the pet through a
/// wrapper script, on every single start, and each rewrite lost the
/// environment the wrapper set.
fn ensure_autostart(config: &Config) {
    let Ok(exe) = desktop::own_program() else {
        return;
    };
    let exe = exe.to_string_lossy().to_string();
    match desktop::autostart_command() {
        // Registered, but for a program that is not this one. Only worth
        // rewriting if what it names has gone away: a stale entry silently
        // stops the pet coming back after a reboot, while an entry that starts
        // the pet through a wrapper is somebody's deliberate setup and taking
        // it over deletes whatever the wrapper was for.
        Some(registered) if !desktop::same_program(&registered, &exe) => {
            if desktop::program_is_present(&registered) {
                log::write(&format!(
                    "autostart starts {registered}, which is not this program but does exist — left alone"
                ));
            } else {
                log::write(&format!(
                    "autostart pointed at {registered}, which is not there any more; correcting it to {exe}"
                ));
                let _ = desktop::set_autostart(true);
            }
        }
        Some(_) => {}
        None => {
            if config.autostart_initialised {
                // Deliberately off. Leave it alone.
                return;
            }
            let mut stored = config.clone();
            stored.autostart_initialised = true;
            // Record the decision before acting on it, so a failure here is
            // not retried on every launch forever.
            let _ = state::save_config(&stored);
            match desktop::set_autostart(true) {
                Ok(()) => log::write(&format!(
                    "first run: registered to start {}",
                    desktop::AT_LOGIN
                )),
                Err(err) => log::write(&format!("could not register autostart: {err}")),
            }
        }
    }
}

/// Notices that the program has been deleted out from under itself.
///
/// Windows keeps a running image mapped after its file is gone, so the overlay
/// carries on looking healthy while every hook Claude Code fires is pointing
/// at a path that no longer exists. Unix does the same by keeping the inode
/// alive for the open file, and an AppImage moved or deleted while running is
/// the ordinary way to reach that state there. Nothing recovers from this on
/// its own, and the only honest thing to do is say so once, loudly, and keep
/// the record.
fn check_own_binary(app: &AppHandle) {
    static REPORTED: AtomicBool = AtomicBool::new(false);
    let Ok(exe) = desktop::own_program() else {
        return;
    };
    if exe.exists() {
        REPORTED.store(false, Ordering::Relaxed);
        return;
    }
    if REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    log::write(&format!(
        "our own program is gone from {} — hooks will do nothing until it is reinstalled",
        exe.display()
    ));
    let _ = app.emit(
        "pipsqueak://notice",
        "Pipsqueak has been removed from disk. Reinstall it: the hooks now point at a program that is not there.",
    );
}

/// Notices when the page has stopped running, and reloads it.
///
/// The failure this exists for: the window is present, visible, correctly
/// positioned and painting nothing. Hiding and showing it does not help,
/// because an empty page is empty either way, and there is no error anywhere
/// to suggest what happened. The frontend reports its clickable regions on
/// every render, so silence is the signal.
fn watch_frontend(app: &AppHandle) {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    // A hidden window is supposed to be quiet.
    if !window.is_visible().unwrap_or(false) {
        return;
    }
    let frontend = app.state::<Frontend>();
    let last_seen = frontend.last_seen_ms.load(Ordering::Relaxed);
    // Nothing heard yet at all: the page may still be loading.
    if last_seen == 0 {
        return;
    }
    let now = state::now_ms();
    if now.saturating_sub(last_seen) < FRONTEND_SILENT_MS {
        return;
    }
    // Silent is not dead. WebView2 throttles timers for an occluded window
    // down to about once a minute, and a page that stopped rendering because
    // of that is perfectly healthy — reloading it here is what made the
    // overlay blank and repaint every minute under a fullscreen app. Events
    // are not throttled, so ask, and only reload a page that will not answer.
    let pinged = frontend.last_ping_ms.load(Ordering::Relaxed);
    if pinged <= last_seen {
        frontend.last_ping_ms.store(now, Ordering::Relaxed);
        let _ = app.emit("pipsqueak://ping", ());
        return;
    }
    if now.saturating_sub(pinged) < PING_GRACE_MS {
        return;
    }
    if now.saturating_sub(frontend.last_rescue_ms.load(Ordering::Relaxed)) < RESCUE_INTERVAL_MS {
        return;
    }
    frontend.last_rescue_ms.store(now, Ordering::Relaxed);
    log::write(&format!(
        "frontend silent for {}s and not answering pings, reloading the page",
        now.saturating_sub(last_seen) / 1000
    ));
    reload_frontend(app);
}

/// Reloads the page in the overlay window.
pub fn reload_frontend(app: &AppHandle) {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    match window.url() {
        Ok(url) => {
            if let Err(err) = window.navigate(url) {
                log::write(&format!("reload failed: {err}"));
            }
        }
        Err(err) => log::write(&format!("reload could not read the current url: {err}")),
    }
}

/// Tells other invocations of the binary that an overlay is already up.
fn beat() {
    let payload = serde_json::json!({ "ms": state::now_ms(), "pid": std::process::id(), "codex_live": crate::codex_live::summary() });
    let _ = state::write_atomic(
        &state::heartbeat_path(),
        serde_json::to_vec(&payload).unwrap_or_default().as_slice(),
    );
}

/// Applies whatever `pipsqueak control …` left for us, then clears it.
fn drain_commands(app: &AppHandle) {
    let path = state::command_path();
    if !path.is_file() {
        return;
    }
    // Claimed by rename before it is read, so a command written between the
    // read and the delete is not thrown away unread.
    let claimed = path.with_extension("json.busy");
    if fs::rename(&path, &claimed).is_err() {
        return;
    }
    let Ok(raw) = fs::read_to_string(&claimed) else {
        let _ = fs::remove_file(&claimed);
        return;
    };
    let parsed = serde_json::from_str::<Value>(&raw).ok();
    let Some(action) = parsed.as_ref().and_then(|value| {
        value
            .get("action")
            .and_then(Value::as_str)
            .map(String::from)
    }) else {
        let _ = fs::remove_file(&claimed);
        return;
    };
    // A command nobody was around to drain — written in the last moment
    // before a logout or a crash — used to run on the first tick of the next
    // login: the pet quit, or came up hidden, with nothing in the log to say
    // why. A command is an instruction for now, not for whenever.
    let age_ms = parsed
        .as_ref()
        .and_then(|value| value.get("ms"))
        .and_then(Value::as_u64)
        .map(|ms| state::now_ms().saturating_sub(ms))
        .unwrap_or(0);
    if age_ms > COMMAND_STALE_MS {
        log::write(&format!(
            "ignored a {action} command from {}s ago",
            age_ms / 1000
        ));
        let _ = fs::remove_file(&claimed);
        return;
    }
    // A pet switch is delivered as an event, and an event emitted before the
    // page's listeners attach is dropped while the CLI has already printed
    // "Pet switched". Leave the command for a later tick instead — unless a
    // newer one has arrived in the meantime, which supersedes it.
    if action.starts_with("pet:")
        && app.state::<Frontend>().last_seen_ms.load(Ordering::Relaxed) == 0
    {
        if path.exists() {
            let _ = fs::remove_file(&claimed);
        } else {
            let _ = fs::rename(&claimed, &path);
        }
        return;
    }
    let _ = fs::remove_file(&claimed);
    log::write(&format!("command: {action}"));

    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    match action.as_str() {
        "show" => {
            let _ = window.show();
            // "off then on" is what everyone tries first when the overlay is
            // misbehaving, so it must fix a dead page — but reloading a
            // healthy one is itself a visible blank-and-repaint. Ask the page
            // instead; the watchdog reloads it if it stays silent.
            let frontend = app.state::<Frontend>();
            let last_seen = frontend.last_seen_ms.load(Ordering::Relaxed);
            let now = state::now_ms();
            if last_seen != 0 && now.saturating_sub(last_seen) >= FRONTEND_SILENT_MS {
                frontend.last_ping_ms.store(now, Ordering::Relaxed);
                let _ = app.emit("pipsqueak://ping", ());
            }
        }
        "hide" => {
            let _ = window.hide();
        }
        "toggle" => toggle_window(app),
        "quit" => shutdown(app, "a control command"),
        other => {
            if let Some(pet) = other.strip_prefix("pet:") {
                let _ = window.show();
                // The frontend owns both sprite loading and the config file, and
                // writing it from here would race its copy and lose the window
                // position.
                let _ = app.emit("pipsqueak://pet", pet.to_string());
            }
        }
    }
}

fn toggle_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
    } else {
        let _ = window.show();
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let config = load_config();
    let toggle = MenuItem::with_id(app, "toggle", "Show / hide pet", true, None::<&str>)?;
    let click_through = CheckMenuItem::with_id(
        app,
        "click_through",
        "Click through the pet",
        true,
        config.click_through,
        None::<&str>,
    )?;
    let quiet = CheckMenuItem::with_id(
        app,
        "quiet",
        "Do not disturb",
        true,
        config.quiet,
        None::<&str>,
    )?;
    let reload = MenuItem::with_id(app, "reload", "Reload the overlay", true, None::<&str>)?;
    let pets = MenuItem::with_id(app, "pets", "Open pets folder", true, None::<&str>)?;
    let hooks = MenuItem::with_id(
        app,
        "hooks",
        "Reinstall Claude Code hooks",
        true,
        None::<&str>,
    )?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit Pipsqueak", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &toggle,
            &click_through,
            &quiet,
            &PredefinedMenuItem::separator(app)?,
            &reload,
            &pets,
            &hooks,
            &PredefinedMenuItem::separator(app)?,
            &quit_item,
        ],
    )?;

    let mut builder = TrayIconBuilder::with_id("pipsqueak")
        .tooltip("Pipsqueak")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "toggle" => toggle_window(app),
            "click_through" => {
                let mut config = load_config();
                config.click_through = !config.click_through;
                let _ = state::save_config(&config);
                app.state::<ClickThrough>()
                    .0
                    .store(config.click_through, Ordering::Relaxed);
                if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
                    let _ = window.set_ignore_cursor_events(config.click_through);
                }
                // Tell the frontend, or its own copy of the config silently
                // reverts this on its next save.
                let _ = app.emit("pipsqueak://config", &config);
            }
            "quiet" => {
                let mut config = load_config();
                config.quiet = !config.quiet;
                let _ = state::save_config(&config);
                if config.quiet {
                    attention::clear(app);
                }
                let _ = app.emit("pipsqueak://config", &config);
            }
            "reload" => {
                log::write("reload requested from the tray");
                reload_frontend(app);
            }
            "pets" => {
                let _ = open_pets_dir();
            }
            "hooks" => {
                let message = match install::install() {
                    Ok(text) => text,
                    Err(text) => text,
                };
                let _ = app.emit("pipsqueak://notice", message);
            }
            "quit" => shutdown(app, "the tray menu"),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                // Clicking the tray is the user noticing it. Whatever the
                // blink was for, they have seen it.
                attention::clear(tray.app_handle());
                toggle_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }
    builder.build(app)?;
    if let Ok(mut guard) = app.state::<TrayToggles>().0.lock() {
        *guard = Some((click_through, quiet));
    }
    Ok(())
}

pub fn run() {
    // One overlay at a time. A second instance stacks a second pet on the
    // first and both drain the same command file, so show/hide lands on a
    // random one. If somebody is already home, wake them instead.
    if crate::control::is_running() {
        let _ = crate::control::run(Some("show".to_string()));
        return;
    }
    let _ = fs::create_dir_all(sessions_dir());
    let _ = fs::create_dir_all(pets_dir());
    let _ = fs::create_dir_all(root());
    let stale = state::remove_stale_temp_files();
    if stale > 0 {
        log::write(&format!(
            "removed {stale} leftover temp file(s): the previous stop was not clean"
        ));
    }
    state::sweep();

    tauri::Builder::default()
        // A resize we did not ask for is the display configuration changing
        // under us; put the window back before the next frame is drawn rather
        // than waiting up to a sweep for the poller to notice. See ensure_size.
        .on_window_event(|window, event| {
            if window.label() != WINDOW_LABEL || !matches!(event, WindowEvent::Resized(_)) {
                return;
            }
            // Correcting from inside the handler would re-enter GTK's
            // configure-event; and a compositor that insists on its own size
            // would turn "ask again" into a resize loop with a core in it. One
            // correction per interval at most, on a later turn of the loop.
            static LAST_FIX: Mutex<Option<Instant>> = Mutex::new(None);
            let Ok(mut last) = LAST_FIX.lock() else {
                return;
            };
            if last.is_some_and(|t| t.elapsed() < RESIZE_FIX_INTERVAL) {
                return;
            }
            *last = Some(Instant::now());
            drop(last);
            let app = window.app_handle().clone();
            let _ = app.clone().run_on_main_thread(move || {
                if ensure_size(&app) {
                    // The saved position was chosen for the old size, so a
                    // window that just changed size can be off screen at it.
                    ensure_on_screen(&app);
                }
            });
        })
        .manage(Interactive::default())
        .manage(Attention::default())
        .manage(Binding::default())
        .manage(Frontend::default())
        .manage(ClickThrough::default())
        .manage(Emitted::default())
        .manage(TrayToggles::default())
        .invoke_handler(tauri::generate_handler![
            get_sessions,
            dismiss_codex_questions,
            connection_status,
            get_config,
            set_config,
            set_hit_rects,
            save_position,
            list_pets,
            load_pet,
            install_hooks,
            hooks_installed,
            open_pets_dir,
            focus_session,
            alert,
            flash_tray,
            run_doctor,
            watch_start,
            watch_result,
            doctor_report,
            clear_attention,
            hotkey_binding,
            autostart_enabled,
            at_login,
            set_autostart,
            quit,
            hide_window,
            frontend_pong,
            frontend_ready
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let config = load_config();
            handle
                .state::<ClickThrough>()
                .0
                .store(config.click_through, Ordering::Relaxed);
            narration::set_mode(narration::Mode::parse(&config.narrate));
            if let (Some(window), Some(size)) = (
                handle.get_webview_window(WINDOW_LABEL),
                configured_size(&handle),
            ) {
                let _ = window.set_size(size);
            }
            place_window(&handle, &config);
            if let Some(window) = handle.get_webview_window(WINDOW_LABEL) {
                // Order matters, and differs by platform. Setting click-through
                // before show() avoids a frame where the pet swallows clicks,
                // which is why it is first everywhere it can be.
                //
                // On Linux it cannot be: a GTK widget has no underlying GdkWindow
                // until it is realized, and realization is what show() does. tao's
                // CursorIgnoreEvents handler does `window.window().unwrap()`, so
                // calling this while hidden aborts the process
                // (tao/src/platform_impl/linux/event_loop.rs:457). Set it after.
                #[cfg(not(target_os = "linux"))]
                let _ = window.set_ignore_cursor_events(true);
                let _ = window.show();
                #[cfg(target_os = "linux")]
                let _ = window.set_ignore_cursor_events(true);
            }
            build_tray(&handle)?;
            log::write(&format!(
                "started {} pid {} from {}",
                env!("CARGO_PKG_VERSION"),
                std::process::id(),
                desktop::own_program()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "an unknown path".into())
            ));

            // Whatever chord we actually got, so the menu can show it rather
            // than the one that was asked for.
            match hotkey::start(handle.clone()) {
                Ok(chord) => {
                    log::write(&format!("hotkey registered as {chord}"));
                    if let Ok(mut held) = handle.state::<Binding>().0.lock() {
                        *held = chord;
                    }
                }
                Err(reason) => log::write(&format!("no global hotkey: {reason}")),
            }

            ensure_autostart(&config);

            // The heartbeat must exist before anything can race us: the
            // poller's first tick used to be the first beat, and a `control`
            // call landing in that gap launched a second overlay.
            crate::codex_live::start();
            beat();
            spawn_poller(handle.clone());
            #[cfg(target_os = "linux")]
            watch_pointer_crossing(&handle);
            spawn_hit_test(handle);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to start Pipsqueak");
}

#[cfg(test)]
mod sizing_tests {
    use super::{request_resize, scaled_window_size, settled_position};
    use std::time::{Duration, Instant};
    #[test]
    fn all_presets_scale_the_entire_window() {
        assert_eq!(scaled_window_size(360.0, 640.0, 1.5, None), (270.0, 480.0));
        assert_eq!(scaled_window_size(360.0, 640.0, 2.0, None), (360.0, 640.0));
        assert_eq!(scaled_window_size(360.0, 640.0, 3.0, None), (540.0, 960.0));
    }
    #[test]
    fn large_window_fits_a_short_display_without_reducing_text_size() {
        assert_eq!(
            scaled_window_size(360.0, 640.0, 3.0, Some((1366.0, 720.0))),
            (540.0, 720.0)
        );
    }
    #[test]
    fn asynchronous_resize_does_not_move_the_anchor_twice() {
        let now = Instant::now();
        let mut pending = None;
        assert!(request_resize(&mut pending, (270, 480), now));
        assert!(!request_resize(
            &mut pending,
            (270, 480),
            now + Duration::from_millis(10)
        ));
        assert!(request_resize(
            &mut pending,
            (540, 960),
            now + Duration::from_millis(20)
        ));
        assert!(!request_resize(
            &mut pending,
            (540, 960),
            now + Duration::from_millis(30)
        ));
        // A compositor that ignored the request can be retried, boundedly.
        assert!(request_resize(
            &mut pending,
            (540, 960),
            now + Duration::from_secs(3)
        ));
    }
    #[test]
    fn shrinking_waits_for_confirmed_dimensions_before_repositioning() {
        let mut pending = Some((360, 640, 1531, 425));
        assert_eq!(settled_position(&mut pending, (540, 960)), None);
        assert!(pending.is_some());
        assert_eq!(
            settled_position(&mut pending, (360, 640)),
            Some((1531, 425))
        );
        assert_eq!(settled_position(&mut pending, (360, 640)), None);
    }
}
