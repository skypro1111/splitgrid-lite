//! Saved SSH connections, their groups and pinned host keys — the Electron
//! app's files and shapes (saved-connections.json, connection-groups.json,
//! known-hosts.json), in the data directory both apps share.
//!
//! Secrets stay sealed (`aes:` values) in the file and are opened only when a
//! connection is made. The Electron app may change these files while this one
//! runs, so they are re-read when their mtime moves (`sync_from_disk`, polled,
//! and before every host-key write).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::store::write_atomic;
use crate::secrets::SecretBox;

const CONNECTIONS_FILE: &str = "saved-connections.json";
const GROUPS_FILE: &str = "connection-groups.json";
const KNOWN_HOSTS_FILE: &str = "known-hosts.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthMethod {
    Password,
    PrivateKey,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedConnection {
    pub id: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_method: AuthMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_passphrase: Option<String>,
    /// A dedicated sudo password (falls back to the login password).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_sudo_password: Option<String>,
    /// Offer to fill the saved password at sudo / password prompts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer_saved_password: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl SavedConnection {
    /// `user@host`, with `:port` when it isn't 22.
    pub fn address(&self) -> String {
        if self.port == 22 {
            format!("{}@{}", self.username, self.host)
        } else {
            format!("{}@{}:{}", self.username, self.host, self.port)
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConnectionGroups {
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub collapsed: Vec<String>,
}

/// What a host key check found (TOFU, OpenSSH `accept-new`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyCheck {
    Match,
    /// First time: pinned silently.
    New,
    /// A different key than the pinned one: refuse (possible man-in-the-middle).
    Mismatch { expected: String, actual: String },
}

pub struct ConnectionStore {
    dir: PathBuf,
    pub connections: Vec<SavedConnection>,
    pub groups: ConnectionGroups,
    known_hosts: BTreeMap<String, String>,
    secrets: SecretBox,
    /// The mtimes last read or written, per file name.
    seen: BTreeMap<&'static str, Option<SystemTime>>,
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_private(path: &Path, value: &impl Serialize) -> Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(value)?)?;
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

impl ConnectionStore {
    pub fn open(dir: &Path) -> Result<Self> {
        let mut store = Self {
            dir: dir.to_owned(),
            connections: Vec::new(),
            groups: ConnectionGroups::default(),
            known_hosts: BTreeMap::new(),
            secrets: SecretBox::open(dir)?,
            seen: BTreeMap::new(),
        };
        store.sync_from_disk();
        Ok(store)
    }

    /// Re-read whichever file changed on disk since it was last read or
    /// written (the other app saved it). True when anything was reloaded.
    pub fn sync_from_disk(&mut self) -> bool {
        let mut changed = false;
        for name in [CONNECTIONS_FILE, GROUPS_FILE, KNOWN_HOSTS_FILE] {
            let path = self.dir.join(name);
            let now = mtime(&path);
            if self.seen.get(name) == Some(&now) {
                continue;
            }
            self.seen.insert(name, now);
            match name {
                CONNECTIONS_FILE => self.connections = read_json(&path),
                GROUPS_FILE => self.groups = read_json(&path),
                _ => self.known_hosts = read_json(&path),
            }
            changed = true;
        }
        changed
    }

    fn write(&mut self, name: &'static str, value: &impl Serialize) -> Result<()> {
        let path = self.dir.join(name);
        write_private(&path, value)?;
        self.seen.insert(name, mtime(&path));
        Ok(())
    }

    pub fn save(&mut self) -> Result<()> {
        let (connections, groups) = (self.connections.clone(), self.groups.clone());
        self.write(CONNECTIONS_FILE, &connections)?;
        self.write(GROUPS_FILE, &groups)
    }

    pub fn get(&self, id: &str) -> Option<&SavedConnection> {
        self.connections.iter().find(|c| c.id == id)
    }

    pub fn password(&self, c: &SavedConnection) -> Option<String> {
        self.secrets.decrypt(c.encrypted_password.as_deref()?)
    }

    pub fn passphrase(&self, c: &SavedConnection) -> Option<String> {
        self.secrets.decrypt(c.encrypted_passphrase.as_deref()?)
    }

    /// The sudo password: the dedicated one, else the login password.
    pub fn sudo_password(&self, c: &SavedConnection) -> Option<String> {
        c.encrypted_sudo_password.as_deref().and_then(|s| self.secrets.decrypt(s)).or_else(|| self.password(c))
    }

    pub fn seal(&self, plain: &str) -> String {
        self.secrets.encrypt(plain)
    }

    // ── Host keys ──────────────────────────────────────────────────────────

    pub fn check_host_key(&mut self, host: &str, port: u16, fingerprint: &str) -> HostKeyCheck {
        self.sync_from_disk();
        let id = format!("{host}:{port}");
        match self.known_hosts.get(&id) {
            Some(fp) if fp == fingerprint => HostKeyCheck::Match,
            Some(fp) => HostKeyCheck::Mismatch { expected: fp.clone(), actual: fingerprint.to_owned() },
            None => {
                self.known_hosts.insert(id, fingerprint.to_owned());
                let hosts = self.known_hosts.clone();
                if let Err(e) = self.write(KNOWN_HOSTS_FILE, &hosts) {
                    log::error!("saving known hosts: {e:#}");
                }
                HostKeyCheck::New
            }
        }
    }

    /// Accept a changed host key (the user confirmed the server was rebuilt).
    pub fn forget_host_key(&mut self, host: &str, port: u16) -> Result<()> {
        self.sync_from_disk();
        self.known_hosts.remove(&format!("{host}:{port}"));
        let hosts = self.known_hosts.clone();
        self.write(KNOWN_HOSTS_FILE, &hosts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_keys_are_pinned_on_first_use() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(dir.path()).unwrap();
        assert_eq!(store.check_host_key("h", 22, "SHA256:a"), HostKeyCheck::New);
        assert_eq!(store.check_host_key("h", 22, "SHA256:a"), HostKeyCheck::Match);
        assert!(matches!(store.check_host_key("h", 22, "SHA256:b"), HostKeyCheck::Mismatch { .. }));
        assert_eq!(store.check_host_key("h", 2222, "SHA256:b"), HostKeyCheck::New, "keyed by host and port");
        // Survives a reopen.
        let mut again = ConnectionStore::open(dir.path()).unwrap();
        assert_eq!(again.check_host_key("h", 22, "SHA256:a"), HostKeyCheck::Match);
        again.forget_host_key("h", 22).unwrap();
        assert_eq!(again.check_host_key("h", 22, "SHA256:b"), HostKeyCheck::New);
    }

    #[test]
    fn picks_up_what_the_other_app_saved() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(dir.path()).unwrap();
        assert!(!store.sync_from_disk(), "nothing changed");
        // The Electron app saves a connection (an mtime clearly newer than ours).
        let json = r#"[{"id":"1","label":"Box","host":"h","port":22,"username":"u","authMethod":"password"}]"#;
        let path = dir.path().join(CONNECTIONS_FILE);
        fs::write(&path, json).unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        fs::File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
        assert!(store.sync_from_disk());
        assert_eq!(store.get("1").map(|c| c.label.as_str()), Some("Box"));
        // Our own save is not mistaken for theirs.
        store.connections[0].label = "Renamed".into();
        store.save().unwrap();
        assert!(!store.sync_from_disk());
        assert_eq!(store.connections[0].label, "Renamed");
    }

    #[test]
    fn address_and_round_trip() {
        let json = r#"[{"id":"1","label":"Box","host":"10.0.0.1","port":2222,"username":"root",
            "authMethod":"privateKey","privateKeyPath":"~/.ssh/id","group":"Prod","someFutureField":1}]"#;
        let list: Vec<SavedConnection> = serde_json::from_str(json).unwrap();
        assert_eq!(list[0].address(), "root@10.0.0.1:2222");
        assert_eq!(list[0].auth_method, AuthMethod::PrivateKey);
        let back = serde_json::to_value(&list).unwrap();
        assert_eq!(back[0]["someFutureField"], 1);
        assert_eq!(back[0]["authMethod"], "privateKey");
    }
}

#[cfg(test)]
mod real_data {
    //! The Electron app's real connections: every secret must open. Prints counts only.
    //! `cargo test -- --ignored real_splitgrid_connections`
    use super::*;
    use crate::model::store::app_support_dir;

    #[test]
    #[ignore]
    fn real_splitgrid_connections() {
        let from = app_support_dir("SplitGrid");
        let list: Vec<SavedConnection> = serde_json::from_slice(&fs::read(from.join(CONNECTIONS_FILE)).unwrap()).unwrap();
        let keys = SecretBox::open(&from).unwrap(); // reads the existing key, writes nothing
        let (mut ok, mut bad) = (0, 0);
        for c in &list {
            for v in [&c.encrypted_password, &c.encrypted_passphrase, &c.encrypted_sudo_password].into_iter().flatten() {
                if keys.decrypt(v).is_some() { ok += 1 } else { bad += 1 }
            }
        }
        println!("{} connections, {ok} secrets open, {bad} don't", list.len());
        assert_eq!(bad, 0);
    }
}
