//! Secure Keyboard Entry's native owner (plan 074 §D3): the Carbon calls,
//! the main thread's [`SecureInput`], and the app-activation observers.
//!
//! [`apply`] reads `NSApp.isActive` when it runs, never a value fed to it,
//! and the observers call it directly. A resign that lands while AppKit
//! runs a native popup's tracking loop — with iced's `update` suspended —
//! therefore gives the enable back at once rather than when the loop ends.
//!
//! Nothing here runs at a crash: macOS drops a dead process's secure input
//! itself (plan 074 §2.12).

use std::cell::RefCell;
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSApplication, NSApplicationDidBecomeActiveNotification,
    NSApplicationDidResignActiveNotification,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSOperationQueue};

use crate::engine_feed::{EngineFeed, EngineFeedSender};
use crate::secure_input::{Inputs, OsStatus, SecureEventInput, SecureInput, Status};

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn EnableSecureEventInput() -> OsStatus;
    fn DisableSecureEventInput() -> OsStatus;
    /// Carbon's `Boolean`.
    fn IsSecureEventInputEnabled() -> u8;
}

struct Carbon;

impl SecureEventInput for Carbon {
    fn enable(&mut self) -> OsStatus {
        // SAFETY: no arguments; HIToolbox documents it callable from the
        // main thread, which every caller here is (they hold a marker).
        unsafe { EnableSecureEventInput() }
    }

    fn disable(&mut self) -> OsStatus {
        // SAFETY: as `enable`.
        unsafe { DisableSecureEventInput() }
    }

    fn system_enabled(&self) -> bool {
        // SAFETY: a read with no arguments.
        unsafe { IsSecureEventInputEnabled() != 0 }
    }
}

thread_local! {
    static OWNER: RefCell<SecureInput<Carbon>> = RefCell::new(SecureInput::new(Carbon));
    /// The two observer tokens. Never removed: after the App is gone the
    /// owner is latched off, so a late notification changes nothing.
    static OBSERVERS: RefCell<Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>> =
        const { RefCell::new(Vec::new()) };
}

/// Run `body` against the owner. A re-entrant call — a Carbon call that
/// somehow posted an activation notification synchronously — is refused
/// loudly rather than panicking, because the observers run inside ObjC
/// frames.
fn with_owner(what: &str, body: impl FnOnce(&mut SecureInput<Carbon>)) {
    OWNER.with(|cell| match cell.try_borrow_mut() {
        Ok(mut owner) => body(&mut owner),
        Err(_) => tracing::error!(what, "secure input re-entered its own owner; skipping"),
    });
}

pub(crate) fn set_inputs(_mtm: MainThreadMarker, inputs: Inputs) {
    with_owner("set_inputs", |owner| owner.set_inputs(inputs));
}

pub(crate) fn apply(mtm: MainThreadMarker) {
    let active = NSApplication::sharedApplication(mtm).isActive();
    with_owner("apply", |owner| owner.apply(active));
}

/// Latch off and give back the enable: the quit path and `App`'s `Drop`.
pub(crate) fn release(_mtm: MainThreadMarker) {
    with_owner("release", SecureInput::release);
}

pub(crate) fn status(_mtm: MainThreadMarker) -> Status {
    OWNER.with(|cell| {
        cell.try_borrow()
            .map(|owner| owner.status())
            .unwrap_or_default()
    })
}

/// Follow the app becoming and resigning active: [`apply`] first, then
/// [`EngineFeed::AppActive`] so the window redraws the lock.
///
/// Idempotent, because `window_opened` runs again on every focus change.
pub(crate) fn observe(_mtm: MainThreadMarker, feed: &EngineFeedSender) {
    if OBSERVERS.with(|cell| !cell.borrow().is_empty()) {
        return;
    }
    let center = NSNotificationCenter::defaultCenter();
    // SAFETY: immutable AppKit string constants.
    let names = unsafe {
        [
            (NSApplicationDidBecomeActiveNotification, true),
            (NSApplicationDidResignActiveNotification, false),
        ]
    };
    for (name, active) in names {
        let feed = feed.clone();
        let changed = RcBlock::new(move |_: NonNull<NSNotification>| {
            let Some(mtm) = MainThreadMarker::new() else {
                tracing::error!("app activation delivered off the main queue; ignoring");
                return;
            };
            apply(mtm);
            feed.send(EngineFeed::AppActive(active));
        });
        // SAFETY: the name is an AppKit constant, there is no sender object
        // to match, the queue is the main queue, and the block owns
        // everything it captures (the feed sender is `Send + Sync`).
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(name),
                None,
                Some(&NSOperationQueue::mainQueue()),
                &changed,
            )
        };
        OBSERVERS.with(|cell| cell.borrow_mut().push(token));
    }
    tracing::info!("following app activation for secure keyboard entry");
}
