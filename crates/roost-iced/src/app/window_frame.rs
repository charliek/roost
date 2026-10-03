//! Remembering the window frame across launches — macOS only, after
//! Ghostty's `LastWindowPosition` (plan 074 §D5b).
//!
//! The frame is kept in iced's own logical points: the content size
//! `window::Settings::size` means, and the outer top-left that
//! `window::Event::Moved` reports and `Position::Specific` places on
//! macOS. So the window opens where it was, with no jump. AppKit's
//! coordinates appear only in [`fit_on_screens`] and [`iced_frame`],
//! which `macos::window_frame` calls once the window exists, to bring a
//! frame saved on a since-disconnected display back onto a screen.

use std::time::Duration;

use iced::{window, Point, Size};
use roost_engine::persistence::WindowFrame;
use roost_engine::Workspace;

use super::{TrailingDebounce, INITIAL_WINDOW_SIZE, MIN_WINDOW_SIZE};

/// Ghostty's GTK build doesn't remember the frame, and Wayland can
/// neither read nor set a window's position, so Linux never reads or
/// writes it.
pub(crate) const REMEMBERS_WINDOW_FRAME: bool = cfg!(target_os = "macos");

/// How long after the last resize or move the frame is written.
pub(super) const SAVE_DELAY: Duration = Duration::from_millis(500);

/// What the window is created at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct OpeningFrame {
    pub(crate) size: Size,
    /// `None` leaves the placement to the platform.
    pub(crate) position: Option<Point>,
}

impl OpeningFrame {
    /// The saved frame, if this build remembers one, it is
    /// [`WindowFrame::is_valid`], and its position survives the trip into
    /// iced's `f32`s; otherwise the defaults. The size is clamped up only
    /// to the window's minimum — whether it fits a screen is the post-open
    /// check's question, which needs `NSScreen`.
    pub(crate) fn from_saved(saved: Option<WindowFrame>, remembers: bool) -> Self {
        let defaults = Self {
            size: INITIAL_WINDOW_SIZE,
            position: None,
        };
        let Some(frame) = saved.filter(|frame| remembers && frame.is_valid()) else {
            return defaults;
        };
        let size = Size::new(frame.content_width as f32, frame.content_height as f32);
        let position = Point::new(frame.outer_x as f32, frame.outer_y as f32);
        if !(position.x.is_finite() && position.y.is_finite()) {
            return defaults;
        }
        Self {
            size: Size::new(
                size.width.max(MIN_WINDOW_SIZE.width),
                size.height.max(MIN_WINDOW_SIZE.height),
            ),
            position: Some(position),
        }
    }
}

/// What the post-open screen check reports: the window's frame as it now
/// stands, and whether the check had to move it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CheckedFrame {
    pub(crate) frame: WindowFrame,
    pub(crate) adjusted: bool,
}

/// The running window's frame, and whether `state.json` is owed it.
#[derive(Debug)]
pub(super) struct WindowFrameMemory {
    remembers: bool,
    opening: OpeningFrame,
    check_requested: bool,
    /// Whether the post-open screen check has answered. Until it has, a
    /// `Moved` is the window being placed, not the user moving it.
    checked: bool,
    /// The content size the latest native resize reported. Those events,
    /// not the screen check's look, are what the size follows: a resize
    /// can be applied after the check looked and still reach `update`
    /// before its answer does.
    content: Size,
    outer: Option<Point>,
    /// The window's mode as of its latest query: `Windowed` is visible
    /// and not full screen.
    mode: window::Mode,
    /// A resize or move `state.json` doesn't have yet. A full-screen
    /// answer clears it: a resize can land before the query that says it
    /// was a full-screen transition's.
    owed: bool,
    /// Roost's own full-screen toggle ran and its transition has not
    /// settled: what a resize records now may be transition geometry the
    /// mode queries have not caught up with.
    toggling: bool,
    debounce: TrailingDebounce,
}

impl WindowFrameMemory {
    pub(super) fn new(remembers: bool, opening: OpeningFrame) -> Self {
        Self {
            remembers,
            opening,
            check_requested: false,
            checked: false,
            content: opening.size,
            outer: opening.position,
            mode: window::Mode::Windowed,
            owed: false,
            toggling: false,
            debounce: TrailingDebounce::default(),
        }
    }

    pub(super) fn opening(&self) -> OpeningFrame {
        self.opening
    }

    /// `Some` exactly once, for the first window open, when this build
    /// remembers the frame: whether the check may move the window. Only a
    /// frame `state.json` placed is fitted onto a screen — the platform's
    /// own placement of a fresh window is left as it always was.
    pub(super) fn take_check(&mut self) -> Option<bool> {
        let take = self.remembers && !self.check_requested;
        self.check_requested = true;
        take.then_some(self.opening.position.is_some())
    }

    /// The screen check answered. `None` means it could not read the
    /// window, which still ends the placement: later moves are the user's.
    /// A frame the check had to move is owed to `state.json`, so it arms
    /// a save; the returned generation is that arm.
    pub(super) fn checked(&mut self, observed: Option<CheckedFrame>) -> Option<u64> {
        self.checked = true;
        let observed = observed?;
        self.outer = Some(Point::new(
            observed.frame.outer_x as f32,
            observed.frame.outer_y as f32,
        ));
        if !observed.adjusted {
            return None;
        }
        self.content = Size::new(
            observed.frame.content_width as f32,
            observed.frame.content_height as f32,
        );
        self.record()
    }

    /// A native resize, in logical points. Returns the save to arm. Before
    /// the screen check answers, only a resize to the size the window was
    /// created at is the creation's own; any other is a real one, and the
    /// check's answer arriving after it is no reason to lose it.
    pub(super) fn resized(&mut self, size: Size) -> Option<u64> {
        self.content = size;
        if !self.checked && size == self.opening.size {
            return None;
        }
        self.record()
    }

    /// A native move of the outer top-left. Returns the save to arm.
    pub(super) fn moved(&mut self, outer: Point) -> Option<u64> {
        if !self.checked {
            return None;
        }
        self.outer = Some(outer);
        self.record()
    }

    pub(super) fn observe_mode(&mut self, mode: window::Mode) {
        self.mode = mode;
        if mode == window::Mode::Fullscreen {
            self.owed = false;
        }
    }

    /// A save's deadline. Only the latest arm acts, and only while the
    /// window is visible and not full screen — as of the mode query the
    /// deadline itself made (`observe_mode` just before this).
    ///
    /// `outer` is the position the deadline read back from the window, and
    /// it wins over the one `Moved` left: winit converts a move with the
    /// window's new backing scale while iced still holds the old one, so a
    /// move onto a screen of another scale can arrive scaled wrong.
    pub(super) fn save_due(
        &mut self,
        workspace: &Workspace,
        generation: u64,
        outer: Option<Point>,
    ) {
        if !self.debounce.is_latest(generation) || self.mode != window::Mode::Windowed {
            return;
        }
        if outer.is_some() {
            self.outer = outer;
        }
        self.write_owed(workspace);
    }

    /// Roost's own full-screen toggle is about to run. Entering full screen
    /// clears what is owed (see `owed`), so a windowed frame is written now,
    /// before it can be. The AppKit green button gives no such warning:
    /// there the last frame written is the one that stays.
    pub(super) fn full_screen_toggling(&mut self, workspace: &Workspace) {
        if !self.remembers {
            return;
        }
        self.toggling = true;
        if self.mode == window::Mode::Windowed {
            self.write_owed(workspace);
        }
    }

    /// The full-screen settle after the last resize came due: a transition
    /// Roost started is over.
    pub(super) fn full_screen_settled(&mut self) {
        self.toggling = false;
    }

    /// A quit inside the debounce would otherwise lose the last move.
    /// No visibility check: a hidden or minimized window keeps the frame
    /// it had, and the window may be gone by now anyway. Nothing during a
    /// transition Roost started — its windowed frame was written when the
    /// toggle ran.
    pub(super) fn save_on_exit(&mut self, workspace: &Workspace) {
        if self.toggling || self.mode == window::Mode::Fullscreen {
            return;
        }
        self.write_owed(workspace);
    }

    fn record(&mut self) -> Option<u64> {
        if !self.remembers || self.mode == window::Mode::Fullscreen {
            return None;
        }
        self.owed = true;
        Some(self.debounce.arm())
    }

    /// Write what is owed, once the position is known: a first launch's
    /// window has none until the screen check or a deadline reads one.
    fn write_owed(&mut self, workspace: &Workspace) {
        if !self.owed {
            return;
        }
        let Some(outer) = self.outer else {
            return;
        };
        self.owed = false;
        workspace.set_window_frame(WindowFrame {
            content_width: f64::from(self.content.width),
            content_height: f64::from(self.content.height),
            outer_x: f64::from(outer.x),
            outer_y: f64::from(outer.y),
        });
    }
}

/// `App::drop`'s persistence, in its required order: the frame a quit
/// inside the debounce still owes, then the flush that freezes
/// persistence for good.
pub(super) fn flush_on_exit(
    workspace: &Workspace,
    frames: &mut WindowFrameMemory,
) -> Result<(), String> {
    frames.save_on_exit(workspace);
    workspace.flush()
}

/// A rectangle in AppKit's global screen space: points, with the origin at
/// the primary screen's bottom-left and y growing up.
///
/// This and the geometry below are portable, though only macOS calls
/// them, so their tests run on every CI cell.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScreenRect {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

/// Slack for AppKit's rounding: a frame flush with a screen's edge is on
/// that screen.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const EDGE_SLACK: f64 = 0.5;

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl ScreenRect {
    fn max_x(self) -> f64 {
        self.x + self.width
    }

    fn max_y(self) -> f64 {
        self.y + self.height
    }

    fn contains(self, other: Self) -> bool {
        other.x >= self.x - EDGE_SLACK
            && other.y >= self.y - EDGE_SLACK
            && other.max_x() <= self.max_x() + EDGE_SLACK
            && other.max_y() <= self.max_y() + EDGE_SLACK
    }

    fn overlap_area(self, other: Self) -> f64 {
        let width = self.max_x().min(other.max_x()) - self.x.max(other.x);
        let height = self.max_y().min(other.max_y()) - self.y.max(other.y);
        width.max(0.0) * height.max(0.0)
    }
}

/// Where `frame` has to go to lie wholly on one screen's visible area, or
/// `None` when it already does. It goes to the screen it overlaps most,
/// or to `main` when it is on none, and shrinks only when it no longer
/// fits there.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn fit_on_screens(
    frame: ScreenRect,
    visible: &[ScreenRect],
    main: ScreenRect,
) -> Option<ScreenRect> {
    if visible.iter().any(|screen| screen.contains(frame)) {
        return None;
    }
    let target = visible
        .iter()
        .copied()
        .map(|screen| (screen.overlap_area(frame), screen))
        .filter(|(area, _)| *area > 0.0)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map_or(main, |(_, screen)| screen);
    let width = frame.width.min(target.width);
    let height = frame.height.min(target.height);
    Some(ScreenRect {
        x: frame.x.clamp(target.x, target.max_x() - width),
        y: frame.y.clamp(target.y, target.max_y() - height),
        width,
        height,
    })
}

/// An AppKit window frame as iced means it: the content size, and the
/// outer top-left measured down from the top of the primary screen —
/// the flip winit applies with that screen's height.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn iced_frame(
    frame: ScreenRect,
    content_width: f64,
    content_height: f64,
    primary_height: f64,
) -> WindowFrame {
    WindowFrame {
        content_width,
        content_height,
        outer_x: frame.x,
        outer_y: primary_height - frame.max_y(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: WindowFrame = WindowFrame {
        content_width: 900.0,
        content_height: 600.0,
        outer_x: 120.0,
        outer_y: 80.0,
    };

    fn rect(x: f64, y: f64, width: f64, height: f64) -> ScreenRect {
        ScreenRect {
            x,
            y,
            width,
            height,
        }
    }

    /// A memory past its screen check, at `FRAME`, the way a launch that
    /// restored it stands once the window is up.
    fn checked_memory(remembers: bool) -> WindowFrameMemory {
        let mut memory =
            WindowFrameMemory::new(remembers, OpeningFrame::from_saved(Some(FRAME), remembers));
        assert_eq!(memory.take_check(), remembers.then_some(true));
        memory.checked(Some(CheckedFrame {
            frame: FRAME,
            adjusted: false,
        }));
        memory
    }

    fn state(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("state.json")
    }

    #[test]
    fn a_saved_frame_opens_at_its_size_and_position() {
        let opening = OpeningFrame::from_saved(Some(FRAME), true);
        assert_eq!(opening.size, Size::new(900.0, 600.0));
        assert_eq!(opening.position, Some(Point::new(120.0, 80.0)));
    }

    #[test]
    fn no_saved_frame_opens_at_the_defaults() {
        let defaults = OpeningFrame {
            size: INITIAL_WINDOW_SIZE,
            position: None,
        };
        assert_eq!(OpeningFrame::from_saved(None, true), defaults);
        assert_eq!(
            OpeningFrame::from_saved(Some(FRAME), false),
            defaults,
            "a build that does not remember the frame never reads it"
        );
    }

    #[test]
    fn a_saved_size_is_clamped_only_to_the_minimum() {
        let small = WindowFrame {
            content_width: 200.0,
            content_height: 5000.0,
            ..FRAME
        };
        let opening = OpeningFrame::from_saved(Some(small), true);
        assert_eq!(opening.size, Size::new(MIN_WINDOW_SIZE.width, 5000.0));
    }

    #[test]
    fn an_invalid_saved_frame_opens_at_the_defaults() {
        for broken in [
            WindowFrame {
                content_width: 0.0,
                ..FRAME
            },
            WindowFrame {
                content_height: -600.0,
                ..FRAME
            },
            WindowFrame {
                outer_x: f64::NAN,
                ..FRAME
            },
            // Finite as an `f64`, not as the `f32` iced places it with.
            WindowFrame {
                outer_y: 1e300,
                ..FRAME
            },
            // Finite everywhere, and a GPU surface wgpu panics on.
            WindowFrame {
                content_width: 100_000.0,
                content_height: 100_000.0,
                ..FRAME
            },
        ] {
            assert_eq!(
                OpeningFrame::from_saved(Some(broken), true),
                OpeningFrame {
                    size: INITIAL_WINDOW_SIZE,
                    position: None,
                },
                "{broken:?}"
            );
        }
    }

    #[test]
    fn the_screen_check_is_asked_for_once() {
        let mut memory = WindowFrameMemory::new(true, OpeningFrame::from_saved(None, true));
        assert!(memory.take_check().is_some());
        assert_eq!(
            memory.take_check(),
            None,
            "a later focus re-runs window_opened"
        );
    }

    #[test]
    fn only_a_saved_frame_is_fitted_onto_a_screen() {
        let mut fresh = WindowFrameMemory::new(true, OpeningFrame::from_saved(None, true));
        assert_eq!(
            fresh.take_check(),
            Some(false),
            "a default placement is the platform's, whatever the screen"
        );
        let mut restored =
            WindowFrameMemory::new(true, OpeningFrame::from_saved(Some(FRAME), true));
        assert_eq!(restored.take_check(), Some(true));
    }

    #[test]
    fn a_move_before_the_screen_check_is_the_placement_not_the_user() {
        let mut memory = WindowFrameMemory::new(true, OpeningFrame::from_saved(Some(FRAME), true));
        assert_eq!(memory.moved(Point::new(0.0, 0.0)), None);
        assert_eq!(
            memory.resized(Size::new(900.0, 600.0)),
            None,
            "a resize to the size it opened at is the creation's own"
        );
        assert!(!memory.owed);
    }

    /// Every save `arms` asks for, at its deadline, with `outer` read back.
    fn run_deadlines(
        memory: &mut WindowFrameMemory,
        workspace: &Workspace,
        arms: impl IntoIterator<Item = Option<u64>>,
        outer: Point,
    ) {
        memory.observe_mode(window::Mode::Windowed);
        for generation in arms.into_iter().flatten() {
            memory.save_due(workspace, generation, Some(outer));
        }
    }

    #[test]
    fn a_resize_that_beats_the_screen_checks_answer_is_still_saved() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(state(&dir));
        let mut memory = WindowFrameMemory::new(true, OpeningFrame::from_saved(Some(FRAME), true));
        assert_eq!(memory.take_check(), Some(true));
        // A resize asked for right after launch reaches `update` before the
        // check's answer — which looked at the window before the resize was
        // applied, and so reports the frame it opened at.
        let early = memory.resized(Size::new(1100.0, 720.0));
        let late = memory.checked(Some(CheckedFrame {
            frame: FRAME,
            adjusted: false,
        }));
        run_deadlines(
            &mut memory,
            &workspace,
            [early, late],
            Point::new(120.0, 80.0),
        );
        assert_eq!(
            workspace.window_frame(),
            Some(WindowFrame {
                content_width: 1100.0,
                content_height: 720.0,
                ..FRAME
            }),
            "the resize must reach state.json while the window is open"
        );
    }

    #[test]
    fn a_first_launch_resize_before_any_position_is_known_is_still_saved() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(state(&dir));
        let mut memory = WindowFrameMemory::new(true, OpeningFrame::from_saved(None, true));
        assert_eq!(memory.take_check(), Some(false));
        let early = memory.resized(Size::new(800.0, 500.0));
        let late = memory.checked(None);
        run_deadlines(
            &mut memory,
            &workspace,
            [early, late],
            Point::new(64.0, 48.0),
        );
        assert_eq!(
            workspace.window_frame(),
            Some(WindowFrame {
                content_width: 800.0,
                content_height: 500.0,
                outer_x: 64.0,
                outer_y: 48.0,
            })
        );
    }

    #[test]
    fn the_debounce_writes_the_frame_while_running() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(state(&dir));
        let mut memory = checked_memory(true);

        let first = memory.moved(Point::new(300.0, 40.0)).expect("a move arms");
        let latest = memory
            .resized(Size::new(1000.0, 700.0))
            .expect("a resize re-arms");
        memory.observe_mode(window::Mode::Windowed);
        memory.save_due(&workspace, first, None);
        assert_eq!(
            workspace.window_frame(),
            None,
            "a superseded deadline writes nothing"
        );

        memory.observe_mode(window::Mode::Windowed);
        memory.save_due(&workspace, latest, None);
        drop(workspace);
        assert_eq!(
            Workspace::open(state(&dir)).window_frame(),
            Some(WindowFrame {
                content_width: 1000.0,
                content_height: 700.0,
                outer_x: 300.0,
                outer_y: 40.0,
            }),
            "the deadline wrote the frame with the window still open"
        );
    }

    #[test]
    fn a_quit_inside_the_debounce_writes_the_final_frame_before_the_flush() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(state(&dir));
        let mut memory = checked_memory(true);
        memory
            .moved(Point::new(-1500.0, 60.0))
            .expect("a move arms");

        // No deadline: the quit lands inside the 500 ms.
        flush_on_exit(&workspace, &mut memory).unwrap();
        drop(workspace);
        assert_eq!(
            Workspace::open(state(&dir)).window_frame(),
            Some(WindowFrame {
                outer_x: -1500.0,
                outer_y: 60.0,
                ..FRAME
            })
        );
    }

    #[test]
    fn nothing_is_written_while_full_screen() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(state(&dir));
        let mut memory = checked_memory(true);
        memory.observe_mode(window::Mode::Fullscreen);
        assert_eq!(memory.resized(Size::new(1512.0, 982.0)), None);
        assert_eq!(memory.moved(Point::new(0.0, 0.0)), None);
        flush_on_exit(&workspace, &mut memory).unwrap();
        assert_eq!(workspace.window_frame(), None);
    }

    #[test]
    fn a_full_screen_enter_and_exit_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(state(&dir));
        let mut memory = checked_memory(true);

        // Entering: the first resize lands before the query that says it
        // was the transition's, so it is recorded — and then dropped.
        let entering = memory
            .resized(Size::new(1512.0, 982.0))
            .expect("the mode is not known to be full screen yet");
        memory.observe_mode(window::Mode::Fullscreen);
        memory.save_due(&workspace, entering, None);

        // Leaving: winit reports full screen until the transition ends, so
        // the resize back is never recorded, and the settle's query that
        // finally reads windowed finds nothing owed.
        memory.observe_mode(window::Mode::Fullscreen);
        assert_eq!(memory.resized(Size::new(900.0, 600.0)), None);
        memory.observe_mode(window::Mode::Windowed);
        memory.save_due(&workspace, entering, None);
        flush_on_exit(&workspace, &mut memory).unwrap();
        assert_eq!(workspace.window_frame(), None);
    }

    fn saved_origin(workspace: &Workspace) -> Option<(f64, f64)> {
        workspace
            .window_frame()
            .map(|frame| (frame.outer_x, frame.outer_y))
    }

    #[test]
    fn the_deadline_saves_the_position_it_reads_back_not_the_last_move() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(state(&dir));
        let mut memory = checked_memory(true);
        // The last move onto a 1x screen, converted with the 2x scale iced
        // still held: half the real x.
        let armed = memory.moved(Point::new(800.0, 40.0)).expect("a move arms");
        memory.observe_mode(window::Mode::Windowed);
        memory.save_due(&workspace, armed, Some(Point::new(1600.0, 40.0)));
        assert_eq!(saved_origin(&workspace), Some((1600.0, 40.0)));

        // A resize after it composes with the position read back, not the
        // one the move left.
        let armed = memory
            .resized(Size::new(1000.0, 700.0))
            .expect("a resize arms");
        memory.save_due(&workspace, armed, None);
        assert_eq!(saved_origin(&workspace), Some((1600.0, 40.0)));
    }

    #[test]
    fn roosts_own_full_screen_toggle_writes_the_pending_windowed_frame_first() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(state(&dir));
        let mut memory = checked_memory(true);
        memory.moved(Point::new(300.0, 40.0)).expect("a move arms");

        // Entering inside the debounce: the deadline never gets to write it.
        memory.full_screen_toggling(&workspace);
        assert_eq!(
            saved_origin(&workspace),
            Some((300.0, 40.0)),
            "written before full screen can drop it"
        );
        memory.observe_mode(window::Mode::Fullscreen);
        flush_on_exit(&workspace, &mut memory).unwrap();
        assert_eq!(saved_origin(&workspace), Some((300.0, 40.0)));
    }

    #[test]
    fn a_quit_during_roosts_own_full_screen_entry_writes_no_transition_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(state(&dir));
        let mut memory = checked_memory(true);
        memory.full_screen_toggling(&workspace);
        // The transition's first resize, before any query says full screen.
        memory
            .resized(Size::new(1512.0, 982.0))
            .expect("the mode is not known to be full screen yet");
        flush_on_exit(&workspace, &mut memory).unwrap();
        assert_eq!(
            workspace.window_frame(),
            None,
            "full-screen transition geometry must never be the saved frame"
        );
    }

    #[test]
    fn the_settle_ends_a_toggle_so_a_quit_saves_again() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(state(&dir));
        let mut memory = checked_memory(true);
        memory.full_screen_toggling(&workspace);
        memory.observe_mode(window::Mode::Windowed);
        memory.full_screen_settled();
        memory.moved(Point::new(5.0, 6.0)).expect("a move arms");
        flush_on_exit(&workspace, &mut memory).unwrap();
        assert_eq!(saved_origin(&workspace), Some((5.0, 6.0)));
    }

    #[test]
    fn a_deadline_on_a_hidden_window_writes_nothing_but_the_quit_does() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(state(&dir));
        let mut memory = checked_memory(true);
        let armed = memory.moved(Point::new(10.0, 30.0)).expect("a move arms");
        memory.observe_mode(window::Mode::Hidden);
        memory.save_due(&workspace, armed, None);
        assert_eq!(workspace.window_frame(), None, "not while hidden");

        flush_on_exit(&workspace, &mut memory).unwrap();
        assert_eq!(
            workspace
                .window_frame()
                .map(|frame| (frame.outer_x, frame.outer_y)),
            Some((10.0, 30.0)),
            "a minimized window keeps its frame"
        );
    }

    #[test]
    fn a_frame_the_screen_check_moved_is_owed_to_state_json() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(state(&dir));
        let mut memory = WindowFrameMemory::new(true, OpeningFrame::from_saved(Some(FRAME), true));
        assert_eq!(memory.take_check(), Some(true));
        let moved_on = WindowFrame {
            outer_x: 0.0,
            ..FRAME
        };
        let armed = memory
            .checked(Some(CheckedFrame {
                frame: moved_on,
                adjusted: true,
            }))
            .expect("an adjusted frame arms a save");
        memory.save_due(&workspace, armed, None);
        assert_eq!(workspace.window_frame(), Some(moved_on));
    }

    #[test]
    fn a_build_that_does_not_remember_never_writes_a_window_key() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(state(&dir));
        let mut memory = checked_memory(false);
        assert_eq!(memory.moved(Point::new(300.0, 40.0)), None);
        assert_eq!(memory.resized(Size::new(1000.0, 700.0)), None);
        assert_eq!(
            memory.checked(Some(CheckedFrame {
                frame: FRAME,
                adjusted: true,
            })),
            None
        );
        workspace.set_sidebar_width(300.0);
        flush_on_exit(&workspace, &mut memory).unwrap();
        let raw = std::fs::read_to_string(state(&dir)).unwrap();
        assert!(!raw.contains("\"window\""), "{raw}");
    }

    // ── the screen check's geometry ──

    /// A 14" MacBook's built-in Retina panel (points) as the primary, with
    /// the menu bar and a bottom Dock taken out of its visible frame.
    const BUILT_IN: ScreenRect = ScreenRect {
        x: 0.0,
        y: 70.0,
        width: 1512.0,
        height: 882.0,
    };

    #[test]
    fn a_frame_wholly_on_a_screen_stays_put() {
        let frame = rect(100.0, 200.0, 900.0, 628.0);
        assert_eq!(fit_on_screens(frame, &[BUILT_IN], BUILT_IN), None);
        let flush = rect(0.0, 70.0, 1512.0, 882.0);
        assert_eq!(
            fit_on_screens(flush, &[BUILT_IN], BUILT_IN),
            None,
            "a frame filling the visible area is on it"
        );
    }

    #[test]
    fn a_partly_off_screen_frame_moves_back_without_shrinking() {
        let off_right = rect(1200.0, 200.0, 900.0, 628.0);
        assert_eq!(
            fit_on_screens(off_right, &[BUILT_IN], BUILT_IN),
            Some(rect(612.0, 200.0, 900.0, 628.0))
        );
        let into_the_dock = rect(100.0, 20.0, 900.0, 628.0);
        assert_eq!(
            fit_on_screens(into_the_dock, &[BUILT_IN], BUILT_IN),
            Some(rect(100.0, 70.0, 900.0, 628.0))
        );
    }

    #[test]
    fn a_frame_on_no_screen_goes_to_the_main_one() {
        // Saved on an external display that is no longer connected.
        let gone = rect(3000.0, 300.0, 900.0, 628.0);
        assert_eq!(
            fit_on_screens(gone, &[BUILT_IN], BUILT_IN),
            Some(rect(612.0, 300.0, 900.0, 628.0))
        );
    }

    #[test]
    fn a_frame_too_big_for_its_screen_shrinks_to_it() {
        let huge = rect(-50.0, 0.0, 2560.0, 1440.0);
        assert_eq!(fit_on_screens(huge, &[BUILT_IN], BUILT_IN), Some(BUILT_IN));
    }

    #[test]
    fn negative_origin_screens_are_real_screens() {
        // An external display left of the primary, and one above it: both
        // have origins AppKit gives negative or beyond-the-primary values.
        let left = rect(-1920.0, 0.0, 1920.0, 1055.0);
        let above = rect(0.0, 982.0, 2560.0, 1415.0);
        let screens = [BUILT_IN, left, above];
        let on_left = rect(-1800.0, 100.0, 900.0, 628.0);
        assert_eq!(fit_on_screens(on_left, &screens, BUILT_IN), None);
        let past_left_edge = rect(-2000.0, 100.0, 900.0, 628.0);
        assert_eq!(
            fit_on_screens(past_left_edge, &screens, BUILT_IN),
            Some(rect(-1920.0, 100.0, 900.0, 628.0))
        );
        let over_the_top = rect(100.0, 1900.0, 900.0, 628.0);
        assert_eq!(
            fit_on_screens(over_the_top, &screens, BUILT_IN),
            Some(rect(100.0, 1769.0, 900.0, 628.0))
        );
    }

    #[test]
    fn a_frame_across_mixed_scale_screens_goes_to_the_one_it_overlaps_most() {
        // A 1x 2560×1440 display right of the 2x built-in: AppKit's global
        // space is points on both, so the geometry needs no scale.
        let external = rect(1512.0, -200.0, 2560.0, 1415.0);
        let screens = [BUILT_IN, external];
        let mostly_external = rect(1400.0, 300.0, 900.0, 628.0);
        assert_eq!(
            fit_on_screens(mostly_external, &screens, BUILT_IN),
            Some(rect(1512.0, 300.0, 900.0, 628.0))
        );
        let mostly_built_in = rect(1000.0, 300.0, 900.0, 628.0);
        assert_eq!(
            fit_on_screens(mostly_built_in, &screens, BUILT_IN),
            Some(rect(612.0, 300.0, 900.0, 628.0))
        );
    }

    #[test]
    fn the_appkit_frame_reads_back_as_iceds_top_left() {
        // Primary 982 points tall: a frame whose top edge is 182 points
        // below the primary's top.
        let frame = rect(100.0, 200.0, 900.0, 600.0);
        assert_eq!(
            iced_frame(frame, 900.0, 572.0, 982.0),
            WindowFrame {
                content_width: 900.0,
                content_height: 572.0,
                outer_x: 100.0,
                outer_y: 182.0,
            }
        );
        // On a display above the primary, the top-left is above iced's
        // origin: negative, and still a real place.
        let above = rect(-300.0, 1200.0, 900.0, 600.0);
        assert_eq!(iced_frame(above, 900.0, 572.0, 982.0).outer_y, -818.0);
    }
}
