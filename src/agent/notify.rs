//! macOS notifications for agents that need the user (the Electron app's
//! agent-notifications.ts), through UNUserNotificationCenter.
//!
//! - Silent, like Electron's `silent: true`: the user's chosen sound is played
//!   by `app_settings::play_alert_sound` (afplay, with their volume), focused
//!   or not, so the banner must not add the system sound on top.
//! - One live notification per terminal (identifier `agent.<terminal>`): a newer
//!   alert replaces the last instead of stacking up. Grouped per workspace
//!   (`threadIdentifier`). Withdrawn once the user looks at that workspace.
//! - Clicking one brings forward the window of its environment, switches to the
//!   workspace and focuses the terminal (`windows::reveal_terminal`).
//! - While another SplitGrid window is frontmost the banner still shows (the
//!   window that owns the terminal is not the focused one, else we would not
//!   have posted).
//!
//! UNUserNotificationCenter needs an app bundle: an unbundled `cargo run` only
//! logs what it would have shown.

use std::cell::{OnceCell, RefCell};
use std::collections::HashSet;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, Ordering};

use block2::RcBlock;
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui_kit::App;
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, define_class, msg_send};
use objc2_foundation::{NSArray, NSBundle, NSDictionary, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent, UNNotification, UNNotificationContent, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationResponse, UNNotificationSettings, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};

const MAX_TEXT: usize = 200;
/// System Settings → Notifications (where the user turns ours back on).
pub const SETTINGS_URL: &str = "x-apple.systempreferences:com.apple.Notifications-Settings.extension";

/// What a notification opens when clicked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub env: String,
    pub workspace: String,
    pub terminal: String,
}

/// One banner.
pub struct Alert<'a> {
    pub target: Target,
    pub title: &'a str,
    pub subtitle: Option<&'a str>,
    pub body: &'a str,
}

enum Event {
    Activate(Target),
    /// The authorization status changed: Settings may show or drop its hint.
    Status,
}

/// From any thread (completion handlers run on private queues) to the gpui task.
static EVENTS: OnceLock<UnboundedSender<Event>> = OnceLock::new();
/// UNAuthorizationStatus as last read; -1 until known.
static STATUS: AtomicIsize = AtomicIsize::new(-1);

thread_local! {
    /// The center keeps its delegate weakly.
    static DELEGATE: OnceCell<Retained<Delegate>> = const { OnceCell::new() };
    /// Terminals with a notification out (main thread only).
    static POSTED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Whether this process runs from an app bundle (UNUserNotificationCenter throws without one).
pub fn available() -> bool {
    static BUNDLED: OnceLock<bool> = OnceLock::new();
    *BUNDLED.get_or_init(|| NSBundle::mainBundle().bundleIdentifier().is_some())
}

fn center() -> Retained<UNUserNotificationCenter> {
    UNUserNotificationCenter::currentNotificationCenter()
}

fn send(ev: Event) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.unbounded_send(ev);
    }
}

fn clip(s: &str) -> Retained<NSString> {
    NSString::from_str(&s.chars().take(MAX_TEXT).collect::<String>())
}

fn identifier(terminal: &str) -> String {
    format!("agent.{terminal}")
}

// ── The delegate ─────────────────────────────────────────────────────────────

define_class!(
    // Any thread: the center may call it off the main thread; it only sends on a channel.
    #[unsafe(super(NSObject))]
    #[name = "SplitGridNotificationDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        /// The app is frontmost (another of our windows is focused): show it anyway, silently.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            handler.call((UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(&self, _center: &UNUserNotificationCenter, response: &UNNotificationResponse, handler: &block2::DynBlock<dyn Fn()>) {
            let target = target_of(&response.notification().request().content());
            log::info!("[notify] clicked: {target:?}");
            if !target.env.is_empty() {
                send(Event::Activate(target));
            }
            handler.call(());
        }
    }
);

/// The click target a notification carries in its user info.
fn target_of(content: &UNNotificationContent) -> Target {
    let info = content.userInfo();
    // SAFETY: we only post string → string user info (see `post`).
    let info: &NSDictionary<NSString, NSString> = unsafe { info.cast_unchecked() };
    let get = |k: &str| info.objectForKey(&NSString::from_str(k)).map(|v| v.to_string()).unwrap_or_default();
    Target { env: get("env"), workspace: get("workspace"), terminal: get("terminal") }
}

impl Delegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

// ── Setup and authorization ──────────────────────────────────────────────────

/// Install the click handler; ask for permission now when notifications are on.
pub fn init(enabled: bool, cx: &mut App) {
    if !available() {
        log::info!("[notify] not running from an app bundle: notifications are only logged");
        return;
    }
    let (tx, mut rx) = unbounded();
    let _ = EVENTS.set(tx);
    cx.spawn(async move |cx| {
        while let Some(ev) = rx.next().await {
            cx.update(|cx| match ev {
                Event::Activate(target) => crate::windows::reveal_terminal(target, cx),
                Event::Status => cx.refresh_windows(),
            });
        }
    })
    .detach();

    let delegate = Delegate::new();
    center().setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    DELEGATE.with(|d| {
        let _ = d.set(delegate);
    });
    refresh_status();
    if enabled {
        request_authorization();
    }
    if std::env::var_os("SPLITGRID_DEBUG_NOTIFY").is_some() {
        debug_selftest(cx);
    }
}

/// Ask for permission (the system prompts only the first time; afterwards it
/// answers from the user's choice in System Settings).
pub fn request_authorization() {
    if !available() {
        return;
    }
    let done = RcBlock::new(|granted: Bool, err: *mut NSError| {
        // SAFETY: a valid NSError or null, for the duration of the call.
        match unsafe { err.as_ref() } {
            Some(e) => log::warn!("[notify] authorization request failed: {}", e.localizedDescription()),
            None => log::info!("[notify] authorization granted: {}", granted.as_bool()),
        }
        refresh_status();
    });
    center().requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound, &done);
}

/// Re-read the authorization status (the user may change it in System Settings at any time).
pub fn refresh_status() {
    if !available() {
        return;
    }
    let done = RcBlock::new(|settings: NonNull<UNNotificationSettings>| {
        // SAFETY: valid for the duration of the call.
        let status = unsafe { settings.as_ref() }.authorizationStatus().0;
        if STATUS.swap(status, Ordering::Relaxed) != status {
            log::info!("[notify] authorization status: {}", status_name(status));
            send(Event::Status);
        }
    });
    center().getNotificationSettingsWithCompletionHandler(&done);
}

/// `refresh_status`, at most every few seconds (Settings calls it while drawn).
pub fn refresh_status_throttled() {
    use std::time::{Duration, Instant};
    thread_local!(static LAST: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) });
    let due = LAST.with(|l| {
        let due = l.get().is_none_or(|t| t.elapsed() > Duration::from_secs(3));
        if due {
            l.set(Some(Instant::now()));
        }
        due
    });
    if due {
        refresh_status();
    }
}

/// The user turned SplitGrid Lite's notifications off in System Settings.
pub fn denied() -> bool {
    STATUS.load(Ordering::Relaxed) == UNAuthorizationStatus::Denied.0
}

fn status_name(status: isize) -> &'static str {
    match UNAuthorizationStatus(status) {
        UNAuthorizationStatus::NotDetermined => "not determined",
        UNAuthorizationStatus::Denied => "denied",
        UNAuthorizationStatus::Authorized => "authorized",
        UNAuthorizationStatus::Provisional => "provisional",
        UNAuthorizationStatus::Ephemeral => "ephemeral",
        _ => "unknown",
    }
}

// ── Posting and withdrawing ──────────────────────────────────────────────────

/// Post one banner, replacing the terminal's previous one.
pub fn post(alert: &Alert) {
    let t = &alert.target;
    if !available() {
        log::info!("[notify] (unbundled, not shown) {} — {}", alert.title, alert.body);
        return;
    }
    let content = UNMutableNotificationContent::new();
    content.setTitle(&clip(alert.title));
    content.setBody(&clip(alert.body));
    if let Some(sub) = alert.subtitle {
        content.setSubtitle(&clip(sub));
    }
    content.setThreadIdentifier(&NSString::from_str(&t.workspace));
    let keys = [NSString::from_str("env"), NSString::from_str("workspace"), NSString::from_str("terminal")];
    let values = [NSString::from_str(&t.env), NSString::from_str(&t.workspace), NSString::from_str(&t.terminal)];
    let info = NSDictionary::from_slices(&keys.each_ref().map(|k| &**k), &values.each_ref().map(|v| &**v));
    // SAFETY: strings only: a valid property-list dictionary.
    unsafe { content.setUserInfo(info.cast_unchecked()) };
    // No sound: `play_alert_sound` already played the user's own.
    let id = identifier(&t.terminal);
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&NSString::from_str(&id), &content, None);
    let done = RcBlock::new(move |err: *mut NSError| {
        // SAFETY: a valid NSError or null, for the duration of the call.
        if let Some(e) = unsafe { err.as_ref() } {
            log::warn!("[notify] posting failed: {}", e.localizedDescription());
        }
    });
    center().addNotificationRequest_withCompletionHandler(&request, Some(&done));
    POSTED.with(|p| p.borrow_mut().insert(t.terminal.clone()));
}

/// Take back the notifications of terminals the user is now looking at (or that closed).
pub fn withdraw<'a>(terminals: impl IntoIterator<Item = &'a str>) {
    let ids: Vec<Retained<NSString>> = POSTED.with(|p| {
        let mut p = p.borrow_mut();
        terminals.into_iter().filter(|t| p.remove(*t)).map(|t| NSString::from_str(&identifier(t))).collect()
    });
    if ids.is_empty() || !available() {
        return;
    }
    let ids = NSArray::from_retained_slice(&ids);
    center().removeDeliveredNotificationsWithIdentifiers(&ids);
}

// ── SPLITGRID_DEBUG_NOTIFY: post a test banner, log what the center holds ────

fn log_delivered() {
    let done = RcBlock::new(|list: NonNull<NSArray<UNNotification>>| {
        // SAFETY: valid for the duration of the call.
        let list = unsafe { list.as_ref() };
        log::info!("[notify] delivered: {}", list.count());
        for n in list.iter() {
            let req = n.request();
            let c = req.content();
            log::info!("[notify]   {} thread={} title={:?} body={:?} target={:?}", req.identifier(), c.threadIdentifier(), c.title().to_string(), c.body().to_string(), target_of(&c));
        }
    });
    center().getDeliveredNotificationsWithCompletionHandler(&done);
}

fn debug_selftest(cx: &mut App) {
    cx.spawn(async move |cx| {
        let wait = |secs: u64| cx.background_executor().timer(std::time::Duration::from_secs(secs));
        wait(2).await;
        log::info!("[notify] debug: status = {}", status_name(STATUS.load(Ordering::Relaxed)));
        let target = Target { env: "debug-env".into(), workspace: "debug-ws".into(), terminal: "debug-term".into() };
        post(&Alert { target: target.clone(), title: "SplitGrid Lite · test — Done", subtitle: Some("first"), body: "A test notification (SPLITGRID_DEBUG_NOTIFY)." });
        wait(2).await;
        log_delivered();
        // The same terminal again: replaces the first.
        post(&Alert { target, title: "SplitGrid Lite · test — waiting for you", subtitle: Some("second"), body: "This one replaced the first." });
        wait(2).await;
        log_delivered();
        withdraw(["debug-term"]);
        wait(1).await;
        log_delivered();
    })
    .detach();
}

#[cfg(test)]
mod tests {
    #[test]
    fn one_identifier_per_terminal() {
        assert_eq!(super::identifier("t1"), "agent.t1");
    }

    #[test]
    fn unbundled_test_binary_only_logs() {
        // The test harness isn't an app bundle: posting must not touch the center.
        assert!(!super::available());
        super::post(&super::Alert {
            target: super::Target { env: "e".into(), workspace: "w".into(), terminal: "t".into() },
            title: "t",
            subtitle: None,
            body: "b",
        });
        super::withdraw(["t"]);
    }
}
