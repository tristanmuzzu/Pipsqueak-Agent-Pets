//! Small platform integrations: bring another window forward, make a sound,
//! and start with the machine.
//!
//! Dependency-free on purpose: a handful of `user32` calls and the `reg`
//! command are cheaper than pulling a Windows crate graph into an 8 MB app.
//! The Linux side is the same bargain — an XDG autostart entry is a text file,
//! and writing it costs nothing that a crate would save.

/// Where this program lives from the point of view of anything that has to
/// launch it later.
///
/// Not always `current_exe()`. An AppImage is a self-mounting archive: the
/// running binary is `/tmp/.mount_PipsqXXXXXX/usr/bin/pipsqueak`, and that
/// mount is gone the moment the app exits. Registering *that* as the hook
/// command produces a setup where every light is green until the first quit,
/// after which Claude Code spawns a path that no longer exists and the pet
/// never sees another event. The launcher exports `APPIMAGE` with the path of
/// the `.AppImage` file itself, which is the thing that actually persists.
pub fn own_program() -> Result<std::path::PathBuf, String> {
    #[cfg(target_os = "linux")]
    if let Some(bundle) = std::env::var_os("APPIMAGE") {
        let path = std::path::PathBuf::from(bundle);
        // A relative or empty value would be worse than no value at all: it
        // resolves against whatever directory the launching process happened
        // to be in.
        if path.is_absolute() {
            return Ok(path);
        }
    }
    std::env::current_exe().map_err(|e| format!("cannot resolve own path: {e}"))
}

/// Builds a `Command` that never flashes a console window.
///
/// Without this, every `reg`/`explorer` call from the tray menu pops a black
/// rectangle on screen for a frame, which is the one thing a quiet desktop
/// overlay must not do.
pub fn quiet_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    // Nothing to set anywhere else, so the binding is only ever mutated on
    // Windows. Elsewhere this is `Command::new` with a comment.
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Focuses the first visible window whose title contains one of `fragments`
/// (matched case-insensitively, in order of preference).
///
/// Best-effort: Windows refuses foreground changes from background processes
/// under some conditions, so a `false` return is normal and not an error.
pub fn focus_window_titled(fragments: &[String]) -> bool {
    #[cfg(windows)]
    {
        windows_impl::focus(fragments)
    }
    #[cfg(not(windows))]
    {
        let _ = fragments;
        false
    }
}

/// Hands a URL to whatever is registered for its scheme.
///
/// Deliberately not `explorer`, which is right for a folder and wrong for a
/// URL: given a scheme it does not recognise it falls back to treating the
/// string as a path and opens a file window, which is what the ↗ arrow did
/// before this existed.
pub fn open_url(app: &tauri::AppHandle, url: &str) -> bool {
    #[cfg(windows)]
    {
        let _ = app;
        windows_impl::shell_open(url)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = app;
        quiet_command("open").arg(url).spawn().is_ok()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        linux_impl::open_uri(app, url)
    }
}

/// Handing a URL to another application on a Wayland desktop.
///
/// THE BUG THIS EXISTS FOR
///
/// The ↗ arrow did nothing visible for Claude Code on Linux. The deep link was
/// delivered — the desktop app received it and switched to the chat — but the
/// window stayed behind whatever the user was looking at, and GNOME posted a
/// "Claude is ready" notification instead of raising it. That notification is
/// the tell: it is what GNOME shows when an application asks for the focus and
/// the compositor refuses, which it does for any request that does not carry a
/// token proving a user action asked for it.
///
/// `xdg-open`, spawned from a background thread with a bare environment, has no
/// such token to pass on. The launching application is the only one that can
/// mint one, from its own surface and the input event that started this, and
/// GDK does exactly that when a URL is opened through a launch context taken
/// from the display: `XDG_ACTIVATION_TOKEN` on Wayland, `DESKTOP_STARTUP_ID` on
/// X11. The target then hands the token back to the compositor with its raise
/// request, and the raise is allowed.
///
/// Codex was unaffected, which is what made this look like a Claude Code bug:
/// its window was usually being opened rather than raised, and a window that is
/// being mapped for the first time is not competing with anything for focus.
///
/// The second thing this fixes is the answer. `spawn().is_ok()` reported
/// success whenever `xdg-open` itself started, which is to say always, so the
/// arrow never once said it had failed. `launch_default_for_uri` fails loudly
/// when nothing is registered for the scheme, which is the case the user needs
/// told: the desktop app is not installed.
#[cfg(all(unix, not(target_os = "macos")))]
mod linux_impl {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    /// GTK has to be touched on the main thread, and commands arrive on a
    /// worker. Long enough to cross the loop, short enough that a wedged main
    /// thread costs a click rather than the window.
    const MAIN_THREAD_TIMEOUT: Duration = Duration::from_secs(4);

    pub fn open_uri(app: &tauri::AppHandle, url: &str) -> bool {
        let (tx, rx) = mpsc::channel();
        let uri = url.to_string();
        // Whoever gets here first owns the launch. Without this, a main thread
        // that is merely slow rather than wedged would open the URL twice: once
        // from the fallback after the wait gave up, and again when the queued
        // closure finally ran.
        let claim = Arc::new(AtomicBool::new(false));
        let claimed = claim.clone();
        let queued = app.run_on_main_thread(move || {
            if claimed.swap(true, Ordering::SeqCst) {
                return;
            }
            let result = launch(&uri);
            // Nothing was handed over after all, so the claim goes back before
            // the answer does — the waiting side reads it on the way past.
            if result.is_none() {
                claimed.store(false, Ordering::SeqCst);
            }
            let _ = tx.send(result);
        });
        if queued.is_err() {
            return fallback(url, &claim);
        }
        match rx.recv_timeout(MAIN_THREAD_TIMEOUT) {
            // Only the "nothing handles this scheme" answer is worth a second
            // attempt; a missing display is the same answer twice.
            Ok(Some(opened)) => opened,
            Ok(None) | Err(_) => fallback(url, &claim),
        }
    }

    /// `None` when there is no display to take a launch context from, which is
    /// not an answer about the URL and so is left to the caller.
    fn launch(uri: &str) -> Option<bool> {
        // The context is what carries the token. Without a display there is
        // none to mint, and launching without one is the old behaviour.
        let context = gtk::gdk::Display::default()?.app_launch_context()?;
        Some(gtk::gio::AppInfo::launch_default_for_uri(uri, Some(&context)).is_ok())
    }

    /// The old path, kept for the cases GDK cannot answer: no display, or a
    /// main thread that never got to the closure. Loses the token with it.
    fn fallback(url: &str, claim: &Arc<AtomicBool>) -> bool {
        if claim.swap(true, Ordering::SeqCst) {
            return true;
        }
        super::quiet_command("xdg-open").arg(url).spawn().is_ok()
    }
}

pub fn alert() {
    #[cfg(windows)]
    unsafe {
        // MB_ICONASTERISK: the quiet "something happened" sound.
        windows_impl::MessageBeep(0x00000040);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = quiet_command("afplay")
            .arg("/System/Library/Sounds/Ping.aiff")
            .spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // The desktop's own "message" event sound through libcanberra, which
        // honours the user's sound theme and their mute; the freedesktop
        // file directly when that helper is not installed. Silent until now:
        // the menu offered "Play a sound when Claude needs you" and nothing
        // on this platform ever played one.
        let played = quiet_command("canberra-gtk-play")
            .args(["-i", "message"])
            .spawn()
            .is_ok();
        if !played {
            let _ = quiet_command("paplay")
                .arg("/usr/share/sounds/freedesktop/stereo/message.oga")
                .spawn();
        }
    }
}

/// What "start with the machine" is called where the user can see it.
///
/// The mechanism differs, and so does the honest sentence: Windows starts the
/// program with the operating system, an XDG autostart entry starts it when
/// the desktop session begins.
pub const AT_LOGIN: &str = if cfg!(windows) {
    "with Windows"
} else {
    "when you log in"
};

/// Are these two path strings the same program?
///
/// Windows: case and slash direction do not make two programs. Elsewhere they
/// do — `/opt/Pipsqueak` and `/opt/pipsqueak` are two different files on a
/// case-sensitive filesystem, and folding them would report a stale autostart
/// entry as healthy.
pub fn same_program(a: &str, b: &str) -> bool {
    let trim = |p: &str| p.trim().trim_matches('"').to_string();
    #[cfg(windows)]
    let normalise = |p: &str| trim(p).replace('/', "\\").to_lowercase();
    #[cfg(not(windows))]
    let normalise = trim;
    normalise(a) == normalise(b)
}

/// Is there a program at the other end of this command?
///
/// The difference between an autostart entry worth correcting and one worth
/// leaving alone. A stale entry names a path the app was installed to two
/// versions ago and starts nothing; an entry that launches the pet through a
/// wrapper script also names something that is not this binary, and works.
/// Rewriting the second kind throws away whatever the wrapper existed to do —
/// on the machine this was found on, the `GDK_BACKEND=x11` without which the
/// pet is not on top of anything (2026-08-16).
///
/// A bare name is resolved against `PATH`, because `Exec=pipsqueak` is a legal
/// entry and calling it missing would delete somebody's working setup.
pub fn program_is_present(command: &str) -> bool {
    let command = command.trim().trim_matches('"');
    if command.is_empty() {
        return false;
    }
    let path = std::path::Path::new(command);
    if path.components().count() > 1 || path.is_absolute() {
        return is_runnable(path);
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| is_runnable(&dir.join(command)))
}

fn is_runnable(path: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    // A file nobody may execute cannot start the pet, and on Windows the
    // question does not arise: being there is the whole test.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn autostart_enabled() -> bool {
    autostart_command().is_some()
}

/// The program the autostart entry actually launches, if there is one.
///
/// Worth having separately from "is it enabled": an entry pointing at a path
/// the app was installed to two versions ago is present, looks healthy, and
/// starts nothing.
pub fn autostart_command() -> Option<String> {
    #[cfg(windows)]
    {
        let out = quiet_command("reg")
            .args(["query", RUN_KEY, "/v", "Pipsqueak"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        // "    Pipsqueak    REG_SZ    "C:\…\pipsqueak.exe""
        let text = String::from_utf8_lossy(&out.stdout);
        let value = text
            .lines()
            .find(|line| line.contains("REG_SZ"))?
            .split_once("REG_SZ")?
            .1
            .trim()
            .trim_matches('"')
            .to_string();
        (!value.is_empty()).then_some(value)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let raw = std::fs::read_to_string(autostart_entry()).ok()?;
        // A desktop entry can be disabled without being deleted, and something
        // that will not launch is not autostart however present the file is.
        // `Hidden=true` is the XDG way (and what KDE's settings write);
        // GNOME's key is honoured too.
        if raw.lines().any(|line| {
            let line = line.trim();
            line.eq_ignore_ascii_case("X-GNOME-Autostart-enabled=false")
                || line.eq_ignore_ascii_case("Hidden=true")
        }) {
            return None;
        }
        let exec = raw
            .lines()
            .find_map(|line| line.trim().strip_prefix("Exec="))?
            .trim();
        let command = unquote_exec(exec);
        (!command.is_empty()).then_some(command)
    }
    #[cfg(target_os = "macos")]
    {
        None
    }
}

pub fn set_autostart(enabled: bool) -> Result<(), String> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let entry = autostart_entry();
        if !enabled {
            return match std::fs::remove_file(&entry) {
                Ok(()) => Ok(()),
                // Already absent is the state that was asked for.
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(format!("cannot remove {}: {err}", entry.display())),
            };
        }
        let exe = own_program()?;
        // An entry that starts something else which exists is a launcher for
        // the pet — a wrapper that sets the environment or waits for the
        // tray — and turning autostart "on" over it replaced it with the bare
        // binary and silently undid whatever the wrapper was for. Already on,
        // so leave it.
        if let Some(registered) = autostart_command() {
            if !same_program(&registered, &exe.to_string_lossy()) && program_is_present(&registered)
            {
                return Ok(());
            }
        }
        if let Some(dir) = entry.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        // Terminal=false and NoDisplay=true keep it out of the applications
        // menu: this file exists to start the overlay at login, and a second
        // copy of Pipsqueak in the launcher grid is not a feature.
        let entry_text = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Pipsqueak\n\
             Comment=Desktop pet for Claude Code and Codex\n\
             Exec={}\n\
             Icon=pipsqueak\n\
             Terminal=false\n\
             NoDisplay=true\n\
             X-GNOME-Autostart-enabled=true\n",
            quote_exec(&exe.to_string_lossy())
        );
        crate::state::write_atomic(&entry, entry_text.as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", entry.display()))
    }
    #[cfg(windows)]
    {
        let exe = own_program()?.to_string_lossy().to_string();
        let status = if enabled {
            quiet_command("reg")
                .args([
                    "add",
                    RUN_KEY,
                    "/v",
                    "Pipsqueak",
                    "/t",
                    "REG_SZ",
                    "/d",
                    &exe,
                    "/f",
                ])
                .status()
        } else {
            quiet_command("reg")
                .args(["delete", RUN_KEY, "/v", "Pipsqueak", "/f"])
                .status()
        };
        match status {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!("reg exited with {status}")),
            Err(err) => Err(err.to_string()),
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = enabled;
        Err("autostart is not implemented on macOS yet".to_string())
    }
}

/// The XDG autostart entry: every desktop that implements the spec reads this
/// folder at login, so one file covers GNOME, KDE, XFCE and the rest.
#[cfg(all(unix, not(target_os = "macos")))]
fn autostart_entry() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| crate::state::home_dir().join(".config"));
    base.join("autostart").join("pipsqueak.desktop")
}

/// Quotes a program path for a desktop entry's `Exec=` key.
///
/// The spec reserves a small pile of characters, and an unquoted path
/// containing any of them is not merely wrong, it is a different command. An
/// AppImage in `~/Downloads/My Apps/` is the ordinary case.
#[cfg(all(unix, not(target_os = "macos")))]
fn quote_exec(path: &str) -> String {
    let needs_quoting = path.chars().any(|c| " \t\n\"'\\><~|&;$*?#()`".contains(c));
    if !needs_quoting {
        return path.to_string();
    }
    let escaped = path.replace('\\', r"\\").replace('"', r#"\""#);
    format!("\"{escaped}\"")
}

/// The inverse, tolerant of an entry somebody wrote by hand.
///
/// Only the program is wanted, so anything after it is dropped — including the
/// `%f`-style field codes the spec allows.
#[cfg(all(unix, not(target_os = "macos")))]
fn unquote_exec(exec: &str) -> String {
    let exec = exec.trim();
    if let Some(rest) = exec.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => out.extend(chars.next()),
                '"' => break,
                _ => out.push(c),
            }
        }
        return out;
    }
    exec.split_whitespace().next().unwrap_or("").to_string()
}

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
mod windows_impl {
    use std::ffi::c_void;

    type Hwnd = *mut c_void;
    type Bool = i32;

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(callback: extern "system" fn(Hwnd, isize) -> Bool, data: isize) -> Bool;
        fn GetWindowTextW(window: Hwnd, buffer: *mut u16, max: i32) -> i32;
        fn IsWindowVisible(window: Hwnd) -> Bool;
        fn IsIconic(window: Hwnd) -> Bool;
        fn SetForegroundWindow(window: Hwnd) -> Bool;
        fn ShowWindow(window: Hwnd, command: i32) -> Bool;
        pub fn MessageBeep(kind: u32) -> Bool;
    }

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            window: Hwnd,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> Hwnd;
    }

    const SW_RESTORE: i32 = 9;
    const SW_SHOWNORMAL: i32 = 1;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The same call the shell makes when you type a URL into Run. Anything
    /// over 32 is success; the error codes below it are all "nothing is
    /// registered for this", which is not worth reporting to a pet.
    pub fn shell_open(target: &str) -> bool {
        let operation = wide("open");
        let file = wide(target);
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                operation.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        result as isize > 32
    }

    struct Search {
        fragments: Vec<String>,
        /// Index of the matched fragment, so an earlier (more specific)
        /// fragment always wins over a later one.
        best_rank: usize,
        best: Hwnd,
    }

    extern "system" fn visit(window: Hwnd, data: isize) -> Bool {
        // Safety: `data` is the pointer we handed to EnumWindows below, which
        // outlives the enumeration.
        let search = unsafe { &mut *(data as *mut Search) };
        if unsafe { IsWindowVisible(window) } == 0 {
            return 1;
        }
        let mut buffer = [0u16; 512];
        let len = unsafe { GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32) };
        if len <= 0 {
            return 1;
        }
        let title = String::from_utf16_lossy(&buffer[..len as usize]).to_lowercase();
        for (rank, fragment) in search.fragments.iter().enumerate() {
            if rank >= search.best_rank {
                break;
            }
            if title.contains(fragment.as_str()) {
                search.best_rank = rank;
                search.best = window;
                break;
            }
        }
        // Stop early only on a first-choice match.
        if search.best_rank == 0 {
            0
        } else {
            1
        }
    }

    pub fn focus(fragments: &[String]) -> bool {
        let mut search = Search {
            fragments: fragments
                .iter()
                .map(|f| f.to_lowercase())
                .filter(|f| !f.is_empty())
                .collect(),
            best_rank: usize::MAX,
            best: std::ptr::null_mut(),
        };
        if search.fragments.is_empty() {
            return false;
        }
        unsafe { EnumWindows(visit, &mut search as *mut Search as isize) };
        if search.best.is_null() {
            return false;
        }
        unsafe {
            // Only a *minimised* window is restored. `SW_RESTORE` on a
            // maximised one un-maximises it, which is why bringing an editor
            // forward used to shrink it to half the screen on the way.
            if IsIconic(search.best) != 0 {
                ShowWindow(search.best, SW_RESTORE);
            }
            SetForegroundWindow(search.best) != 0
        }
    }
}

#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::{program_is_present, quote_exec, unquote_exec};

    #[test]
    fn a_path_that_is_not_there_is_not_a_program() {
        assert!(!program_is_present(
            "/nonexistent/pipsqueak-from-two-versions-ago"
        ));
        assert!(!program_is_present(""));
    }

    /// The wrapper case, which is the whole point: something that is not this
    /// binary, exists, and must survive the next start.
    #[test]
    fn an_executable_that_is_there_is_a_program() {
        assert!(program_is_present("/bin/sh"));
    }

    /// A directory and a file nobody can run are both present and neither can
    /// start anything.
    #[test]
    fn present_is_not_enough() {
        let dir = std::env::temp_dir();
        assert!(!program_is_present(&dir.to_string_lossy()));

        let plain = dir.join("pipsqueak-not-executable-test");
        std::fs::write(&plain, b"not a program").unwrap();
        assert!(!program_is_present(&plain.to_string_lossy()));
        let _ = std::fs::remove_file(&plain);
    }

    /// `Exec=pipsqueak` is a legal entry, so a bare name is looked for on PATH
    /// rather than called missing.
    #[test]
    fn a_bare_name_is_resolved_against_path() {
        assert!(program_is_present("sh"));
        assert!(!program_is_present("pipsqueak-that-nobody-installed"));
    }

    #[test]
    fn a_plain_path_is_left_alone() {
        assert_eq!(quote_exec("/usr/bin/pipsqueak"), "/usr/bin/pipsqueak");
    }

    #[test]
    fn a_path_with_a_space_survives_the_round_trip() {
        let path = "/home/me/My Apps/Pipsqueak_0.7.4_amd64.AppImage";
        assert_eq!(unquote_exec(&quote_exec(path)), path);
    }

    #[test]
    fn quotes_and_backslashes_survive_too() {
        let path = r#"/home/me/it's "here"/pip\squeak"#;
        assert_eq!(unquote_exec(&quote_exec(path)), path);
    }

    /// The point of reading `Exec=` at all is to compare it with our own path,
    /// so a trailing field code must not end up part of the program.
    #[test]
    fn field_codes_are_not_part_of_the_program() {
        assert_eq!(unquote_exec("/usr/bin/pipsqueak %U"), "/usr/bin/pipsqueak");
        assert_eq!(
            unquote_exec(r#""/usr/bin/pip squeak" %U"#),
            "/usr/bin/pip squeak"
        );
    }
}
