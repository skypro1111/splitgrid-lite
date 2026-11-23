//! What the buttons do. Anything that talks to a daemon runs on a thread of
//! its own and rescans when done; the main thread only asks and dispatches.

use std::path::PathBuf;
use std::time::Duration;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication};
use objc2_foundation::{NSString, ns_string};
use sg_ptyd::client;

use crate::scan;

/// Lite's bundle id (scripts/bundle.sh), for "Open SplitGrid Lite".
const LITE_BUNDLE_ID: &str = "dev.splitgrid.lite";
const TIMEOUT: Duration = Duration::from_secs(3);

/// Ctrl-C to each session's foreground job.
pub fn stop(sockets: Vec<PathBuf>) {
    if sockets.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        for s in sockets {
            let _ = client::interrupt(&s, TIMEOUT);
        }
        scan::rescan_soon();
    });
}

/// End each session (whatever protocol it speaks).
pub fn close(sockets: Vec<PathBuf>) {
    if sockets.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        for s in sockets {
            let _ = client::terminate(&s, TIMEOUT);
        }
        scan::rescan_soon();
    });
}

#[allow(deprecated)] // activate() is macOS 14+; Lite supports 13.
pub fn bring_forward(mtm: MainThreadMarker) {
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

/// A warning with "End" / "Cancel"; true on "End".
pub fn ask_end(message: &str, informative: &str, mtm: MainThreadMarker) -> bool {
    bring_forward(mtm);
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&NSString::from_str(message));
    alert.setInformativeText(&NSString::from_str(informative));
    alert.addButtonWithTitle(ns_string!("End"));
    alert.addButtonWithTitle(ns_string!("Cancel"));
    alert.runModal() == NSAlertFirstButtonReturn
}

pub fn open_lite() {
    if let Ok(mut child) = std::process::Command::new("/usr/bin/open").args(["-b", LITE_BUNDLE_ID]).spawn() {
        std::thread::spawn(move || child.wait());
    }
}

pub fn quit(mtm: MainThreadMarker) {
    NSApplication::sharedApplication(mtm).terminate(None);
}
