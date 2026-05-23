//! Verifies the file-log writer pattern used by `main::init_logging` actually
//! writes `tracing` events to a `roost.log` file. Uses a *scoped* subscriber
//! (`with_default`) rather than the global `init()` so it doesn't collide with
//! other tests / the process-global subscriber. `Mutex<File>` writes
//! synchronously (unbuffered `File`), so the bytes are on disk once the event
//! returns — the same crash-safe property `init_logging` relies on.

use std::fs;
use std::io::Read;
use std::sync::Mutex;

use tracing_subscriber::layer::SubscriberExt;

#[test]
fn file_layer_writes_tracing_events() {
    let dir = std::env::temp_dir().join(format!("roost-logtest-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("roost.log");
    let _ = fs::remove_file(&path);

    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();

    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(Mutex::new(file)),
    );

    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(probe = "roost-file-log-probe", "startup line");
    });

    let mut contents = String::new();
    fs::File::open(&path)
        .unwrap()
        .read_to_string(&mut contents)
        .unwrap();
    let _ = fs::remove_file(&path);

    assert!(
        contents.contains("roost-file-log-probe") && contents.contains("startup line"),
        "log file did not capture the tracing event; got: {contents:?}"
    );
}
