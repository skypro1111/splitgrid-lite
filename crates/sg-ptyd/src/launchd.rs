//! Running a helper as its own launchd job in the user's GUI domain.
//!
//! macOS ends everything an app launched through LaunchServices started when
//! the app quits — setsid and a reparent to launchd don't help: the processes
//! stay in the app's job. A session daemon (and the menu-bar helper) must
//! outlive Lite, so Lite submits each as a separate job (`launchctl bootstrap
//! gui/<uid> <plist>`), the way iTerm2 runs its session servers. Jobs never
//! restart (KeepAlive false); a finished job's label is booted out by the
//! daemon itself as it exits, or swept by Lite.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const LABEL_PREFIX: &str = "dev.splitgrid.lite.";

/// `dev.splitgrid.lite.ptyd.<socket stem>`.
pub fn session_label(socket: &Path) -> String {
    let stem = socket.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    format!("{LABEL_PREFIX}ptyd.{stem}")
}

pub const MENU_LABEL: &str = "dev.splitgrid.lite.sessions";

pub fn jobs_dir(data_dir: &Path) -> PathBuf {
    crate::paths::root(data_dir).join("jobs")
}

fn uid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

pub fn domain() -> String {
    format!("gui/{}", uid())
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The job's plist: run once at load, never restart, keep its children.
pub fn plist(label: &str, args: &[String], workdir: &Path) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
    );
    out.push_str(&format!("  <key>Label</key><string>{}</string>\n", xml(label)));
    out.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    for a in args {
        out.push_str(&format!("    <string>{}</string>\n", xml(a)));
    }
    out.push_str("  </array>\n");
    out.push_str(&format!("  <key>WorkingDirectory</key><string>{}</string>\n", xml(&workdir.to_string_lossy())));
    out.push_str("  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><false/>\n  <key>AbandonProcessGroup</key><true/>\n");
    out.push_str("  <key>ProcessType</key><string>Interactive</string>\n</dict>\n</plist>\n");
    out
}

fn launchctl(args: &[&str]) -> io::Result<std::process::Output> {
    Command::new("/bin/launchctl").args(args).stdin(Stdio::null()).output()
}

/// Whether `label` is loaded in the GUI domain.
pub fn loaded(label: &str) -> bool {
    launchctl(&["print", &format!("{}/{label}", domain())]).is_ok_and(|o| o.status.success())
}

/// Unload `label` (ends its process if it still runs). Errors are ignored: it
/// may not be loaded.
pub fn remove(label: &str) {
    let _ = launchctl(&["bootout", &format!("{}/{label}", domain())]);
}

/// Unload `label` without waiting: for a job removing itself as it exits.
pub fn remove_detached(label: &str) {
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("{}/{label}", domain())])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Write the plist into `jobs_dir` and load it, replacing a stale job of the
/// same label. The job starts at once.
pub fn submit(jobs_dir: &Path, label: &str, args: &[String], workdir: &Path) -> io::Result<()> {
    crate::paths::ensure_private_dir(jobs_dir)?;
    let file = jobs_dir.join(format!("{label}.plist"));
    std::fs::write(&file, plist(label, args, workdir))?;
    if loaded(label) {
        remove(label);
    }
    let out = launchctl(&["bootstrap", &domain(), &file.to_string_lossy()])?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("launchctl bootstrap {label}: {}", String::from_utf8_lossy(&out.stderr).trim())))
    }
}

/// Start an already loaded job again (it ran and exited); no-op if running.
pub fn kickstart(label: &str) -> bool {
    launchctl(&["kickstart", &format!("{}/{label}", domain())]).is_ok_and(|o| o.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_escapes_and_never_restarts() {
        let p = plist("dev.splitgrid.lite.ptyd.ab", &["/a b/x".into(), "<&>".into()], Path::new("/Users/me"));
        assert!(p.contains("<string>/a b/x</string>"));
        assert!(p.contains("<string>&lt;&amp;&gt;</string>"));
        assert!(p.contains("<key>KeepAlive</key><false/>"));
        assert!(p.contains("<key>AbandonProcessGroup</key><true/>"));
        assert!(p.contains("<key>RunAtLoad</key><true/>"));
    }

    #[test]
    fn labels() {
        assert_eq!(session_label(Path::new("/d/ptyd/s/34f8f26371688c3c.sock")), "dev.splitgrid.lite.ptyd.34f8f26371688c3c");
    }
}
