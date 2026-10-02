//! The right-click menu's one dispatcher, and the overlay that draws it
//! on Linux (plan 073 D9).
//!
//! Every surface — the macOS popup, the Linux overlay, the
//! `app.context_menu_*` test ops — lists a row's menu with
//! [`App::context_entries`] and runs an item with
//! [`App::context_activate`]. Neither trusts what the menu showed when it
//! opened: an activation re-reads the row and rebuilds the menu first, so
//! an item whose row closed, whose host reconnected, or which stopped
//! applying is refused rather than run against whatever is there now.

use std::fmt;

use iced::advanced::widget::Tree;
use iced::advanced::{layout, mouse, renderer, Clipboard, Layout, Shell, Widget};
use iced::widget::column;
use iced::{touch, Event, Length, Padding, Point, Rectangle};
use roost_ipc::messages::AppContextMenuEntry;
use roost_ui_model::context_menu::{
    self, ContextAction, ContextEntry, ContextFacts, ContextTarget,
};

use super::*;
use crate::url_launcher;

/// What `app.context_menu_open` answers on macOS.
pub(super) const OPEN_UNSUPPORTED: &str =
    "app.context_menu_open is not supported on macOS: the menu is the native popup";

/// Where `app.context_menu_open` shows the menu: the content area's
/// top-left, inset.
pub(super) const OPEN_ANCHOR: Point = Point::new(40.0, 40.0);

const MENU_PADDING: f32 = 5.0;
const ITEM_HEIGHT: f32 = 24.0;
const ITEM_PADDING_X: f32 = 10.0;
const SEPARATOR_HEIGHT: f32 = 9.0;
const LABEL_SIZE: f32 = 13.0;
const MENU_MIN_WIDTH: f32 = 180.0;
const MENU_MAX_WIDTH: f32 = 360.0;

/// Why a menu item did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContextError {
    /// A modal, the palette, a rename editor or an IME composition owns
    /// input.
    Blocked,
    /// The row is gone, or names a connection that has since been
    /// replaced.
    Missing,
    /// A name off the wire that is no action at all.
    Unknown(String),
    Absent(ContextAction),
    Disabled(ContextAction),
    /// The row's menu has no items to show — a host whose verbs this
    /// build withholds.
    Empty,
    /// A local-backend switch is in flight (plan 063 §D8a).
    Busy,
    /// The action itself refused.
    Failed(String),
}

impl ContextError {
    pub(crate) fn failure(&self) -> HostOpFailure {
        let code = match self {
            Self::Busy => codes::BUSY,
            Self::Failed(_) => codes::INTERNAL,
            Self::Blocked
            | Self::Missing
            | Self::Empty
            | Self::Unknown(_)
            | Self::Absent(_)
            | Self::Disabled(_) => codes::INVALID_PARAM,
        };
        HostOpFailure::new(code, self.to_string())
    }
}

impl fmt::Display for ContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blocked => {
                f.write_str("a dialog, the palette or an editor owns input; no menu item can run")
            }
            Self::Missing => f.write_str("the row this menu names no longer exists"),
            Self::Unknown(name) => write!(f, "{name:?} is not a context-menu action"),
            Self::Empty => f.write_str("this row's menu has no items"),
            Self::Absent(action) => write!(f, "{} is not on this row's menu", action.as_str()),
            Self::Disabled(action) => {
                write!(f, "{} is disabled on this row's menu", action.as_str())
            }
            Self::Busy => f.write_str(roost_ipc::local_route::SWITCH_BUSY_MESSAGE),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

/// What a target names right now, read off the live rows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextRow {
    /// The row's project; `None` for a host band.
    project: Option<ProjectKey>,
    cwd: String,
    saved_host: Option<String>,
    on_this_machine: bool,
    interactive: bool,
}

impl ContextRow {
    fn facts(&self) -> ContextFacts<'_> {
        ContextFacts {
            host: self.saved_host.as_deref(),
            interactive: self.interactive,
            on_this_machine: self.on_this_machine,
            cwd_known: !self.cwd.is_empty(),
        }
    }
}

/// Resolve `target` against the in-process rows and every host band.
///
/// A key is matched on its whole `HostId`, so a key minted against a
/// connection that has since reconnected names nothing (`keys.rs`). A
/// dimmed host's rows still resolve, as not interactive.
fn context_row(
    target: &ContextTarget,
    local: &[Project],
    hosts: &[HostView],
) -> Option<ContextRow> {
    let instance = |host: HostId| {
        if host.is_local() {
            return Some((local, None));
        }
        let view = hosts.iter().find(|view| view.host == host)?;
        Some((view.projects.as_slice(), Some(view)))
    };
    let (project, cwd, view) = match target {
        ContextTarget::Host(saved_id) => {
            let view = hosts.iter().find(|view| &view.saved_id == saved_id)?;
            (None, "", Some(view))
        }
        ContextTarget::Tab(tab) => {
            let (rows, view) = instance(tab.host)?;
            let row = rows
                .iter()
                .find(|row| row.tabs.iter().any(|listed| listed.id == tab.tab))?;
            (
                Some(ProjectKey::new(tab.host, row.id)),
                listed_tab_cwd(row, tab.tab),
                view,
            )
        }
        ContextTarget::Project(project) => {
            let (rows, view) = instance(project.host)?;
            let row = rows.iter().find(|row| row.id == project.project)?;
            (Some(*project), row.cwd.as_str(), view)
        }
    };
    Some(ContextRow {
        project,
        cwd: cwd.to_string(),
        saved_host: view.map(|view| view.saved_id.clone()),
        on_this_machine: view.is_none_or(|view| view.transport.localhost()),
        interactive: view.is_none_or(|view| view.state.interactive()),
    })
}

/// The row `action` may run on: `menu` is that row and the menu it shows
/// now, `None` when the row is gone.
fn admit(
    blocked: bool,
    menu: Option<(ContextRow, Vec<ContextEntry>)>,
    action: ContextAction,
) -> Result<ContextRow, ContextError> {
    if blocked {
        return Err(ContextError::Blocked);
    }
    let (row, entries) = menu.ok_or(ContextError::Missing)?;
    match context_menu::item_enabled(&entries, action) {
        Some(true) => Ok(row),
        Some(false) => Err(ContextError::Disabled(action)),
        None => Err(ContextError::Absent(action)),
    }
}

pub(super) fn wire_entry(entry: &ContextEntry) -> AppContextMenuEntry {
    match entry {
        ContextEntry::Item {
            action,
            label,
            enabled,
        } => AppContextMenuEntry::Item {
            action: action.as_str().to_string(),
            label: label.clone(),
            enabled: *enabled,
        },
        ContextEntry::Separator => AppContextMenuEntry::Separator { separator: true },
    }
}

impl App {
    /// The menu `target` shows now; `None` when its row is gone.
    pub(crate) fn context_entries(&self, target: &ContextTarget) -> Option<Vec<ContextEntry>> {
        self.context_menu(target).map(|(_, entries)| entries)
    }

    fn context_menu(&self, target: &ContextTarget) -> Option<(ContextRow, Vec<ContextEntry>)> {
        let row = context_row(target, &self.projects, &self.host_views)?;
        // The palette's own inputs, so a host's items are the palette's
        // host rows and cannot disagree with them.
        let verbs = match row.saved_host {
            Some(_) => host_verbs::verbs(
                &self.host_verb_rows(),
                &self.host_recent_rows(),
                self.local_slot_input(),
                host_verbs::VerbPolicy::current(),
                self.switch_in_flight(),
                self.local_slot_history(),
            ),
            None => Vec::new(),
        };
        let entries = context_menu::entries(target, &row.facts(), &verbs);
        Some((row, entries))
    }

    /// Run one item of `target`'s menu, as it is now.
    pub(crate) fn context_activate(
        &mut self,
        target: &ContextTarget,
        action: ContextAction,
    ) -> Result<UiTask, ContextError> {
        self.close_context_menu();
        let row = admit(self.context_blocked(), self.context_menu(target), action)?;
        if local_backend::context_action_mutates_local_backend(action) && self.switch_in_flight() {
            return Err(ContextError::Busy);
        }
        self.run_context_action(target, action, row)
    }

    fn context_blocked(&self) -> bool {
        self.text_capture() || self.palette.is_some()
    }

    fn run_context_action(
        &mut self,
        target: &ContextTarget,
        action: ContextAction,
        row: ContextRow,
    ) -> Result<UiTask, ContextError> {
        match (target, action) {
            (ContextTarget::Tab(tab), ContextAction::RenameTab) => {
                self.context_rename(RenameTarget::Tab(*tab))
            }
            (ContextTarget::Project(project), ContextAction::RenameProject) => {
                self.context_rename(RenameTarget::Project(*project))
            }
            (ContextTarget::Tab(tab), ContextAction::NewTabHere) => {
                let project = row.project.ok_or(ContextError::Missing)?;
                Ok(self.new_tab_in(project, Some(*tab)))
            }
            (ContextTarget::Project(project), ContextAction::NewTab) => {
                Ok(self.new_tab_in(*project, None))
            }
            (ContextTarget::Tab(tab), ContextAction::CloseTab) => Ok(self.close_tab(*tab)),
            (ContextTarget::Project(project), ContextAction::CloseProject) => {
                self.confirm_close_project(*project)
                    .map_err(ContextError::Failed)?;
                Ok(UiTask::None)
            }
            (ContextTarget::Tab(_), ContextAction::CopyTabPath)
            | (ContextTarget::Project(_), ContextAction::CopyProjectPath) => {
                self.clipboard.enqueue_write(ClipboardOp::System, row.cwd);
                Ok(self.clipboard.start_next())
            }
            (ContextTarget::Project(_), ContextAction::OpenProjectFolder) => {
                let url = url_launcher::file_url(Path::new(&row.cwd));
                Ok(self.open_external(url_launcher::External::Url(url)))
            }
            _ => {
                let verb = row
                    .saved_host
                    .and_then(|saved_id| action.host_verb(&saved_id))
                    .ok_or(ContextError::Absent(action))?;
                self.run_host_verb(verb, Self::CLICK_ACTIVATION_ORIGIN)
                    .map(|dispatch| dispatch.task)
                    .map_err(ContextError::Failed)
            }
        }
    }

    fn context_rename(&mut self, target: RenameTarget) -> Result<UiTask, ContextError> {
        self.begin_rename_target(target)
            .map_err(ContextError::Failed)?;
        Ok(self.take_rename_focus_task())
    }
}

/// The menu on screen, drawn over the window by
/// [`App::with_context_menu`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OpenContextMenu {
    target: ContextTarget,
    /// The menu as it was when it opened, labels fitted to the panel.
    entries: Vec<ContextEntry>,
    /// Where it was asked for, in window coordinates.
    at: Point,
    size: Size,
    highlighted: Option<usize>,
}

impl OpenContextMenu {
    fn highlighted_action(&self) -> Option<ContextAction> {
        match self.entries.get(self.highlighted?)? {
            ContextEntry::Item {
                action,
                enabled: true,
                ..
            } => Some(*action),
            _ => None,
        }
    }

    /// The pointer moved onto row `index`, or off the menu. Only a row
    /// that can run takes the highlight.
    fn hover(&mut self, index: Option<usize>) {
        self.highlighted = index.filter(|&index| self.entries.get(index).is_some_and(selectable));
    }

    fn step(&mut self, step: MenuStep) {
        self.highlighted = stepped(&self.entries, self.highlighted, step);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuStep {
    Up,
    Down,
    Home,
    End,
}

impl MenuStep {
    fn of(key: &Key<&str>) -> Option<Self> {
        match key {
            Key::Named(Named::ArrowUp) => Some(Self::Up),
            Key::Named(Named::ArrowDown) => Some(Self::Down),
            Key::Named(Named::Home) => Some(Self::Home),
            Key::Named(Named::End) => Some(Self::End),
            _ => None,
        }
    }
}

fn selectable(entry: &ContextEntry) -> bool {
    matches!(entry, ContextEntry::Item { enabled: true, .. })
}

/// Where the highlight lands. Separators and disabled rows are passed
/// over, and Up and Down wrap at the ends, as a native menu's do; with
/// nothing highlighted yet, Down starts at the top and Up at the bottom.
fn stepped(entries: &[ContextEntry], from: Option<usize>, step: MenuStep) -> Option<usize> {
    let rows: Vec<usize> = (0..entries.len())
        .filter(|&index| selectable(&entries[index]))
        .collect();
    let first = rows.first().copied();
    let last = rows.last().copied();
    match (step, from) {
        (MenuStep::Home, _) | (MenuStep::Down, None) => first,
        (MenuStep::End, _) | (MenuStep::Up, None) => last,
        (MenuStep::Down, Some(from)) => rows.iter().copied().find(|&row| row > from).or(first),
        (MenuStep::Up, Some(from)) => rows.iter().copied().rfind(|&row| row < from).or(last),
    }
}

fn entry_height(entry: &ContextEntry) -> f32 {
    match entry {
        ContextEntry::Item { .. } => ITEM_HEIGHT,
        ContextEntry::Separator => SEPARATOR_HEIGHT,
    }
}

/// The menu with every label fitted to the panel, and the panel's size.
/// Computed once, at open: [`place`] needs the size before anything is
/// laid out, so every row is a fixed height and the width is measured.
fn fitted(entries: Vec<ContextEntry>) -> (Vec<ContextEntry>, Size) {
    let font = chrome::chrome_font(font::Weight::Normal);
    let inset = 2.0 * (MENU_PADDING + ITEM_PADDING_X);
    let widest = entries
        .iter()
        .filter_map(|entry| match entry {
            ContextEntry::Item { label, .. } => Some(chrome::text_width(label, font, LABEL_SIZE)),
            ContextEntry::Separator => None,
        })
        .fold(0.0, f32::max);
    let width = (widest.ceil() + inset).clamp(MENU_MIN_WIDTH, MENU_MAX_WIDTH);
    let budget = width - inset;
    let entries: Vec<ContextEntry> = entries
        .into_iter()
        .map(|entry| match entry {
            ContextEntry::Item {
                action,
                label,
                enabled,
            } if chrome::text_width(&label, font, LABEL_SIZE) > budget => ContextEntry::Item {
                action,
                label: chrome::elide_to_width(&label, font, LABEL_SIZE, budget)
                    .0
                    .into_owned(),
                enabled,
            },
            entry => entry,
        })
        .collect();
    let height = 2.0 * MENU_PADDING + entries.iter().map(entry_height).sum::<f32>();
    (entries, Size::new(width, height))
}

/// The panel's top-left for a click at `at`: below and to the right of
/// it, flipped left or up where the window's edge would cut it off, and
/// kept inside the window when neither side fits.
pub(super) fn place(at: Point, menu: Size, window: Size) -> Point {
    let axis = |at: f32, extent: f32, limit: f32| {
        let corner = if at + extent <= limit {
            at
        } else {
            at - extent
        };
        corner.clamp(0.0, (limit - extent).max(0.0))
    };
    Point::new(
        axis(at.x, menu.width, window.width),
        axis(at.y, menu.height, window.height),
    )
}

/// What the backdrop under an open menu does with an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackdropResponse {
    /// Not a pointer event: keys still reach the menu's route.
    PassThrough,
    /// Stops here, as a native menu holds the pointer while it is up: a
    /// wheel scrolls nothing under it, and a program tracking the mouse
    /// sees no motion.
    Swallow,
    /// A press outside the panel, which also closes the menu.
    Dismiss,
}

fn backdrop_response(event: &Event) -> BackdropResponse {
    match event {
        Event::Mouse(mouse::Event::ButtonPressed(_))
        | Event::Touch(touch::Event::FingerPressed { .. }) => BackdropResponse::Dismiss,
        Event::Mouse(_) | Event::Touch(_) => BackdropResponse::Swallow,
        _ => BackdropResponse::PassThrough,
    }
}

/// The layer between the open menu's panel and the window under it. The
/// panel sees every event first, so its hover and clicks are its own;
/// whatever pointer event it leaves stops here.
struct MenuBackdrop;

impl Widget<Message, iced::Theme, iced::Renderer> for MenuBackdrop {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }

    fn update(
        &mut self,
        _tree: &mut Tree,
        event: &Event,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        match backdrop_response(event) {
            BackdropResponse::PassThrough => {}
            BackdropResponse::Swallow => shell.capture_event(),
            BackdropResponse::Dismiss => {
                shell.publish(Message::ContextMenuDismiss);
                shell.capture_event();
            }
        }
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        mouse::Interaction::Idle
    }

    fn draw(
        &self,
        _tree: &Tree,
        _renderer: &mut iced::Renderer,
        _theme: &iced::Theme,
        _style: &renderer::Style,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
    }
}

fn menu_row<'a>(
    chrome: &ChromePalette,
    index: usize,
    entry: &'a ContextEntry,
    highlighted: bool,
) -> Element<'a, Message> {
    let row: Element<'a, Message> = match entry {
        ContextEntry::Separator => container(
            container(Space::new().width(Fill).height(1)).style(chrome::menu_separator(chrome)),
        )
        .width(Fill)
        .center_y(SEPARATOR_HEIGHT)
        .padding([0.0, ITEM_PADDING_X / 2.0])
        .into(),
        ContextEntry::Item {
            action,
            label,
            enabled,
        } => button(
            container(
                text(label.as_str())
                    .size(LABEL_SIZE)
                    // Measured with Advanced shaping, as the pill titles are.
                    .shaping(iced::widget::text::Shaping::Advanced)
                    .wrapping(iced::widget::text::Wrapping::None),
            )
            .center_y(Fill),
        )
        .width(Fill)
        .height(ITEM_HEIGHT)
        .padding([0.0, ITEM_PADDING_X])
        .style(chrome::palette_row(chrome, highlighted, *enabled))
        .on_press_maybe(enabled.then_some(Message::ContextMenuChosen(*action)))
        .into(),
    };
    mouse_area(row)
        .on_enter(Message::ContextMenuHovered(Some(index)))
        .into()
}

impl App {
    /// A right-click on a row.
    pub fn context_menu_requested(&mut self, target: ContextTarget, at: Point) {
        if let Err(refusal) = self.open_context_menu(target, at) {
            tracing::debug!(%refusal, "a right-click opened no menu");
        }
    }

    /// Show `target`'s menu with its corner at `at`, in window
    /// coordinates. Refused while something else owns input, as an
    /// activation is.
    pub(super) fn open_context_menu(
        &mut self,
        target: ContextTarget,
        at: Point,
    ) -> Result<(), ContextError> {
        if self.context_blocked() {
            return Err(ContextError::Blocked);
        }
        let entries = self.context_entries(&target).ok_or(ContextError::Missing)?;
        if entries.is_empty() {
            return Err(ContextError::Empty);
        }
        self.cancel_drags();
        self.cancel_terminal_pointers("pointer cancel before a context menu");
        self.cancel_ime_composition();
        let (entries, size) = fitted(entries);
        self.context_menu = Some(OpenContextMenu {
            target,
            entries,
            at,
            size,
            highlighted: None,
        });
        Ok(())
    }

    pub fn close_context_menu(&mut self) {
        self.context_menu = None;
    }

    /// The menu closes once anything else takes input or its row goes —
    /// a palette, card or editor opened over IPC, a tab closed by its
    /// shell, a host that reconnected. Every update turn passes through
    /// here, so no path that does one of those has to remember the menu.
    pub fn observe_context_menu(&mut self) {
        let Some(menu) = &self.context_menu else {
            return;
        };
        if self.context_blocked()
            || context_row(&menu.target, &self.projects, &self.host_views).is_none()
        {
            self.close_context_menu();
        }
    }

    pub fn context_menu_hovered(&mut self, index: Option<usize>) {
        if let Some(menu) = &mut self.context_menu {
            menu.hover(index);
        }
    }

    /// A row of the open menu, clicked or entered.
    pub fn context_menu_chosen(&mut self, action: ContextAction) -> UiTask {
        let Some(target) = self.context_menu.as_ref().map(|menu| menu.target.clone()) else {
            return UiTask::None;
        };
        match self.context_activate(&target, action) {
            Ok(task) => task,
            Err(refusal) => {
                self.set_status(refusal.to_string());
                UiTask::None
            }
        }
    }

    /// A key while the menu is up. Every key is swallowed here, so none
    /// reaches an accelerator, a pending tab or the terminal.
    pub(super) fn context_menu_key(&mut self, event: &keyboard::Event) -> UiTask {
        let keyboard::Event::KeyPressed { key, .. } = event else {
            return UiTask::None;
        };
        match key.as_ref() {
            Key::Named(Named::Escape) => {
                self.rename_completion_key = Some(RenameCompletionKey::Escape);
                self.close_context_menu();
                UiTask::None
            }
            Key::Named(Named::Enter) => {
                let Some(action) = self
                    .context_menu
                    .as_ref()
                    .and_then(OpenContextMenu::highlighted_action)
                else {
                    return UiTask::None;
                };
                self.rename_completion_key = Some(RenameCompletionKey::Enter);
                self.context_menu_chosen(action)
            }
            key => {
                if let (Some(step), Some(menu)) = (MenuStep::of(&key), &mut self.context_menu) {
                    menu.step(step);
                }
                UiTask::None
            }
        }
    }

    /// `content` with the open menu over it: the panel, over a backdrop
    /// that keeps every pointer event off `content` — the dismissing press
    /// never reaches the terminal or a row.
    pub(super) fn with_context_menu<'a>(
        &'a self,
        content: Element<'a, Message>,
    ) -> Element<'a, Message> {
        let Some(menu) = &self.context_menu else {
            // A stack either way, so opening the menu leaves `content`
            // where it was in the widget tree: a new parent would rebuild
            // its state, scrolling the sidebar and tab strip back to the
            // start under the menu.
            return stack![content].width(Fill).height(Fill).into();
        };
        let rows = menu
            .entries
            .iter()
            .enumerate()
            .fold(column![], |rows, (index, entry)| {
                rows.push(menu_row(
                    &self.chrome,
                    index,
                    entry,
                    menu.highlighted == Some(index),
                ))
            });
        let panel = container(rows)
            .width(menu.size.width)
            .height(menu.size.height)
            .padding(MENU_PADDING)
            .style(chrome::palette_panel(&self.chrome));
        let panel = mouse_area(panel)
            .on_press(Message::ContextMenuPanelPressed)
            .on_right_press(Message::ContextMenuPanelPressed)
            .on_exit(Message::ContextMenuHovered(None));
        let corner = place(menu.at, menu.size, self.window_size);
        let layer = container(panel).width(Fill).height(Fill).padding(Padding {
            top: corner.y,
            left: corner.x,
            ..Padding::ZERO
        });
        stack![content, Element::new(MenuBackdrop), layer]
            .width(Fill)
            .height(Fill)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use roost_ipc::messages::{Tab, TabState};

    use super::*;

    fn tab(id: i64, project_id: i64, cwd: &str) -> Tab {
        Tab {
            id,
            project_id,
            title: String::new(),
            cwd: cwd.into(),
            state: TabState::None,
            has_notification: false,
            is_active: false,
            user_titled: false,
            position: 0,
            created_at: 0,
            last_active: 0,
            hook_active: false,
            shell_state: Default::default(),
            agent_lifecycle: Default::default(),
            ownership: None,
        }
    }

    fn project(id: i64, cwd: &str, tabs: Vec<Tab>) -> Project {
        Project {
            id,
            name: format!("p{id}"),
            cwd: cwd.into(),
            position: 0,
            created_at: 0,
            tabs,
        }
    }

    fn view(saved_id: &str, host: HostId, state: host_sidebar::SectionState) -> HostView {
        HostView {
            saved_id: saved_id.into(),
            label: saved_id.into(),
            target: saved_id.into(),
            transport: host_sidebar::HostTransportKind::Ssh,
            host,
            state,
            reduced_fidelity: false,
            reason: None,
            projects: vec![project(3, "/srv/p3", vec![tab(7, 3, "/srv/p3/t7")])],
            active_tab_id: 7,
            agents: 0,
        }
    }

    fn activation(
        blocked: bool,
        target: &ContextTarget,
        local: &[Project],
        hosts: &[HostView],
        action: ContextAction,
    ) -> Result<(), ContextError> {
        let menu = context_row(target, local, hosts).map(|row| {
            let entries = context_menu::entries(target, &row.facts(), &[]);
            (row, entries)
        });
        admit(blocked, menu, action).map(drop)
    }

    #[test]
    fn an_item_on_a_row_of_a_replaced_connection_is_refused() {
        let hosts = [view(
            "aa",
            HostId::new(5),
            host_sidebar::SectionState::Connected,
        )];
        let stale = HostId::new(4);
        for target in [
            ContextTarget::Tab(TabKey::new(stale, 7)),
            ContextTarget::Project(ProjectKey::new(stale, 3)),
        ] {
            assert_eq!(
                activation(false, &target, &[], &hosts, ContextAction::CloseTab),
                Err(ContextError::Missing),
                "{target:?}"
            );
        }
        let live = ContextTarget::Tab(TabKey::new(HostId::new(5), 7));
        assert_eq!(
            activation(false, &live, &[], &hosts, ContextAction::CloseTab),
            Ok(())
        );
        assert_eq!(
            context_row(&live, &[], &hosts).map(|row| (row.project, row.cwd, row.saved_host)),
            Some((
                Some(ProjectKey::new(HostId::new(5), 3)),
                "/srv/p3/t7".to_string(),
                Some("aa".to_string())
            ))
        );
    }

    #[test]
    fn an_item_on_a_closed_tab_is_refused() {
        let local = [project(1, "/tmp", vec![tab(2, 1, "")])];
        assert_eq!(
            activation(
                false,
                &ContextTarget::Tab(TabKey::local(9)),
                &local,
                &[],
                ContextAction::CloseTab
            ),
            Err(ContextError::Missing)
        );
        let open = ContextTarget::Tab(TabKey::local(2));
        assert_eq!(
            activation(false, &open, &local, &[], ContextAction::CloseTab),
            Ok(())
        );
        assert_eq!(
            context_row(&open, &local, &[]).map(|row| row.cwd),
            Some("/tmp".to_string()),
            "a tab with no cwd of its own copies its project's"
        );
    }

    #[test]
    fn an_item_that_no_longer_applies_is_refused() {
        let local = [project(1, "", vec![])];
        let target = ContextTarget::Project(ProjectKey::local(1));
        assert_eq!(
            activation(false, &target, &local, &[], ContextAction::CopyProjectPath),
            Err(ContextError::Disabled(ContextAction::CopyProjectPath))
        );
        assert_eq!(
            activation(false, &target, &local, &[], ContextAction::CloseTab),
            Err(ContextError::Absent(ContextAction::CloseTab))
        );
        let dimmed = [view(
            "aa",
            HostId::new(5),
            host_sidebar::SectionState::Disconnected,
        )];
        let row = ContextTarget::Project(ProjectKey::new(HostId::new(5), 3));
        assert_eq!(
            activation(false, &row, &[], &dimmed, ContextAction::RenameProject),
            Err(ContextError::Absent(ContextAction::RenameProject)),
            "a dimmed host's row offers only its host's verbs"
        );
    }

    #[test]
    fn no_item_runs_while_something_else_owns_input() {
        let local = [project(1, "/tmp", vec![tab(2, 1, "/tmp")])];
        let target = ContextTarget::Tab(TabKey::local(2));
        assert_eq!(
            activation(true, &target, &local, &[], ContextAction::RenameTab),
            Err(ContextError::Blocked)
        );
        assert_eq!(
            activation(false, &target, &local, &[], ContextAction::RenameTab),
            Ok(())
        );
    }

    #[test]
    fn refusals_are_invalid_param_with_distinct_messages() {
        let refusals = [
            ContextError::Blocked,
            ContextError::Missing,
            ContextError::Unknown("remove_host".into()),
            ContextError::Empty,
            ContextError::Absent(ContextAction::CloseTab),
            ContextError::Disabled(ContextAction::CloseTab),
        ];
        let messages: HashSet<String> = refusals
            .iter()
            .map(|refusal| {
                let failure = refusal.failure();
                assert_eq!(failure.code, codes::INVALID_PARAM, "{refusal:?}");
                failure.message
            })
            .collect();
        assert_eq!(messages.len(), refusals.len());
        let busy = ContextError::Busy.failure();
        assert_eq!(
            (busy.code.as_str(), busy.message.as_str()),
            (codes::BUSY, roost_ipc::local_route::SWITCH_BUSY_MESSAGE)
        );
    }

    #[test]
    fn a_host_band_resolves_by_saved_id_even_before_it_ever_connected() {
        let never = view(
            "bb",
            HostId::LOCAL,
            host_sidebar::SectionState::Disconnected,
        );
        let row = context_row(&ContextTarget::Host("bb".into()), &[], &[never]).expect("listed");
        assert_eq!(row.saved_host.as_deref(), Some("bb"));
        assert_eq!(row.project, None);
        assert_eq!(
            context_row(&ContextTarget::Host("cc".into()), &[], &[]),
            None
        );
    }

    #[test]
    fn entries_go_on_the_wire_as_items_and_separators() {
        assert_eq!(
            wire_entry(&ContextEntry::Item {
                action: ContextAction::NewTabHere,
                label: "New Tab Here".into(),
                enabled: false,
            }),
            AppContextMenuEntry::Item {
                action: "new_tab_here".into(),
                label: "New Tab Here".into(),
                enabled: false,
            }
        );
        assert_eq!(
            wire_entry(&ContextEntry::Separator),
            AppContextMenuEntry::Separator { separator: true }
        );
    }

    fn row(action: ContextAction, enabled: bool) -> ContextEntry {
        ContextEntry::Item {
            action,
            label: action.as_str().into(),
            enabled,
        }
    }

    #[test]
    fn the_backdrop_keeps_every_pointer_event_off_the_window_and_a_press_closes_the_menu() {
        let wheel = Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Lines { x: 0.0, y: -3.0 },
        });
        let moved = Event::Mouse(mouse::Event::CursorMoved {
            position: Point::new(400.0, 300.0),
        });
        let released = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
        for swallowed in [wheel, moved, released] {
            assert_eq!(
                backdrop_response(&swallowed),
                BackdropResponse::Swallow,
                "{swallowed:?}"
            );
        }
        for button in [
            mouse::Button::Left,
            mouse::Button::Right,
            mouse::Button::Middle,
        ] {
            assert_eq!(
                backdrop_response(&Event::Mouse(mouse::Event::ButtonPressed(button))),
                BackdropResponse::Dismiss,
                "{button:?}"
            );
        }
        let key = Event::Keyboard(keyboard::Event::ModifiersChanged(
            keyboard::Modifiers::SHIFT,
        ));
        assert_eq!(
            backdrop_response(&key),
            BackdropResponse::PassThrough,
            "keys go on to the menu's own route"
        );
    }

    #[test]
    fn the_menu_opens_below_right_of_the_click_and_flips_at_the_window_edges() {
        let window = Size::new(800.0, 600.0);
        let menu = Size::new(200.0, 150.0);
        assert_eq!(
            place(Point::new(100.0, 100.0), menu, window),
            Point::new(100.0, 100.0)
        );
        assert_eq!(
            place(Point::new(700.0, 100.0), menu, window),
            Point::new(500.0, 100.0),
            "too close to the right edge: it opens to the left of the click"
        );
        assert_eq!(
            place(Point::new(100.0, 500.0), menu, window),
            Point::new(100.0, 350.0),
            "too close to the bottom: it opens above the click"
        );
        assert_eq!(
            place(Point::new(700.0, 500.0), menu, window),
            Point::new(500.0, 350.0)
        );
        assert_eq!(
            place(Point::new(10.0, 10.0), menu, Size::new(150.0, 100.0)),
            Point::new(0.0, 0.0),
            "a window narrower than the menu pins it to the window's corner"
        );
        assert_eq!(
            place(Point::new(150.0, 100.0), menu, Size::new(300.0, 600.0)),
            Point::new(0.0, 100.0),
            "fitting on neither side, it stays inside the window"
        );
    }

    #[test]
    fn the_keys_move_past_separators_and_disabled_rows_and_wrap() {
        let entries = [
            row(ContextAction::RenameTab, false),
            row(ContextAction::NewTabHere, true),
            row(ContextAction::CopyTabPath, true),
            ContextEntry::Separator,
            row(ContextAction::CloseTab, true),
            row(ContextAction::NewTab, false),
        ];
        let step = |from, step| stepped(&entries, from, step);
        assert_eq!(
            step(None, MenuStep::Down),
            Some(1),
            "past a disabled first row"
        );
        assert_eq!(step(Some(2), MenuStep::Down), Some(4), "past the separator");
        assert_eq!(
            step(Some(4), MenuStep::Down),
            Some(1),
            "past a disabled last row, to the top"
        );
        assert_eq!(
            step(Some(4), MenuStep::Up),
            Some(2),
            "back past the separator"
        );
        assert_eq!(
            step(Some(1), MenuStep::Up),
            Some(4),
            "and from the top round to the bottom"
        );
        assert_eq!(step(None, MenuStep::Up), Some(4));
        assert_eq!(step(Some(2), MenuStep::Home), Some(1));
        assert_eq!(step(Some(1), MenuStep::End), Some(4));
        assert_eq!(
            stepped(
                &[ContextEntry::Separator, row(ContextAction::CloseTab, false)],
                None,
                MenuStep::Down
            ),
            None,
            "a menu with nothing to run highlights nothing"
        );

        let mut menu = OpenContextMenu {
            target: ContextTarget::Tab(TabKey::local(2)),
            entries: entries.to_vec(),
            at: Point::ORIGIN,
            size: Size::ZERO,
            highlighted: None,
        };
        menu.step(MenuStep::Down);
        menu.step(MenuStep::Down);
        menu.step(MenuStep::Down);
        assert_eq!(menu.highlighted_action(), Some(ContextAction::CloseTab));
        menu.hover(Some(3));
        assert_eq!(
            menu.highlighted_action(),
            None,
            "the pointer on the separator"
        );
        menu.hover(Some(5));
        assert_eq!(menu.highlighted, None, "a disabled row takes no highlight");
        menu.hover(Some(2));
        assert_eq!(menu.highlighted_action(), Some(ContextAction::CopyTabPath));
    }

    #[test]
    fn the_panel_is_sized_from_its_rows_and_a_long_label_is_elided_to_fit() {
        let (short, size) = fitted(vec![
            row(ContextAction::RenameTab, true),
            ContextEntry::Separator,
            row(ContextAction::CloseTab, true),
        ]);
        assert_eq!(short[0], row(ContextAction::RenameTab, true));
        assert_eq!(size.width, MENU_MIN_WIDTH);
        assert_eq!(
            size.height,
            2.0 * MENU_PADDING + 2.0 * ITEM_HEIGHT + SEPARATOR_HEIGHT
        );

        let long = ContextEntry::Item {
            action: ContextAction::HostUpdateSession,
            label: format!(
                "Update roost-session on {}",
                "a-very-long-host-name".repeat(4)
            ),
            enabled: true,
        };
        let (fitted_rows, size) = fitted(vec![long]);
        assert_eq!(size.width, MENU_MAX_WIDTH);
        let ContextEntry::Item { label, .. } = &fitted_rows[0] else {
            panic!("an item stays an item");
        };
        assert!(label.ends_with(chrome::ELLIPSIS), "{label}");
        assert!(label.starts_with("Update roost-session on "), "{label}");
    }
}
