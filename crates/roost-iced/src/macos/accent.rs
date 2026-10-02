//! The system accent color (plan 073 D4).
//!
//! The chrome is dark whatever the OS appearance, as the Swift app's forced
//! darkAqua window is (`App.swift:602`), so the accent is resolved under
//! darkAqua rather than under whatever appearance the system is showing.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;

use block2::RcBlock;
use iced::Color;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameDarkAqua, NSColor, NSColorSpace,
    NSSystemColorsDidChangeNotification,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSOperationQueue};

use crate::engine_feed::{EngineFeed, EngineFeedSender};

thread_local! {
    /// The notification observer's token. Never removed: the chrome follows
    /// the accent for the life of the process.
    static OBSERVER: RefCell<Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>> =
        const { RefCell::new(None) };
}

/// `NSColor.controlAccentColor` as the dark chrome shows it, in sRGB.
///
/// `None` only if AppKit cannot name darkAqua or the accent has no sRGB
/// form; the chrome then keeps whatever accent it already has.
pub(crate) fn current(_mtm: MainThreadMarker) -> Option<Color> {
    // SAFETY: an immutable AppKit string constant.
    let dark = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua })?;
    let resolved = Cell::new(None);
    // `controlAccentColor` is dynamic: it resolves against the current
    // drawing appearance at the moment it is converted, so the conversion
    // has to happen inside this block.
    let resolve = RcBlock::new(|| {
        let srgb =
            NSColor::controlAccentColor().colorUsingColorSpace(&NSColorSpace::sRGBColorSpace());
        resolved.set(srgb.map(|color| {
            Color::from_rgb(
                color.redComponent() as f32,
                color.greenComponent() as f32,
                color.blueComponent() as f32,
            )
        }));
    });
    dark.performAsCurrentDrawingAppearance(&resolve);
    resolved.get()
}

/// Send [`EngineFeed::AccentChanged`] whenever the system colors change.
///
/// Idempotent, because `window_opened` runs again on every focus change.
/// The observer runs on the main queue, so its read of the accent is on
/// the main thread like every other AppKit call here; only the plain color
/// crosses the feed.
pub(crate) fn observe(_mtm: MainThreadMarker, feed: EngineFeedSender) {
    if OBSERVER.with(|cell| cell.borrow().is_some()) {
        return;
    }
    let changed = RcBlock::new(move |_: NonNull<NSNotification>| {
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::error!("accent change delivered off the main queue; ignoring");
            return;
        };
        if let Some(accent) = current(mtm) {
            feed.send(EngineFeed::AccentChanged(accent));
        }
    });
    // SAFETY: the name is an immutable AppKit constant, there is no sender
    // object to match, the queue is the main queue, and the block owns
    // everything it captures (the feed sender is `Send + Sync`).
    let token = unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(NSSystemColorsDidChangeNotification),
            None,
            Some(&NSOperationQueue::mainQueue()),
            &changed,
        )
    };
    OBSERVER.with(|cell| *cell.borrow_mut() = Some(token));
    tracing::info!("following the system accent color");
}
