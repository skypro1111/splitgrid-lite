//! The menu-bar icon: the app's own icon (AppIcon.icns in the helper bundle,
//! "SplitGrid Sessions.app"), else the SF Symbol "terminal", plus the session
//! count. Its button toggles the popover
//! (target/action set by [`crate::popover::Popover::new`]); no NSMenu.

use objc2::rc::Retained;
use objc2::{AnyThread as _, MainThreadMarker};
use objc2_app_kit::{NSCellImagePosition, NSImage, NSStatusBar, NSStatusItem};
use objc2_foundation::{NSBundle, NSSize, NSString, ns_string};

/// The app icon shipped in the helper bundle, at menu-bar size.
fn app_icon() -> Option<Retained<NSImage>> {
    let path = NSBundle::mainBundle().pathForResource_ofType(Some(ns_string!("AppIcon")), Some(ns_string!("icns")))?;
    let image = NSImage::initWithContentsOfFile(NSImage::alloc(), &path)?;
    image.setSize(NSSize::new(18., 18.));
    Some(image)
}

fn symbol() -> Option<Retained<NSImage>> {
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(ns_string!("terminal"), Some(ns_string!("SplitGrid terminals")))?;
    image.setTemplate(true);
    Some(image)
}

pub fn create(mtm: MainThreadMarker) -> Retained<NSStatusItem> {
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(-1.0);
    if let Some(button) = item.button(mtm) {
        if let Some(image) = app_icon().or_else(symbol) {
            button.setImage(Some(&image));
            button.setImagePosition(NSCellImagePosition::ImageLeading);
        } else {
            button.setTitle(ns_string!("SG"));
        }
        button.setToolTip(Some(ns_string!("SplitGrid Lite: running terminals")));
    }
    item
}

/// " 3" next to the glyph; nothing at zero.
pub fn set_count(item: &NSStatusItem, count: usize, mtm: MainThreadMarker) {
    if let Some(button) = item.button(mtm) {
        let title = if count > 0 { format!(" {count}") } else { String::new() };
        button.setTitle(&NSString::from_str(&title));
    }
}
