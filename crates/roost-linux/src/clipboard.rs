//! Terminal clipboard access. Wraps the CLIPBOARD and the X11/Wayland
//! PRIMARY selection. PRIMARY is a Linux-only concept; off Linux the
//! `Primary` operations are no-ops so callers stay cfg-free.

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

use crate::paste_image;

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
    let Some(clipboard) = target_clipboard(target) else {
        return;
    };
    glib::spawn_future_local(async move {
        if let Ok(Some(text)) = clipboard.read_text_future().await {
            on_text(text.to_string());
        }
    });
}

/// Read raw image bytes from the CLIPBOARD if it advertises any of
/// `paste_image::IMAGE_MIMES`. The callback fires on the GTK main loop
/// with the chosen MIME type and the bytes. Drains the stream up to
/// `paste_image::MAX_BYTES`; oversized payloads fire `on_image(None)`
/// after a warn log so the caller can fall through silently.
pub fn read_image(on_image: impl FnOnce(Option<(Vec<u8>, String)>) + 'static) {
    let Some(clipboard) = target_clipboard(Target::Clipboard) else {
        on_image(None);
        return;
    };
    let Some(mime) = first_available_image_mime(&clipboard) else {
        on_image(None);
        return;
    };
    glib::spawn_future_local(async move {
        let result = drain_image(&clipboard, &mime).await;
        match result {
            Ok(bytes) => on_image(Some((bytes, mime))),
            Err(e) => {
                tracing::warn!(error = %e, "clipboard image read");
                on_image(None);
            }
        }
    });
}

/// Read a `text/uri-list` payload if present and return the
/// image-extension subset on the GTK main loop. `on_paths` fires with
/// an empty Vec when the format isn't advertised or the list contains
/// no images.
pub fn read_file_uris(on_paths: impl FnOnce(Vec<String>) + 'static) {
    let Some(clipboard) = target_clipboard(Target::Clipboard) else {
        on_paths(Vec::new());
        return;
    };
    if !has_mime(&clipboard, "text/uri-list") {
        on_paths(Vec::new());
        return;
    }
    glib::spawn_future_local(async move {
        match drain_uri_list(&clipboard).await {
            Ok(payload) => on_paths(paste_image::file_uris_to_paths(&payload)),
            Err(e) => {
                tracing::warn!(error = %e, "clipboard uri-list read");
                on_paths(Vec::new());
            }
        }
    });
}

fn target_clipboard(target: Target) -> Option<gtk4::gdk::Clipboard> {
    let display = gtk4::gdk::Display::default()?;
    Some(match target {
        Target::Clipboard => display.clipboard(),
        #[cfg(target_os = "linux")]
        Target::Primary => display.primary_clipboard(),
        #[cfg(not(target_os = "linux"))]
        Target::Primary => return None,
    })
}

fn has_mime(clipboard: &gtk4::gdk::Clipboard, mime: &str) -> bool {
    clipboard
        .formats()
        .mime_types()
        .iter()
        .any(|m| m.as_str() == mime)
}

fn first_available_image_mime(clipboard: &gtk4::gdk::Clipboard) -> Option<String> {
    let mimes = clipboard.formats().mime_types();
    let advertised: Vec<&str> = mimes.iter().map(|m| m.as_str()).collect();
    for want in paste_image::IMAGE_MIMES {
        if advertised.iter().any(|a| a == want) {
            return Some((*want).to_string());
        }
    }
    None
}

async fn drain_image(
    clipboard: &gtk4::gdk::Clipboard,
    mime: &str,
) -> Result<Vec<u8>, anyhow::Error> {
    let (stream, _gotten) = clipboard
        .read_future(&[mime], glib::Priority::DEFAULT)
        .await?;
    drain_stream(stream, paste_image::MAX_BYTES).await
}

async fn drain_uri_list(clipboard: &gtk4::gdk::Clipboard) -> Result<String, anyhow::Error> {
    let (stream, _gotten) = clipboard
        .read_future(&["text/uri-list"], glib::Priority::DEFAULT)
        .await?;
    // The URI list is small (a handful of paths). Cap at 64 KiB to
    // bound a hostile clipboard.
    let bytes = drain_stream(stream, 64 * 1024).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

async fn drain_stream(
    stream: gio::InputStream,
    cap: usize,
) -> Result<Vec<u8>, anyhow::Error> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        let bytes = stream
            .read_bytes_future(64 * 1024, glib::Priority::DEFAULT)
            .await?;
        if bytes.is_empty() {
            break;
        }
        if out.len().saturating_add(bytes.len()) > cap {
            return Err(anyhow::anyhow!(
                "clipboard payload exceeds {} bytes",
                cap
            ));
        }
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}
