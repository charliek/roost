//! `session.put_file` against a live session: where the bytes land, who
//! can read them, and when they go away (plan 047 §3.1, W1).
//!
//! The engine's own tests cover the op's refusals; what only a real
//! `serve()` can show is the half this crate owns — the root it picks,
//! the modes it creates, the sweep at start and the sweep at a clean
//! stop. The direct-finalize entrance, which must *not* sweep, is
//! fenced next to the two stop paths themselves in `serve.rs`, because
//! that entrance is only reachable from inside the process.

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use roost_ipc::client::ClientError;
use roost_ipc::messages::{ops, SessionPutFileParams, SessionPutFileResult};
use roost_ipc::IpcClient;
use roost_session::SessionConfig;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n-- not really a png, but bytes are bytes --";

async fn put_file(
    client: &mut IpcClient,
    name: &str,
    data: &[u8],
) -> Result<SessionPutFileResult, ClientError> {
    client
        .call(
            ops::SESSION_PUT_FILE,
            SessionPutFileParams {
                name: name.to_string(),
                data: data.to_vec(),
            },
        )
        .await
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .unwrap_or_else(|error| panic!("stat {}: {error}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

/// The grammar the client re-checks every returned path against.
fn is_paste_safe(path: &str) -> bool {
    path.starts_with('/')
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
}

fn code(error: &ClientError) -> String {
    match error {
        ClientError::Server { code, .. } => code.clone(),
        other => panic!("expected a server error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_uploaded_file_lands_private_under_a_pasteable_path() {
    let layout = support::Layout::new();
    let launch_cwd = layout.launch_cwd.clone();
    let served = layout.spawn(&launch_cwd);
    let mut client = support::connect(&layout.socket_path()).await;

    let result = put_file(&mut client, "roost-image-1757083567-8f3a.png", PNG)
        .await
        .expect("session.put_file");

    assert_eq!(result.bytes, PNG.len() as u64);
    assert!(is_paste_safe(&result.path), "{}", result.path);
    let path = PathBuf::from(&result.path);
    assert_eq!(
        path.file_name().and_then(|n| n.to_str()),
        Some("roost-image-1757083567-8f3a.png"),
        "the final component is the name the client sent, unrepaired"
    );
    assert_eq!(std::fs::read(&path).expect("read the landed file"), PNG);
    assert_eq!(mode(&path), 0o600, "the file is the session user's alone");

    let upload_dir = path.parent().expect("an upload directory");
    assert_eq!(mode(upload_dir), 0o700);
    assert_eq!(mode(&layout.files_dir()), 0o700);
    assert_eq!(
        upload_dir.parent(),
        Some(layout.files_dir().as_path()),
        "uploads hang directly off the configured root"
    );
    // Nothing partial is left beside it.
    let siblings: Vec<_> = std::fs::read_dir(upload_dir)
        .expect("read the upload directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(siblings.len(), 1, "got {siblings:?}");

    support::session_stop(&mut client).await;
    served.await.expect("join").expect("serve");
}

/// A clean stop takes the store with it — and a start after a crash
/// takes whatever the crash left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clean_stop_sweeps_the_store_and_so_does_the_next_start() {
    let layout = support::Layout::new();
    let launch_cwd = layout.launch_cwd.clone();

    let served = layout.spawn(&launch_cwd);
    let mut client = support::connect(&layout.socket_path()).await;
    let landed = put_file(&mut client, "design.pdf", PNG)
        .await
        .expect("session.put_file")
        .path;
    assert!(Path::new(&landed).exists());

    support::session_stop(&mut client).await;
    served.await.expect("join").expect("serve");
    assert!(
        !layout.files_dir().exists(),
        "a clean stop must leave no store behind"
    );

    // Now the crash case: leftovers nobody swept, and a fresh start
    // over the same layout.
    let stale = layout.files_dir().join("4b9d1e7f0a3c5e21");
    std::fs::create_dir_all(&stale).expect("seed leftovers");
    std::fs::write(stale.join("old.png"), PNG).expect("seed a leftover file");

    let served = layout.spawn(&launch_cwd);
    let mut client = support::connect(&layout.socket_path()).await;
    assert!(
        !stale.exists(),
        "a start must sweep what a crash left in the store"
    );

    support::session_stop(&mut client).await;
    tokio::time::timeout(support::scaled(Duration::from_secs(30)), served)
        .await
        .expect("the second session must stop within its budget")
        .expect("join")
        .expect("serve");
}

/// A cache path that could not be pasted bare — a space is the one
/// every macOS `~/Library` path has — moves the whole store to `/tmp`
/// rather than handing back a path the agent on the far side would
/// split in half.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unpasteable_root_falls_back_to_tmp() {
    let layout = support::Layout::new();
    let launch_cwd = layout.launch_cwd.clone();
    let mut config = layout.config(&launch_cwd);
    config.files_dir = Some(layout.root().join("Application Support/files"));
    let config_fallback = config.files_fallback.clone();

    let served = layout.spawn_config(config);
    let mut client = support::connect(&layout.socket_path()).await;

    let result = put_file(&mut client, "shot.png", PNG)
        .await
        .expect("session.put_file");

    assert!(is_paste_safe(&result.path), "{}", result.path);
    assert!(
        Path::new(&result.path).starts_with(&config_fallback),
        "expected the configured fallback {}, got {}",
        config_fallback.display(),
        result.path
    );
    assert_eq!(std::fs::read(&result.path).expect("read it back"), PNG);
    assert!(
        !layout.root().join("Application Support/files").exists(),
        "the unpasteable root must not be used at all"
    );

    support::session_stop(&mut client).await;
    served.await.expect("join").expect("serve");
    assert!(!Path::new(&result.path).exists());
}

/// A store that cannot be created is not a session that refuses to
/// start: everything else still serves, and the op says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_store_that_cannot_be_created_answers_not_supported() {
    let layout = support::Layout::new();
    let launch_cwd = layout.launch_cwd.clone();
    let blocker = layout.root().join("blocker");
    std::fs::write(&blocker, b"a file, not a directory").expect("seed the blocker");
    let mut config = layout.config(&launch_cwd);
    config.files_dir = Some(blocker.join("files"));

    let served = layout.spawn_config(config);
    let mut client = support::connect(&layout.socket_path()).await;

    let error = put_file(&mut client, "shot.png", PNG)
        .await
        .expect_err("a session with no store cannot land a file");
    assert_eq!(code(&error), "not-supported");

    // The session is otherwise entirely healthy.
    assert!(!support::identify(&mut client).await.app_label.is_empty());

    support::session_stop(&mut client).await;
    served.await.expect("join").expect("serve");
}

/// The config a session actually ships with points at the profile's
/// cache, not at a tempdir — the one thing `Layout` cannot show.
#[test]
fn the_shipped_config_takes_its_store_from_the_profile() {
    let profile = roost_ipc::paths::BundleProfile::session().expect("a session profile");
    let config = SessionConfig::from_profile(&profile, PathBuf::from("/tmp"));
    assert_eq!(
        config.files_dir,
        Some(profile.files_dir().expect("a files dir"))
    );
}
