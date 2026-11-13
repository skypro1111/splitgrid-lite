//! SFTP over a saved connection (russh-sftp on the SSH runtime): what the
//! SFTP pane and, later, the sg-sftp CLI and workspace sync need — list,
//! stat, mkdir, rename, delete (recursively), transfer with progress, and
//! whole-file read/write for the remote file editor.
//!
//! A client is one SSH connection with one SFTP channel; panes keep theirs
//! open while they live. Every call runs on the SSH runtime; `call` awaits it
//! from gpui's executor.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow};
use russh::client::Handle;
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::ssh::{self, Client, HostKeyChecker, SshParams};

/// Copy buffer for transfers.
const CHUNK: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    /// Seconds since the epoch.
    pub modified: Option<u64>,
    /// Unix mode bits.
    pub mode: Option<u32>,
}

pub struct SftpClient {
    sftp: SftpSession,
    ssh: Handle<Client>,
    /// Remote folders already made (or found) on this connection, so `ensure_dir_all` skips them.
    known_dirs: std::sync::Mutex<std::collections::HashSet<String>>,
}

/// Run an SFTP future on the SSH runtime and await it from anywhere.
pub async fn call<T: Send + 'static>(fut: impl Future<Output = Result<T>> + Send + 'static) -> Result<T> {
    ssh::runtime().spawn(fut).await.map_err(|e| anyhow!("SFTP task failed: {e}"))?
}

/// Join a remote directory and a name with exactly one slash.
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

/// The parent of a remote path ("/" stays "/").
pub fn parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => trimmed[..i].to_owned(),
    }
}

impl SftpClient {
    pub async fn open(params: SshParams, check: HostKeyChecker) -> Result<Arc<Self>> {
        let ssh = ssh::connect(&params, check).await?;
        let channel = ssh.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let sftp = SftpSession::new(channel.into_stream()).await.context("start SFTP")?;
        sftp.set_timeout(30);
        Ok(Arc::new(Self { sftp, ssh, known_dirs: Default::default() }))
    }

    /// The login directory.
    pub async fn home(&self) -> Result<String> {
        Ok(self.sftp.canonicalize(".").await?)
    }

    /// Directory entries, folders first, then by name (case-insensitive); no `.`/`..`.
    pub async fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let mut out: Vec<Entry> = self
            .sftp
            .read_dir(dir)
            .await
            .with_context(|| format!("list {dir}"))?
            .filter(|e| e.file_name() != "." && e.file_name() != "..")
            .map(|e| {
                let m = e.metadata();
                Entry {
                    name: e.file_name(),
                    is_dir: m.is_dir(),
                    is_symlink: m.is_symlink(),
                    size: m.size.unwrap_or(0),
                    modified: m.mtime.map(u64::from),
                    mode: m.permissions,
                }
            })
            .collect();
        sort_entries(&mut out);
        Ok(out)
    }

    /// The SSH transport under this session is gone (reconnect before reuse).
    pub fn is_closed(&self) -> bool {
        self.ssh.is_closed()
    }

    /// One entry's attributes (following symlinks).
    pub async fn stat(&self, path: &str) -> Result<Entry> {
        let m = self.sftp.metadata(path).await.with_context(|| format!("stat {path}"))?;
        let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_owned();
        Ok(Entry { name, is_dir: m.is_dir(), is_symlink: m.is_symlink(), size: m.size.unwrap_or(0), modified: m.mtime.map(u64::from), mode: m.permissions })
    }

    /// `mkdir -p`: create every missing folder of `dir`, ignoring "already exists"
    /// (the Electron app's ensureRemoteDir). Remembered per connection.
    pub async fn ensure_dir_all(&self, dir: &str) {
        let mut current = String::new();
        for part in dir.split('/').filter(|p| !p.is_empty()) {
            current = if current.is_empty() && dir.starts_with('/') { format!("/{part}") } else if current.is_empty() { part.to_owned() } else { format!("{current}/{part}") };
            if self.known_dirs.lock().unwrap().contains(&current) {
                continue;
            }
            let _ = self.sftp.create_dir(current.as_str()).await;
            self.known_dirs.lock().unwrap().insert(current.clone());
        }
    }

    /// `remove`, but a path that is already gone is not an error.
    pub async fn remove_if_exists(&self, path: &str) -> Result<()> {
        if self.sftp.symlink_metadata(path).await.is_err() {
            return Ok(());
        }
        self.remove(path).await
    }

    pub async fn is_dir(&self, path: &str) -> Result<bool> {
        Ok(self.sftp.metadata(path).await?.is_dir())
    }

    pub async fn mkdir(&self, path: &str) -> Result<()> {
        Ok(self.sftp.create_dir(path).await?)
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        Ok(self.sftp.rename(from, to).await?)
    }

    /// Delete a file, or a directory with everything in it.
    pub async fn remove(&self, path: &str) -> Result<()> {
        let meta = self.sftp.symlink_metadata(path).await?;
        if meta.is_dir() && !meta.is_symlink() {
            for e in self.list(path).await? {
                Box::pin(self.remove(&join(path, &e.name))).await?;
            }
            self.sftp.remove_dir(path).await?;
        } else {
            self.sftp.remove_file(path).await?;
        }
        Ok(())
    }

    /// Whole-file read/write, for the remote file editor.
    pub async fn read(&self, path: &str) -> Result<Vec<u8>> {
        Ok(self.sftp.read(path).await?)
    }

    pub async fn write(&self, path: &str, bytes: &[u8]) -> Result<()> {
        Ok(self.sftp.write(path, bytes).await?)
    }

    /// Copy a remote file or directory to `local`; `progress(bytes)` after each chunk.
    pub async fn download(&self, remote: &str, local: &Path, progress: &(dyn Fn(u64) + Send + Sync)) -> Result<()> {
        if self.is_dir(remote).await? {
            tokio::fs::create_dir_all(local).await?;
            for e in self.list(remote).await? {
                Box::pin(self.download(&join(remote, &e.name), &local.join(&e.name), progress)).await?;
            }
            return Ok(());
        }
        let mut src = self.sftp.open(remote).await.with_context(|| format!("open {remote}"))?;
        let mut dst = tokio::fs::File::create(local).await.with_context(|| format!("create {}", local.display()))?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = src.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            dst.write_all(&buf[..n]).await?;
            progress(n as u64);
        }
        dst.flush().await?;
        Ok(())
    }

    /// Copy a local file or directory to `remote`; `progress(bytes)` after each chunk.
    pub async fn upload(&self, local: &Path, remote: &str, progress: &(dyn Fn(u64) + Send + Sync)) -> Result<()> {
        if local.is_dir() {
            if self.sftp.metadata(remote).await.is_err() {
                self.sftp.create_dir(remote).await.with_context(|| format!("mkdir {remote}"))?;
            }
            let mut entries = tokio::fs::read_dir(local).await?;
            while let Some(e) = entries.next_entry().await? {
                let name = e.file_name().to_string_lossy().into_owned();
                Box::pin(self.upload(&e.path(), &join(remote, &name), progress)).await?;
            }
            return Ok(());
        }
        let mut src = tokio::fs::File::open(local).await.with_context(|| format!("open {}", local.display()))?;
        let mut dst = self.sftp.create(remote).await.with_context(|| format!("create {remote}"))?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = src.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            dst.write_all(&buf[..n]).await?;
            progress(n as u64);
        }
        dst.shutdown().await?;
        Ok(())
    }
}

/// Where a batch transfer is.
#[derive(Clone, Debug)]
pub struct Progress {
    pub current: usize,
    pub total: usize,
    /// The file starting now (remote path for uploads, local for downloads).
    pub file: String,
}

#[derive(Debug, Default)]
pub struct TransferResult {
    pub transferred: usize,
    pub total: usize,
    pub errors: Vec<String>,
    pub cancelled: bool,
}

/// A name a server sent that must not become a local path component.
fn unsafe_name(name: &str) -> bool {
    name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') || name.contains('\0')
}

impl SftpClient {
    /// Upload local files and folders into `remote_dir` (a folder keeps its name), one file at a time.
    /// Cancelling stops before the next file.
    pub async fn upload_many(
        &self,
        locals: Vec<PathBuf>,
        remote_dir: String,
        on: &(dyn Fn(Progress) + Send + Sync),
        cancel: &std::sync::atomic::AtomicBool,
    ) -> TransferResult {
        // Flatten: (local file, remote path), creating folders as they come.
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        fn walk(local: &Path, remote: String, dirs: &mut Vec<String>, files: &mut Vec<(PathBuf, String)>) {
            if local.is_dir() {
                dirs.push(remote.clone());
                if let Ok(rd) = std::fs::read_dir(local) {
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().into_owned();
                        walk(&e.path(), join(&remote, &name), dirs, files);
                    }
                }
            } else {
                files.push((local.to_owned(), remote));
            }
        }
        for l in &locals {
            let name = l.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            walk(l, join(&remote_dir, &name), &mut dirs, &mut files);
        }
        let mut result = TransferResult { total: files.len(), ..Default::default() };
        for d in dirs {
            if self.sftp.metadata(&d).await.is_err()
                && let Err(e) = self.sftp.create_dir(&d).await
            {
                result.errors.push(format!("{d}: {e}"));
            }
        }
        for (i, (local, remote)) in files.into_iter().enumerate() {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                result.cancelled = true;
                break;
            }
            on(Progress { current: i + 1, total: result.total, file: remote.clone() });
            match self.upload(&local, &remote, &|_| {}).await {
                Ok(()) => result.transferred += 1,
                Err(e) => result.errors.push(format!("{remote}: {e:#}")),
            }
        }
        result
    }

    /// Download remote files and folders into `local_dir`; names that could
    /// escape it ("..", slashes) are skipped.
    pub async fn download_many(
        &self,
        remotes: Vec<(String, bool)>,
        local_dir: PathBuf,
        on: &(dyn Fn(Progress) + Send + Sync),
        cancel: &std::sync::atomic::AtomicBool,
    ) -> TransferResult {
        let mut result = TransferResult::default();
        let mut files: Vec<(String, PathBuf)> = Vec::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut stack: Vec<(String, PathBuf)> = Vec::new();
        for (path, is_dir) in remotes {
            let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_owned();
            if unsafe_name(&name) {
                result.errors.push(format!("{path}: unsafe name, skipped"));
                continue;
            }
            let local = local_dir.join(&name);
            if is_dir { stack.push((path, local)) } else { files.push((path, local)) }
        }
        while let Some((remote, local)) = stack.pop() {
            dirs.push(local.clone());
            match self.list(&remote).await {
                Ok(entries) => {
                    for e in entries {
                        if unsafe_name(&e.name) {
                            result.errors.push(format!("{}: unsafe name, skipped", join(&remote, &e.name)));
                            continue;
                        }
                        let (r, l) = (join(&remote, &e.name), local.join(&e.name));
                        if e.is_dir { stack.push((r, l)) } else { files.push((r, l)) }
                    }
                }
                Err(e) => result.errors.push(format!("{remote}: {e:#}")),
            }
        }
        result.total = files.len();
        for d in dirs {
            if let Err(e) = tokio::fs::create_dir_all(&d).await {
                result.errors.push(format!("{}: {e}", d.display()));
            }
        }
        for (i, (remote, local)) in files.into_iter().enumerate() {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                result.cancelled = true;
                break;
            }
            if !local.starts_with(&local_dir) {
                result.errors.push(format!("{remote}: target escapes download dir, skipped"));
                continue;
            }
            on(Progress { current: i + 1, total: result.total, file: local.to_string_lossy().into_owned() });
            match self.download(&remote, &local, &|_| {}).await {
                Ok(()) => result.transferred += 1,
                Err(e) => result.errors.push(format!("{remote}: {e:#}")),
            }
        }
        result
    }
}

/// Folders first, then by name, case-insensitively — as both file tables show them.
pub fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
}

/// A local directory listed the same way.
pub fn list_local(dir: &Path) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).with_context(|| format!("list {}", dir.display()))? {
        let e = e?;
        let link = e.file_type()?.is_symlink();
        let meta = std::fs::metadata(e.path()).or_else(|_| e.metadata())?;
        use std::os::unix::fs::PermissionsExt as _;
        out.push(Entry {
            name: e.file_name().to_string_lossy().into_owned(),
            is_dir: meta.is_dir(),
            is_symlink: link,
            size: meta.len(),
            modified: meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d: Duration| d.as_secs()),
            mode: Some(meta.permissions().mode()),
        });
    }
    sort_entries(&mut out);
    Ok(out)
}

pub fn local_home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths() {
        assert_eq!(join("/home/me", "a"), "/home/me/a");
        assert_eq!(join("/", "a"), "/a");
        assert_eq!(parent("/home/me/a"), "/home/me");
        assert_eq!(parent("/home"), "/");
        assert_eq!(parent("/"), "/");
        assert_eq!(parent("/home/me/"), "/home");
    }

    #[test]
    fn unsafe_names_are_refused() {
        for n in ["", ".", "..", "a/b", "a\\b", "x\0y"] {
            assert!(unsafe_name(n), "{n:?}");
        }
        assert!(!unsafe_name("report.pdf"));
    }

    #[test]
    fn folders_first_then_names() {
        let e = |name: &str, is_dir| Entry { name: name.into(), is_dir, is_symlink: false, size: 0, modified: None, mode: None };
        let mut v = vec![e("b.txt", false), e("Zdir", true), e("a.txt", false), e("adir", true)];
        sort_entries(&mut v);
        assert_eq!(v.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["adir", "Zdir", "a.txt", "b.txt"]);
    }

    #[test]
    fn lists_a_local_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("f.txt"), b"hello").unwrap();
        let list = list_local(dir.path()).unwrap();
        assert_eq!(list[0].name, "sub");
        assert!(list[0].is_dir);
        assert_eq!((list[1].name.as_str(), list[1].size), ("f.txt", 5));
    }
}
