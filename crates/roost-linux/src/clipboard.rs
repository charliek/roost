//! Terminal clipboard access. Wraps the CLIPBOARD and the X11/Wayland
//! PRIMARY selection. PRIMARY is a Linux-only concept; off Linux the
//! `Primary` operations are no-ops so callers stay cfg-free.

use gtk4::glib;
use gtk4::prelude::*;

#[derive(Clone, Copy)]
pub enum Target {
    Clipboard,
    /// X11/Wayland PRIMARY selection. No-op off Linux.
    Primary,
}

/// Write `text` to the given selection. No-op if there is no display, or
/// for `Primary` off Linux.
pub fn write(target: Target, text: &str) {
    let Some(display) = gtk4::gdk::Display::default() else {
        return;
    };
    match target {
        Target::Clipboard => display.clipboard().set_text(text),
        #[cfg(target_os = "linux")]
        Target::Primary => display.primary_clipboard().set_text(text),
        #[cfg(not(target_os = "linux"))]
        Target::Primary => {}
    }
}

/// Read text from the given selection and hand it to `on_text` on the GTK
/// main loop. No-op if there is no display, or for `Primary` off Linux.
pub fn read(target: Target, on_text: impl FnOnce(String) + 'static) {
    let Some(display) = gtk4::gdk::Display::default() else {
        return;
    };
    let clipboard = match target {
        Target::Clipboard => display.clipboard(),
        #[cfg(target_os = "linux")]
        Target::Primary => display.primary_clipboard(),
        #[cfg(not(target_os = "linux"))]
        Target::Primary => return,
    };
    glib::spawn_future_local(async move {
        if let Ok(Some(text)) = clipboard.read_text_future().await {
            on_text(text.to_string());
        }
    });
}
