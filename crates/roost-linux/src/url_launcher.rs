//! URL launcher for the Ctrl-click handler.
//!
//! Two-tier dispatch:
//!
//! 1. `gio::AppInfo::launch_default_for_uri` — uses the user's GIO
//!    URI-scheme default (set by `xdg-mime`, GNOME Settings, etc.).
//!    This is the same path GTK's URILauncher uses under the hood.
//! 2. Fallback: `xdg-open` subprocess. Matches the legacy Go binary's
//!    `internal/openuri/openuri_linux.go` heuristic — some sandboxed
//!    environments expose `xdg-open` even when `AppInfo` lookup fails
//!    (no `mimeapps.list`, missing `desktop-file-utils`).

use gtk4::gio;

/// Open `uri` in the system default handler. Tries GIO's AppInfo
/// first; falls back to `xdg-open` if AppInfo lookup or launch fails.
/// Returns `Ok(())` if either path accepted the URI, `Err(...)` if
/// both refused.
pub fn open_uri(uri: &str) -> Result<(), UrlOpenError> {
    let ctx: Option<&gio::AppLaunchContext> = None;
    match gio::AppInfo::launch_default_for_uri(uri, ctx) {
        Ok(()) => return Ok(()),
        Err(err) => {
            tracing::debug!(
                uri,
                ?err,
                "AppInfo::launch_default_for_uri failed; falling back to xdg-open"
            );
        }
    }
    // Fallback: xdg-open. Spawn detached so the URL handler outlives
    // this process and doesn't appear in `ps` as a roost child.
    match std::process::Command::new("xdg-open").arg(uri).spawn() {
        Ok(child) => {
            // Don't wait — let xdg-open hand off to the browser
            // process and exit on its own. Reaping is best-effort
            // through gtk4's normal event loop.
            let _ = child.id();
            Ok(())
        }
        Err(err) => Err(UrlOpenError::XdgOpenFailed(err)),
    }
}

#[derive(Debug)]
pub enum UrlOpenError {
    XdgOpenFailed(std::io::Error),
}

impl std::fmt::Display for UrlOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UrlOpenError::XdgOpenFailed(err) => write!(f, "xdg-open failed: {err}"),
        }
    }
}

impl std::error::Error for UrlOpenError {}
