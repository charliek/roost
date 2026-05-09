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

/// Configuration for a daemon run.
pub struct Config {
    pub socket_path: PathBuf,
}

/// Resolve the default Unix domain socket path.
///
/// Linux: `$XDG_RUNTIME_DIR/roost/roost.sock` (falls back to `/tmp/roost-<uid>/roost.sock`).
/// macOS: `~/Library/Caches/roost/roost.sock`.
pub fn default_socket_path() -> anyhow::Result<PathBuf> {
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").context("$HOME not set")?;
        Ok(PathBuf::from(home)
            .join("Library/Caches/roost")
            .join("roost.sock"))
    } else {
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            return Ok(PathBuf::from(dir).join("roost").join("roost.sock"));
        }
        // Fallback for systems without XDG_RUNTIME_DIR (containers, SSH).
        let uid = libc_getuid();
        Ok(PathBuf::from(format!("/tmp/roost-{uid}")).join("roost.sock"))
    }
}

#[cfg(unix)]
extern "C" {
    fn getuid() -> u32;
}

#[cfg(unix)]
fn libc_getuid() -> u32 {
    unsafe { getuid() }
}

#[cfg(not(unix))]
fn libc_getuid() -> u32 {
    0
}

/// Bind the socket and run the gRPC server until ctrl-c.
pub async fn run(config: Config) -> anyhow::Result<()> {
    prepare_socket_path(&config.socket_path).await?;

    let listener = UnixListener::bind(&config.socket_path)
        .with_context(|| format!("failed to bind {}", config.socket_path.display()))?;
    set_socket_perms(&config.socket_path)?;

    info!(path = %config.socket_path.display(), "roost-core listening");

    let workspace = Arc::new(Workspace::new());
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
