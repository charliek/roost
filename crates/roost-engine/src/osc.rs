//! Toolkit-neutral OSC routing for one terminal session.
//!
//! A UI adapter owns one [`OscRouter`] beside each terminal parser. It feeds
//! PTY output bytes and an owned snapshot of the terminal's live colors, then
//! executes the returned [`OscAction`]s after the router call returns. This
//! keeps streaming parse state and action ordering shared without giving the
//! engine renderer, clipboard, or toolkit callbacks.

pub use roost_osc::ClipboardTarget;
use roost_osc::{OscEvent, OscScanner};

/// Renderer-independent 8-bit RGB color.
pub type OscRgb = (u8, u8, u8);

/// Colors that are effective immediately before a chunk is applied to the
/// terminal parser. Keeping this DTO independent of `roost-vt` lets a future
/// Swift adapter supply colors from its native render state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OscColorSnapshot {
    pub foreground: OscRgb,
    pub background: OscRgb,
    pub cursor: OscRgb,
    pub palette: Box<[OscRgb; 256]>,
}

impl OscColorSnapshot {
    pub fn new(
        foreground: OscRgb,
        background: OscRgb,
        cursor: OscRgb,
        palette: [OscRgb; 256],
    ) -> Self {
        Self {
            foreground,
            background,
            cursor,
            palette: Box::new(palette),
        }
    }
}

/// Which values a program has taken ownership of with an OSC set. A
/// theme change moves everything else; these keep the program's color
/// until it resets them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExplicitColors {
    foreground: bool,
    background: bool,
    cursor: bool,
    palette: Box<[bool; 256]>,
}

impl Default for ExplicitColors {
    fn default() -> Self {
        Self {
            foreground: false,
            background: false,
            cursor: false,
            palette: Box::new([false; 256]),
        }
    }
}

/// Color state owned by a scanner that answers queries *ahead* of the
/// terminal parser.
///
/// The UI-supplied snapshot [`OscRouter::feed`] takes is read off the
/// live terminal, which is only correct while the scanner and the
/// parser see the same bytes at the same time. iced's drain-side router
/// runs ahead of `vt_write` by design (that is the whole point — the
/// reply leaves before the UI's event loop turns), so it tracks the
/// colors itself: seeded from the theme at attach, moved by the OSC
/// SET/RESET events the scanner surfaces, re-seeded when the theme
/// changes. The terminal converges on the same values because it reads
/// the same byte stream in the same order.
///
/// **Why an explicit-set flag rather than "a theme wins".** libghostty
/// models each color as `{ override, default }` and answers with
/// `override orelse default`; the option a theme push writes is
/// `default`, and its palette's `changeDefault` preserves every entry a
/// program set. So an OSC override OUTLIVES a theme change in the
/// terminal — measured, not assumed:
/// `crates/roost-vt/tests/theme_vs_osc_override_test.rs`. Dropping
/// overrides on reseed would desync this state permanently whenever a
/// theme lands while SET-carrying chunks are still queued for
/// `vt_write`: the terminal would end up on the program's color and
/// this state on the theme's, with nothing to reconcile them.
///
/// The one deliberate divergence is the reset path — see
/// [`OscColorState::apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OscColorState {
    current: OscColorSnapshot,
    /// What a theme-less channel shows, and what OSC 110/111/112/104
    /// reset *to*: the last seeded theme.
    defaults: OscColorSnapshot,
    explicit: ExplicitColors,
}

impl OscColorState {
    /// Seed from the theme the tab launched with.
    pub fn new(seed: OscColorSnapshot) -> Self {
        Self {
            current: seed.clone(),
            defaults: seed,
            explicit: ExplicitColors::default(),
        }
    }

    /// Adopt a new theme: it becomes both the live value and the reset
    /// target for every channel and palette entry the program has NOT
    /// set itself. Program-set values are left alone, which is what the
    /// terminal does with them.
    pub fn reseed(&mut self, seed: OscColorSnapshot) {
        if !self.explicit.foreground {
            self.current.foreground = seed.foreground;
        }
        if !self.explicit.background {
            self.current.background = seed.background;
        }
        if !self.explicit.cursor {
            self.current.cursor = seed.cursor;
        }
        for index in 0..256 {
            if !self.explicit.palette[index] {
                self.current.palette[index] = seed.palette[index];
            }
        }
        self.defaults = seed;
    }

    /// The colors a query answers from.
    pub fn snapshot(&self) -> &OscColorSnapshot {
        &self.current
    }

    /// Apply one scanner event. Non-color events are ignored.
    ///
    /// A reset drops the value back to the current default AND clears
    /// the explicit flag, so a later theme owns the channel again. That
    /// is xterm's semantics and what libghostty's palette does
    /// (`OSC 104` unsets the entry's mask bit). It diverges from the
    /// PINNED libghostty for fg/bg/cursor only: that build's
    /// `DynamicRGB.reset()` assigns `override = default` instead of
    /// clearing it, so a reset channel there stops following later
    /// themes (pinned by
    /// `crates/roost-vt/tests/theme_vs_osc_override_test.rs`, which
    /// goes red when a SHA bump adopts upstream's fix). Observing the
    /// difference takes reset → theme change → color query on the same
    /// channel; we answer with the new theme, the pinned terminal
    /// renders the old one.
    fn apply(&mut self, event: &OscEvent) {
        match event {
            OscEvent::ColorSet { number, color } => match number {
                10 => self.set_foreground(*color),
                11 => self.set_background(*color),
                12 => self.set_cursor(*color),
                _ => {}
            },
            OscEvent::ColorReset(number) => match number {
                110 => {
                    self.current.foreground = self.defaults.foreground;
                    self.explicit.foreground = false;
                }
                111 => {
                    self.current.background = self.defaults.background;
                    self.explicit.background = false;
                }
                112 => {
                    self.current.cursor = self.defaults.cursor;
                    self.explicit.cursor = false;
                }
                _ => {}
            },
            OscEvent::PaletteSet(pairs) => {
                for (index, color) in pairs {
                    let index = usize::from(*index);
                    self.current.palette[index] = *color;
                    self.explicit.palette[index] = true;
                }
            }
            OscEvent::PaletteReset(indices) => {
                if indices.is_empty() {
                    self.current.palette.clone_from(&self.defaults.palette);
                    self.explicit.palette.fill(false);
                } else {
                    for index in indices {
                        let index = usize::from(*index);
                        self.current.palette[index] = self.defaults.palette[index];
                        self.explicit.palette[index] = false;
                    }
                }
            }
            _ => {}
        }
    }

    fn set_foreground(&mut self, color: OscRgb) {
        self.current.foreground = color;
        self.explicit.foreground = true;
    }

    fn set_background(&mut self, color: OscRgb) {
        self.current.background = color;
        self.explicit.background = true;
    }

    fn set_cursor(&mut self, color: OscRgb) {
        self.current.cursor = color;
        self.explicit.cursor = true;
    }
}

/// Whether an event moves [`OscColorState`] — the cheap test that keeps
/// the chunk-start snapshot clone off every ordinary chunk.
fn moves_color_state(event: &OscEvent) -> bool {
    matches!(
        event,
        OscEvent::ColorSet { .. }
            | OscEvent::ColorReset(_)
            | OscEvent::PaletteSet(_)
            | OscEvent::PaletteReset(_)
    )
}

/// Explicit effect produced by an OSC sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscAction {
    /// Apply an authoritative workspace transition through
    /// `LocalClient::apply_osc`.
    Workspace { command: u32, payload: String },
    /// Write a synthesized terminal query reply to the PTY input channel.
    PtyInput(Vec<u8>),
    /// UI port: write decoded text to the named native clipboard.
    ClipboardWrite {
        target: ClipboardTarget,
        text: String,
    },
    /// UI port: apply a W3C pointer name to the matching terminal surface.
    PointerShape(String),
}

/// Stateful OSC parser and pure event-to-action mapper for one terminal.
#[derive(Default)]
pub struct OscRouter {
    scanner: OscScanner,
}

impl OscRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume one PTY output chunk and return effects in wire order.
    ///
    /// `colors` describes the terminal immediately before `bytes` is applied.
    /// This intentionally preserves Roost's established behavior: a SET in an
    /// earlier chunk affects a later QUERY, while SET+QUERY in one chunk sees
    /// the pre-chunk value.
    pub fn feed(&mut self, bytes: &[u8], colors: &OscColorSnapshot) -> Vec<OscAction> {
        map_events(self.scanner.feed(bytes), colors)
    }

    /// Consume one PTY output chunk against router-owned color state.
    ///
    /// The same-chunk contract [`OscRouter::feed`] documents is
    /// preserved exactly, and here it is enforced rather than inherited
    /// from the caller's read of the terminal: `state` is snapshotted at
    /// chunk start, every query in the chunk answers from that
    /// snapshot, and the chunk's SETs land in `state` for the chunks
    /// that follow.
    pub fn feed_stateful(&mut self, bytes: &[u8], state: &mut OscColorState) -> Vec<OscAction> {
        let events = self.scanner.feed(bytes);
        if events.is_empty() {
            return Vec::new();
        }
        // Only a chunk that actually moves color state pays for the
        // snapshot clone; the overwhelming majority carry none.
        let pre_chunk = if events.iter().any(moves_color_state) {
            let pre_chunk = state.snapshot().clone();
            for event in &events {
                state.apply(event);
            }
            Some(pre_chunk)
        } else {
            None
        };
        map_events(events, pre_chunk.as_ref().unwrap_or(state.snapshot()))
    }
}

/// The pure event → action mapping both feed paths share.
fn map_events(events: Vec<OscEvent>, colors: &OscColorSnapshot) -> Vec<OscAction> {
    let mut actions = Vec::new();
    for event in events {
        match event {
            OscEvent::Title(title) => actions.push(OscAction::Workspace {
                command: 0,
                payload: title,
            }),
            OscEvent::Pwd(path) => actions.push(OscAction::Workspace {
                command: 7,
                payload: format!("file://{path}"),
            }),
            OscEvent::Notification { title, body } => {
                let (command, payload) = if body.is_empty() {
                    (9, title)
                } else {
                    (777, format!("notify;{title};{body}"))
                };
                actions.push(OscAction::Workspace { command, payload });
            }
            OscEvent::CommandMark(payload) => actions.push(OscAction::Workspace {
                command: 133,
                payload,
            }),
            OscEvent::ColorQuery(number) => {
                let color = match number {
                    10 => Some(colors.foreground),
                    11 => Some(colors.background),
                    12 => Some(colors.cursor),
                    _ => None,
                };
                if let Some(reply) =
                    color.and_then(|color| roost_osc::format_color_query_response(number, color))
                {
                    actions.push(OscAction::PtyInput(reply));
                }
            }
            OscEvent::PaletteQuery(indices) => {
                let mut reply = Vec::new();
                for index in indices {
                    reply.extend_from_slice(&roost_osc::format_palette_query_response(
                        index,
                        colors.palette[usize::from(index)],
                    ));
                }
                if !reply.is_empty() {
                    actions.push(OscAction::PtyInput(reply));
                }
            }
            OscEvent::Clipboard { target, text } => {
                actions.push(OscAction::ClipboardWrite { target, text });
            }
            OscEvent::MouseShape(name) => actions.push(OscAction::PointerShape(name)),
            // Color SETs and RESETs produce no action: libghostty
            // applies them to the terminal from the same bytes.
            // They exist for `OscColorState`, and a router fed
            // without one (the original, pre-`OscColorState` calling
            // convention — the now-removed GTK UI's only mode) must
            // behave exactly as it did before they were surfaced.
            OscEvent::ColorSet { .. }
            | OscEvent::ColorReset(_)
            | OscEvent::PaletteSet(_)
            | OscEvent::PaletteReset(_) => {}
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> OscColorSnapshot {
        let mut palette = [(0, 0, 0); 256];
        palette[5] = (0xde, 0xad, 0xbe);
        OscColorSnapshot::new(
            (0xaa, 0xbb, 0xcc),
            (0x00, 0x11, 0x22),
            (0x98, 0x98, 0x9d),
            palette,
        )
    }

    /// A second theme, distinct from [`colors`] in every channel and in
    /// the two palette entries the reseed tests read.
    fn theme_b() -> OscColorSnapshot {
        let mut palette = [(0, 0, 0); 256];
        palette[5] = (0x0a, 0x0b, 0x0c);
        palette[6] = (0x0d, 0x0e, 0x0f);
        OscColorSnapshot::new(
            (0x11, 0x11, 0x11),
            (0x22, 0x22, 0x22),
            (0x33, 0x33, 0x33),
            palette,
        )
    }

    #[test]
    fn split_sequences_keep_per_session_scan_state() {
        let mut router = OscRouter::new();
        assert!(router.feed(b"\x1b]7;file:///Us", &colors()).is_empty());
        assert_eq!(
            router.feed(b"ers/me\x07", &colors()),
            vec![OscAction::Workspace {
                command: 7,
                payload: "file:///Users/me".into(),
            }]
        );
    }

    #[test]
    fn actions_preserve_wire_order_and_separate_ui_ports() {
        let mut router = OscRouter::new();
        let bytes = b"\x1b]0;build\x07\x1b]133;C\x07\x1b]22;pointer\x07";
        assert_eq!(
            router.feed(bytes, &colors()),
            vec![
                OscAction::Workspace {
                    command: 0,
                    payload: "build".into(),
                },
                OscAction::Workspace {
                    command: 133,
                    payload: "C".into(),
                },
                OscAction::PointerShape("pointer".into()),
            ]
        );
    }

    #[test]
    fn color_and_palette_queries_use_supplied_live_snapshot() {
        let mut router = OscRouter::new();
        let actions = router.feed(b"\x1b]11;?\x07\x1b]4;5;?\x07", &colors());
        assert_eq!(actions.len(), 2);
        assert!(matches!(
            &actions[0],
            OscAction::PtyInput(bytes) if bytes.windows(14).any(|w| w == b"0000/1111/2222")
        ));
        assert!(matches!(
            &actions[1],
            OscAction::PtyInput(bytes) if bytes.windows(14).any(|w| w == b"dede/adad/bebe")
        ));
    }

    /// The no-`OscColorState` path: a router fed the caller's snapshot
    /// must produce exactly the actions it produced before SET/RESET
    /// events existed.
    #[test]
    fn caller_snapshot_feed_ignores_sets_and_resets() {
        let mut router = OscRouter::new();
        let actions = router.feed(
            b"\x1b]11;rgb:00/11/22\x07\x1b]4;5;rgb:11/22/33\x07\x1b]111\x07\x1b]104\x07",
            &colors(),
        );
        assert_eq!(actions, Vec::<OscAction>::new());
    }

    fn reply_color(action: &OscAction) -> String {
        match action {
            OscAction::PtyInput(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            other => panic!("expected a PTY reply, got {other:?}"),
        }
    }

    #[test]
    fn stateful_feed_answers_a_cross_chunk_set_with_the_new_color() {
        let mut router = OscRouter::new();
        let mut state = OscColorState::new(colors());
        assert!(router
            .feed_stateful(b"\x1b]11;rgb:aa/aa/aa\x07", &mut state)
            .is_empty());
        let actions = router.feed_stateful(b"\x1b]11;?\x07", &mut state);
        assert_eq!(actions.len(), 1);
        assert!(reply_color(&actions[0]).contains("aaaa/aaaa/aaaa"));
    }

    /// The pinned same-chunk contract: a SET and a QUERY in one chunk
    /// answer from the chunk-start snapshot, and the SET applies to
    /// everything after.
    #[test]
    fn stateful_feed_answers_a_same_chunk_set_with_the_pre_chunk_color() {
        let mut router = OscRouter::new();
        let mut state = OscColorState::new(colors());
        let actions = router.feed_stateful(b"\x1b]11;rgb:aa/aa/aa\x07\x1b]11;?\x07", &mut state);
        assert_eq!(actions.len(), 1);
        let reply = reply_color(&actions[0]);
        assert!(reply.contains("0000/1111/2222"), "{reply}");
        assert!(!reply.contains("aaaa/aaaa/aaaa"), "{reply}");

        // …and the chunk after it sees the SET.
        let actions = router.feed_stateful(b"\x1b]11;?\x07", &mut state);
        assert!(reply_color(&actions[0]).contains("aaaa/aaaa/aaaa"));
    }

    #[test]
    fn stateful_feed_tracks_the_palette_and_its_resets() {
        let mut router = OscRouter::new();
        let mut state = OscColorState::new(colors());
        router.feed_stateful(b"\x1b]4;5;rgb:11/22/33\x07", &mut state);
        let actions = router.feed_stateful(b"\x1b]4;5;?\x07", &mut state);
        assert!(reply_color(&actions[0]).contains("1111/2222/3333"));

        router.feed_stateful(b"\x1b]104;5\x07", &mut state);
        let actions = router.feed_stateful(b"\x1b]4;5;?\x07", &mut state);
        assert!(
            reply_color(&actions[0]).contains("dede/adad/bebe"),
            "OSC 104 must restore the seeded palette entry"
        );
    }

    #[test]
    fn stateful_feed_resets_each_color_channel_to_the_seed() {
        let mut router = OscRouter::new();
        let mut state = OscColorState::new(colors());
        router.feed_stateful(
            b"\x1b]10;rgb:01/02/03\x07\x1b]11;rgb:04/05/06\x07\x1b]12;rgb:07/08/09\x07",
            &mut state,
        );
        router.feed_stateful(b"\x1b]110\x07\x1b]111\x07\x1b]112\x07", &mut state);
        let actions = router.feed_stateful(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07", &mut state);
        assert_eq!(actions.len(), 3);
        assert!(reply_color(&actions[0]).contains("aaaa/bbbb/cccc"));
        assert!(reply_color(&actions[1]).contains("0000/1111/2222"));
        assert!(reply_color(&actions[2]).contains("9898/9898/9d9d"));
    }

    #[test]
    fn a_malformed_set_leaves_the_state_untouched() {
        let mut router = OscRouter::new();
        let mut state = OscColorState::new(colors());
        router.feed_stateful(b"\x1b]11;not-a-color\x07", &mut state);
        let actions = router.feed_stateful(b"\x1b]11;?\x07", &mut state);
        assert!(reply_color(&actions[0]).contains("0000/1111/2222"));
    }

    /// A theme change replaces the drain-local state outright: the next
    /// query answers the new theme, and so does a later OSC reset.
    #[test]
    fn reseeding_moves_every_channel_the_program_has_not_claimed() {
        let mut router = OscRouter::new();
        let mut state = OscColorState::new(colors());
        state.reseed(theme_b());

        let actions = router.feed_stateful(
            b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07\x1b]4;5;?\x07",
            &mut state,
        );
        assert!(reply_color(&actions[0]).contains("1111/1111/1111"));
        assert!(reply_color(&actions[1]).contains("2222/2222/2222"));
        assert!(reply_color(&actions[2]).contains("3333/3333/3333"));
        assert!(reply_color(&actions[3]).contains("0a0a/0b0b/0c0c"));
    }

    /// The desync this flag exists for: a theme lands while a SET the
    /// terminal WILL apply is still queued for `vt_write`. The terminal
    /// keeps the program's color (libghostty's `override orelse
    /// default`, pinned in
    /// `crates/roost-vt/tests/theme_vs_osc_override_test.rs`), so this
    /// state has to as well — in both orders, because the race can land
    /// either way.
    #[test]
    fn a_program_set_color_outlives_a_reseed_in_either_order() {
        // SET, then the theme change.
        let mut router = OscRouter::new();
        let mut state = OscColorState::new(colors());
        router.feed_stateful(b"\x1b]11;rgb:ab/cd/ef\x07", &mut state);
        state.reseed(theme_b());
        let actions = router.feed_stateful(b"\x1b]11;?\x07", &mut state);
        assert!(
            reply_color(&actions[0]).contains("abab/cdcd/efef"),
            "the theme must not clobber a color the program set"
        );

        // The theme change, then the SET.
        let mut router = OscRouter::new();
        let mut state = OscColorState::new(colors());
        state.reseed(theme_b());
        router.feed_stateful(b"\x1b]11;rgb:ab/cd/ef\x07", &mut state);
        let actions = router.feed_stateful(b"\x1b]11;?\x07", &mut state);
        assert!(reply_color(&actions[0]).contains("abab/cdcd/efef"));
    }

    #[test]
    fn a_reset_hands_the_channel_back_to_the_theme() {
        let mut router = OscRouter::new();
        let mut state = OscColorState::new(colors());
        router.feed_stateful(b"\x1b]11;rgb:ab/cd/ef\x07\x1b]111\x07", &mut state);
        // Back to the seed it was reset against…
        let actions = router.feed_stateful(b"\x1b]11;?\x07", &mut state);
        assert!(reply_color(&actions[0]).contains("0000/1111/2222"));
        // …and the next theme owns it again.
        state.reseed(theme_b());
        let actions = router.feed_stateful(b"\x1b]11;?\x07", &mut state);
        assert!(
            reply_color(&actions[0]).contains("2222/2222/2222"),
            "the reset cleared the program's claim, so the reseed applies"
        );
    }

    #[test]
    fn a_program_set_palette_entry_outlives_a_reseed_until_it_is_reset() {
        let mut router = OscRouter::new();
        let mut state = OscColorState::new(colors());
        router.feed_stateful(b"\x1b]4;5;rgb:ab/cd/ef\x07", &mut state);
        state.reseed(theme_b());

        let actions = router.feed_stateful(b"\x1b]4;5;?\x07\x1b]4;6;?\x07", &mut state);
        assert!(
            reply_color(&actions[0]).contains("abab/cdcd/efef"),
            "entry 5 was claimed by the program"
        );
        assert!(
            reply_color(&actions[1]).contains("0d0d/0e0e/0f0f"),
            "entry 6 was not, so the new theme owns it"
        );

        // OSC 104 releases the claim; the CURRENT theme takes over, and
        // so does the next one.
        router.feed_stateful(b"\x1b]104;5\x07", &mut state);
        let actions = router.feed_stateful(b"\x1b]4;5;?\x07", &mut state);
        assert!(reply_color(&actions[0]).contains("0a0a/0b0b/0c0c"));

        let mut third = theme_b();
        third.palette[5] = (0x77, 0x77, 0x77);
        state.reseed(third);
        let actions = router.feed_stateful(b"\x1b]4;5;?\x07", &mut state);
        assert!(reply_color(&actions[0]).contains("7777/7777/7777"));
    }

    #[test]
    fn a_bare_osc_104_releases_every_palette_claim() {
        let mut router = OscRouter::new();
        let mut state = OscColorState::new(colors());
        router.feed_stateful(b"\x1b]4;5;rgb:ab/cd/ef;6;rgb:12/34/56\x07", &mut state);
        router.feed_stateful(b"\x1b]104\x07", &mut state);
        state.reseed(theme_b());

        let actions = router.feed_stateful(b"\x1b]4;5;?\x07\x1b]4;6;?\x07", &mut state);
        assert!(reply_color(&actions[0]).contains("0a0a/0b0b/0c0c"));
        assert!(reply_color(&actions[1]).contains("0d0d/0e0e/0f0f"));
    }

    #[test]
    fn notification_and_clipboard_payloads_are_explicit_actions() {
        let mut router = OscRouter::new();
        let actions = router.feed(
            b"\x1b]777;notify;Build;Passed\x07\x1b]52;c;aGVsbG8=\x07",
            &colors(),
        );
        assert_eq!(
            actions,
            vec![
                OscAction::Workspace {
                    command: 777,
                    payload: "notify;Build;Passed".into(),
                },
                OscAction::ClipboardWrite {
                    target: ClipboardTarget::System,
                    text: "hello".into(),
                },
            ]
        );
    }
}
