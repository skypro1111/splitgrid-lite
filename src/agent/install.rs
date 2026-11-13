//! The agent artifacts Agent integrations put on the machine (the Electron
//! app's agent-hooks/installer, agent-skills/installer and agent-integrations):
//!   • lifecycle hooks merged into `~/.claude/settings.json` and `~/.codex/hooks.json`,
//!   • the sg-* skills in `~/.claude/skills/<name>/SKILL.md`.
//!
//! Same files, markers and semantics as Electron: nothing is installed unless
//! `agentIntegrations` is on; on startup it only installs (never uninstalls);
//! toggling the switch off removes our hooks and skills; a sub-switch off
//! removes its skill. Only our own entries are touched, everything else in the
//! configs is kept as it is.
//!
//! Living beside the Electron app, which writes the same entries with the
//! absolute path of ITS helper and rewrites them on every launch:
//!   • hooks: a config whose SplitGrid entries call a helper that exists and is
//!     not ours is left alone (the Electron app owns it; its helper speaks the
//!     same protocol and finds whichever app runs through `$SPLITGRID_HOOK_ENDPOINT`).
//!     Ours is written only where there are none, or they point at a helper
//!     that is gone (an uninstalled or moved app).
//!   • skills: identical content is left as is; content we didn't write (the
//!     Electron app's copy, whatever its version) is left alone. What we write
//!     is remembered by hash in `lite-agent-skills.json` in the data dir, so a
//!     later Lite build refreshes its own copies only.
//!
//! Every path derives from `$HOME` (or `$CLAUDE_CONFIG_DIR` / `$CODEX_HOME`,
//! like the agents themselves) and the data dir.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context as _, Result, bail};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

/// Recognises SplitGrid's hook entries by the helper's file name in the command.
const HELPER_MARKER: &str = "splitgrid-hook";
/// The marker of the app's previous product name (removed on startup).
const LEGACY_HELPER_MARKER: &str = "swapit-hook";
/// Skill dirs shipped under previous names.
const LEGACY_SKILLS: [&str; 6] = ["swapit-browser", "swapit-terminal", "swapit-sql", "splitgrid-browser", "splitgrid-terminal", "splitgrid-sql"];
const RECORD_FILE: &str = "lite-agent-skills.json";

struct EventMap {
    agent_event: &'static str,
    splitgrid_event: &'static str,
    matcher: bool,
}

const fn ev(agent_event: &'static str, splitgrid_event: &'static str, matcher: bool) -> EventMap {
    EventMap { agent_event, splitgrid_event, matcher }
}

pub struct HookDef {
    pub id: &'static str,
    config_dir: &'static str,
    config_dir_env: &'static str,
    config_file: &'static str,
    hooks_key: &'static str,
    timeout_sec: u64,
    events: &'static [EventMap],
}

/// Claude Code (agent-hooks/registry.ts CLAUDE).
pub const CLAUDE: HookDef = HookDef {
    id: "claude",
    config_dir: ".claude",
    config_dir_env: "CLAUDE_CONFIG_DIR",
    config_file: "settings.json",
    hooks_key: "hooks",
    timeout_sec: 5,
    events: &[
        ev("UserPromptSubmit", "prompt-submit", false),
        ev("PreToolUse", "tool", true),
        ev("PostToolUse", "tool", true),
        ev("Stop", "stop", false),
        ev("StopFailure", "stop", false),
        ev("Notification", "notification", false),
        ev("SessionStart", "session-start", false),
        ev("SessionEnd", "session-end", false),
    ],
};

/// Codex (agent-hooks/registry.ts CODEX).
pub const CODEX: HookDef = HookDef {
    id: "codex",
    config_dir: ".codex",
    config_dir_env: "CODEX_HOME",
    config_file: "hooks.json",
    hooks_key: "hooks",
    timeout_sec: 5,
    events: &[ev("UserPromptSubmit", "prompt-submit", false), ev("Stop", "stop", false), ev("PermissionRequest", "notification", true)],
};

pub const HOOK_DEFS: [&HookDef; 2] = [&CLAUDE, &CODEX];

/// Where things are: the user's home (and the agents' own overrides).
#[derive(Clone, Debug)]
pub struct Env {
    pub home: PathBuf,
    pub claude_config_dir: Option<String>,
    pub codex_home: Option<String>,
}

impl Env {
    pub fn from_process() -> Self {
        Self {
            home: std::env::home_dir().unwrap_or_else(|| PathBuf::from(".")),
            claude_config_dir: std::env::var("CLAUDE_CONFIG_DIR").ok(),
            codex_home: std::env::var("CODEX_HOME").ok(),
        }
    }

    fn config_dir(&self, def: &HookDef) -> PathBuf {
        let over = match def.config_dir_env {
            "CLAUDE_CONFIG_DIR" => self.claude_config_dir.as_deref(),
            "CODEX_HOME" => self.codex_home.as_deref(),
            _ => None,
        };
        match over.map(str::trim) {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => self.home.join(def.config_dir),
        }
    }

    fn claude_dir(&self) -> PathBuf {
        self.config_dir(&CLAUDE)
    }
}

/// Which artifacts the settings ask for (sub-switches only count under the master).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wanted {
    pub integrations: bool,
    pub terminal: bool,
    pub long_running_in_panes: bool,
    pub sql: bool,
    pub sftp: bool,
    pub history: bool,
}

impl Wanted {
    pub fn from_settings(s: &Map<String, Value>) -> Self {
        let on = |k: &str| s.get(k).and_then(Value::as_bool).unwrap_or(false);
        let integrations = on("agentIntegrations");
        Self {
            integrations,
            terminal: integrations && on("agentTerminalControl"),
            long_running_in_panes: on("agentLongRunningInPanes"),
            sql: integrations && on("agentSqlControl"),
            sftp: integrations && on("agentSftpControl"),
            history: integrations && on("historyEnabled"),
        }
    }
}

/// The settings keys whose change re-runs the installer (ipc-handlers' app-settings:save).
pub const SWITCH_KEYS: [&str; 7] =
    ["agentIntegrations", "agentTerminalControl", "agentLongRunningInPanes", "agentSqlControl", "agentSqlWrite", "agentSftpControl", "agentSftpWrite"];

pub struct Installer {
    pub env: Env,
    /// Our `splitgrid-hook.sh`.
    pub helper: PathBuf,
    /// The bundled skills: `<dir>/<name>/SKILL.md`.
    pub skills_src: PathBuf,
    /// Our data dir (the skills record lives there).
    pub data_dir: PathBuf,
}

/// One installer run at a time (startup's thread and a Settings toggle).
static RUN: Mutex<()> = Mutex::new(());

impl Installer {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            env: Env::from_process(),
            // Canonical, as the Electron app writes its own (a dev build's path has `..`).
            helper: super::helpers_dir().join("splitgrid-hook.sh").canonicalize().unwrap_or_else(|_| super::helpers_dir().join("splitgrid-hook.sh")),
            skills_src: skills_dir(),
            data_dir: data_dir.to_path_buf(),
        }
    }

    fn settings(&self) -> Map<String, Value> {
        fs::read(self.data_dir.join("app-settings.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// On launch: drop the previous product name's leftovers, then install what
    /// the settings ask for. Never uninstalls (launch doesn't change the user's
    /// config on its own).
    pub fn startup(&self) {
        let _guard = RUN.lock().unwrap_or_else(|e| e.into_inner());
        self.migrate_legacy();
        let wanted = Wanted::from_settings(&self.settings());
        if wanted.integrations {
            self.sync(wanted);
        }
    }

    /// After a switch changed in Settings: install, or remove everything when off.
    pub fn apply(&self) {
        let _guard = RUN.lock().unwrap_or_else(|e| e.into_inner());
        let wanted = Wanted::from_settings(&self.settings());
        if wanted.integrations { self.sync(wanted) } else { self.uninstall() }
    }

    pub fn sync(&self, wanted: Wanted) {
        for def in HOOK_DEFS {
            if !self.env.config_dir(def).exists() {
                continue; // that agent isn't on this machine
            }
            match self.sync_hooks(def) {
                Ok(HookOutcome::Written) => log::info!("agent hooks: installed for {}", def.id),
                Ok(HookOutcome::Unchanged) => {}
                Ok(HookOutcome::Foreign(p)) => log::info!("agent hooks: {} keeps SplitGrid's entries ({})", def.id, p.display()),
                Err(e) => log::error!("agent hooks: installing for {}: {e:#}", def.id),
            }
        }
        self.sync_skills(wanted);
    }

    pub fn uninstall(&self) {
        for def in HOOK_DEFS {
            if let Err(e) = remove_hooks_in_file(&self.config_file(def), HELPER_MARKER) {
                log::error!("agent hooks: removing for {}: {e:#}", def.id);
            }
        }
        let claude = self.env.claude_dir();
        let mut record = self.record();
        for name in SKILLS.iter().map(|s| s.name) {
            remove_skill(&claude, name);
            record.remove(name);
        }
        self.save_record(&record);
    }

    fn migrate_legacy(&self) {
        let claude = self.env.claude_dir();
        if claude.exists() {
            for name in LEGACY_SKILLS {
                remove_skill(&claude, name);
            }
        }
        for def in HOOK_DEFS {
            if let Err(e) = remove_hooks_in_file(&self.config_file(def), LEGACY_HELPER_MARKER) {
                log::warn!("agent hooks: legacy cleanup for {}: {e:#}", def.id);
            }
        }
    }

    fn config_file(&self, def: &HookDef) -> PathBuf {
        self.env.config_dir(def).join(def.config_file)
    }

    pub fn sync_hooks(&self, def: &HookDef) -> Result<HookOutcome> {
        let file = self.config_file(def);
        let (mut root, before) = read_config(&file)?;
        if let Some(helper) = foreign_helper(&root, def, &self.helper) {
            return Ok(HookOutcome::Foreign(helper));
        }
        apply_hooks(&mut root, def, &self.helper)?;
        if serde_json::to_string(&root)? == before {
            return Ok(HookOutcome::Unchanged);
        }
        write_config(&file, &root)?;
        Ok(HookOutcome::Written)
    }

    // ── Skills ──────────────────────────────────────────────────────────────

    fn sync_skills(&self, wanted: Wanted) {
        let claude = self.env.claude_dir();
        if !claude.exists() {
            return; // Claude Code isn't on this machine
        }
        for name in LEGACY_SKILLS {
            remove_skill(&claude, name);
        }
        let mut record = self.record();
        for skill in &SKILLS {
            if (skill.wanted)(&wanted) {
                match self.install_skill(&claude, skill, wanted, &mut record) {
                    Ok(SkillOutcome::Written) => log::info!("agent skills: installed {}", skill.name),
                    Ok(SkillOutcome::Foreign) => log::info!("agent skills: {} keeps the copy SplitGrid installed", skill.name),
                    Ok(SkillOutcome::Unchanged) => {}
                    Err(e) => log::error!("agent skills: installing {}: {e:#}", skill.name),
                }
            } else {
                remove_skill(&claude, skill.name);
                record.remove(skill.name);
            }
        }
        self.save_record(&record);
    }

    pub fn install_skill(&self, claude: &Path, skill: &Skill, wanted: Wanted, record: &mut BTreeMap<String, String>) -> Result<SkillOutcome> {
        let source = self.skills_src.join(skill.name).join("SKILL.md");
        let text = fs::read_to_string(&source).with_context(|| format!("bundled skill missing at {}", source.display()))?;
        let desired = if skill.name == "sg-terminal" { render_terminal_skill(&text, wanted.long_running_in_panes) } else { text };
        let dest_dir = claude.join("skills").join(skill.name);
        let dest = dest_dir.join("SKILL.md");
        let current = fs::read_to_string(&dest).ok();
        if current.as_deref() == Some(desired.as_str()) {
            return Ok(SkillOutcome::Unchanged);
        }
        // Someone else's copy (the Electron app's, any version): theirs to refresh.
        if let Some(current) = &current
            && record.get(skill.name) != Some(&hash(current))
        {
            return Ok(SkillOutcome::Foreign);
        }
        fs::create_dir_all(&dest_dir)?;
        fs::write(&dest, &desired)?;
        record.insert(skill.name.to_owned(), hash(&desired));
        Ok(SkillOutcome::Written)
    }

    fn record(&self) -> BTreeMap<String, String> {
        fs::read(self.data_dir.join(RECORD_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save_record(&self, record: &BTreeMap<String, String>) {
        let path = self.data_dir.join(RECORD_FILE);
        if (record.is_empty() && !path.exists()) || self.record() == *record {
            return;
        }
        if let Err(e) = serde_json::to_vec_pretty(record).map_err(anyhow::Error::from).and_then(|b| crate::model::store::write_atomic(&path, &b)) {
            log::error!("agent skills: saving {}: {e:#}", path.display());
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum HookOutcome {
    Written,
    Unchanged,
    /// Left to the app whose (existing) helper the entries call.
    Foreign(PathBuf),
}

#[derive(Debug, PartialEq, Eq)]
pub enum SkillOutcome {
    Written,
    Unchanged,
    Foreign,
}

pub struct Skill {
    pub name: &'static str,
    wanted: fn(&Wanted) -> bool,
}

/// agent-skills/installer.ts: browser rides the master switch; the agent-pane
/// skill rides terminal control (prompting a pane is prompting an agent).
pub const SKILLS: [Skill; 6] = [
    Skill { name: "sg-browser", wanted: |_| true },
    Skill { name: "sg-terminal", wanted: |w| w.terminal },
    Skill { name: "sg-agent", wanted: |w| w.terminal },
    Skill { name: "sg-sql", wanted: |w| w.sql },
    Skill { name: "sg-sftp", wanted: |w| w.sftp },
    Skill { name: "sg-history", wanted: |w| w.history },
];

fn hash(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn remove_skill(claude: &Path, name: &str) {
    let dir = claude.join("skills").join(name);
    if dir.exists() {
        match fs::remove_dir_all(&dir) {
            Ok(()) => log::info!("agent skills: removed {name}"),
            Err(e) => log::error!("agent skills: removing {name}: {e}"),
        }
    }
}

/// The bundled skills: in the app bundle's Resources, else (a dev build) the
/// Electron app's sources next to this repo.
fn skills_dir() -> PathBuf {
    let bundled = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.parent()?.join("Resources/skills")));
    match bundled {
        Some(dir) if dir.join("sg-browser/SKILL.md").is_file() => dir,
        _ => PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../swapit/resources/skills")),
    }
}

// ── sg-terminal's long-running-process policy (agent-skills/terminal-skill.ts) ──

const BACKGROUND_DESCRIPTION: &str =
    " - and to open a small tray pane for a long-running process the user wants to watch. Simple background jobs may stay background Bash jobs.";
const PANES_DESCRIPTION: &str = " - and to start any long-running process (dev server, watcher, tail -f, http.server) in a small tray pane instead of a background Bash job, so the user can watch and stop it.";
const BACKGROUND_RULE: &str = "- **Simple background jobs may stay in the background.** A dev server,
  watcher or `tail -f` that only has to keep running can be a background Bash
  job (`run_in_background`). Open a tray pane with `open <label> --background`
  instead when the user asks to see it, or it must outlive this session.
  Background panes sit in a thin strip along the zone's bottom.";
const PANES_RULE: &str = "- **Long-running processes go in a tray pane, not the background.** A dev
  server, watcher, `tail -f` or `python3 -m http.server` started with
  `run_in_background` or `&` is invisible to the user and dies with this
  session. `open <label> --background` a terminal, `send` the command there,
  then `read` it. Background panes sit in a thin strip along the zone's
  bottom, so they stay visible without crowding the layout.";

/// The sg-terminal skill text for the chosen long-running-process policy.
pub fn render_terminal_skill(text: &str, long_running_in_panes: bool) -> String {
    if !long_running_in_panes {
        return text.to_owned();
    }
    text.replacen(BACKGROUND_DESCRIPTION, PANES_DESCRIPTION, 1).replacen(BACKGROUND_RULE, PANES_RULE, 1)
}

// ── Hook config JSON ─────────────────────────────────────────────────────────

/// The config and its compact form (to tell a real change from a re-format).
/// A missing file is `{}`; a malformed one is an error, never overwritten.
fn read_config(file: &Path) -> Result<(Value, String)> {
    let text = match fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", file.display())),
    };
    let root: Value = if text.trim().is_empty() { json!({}) } else { serde_json::from_str(&text).with_context(|| format!("parse {}", file.display()))? };
    if !root.is_object() {
        bail!("{} is not a JSON object", file.display());
    }
    let compact = if text.trim().is_empty() { String::new() } else { serde_json::to_string(&root)? };
    Ok((root, compact))
}

/// As the Electron app writes it: two-space JSON and a newline, in place (a
/// symlinked config stays a symlink).
fn write_config(file: &Path, root: &Value) -> Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string_pretty(root)?;
    text.push('\n');
    fs::write(file, text).with_context(|| format!("write {}", file.display()))
}

fn hooks_obj<'a>(root: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
    let obj = root.as_object_mut().expect("config root is an object");
    let slot = obj.entry(key).or_insert_with(|| json!({}));
    if !slot.is_object() {
        *slot = json!({});
    }
    slot.as_object_mut().unwrap()
}

fn commands(entry: &Value) -> impl Iterator<Item = &str> {
    entry.get("hooks").and_then(Value::as_array).into_iter().flatten().filter_map(|h| h.get("command").and_then(Value::as_str))
}

fn is_owned(entry: &Value, marker: &str) -> bool {
    commands(entry).any(|c| c.contains(marker))
}

/// `"<helper>" <event>` → the helper path.
fn helper_of(command: &str) -> Option<PathBuf> {
    let path = match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next()?,
        None => command.split_whitespace().next()?,
    };
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// The helper of a SplitGrid entry that calls an existing helper other than
/// `own` — another SplitGrid install manages this config.
fn foreign_helper(root: &Value, def: &HookDef, own: &Path) -> Option<PathBuf> {
    let hooks = root.get(def.hooks_key)?.as_object()?;
    def.events
        .iter()
        .filter_map(|e| hooks.get(e.agent_event)?.as_array())
        .flatten()
        .filter(|entry| is_owned(entry, HELPER_MARKER))
        .flat_map(commands)
        .filter(|c| c.contains(HELPER_MARKER))
        .filter_map(helper_of)
        .find(|p| !same_file(p, own) && p.is_file())
}

/// Merge our hooks in (idempotent: any earlier SplitGrid entry is replaced,
/// the user's are kept) — applyHooksToConfig.
pub fn apply_hooks(root: &mut Value, def: &HookDef, helper: &Path) -> Result<()> {
    if !root.is_object() {
        bail!("config is not a JSON object");
    }
    let hooks = hooks_obj(root, def.hooks_key);
    for e in def.events {
        let mut kept: Vec<Value> = hooks.get(e.agent_event).and_then(Value::as_array).cloned().unwrap_or_default();
        kept.retain(|entry| !is_owned(entry, HELPER_MARKER));
        let mut entry = Map::new();
        if e.matcher {
            entry.insert("matcher".into(), "*".into());
        }
        entry.insert(
            "hooks".into(),
            json!([{ "type": "command", "command": format!("\"{}\" {}", helper.display(), e.splitgrid_event), "timeout": def.timeout_sec }]),
        );
        kept.push(Value::Object(entry));
        hooks.insert(e.agent_event.to_owned(), Value::Array(kept));
    }
    Ok(())
}

/// Remove the entries carrying `marker`, keeping everything else; an emptied
/// event list and an emptied hooks object go too. Returns whether anything changed.
pub fn remove_hooks(root: &mut Value, def: &HookDef, marker: &str) -> bool {
    let Some(hooks) = root.get_mut(def.hooks_key).and_then(Value::as_object_mut) else { return false };
    let mut changed = false;
    for e in def.events {
        let Some(list) = hooks.get_mut(e.agent_event).and_then(Value::as_array_mut) else { continue };
        let before = list.len();
        list.retain(|entry| !is_owned(entry, marker));
        if list.len() != before {
            changed = true;
            if list.is_empty() {
                hooks.remove(e.agent_event);
            }
        }
    }
    if changed && hooks.is_empty() {
        root.as_object_mut().unwrap().remove(def.hooks_key);
    }
    changed
}

fn remove_hooks_in_file(file: &Path, marker: &str) -> Result<()> {
    if !file.exists() {
        return Ok(());
    }
    let (mut root, _) = read_config(file)?;
    let def = HOOK_DEFS.iter().find(|d| file.file_name().and_then(|n| n.to_str()) == Some(d.config_file)).copied().unwrap_or(&CLAUDE);
    if remove_hooks(&mut root, def, marker) {
        write_config(file, &root)?;
        log::info!("agent hooks: removed {marker} entries from {}", file.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _tmp: tempfile::TempDir,
        inst: Installer,
        claude: PathBuf,
        codex: PathBuf,
    }

    /// A scratch home with ~/.claude and ~/.codex, a helper and bundled skills.
    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let (claude, codex) = (home.join(".claude"), home.join(".codex"));
        fs::create_dir_all(&claude).unwrap();
        fs::create_dir_all(&codex).unwrap();
        let lite = tmp.path().join("Lite.app/helpers");
        fs::create_dir_all(&lite).unwrap();
        fs::write(lite.join("splitgrid-hook.sh"), "#!/bin/sh\n").unwrap();
        let src = tmp.path().join("skills");
        for s in SKILLS {
            fs::create_dir_all(src.join(s.name)).unwrap();
            let body = if s.name == "sg-terminal" { format!("terminal{BACKGROUND_DESCRIPTION}\n{BACKGROUND_RULE}\n") } else { format!("{} v1\n", s.name) };
            fs::write(src.join(s.name).join("SKILL.md"), body).unwrap();
        }
        let data = tmp.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let inst = Installer {
            env: Env { home, claude_config_dir: None, codex_home: None },
            helper: lite.join("splitgrid-hook.sh"),
            skills_src: src,
            data_dir: data,
        };
        Fixture { _tmp: tmp, inst, claude, codex }
    }

    fn settings(f: &Fixture, v: Value) {
        fs::write(f.inst.data_dir.join("app-settings.json"), v.to_string()).unwrap();
    }

    fn read(p: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn hooks_merge_keep_the_users_and_are_idempotent() {
        let f = fixture();
        let file = f.claude.join("settings.json");
        fs::write(
            &file,
            r#"{"model":"opus","permissions":{"allow":["Bash(ls)"]},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"say done"}]}],"PreCompact":[{"hooks":[{"type":"command","command":"x"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(f.inst.sync_hooks(&CLAUDE).unwrap(), HookOutcome::Written);
        let v = read(&file);
        assert_eq!(v["model"], "opus");
        assert_eq!(v["permissions"]["allow"][0], "Bash(ls)");
        assert_eq!(v["hooks"]["PreCompact"][0]["hooks"][0]["command"], "x", "events we don't use are untouched");
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "say done", "the user's own hook stays first");
        let ours = stop[1]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(ours, format!("\"{}\" stop", f.inst.helper.display()));
        assert_eq!(stop[1]["hooks"][0]["timeout"], 5);
        assert!(stop[1].get("matcher").is_none());
        assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "*");
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["model", "permissions", "hooks"], "key order kept");
        assert!(fs::read_to_string(&file).unwrap().ends_with("}\n"));

        let before = fs::read_to_string(&file).unwrap();
        assert_eq!(f.inst.sync_hooks(&CLAUDE).unwrap(), HookOutcome::Unchanged);
        assert_eq!(fs::read_to_string(&file).unwrap(), before);

        // Codex: its own file, PermissionRequest with a matcher; a missing file is created.
        assert_eq!(f.inst.sync_hooks(&CODEX).unwrap(), HookOutcome::Written);
        let c = read(&f.codex.join("hooks.json"));
        assert_eq!(c["hooks"]["PermissionRequest"][0]["matcher"], "*");
        assert_eq!(c["hooks"].as_object().unwrap().len(), 3);
    }

    #[test]
    fn hooks_uninstall_is_surgical_and_idempotent() {
        let f = fixture();
        let file = f.claude.join("settings.json");
        fs::write(&file, r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"say done"}]}]}}"#).unwrap();
        f.inst.sync_hooks(&CLAUDE).unwrap();
        f.inst.uninstall();
        let v = read(&file);
        assert_eq!(v, json!({"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"say done"}]}]}}));
        let before = fs::read_to_string(&file).unwrap();
        f.inst.uninstall();
        assert_eq!(fs::read_to_string(&file).unwrap(), before);

        // Only ours: the hooks object goes entirely.
        fs::write(&file, r#"{"model":"opus"}"#).unwrap();
        f.inst.sync_hooks(&CLAUDE).unwrap();
        f.inst.uninstall();
        assert_eq!(read(&file), json!({"model":"opus"}));
        assert!(!f.codex.join("hooks.json").exists(), "never created just to remove nothing");
    }

    #[test]
    fn hooks_leave_the_electron_apps_entries_alone() {
        let f = fixture();
        let electron = f._tmp.path().join("SplitGrid.app/resources/splitgrid-hook.sh");
        fs::create_dir_all(electron.parent().unwrap()).unwrap();
        fs::write(&electron, "#!/bin/sh\n").unwrap();
        let file = f.claude.join("settings.json");
        let mut root = json!({"theme":"dark"});
        apply_hooks(&mut root, &CLAUDE, &electron).unwrap();
        fs::write(&file, serde_json::to_string_pretty(&root).unwrap()).unwrap();
        let before = fs::read_to_string(&file).unwrap();
        assert_eq!(f.inst.sync_hooks(&CLAUDE).unwrap(), HookOutcome::Foreign(electron.clone()));
        assert_eq!(fs::read_to_string(&file).unwrap(), before, "not rewritten to our helper");

        // Its helper gone (app removed): ours replace the dead entries.
        fs::remove_file(&electron).unwrap();
        assert_eq!(f.inst.sync_hooks(&CLAUDE).unwrap(), HookOutcome::Written);
        let v = read(&file);
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "the dead entry is replaced, not duplicated");
        assert!(stop[0]["hooks"][0]["command"].as_str().unwrap().contains(&*f.inst.helper.to_string_lossy()));
        assert_eq!(v["theme"], "dark");
    }

    #[test]
    fn malformed_config_is_never_overwritten() {
        let f = fixture();
        let file = f.claude.join("settings.json");
        fs::write(&file, "{ not json").unwrap();
        assert!(f.inst.sync_hooks(&CLAUDE).is_err());
        f.inst.uninstall();
        assert_eq!(fs::read_to_string(&file).unwrap(), "{ not json");
    }

    #[test]
    fn startup_installs_only_when_on_and_never_uninstalls() {
        let f = fixture();
        f.inst.startup();
        assert!(!f.claude.join("settings.json").exists(), "integrations off: nothing touched");
        assert!(!f.claude.join("skills").exists());

        settings(&f, json!({"agentIntegrations": true}));
        f.inst.startup();
        assert!(f.claude.join("skills/sg-browser/SKILL.md").is_file());
        assert!(!f.claude.join("skills/sg-terminal").exists(), "terminal control off");
        assert!(read(&f.claude.join("settings.json"))["hooks"]["Stop"].is_array());

        settings(&f, json!({"agentIntegrations": false}));
        f.inst.startup();
        assert!(f.claude.join("skills/sg-browser").exists(), "startup never uninstalls");
        f.inst.apply();
        assert!(!f.claude.join("skills/sg-browser").exists(), "switching off removes");
        assert!(read(&f.claude.join("settings.json")).get("hooks").is_none());
    }

    #[test]
    fn agent_absent_means_nothing_installed() {
        let f = fixture();
        fs::remove_dir_all(&f.codex).unwrap();
        settings(&f, json!({"agentIntegrations": true}));
        f.inst.apply();
        assert!(!f.codex.exists(), "no ~/.codex created");
        assert!(f.claude.join("settings.json").exists());
    }

    #[test]
    fn skills_follow_the_switches() {
        let f = fixture();
        settings(&f, json!({"agentIntegrations": true, "agentTerminalControl": true, "agentSqlControl": true, "agentLongRunningInPanes": true}));
        f.inst.apply();
        let skills = f.claude.join("skills");
        for s in ["sg-browser", "sg-terminal", "sg-agent", "sg-sql"] {
            assert!(skills.join(s).join("SKILL.md").is_file(), "{s}");
        }
        assert!(!skills.join("sg-sftp").exists());
        assert!(!skills.join("sg-history").exists());
        let term = fs::read_to_string(skills.join("sg-terminal/SKILL.md")).unwrap();
        assert!(term.contains(PANES_RULE) && term.contains(PANES_DESCRIPTION), "long-running policy rendered");

        settings(&f, json!({"agentIntegrations": true, "agentTerminalControl": false, "agentSqlControl": true}));
        f.inst.apply();
        assert!(!skills.join("sg-terminal").exists() && !skills.join("sg-agent").exists());
        assert!(skills.join("sg-sql").exists());
    }

    #[test]
    fn skills_leave_foreign_copies_and_refresh_their_own() {
        let f = fixture();
        let skills = f.claude.join("skills");
        settings(&f, json!({"agentIntegrations": true}));
        let mut record = BTreeMap::new();
        let w = Wanted::from_settings(&f.inst.settings());

        // Identical content (whoever wrote it): nothing to do.
        fs::create_dir_all(skills.join("sg-browser")).unwrap();
        fs::write(skills.join("sg-browser/SKILL.md"), "sg-browser v1\n").unwrap();
        assert_eq!(f.inst.install_skill(&f.claude, &SKILLS[0], w, &mut record).unwrap(), SkillOutcome::Unchanged);

        // The Electron app's (different) copy: left alone.
        fs::write(skills.join("sg-browser/SKILL.md"), "sg-browser v2 from SplitGrid\n").unwrap();
        assert_eq!(f.inst.install_skill(&f.claude, &SKILLS[0], w, &mut record).unwrap(), SkillOutcome::Foreign);
        assert_eq!(fs::read_to_string(skills.join("sg-browser/SKILL.md")).unwrap(), "sg-browser v2 from SplitGrid\n");

        // Missing: installed and remembered; a newer bundle refreshes our own copy.
        fs::remove_dir_all(skills.join("sg-browser")).unwrap();
        assert_eq!(f.inst.install_skill(&f.claude, &SKILLS[0], w, &mut record).unwrap(), SkillOutcome::Written);
        fs::write(f.inst.skills_src.join("sg-browser/SKILL.md"), "sg-browser v3\n").unwrap();
        assert_eq!(f.inst.install_skill(&f.claude, &SKILLS[0], w, &mut record).unwrap(), SkillOutcome::Written);
        assert_eq!(fs::read_to_string(skills.join("sg-browser/SKILL.md")).unwrap(), "sg-browser v3\n");

        // The record persists across runs.
        f.inst.save_record(&record);
        assert_eq!(f.inst.record(), record);
    }

    #[test]
    fn legacy_artifacts_are_removed_on_startup() {
        let f = fixture();
        let file = f.claude.join("settings.json");
        fs::write(&file, r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"\"/old/swapit-hook.sh\" stop"}]},{"hooks":[{"type":"command","command":"mine"}]}]}}"#).unwrap();
        fs::create_dir_all(f.claude.join("skills/swapit-browser")).unwrap();
        fs::create_dir_all(f.claude.join("skills/my-skill")).unwrap();
        f.inst.startup();
        assert_eq!(read(&file), json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"mine"}]}]}}));
        assert!(!f.claude.join("skills/swapit-browser").exists());
        assert!(f.claude.join("skills/my-skill").exists());
    }

    #[test]
    fn config_dir_overrides_follow_the_agents() {
        let env = Env { home: "/h".into(), claude_config_dir: Some("/c".into()), codex_home: Some("  ".into()) };
        assert_eq!(env.config_dir(&CLAUDE), PathBuf::from("/c"));
        assert_eq!(env.config_dir(&CODEX), PathBuf::from("/h/.codex"));
    }

    #[test]
    fn bundled_terminal_skill_has_the_swappable_passages() {
        let text = fs::read_to_string(skills_dir().join("sg-terminal/SKILL.md")).unwrap();
        let panes = render_terminal_skill(&text, true);
        assert!(panes.contains(PANES_RULE) && panes.contains(PANES_DESCRIPTION));
        assert_eq!(render_terminal_skill(&text, false), text);
    }
}
