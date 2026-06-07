//! URL launcher for the Ctrl-click handler.
//!
//! Two-tier dispatch, **xdg-open first**: GIO's `AppInfo` lookup can
//! disagree with `xdg-mime`'s configured URI handler on some
//! desktops, so preferring `xdg-open` avoids opening the wrong
//! browser. `gio::AppInfo::launch_default_for_uri` is the fallback
//! for sandboxed environments where `xdg-open` isn't on `PATH`.
//!
//! The xdg-open spawn uses `gio::Subprocess` rather than
//! `std::process::Command` so the child is reaped by GIO's child-
//! watch mechanism instead of becoming a zombie.

use gtk4::gio;

/// Open `uri` in the system default handler. Tries `xdg-open` first;
/// falls back to GIO's AppInfo if `xdg-open` is unavailable. Returns
/// `Ok(())` if either path accepted the URI, `Err(...)` if both
/// refused.
pub fn open_uri(uri: &str) -> Result<(), UrlOpenError> {
    // Try xdg-open first via gio::Subprocess. GIO sets up a child
    // watch so the subprocess gets reaped on exit; we don't need to
    // wait synchronously. `wait_async(None::<&gio::Cancellable>, ...)`
    // installs the watch without blocking the main loop.
    match gio::Subprocess::newv(
        &["xdg-open".as_ref(), uri.as_ref()],
        gio::SubprocessFlags::NONE,
    ) {
        Ok(child) => {
            // Install a no-op completion handler so GIO reaps the
            // exit status. Without this the Subprocess wrapper stays
            // alive but no one actually waits on the OS process.
            child.wait_async(None::<&gio::Cancellable>, |_| {});
            return Ok(());
        }
        Err(err) => {
            tracing::debug!(
                uri,
                ?err,
                "xdg-open subprocess failed; falling back to AppInfo"
            );
        }
    }
    // Fallback: GIO's URI-scheme default handler.
    let ctx: Option<&gio::AppLaunchContext> = None;
    match gio::AppInfo::launch_default_for_uri(uri, ctx) {
        Ok(()) => Ok(()),
        Err(err) => Err(UrlOpenError::AppInfoFailed(err.to_string())),
    }
}

#[derive(Debug)]
pub enum UrlOpenError {
    AppInfoFailed(String),
}

impl std::fmt::Display for UrlOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UrlOpenError::AppInfoFailed(err) => {
                write!(f, "AppInfo::launch_default_for_uri failed: {err}")
            }
        }
    }
}

impl std::error::Error for UrlOpenError {}
