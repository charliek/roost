//! Payload helpers shared by every adapter.
//!
//! Four of the five agents borrow Claude's hook envelope, so the same
//! JSON accessors and the same event-name normalization serve all of
//! them. Policy stays in the per-agent module: nothing here knows an
//! event name, a lifecycle, or a source string.

use serde_json::Value;

/// A string field, or `""` when it is absent, null, or not a string.
///
/// Every accessor here degrades rather than failing: an adapter reads
/// open payloads written by five different producers, and a hook that
/// cannot be interpreted must not break the turn it fired from.
pub(crate) fn field<'a>(payload: &'a Value, key: &str) -> &'a str {
    payload.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Present and non-null. `agent_id: null` is JSON's way of saying the
/// key isn't carrying a value, so it reads as absent.
pub(crate) fn has_field(payload: &Value, key: &str) -> bool {
    payload.get(key).is_some_and(|v| !v.is_null())
}

pub(crate) fn non_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

pub(crate) fn array_len(payload: &Value, key: &str) -> usize {
    payload
        .get(key)
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

/// Resolve `name` against `table`, ignoring case and every
/// non-alphanumeric character, so `SessionStart`, `session-start` and
/// `SESSION_START` all reach the same entry.
///
/// Matches without allocating, so an unrecognized name of any length
/// costs only the comparisons. Every table key must already be
/// lowercase alphanumerics — anything else can never match.
pub(crate) fn parse_normalized<T: Copy>(name: &str, table: &[(&str, T)]) -> Option<T> {
    for (want, value) in table {
        let mut wanted = want.bytes();
        let matched = name
            .bytes()
            .filter(u8::is_ascii_alphanumeric)
            .all(|c| wanted.next() == Some(c.to_ascii_lowercase()));
        if matched && wanted.next().is_none() {
            return Some(*value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accessors_degrade_on_anything_that_is_not_the_expected_shape() {
        let payload = json!({ "s": "v", "empty": "", "n": 7, "null": null, "arr": [1, 2] });
        assert_eq!(field(&payload, "s"), "v");
        assert_eq!(field(&payload, "n"), "");
        assert_eq!(field(&payload, "missing"), "");
        assert_eq!(field(&json!([]), "s"), "");
        assert_eq!(non_empty(field(&payload, "empty")), None);
        assert_eq!(non_empty(field(&payload, "s")), Some("v"));
        assert!(has_field(&payload, "empty"));
        assert!(!has_field(&payload, "null"));
        assert!(!has_field(&payload, "missing"));
        assert_eq!(array_len(&payload, "arr"), 2);
        assert_eq!(array_len(&payload, "n"), 0);
        assert_eq!(array_len(&payload, "missing"), 0);
    }

    #[test]
    fn parse_normalized_ignores_case_and_separators() {
        let table = [("sessionstart", 1), ("stopfailure", 2), ("stop", 3)];
        for spelling in [
            "SessionStart",
            "session-start",
            "SESSION_START",
            "session start",
        ] {
            assert_eq!(parse_normalized(spelling, &table), Some(1), "{spelling}");
        }
        // Exact after normalization: a prefix is not a match in either
        // direction, or `Stop` and `StopFailure` would collide.
        assert_eq!(parse_normalized("Stop", &table), Some(3));
        assert_eq!(parse_normalized("StopFailure", &table), Some(2));
        assert_eq!(parse_normalized("Sto", &table), None);
        assert_eq!(parse_normalized("Stopped", &table), None);
        assert_eq!(parse_normalized("", &table), None);
        assert_eq!(parse_normalized("🙂", &table), None);
    }
}
