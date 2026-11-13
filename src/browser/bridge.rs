//! The in-page half of the `sg-browser` agent bridge (the Electron app's
//! browserAutomation.ts + the console/network capture it got from Chromium).
//!
//! WKWebView has no DevTools protocol, so everything an agent reads or does
//! in a page is JavaScript:
//! - [`INIT_SCRIPT`] runs at document start in every page. It records the
//!   console (plus uncaught errors and rejections) and the page's requests
//!   (fetch, XHR, and resource timing for documents, scripts, images, styles…)
//!   and posts them in small batches to `window.ipc`. They arrive as
//!   [`BrowserEvent::Ipc`] and are kept here per pane ([`PaneLog`]), so they
//!   outlive the page that produced them, as Electron's buffers do.
//! - The `*_script` functions build the action scripts (snapshot, click, fill,
//!   waits…). Each is an expression whose value is a JSON string; values put
//!   into a script are always JSON-encoded so text can't break out of it.

use std::collections::{HashMap, VecDeque};

use gpui_kit::{App, EntityId, Global};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{BrowserEvent, BrowserPane};

/// What a pane's console buffer keeps (Electron: CONSOLE_BUFFER_MAX).
pub const CONSOLE_CAP: usize = 200;
/// Captured requests kept per pane (Electron: the CDP buffer's cap).
pub const NETWORK_CAP: usize = 300;
/// Cap on a snapshot, so a huge page can't return megabytes of refs.
const SNAPSHOT_CAP: usize = 250;

/// Runs at document start in every page; see the module docs.
pub const INIT_SCRIPT: &str = r#"(function () {
  if (window.__sgBridge) return;
  var B = window.__sgBridge = { q: { console: [], net: [] }, t: null, seq: 0, res: {},
    page: Math.random().toString(36).slice(2, 8) + Date.now().toString(36) };
  function flush() {
    if (B.t) { clearTimeout(B.t); B.t = null; }
    if (!B.q.console.length && !B.q.net.length) return;
    var msg = { __sg: 'bridge', console: B.q.console, net: B.q.net };
    B.q = { console: [], net: [] };
    try { window.ipc.postMessage(JSON.stringify(msg)); } catch (e) {}
  }
  function soon() { if (!B.t) B.t = setTimeout(flush, 25); }
  window.addEventListener('pagehide', flush, true);
  function nextId() { return B.page + '.' + (++B.seq); }
  function abs(u) { try { return new URL(String(u), location.href).href; } catch (e) { return String(u); } }
  function mime(ct) { return ct ? String(ct).split(';')[0].trim() : undefined; }

  // ── Console ──
  function show(a) {
    if (typeof a === 'string') return a;
    if (a instanceof Error) return a.name + ': ' + a.message;
    if (a === undefined) return 'undefined';
    if (typeof a === 'function' || typeof a === 'symbol') return String(a);
    if (a && typeof a === 'object' && a.nodeType === 1) return '<' + a.tagName.toLowerCase() + '>';
    try { var s = JSON.stringify(a); if (s !== undefined) return s; } catch (e) {}
    return String(a);
  }
  function format(args) {
    args = Array.prototype.slice.call(args);
    var out = [];
    if (typeof args[0] === 'string' && args[0].indexOf('%') !== -1) {
      var f = args.shift();
      out.push(f.replace(/%([sdifoOc%])/g, function (m, c) {
        if (c === '%') return '%';
        if (!args.length) return m;
        var v = args.shift();
        if (c === 'c') return '';
        if (c === 'd' || c === 'i') return String(parseInt(v, 10));
        if (c === 'f') return String(parseFloat(v));
        return show(v);
      }));
    }
    for (var i = 0; i < args.length; i++) out.push(show(args[i]));
    return out.join(' ');
  }
  function record(level, text) {
    B.q.console.push({ level: level, text: String(text).slice(0, 10000), at: Date.now() });
    if (B.q.console.length > 200) B.q.console.splice(0, B.q.console.length - 200);
    soon();
  }
  var LEVEL = { log: 'log', info: 'log', debug: 'debug', trace: 'log', warn: 'warning', error: 'error' };
  Object.keys(LEVEL).forEach(function (name) {
    var orig = console[name];
    if (typeof orig !== 'function') return;
    console[name] = function () {
      try { record(LEVEL[name], format(arguments)); } catch (e) {}
      return orig.apply(this, arguments);
    };
  });
  var oassert = console.assert;
  if (typeof oassert === 'function') console.assert = function (cond) {
    if (!cond) try { record('error', 'Assertion failed: ' + format(Array.prototype.slice.call(arguments, 1))); } catch (e) {}
    return oassert.apply(this, arguments);
  };
  window.addEventListener('error', function (e) {
    var t = e.target;
    if (t && t !== window && t.tagName) {
      // A resource that failed to load (img, script, link…).
      var url = t.currentSrc || t.src || t.href;
      if (!url) return;
      url = abs(url);
      record('error', 'Failed to load resource: ' + url);
      var r = B.res[url];
      var id = r && !r.err ? r.id : nextId();
      B.res[url] = { id: id, obs: r ? r.obs : false, err: true };
      net({ requestId: id, url: url, method: 'GET', resourceType: kindOf(url, t.tagName.toLowerCase()), failed: true, errorText: 'net::ERR_FAILED', finished: true });
      return;
    }
    var msg = e.error ? show(e.error) : (e.message || 'Script error.');
    var where = e.filename ? ' (' + e.filename + (e.lineno ? ':' + e.lineno : '') + ')' : '';
    record('error', 'Uncaught ' + msg + where);
  }, true);
  window.addEventListener('unhandledrejection', function (e) {
    record('error', 'Uncaught (in promise) ' + show(e.reason));
  });

  // ── Network ──
  function net(entry) { B.q.net.push(entry); soon(); }
  function kindOf(url, initiator) {
    var path = String(url).split(/[?#]/)[0].toLowerCase();
    if (/\.css$/.test(path)) return 'Stylesheet';
    if (/\.m?js$/.test(path)) return 'Script';
    if (/\.(png|jpe?g|gif|svg|webp|ico|avif|bmp)$/.test(path)) return 'Image';
    if (/\.(woff2?|ttf|otf|eot)$/.test(path)) return 'Font';
    if (/\.(mp4|webm|mp3|ogg|wav|m4a)$/.test(path)) return 'Media';
    return ({ navigation: 'Document', iframe: 'Document', frame: 'Document', script: 'Script', img: 'Image', image: 'Image',
      link: 'Stylesheet', css: 'Stylesheet', audio: 'Media', video: 'Media', beacon: 'Ping' })[initiator] || 'Other';
  }
  var ofetch = window.fetch;
  if (typeof ofetch === 'function') window.fetch = function (input, init) {
    var id = nextId();
    var url = abs(input && typeof input === 'object' && 'url' in input ? input.url : input);
    var method = String((init && init.method) || (input && typeof input === 'object' && input.method) || 'GET').toUpperCase();
    net({ requestId: id, url: url, method: method, resourceType: 'Fetch' });
    var p = ofetch.apply(this, arguments);
    try {
      p.then(function (r) {
        net({ requestId: id, url: r.url || url, status: r.status, statusText: r.statusText, mimeType: mime(r.headers.get('content-type')), fromCache: false, finished: true });
      }, function (err) {
        var aborted = err && err.name === 'AbortError';
        net({ requestId: id, failed: true, errorText: aborted ? 'net::ERR_ABORTED' : String((err && err.message) || err), finished: true });
      });
    } catch (e) {}
    return p;
  };
  var XP = window.XMLHttpRequest && XMLHttpRequest.prototype;
  if (XP) {
    var xopen = XP.open, xsend = XP.send;
    XP.open = function (m, u) { this.__sg = { method: String(m || 'GET').toUpperCase(), url: abs(u) }; return xopen.apply(this, arguments); };
    XP.send = function () {
      var x = this, info = x.__sg;
      if (info && !info.id) {
        info.id = nextId();
        net({ requestId: info.id, url: info.url, method: info.method, resourceType: 'XHR' });
        x.addEventListener('abort', function () { info.aborted = true; });
        x.addEventListener('timeout', function () { info.timedOut = true; });
        x.addEventListener('loadend', function () {
          if (!x.status) {
            net({ requestId: info.id, failed: true, errorText: info.aborted ? 'net::ERR_ABORTED' : info.timedOut ? 'net::ERR_TIMED_OUT' : 'net::ERR_FAILED', finished: true });
          } else {
            var ct = null; try { ct = x.getResponseHeader('content-type'); } catch (e) {}
            net({ requestId: info.id, url: x.responseURL || info.url, status: x.status, statusText: x.statusText, mimeType: mime(ct), fromCache: false, finished: true });
          }
        });
      }
      return xsend.apply(this, arguments);
    };
  }
  function timing(list) {
    list.getEntries().forEach(function (en) {
      var init = en.initiatorType || (en.entryType === 'navigation' ? 'navigation' : 'other');
      if (init === 'fetch' || init === 'xmlhttprequest') return;
      var url = en.entryType === 'navigation' ? location.href : en.name;
      var r = B.res[url];
      var id = r && !r.obs ? r.id : nextId();
      B.res[url] = { id: id, obs: true, err: r ? r.err : false };
      var e = { requestId: id, url: url, method: 'GET', resourceType: kindOf(url, init), finished: true };
      if (en.responseStatus) e.status = en.responseStatus;
      if (en.contentType) e.mimeType = en.contentType;
      if (en.transferSize === 0 && en.decodedBodySize > 0) e.fromCache = true;
      net(e);
    });
  }
  try { new PerformanceObserver(timing).observe({ type: 'navigation', buffered: true }); } catch (e) {}
  try { new PerformanceObserver(timing).observe({ type: 'resource', buffered: true }); } catch (e) {}
})();"#;

// ─── Per-pane logs ───────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConsoleEntry {
    pub level: String,
    pub text: String,
    /// Epoch ms (the page's `Date.now()`).
    #[serde(deserialize_with = "de_ms")]
    pub at: i64,
}

fn de_ms<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    Ok(f64::deserialize(d)? as i64)
}

/// One request, as Electron's CDP capture reports it (absent fields omitted).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetEntry {
    pub request_id: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_cache: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<bool>,
}

impl NetEntry {
    /// Fold a later message about the same request into this one.
    fn merge(&mut self, update: &Map<String, Value>) {
        let s = |k: &str| update.get(k).and_then(Value::as_str).map(str::to_owned);
        let b = |k: &str| update.get(k).and_then(Value::as_bool);
        if let Some(v) = s("url").filter(|v| !v.is_empty()) {
            self.url = v;
        }
        if let Some(v) = s("method").filter(|v| !v.is_empty()) {
            self.method = v;
        }
        self.resource_type = s("resourceType").or(self.resource_type.take());
        self.status = update.get("status").and_then(Value::as_u64).and_then(|n| u16::try_from(n).ok()).or(self.status);
        self.status_text = s("statusText").or(self.status_text.take());
        self.mime_type = s("mimeType").or(self.mime_type.take());
        self.from_cache = b("fromCache").or(self.from_cache);
        self.failed = b("failed").or(self.failed);
        self.error_text = s("errorText").or(self.error_text.take());
        self.finished = b("finished").or(self.finished);
    }
}

/// What the bridge keeps for one pane.
#[derive(Default, Debug)]
pub struct PaneLog {
    pub console: VecDeque<ConsoleEntry>,
    /// Network capture is on (`network start`, or the first `network list`).
    pub capturing: bool,
    pub requests: Vec<NetEntry>,
    /// When the bridge last started a navigation that WebKit hasn't reported
    /// starting yet, so `wait load` right after `open`/`reload` doesn't see the
    /// old, idle page as loaded.
    pub nav_issued: Option<std::time::Instant>,
}

impl PaneLog {
    /// How long a navigation may take to start before `wait load` stops waiting for it.
    pub const NAV_START_GRACE: std::time::Duration = std::time::Duration::from_millis(1500);

    /// A navigation this bridge started is still due to begin.
    pub fn nav_pending(&self, now: std::time::Instant) -> bool {
        self.nav_issued.is_some_and(|t| now.duration_since(t) < Self::NAV_START_GRACE)
    }
}

/// Note that the bridge just started a navigation in `pane`.
pub fn mark_navigation(pane: EntityId, cx: &mut App) {
    with_log(pane, cx, |log| log.nav_issued = Some(std::time::Instant::now()));
}

impl PaneLog {
    /// One batch the init script posted; false when it isn't ours.
    pub fn ingest(&mut self, message: &str) -> bool {
        let Ok(Value::Object(msg)) = serde_json::from_str::<Value>(message) else { return false };
        if msg.get("__sg").and_then(Value::as_str) != Some("bridge") {
            return false;
        }
        for c in msg.get("console").and_then(Value::as_array).into_iter().flatten() {
            if let Ok(entry) = serde_json::from_value::<ConsoleEntry>(c.clone()) {
                self.console.push_back(entry);
            }
        }
        while self.console.len() > CONSOLE_CAP {
            self.console.pop_front();
        }
        if self.capturing {
            for n in msg.get("net").and_then(Value::as_array).into_iter().flatten() {
                let Some(update) = n.as_object() else { continue };
                let Some(id) = update.get("requestId").and_then(Value::as_str) else { continue };
                match self.requests.iter_mut().find(|e| e.request_id == id) {
                    Some(e) => e.merge(update),
                    // Only a request's first message carries its url; updates for one
                    // started before capture began are dropped, as with CDP.
                    None if update.contains_key("url") && update.contains_key("method") => {
                        let mut e = NetEntry { request_id: id.to_owned(), ..Default::default() };
                        e.merge(update);
                        self.requests.push(e);
                    }
                    None => {}
                }
            }
            if self.requests.len() > NETWORK_CAP {
                self.requests.drain(..self.requests.len() - NETWORK_CAP);
            }
        }
        true
    }
}

/// Every browser pane's log, by the pane entity.
#[derive(Default)]
pub struct BridgeLogs(pub HashMap<EntityId, PaneLog>);

impl Global for BridgeLogs {}

/// Register the init script and start logging every browser pane created from now on.
pub fn init(cx: &mut App) {
    super::register_init_script(cx, INIT_SCRIPT.to_owned());
    cx.set_global(BridgeLogs::default());
    cx.observe_new::<BrowserPane>(|_, _, cx| {
        let id = cx.entity_id();
        cx.subscribe_self(move |_, event: &BrowserEvent, cx| {
            match event {
                BrowserEvent::Ipc(message) => {
                    cx.global_mut::<BridgeLogs>().0.entry(id).or_default().ingest(message);
                }
                BrowserEvent::LoadStarted => {
                    cx.global_mut::<BridgeLogs>().0.entry(id).or_default().nav_issued = None;
                }
                _ => {}
            }
        })
        .detach();
        cx.on_release(move |_, cx| {
            cx.global_mut::<BridgeLogs>().0.remove(&id);
        })
        .detach();
    })
    .detach();
}

/// Read or change one pane's log.
pub fn with_log<R>(pane: EntityId, cx: &mut App, f: impl FnOnce(&mut PaneLog) -> R) -> R {
    if !cx.has_global::<BridgeLogs>() {
        cx.set_global(BridgeLogs::default());
    }
    f(cx.global_mut::<BridgeLogs>().0.entry(pane).or_default())
}

// ─── Action scripts ──────────────────────────────────────────────────────────

/// A value as a JS literal (JSON is one).
fn js(v: impl Serialize) -> String {
    serde_json::to_string(&v).unwrap_or_else(|_| "null".into())
}

/// Fill `$NAME` placeholders in one pass, so a value that itself contains
/// `$SOMETHING` is never substituted into. Unknown names are left alone.
fn fill(template: &str, vars: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let len = after.bytes().take_while(u8::is_ascii_uppercase).count();
        match vars.iter().find(|(k, _)| *k == &after[..len]).filter(|_| len > 0) {
            Some((_, v)) => out.push_str(v),
            None => out.push_str(&rest[i..i + 1 + len]),
        }
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

/// Resolve a target, a snapshot ref (`e12`) or a CSS selector, across the
/// document and its same-origin frames.
const RESOLVE_SRC: &str = r#"function __sgResolve(target) {
  var isRef = /^e[0-9]+$/.test(target);
  function inDoc(doc) {
    try {
      var el = null;
      if (isRef) { el = doc.querySelector('[data-splitgrid-ref="' + target + '"]'); }
      else { try { el = doc.querySelector(target); } catch (e) { el = null; } }
      if (el) return el;
      var frames = doc.querySelectorAll('iframe, frame');
      for (var i = 0; i < frames.length; i++) {
        try { var d = frames[i].contentDocument; if (d) { var f = inDoc(d); if (f) return f; } } catch (e) {}
      }
    } catch (e) {}
    return null;
  }
  return inDoc(document);
}"#;

/// An element action: resolve `target`, bail with not_found, else run `body` (which has `el`, `win`, `T`).
fn element_script(target: &str, body: &str) -> String {
    fill(
        r#"(function () {
  try {
    $RESOLVE
    var T = $TARGET;
    var el = __sgResolve(T);
    if (!el) return JSON.stringify({ ok: false, error: 'not_found', target: T });
    var win = (el.ownerDocument && el.ownerDocument.defaultView) || window;
    $BODY
  } catch (e) { return JSON.stringify({ ok: false, error: String(e), target: $TARGET }); }
})()"#,
        &[("RESOLVE", RESOLVE_SRC.into()), ("TARGET", js(target)), ("BODY", body.into())],
    )
}

pub fn page_text_script() -> String {
    "JSON.stringify(document.body ? document.body.innerText : '')".into()
}

pub fn page_html_script() -> String {
    "JSON.stringify(document.documentElement ? document.documentElement.outerHTML : '')".into()
}

/// Tag every visible interactive element (and those of same-origin frames) with
/// `data-splitgrid-ref` and list them.
pub fn snapshot_script(viewport_only: bool) -> String {
    fill(
        r#"(function () {
  try {
    var VIEWPORT_ONLY = $VIEWPORT;
    var SEL = [
      'a[href]', 'button', 'input:not([type=hidden])', 'textarea', 'select',
      'summary', '[contenteditable=""]', '[contenteditable="true"]', '[onclick]',
      '[role="button"]', '[role="link"]', '[role="checkbox"]', '[role="radio"]',
      '[role="tab"]', '[role="menuitem"]', '[role="switch"]', '[role="option"]',
      '[role="textbox"]', '[role="combobox"]', '[role="searchbox"]',
      '[tabindex]:not([tabindex="-1"])'
    ].join(', ');
    var SEMANTIC = { A: 1, BUTTON: 1, INPUT: 1, TEXTAREA: 1, SELECT: 1, SUMMARY: 1 };
    function visible(el) {
      if (el.disabled) return false;
      var win = (el.ownerDocument && el.ownerDocument.defaultView) || window;
      var s = win.getComputedStyle(el);
      if (s.visibility === 'hidden' || s.display === 'none' || s.opacity === '0') return false;
      var r = el.getBoundingClientRect();
      if (r.width < 1 || r.height < 1) return false;
      if (VIEWPORT_ONLY) {
        var vw = win.innerWidth, vh = win.innerHeight;
        if (r.bottom <= 0 || r.right <= 0 || r.top >= vh || r.left >= vw) return false;
      }
      return true;
    }
    function labelledBy(el, doc) {
      var ids = (el.getAttribute('aria-labelledby') || '').trim();
      if (!ids) return '';
      return ids.split(/\s+/).map(function (id) {
        var n = doc.getElementById(id);
        return n ? (n.innerText || n.textContent || '').trim() : '';
      }).filter(Boolean).join(' ');
    }
    function name(el, doc) {
      var n = labelledBy(el, doc) || el.getAttribute('aria-label') || '';
      if (!n && el.id) {
        try { var lab = doc.querySelector('label[for="' + el.id + '"]'); if (lab) n = (lab.innerText || lab.textContent || '').trim(); } catch (e) {}
      }
      if (!n) { var wrap = el.closest && el.closest('label'); if (wrap) n = (wrap.innerText || wrap.textContent || '').trim(); }
      if (!n) n = el.getAttribute('placeholder') || el.getAttribute('alt') || el.getAttribute('title') || '';
      if (!n && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) n = el.value || '';
      if (!n) n = (el.innerText || el.textContent || '').trim();
      if (!n && el.tagName === 'INPUT') n = el.getAttribute('name') || el.type || '';
      return String(n).replace(/\s+/g, ' ').slice(0, 160);
    }
    function isGeneric(el) { return !SEMANTIC[el.tagName] && !el.getAttribute('role'); }
    function dedup(cands) {
      return cands.filter(function (el) {
        if (!isGeneric(el)) return true;
        for (var i = 0; i < cands.length; i++) { if (cands[i] !== el && el.contains(cands[i])) return false; }
        return true;
      });
    }
    var out = [];
    var truncated = false;
    var counter = { n: 0 };
    function collect(doc, frameId) {
      var nodes;
      try { nodes = Array.prototype.slice.call(doc.querySelectorAll(SEL)); } catch (e) { return; }
      var cands = dedup(nodes.filter(visible));
      for (var k = 0; k < cands.length; k++) {
        if (out.length >= $CAP) { truncated = true; return; }
        var el = cands[k];
        counter.n++;
        var ref = 'e' + counter.n;
        el.setAttribute('data-splitgrid-ref', ref);
        var item = { ref: ref, role: el.getAttribute('role') || el.tagName.toLowerCase(), name: name(el, doc) };
        if (frameId != null) item.frame = frameId;
        var tag = el.tagName;
        if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') {
          item.tag = tag.toLowerCase();
          if (el.type) item.type = el.type;
          if (el.value) item.value = String(el.value).slice(0, 120);
          if (el.checked !== undefined && (el.type === 'checkbox' || el.type === 'radio')) item.checked = !!el.checked;
        } else if (tag === 'A') {
          var href = el.getAttribute('href');
          if (href) item.href = href.slice(0, 200);
        }
        out.push(item);
      }
      var frames;
      try { frames = doc.querySelectorAll('iframe, frame'); } catch (e) { frames = []; }
      for (var fi = 0; fi < frames.length; fi++) {
        if (out.length >= $CAP) { truncated = true; return; }
        var fdoc = null;
        try { fdoc = frames[fi].contentDocument; } catch (e) { fdoc = null; }
        if (fdoc) {
          try { fdoc.querySelectorAll('[data-splitgrid-ref]').forEach(function (e) { e.removeAttribute('data-splitgrid-ref'); }); } catch (e) {}
          collect(fdoc, (frameId == null ? '' : frameId + '.') + fi);
        }
      }
    }
    try { document.querySelectorAll('[data-splitgrid-ref]').forEach(function (e) { e.removeAttribute('data-splitgrid-ref'); }); } catch (e) {}
    collect(document, null);
    return JSON.stringify({ url: location.href, title: document.title, count: out.length, truncated: truncated, viewportOnly: VIEWPORT_ONLY, elements: out });
  } catch (e) { return JSON.stringify({ ok: false, error: String(e) }); }
})()"#,
        &[("VIEWPORT", js(viewport_only)), ("CAP", SNAPSHOT_CAP.to_string())],
    )
}

pub fn click_script(target: &str) -> String {
    element_script(
        target,
        r#"el.scrollIntoView({ block: 'center', inline: 'center' });
    if (el.focus) try { el.focus(); } catch (e) {}
    el.click();
    return JSON.stringify({ ok: true, target: T });"#,
    )
}

pub fn dblclick_script(target: &str) -> String {
    element_script(
        target,
        r#"el.scrollIntoView({ block: 'center', inline: 'center' });
    if (el.focus) try { el.focus(); } catch (e) {}
    el.dispatchEvent(new win.MouseEvent('dblclick', { bubbles: true, cancelable: true, detail: 2 }));
    return JSON.stringify({ ok: true, target: T });"#,
    )
}

pub fn hover_script(target: &str) -> String {
    element_script(
        target,
        r#"el.scrollIntoView({ block: 'center', inline: 'center' });
    var r = el.getBoundingClientRect();
    var opt = { bubbles: true, cancelable: true, clientX: r.left + r.width / 2, clientY: r.top + r.height / 2 };
    ['pointerover', 'pointerenter', 'mouseover', 'mouseenter', 'mousemove'].forEach(function (t) {
      try { el.dispatchEvent(new win.MouseEvent(t, opt)); } catch (e) {}
    });
    return JSON.stringify({ ok: true, target: T });"#,
    )
}

pub fn focus_script(target: &str) -> String {
    element_script(
        target,
        r#"el.scrollIntoView({ block: 'center' });
    if (el.focus) el.focus();
    return JSON.stringify({ ok: true, target: T, focused: true });"#,
    )
}

/// Set an input's value through the native setter so React's controlled inputs see it.
const SET_VALUE_SRC: &str = r#"function __sgSetValue(el, win, text) {
  if (el.isContentEditable) {
    el.textContent = text;
    el.dispatchEvent(new win.InputEvent('input', { bubbles: true }));
    return;
  }
  var proto = el.tagName === 'TEXTAREA' ? win.HTMLTextAreaElement.prototype : el.tagName === 'SELECT' ? win.HTMLSelectElement.prototype : win.HTMLInputElement.prototype;
  var desc = Object.getOwnPropertyDescriptor(proto, 'value');
  if (desc && desc.set) desc.set.call(el, text); else el.value = text;
  el.dispatchEvent(new win.Event('input', { bubbles: true }));
  el.dispatchEvent(new win.Event('change', { bubbles: true }));
}"#;

pub fn fill_script(target: &str, text: &str) -> String {
    element_script(
        target,
        &fill(
            r#"$SETVALUE
    var text = $TEXT;
    el.scrollIntoView({ block: 'center' });
    if (el.focus) try { el.focus(); } catch (e) {}
    __sgSetValue(el, win, text);
    return JSON.stringify({ ok: true, ref: T, value: el.value !== undefined ? String(el.value) : text });"#,
            &[("SETVALUE", SET_VALUE_SRC.into()), ("TEXT", js(text))],
        ),
    )
}

/// The focused editable element, reaching into focused same-origin frames.
const ACTIVE_SRC: &str = r#"function __sgActive() {
  var el = document.activeElement;
  try {
    while (el && (el.tagName === 'IFRAME' || el.tagName === 'FRAME') && el.contentDocument && el.contentDocument.activeElement) {
      el = el.contentDocument.activeElement;
    }
  } catch (e) {}
  return el;
}"#;

pub fn type_script(text: &str) -> String {
    fill(
        r#"(function () {
  try {
    $ACTIVE
    $SETVALUE
    var el = __sgActive();
    if (!el || (el.tagName !== 'INPUT' && el.tagName !== 'TEXTAREA' && !el.isContentEditable))
      return JSON.stringify({ ok: false, error: 'no_focused_field', message: 'click a field first, then type' });
    var win = (el.ownerDocument && el.ownerDocument.defaultView) || window;
    var text = $TEXT;
    for (var i = 0; i < text.length; i++) {
      var ch = text[i];
      var ev = { key: ch, bubbles: true, cancelable: true };
      var go = el.dispatchEvent(new win.KeyboardEvent('keydown', ev));
      if (go && el.dispatchEvent(new win.KeyboardEvent('keypress', ev))) {
        try { el.dispatchEvent(new win.InputEvent('beforeinput', { inputType: 'insertText', data: ch, bubbles: true, cancelable: true })); } catch (e) {}
        if (el.isContentEditable) { el.textContent = (el.textContent || '') + ch; }
        else {
          var proto = el.tagName === 'TEXTAREA' ? win.HTMLTextAreaElement.prototype : win.HTMLInputElement.prototype;
          var desc = Object.getOwnPropertyDescriptor(proto, 'value');
          var next = (el.value || '') + ch;
          if (desc && desc.set) desc.set.call(el, next); else el.value = next;
        }
        try { el.dispatchEvent(new win.InputEvent('input', { inputType: 'insertText', data: ch, bubbles: true })); } catch (e) { el.dispatchEvent(new win.Event('input', { bubbles: true })); }
      }
      el.dispatchEvent(new win.KeyboardEvent('keyup', ev));
    }
    if (!el.isContentEditable) el.dispatchEvent(new win.Event('change', { bubbles: true }));
    return JSON.stringify({ ok: true, value: el.value !== undefined ? String(el.value) : (el.textContent || '') });
  } catch (e) { return JSON.stringify({ ok: false, error: String(e) }); }
})()"#,
        &[("ACTIVE", ACTIVE_SRC.into()), ("SETVALUE", SET_VALUE_SRC.into()), ("TEXT", js(text))],
    )
}

pub fn select_script(target: &str, value: &str) -> String {
    element_script(
        target,
        &fill(
            r#"if (el.tagName !== 'SELECT') return JSON.stringify({ ok: false, error: 'not_a_select', target: T });
    var want = $VALUE;
    var opts = Array.prototype.slice.call(el.options);
    var opt = opts.find(function (o) { return o.value === want; })
      || opts.find(function (o) { return (o.text || '').trim() === want; })
      || opts.find(function (o) { return (o.text || '').trim().indexOf(want) !== -1; });
    if (!opt) return JSON.stringify({ ok: false, error: 'option_not_found', value: want, options: opts.map(function (o) { return { value: o.value, label: (o.text || '').trim() }; }) });
    el.value = opt.value;
    el.dispatchEvent(new win.Event('input', { bubbles: true }));
    el.dispatchEvent(new win.Event('change', { bubbles: true }));
    return JSON.stringify({ ok: true, ref: T, selected: opt.value, label: (opt.text || '').trim() });"#,
            &[("VALUE", js(value))],
        ),
    )
}

pub fn check_script(target: &str, checked: bool) -> String {
    element_script(
        target,
        &fill(
            r#"if (el.tagName !== 'INPUT' || (el.type !== 'checkbox' && el.type !== 'radio'))
      return JSON.stringify({ ok: false, error: 'not_checkable', target: T });
    var want = $WANT;
    el.scrollIntoView({ block: 'center' });
    if (el.checked !== want) {
      el.checked = want;
      el.dispatchEvent(new win.Event('input', { bubbles: true }));
      el.dispatchEvent(new win.Event('change', { bubbles: true }));
    }
    return JSON.stringify({ ok: true, target: T, checked: !!el.checked });"#,
            &[("WANT", js(checked))],
        ),
    )
}

pub fn scroll_script(direction: &str, px: u32) -> String {
    fill(
        r#"(function () {
  try {
    var dir = $DIR, amt = $PX, dx = 0, dy = 0;
    if (dir === 'up') dy = -amt; else if (dir === 'down') dy = amt;
    else if (dir === 'left') dx = -amt; else if (dir === 'right') dx = amt;
    else return JSON.stringify({ ok: false, error: 'bad_direction', message: 'up|down|left|right' });
    window.scrollBy(dx, dy);
    return JSON.stringify({ ok: true, direction: dir, by: amt, scrollY: window.scrollY, scrollX: window.scrollX });
  } catch (e) { return JSON.stringify({ ok: false, error: String(e) }); }
})()"#,
        &[("DIR", js(direction)), ("PX", px.to_string())],
    )
}

pub fn scroll_into_view_script(target: &str) -> String {
    element_script(target, "el.scrollIntoView({ block: 'center', inline: 'center' });\n    return JSON.stringify({ ok: true, target: T });")
}

/// `get text|html|value|attr|count|box <selector> [attr]`.
pub fn get_element_script(what: &str, selector: &str, attr: Option<&str>) -> String {
    fill(
        r#"(function () {
  try {
    $RESOLVE
    var what = $WHAT, sel = $SEL;
    if (what === 'count') {
      var n = 0;
      try { n = document.querySelectorAll(sel).length; } catch (e) { return JSON.stringify({ ok: false, error: 'bad_selector', detail: String(e) }); }
      return JSON.stringify({ ok: true, selector: sel, count: n });
    }
    var el = __sgResolve(sel);
    if (!el) return JSON.stringify({ ok: false, error: 'not_found', target: sel });
    if (what === 'text') return JSON.stringify({ ok: true, selector: sel, text: String(el.innerText || el.textContent || '').slice(0, 20000) });
    if (what === 'html') return JSON.stringify({ ok: true, selector: sel, html: String(el.innerHTML || '').slice(0, 50000) });
    if (what === 'value') return JSON.stringify({ ok: true, selector: sel, value: el.value !== undefined ? String(el.value) : null });
    if (what === 'attr') { var a = $ATTR; return JSON.stringify({ ok: true, selector: sel, attr: a, result: el.getAttribute(a) }); }
    if (what === 'box') { var r = el.getBoundingClientRect(); return JSON.stringify({ ok: true, selector: sel, box: { x: r.x, y: r.y, width: r.width, height: r.height, top: r.top, left: r.left } }); }
    return JSON.stringify({ ok: false, error: 'unknown_get' });
  } catch (e) { return JSON.stringify({ ok: false, error: String(e) }); }
})()"#,
        &[("RESOLVE", RESOLVE_SRC.into()), ("WHAT", js(what)), ("SEL", js(selector)), ("ATTR", js(attr.unwrap_or("")))],
    )
}

/// `is visible|enabled|checked <selector>`.
pub fn is_state_script(what: &str, selector: &str) -> String {
    fill(
        r#"(function () {
  try {
    $RESOLVE
    var what = $WHAT, sel = $SEL;
    var el = __sgResolve(sel);
    if (!el) return JSON.stringify({ ok: true, selector: sel, result: false, reason: 'not_found' });
    if (what === 'enabled') return JSON.stringify({ ok: true, selector: sel, result: !el.disabled });
    if (what === 'checked') return JSON.stringify({ ok: true, selector: sel, result: !!el.checked });
    var win = (el.ownerDocument && el.ownerDocument.defaultView) || window;
    var s = win.getComputedStyle(el);
    var r = el.getBoundingClientRect();
    var vis = s.visibility !== 'hidden' && s.display !== 'none' && s.opacity !== '0' && r.width >= 1 && r.height >= 1;
    return JSON.stringify({ ok: true, selector: sel, result: !!vis });
  } catch (e) { return JSON.stringify({ ok: false, error: String(e) }); }
})()"#,
        &[("RESOLVE", RESOLVE_SRC.into()), ("WHAT", js(what)), ("SEL", js(selector))],
    )
}

/// `find <strategy> <query> <action> [value]`: locate one element and act on it.
pub fn find_script(strategy: &str, query: &str, action: &str, value: &str) -> String {
    fill(
        r#"(function () {
  try {
    $SETVALUE
    var strategy = $STRATEGY, query = $QUERY, action = $ACTION, value = $VALUE;
    function norm(s) { return String(s || '').replace(/\s+/g, ' ').trim(); }
    function matches(el) {
      if (strategy === 'selector') { try { return el.matches(query); } catch (e) { return false; } }
      if (strategy === 'testid') return el.getAttribute('data-testid') === query;
      if (strategy === 'role') { var role = el.getAttribute('role') || ({ A: 'link', BUTTON: 'button', INPUT: 'textbox', SELECT: 'combobox', TEXTAREA: 'textbox' }[el.tagName] || ''); return role === query; }
      if (strategy === 'placeholder') return el.getAttribute('placeholder') === query;
      if (strategy === 'label') {
        var al = el.getAttribute('aria-label'); if (al && norm(al) === norm(query)) return true;
        if (el.id) { try { var lab = document.querySelector('label[for="' + el.id + '"]'); if (lab && norm(lab.innerText || lab.textContent) === norm(query)) return true; } catch (e) {} }
        var wrap = el.closest && el.closest('label'); if (wrap && norm(wrap.innerText || wrap.textContent).indexOf(norm(query)) !== -1) return true;
        return false;
      }
      if (strategy === 'text') return norm(el.innerText || el.textContent).indexOf(norm(query)) !== -1;
      return false;
    }
    var INTERACTIVE = 'a,button,input,textarea,select,summary,[role],[onclick],[tabindex],[contenteditable]';
    var pool;
    if (strategy === 'selector') { try { pool = Array.prototype.slice.call(document.querySelectorAll(query)); } catch (e) { return JSON.stringify({ ok: false, error: 'bad_selector' }); } }
    else pool = Array.prototype.slice.call(document.querySelectorAll(INTERACTIVE));
    var hits = pool.filter(matches);
    if (strategy === 'text') hits.sort(function (a, b) { return norm(a.innerText || a.textContent).length - norm(b.innerText || b.textContent).length; });
    var el = hits[0];
    if (!el) return JSON.stringify({ ok: false, error: 'not_found', strategy: strategy, query: query, candidates: 0 });
    el.scrollIntoView({ block: 'center', inline: 'center' });
    var win = (el.ownerDocument && el.ownerDocument.defaultView) || window;
    if (action === 'click' || action === '') { if (el.focus) try { el.focus(); } catch (e) {} el.click(); return JSON.stringify({ ok: true, action: 'click', tag: el.tagName.toLowerCase() }); }
    if (action === 'hover') { var r = el.getBoundingClientRect(); ['pointerover', 'mouseover', 'mousemove'].forEach(function (t) { try { el.dispatchEvent(new win.MouseEvent(t, { bubbles: true, clientX: r.left + 1, clientY: r.top + 1 })); } catch (e) {} }); return JSON.stringify({ ok: true, action: 'hover' }); }
    if (action === 'focus') { if (el.focus) el.focus(); return JSON.stringify({ ok: true, action: 'focus' }); }
    if (action === 'text') return JSON.stringify({ ok: true, action: 'text', text: norm(el.innerText || el.textContent).slice(0, 5000) });
    if (action === 'check' || action === 'uncheck') { var want = action === 'check'; if (el.checked !== want) { el.checked = want; el.dispatchEvent(new win.Event('change', { bubbles: true })); } return JSON.stringify({ ok: true, action: action, checked: !!el.checked }); }
    if (action === 'fill') {
      if (el.focus) try { el.focus(); } catch (e) {}
      __sgSetValue(el, win, value);
      return JSON.stringify({ ok: true, action: 'fill', value: String(el.value) });
    }
    return JSON.stringify({ ok: false, error: 'unknown_action', action: action });
  } catch (e) { return JSON.stringify({ ok: false, error: String(e) }); }
})()"#,
        &[("SETVALUE", SET_VALUE_SRC.into()), ("STRATEGY", js(strategy)), ("QUERY", js(query)), ("ACTION", js(action)), ("VALUE", js(value))],
    )
}

/// A Promise that polls `check` (an expression yielding a reply object or a
/// falsy value) every 100ms until it answers or `timeout_ms` runs out.
fn poll_script(setup: &str, check: &str, on_timeout: &str, timeout_ms: u64) -> String {
    fill(
        r#"(function () {
  return new Promise(function (resolve) {
    try {
      $SETUP
      var deadline = Date.now() + $MS;
      (function tick() {
        var r;
        try { r = (function () { $CHECK })(); } catch (e) { return resolve(JSON.stringify({ ok: false, error: String(e) })); }
        if (r) return resolve(JSON.stringify(r));
        if (Date.now() >= deadline) return resolve(JSON.stringify($TIMEOUT));
        setTimeout(tick, 100);
      })();
    } catch (e) { resolve(JSON.stringify({ ok: false, error: String(e) })); }
  });
})()"#,
        &[("SETUP", setup.into()), ("CHECK", check.into()), ("TIMEOUT", on_timeout.into()), ("MS", timeout_ms.to_string())],
    )
}

pub fn wait_selector_script(selector: &str, timeout_ms: u64) -> String {
    poll_script(
        &format!(
            "var sel = {}; function visible(el) {{ if (!el) return false; var s = window.getComputedStyle(el); if (s.visibility === 'hidden' || s.display === 'none') return false; var r = el.getBoundingClientRect(); return r.width >= 1 && r.height >= 1; }}",
            js(selector)
        ),
        "var el; try { el = document.querySelector(sel); } catch (e) { return { ok: false, error: 'bad_selector', selector: sel, detail: String(e) }; }\n        return visible(el) ? { ok: true, found: true, selector: sel } : null;",
        "{ ok: false, error: 'timeout', found: false, selector: sel }",
        timeout_ms,
    )
}

pub fn wait_text_script(text: &str, timeout_ms: u64) -> String {
    poll_script(
        &format!("var needle = {};", js(text)),
        "var hay = (document.body ? document.body.innerText : '') || '';\n        return hay.indexOf(needle) !== -1 ? { ok: true, found: true, text: needle } : null;",
        "{ ok: false, error: 'timeout', found: false, text: needle }",
        timeout_ms,
    )
}

pub fn wait_url_script(pattern: &str, timeout_ms: u64) -> String {
    poll_script(
        &format!("var pat = {}; var rx = null; try {{ rx = new RegExp(pat); }} catch (e) {{ rx = null; }}", js(pattern)),
        "var u = location.href; return (rx ? rx.test(u) : u.indexOf(pat) !== -1) ? { ok: true, matched: true, url: u } : null;",
        "{ ok: false, error: 'timeout', url: location.href }",
        timeout_ms,
    )
}

pub fn wait_fn_script(code: &str, timeout_ms: u64) -> String {
    poll_script(
        &format!("var code = {};", js(code)),
        "var v;\n        try { v = (0, eval)(code); } catch (e) { return { ok: false, error: 'fn_threw', detail: String(e) }; }\n        if (!v) return null;\n        var out; try { out = JSON.stringify(v); } catch (e) { out = null; }\n        return { ok: true, satisfied: true, value: out ? JSON.parse(out) : true };",
        "{ ok: false, error: 'timeout', satisfied: false }",
        timeout_ms,
    )
}

/// `document.readyState === 'complete'` as a reply.
pub fn ready_state_script() -> String {
    "JSON.stringify(document.readyState)".into()
}

pub fn storage_script(area: &str, op: &str, key: &str, value: &str) -> String {
    fill(
        r#"(function () {
  try {
    var store = $AREA === 'session' ? window.sessionStorage : window.localStorage;
    var op = $OP, key = $KEY;
    if (op === 'set') { store.setItem(key, $VALUE); return JSON.stringify({ ok: true, set: key }); }
    if (op === 'clear') { store.clear(); return JSON.stringify({ ok: true, cleared: true }); }
    if (op === 'get' && key) return JSON.stringify({ ok: true, key: key, value: store.getItem(key) });
    var all = {};
    for (var i = 0; i < store.length; i++) { var k = store.key(i); all[k] = store.getItem(k); }
    return JSON.stringify({ ok: true, count: store.length, items: all });
  } catch (e) { return JSON.stringify({ ok: false, error: String(e) }); }
})()"#,
        &[("AREA", js(area)), ("OP", js(op)), ("KEY", js(key)), ("VALUE", js(value))],
    )
}

pub fn push_state_script(url: &str) -> String {
    fill(
        r#"(function () {
  try {
    history.pushState({}, '', $URL);
    window.dispatchEvent(new PopStateEvent('popstate', { state: {} }));
    return JSON.stringify({ ok: true, url: location.href });
  } catch (e) { return JSON.stringify({ ok: false, error: String(e) }); }
})()"#,
        &[("URL", js(url))],
    )
}

/// `cookies list|clear` without a cookie store API: what `document.cookie`
/// shows (never HttpOnly ones), and expiring those for this path and domain.
pub fn cookies_script(op: &str) -> String {
    fill(
        r#"(function () {
  try {
    var list = (document.cookie || '').split(/;\s*/).filter(Boolean).map(function (c) {
      var i = c.indexOf('=');
      return { name: i < 0 ? c : c.slice(0, i), value: i < 0 ? '' : c.slice(i + 1), domain: location.hostname };
    });
    if ($OP === 'clear') {
      var host = location.hostname, parts = host.split('.'), paths = location.pathname.split('/');
      list.forEach(function (c) {
        for (var d = 0; d <= parts.length; d++) {
          var dom = d === parts.length ? '' : '; domain=.' + parts.slice(d).join('.');
          for (var p = paths.length; p > 0; p--) {
            document.cookie = c.name + '=; expires=Thu, 01 Jan 1970 00:00:00 GMT' + dom + '; path=' + (paths.slice(0, p).join('/') || '/');
          }
        }
      });
      return JSON.stringify({ ok: true, cleared: true, count: list.length, note: 'HttpOnly cookies are not visible to the page and were kept' });
    }
    return JSON.stringify({ ok: true, count: list.length, cookies: list.slice(0, 200), note: 'from document.cookie: HttpOnly cookies are not listed' });
  } catch (e) { return JSON.stringify({ ok: false, error: String(e) }); }
})()"#,
        &[("OP", js(op))],
    )
}

/// The modifiers of a key combo ("Control+a" → control), as Electron names them.
pub fn parse_key_combo(combo: &str) -> Option<(String, Vec<&'static str>)> {
    let mut parts: Vec<&str> = combo.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
    // "Control++" means the plus key.
    if combo.ends_with("++") || combo == "+" {
        parts.push("+");
    }
    let key = parts.pop()?.to_owned();
    let mods = parts
        .iter()
        .filter_map(|p| match p.to_lowercase().as_str() {
            "ctrl" | "control" => Some("control"),
            "cmd" | "command" | "meta" | "super" => Some("meta"),
            "alt" | "option" | "opt" => Some("alt"),
            "shift" => Some("shift"),
            _ => None,
        })
        .collect();
    Some((key, mods))
}

/// `press <key>`: WKWebView can't inject trusted key events, so dispatch the
/// keydown/keypress/keyup a real press makes and do its default action by hand
/// (insert a character, Backspace/Delete, Enter submits or adds a newline,
/// Tab moves focus, Cmd/Ctrl+A selects all, arrows move the caret or scroll).
pub fn press_script(key: &str, modifiers: &[&str]) -> String {
    let has = |m: &str| modifiers.contains(&m);
    fill(
        r#"(function () {
  try {
    $ACTIVE
    $SETVALUE
    var KEY = $KEY, ctrl = $CTRL, meta = $META, alt = $ALT, shift = $SHIFT;
    var NAMES = { enter: 'Enter', return: 'Enter', tab: 'Tab', esc: 'Escape', escape: 'Escape', space: ' ', backspace: 'Backspace',
      delete: 'Delete', del: 'Delete', up: 'ArrowUp', down: 'ArrowDown', left: 'ArrowLeft', right: 'ArrowRight',
      arrowup: 'ArrowUp', arrowdown: 'ArrowDown', arrowleft: 'ArrowLeft', arrowright: 'ArrowRight', home: 'Home', end: 'End',
      pageup: 'PageUp', pagedown: 'PageDown' };
    var key = NAMES[KEY.toLowerCase()] || KEY;
    var printable = key.length === 1;
    if (printable && shift) key = key.toUpperCase();
    var CODES = { Enter: 13, Tab: 9, Escape: 27, ' ': 32, Backspace: 8, Delete: 46, ArrowUp: 38, ArrowDown: 40, ArrowLeft: 37, ArrowRight: 39, Home: 36, End: 35, PageUp: 33, PageDown: 34 };
    var keyCode = CODES[key] || (printable ? key.toUpperCase().charCodeAt(0) : 0);
    var code = printable ? (/[a-z]/i.test(key) ? 'Key' + key.toUpperCase() : /[0-9]/.test(key) ? 'Digit' + key : '') : (key === ' ' ? 'Space' : key);
    var el = __sgActive() || document.body;
    var win = (el.ownerDocument && el.ownerDocument.defaultView) || window;
    function fire(type) {
      var ev = new win.KeyboardEvent(type, { key: key, code: code, keyCode: keyCode, which: keyCode, charCode: type === 'keypress' ? key.charCodeAt(0) : 0, ctrlKey: ctrl, metaKey: meta, altKey: alt, shiftKey: shift, bubbles: true, cancelable: true, composed: true });
      return el.dispatchEvent(ev);
    }
    var editable = el.isContentEditable || el.tagName === 'TEXTAREA' || (el.tagName === 'INPUT' && !/^(checkbox|radio|button|submit|reset|file|image|range|color)$/.test(el.type));
    var done = 'none';
    if (fire('keydown')) {
      var chord = ctrl || meta;
      if (printable && !chord && !alt && fire('keypress')) {
        if (editable && !el.isContentEditable) {
          var s = el.selectionStart, e = el.selectionEnd, v = el.value || '';
          if (s == null) { s = e = v.length; }
          __sgSetValue(el, win, v.slice(0, s) + key + v.slice(e));
          try { el.setSelectionRange(s + 1, s + 1); } catch (x) {}
          done = 'insert';
        } else if (el.isContentEditable) { document.execCommand('insertText', false, key); done = 'insert'; }
        else if (key === ' ' && (el.tagName === 'BUTTON' || (el.tagName === 'INPUT' && /^(checkbox|radio|button|submit|reset)$/.test(el.type)))) { el.click(); done = 'click'; }
      } else if (chord && key.toLowerCase() === 'a') {
        if (editable && el.select) { el.select(); } else { var sel = win.getSelection(); sel.selectAllChildren(el.isContentEditable ? el : el.ownerDocument.body); }
        done = 'selectAll';
      } else if ((key === 'Backspace' || key === 'Delete') && editable && !el.isContentEditable) {
        var s2 = el.selectionStart, e2 = el.selectionEnd, v2 = el.value || '';
        if (s2 == null) { s2 = e2 = v2.length; }
        if (s2 === e2) { if (key === 'Backspace') s2 = Math.max(0, s2 - 1); else e2 = Math.min(v2.length, e2 + 1); }
        __sgSetValue(el, win, v2.slice(0, s2) + v2.slice(e2));
        try { el.setSelectionRange(s2, s2); } catch (x) {}
        done = 'delete';
      } else if ((key === 'Backspace' || key === 'Delete') && el.isContentEditable) {
        document.execCommand(key === 'Backspace' ? 'delete' : 'forwardDelete'); done = 'delete';
      } else if (key === 'Enter') {
        if (fire('keypress')) {
          if (el.tagName === 'TEXTAREA') { var s3 = el.selectionStart, v3 = el.value || ''; __sgSetValue(el, win, v3.slice(0, s3) + '\n' + v3.slice(el.selectionEnd)); try { el.setSelectionRange(s3 + 1, s3 + 1); } catch (x) {} done = 'newline'; }
          else if (el.isContentEditable) { document.execCommand('insertLineBreak'); done = 'newline'; }
          else if (el.tagName === 'INPUT' && el.form) {
            var form = el.form;
            var btn = form.querySelector('button[type=submit], button:not([type]), input[type=submit], input[type=image]');
            if (btn) { btn.click(); } else if (form.requestSubmit) { form.requestSubmit(); } else { form.submit(); }
            done = 'submit';
          } else if (el.tagName === 'A' || el.tagName === 'BUTTON' || el.getAttribute('role') === 'button' || el.getAttribute('role') === 'link') { el.click(); done = 'click'; }
        }
      } else if (key === 'Tab') {
        var doc = el.ownerDocument || document;
        var all = Array.prototype.slice.call(doc.querySelectorAll('a[href], area[href], button, input:not([type=hidden]), select, textarea, iframe, [tabindex], [contenteditable=""], [contenteditable="true"], summary'))
          .filter(function (n) { if (n.disabled) return false; var t = n.getAttribute('tabindex'); if (t !== null && +t < 0) return false; var r = n.getBoundingClientRect(); return r.width > 0 && r.height > 0; });
        var pos = all.filter(function (n) { return +(n.getAttribute('tabindex') || 0) > 0; }).sort(function (a, b) { return +a.getAttribute('tabindex') - +b.getAttribute('tabindex'); });
        var order = pos.concat(all.filter(function (n) { return !(+(n.getAttribute('tabindex') || 0) > 0); }));
        if (order.length) {
          var i = order.indexOf(el);
          var next = order[shift ? (i <= 0 ? order.length - 1 : i - 1) : (i + 1) % order.length];
          next.focus(); done = 'focus';
        }
      } else if (/^(ArrowUp|ArrowDown|PageUp|PageDown|Home|End|ArrowLeft|ArrowRight)$/.test(key) && !editable) {
        var h = win.innerHeight;
        var by = { ArrowUp: [0, -40], ArrowDown: [0, 40], ArrowLeft: [-40, 0], ArrowRight: [40, 0], PageUp: [0, -h * 0.9], PageDown: [0, h * 0.9] }[key];
        if (by) win.scrollBy(by[0], by[1]); else win.scrollTo(win.scrollX, key === 'Home' ? 0 : el.ownerDocument.documentElement.scrollHeight);
        done = 'scroll';
      } else if (/^(ArrowLeft|ArrowRight|Home|End)$/.test(key) && editable && !el.isContentEditable && el.selectionStart != null) {
        var p = el.selectionStart, len = (el.value || '').length;
        p = key === 'Home' ? 0 : key === 'End' ? len : key === 'ArrowLeft' ? Math.max(0, p - 1) : Math.min(len, p + 1);
        try { el.setSelectionRange(p, p); } catch (x) {}
        done = 'caret';
      }
    } else { done = 'prevented'; }
    fire('keyup');
    var mods = []; if (ctrl) mods.push('control'); if (meta) mods.push('meta'); if (alt) mods.push('alt'); if (shift) mods.push('shift');
    return JSON.stringify({ ok: true, pressed: KEY, modifiers: mods, synthetic: true, defaultAction: done });
  } catch (e) { return JSON.stringify({ ok: false, error: String(e) }); }
})()"#,
        &[
            ("ACTIVE", ACTIVE_SRC.into()),
            ("SETVALUE", SET_VALUE_SRC.into()),
            ("KEY", js(key)),
            ("CTRL", js(has("control"))),
            ("META", js(has("meta"))),
            ("ALT", js(has("alt"))),
            ("SHIFT", js(has("shift"))),
        ],
    )
}

/// A script's value → the reply object it encoded. Our scripts answer with a
/// JSON string; a value that isn't one is passed through as-is.
pub fn decode(value: Value) -> Value {
    match value {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn values_are_json_encoded_into_scripts() {
        let s = click_script(r#"a[title="x"]'); alert(1); ('"#);
        assert!(s.contains(r#"var T = "a[title=\"x\"]'); alert(1); ('";"#), "{s}");
        assert!(!regex::Regex::new(r"\$[A-Z]{2,}").unwrap().is_match(&s), "no placeholder left: {s}");
        let f = fill_script("e3", "line1\nline2 </script>");
        assert!(f.contains(r#""line1\nline2 </script>""#));
        // A value carrying a placeholder name is not substituted into.
        let t = fill_script("$BODY", "$TEXT $SETVALUE");
        assert!(t.contains(r#"var T = "$BODY";"#) && t.contains(r#"var text = "$TEXT $SETVALUE";"#), "{t}");
    }

    #[test]
    fn no_placeholders_left() {
        // `$` only appears in JS regexes (`/[a-z]$/`) — never as one of our `$NAME`s.
        let leftover = regex::Regex::new(r"\$[A-Z]{2,}").unwrap();
        for s in [
            snapshot_script(true),
            type_script("hi"),
            select_script("#s", "v"),
            check_script("#c", false),
            scroll_script("down", 400),
            get_element_script("attr", "a", Some("href")),
            is_state_script("visible", "#x"),
            find_script("text", "Sign in", "click", ""),
            wait_selector_script("#a", 1000),
            wait_text_script("t", 1000),
            wait_url_script("/x", 1000),
            wait_fn_script("window.ok", 1000),
            storage_script("local", "get", "", ""),
            push_state_script("/a"),
            cookies_script("list"),
            press_script("Enter", &["control"]),
        ] {
            assert!(!leftover.is_match(&s), "{s}");
        }
        assert!(snapshot_script(true).contains("var VIEWPORT_ONLY = true;"));
        assert!(wait_text_script("t", 1234).contains("Date.now() + 1234"));
        assert!(press_script("a", &["control", "shift"]).contains("ctrl = true, meta = false, alt = false, shift = true"));
    }

    #[test]
    fn key_combos() {
        assert_eq!(parse_key_combo("Enter"), Some(("Enter".into(), vec![])));
        assert_eq!(parse_key_combo("Control+a"), Some(("a".into(), vec!["control"])));
        assert_eq!(parse_key_combo("cmd+Shift+Z"), Some(("Z".into(), vec!["meta", "shift"])));
        assert_eq!(parse_key_combo("Control++"), Some(("+".into(), vec!["control"])));
        assert_eq!(parse_key_combo(""), None);
    }

    #[test]
    fn decode_script_values() {
        assert_eq!(decode(json!(r#"{"ok":true,"n":1}"#)), json!({ "ok": true, "n": 1 }));
        assert_eq!(decode(json!("not json")), json!("not json"));
        assert_eq!(decode(json!({ "a": 1 })), json!({ "a": 1 }));
    }

    #[test]
    fn ingest_console_and_network() {
        let mut log = PaneLog::default();
        assert!(!log.ingest(r#"{"other":1}"#));
        assert!(!log.ingest("garbage"));
        let first = json!({ "__sg": "bridge",
            "console": [{ "level": "error", "text": "boom", "at": 1.0 }],
            "net": [{ "requestId": "p.1", "url": "http://x/api", "method": "GET", "resourceType": "Fetch" }] });
        assert!(log.ingest(&first.to_string()));
        assert_eq!(log.console.len(), 1);
        assert!(log.requests.is_empty(), "not capturing yet");

        log.capturing = true;
        log.ingest(&first.to_string());
        let update = json!({ "__sg": "bridge", "console": [],
            "net": [{ "requestId": "p.1", "status": 404, "statusText": "Not Found", "mimeType": "text/html", "finished": true },
                    { "requestId": "p.0", "status": 200 }] });
        log.ingest(&update.to_string());
        assert_eq!(log.requests.len(), 1, "an update for an unseen request is dropped");
        assert_eq!(
            serde_json::to_value(&log.requests[0]).unwrap(),
            json!({ "requestId": "p.1", "url": "http://x/api", "method": "GET", "resourceType": "Fetch",
                    "status": 404, "statusText": "Not Found", "mimeType": "text/html", "finished": true })
        );
    }

    #[test]
    fn navigation_grace() {
        let now = std::time::Instant::now();
        let mut log = PaneLog::default();
        assert!(!log.nav_pending(now));
        log.nav_issued = Some(now);
        assert!(log.nav_pending(now + std::time::Duration::from_millis(100)));
        assert!(!log.nav_pending(now + PaneLog::NAV_START_GRACE));
    }

    #[test]
    fn buffers_are_capped() {
        let mut log = PaneLog { capturing: true, ..Default::default() };
        let console: Vec<Value> = (0..250).map(|i| json!({ "level": "log", "text": i.to_string(), "at": 0 })).collect();
        let net: Vec<Value> = (0..350).map(|i| json!({ "requestId": i.to_string(), "url": "u", "method": "GET" })).collect();
        log.ingest(&json!({ "__sg": "bridge", "console": console, "net": net }).to_string());
        assert_eq!(log.console.len(), CONSOLE_CAP);
        assert_eq!(log.console.front().unwrap().text, "50");
        assert_eq!(log.requests.len(), NETWORK_CAP);
        assert_eq!(log.requests[0].request_id, "50");
    }
}
