//! The IDE pane (the Electron app's IDE): an explorer over the workspace
//! folder, tabs, and a tree-sitter editor. Behaviour follows the original:
//! a single click opens a file pinned, Space opens a preview (italic tab,
//! replaced by the next preview, pinned by editing), Enter/F2 rename, ⌘S
//! saves, closing a dirty tab saves it, files changed on disk reload unless
//! they have unsaved edits.

pub mod comment;
pub mod files;
pub mod tree;

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;
use serde::{Deserialize, Serialize};

use crate::assets::icon;
use crate::theme::*;
use tree::FileTree;

actions!(ide, [SaveFile, CloseTab, ToggleExplorer, ToggleComment]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-s", SaveFile, Some("Ide")),
        KeyBinding::new("cmd-s", SaveFile, Some("RemoteEditor")),
        KeyBinding::new("cmd-w", CloseTab, Some("Ide")),
        KeyBinding::new("cmd-b", ToggleExplorer, Some("Ide")),
        KeyBinding::new("cmd-/", ToggleComment, Some("Ide")),
        KeyBinding::new("cmd-/", ToggleComment, Some("RemoteEditor")),
    ]);
}

/// ⌘/ in an editor: comment or uncomment the selected lines (undoable, one step).
pub fn toggle_comment(editor: &Entity<EditorState>, path: &Path, window: &mut Window, cx: &mut App) {
    let Some(syntax) = comment::syntax(files::language(path).0) else { return };
    let (text, sel) = {
        let e = editor.read(cx);
        (e.value().to_string(), e.selected_range())
    };
    let edit = comment::toggle(&text, sel, syntax);
    editor.update(cx, |e, cx| {
        e.set_selected_range(edit.range.clone(), cx);
        e.replace(edit.text, window, cx);
        e.set_selected_range(edit.selection, cx);
    });
}

const SIDEBAR_DEFAULT: f32 = 240.;
const SIDEBAR_MIN: f32 = 160.;
const SIDEBAR_MAX: f32 = 500.;
const MAX_FILE: u64 = 50 * 1024 * 1024;
const WATCH_DEBOUNCE: Duration = Duration::from_millis(300);
pub const EDITOR_FONT: &str = "JetBrains Mono";

// ── Persisted state (IDEContainerState) ─────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TabState {
    pub file_path: String,
    pub is_preview: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplorerState {
    #[serde(default)]
    pub expanded_items: Vec<String>,
    #[serde(default)]
    pub selected_items: Vec<String>,
    #[serde(default)]
    pub focused_item: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdeState {
    #[serde(default)]
    pub tabs: Vec<TabState>,
    #[serde(default)]
    pub active_tab_id: Option<String>,
    #[serde(default = "default_width")]
    pub sidebar_width: f32,
    #[serde(default)]
    pub sidebar_collapsed: bool,
    #[serde(default = "default_font")]
    pub editor_font_size: f32,
    #[serde(default)]
    pub explorer: ExplorerState,
}

fn default_width() -> f32 {
    SIDEBAR_DEFAULT
}
fn default_font() -> f32 {
    13.
}

impl Default for IdeState {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active_tab_id: None,
            sidebar_width: SIDEBAR_DEFAULT,
            sidebar_collapsed: false,
            editor_font_size: 13.,
            explorer: ExplorerState::default(),
        }
    }
}

pub enum IdeEvent {
    /// Persist this (the owner debounces saving).
    State(IdeState),
    /// The active file or cursor moved: the status bar shows it.
    Cursor,
}

impl EventEmitter<IdeEvent> for IdePane {}

struct Tab {
    path: PathBuf,
    preview: bool,
    editor: Entity<EditorState>,
    /// The text as last saved or loaded.
    saved: String,
    dirty: bool,
    _sub: Subscription,
}

#[derive(Clone, PartialEq)]
enum Inline {
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
}

#[derive(Clone)]
enum Menu {
    Tree(Option<PathBuf>),
    Tab(usize),
}

pub struct IdePane {
    tree: FileTree,
    tree_focus: FocusHandle,
    sidebar_width: f32,
    sidebar_collapsed: bool,
    resizing: bool,
    tabs: Vec<Tab>,
    active: Option<usize>,
    font_size: f32,
    inline: Option<Inline>,
    inline_input: Entity<InputState>,
    menu: Option<(Menu, Point<Pixels>)>,
    error: Option<String>,
    /// Files the watcher saw change, reloaded on the next render.
    pending_reload: Vec<PathBuf>,
    /// The explorer's left edge last frame (for the resize drag).
    explorer_left: std::rc::Rc<std::cell::Cell<f32>>,
    _watcher: Option<notify::RecommendedWatcher>,
    _subs: Vec<Subscription>,
}

impl IdePane {
    pub fn new(root: PathBuf, state: IdeState, font_size: f32, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut tree = FileTree::new(root.clone());
        for dir in &state.explorer.expanded_items {
            let p = PathBuf::from(dir);
            if p.is_dir() && p.starts_with(&root) {
                tree.expand(&p);
            }
        }
        tree.selected = state.explorer.selected_items.iter().map(PathBuf::from).collect();
        tree.focused = state.explorer.focused_item.as_ref().map(PathBuf::from);

        let inline_input = cx.new(|cx| InputState::new(window, cx));
        let subs = vec![cx.subscribe_in(&inline_input, window, |this, input, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                let name = input.read(cx).value().trim().to_string();
                this.commit_inline(name, window, cx);
            }
        })];

        let mut this = Self {
            tree,
            tree_focus: cx.focus_handle(),
            sidebar_width: state.sidebar_width.clamp(SIDEBAR_MIN, SIDEBAR_MAX),
            sidebar_collapsed: state.sidebar_collapsed,
            resizing: false,
            tabs: Vec::new(),
            active: None,
            font_size,
            inline: None,
            inline_input,
            menu: None,
            error: None,
            pending_reload: Vec::new(),
            explorer_left: Default::default(),
            _watcher: None,
            _subs: subs,
        };
        for t in &state.tabs {
            let p = PathBuf::from(&t.file_path);
            if p.is_file() {
                this.open(p, t.is_preview, false, window, cx);
            }
        }
        this.active = state
            .active_tab_id
            .and_then(|id| this.tabs.iter().position(|t| t.path.to_string_lossy() == id))
            .or(if this.tabs.is_empty() { None } else { Some(0) });
        this.watch(root, cx);
        this
    }

    pub fn state(&self) -> IdeState {
        IdeState {
            tabs: self.tabs.iter().map(|t| TabState { file_path: t.path.to_string_lossy().into_owned(), is_preview: t.preview }).collect(),
            active_tab_id: self.active.and_then(|i| self.tabs.get(i)).map(|t| t.path.to_string_lossy().into_owned()),
            sidebar_width: self.sidebar_width,
            sidebar_collapsed: self.sidebar_collapsed,
            editor_font_size: self.font_size,
            explorer: ExplorerState {
                expanded_items: self.tree.expanded.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
                selected_items: self.tree.selected.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
                focused_item: self.tree.focused.as_ref().map(|p| p.to_string_lossy().into_owned()),
            },
        }
    }

    fn changed(&self, cx: &mut Context<Self>) {
        cx.emit(IdeEvent::State(self.state()));
        cx.notify();
    }

    pub fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.font_size = size;
        cx.notify();
    }

    /// A context menu is open (it may spill over a browser pane's web view).
    pub(crate) fn has_menu(&self) -> bool {
        self.menu.is_some()
    }

    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    /// "main.rs · Rust · Ln 3, Col 14" for the status bar, or "IDE".
    pub fn status(&self, cx: &App) -> String {
        let Some(tab) = self.active.and_then(|i| self.tabs.get(i)) else { return "IDE".into() };
        let name = tab.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let lang = files::language(&tab.path).1;
        let pos = tab.editor.read(cx).cursor_position();
        format!("{name} · {lang} · Ln {}, Col {}", pos.line + 1, pos.character + 1)
    }

    // ── Files ───────────────────────────────────────────────────────────────

    /// Open a file in a tab: activate it if open (pinning on a pinned open);
    /// a preview replaces the current preview tab in place.
    fn open(&mut self, path: PathBuf, preview: bool, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self.tabs.iter().position(|t| t.path == path) {
            if !preview {
                self.tabs[i].preview = false;
            }
            self.active = Some(i);
            if focus {
                let ed = self.tabs[i].editor.clone();
                ed.update(cx, |e, cx| e.focus(window, cx));
            }
            self.changed(cx);
            return;
        }
        let text = match std::fs::metadata(&path) {
            Ok(m) if m.len() > MAX_FILE => {
                self.error = Some(format!("File too large ({}MB). Maximum is 50MB.", m.len() / (1024 * 1024)));
                cx.notify();
                return;
            }
            _ => match std::fs::read(&path) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(e) => {
                    log::error!("open {}: {e}", path.display());
                    return;
                }
            },
        };
        let lang = files::language(&path).0;
        let editor = cx.new(|cx| EditorState::new(window, cx).language(lang).default_value(text.clone()));
        let tab_path = path.clone();
        let sub = cx.subscribe_in(&editor, window, move |this, ed, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                let value = ed.read(cx).value();
                if let Some(t) = this.tabs.iter_mut().find(|t| t.path == tab_path) {
                    t.dirty = value.as_ref() != t.saved;
                    // Editing pins a preview tab.
                    t.preview = false;
                }
                this.changed(cx);
            }
            cx.emit(IdeEvent::Cursor);
        });
        let tab = Tab { path, preview, editor: editor.clone(), saved: text, dirty: false, _sub: sub };
        match self.tabs.iter().position(|t| t.preview).filter(|_| preview) {
            Some(i) => {
                self.tabs[i] = tab;
                self.active = Some(i);
            }
            None => {
                self.tabs.push(tab);
                self.active = Some(self.tabs.len() - 1);
            }
        }
        if focus {
            editor.update(cx, |e, cx| e.focus(window, cx));
        }
        self.changed(cx);
    }

    fn save(&mut self, ix: usize, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.tabs.get_mut(ix) else { return false };
        let text = tab.editor.read(cx).value().to_string();
        match std::fs::write(&tab.path, &text) {
            Ok(()) => {
                tab.saved = text;
                tab.dirty = false;
                self.error = None;
                cx.notify();
                true
            }
            Err(e) => {
                let name = tab.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.error = Some(format!("Failed to save {name}: {e}"));
                cx.notify();
                false
            }
        }
    }

    /// Close a tab; a dirty one is saved first (and stays open if that fails).
    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() || (self.tabs[ix].dirty && !self.save(ix, cx)) {
            return;
        }
        self.tabs.remove(ix);
        self.active = match self.active {
            _ if self.tabs.is_empty() => None,
            Some(a) if a > ix => Some(a - 1),
            Some(a) if a == ix => Some(ix.min(self.tabs.len() - 1)),
            other => other,
        };
        self.changed(cx);
    }

    fn close_where(&mut self, keep: impl Fn(usize) -> bool, cx: &mut Context<Self>) {
        for i in (0..self.tabs.len()).rev() {
            if !keep(i) {
                self.close_tab(i, cx);
            }
        }
    }

    /// Reload open files changed on disk, unless they have unsaved edits.
    fn reload_changed(&mut self, paths: &[PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        for t in self.tabs.iter_mut().filter(|t| !t.dirty && paths.contains(&t.path)) {
            if let Ok(bytes) = std::fs::read(&t.path) {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if text != t.saved {
                    t.saved = text.clone();
                    t.editor.update(cx, |e, cx| e.set_value(text, window, cx));
                }
            }
        }
    }

    fn watch(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        use notify::Watcher as _;
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<PathBuf>();
        let watch_root = root.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                for p in ev.paths {
                    if tree::watched(&watch_root, &p) {
                        let _ = tx.unbounded_send(p);
                    }
                }
            }
        });
        let Ok(mut watcher) = watcher else { return };
        if watcher.watch(&root, notify::RecursiveMode::Recursive).is_err() {
            return;
        }
        self._watcher = Some(watcher);
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(first) = rx.next().await {
                cx.background_executor().timer(WATCH_DEBOUNCE).await;
                let mut changed = vec![first];
                while let Ok(p) = rx.try_recv() {
                    changed.push(p);
                }
                let ok = this.update(cx, |this, cx| {
                    let mut dirs: Vec<PathBuf> = changed.iter().filter_map(|p| p.parent().map(Path::to_path_buf)).collect();
                    dirs.sort();
                    dirs.dedup();
                    for d in &dirs {
                        this.tree.refresh(d);
                    }
                    // Open files are reloaded on the next frame, where the window is at hand.
                    this.pending_reload.extend(changed);
                    cx.notify();
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    // ── Explorer actions ────────────────────────────────────────────────────

    fn click_node(&mut self, path: PathBuf, is_dir: bool, ev: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = ev.modifiers();
        self.tree_focus.focus(window, cx);
        self.menu = None;
        if m.platform || m.control {
            if !self.tree.selected.remove(&path) {
                self.tree.selected.insert(path.clone());
            }
            self.tree.focused = Some(path);
            self.changed(cx);
            return;
        }
        if m.shift
            && let Some(anchor) = self.tree.focused.clone()
        {
            let rows = self.tree.rows();
            let (a, b) = (rows.iter().position(|(_, n)| n.path == anchor), rows.iter().position(|(_, n)| n.path == path));
            if let (Some(a), Some(b)) = (a, b) {
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                self.tree.selected = rows[lo..=hi].iter().map(|(_, n)| n.path.clone()).collect();
                self.changed(cx);
                return;
            }
        }
        self.tree.selected = [path.clone()].into();
        self.tree.focused = Some(path.clone());
        if is_dir {
            self.tree.toggle(&path);
            self.changed(cx);
        } else {
            self.open(path, false, true, window, cx);
        }
    }

    fn tree_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.inline.is_some() {
            return;
        }
        let rows = self.tree.rows();
        if rows.is_empty() {
            return;
        }
        let cur = self.tree.focused.as_ref().and_then(|f| rows.iter().position(|(_, n)| &n.path == f));
        let shift = ev.keystroke.modifiers.shift;
        let move_to = |this: &mut Self, i: usize| {
            let p = rows[i].1.path.clone();
            if shift {
                this.tree.selected.insert(p.clone());
            }
            this.tree.focused = Some(p);
        };
        match ev.keystroke.key.as_str() {
            "up" => move_to(self, cur.map_or(0, |i| i.saturating_sub(1))),
            "down" => move_to(self, cur.map_or(0, |i| (i + 1).min(rows.len() - 1))),
            "home" => move_to(self, 0),
            "end" => move_to(self, rows.len() - 1),
            "right" => {
                if let Some(i) = cur
                    && rows[i].1.is_dir
                {
                    let p = rows[i].1.path.clone();
                    if self.tree.expanded.contains(&p) {
                        if rows.get(i + 1).is_some_and(|(d, _)| *d > rows[i].0) {
                            move_to(self, i + 1);
                        }
                    } else {
                        self.tree.expand(&p);
                    }
                }
            }
            "left" => {
                if let Some(i) = cur {
                    let p = rows[i].1.path.clone();
                    if rows[i].1.is_dir && self.tree.expanded.contains(&p) {
                        self.tree.expanded.remove(&p);
                    } else if let Some(parent) = p.parent()
                        && let Some(j) = rows.iter().position(|(_, n)| n.path == parent)
                    {
                        move_to(self, j);
                    }
                }
            }
            "enter" | "f2" => {
                if let Some(i) = cur {
                    let p = rows[i].1.path.clone();
                    self.start_inline(Inline::Rename(p), window, cx);
                }
            }
            "space" => {
                if let Some(i) = cur
                    && !rows[i].1.is_dir
                {
                    let p = rows[i].1.path.clone();
                    self.open(p, true, false, window, cx);
                }
            }
            "a" if ev.keystroke.modifiers.control || ev.keystroke.modifiers.platform => {
                self.tree.selected = rows.iter().map(|(_, n)| n.path.clone()).collect();
            }
            _ => return,
        }
        cx.stop_propagation();
        self.changed(cx);
    }

    fn start_inline(&mut self, what: Inline, window: &mut Window, cx: &mut Context<Self>) {
        let (value, ph) = match &what {
            Inline::NewFile(_) => (String::new(), "File name..."),
            Inline::NewFolder(_) => (String::new(), "Folder name..."),
            Inline::Rename(p) => (p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), ""),
        };
        self.menu = None;
        self.inline = Some(what);
        self.inline_input.update(cx, |i, cx| {
            i.set_placeholder(ph, window, cx);
            i.set_value(value, window, cx);
            i.focus(window, cx);
        });
        cx.notify();
    }

    fn commit_inline(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(what) = self.inline.take() else { return };
        cx.notify();
        if name.is_empty() {
            return;
        }
        let (result, parent, target, open) = match &what {
            Inline::NewFile(dir) => {
                let p = dir.join(&name);
                (std::fs::OpenOptions::new().write(true).create_new(true).open(&p).map(|_| ()), dir.clone(), p, true)
            }
            Inline::NewFolder(dir) => {
                let p = dir.join(&name);
                (std::fs::create_dir(&p), dir.clone(), p, false)
            }
            Inline::Rename(old) => {
                let parent = old.parent().map(Path::to_path_buf).unwrap_or_default();
                let p = parent.join(&name);
                if p == *old {
                    return;
                }
                let r = std::fs::rename(old, &p);
                if r.is_ok() {
                    for t in self.tabs.iter_mut().filter(|t| t.path.starts_with(old)) {
                        t.path = p.join(t.path.strip_prefix(old).unwrap_or(Path::new("")));
                    }
                }
                (r, parent, p, false)
            }
        };
        if let Err(e) = result {
            log::error!("{}: {e}", target.display());
            return;
        }
        self.tree.load(&parent);
        self.tree.refresh(&parent);
        self.tree.expanded.insert(parent);
        self.tree.selected = [target.clone()].into();
        self.tree.focused = Some(target.clone());
        if open {
            self.open(target, false, true, window, cx);
        }
        self.changed(cx);
    }

    fn delete(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        for p in &paths {
            if let Err(e) = trash::delete(p) {
                log::error!("trash {}: {e}", p.display());
            }
            if let Some(parent) = p.parent() {
                self.tree.refresh(parent);
            }
        }
        self.menu = None;
        self.changed(cx);
    }

    /// The paths a tree menu acts on: the selection when the clicked row is in it.
    fn targets(&self, clicked: &Path) -> Vec<PathBuf> {
        if self.tree.selected.contains(clicked) && self.tree.selected.len() > 1 {
            self.tree.selected.iter().cloned().collect()
        } else {
            vec![clicked.to_owned()]
        }
    }
}

// ── Rendering ───────────────────────────────────────────────────────────────

fn file_icon(path: &Path, size: f32) -> Div {
    let (glyph, color) = files::icon(path);
    div()
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px((size * 0.78).round().max(10.)))
        .font_weight(FontWeight::BOLD)
        .text_color(hex(color))
        .child(glyph)
}

impl Render for IdePane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.pending_reload.is_empty() {
            let paths = std::mem::take(&mut self.pending_reload);
            self.reload_changed(&paths, window, cx);
        }
        let explorer = (!self.sidebar_collapsed).then(|| self.render_explorer(window, cx));
        let menu = self.menu.clone().map(|(m, at)| self.render_menu(m, at, cx));
        div()
            .size_full()
            .key_context("Ide")
            .flex()
            .bg(BG_PRIMARY)
            .text_color(TEXT_PRIMARY)
            .on_action(cx.listener(|this, _: &SaveFile, _, cx| {
                if let Some(i) = this.active {
                    this.save(i, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| {
                if let Some(i) = this.active {
                    this.close_tab(i, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleComment, window, cx| {
                if let Some(t) = this.active.and_then(|i| this.tabs.get(i)) {
                    let (ed, path) = (t.editor.clone(), t.path.clone());
                    toggle_comment(&ed, &path, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleExplorer, _, cx| {
                this.sidebar_collapsed = !this.sidebar_collapsed;
                this.changed(cx);
            }))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if this.resizing {
                    if ev.pressed_button != Some(MouseButton::Left) {
                        this.resizing = false;
                        this.changed(cx);
                        return;
                    }
                    let left = this.tree_focus_left();
                    this.sidebar_width = (crate::ui::unpx(ev.position.x) - left).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
                    cx.notify();
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| {
                if this.resizing {
                    this.resizing = false;
                    this.changed(cx);
                }
            }))
            .children(explorer)
            .when(!self.sidebar_collapsed, |d| {
                d.child(
                    div()
                        .id("ide-resize")
                        .w(px(6.))
                        .h_full()
                        .flex_none()
                        .flex()
                        .justify_center()
                        .cursor_col_resize()
                        .child(div().h_full().w(px(if self.resizing { 2. } else { 1. })).bg(if self.resizing { c(TEXT_MUTED) } else { c(BORDER_SUBTLE) }))
                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            this.resizing = true;
                            cx.stop_propagation();
                            cx.notify();
                        })),
                )
            })
            .child(self.render_editor_column(window, cx))
            .children(menu)
    }
}

impl IdePane {
    /// The explorer's left edge in window coordinates (for the resize drag).
    fn tree_focus_left(&self) -> f32 {
        self.explorer_left.get()
    }

    fn render_explorer(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.tree.rows();
        let fs = self.font_size;
        let row_h = (fs * 1.7).round();
        let header_btn = |id: &'static str, name: &'static str, tip: &'static str| {
            let group = SharedString::from(format!("hb-{id}"));
            div()
                .id(id)
                .px(px(4.))
                .py(px(2.))
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|d| d.bg(BG_HOVER))
                .group(group.clone())
                .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx))
                .child(svg().path(icon(name)).size(px(14.)).text_color(TEXT_MUTED).group_hover(group, |s| s.text_color(TEXT_PRIMARY)))
        };
        let root = self.tree.root.clone();
        let (r1, r2) = (root.clone(), root.clone());
        let header = div()
            .px(px(12.))
            .pt(px(8.))
            .pb(px(4.))
            .flex()
            .items_center()
            .text_size(px(11.))
            .font_weight(FontWeight::BOLD)
            .text_color(TEXT_MUTED)
            .child(div().flex_1().child("EXPLORER"))
            .child(
                div()
                    .flex()
                    .gap(px(2.))
                    .child(header_btn("ide-new-file", "new-file", "New File").on_click(cx.listener(move |this, _, window, cx| this.start_inline(Inline::NewFile(r1.clone()), window, cx))))
                    .child(header_btn("ide-new-folder", "new-folder", "New Folder").on_click(cx.listener(move |this, _, window, cx| this.start_inline(Inline::NewFolder(r2.clone()), window, cx))))
                    .child(header_btn("ide-refresh", "refresh", "Refresh").on_click(cx.listener(|this, _, _, cx| {
                        this.tree.refresh_all();
                        cx.notify();
                    }))),
            );

        let inline_field = || {
            div()
                .flex_1()
                .px(px(2.))
                .rounded(px(2.))
                .bg(hex(0x1e1e1e))
                .border_1()
                .border_color(BORDER)
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.inline = None;
                        cx.notify();
                        cx.stop_propagation();
                    }
                }))
                .child(Input::new(&self.inline_input).appearance(false).xsmall())
        };

        let mut list: Vec<AnyElement> = Vec::new();
        if matches!(self.inline, Some(Inline::NewFile(_)) | Some(Inline::NewFolder(_))) {
            list.push(div().px(px(12.)).py(px(2.)).child(inline_field()).into_any_element());
        }
        let focused_tree = self.tree_focus.is_focused(window);
        for (depth, node) in rows {
            let selected = self.tree.selected.contains(&node.path) || (focused_tree && self.tree.focused.as_ref() == Some(&node.path));
            let renaming = self.inline == Some(Inline::Rename(node.path.clone()));
            let expanded = self.tree.expanded.contains(&node.path);
            let (p_click, p_menu) = (node.path.clone(), node.path.clone());
            let is_dir = node.is_dir;
            let icon_el: AnyElement = if is_dir {
                svg().path(icon(if expanded { "folder-open" } else { "folder" })).size(px(16.)).flex_none().text_color(hexa(0x7a797a, if expanded { 0.9 } else { 0.85 })).into_any_element()
            } else {
                file_icon(&node.path, 16.).into_any_element()
            };
            list.push(
                div()
                    .id(SharedString::from(format!("node-{}", node.path.display())))
                    .h(px(row_h))
                    .pl(px(4. + 17. * depth as f32))
                    .pr(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .when(selected, |d| d.bg(BG_HOVER))
                    .hover(|d| d.bg(BG_HOVER))
                    .child(
                        div().w(px(18.)).flex_none().flex().justify_center().when(is_dir, |d| {
                            d.child(svg().path(icon(if expanded { "chevron-down" } else { "chevron-right" })).size(px(12.)).text_color(TEXT_MUTED))
                        }),
                    )
                    .child(icon_el)
                    .map(|d| if renaming { d.child(inline_field()) } else { d.child(div().min_w_0().truncate().child(node.name.clone())) })
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| this.click_node(p_click.clone(), is_dir, ev, window, cx)))
                    .on_mouse_down(MouseButton::Right, cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.tree_focus.focus(window, cx);
                        if !this.tree.selected.contains(&p_menu) {
                            this.tree.selected = [p_menu.clone()].into();
                        }
                        this.tree.focused = Some(p_menu.clone());
                        this.menu = Some((Menu::Tree(Some(p_menu.clone())), ev.position));
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        let left_cell = self.explorer_left.clone();
        let recorder = canvas(move |b, _, _| left_cell.set(crate::ui::unpx(b.left())), |_, _, _, _| {}).absolute().size_full();
        div()
            .relative()
            .w(px(self.sidebar_width))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .bg(BG_PRIMARY)
            .text_size(px(fs))
            .child(recorder)
            .child(header)
            .child(
                div()
                    .id("ide-tree")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_focus(&self.tree_focus)
                    .on_key_down(cx.listener(Self::tree_key))
                    .on_mouse_down(MouseButton::Right, cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                        let target = this.tree.selected.iter().next().cloned().or_else(|| this.tree.focused.clone());
                        this.menu = Some((Menu::Tree(target), ev.position));
                        cx.notify();
                    }))
                    .children(list),
            )
            .into_any_element()
    }

    fn render_editor_column(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let tabs: Vec<AnyElement> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let active = self.active == Some(i);
                let name = t.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let group = SharedString::from(format!("tabx-{i}"));
                div()
                    .id(SharedString::from(format!("tab-{}", t.path.display())))
                    .h_full()
                    .max_w(px(200.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .border_r_1()
                    .border_color(BORDER)
                    .text_size(px(12.))
                    .cursor_pointer()
                    .map(|d| if active { d.bg(EDITOR_BG).text_color(TEXT_PRIMARY) } else { d.text_color(TEXT_MUTED).hover(|d| d.bg(BG_HOVER)) })
                    .child(file_icon(&t.path, 14.))
                    .when(t.dirty, |d| d.child(div().mr(px(2.)).text_color(TEXT_MUTED).child("•")))
                    .child(div().min_w_0().truncate().when(t.preview, |d| d.italic()).child(name))
                    .child(
                        div()
                            .id(SharedString::from(format!("tabclose-{i}")))
                            .size(px(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .text_size(px(14.))
                            .text_color(TEXT_MUTED)
                            .group(group.clone())
                            .when(!active, |d| d.opacity(0.).hover(|d| d.opacity(1.)))
                            .hover(|d| d.bg(BG_HOVER))
                            .child("×")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_tab(i, cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        this.active = Some(i);
                        if ev.click_count() >= 2 {
                            this.tabs[i].preview = false;
                        }
                        let ed = this.tabs[i].editor.clone();
                        ed.update(cx, |e, cx| e.focus(window, cx));
                        this.changed(cx);
                        cx.emit(IdeEvent::Cursor);
                    }))
                    .on_mouse_down(MouseButton::Middle, cx.listener(move |this, _, _, cx| this.close_tab(i, cx)))
                    .on_mouse_down(MouseButton::Right, cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        this.menu = Some((Menu::Tab(i), ev.position));
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();

        let tab_bar = div()
            .id("ide-tabs")
            .h(px(30.))
            .flex_none()
            .flex()
            .overflow_x_scroll()
            .bg(BG_PRIMARY)
            .border_b_1()
            .border_color(BORDER)
            .children(tabs);

        let body: AnyElement = match self.active.and_then(|i| self.tabs.get(i)) {
            Some(tab) => Editor::new(&tab.editor)
                .appearance(false)
                .bordered(false)
                .font_family(EDITOR_FONT)
                .text_size(px(self.font_size))
                .line_height(px((self.font_size * 1.5).round()))
                .size_full()
                .into_any_element(),
            None => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(16.))
                .bg(EDITOR_BG)
                .text_color(TEXT_MUTED)
                .child(div().text_size(px(48.)).opacity(0.15).child("</>"))
                .child(div().text_size(px(14.)).opacity(0.4).child("Open a file to start editing"))
                .child(div().text_size(px(12.)).opacity(0.3).child("Use the file explorer on the left"))
                .into_any_element(),
        };

        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(tab_bar)
            .when_some(self.error.clone(), |d, e| {
                d.child(div().px(px(10.)).py(px(5.)).bg(hexa(0xf14c4c, 0.12)).text_size(px(11.)).text_color(ACCENT_RED).child(e))
            })
            .child(div().flex_1().min_h_0().bg(EDITOR_BG).pt(px(8.)).child(body))
            .into_any_element()
    }

    fn render_menu(&self, menu: Menu, at: Point<Pixels>, cx: &mut Context<Self>) -> AnyElement {
        let item = |id: SharedString, label: String, shortcut: Option<&'static str>, danger: bool| {
            div()
                .id(id)
                .pl(px(8.))
                .pr(px(12.))
                .py(px(5.))
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(12.))
                .text_color(if danger { ACCENT_RED } else { TEXT_PRIMARY })
                .cursor_pointer()
                .hover(|d| d.bg(BG_HOVER))
                .child(div().w(px(16.)).flex_none())
                .child(div().flex_1().child(label))
                .when_some(shortcut, |d, s| d.child(div().ml(px(16.)).text_size(px(11.)).text_color(TEXT_MUTED).child(s)))
        };
        let sep = || div().mx(px(8.)).my(px(4.)).h(px(1.)).bg(BORDER);
        let mut items: Vec<AnyElement> = Vec::new();
        match menu {
            Menu::Tree(target) => {
                let clicked = target.unwrap_or_else(|| self.tree.root.clone());
                let is_dir = clicked.is_dir();
                let targets = self.targets(&clicked);
                let multi = targets.len() > 1;
                if is_dir {
                    let (a, b) = (clicked.clone(), clicked.clone());
                    items.push(item("m-new-file".into(), "New File".into(), None, false).on_click(cx.listener(move |this, _, window, cx| this.start_inline(Inline::NewFile(a.clone()), window, cx))).into_any_element());
                    items.push(item("m-new-folder".into(), "New Folder".into(), None, false).on_click(cx.listener(move |this, _, window, cx| this.start_inline(Inline::NewFolder(b.clone()), window, cx))).into_any_element());
                    items.push(sep().into_any_element());
                }
                if !multi {
                    let r = clicked.clone();
                    items.push(item("m-rename".into(), "Rename".into(), Some("F2"), false).on_click(cx.listener(move |this, _, window, cx| this.start_inline(Inline::Rename(r.clone()), window, cx))).into_any_element());
                }
                let copy = targets.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n");
                let label = if multi { format!("Copy {} Paths", targets.len()) } else { "Copy Path".into() };
                items.push(item("m-copy".into(), label, None, false).on_click(cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                    this.menu = None;
                    cx.notify();
                })).into_any_element());
                items.push(sep().into_any_element());
                let del = targets.clone();
                let label = if multi { format!("Delete {} Items", targets.len()) } else { "Delete".into() };
                items.push(item("m-delete".into(), label, None, true).on_click(cx.listener(move |this, _, _, cx| this.delete(del.clone(), cx))).into_any_element());
                items.push(sep().into_any_element());
                let dir = if is_dir { clicked.clone() } else { clicked.parent().map(Path::to_path_buf).unwrap_or_default() };
                items.push(item("m-refresh".into(), "Refresh".into(), None, false).on_click(cx.listener(move |this, _, _, cx| {
                    this.tree.refresh(&dir);
                    this.menu = None;
                    cx.notify();
                })).into_any_element());
            }
            Menu::Tab(i) => {
                let path = self.tabs.get(i).map(|t| t.path.to_string_lossy().into_owned()).unwrap_or_default();
                items.push(item("t-close".into(), "Close".into(), Some("⌘W"), false).on_click(cx.listener(move |this, _, _, cx| {
                    this.menu = None;
                    this.close_tab(i, cx);
                })).into_any_element());
                items.push(item("t-others".into(), "Close Others".into(), None, false).on_click(cx.listener(move |this, _, _, cx| {
                    this.menu = None;
                    this.close_where(|j| j == i, cx);
                })).into_any_element());
                items.push(item("t-all".into(), "Close All".into(), None, false).on_click(cx.listener(|this, _, _, cx| {
                    this.menu = None;
                    this.close_where(|_| false, cx);
                })).into_any_element());
                items.push(item("t-right".into(), "Close to the Right".into(), None, false).on_click(cx.listener(move |this, _, _, cx| {
                    this.menu = None;
                    this.close_where(|j| j <= i, cx);
                })).into_any_element());
                items.push(sep().into_any_element());
                items.push(item("t-copy".into(), "Copy Path".into(), None, false).on_click(cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                    this.menu = None;
                    cx.notify();
                })).into_any_element());
            }
        }
        deferred(
            anchored().position(at).snap_to_window_with_margin(px(4.)).child(
                div()
                    .id("ide-menu")
                    .min_w(px(200.))
                    .py(px(4.))
                    .flex()
                    .flex_col()
                    .bg(BG_SURFACE)
                    .border_1()
                    .border_color(BORDER)
                    .rounded(px(6.))
                    .shadow(vec![BoxShadow { color: c(hexa(0, 0.4)), offset: point(px(0.), px(8.)), blur_radius: px(24.), spread_radius: px(0.), inset: false }])
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.menu = None;
                        cx.notify();
                    }))
                    .children(items),
            ),
        )
        .with_priority(1)
        .into_any_element()
    }
}
