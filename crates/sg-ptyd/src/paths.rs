//! Where sessions and the installed binaries live, under Lite's data dir:
//!
//! ```text
//! <data dir>/ptyd/        0700
//!   s/<hash>.sock         one socket per session, named by a hash of the container id
//!   bin/                  the installed daemon and menu helper (versioned names)
//!   sessions-menu.lock    the menu helper's single-instance lock
//! ```
//!
//! The socket name is a short hash because `sun_path` holds 104 bytes on
//! macOS and `~/Library/Application Support/SplitGrid/…` already uses half.

use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

/// `sun_path` is 104 bytes on macOS, NUL included.
pub const SUN_PATH_MAX: usize = 103;

pub fn root(data_dir: &Path) -> PathBuf {
    data_dir.join("ptyd")
}

pub fn sessions_dir(data_dir: &Path) -> PathBuf {
    root(data_dir).join("s")
}

pub fn bin_dir(data_dir: &Path) -> PathBuf {
    root(data_dir).join("bin")
}

pub fn menu_lock(data_dir: &Path) -> PathBuf {
    root(data_dir).join("sessions-menu.lock")
}

/// The socket file name for a container id.
pub fn socket_name(container_id: &str) -> String {
    let digest = Sha256::digest(container_id.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("{hex}.sock")
}

pub fn socket_path(sessions_dir: &Path, container_id: &str) -> PathBuf {
    sessions_dir.join(socket_name(container_id))
}

pub fn check_len(socket: &Path) -> io::Result<()> {
    if socket.as_os_str().len() > SUN_PATH_MAX {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("socket path too long ({} bytes): {}", socket.as_os_str().len(), socket.display())));
    }
    Ok(())
}

/// Create `dir` (and parents) and make it 0700 and ours; refuse a directory
/// another user owns.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let meta = std::fs::metadata(dir)?;
    if meta.uid() != crate::sys::euid() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} is owned by uid {}", dir.display(), meta.uid())));
    }
    if meta.permissions().mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Every `*.sock` in the sessions dir.
pub fn list_sockets(sessions_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "sock")).collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_names_are_short_stable_and_distinct() {
        let a = socket_name("container-mhx1abc0");
        assert_eq!(a, socket_name("container-mhx1abc0"));
        assert_ne!(a, socket_name("container-mhx1abc1"));
        assert_eq!(a.len(), 16 + 5);
        // The real data dir's socket fits sun_path with room to spare.
        let real = Path::new("/Users/someone.with.a.long.name/Library/Application Support/SplitGrid");
        check_len(&socket_path(&sessions_dir(real), "x")).unwrap();
    }

    #[test]
    fn private_dir_is_0700() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("a/b");
        ensure_private_dir(&d).unwrap();
        assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&d).unwrap();
        assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
    }
}
