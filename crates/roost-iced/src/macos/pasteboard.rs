//! The macOS selection pasteboard (plan 073 D1), and the file-list write
//! the paste test seam needs (D2).
//!
//! macOS has no PRIMARY, and `window_clipboard`'s macOS backend leaves the
//! primary-selection calls unimplemented, so `copy-on-select` wrote nowhere
//! and middle-click pasted nothing. The Swift app answers both with a
//! private named pasteboard (`TerminalView.swift`'s `selectionPasteboard`),
//! and this is that pasteboard under the same name. Named pasteboards are
//! per user session, so while both apps ship, a selection made in one
//! middle-click-pastes in the other — the sharing PRIMARY gives on Linux.
//!
//! **The exception to `macos/mod.rs`'s main-thread rule.** Every call here
//! is a cross-process round trip to the pasteboard server that can block,
//! so callers run it on the blocking pool, as `paste_image.rs` does its
//! reads — never on the UI thread. `NSPasteboard` is not AppKit UI: its
//! bindings take no `MainThreadMarker`, and arboard drives the same class
//! from the same pool. The second rule holds: plain data in, plain data
//! out.

use std::path::PathBuf;

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSPasteboard, NSPasteboardItem, NSPasteboardTypeFileURL, NSPasteboardTypeString,
    NSPasteboardWriting,
};
use objc2_foundation::{ns_string, NSArray, NSString, NSURL};

fn selection() -> Retained<NSPasteboard> {
    NSPasteboard::pasteboardWithName(ns_string!("ai.stridelabs.Roost.selection"))
}

/// The selection pasteboard's text, or `None` when it holds none.
pub(crate) fn selection_read() -> Option<String> {
    autoreleasepool(|_| {
        // SAFETY: an immutable AppKit constant, read and never stored.
        let string = unsafe { NSPasteboardTypeString };
        selection()
            .stringForType(string)
            .map(|text| text.to_string())
    })
}

pub(crate) fn selection_write(text: &str) -> Result<(), String> {
    autoreleasepool(|_| {
        let pasteboard = selection();
        pasteboard.clearContents();
        // SAFETY: as in `selection_read`.
        let string = unsafe { NSPasteboardTypeString };
        if pasteboard.setString_forType(&NSString::from_str(text), string) {
            Ok(())
        } else {
            Err("selection pasteboard: the write was refused".to_string())
        }
    })
}

/// Put `paths` on the general pasteboard the way a Finder copy leaves
/// them: one item per file, carrying its file URL and, as the string
/// flavor, only its name. The name is the point — it is what a text-first
/// paste would read, so `clipboard.write_files` reproduces exactly the
/// clipboard D2's file step exists for.
///
/// Paths are written as given; the caller owns making them canonical.
pub(crate) fn write_file_list(paths: &[PathBuf]) -> Result<(), String> {
    autoreleasepool(|_| {
        // SAFETY: immutable AppKit constants, read and never stored.
        let (file_url, string) = unsafe { (NSPasteboardTypeFileURL, NSPasteboardTypeString) };
        let mut items = Vec::with_capacity(paths.len());
        for path in paths {
            let text = path
                .to_str()
                .ok_or_else(|| format!("clipboard files: not UTF-8: {}", path.display()))?;
            let url = NSURL::fileURLWithPath(&NSString::from_str(text))
                .absoluteString()
                .ok_or_else(|| format!("clipboard files: no file URL for {text}"))?;
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            let item = NSPasteboardItem::new();
            if !item.setString_forType(&url, file_url)
                || !item.setString_forType(&NSString::from_str(&name), string)
            {
                return Err(format!("clipboard files: the item for {text} was refused"));
            }
            items.push(ProtocolObject::<dyn NSPasteboardWriting>::from_retained(
                item,
            ));
        }
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        if pasteboard.writeObjects(&NSArray::from_retained_slice(&items)) {
            Ok(())
        } else {
            Err("clipboard files: the pasteboard refused the write".to_string())
        }
    })
}
