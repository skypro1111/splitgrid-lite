//! Saved SQL connections: the Electron app's `saved-sql-connections.json`
//! (main/sql-connection-store.ts, SavedSQLConnection), in the data directory
//! both apps share.
//!
//! Passwords are `aes:` blobs under the shared credential key, which open
//! here. The Electron app may also have sealed them with safeStorage
//! (`safe:`, the macOS Keychain) — those can't be opened by this binary, so
//! connecting asks for the password once and saves it back as `aes:`, which
//! both apps read. Unknown fields round-trip; the file is re-read when its
//! mtime moves (the other app saved it).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sql_core::{ConnectConfig, Dialect};

use crate::model::store::write_atomic;
use crate::secrets::SecretBox;

const FILE: &str = "saved-sql-connections.json";

fn default_dialect() -> String {
    // Files from before the field existed are Postgres (the Electron app's load()).
    "postgres".into()
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedSql {
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// The Electron app's dialect name; `mariadb` is kept as written and connects as MySQL.
    #[serde(default = "default_dialect")]
    pub dialect: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    /// For SQLite the Electron app may keep the file here instead of `filePath`.
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub ssl: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_password: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub fn dialect_from_name(name: &str) -> Option<Dialect> {
    match name {
        "postgres" => Some(Dialect::Postgres),
        "mysql" | "mariadb" => Some(Dialect::Mysql),
        "sqlite" => Some(Dialect::Sqlite),
        "mssql" => Some(Dialect::Mssql),
        _ => None,
    }
}

pub fn dialect_name(d: Dialect) -> &'static str {
    match d {
        Dialect::Postgres => "postgres",
        Dialect::Mysql => "mysql",
        Dialect::Sqlite => "sqlite",
        Dialect::Mssql => "mssql",
    }
}

impl SavedSql {
    pub fn dialect(&self) -> Option<Dialect> {
        dialect_from_name(&self.dialect)
    }

    /// The database file of a SQLite connection.
    pub fn file(&self) -> Option<&str> {
        self.file_path.as_deref().filter(|p| !p.is_empty()).or(Some(self.database.as_str()).filter(|d| !d.is_empty()))
    }

    /// What the explorer shows next to the label.
    pub fn address(&self) -> String {
        match self.dialect() {
            Some(Dialect::Sqlite) => self.file().map(crate::grid::tilde).unwrap_or_default(),
            _ => {
                let mut s = if self.user.is_empty() { self.host.clone() } else { format!("{}@{}", self.user, self.host) };
                if self.port != 0 && self.dialect().is_some_and(|d| d.default_port() != self.port) {
                    s.push_str(&format!(":{}", self.port));
                }
                if !self.database.is_empty() {
                    s.push_str(&format!("/{}", self.database));
                }
                s
            }
        }
    }

    pub fn config(&self, password: String) -> ConnectConfig {
        let dialect = self.dialect();
        let file = dialect.filter(|d| d.is_file()).and_then(|_| self.file()).map(PathBuf::from);
        ConnectConfig {
            dialect,
            host: self.host.clone(),
            port: if self.port == 0 { dialect.map(Dialect::default_port).unwrap_or(0) } else { self.port },
            user: self.user.clone(),
            password,
            database: self.database.clone(),
            ssl: self.ssl,
            file_path: file,
        }
    }
}

/// What a saved connection's password is, here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Password {
    /// None saved (SQLite, or a server that trusts the user).
    None,
    Known(String),
    /// Sealed where this app can't open it (`safe:`, a foreign key): ask the user.
    Ask,
}

pub struct SqlConnectionStore {
    path: PathBuf,
    pub connections: Vec<SavedSql>,
    secrets: Option<SecretBox>,
    seen: Option<SystemTime>,
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl SqlConnectionStore {
    pub fn open(dir: &Path) -> Self {
        let secrets = SecretBox::open(dir).inspect_err(|e| log::error!("SQL connections: credential key: {e:#}")).ok();
        let mut store = Self { path: dir.join(FILE), connections: Vec::new(), secrets, seen: None };
        store.reload();
        store
    }

    fn reload(&mut self) {
        self.seen = mtime(&self.path);
        // A damaged file reads as empty, like the Electron app's load(); it is
        // only rewritten when the user saves a connection.
        self.connections = fs::read(&self.path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    }

    /// Re-read the file when the other app saved it. True when it changed.
    pub fn sync_from_disk(&mut self) -> bool {
        if mtime(&self.path) == self.seen {
            return false;
        }
        self.reload();
        true
    }

    fn save(&mut self) -> Result<()> {
        write_atomic(&self.path, &serde_json::to_vec_pretty(&self.connections)?)?;
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        self.seen = mtime(&self.path);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&SavedSql> {
        self.connections.iter().find(|c| c.id == id)
    }

    pub fn password(&self, c: &SavedSql) -> Password {
        match c.encrypted_password.as_deref() {
            None | Some("") => Password::None,
            Some(sealed) => match self.secrets.as_ref().and_then(|s| s.decrypt(sealed)) {
                Some(p) => Password::Known(p),
                None => Password::Ask,
            },
        }
    }

    /// Add or replace a connection (by id). `password`: Some to set (empty
    /// clears it), None to keep what is stored.
    pub fn upsert(&mut self, mut conn: SavedSql, password: Option<&str>) -> Result<()> {
        self.sync_from_disk();
        let old = self.connections.iter().position(|c| c.id == conn.id);
        conn.encrypted_password = match password {
            Some("") => None,
            Some(p) => Some(self.seal(p)?),
            None => old.and_then(|i| self.connections[i].encrypted_password.clone()),
        };
        match old {
            Some(i) => {
                // Fields only the other app knows survive an edit made here.
                let mut extra = std::mem::take(&mut self.connections[i].extra);
                extra.extend(std::mem::take(&mut conn.extra));
                conn.extra = extra;
                self.connections[i] = conn;
            }
            None => self.connections.push(conn),
        }
        self.save()
    }

    /// Store a password the user typed for a connection whose saved one couldn't be opened.
    pub fn set_password(&mut self, id: &str, password: &str) -> Result<()> {
        self.sync_from_disk();
        let sealed = self.seal(password)?;
        let Some(c) = self.connections.iter_mut().find(|c| c.id == id) else { anyhow::bail!("saved SQL connection not found") };
        c.encrypted_password = Some(sealed);
        self.save()
    }

    /// A copy of a connection under `new_id`, placed after it: label "… copy",
    /// the sealed password and the other app's fields carried over as they are.
    pub fn duplicate(&mut self, id: &str, new_id: &str) -> Result<SavedSql> {
        self.sync_from_disk();
        let Some(i) = self.connections.iter().position(|c| c.id == id) else { anyhow::bail!("saved SQL connection not found") };
        let mut copy = self.connections[i].clone();
        copy.id = new_id.to_owned();
        copy.label = format!("{} copy", copy.label);
        self.connections.insert(i + 1, copy.clone());
        self.save()?;
        Ok(copy)
    }

    pub fn delete(&mut self, id: &str) -> Result<()> {
        self.sync_from_disk();
        self.connections.retain(|c| c.id != id);
        self.save()
    }

    fn seal(&self, plain: &str) -> Result<String> {
        match &self.secrets {
            Some(s) => Ok(s.encrypt(plain)),
            None => anyhow::bail!("the credential key can't be opened, so passwords can't be saved"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(id: &str, dialect: &str) -> SavedSql {
        SavedSql { id: id.into(), label: id.into(), dialect: dialect.into(), host: "db.local".into(), port: 5432, user: "app".into(), ..Default::default() }
    }

    #[test]
    fn reads_the_electron_shape_and_keeps_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"[
            {"id":"a","label":"Old","host":"h","port":5432,"user":"u","database":"d","ssl":true,"someFutureField":{"x":1}},
            {"id":"b","label":"Lite","dialect":"sqlite","host":"","port":0,"user":"","database":"/tmp/x.db","ssl":false},
            {"id":"c","label":"Maria","dialect":"mariadb","host":"m","port":3307,"user":"r","database":"","ssl":false}
        ]"#;
        fs::write(dir.path().join(FILE), json).unwrap();
        let mut store = SqlConnectionStore::open(dir.path());
        assert_eq!(store.connections.len(), 3);
        let a = store.get("a").unwrap();
        assert_eq!(a.dialect(), Some(Dialect::Postgres), "a missing dialect is Postgres");
        let b = store.get("b").unwrap();
        assert_eq!(b.config(String::new()).file_path, Some(PathBuf::from("/tmp/x.db")), "SQLite's file may be in `database`");
        let c = store.get("c").unwrap().clone();
        assert_eq!(c.dialect(), Some(Dialect::Mysql));
        assert_eq!(c.address(), "r@m:3307");

        // An edit keeps the unknown field and the dialect name as the other app wrote it.
        let mut edited = store.get("a").unwrap().clone();
        edited.label = "New".into();
        edited.extra.clear();
        store.upsert(edited, None).unwrap();
        store.upsert(c, None).unwrap();
        let back: Value = serde_json::from_slice(&fs::read(dir.path().join(FILE)).unwrap()).unwrap();
        assert_eq!(back[0]["label"], "New");
        assert_eq!(back[0]["someFutureField"]["x"], 1);
        assert_eq!(back[2]["dialect"], "mariadb");
        assert!(back[1].get("filePath").is_none(), "absent optional fields stay absent");
    }

    #[test]
    fn aes_passwords_round_trip_and_safe_ones_ask() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SqlConnectionStore::open(dir.path());
        store.upsert(conn("a", "postgres"), Some("s3cret")).unwrap();
        let sealed = store.get("a").unwrap().encrypted_password.clone().unwrap();
        assert!(sealed.starts_with("aes:"));
        assert!(!sealed.contains("s3cret"));
        assert_eq!(store.password(store.get("a").unwrap()), Password::Known("s3cret".into()));

        // Another process (the Electron app) opens the same files with the same key.
        let again = SqlConnectionStore::open(dir.path());
        assert_eq!(again.password(again.get("a").unwrap()), Password::Known("s3cret".into()));

        // Keychain-sealed: ask, then save it back as aes:.
        let mut safe = conn("s", "mysql");
        safe.encrypted_password = Some("safe:AAAA".into());
        store.connections.push(safe);
        assert_eq!(store.password(store.get("s").unwrap()), Password::Ask);
        store.set_password("s", "typed").unwrap();
        assert_eq!(store.password(store.get("s").unwrap()), Password::Known("typed".into()));

        // None saved; keeping, then clearing.
        store.upsert(conn("n", "postgres"), None).unwrap();
        assert_eq!(store.password(store.get("n").unwrap()), Password::None);
        store.upsert(conn("a", "postgres"), None).unwrap();
        assert_eq!(store.password(store.get("a").unwrap()), Password::Known("s3cret".into()), "None keeps the stored password");
        store.upsert(conn("a", "postgres"), Some("")).unwrap();
        assert_eq!(store.password(store.get("a").unwrap()), Password::None);
    }

    #[test]
    fn follows_the_other_apps_saves_and_writes_privately() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SqlConnectionStore::open(dir.path());
        store.upsert(conn("a", "postgres"), None).unwrap();
        assert!(!store.sync_from_disk(), "our own save is not news");
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(fs::metadata(dir.path().join(FILE)).unwrap().permissions().mode() & 0o777, 0o600);

        let path = dir.path().join(FILE);
        fs::write(&path, r#"[{"id":"z","label":"Z","dialect":"mssql","host":"h","port":1433,"user":"sa","database":"","ssl":false}]"#).unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        fs::File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
        assert!(store.sync_from_disk());
        assert_eq!(store.connections.len(), 1);
        assert_eq!(store.get("z").unwrap().dialect(), Some(Dialect::Mssql));
        let copy = store.duplicate("z", "z2").unwrap();
        assert_eq!((copy.label.as_str(), store.connections[1].id.as_str()), ("Z copy", "z2"));
        assert_eq!(store.get("z2").unwrap().host, "h");
        store.delete("z").unwrap();
        store.delete("z2").unwrap();
        assert!(store.connections.is_empty());
    }
}
