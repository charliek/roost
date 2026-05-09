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
    let _ = tokio::fs::remove_file(&config.socket_path).await;
    Ok(())
}

async fn prepare_socket_path(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    // Remove any stale socket from a previous run. roost-core is single-instance.
    if path.exists() {
        let _ = tokio::fs::remove_file(path).await;
    }
    Ok(())
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
