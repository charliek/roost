//! Shared helpers for the Rust side of Roost.
//!
//! Single source of truth for:
//!   * Default `roost-core` Unix-domain-socket path.
//!   * Default `roost-core` SQLite database path.
//!   * The unix-socket-aware `tonic` Channel constructor that all
//!     clients (roost-cli-rs, roost-smoke, future roost-linux) need.
//!
//! Before this crate, each binary carried its own copy of these
//! helpers. Drift between copies — especially between the daemon's
//! resolved path and a client's resolved path — would silently route
//! the client at the wrong socket. Putting all of it here removes that
//! whole class of bug.
//!
//! The Mac UI implements the same logic in Swift (it doesn't link Rust
//! crates); the algorithm is intentionally simple and identical.

#![deny(unsafe_op_in_unsafe_fn)]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;

// ============================================================================
// Path resolution
// ============================================================================

/// Resolve the default `roost-core` Unix-domain-socket path.
///
/// * **macOS:** `~/Library/Caches/roost/roost.sock`
/// * **Linux (XDG):** `$XDG_RUNTIME_DIR/roost/roost.sock`
/// * **Linux (fallback):** `/tmp/roost-<uid>/roost.sock`
///
/// Empty or non-absolute values for `XDG_RUNTIME_DIR` fall through to
/// the uid-suffixed `/tmp` fallback. Some launchd-spawned processes
/// inherit `XDG_RUNTIME_DIR=""` (set but empty); a relative path would
/// otherwise yield a silently-broken socket. Mirrors the Swift
/// `RoostApp.defaultSocketPath` validation rules — keep them in sync.
pub fn default_socket_path() -> anyhow::Result<PathBuf> {
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").context("$HOME not set")?;
        Ok(PathBuf::from(home)
            .join("Library/Caches/roost")
            .join("roost.sock"))
    } else {
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            let p = PathBuf::from(dir);
            if !p.as_os_str().is_empty() && p.is_absolute() {
                return Ok(p.join("roost").join("roost.sock"));
            }
        }
        let uid = libc_getuid();
        Ok(PathBuf::from(format!("/tmp/roost-{uid}")).join("roost.sock"))
    }
}

/// Resolve the default SQLite database path. Persisted state lives
/// alongside the OS's per-user data directory.
///
/// * **macOS:** `~/Library/Application Support/roost/roost.db`
/// * **Linux (XDG):** `$XDG_DATA_HOME/roost/roost.db`
/// * **Linux (fallback):** `$HOME/.local/share/roost/roost.db`
///
/// Same empty/non-absolute fallthrough rule as `default_socket_path`.
pub fn default_db_path() -> anyhow::Result<PathBuf> {
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").context("$HOME not set")?;
        Ok(PathBuf::from(home)
            .join("Library/Application Support/roost")
            .join("roost.db"))
    } else {
        if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
            let p = PathBuf::from(dir);
            if !p.as_os_str().is_empty() && p.is_absolute() {
                return Ok(p.join("roost").join("roost.db"));
            }
        }
        let home = std::env::var_os("HOME").context("$HOME not set")?;
        Ok(PathBuf::from(home)
            .join(".local/share/roost")
            .join("roost.db"))
    }
}

// ============================================================================
// libc::getuid wrapper
// ============================================================================
//
// Avoiding the libc dependency for one symbol. The Linux fallback path
// uses `/tmp/roost-<uid>` so two users on the same host don't collide
// on a default socket. Returns 0 on non-unix targets (which we don't
// actually ship to, but cfg-gating keeps the workspace cross-target).

#[cfg(unix)]
extern "C" {
    fn getuid() -> u32;
}

/// User-id wrapper. Safe to call.
pub fn libc_getuid() -> u32 {
    #[cfg(unix)]
    unsafe {
        getuid()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

// ============================================================================
// gRPC client channel over a Unix domain socket
// ============================================================================

/// Build a `tonic` Channel that talks to `roost-core` over the given
/// Unix-domain-socket path.
///
/// `tonic`'s `Endpoint::connect_with_connector` lets us plug in a
/// custom service that returns a Tokio `UnixStream` instead of a TCP
/// one. The URL passed to `Endpoint::from_static` is irrelevant —
/// tonic only uses it for HTTP/2 pseudo-header routing — but it must
/// be a syntactically valid http URI.
pub async fn connect_uds(path: PathBuf) -> anyhow::Result<Channel> {
    let path = Arc::new(path);
    let display_path = path.display().to_string();
    let endpoint = Endpoint::from_static("http://[::]:0");
    let channel = endpoint
        .connect_with_connector(service_fn(move |_| {
            let path = path.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(&*path).await?;
                let io = hyper_util::rt::TokioIo::new(stream);
                Ok::<_, std::io::Error>(io)
            }
        }))
        .await
        .with_context(|| format!("connect uds at {display_path}"))?;
    Ok(channel)
}

/// Convenience: connect to whatever path resolves through the
/// daemon's defaults. Useful for one-shot CLI calls that don't want
/// to plumb explicit socket-path handling.
#[allow(dead_code)] // Provided for downstream binaries; not all use it directly.
pub async fn connect_default() -> anyhow::Result<Channel> {
    connect_uds(default_socket_path()?).await
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Both paths must always be absolute and non-empty regardless of
    /// the host's environment. The exact value depends on env vars
    /// outside our control, so we don't assert on it.
    fn assert_path_well_formed(p: &Path) {
        assert!(p.is_absolute(), "{} should be absolute", p.display());
        assert!(p.to_string_lossy().contains("roost"));
    }

    #[test]
    fn socket_path_is_well_formed() {
        let p = default_socket_path().expect("socket path resolves");
        assert_path_well_formed(&p);
        assert!(
            p.to_string_lossy().ends_with(".sock"),
            "{} should end in .sock",
            p.display()
        );
    }

    #[test]
    fn db_path_is_well_formed() {
        let p = default_db_path().expect("db path resolves");
        assert_path_well_formed(&p);
        assert!(
            p.to_string_lossy().ends_with(".db"),
            "{} should end in .db",
            p.display()
        );
    }

    #[test]
    fn libc_getuid_is_callable() {
        let _ = libc_getuid();
    }
}
