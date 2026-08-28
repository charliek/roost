//! The session's log file.
//!
//! Same shape as the Linux UI's: a **synchronous** appender behind a
//! `Mutex`, no background writer thread and no buffering layer, so a
//! record is on disk before the call that made it returns. A daemon that
//! aborts — the panic hook's last act — must leave its last lines
//! behind, and a buffered writer is exactly what loses them.
//!
//! The console tee goes to **stderr**, not stdout. Under `--foreground`
//! stdout carries the readiness verdict and nothing else, so a caller
//! can read one line from it without a parser; an operator watching the
//! same run still sees the log. Daemonized, stderr is `/dev/null` and
//! the file is the whole record.

use std::fs::OpenOptions;
use std::sync::Mutex;

use anyhow::{Context, Result};
use roost_ipc::paths::BundleProfile;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Install the process-global subscriber. Call once, after the log
/// directory exists.
pub fn init(profile: &BundleProfile) -> Result<()> {
    let path = profile.log_path();
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(Mutex::new(file)),
        )
        .try_init()
        .map_err(|error| anyhow::anyhow!("initialize tracing: {error}"))
}
