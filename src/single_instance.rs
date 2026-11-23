//! One Lite per data directory. A second launch (a notification click while
//! LaunchServices doesn't match the running copy, a second bundle, `open -n`)
//! brings the running one forward and exits, instead of opening another set of
//! windows on the same store.
//!
//! The lock is an flock on `<data>/lite.lock` (the Electron app, which shares the
//! data dir, doesn't take it); the holder writes its pid there. The kernel drops
//! the lock when the process ends, however it ends.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::os::fd::AsRawFd as _;
use std::path::Path;

/// Take the lock (None: it can't be taken, so don't stand in the way of
/// starting), or return the pid of the Lite that holds it.
pub fn acquire(data_dir: &Path) -> Result<Option<File>, Option<i32>> {
    let _ = std::fs::create_dir_all(data_dir);
    let Ok(mut file) = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(data_dir.join("lite.lock")) else {
        return Ok(None);
    };
    // SAFETY: flock on a descriptor we own.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let mut s = String::new();
        let _ = file.read_to_string(&mut s);
        return Err(s.trim().parse().ok());
    }
    let _ = file.set_len(0);
    let _ = file.seek(SeekFrom::Start(0));
    let _ = write!(file, "{}", std::process::id());
    let _ = file.flush();
    Ok(Some(file))
}

/// Bring the Lite with `pid` forward (all its windows).
pub fn activate(pid: i32) {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
        #[allow(deprecated)]
        app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
    }
}

#[cfg(test)]
mod tests {
    use super::acquire;

    #[test]
    fn second_holder_sees_the_first_pid() {
        let dir = std::env::temp_dir().join(format!("sg-lock-{}", std::process::id()));
        let first = acquire(&dir).expect("first");
        // A second open file description conflicts, as another process would.
        assert_eq!(acquire(&dir).err(), Some(Some(std::process::id() as i32)));
        drop(first);
        assert!(acquire(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
