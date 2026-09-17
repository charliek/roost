//! Every `config.conf` write this process makes, in one place, off the
//! UI thread, and in the order the UI asked for them.
//!
//! # Why not just call `set_key`
//!
//! [`roost_ui_model::config::set_key`] takes `config.lock` and waits up
//! to ten seconds for it. The other holder is an agent-hooks ensure —
//! this process's own, `roostctl`'s, the Swift app's, or a session
//! raising the key for a connecting client — and that hold spans a read,
//! a union, up to five agent-file rewrites and the key write. On a
//! network-mounted `$HOME` that is seconds. Iced's `update`/`view` is
//! the winit event-loop thread, so a wait there is a frozen window.
//!
//! # Why one task rather than a task per write
//!
//! Font size is a keystroke each. A task per write hands the ordering to
//! whichever one wins the lock, so the file can end up holding the size
//! the user passed through rather than the one they stopped on. One
//! `mpsc` drained by one task means the file lands in the order the UI
//! asked, and the last write is the last thing the user did.

use std::path::PathBuf;

use tokio::sync::{mpsc, oneshot};

use crate::engine_feed::{EngineFeed, EngineFeedSender};

/// One queued write.
struct ConfigWrite {
    key: String,
    value: String,
    /// `Some` for a caller that has to know — see
    /// [`ConfigWriter::record`]. `None` routes a failure to the toast.
    reply: Option<oneshot::Sender<Result<(), String>>>,
}

/// The UI-side handle. Cloneable, cheap, and never blocks.
#[derive(Debug, Clone)]
pub(crate) struct ConfigWriter {
    tx: mpsc::UnboundedSender<ConfigWrite>,
}

/// There is nowhere to write: `$HOME` is unset and `$ROOST_CONFIG` is
/// empty, so this machine has no config file at all.
///
/// A [`ConfigWriter::record`] caller is told, because it has a decision
/// resting on the write. A [`ConfigWriter::set`] caller is not, and gets
/// no toast either: nothing failed, there is simply nowhere to persist a
/// preference to, and every one of those call sites has treated an
/// absent config path as a silent no-op since before this existed.
const NO_CONFIG: &str = "there is no config file to write";

impl ConfigWriter {
    /// Start the one writer task.
    ///
    /// `path` is resolved once, at startup, for the reason every other
    /// reader of `config_path()` resolves it once: the environment does
    /// not change under a running process, and re-deriving it per write
    /// would be a different answer only if it had.
    pub(crate) fn spawn(
        runtime: &tokio::runtime::Handle,
        path: Option<PathBuf>,
        feed: EngineFeedSender,
    ) -> ConfigWriter {
        let (tx, mut rx) = mpsc::unbounded_channel::<ConfigWrite>();
        runtime.spawn(async move {
            while let Some(write) = rx.recv().await {
                let outcome = match path.clone() {
                    None => {
                        if write.reply.is_none() {
                            tracing::debug!(key = %write.key, "{NO_CONFIG}");
                            continue;
                        }
                        Err(NO_CONFIG.to_string())
                    }
                    Some(path) => {
                        let (key, value) = (write.key.clone(), write.value.clone());
                        // Awaited inline, so the next queued write does
                        // not start until this one has landed — this is
                        // the FIFO the module doc promises.
                        tokio::task::spawn_blocking(move || {
                            roost_ui_model::config::set_key(&path, &key, &value)
                                .map_err(|error| error.to_string())
                        })
                        .await
                        .unwrap_or_else(|error| Err(format!("the write did not finish: {error}")))
                    }
                };
                match (write.reply, outcome) {
                    (Some(reply), outcome) => {
                        let _ = reply.send(outcome);
                    }
                    (None, Err(error)) => {
                        tracing::warn!(key = %write.key, %error, "could not write config.conf");
                        feed.send(EngineFeed::ConfigWriteFailed {
                            key: write.key,
                            error,
                        });
                    }
                    (None, Ok(())) => {}
                }
            }
        });
        ConfigWriter { tx }
    }

    /// Queue a write and forget it.
    ///
    /// The caller's in-memory value has already moved — these are
    /// settings the UI applies live and persists as a record of what the
    /// user chose — so a failure is a sentence on the status line, not a
    /// reason to undo what they are looking at.
    pub(crate) fn set(&self, key: &str, value: &str) {
        self.enqueue(key, value, None);
    }

    /// Queue a write whose **outcome** the caller needs.
    ///
    /// The local-backend commit points are the callers: the key is the
    /// record that a switch may be trusted, and the journal write behind
    /// it must not happen if the key did not land.
    pub(crate) fn record(
        &self,
        key: &str,
        value: &str,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send + 'static {
        let (reply, answer) = oneshot::channel();
        self.enqueue(key, value, Some(reply));
        async move {
            answer
                .await
                .unwrap_or_else(|_| Err("the config writer went away".to_string()))
        }
    }

    fn enqueue(&self, key: &str, value: &str, reply: Option<oneshot::Sender<Result<(), String>>>) {
        let write = ConfigWrite {
            key: key.to_string(),
            value: value.to_string(),
            reply,
        };
        if let Err(mpsc::error::SendError(write)) = self.tx.send(write) {
            // Only reachable once the runtime is gone, which is to say
            // during shutdown — but a write nobody is waiting for must
            // still leave a trace rather than disappearing.
            tracing::warn!(key = %write.key, "the config writer went away");
            if let Some(reply) = write.reply {
                let _ = reply.send(Err("the config writer went away".to_string()));
            }
        }
    }
}
