//! Shared helpers for the Rust side of Roost.
//!
//! As of the daemon-removal refactor M2 this crate is a thin shim:
//!
//! * **Path resolution** (BundleProfile + the legacy
//!   `default_socket_path` / `default_db_path` helpers) now lives in
//!   `roost-ipc::paths`. This crate re-exports the types so existing
//!   callers (`roost-core`, `roost-cli-rs`, `roost-smoke`,
//!   `roost-linux`) keep compiling.
//! * **gRPC connect-over-UDS** stays here for now — the daemon and
//!   gRPC clients are the only remaining consumers, and the whole
//!   crate is deleted in M7 when the daemon goes away.
//!
//! New code should depend on `roost-ipc` directly. This crate exists
//! to keep M1–M6 buildable without churning the gRPC consumers.

#![deny(unsafe_op_in_unsafe_fn)]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;

// ============================================================================
// Re-exports from roost-ipc
// ============================================================================

pub use roost_ipc::paths::{BundleProfile, BundleProfileKind};

/// Legacy compatibility shim. Resolves to the Mac bundle profile's
/// socket path (the daemon's default profile).
pub fn default_socket_path() -> anyhow::Result<PathBuf> {
    Ok(BundleProfile::mac()?.socket_path)
}

/// Legacy compatibility shim. Resolves to the Mac bundle profile's
/// SQLite database path. Daemon-only; goes away in M7.
pub fn default_db_path() -> anyhow::Result<PathBuf> {
    Ok(BundleProfile::mac()?.db_path())
}

// Legacy compatibility shim — same as `roost_ipc::paths::libc_getuid`
// would be if it existed publicly. Kept here for the small number of
// callers that imported it directly.
#[cfg(unix)]
extern "C" {
    fn getuid() -> u32;
}

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
/// daemon's defaults.
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

    fn assert_path_well_formed(p: &Path) {
        assert!(p.is_absolute(), "{} should be absolute", p.display());
        let lossy = p.to_string_lossy();
        assert!(
            lossy.contains("roost") || lossy.contains("Roost"),
            "{} should reference roost",
            p.display()
        );
    }

    #[test]
    fn socket_path_is_well_formed() {
        let p = default_socket_path().expect("socket path resolves");
        assert_path_well_formed(&p);
        assert!(p.to_string_lossy().ends_with(".sock"));
    }

    #[test]
    fn db_path_is_well_formed() {
        let p = default_db_path().expect("db path resolves");
        assert_path_well_formed(&p);
        assert!(p.to_string_lossy().ends_with(".db"));
    }

    #[test]
    fn libc_getuid_is_callable() {
        let _ = libc_getuid();
    }

    #[test]
    fn bundle_profile_reexport_works() {
        let p = BundleProfile::mac().expect("mac profile");
        assert_eq!(p.app_id, "ai.stridelabs.Roost");
    }
}
