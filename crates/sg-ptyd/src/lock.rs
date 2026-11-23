//! Single-instance lock (the menu-bar helper): an exclusive `flock` on a file,
//! held for the life of the process and released by the kernel when it dies.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd as _;
use std::path::Path;

pub struct InstanceLock {
    _file: File,
}

/// Take the lock, or `None` when another holder has it.
pub fn try_lock(path: &Path) -> io::Result<Option<InstanceLock>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    // SAFETY: flock on an fd we own.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::EWOULDBLOCK) { Ok(None) } else { Err(e) };
    }
    Ok(Some(InstanceLock { _file: file }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_holder_at_a_time() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("x/menu.lock");
        let first = try_lock(&p).unwrap();
        assert!(first.is_some());
        // A second open file description (as a second process would have) is refused.
        assert!(try_lock(&p).unwrap().is_none());
        drop(first);
        assert!(try_lock(&p).unwrap().is_some());
    }
}
