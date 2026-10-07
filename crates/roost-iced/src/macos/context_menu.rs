//! A row's right-click menu as the native popup (plan 073 D9).
//!
//! The popup's tracking loop runs inside [`pop_up`], blocking the main
//! thread until the menu closes; winit queues what arrives meanwhile and
//! delivers it afterwards. An item never acts from inside that loop: its
//! action puts the row and the item on the engine feed, and the drain
//! runs it through `App::context_activate` once the menu is gone, against
//! the rows as they are then.
//!
//! The items answer to their own target object and their own table, not
//! the menu bar's: [`super::menu`]'s table is read through a `try_borrow`
//! that drops an action fired while it is borrowed, and nothing here may
//! depend on what the menu bar is doing.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSColor, NSEvent, NSFont, NSLineBreakMode, NSMenu, NSMenuItem, NSTextField, NSView,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use roost_ui_model::context_menu::{ContextEntry, ContextTarget};

use crate::app::context_menu::{native_rows, NativeRow, NativeTable};
use crate::engine_feed::{EngineFeed, EngineFeedSender};

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - The class does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RoostContextMenuTarget"]
    // The generation of the popup whose items target it.
    #[ivars = u64]
    struct ContextMenuTarget;

    impl ContextMenuTarget {
        #[unsafe(method(roostContextAction:))]
        fn context_action(&self, sender: &NSMenuItem) {
            chosen(*self.ivars(), sender.tag());
        }
    }
);

impl ContextMenuTarget {
    fn new(mtm: MainThreadMarker, generation: u64) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(generation);
        // SAFETY: `NSObject`'s `init`, on an allocation whose ivars are set.
        unsafe { msg_send![super(this), init] }
    }
}

/// The last popup shown.
///
/// It outlives its own [`pop_up`] call, until the next popup replaces it:
/// whether AppKit sends the chosen item's action before
/// `popUpMenuPositioningItem…` returns or after, the item, its target —
/// a **weak** property of the item, so this is what keeps it alive — and
/// its table are still there to receive it.
struct Popup {
    _receiver: Retained<ContextMenuTarget>,
    _menu: Retained<NSMenu>,
    table: NativeTable,
    feed: EngineFeedSender,
}

struct Popups {
    last: Option<Popup>,
    /// A [`pop_up`] call is tracking a menu.
    tracking: bool,
    generation: u64,
}

thread_local! {
    /// Main-thread-only, and never handed out: `Retained<_>` does not
    /// cross this module's boundary (`super`'s rules).
    static POPUPS: RefCell<Popups> = const {
        RefCell::new(Popups {
            last: None,
            tracking: false,
            generation: 0,
        })
    };
}

/// Put the item tagged `tag` of popup `generation` on the feed, if that
/// popup is still the last one shown.
///
/// Runs under AppKit's frames, so the slot is read through `try_borrow`:
/// a panic here would unwind through them.
fn chosen(generation: u64, tag: isize) {
    POPUPS.with(|cell| {
        let Ok(popups) = cell.try_borrow() else {
            tracing::error!(tag, "context menu item fired while its table was borrowed");
            return;
        };
        let Some(popup) = popups.last.as_ref() else {
            tracing::warn!(tag, "context menu item fired with no menu shown");
            return;
        };
        match popup.table.chosen(generation, tag) {
            // `false` means the drain side is gone: the app is already
            // tearing down and there is nobody left to act on the click.
            Ok((target, action)) => {
                popup.feed.send(EngineFeed::Context(target, action));
            }
            Err(refusal) => tracing::warn!(generation, tag, "{refusal}"),
        }
    });
}

/// Show `entries` as `target`'s menu at the pointer, and return once it
/// has closed.
pub(crate) fn pop_up(
    mtm: MainThreadMarker,
    target: ContextTarget,
    entries: &[ContextEntry],
    feed: EngineFeedSender,
) {
    let generation = POPUPS.with(|cell| {
        let mut popups = cell.try_borrow_mut().ok()?;
        if popups.tracking {
            return None;
        }
        popups.tracking = true;
        popups.generation = popups.generation.wrapping_add(1);
        Some(popups.generation)
    });
    let Some(generation) = generation else {
        tracing::warn!("a context menu was asked for while another was open");
        return;
    };

    let receiver = ContextMenuTarget::new(mtm, generation);
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(""));
    // The model decides what is enabled, not AppKit's responder chain.
    menu.setAutoenablesItems(false);
    let (rows, actions) = native_rows(entries);
    for row in rows {
        match row {
            NativeRow::Item {
                title,
                enabled,
                tag,
            } => {
                let item = NSMenuItem::new(mtm);
                item.setTitle(&NSString::from_str(title));
                item.setTag(tag);
                item.setEnabled(enabled);
                // SAFETY: `roostContextAction:` is declared on
                // `ContextMenuTarget` above with the `(id sender)`
                // signature AppKit calls actions with; the target is a
                // weak reference kept alive by `Popup::_receiver`.
                unsafe {
                    item.setAction(Some(sel!(roostContextAction:)));
                    item.setTarget(Some(&receiver));
                }
                menu.addItem(&item);
            }
            NativeRow::Separator => menu.addItem(&NSMenuItem::separatorItem(mtm)),
            NativeRow::Header { title } => menu.addItem(&section_header(title, mtm)),
        }
    }

    let popup = Popup {
        _receiver: receiver,
        _menu: menu.clone(),
        table: NativeTable {
            generation,
            target,
            actions,
        },
        feed,
    };
    // Released once the borrow is gone, so nothing AppKit does while
    // tearing the old popup down can find the slot borrowed.
    let replaced = POPUPS.with(|cell| cell.borrow_mut().last.replace(popup));
    drop(replaced);

    // No borrow of `POPUPS` is held across the tracking loop: [`chosen`]
    // borrows it from inside.
    let picked =
        menu.popUpMenuPositioningItem_atLocation_inView(None, NSEvent::mouseLocation(), None);
    POPUPS.with(|cell| cell.borrow_mut().tracking = false);
    tracing::debug!(picked, generation, "context menu closed");
}

/// A section header drawn by a label in `secondaryLabelColor`. AppKit
/// paints a section header, and a disabled item, in its own faint gray
/// whatever color the title asks for, too faint for the session line this
/// row carries; a view is the one thing it draws as given. Disabled, so
/// it is never chosen and the arrow keys pass it by.
fn section_header(title: &str, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let label = NSTextField::labelWithString(&NSString::from_str(title), mtm);
    label.setTextColor(Some(&NSColor::secondaryLabelColor()));
    label.setFont(Some(&NSFont::boldSystemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    label.sizeToFit();
    let mut size = label.frame().size;
    size.width = size.width.min(HEADER_MAX_WIDTH - 2.0 * HEADER_INSET_X);
    label.setFrame(NSRect::new(
        NSPoint::new(HEADER_INSET_X, HEADER_INSET_Y),
        size,
    ));
    let view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(
                size.width + 2.0 * HEADER_INSET_X,
                size.height + 2.0 * HEADER_INSET_Y,
            ),
        ),
    );
    view.addSubview(&label);
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    item.setEnabled(false);
    item.setView(Some(&view));
    item
}

/// Where AppKit starts an item's title, so the header lines up with the
/// rows under it.
const HEADER_INSET_X: f64 = 14.0;
const HEADER_INSET_Y: f64 = 3.0;
/// The overlay's widest menu (`MENU_MAX_WIDTH`), so a long host label is
/// elided here as it is there rather than widening the whole menu.
const HEADER_MAX_WIDTH: f64 = 360.0;
