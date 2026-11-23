//! The browser-extension channel (the Electron app's agent-browser-channel):
//! the connection to the companion extension, and the tabs the user activated
//! in it as agent-targetable surfaces (`tab:c1.842`). sg-browser routes those
//! surfaces here instead of to a browser pane.
//!
//! Transport: a WebSocket upgraded off the loopback receiver (server.rs) on
//! `/ext`. Any page in the user's browser can reach a loopback socket (no CORS,
//! no preflight), so the gate is twofold:
//!   1. Origin must be chrome-extension:// or moz-extension://. Browsers set it
//!      and scripts can't forge it, so this excludes every web page.
//!   2. A pairing token the user authorised by typing a one-time code shown in
//!      Settings into the extension. This excludes the user's OTHER extensions.
//!
//! Every command is also checked against the settings gates (agentChromeControl,
//! agentChromeWrite) before it leaves the desktop.
//!
//! The core is a state machine over a [`Sink`] and an explicit clock, so the
//! handshake, gates and command correlation are tested without a browser; the
//! WebSocket half ([`accept`]) runs one thread per socket that drains the
//! outgoing queue, reads with a short timeout and sweeps the timers.
//!
//! Lite keeps its own pairing file (`lite-browser-extension-pairings.json`) in
//! the shared data dir: Lite's extension is a different extension, and the
//! Electron app's store must not be touched.

use std::collections::HashMap;
use std::io::Write as _;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use futures::channel::oneshot;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::ext_protocol::{self as proto, Command, Identity, Tab, errors};

pub const PAIRINGS_FILE: &str = "lite-browser-extension-pairings.json";

/// A 6-digit code is only 10^6 wide: attempts are capped per pairing WINDOW
/// (not per socket, so reconnecting doesn't reset it), and blowing it burns the code.
const MAX_PAIRING_ATTEMPTS: u32 = 5;

/// Screenshots come back as base64 in one frame: room for a full-page PNG.
const MAX_MESSAGE: usize = 32 * 1024 * 1024;

/// The socket loop's tick: how often it sweeps timers while idle.
const TICK: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub command_timeout_ms: u64,
    pub heartbeat_ms: u64,
    pub pairing_ttl_ms: u64,
    pub auth_timeout_ms: u64,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            command_timeout_ms: proto::COMMAND_TIMEOUT_MS,
            heartbeat_ms: proto::HEARTBEAT_MS,
            pairing_ttl_ms: proto::PAIRING_TTL_MS,
            auth_timeout_ms: proto::AUTH_TIMEOUT_MS,
        }
    }
}

/// The one thing the channel needs from a transport.
pub trait Sink: Send {
    fn send(&self, text: String) -> bool;
    fn close(&self, code: u16, reason: &str);
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtResult {
    pub ok: bool,
    pub data: Map<String, Value>,
    pub error: Option<String>,
}

impl ExtResult {
    pub fn err(error: &str) -> Self {
        Self { ok: false, data: Map::new(), error: Some(error.into()) }
    }

    fn err_msg(error: &str, message: impl Into<String>) -> Self {
        Self { ok: false, data: Map::from_iter([("message".to_owned(), message.into().into())]), error: Some(error.into()) }
    }
}

/// An activated tab, as an agent-targetable surface.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SurfaceInfo {
    pub surface: String,
    pub client_id: String,
    pub tab_id: u64,
    pub title: String,
    pub url: String,
    pub origin: String,
    pub writable: bool,
    pub browser: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClientInfo {
    pub client_id: String,
    pub identity: Identity,
    pub connected_at: u64,
}

/// A stored pairing, for Settings (no token).
#[derive(Clone, Debug, PartialEq)]
pub struct PairedInfo {
    pub client_id: String,
    pub browser: String,
    pub extension_id: String,
    pub paired_at: u64,
    pub connected: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct PairingRecord {
    extension_id: String,
    token: String,
    #[serde(default)]
    paired_at: u64,
    #[serde(default)]
    browser: String,
    /// The browser's stable client id, persisted so a surface an agent holds
    /// survives service-worker evictions and app restarts.
    #[serde(default)]
    client_id: String,
}

struct Pending {
    tx: oneshot::Sender<ExtResult>,
    deadline: u64,
}

struct Client {
    sid: u64,
    /// Agent-facing id (`c1`); empty until authenticated.
    id: String,
    /// Log label: `s3` until authenticated, then the client id.
    tag: String,
    sink: Box<dyn Sink>,
    /// The extension id the browser vouched for in Origin.
    origin_host: String,
    identity: Option<Identity>,
    authed: bool,
    connected_at: u64,
    tabs: Vec<Tab>,
    pending: HashMap<String, Pending>,
    missed_pongs: u32,
    next_ping: u64,
    auth_deadline: u64,
}

struct PairingWindow {
    code: String,
    expires_at: u64,
    attempts: u32,
}

#[derive(Default)]
struct Inner {
    clients: Vec<Client>,
    socket_counter: u64,
    req_counter: u64,
    pairing: Option<PairingWindow>,
    /// None = not loaded from disk yet.
    pairings: Option<Vec<PairingRecord>>,
}

pub struct Channel {
    inner: Mutex<Inner>,
    pairings_file: PathBuf,
    settings_file: PathBuf,
    timing: Timing,
}

/// Wall clock in ms (what the socket loop and the UI pass as `now`).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Constant-time compare. Length isn't hidden (tokens and codes are fixed-width);
/// an empty string never matches.
pub fn constant_time_equals(a: &str, b: &str) -> bool {
    !a.is_empty() && super::constant_time_eq(a.as_bytes(), b.as_bytes())
}

/// The extension id an Origin header vouches for, or None if it isn't an
/// extension origin. Lenient about the id itself (Chrome: 32 a–p, Firefox: a
/// UUID): the SCHEME is what's trusted here, the token tells extensions apart.
pub fn extension_origin_host(origin: Option<&str>) -> Option<String> {
    let o = origin?.trim();
    let (scheme, host) = o.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("chrome-extension") && !scheme.eq_ignore_ascii_case("moz-extension") {
        return None;
    }
    let valid = !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    valid.then(|| host.to_owned())
}

fn device_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname NUL-terminates within it on success.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let name = if ok { String::from_utf8_lossy(&buf[..buf.iter().position(|&b| b == 0).unwrap_or(0)]).into_owned() } else { String::new() };
    if name.is_empty() { "SplitGrid Lite".into() } else { name }
}

fn sanitize_identity(v: &Value) -> Option<Identity> {
    let id = v.get("extensionId")?.as_str().filter(|s| !s.is_empty())?;
    Some(Identity {
        extension_id: id.to_owned(),
        browser: v.get("browser").and_then(Value::as_str).unwrap_or("browser").to_owned(),
        version: v.get("version").and_then(Value::as_str).unwrap_or("0").to_owned(),
    })
}

/// Tabs come from outside the process: normalise rather than trust the shape.
fn sanitize_tabs(v: &Value, now: u64) -> Vec<Tab> {
    let Some(list) = v.as_array() else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|t| {
            let tab_id = t.get("tabId")?.as_u64()?;
            let s = |k: &str| t.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
            Some(Tab {
                tab_id,
                title: s("title"),
                url: s("url"),
                origin: s("origin"),
                writable: t.get("writable") == Some(&Value::Bool(true)),
                activated_at: t.get("activatedAt").and_then(Value::as_f64).unwrap_or(now as f64),
            })
        })
        .collect()
}

impl Channel {
    /// `data_dir`: where app-settings.json lives (the gates) and the pairings go.
    pub fn new(data_dir: &Path) -> Self {
        Self::with_timing(data_dir, Timing::default())
    }

    pub fn with_timing(data_dir: &Path, timing: Timing) -> Self {
        Self { inner: Mutex::new(Inner::default()), pairings_file: data_dir.join(PAIRINGS_FILE), settings_file: data_dir.join("app-settings.json"), timing }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ── Settings gates ───────────────────────────────────────────────────────

    /// (control, write), read fresh per command: the user may flip them in
    /// Settings at any time. Fails closed on any read error.
    pub fn gates(&self) -> (bool, bool) {
        let Some(v) = std::fs::read(&self.settings_file).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else {
            return (false, false);
        };
        let on = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
        (on("agentChromeControl"), on("agentChromeWrite"))
    }

    // ── Pairing store ────────────────────────────────────────────────────────

    fn load_pairings<'a>(&self, inner: &'a mut Inner) -> &'a mut Vec<PairingRecord> {
        if inner.pairings.is_none() {
            let mut records: Vec<PairingRecord> = match std::fs::read(&self.pairings_file) {
                Ok(bytes) => match serde_json::from_slice::<Vec<Value>>(&bytes) {
                    Ok(list) => list
                        .into_iter()
                        .filter_map(|v| serde_json::from_value::<PairingRecord>(v).ok())
                        .filter(|r| !r.extension_id.is_empty() && !r.token.is_empty())
                        .map(|mut r| {
                            if !proto::is_client_id(&r.client_id) {
                                r.client_id.clear();
                            }
                            if r.browser.is_empty() {
                                r.browser = "browser".into();
                            }
                            r
                        })
                        .collect(),
                    // A corrupt file must not brick the channel: the user can pair again.
                    Err(e) => {
                        log::error!("[ext] failed to read pairings: {e}");
                        Vec::new()
                    }
                },
                Err(_) => Vec::new(),
            };
            // A record without an id (or a duplicate) is adopted in place, not dropped.
            let mut seen: Vec<String> = Vec::new();
            let mut adopted = false;
            for i in 0..records.len() {
                let id = records[i].client_id.clone();
                if !id.is_empty() && !seen.contains(&id) {
                    seen.push(id);
                    continue;
                }
                records[i].client_id.clear();
                let taken = |id: &str| seen.iter().any(|s| s == id) || records.iter().any(|r| r.client_id == id) || inner.clients.iter().any(|c| c.id == id);
                let fresh = (1..).map(|n| format!("c{n}")).find(|id| !taken(id)).unwrap_or_default();
                records[i].client_id = fresh.clone();
                seen.push(fresh);
                adopted = true;
            }
            if adopted {
                self.save_pairings(&records);
            }
            inner.pairings = Some(records);
        }
        inner.pairings.as_mut().expect("loaded above")
    }

    fn save_pairings(&self, records: &[PairingRecord]) {
        if let Err(e) = write_private(&self.pairings_file, &serde_json::to_vec_pretty(records).unwrap_or_default()) {
            log::error!("[ext] failed to persist pairings: {e}");
        }
    }

    /// The lowest `c<n>` neither a stored pairing nor a live client holds:
    /// tab ids collide across browsers, so the client id may never be reused.
    fn allocate_client_id(&self, inner: &mut Inner) -> String {
        self.load_pairings(inner);
        let taken =
            |inner: &Inner, id: &str| inner.pairings.as_ref().is_some_and(|p| p.iter().any(|r| r.client_id == id)) || inner.clients.iter().any(|c| c.id == id);
        (1..).map(|n| format!("c{n}")).find(|id| !taken(inner, id)).unwrap_or_default()
    }

    /// One live token per extension id: re-pairing replaces the old one, keeping its client id.
    fn remember_pairing(&self, inner: &mut Inner, extension_id: &str, token: String, paired_at: u64, browser: &str) -> PairingRecord {
        let prior = self.load_pairings(inner).iter().find(|r| r.extension_id == extension_id).map(|r| r.client_id.clone()).filter(|id| !id.is_empty());
        let client_id = prior.unwrap_or_else(|| self.allocate_client_id(inner));
        let record = PairingRecord { extension_id: extension_id.into(), token, paired_at, browser: browser.into(), client_id };
        let all = self.load_pairings(inner);
        all.retain(|r| r.extension_id != extension_id);
        all.push(record.clone());
        let snapshot = all.clone();
        self.save_pairings(&snapshot);
        record
    }

    /// The stored pairing for this token, bound to the extension id the browser
    /// vouched for (a leaked token is useless to another extension). Every
    /// record is compared, no early exit.
    fn find_pairing(&self, inner: &mut Inner, token: &str, extension_id: &str) -> Option<PairingRecord> {
        let mut found = None;
        for r in self.load_pairings(inner).iter() {
            if constant_time_equals(&r.token, token) && r.extension_id == extension_id {
                found = Some(r.clone());
            }
        }
        found
    }

    // ── Connection lifecycle ─────────────────────────────────────────────────

    /// Register a freshly connected socket: unauthenticated, without a client id,
    /// until it pairs or says hello.
    pub fn attach(&self, sink: Box<dyn Sink>, origin_host: &str, now: u64) -> u64 {
        let mut inner = self.lock();
        inner.socket_counter += 1;
        let sid = inner.socket_counter;
        inner.clients.push(Client {
            sid,
            id: String::new(),
            tag: format!("s{sid}"),
            sink,
            origin_host: origin_host.into(),
            identity: None,
            authed: false,
            connected_at: now,
            tabs: Vec::new(),
            pending: HashMap::new(),
            missed_pongs: 0,
            next_ping: 0,
            auth_deadline: now + self.timing.auth_timeout_ms,
        });
        log::info!("[ext] s{sid} connected from {origin_host}");
        sid
    }

    /// Forget a socket that's already gone; settles everything it had pending.
    pub fn detach(&self, sid: u64, why: &str) {
        let mut inner = self.lock();
        detach_locked(&mut inner, sid, why);
    }

    /// Is the socket still a client (not dropped)?
    #[cfg(test)]
    pub fn alive(&self, sid: u64) -> bool {
        self.lock().clients.iter().any(|c| c.sid == sid)
    }

    /// The timers of one socket: auth timeout, heartbeat, command deadlines.
    pub fn tick(&self, sid: u64, now: u64) {
        let mut inner = self.lock();
        let Some(c) = inner.clients.iter_mut().find(|c| c.sid == sid) else {
            return;
        };
        let drop = if !c.authed {
            (now >= c.auth_deadline).then_some((4008, "auth timeout"))
        } else {
            let expired: Vec<String> = c.pending.iter().filter(|(_, p)| now >= p.deadline).map(|(k, _)| k.clone()).collect();
            for k in expired {
                if let Some(p) = c.pending.remove(&k) {
                    let _ = p.tx.send(ExtResult::err(errors::TIMEOUT));
                }
            }
            // Besides spotting a dead socket, the pings keep the MV3 worker alive.
            if now < c.next_ping {
                None
            } else if c.missed_pongs >= 2 {
                Some((4009, "heartbeat lost"))
            } else {
                c.missed_pongs += 1;
                c.next_ping = now + self.timing.heartbeat_ms;
                send(c, json!({ "t": "ping" }));
                None
            }
        };
        if let Some((code, reason)) = drop {
            drop_locked(&mut inner, sid, code, reason);
        }
    }

    /// Settle the agent-facing id at authentication. Two profiles running the
    /// same extension share its id but not their tab ids, so a second LIVE
    /// connection gets a distinct id for its lifetime; both stay drivable.
    fn assign_client_id(&self, inner: &mut Inner, sid: u64, record: &PairingRecord) {
        let stable_is_live = inner.clients.iter().any(|c| c.sid != sid && c.id == record.client_id);
        let id = if stable_is_live { self.allocate_client_id(inner) } else { record.client_id.clone() };
        if stable_is_live {
            log::info!("[ext] {} is already live as {} (second browser profile): this connection is {id}", record.extension_id, record.client_id);
        }
        if let Some(c) = inner.clients.iter_mut().find(|c| c.sid == sid) {
            c.id = id.clone();
            c.tag = id;
        }
    }

    fn authenticate(&self, inner: &mut Inner, sid: u64, identity: Identity, record: &PairingRecord, reply: Value, now: u64) {
        self.assign_client_id(inner, sid, record);
        let Some(c) = inner.clients.iter_mut().find(|c| c.sid == sid) else {
            return;
        };
        log::info!("[ext] {} authenticated: {} ({})", c.tag, identity.browser, identity.extension_id);
        c.identity = Some(identity);
        c.authed = true;
        c.missed_pongs = 0;
        c.next_ping = now + self.timing.heartbeat_ms;
        send(c, reply);
    }

    /// Handle one frame from a socket.
    pub fn handle_message(&self, sid: u64, raw: &str, now: u64) {
        let mut inner = self.lock();
        let Some(ix) = inner.clients.iter().position(|c| c.sid == sid) else {
            return;
        };
        let msg: Option<Value> = serde_json::from_str::<Value>(raw).ok().filter(|v| v.get("t").is_some_and(Value::is_string));
        let Some(msg) = msg else {
            drop_locked(&mut inner, sid, 1003, "unparseable message");
            return;
        };
        let t = msg["t"].as_str().unwrap_or_default();
        // Only the handshake is allowed before authentication: an unpaired
        // extension must not seed the tab registry or spoof a result.
        if t != "pair" && t != "hello" && !inner.clients[ix].authed {
            drop_locked(&mut inner, sid, 4001, "not authenticated");
            return;
        }
        match t {
            "pair" => self.handle_pair(&mut inner, sid, &msg, now),
            "hello" => self.handle_hello(&mut inner, sid, &msg, now),
            "pong" => inner.clients[ix].missed_pongs = 0,
            "tabs" => inner.clients[ix].tabs = sanitize_tabs(&msg["tabs"], now),
            "result" => {
                let c = &mut inner.clients[ix];
                let req_id = msg["reqId"].as_str().unwrap_or_default();
                // Timed out already, or never issued.
                if let Some(p) = c.pending.remove(req_id) {
                    let _ = p.tx.send(ExtResult {
                        ok: msg["ok"] == Value::Bool(true),
                        data: msg["data"].as_object().cloned().unwrap_or_default(),
                        error: msg["error"].as_str().map(str::to_owned),
                    });
                }
            }
            // Forward-compat: ignore what a newer extension invents.
            _ => {}
        }
    }

    /// The protocol version and the identity (which must be the one Origin vouched for).
    fn check_handshake(&self, inner: &mut Inner, sid: u64, msg: &Value) -> Option<Identity> {
        let c = inner.clients.iter().find(|c| c.sid == sid)?;
        let (tag, origin_host) = (c.tag.clone(), c.origin_host.clone());
        if msg["protocol"].as_u64() != Some(proto::PROTOCOL_VERSION) {
            let reason = format!("protocol mismatch: desktop speaks v{}, extension sent v{}", proto::PROTOCOL_VERSION, msg["protocol"]);
            log::warn!("[ext] {tag} {reason}");
            send(c, json!({ "t": "pair-fail", "reason": reason }));
            drop_locked(inner, sid, 4002, &format!("protocol mismatch (v{} expected)", proto::PROTOCOL_VERSION));
            return None;
        }
        match sanitize_identity(&msg["identity"]) {
            Some(id) if id.extension_id == origin_host => Some(id),
            _ => {
                log::warn!("[ext] {tag} identity does not match origin {origin_host}");
                send(c, json!({ "t": "pair-fail", "reason": "identity_mismatch" }));
                drop_locked(inner, sid, 4003, "identity mismatch");
                None
            }
        }
    }

    fn fail_pair(inner: &mut Inner, sid: u64, reason: &str) {
        if let Some(c) = inner.clients.iter().find(|c| c.sid == sid) {
            log::warn!("[ext] {} pairing rejected ({reason})", c.tag);
            send(c, json!({ "t": "pair-fail", "reason": reason }));
        }
        drop_locked(inner, sid, 4004, &format!("pairing rejected: {reason}"));
    }

    fn handle_pair(&self, inner: &mut Inner, sid: u64, msg: &Value, now: u64) {
        if inner.clients.iter().any(|c| c.sid == sid && c.authed) {
            return;
        }
        let Some(identity) = self.check_handshake(inner, sid, msg) else {
            return;
        };
        let Some(window) = inner.pairing.as_mut().filter(|p| now <= p.expires_at) else {
            inner.pairing = None;
            return Self::fail_pair(inner, sid, "no_pairing_window");
        };
        window.attempts += 1;
        if window.attempts > MAX_PAIRING_ATTEMPTS {
            // Burn the window: a fresh socket must not get a fresh budget.
            inner.pairing = None;
            return Self::fail_pair(inner, sid, "too_many_attempts");
        }
        if !msg["code"].as_str().is_some_and(|code| constant_time_equals(code, &window.code)) {
            return Self::fail_pair(inner, sid, "bad_code");
        }
        inner.pairing = None; // single use
        let token: String = rand::random::<[u8; 32]>().iter().map(|b| format!("{b:02x}")).collect();
        let record = self.remember_pairing(inner, &identity.extension_id, token.clone(), now, &identity.browser);
        self.authenticate(inner, sid, identity, &record, json!({ "t": "pair-ok", "token": token, "device": device_name() }), now);
    }

    fn handle_hello(&self, inner: &mut Inner, sid: u64, msg: &Value, now: u64) {
        if inner.clients.iter().any(|c| c.sid == sid && c.authed) {
            return;
        }
        let Some(identity) = self.check_handshake(inner, sid, msg) else {
            return;
        };
        let token = msg["token"].as_str().unwrap_or_default();
        let Some(record) = self.find_pairing(inner, token, &identity.extension_id) else {
            log::warn!("[ext] s{sid} unknown token from {}: closing", identity.extension_id);
            drop_locked(inner, sid, 4001, "not paired");
            return;
        };
        // The persisted id: an evicted service worker gets the SAME surfaces back.
        self.authenticate(inner, sid, identity, &record, json!({ "t": "ready", "device": device_name() }), now);
    }

    // ── Pairing (Settings) ───────────────────────────────────────────────────

    /// Mint a one-time code for the user to type into the extension: (code, expires at ms).
    pub fn begin_pairing(&self, now: u64) -> (String, u64) {
        // rand's thread RNG is a CSPRNG: this code alone authorises handing an
        // agent the user's logged-in browser.
        let code = format!("{:06}", rand::random_range(0..1_000_000u32));
        let expires_at = now + self.timing.pairing_ttl_ms;
        self.lock().pairing = Some(PairingWindow { code: code.clone(), expires_at, attempts: 0 });
        log::info!("[ext] pairing window open for {}s", self.timing.pairing_ttl_ms / 1000);
        (code, expires_at)
    }

    /// Close the pairing window if `code` is still its code (Settings, per window).
    pub fn cancel_pairing_code(&self, code: &str) {
        let mut inner = self.lock();
        if inner.pairing.as_ref().is_some_and(|p| p.code == code) {
            inner.pairing = None;
            log::info!("[ext] pairing window cancelled");
        }
    }

    #[cfg(test)]
    pub fn cancel_pairing(&self) {
        if self.lock().pairing.take().is_some() {
            log::info!("[ext] pairing window cancelled");
        }
    }

    /// Forget the pairing behind `client_id` (its token stops working) and drop
    /// every live connection that holds it.
    pub fn unpair(&self, client_id: &str) {
        let mut inner = self.lock();
        let live_ext = inner.clients.iter().find(|c| c.id == client_id).and_then(|c| c.identity.as_ref()).map(|i| i.extension_id.clone());
        let records = self.load_pairings(&mut inner);
        let ext = records.iter().find(|r| r.client_id == client_id).map(|r| r.extension_id.clone()).or(live_ext);
        let Some(ext) = ext else { return };
        records.retain(|r| r.extension_id != ext);
        let snapshot = records.clone();
        self.save_pairings(&snapshot);
        let sids: Vec<u64> = inner.clients.iter().filter(|c| c.identity.as_ref().is_some_and(|i| i.extension_id == ext)).map(|c| c.sid).collect();
        for sid in sids {
            drop_locked(&mut inner, sid, 4001, "unpaired");
        }
        log::info!("[ext] unpaired {ext}");
    }

    // ── Queries ──────────────────────────────────────────────────────────────

    pub fn is_connected(&self) -> bool {
        self.lock().clients.iter().any(|c| c.authed)
    }

    pub fn clients(&self) -> Vec<ClientInfo> {
        self.lock()
            .clients
            .iter()
            .filter(|c| c.authed)
            .filter_map(|c| Some(ClientInfo { client_id: c.id.clone(), identity: c.identity.clone()?, connected_at: c.connected_at }))
            .collect()
    }

    /// Every activated tab across every connected browser.
    pub fn surfaces(&self) -> Vec<SurfaceInfo> {
        let inner = self.lock();
        inner
            .clients
            .iter()
            .filter(|c| c.authed)
            .flat_map(|c| {
                c.tabs.iter().map(move |t| SurfaceInfo {
                    surface: proto::format_surface(&c.id, t.tab_id),
                    client_id: c.id.clone(),
                    tab_id: t.tab_id,
                    title: t.title.clone(),
                    url: t.url.clone(),
                    origin: t.origin.clone(),
                    writable: t.writable,
                    browser: c.identity.as_ref().map(|i| i.browser.clone()).unwrap_or_else(|| "browser".into()),
                })
            })
            .collect()
    }

    /// The stored pairings, and whether each is connected now.
    pub fn paired(&self) -> Vec<PairedInfo> {
        let mut inner = self.lock();
        let records = self.load_pairings(&mut inner).clone();
        records
            .into_iter()
            .map(|r| PairedInfo {
                connected: inner.clients.iter().any(|c| c.authed && c.identity.as_ref().is_some_and(|i| i.extension_id == r.extension_id)),
                client_id: r.client_id,
                browser: r.browser,
                extension_id: r.extension_id,
                paired_at: r.paired_at,
            })
            .collect()
    }

    // ── Commands ─────────────────────────────────────────────────────────────

    /// Send one command to a tab surface. The error now, or a receiver for the
    /// extension's reply (settled with `ext_timeout` past the command timeout,
    /// `no_extension` if the socket goes). Both gates are checked HERE, on every
    /// command: the extension checks its own switch too, neither trusts the other.
    pub fn run_command(&self, surface: &str, command: &Command, now: u64) -> Result<oneshot::Receiver<ExtResult>, ExtResult> {
        let (control, write) = self.gates();
        if !control {
            return Err(ExtResult::err_msg(
                errors::DISABLED,
                "Enable \"Let agents inspect a tab in your real browser\" in SplitGrid Lite Settings → Agent integrations.",
            ));
        }
        if command.is_write() && !write {
            return Err(ExtResult::err_msg(
                errors::WRITE_DENIED,
                format!("\"{}\" needs \"Allow agents to interact with pages\" in SplitGrid Lite Settings → Agent integrations.", command.op()),
            ));
        }
        let Some((client_id, tab_id)) = proto::parse_surface(surface) else {
            return Err(ExtResult::err_msg(errors::UNKNOWN_TAB, format!("not a browser tab surface: {surface}")));
        };
        let mut inner = self.lock();
        let connected: Vec<usize> = (0..inner.clients.len()).filter(|&i| inner.clients[i].authed).collect();
        if connected.is_empty() {
            return Err(ExtResult::err(errors::NO_EXTENSION));
        }
        let ids = || Value::from(connected.iter().map(|&i| inner.clients[i].id.clone()).collect::<Vec<_>>());
        let ix = match client_id {
            Some(cid) => match connected.iter().copied().find(|&i| inner.clients[i].id == cid) {
                Some(i) => i,
                None => return Err(stale_client_error(&inner, &connected, &cid, tab_id)),
            },
            None if connected.len() > 1 => {
                let mut r = ExtResult::err_msg(
                    errors::AMBIGUOUS_TAB,
                    format!(
                        "{} browsers are connected — qualify the tab, e.g. {}",
                        connected.len(),
                        proto::format_surface(&inner.clients[connected[0]].id, tab_id)
                    ),
                );
                r.data.insert("clients".into(), ids());
                return Err(r);
            }
            None => connected[0],
        };
        if !inner.clients[ix].tabs.iter().any(|t| t.tab_id == tab_id) {
            return Err(ExtResult::err_msg(errors::UNKNOWN_TAB, format!("tab {tab_id} is not activated for agent control")));
        }
        inner.req_counter += 1;
        let req_id = format!("x{}", inner.req_counter);
        let (tx, rx) = oneshot::channel();
        let deadline = now + self.timing.command_timeout_ms;
        let c = &mut inner.clients[ix];
        if !send(c, json!({ "t": "cmd", "reqId": req_id, "tabId": tab_id, "command": command })) {
            return Err(ExtResult::err(errors::NO_EXTENSION));
        }
        c.pending.insert(req_id, Pending { tx, deadline });
        Ok(rx)
    }
}

/// A surface naming a client nothing is connected as: say what IS connected and
/// where the tab lives now, if it's still reachable. Never retarget silently.
fn stale_client_error(inner: &Inner, connected: &[usize], client_id: &str, tab_id: u64) -> ExtResult {
    let ids: Vec<String> = connected.iter().map(|&i| inner.clients[i].id.clone()).collect();
    let mut data = Map::new();
    data.insert("clientId".into(), client_id.into());
    data.insert("clients".into(), ids.clone().into());
    match connected.iter().map(|&i| &inner.clients[i]).find(|c| c.tabs.iter().any(|t| t.tab_id == tab_id)) {
        None => {
            data.insert(
                "message".into(),
                format!("no browser is connected as {client_id} (connected: {}) — list the browser surfaces again for a current one", ids.join(", ")).into(),
            );
        }
        Some(holder) => {
            let current = proto::format_surface(&holder.id, tab_id);
            let bare = if connected.len() == 1 { format!(", or the bare tab:{tab_id} form") } else { String::new() };
            let browser = holder.identity.as_ref().map(|i| i.browser.as_str()).unwrap_or("browser");
            data.insert(
                "message".into(),
                format!(
                    "{client_id} is not connected — that surface belongs to a client that reconnected. Tab {tab_id} is still activated on {} ({browser}): use {current}{bare}.",
                    holder.id
                )
                .into(),
            );
            data.insert("surface".into(), current.into());
        }
    }
    ExtResult { ok: false, data, error: Some(errors::NO_EXTENSION.into()) }
}

fn send(c: &Client, msg: Value) -> bool {
    let ok = c.sink.send(msg.to_string());
    if !ok {
        log::error!("[ext] {} send failed", c.tag);
    }
    ok
}

fn detach_locked(inner: &mut Inner, sid: u64, why: &str) {
    let Some(ix) = inner.clients.iter().position(|c| c.sid == sid) else {
        return;
    };
    let c = inner.clients.remove(ix);
    for (_, p) in c.pending {
        let _ = p.tx.send(ExtResult::err(errors::NO_EXTENSION));
    }
    log::info!("[ext] {} disconnected ({why})", c.tag);
}

/// Close a client's socket and forget it.
fn drop_locked(inner: &mut Inner, sid: u64, code: u16, reason: &str) {
    if let Some(c) = inner.clients.iter().find(|c| c.sid == sid) {
        c.sink.close(code, reason);
    }
    detach_locked(inner, sid, reason);
}

/// Write `bytes` as a 0600 file via a temp file + rename: these tokens ARE the
/// credential for driving the user's browser.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

// ─── WebSocket transport ─────────────────────────────────────────────────────

enum Out {
    Text(String),
    Close(u16, String),
}

struct QueueSink(Mutex<mpsc::Sender<Out>>);

impl Sink for QueueSink {
    fn send(&self, text: String) -> bool {
        self.0.lock().map(|tx| tx.send(Out::Text(text)).is_ok()).unwrap_or(false)
    }
    fn close(&self, code: u16, reason: &str) {
        if let Ok(tx) = self.0.lock() {
            let _ = tx.send(Out::Close(code, reason.into()));
        }
    }
}

/// The upgrade request's relevant headers.
#[derive(Default, Debug)]
pub struct Upgrade {
    pub origin: Option<String>,
    pub upgrade: Option<String>,
    pub key: Option<String>,
}

fn refuse(stream: &mut TcpStream, status: &str) -> std::io::Result<()> {
    write!(stream, "HTTP/1.1 {status}\r\nconnection: close\r\ncontent-length: 0\r\n\r\n")?;
    stream.flush()
}

/// Serve `GET /ext` on `stream` (its request head already read; `leftover` is
/// whatever the reader buffered past it). Returns when the socket closes.
pub fn accept(channel: Arc<Channel>, mut stream: TcpStream, leftover: Vec<u8>, head: &Upgrade) -> std::io::Result<()> {
    use tungstenite::protocol::frame::CloseFrame;
    use tungstenite::protocol::{Role, WebSocketConfig};
    use tungstenite::{Message, WebSocket};

    let Some(origin_host) = extension_origin_host(head.origin.as_deref()) else {
        log::warn!("[ext] upgrade refused, origin={}", head.origin.as_deref().unwrap_or("(none)"));
        return refuse(&mut stream, "403 Forbidden");
    };
    let key = head.key.as_deref().filter(|_| head.upgrade.as_deref().is_some_and(|u| u.eq_ignore_ascii_case("websocket")));
    let Some(key) = key else {
        return refuse(&mut stream, "400 Bad Request");
    };
    let accept = tungstenite::handshake::derive_accept_key(key.trim().as_bytes());
    write!(stream, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n")?;
    stream.flush()?;
    stream.set_read_timeout(Some(TICK))?;
    stream.set_write_timeout(None)?;

    let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE)).max_frame_size(Some(MAX_MESSAGE));
    let mut ws = WebSocket::from_partially_read(stream, leftover, Role::Server, Some(config));
    let (tx, rx) = mpsc::channel();
    let sid = channel.attach(Box::new(QueueSink(Mutex::new(tx))), &origin_host, now_ms());

    'socket: loop {
        // Outgoing first: a drop queues its pair-fail and close before forgetting the client.
        loop {
            match rx.try_recv() {
                Ok(Out::Text(t)) => {
                    if ws.send(Message::text(t)).is_err() {
                        break 'socket;
                    }
                }
                Ok(Out::Close(code, reason)) => {
                    let _ = ws.close(Some(CloseFrame { code: code.into(), reason: reason.into() }));
                    let _ = ws.flush();
                    break 'socket;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                // The client was dropped and its queue is drained.
                Err(mpsc::TryRecvError::Disconnected) => break 'socket,
            }
        }
        match ws.read() {
            Ok(Message::Text(t)) => channel.handle_message(sid, t.as_str(), now_ms()),
            Ok(Message::Binary(b)) => channel.handle_message(sid, &String::from_utf8_lossy(&b), now_ms()),
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => {
                log::debug!("[ext] s{sid} socket: {e}");
                break;
            }
        }
        channel.tick(sid, now_ms());
    }
    channel.detach(sid, "socket closed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt as _;

    const EXT_ID: &str = "abcdefghijklmnopabcdefghijklmnop";
    const OTHER_EXT: &str = "ponmlkjihgfedcbaponmlkjihgfedcba";

    #[derive(Clone, Default)]
    struct Fake {
        sent: Arc<Mutex<Vec<Value>>>,
        closed: Arc<Mutex<Option<(u16, String)>>>,
    }

    impl Sink for Fake {
        fn send(&self, text: String) -> bool {
            self.sent.lock().unwrap().push(serde_json::from_str(&text).unwrap());
            true
        }
        fn close(&self, code: u16, reason: &str) {
            *self.closed.lock().unwrap() = Some((code, reason.into()));
        }
    }

    impl Fake {
        fn last(&self) -> Value {
            self.sent.lock().unwrap().last().cloned().unwrap_or(Value::Null)
        }
        fn closed(&self) -> Option<u16> {
            self.closed.lock().unwrap().as_ref().map(|(c, _)| *c)
        }
        fn of_type(&self, t: &str) -> Vec<Value> {
            self.sent.lock().unwrap().iter().filter(|m| m["t"] == t).cloned().collect()
        }
    }

    struct Env {
        dir: tempfile::TempDir,
        ch: Channel,
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        set_gates(dir.path(), true, true);
        let timing = Timing { heartbeat_ms: 60_000, auth_timeout_ms: 60_000, ..Timing::default() };
        let ch = Channel::with_timing(dir.path(), timing);
        Env { dir, ch }
    }

    fn set_gates(dir: &Path, control: bool, write: bool) {
        std::fs::write(dir.join("app-settings.json"), json!({ "agentChromeControl": control, "agentChromeWrite": write }).to_string()).unwrap();
    }

    fn ident(ext: &str) -> Value {
        json!({ "extensionId": ext, "browser": "Chrome 141", "version": "1.0.0" })
    }

    fn connect(ch: &Channel, ext: &str) -> (u64, Fake) {
        let fake = Fake::default();
        let sid = ch.attach(Box::new(fake.clone()), ext, 0);
        (sid, fake)
    }

    /// Pair a fresh socket; returns (sid, sink, token).
    fn pair(ch: &Channel, ext: &str) -> (u64, Fake, String) {
        let (code, _) = ch.begin_pairing(0);
        let (sid, fake) = connect(ch, ext);
        ch.handle_message(sid, &json!({ "t": "pair", "protocol": 1, "code": code, "identity": ident(ext) }).to_string(), 0);
        let ok = fake.last();
        assert_eq!(ok["t"], "pair-ok", "{ok}");
        (sid, fake, ok["token"].as_str().unwrap().to_owned())
    }

    fn hello(ch: &Channel, ext: &str, token: &str) -> (u64, Fake) {
        let (sid, fake) = connect(ch, ext);
        ch.handle_message(sid, &json!({ "t": "hello", "protocol": 1, "token": token, "identity": ident(ext) }).to_string(), 0);
        (sid, fake)
    }

    fn push_tab(ch: &Channel, sid: u64, tab_id: u64, writable: bool) {
        let tabs = json!([{ "tabId": tab_id, "title": "Local", "url": "http://localhost:3000/", "origin": "http://localhost:3000", "writable": writable, "activatedAt": 1 }]);
        ch.handle_message(sid, &json!({ "t": "tabs", "tabs": tabs }).to_string(), 0);
    }

    fn now_or_never(rx: &mut oneshot::Receiver<ExtResult>) -> Option<ExtResult> {
        rx.now_or_never().map(|r| r.unwrap_or_else(|_| ExtResult::err("cancelled")))
    }

    // ── Origin gate ──────────────────────────────────────────────────────────

    #[test]
    fn accepts_only_extension_origins() {
        assert_eq!(extension_origin_host(Some(&format!("chrome-extension://{EXT_ID}"))).as_deref(), Some(EXT_ID));
        assert!(extension_origin_host(Some("moz-extension://f81d4fae-7dec-11d0-a765-00a0c91e6bf6")).is_some());
        for bad in [
            None,
            Some(""),
            Some("null"),
            Some("http://localhost:3000"),
            Some("https://evil.example"),
            Some("file://"),
            Some("https://chrome-extension://x"),
            Some("chrome-extension://"),
            Some("ws://127.0.0.1:19558"),
            Some("chrome-extension://a/b"),
        ] {
            assert_eq!(extension_origin_host(bad), None, "{bad:?}");
        }
    }

    // ── Pairing ──────────────────────────────────────────────────────────────

    #[test]
    fn pairing_codes() {
        let e = env();
        let (code, expires) = e.ch.begin_pairing(1000);
        assert!(code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()));
        assert_eq!(expires, 1000 + proto::PAIRING_TTL_MS);
    }

    #[test]
    fn wrong_code_is_rejected_and_closed() {
        let e = env();
        let (code, _) = e.ch.begin_pairing(0);
        let wrong = if code == "000000" { "000001" } else { "000000" };
        let (sid, fake) = connect(&e.ch, EXT_ID);
        e.ch.handle_message(sid, &json!({ "t": "pair", "protocol": 1, "code": wrong, "identity": ident(EXT_ID) }).to_string(), 0);
        assert_eq!(fake.last(), json!({ "t": "pair-fail", "reason": "bad_code" }));
        assert_eq!(fake.closed(), Some(4004));
        assert!(!e.ch.alive(sid) && !e.ch.is_connected());
    }

    #[test]
    fn expired_code_is_rejected() {
        let e = env();
        let (code, expires) = e.ch.begin_pairing(0);
        let (sid, fake) = connect(&e.ch, EXT_ID);
        e.ch.handle_message(sid, &json!({ "t": "pair", "protocol": 1, "code": code, "identity": ident(EXT_ID) }).to_string(), expires + 1);
        assert_eq!(fake.last()["reason"], "no_pairing_window");
    }

    #[test]
    fn brute_force_burns_the_code() {
        let e = env();
        let (code, _) = e.ch.begin_pairing(0);
        let wrong = if code == "000000" { "000001" } else { "000000" };
        for _ in 0..MAX_PAIRING_ATTEMPTS {
            let (sid, fake) = connect(&e.ch, EXT_ID);
            e.ch.handle_message(sid, &json!({ "t": "pair", "protocol": 1, "code": wrong, "identity": ident(EXT_ID) }).to_string(), 0);
            assert_eq!(fake.last()["reason"], "bad_code");
        }
        // Even the right code is refused now, and the window is gone.
        let (sid, fake) = connect(&e.ch, EXT_ID);
        e.ch.handle_message(sid, &json!({ "t": "pair", "protocol": 1, "code": code, "identity": ident(EXT_ID) }).to_string(), 0);
        assert_eq!(fake.last()["reason"], "too_many_attempts");
        let (sid, fake) = connect(&e.ch, EXT_ID);
        e.ch.handle_message(sid, &json!({ "t": "pair", "protocol": 1, "code": code, "identity": ident(EXT_ID) }).to_string(), 0);
        assert_eq!(fake.last()["reason"], "no_pairing_window");
    }

    #[test]
    fn right_code_works_exactly_once() {
        let e = env();
        let (code, _) = e.ch.begin_pairing(0);
        let (sid, fake) = connect(&e.ch, EXT_ID);
        e.ch.handle_message(sid, &json!({ "t": "pair", "protocol": 1, "code": code, "identity": ident(EXT_ID) }).to_string(), 0);
        let ok = fake.last();
        assert_eq!(ok["t"], "pair-ok");
        assert_eq!(ok["token"].as_str().unwrap().len(), 64);
        assert!(e.ch.is_connected());
        assert_eq!(e.ch.clients()[0].client_id, "c1");

        let (sid2, fake2) = connect(&e.ch, OTHER_EXT);
        e.ch.handle_message(sid2, &json!({ "t": "pair", "protocol": 1, "code": code, "identity": ident(OTHER_EXT) }).to_string(), 0);
        assert_eq!(fake2.last()["reason"], "no_pairing_window");
        assert!(e.ch.alive(sid));
    }

    #[test]
    fn identity_must_match_the_origin() {
        let e = env();
        let (code, _) = e.ch.begin_pairing(0);
        let (sid, fake) = connect(&e.ch, EXT_ID);
        e.ch.handle_message(sid, &json!({ "t": "pair", "protocol": 1, "code": code, "identity": ident(OTHER_EXT) }).to_string(), 0);
        assert_eq!(fake.last()["reason"], "identity_mismatch");
        assert_eq!(fake.closed(), Some(4003));
    }

    #[test]
    fn protocol_mismatch_says_why() {
        let e = env();
        let (code, _) = e.ch.begin_pairing(0);
        let (sid, fake) = connect(&e.ch, EXT_ID);
        e.ch.handle_message(sid, &json!({ "t": "pair", "protocol": 2, "code": code, "identity": ident(EXT_ID) }).to_string(), 0);
        assert!(fake.last()["reason"].as_str().unwrap().contains("protocol mismatch"));
        assert_eq!(fake.closed(), Some(4002));
    }

    // ── Hello ────────────────────────────────────────────────────────────────

    #[test]
    fn paired_client_returns_with_its_token() {
        let e = env();
        let (sid, _, token) = pair(&e.ch, EXT_ID);
        e.ch.detach(sid, "evicted");
        assert!(!e.ch.is_connected());
        let (sid, fake) = hello(&e.ch, EXT_ID, &token);
        assert_eq!(fake.last()["t"], "ready");
        assert!(e.ch.alive(sid) && e.ch.is_connected());
        // The token is on disk, 0600, and in Lite's own file.
        let file = e.dir.path().join(PAIRINGS_FILE);
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn unknown_or_stolen_token_is_closed() {
        let e = env();
        let (sid, _, token) = pair(&e.ch, EXT_ID);
        e.ch.detach(sid, "x");
        let (sid, fake) = hello(&e.ch, EXT_ID, &"0".repeat(64));
        assert_eq!(fake.closed(), Some(4001));
        assert!(!e.ch.alive(sid));
        // Another extension replaying the real token.
        let (_, fake) = hello(&e.ch, OTHER_EXT, &token);
        assert_eq!(fake.closed(), Some(4001));
        let (_, fake) = hello(&e.ch, EXT_ID, "");
        assert_eq!(fake.closed(), Some(4001));
    }

    #[test]
    fn unauthenticated_socket_cannot_seed_tabs_or_fake_results() {
        let e = env();
        let (sid, fake) = connect(&e.ch, EXT_ID);
        push_tab(&e.ch, sid, 5, true);
        assert_eq!(fake.closed(), Some(4001));
        assert!(e.ch.surfaces().is_empty());
        let (sid, fake) = connect(&e.ch, EXT_ID);
        e.ch.handle_message(sid, r#"{"t":"result","reqId":"x1","ok":true}"#, 0);
        assert_eq!(fake.closed(), Some(4001));
        let (sid, fake) = connect(&e.ch, EXT_ID);
        e.ch.handle_message(sid, "not json", 0);
        assert_eq!(fake.closed(), Some(1003));
    }

    // ── Client ids ───────────────────────────────────────────────────────────

    #[test]
    fn ids_survive_eviction_and_restart() {
        let e = env();
        let (sid, _, token) = pair(&e.ch, EXT_ID);
        e.ch.detach(sid, "evicted");
        let (_, _) = hello(&e.ch, EXT_ID, &token);
        assert_eq!(e.ch.clients()[0].client_id, "c1");
        // A restart: a new channel over the same files.
        let ch2 = Channel::with_timing(e.dir.path(), e.ch.timing);
        let _ = hello(&ch2, EXT_ID, &token);
        assert_eq!(ch2.clients()[0].client_id, "c1");
    }

    #[test]
    fn two_extensions_get_distinct_stable_ids() {
        let e = env();
        let (s1, _, t1) = pair(&e.ch, EXT_ID);
        let (s2, _, t2) = pair(&e.ch, OTHER_EXT);
        e.ch.detach(s1, "x");
        e.ch.detach(s2, "x");
        // Reconnecting in the other order changes nothing.
        let _ = hello(&e.ch, OTHER_EXT, &t2);
        let _ = hello(&e.ch, EXT_ID, &t1);
        let mut ids: Vec<(String, String)> = e.ch.clients().into_iter().map(|c| (c.identity.extension_id, c.client_id)).collect();
        ids.sort();
        assert_eq!(ids, [(EXT_ID.to_owned(), "c1".to_owned()), (OTHER_EXT.to_owned(), "c2".to_owned())]);
    }

    #[test]
    fn adopts_pairings_without_ids() {
        let e = env();
        let file = e.dir.path().join(PAIRINGS_FILE);
        std::fs::write(&file, json!([{ "extensionId": EXT_ID, "token": "a".repeat(64), "pairedAt": 1, "browser": "Chrome" }, { "extensionId": OTHER_EXT, "token": "b".repeat(64), "clientId": "bogus" }]).to_string()).unwrap();
        let ids: Vec<String> = e.ch.paired().into_iter().map(|p| p.client_id).collect();
        assert_eq!(ids, ["c1", "c2"]);
        let saved: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(saved[0]["clientId"], "c1");
        let _ = hello(&e.ch, OTHER_EXT, &"b".repeat(64));
        assert_eq!(e.ch.clients()[0].client_id, "c2");
    }

    #[test]
    fn corrupt_pairings_file_is_survivable() {
        let e = env();
        std::fs::write(e.dir.path().join(PAIRINGS_FILE), "{nope").unwrap();
        assert!(e.ch.paired().is_empty());
        let _ = pair(&e.ch, EXT_ID);
    }

    #[test]
    fn two_profiles_of_one_extension_both_drivable() {
        let e = env();
        let (s1, f1, token) = pair(&e.ch, EXT_ID);
        let (s2, f2) = hello(&e.ch, EXT_ID, &token);
        assert!(e.ch.alive(s1) && e.ch.alive(s2));
        let mut ids: Vec<String> = e.ch.clients().into_iter().map(|c| c.client_id).collect();
        ids.sort();
        assert_eq!(ids, ["c1", "c2"]);
        push_tab(&e.ch, s1, 7, true);
        push_tab(&e.ch, s2, 7, true);
        let mut rx = e.ch.run_command("tab:c2.7", &Command::Info, 0).unwrap();
        let cmd = f2.of_type("cmd");
        assert_eq!(cmd.len(), 1);
        assert!(f1.of_type("cmd").is_empty());
        e.ch.handle_message(s2, &json!({ "t": "result", "reqId": cmd[0]["reqId"], "ok": true, "data": { "url": "u" } }).to_string(), 0);
        assert!(now_or_never(&mut rx).unwrap().ok);
    }

    #[test]
    fn stale_client_id_says_where_the_tab_went() {
        let e = env();
        let (s1, _, token) = pair(&e.ch, EXT_ID);
        let (s2, _) = hello(&e.ch, EXT_ID, &token); // c2 (second profile)
        push_tab(&e.ch, s2, 9, false);
        e.ch.detach(s1, "x");
        let err = e.ch.run_command("tab:c1.9", &Command::Info, 0).unwrap_err();
        assert_eq!(err.error.as_deref(), Some(errors::NO_EXTENSION));
        assert_eq!(err.data["surface"], "tab:c2.9");
        assert!(err.data["message"].as_str().unwrap().contains("bare tab:9"));
        let err = e.ch.run_command("tab:c1.10", &Command::Info, 0).unwrap_err();
        assert!(err.data["message"].as_str().unwrap().contains("no browser is connected as c1"));
    }

    // ── Commands ─────────────────────────────────────────────────────────────

    #[test]
    fn surfaces_from_the_last_tab_push() {
        let e = env();
        let (sid, _, _) = pair(&e.ch, EXT_ID);
        push_tab(&e.ch, sid, 842, true);
        let s = e.ch.surfaces();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].surface, "tab:c1.842");
        assert_eq!((s[0].browser.as_str(), s[0].writable, s[0].origin.as_str()), ("Chrome 141", true, "http://localhost:3000"));
        // Junk tabs are dropped.
        e.ch.handle_message(sid, r#"{"t":"tabs","tabs":[{"tabId":-1},{"tabId":"x"},{"tabId":3}]}"#, 0);
        assert_eq!(e.ch.surfaces().iter().map(|s| s.tab_id).collect::<Vec<_>>(), [3]);
    }

    #[test]
    fn results_are_correlated_and_strays_ignored() {
        let e = env();
        let (sid, fake, _) = pair(&e.ch, EXT_ID);
        push_tab(&e.ch, sid, 5, true);
        let mut rx = e.ch.run_command("tab:c1.5", &Command::Text, 0).unwrap();
        let cmd = fake.last();
        assert_eq!((cmd["t"].as_str(), cmd["tabId"].as_u64(), &cmd["command"]), (Some("cmd"), Some(5), &json!({ "op": "text" })));
        e.ch.handle_message(sid, r#"{"t":"result","reqId":"nope","ok":true}"#, 0);
        assert!(now_or_never(&mut rx).is_none());
        assert!(e.ch.alive(sid));
        e.ch.handle_message(sid, &json!({ "t": "result", "reqId": cmd["reqId"], "ok": true, "data": { "text": "hi" } }).to_string(), 0);
        let r = now_or_never(&mut rx).unwrap();
        assert!(r.ok);
        assert_eq!(r.data["text"], "hi");
    }

    #[test]
    fn unanswered_commands_time_out() {
        let e = env();
        let (sid, _, _) = pair(&e.ch, EXT_ID);
        push_tab(&e.ch, sid, 5, true);
        let mut rx = e.ch.run_command("tab:c1.5", &Command::Info, 0).unwrap();
        e.ch.tick(sid, proto::COMMAND_TIMEOUT_MS - 1);
        assert!(now_or_never(&mut rx).is_none());
        e.ch.tick(sid, proto::COMMAND_TIMEOUT_MS);
        assert_eq!(now_or_never(&mut rx).unwrap().error.as_deref(), Some(errors::TIMEOUT));
    }

    #[test]
    fn disconnect_settles_pending() {
        let e = env();
        let (sid, _, _) = pair(&e.ch, EXT_ID);
        push_tab(&e.ch, sid, 5, true);
        let mut a = e.ch.run_command("tab:c1.5", &Command::Info, 0).unwrap();
        let mut b = e.ch.run_command("tab:5", &Command::Html, 0).unwrap();
        e.ch.detach(sid, "gone");
        assert_eq!(now_or_never(&mut a).unwrap().error.as_deref(), Some(errors::NO_EXTENSION));
        assert_eq!(now_or_never(&mut b).unwrap().error.as_deref(), Some(errors::NO_EXTENSION));
    }

    #[test]
    fn command_errors() {
        let e = env();
        assert_eq!(e.ch.run_command("tab:c1.5", &Command::Info, 0).unwrap_err().error.as_deref(), Some(errors::NO_EXTENSION));
        let (s1, _, _) = pair(&e.ch, EXT_ID);
        push_tab(&e.ch, s1, 5, true);
        assert_eq!(e.ch.run_command("tab:c1.6", &Command::Info, 0).unwrap_err().error.as_deref(), Some(errors::UNKNOWN_TAB));
        assert_eq!(e.ch.run_command("tab:zz", &Command::Info, 0).unwrap_err().error.as_deref(), Some(errors::UNKNOWN_TAB));
        assert!(e.ch.run_command("tab:5", &Command::Info, 0).is_ok(), "bare form, one browser");
        let _ = pair(&e.ch, OTHER_EXT);
        let err = e.ch.run_command("tab:5", &Command::Info, 0).unwrap_err();
        assert_eq!(err.error.as_deref(), Some(errors::AMBIGUOUS_TAB));
        assert!(err.data["message"].as_str().unwrap().contains("tab:c1.5"));
    }

    #[test]
    fn settings_gates() {
        let e = env();
        let (sid, fake, _) = pair(&e.ch, EXT_ID);
        push_tab(&e.ch, sid, 5, true);
        set_gates(e.dir.path(), true, false);
        let before = fake.of_type("cmd").len();
        let err = e.ch.run_command("tab:c1.5", &Command::Eval { js: "1".into() }, 0).unwrap_err();
        assert_eq!(err.error.as_deref(), Some(errors::WRITE_DENIED));
        assert_eq!(e.ch.run_command("tab:c1.5", &Command::Reload, 0).unwrap_err().error.as_deref(), Some(errors::WRITE_DENIED));
        assert_eq!(fake.of_type("cmd").len(), before, "nothing sent");
        for c in [Command::Info, Command::Text, Command::Html, Command::Screenshot { full_page: None }, Command::Release] {
            assert!(e.ch.run_command("tab:c1.5", &c, 0).is_ok(), "{c:?}");
        }
        set_gates(e.dir.path(), true, true);
        assert!(e.ch.run_command("tab:c1.5", &Command::Navigate { url: "http://localhost:3000/x".into() }, 0).is_ok());
        set_gates(e.dir.path(), false, true);
        assert_eq!(e.ch.run_command("tab:c1.5", &Command::Info, 0).unwrap_err().error.as_deref(), Some(errors::DISABLED));
        // No settings at all: closed.
        std::fs::remove_file(e.dir.path().join("app-settings.json")).unwrap();
        assert_eq!(e.ch.gates(), (false, false));
    }

    #[test]
    fn heartbeat_drops_a_silent_client() {
        let e = env();
        let hb = e.ch.timing.heartbeat_ms;
        let (sid, fake, _) = pair(&e.ch, EXT_ID);
        e.ch.tick(sid, hb);
        e.ch.tick(sid, 2 * hb);
        assert_eq!(fake.of_type("ping").len(), 2);
        assert!(e.ch.alive(sid));
        e.ch.tick(sid, 3 * hb);
        assert_eq!(fake.closed(), Some(4009));
        assert!(!e.ch.alive(sid));

        let (sid, fake, _) = pair(&e.ch, EXT_ID);
        for n in 1..6 {
            e.ch.tick(sid, n * hb);
            e.ch.handle_message(sid, r#"{"t":"pong"}"#, n * hb);
        }
        assert!(e.ch.alive(sid) && fake.closed().is_none());
    }

    #[test]
    fn auth_timeout() {
        let e = env();
        let (sid, fake) = connect(&e.ch, EXT_ID);
        e.ch.tick(sid, e.ch.timing.auth_timeout_ms - 1);
        assert!(e.ch.alive(sid));
        e.ch.tick(sid, e.ch.timing.auth_timeout_ms);
        assert_eq!(fake.closed(), Some(4008));
    }

    #[test]
    fn unpair_forgets_the_token_and_drops_the_browser() {
        let e = env();
        let (sid, fake, token) = pair(&e.ch, EXT_ID);
        let (other, _, _) = pair(&e.ch, OTHER_EXT);
        assert_eq!(e.ch.paired().len(), 2);
        e.ch.unpair("c1");
        assert_eq!(fake.closed(), Some(4001));
        assert!(!e.ch.alive(sid) && e.ch.alive(other));
        assert_eq!(e.ch.paired().iter().map(|p| p.client_id.as_str()).collect::<Vec<_>>(), ["c2"]);
        let (_, fake) = hello(&e.ch, EXT_ID, &token);
        assert_eq!(fake.closed(), Some(4001), "the token no longer works");
        // And from disk too.
        let ch2 = Channel::new(e.dir.path());
        assert_eq!(ch2.paired().len(), 1);
    }

    #[test]
    fn cancel_pairing_closes_the_window() {
        let e = env();
        let (code, _) = e.ch.begin_pairing(0);
        e.ch.cancel_pairing_code("not-it");
        let (new_code, _) = e.ch.begin_pairing(0);
        e.ch.cancel_pairing_code(&code);
        assert!(e.ch.lock().pairing.as_ref().is_some_and(|p| p.code == new_code), "a stale code leaves the newer window open");
        e.ch.cancel_pairing_code(&new_code);
        assert!(e.ch.lock().pairing.is_none());
        e.ch.begin_pairing(0);
        e.ch.cancel_pairing();
        let (sid, fake) = connect(&e.ch, EXT_ID);
        e.ch.handle_message(sid, &json!({ "t": "pair", "protocol": 1, "code": code, "identity": ident(EXT_ID) }).to_string(), 0);
        assert_eq!(fake.last()["reason"], "no_pairing_window");
    }
}
