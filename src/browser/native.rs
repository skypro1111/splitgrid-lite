//! The WKWebView behind a browser pane: built by wry as a child NSView of the
//! gpui window's view, then driven directly through WebKit (frame, visibility,
//! history, snapshots, async JS) where wry has no API for it.
//!
//! gpui draws the window into one Metal layer; this view sits above it, so the
//! pane keeps it over its laid-out bounds every frame (`place`) and hides it
//! whenever gpui content has to show on top (`set_shown(false)`).

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;

use anyhow::{Context as _, anyhow};
use block2::RcBlock;
use futures::channel::{mpsc, oneshot};
use objc2::MainThreadMarker;
use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, Sel};
use objc2::sel;
use objc2_app_kit::{NSApplication, NSBitmapImageFileType, NSBitmapImageRep, NSEvent, NSEventMask, NSImage, NSView};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSData, NSDictionary, NSError, NSJSONSerialization, NSJSONWritingOptions, NSString};
use objc2_web_kit::{WKContentWorld, WKSnapshotConfiguration, WKWebView};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{NewWindowResponse, PageLoadEvent, WebViewBuilder, WebViewBuilderExtDarwin as _, WebViewExtMacOS as _, WryWebView};

/// Page → pane notifications, posted from WebKit callbacks (main thread) and
/// drained by the pane's task, outside any entity update.
#[derive(Debug)]
pub(super) enum Signal {
    /// Something may have changed (URL, title, history): re-read the page state.
    Sync,
    LoadStarted,
    LoadFinished,
    Ipc(String),
    /// window.open / target=_blank: Electron hands web links to the OS browser.
    OpenExternal(String),
    /// A click landed in the page.
    Focus,
}

/// What a page posts through `window.ipc` for the pane itself, not the bridge.
pub(super) const PANE_IPC_PREFIX: &str = "__sg_pane:";

/// Tells the pane about SPA navigations (history API, hash changes), which
/// WebKit's navigation delegate never reports.
const URL_HOOK_SCRIPT: &str = r#"(function () {
  if (window.__sgPaneHooks) return;
  window.__sgPaneHooks = true;
  var post = function () { try { window.ipc.postMessage('__sg_pane:url'); } catch (e) {} };
  ['pushState', 'replaceState'].forEach(function (k) {
    var orig = history[k];
    history[k] = function () { var r = orig.apply(this, arguments); post(); return r; };
  });
  addEventListener('popstate', post);
  addEventListener('hashchange', post);
})();"#;

/// The body `eval` runs through callAsyncJavaScript: an indirect eval keeps
/// evaluateJavaScript's semantics (global scope, completion value) and the
/// `await` resolves a returned Promise. The value comes back as JSON text.
const EVAL_BODY: &str = concat!("const __sgValue = await (0, eval)(__sgSource);\n", eval_return!());
macro_rules! eval_return {
    () => {
        r#"let __sgJson;
try { __sgJson = JSON.stringify(__sgValue); } catch (e) { __sgJson = JSON.stringify(String(__sgValue)); }
return __sgJson === undefined ? 'null' : __sgJson;"#
    };
}
use eval_return;
/// Turns `__sgValue` into the JSON text the pane parses.
const EVAL_RETURN: &str = eval_return!();

pub(super) struct Native {
    webview: wry::WebView,
    wk: Retained<WryWebView>,
    monitor: Option<Retained<AnyObject>>,
    /// Allowed on screen by the app (right workspace, nothing over it).
    shown: Cell<bool>,
    /// The pane's laid-out size in points (top-left origin), for `viewport`.
    size: Cell<(f64, f64)>,
    alpha: Cell<f64>,
    destroyed: Cell<bool>,
    _keep: RefCell<Vec<Box<dyn std::any::Any>>>,
}

pub(super) struct Options<'a> {
    pub url: &'a str,
    pub store_id: [u8; 16],
    pub scripts: &'a [String],
}

impl Native {
    pub fn new(window: &gpui_kit::Window, opts: Options<'_>, tx: mpsc::UnboundedSender<Signal>) -> anyhow::Result<Self> {
        let mtm = MainThreadMarker::new().context("the browser pane must be created on the main thread")?;
        let (t_ipc, t_load, t_title, t_new, t_nav) = (tx.clone(), tx.clone(), tx.clone(), tx.clone(), tx.clone());
        let mut builder = WebViewBuilder::new()
            .with_url(if opts.url.is_empty() { "about:blank" } else { opts.url })
            .with_bounds(wry::Rect { position: LogicalPosition::new(0., 0.).into(), size: LogicalSize::new(1., 1.).into() })
            .with_visible(false)
            .with_focused(false)
            .with_accept_first_mouse(true)
            .with_devtools(true)
            .with_back_forward_navigation_gestures(true)
            // Agents drive panes the user isn't looking at: keep their pages running.
            .with_background_throttling(wry::BackgroundThrottlingPolicy::Disabled)
            .with_background_color((0x14, 0x14, 0x14, 0xff))
            .with_data_store_identifier(opts.store_id)
            .with_initialization_script_for_main_only(URL_HOOK_SCRIPT, true)
            .with_ipc_handler(move |req| {
                let body = req.into_body();
                let _ = t_ipc.unbounded_send(match body.strip_prefix(PANE_IPC_PREFIX) {
                    Some(_) => Signal::Sync,
                    None => Signal::Ipc(body),
                });
            })
            .with_on_page_load_handler(move |ev, _url| {
                let _ = t_load.unbounded_send(match ev {
                    PageLoadEvent::Started => Signal::LoadStarted,
                    PageLoadEvent::Finished => Signal::LoadFinished,
                });
            })
            .with_document_title_changed_handler(move |_| {
                let _ = t_title.unbounded_send(Signal::Sync);
            })
            .with_navigation_handler(move |_| {
                let _ = t_nav.unbounded_send(Signal::Sync);
                true
            })
            .with_new_window_req_handler(move |url, _features| {
                let _ = t_new.unbounded_send(Signal::OpenExternal(url));
                NewWindowResponse::Deny
            });
        for script in opts.scripts {
            builder = builder.with_initialization_script(script.as_str());
        }
        let webview = builder.build_as_child(window).map_err(|e| anyhow!("creating the web view: {e}"))?;
        let wk = webview.webview();
        wk.setHidden(true);
        // Until the pane is laid out (its workspace may never be shown) the page
        // gets the window's size, so an agent's screenshot of it is still useful.
        let initial = unsafe { wk.superview() }.map(|p| p.bounds().size).map_or((1024., 768.), |s| (s.width, s.height));
        wk.setFrame(CGRect::new(CGPoint::new(0., 0.), CGSize::new(initial.0, initial.1)));

        // Clicks: one in the page makes its pane the focused one; one anywhere
        // else in the window takes the keyboard back from the page, so gpui's
        // view gets keys again (gpui never sees clicks that land on the page).
        let weak = Weak::from_retained(&wk);
        let block = RcBlock::new(move |ev: NonNull<NSEvent>| -> *mut NSEvent {
            if let Some(wk) = weak.load() {
                on_mouse_down(&wk, unsafe { ev.as_ref() }, mtm, &tx);
            }
            ev.as_ptr()
        });
        let mask = NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown | NSEventMask::OtherMouseDown;
        let monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) };

        Ok(Self {
            webview,
            wk,
            monitor,
            shown: Cell::new(false),
            size: Cell::new(initial),
            alpha: Cell::new(1.),
            destroyed: Cell::new(false),
            _keep: RefCell::new(vec![Box::new(block)]),
        })
    }

    fn wk(&self) -> &WKWebView {
        &self.wk
    }

    // ── Geometry & visibility ───────────────────────────────────────────────

    /// Allow (or forbid) the view on screen. Hiding is immediate; showing
    /// waits for the next `place` with fresh bounds.
    pub fn set_shown(&self, shown: bool) {
        if self.destroyed.get() {
            return;
        }
        if !shown && self.shown.get() {
            self.release_keyboard();
            self.wk.setHidden(true);
        }
        self.shown.set(shown);
    }

    /// Unfocused panes are dimmed (the CSS `.terminal-dim-overlay`); gpui can't
    /// draw over the view, so it fades into the dark pane behind instead.
    pub fn set_dimmed(&self, dimmed: bool) {
        let alpha = if dimmed { 0.76 } else { 1. };
        if (self.alpha.get() - alpha).abs() > f64::EPSILON && !self.destroyed.get() {
            self.alpha.set(alpha);
            self.wk.setAlphaValue(alpha);
        }
    }

    /// Put the view over `(x, y, w, h)`: window points, top-left origin.
    pub fn place(&self, x: f64, y: f64, w: f64, h: f64) {
        if self.destroyed.get() {
            return;
        }
        self.size.set((w, h));
        if !self.shown.get() || w < 2. || h < 2. {
            if !self.wk.isHidden() {
                self.release_keyboard();
                self.wk.setHidden(true);
            }
            return;
        }
        let Some(parent) = (unsafe { self.wk.superview() }) else { return };
        let fy = if parent.isFlipped() { y } else { parent.bounds().size.height - y - h };
        let frame = (x, fy, w, h);
        // Against the view's real frame: WebKit moves it too (a docked inspector).
        let f = self.wk.frame();
        if (f.origin.x, f.origin.y, f.size.width, f.size.height) != frame {
            self.wk.setFrame(CGRect::new(CGPoint::new(x, fy), CGSize::new(w, h)));
            log::debug!("browser: webview frame {frame:?} for pane bounds ({x}, {y}, {w}, {h}) in a {}pt-high view", parent.bounds().size.height);
        }
        if self.wk.isHidden() {
            self.wk.setHidden(false);
        }
    }

    /// The applied native frame (bottom-left origin), the parent's height and
    /// whether the view is hidden, for diagnostics.
    pub fn debug_frame(&self) -> String {
        let f = self.wk.frame();
        let parent_h = unsafe { self.wk.superview() }.map(|p| p.bounds().size.height);
        format!("({}, {}, {}, {}) in a {parent_h:?}pt-high view, hidden={}", f.origin.x, f.origin.y, f.size.width, f.size.height, self.wk.isHidden())
    }

    /// Viewport in CSS px.
    pub fn viewport(&self) -> (f32, f32) {
        let (w, h) = self.size.get();
        let zoom = unsafe { self.wk().pageZoom() }.max(0.01);
        ((w / zoom) as f32, (h / zoom) as f32)
    }

    // ── Keyboard focus ──────────────────────────────────────────────────────

    fn holds_keyboard(&self) -> bool {
        let Some(window) = self.wk.window() else { return false };
        let Some(responder) = window.firstResponder() else { return false };
        match responder.downcast::<NSView>() {
            Ok(view) => view.isDescendantOf(&self.wk),
            Err(_) => false,
        }
    }

    /// Give the page the keyboard (when it's on screen).
    pub fn focus(&self) {
        if self.destroyed.get() || self.wk.isHidden() || self.holds_keyboard() {
            return;
        }
        if let Some(window) = self.wk.window() {
            window.makeFirstResponder(Some(&self.wk));
        }
    }

    /// Hand the keyboard back to gpui's view if the page holds it.
    pub fn release_keyboard(&self) {
        if !self.holds_keyboard() {
            return;
        }
        if let (Some(window), Some(parent)) = (self.wk.window(), unsafe { self.wk.superview() }) {
            window.makeFirstResponder(Some(&parent));
        }
    }

    /// An editing command (copy:, paste:, …) for the page, when it holds the keyboard.
    pub fn edit_command(&self, sel: Sel) -> bool {
        if !self.holds_keyboard() {
            return false;
        }
        let Some(mtm) = MainThreadMarker::new() else { return false };
        unsafe { NSApplication::sharedApplication(mtm).sendAction_to_from(sel, None, None) }
    }

    // ── Page ────────────────────────────────────────────────────────────────

    pub fn url(&self) -> String {
        unsafe { self.wk().URL() }.and_then(|u| u.absoluteString()).map(|s| s.to_string()).unwrap_or_default()
    }

    pub fn title(&self) -> String {
        unsafe { self.wk().title() }.map(|s| s.to_string()).unwrap_or_default()
    }

    pub fn is_loading(&self) -> bool {
        unsafe { self.wk().isLoading() }
    }

    pub fn can_go_back(&self) -> bool {
        unsafe { self.wk().canGoBack() }
    }

    pub fn can_go_forward(&self) -> bool {
        unsafe { self.wk().canGoForward() }
    }

    pub fn load(&self, url: &str) {
        if let Err(e) = self.webview.load_url(url) {
            log::error!("browser: loading {url}: {e}");
        }
    }

    pub fn back(&self) {
        unsafe { self.wk().goBack() };
    }

    pub fn forward(&self) {
        unsafe { self.wk().goForward() };
    }

    pub fn reload(&self, from_origin: bool) {
        unsafe {
            if from_origin {
                self.wk().reloadFromOrigin();
            } else {
                self.wk().reload();
            }
        }
    }

    pub fn stop(&self) {
        unsafe { self.wk().stopLoading() };
    }

    /// Page zoom (pane zoom × UI zoom); a no-op when it's already that.
    pub fn set_zoom(&self, factor: f64) {
        if self.destroyed.get() || (unsafe { self.wk().pageZoom() } - factor).abs() < 1e-6 {
            return;
        }
        unsafe { self.wk().setPageZoom(factor) };
    }

    pub fn devtools_open(&self) -> bool {
        self.webview.is_devtools_open()
    }

    /// Open or close Web Inspector. WebKit docks it into the page view's
    /// superview (gpui's view) and resizes the page; `undock_devtools` moves it
    /// to a window of its own once it is up.
    pub fn toggle_devtools(&self) {
        if self.webview.is_devtools_open() {
            self.webview.close_devtools();
        } else {
            self.webview.open_devtools();
        }
    }

    /// Detach a docked Web Inspector; true once it is in its own window (or closed).
    pub fn undock_devtools(&self) -> bool {
        if !self.webview.is_devtools_open() {
            return true;
        }
        unsafe {
            let inspector: Option<Retained<AnyObject>> = objc2::msg_send![&*self.wk, _inspector];
            let Some(inspector) = inspector else { return true };
            let known: bool = objc2::msg_send![&*inspector, respondsToSelector: sel!(isAttached)];
            let detachable: bool = objc2::msg_send![&*inspector, respondsToSelector: sel!(detach)];
            if !known || !detachable {
                return true;
            }
            let attached: bool = objc2::msg_send![&*inspector, isAttached];
            if attached {
                let () = objc2::msg_send![&*inspector, detach];
            }
            let attached: bool = objc2::msg_send![&*inspector, isAttached];
            !attached
        }
    }

    /// Run `script` in the page and resolve with its completion value as JSON.
    pub fn eval(&self, script: &str) -> oneshot::Receiver<anyhow::Result<serde_json::Value>> {
        let (tx, rx) = oneshot::channel();
        let wk = self.wk.clone();
        let source = script.to_owned();
        call_async(&self.wk, EVAL_BODY, script, move |result| match result {
            // The page's CSP forbids eval (the agent's script is trusted; the
            // page's policy is what blocks it): run the script as an awaited
            // expression instead, and if it is statements rather than an
            // expression, through evaluateJavaScript (exempt from CSP too, but
            // a Promise result can't be awaited there).
            Err(message) if is_eval_blocked(&message) => {
                let body = format!("const __sgValue = await (\n{source}\n);\n{EVAL_RETURN}");
                let (wk2, source2) = (wk.clone(), source.clone());
                call_async(&wk, &body, "", move |result| match result {
                    Err(message) if message.contains("SyntaxError") => evaluate_plain(&wk2, &source2, tx),
                    other => {
                        let _ = tx.send(other.map_err(|m| anyhow!(m)));
                    }
                });
            }
            other => {
                let _ = tx.send(other.map_err(|m| anyhow!(m)));
            }
        });
        rx
    }

    /// PNG of the visible page at the screen's scale.
    pub fn snapshot(&self) -> oneshot::Receiver<anyhow::Result<Vec<u8>>> {
        let (tx, rx) = oneshot::channel();
        let Some(mtm) = MainThreadMarker::new() else {
            let _ = tx.send(Err(anyhow!("snapshot off the main thread")));
            return rx;
        };
        let tx = RefCell::new(Some(tx));
        let block = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
            let Some(tx) = tx.borrow_mut().take() else { return };
            let result = match (unsafe { image.as_ref() }, unsafe { error.as_ref() }) {
                (_, Some(error)) => Err(anyhow!("snapshot failed: {}", error.localizedDescription())),
                (Some(image), None) => png_of(image),
                (None, None) => Err(anyhow!("snapshot failed")),
            };
            let _ = tx.send(result);
        });
        unsafe {
            let config = WKSnapshotConfiguration::new(mtm);
            config.setAfterScreenUpdates(true);
            self.wk().takeSnapshotWithConfiguration_completionHandler(Some(&config), &block);
        }
        rx
    }

    /// Remove the view now (the pane closed); later calls are no-ops.
    pub fn destroy(&self) {
        if self.destroyed.replace(true) {
            return;
        }
        self.release_keyboard();
        if let Some(monitor) = &self.monitor {
            unsafe { NSEvent::removeMonitor(monitor) };
        }
        self.wk.setHidden(true);
        self.wk.removeFromSuperview();
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        self.destroy();
    }
}

fn on_mouse_down(wk: &WryWebView, event: &NSEvent, mtm: MainThreadMarker, tx: &mpsc::UnboundedSender<Signal>) {
    let (Some(window), Some(event_window)) = (wk.window(), event.window(mtm)) else { return };
    if !std::ptr::eq(&*window, &*event_window) {
        return;
    }
    let p = wk.convertPoint_fromView(event.locationInWindow(), None);
    let b = wk.bounds();
    let inside = !wk.isHidden() && p.x >= 0. && p.y >= 0. && p.x < b.size.width && p.y < b.size.height;
    if inside {
        let _ = tx.unbounded_send(Signal::Focus);
    } else if let Some(responder) = window.firstResponder()
        && let Ok(view) = responder.downcast::<NSView>()
        && view.isDescendantOf(wk)
        && let Some(parent) = unsafe { wk.superview() }
    {
        window.makeFirstResponder(Some(&parent));
    }
}

/// callAsyncJavaScript in the page world with `__sgSource` bound to `source`;
/// `done` gets the JSON the body returns, parsed, or the JS error message.
fn call_async(wk: &WryWebView, body: &str, source: &str, done: impl FnOnce(Result<serde_json::Value, String>) + 'static) {
    let Some(mtm) = MainThreadMarker::new() else {
        done(Err("eval off the main thread".into()));
        return;
    };
    let done = RefCell::new(Some(done));
    let block = RcBlock::new(move |value: *mut AnyObject, error: *mut NSError| {
        let Some(done) = done.borrow_mut().take() else { return };
        if let Some(error) = unsafe { error.as_ref() } {
            done(Err(js_error_message(error)));
            return;
        }
        let text = unsafe { value.as_ref() }.and_then(|v| v.downcast_ref::<NSString>().map(|s| s.to_string()));
        done(match text {
            Some(json) => serde_json::from_str(&json).map_err(|e| format!("bad JSON from the page: {e}")),
            None => Ok(serde_json::Value::Null),
        });
    });
    let key = NSString::from_str("__sgSource");
    let value = NSString::from_str(source);
    let args: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(&[&*key], &[value.as_ref() as &AnyObject]);
    unsafe {
        let world = WKContentWorld::pageWorld(mtm);
        wk.callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(&NSString::from_str(body), Some(&args), None, &world, Some(&block));
    }
}

fn is_eval_blocked(message: &str) -> bool {
    message.contains("unsafe-eval") || message.contains("EvalError") || message.contains("Refused to evaluate")
}

/// The JS exception text WebKit puts in a script error, else its description.
fn js_error_message(error: &NSError) -> String {
    let info = error.userInfo();
    let key = NSString::from_str("WKJavaScriptExceptionMessage");
    if let Some(msg) = info.objectForKey(&key)
        && let Some(s) = msg.downcast_ref::<NSString>()
    {
        return s.to_string();
    }
    error.localizedDescription().to_string()
}

/// evaluateJavaScript, its result turned to JSON by Foundation.
fn evaluate_plain(wk: &WryWebView, script: &str, tx: oneshot::Sender<anyhow::Result<serde_json::Value>>) {
    let tx = RefCell::new(Some(tx));
    let block = RcBlock::new(move |value: *mut AnyObject, error: *mut NSError| {
        let Some(tx) = tx.borrow_mut().take() else { return };
        if let Some(error) = unsafe { error.as_ref() } {
            let _ = tx.send(Err(anyhow!(js_error_message(error))));
            return;
        }
        let _ = tx.send(match unsafe { value.as_ref() } {
            None => Ok(serde_json::Value::Null),
            Some(v) => to_json(v),
        });
    });
    unsafe { wk.evaluateJavaScript_completionHandler(&NSString::from_str(script), Some(&block)) };
}

fn to_json(value: &AnyObject) -> anyhow::Result<serde_json::Value> {
    let data = unsafe { NSJSONSerialization::dataWithJSONObject_options_error(value, NSJSONWritingOptions::FragmentsAllowed) }
        .map_err(|e| anyhow!("result is not JSON: {}", e.localizedDescription()))?;
    Ok(serde_json::from_slice(&data.to_vec())?)
}

fn png_of(image: &NSImage) -> anyhow::Result<Vec<u8>> {
    let tiff: Retained<NSData> = image.TIFFRepresentation().context("snapshot has no bitmap")?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff).context("snapshot bitmap unreadable")?;
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }.context("PNG encoding failed")?;
    Ok(png.to_vec())
}
