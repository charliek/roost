//! One attached tab's client terminal, driven by the attach stream's
//! events: the iced UI's hydration machine (`roost-iced`'s
//! `app/host_tab.rs`) without its UI-thread stepping budget, since this
//! runs on the owner thread and can step a snapshot's history to the end
//! of what has arrived in one go.
//!
//! The terminal is always at the session PTY's grid. A phone never
//! resizes it to its own screen: the Flutter painter scales or
//! letterboxes the grid, and only the size owner's `RESIZE` moves it.

use std::collections::VecDeque;

use roost_client::attach::{AttachEvent, PayloadKind};
use roost_ipc::dataframe::InputTake;
use roost_ipc::messages::{AttachMode, SizeOwner};
use roost_ui_model::theme::Theme;
use roost_vt::{
    key_action, mouse_action, mouse_button, ActiveScreen, Cell, Dirty, HistoryStep, KeyEncoder,
    KeyEvent, MouseEncoder, MouseEvent, ReadyState, RenderState, RenderedRow, ScrollDirection,
    ScrollRoute, SnapshotDecodeOptions, SnapshotDecoder, Terminal, TerminalOptions, TerminalScroll,
};

use crate::frame::{self, Frame, Header};
use crate::keys::{self, KeyInput};

/// Scrollback a `vt` hydration terminal keeps — the desktop's.
const MAX_SCROLLBACK: usize = 2_000;

/// The deferral bound: the session's own queued-PTY budget. More than
/// this before the payload can take it means the peer is not holding
/// PTY until READY, and the stream is rebuilt rather than buffered
/// without limit.
pub const MAX_DEFERRED_BYTES: usize = 8 * 1024 * 1024;

/// A tab's grid and the session terminal's cell size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Geometry {
    pub cols: u16,
    pub rows: u16,
    pub cell_w_px: u16,
    pub cell_h_px: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachState {
    Attaching,
    Live,
    Reconnecting,
    Ended,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub state: AttachState,
    pub message: Option<String>,
}

/// Who sizes the tab, and the grid they set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerView {
    pub owner: Option<SizeOwner>,
    pub cols: u16,
    pub rows: u16,
}

/// What applying one event changed. Several can change at once.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// A new frame is ready to pull.
    pub frame_dirty: bool,
    pub status: Option<Status>,
    pub owner: Option<OwnerView>,
    pub exited: Option<i32>,
    /// This terminal is not at the stream's resume point, and never will
    /// be: a payload it could not take, or a resume offered to a terminal
    /// that never finished hydrating. The stream must forget its resume
    /// point and re-attach for a fresh snapshot.
    pub reattach: Option<String>,
}

/// Bytes for the tab's program, and whether drawing them moved the
/// screen (a snap back to the live bottom, a local scroll).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Input {
    pub bytes: Vec<u8>,
    pub take: InputTake,
    pub frame_dirty: bool,
}

/// How a payload becomes the terminal that will be swapped in.
enum Hydrator {
    /// GHOSTSNP: renderable from READY, its history stepped as it
    /// arrives, finished by its own FINISH record.
    Snapshot(SnapshotDecoder),
    /// `vt`: this client's own terminal, at the attach geometry and in
    /// this client's theme, the payload written into it. Not a drawable
    /// screen until its terminator (one empty SNAP) says it is whole.
    Vt(Terminal),
}

enum Deferred {
    Pty(Vec<u8>),
    Grid(Geometry),
}

struct Hydration {
    hydrator: Hydrator,
    /// PTY and GRID that arrived before the payload could take them, in
    /// arrival order.
    deferred: VecDeque<Deferred>,
    deferred_bytes: usize,
    /// The snapshot decoder is past READY: its terminal is the one drawn.
    ready: bool,
    /// The grid the terminal being built is at: the payload's own, then
    /// each GRID applied to it.
    built_at: Geometry,
}

enum Phase {
    /// No stream, or between streams.
    Waiting,
    Hydrating(Box<Hydration>),
    Live,
    /// The program exited, or the stream gave up for good.
    Over,
}

struct Screen {
    terminal: Terminal,
    geometry: Geometry,
}

/// What the last frame was built from, so the next one sends only what
/// moved — or everything, when the basis itself moved.
#[derive(Default)]
struct FrameBasis {
    /// The [`TabTerminal::rendered_generation`] the render state was
    /// made for.
    generation: u64,
    theme_generation: u64,
    grid: (u16, u16),
    defaults: Option<(u32, u32)>,
    header: Option<Header>,
    /// The render state was rebuilt since the last frame.
    full_pending: bool,
}

pub struct TabTerminal {
    theme: Theme,
    theme_generation: u64,
    /// The last swapped-in terminal. While a snapshot hydrates past
    /// READY the decoder's terminal is drawn instead, and this one stays
    /// as the fallback should the hydration be abandoned.
    screen: Option<Screen>,
    phase: Phase,
    status: AttachState,
    owner: Option<OwnerView>,
    /// The screen is not at the stream's resume point: a hydration was
    /// dropped or cut short beside it, or a re-attach was asked for. The
    /// stream's point then counts bytes this terminal never took, so only
    /// a finished payload makes a resume safe again.
    off_resume_point: bool,
    /// A re-attach was asked for and no handshake has answered it yet;
    /// the old stream's leftover frames are expected, not news.
    reattach_pending: bool,
    /// Moves whenever the terminal frames are drawn from changes.
    rendered_generation: u64,
    render: RenderState,
    basis: FrameBasis,
    keys: KeyEncoder,
    mouse: MouseEncoder,
    scroll: TerminalScroll,
}

fn paint(terminal: &mut Terminal, theme: &Theme) -> roost_vt::Result<()> {
    terminal.set_color_foreground(theme.foreground)?;
    terminal.set_color_background(theme.background)?;
    terminal.set_color_cursor(theme.cursor)?;
    terminal.set_color_palette(&theme.palette)
}

/// Bring `render` up to the terminal frames are drawn from, rebuilding
/// it when that terminal changed (`generation` moved).
fn sync_render<'a>(
    phase: &'a Phase,
    screen: &'a Option<Screen>,
    render: &mut RenderState,
    basis: &mut FrameBasis,
    generation: u64,
) -> roost_vt::Result<Option<(&'a Terminal, Geometry)>> {
    let Some((terminal, geometry)) = rendered(phase, screen) else {
        return Ok(None);
    };
    if basis.generation != generation {
        *render = RenderState::new()?;
        basis.generation = generation;
        basis.full_pending = true;
    }
    render.update(terminal)?;
    Ok(Some((terminal, geometry)))
}

/// The terminal frames are drawn from, and its grid.
fn rendered<'a>(phase: &'a Phase, screen: &'a Option<Screen>) -> Option<(&'a Terminal, Geometry)> {
    if let Phase::Hydrating(hydration) = phase {
        if let (true, Hydrator::Snapshot(decoder)) = (hydration.ready, &hydration.hydrator) {
            return decoder
                .terminal()
                .map(|terminal| (terminal, hydration.built_at));
        }
    }
    screen
        .as_ref()
        .map(|screen| (&screen.terminal, screen.geometry))
}

impl TabTerminal {
    pub fn new(theme: Theme) -> roost_vt::Result<Self> {
        Ok(Self {
            theme,
            theme_generation: 0,
            screen: None,
            phase: Phase::Waiting,
            status: AttachState::Attaching,
            owner: None,
            off_resume_point: false,
            reattach_pending: false,
            rendered_generation: 0,
            render: RenderState::new()?,
            basis: FrameBasis::default(),
            keys: KeyEncoder::new()?,
            mouse: MouseEncoder::new()?,
            scroll: TerminalScroll::new(),
        })
    }

    pub fn status(&self) -> AttachState {
        self.status
    }

    /// The grid of the terminal frames are drawn from; `None` until a
    /// payload has become drawable.
    pub fn geometry(&self) -> Option<Geometry> {
        rendered(&self.phase, &self.screen).map(|(_, geometry)| geometry)
    }

    pub fn apply(&mut self, event: AttachEvent) -> Applied {
        let mut applied = Applied::default();
        match event {
            AttachEvent::Accepted {
                kind,
                mode,
                cols,
                rows,
                cell_w_px,
                cell_h_px,
                owner,
            } => {
                let geometry = Geometry {
                    cols,
                    rows,
                    cell_w_px,
                    cell_h_px,
                };
                self.reattach_pending = false;
                self.note_owner(owner, geometry, &mut applied);
                self.accepted(kind, mode, geometry, &mut applied);
            }
            AttachEvent::Snap(bytes) => self.snap(&bytes, &mut applied),
            AttachEvent::Pty(bytes) => self.pty(bytes, &mut applied),
            AttachEvent::Grid {
                cols,
                rows,
                cell_w_px,
                cell_h_px,
                owner,
            } => {
                let geometry = Geometry {
                    cols,
                    rows,
                    cell_w_px,
                    cell_h_px,
                };
                self.note_owner(owner, geometry, &mut applied);
                self.grid(geometry, &mut applied);
            }
            AttachEvent::Exit { code } => {
                self.salvage(&mut applied);
                self.phase = Phase::Over;
                applied.exited = Some(code);
            }
            AttachEvent::Reconnecting { reason } => {
                // A half-built payload cannot outlive its stream: the
                // stream's resume point counts bytes this terminal has
                // not taken, so it has to go too.
                if self.abandon(&mut applied) {
                    self.request_reattach(
                        format!("stream dropped mid-hydration: {reason}"),
                        &mut applied,
                    );
                }
                if !matches!(self.phase, Phase::Over) {
                    self.phase = Phase::Waiting;
                }
                self.set_status(AttachState::Reconnecting, Some(reason), &mut applied);
            }
            AttachEvent::Ended { reason } => {
                self.salvage(&mut applied);
                self.phase = Phase::Over;
                self.set_status(AttachState::Ended, Some(reason), &mut applied);
            }
        }
        applied
    }

    fn accepted(
        &mut self,
        kind: PayloadKind,
        mode: AttachMode,
        geometry: Geometry,
        applied: &mut Applied,
    ) {
        if mode == AttachMode::Resume {
            if matches!(self.phase, Phase::Hydrating(_)) {
                return self.reattach("resume offered mid-hydration", applied);
            }
            // A resume replays bytes the session wrote for the grid it
            // last heard from this client; into a terminal at another
            // grid every absolute move lands elsewhere (#564).
            let reason = match &self.screen {
                None => "resume offered to a terminal that never hydrated",
                Some(_) if self.off_resume_point => {
                    "resume offered to a terminal that is not at the resume point"
                }
                Some(screen) if screen.geometry != geometry => {
                    "resume offered for a grid the terminal is not at"
                }
                Some(_) => {
                    self.phase = Phase::Live;
                    return self.set_status(AttachState::Live, None, applied);
                }
            };
            return self.reattach(reason, applied);
        }
        self.abandon(applied);
        let hydrator = match kind {
            PayloadKind::GhosttySnapshot => {
                Hydrator::Snapshot(SnapshotDecoder::new(SnapshotDecodeOptions::default()))
            }
            PayloadKind::Vt => match self.vt_terminal(geometry) {
                Ok(terminal) => Hydrator::Vt(terminal),
                Err(error) => {
                    return self.reattach(&format!("vt terminal build failed: {error}"), applied)
                }
            },
        };
        self.phase = Phase::Hydrating(Box::new(Hydration {
            hydrator,
            deferred: VecDeque::new(),
            deferred_bytes: 0,
            ready: false,
            built_at: geometry,
        }));
        self.set_status(AttachState::Attaching, None, applied);
    }

    fn vt_terminal(&self, at: Geometry) -> roost_vt::Result<Terminal> {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: at.cols,
            rows: at.rows,
            max_scrollback: MAX_SCROLLBACK,
            ..Default::default()
        })?;
        paint(&mut terminal, &self.theme)?;
        terminal.resize(
            at.cols,
            at.rows,
            u32::from(at.cell_w_px),
            u32::from(at.cell_h_px),
        )?;
        Ok(terminal)
    }

    fn snap(&mut self, bytes: &[u8], applied: &mut Applied) {
        let Phase::Hydrating(hydration) = &mut self.phase else {
            if self.reattach_pending || matches!(self.phase, Phase::Over) {
                return;
            }
            // Including a non-empty SNAP after a `vt` terminator: the
            // stream is confused.
            return self.reattach("SNAP outside a hydration", applied);
        };
        match &mut hydration.hydrator {
            Hydrator::Vt(terminal) => {
                if !bytes.is_empty() {
                    terminal.vt_write(bytes);
                    return;
                }
                while let Some(deferred) = hydration.deferred.pop_front() {
                    match deferred {
                        Deferred::Pty(bytes) => terminal.vt_write(&bytes),
                        Deferred::Grid(grid) => {
                            if let Err(error) = terminal.resize(
                                grid.cols,
                                grid.rows,
                                u32::from(grid.cell_w_px),
                                u32::from(grid.cell_h_px),
                            ) {
                                return self
                                    .reattach(&format!("deferred GRID failed: {error}"), applied);
                            }
                            hydration.built_at = grid;
                        }
                    }
                }
                self.finish(applied);
            }
            Hydrator::Snapshot(decoder) => {
                if let Err(error) = decoder.feed(bytes) {
                    return self.reattach(&format!("snapshot decode failed: {error}"), applied);
                }
                if !hydration.ready {
                    match decoder.try_ready() {
                        Ok(ReadyState::NeedMoreBytes) => return,
                        Ok(ReadyState::Ready) => {}
                        Err(error) => {
                            return self
                                .reattach(&format!("snapshot READY failed: {error}"), applied)
                        }
                    }
                    let theme = &self.theme;
                    let mut replay = decoder.set_default_colors(
                        theme.foreground,
                        theme.background,
                        theme.cursor,
                        &theme.palette,
                    );
                    while replay.is_ok() {
                        let Some(deferred) = hydration.deferred.pop_front() else {
                            break;
                        };
                        replay = match deferred {
                            Deferred::Pty(bytes) => decoder.vt_write(&bytes),
                            Deferred::Grid(grid) => {
                                hydration.built_at = grid;
                                decoder.resize(
                                    grid.cols,
                                    grid.rows,
                                    u32::from(grid.cell_w_px),
                                    u32::from(grid.cell_h_px),
                                )
                            }
                        };
                    }
                    if let Err(error) = replay {
                        return self.reattach(&format!("READY replay failed: {error}"), applied);
                    }
                    hydration.ready = true;
                    hydration.deferred_bytes = 0;
                    self.rendered_changed();
                }
                applied.frame_dirty = true;
                self.step_history(applied);
            }
        }
    }

    /// Step every history record that has fully arrived; on FINISH, swap
    /// the hydrated terminal in.
    fn step_history(&mut self, applied: &mut Applied) {
        let Phase::Hydrating(hydration) = &mut self.phase else {
            return;
        };
        let Hydrator::Snapshot(decoder) = &mut hydration.hydrator else {
            return;
        };
        loop {
            match decoder.try_next() {
                Ok(HistoryStep::NeedMoreBytes) => return,
                Ok(HistoryStep::Page { .. } | HistoryStep::Image { .. }) => {}
                Ok(HistoryStep::Finished) => return self.finish(applied),
                Err(error) => {
                    return self.reattach(&format!("history decode failed: {error}"), applied)
                }
            }
        }
    }

    /// The payload is whole: its terminal becomes the screen.
    fn finish(&mut self, applied: &mut Applied) {
        let Phase::Hydrating(hydration) = std::mem::replace(&mut self.phase, Phase::Live) else {
            return;
        };
        let Hydration {
            hydrator,
            built_at: geometry,
            ..
        } = *hydration;
        let terminal = match hydrator {
            Hydrator::Snapshot(decoder) => match decoder.finish() {
                // The same handle that has been drawn since READY.
                Ok(decoded) => decoded.terminal,
                Err(error) => {
                    self.rendered_changed();
                    return self.reattach(&format!("snapshot finish failed: {error}"), applied);
                }
            },
            Hydrator::Vt(terminal) => {
                self.rendered_changed();
                terminal
            }
        };
        self.screen = Some(Screen { terminal, geometry });
        self.off_resume_point = false;
        self.scroll = TerminalScroll::new();
        applied.frame_dirty = true;
        self.set_status(AttachState::Live, None, applied);
    }

    fn pty(&mut self, bytes: Vec<u8>, applied: &mut Applied) {
        match &mut self.phase {
            Phase::Hydrating(hydration) => match &mut hydration.hydrator {
                Hydrator::Snapshot(decoder) if hydration.ready => {
                    if let Err(error) = decoder.vt_write(&bytes) {
                        return self.reattach(&format!("hydration write failed: {error}"), applied);
                    }
                    applied.frame_dirty = true;
                }
                _ => {
                    hydration.deferred_bytes += bytes.len();
                    if hydration.deferred_bytes > MAX_DEFERRED_BYTES {
                        return self.reattach("peer streamed PTY past the hold budget", applied);
                    }
                    hydration.deferred.push_back(Deferred::Pty(bytes));
                }
            },
            Phase::Live => {
                if let Some(screen) = &mut self.screen {
                    screen.terminal.vt_write(&bytes);
                    applied.frame_dirty = true;
                }
            }
            Phase::Waiting if !self.reattach_pending => {
                self.reattach("PTY outside a stream", applied)
            }
            Phase::Waiting | Phase::Over => {}
        }
    }

    /// Follow the tab's grid at this point in the stream — never echoed:
    /// no RESIZE goes back for it.
    fn grid(&mut self, grid: Geometry, applied: &mut Applied) {
        let resized = match &mut self.phase {
            Phase::Hydrating(hydration) => match &mut hydration.hydrator {
                Hydrator::Snapshot(decoder) if hydration.ready => {
                    hydration.built_at = grid;
                    // Forfeits the history pages still to come; a
                    // re-attach recovers them.
                    decoder.resize(
                        grid.cols,
                        grid.rows,
                        u32::from(grid.cell_w_px),
                        u32::from(grid.cell_h_px),
                    )
                }
                _ => {
                    hydration.deferred.push_back(Deferred::Grid(grid));
                    return;
                }
            },
            Phase::Live => match &mut self.screen {
                Some(screen) => {
                    screen.geometry = grid;
                    screen.terminal.resize(
                        grid.cols,
                        grid.rows,
                        u32::from(grid.cell_w_px),
                        u32::from(grid.cell_h_px),
                    )
                }
                None => Ok(()),
            },
            Phase::Waiting if !self.reattach_pending => {
                return self.reattach("GRID outside a stream", applied)
            }
            Phase::Waiting | Phase::Over => return,
        };
        match resized {
            Ok(()) => applied.frame_dirty = true,
            Err(error) => self.reattach(&format!("GRID failed: {error}"), applied),
        }
    }

    fn note_owner(&mut self, owner: Option<SizeOwner>, at: Geometry, applied: &mut Applied) {
        let view = OwnerView {
            owner,
            cols: at.cols,
            rows: at.rows,
        };
        if self.owner.as_ref() != Some(&view) {
            self.owner = Some(view.clone());
            applied.owner = Some(view);
        }
    }

    fn set_status(&mut self, state: AttachState, message: Option<String>, applied: &mut Applied) {
        if self.status != state || message.is_some() {
            self.status = state;
            applied.status = Some(Status { state, message });
        }
    }

    fn rendered_changed(&mut self) {
        self.rendered_generation += 1;
    }

    /// Drop a hydration in flight; true when there was one. The screen
    /// it was being built beside is drawn again.
    fn abandon(&mut self, applied: &mut Applied) -> bool {
        if !matches!(self.phase, Phase::Hydrating(_)) {
            return false;
        }
        let Phase::Hydrating(hydration) = std::mem::replace(&mut self.phase, Phase::Waiting) else {
            return false;
        };
        let Hydration {
            hydrator, ready, ..
        } = *hydration;
        self.off_resume_point = true;
        if ready {
            self.rendered_changed();
            applied.frame_dirty = true;
        }
        match hydrator {
            Hydrator::Snapshot(decoder) => drop(decoder.abandon()),
            Hydrator::Vt(terminal) => drop(terminal),
        }
        true
    }

    /// The stream is over for good: keep the best picture there is. A
    /// snapshot past READY is the newest screen, short only of history,
    /// and libghostty leaves an abandoned decode's terminal usable.
    fn salvage(&mut self, applied: &mut Applied) {
        let Phase::Hydrating(hydration) = std::mem::replace(&mut self.phase, Phase::Waiting) else {
            return;
        };
        let Hydration {
            hydrator,
            ready,
            built_at: geometry,
            ..
        } = *hydration;
        self.off_resume_point = true;
        match hydrator {
            Hydrator::Snapshot(decoder) if ready => {
                if let Some(terminal) = decoder.abandon() {
                    self.screen = Some(Screen { terminal, geometry });
                    return;
                }
                self.rendered_changed();
                applied.frame_dirty = true;
            }
            Hydrator::Snapshot(decoder) => drop(decoder.abandon()),
            Hydrator::Vt(terminal) => drop(terminal),
        }
    }

    fn reattach(&mut self, reason: &str, applied: &mut Applied) {
        self.abandon(applied);
        self.phase = Phase::Waiting;
        self.request_reattach(reason.to_string(), applied);
        self.set_status(AttachState::Attaching, Some(reason.to_string()), applied);
    }

    fn request_reattach(&mut self, reason: String, applied: &mut Applied) {
        tracing::debug!(%reason, "client terminal needs a fresh snapshot");
        self.off_resume_point = true;
        self.reattach_pending = true;
        applied.reattach = Some(reason);
    }

    /// Recolor with a new theme: the screen, a payload being built, and
    /// every terminal built from here on. A program's own color
    /// overrides survive it.
    pub fn set_theme(&mut self, theme: Theme) -> roost_vt::Result<()> {
        self.theme = theme;
        self.theme_generation += 1;
        let theme = &self.theme;
        if let Some(screen) = &mut self.screen {
            paint(&mut screen.terminal, theme)?;
        }
        if let Phase::Hydrating(hydration) = &mut self.phase {
            match &mut hydration.hydrator {
                Hydrator::Vt(terminal) => paint(terminal, theme)?,
                Hydrator::Snapshot(decoder) if hydration.ready => decoder.set_default_colors(
                    theme.foreground,
                    theme.background,
                    theme.cursor,
                    &theme.palette,
                )?,
                Hydrator::Snapshot(_) => {}
            }
        }
        Ok(())
    }

    /// The screen, when it is the terminal frames are drawn from and so
    /// the one a scroll or a snap may move. A snapshot hydrating past
    /// READY is drawn from inside its decoder, which lends no `&mut`.
    fn screen_mut(&mut self) -> Option<&mut Terminal> {
        match &self.phase {
            Phase::Hydrating(hydration) if hydration.ready => None,
            _ => self.screen.as_mut().map(|screen| &mut screen.terminal),
        }
    }

    /// Back to the live bottom before input reaches the program.
    fn snap_to_bottom(&mut self) -> bool {
        let mut scroll = std::mem::take(&mut self.scroll);
        let snapped = self
            .screen_mut()
            .is_some_and(|terminal| scroll.snap_to_bottom(terminal));
        self.scroll = scroll;
        snapped
    }

    pub fn key(&mut self, input: &KeyInput) -> roost_vt::Result<Input> {
        let terminal = rendered(&self.phase, &self.screen).map(|(terminal, _)| terminal);
        let bytes = keys::encode_key(&mut self.keys, terminal, input)?;
        Ok(self.typed(bytes))
    }

    /// Committed soft-keyboard text.
    pub fn text(&mut self, text: &str) -> roost_vt::Result<Input> {
        let terminal = rendered(&self.phase, &self.screen).map(|(terminal, _)| terminal);
        let bytes = keys::encode_text(&mut self.keys, terminal, text)?;
        Ok(self.typed(bytes))
    }

    pub fn paste(&mut self, text: &str) -> Input {
        let bracketed = rendered(&self.phase, &self.screen)
            .is_some_and(|(terminal, _)| terminal.mode_get(2004));
        self.typed(keys::paste_bytes(text, bracketed))
    }

    fn typed(&mut self, bytes: Vec<u8>) -> Input {
        let frame_dirty = !bytes.is_empty() && self.snap_to_bottom();
        Input {
            bytes,
            take: InputTake::Before,
            frame_dirty,
        }
    }

    /// Scroll by `rows`, positive toward older history. The terminal's
    /// own viewport moves unless the program reads the wheel: mouse
    /// tracking gets one wheel report per row aimed at (`col`, `row`),
    /// the alternate screen one arrow key per row. The bytes are the
    /// caller's to send, taking the size after them.
    pub fn scroll(&mut self, rows: i32, col: u16, row: u16) -> roost_vt::Result<Input> {
        let mut scroll = std::mem::take(&mut self.scroll);
        let route = self
            .screen_mut()
            .and_then(|terminal| scroll.route(terminal, f64::from(rows)));
        self.scroll = scroll;
        let mut input = Input {
            take: InputTake::After,
            ..Input::default()
        };
        let Some(screen) = &self.screen else {
            return Ok(input);
        };
        match route {
            Some(ScrollRoute::MouseReport { direction, rows }) => {
                let geometry = screen.geometry;
                let cell_w = u32::from(geometry.cell_w_px.max(1));
                let cell_h = u32::from(geometry.cell_h_px.max(1));
                self.mouse.sync_from_terminal(&screen.terminal);
                self.mouse.set_size(
                    u32::from(geometry.cols) * cell_w,
                    u32::from(geometry.rows) * cell_h,
                    cell_w,
                    cell_h,
                );
                let mut event = MouseEvent::new()?;
                event
                    .set_action(mouse_action::PRESS)
                    .set_button(match direction {
                        ScrollDirection::History => mouse_button::FOUR,
                        ScrollDirection::Bottom => mouse_button::FIVE,
                    })
                    .set_mods(0)
                    .set_position(
                        (u32::from(col) * cell_w) as f32 + cell_w as f32 / 2.0,
                        (u32::from(row) * cell_h) as f32 + cell_h as f32 / 2.0,
                    );
                for _ in 0..rows {
                    input.bytes.extend(self.mouse.encode(&event)?);
                }
            }
            Some(ScrollRoute::AlternateScreenKey { direction, rows }) => {
                let mut event = KeyEvent::new()?;
                event
                    .set_action(key_action::PRESS)
                    .set_key(match direction {
                        ScrollDirection::History => roost_vt::ffi::GhosttyKey_GHOSTTY_KEY_ARROW_UP,
                        ScrollDirection::Bottom => roost_vt::ffi::GhosttyKey_GHOSTTY_KEY_ARROW_DOWN,
                    })
                    .set_mods(0);
                self.keys.sync_from_terminal(&screen.terminal);
                for _ in 0..rows {
                    input.bytes.extend(self.keys.encode(&event)?);
                }
            }
            Some(ScrollRoute::LocalViewport { .. }) => input.frame_dirty = true,
            None => {}
        }
        Ok(input)
    }

    /// The next frame (C6), or `None` when nothing changed since the
    /// last one. `full` asks for every row whatever moved — a consumer
    /// that lost its row cache. Also `None` before any payload has
    /// become drawable.
    pub fn take_frame(&mut self, full: bool) -> roost_vt::Result<Option<Vec<u8>>> {
        let theme_generation = self.theme_generation;
        let bold = self.theme.bold_color;
        let theme_cursor = self.theme.cursor;
        let basis = &mut self.basis;
        let render = &mut self.render;
        let Some((terminal, geometry)) = sync_render(
            &self.phase,
            &self.screen,
            render,
            basis,
            self.rendered_generation,
        )?
        else {
            return Ok(None);
        };
        let colors = render.colors()?;
        let defaults = (frame::rgb(colors.foreground), frame::rgb(colors.background));
        let grid = (geometry.cols, geometry.rows);
        // Every run carries resolved colors, so a default-color change
        // (OSC 10/11, a theme) re-sends rows libghostty reports clean.
        let full = full
            || std::mem::take(&mut basis.full_pending)
            || basis.grid != grid
            || basis.defaults != Some(defaults)
            || basis.theme_generation != theme_generation;
        if full {
            render.mark_full()?;
        }
        let mut rows = Vec::new();
        let walked = render.walk_dirty(terminal, |row, cells: &[Cell]| {
            let Ok(index) = u16::try_from(row) else {
                return;
            };
            if index < geometry.rows {
                rows.push(frame::Row {
                    index,
                    runs: frame::row_runs(
                        cells,
                        (colors.foreground, colors.background),
                        bold,
                        geometry.cols,
                    ),
                });
            }
        })?;
        let full = full || walked == Dirty::Full;

        let cursor = render.cursor();
        let scrollbar = terminal.scrollbar().ok();
        let mut flags = 0;
        for (on, bit) in [
            (full, frame::flags::FULL),
            (
                cursor.is_some_and(|cursor| cursor.visible),
                frame::flags::CURSOR_VISIBLE,
            ),
            (terminal.mouse_tracking(), frame::flags::MOUSE_TRACKING),
            (
                terminal.active_screen() == ActiveScreen::Alternate,
                frame::flags::ALT_SCREEN,
            ),
            (terminal.mode_get(2004), frame::flags::BRACKETED_PASTE),
        ] {
            if on {
                flags |= bit;
            }
        }
        let to_u32 = |value: u64| u32::try_from(value).unwrap_or(u32::MAX);
        let header = Header {
            flags,
            cols: geometry.cols,
            rows: geometry.rows,
            default_fg: defaults.0,
            default_bg: defaults.1,
            cursor_color: frame::rgb(colors.cursor.unwrap_or(theme_cursor)),
            // A cursor on a wide character's second cell is drawn on the
            // character.
            cursor_col: cursor.map_or(0, |cursor| {
                let col = if cursor.wide_tail {
                    cursor.col.saturating_sub(1)
                } else {
                    cursor.col
                };
                u16::try_from(col).unwrap_or(u16::MAX)
            }),
            cursor_row: cursor.map_or(0, |cursor| u16::try_from(cursor.row).unwrap_or(u16::MAX)),
            cursor_style: cursor.map_or(frame::cursor_style::BLOCK, |cursor| {
                frame::cursor_style_code(cursor.visual_style)
            }),
            scroll_total: scrollbar.map_or(0, |bar| to_u32(bar.total)),
            scroll_offset: scrollbar.map_or(0, |bar| to_u32(bar.offset)),
            scroll_len: scrollbar.map_or(0, |bar| to_u32(bar.len)),
        };
        if !full && rows.is_empty() && basis.header.as_ref() == Some(&header) {
            return Ok(None);
        }
        let mut sent = header.clone();
        sent.flags &= !frame::flags::FULL;
        basis.header = Some(sent);
        basis.grid = grid;
        basis.defaults = Some(defaults);
        basis.theme_generation = theme_generation;
        Ok(Some(Frame { header, rows }.encode()))
    }

    /// The drawn terminal's viewport as text, one trimmed line per row —
    /// what `tab.dump` reports for a desktop tab.
    pub fn viewport_text(&mut self) -> roost_vt::Result<Vec<String>> {
        let bold = self.theme.bold_color;
        let render = &mut self.render;
        let Some((terminal, geometry)) = sync_render(
            &self.phase,
            &self.screen,
            render,
            &mut self.basis,
            self.rendered_generation,
        )?
        else {
            return Ok(Vec::new());
        };
        let colors = render.colors()?;
        let mut grid: Vec<Vec<Cell>> = vec![Vec::new(); usize::from(geometry.rows)];
        render.walk(terminal, |row, cell| {
            if let Some(line) = grid.get_mut(row as usize) {
                line.push(cell);
            }
        })?;
        Ok(grid
            .iter()
            .map(|cells| {
                RenderedRow::build(
                    cells,
                    (colors.foreground, colors.background),
                    bold,
                    geometry.cols,
                )
                .text
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use roost_vt::{ready_boundary, vt_snapshot, ColorRgb};

    use super::*;
    use crate::frame::{decode, row_text};

    const AT: Geometry = Geometry {
        cols: 20,
        rows: 5,
        cell_w_px: 8,
        cell_h_px: 16,
    };

    fn theme() -> Theme {
        Theme::roost_dark_fallback()
    }

    fn tab() -> TabTerminal {
        TabTerminal::new(theme()).expect("tab terminal")
    }

    /// The session's terminal: history, a prompt, and a parser that can
    /// be snapshotted wherever it stopped.
    fn source(at: Geometry, lines: usize) -> Terminal {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: at.cols,
            rows: at.rows,
            max_scrollback: 200,
            continuation_max_bytes: 4096,
            ..Default::default()
        })
        .expect("source terminal");
        terminal
            .resize(
                at.cols,
                at.rows,
                u32::from(at.cell_w_px),
                u32::from(at.cell_h_px),
            )
            .expect("cell size");
        for line in 0..lines {
            terminal.vt_write(format!("line-{line:02}\r\n").as_bytes());
        }
        terminal.vt_write(b"\x1b[1m$\x1b[0m ");
        terminal
    }

    fn text_of(terminal: &Terminal) -> Vec<String> {
        let mut render = RenderState::new().expect("render state");
        render.update(terminal).expect("update");
        let colors = render.colors().expect("colors");
        let mut grid: Vec<Vec<Cell>> = Vec::new();
        render
            .walk(terminal, |row, cell| {
                let row = row as usize;
                if grid.len() <= row {
                    grid.resize(row + 1, Vec::new());
                }
                grid[row].push(cell);
            })
            .expect("walk");
        grid.iter()
            .map(|cells| {
                RenderedRow::build(cells, (colors.foreground, colors.background), None, 200).text
            })
            .collect()
    }

    fn accepted(kind: PayloadKind, mode: AttachMode, at: Geometry) -> AttachEvent {
        AttachEvent::Accepted {
            kind,
            mode,
            cols: at.cols,
            rows: at.rows,
            cell_w_px: at.cell_w_px,
            cell_h_px: at.cell_h_px,
            owner: Some(SizeOwner {
                client_id: "desk".into(),
                label: "desktop".into(),
            }),
        }
    }

    fn grid_event(at: Geometry) -> AttachEvent {
        AttachEvent::Grid {
            cols: at.cols,
            rows: at.rows,
            cell_w_px: at.cell_w_px,
            cell_h_px: at.cell_h_px,
            owner: None,
        }
    }

    /// Every row of a FULL frame, as text.
    fn frame_rows(frame: &Frame) -> Vec<String> {
        assert_ne!(
            frame.header.flags & frame::flags::FULL,
            0,
            "expected a FULL frame"
        );
        assert_eq!(frame.rows.len(), usize::from(frame.header.rows));
        frame
            .rows
            .iter()
            .enumerate()
            .map(|(at, row)| {
                assert_eq!(usize::from(row.index), at);
                row_text(row, frame.header.cols)
            })
            .collect()
    }

    fn pull(tab: &mut TabTerminal, full: bool) -> Option<Frame> {
        tab.take_frame(full)
            .expect("take_frame")
            .map(|bytes| decode(&bytes).expect("decodes"))
    }

    /// Hydrate `tab` from `snap`, all at once, to Live.
    fn hydrate(tab: &mut TabTerminal, snap: &[u8], at: Geometry) {
        tab.apply(accepted(
            PayloadKind::GhosttySnapshot,
            AttachMode::Snapshot,
            at,
        ));
        let applied = tab.apply(AttachEvent::Snap(snap.to_vec()));
        assert_eq!(tab.status(), AttachState::Live, "{applied:?}");
    }

    #[test]
    fn a_snapshot_hydrates_across_awkward_chunks_with_pty_around_ready() {
        let mut src = source(AT, 12);
        let snap = src.snapshot().expect("snapshot");
        let ready_at = ready_boundary(&snap).expect("READY");
        assert!(
            ready_at > 20 && ready_at < snap.len(),
            "history follows READY"
        );

        let mut tab = tab();
        let applied = tab.apply(accepted(
            PayloadKind::GhosttySnapshot,
            AttachMode::Snapshot,
            AT,
        ));
        assert_eq!(
            applied.owner.as_ref().map(|view| (view.cols, view.rows)),
            Some((20, 5))
        );
        assert_eq!(applied.status, None, "still attaching");

        let chunks = [0..1, 1..8, 8..ready_at - 3];
        for chunk in chunks {
            let applied = tab.apply(AttachEvent::Snap(snap[chunk].to_vec()));
            assert_eq!(applied, Applied::default(), "nothing drawable before READY");
        }
        let held = tab.apply(AttachEvent::Pty(b"ab".to_vec()));
        assert!(!held.frame_dirty, "PTY before READY waits");
        assert!(tab.take_frame(false).expect("take").is_none());

        let across = tab.apply(AttachEvent::Snap(snap[ready_at - 3..ready_at + 5].to_vec()));
        assert!(across.frame_dirty, "READY makes the snapshot drawable");
        assert_eq!(
            tab.status(),
            AttachState::Attaching,
            "history still to come"
        );
        assert!(tab.apply(AttachEvent::Pty(b"c\r\n".to_vec())).frame_dirty);

        let mut last = Applied::default();
        for chunk in snap[ready_at + 5..].chunks(13) {
            last = tab.apply(AttachEvent::Snap(chunk.to_vec()));
        }
        assert_eq!(
            last.status,
            Some(Status {
                state: AttachState::Live,
                message: None
            })
        );
        assert_eq!(last.reattach, None);

        src.vt_write(b"ab");
        src.vt_write(b"c\r\n");
        let expected = text_of(&src);
        assert_eq!(tab.viewport_text().expect("text"), expected);

        let frame = pull(&mut tab, false).expect("a frame");
        assert_eq!(frame_rows(&frame), expected);
        assert_eq!((frame.header.cols, frame.header.rows), (20, 5));
        let bar = src.scrollbar().expect("scrollbar");
        assert_eq!(
            u64::from(frame.header.scroll_total),
            bar.total,
            "the history pages landed"
        );
        assert_eq!(
            frame.header.default_bg,
            frame::rgb(theme().background),
            "the client's theme, not the session's"
        );
        assert_ne!(frame.header.flags & frame::flags::CURSOR_VISIBLE, 0);
        assert_eq!((frame.header.cursor_col, frame.header.cursor_row), (0, 4));

        assert!(pull(&mut tab, false).is_none(), "nothing new");
        assert!(tab.apply(AttachEvent::Pty(b"zz".to_vec())).frame_dirty);
        let next = pull(&mut tab, false).expect("the typed row");
        assert_eq!(next.header.flags & frame::flags::FULL, 0);
        assert_eq!(next.rows.len(), 1);
        assert_eq!(row_text(&next.rows[0], 20), "zz");
        assert_eq!(next.header.cursor_col, 2);
    }

    #[test]
    fn a_vt_payload_is_drawn_only_at_its_terminator() {
        let mut src = source(AT, 3);
        src.vt_write(b"\x1b[31mred\x1b[0m");
        let payload = vt_snapshot(&src, &mut RenderState::new().expect("render"))
            .expect("compose")
            .expect("at ground");

        let mut tab = tab();
        tab.apply(accepted(PayloadKind::Vt, AttachMode::Snapshot, AT));
        for chunk in payload.chunks(7) {
            assert!(!tab.apply(AttachEvent::Snap(chunk.to_vec())).frame_dirty);
        }
        assert!(!tab.apply(AttachEvent::Pty(b"!".to_vec())).frame_dirty);
        assert!(tab.take_frame(false).expect("take").is_none());
        assert!(tab.viewport_text().expect("text").is_empty());

        let done = tab.apply(AttachEvent::Snap(Vec::new()));
        assert!(done.frame_dirty);
        assert_eq!(tab.status(), AttachState::Live);

        src.vt_write(b"!");
        assert_eq!(tab.viewport_text().expect("text"), text_of(&src));
        let frame = pull(&mut tab, false).expect("frame");
        assert_eq!(frame_rows(&frame), text_of(&src));
        let red = frame.rows[3]
            .runs
            .iter()
            .find(|run| run.text.starts_with("red"))
            .expect("the red run");
        assert_eq!(
            red.fg,
            frame::rgb(theme().palette[1]),
            "palette 1 in this theme"
        );

        let stray = tab.apply(AttachEvent::Snap(b"more".to_vec()));
        assert!(stray.reattach.is_some(), "SNAP after the terminator");
    }

    #[test]
    fn a_grid_before_ready_is_replayed_at_ready() {
        let src = source(AT, 12);
        let snap = src.snapshot().expect("snapshot");
        let ready_at = ready_boundary(&snap).expect("READY");
        let wider = Geometry {
            cols: 30,
            rows: 6,
            ..AT
        };

        let mut tab = tab();
        tab.apply(accepted(
            PayloadKind::GhosttySnapshot,
            AttachMode::Snapshot,
            AT,
        ));
        tab.apply(AttachEvent::Snap(snap[..ready_at - 1].to_vec()));
        let held = tab.apply(grid_event(wider));
        assert!(!held.frame_dirty);
        assert_eq!(
            held.owner,
            Some(OwnerView {
                owner: None,
                cols: 30,
                rows: 6
            })
        );
        tab.apply(AttachEvent::Snap(snap[ready_at - 1..].to_vec()));
        assert_eq!(tab.status(), AttachState::Live);
        assert_eq!(tab.geometry(), Some(wider));

        let frame = pull(&mut tab, false).expect("frame");
        assert_eq!((frame.header.cols, frame.header.rows), (30, 6));
        let rows = frame_rows(&frame);
        assert_eq!(rows.last().map(String::as_str), Some("$"));
    }

    #[test]
    fn a_grid_mid_history_resizes_the_drawn_terminal() {
        let src = source(AT, 40);
        let snap = src.snapshot().expect("snapshot");
        let ready_at = ready_boundary(&snap).expect("READY");
        let narrow = Geometry {
            cols: 12,
            rows: 4,
            ..AT
        };

        let mut tab = tab();
        tab.apply(accepted(
            PayloadKind::GhosttySnapshot,
            AttachMode::Snapshot,
            AT,
        ));
        tab.apply(AttachEvent::Snap(snap[..ready_at].to_vec()));
        let first = pull(&mut tab, false).expect("drawable at READY");
        assert_eq!((first.header.cols, first.header.rows), (20, 5));

        assert!(tab.apply(grid_event(narrow)).frame_dirty);
        tab.apply(AttachEvent::Snap(snap[ready_at..].to_vec()));
        assert_eq!(tab.status(), AttachState::Live);
        let frame = pull(&mut tab, false).expect("frame");
        assert_eq!((frame.header.cols, frame.header.rows), (12, 4));
        assert_ne!(
            frame.header.flags & frame::flags::FULL,
            0,
            "a new grid is FULL"
        );
        assert_eq!(frame_rows(&frame).last().map(String::as_str), Some("$"));
    }

    #[test]
    fn a_live_grid_follows_the_pty_and_sends_everything() {
        let src = source(AT, 2);
        let mut tab = tab();
        hydrate(&mut tab, &src.snapshot().expect("snapshot"), AT);
        pull(&mut tab, false).expect("first frame");

        let taller = Geometry { rows: 8, ..AT };
        assert!(tab.apply(grid_event(taller)).frame_dirty);
        let frame = pull(&mut tab, false).expect("frame");
        assert_eq!(frame.header.rows, 8);
        assert_eq!(frame.rows.len(), 8, "FULL after a resize");
    }

    #[test]
    fn a_resume_keeps_the_terminal_and_appends() {
        let src = source(AT, 2);
        let mut tab = tab();
        hydrate(&mut tab, &src.snapshot().expect("snapshot"), AT);
        let before = tab.viewport_text().expect("text");

        let dropped = tab.apply(AttachEvent::Reconnecting {
            reason: "eof".into(),
        });
        assert_eq!(
            dropped.reattach, None,
            "a live terminal is at the resume point"
        );
        assert_eq!(tab.status(), AttachState::Reconnecting);
        assert_eq!(tab.viewport_text().expect("text"), before);

        let resumed = tab.apply(accepted(
            PayloadKind::GhosttySnapshot,
            AttachMode::Resume,
            AT,
        ));
        assert_eq!(resumed.reattach, None);
        assert_eq!(
            resumed.status.map(|status| status.state),
            Some(AttachState::Live)
        );
        assert_eq!(tab.viewport_text().expect("text"), before);
        tab.apply(AttachEvent::Pty(b"resumed".to_vec()));
        let text = tab.viewport_text().expect("text");
        assert_eq!(text[..2], before[..2]);
        assert_eq!(text[2], "$ resumed");
    }

    #[test]
    fn a_resume_the_terminal_cannot_honor_asks_for_a_snapshot() {
        let snap = source(AT, 2).snapshot().expect("snapshot");

        let mut never = tab();
        let applied = never.apply(accepted(
            PayloadKind::GhosttySnapshot,
            AttachMode::Resume,
            AT,
        ));
        assert!(applied.reattach.is_some(), "no terminal to resume onto");

        let mut regridded = tab();
        hydrate(&mut regridded, &snap, AT);
        let other = Geometry { cols: 21, ..AT };
        let applied = regridded.apply(accepted(
            PayloadKind::GhosttySnapshot,
            AttachMode::Resume,
            other,
        ));
        assert!(applied.reattach.is_some(), "a resume for another grid");

        let mut mid = tab();
        mid.apply(accepted(
            PayloadKind::GhosttySnapshot,
            AttachMode::Snapshot,
            AT,
        ));
        mid.apply(AttachEvent::Snap(snap[..snap.len() / 2].to_vec()));
        let applied = mid.apply(AttachEvent::Reconnecting {
            reason: "eof".into(),
        });
        assert!(applied.reattach.is_some(), "a drop mid-hydration");
        let applied = mid.apply(accepted(
            PayloadKind::GhosttySnapshot,
            AttachMode::Resume,
            AT,
        ));
        assert!(applied.reattach.is_some(), "and a resume after it");
    }

    #[test]
    fn pty_past_the_hold_budget_rebuilds_the_stream() {
        for kind in [PayloadKind::GhosttySnapshot, PayloadKind::Vt] {
            let mut tab = tab();
            tab.apply(accepted(kind, AttachMode::Snapshot, AT));
            let half = vec![b'x'; MAX_DEFERRED_BYTES / 2];
            assert_eq!(tab.apply(AttachEvent::Pty(half.clone())).reattach, None);
            assert_eq!(tab.apply(AttachEvent::Pty(half)).reattach, None);
            let over = tab.apply(AttachEvent::Pty(b"!".to_vec()));
            assert!(over.reattach.is_some(), "{kind:?}");
            assert_eq!(tab.status(), AttachState::Attaching);
        }
    }

    #[test]
    fn a_reattach_keeps_drawing_the_last_screen() {
        let snap = source(AT, 2).snapshot().expect("snapshot");
        let mut tab = tab();
        hydrate(&mut tab, &snap, AT);
        let before = tab.viewport_text().expect("text");
        pull(&mut tab, false);

        tab.apply(accepted(
            PayloadKind::GhosttySnapshot,
            AttachMode::Snapshot,
            AT,
        ));
        tab.apply(AttachEvent::Snap(
            snap[..ready_boundary(&snap).expect("READY")].to_vec(),
        ));
        let broken = tab.apply(AttachEvent::Snap(vec![0xff; 64]));
        assert!(broken.reattach.is_some(), "a corrupt record");
        assert_eq!(tab.viewport_text().expect("text"), before);
        let frame = pull(&mut tab, false).expect("the old screen again");
        assert_eq!(frame_rows(&frame), before);
    }

    #[test]
    fn stream_confusion_asks_for_a_snapshot_once() {
        for stray in [
            AttachEvent::Snap(b"x".to_vec()),
            AttachEvent::Pty(b"x".to_vec()),
            grid_event(AT),
        ] {
            let mut tab = tab();
            assert!(tab.apply(stray.clone()).reattach.is_some(), "{stray:?}");
            assert_eq!(
                tab.apply(stray.clone()),
                Applied::default(),
                "the old stream's leftovers while the re-attach is pending"
            );
            tab.apply(accepted(PayloadKind::Vt, AttachMode::Snapshot, AT));
            tab.apply(AttachEvent::Snap(Vec::new()));
            assert_eq!(tab.status(), AttachState::Live);
        }
    }

    #[test]
    fn a_resume_after_a_dropped_hydration_asks_for_a_snapshot() {
        let snap = source(AT, 2).snapshot().expect("snapshot");
        let ready_at = ready_boundary(&snap).expect("READY");
        let mut tab = tab();
        hydrate(&mut tab, &snap, AT);

        // A second attach drops before its payload is whole. The screen
        // left standing is the first stream's, at the same grid; the
        // stream's resume point is the second one's.
        tab.apply(accepted(
            PayloadKind::GhosttySnapshot,
            AttachMode::Snapshot,
            AT,
        ));
        tab.apply(AttachEvent::Snap(snap[..ready_at].to_vec()));
        let dropped = tab.apply(AttachEvent::Reconnecting {
            reason: "eof".into(),
        });
        assert!(dropped.reattach.is_some());
        let resumed = tab.apply(accepted(
            PayloadKind::GhosttySnapshot,
            AttachMode::Resume,
            AT,
        ));
        assert!(
            resumed.reattach.is_some(),
            "the old screen is not at that point"
        );

        hydrate(&mut tab, &snap, AT);
        tab.apply(AttachEvent::Reconnecting {
            reason: "eof".into(),
        });
        let resumed = tab.apply(accepted(
            PayloadKind::GhosttySnapshot,
            AttachMode::Resume,
            AT,
        ));
        assert_eq!(resumed.reattach, None, "a finished payload resumes again");
    }

    #[test]
    fn exit_ends_the_tab_and_keeps_its_screen() {
        let snap = source(AT, 2).snapshot().expect("snapshot");
        let mut tab = tab();
        hydrate(&mut tab, &snap, AT);
        let before = tab.viewport_text().expect("text");
        assert_eq!(tab.apply(AttachEvent::Exit { code: 3 }).exited, Some(3));
        assert_eq!(
            tab.apply(AttachEvent::Pty(b"late".to_vec())),
            Applied::default()
        );
        assert_eq!(tab.viewport_text().expect("text"), before);
    }

    #[test]
    fn a_stream_ending_past_ready_keeps_the_newest_screen() {
        let src = source(AT, 40);
        let snap = src.snapshot().expect("snapshot");
        let mut tab = tab();
        tab.apply(accepted(
            PayloadKind::GhosttySnapshot,
            AttachMode::Snapshot,
            AT,
        ));
        tab.apply(AttachEvent::Snap(
            snap[..ready_boundary(&snap).expect("READY")].to_vec(),
        ));
        let ended = tab.apply(AttachEvent::Ended {
            reason: "detached".into(),
        });
        assert_eq!(
            ended.status.map(|status| status.state),
            Some(AttachState::Ended)
        );
        assert_eq!(tab.viewport_text().expect("text"), text_of(&src));
    }

    #[test]
    fn scrolling_moves_the_viewport_and_typing_snaps_it_back() {
        let snap = source(AT, 30).snapshot().expect("snapshot");
        let mut tab = tab();
        hydrate(&mut tab, &snap, AT);
        let live = pull(&mut tab, false).expect("frame").header;

        let scrolled = tab.scroll(3, 0, 0).expect("scroll");
        assert!(scrolled.bytes.is_empty());
        assert!(scrolled.frame_dirty);
        let back = pull(&mut tab, false).expect("frame").header;
        assert_eq!(back.scroll_offset + 3, live.scroll_offset);
        assert_eq!(
            back.flags & frame::flags::CURSOR_VISIBLE,
            0,
            "the cursor scrolled away"
        );

        let typed = tab
            .key(&KeyInput {
                key: "a".into(),
                ..Default::default()
            })
            .expect("key");
        assert_eq!(typed.bytes, b"a");
        assert_eq!(typed.take, InputTake::Before);
        assert!(typed.frame_dirty, "typing snaps to the bottom");
        let bottom = pull(&mut tab, false).expect("frame").header;
        assert_eq!(bottom.scroll_offset, live.scroll_offset);
    }

    #[test]
    fn scrolling_a_program_that_reads_the_wheel_makes_bytes() {
        let snap = source(AT, 2).snapshot().expect("snapshot");
        let mut tab = tab();
        hydrate(&mut tab, &snap, AT);

        tab.apply(AttachEvent::Pty(b"\x1b[?1000h\x1b[?1006h".to_vec()));
        let wheel = tab.scroll(2, 4, 1).expect("scroll");
        assert_eq!(wheel.take, InputTake::After);
        assert_eq!(wheel.bytes, b"\x1b[<64;5;2M\x1b[<64;5;2M");
        let frame = pull(&mut tab, false).expect("frame");
        assert_ne!(frame.header.flags & frame::flags::MOUSE_TRACKING, 0);

        tab.apply(AttachEvent::Pty(b"\x1b[?1000l\x1b[?1049h".to_vec()));
        let arrows = tab.scroll(-2, 0, 0).expect("scroll");
        assert_eq!(arrows.bytes, b"\x1b[B\x1b[B");
        let frame = pull(&mut tab, false).expect("frame");
        assert_ne!(frame.header.flags & frame::flags::ALT_SCREEN, 0);
    }

    #[test]
    fn paste_brackets_when_the_program_asks() {
        let snap = source(AT, 1).snapshot().expect("snapshot");
        let mut tab = tab();
        hydrate(&mut tab, &snap, AT);
        assert_eq!(tab.paste("a\nb").bytes, b"a\rb");
        tab.apply(AttachEvent::Pty(b"\x1b[?2004h".to_vec()));
        assert_eq!(tab.paste("a\nb").bytes, b"\x1b[200~a\nb\x1b[201~");
        let frame = pull(&mut tab, true).expect("frame");
        assert_ne!(frame.header.flags & frame::flags::BRACKETED_PASTE, 0);
    }

    #[test]
    fn a_theme_change_resends_every_row_in_its_colors() {
        let snap = source(AT, 2).snapshot().expect("snapshot");
        let mut tab = tab();
        hydrate(&mut tab, &snap, AT);
        pull(&mut tab, false);

        // Only the bold color moves: libghostty never sees it and the
        // defaults stay, yet the bold prompt must be re-sent in it.
        let mut recolored = theme();
        recolored.bold_color = Some(ColorRgb::new(0x12, 0x34, 0x56));
        tab.set_theme(recolored.clone()).expect("theme");
        let frame = pull(&mut tab, false).expect("frame");
        assert_ne!(frame.header.flags & frame::flags::FULL, 0);
        let prompt = frame.rows[2]
            .runs
            .iter()
            .find(|run| run.text == "$")
            .expect("the bold prompt");
        assert_eq!(prompt.fg, 0x0012_3456);

        let mut light = recolored;
        light.background = ColorRgb::new(0xfa, 0xfa, 0xfa);
        tab.set_theme(light).expect("theme");
        let frame = pull(&mut tab, false).expect("frame");
        assert_ne!(frame.header.flags & frame::flags::FULL, 0);
        assert_eq!(frame.header.default_bg, 0x00fa_fafa);
    }

    #[test]
    fn a_full_frame_on_request_and_an_osc_background_change() {
        let snap = source(AT, 2).snapshot().expect("snapshot");
        let mut tab = tab();
        hydrate(&mut tab, &snap, AT);
        pull(&mut tab, false);

        let again = pull(&mut tab, true).expect("asked for");
        assert_eq!(again.rows.len(), 5);

        tab.apply(AttachEvent::Pty(b"\x1b]11;rgb:00/11/22\x07".to_vec()));
        let frame = pull(&mut tab, false).expect("frame");
        assert_ne!(frame.header.flags & frame::flags::FULL, 0);
        assert_eq!(frame.header.default_bg, 0x0000_1122);
    }
}
