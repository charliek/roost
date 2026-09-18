//! Every error code a Roost socket answers with, one constant each.
//!
//! The set is open: a newer server may add a code, and a client treats one
//! it does not know as fatal for the request. `docs/reference/ipc.md` is the
//! catalogue of meanings. Producers and consumers name a code from here,
//! never as a literal: `tests/codes_test.rs` checks [`ALL`] against that
//! catalogue both ways and scans every crate's sources for a stray spelling.

pub const UNKNOWN_OP: &str = "unknown-op";
pub const UNKNOWN_FIELD: &str = "unknown-field";
pub const MISSING_PARAM: &str = "missing-param";
pub const INVALID_PARAM: &str = "invalid-param";
pub const PARSE_ERROR: &str = "parse-error";
pub const FRAME_TOO_LARGE: &str = "frame-too-large";
pub const DUPLICATE_ID: &str = "duplicate-id";
pub const NOT_FOUND: &str = "not-found";
pub const NOT_IMPLEMENTED: &str = "not-implemented";
pub const INTERNAL: &str = "internal";
pub const TOO_LARGE: &str = "too-large";
pub const STORE_FULL: &str = "store-full";
pub const SHUTTING_DOWN: &str = "shutting-down";
pub const HOST_UNAVAILABLE: &str = "host-unavailable";
pub const NOT_ENABLED: &str = "not-enabled";
pub const BUSY: &str = "busy";

pub const REPLAY_EXPIRED: &str = "replay-expired";
pub const REVISION_AHEAD: &str = "revision-ahead";
pub const SESSION_MISMATCH: &str = "session-mismatch";

pub const PROTOCOL_MISMATCH: &str = "protocol-mismatch";
pub const BUILD_MISMATCH: &str = "build-mismatch";
pub const UNSUPPORTED_KIND: &str = "unsupported-kind";
pub const TOO_MANY_ATTACHES: &str = "too-many-attaches";
pub const SNAPSHOT_FAILED: &str = "snapshot-failed";
pub const NOT_SUPPORTED: &str = "not-supported";

pub const DESYNC: &str = "desync";
pub const OVERFLOW: &str = "overflow";
pub const PROTOCOL_ERROR: &str = "protocol-error";

pub const ALL: &[&str] = &[
    UNKNOWN_OP,
    UNKNOWN_FIELD,
    MISSING_PARAM,
    INVALID_PARAM,
    PARSE_ERROR,
    FRAME_TOO_LARGE,
    DUPLICATE_ID,
    NOT_FOUND,
    NOT_IMPLEMENTED,
    INTERNAL,
    TOO_LARGE,
    STORE_FULL,
    SHUTTING_DOWN,
    HOST_UNAVAILABLE,
    NOT_ENABLED,
    BUSY,
    REPLAY_EXPIRED,
    REVISION_AHEAD,
    SESSION_MISMATCH,
    PROTOCOL_MISMATCH,
    BUILD_MISMATCH,
    UNSUPPORTED_KIND,
    TOO_MANY_ATTACHES,
    SNAPSHOT_FAILED,
    NOT_SUPPORTED,
    DESYNC,
    OVERFLOW,
    PROTOCOL_ERROR,
];
