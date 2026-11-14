//! Getting your attention when the overlay itself cannot.
//!
//! The whole point of this app is that you can glance at it while doing
//! something else. But a maximised video or a full-screen editor covers it
//! completely, and then a finished turn goes unnoticed until you happen to
//! look. The tray icon is the one surface that stays reachable, so a
//! completion blinks it.
//!
//! Small and interruptible: it blinks for a few seconds, any
//! click stops it, and Do Not Disturb suppresses it entirely.

use crate::state::load_config;
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::image::Image;
use tauri::{AppHandle, Manager};

const TRAY_ID: &str = "pipsqueak";
const BLINK_MS: u64 = 500;
/// Long enough to catch the corner of your eye, short enough that a finished
/// build does not sit there strobing at you.
const BLINKS: u32 = 10;

/// Cancels whatever flash is in flight. Every start takes a ticket, and a
/// blink whose ticket is stale stops rather than fighting the newer one.
#[derive(Default)]
pub struct Attention {
    generation: AtomicU64,
}

impl Attention {
    fn next(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn current(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}

/// Stops any flash and puts the normal icon back.
pub fn clear(app: &AppHandle) {
    app.state::<Attention>().next();
    if let (Some(tray), Some(icon)) = (app.tray_by_id(TRAY_ID), normal_icon(app)) {
        let _ = tray.set_icon(Some(icon));
    }
}

pub fn flash(app: &AppHandle, waiting: bool) {
    let config = load_config();
    if !should_flash(config.quiet, config.flash_on_finish, waiting) {
        return;
    }
    let Some(normal) = normal_icon(app) else {
        return;
    };
    let Some(alert) = alert_icon(app) else {
        return;
    };

    let ticket = app.state::<Attention>().next();
    let app = app.clone();
    std::thread::spawn(move || {
        for step in 0..BLINKS {
            if app.state::<Attention>().current() != ticket {
                return;
            }
            let icon = if step % 2 == 0 { &alert } else { &normal };
            if let Some(tray) = app.tray_by_id(TRAY_ID) {
                let _ = tray.set_icon(Some(icon.clone()));
            }
            std::thread::sleep(std::time::Duration::from_millis(BLINK_MS));
        }
        // Only tidy up after ourselves. A newer flash owns the icon now.
        if app.state::<Attention>().current() == ticket {
            if let Some(tray) = app.tray_by_id(TRAY_ID) {
                let _ = tray.set_icon(Some(normal.clone()));
            }
        }
    });
}

fn should_flash(quiet: bool, flash_on_finish: bool, waiting: bool) -> bool {
    !quiet && (waiting || flash_on_finish)
}

#[cfg(test)]
mod tests {
    use super::should_flash;

    #[test]
    fn waiting_alerts_are_independent_of_completion_alerts_but_respect_quiet() {
        assert!(should_flash(false, false, true));
        assert!(!should_flash(false, false, false));
        assert!(should_flash(false, true, false));
        for waiting in [false, true] {
            assert!(!should_flash(true, true, waiting));
            assert!(!should_flash(true, false, waiting));
        }
    }
}

fn normal_icon(app: &AppHandle) -> Option<Image<'static>> {
    let base = app.default_window_icon()?;
    Some(Image::new_owned(
        base.rgba().to_vec(),
        base.width(),
        base.height(),
    ))
}

/// The same icon pushed towards the alert colour.
///
/// Recolouring rather than swapping in a second asset keeps it recognisably
/// the same pet. A blink between two unrelated pictures reads as a glitch.
fn alert_icon(app: &AppHandle) -> Option<Image<'static>> {
    let base = app.default_window_icon()?;
    let mut rgba = base.rgba().to_vec();
    for pixel in rgba.chunks_exact_mut(4) {
        if pixel[3] == 0 {
            continue;
        }
        pixel[0] = pixel[0].saturating_add(96);
        pixel[1] = ((pixel[1] as u16 * 3) / 5) as u8;
        pixel[2] = ((pixel[2] as u16 * 2) / 5) as u8;
    }
    Some(Image::new_owned(rgba, base.width(), base.height()))
}
