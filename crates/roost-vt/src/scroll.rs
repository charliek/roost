//! Toolkit-neutral terminal wheel policy.
//!
//! Native adapters convert their event units into `history_rows`: positive
//! moves toward older history, negative moves toward the live bottom. This
//! module owns fractional accumulation, terminal-mode precedence, local
//! viewport movement, and the next-keystroke snap state. It deliberately does
//! not own native events, pointer geometry, encoders, PTY writes, or callbacks.

use crate::{ActiveScreen, ScrollViewport, Terminal};

const MAX_ROWS_PER_EVENT: f64 = 1_000.0;

/// Direction of a whole-row wheel action after native-unit normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    /// Toward older scrollback (wheel up).
    History,
    /// Toward the live bottom (wheel down).
    Bottom,
}

/// Explicit adapter work selected by [`TerminalScroll::route`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollRoute {
    /// Encode one terminal mouse report per row: button 4 for `History`,
    /// button 5 for `Bottom`.
    MouseReport {
        direction: ScrollDirection,
        rows: usize,
    },
    /// Encode one arrow press per row: Up for `History`, Down for `Bottom`.
    AlternateScreenKey {
        direction: ScrollDirection,
        rows: usize,
    },
    /// The shared policy moved libghostty's local viewport itself.
    LocalViewport { scrolled_back: bool },
}

/// Direction of a page-wise viewport action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageDirection {
    /// Toward older scrollback (Page Up).
    Up,
    /// Toward the live bottom (Page Down).
    Down,
}

/// Explicit adapter work selected by [`TerminalScroll::route_page`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageRoute {
    /// Encode the page key itself and send it to the application. Unlike the
    /// wheel there is no arrow synthesis: Page Up/Down are real keys the
    /// running application already understands.
    Forward,
    /// The shared policy moved libghostty's local viewport itself.
    LocalViewport { scrolled_back: bool },
}

/// Per-terminal wheel accumulator and snap-to-bottom state.
#[derive(Debug, Default)]
pub struct TerminalScroll {
    fractional_history_rows: f64,
    last_direction: i8,
    scrolled_back: bool,
}

impl TerminalScroll {
    pub fn new() -> Self {
        Self::default()
    }

    /// Route a native wheel delta already normalized into terminal rows.
    ///
    /// Positive values move toward older history. Direction changes discard
    /// stale fractional momentum. Extremely large native deltas are bounded so
    /// a malformed event cannot request an unbounded encoder loop.
    pub fn route(&mut self, terminal: &mut Terminal, history_rows: f64) -> Option<ScrollRoute> {
        if !history_rows.is_finite() || history_rows == 0.0 {
            return None;
        }
        let direction = if history_rows > 0.0 { 1 } else { -1 };
        if self.last_direction != 0 && self.last_direction != direction {
            self.fractional_history_rows = 0.0;
        }
        self.last_direction = direction;
        self.fractional_history_rows = (self.fractional_history_rows + history_rows)
            .clamp(-MAX_ROWS_PER_EVENT, MAX_ROWS_PER_EVENT);
        let whole_rows = self.fractional_history_rows.trunc() as isize;
        if whole_rows == 0 {
            return None;
        }
        self.fractional_history_rows -= whole_rows as f64;

        let direction = if whole_rows > 0 {
            ScrollDirection::History
        } else {
            ScrollDirection::Bottom
        };
        let rows = whole_rows.unsigned_abs();
        if terminal.mouse_tracking() {
            return Some(ScrollRoute::MouseReport { direction, rows });
        }
        if terminal.active_screen() == ActiveScreen::Alternate {
            return Some(ScrollRoute::AlternateScreenKey { direction, rows });
        }

        Some(ScrollRoute::LocalViewport {
            scrolled_back: self.move_local_viewport(terminal, whole_rows),
        })
    }

    /// Route a page-wise viewport request (Page Up / Page Down).
    ///
    /// Precedence matches [`TerminalScroll::route`], but forwarding hands the
    /// real page key to the application instead of synthesizing arrows. A local
    /// page moves exactly one full viewport with no overlap and discards any
    /// accumulated wheel fraction, which a page jump invalidates.
    pub fn route_page(
        &mut self,
        terminal: &mut Terminal,
        direction: PageDirection,
        viewport_rows: usize,
    ) -> PageRoute {
        if terminal.mouse_tracking() {
            return PageRoute::Forward;
        }
        if terminal.active_screen() == ActiveScreen::Alternate {
            return PageRoute::Forward;
        }

        let rows = isize::try_from(viewport_rows.max(1)).unwrap_or(isize::MAX);
        let history_rows = match direction {
            PageDirection::Up => rows,
            PageDirection::Down => -rows,
        };
        self.fractional_history_rows = 0.0;
        self.last_direction = 0;
        PageRoute::LocalViewport {
            scrolled_back: self.move_local_viewport(terminal, history_rows),
        }
    }

    /// Move the local viewport by whole rows (positive = toward older history)
    /// and refresh the snap state from the authoritative scrollbar.
    fn move_local_viewport(&mut self, terminal: &mut Terminal, history_rows: isize) -> bool {
        // libghostty's delta is negative toward older history, the inverse of
        // this API's positive-history convention.
        terminal.scroll_viewport(ScrollViewport::Delta(-history_rows));
        match terminal.scrollbar() {
            Ok(scrollbar) => self.scrolled_back = !scrollbar.is_at_bottom(),
            Err(_) if history_rows > 0 => self.scrolled_back = true,
            Err(_) => {
                // A failed authoritative query after a downward move must not
                // guess that bottom was reached. Preserve the prior state so
                // the next real terminal key still snaps safely.
            }
        }
        self.scrolled_back
    }

    /// Snap a locally scrolled terminal to the live bottom immediately before
    /// a real terminal keystroke. Returns whether a move occurred so adapters
    /// can schedule a repaint.
    pub fn snap_to_bottom(&mut self, terminal: &mut Terminal) -> bool {
        if !self.scrolled_back {
            return false;
        }
        terminal.scroll_viewport(ScrollViewport::Bottom);
        self.fractional_history_rows = 0.0;
        self.last_direction = 0;
        self.scrolled_back = false;
        true
    }

    pub fn is_scrolled_back(&self) -> bool {
        self.scrolled_back
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TerminalOptions;

    fn terminal() -> Terminal {
        Terminal::new(TerminalOptions {
            cols: 8,
            rows: 2,
            max_scrollback: 20,
            ..Default::default()
        })
        .expect("terminal")
    }

    fn terminal_with_history() -> Terminal {
        let mut terminal = terminal();
        terminal.vt_write(b"old\r\nline\r\nnew\r\nlive");
        assert!(terminal.scrollbar().expect("scrollbar").is_at_bottom());
        terminal
    }

    fn terminal_with_paged_history() -> Terminal {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 8,
            rows: 4,
            max_scrollback: 64,
            ..Default::default()
        })
        .expect("terminal");
        for index in 0..16 {
            terminal.vt_write(format!("line{index}\r\n").as_bytes());
        }
        assert!(terminal.scrollbar().expect("scrollbar").is_at_bottom());
        terminal
    }

    fn viewport_offset(terminal: &Terminal) -> u64 {
        terminal.scrollbar().expect("scrollbar").offset
    }

    #[test]
    fn fractions_accumulate_and_direction_change_discards_momentum() {
        let mut terminal = terminal_with_history();
        let mut scroll = TerminalScroll::new();
        assert_eq!(scroll.route(&mut terminal, 0.75), None);
        assert_eq!(scroll.route(&mut terminal, -0.5), None);
        assert_eq!(
            scroll.route(&mut terminal, -0.5),
            Some(ScrollRoute::LocalViewport {
                scrolled_back: false
            })
        );
    }

    #[test]
    fn local_history_uses_authoritative_bottom_state() {
        let mut terminal = terminal_with_history();
        let mut scroll = TerminalScroll::new();
        assert_eq!(
            scroll.route(&mut terminal, 2.0),
            Some(ScrollRoute::LocalViewport {
                scrolled_back: true
            })
        );
        assert!(scroll.is_scrolled_back());
        assert_eq!(
            scroll.route(&mut terminal, -1.0),
            Some(ScrollRoute::LocalViewport {
                scrolled_back: true
            }),
            "partial movement toward bottom must remain scrolled"
        );
        assert!(scroll.snap_to_bottom(&mut terminal));
        assert!(!scroll.is_scrolled_back());
        assert!(terminal.scrollbar().expect("scrollbar").is_at_bottom());
        assert!(!scroll.snap_to_bottom(&mut terminal));
    }

    #[test]
    fn scroll_state_is_isolated_per_terminal() {
        let mut first_terminal = terminal_with_history();
        let mut second_terminal = terminal_with_history();
        let mut first = TerminalScroll::new();
        let mut second = TerminalScroll::new();
        assert!(matches!(
            first.route(&mut first_terminal, 1.0),
            Some(ScrollRoute::LocalViewport {
                scrolled_back: true
            })
        ));
        assert!(first.is_scrolled_back());
        assert!(!second.is_scrolled_back());
        assert!(!second.snap_to_bottom(&mut second_terminal));
        assert!(first.is_scrolled_back());
    }

    #[test]
    fn mouse_tracking_precedes_alternate_screen() {
        let mut terminal = terminal();
        terminal.vt_write(b"\x1b[?1049h\x1b[?1000h");
        let mut scroll = TerminalScroll::new();
        assert_eq!(
            scroll.route(&mut terminal, 3.0),
            Some(ScrollRoute::MouseReport {
                direction: ScrollDirection::History,
                rows: 3,
            })
        );
    }

    #[test]
    fn alternate_screen_routes_to_directional_keys() {
        let mut terminal = terminal();
        terminal.vt_write(b"\x1b[?1049h");
        let mut scroll = TerminalScroll::new();
        assert_eq!(
            scroll.route(&mut terminal, -2.0),
            Some(ScrollRoute::AlternateScreenKey {
                direction: ScrollDirection::Bottom,
                rows: 2,
            })
        );
    }

    #[test]
    fn invalid_and_zero_deltas_are_ignored() {
        let mut terminal = terminal();
        let mut scroll = TerminalScroll::new();
        for delta in [0.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(scroll.route(&mut terminal, delta), None);
        }
    }

    #[test]
    fn page_mouse_tracking_precedes_alternate_screen() {
        let mut terminal = terminal();
        terminal.vt_write(b"\x1b[?1049h\x1b[?1000h");
        let mut scroll = TerminalScroll::new();
        assert_eq!(
            scroll.route_page(&mut terminal, PageDirection::Up, 4),
            PageRoute::Forward
        );
    }

    #[test]
    fn page_on_alternate_screen_forwards_the_real_key() {
        let mut terminal = terminal();
        terminal.vt_write(b"\x1b[?1049h");
        let mut scroll = TerminalScroll::new();
        assert_eq!(
            scroll.route_page(&mut terminal, PageDirection::Down, 4),
            PageRoute::Forward
        );
        assert!(!scroll.is_scrolled_back());
    }

    #[test]
    fn pages_walk_local_history_and_clamp_at_both_ends() {
        let mut terminal = terminal_with_paged_history();
        let mut scroll = TerminalScroll::new();
        let bottom = viewport_offset(&terminal);
        assert_eq!(
            scroll.route_page(&mut terminal, PageDirection::Up, 4),
            PageRoute::LocalViewport {
                scrolled_back: true
            }
        );
        let first = viewport_offset(&terminal);
        assert_eq!(bottom - first, 4);
        assert_eq!(
            scroll.route_page(&mut terminal, PageDirection::Up, 4),
            PageRoute::LocalViewport {
                scrolled_back: true
            }
        );
        let second = viewport_offset(&terminal);
        assert_eq!(first - second, 4);

        for _ in 0..8 {
            scroll.route_page(&mut terminal, PageDirection::Up, 4);
        }
        assert_eq!(viewport_offset(&terminal), 0, "page up clamps at the top");

        assert_eq!(
            scroll.route_page(&mut terminal, PageDirection::Down, 4),
            PageRoute::LocalViewport {
                scrolled_back: true
            }
        );
        assert_eq!(viewport_offset(&terminal), 4);
    }

    #[test]
    fn page_down_at_bottom_stays_at_bottom() {
        let mut terminal = terminal_with_paged_history();
        let mut scroll = TerminalScroll::new();
        let bottom = viewport_offset(&terminal);
        assert_eq!(
            scroll.route_page(&mut terminal, PageDirection::Down, 4),
            PageRoute::LocalViewport {
                scrolled_back: false
            }
        );
        assert_eq!(viewport_offset(&terminal), bottom);
        assert!(!scroll.is_scrolled_back());
    }

    #[test]
    fn page_size_is_the_viewport_and_zero_rows_moves_one() {
        let mut terminal = terminal_with_paged_history();
        let mut scroll = TerminalScroll::new();
        let bottom = viewport_offset(&terminal);
        scroll.route_page(&mut terminal, PageDirection::Up, 3);
        assert_eq!(bottom - viewport_offset(&terminal), 3);
        scroll.route_page(&mut terminal, PageDirection::Up, 0);
        assert_eq!(bottom - viewport_offset(&terminal), 4);
    }

    #[test]
    fn page_discards_stale_wheel_momentum() {
        let mut terminal = terminal_with_paged_history();
        let mut scroll = TerminalScroll::new();
        assert_eq!(scroll.route(&mut terminal, 0.5), None);
        assert_eq!(
            scroll.route_page(&mut terminal, PageDirection::Up, 4),
            PageRoute::LocalViewport {
                scrolled_back: true
            }
        );
        assert_eq!(
            scroll.route(&mut terminal, 0.5),
            None,
            "the page must discard the fraction accumulated before it"
        );
    }

    #[test]
    fn page_uses_authoritative_bottom_state_for_snap() {
        let mut terminal = terminal_with_paged_history();
        let mut scroll = TerminalScroll::new();
        assert_eq!(
            scroll.route_page(&mut terminal, PageDirection::Up, 4),
            PageRoute::LocalViewport {
                scrolled_back: true
            }
        );
        assert!(scroll.is_scrolled_back());
        assert!(scroll.snap_to_bottom(&mut terminal));
        assert!(!scroll.is_scrolled_back());
        assert!(terminal.scrollbar().expect("scrollbar").is_at_bottom());
        assert!(!scroll.snap_to_bottom(&mut terminal));
    }
}
