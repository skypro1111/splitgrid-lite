//! Environments on disk, in SplitGrid's file layout:
//!   workspaces-state-<env>.json   one environment's WorkspaceState
//!   environment-names.json        { "<env>": "Display name" }
//!   native-settings.json          this app's own UI state
//!
//! The directory is the Electron app's own (`SplitGrid`): both apps read and
//! write the same files. `native-settings.json` is the only file just Lite uses.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::workspace::{WorkspaceState, now_ms};

const STATE_PREFIX: &str = "workspaces-state-";
const NAMES_FILE: &str = "environment-names.json";
const SETTINGS_FILE: &str = "native-settings.json";
const STATE_VERSION: u64 = 1;
const MAX_RECENT_WORKSPACES: usize = 30;
const APP_SETTINGS_FILE: &str = "app-settings.json";
pub const DEFAULT_ENV: &str = "default";

#[derive(Clone, Debug, PartialEq)]
pub struct EnvSummary {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NativeSettings {
    #[serde(default = "default_env")]
    pub active_environment: String,
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    #[serde(default)]
    pub sidebar_collapsed: bool,
    /// Whole-window UI zoom, as a Chromium zoom level (0 = 100%, ±0.5 a step; crate::ui).
    #[serde(default)]
    pub ui_zoom_level: f32,
}

fn default_env() -> String {
    DEFAULT_ENV.into()
}

/// The Electron app's default sidebar width.
fn default_sidebar_width() -> f32 {
    200.
}

impl Default for NativeSettings {
    fn default() -> Self {
        Self { active_environment: default_env(), sidebar_width: default_sidebar_width(), sidebar_collapsed: false, ui_zoom_level: 0. }
    }
}

#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
}

/// `~/Library/Application Support/<name>` on macOS.
pub fn app_support_dir(name: &str) -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from(".")).join("Library/Application Support").join(name)
}

/// Write via a temp file + rename, so a crash mid-write never leaves half a file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

impl Store {
    pub fn open(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(Self { dir })
    }

    fn state_path(&self, env: &str) -> PathBuf {
        self.dir.join(format!("{STATE_PREFIX}{}.json", sanitize(env)))
    }

    fn env_ids(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(&self.dir) else { return Vec::new() };
        let mut ids: Vec<String> = entries
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter_map(|n| n.strip_prefix(STATE_PREFIX)?.strip_suffix(".json").map(str::to_owned))
            .collect();
        ids.sort();
        ids
    }

    /// The data directory (shared with the Electron app).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn app_settings_path(&self) -> PathBuf {
        self.dir.join(APP_SETTINGS_FILE)
    }

    /// The Electron app's `app-settings.json` (shared: both apps read and write it).
    pub fn app_settings(&self) -> Map<String, Value> {
        fs::read(self.app_settings_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// Change some keys of `app-settings.json`, keeping every other one as it
    /// is: the file is re-read right before the write, so whatever the Electron
    /// app saved meanwhile stays. `null` removes a key (the Electron app's
    /// `undefined`, e.g. a shortcut reset to its default).
    pub fn patch_app_settings(&self, patch: &[(&str, Value)]) -> Result<()> {
        let mut all = self.app_settings();
        for (k, v) in patch {
            if v.is_null() {
                all.remove(*k);
            } else {
                all.insert((*k).to_owned(), v.clone());
            }
        }
        write_atomic(&self.app_settings_path(), &serde_json::to_vec_pretty(&all)?)
    }

    /// Workspaces closed in `env`, newest first (the "+ workspace → Open Recent" menu).
    pub fn recent_workspaces(&self, env: &str) -> Vec<super::workspace::Workspace> {
        fs::read(self.recent_path(env)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// Remember a closed workspace (newest first, at most 30, one entry per id).
    pub fn add_recent_workspace(&self, env: &str, ws: &super::workspace::Workspace) -> Result<()> {
        let mut list = self.recent_workspaces(env);
        list.retain(|w| w.id != ws.id);
        list.insert(0, ws.clone());
        list.truncate(MAX_RECENT_WORKSPACES);
        write_atomic(&self.recent_path(env), &serde_json::to_vec_pretty(&list)?)
    }

    fn recent_path(&self, env: &str) -> PathBuf {
        self.dir.join(format!("recent-workspaces-{}.json", sanitize(env)))
    }

    fn names(&self) -> BTreeMap<String, String> {
        fs::read(self.dir.join(NAMES_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice::<BTreeMap<String, Value>>(&b).ok())
            .map(|m| m.into_iter().filter_map(|(k, v)| Some((k, v.as_str()?.to_owned()))).collect())
            .unwrap_or_default()
    }

    fn write_names(&self, names: &BTreeMap<String, String>) -> Result<()> {
        write_atomic(&self.dir.join(NAMES_FILE), &serde_json::to_vec_pretty(names)?)
    }

    /// All environments, the default one first; it always exists.
    pub fn list_envs(&self) -> Vec<EnvSummary> {
        let names = self.names();
        let mut ids = self.env_ids();
        if !ids.iter().any(|i| i == DEFAULT_ENV) {
            ids.insert(0, DEFAULT_ENV.into());
        }
        ids.sort_by_key(|i| (i != DEFAULT_ENV, names.get(i).cloned().unwrap_or_else(|| i.clone()).to_lowercase()));
        ids.into_iter()
            .map(|id| {
                let fallback = if id == DEFAULT_ENV { "Default Environment".to_owned() } else { id.clone() };
                EnvSummary { name: names.get(&id).cloned().unwrap_or(fallback), id }
            })
            .collect()
    }

    pub fn load(&self, env: &str) -> Result<WorkspaceState> {
        let path = self.state_path(env);
        if !path.exists() {
            return Ok(WorkspaceState::default());
        }
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let mut state: WorkspaceState =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        state.normalize();
        Ok(state)
    }

    pub fn save(&self, env: &str, state: &WorkspaceState) -> Result<()> {
        let mut state = state.clone();
        state.extra.insert("version".into(), STATE_VERSION.into());
        state.extra.insert("savedAt".into(), now_ms().into());
        write_atomic(&self.state_path(env), &serde_json::to_vec_pretty(&state)?)
    }

    /// A new, empty environment; returns its id.
    pub fn create_env(&self, name: &str) -> Result<String> {
        let id = super::workspace::new_id("env");
        self.save(&id, &WorkspaceState::default())?;
        self.rename_env(&id, name)?;
        Ok(id)
    }

    pub fn rename_env(&self, env: &str, name: &str) -> Result<()> {
        let mut names = self.names();
        names.insert(env.to_owned(), name.trim().to_owned());
        self.write_names(&names)
    }

    pub fn delete_env(&self, env: &str) -> Result<()> {
        anyhow::ensure!(env != DEFAULT_ENV, "the default environment can't be deleted");
        let path = self.state_path(env);
        if path.exists() {
            fs::remove_file(path)?;
        }
        let mut names = self.names();
        if names.remove(env).is_some() {
            self.write_names(&names)?;
        }
        Ok(())
    }

    pub fn settings(&self) -> NativeSettings {
        fs::read(self.dir.join(SETTINGS_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save_settings(&self, s: &NativeSettings) -> Result<()> {
        write_atomic(&self.dir.join(SETTINGS_FILE), &serde_json::to_vec_pretty(s)?)
    }

    /// Persist the UI zoom level alone: read-modify-write, so the other fields
    /// another window saved stay as they are.
    pub fn save_ui_zoom_level(&self, level: f32) -> Result<()> {
        let mut s = self.settings();
        s.ui_zoom_level = level;
        self.save_settings(&s)
    }
}

/// Environment ids become file names: keep them to safe characters, like SplitGrid does.
fn sanitize(id: &str) -> String {
    let safe: String = id.trim().chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' }).collect();
    if safe.is_empty() { DEFAULT_ENV.into() } else { safe }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::workspace::Workspace;

    #[test]
    fn round_trip_and_environments() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("native")).unwrap();
        assert_eq!(store.list_envs(), [EnvSummary { id: "default".into(), name: "Default Environment".into() }]);

        let mut state = WorkspaceState::default();
        state.workspaces.push(Workspace::new("One", None));
        state.normalize();
        store.save(DEFAULT_ENV, &state).unwrap();
        let loaded = store.load(DEFAULT_ENV).unwrap();
        assert_eq!(loaded.workspaces[0].name, "One");
        assert_eq!(loaded.active_workspace_id, state.active_workspace_id);

        let id = store.create_env("Work").unwrap();
        let envs = store.list_envs();
        assert_eq!(envs.len(), 2);
        assert_eq!(envs[0].id, "default", "default stays first");
        assert_eq!(envs[1], EnvSummary { id: id.clone(), name: "Work".into() });

        store.rename_env(&id, "  Clients ").unwrap();
        assert_eq!(store.list_envs()[1].name, "Clients");
        assert!(store.delete_env(DEFAULT_ENV).is_err());
        store.delete_env(&id).unwrap();
        assert_eq!(store.list_envs().len(), 1);
    }

    #[test]
    fn app_settings_patches_keep_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().to_path_buf()).unwrap();
        fs::write(dir.path().join(APP_SETTINGS_FILE), r#"{"defaultSoundId":"bell","muteAll":true}"#).unwrap();
        store.patch_app_settings(&[("muteAll", false.into()), ("agentIntegrations", true.into())]).unwrap();
        let all = store.app_settings();
        assert_eq!(all["defaultSoundId"], "bell");
        assert_eq!(all["muteAll"], false);
        assert_eq!(all["agentIntegrations"], true);
    }

    #[test]
    fn app_settings_round_trip_unknown_keys_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().to_path_buf()).unwrap();
        let original = r#"{"zeta":{"nested":[1,2,{"x":null}]},"fastChat":{"apiKey":"k","models":["a"]},"defaultVolume":0.35,"focusModeHotkey":{"key":"p","meta":true},"alpha":"keep"}"#;
        fs::write(store.app_settings_path(), original).unwrap();
        store.patch_app_settings(&[("defaultVolume", 0.8.into()), ("focusModeHotkey", Value::Null), ("muteAll", true.into())]).unwrap();
        let raw: Value = serde_json::from_slice(&fs::read(store.app_settings_path()).unwrap()).unwrap();
        let keys: Vec<&String> = raw.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["zeta", "fastChat", "defaultVolume", "alpha", "muteAll"], "order kept, reset key gone, new key last");
        assert_eq!(raw["zeta"], serde_json::json!({"nested":[1,2,{"x":null}]}));
        assert_eq!(raw["fastChat"]["apiKey"], "k");
        assert_eq!(raw["defaultVolume"], 0.8);
        assert_eq!(raw["alpha"], "keep");
        // A missing file is created with just the patch.
        fs::remove_file(store.app_settings_path()).unwrap();
        store.patch_app_settings(&[("muteAll", false.into())]).unwrap();
        assert_eq!(store.app_settings().len(), 1);
    }

    #[test]
    fn recent_workspaces_newest_first_and_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().to_path_buf()).unwrap();
        let a = super::super::workspace::Workspace::new("a", Some("/p/a".into()));
        let b = super::super::workspace::Workspace::new("b", None);
        store.add_recent_workspace("env-x", &a).unwrap();
        store.add_recent_workspace("env-x", &b).unwrap();
        store.add_recent_workspace("env-x", &a).unwrap();
        let names: Vec<String> = store.recent_workspaces("env-x").into_iter().map(|w| w.name).collect();
        assert_eq!(names, ["a", "b"]);
        assert!(store.recent_workspaces("default").is_empty(), "per environment");
        assert!(dir.path().join("recent-workspaces-env-x.json").exists(), "the Electron app's file name");
    }

    #[test]
    fn settings_default_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().to_path_buf()).unwrap();
        assert_eq!(store.settings(), NativeSettings::default());
        let s = NativeSettings { active_environment: "env-x".into(), sidebar_width: 300., sidebar_collapsed: true, ui_zoom_level: 0. };
        store.save_settings(&s).unwrap();
        assert_eq!(store.settings(), s);
    }

    #[test]
    fn ui_zoom_level_persists_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().to_path_buf()).unwrap();
        // Older files have no zoom: 100%.
        fs::write(dir.path().join(SETTINGS_FILE), br#"{"activeEnvironment":"env-x","sidebarWidth":250}"#).unwrap();
        assert_eq!(store.settings().ui_zoom_level, 0.);
        store.save_ui_zoom_level(1.5).unwrap();
        let s = store.settings();
        assert_eq!(s.ui_zoom_level, 1.5);
        assert_eq!((s.active_environment.as_str(), s.sidebar_width), ("env-x", 250.), "the rest untouched");
    }
}

#[cfg(test)]
mod real_data {
    //! Parses the Electron app's real state files, read-only.
    //! `cargo test -- --ignored real_splitgrid_state`
    use super::*;

    #[test]
    #[ignore]
    fn real_splitgrid_state_parses() {
        let src = app_support_dir("SplitGrid");
        for entry in fs::read_dir(&src).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !(name.starts_with(STATE_PREFIX) && name.ends_with(".json")) || name == "workspaces-state.json" {
                continue;
            }
            let raw: Value = serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
            let mut state: WorkspaceState = serde_json::from_value(raw.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));
            let before: usize = state.workspaces.iter().map(|w| w.containers.len()).sum();
            state.normalize();
            let after: usize = state.workspaces.iter().map(|w| w.containers.len()).sum();
            // Unknown fields must survive a round trip.
            let back = serde_json::to_value(&state).unwrap();
            for key in raw.as_object().unwrap().keys() {
                assert!(back.get(key).is_some(), "{name}: lost top-level key {key}");
            }
            println!("{name}: {} workspaces, containers {before} -> {after} after normalize", state.workspaces.len());
        }
    }
}
