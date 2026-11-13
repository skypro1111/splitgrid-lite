//! Saved passwords at rest: AES-256-GCM, stored as `aes:` + base64(iv ‖ tag ‖ ciphertext)
//! — the Electron app's format (main/connection-store.ts), with its key.
//!
//! Both apps share one data directory, so they share `credential-key.json`:
//! a random secret and salt (mode 0600), the key being scrypt(secret, salt)
//! with Node's defaults (main/fallback-key.ts). Not the macOS Keychain on
//! purpose: an unsigned binary gets a new code hash on every build, and the
//! Keychain would ask for access again each time. Very old saves were sealed
//! with sha256 of a few machine paths; those still open.

use std::fs;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const KEY_FILE: &str = "credential-key.json";

pub struct SecretBox {
    /// The current key first (it seals), then the legacy one (open only).
    keys: [[u8; 32]; 2],
}

/// credential-key.json as the Electron app writes it.
#[derive(Serialize, Deserialize)]
struct KeyFile {
    v: u8,
    salt: String,
    /// base64 of the secret; sealed by Electron's safeStorage when `wrapped`.
    secret: String,
    wrapped: bool,
}

impl SecretBox {
    /// Load the shared key from `dir`, creating it (in the Electron app's
    /// format) on first use. An existing key file is never rewritten: that
    /// would leave every saved secret unreadable.
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join(KEY_FILE);
        let key = if path.exists() {
            let file: KeyFile = serde_json::from_slice(&fs::read(&path)?).context("parse credential key")?;
            anyhow::ensure!(!file.wrapped, "SplitGrid's credential key is sealed in the Keychain, which this app can't open");
            derive(&B64.decode(file.secret)?, &B64.decode(file.salt)?)?
        } else {
            let (secret, salt): ([u8; 32], [u8; 16]) = (rand::random(), rand::random());
            let json = serde_json::to_vec(&KeyFile { v: 1, salt: B64.encode(salt), secret: B64.encode(secret), wrapped: false })?;
            crate::model::store::write_atomic(&path, &json)?;
            restrict(&path)?;
            derive(&secret, &salt)?
        };
        Ok(Self { keys: [key, legacy_key(dir)] })
    }

    pub fn encrypt(&self, plain: &str) -> String {
        encrypt_with(&self.keys[0], plain)
    }

    /// The plain text, or None when it isn't ours or was tampered with.
    pub fn decrypt(&self, stored: &str) -> Option<String> {
        decrypt_with(&self.keys, stored)
    }
}

/// Node's scryptSync defaults: N = 16384, r = 8, p = 1.
fn derive(secret: &[u8], salt: &[u8]) -> Result<[u8; 32]> {
    let params = scrypt::Params::new(14, 8, 1, 32).map_err(|e| anyhow!("{e}"))?;
    let mut key = [0u8; 32];
    scrypt::scrypt(secret, salt, &params, &mut key).map_err(|e| anyhow!("{e}"))?;
    Ok(key)
}

fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn encrypt_with(key: &[u8; 32], plain: &str) -> String {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let iv: [u8; 12] = rand::random();
    // aes-gcm appends the 16-byte tag; the stored layout puts it before the ciphertext.
    let sealed = cipher.encrypt(Nonce::from_slice(&iv), Payload { msg: plain.as_bytes(), aad: &[] }).expect("AES-GCM encryption");
    let (ct, tag) = sealed.split_at(sealed.len() - 16);
    let mut blob = Vec::with_capacity(12 + sealed.len());
    blob.extend_from_slice(&iv);
    blob.extend_from_slice(tag);
    blob.extend_from_slice(ct);
    format!("aes:{}", B64.encode(blob))
}

fn decrypt_with(keys: &[[u8; 32]], stored: &str) -> Option<String> {
    let blob = B64.decode(stored.strip_prefix("aes:")?).ok()?;
    if blob.len() < 28 {
        return None;
    }
    let (iv, rest) = blob.split_at(12);
    let (tag, ct) = rest.split_at(16);
    let mut sealed = ct.to_vec();
    sealed.extend_from_slice(tag);
    keys.iter().find_map(|key| {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
        let plain = cipher.decrypt(Nonce::from_slice(iv), Payload { msg: &sealed, aad: &[] }).ok()?;
        String::from_utf8(plain).ok()
    })
}

/// sha256("<userData>:<user>:<home>"): what the Electron app used before it had a key file.
fn legacy_key(dir: &Path) -> [u8; 32] {
    let user = std::env::var("USER").unwrap_or_default();
    let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("")).to_string_lossy().into_owned();
    let material = format!("{}:{user}:{home}", dir.to_string_lossy());
    Sha256::digest(material.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let sb = SecretBox::open(dir.path()).unwrap();
        let sealed = sb.encrypt("hunter2 ü");
        assert!(sealed.starts_with("aes:"));
        assert_eq!(sb.decrypt(&sealed).as_deref(), Some("hunter2 ü"));

        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(dir.path().join(KEY_FILE)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // Written in the Electron app's shape; the same key comes back on the next open.
        let file: KeyFile = serde_json::from_slice(&fs::read(dir.path().join(KEY_FILE)).unwrap()).unwrap();
        assert!(!file.wrapped);
        let again = SecretBox::open(dir.path()).unwrap();
        assert_eq!(again.decrypt(&sealed).as_deref(), Some("hunter2 ü"));
    }

    #[test]
    fn rejects_foreign_or_damaged_values() {
        let dir = tempfile::tempdir().unwrap();
        let sb = SecretBox::open(dir.path()).unwrap();
        let other = encrypt_with(&[7u8; 32], "x");
        assert!(sb.decrypt(&other).is_none());
        assert!(sb.decrypt("aes:AAAA").is_none());
        assert!(sb.decrypt("safe:whatever").is_none());
    }

    #[test]
    fn reads_the_electron_app_format() {
        // A key file as the Electron app writes it, and a value sealed with scrypt(secret, salt).
        let dir = tempfile::tempdir().unwrap();
        let (secret, salt) = ([3u8; 32], [9u8; 16]);
        fs::write(
            dir.path().join(KEY_FILE),
            format!(r#"{{"v":1,"salt":"{}","secret":"{}","wrapped":false}}"#, B64.encode(salt), B64.encode(secret)),
        )
        .unwrap();
        let params = scrypt::Params::new(14, 8, 1, 32).unwrap();
        let mut key = [0u8; 32];
        scrypt::scrypt(&secret, &salt, &params, &mut key).unwrap();
        let sealed = encrypt_with(&key, "from electron");

        let keys = SecretBox::open(dir.path()).unwrap();
        assert_eq!(keys.decrypt(&sealed).as_deref(), Some("from electron"));
        assert_eq!(keys.decrypt(&keys.encrypt("back")).as_deref(), Some("back"));
        // Old saves under the legacy key still open.
        let legacy = encrypt_with(&legacy_key(dir.path()), "old");
        assert_eq!(keys.decrypt(&legacy).as_deref(), Some("old"));

        // A Keychain-sealed key is refused, and the file left alone.
        let sealed_dir = tempfile::tempdir().unwrap();
        let raw = r#"{"v":1,"salt":"AA==","secret":"AA==","wrapped":true}"#;
        fs::write(sealed_dir.path().join(KEY_FILE), raw).unwrap();
        assert!(SecretBox::open(sealed_dir.path()).is_err());
        assert_eq!(fs::read_to_string(sealed_dir.path().join(KEY_FILE)).unwrap(), raw);
    }
}
