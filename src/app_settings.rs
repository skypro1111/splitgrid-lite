//! What the shared `app-settings.json` makes this app do beyond drawing the
//! Settings window: the notification sound, the focus-mode shortcut, and the
//! managed tmux/screen mouse block. Keys, defaults and file formats are the
//! Electron app's (shared/types AppSettings, sounds/, quick-chat-hotkey.ts,
//! terminal-mouse-config.ts), since both apps read and write the same file.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use gpui_kit::{App, KeyBinding, Keystroke};
use serde_json::{Map, Value, json};

pub type Settings = Map<String, Value>;

pub fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

// ── Appearance: background colour and fonts (Settings → Appearance) ─────────
//
// `backgroundColor` is the Electron app's key (both apps follow it). The font
// keys are Lite's own: the Electron app has no font settings and keeps them as
// unknown keys.

pub const DEFAULT_TERMINAL_FONT: &str = "Menlo";
pub const DEFAULT_TERMINAL_SIZE: f32 = 13.;
pub const DEFAULT_EDITOR_SIZE: f32 = 13.;
pub const FONT_SIZES: std::ops::RangeInclusive<u32> = 9..=24;

pub fn default_fonts() -> crate::theme::Fonts {
    crate::theme::Fonts {
        ui: crate::theme::UI_FONT.into(),
        terminal: DEFAULT_TERMINAL_FONT.into(),
        terminal_size: DEFAULT_TERMINAL_SIZE,
        editor: crate::ide::EDITOR_FONT.into(),
        editor_size: DEFAULT_EDITOR_SIZE,
    }
}

pub fn fonts(s: &Settings) -> crate::theme::Fonts {
    let d = default_fonts();
    let family = |k: &str, d: gpui_kit::SharedString| s.get(k).and_then(Value::as_str).map(str::trim).filter(|f| !f.is_empty()).map(|f| f.to_owned().into()).unwrap_or(d);
    let size = |k: &str, d: f32| {
        s.get(k).and_then(Value::as_f64).map(|v| v as f32).filter(|v| (*FONT_SIZES.start() as f32..=*FONT_SIZES.end() as f32).contains(v)).unwrap_or(d)
    };
    crate::theme::Fonts {
        ui: family("uiFontFamily", d.ui),
        terminal: family("terminalFontFamily", d.terminal),
        terminal_size: size("terminalFontSize", d.terminal_size),
        editor: family("editorFontFamily", d.editor),
        editor_size: size("editorFontSize", d.editor_size),
    }
}

/// Take the colour and fonts the settings name: the palette and fonts every
/// view reads, gpui-component's theme, then a redraw. Returns the fonts before.
pub fn apply_appearance(s: &Settings, cx: &mut App) -> crate::theme::Fonts {
    let before = crate::theme::fonts();
    crate::theme::set_palette(crate::background::derive(Some(&crate::background::from_settings(s))));
    crate::theme::set_fonts(fonts(s));
    crate::theme::apply_to_components(cx);
    // apply_to_components resets the sizes; the window zoom scales them again.
    crate::ui::apply(crate::ui::level(), cx);
    before
}

// ── Notification sound (renderer/sounds, useNotificationSound) ───────────────

/// (id, label), in the order the picker shows them.
pub const SOUNDS: [(&str, &str); 9] = [
    ("notification-new-1", "New 1"),
    ("notification-new-2", "New 2"),
    ("notification-new-3", "New 3"),
    ("notification-new-4", "New 4"),
    ("notification-bright-1", "Bright"),
    ("notification-jump-1", "Jump"),
    ("notification-power-1", "Power"),
    ("notification-space-1", "Space"),
    ("notification-error-1", "Error"),
];
pub const SILENT_SOUND_ID: &str = "none";
pub const DEFAULT_SOUND_ID: &str = "notification-new-1";
pub const DEFAULT_VOLUME: f32 = 0.5;
/// Many terminals finishing at once make one sound.
const BURST_COALESCE: Duration = Duration::from_millis(250);

pub fn sound_label(id: &str) -> &'static str {
    if id == SILENT_SOUND_ID {
        return "Silent";
    }
    SOUNDS.iter().find(|(i, _)| *i == id).map(|(_, l)| *l).unwrap_or("New 1")
}

pub fn default_sound(s: &Settings) -> String {
    s.get("defaultSoundId").and_then(Value::as_str).unwrap_or(DEFAULT_SOUND_ID).to_owned()
}

pub fn default_volume(s: &Settings) -> f32 {
    s.get("defaultVolume").and_then(Value::as_f64).map(|v| v.clamp(0., 1.) as f32).unwrap_or(DEFAULT_VOLUME)
}

/// The bundled mp3s: in the app bundle's Resources, else (a dev build) this
/// repo's resources/sounds.
fn sounds_dir() -> PathBuf {
    let bundled = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.parent()?.join("Resources/sounds")));
    match bundled {
        Some(dir) if dir.join(format!("{DEFAULT_SOUND_ID}.mp3")).is_file() => dir,
        _ => PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/resources/sounds")),
    }
}

/// Play a sound by id at a volume 0–1; silent and unknown ids play nothing.
pub fn play(id: &str, volume: f32) {
    if !SOUNDS.iter().any(|(i, _)| *i == id) {
        return;
    }
    let file = sounds_dir().join(format!("{id}.mp3"));
    let volume = format!("{:.2}", volume.clamp(0., 1.));
    std::thread::spawn(move || {
        if let Err(e) = std::process::Command::new("/usr/bin/afplay").arg("-v").arg(volume).arg(&file).output() {
            log::warn!("playing {}: {e}", file.display());
        }
    });
}

/// The sound for an agent that needs the user: the workspace's own choice
/// (`notifySoundId` / `notifyVolume`), else the defaults; mute-all and the
/// workspace's mute silence it.
pub fn alert_sound(s: &Settings, ws: &Map<String, Value>) -> Option<(String, f32)> {
    if s.get("muteAll").and_then(Value::as_bool) == Some(true) || ws.get("notifyMuted").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let id = ws.get("notifySoundId").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| default_sound(s));
    let volume = ws.get("notifyVolume").and_then(Value::as_f64).map(|v| v.clamp(0., 1.) as f32).unwrap_or_else(|| default_volume(s));
    Some((id, volume))
}

pub fn play_alert_sound(s: &Settings, ws: &Map<String, Value>) {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let Some((id, volume)) = alert_sound(s, ws) else { return };
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_some_and(|t| t.elapsed() < BURST_COALESCE) {
        return;
    }
    *last = Some(Instant::now());
    play(&id, volume);
}

// ── Focus-mode shortcut (quick-chat-hotkey.ts) ───────────────────────────────

/// A chord as the Electron app stores it: the latin key, lowercased, plus modifiers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub key: String,
    pub meta: bool,
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Hotkey {
    pub fn default_focus_mode() -> Self {
        Self { key: "f".into(), meta: true, control: false, alt: false, shift: true }
    }

    /// normalizeQuickChatHotkey: unusable (no key, or no modifier but Shift) → None.
    pub fn from_value(v: &Value) -> Option<Self> {
        let o = v.as_object()?;
        let flag = |k: &str| o.get(k).and_then(Value::as_bool).unwrap_or(false);
        let key = o.get("key")?.as_str()?.to_lowercase();
        let h = Self { key, meta: flag("meta"), control: flag("control"), alt: flag("alt"), shift: flag("shift") };
        (!h.key.is_empty() && (h.meta || h.control || h.alt)).then_some(h)
    }

    pub fn to_value(&self) -> Value {
        json!({ "key": self.key, "meta": self.meta, "control": self.control, "alt": self.alt, "shift": self.shift })
    }

    pub fn focus_mode(s: &Settings) -> Self {
        s.get("focusModeHotkey").and_then(Self::from_value).unwrap_or_else(Self::default_focus_mode)
    }

    /// A recorded keystroke; None for a bare modifier or a chord without ⌘/⌃/⌥.
    pub fn from_keystroke(k: &Keystroke) -> Option<Self> {
        let key = match k.key.as_str() {
            "shift" | "control" | "alt" | "platform" | "function" | "" => return None,
            "space" => " ".to_owned(),
            "up" | "down" | "left" | "right" => format!("arrow{}", k.key),
            other => other.to_lowercase(),
        };
        let m = &k.modifiers;
        let h = Self { key, meta: m.platform, control: m.control, alt: m.alt, shift: m.shift };
        (h.meta || h.control || h.alt).then_some(h)
    }

    /// The chord in gpui's keystroke syntax, e.g. `cmd-shift-f`.
    pub fn keystroke(&self) -> String {
        let key = match self.key.as_str() {
            " " => "space".to_owned(),
            k if k.starts_with("arrow") => k["arrow".len()..].to_owned(),
            k => k.to_owned(),
        };
        let mut s = String::new();
        for (on, m) in [(self.control, "ctrl-"), (self.alt, "alt-"), (self.shift, "shift-"), (self.meta, "cmd-")] {
            if on {
                s.push_str(m);
            }
        }
        s + &key
    }

    /// formatQuickChatHotkey on macOS: "⌘⇧F".
    pub fn label(&self) -> String {
        let mut s = String::new();
        for (on, m) in [(self.control, "⌃"), (self.alt, "⌥"), (self.shift, "⇧"), (self.meta, "⌘")] {
            if on {
                s.push_str(m);
            }
        }
        let key = match self.key.as_str() {
            " " => "Space".to_owned(),
            "arrowup" => "↑".into(),
            "arrowdown" => "↓".into(),
            "arrowleft" => "←".into(),
            "arrowright" => "→".into(),
            "escape" => "Esc".into(),
            "enter" => "Enter".into(),
            "tab" => "Tab".into(),
            "backspace" => "⌫".into(),
            k if k.chars().count() == 1 => k.to_uppercase(),
            k => {
                let mut c = k.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            }
        };
        s + &key
    }
}

/// Bind focus mode to the chord the settings name, replacing the previous
/// binding (a no-op when it hasn't changed).
pub fn apply_focus_hotkey(s: &Settings, cx: &mut App) {
    static APPLIED: Mutex<Option<String>> = Mutex::new(None);
    let chord = Hotkey::focus_mode(s).keystroke();
    let mut applied = APPLIED.lock().unwrap_or_else(|e| e.into_inner());
    if applied.as_deref() == Some(chord.as_str()) {
        return;
    }
    let Ok(binding) = KeyBinding::load(&chord, Box::new(crate::app::ToggleFocusMode), None, false, None, cx) else {
        log::warn!("focus mode shortcut {chord:?} isn't a valid key binding");
        return;
    };
    let name = gpui_kit::Action::name(&crate::app::ToggleFocusMode);
    let keymap = cx.key_bindings();
    let kept: Vec<KeyBinding> = keymap.borrow().bindings().filter(|b| b.action().name() != name).cloned().collect();
    cx.clear_key_bindings();
    cx.bind_keys(kept);
    cx.bind_keys([binding]);
    *applied = Some(chord);
}

// ── Mouse wheel in tmux / screen (terminal-mouse-config.ts) ─────────────────

const BEGIN: &str = "# >>> SplitGrid: mouse scroll (managed — do not edit inside) >>>";
const END: &str = "# <<< SplitGrid: mouse scroll (managed) <<<";

fn tmux_block() -> String {
    [BEGIN, "# Lets the mouse wheel scroll tmux scrollback / copy-mode.", "# Toggle via SplitGrid → Settings → Terminal, or delete this block.", "set -g mouse on", END, ""].join("\n")
}

fn screen_block() -> String {
    [
        BEGIN,
        "# Lets the mouse wheel scroll GNU screen via the host terminal scrollback.",
        "# Toggle via SplitGrid → Settings → Terminal, or delete this block.",
        "termcapinfo xterm*|rxvt*|kterm*|Eterm* ti@:te@",
        "defscrollback 10000",
        END,
        "",
    ]
    .join("\n")
}

/// Our block (and the blank lines hugging it) out, leftover blank runs tidied.
pub fn strip_managed_block(content: &str) -> String {
    let re = regex::Regex::new(&format!(r"\n*{}[\s\S]*?{}[^\n]*\n?", regex::escape(BEGIN), regex::escape(END))).unwrap();
    let s = re.replace_all(content, "\n");
    let s = regex::Regex::new(r"^\n+").unwrap().replace(&s, "");
    regex::Regex::new(r"\n{3,}").unwrap().replace_all(&s, "\n\n").into_owned()
}

pub fn transform_config(original: &str, block: &str, enable: bool) -> String {
    let mut next = strip_managed_block(original);
    if enable {
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(block);
    }
    next
}

fn apply_to_file(path: &Path, block: &str, enable: bool) -> std::io::Result<()> {
    let existed = path.exists();
    if !enable && !existed {
        return Ok(());
    }
    let original = if existed { std::fs::read_to_string(path)? } else { String::new() };
    let next = transform_config(&original, block, enable);
    if (!existed && next.trim().is_empty()) || next == original {
        return Ok(());
    }
    std::fs::write(path, next)
}

/// Add or remove the managed block in `<home>/.tmux.conf` and `<home>/.screenrc`.
pub fn apply_terminal_mouse_config(home: &Path, enable: bool) {
    for (file, block) in [(".tmux.conf", tmux_block()), (".screenrc", screen_block())] {
        let path = home.join(file);
        if let Err(e) = apply_to_file(&path, &block, enable) {
            log::error!("updating {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkeys_round_trip_the_electron_format() {
        let s: Settings = serde_json::from_str(r#"{"focusModeHotkey":{"key":"P","meta":true,"alt":true}}"#).unwrap();
        let h = Hotkey::focus_mode(&s);
        assert_eq!(h.keystroke(), "alt-cmd-p");
        assert_eq!(h.label(), "⌥⌘P");
        assert_eq!(Hotkey::focus_mode(&Settings::new()).keystroke(), "shift-cmd-f");
        // No usable modifier: the default stands.
        let bare: Settings = serde_json::from_str(r#"{"focusModeHotkey":{"key":"p","shift":true}}"#).unwrap();
        assert_eq!(Hotkey::focus_mode(&bare), Hotkey::default_focus_mode());

        let k = Keystroke::parse("ctrl-shift-up").unwrap();
        let h = Hotkey::from_keystroke(&k).unwrap();
        assert_eq!(h, Hotkey { key: "arrowup".into(), meta: false, control: true, alt: false, shift: true });
        assert_eq!(h.keystroke(), "ctrl-shift-up");
        assert_eq!(h.label(), "⌃⇧↑");
        assert_eq!(Hotkey::from_value(&h.to_value()), Some(h));
        assert!(Hotkey::from_keystroke(&Keystroke::parse("shift-a").unwrap()).is_none());
        let space: &'static str = Hotkey::from_keystroke(&Keystroke::parse("cmd-space").unwrap()).unwrap().keystroke().leak();
        assert_eq!(Keystroke::parse(space).unwrap().key, "space");
    }

    #[test]
    fn alert_sound_follows_workspace_overrides_and_mutes() {
        let s: Settings = serde_json::from_str(r#"{"defaultSoundId":"notification-jump-1","defaultVolume":0.8}"#).unwrap();
        assert_eq!(alert_sound(&s, &Map::new()), Some(("notification-jump-1".into(), 0.8)));
        let ws: Map<String, Value> = serde_json::from_str(r#"{"notifySoundId":"none","notifyVolume":null}"#).unwrap();
        assert_eq!(alert_sound(&s, &ws), Some(("none".into(), 0.8)));
        let muted: Map<String, Value> = serde_json::from_str(r#"{"notifyMuted":true}"#).unwrap();
        assert_eq!(alert_sound(&s, &muted), None);
        let all: Settings = serde_json::from_str(r#"{"muteAll":true}"#).unwrap();
        assert_eq!(alert_sound(&all, &Map::new()), None);
        assert_eq!(alert_sound(&Settings::new(), &Map::new()), Some((DEFAULT_SOUND_ID.into(), DEFAULT_VOLUME)));
    }

    #[test]
    fn bundled_sounds_exist() {
        for (id, _) in SOUNDS {
            assert!(sounds_dir().join(format!("{id}.mp3")).is_file(), "{id}");
        }
    }

    #[test]
    fn mouse_block_toggles_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let conf = tmp.path().join(".tmux.conf");
        std::fs::write(&conf, "set -g history-limit 5000").unwrap();
        apply_terminal_mouse_config(tmp.path(), true);
        let on = std::fs::read_to_string(&conf).unwrap();
        assert!(on.starts_with("set -g history-limit 5000\n# >>> SplitGrid"));
        assert!(on.contains("set -g mouse on"));
        apply_terminal_mouse_config(tmp.path(), true);
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), on, "idempotent");
        assert!(std::fs::read_to_string(tmp.path().join(".screenrc")).unwrap().contains("defscrollback 10000"));
        apply_terminal_mouse_config(tmp.path(), false);
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "set -g history-limit 5000\n");
        assert_eq!(std::fs::read_to_string(tmp.path().join(".screenrc")).unwrap(), "");
        let empty = tempfile::tempdir().unwrap();
        apply_terminal_mouse_config(empty.path(), false);
        assert!(!empty.path().join(".tmux.conf").exists(), "never created to remove nothing");
    }
}
