//! Streaming OSC scanner — Phase 6a P4.
//!
//! Port of `internal/osc/scanner.go` from the Go binary, adapted for
//! the Rust daemon's architecture:
//!
//!   * The Go scanner sits next to libghostty-vt and handles only
//!     OSC classes libghostty doesn't surface (notifications, cwd,
//!     color queries). The Mac/Linux UI's libghostty handles
//!     window-title OSC (0/1/2) itself.
//!
//!   * The Rust scanner sits in the daemon, which is intentionally
//!     libghostty-free (see goal-rust-port-polish DL choices). The
//!     UI reports raw OSC bytes via `ReportOsc` and the daemon
//!     parses everything it needs to route. So this scanner emits
//!     `Title` for OSC 0/1/2 in addition to the classes the Go
//!     scanner already handled.
//!
//! Architecture:
//!
//!   * `OscScanner::feed(bytes)` advances a stateful byte-by-byte
//!     state machine and returns any complete `OscEvent`s parsed
//!     out of the slice. State persists across `feed` calls —
//!     sequences split across multiple PTY reads scan correctly.
//!
//!   * Bodies longer than `MAX_BODY` (8192 bytes) are truncated
//!     rather than buffered indefinitely. A misbehaving program
//!     shouldn't be able to OOM the daemon.
//!
//! Out of scope (deliberately):
//!
//!   * OSC 99 (id'd notification with replace-by-id semantics):
//!     `NotificationEvent` in `proto/roost.proto` has no id field,
//!     so a clean wiring doesn't exist yet. P5's dispatch can drop
//!     OSC 99 silently; Phase 6b can extend the proto + scanner if
//!     dogfooding shows it's needed.
//!
//!   * OSC 10/11/12 color queries: emitted as `ColorQuery` events,
//!     but no synthesised response. The daemon side doesn't yet
//!     know a tab's current fg/bg/cursor (those live in the UI's
//!     libghostty render state). P5 may decide to route color
//!     queries back to the UI via a separate RPC, or just drop
//!     them. The scanner just surfaces the event so P5 has the
//!     choice.

use std::str;

/// Maximum number of body bytes the scanner will buffer before
/// truncating. 8KB matches the Go binary's `maxBody`.
const MAX_BODY: usize = 8192;

/// One parsed-out OSC event. Returned in order by `OscScanner::feed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscEvent {
    /// OSC 0 / OSC 1 / OSC 2 — set window title (and/or icon name).
    /// libghostty handles these inside the UI client; the daemon
    /// also wants them so it can push the UI's tab strip label
    /// over `TabTitleChangedEvent`.
    Title(String),

    /// OSC 7 — set working directory. Path has been extracted from
    /// the `file://[host]/path` URI form and percent-decoded.
    /// Emitted only for syntactically valid bodies; malformed
    /// percent-encoding (`%`, `%ZZ`) drops silently rather than
    /// shipping gibberish through the chrome.
    Pwd(String),

    /// OSC 9 (iTerm2 notification, title-only) or OSC 777 (Konsole
    /// `notify;title;body` form). The Go binary's ConEmu OSC 9 sub-
    /// commands (`OSC 9;<1..12>`) are filtered out — libghostty's
    /// OSC handler owns those semantics — and the resulting body
    /// dropped.
    Notification { title: String, body: String },

    /// OSC 10 / 11 / 12 with body `"?"` — query for the current
    /// foreground (10), background (11), or cursor (12) color.
    /// The scanner doesn't synthesise the response; the daemon
    /// caller decides whether to route back to the UI or drop.
    ColorQuery(u8),
}

/// State the byte-by-byte parser cycles through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Outside,
    Esc,      // saw ESC, waiting for ']'
    Prefix,   // collecting <number> before ';'
    Body,     // collecting body before BEL or ESC \
    BodyEsc,  // saw ESC inside body, expecting '\\'
}

/// Stateful OSC byte-stream scanner. Not safe for concurrent use;
/// each tab's PTY gets its own.
pub struct OscScanner {
    state: State,
    num: String,
    body: String,
    pending: Vec<OscEvent>,
}

impl Default for OscScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl OscScanner {
    pub fn new() -> Self {
        Self {
            state: State::Outside,
            num: String::new(),
            body: String::new(),
            pending: Vec::new(),
        }
    }

    /// Feed a slice of PTY bytes. Returns all OSC events parsed out
    /// in feed order. The caller is responsible for ALSO writing
    /// the bytes through to libghostty / the renderer — this is
    /// purely additive, observing the stream.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<OscEvent> {
        for &b in bytes {
            self.step(b);
        }
        std::mem::take(&mut self.pending)
    }

    fn step(&mut self, b: u8) {
        match self.state {
            State::Outside => {
                if b == 0x1B {
                    self.state = State::Esc;
                }
            }
            State::Esc => match b {
                b']' => {
                    self.state = State::Prefix;
                    self.num.clear();
                    self.body.clear();
                }
                0x1B => {
                    // ESC ESC: stay in Esc.
                }
                _ => self.state = State::Outside,
            },
            State::Prefix => match b {
                b';' => self.state = State::Body,
                0x07 => {
                    // BEL terminator with no body
                    self.dispatch();
                    self.state = State::Outside;
                }
                0x1B => self.state = State::BodyEsc,
                b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' => {
                    if self.num.len() < 8 {
                        self.num.push(b as char);
                    }
                }
                _ => self.state = State::Outside,
            },
            State::Body => match b {
                0x07 => {
                    self.dispatch();
                    self.state = State::Outside;
                }
                0x1B => self.state = State::BodyEsc,
                _ => {
                    if self.body.len() < MAX_BODY {
                        self.body.push(b as char);
                    }
                }
            },
            State::BodyEsc => {
                if b == b'\\' {
                    self.dispatch();
                    self.state = State::Outside;
                    return;
                }
                // Any byte other than `\` aborts the sequence
                // (malformed). Re-feed the byte through the outer
                // state machine so an ESC that starts a NEW OSC
                // isn't lost.
                self.state = State::Outside;
                self.step(b);
            }
        }
    }

    fn dispatch(&mut self) {
        let num = self.num.as_str();
        let body = self.body.as_str();
        match num {
            "0" | "1" | "2" => {
                // Title. OSC 0 = window + icon; 1 = icon only; 2 =
                // window only. Roost has no separate icon-title
                // concept, so all three map to the same Title event.
                if !body.is_empty() {
                    self.pending.push(OscEvent::Title(body.to_string()));
                }
            }
            "7" => {
                if let Some(path) = parse_osc7(body) {
                    self.pending.push(OscEvent::Pwd(path));
                }
            }
            "9" => {
                if is_conemu_body(body) {
                    return;
                }
                self.pending.push(OscEvent::Notification {
                    title: body.to_string(),
                    body: String::new(),
                });
            }
            "777" => {
                // Konsole form: `notify;<title>;<body>`. Some senders
                // also emit `notify;<title>` (no body).
                let parts: Vec<&str> = body.splitn(3, ';').collect();
                if parts.len() >= 2 && parts[0] == "notify" {
                    let title = parts[1].to_string();
                    let body_text = if parts.len() == 3 {
                        parts[2].to_string()
                    } else {
                        String::new()
                    };
                    self.pending.push(OscEvent::Notification {
                        title,
                        body: body_text,
                    });
                }
            }
            "10" | "11" | "12" => {
                if body != "?" {
                    return;
                }
                let n: u8 = num.parse().unwrap_or(0);
                if n == 10 || n == 11 || n == 12 {
                    self.pending.push(OscEvent::ColorQuery(n));
                }
            }
            _ => {
                // Unhandled OSC command. libghostty handles many
                // others (4 = palette, 8 = hyperlink, 52 =
                // clipboard, 110/111 = reset colors, …); we don't
                // need to route those daemon-side.
            }
        }
    }
}

/// True if an OSC 9 body looks like a ConEmu extension rather than
/// an iTerm2 notification. ConEmu uses `OSC 9;<n>[;...]` for n in
/// 1..12 (sleep, message-box, change-tab-title, progress, etc.).
///
/// A bare numeric body outside the 1..12 range (e.g. `"42;summary"`)
/// is treated as iTerm2 — ConEmu doesn't define those — so a sender
/// using a numeric title still gets the notification. A genuine
/// iTerm2 notification whose text starts with a digit followed by
/// any non-digit byte (e.g. `"1 file changed"`) still passes
/// through.
fn is_conemu_body(body: &str) -> bool {
    let bytes = body.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() {
        return false;
    }
    let mut n: i32 = 0;
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        // Cap to keep this from overflowing on absurd inputs.
        if n < 100 {
            n = n * 10 + (bytes[i] - b'0') as i32;
        } else {
            n = 100;
        }
        i += 1;
    }
    if !(1..=12).contains(&n) {
        return false;
    }
    i == bytes.len() || bytes[i] == b';'
}

/// Decode an OSC 7 body of the form `file://[host]/path` into the
/// percent-decoded path. Returns `None` for bodies that aren't a
/// recognized file URI or that fail percent-decoding.
fn parse_osc7(body: &str) -> Option<String> {
    let rest = body.strip_prefix("file://")?;
    let slash = rest.find('/')?;
    let path = &rest[slash..];
    percent_decode(path)
}

/// Percent-decode a path. Returns `None` on malformed encoding
/// (trailing `%`, `%ZZ`).
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = hex_digit(bytes[i + 1])?;
            let lo = hex_digit(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(bytes: &[u8]) -> Vec<OscEvent> {
        let mut s = OscScanner::new();
        s.feed(bytes)
    }

    // OSC 9 (iTerm2 notification)

    #[test]
    fn osc9_bel_terminator() {
        let events = feed_all(b"\x1b]9;hello\x07");
        assert_eq!(
            events,
            vec![OscEvent::Notification {
                title: "hello".into(),
                body: String::new(),
            }]
        );
    }

    #[test]
    fn osc9_st_terminator() {
        let events = feed_all(b"\x1b]9;hello\x1b\\");
        assert_eq!(
            events,
            vec![OscEvent::Notification {
                title: "hello".into(),
                body: String::new(),
            }]
        );
    }

    #[test]
    fn osc9_split_across_feeds() {
        let mut s = OscScanner::new();
        let a = s.feed(b"\x1b]9;hel");
        assert!(a.is_empty());
        let b = s.feed(b"lo\x07");
        assert_eq!(
            b,
            vec![OscEvent::Notification {
                title: "hello".into(),
                body: String::new(),
            }]
        );
    }

    #[test]
    fn osc9_conemu_dropped() {
        // ConEmu sub-commands are 1..12. Bare "1" dropped.
        let events = feed_all(b"\x1b]9;1\x07");
        assert!(events.is_empty());
        // With trailing ; also dropped.
        let events = feed_all(b"\x1b]9;5;sleeping\x07");
        assert!(events.is_empty());
    }

    #[test]
    fn osc9_iterm_numeric_outside_conemu_range() {
        // Outside 1..12 — treated as iTerm2 notification with
        // numeric title.
        let events = feed_all(b"\x1b]9;42\x07");
        assert_eq!(
            events,
            vec![OscEvent::Notification {
                title: "42".into(),
                body: String::new(),
            }]
        );
    }

    #[test]
    fn osc9_iterm_starts_with_digit_then_text() {
        // `"1 file changed"` passes through (digit then space).
        let events = feed_all(b"\x1b]9;1 file changed\x07");
        assert_eq!(
            events,
            vec![OscEvent::Notification {
                title: "1 file changed".into(),
                body: String::new(),
            }]
        );
    }

    // OSC 777 (Konsole notify)

    #[test]
    fn osc777_with_body() {
        let events = feed_all(b"\x1b]777;notify;Build done;Tests passed\x07");
        assert_eq!(
            events,
            vec![OscEvent::Notification {
                title: "Build done".into(),
                body: "Tests passed".into(),
            }]
        );
    }

    #[test]
    fn osc777_without_body() {
        let events = feed_all(b"\x1b]777;notify;Just a title\x07");
        assert_eq!(
            events,
            vec![OscEvent::Notification {
                title: "Just a title".into(),
                body: String::new(),
            }]
        );
    }

    #[test]
    fn osc777_non_notify_dropped() {
        // OSC 777 with a non-`notify` opcode (Konsole has others)
        // shouldn't produce a notification event.
        let events = feed_all(b"\x1b]777;set-color;1;ff0000\x07");
        assert!(events.is_empty());
    }

    // OSC 7 (cwd)

    #[test]
    fn osc7_simple_path() {
        let events = feed_all(b"\x1b]7;file:///Users/me/work\x07");
        assert_eq!(events, vec![OscEvent::Pwd("/Users/me/work".into())]);
    }

    #[test]
    fn osc7_with_host_ignored() {
        let events = feed_all(b"\x1b]7;file://myhost/Users/me/work\x07");
        assert_eq!(events, vec![OscEvent::Pwd("/Users/me/work".into())]);
    }

    #[test]
    fn osc7_percent_decoded() {
        let events = feed_all(b"\x1b]7;file:///Users/me/spaces%20here\x07");
        assert_eq!(events, vec![OscEvent::Pwd("/Users/me/spaces here".into())]);
    }

    #[test]
    fn osc7_malformed_percent_dropped() {
        let events = feed_all(b"\x1b]7;file:///bad%ZZ\x07");
        assert!(events.is_empty());
    }

    #[test]
    fn osc7_trailing_percent_dropped() {
        let events = feed_all(b"\x1b]7;file:///bad%\x07");
        assert!(events.is_empty());
    }

    #[test]
    fn osc7_non_file_uri_dropped() {
        let events = feed_all(b"\x1b]7;ssh://elsewhere/path\x07");
        assert!(events.is_empty());
    }

    // OSC 0/1/2 (title)

    #[test]
    fn osc0_title() {
        let events = feed_all(b"\x1b]0;my window title\x07");
        assert_eq!(events, vec![OscEvent::Title("my window title".into())]);
    }

    #[test]
    fn osc1_title() {
        let events = feed_all(b"\x1b]1;icon\x07");
        assert_eq!(events, vec![OscEvent::Title("icon".into())]);
    }

    #[test]
    fn osc2_title() {
        let events = feed_all(b"\x1b]2;window-only title\x07");
        assert_eq!(events, vec![OscEvent::Title("window-only title".into())]);
    }

    #[test]
    fn empty_title_dropped() {
        let events = feed_all(b"\x1b]0;\x07");
        assert!(events.is_empty());
    }

    // OSC 10/11/12 (color queries)

    #[test]
    fn osc10_query_emits() {
        let events = feed_all(b"\x1b]10;?\x07");
        assert_eq!(events, vec![OscEvent::ColorQuery(10)]);
    }

    #[test]
    fn osc11_query_emits() {
        let events = feed_all(b"\x1b]11;?\x07");
        assert_eq!(events, vec![OscEvent::ColorQuery(11)]);
    }

    #[test]
    fn osc10_set_dropped() {
        // Set-color body shouldn't emit (libghostty handles).
        let events = feed_all(b"\x1b]10;rgb:00/00/00\x07");
        assert!(events.is_empty());
    }

    // Multiple sequences

    #[test]
    fn back_to_back_sequences() {
        let events = feed_all(b"\x1b]0;t1\x07\x1b]7;file:///a\x07\x1b]9;notif\x07");
        assert_eq!(
            events,
            vec![
                OscEvent::Title("t1".into()),
                OscEvent::Pwd("/a".into()),
                OscEvent::Notification {
                    title: "notif".into(),
                    body: String::new(),
                },
            ]
        );
    }

    #[test]
    fn malformed_st_recovers_following_osc() {
        // ESC followed by non-\ aborts the sequence in flight,
        // but should re-feed the byte so a subsequent OSC ESC
        // starts a NEW sequence cleanly. The Go scanner has the
        // same behaviour — preserves the back-to-back OSC case.
        let mut s = OscScanner::new();
        // Start an OSC, abort it with ESC + bogus byte, then
        // start a fresh OSC.
        let events = s.feed(b"\x1b]9;abc\x1bX\x1b]7;file:///b\x07");
        // Aborted sequence drops; the second OSC parses cleanly.
        assert_eq!(events, vec![OscEvent::Pwd("/b".into())]);
    }

    #[test]
    fn body_truncates_at_max() {
        // A 10KB body should truncate at MAX_BODY (8KB).
        let mut payload = Vec::with_capacity(20_000);
        payload.extend_from_slice(b"\x1b]0;");
        payload.extend(std::iter::repeat(b'A').take(10_000));
        payload.push(0x07);
        let events = feed_all(&payload);
        assert_eq!(events.len(), 1);
        if let OscEvent::Title(t) = &events[0] {
            assert_eq!(t.len(), MAX_BODY);
        } else {
            panic!("expected Title event");
        }
    }

    #[test]
    fn unrelated_bytes_pass_through() {
        // Non-OSC bytes should leave the scanner state at Outside
        // and emit nothing.
        let events = feed_all(b"some shell output\nmore output\n");
        assert!(events.is_empty());
    }
}
