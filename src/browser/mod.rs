//! The browser pane (the Electron app's GridBrowser + BrowserPortal): a toolbar
//! drawn by gpui over a WKWebView (see `native`) that follows the pane's bounds.
//!
//! The web view is a native view above gpui's Metal layer, so the app decides
//! each frame whether it may show (`set_presentation`): only in the active
//! workspace, with no app overlay, drag or other focus-mode pane over it.
//! The pane's own canvas then puts it over the laid-out bounds as it paints.
//!
//! Persistence: the container's `browserUrl` follows every committed
//! navigation (the app saves it on `BrowserEvent::Navigated`); `browserPartition`
//! picks a WKWebsiteDataStore (macOS 14+; the default store before that).
//! window.open / target=_blank go to the OS browser (http/https/mailto), as in
//! Electron; downloads land in ~/Downloads (wry's default handler).

pub mod url;
mod native;
pub(crate) mod panes;
pub mod bridge;

use std::rc::Rc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::theme::*;
use native::{Native, Signal};

pub use url::{DEFAULT_PARTITION, normalize_input};

actions!(
    browser,
    [FocusAddress, Reload, HardReload, Back, Forward, ToggleDevTools, PageCopy, PageCut, PagePaste, PageSelectAll, PageUndo, PageRedo]
);

const CONTEXT: &str = "BrowserPane";
/// How often the page state is re-read besides WebKit's own callbacks (a failed
/// load reports nothing, and that must still end the loading state).
const POLL: Duration = Duration::from_millis(500);
/// Address shown for a new pane (Electron's handleAddBrowser).
pub const NEW_PANE_URL: &str = "https://www.google.com";

pub fn bind_keys(cx: &mut App) {
    let ctx = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("cmd-l", FocusAddress, ctx),
        KeyBinding::new("cmd-r", Reload, ctx),
        KeyBinding::new("f5", Reload, ctx),
        KeyBinding::new("cmd-shift-r", HardReload, ctx),
        KeyBinding::new("shift-f5", HardReload, ctx),
        KeyBinding::new("cmd-[", Back, ctx),
        KeyBinding::new("cmd-]", Forward, ctx),
        KeyBinding::new("cmd-alt-i", ToggleDevTools, ctx),
        KeyBinding::new("cmd-alt-j", ToggleDevTools, ctx),
        KeyBinding::new("f12", ToggleDevTools, ctx),
        // The page's editing keys: there is no Edit menu to route them, so the
        // pane forwards them to the web view (the address bar's own bindings,
        // deeper in the context stack, win while it has focus).
        KeyBinding::new("cmd-c", PageCopy, ctx),
        KeyBinding::new("cmd-x", PageCut, ctx),
        KeyBinding::new("cmd-v", PagePaste, ctx),
        KeyBinding::new("cmd-a", PageSelectAll, ctx),
        KeyBinding::new("cmd-z", PageUndo, ctx),
        KeyBinding::new("cmd-shift-z", PageRedo, ctx),
        // App shortcuts that are terminal-scoped elsewhere.
        KeyBinding::new("cmd-w", crate::app::ClosePane, ctx),
        KeyBinding::new("cmd-=", crate::app::ZoomIn, ctx),
        KeyBinding::new("cmd-shift-=", crate::app::ZoomIn, ctx),
        KeyBinding::new("cmd--", crate::app::ZoomOut, ctx),
        KeyBinding::new("cmd-0", crate::app::ZoomReset, ctx),
    ]);
}

// ── Init scripts ────────────────────────────────────────────────────────────

#[derive(Default)]
struct InitScripts(Vec<String>);
impl Global for InitScripts {}

/// Scripts injected at document start in every page of every browser pane
/// (the bridge registers console/network hooks here at startup). Pages post
/// back with `window.ipc.postMessage(string)` → BrowserEvent::Ipc. Panes
/// created before the call don't get it.
pub fn register_init_script(cx: &mut App, script: String) {
    cx.default_global::<InitScripts>().0.push(script);
}

// ── Events ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum BrowserEvent {
    /// The page committed a new URL (also SPA history changes).
    Navigated(String),
    TitleChanged(String),
    LoadStarted,
    LoadFinished,
    /// A message from `window.ipc.postMessage`.
    Ipc(String),
}

/// What the pane asks of the app around it.
#[derive(Clone, Debug, PartialEq)]
pub enum BrowserPaneEvent {
    /// A click landed in the page: make this the focused pane.
    Focused,
}

/// The page state the toolbar shows, re-read from WebKit.
#[derive(Clone, Debug, Default, PartialEq)]
struct PageState {
    url: String,
    title: String,
    loading: bool,
    can_back: bool,
    can_forward: bool,
}

/// The events a change of page state amounts to.
fn state_events(prev: &PageState, now: &PageState) -> Vec<BrowserEvent> {
    let mut out = Vec::new();
    if now.loading && !prev.loading {
        out.push(BrowserEvent::LoadStarted);
    }
    // The blank boot page is never the pane's URL worth reporting.
    if now.url != prev.url && !now.url.is_empty() && now.url != "about:blank" {
        out.push(BrowserEvent::Navigated(now.url.clone()));
    }
    if now.title != prev.title {
        out.push(BrowserEvent::TitleChanged(now.title.clone()));
    }
    if !now.loading && prev.loading {
        out.push(BrowserEvent::LoadFinished);
    }
    out
}

// ── The pane ────────────────────────────────────────────────────────────────

pub struct BrowserPane {
    native: Option<Rc<Native>>,
    /// Why there is no page (the web view couldn't be created).
    error: Option<String>,
    focus: FocusHandle,
    address: Entity<InputState>,
    /// The user typed in the address bar since it last showed the page's URL.
    address_dirty: bool,
    /// A URL for the address bar, applied on the next render.
    pending_address: Option<String>,
    state: PageState,
    /// The URL asked for last, shown until the page commits one.
    requested: String,
    zoom: f64,
    devtools: bool,
    devtools_task: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<BrowserEvent> for BrowserPane {}
impl EventEmitter<BrowserPaneEvent> for BrowserPane {}

impl BrowserPane {
    /// A pane on `url` (normalised; blank: about:blank) in `partition`'s data store,
    /// at `font_size` (the pane zoom; 13 = 100%).
    pub fn new(url: &str, partition: Option<&str>, font_size: Option<f32>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let url = normalize_input(url);
        let partition = partition.filter(|p| !p.is_empty()).unwrap_or(DEFAULT_PARTITION);
        let scripts = cx.try_global::<InitScripts>().map(|s| s.0.clone()).unwrap_or_default();
        let (tx, mut rx) = mpsc::unbounded();
        let opts = native::Options { url: &url, store_id: url::partition_store_id(partition), scripts: &scripts };
        let (native, error) = match Native::new(window, opts, tx) {
            Ok(n) => (Some(Rc::new(n)), None),
            Err(e) => {
                log::error!("browser pane: {e:#}");
                (None, Some(format!("{e:#}")))
            }
        };

        let address = cx.new(|cx| InputState::new(window, cx).placeholder("Search or enter address").default_value(url::display_url(&url).to_owned()));
        let focus = cx.focus_handle();
        let subscriptions = vec![
            cx.subscribe_in(&address, window, |this, input, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    this.address_dirty = true;
                }
                InputEvent::PressEnter { .. } => {
                    let text = input.read(cx).value().to_string();
                    this.navigate(&text, cx);
                    // Hand the keyboard to the page, as Chrome does after Enter.
                    this.focus_page(window, cx);
                }
                InputEvent::Focus => {
                    if let Some(n) = &this.native {
                        n.release_keyboard();
                    }
                    // Chrome-style: focusing the bar selects the whole URL.
                    input.update(cx, |i, cx| i.select_all(window, cx));
                }
                InputEvent::Blur => {}
            }),
            // gpui's focus left the page (another pane, the address bar, a popover):
            // the web view must not keep the keyboard.
            cx.on_blur(&focus, window, |this, _, _| {
                if let Some(n) = &this.native {
                    n.release_keyboard();
                }
            }),
        ];

        let signals = cx.spawn(async move |this, cx| {
            while let Some(signal) = rx.next().await {
                if this.update(cx, |this, cx| this.on_signal(signal, cx)).is_err() {
                    break;
                }
            }
        });
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                if this.update(cx, |this, cx| this.sync(cx)).is_err() {
                    break;
                }
            }
        });

        let zoom = font_size.map_or(1., zoom_factor);
        if let Some(n) = &native
            && (zoom - 1.).abs() > f64::EPSILON
        {
            n.set_zoom(zoom);
        }
        let mut pane = Self {
            native,
            error,
            focus,
            address,
            address_dirty: false,
            pending_address: None,
            state: PageState::default(),
            requested: url,
            zoom,
            devtools: false,
            devtools_task: None,
            _tasks: vec![signals, poll],
            _subscriptions: subscriptions,
        };
        pane.debug_hook(cx);
        pane
    }

    // ── The API the agent bridge drives ─────────────────────────────────────

    pub fn url(&self) -> String {
        if self.state.url.is_empty() { self.requested.clone() } else { self.state.url.clone() }
    }

    pub fn title(&self) -> String {
        self.state.title.clone()
    }

    pub fn is_loading(&self) -> bool {
        self.state.loading
    }

    /// Load what the address bar would (see `url::normalize_input`).
    pub fn navigate(&mut self, url: &str, cx: &mut Context<Self>) {
        let target = normalize_input(url);
        if target.is_empty() {
            return;
        }
        self.requested = target.clone();
        self.address_dirty = false;
        if let Some(n) = &self.native {
            n.load(&target);
        }
        self.show_url(&target, cx);
        cx.notify();
    }

    pub fn back(&mut self, cx: &mut Context<Self>) {
        if let Some(n) = &self.native {
            n.back();
        }
        self.sync(cx);
    }

    pub fn forward(&mut self, cx: &mut Context<Self>) {
        if let Some(n) = &self.native {
            n.forward();
        }
        self.sync(cx);
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.reload_page(false, cx);
    }

    /// Run JS in the page; resolves with the script's completion value as JSON
    /// (a Promise is awaited). Errors carry the JS message.
    pub fn eval(&self, script: &str, cx: &mut Context<Self>) -> Task<anyhow::Result<serde_json::Value>> {
        let Some(n) = &self.native else { return Task::ready(Err(self.missing())) };
        let rx = n.eval(script);
        cx.background_spawn(async move { rx.await.unwrap_or_else(|_| Err(anyhow::anyhow!("the page went away"))) })
    }

    /// PNG of the visible page (WKWebView takeSnapshot), at device scale.
    pub fn screenshot(&self, cx: &mut Context<Self>) -> Task<anyhow::Result<Vec<u8>>> {
        let Some(n) = &self.native else { return Task::ready(Err(self.missing())) };
        let rx = n.snapshot();
        cx.background_spawn(async move { rx.await.unwrap_or_else(|_| Err(anyhow::anyhow!("the page went away"))) })
    }

    /// Viewport size in CSS px.
    pub fn viewport(&self) -> (f32, f32) {
        self.native.as_ref().map_or((0., 0.), |n| n.viewport())
    }

    fn missing(&self) -> anyhow::Error {
        anyhow::anyhow!(self.error.clone().unwrap_or_else(|| "the browser pane has no page".into()))
    }

    // ── Used by the app ─────────────────────────────────────────────────────

    /// Whether the page may be on screen this frame, and whether it's dimmed
    /// (an unfocused pane). Hiding takes effect at once.
    pub(crate) fn set_presentation(&mut self, visible: bool, dimmed: bool) {
        if let Some(n) = &self.native {
            n.set_shown(visible);
            n.set_dimmed(dimmed);
        }
    }

    /// Focus the pane and give the page the keyboard (the app's focus request).
    pub(crate) fn focus_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        if let Some(n) = self.native.clone() {
            // After this frame's layout: a page that was hidden shows first.
            window.on_next_frame(move |_, _| n.focus());
        }
    }

    /// The pane zoom (terminal font size points; 13 = 100%).
    pub(crate) fn set_font_size(&mut self, size: f32) {
        self.zoom = zoom_factor(size);
        if let Some(n) = &self.native {
            n.set_zoom(self.zoom);
        }
    }

    pub(crate) fn font_size(&self) -> f32 {
        (self.zoom * 13.) as f32
    }

    /// Tear the web view down now (the pane closed), whoever still holds the entity.
    pub(crate) fn destroy(&mut self) {
        if let Some(n) = self.native.take() {
            n.destroy();
        }
        self.error = Some("closed".into());
    }

    // ── Internals ───────────────────────────────────────────────────────────

    fn on_signal(&mut self, signal: Signal, cx: &mut Context<Self>) {
        match signal {
            Signal::Sync => self.sync(cx),
            Signal::LoadStarted => {
                self.sync(cx);
                if !self.state.loading {
                    self.state.loading = true;
                    cx.emit(BrowserEvent::LoadStarted);
                    cx.notify();
                }
            }
            Signal::LoadFinished => {
                self.sync(cx);
                if self.state.loading {
                    self.state.loading = false;
                    cx.emit(BrowserEvent::LoadFinished);
                    cx.notify();
                }
            }
            Signal::Ipc(msg) => cx.emit(BrowserEvent::Ipc(msg)),
            Signal::OpenExternal(url) => {
                if ["http:", "https:", "mailto:"].iter().any(|s| url.to_ascii_lowercase().starts_with(s)) {
                    cx.open_url(&url);
                }
            }
            Signal::Focus => cx.emit(BrowserPaneEvent::Focused),
        }
    }

    /// Re-read the page state; emit what changed.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let Some(n) = &self.native else { return };
        let now = PageState {
            url: n.url(),
            title: n.title(),
            loading: n.is_loading(),
            can_back: n.can_go_back(),
            can_forward: n.can_go_forward(),
        };
        self.devtools = n.devtools_open();
        if now == self.state {
            return;
        }
        let events = state_events(&self.state, &now);
        let url_changed = now.url != self.state.url;
        self.state = now;
        if url_changed && !self.state.url.is_empty() && self.state.url != "about:blank" {
            self.requested = self.state.url.clone();
            let url = self.state.url.clone();
            self.show_url(&url, cx);
        }
        for ev in events {
            cx.emit(ev);
        }
        cx.notify();
    }

    /// Put `url` in the address bar (on the next render, which has the window)
    /// unless the user is editing it there.
    fn show_url(&mut self, url: &str, cx: &mut Context<Self>) {
        self.pending_address = Some(url::display_url(url).to_owned());
        cx.notify();
    }

    fn apply_pending_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.pending_address.take() else { return };
        let focused = self.address.read(cx).focus_handle(cx).is_focused(window);
        if self.address_dirty && focused {
            return;
        }
        self.address_dirty = false;
        if self.address.read(cx).value().as_ref() != text {
            self.address.update(cx, |i, cx| i.set_value(text, window, cx));
        }
    }

    fn reload_page(&mut self, from_origin: bool, cx: &mut Context<Self>) {
        let Some(n) = &self.native else { return };
        // A load that never committed has nothing to reload: retry it.
        if self.state.url.is_empty() || self.state.url == "about:blank" {
            if !self.requested.is_empty() && self.requested != "about:blank" {
                n.load(&self.requested.clone());
            }
        } else {
            n.reload(from_origin);
        }
        self.sync(cx);
    }

    fn reload_or_stop(&mut self, cx: &mut Context<Self>) {
        if self.state.loading {
            if let Some(n) = &self.native {
                n.stop();
            }
            self.sync(cx);
        } else {
            self.reload_page(false, cx);
        }
    }

    fn focus_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(n) = &self.native {
            n.release_keyboard();
        }
        self.address.update(cx, |i, cx| {
            i.focus(window, cx);
            i.select_all(window, cx);
        });
    }

    fn revert_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.address_dirty = false;
        let text = url::display_url(&self.url()).to_owned();
        self.address.update(cx, |i, cx| {
            i.set_value(text, window, cx);
            i.select_all(window, cx);
        });
    }

    fn toggle_devtools(&mut self, cx: &mut Context<Self>) {
        let Some(n) = self.native.clone() else { return };
        n.toggle_devtools();
        // WebKit docks the inspector over the window; move it out as soon as it's up.
        self.devtools = n.devtools_open();
        self.devtools_task = Some(cx.spawn(async move |this, cx| {
            for _ in 0..20 {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                if n.undock_devtools() {
                    break;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.devtools = n.devtools_open();
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn edit(&mut self, sel: objc2::runtime::Sel) {
        if let Some(n) = &self.native {
            n.edit_command(sel);
        }
    }

    /// Development aid: SPLITGRID_DEBUG_BROWSER_SNAPSHOT=<file.png> snapshots the
    /// first browser pane's page once it has loaded (and <file>-hidden.png with
    /// the view hidden), and logs eval results and the view's frame.
    fn debug_hook(&mut self, cx: &mut Context<Self>) {
        static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let Ok(path) = std::env::var("SPLITGRID_DEBUG_BROWSER_SNAPSHOT") else { return };
        if self.native.is_none() || DONE.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let task = cx.spawn(async move |this, cx| {
            // Wait for the first load to finish.
            for _ in 0..100 {
                cx.background_executor().timer(Duration::from_millis(200)).await;
                let ready = this.update(cx, |p, _| !p.state.loading && !p.state.url.is_empty()).unwrap_or(true);
                if ready {
                    break;
                }
            }
            cx.background_executor().timer(Duration::from_millis(800)).await;
            let Ok((shot, evals)) = this.update(cx, |p, cx| {
                let n = p.native.as_ref().unwrap();
                eprintln!("[browser-debug] url={} title={:?} viewport={:?} native frame={}", p.url(), p.title(), p.viewport(), n.debug_frame());
                let evals = [
                    "document.title",
                    "1 + 2",
                    "new Promise(r => setTimeout(() => r({ok: true, n: 42}), 50))",
                    "var x = 5; x * 2",
                    "throw new Error('boom')",
                    "document.body",
                    "undefined",
                ]
                .map(|s| (s, p.eval(s, cx)));
                (p.screenshot(cx), evals)
            }) else {
                return;
            };
            match shot.await {
                Ok(png) => {
                    let _ = std::fs::write(&path, &png);
                    eprintln!("[browser-debug] snapshot {} bytes → {path}", png.len());
                }
                Err(e) => eprintln!("[browser-debug] snapshot failed: {e:#}"),
            }
            for (src, task) in evals {
                eprintln!("[browser-debug] eval {src:?} → {:?}", task.await.map_err(|e| e.to_string()));
            }
            // The same with the view hidden (another workspace, an overlay).
            let Ok(hidden) = this.update(cx, |p, cx| {
                p.native.as_ref().unwrap().set_shown(false);
                p.screenshot(cx)
            }) else {
                return;
            };
            match hidden.await {
                Ok(png) => {
                    let hidden_path = path.trim_end_matches(".png").to_owned() + "-hidden.png";
                    let _ = std::fs::write(&hidden_path, &png);
                    eprintln!("[browser-debug] hidden snapshot {} bytes → {hidden_path}", png.len());
                }
                Err(e) => eprintln!("[browser-debug] hidden snapshot failed: {e:#}"),
            }
            if std::env::var_os("SPLITGRID_DEBUG_BROWSER_DEVTOOLS").is_some() {
                let _ = this.update(cx, |p, cx| {
                    p.native.as_ref().unwrap().set_shown(true);
                    p.toggle_devtools(cx);
                    eprintln!("[browser-debug] devtools open: {}", p.native.as_ref().unwrap().devtools_open());
                });
                cx.background_executor().timer(Duration::from_millis(1500)).await;
                let _ = this.update(cx, |p, _| {
                    let n = p.native.as_ref().unwrap();
                    eprintln!("[browser-debug] devtools after 1.5s: {} frame={}", n.devtools_open(), n.debug_frame());
                    n.toggle_devtools();
                    eprintln!("[browser-debug] devtools closed: {}", !n.devtools_open());
                });
            }
            let _ = this.update(cx, |_, cx| cx.notify());
        });
        self._tasks.push(task);
    }
}

/// Pane font size → page zoom (Electron: zoomLevel / 13, 0.5–3).
fn zoom_factor(font_size: f32) -> f64 {
    (font_size as f64 / 13.).clamp(0.5, 3.)
}

impl Focusable for BrowserPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for BrowserPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.apply_pending_address(window, cx);
        let loading = self.state.loading;
        let (can_back, can_forward) = (self.state.can_back, self.state.can_forward);
        let address_focused = self.address.read(cx).focus_handle(cx).is_focused(window);

        // No gpui tooltips: they'd open under the web view.
        let nav_button = |id: &'static str, glyph: &'static str, enabled: bool| {
            div()
                .id(id)
                .size(px(24.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .text_size(px(14.))
                .text_color(TEXT_MUTED)
                .when(enabled, |d| d.cursor_pointer().hover(|d| d.bg(BG_HOVER)))
                .when(!enabled, |d| d.opacity(0.3))
                .child(glyph)
        };

        let toolbar = div()
            .relative()
            .h(px(34.))
            .flex_none()
            .px(px(8.))
            .py(px(4.))
            .flex()
            .items_center()
            .gap(px(4.))
            .bg(BG_SURFACE)
            .border_b_1()
            .border_color(BORDER)
            .child(nav_button("browser-back", "←", can_back).on_click(cx.listener(|this, _, _, cx| this.back(cx))))
            .child(nav_button("browser-forward", "→", can_forward).on_click(cx.listener(|this, _, _, cx| this.forward(cx))))
            .child(
                nav_button("browser-reload", if loading { "×" } else { "↻" }, true)
                    .on_click(cx.listener(|this, _, _, cx| this.reload_or_stop(cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(26.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .rounded(px(6.))
                    .bg(BG_PRIMARY)
                    .border_1()
                    .border_color(if address_focused { ACCENT } else { BORDER })
                    .text_size(px(11.))
                    .key_context("BrowserAddress")
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                        if ev.keystroke.key == "escape" {
                            this.revert_address(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(Input::new(&self.address).appearance(false).small()),
            )
            .child(
                nav_button("browser-external", "↗", self.native.is_some()).on_click(cx.listener(|this, _, _, cx| {
                    let url = this.url();
                    if url.starts_with("http") || url.starts_with("file:") {
                        cx.open_url(&url);
                    }
                })),
            )
            .child(
                nav_button("browser-devtools", "</>", self.native.is_some())
                    .text_size(px(11.))
                    .when(self.devtools, |d| d.text_color(ACCENT))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_devtools(cx))),
            )
            // Loading sweep along the toolbar's bottom edge (over the border).
            .when(loading, |d| {
                d.child(
                    div().absolute().left_0().right_0().bottom(px(-1.)).h(px(2.)).overflow_hidden().child(
                        div().absolute().top_0().bottom_0().w(relative(0.33)).bg(ACCENT).with_animation(
                            "browser-loading",
                            Animation::new(Duration::from_millis(1150)).repeat().with_easing(ease_in_out),
                            |d, t| d.left(relative(-0.33 + 1.33 * t)),
                        ),
                    ),
                )
            });

        // The page's slot: the web view is placed over it as this paints.
        let native = self.native.clone();
        let page = div()
            .relative()
            .flex_1()
            .min_h_0()
            .bg(BG_PRIMARY)
            .child(
                canvas(
                    move |b, _, _| {
                        if let Some(n) = native {
                            n.place(f64::from(f32::from(b.origin.x)), f64::from(f32::from(b.origin.y)), f64::from(f32::from(b.size.width)), f64::from(f32::from(b.size.height)));
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .when_some(self.error.clone(), |d, e| {
                d.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(8.))
                        .p(px(24.))
                        .text_color(TEXT_MUTED)
                        .text_size(px(12.))
                        .child(div().text_color(TEXT_PRIMARY).font_weight(FontWeight::SEMIBOLD).child("The page couldn't be shown"))
                        .child(e),
                )
            });

        div()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .key_context(CONTEXT)
            .bg(BG_PRIMARY)
            .text_color(TEXT_PRIMARY)
            .on_action(cx.listener(|this, _: &FocusAddress, window, cx| this.focus_address(window, cx)))
            .on_action(cx.listener(|this, _: &Reload, _, cx| this.reload_page(false, cx)))
            .on_action(cx.listener(|this, _: &HardReload, _, cx| this.reload_page(true, cx)))
            .on_action(cx.listener(|this, _: &Back, _, cx| this.back(cx)))
            .on_action(cx.listener(|this, _: &Forward, _, cx| this.forward(cx)))
            .on_action(cx.listener(|this, _: &ToggleDevTools, _, cx| this.toggle_devtools(cx)))
            .on_action(cx.listener(|this, _: &PageCopy, _, _| this.edit(objc2::sel!(copy:))))
            .on_action(cx.listener(|this, _: &PageCut, _, _| this.edit(objc2::sel!(cut:))))
            .on_action(cx.listener(|this, _: &PagePaste, _, _| this.edit(objc2::sel!(paste:))))
            .on_action(cx.listener(|this, _: &PageSelectAll, _, _| this.edit(objc2::sel!(selectAll:))))
            .on_action(cx.listener(|this, _: &PageUndo, _, _| this.edit(objc2::sel!(undo:))))
            .on_action(cx.listener(|this, _: &PageRedo, _, _| this.edit(objc2::sel!(redo:))))
            .child(toolbar)
            .child(page)
    }
}

#[cfg(test)]
mod tests {
    use super::{BrowserEvent, PageState, state_events, zoom_factor};

    fn st(url: &str, title: &str, loading: bool) -> PageState {
        PageState { url: url.into(), title: title.into(), loading, ..Default::default() }
    }

    #[test]
    fn a_load_reports_start_url_title_finish_in_order() {
        let a = st("", "", false);
        let b = st("https://a.dev/", "", true);
        assert_eq!(state_events(&a, &b), vec![BrowserEvent::LoadStarted, BrowserEvent::Navigated("https://a.dev/".into())]);
        let c = st("https://a.dev/", "A", false);
        assert_eq!(state_events(&b, &c), vec![BrowserEvent::TitleChanged("A".into()), BrowserEvent::LoadFinished]);
        assert!(state_events(&c, &c).is_empty());
    }

    #[test]
    fn the_blank_boot_page_is_not_a_navigation() {
        assert_eq!(state_events(&st("", "", false), &st("about:blank", "", false)), vec![]);
    }

    #[test]
    fn history_changes_alone_emit_nothing() {
        let a = st("https://a.dev/", "A", false);
        let mut b = a.clone();
        b.can_back = true;
        assert!(state_events(&a, &b).is_empty());
    }

    #[test]
    fn zoom_follows_the_pane_font_size() {
        assert_eq!(zoom_factor(13.), 1.);
        assert_eq!(zoom_factor(26.), 2.);
        assert_eq!(zoom_factor(2.), 0.5);
        assert_eq!(zoom_factor(100.), 3.);
    }
}
