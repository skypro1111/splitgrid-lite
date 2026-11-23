//! The SFTP pane (SFTPPanel + FileTable in the Electron app): Local on the
//! left, Remote on the right, a transfer strip between them. Each half lists a
//! directory with breadcrumbs, "+ New folder" and "⟳ Refresh", sortable
//! columns that drop out as the half narrows, multi-selection, keyboard
//! navigation, a context menu, inline new-folder / rename, and delete with a
//! confirmation (local deletes go to the Trash). Files dropped from Finder
//! upload (remote) or copy (local).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// Zoom-aware: scales with the whole-window UI zoom (crate::ui).
use crate::ui::px;

use crate::sftp::{self, Entry, Progress, SftpClient, TransferResult};
use crate::sftp_format;
use crate::ssh::{HostKeyChecker, SshParams};
use crate::theme::*;

const FONT: f32 = 13.;
const MONO: &str = "Menlo";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SideKind {
    Local,
    Remote,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    Name,
    Size,
    Modified,
    Mode,
}

#[derive(Clone, PartialEq)]
enum Inline {
    NewFolder,
    Rename(String),
}

#[derive(Clone, PartialEq)]
enum Status {
    Connecting,
    Ready,
    Failed(String),
}

struct Side {
    kind: SideKind,
    path: String,
    entries: Vec<Entry>,
    loading: bool,
    error: Option<String>,
    op_error: Option<String>,
    selected: BTreeSet<String>,
    anchor: Option<usize>,
    sort: (Col, bool),
    confirm_delete: bool,
    busy: bool,
    /// Only the latest listing is applied.
    token: u64,
    focus: FocusHandle,
    /// The half's width last frame, to drop columns as it narrows.
    width: std::rc::Rc<std::cell::Cell<f32>>,
}

impl Side {
    fn new(kind: SideKind, cx: &mut Context<SftpPane>) -> Self {
        Self {
            kind,
            path: String::new(),
            entries: Vec::new(),
            loading: false,
            error: None,
            op_error: None,
            selected: BTreeSet::new(),
            anchor: None,
            sort: (Col::Name, true),
            confirm_delete: false,
            busy: false,
            token: 0,
            focus: cx.focus_handle(),
            width: Default::default(),
        }
    }

    fn sorted(&self) -> Vec<Entry> {
        let mut v = self.entries.clone();
        let (col, asc) = self.sort;
        v.sort_by(|a, b| {
            let by_name = || a.name.to_lowercase().cmp(&b.name.to_lowercase());
            let ord = match col {
                Col::Name => by_name(),
                Col::Size => a.size.cmp(&b.size).then_with(by_name),
                Col::Modified => a.modified.cmp(&b.modified).then_with(by_name),
                Col::Mode => a.mode.cmp(&b.mode).then_with(by_name),
            };
            // Folders first whatever the direction.
            b.is_dir.cmp(&a.is_dir).then(if asc { ord } else { ord.reverse() })
        });
        v
    }

    fn join(&self, name: &str) -> String {
        match self.kind {
            SideKind::Remote => sftp::join(&self.path, name),
            SideKind::Local => Path::new(&self.path).join(name).to_string_lossy().into_owned(),
        }
    }

    fn parent(&self) -> Option<String> {
        match self.kind {
            SideKind::Remote => (self.path != "/").then(|| sftp::parent(&self.path)),
            SideKind::Local => Path::new(&self.path).parent().map(|p| p.to_string_lossy().into_owned()),
        }
    }
}

/// Files the remote editor won't open (the Electron app's limit).
const EDIT_MAX: u64 = 5 * 1024 * 1024;

enum EditorBody {
    Loading,
    Failed(String),
    Ready(Entity<EditorState>),
}

/// RemoteFileEditor: a remote file open over both halves.
struct RemoteEditor {
    path: String,
    body: EditorBody,
    saved: String,
    dirty: bool,
    saving: bool,
    error: Option<String>,
    /// "Saved" shows for a moment after a save.
    saved_flash: bool,
    confirm_discard: bool,
    _sub: Option<Subscription>,
}

struct Transfer {
    upload: bool,
    progress: Option<Progress>,
    cancel: Arc<AtomicBool>,
}

pub enum SftpEvent {
    /// Remember where the halves are.
    Paths { remote: String, local: String },
    /// "Change connection": back to the picker.
    ChangeConnection,
}

impl EventEmitter<SftpEvent> for SftpPane {}

pub struct SftpPane {
    pub label: String,
    params: SshParams,
    check: HostKeyChecker,
    status: Status,
    client: Option<Arc<SftpClient>>,
    local: Side,
    remote: Side,
    transfer: Option<Transfer>,
    transfer_error: Option<String>,
    menu: Option<(SideKind, Point<Pixels>)>,
    inline: Option<(SideKind, Inline)>,
    inline_input: Entity<InputState>,
    _inline_sub: Subscription,
    editor: Option<RemoteEditor>,
}

impl SftpPane {
    pub fn new(
        label: String,
        params: SshParams,
        check: HostKeyChecker,
        remote_path: Option<String>,
        local_path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let inline_input = cx.new(|cx| InputState::new(window, cx));
        let sub = cx.subscribe_in(&inline_input, window, |this, input, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                let text = input.read(cx).value().trim().to_string();
                this.commit_inline(text, cx);
            }
        });
        let mut this = Self {
            label,
            params,
            check,
            status: Status::Connecting,
            client: None,
            local: Side::new(SideKind::Local, cx),
            remote: Side::new(SideKind::Remote, cx),
            transfer: None,
            transfer_error: None,
            menu: None,
            inline: None,
            inline_input,
            _inline_sub: sub,
            editor: None,
        };
        this.local.path = local_path;
        this.remote.path = remote_path.unwrap_or_default();
        this.connect(cx);
        this
    }

    /// A context menu is open (it may spill over a browser pane's web view).
    /// The open context menu, for the browser panes it may cover.
    pub(crate) fn menu_cover(&self) -> Option<crate::overlay::Cover> {
        self.menu
            .as_ref()
            .map(|(_, at)| crate::overlay::Cover::menu(*at))
    }

    /// The pane title: "label — /remote/path" once connected.
    pub fn title(&self) -> String {
        if self.status == Status::Ready { format!("{} — {}", self.label, self.remote.path) } else { self.label.clone() }
    }

    fn side(&mut self, kind: SideKind) -> &mut Side {
        match kind {
            SideKind::Local => &mut self.local,
            SideKind::Remote => &mut self.remote,
        }
    }

    fn side_ref(&self, kind: SideKind) -> &Side {
        match kind {
            SideKind::Local => &self.local,
            SideKind::Remote => &self.remote,
        }
    }

    // ── Connection ──────────────────────────────────────────────────────────

    fn connect(&mut self, cx: &mut Context<Self>) {
        self.status = Status::Connecting;
        let (params, check, remembered) = (self.params.clone(), self.check.clone(), self.remote.path.clone());
        cx.spawn(async move |this, cx| {
            let result = sftp::call(async move {
                let client = SftpClient::open(params, check).await?;
                // The remembered directory if it still exists, else home.
                let start = if !remembered.is_empty() && client.is_dir(&remembered).await.unwrap_or(false) {
                    remembered
                } else {
                    client.home().await?
                };
                Ok((client, start))
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((client, start)) => {
                        this.client = Some(client);
                        this.status = Status::Ready;
                        this.remote.path = start;
                        this.load(SideKind::Remote, cx);
                        this.load(SideKind::Local, cx);
                        this.emit_paths(cx);
                    }
                    Err(e) => this.status = Status::Failed(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn emit_paths(&self, cx: &mut Context<Self>) {
        cx.emit(SftpEvent::Paths { remote: self.remote.path.clone(), local: self.local.path.clone() });
    }

    // ── Listing ─────────────────────────────────────────────────────────────

    fn load(&mut self, kind: SideKind, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let side = self.side(kind);
        side.loading = true;
        side.error = None;
        side.token += 1;
        let (token, path) = (side.token, side.path.clone());
        cx.spawn(async move |this, cx| {
            let result = match (kind, client) {
                (SideKind::Remote, Some(client)) => {
                    let p = path.clone();
                    sftp::call(async move { client.list(&p).await }).await
                }
                (SideKind::Remote, None) => Err(anyhow::anyhow!("not connected")),
                (SideKind::Local, _) => {
                    let p = PathBuf::from(&path);
                    cx.background_executor().spawn(async move { sftp::list_local(&p) }).await
                }
            };
            let _ = this.update(cx, |this, cx| {
                let side = this.side(kind);
                if side.token != token {
                    return;
                }
                side.loading = false;
                side.selected.clear();
                side.anchor = None;
                match result {
                    Ok(entries) => side.entries = entries,
                    Err(e) => {
                        side.entries.clear();
                        side.error = Some(format!("{e:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn navigate(&mut self, kind: SideKind, path: String, cx: &mut Context<Self>) {
        let side = self.side(kind);
        side.path = path;
        side.op_error = None;
        side.confirm_delete = false;
        self.menu = None;
        self.inline = None;
        self.load(kind, cx);
        self.emit_paths(cx);
    }

    // ── Selection and keys ──────────────────────────────────────────────────

    fn click_row(&mut self, kind: SideKind, ix: usize, ev: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.side_ref(kind).sorted();
        let Some(entry) = rows.get(ix).cloned() else { return };
        if ev.click_count() >= 2 {
            let path = self.side_ref(kind).join(&entry.name);
            if entry.is_dir {
                self.navigate(kind, path, cx);
            } else if kind == SideKind::Remote {
                self.open_remote(path, entry.size, window, cx);
            }
            return;
        }
        let m = ev.modifiers();
        let side = self.side(kind);
        side.focus.focus(window, cx);
        if m.platform || m.control {
            if !side.selected.remove(&entry.name) {
                side.selected.insert(entry.name.clone());
            }
            side.anchor = Some(ix);
        } else if m.shift && let Some(a) = side.anchor {
            let (lo, hi) = if a <= ix { (a, ix) } else { (ix, a) };
            side.selected = rows[lo..=hi].iter().map(|e| e.name.clone()).collect();
        } else {
            side.selected = [entry.name.clone()].into();
            side.anchor = Some(ix);
        }
        self.menu = None;
        cx.notify();
    }

    fn on_key(&mut self, kind: SideKind, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.inline.is_some() || self.side_ref(kind).confirm_delete {
            return;
        }
        let rows = self.side_ref(kind).sorted();
        let side = self.side(kind);
        let current = side.selected.iter().next().and_then(|n| rows.iter().position(|e| &e.name == n));
        match ev.keystroke.key.as_str() {
            "up" | "down" if !rows.is_empty() => {
                let next = match (current, ev.keystroke.key.as_str()) {
                    (None, _) => 0,
                    (Some(i), "up") => i.saturating_sub(1),
                    (Some(i), _) => (i + 1).min(rows.len() - 1),
                };
                side.selected = [rows[next].name.clone()].into();
                side.anchor = Some(next);
                cx.notify();
            }
            "enter" => {
                if let Some(i) = current
                    && side.selected.len() == 1
                {
                    let path = side.join(&rows[i].name);
                    if rows[i].is_dir {
                        self.navigate(kind, path, cx);
                    } else if kind == SideKind::Remote {
                        self.open_remote(path, rows[i].size, window, cx);
                    }
                }
            }
            "escape" => {
                self.menu = None;
                cx.notify();
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    // ── Operations ──────────────────────────────────────────────────────────

    fn start_inline(&mut self, kind: SideKind, what: Inline, window: &mut Window, cx: &mut Context<Self>) {
        let (value, ph) = match &what {
            Inline::NewFolder => (String::new(), "New folder name"),
            Inline::Rename(n) => (n.clone(), ""),
        };
        self.menu = None;
        self.inline = Some((kind, what));
        self.inline_input.update(cx, |i, cx| {
            i.set_placeholder(ph, window, cx);
            i.set_value(value, window, cx);
            i.focus(window, cx);
        });
        cx.notify();
    }

    fn commit_inline(&mut self, text: String, cx: &mut Context<Self>) {
        let Some((kind, what)) = self.inline.take() else { return };
        cx.notify();
        if text.is_empty() || matches!(&what, Inline::Rename(old) if *old == text) {
            return;
        }
        let side = self.side_ref(kind);
        let (from, to) = match &what {
            Inline::NewFolder => (None, side.join(&text)),
            Inline::Rename(old) => (Some(side.join(old)), side.join(&text)),
        };
        self.run_op(kind, cx, move |client| async move {
            match (kind, from) {
                (SideKind::Remote, None) => client.expect("connected").mkdir(&to).await,
                (SideKind::Remote, Some(from)) => client.expect("connected").rename(&from, &to).await,
                (SideKind::Local, None) => Ok(std::fs::create_dir(&to)?),
                (SideKind::Local, Some(from)) => Ok(std::fs::rename(&from, &to)?),
            }
        });
    }

    fn delete_selected(&mut self, kind: SideKind, cx: &mut Context<Self>) {
        let side = self.side(kind);
        side.confirm_delete = false;
        let paths: Vec<String> = side.selected.iter().map(|n| side.join(n)).collect();
        self.run_op(kind, cx, move |client| async move {
            for p in paths {
                match kind {
                    SideKind::Remote => client.as_ref().expect("connected").remove(&p).await?,
                    SideKind::Local => trash::delete(&p).map_err(|e| anyhow::anyhow!("{p}: {e}"))?,
                }
            }
            Ok(())
        });
    }

    /// Run a file operation on the right executor, then reload that half.
    fn run_op<F, Fut>(&mut self, kind: SideKind, cx: &mut Context<Self>, op: F)
    where
        F: FnOnce(Option<Arc<SftpClient>>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let client = self.client.clone();
        let side = self.side(kind);
        side.busy = true;
        side.op_error = None;
        cx.spawn(async move |this, cx| {
            let result = sftp::call(op(client)).await;
            let _ = this.update(cx, |this, cx| {
                let side = this.side(kind);
                side.busy = false;
                if let Err(e) = result {
                    side.op_error = Some(format!("{e:#}"));
                }
                this.load(kind, cx);
            });
        })
        .detach();
        cx.notify();
    }

    // ── Transfers ───────────────────────────────────────────────────────────

    fn transfer(&mut self, upload: bool, cx: &mut Context<Self>) {
        if self.transfer.is_some() {
            return;
        }
        let Some(client) = self.client.clone() else { return };
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<Progress>();
        let job: std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<TransferResult>> + Send>> = if upload {
            let locals: Vec<PathBuf> = self.local.selected.iter().map(|n| PathBuf::from(self.local.join(n))).collect();
            let dest = self.remote.path.clone();
            let cancel = cancel.clone();
            Box::pin(async move { Ok(client.upload_many(locals, dest, &move |p| drop(tx.unbounded_send(p)), &cancel).await) })
        } else {
            let names: Vec<(String, bool)> = self
                .remote
                .entries
                .iter()
                .filter(|e| self.remote.selected.contains(&e.name))
                .map(|e| (self.remote.join(&e.name), e.is_dir))
                .collect();
            let dest = PathBuf::from(&self.local.path);
            let cancel = cancel.clone();
            Box::pin(async move { Ok(client.download_many(names, dest, &move |p| drop(tx.unbounded_send(p)), &cancel).await) })
        };
        self.transfer = Some(Transfer { upload, progress: None, cancel });
        self.transfer_error = None;
        cx.spawn(async move |this, cx| {
            use futures::{FutureExt as _, StreamExt as _};
            let mut done = Box::pin(sftp::call(job)).fuse();
            let result = loop {
                futures::select_biased! {
                    p = rx.next() => if let Some(p) = p {
                        let _ = this.update(cx, |this, cx| {
                            if let Some(t) = this.transfer.as_mut() { t.progress = Some(p); }
                            cx.notify();
                        });
                    },
                    r = done => break r,
                }
            };
            let _ = this.update(cx, |this, cx| {
                this.transfer = None;
                let what = if upload { "Upload" } else { "Download" };
                this.transfer_error = match result {
                    Err(e) => Some(format!("{what} failed: {e:#}")),
                    Ok(r) if r.cancelled => Some(format!("{what} cancelled after {}/{} files.", r.transferred, r.total)),
                    Ok(r) if !r.errors.is_empty() => Some(format!("{what} finished with {} error(s): {}", r.errors.len(), r.errors[0])),
                    Ok(_) => None,
                };
                this.load(if upload { SideKind::Remote } else { SideKind::Local }, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn drop_files(&mut self, kind: SideKind, paths: &ExternalPaths, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = paths.paths().to_vec();
        match kind {
            SideKind::Remote => {
                // Same path as the → button, with the dropped files as the selection.
                let Some(client) = self.client.clone() else { return };
                if self.transfer.is_some() {
                    return;
                }
                let dest = self.remote.path.clone();
                let cancel = Arc::new(AtomicBool::new(false));
                self.transfer = Some(Transfer { upload: true, progress: None, cancel: cancel.clone() });
                cx.spawn(async move |this, cx| {
                    let r = sftp::call(async move { Ok(client.upload_many(paths, dest, &|_| {}, &cancel).await) }).await;
                    let _ = this.update(cx, |this, cx| {
                        this.transfer = None;
                        this.transfer_error = match r {
                            Err(e) => Some(format!("Upload failed: {e:#}")),
                            Ok(r) if !r.errors.is_empty() => Some(format!("Upload finished with {} error(s): {}", r.errors.len(), r.errors[0])),
                            Ok(_) => None,
                        };
                        this.load(SideKind::Remote, cx);
                    });
                })
                .detach();
            }
            SideKind::Local => {
                let dest = PathBuf::from(&self.local.path);
                self.run_op(SideKind::Local, cx, move |_| async move {
                    let mut failed = Vec::new();
                    for p in &paths {
                        if let Some(name) = p.file_name()
                            && let Err(e) = copy_recursive(p, &dest.join(name))
                        {
                            failed.push(format!("{}: {e}", name.to_string_lossy()));
                        }
                    }
                    if failed.is_empty() { Ok(()) } else { Err(anyhow::anyhow!("Could not copy {} item(s): {}", failed.len(), failed[0])) }
                });
            }
        }
        cx.notify();
    }
}

impl SftpPane {
    fn open_remote(&mut self, path: String, size: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        self.editor = Some(RemoteEditor {
            path: path.clone(),
            body: if size > EDIT_MAX { EditorBody::Failed("File is larger than 5 MB — download it to edit.".into()) } else { EditorBody::Loading },
            saved: String::new(),
            dirty: false,
            saving: false,
            error: None,
            saved_flash: false,
            confirm_discard: false,
            _sub: None,
        });
        cx.notify();
        if size > EDIT_MAX {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let p = path.clone();
            let bytes = sftp::call(async move { client.read(&p).await }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(ed) = this.editor.as_mut().filter(|e| e.path == path) else { return };
                ed.body = match bytes {
                    Err(e) => EditorBody::Failed(format!("{e:#}")),
                    Ok(b) if b.contains(&0) => EditorBody::Failed("Binary file — download it to view.".into()),
                    Ok(b) => {
                        let text = String::from_utf8_lossy(&b).into_owned();
                        let lang = crate::ide::files::language(Path::new(&path)).0;
                        let state = cx.new(|cx| EditorState::new(window, cx).language(lang).default_value(text.clone()));
                        let sub = cx.subscribe(&state, |this, st, ev: &InputEvent, cx| {
                            if matches!(ev, InputEvent::Change) {
                                let value = st.read(cx).value();
                                if let Some(ed) = this.editor.as_mut() {
                                    ed.dirty = value.as_ref() != ed.saved;
                                }
                                cx.notify();
                            }
                        });
                        state.update(cx, |s, cx| s.focus(window, cx));
                        ed.saved = text;
                        ed._sub = Some(sub);
                        EditorBody::Ready(state)
                    }
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn save_remote(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        let Some(ed) = self.editor.as_mut() else { return };
        let EditorBody::Ready(state) = &ed.body else { return };
        if !ed.dirty || ed.saving {
            return;
        }
        let text = state.read(cx).value().to_string();
        ed.saving = true;
        ed.error = None;
        let path = ed.path.clone();
        cx.spawn(async move |this, cx| {
            let (p, t) = (path.clone(), text.clone());
            let result = sftp::call(async move { client.write(&p, t.as_bytes()).await }).await;
            let _ = this.update(cx, |this, cx| {
                let Some(ed) = this.editor.as_mut().filter(|e| e.path == path) else { return };
                ed.saving = false;
                match result {
                    Ok(()) => {
                        ed.saved = text;
                        if let EditorBody::Ready(st) = &ed.body {
                            ed.dirty = st.read(cx).value().as_ref() != ed.saved;
                        }
                        ed.saved_flash = true;
                        cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(std::time::Duration::from_millis(1600)).await;
                            let _ = this.update(cx, |this, cx| {
                                if let Some(ed) = this.editor.as_mut() {
                                    ed.saved_flash = false;
                                }
                                cx.notify();
                            });
                        })
                        .detach();
                    }
                    Err(e) => ed.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn close_remote(&mut self, force: bool, cx: &mut Context<Self>) {
        match self.editor.as_mut() {
            Some(ed) if ed.dirty && !force => ed.confirm_discard = true,
            _ => {
                self.editor = None;
                self.load(SideKind::Remote, cx);
            }
        }
        cx.notify();
    }

    fn render_remote_editor(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ed = self.editor.as_ref()?;
        let name = ed.path.rsplit('/').next().unwrap_or(&ed.path).to_owned();
        let btn = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px(px(12.))
                .py(px(4.))
                .rounded(px(5.))
                .border_1()
                .border_color(border())
                .bg(bg_surface())
                .text_size(px(11.))
                .text_color(TEXT_PRIMARY)
                .cursor_pointer()
                .hover(|d| d.bg(bg_hover()))
                .child(label)
        };
        let ready = matches!(ed.body, EditorBody::Ready(_));
        let header = div()
            .h(px(36.))
            .flex_none()
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(8.))
            .bg(bg_primary())
            .border_b_1()
            .border_color(border())
            .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).child(name))
            .when(ed.dirty, |d| d.child(div().size(px(7.)).rounded_full().bg(ACCENT)))
            .when(ed.saved_flash, |d| d.child(div().text_size(px(11.)).font_weight(FontWeight::MEDIUM).text_color(ACCENT_GREEN).child("Saved")))
            .child(div().flex_1())
            .when(ready, |d| {
                d.child(
                    btn("re-save", if ed.saving { "Saving…" } else { "Save" })
                        .map(|b| if ed.dirty { b.text_color(ACCENT) } else { b.text_color(TEXT_MUTED).opacity(0.6) })
                        .on_click(cx.listener(|this, _, _, cx| this.save_remote(cx))),
                )
            })
            .child(btn("re-close", "Close").on_click(cx.listener(|this, _, _, cx| this.close_remote(false, cx))));
        let center = || div().flex_1().flex().flex_col().items_center().justify_center().gap(px(10.));
        let body: AnyElement = match &ed.body {
            EditorBody::Loading => center().text_size(px(13.)).text_color(TEXT_MUTED).child("Loading…").into_any_element(),
            EditorBody::Failed(msg) => center()
                .child(div().max_w(px(360.)).text_center().text_size(px(13.)).text_color(ACCENT_RED).child(msg.clone()))
                .child(btn("re-close2", "Close").text_color(ACCENT).on_click(cx.listener(|this, _, _, cx| this.close_remote(true, cx))))
                .into_any_element(),
            EditorBody::Ready(state) => div()
                .flex_1()
                .min_h_0()
                .pt(px(8.))
                .bg(editor_bg())
                .child(
                    Editor::new(state)
                        .appearance(false)
                        .bordered(false)
                        .font_family(crate::theme::fonts().editor)
                        .text_size(px(13.))
                        .line_height(px(20.))
                        .size_full(),
                )
                .into_any_element(),
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .bg(bg_primary())
                .key_context("RemoteEditor")
                .on_action(
                    cx.listener(|this, _: &crate::ide::SaveFile, _, cx| this.save_remote(cx)),
                )
                .on_action(cx.listener(|this, _: &crate::ide::Duplicate, window, cx| {
                    if let Some(ed) = this.editor.as_ref()
                        && let EditorBody::Ready(state) = &ed.body
                    {
                        crate::ide::duplicate(&state.clone(), window, cx);
                    }
                }))
                .on_action(
                    cx.listener(|this, _: &crate::ide::ToggleComment, window, cx| {
                        if let Some(ed) = this.editor.as_ref()
                            && let EditorBody::Ready(state) = &ed.body
                        {
                            let (state, path) = (state.clone(), PathBuf::from(&ed.path));
                            crate::ide::toggle_comment(&state, &path, window, cx);
                        }
                    }),
                )
                .child(header)
                .when_some(ed.error.clone(), |d, e| d.child(red_banner(e)))
                .when(ed.confirm_discard, |d| {
                    d.child(
                        div()
                            .px(px(10.))
                            .py(px(6.))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .bg(hexa(0xf14c4c, 0.12))
                            .text_size(px(11.))
                            .child(div().flex_1().child("Discard unsaved changes?"))
                            .child(btn("re-discard", "Discard").bg(ACCENT_RED).text_color(hex(0xffffff)).on_click(cx.listener(|this, _, _, cx| this.close_remote(true, cx))))
                            .child(btn("re-keep", "Cancel").on_click(cx.listener(|this, _, _, cx| {
                                if let Some(ed) = this.editor.as_mut() {
                                    ed.confirm_discard = false;
                                }
                                cx.notify();
                            }))),
                    )
                })
                .child(body)
                .into_any_element(),
        )
    }
}

fn copy_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)?.flatten() {
            copy_recursive(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

// ── Rendering ───────────────────────────────────────────────────────────────

fn red_banner(text: String) -> Div {
    div()
        .px(px(10.))
        .py(px(5.))
        .bg(hexa(0xf14c4c, 0.12))
        .text_color(ACCENT_RED)
        .text_size(px(11.))
        .truncate()
        .child(text)
}

fn small_btn(id: impl Into<ElementId>, label: &'static str, accent: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(14.))
        .py(px(6.))
        .rounded(px(6.))
        .border_1()
        .border_color(border())
        .bg(bg_surface())
        .text_size(px(11.))
        .text_color(if accent { ACCENT } else { TEXT_PRIMARY })
        .cursor_pointer()
        .hover(|d| d.bg(bg_hover()))
        .child(label)
}

impl Render for SftpPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let center = || div().size_full().flex().flex_col().items_center().justify_center().gap(px(10.));
        match self.status.clone() {
            Status::Connecting => return center().text_size(px(13.)).text_color(TEXT_MUTED).child("Connecting…").into_any_element(),
            Status::Failed(msg) => {
                return center()
                    .child(div().max_w(px(360.)).text_center().line_height(relative(1.5)).text_size(px(12.)).text_color(ACCENT_RED).child(msg))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(small_btn("sftp-retry", "Retry", true).on_click(cx.listener(|this, _, _, cx| this.connect(cx))))
                            .child(small_btn("sftp-change", "Change connection", false).on_click(cx.listener(|_, _, _, cx| cx.emit(SftpEvent::ChangeConnection)))),
                    )
                    .into_any_element();
            }
            Status::Ready => {}
        }

        let can_up = !self.local.selected.is_empty() && self.transfer.is_none();
        let can_down = !self.remote.selected.is_empty() && self.transfer.is_none();
        let arrow = |id: &'static str, glyph: &'static str, enabled: bool, title: String| {
            div()
                .id(id)
                .size(px(26.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .border_1()
                .border_color(border())
                .bg(bg_surface())
                .text_size(px(13.))
                .map(|d| if enabled { d.text_color(ACCENT).cursor_pointer() } else { d.text_color(TEXT_MUTED).opacity(0.5) })
                .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(title.clone()).build(window, cx))
                .child(glyph)
        };
        let items = |n: usize| if n == 1 { "1 item".to_owned() } else { format!("{n} items") };
        let strip = div()
            .w(px(36.))
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.))
            .bg(bg_primary())
            .border_l_1()
            .border_r_1()
            .border_color(border())
            .child(
                arrow("sftp-up", "→", can_up, format!("Upload to remote ({})", items(self.local.selected.len())))
                    .when(can_up, |d| d.on_click(cx.listener(|this, _, _, cx| this.transfer(true, cx)))),
            )
            .child(
                arrow("sftp-down", "←", can_down, format!("Download to local ({})", items(self.remote.selected.len())))
                    .when(can_down, |d| d.on_click(cx.listener(|this, _, _, cx| this.transfer(false, cx)))),
            );

        let progress = self.transfer.as_ref().map(|t| {
            let (cur, total, file) = t.progress.as_ref().map(|p| (p.current, p.total, p.file.clone())).unwrap_or((0, 0, String::new()));
            let pct = if total > 0 { cur as f32 / total as f32 } else { 0. };
            div()
                .px(px(10.))
                .py(px(5.))
                .flex()
                .items_center()
                .gap(px(10.))
                .bg(bg_primary())
                .border_t_1()
                .border_color(border())
                .text_size(px(11.))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(if t.upload { "Uploading" } else { "Downloading" }))
                .child(div().child(if total > 0 { format!("{cur}/{total}") } else { "…".into() }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .child(div().truncate().text_color(TEXT_MUTED).child(file))
                        .child(div().h(px(3.)).rounded(px(2.)).bg(bg_hover()).child(div().h_full().rounded(px(2.)).bg(ACCENT).w(relative(pct)))),
                )
                .child(
                    div()
                        .id("sftp-cancel")
                        .px(px(10.))
                        .py(px(3.))
                        .rounded(px(4.))
                        .border_1()
                        .border_color(border())
                        .bg(bg_surface())
                        .cursor_pointer()
                        .child("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(t) = &this.transfer {
                                t.cancel.store(true, Ordering::Relaxed);
                            }
                            cx.notify();
                        })),
                )
        });

        let menu = self.menu.map(|(kind, at)| self.render_menu(kind, at, cx));
        let remote_editor = self.render_remote_editor(cx);

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_half(SideKind::Local, window, cx))
                    .child(strip)
                    .child(self.render_half(SideKind::Remote, window, cx)),
            )
            .when_some(self.transfer_error.clone(), |d, e| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .border_t_1()
                        .border_color(border())
                        .bg(hexa(0xf14c4c, 0.12))
                        .child(red_banner(e).flex_1())
                        .child(
                            div()
                                .id("sftp-dismiss")
                                .px(px(8.))
                                .text_size(px(13.))
                                .text_color(ACCENT_RED)
                                .cursor_pointer()
                                .child("✕")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.transfer_error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .children(progress)
            .children(menu)
            .children(remote_editor)
            .into_any_element()
    }
}

impl SftpPane {
    fn render_half(&self, kind: SideKind, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let side = self.side_ref(kind);
        let label = if kind == SideKind::Local { "Local" } else { "Remote" };
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(10.))
                    .py(px(6.))
                    .border_b_1()
                    .border_color(border())
                    .bg(bg_primary())
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(TEXT_MUTED)
                    .child(label),
            )
            .child(if side.path.is_empty() {
                div().flex_1().flex().items_center().justify_center().text_size(px(12.)).text_color(TEXT_MUTED).child("Resolving home directory…").into_any_element()
            } else {
                self.render_table(kind, window, cx)
            })
            .into_any_element()
    }

    fn crumbs(&self, kind: SideKind) -> Vec<(String, String)> {
        let path = &self.side_ref(kind).path;
        let mut out = vec![("/".to_owned(), "/".to_owned())];
        let mut acc = String::new();
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            acc.push('/');
            acc.push_str(seg);
            out.push((seg.to_owned(), acc.clone()));
        }
        out
    }

    fn render_table(&self, kind: SideKind, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let side = self.side_ref(kind);
        let rows = side.sorted();
        let width = side.width.get();
        let (show_size, show_mod, show_mode) = (width >= 240., width >= 360., width >= 460.);
        let fs = px(FONT * 0.92);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

        // Toolbar: breadcrumbs, + New folder, ⟳ Refresh.
        let crumbs = self.crumbs(kind);
        let last = crumbs.len() - 1;
        let crumb_els: Vec<AnyElement> = crumbs
            .into_iter()
            .enumerate()
            .flat_map(|(i, (label, path))| {
                let current = i == last;
                let sep = (i > 0).then(|| div().text_size(px(FONT * 0.85)).text_color(TEXT_MUTED).opacity(0.6).child("/").into_any_element());
                let crumb = div()
                    .id(SharedString::from(format!("crumb-{kind:?}-{i}")))
                    .px(px(5.))
                    .py(px(2.))
                    .rounded(px(3.))
                    .max_w(px(160.))
                    .truncate()
                    .text_size(px(FONT * 0.85))
                    .map(|d| {
                        if current {
                            d.text_color(TEXT_PRIMARY).font_weight(FontWeight::SEMIBOLD)
                        } else {
                            d.text_color(TEXT_MUTED).cursor_pointer().hover(|d| d.text_color(ACCENT)).on_click(cx.listener(move |this, _, _, cx| this.navigate(kind, path.clone(), cx)))
                        }
                    })
                    .child(label)
                    .into_any_element();
                sep.into_iter().chain(std::iter::once(crumb))
            })
            .collect();
        let tool = |id: &'static str, label: &'static str, disabled: bool| {
            div()
                .id(SharedString::from(format!("{id}-{kind:?}")))
                .px(px(8.))
                .py(px(3.))
                .rounded(px(4.))
                .text_size(px(FONT * 0.85))
                .text_color(TEXT_MUTED)
                .map(|d| if disabled { d.opacity(0.5) } else { d.cursor_pointer().hover(|d| d.text_color(ACCENT).bg(bg_hover())) })
                .child(label)
        };
        let toolbar = div()
            .h(px(32.))
            .flex_none()
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(6.))
            .bg(bg_primary())
            .border_b_1()
            .border_color(border())
            .child(div().flex_1().min_w_0().flex().items_center().overflow_hidden().children(crumb_els))
            .child(tool("newdir", "+ New folder", side.busy).when(!side.busy, |d| {
                d.on_click(cx.listener(move |this, _, window, cx| this.start_inline(kind, Inline::NewFolder, window, cx)))
            }))
            .child(tool("refresh", "⟳ Refresh", side.busy || side.loading).when(!side.busy && !side.loading, |d| {
                d.on_click(cx.listener(move |this, _, _, cx| this.load(kind, cx)))
            }));

        // Column header.
        let header_cell = |id: &'static str, label: &'static str, col: Col, w: Option<f32>, right: bool, cx: &mut Context<Self>| {
            let (sort_col, asc) = side.sort;
            let arrow = if sort_col == col { if asc { " ↑" } else { " ↓" } } else { "" };
            div()
                .id(SharedString::from(format!("{id}-{kind:?}")))
                .px(px(10.))
                .py(px(5.))
                .map(|d| match w {
                    Some(w) => d.w(px(w * FONT * 0.92)).flex_none(),
                    None => d.flex_1().min_w_0(),
                })
                .when(right, |d| d.flex().justify_end())
                .text_size(px(FONT * 0.92 * 0.8))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(TEXT_MUTED)
                .cursor_pointer()
                .child(format!("{}{arrow}", label.to_uppercase()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let side = this.side(kind);
                    side.sort = if side.sort.0 == col { (col, !side.sort.1) } else { (col, true) };
                    cx.notify();
                }))
        };
        let header = div()
            .flex()
            .flex_none()
            .bg(bg_surface())
            .border_b_1()
            .border_color(border())
            .child(header_cell("h-name", "Name", Col::Name, None, false, cx))
            .when(show_size, |d| d.child(header_cell("h-size", "Size", Col::Size, Some(6.5), true, cx)))
            .when(show_mod, |d| d.child(header_cell("h-mod", "Modified", Col::Modified, Some(8.5), false, cx)))
            .when(show_mode, |d| d.child(header_cell("h-mode", "Permissions", Col::Mode, Some(8.), false, cx)));

        let cell = |w: Option<f32>| {
            div().px(px(10.)).py(px(3.)).map(|d| match w {
                Some(w) => d.w(px(w * FONT * 0.92)).flex_none(),
                None => d.flex_1().min_w_0(),
            })
        };

        let inline = self.inline.clone().filter(|(k, _)| *k == kind).map(|(_, i)| i);
        let editor = || {
            div()
                .flex_1()
                .px(px(4.))
                .rounded(px(3.))
                .bg(bg_primary())
                .border_1()
                .border_color(ACCENT)
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.inline = None;
                        cx.notify();
                        cx.stop_propagation();
                    }
                }))
                .child(Input::new(&self.inline_input).appearance(false).xsmall())
        };

        let mut body: Vec<AnyElement> = Vec::new();
        if let Some(parent) = side.parent() {
            body.push(
                div()
                    .id(SharedString::from(format!("up-{kind:?}")))
                    .flex()
                    .text_color(TEXT_MUTED)
                    .cursor_pointer()
                    .hover(|d| d.bg(bg_hover()))
                    .child(cell(None).child("↩ .."))
                    .on_click(cx.listener(move |this, _, _, cx| this.navigate(kind, parent.clone(), cx)))
                    .into_any_element(),
            );
        }
        if inline == Some(Inline::NewFolder) {
            body.push(div().flex().child(cell(None).flex().items_center().gap(px(6.)).child("📁").child(editor())).into_any_element());
        }
        if let Some(err) = &side.error {
            body.push(
                div()
                    .p(px(14.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.))
                    .child(div().text_color(ACCENT_RED).text_size(px(12.)).child(err.clone()))
                    .child(small_btn(SharedString::from(format!("retry-{kind:?}")), "Retry", true).on_click(cx.listener(move |this, _, _, cx| this.load(kind, cx))))
                    .into_any_element(),
            );
        } else if rows.is_empty() && !side.loading {
            body.push(div().px(px(10.)).py(px(20.)).flex().justify_center().text_color(TEXT_MUTED).child("Empty directory").into_any_element());
        }
        for (ix, e) in rows.iter().enumerate() {
            let selected = side.selected.contains(&e.name);
            let renaming = inline.as_ref() == Some(&Inline::Rename(e.name.clone()));
            let name = e.name.clone();
            let name_cell = cell(None).flex().items_center().gap(px(6.)).child(if e.is_dir { "📁" } else { "📄" }).map(|d| {
                if renaming {
                    d.child(editor())
                } else {
                    d.child(div().min_w_0().truncate().child(e.name.clone()))
                        .when(e.is_symlink, |d| d.child(div().text_color(TEXT_MUTED).child("↗")))
                }
            });
            body.push(
                div()
                    .id(SharedString::from(format!("row-{kind:?}-{}", e.name)))
                    .flex()
                    .when(selected, |d| {
                        d.bg(bg_hover()).shadow(vec![BoxShadow {
                            color: c(ACCENT),
                            offset: point(px(2.), px(0.)),
                            blur_radius: px(0.),
                            spread_radius: px(0.),
                            inset: true,
                        }])
                    })
                    .when(!selected, |d| d.hover(|d| d.bg(bg_hover())))
                    .when(e.is_dir, |d| d.cursor_pointer())
                    .child(name_cell)
                    .when(show_size, |d| d.child(cell(Some(6.5)).flex().justify_end().child(if e.is_dir { "—".into() } else { sftp_format::size(e.size) })))
                    .when(show_mod, |d| d.child(cell(Some(8.5)).child(e.modified.map(|m| sftp_format::mtime(m, now)).unwrap_or_default())))
                    .when(show_mode, |d| {
                        d.child(cell(Some(8.)).font_family(MONO).text_size(px(FONT * 0.92 * 0.85)).text_color(TEXT_MUTED).child(e.mode.map(sftp_format::mode).unwrap_or_default()))
                    })
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| this.click_row(kind, ix, ev, window, cx)))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            let side = this.side(kind);
                            if !side.selected.contains(&name) {
                                side.selected = [name.clone()].into();
                                side.anchor = Some(ix);
                            }
                            this.menu = Some((kind, ev.position));
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
            );
        }

        let confirm = side.confirm_delete.then(|| {
            let n = side.selected.len();
            let what = if n == 1 { format!("Delete “{}”?", side.selected.iter().next().unwrap()) } else { format!("Delete {n} items?") };
            let note = if kind == SideKind::Local { "Items move to the Trash." } else { "This cannot be undone." };
            div()
                .px(px(10.))
                .py(px(6.))
                .flex()
                .items_center()
                .gap(px(8.))
                .bg(hexa(0xf14c4c, 0.12))
                .text_size(px(11.))
                .child(div().flex_1().child(format!("{what} {note}")))
                .child(
                    div()
                        .id(SharedString::from(format!("del-yes-{kind:?}")))
                        .px(px(10.))
                        .py(px(3.))
                        .rounded(px(4.))
                        .bg(ACCENT_RED)
                        .text_color(hex(0xffffff))
                        .cursor_pointer()
                        .child("Delete")
                        .on_click(cx.listener(move |this, _, _, cx| this.delete_selected(kind, cx))),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("del-no-{kind:?}")))
                        .px(px(10.))
                        .py(px(3.))
                        .rounded(px(4.))
                        .border_1()
                        .border_color(border())
                        .bg(bg_surface())
                        .cursor_pointer()
                        .child("Cancel")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.side(kind).confirm_delete = false;
                            cx.notify();
                        })),
                )
        });

        let width_cell = side.width.clone();
        let recorder = canvas(move |b, _, _| width_cell.set(crate::ui::unpx(b.size.width)), |_, _, _, _| {}).absolute().size_full();
        let focus = side.focus.clone();
        let _ = window;
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .text_size(px(FONT))
            .child(toolbar)
            .when(side.loading, |d| d.child(div().h(px(2.)).bg(ACCENT).opacity(0.7)))
            .when_some(side.op_error.clone(), |d, e| d.child(red_banner(e).border_b_1().border_color(border())))
            .children(confirm)
            .child(
                div()
                    .id(SharedString::from(format!("table-{kind:?}")))
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .track_focus(&focus)
                    .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| this.on_key(kind, ev, window, cx)))
                    .drag_over::<ExternalPaths>(|s, _, _, _| s.border_2().border_dashed().border_color(ACCENT).bg(hexa(0x3a96dd, 0.10)))
                    .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| this.drop_files(kind, paths, cx)))
                    .child(recorder)
                    .child(header)
                    .child(div().id(SharedString::from(format!("rows-{kind:?}"))).flex_1().min_h_0().overflow_y_scroll().text_size(fs).children(body)),
            )
            .into_any_element()
    }

    fn render_menu(&self, kind: SideKind, at: Point<Pixels>, cx: &mut Context<Self>) -> AnyElement {
        let item = |id: &'static str, label: &'static str, danger: bool| {
            div()
                .id(id)
                .px(px(10.))
                .py(px(5.))
                .rounded(px(4.))
                .text_size(px(FONT * 0.92))
                .text_color(if danger { ACCENT_RED } else { TEXT_PRIMARY })
                .cursor_pointer()
                .hover(|d| d.bg(bg_hover()))
                .child(label)
        };
        let single = self.side_ref(kind).selected.iter().next().cloned().filter(|_| self.side_ref(kind).selected.len() == 1);
        deferred(
            anchored()
                .position(at)
                .snap_to_window_with_margin(px(8.))
                .child(
                    div()
                        .id("sftp-menu")
                        .child(crate::overlay::recorder(at, 0))
                        .min_w(px(150.))
                        .p(px(4.))
                        .flex()
                        .flex_col()
                        .gap(px(1.))
                        .bg(bg_surface())
                        .border_1()
                        .border_color(border())
                        .rounded(px(6.))
                        .shadow(vec![BoxShadow {
                            color: c(hexa(0, 0.4)),
                            offset: point(px(0.), px(4.)),
                            blur_radius: px(16.),
                            spread_radius: px(0.),
                            inset: false,
                        }])
                        .occlude()
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.menu = None;
                            cx.notify();
                        }))
                        .when_some(single, |d, name| {
                            d.child(item("m-rename", "Rename", false).on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.start_inline(
                                        kind,
                                        Inline::Rename(name.clone()),
                                        window,
                                        cx,
                                    )
                                },
                            )))
                        })
                        .child(item("m-delete", "Delete", true).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.menu = None;
                                this.side(kind).confirm_delete = true;
                                cx.notify();
                            },
                        )))
                        .child(item("m-newdir", "New folder", false).on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.start_inline(kind, Inline::NewFolder, window, cx)
                            },
                        ))),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }
}
