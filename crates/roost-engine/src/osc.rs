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
        let mut actions = Vec::new();
        for event in self.scanner.feed(bytes) {
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
                    if let Some(reply) = color
                        .and_then(|color| roost_osc::format_color_query_response(number, color))
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
            }
        }
        actions
    }
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
