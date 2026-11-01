//! Plan 087 D3: an `accept` that fails `EMFILE` must not end a server's
//! accept loop. Every connection after the error is served. On Linux the
//! connection the failed `accept` could not take stays queued and is
//! served too; XNU has already dequeued it when the descriptor
//! allocation fails, so on macOS the kernel resets it and only the
//! later connection can be asserted.
//!
//! This is the only test in its binary, and has to be: it lowers the
//! process's soft `RLIMIT_NOFILE` and then takes every descriptor under
//! it. CI runs plain `cargo test`, where a binary's tests share one
//! process, so any sibling test would run out of descriptors too.

use std::future::Future;
use std::os::fd::{AsFd, OwnedFd};
use std::pin::Pin;
use std::time::Duration;

use roost_ipc::framing::{write_frame, FrameReader};
use roost_ipc::session_launch::timeout_scale;
use roost_ipc::{ConnCtx, Handler, HandlerError, HandlerOutcome, IpcServer};
use tokio::net::UnixStream;

const ANSWER_BUDGET: Duration = Duration::from_secs(5);
/// Descriptors the lowered limit leaves above the lowest free one; the
/// fillers take all of them.
const FILL_HEADROOM: u64 = 64;

struct IdentifyHandler;

impl Handler for IdentifyHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a ConnCtx,
        op: &'a str,
        _params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<HandlerOutcome, HandlerError>> + Send + 'a>> {
        Box::pin(async move {
            assert_eq!(op, "identify");
            Ok(HandlerOutcome::Reply(
                serde_json::json!({"app": "emfile-test"}),
            ))
        })
    }
}

fn nofile_limit() -> libc::rlimit {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes one `rlimit`, which is what it is given.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
    assert_eq!(rc, 0, "getrlimit: {}", std::io::Error::last_os_error());
    limit
}

fn set_soft_nofile(soft: u64) {
    let limit = libc::rlimit {
        rlim_cur: soft,
        rlim_max: nofile_limit().rlim_max,
    };
    // SAFETY: `setrlimit` reads one `rlimit`, which is what it is given.
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
    assert_eq!(rc, 0, "setrlimit: {}", std::io::Error::last_os_error());
}

fn dup_stderr() -> std::io::Result<OwnedFd> {
    std::io::stderr().as_fd().try_clone_to_owned()
}

/// Lower the soft limit and take every descriptor under it but one.
fn fill_all_but_one() -> Vec<OwnedFd> {
    let probe = dup_stderr().expect("probe dup");
    let lowest_free = u64::try_from(std::os::fd::AsRawFd::as_raw_fd(&probe)).unwrap();
    drop(probe);
    set_soft_nofile(lowest_free + FILL_HEADROOM);
    let mut fillers = Vec::new();
    loop {
        match dup_stderr() {
            Ok(fd) => fillers.push(fd),
            Err(error) if error.raw_os_error() == Some(libc::EMFILE) => break,
            Err(error) => panic!("filling descriptors: {error}"),
        }
    }
    assert!(
        !fillers.is_empty(),
        "the lowered limit left nothing to fill"
    );
    fillers.pop();
    fillers
}

async fn identify(stream: UnixStream) -> serde_json::Value {
    let (read, mut write) = stream.into_split();
    let request = serde_json::to_vec(&serde_json::json!({
        "id": "1",
        "op": "identify",
        "params": {}
    }))
    .unwrap();
    write_frame(&mut write, &request)
        .await
        .expect("write identify");
    let mut reader = FrameReader::new(read);
    let line = tokio::time::timeout(ANSWER_BUDGET.mul_f64(timeout_scale()), reader.read_line())
        .await
        .expect("the server never answered identify")
        .expect("reading the identify reply")
        .expect("the server closed the connection without answering identify");
    serde_json::from_slice(&line).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_that_met_emfile_is_served_once_descriptors_free() {
    // Connects and writes have no budget of their own; this one bounds
    // the whole scenario so a stuck step fails instead of hanging.
    tokio::time::timeout(ANSWER_BUDGET.mul_f64(4.0 * timeout_scale()), scenario())
        .await
        .expect("the EMFILE scenario did not finish within its budget");
}

async fn scenario() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("roost.sock");
    let server = IpcServer::bind(&socket, IdentifyHandler)
        .await
        .expect("bind");
    tokio::spawn(server.run());
    let before = roost_ipc::transient_accept_errors();

    let fillers = fill_all_but_one();
    let pending = UnixStream::connect(&socket)
        .await
        .expect("connect with the last free descriptor");

    // Without this wait, a scheduler that never ran the accept while
    // descriptors were short would pass the test without an EMFILE.
    let deadline = tokio::time::Instant::now() + ANSWER_BUDGET.mul_f64(timeout_scale());
    while roost_ipc::transient_accept_errors() == before {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the server's accept never failed transiently while descriptors were short"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drop(fillers);

    if cfg!(target_os = "linux") {
        let reply = identify(pending).await;
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["result"]["app"], "emfile-test", "{reply}");
    } else {
        drop(pending);
    }

    let fresh = UnixStream::connect(&socket).await.expect("connect again");
    let reply = identify(fresh).await;
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["result"]["app"], "emfile-test", "{reply}");
}
