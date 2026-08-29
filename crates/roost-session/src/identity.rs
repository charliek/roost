//! The two values `session.identify` answers with that nothing else in
//! the process can supply: a random session id and a start timestamp.
//!
//! Both are hand-rolled on purpose. The workspace graph has no date-time
//! crate and no random-number framework, and neither value justifies
//! adding one: the id is 16 bytes of OS entropy rendered hex, and the
//! timestamp is a fixed-shape UTC RFC3339 string with no parsing,
//! arithmetic, or zone handling behind it.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

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
}
