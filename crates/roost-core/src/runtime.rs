//! Daemon runtime: socket setup, graceful shutdown, service wiring.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::info;

use roost_proto::v1::roost_server::RoostServer;

use crate::service::RoostService;
use crate::state::Workspace;

// Re-export for backward compat with main.rs / tests / external callers
// that already import these via `roost_core::runtime::default_*`.
// The actual implementation lives in roost-common so the daemon and
// every client agree byte-for-byte on path resolution.
pub use roost_common::{default_db_path, default_socket_path};

/// Configuration for a daemon run.
pub struct Config {
    pub socket_path: PathBuf,
    /// Path to the SQLite database file. `None` uses an ephemeral in-memory
    /// database — useful for smoke tests but loses state on shutdown.
    pub db_path: Option<PathBuf>,
}

/// Bind the socket and run the gRPC server until ctrl-c.
pub async fn run(config: Config) -> anyhow::Result<()> {
    prepare_socket_path(&config.socket_path).await?;

    let listener = UnixListener::bind(&config.socket_path)
        .with_context(|| format!("failed to bind {}", config.socket_path.display()))?;
    set_socket_perms(&config.socket_path)?;

    let workspace = match config.db_path.as_ref() {
        Some(path) => {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            info!(path = %path.display(), "opening sqlite store");
            Arc::new(
                Workspace::open(path)
                    .with_context(|| format!("open workspace at {}", path.display()))?,
            )
        }
        None => {
            info!("using in-memory sqlite store (state will be lost on shutdown)");
            Arc::new(Workspace::new())
        }
    };

    info!(path = %config.socket_path.display(), "roost-core listening");

    let service = RoostService::new(workspace, config.socket_path.clone());

    let stream = UnixListenerStream::new(listener);
    Server::builder()
        .add_service(RoostServer::new(service))
        .serve_with_incoming_shutdown(stream, shutdown_signal())
        .await
        .context("gRPC server crashed")?;

    info!("roost-core shutting down");
    // Use the same guarded helper so a misconfigured `--socket` pointed
    // at a regular file doesn't get silently destroyed on shutdown.
    let _ = remove_socket_if_present(&config.socket_path).await;
    Ok(())
}

async fn prepare_socket_path(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    // roost-core is single-instance, so a leftover socket from a prior
    // run is expected and we want to remove it. But we ONLY remove it if
    // it's actually a socket — refuse to silently delete a regular file
    // or directory the user accidentally pointed `--socket` at.
    remove_socket_if_present(path).await?;
    Ok(())
}

#[cfg(unix)]
fn is_socket(file_type: std::fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;
    file_type.is_socket()
}

#[cfg(not(unix))]
fn is_socket(_file_type: std::fs::FileType) -> bool {
    false
}

async fn remove_socket_if_present(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => {
            if is_socket(meta.file_type()) {
                tokio::fs::remove_file(path)
                    .await
                    .with_context(|| format!("failed to remove stale socket {}", path.display()))?;
                Ok(())
            } else {
                anyhow::bail!(
                    "refusing to remove non-socket path {} (file type: {:?}). \
                     If this was intentional, remove it manually first.",
                    path.display(),
                    meta.file_type()
                );
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("stat {}", path.display())),
    }
}

#[cfg(unix)]
fn set_socket_perms(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("chmod 0600 {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_socket_perms(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let term = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        } else {
            futures::future::pending::<()>().await;
        }
    };

    #[cfg(not(unix))]
    let term = futures::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("ctrl-c received"),
        _ = term => info!("SIGTERM received"),
    }
}
