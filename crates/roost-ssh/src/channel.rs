//! One exec channel as a [`Duplex`], with the far side's failure kept.
//!
//! `russh::Channel::into_stream` would be the obvious duplex, but it
//! drops extended data and the exit status on the floor — and
//! `client-bridge`'s one-line stderr plus its non-zero exit are the only
//! way to tell "no session is listening" from a clean close. So a pump
//! task owns the channel's read half: data goes to the caller's reader,
//! stderr is kept, and when the remote command exits non-zero the
//! reader's last read is an error carrying the classified
//! [`DialError::Remote`] instead of a bare EOF.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{ready, Context, Poll};

use bytes::{Buf, Bytes};
use roost_client::dial::{DialError, Duplex};
use roost_ipc::ssh::{classify_ssh_failure, SshFailure};
use russh::client::Msg;
use russh::{Channel, ChannelMsg, ChannelReadHalf, ChannelWriteHalf};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::mpsc;

/// The standard stderr stream number (RFC 4254 §5.2).
const STDERR: u32 = 1;

/// How much of the remote command's stderr is kept for classifying it.
/// `client-bridge` fails in one line; a login shell's noise ahead of it
/// is what this bounds.
const STDERR_TAIL: usize = 8 * 1024;

/// Chunks queued between the pump and the caller's reader.
const QUEUE: usize = 32;

/// The far side's classified failure, as the dialer last saw it.
#[derive(Debug, Clone)]
pub(crate) struct RemoteError {
    pub failure: Option<SshFailure>,
    pub message: String,
}

impl RemoteError {
    pub(crate) fn to_dial_error(&self) -> DialError {
        DialError::Remote {
            failure: self.failure.clone(),
            message: self.message.clone(),
        }
    }
}

pub(crate) type LastRemote = Arc<Mutex<Option<RemoteError>>>;

/// Split an exec'd channel into the caller's duplex and a pump task.
/// `target` names the host in a classified message.
pub(crate) fn duplex(channel: Channel<Msg>, target: String, last: LastRemote) -> Duplex {
    let (read, write) = channel.split();
    let writer = write.make_writer();
    let (tx, rx) = mpsc::channel(QUEUE);
    tokio::spawn(pump(read, write, tx, target, last));
    Duplex {
        read: Box::new(ChannelReader {
            rx,
            pending: Bytes::new(),
        }),
        write: Box::new(writer),
    }
}

/// How the remote command ended, as far as the channel said.
#[derive(Debug, Default)]
struct Ending {
    exit: Option<Exit>,
    stderr: Vec<u8>,
    /// The server refused the exec request itself.
    refused: bool,
    /// The connection went away with no `close` for this channel.
    lost: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Exit {
    Status(u32),
    Signal(String),
}

impl Ending {
    fn keep_stderr(&mut self, data: &[u8]) {
        self.stderr.extend_from_slice(data);
        if self.stderr.len() > STDERR_TAIL {
            let excess = self.stderr.len() - STDERR_TAIL;
            self.stderr.drain(..excess);
        }
    }

    /// What the caller's reader ends on: `None` is a clean EOF.
    fn verdict(&self, target: &str) -> Option<Verdict> {
        let stderr = String::from_utf8_lossy(&self.stderr);
        let failure = if self.refused {
            SshFailure::Transport(Some("the host refused to run the bridge command".into()))
        } else {
            match &self.exit {
                Some(Exit::Status(0)) => return None,
                Some(Exit::Status(code)) => {
                    classify_ssh_failure(i32::try_from(*code).ok(), &stderr)
                }
                Some(Exit::Signal(signal)) => match classify_ssh_failure(None, &stderr) {
                    SshFailure::Transport(None) => SshFailure::Transport(Some(format!(
                        "the bridge command was killed by signal {signal}"
                    ))),
                    failure => failure,
                },
                None if !stderr.trim().is_empty() => classify_ssh_failure(None, &stderr),
                None if self.lost => return Some(Verdict::Lost),
                None => return None,
            }
        };
        let message = failure.message(target);
        Some(Verdict::Remote(RemoteError {
            failure: Some(failure),
            message,
        }))
    }
}

#[derive(Debug)]
enum Verdict {
    Remote(RemoteError),
    Lost,
}

async fn pump(
    mut read: ChannelReadHalf,
    write: ChannelWriteHalf<Msg>,
    tx: mpsc::Sender<io::Result<Bytes>>,
    target: String,
    last: LastRemote,
) {
    let mut ending = Ending::default();
    let mut eof = false;
    let mut seen_data = false;
    loop {
        let msg = tokio::select! {
            () = tx.closed() => {
                let _ = write.close().await;
                return;
            }
            msg = read.wait() => msg,
        };
        match msg {
            Some(ChannelMsg::Data { data }) => {
                if data.is_empty() {
                    continue;
                }
                if !seen_data {
                    seen_data = true;
                    // The far side answered, so whatever failed before is
                    // no longer the reason for anything.
                    last.lock().expect("last remote error").take();
                }
                if tx.send(Ok(data)).await.is_err() {
                    let _ = write.close().await;
                    return;
                }
            }
            Some(ChannelMsg::ExtendedData { data, ext: STDERR }) => ending.keep_stderr(&data),
            Some(ChannelMsg::ExitStatus { exit_status }) => {
                ending.exit = Some(Exit::Status(exit_status));
                if eof {
                    break;
                }
            }
            Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                ending.exit = Some(Exit::Signal(format!("{signal_name:?}")));
                if eof {
                    break;
                }
            }
            // Servers send `eof` and the exit status in either order;
            // the reader's ending waits for both, or for `close`.
            Some(ChannelMsg::Eof) => {
                eof = true;
                if ending.exit.is_some() {
                    break;
                }
            }
            Some(ChannelMsg::Failure) => {
                ending.refused = true;
                let _ = write.close().await;
                break;
            }
            Some(ChannelMsg::Close) => break,
            None => {
                ending.lost = true;
                break;
            }
            Some(_) => {}
        }
    }
    let error = match ending.verdict(&target) {
        None => return,
        Some(Verdict::Lost) => io::Error::new(
            io::ErrorKind::ConnectionAborted,
            format!("the SSH connection to {target} was lost"),
        ),
        Some(Verdict::Remote(remote)) => {
            tracing::debug!(host = %target, message = %remote.message, "remote command failed");
            let error = io::Error::other(remote.to_dial_error());
            *last.lock().expect("last remote error") = Some(remote);
            error
        }
    };
    let _ = tx.send(Err(error)).await;
}

struct ChannelReader {
    rx: mpsc::Receiver<io::Result<Bytes>>,
    pending: Bytes,
}

impl AsyncRead for ChannelReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        while self.pending.is_empty() {
            match ready!(self.rx.poll_recv(cx)) {
                Some(Ok(chunk)) => self.pending = chunk,
                Some(Err(error)) => return Poll::Ready(Err(error)),
                None => return Poll::Ready(Ok(())),
            }
        }
        let n = buf.remaining().min(self.pending.len());
        buf.put_slice(&self.pending[..n]);
        self.pending.advance(n);
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ending(exit: Option<Exit>, stderr: &str) -> Ending {
        Ending {
            exit,
            stderr: stderr.as_bytes().to_vec(),
            ..Ending::default()
        }
    }

    fn failure(ending: &Ending) -> Option<SshFailure> {
        match ending.verdict("popos") {
            Some(Verdict::Remote(remote)) => remote.failure,
            other => panic!("expected a remote failure, got {other:?}"),
        }
    }

    #[test]
    fn a_clean_exit_is_a_clean_eof() {
        assert!(ending(Some(Exit::Status(0)), "").verdict("popos").is_none());
        assert!(ending(None, "").verdict("popos").is_none());
    }

    #[test]
    fn the_bridge_without_a_session_is_no_session() {
        let ending = ending(
            Some(Exit::Status(1)),
            "client-bridge: no session is listening at /x/roost.sock; run 'roostctl session start' on this machine\n",
        );
        assert_eq!(failure(&ending), Some(SshFailure::NoSession));
        let Some(Verdict::Remote(remote)) = ending.verdict("popos") else {
            panic!("a remote failure");
        };
        assert!(
            remote
                .message
                .contains("popos is reachable but has no roost session running"),
            "{}",
            remote.message
        );
    }

    #[test]
    fn a_missing_binary_is_not_found() {
        assert_eq!(
            failure(&ending(
                Some(Exit::Status(127)),
                "roost-session: command not found\n"
            )),
            Some(SshFailure::NotFound)
        );
    }

    #[test]
    fn a_signal_with_nothing_on_stderr_names_the_signal() {
        assert_eq!(
            failure(&ending(Some(Exit::Signal("KILL".into())), "")),
            Some(SshFailure::Transport(Some(
                "the bridge command was killed by signal KILL".into()
            )))
        );
    }

    #[test]
    fn a_refused_exec_and_a_lost_connection_are_failures() {
        let refused = Ending {
            refused: true,
            ..Ending::default()
        };
        assert!(matches!(
            failure(&refused),
            Some(SshFailure::Transport(Some(_)))
        ));
        let lost = Ending {
            lost: true,
            ..Ending::default()
        };
        assert!(matches!(lost.verdict("popos"), Some(Verdict::Lost)));
    }

    #[test]
    fn only_the_stderr_tail_is_kept() {
        let mut ending = Ending::default();
        ending.keep_stderr(&vec![b'x'; STDERR_TAIL]);
        ending.keep_stderr(b"client-bridge: no session");
        assert_eq!(ending.stderr.len(), STDERR_TAIL);
        assert!(ending.stderr.ends_with(b"client-bridge: no session"));
    }
}
