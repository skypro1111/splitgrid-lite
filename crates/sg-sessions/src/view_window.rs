//! "View": a read-only window on a session's recent output, drawn the way the
//! terminal draws it: the session's own column count (no soft wrapping), the
//! app's terminal colours, font and background (sg_ptyd::screen::render_styled),
//! refreshed every second.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSBackgroundColorAttributeName, NSBackingStoreType, NSColor, NSFont, NSFontAttributeName,
    NSFontManager, NSFontTraitMask, NSForegroundColorAttributeName, NSScreen, NSStrikethroughStyleAttributeName, NSTextView, NSUnderlineStyleAttributeName, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{NSNumber, NSRange, NSSize, NSString};
use sg_ptyd::client::Conn;
use sg_ptyd::screen::{BACKGROUND, Run, render_styled};

use crate::actions::bring_forward;
use crate::ui::{rect, regular};

const INSET: f64 = 12.;

struct ViewWin {
    window: Retained<NSWindow>,
    text: Retained<NSTextView>,
    socket: PathBuf,
    fonts: Fonts,
    ended: bool,
    last: Vec<Vec<Run>>,
    sized: bool,
}

thread_local! {
    static VIEWS: RefCell<Vec<ViewWin>> = const { RefCell::new(Vec::new()) };
}

/// The terminal font in its four faces, plus the cell width.
struct Fonts {
    regular: Retained<NSFont>,
    bold: Retained<NSFont>,
    italic: Retained<NSFont>,
    bold_italic: Retained<NSFont>,
    cell_w: f64,
    line_h: f64,
}

/// The app's settings next to the session sockets: <data>/ptyd/s/x.sock → <data>/app-settings.json.
fn app_settings(socket: &Path) -> serde_json::Map<String, serde_json::Value> {
    socket
        .ancestors()
        .nth(3)
        .and_then(|d| std::fs::read(d.join("app-settings.json")).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn fonts(settings: &serde_json::Map<String, serde_json::Value>) -> Fonts {
    let family = settings.get("terminalFontFamily").and_then(|v| v.as_str()).unwrap_or("Menlo");
    let size = settings.get("terminalFontSize").and_then(|v| v.as_f64()).unwrap_or(13.).clamp(9., 24.);
    let regular_font = NSFont::fontWithName_size(&NSString::from_str(family), size).unwrap_or_else(|| NSFont::monospacedSystemFontOfSize_weight(size, regular()));
    let manager = NSFontManager::sharedFontManager(MainThreadMarker::new().expect("main thread"));
    let with = |f: &NSFont, t: NSFontTraitMask| manager.convertFont_toHaveTrait(f, t);
    let bold = with(&regular_font, NSFontTraitMask::BoldFontMask);
    let italic = with(&regular_font, NSFontTraitMask::ItalicFontMask);
    let bold_italic = with(&bold, NSFontTraitMask::ItalicFontMask);
    let cell_w = regular_font.maximumAdvancement().width.min(size * 0.7).max(size * 0.5);
    let line_h = (size * 1.3).round();
    Fonts { regular: regular_font, bold, italic, bold_italic, cell_w, line_h }
}

/// The app background (backgroundColor), or the default base.
fn background(settings: &serde_json::Map<String, serde_json::Value>) -> u32 {
    settings
        .get("backgroundColor")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().trim_start_matches('#'))
        .filter(|h| h.len() == 6)
        .and_then(|h| u32::from_str_radix(h, 16).ok())
        .unwrap_or(BACKGROUND)
}

fn color(c: u32) -> Retained<NSColor> {
    let ch = |s: u32| ((c >> s) & 0xff) as f64 / 255.;
    NSColor::colorWithSRGBRed_green_blue_alpha(ch(16), ch(8), ch(0), 1.0)
}

pub fn open(socket: PathBuf, label: &str, mtm: MainThreadMarker) {
    // Already open: bring it forward.
    let existing = VIEWS.with(|v| v.borrow().iter().find(|w| w.socket == socket && w.window.isVisible()).map(|w| w.window.clone()));
    bring_forward(mtm);
    if let Some(w) = existing {
        w.makeKeyAndOrderFront(None);
        return;
    }
    let settings = app_settings(&socket);
    let fonts = fonts(&settings);
    let bg = color(background(&settings));

    let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Resizable | NSWindowStyleMask::Miniaturizable;
    let window = unsafe { NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), rect(0., 0., 820., 520.), style, NSBackingStoreType::Buffered, false) };
    // SAFETY: we keep our own reference (VIEWS) and drop it once it's closed.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(&format!("{label} — read-only")));
    // SAFETY: an AppKit constant.
    window.setAppearance(NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }).as_deref());
    window.setBackgroundColor(Some(&bg));
    window.setMinSize(NSSize::new(360., 200.));

    let scroll = NSTextView::scrollableTextView(mtm);
    let Some(text) = scroll.documentView().and_then(|v| v.downcast::<NSTextView>().ok()) else { return };
    text.setEditable(false);
    text.setSelectable(true);
    text.setRichText(true);
    text.setFont(Some(&fonts.regular));
    text.setBackgroundColor(&bg);
    text.setTextContainerInset(NSSize::new(INSET, INSET));
    // No soft wrapping: a row is as wide as the terminal's, as on screen.
    text.setHorizontallyResizable(true);
    text.setMaxSize(NSSize::new(f64::MAX, f64::MAX));
    // SAFETY: the text view's own container, configured on the main thread.
    if let Some(container) = unsafe { text.textContainer() } {
        container.setWidthTracksTextView(false);
        container.setContainerSize(NSSize::new(f64::MAX, f64::MAX));
    }
    scroll.setHasHorizontalScroller(true);
    scroll.setDrawsBackground(true);
    scroll.setBackgroundColor(&bg);
    window.setContentView(Some(&scroll));
    window.center();
    window.makeKeyAndOrderFront(None);
    VIEWS.with(|v| v.borrow_mut().push(ViewWin { window, text, socket, fonts, ended: false, last: Vec::new(), sized: false }));
    refresh();
}

/// Size the window to the terminal: `cols` cells wide, `rows` lines tall, within the screen.
fn fit(w: &ViewWin, cols: u16, rows: u16) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let width = cols as f64 * w.fonts.cell_w + 2. * INSET + 20.;
    let height = rows as f64 * w.fonts.line_h + 2. * INSET;
    let screen = NSScreen::mainScreen(mtm).map(|s| s.visibleFrame().size).unwrap_or(NSSize::new(1440., 900.));
    w.window.setContentSize(NSSize::new(width.min(screen.width - 40.).max(360.), height.min(screen.height - 60.).max(200.)));
    w.window.center();
}

fn paint(w: &ViewWin, rows: &[Vec<Run>]) {
    // SAFETY: the text view's own storage, edited on the main thread.
    let Some(storage) = (unsafe { w.text.textStorage() }) else { return };
    let mut plain = String::new();
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            plain.push('\n');
        }
        for r in row {
            plain.push_str(&r.text);
        }
    }
    storage.beginEditing();
    w.text.setString(&NSString::from_str(&plain));
    let all = NSRange::new(0, plain.encode_utf16().count());
    let default_fg = color(sg_ptyd::screen::FOREGROUND);
    // SAFETY: AppKit attribute keys with values of the documented types.
    unsafe {
        storage.addAttribute_value_range(NSFontAttributeName, AsRef::<AnyObject>::as_ref(&*w.fonts.regular), all);
        storage.addAttribute_value_range(NSForegroundColorAttributeName, AsRef::<AnyObject>::as_ref(&*default_fg), all);
    }
    let one = NSNumber::numberWithInteger(1);
    let mut at = 0usize;
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            at += 1;
        }
        for r in row {
            let len = r.text.encode_utf16().count();
            let range = NSRange::new(at, len);
            at += len;
            if len == 0 {
                continue;
            }
            // SAFETY: as above; ranges are within the string just set.
            unsafe {
                if r.fg != sg_ptyd::screen::FOREGROUND {
                    storage.addAttribute_value_range(NSForegroundColorAttributeName, AsRef::<AnyObject>::as_ref(&*color(r.fg)), range);
                }
                if let Some(bg) = r.bg {
                    storage.addAttribute_value_range(NSBackgroundColorAttributeName, AsRef::<AnyObject>::as_ref(&*color(bg)), range);
                }
                let face = match (r.bold, r.italic) {
                    (false, false) => None,
                    (true, false) => Some(&w.fonts.bold),
                    (false, true) => Some(&w.fonts.italic),
                    (true, true) => Some(&w.fonts.bold_italic),
                };
                if let Some(f) = face {
                    storage.addAttribute_value_range(NSFontAttributeName, AsRef::<AnyObject>::as_ref(&**f), range);
                }
                if r.underline {
                    storage.addAttribute_value_range(NSUnderlineStyleAttributeName, AsRef::<AnyObject>::as_ref(&*one), range);
                }
                if r.strike {
                    storage.addAttribute_value_range(NSStrikethroughStyleAttributeName, AsRef::<AnyObject>::as_ref(&*one), range);
                }
            }
        }
    }
    storage.endEditing();
    w.text.scrollRangeToVisible(NSRange::new(at, 0));
}

/// Re-render every open View window from its session's recent output.
pub fn refresh() {
    VIEWS.with(|v| {
        v.borrow_mut().retain_mut(|w| {
            if !w.window.isVisible() {
                return false;
            }
            if w.ended {
                return true;
            }
            let shot = Conn::connect(&w.socket, Duration::from_millis(500)).and_then(|mut c| c.snapshot(512 * 1024));
            match shot {
                Ok((size, bytes)) => {
                    if !w.sized {
                        fit(w, size.cols, size.rows);
                        w.sized = true;
                    }
                    let rows = render_styled(&bytes, size.cols, size.rows, 5000);
                    if rows != w.last {
                        paint(w, &rows);
                        w.last = rows;
                    }
                }
                Err(_) => {
                    w.ended = true;
                    let mut rows = std::mem::take(&mut w.last);
                    rows.push(Vec::new());
                    rows.push(vec![Run { text: "[The session has ended.]".into(), fg: 0x9a9a9a, bg: None, bold: false, italic: true, underline: false, strike: false }]);
                    paint(w, &rows);
                    w.last = rows;
                }
            }
            true
        })
    });
}
