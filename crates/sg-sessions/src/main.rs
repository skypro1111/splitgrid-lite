//! `splitgrid-sessions --data-dir <dir>`: a menu-bar icon listing SplitGrid
//! Lite's persistent terminal sessions (crates/sg-ptyd), so forgotten ones
//! don't pile up. It runs while Lite is closed, which is the point.
//!
//! - A plain accessory process (no Dock icon, no .app needed); one instance
//!   per user (an flock on `<data dir>/ptyd/sessions-menu.lock`).
//! - A background thread scans the sessions dir ([`scan`]); clicking the icon
//!   drops a popover ([`popover`]) with the sessions as a tree, environment →
//!   workspace → session, with Stop / Close on every row ([`model`] decides
//!   what each reaches) and a read-only View window per session
//!   ([`view_window`]).
//! - Quits by itself once no session is left, or when the user turns the icon
//!   off in Lite's Settings.

mod actions;
mod model;
mod popover;
mod scan;
mod status_item;
mod ui;
mod view_window;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::NSTimer;
use sg_ptyd::paths;

/// Quit after this many one-second ticks with no session.
const EMPTY_TICKS_TO_QUIT: u32 = 5;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut data_dir: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        if a == "--data-dir" {
            data_dir = args.next().map(PathBuf::from);
        }
    }
    let Some(data_dir) = data_dir else {
        eprintln!("usage: splitgrid-sessions --data-dir <SplitGrid data dir>");
        std::process::exit(2);
    };
    // One icon per user: a second launch just leaves.
    let _lock = match sg_ptyd::lock::try_lock(&paths::menu_lock(&data_dir)) {
        Ok(Some(lock)) => lock,
        Ok(None) => return,
        Err(e) => {
            eprintln!("splitgrid-sessions: lock: {e}");
            std::process::exit(1);
        }
    };
    scan::start(paths::sessions_dir(&data_dir));

    let mtm = MainThreadMarker::new().expect("runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    // The controller owns the status item and the popover; AppKit only
    // points at it weakly (targets, delegates), so it lives here.
    let controller = popover::Popover::new(status_item::create(mtm), mtm);

    let settings = data_dir.join("app-settings.json");
    let empty = Cell::new(0u32);
    let tick_controller = controller.clone();
    let tick = block2::RcBlock::new(move |_timer: NonNull<NSTimer>| {
        view_window::refresh();
        let (count, scans) = scan::count();
        tick_controller.tick();
        empty.set(if scans > 0 && count == 0 {
            empty.get() + 1
        } else {
            0
        });
        if empty.get() >= EMPTY_TICKS_TO_QUIT || icon_turned_off(&settings) {
            actions::quit(mtm);
        }
    });
    let _timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0, true, &tick) };
    app.run();
    drop(controller);
}

fn icon_turned_off(settings: &Path) -> bool {
    let v: Option<serde_json::Value> = std::fs::read(settings).ok().and_then(|b| serde_json::from_slice(&b).ok());
    v.and_then(|v| v.get("ptydMenuBarIcon")?.as_bool()) == Some(false)
}
