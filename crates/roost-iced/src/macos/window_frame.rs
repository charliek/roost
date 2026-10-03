//! The window frame's screen check (plan 074 §D5b): once the window
//! exists, make sure the frame it opened at — the one `state.json`
//! remembered — still lies on a screen.
//!
//! It runs through `iced::window::run`, which hands the window over on the
//! main thread after iced's event loop, and with it winit's own
//! `NSApplication`, exists; reading `NSScreen` any earlier would get ahead
//! of that. The geometry is [`fit_on_screens`], plain data in and out.

use iced::window::raw_window_handle::RawWindowHandle;
use iced::window::Window;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSScreen, NSView, NSWindow};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use crate::app::window_frame::{fit_on_screens, iced_frame, CheckedFrame, ScreenRect};

/// Report the window's frame as it stands — after moving it onto a screen,
/// when `fit` and it is on none or only partly on one. `None` when there
/// is no `NSWindow` or no screen to measure against.
pub(crate) fn check(window: &dyn Window, fit: bool) -> Option<CheckedFrame> {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::error!("window frame check ran off the main thread; skipping");
        return None;
    };
    let ns_window = ns_window(window)?;
    let screens = NSScreen::screens(mtm).to_vec();
    // `screens[0]` holds the menu bar and sits at AppKit's origin: the
    // screen winit flips every window position against.
    let Some(primary) = screens.first() else {
        tracing::warn!("no screens to check the window frame against");
        return None;
    };
    let primary_height = primary.frame().size.height;
    let fitted = if fit {
        let visible: Vec<ScreenRect> = screens
            .iter()
            .map(|screen| screen_rect(screen.visibleFrame()))
            .collect();
        let main = NSScreen::mainScreen(mtm)
            .map_or(visible[0], |screen| screen_rect(screen.visibleFrame()));
        let opened = screen_rect(ns_window.frame());
        let fitted = fit_on_screens(opened, &visible, main);
        if let Some(target) = fitted {
            tracing::info!(
                ?opened,
                ?target,
                "the remembered window frame is off its screens; moving it onto one"
            );
            ns_window.setFrame_display(ns_rect(target), true);
        }
        fitted
    } else {
        None
    };
    let frame = ns_window.frame();
    let content = ns_window.contentRectForFrameRect(frame);
    Some(CheckedFrame {
        frame: iced_frame(
            screen_rect(frame),
            content.size.width,
            content.size.height,
            primary_height,
        ),
        adjusted: fitted.is_some(),
    })
}

/// The `NSWindow` behind iced's window: raw-window-handle 0.6 hands over
/// its content `NSView`, and the view knows its window.
fn ns_window(window: &dyn Window) -> Option<Retained<NSWindow>> {
    let handle = match window.window_handle() {
        Ok(handle) => handle,
        Err(error) => {
            tracing::warn!(%error, "no window handle for the frame check");
            return None;
        }
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        tracing::warn!("the window handle is not AppKit's; skipping the frame check");
        return None;
    };
    // SAFETY: an AppKit handle's `ns_view` is the window's live `NSView`
    // for as long as `handle` borrows the window, and this runs on the
    // main thread (the marker `check` took).
    let view: &NSView = unsafe { appkit.ns_view.cast::<NSView>().as_ref() };
    let ns_window = view.window();
    if ns_window.is_none() {
        tracing::warn!("the window's view is in no NSWindow; skipping the frame check");
    }
    ns_window
}

fn screen_rect(rect: NSRect) -> ScreenRect {
    ScreenRect {
        x: rect.origin.x,
        y: rect.origin.y,
        width: rect.size.width,
        height: rect.size.height,
    }
}

fn ns_rect(rect: ScreenRect) -> NSRect {
    NSRect::new(
        NSPoint::new(rect.x, rect.y),
        NSSize::new(rect.width, rect.height),
    )
}
