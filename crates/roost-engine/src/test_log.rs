//! Captures what `tracing` logs while a closure runs, for tests that pin
//! a log line or its level.

use std::sync::{Arc, Mutex};

pub(crate) fn capture<R>(level: tracing::Level, run: impl FnOnce() -> R) -> (R, String) {
    let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
    let writer = Arc::clone(&captured);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_ansi(false)
        .with_writer(move || CapturedLog(Arc::clone(&writer)))
        .finish();
    let result = tracing::subscriber::with_default(subscriber, run);
    let log = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    (result, log)
}

struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
