//! Everything this binary answers about *itself*: a random session id
//! and a start timestamp for a running session, and — separately — this
//! build's offline identity for `roost-session identify`.
//!
//! The session id and timestamp are hand-rolled on purpose. The
//! workspace graph has no date-time crate and no random-number
//! framework, and neither value justifies adding one: the id is 16
//! bytes of OS entropy rendered hex, and the timestamp is a fixed-shape
//! UTC RFC3339 string with no parsing, arithmetic, or zone handling
//! behind it.
//!
//! [`build_identity`] answers a different question — not "which
//! process is this" but "which build is this" — and exists here rather
//! than in a new module because `serve`'s `SessionInfo` construction and
//! the `identify` subcommand both need it, and a second place to compute
//! it is exactly how the two would drift. [`test_mode_env`] is the same
//! argument one layer down: both callers need `ROOST_TEST_MODE` and
//! `ROOST_SESSION_FAKE_BUILD` read the same way, so there is one reader.

use std::fmt::Write as _;
use std::io::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use roost_ipc::messages::{SessionBinaryIdentity, SESSION_PROTOCOL_VERSION};

/// This build's offline identity: what `roost-session identify` prints,
/// and what `serve`'s `session.identify` answer's `libghostty_build`
/// field is drawn from.
///
/// `fake_libghostty_build` is the already-read `ROOST_SESSION_FAKE_BUILD`
/// value (or `None`); `test_mode` re-gates it here rather than trusting
/// the caller's gate alone, mirroring `serve.rs`'s
/// `.filter(|_| config.test_mode)` — a caller that hand-builds test data
/// with `test_mode: false` and a fake value set still gets the truth.
pub fn build_identity(
    fake_libghostty_build: Option<&str>,
    test_mode: bool,
) -> SessionBinaryIdentity {
    SessionBinaryIdentity {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        session_protocol: SESSION_PROTOCOL_VERSION,
        libghostty_build: resolve_libghostty_build(fake_libghostty_build, test_mode),
    }
}

/// `roost-session identify`: print this build's identity as exactly one
/// JSON line on stdout and exit 0.
///
/// Pure compile-time identity — no socket, no profile, no side effects,
/// so a binary that has never run can still answer it.
pub fn run() -> i32 {
    let (test_mode, fake_libghostty_build) = test_mode_env();
    let identity = build_identity(fake_libghostty_build.as_deref(), test_mode);
    let line = match serde_json::to_string(&identity) {
        Ok(line) => line,
        Err(error) => {
            eprintln!("roost-session identify: {error:#}");
            return 1;
        }
    };

    // A locked, fallible writer rather than `println!`: Rust ignores
    // SIGPIPE by default, so `println!` would panic (and exit 101) if
    // the ssh reader on the far end of stdout closes early.
    let mut stdout = std::io::stdout().lock();
    let write_result = stdout
        .write_all(line.as_bytes())
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush());
    match write_result {
        Ok(()) => 0,
        // The reader is already gone; there is nobody left to tell, and
        // stderr noise here would be misread by the bootstrap classifier
        // as a real failure. 0 is defensible — the process did its job,
        // it just found nobody listening at the end.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => 0,
        Err(error) => {
            eprintln!("roost-session identify: {error:#}");
            1
        }
    }
}

/// Reads `ROOST_TEST_MODE` and, gated on it, `ROOST_SESSION_FAKE_BUILD`
/// from the environment.
///
/// The one place either variable is read directly: [`run`] (the CLI
/// edge, for a binary that has never started) and `SessionConfig::
/// from_profile` (the daemon edge, for a running session) both call
/// this rather than each reading the environment themselves, so the two
/// cannot drift apart on what "test mode" means.
pub(crate) fn test_mode_env() -> (bool, Option<String>) {
    let test_mode = std::env::var("ROOST_TEST_MODE").is_ok_and(|value| value == "1");
    let fake_libghostty_build = test_mode
        .then(|| std::env::var(crate::consts::FAKE_BUILD_ENV).ok())
        .flatten()
        .filter(|value| !value.is_empty());
    (test_mode, fake_libghostty_build)
}

fn resolve_libghostty_build(fake_libghostty_build: Option<&str>, test_mode: bool) -> String {
    match fake_libghostty_build.filter(|_| test_mode) {
        Some(fake) => {
            tracing::warn!(
                build = %fake,
                "reporting a fake libghostty build: {} is set in test mode",
                crate::consts::FAKE_BUILD_ENV
            );
            fake.to_string()
        }
        None => roost_vt::libghostty_build(),
    }
}

/// 128 bits of OS entropy as 32 lowercase hex characters.
///
/// The id only has to be unique across the sessions a user's clients can
/// see at once, and never has to be unguessable — it is not a
/// credential, the socket's uid check is. 128 bits is far past what that
/// needs and costs one syscall.
pub fn session_id() -> String {
    let mut bytes = [0u8; 16];
    // A failure here means the OS could not produce entropy at all,
    // which on Linux/macOS is not a recoverable condition for a process
    // that has to name itself.
    getrandom::fill(&mut bytes).expect("the OS random source must be available");
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `SystemTime` as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Seconds resolution, always UTC, always the `Z` suffix — a session
/// starts once and clients only ever display or compare this. A clock
/// before the epoch (or a `SystemTime` that cannot be differenced)
/// renders as the epoch rather than failing a start over a timestamp.
pub fn rfc3339_utc(at: SystemTime) -> String {
    let secs = at
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let time_of_day = secs.rem_euclid(86_400);
    let (hour, minute, second) = (
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Days-since-epoch to `(year, month, day)` — Howard Hinnant's
/// `civil_from_days`, which is the standard closed-form for this and
/// avoids a leap-year loop.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    // Shift the era origin to 0000-03-01 so the leap-day lands at the
    // end of a year and the month arithmetic below becomes linear.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_session_id_is_128_bits_of_hex() {
        let id = session_id();
        assert_eq!(id.len(), 32);
        assert!(id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        assert_ne!(id, session_id(), "ids must not repeat");
    }

    #[test]
    fn the_epoch_renders_as_the_epoch() {
        assert_eq!(rfc3339_utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn known_instants_render_exactly() {
        for (secs, want) in [
            (1_u64, "1970-01-01T00:00:01Z"),
            // A leap day, which the month arithmetic is the whole reason
            // for getting right.
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_234_567_890, "2009-02-13T23:31:30Z"),
            (1_700_000_000, "2023-11-14T22:13:20Z"),
        ] {
            let at = UNIX_EPOCH + Duration::from_secs(secs);
            assert_eq!(rfc3339_utc(at), want, "for {secs}");
        }
    }

    /// Clients parse this with a strict RFC3339 reader, so the shape is
    /// the contract: fixed width, `T` separator, `Z` zone.
    #[test]
    fn now_has_the_wire_shape() {
        let now = rfc3339_utc(SystemTime::now());
        assert_eq!(now.len(), 20, "{now}");
        assert!(now.ends_with('Z'), "{now}");
        assert_eq!(now.as_bytes()[10], b'T', "{now}");
    }

    #[test]
    fn build_identity_always_carries_the_real_app_version_and_protocol() {
        let identity = build_identity(Some("fake-build"), true);
        assert_eq!(identity.app_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(identity.session_protocol, SESSION_PROTOCOL_VERSION);
    }

    /// The double gate (plan 039 §3.1): a fake build only ever surfaces
    /// with `test_mode: true`. Exercised on the pure helper rather than
    /// by mutating `ROOST_TEST_MODE` — env mutation is process-global and
    /// would race every other test in this binary.
    #[test]
    fn a_fake_build_only_applies_under_test_mode() {
        let faked = build_identity(Some("fake-build-123"), true);
        assert_eq!(faked.libghostty_build, "fake-build-123");

        let real = build_identity(Some("fake-build-123"), false);
        assert_eq!(real.libghostty_build, roost_vt::libghostty_build());
        assert_ne!(real.libghostty_build, "fake-build-123");
    }

    #[test]
    fn no_fake_build_set_always_reports_the_real_value() {
        let identity = build_identity(None, true);
        assert_eq!(identity.libghostty_build, roost_vt::libghostty_build());
    }
}
