//! Small AppKit builders shared by the popover and its rows.

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBox, NSBoxType, NSButton, NSCellImagePosition, NSColor, NSControlSize, NSFont, NSFontWeight, NSImage, NSImageScaling,
    NSImageSymbolConfiguration, NSLineBreakMode, NSTextField,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

pub type Mask = NSAutoresizingMaskOptions;

pub fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

pub fn font(size: f64, weight: NSFontWeight) -> Retained<NSFont> {
    NSFont::systemFontOfSize_weight(size, weight)
}

pub fn regular() -> NSFontWeight {
    // SAFETY: an AppKit constant.
    unsafe { objc2_app_kit::NSFontWeightRegular }
}

pub fn medium() -> NSFontWeight {
    // SAFETY: an AppKit constant.
    unsafe { objc2_app_kit::NSFontWeightMedium }
}

pub fn semibold() -> NSFontWeight {
    // SAFETY: an AppKit constant.
    unsafe { objc2_app_kit::NSFontWeightSemibold }
}

/// A one-line, non-editable label that truncates at the tail.
pub fn label(text: &str, font: &NSFont, color: &NSColor, frame: NSRect, mask: Mask, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setFont(Some(font));
    l.setTextColor(Some(color));
    l.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    l.setUsesSingleLineMode(true);
    l.setMaximumNumberOfLines(1);
    l.setFrame(frame);
    l.setAutoresizingMask(mask);
    l
}

/// An SF Symbol at a point size.
pub fn symbol(name: &str, size: f64, weight: NSFontWeight) -> Option<Retained<NSImage>> {
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)?;
    image.imageWithSymbolConfiguration(&NSImageSymbolConfiguration::configurationWithPointSize_weight(size, weight))
}

/// A borderless SF Symbol button, subtly tinted.
/// `target` may come later (`setTarget`).
pub fn icon_button(name: &str, tip: &str, target: Option<&AnyObject>, action: Sel, frame: NSRect, mtm: MainThreadMarker) -> Retained<NSButton> {
    let image = symbol(name, 13.0, regular()).unwrap_or_default();
    // SAFETY: the target outlives the button (the popover controller lives for the process).
    let b = unsafe { NSButton::buttonWithImage_target_action(&image, target, Some(action), mtm) };
    b.setBordered(false);
    b.setImagePosition(NSCellImagePosition::ImageOnly);
    b.setImageScaling(NSImageScaling::ScaleProportionallyDown);
    b.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
    b.setToolTip(Some(&NSString::from_str(tip)));
    b.setFrame(frame);
    b
}

/// A borderless text button with a leading symbol (the footer's); its
/// target is set by the caller.
pub fn text_button(title: &str, symbol_name: &str, tint: &NSColor, action: Sel, frame: NSRect, mtm: MainThreadMarker) -> Retained<NSButton> {
    // SAFETY: no target yet.
    let b = unsafe { NSButton::buttonWithTitle_target_action(&NSString::from_str(title), None, Some(action), mtm) };
    b.setBordered(false);
    b.setControlSize(NSControlSize::Small);
    b.setFont(Some(&font(12.0, regular())));
    if let Some(image) = symbol(symbol_name, 12.0, regular()) {
        b.setImage(Some(&image));
        b.setImagePosition(NSCellImagePosition::ImageLeading);
    }
    b.setContentTintColor(Some(tint));
    b.setFrame(frame);
    b
}

/// A hairline across `frame`.
pub fn separator(frame: NSRect, mask: Mask, mtm: MainThreadMarker) -> Retained<NSBox> {
    let b = NSBox::new(mtm);
    b.setBoxType(NSBoxType::Separator);
    b.setFrame(frame);
    b.setAutoresizingMask(mask);
    b
}
