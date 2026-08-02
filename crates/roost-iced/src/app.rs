use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use iced::keyboard::{self, key::Named, Key};
use iced::widget::Id;
use iced::widget::{
    button, column, container, mouse_area, row, scrollable, stack, text, text_input,
};
use iced::{font, window, Alignment, Color, Element, Fill, Font, Shrink, Size};
use roost_engine::git_metrics;
use roost_engine::ipc::{
    ClipboardOp, DumpData, ExpandSelectionData, IpcHandler, ResolvedCellData, ResolvedCellsData,
    SelectionData, UiRequest,
};
use roost_engine::osc::{ClipboardTarget, OscAction, OscColorSnapshot, OscRouter};
use roost_engine::pointer::{MotionEmitter, PointerAction, PointerButton};
use roost_engine::process::{self, ProcessRequest};
use roost_engine::session::{InputCapture, TabOutput, TabSession};
use roost_engine::single_instance::InstanceLock;
use roost_engine::{
    LocalClient, PtySupervisor, RestoreTab, Workspace, WorkspaceError, WorkspaceEvent,
};
use roost_ipc::agent;
use roost_ipc::messages::{
    PaletteItemView, PalettePresentResult, PaletteStateResult, Project, SidebarDumpAgentRow,
    SidebarDumpProject, SidebarDumpResult,
};
use roost_ipc::paths::BundleProfile;
use roost_ipc::IpcServer;
use roost_ui_model::theme::Theme;
use roost_ui_model::typography::{self, FamilyApply, TerminalTypography};
use roost_ui_model::{
    agent_palette,
    config::{self, RoostConfig},
    custom_command,
    keybind::{self, Accel, AccelMods, KeybindAction},
    notification_inbox, palette, provider,
    rollup::project_rollup,
};
use roost_url::HoverUrl;
use roost_vt::{
    key_action, mouse_action, mouse_button, KeyEncoder, KeyEvent, MouseEncoder, MouseEvent,
    RenderState, ScrollDirection, ScrollRoute, Terminal, TerminalOptions, TerminalScroll,
    TerminalSelection,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::font_registry::{system_font_registry, FontRegistry};
use crate::palette_scroll::Visibility;
use crate::tab_reorder::{TabStrip, TabStripEvent};
use crate::terminal_widget::{
    resolve_colors, DrawCell, TerminalMetrics, TerminalPointerEvent, TerminalSnapshot,
    TerminalWheelEvent, TerminalWidget, TERMINAL_PADDING,
};
use crate::Message;
use crate::{chrome, input};

const SIDEBAR_WIDTH: f32 = 220.0;
const DEFAULT_COLS: u16 = 100;
const DEFAULT_ROWS: u16 = 32;
// Widget operations can observe an incomplete tree while a slower renderer or
// compositor is still materializing a newly-pushed palette frame. Retry on
// later application ticks for roughly two seconds at the 60 Hz subscription,
// while keeping the work bounded and revision-scoped.
const PALETTE_GEOMETRY_RETRY_LIMIT: u8 = 120;
const PALETTE_AGENT_PROJECT_MAX_COLUMNS: usize = 24;
// Name and status share the width left after project and the reserved
// metrics/time column. Preserve both when they fit. Under genuine pressure,
// retain a useful status tail and let the usually-longer name ellipsize first.
// Unicode display columns, rather than scalar counts, keep wide labels honest.
const PALETTE_AGENT_LEFT_MAX_COLUMNS: usize = 58;
const PALETTE_AGENT_STATUS_FLOOR_COLUMNS: usize = 18;
const STATUS_BANNER_DURATION: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
struct StatusBanner {
    message: Option<String>,
    expires_at: Option<Instant>,
}

impl StatusBanner {
    fn set_at(&mut self, message: impl Into<String>, now: Instant) {
        self.message = Some(message.into());
        self.expires_at = Some(now + STATUS_BANNER_DURATION);
    }

    fn clear(&mut self) {
        self.message = None;
        self.expires_at = None;
    }

    fn expire_at(&mut self, now: Instant) {
        if self.expires_at.is_some_and(|deadline| now >= deadline) {
            self.clear();
        }
    }

    fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaletteTextRun {
    text: String,
    matched: bool,
}

fn palette_title_runs(title: &str, ranges: &[Range<usize>]) -> Vec<PaletteTextRun> {
    let characters = title.chars().collect::<Vec<_>>();
    let mut cursor = 0;
    let mut runs = Vec::new();
    for range in ranges {
        let start = range.start.max(cursor).min(characters.len());
        let end = range.end.max(start).min(characters.len());
        if cursor < start {
            runs.push(PaletteTextRun {
                text: characters[cursor..start].iter().collect(),
                matched: false,
            });
        }
        if start < end {
            runs.push(PaletteTextRun {
                text: characters[start..end].iter().collect(),
                matched: true,
            });
        }
        cursor = end;
    }
    if cursor < characters.len() || runs.is_empty() {
        runs.push(PaletteTextRun {
            text: characters[cursor..].iter().collect(),
            matched: false,
        });
    }
    runs
}

fn ellipsize_palette_text(value: &str, max_columns: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_columns {
        return value.to_string();
    }
    if max_columns == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut width = 0;
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width + 1 > max_columns {
            break;
        }
        result.push(character);
        width += character_width;
    }
    result.push('…');
    result
}

fn palette_agent_left_text(name: &str, status: &str) -> (String, String) {
    let name_width = UnicodeWidthStr::width(name);
    let status_width = UnicodeWidthStr::width(status);
    if name_width + status_width <= PALETTE_AGENT_LEFT_MAX_COLUMNS {
        return (name.to_string(), status.to_string());
    }

    let status_floor = status_width
        .min(PALETTE_AGENT_STATUS_FLOOR_COLUMNS)
        .min(PALETTE_AGENT_LEFT_MAX_COLUMNS);
    let name_budget = name_width.min(PALETTE_AGENT_LEFT_MAX_COLUMNS - status_floor);
    let status_budget = status_width.min(PALETTE_AGENT_LEFT_MAX_COLUMNS - name_budget);
    (
        ellipsize_palette_text(name, name_budget),
        ellipsize_palette_text(status, status_budget),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseTabOutcome {
    Closed,
    AlreadyGone,
}

fn close_tab_by_id(
    runtime: &tokio::runtime::Runtime,
    client: &LocalClient,
    tab_id: i64,
) -> Result<CloseTabOutcome> {
    match runtime.block_on(client.close_tab(tab_id)) {
        Ok(()) => Ok(CloseTabOutcome::Closed),
        Err(error)
            if matches!(
                error.downcast_ref::<WorkspaceError>(),
                Some(WorkspaceError::TabNotFound(id)) if *id == tab_id
            ) =>
        {
            Ok(CloseTabOutcome::AlreadyGone)
        }
        Err(error) => Err(error),
    }
}

fn sidebar_width(collapsed: bool) -> f32 {
    if collapsed {
        0.0
    } else {
        SIDEBAR_WIDTH
    }
}

fn clamped_tab_index(current: usize, len: usize, delta: isize) -> Option<usize> {
    if len == 0 || current >= len {
        return None;
    }
    Some((current as isize + delta).clamp(0, len as isize - 1) as usize)
}

fn project_id_at_index(projects: &[Project], index: u8) -> Option<i64> {
    index
        .checked_sub(1)
        .and_then(|index| projects.get(usize::from(index)))
        .map(|project| project.id)
}

fn active_project_tab_at_index(projects: &[Project], project_id: i64, index: u8) -> Option<i64> {
    index.checked_sub(1).and_then(|index| {
        projects
            .iter()
            .find(|project| project.id == project_id)
            .and_then(|project| project.tabs.get(usize::from(index)))
            .map(|tab| tab.id)
    })
}

fn dispatch_keybind_once_unless_repeat<T>(repeat: bool, dispatch: impl FnOnce() -> T) -> Option<T> {
    (!repeat).then(dispatch)
}

fn accel_label(accel: &Accel) -> Option<String> {
    if accel.key.is_empty() {
        return None;
    }
    #[cfg(target_os = "macos")]
    let mut label = String::new();
    #[cfg(not(target_os = "macos"))]
    let mut parts = Vec::new();

    #[cfg(target_os = "macos")]
    {
        if accel.modifiers.contains(AccelMods::CTRL) {
            label.push('⌃');
        }
        if accel.modifiers.contains(AccelMods::ALT) {
            label.push('⌥');
        }
        if accel.modifiers.contains(AccelMods::SHIFT) {
            label.push('⇧');
        }
        if accel.modifiers.contains(AccelMods::SUPER) {
            label.push('⌘');
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        if accel.modifiers.contains(AccelMods::CTRL) {
            parts.push("Ctrl".to_string());
        }
        if accel.modifiers.contains(AccelMods::ALT) {
            parts.push("Alt".to_string());
        }
        if accel.modifiers.contains(AccelMods::SHIFT) {
            parts.push("Shift".to_string());
        }
        if accel.modifiers.contains(AccelMods::SUPER) {
            parts.push("Super".to_string());
        }
    }

    let key = match accel.key.as_str() {
        "bracketleft" => "[".to_string(),
        "bracketright" => "]".to_string(),
        "braceleft" => "{".to_string(),
        "braceright" => "}".to_string(),
        "equal" => "=".to_string(),
        "minus" => "-".to_string(),
        key if key.chars().count() == 1 => key.to_uppercase(),
        key => key.to_string(),
    };
    #[cfg(target_os = "macos")]
    {
        label.push_str(&key);
        Some(label)
    }
    #[cfg(not(target_os = "macos"))]
    {
        parts.push(key);
        Some(parts.join("+"))
    }
}

fn command_palette_frame(
    notification_count: usize,
    providers: &[provider::Provider],
    keybindings: &HashMap<Accel, KeybindAction>,
) -> palette::PaletteFrame {
    let mut bindings = keybindings.iter().collect::<Vec<_>>();
    bindings.sort_by(|(left, _), (right, _)| {
        (left.modifiers.bits(), &left.key).cmp(&(right.modifiers.bits(), &right.key))
    });
    let mut reverse = HashMap::new();
    for (accel, action) in bindings {
        reverse.entry(*action).or_insert(accel);
    }
    let mut items =
        palette::command_items(|action| reverse.get(&action).and_then(|accel| accel_label(accel)));
    let index = items
        .iter()
        .position(|item| item.id == palette::PaletteCommands::SELECT_FONT_ID)
        .map_or(items.len(), |index| index + 1);
    let mut dynamic =
        vec![
            palette::PaletteItem::new(palette::PaletteCommands::VIEW_AGENTS_ID, "Go to Agent…")
                .with_trailing(
                    reverse
                        .get(&KeybindAction::AgentPalette)
                        .and_then(|accel| accel_label(accel)),
                ),
        ];
    dynamic.extend(notification_inbox::command_items(notification_count));
    items.splice(index..index, dynamic);
    if !providers.is_empty() {
        items.push(
            palette::PaletteItem::new("custom_commands", "Custom Commands…").with_trailing(
                reverse
                    .get(&KeybindAction::CustomPalette)
                    .and_then(|accel| accel_label(accel)),
            ),
        );
    }
    palette::PaletteFrame::new("commands", "Execute a command…", items)
}

fn provider_palette_frame(providers: &[provider::Provider]) -> palette::PaletteFrame {
    palette::PaletteFrame::new(
        "custom",
        "Custom commands…",
        provider::provider_items(providers),
    )
}

fn launcher_palette_frame(config: &RoostConfig) -> palette::PaletteFrame {
    palette::PaletteFrame::new(
        "launcher",
        "Run a command…",
        custom_command::launcher_items(&config.commands),
    )
}

fn theme_palette_frame(active_theme_name: &str) -> palette::PaletteFrame {
    let names = Theme::bundled_names();
    let selection = names
        .iter()
        .position(|name| name == active_theme_name)
        .unwrap_or(0);
    let items = names
        .into_iter()
        .map(|name| palette::PaletteItem::new(name.clone(), name))
        .collect();
    palette::PaletteFrame::new("themes", "Select a theme…", items).with_selection(selection)
}

#[derive(Debug, PartialEq, Eq)]
struct ApplyRollbackFailure<E> {
    apply: E,
    rollback: Option<E>,
}

fn apply_with_rollback<T, E>(
    previous: &T,
    next: &T,
    mut apply: impl FnMut(&T) -> std::result::Result<(), E>,
) -> std::result::Result<(), ApplyRollbackFailure<E>> {
    if let Err(error) = apply(next) {
        return Err(ApplyRollbackFailure {
            apply: error,
            rollback: apply(previous).err(),
        });
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct ThemeBatchFailure {
    tab_id: i64,
    apply: String,
    rollback: Vec<(i64, String)>,
}

fn apply_theme_batch(
    targets: &[(i64, Theme)],
    next: &Theme,
    mut apply: impl FnMut(i64, &Theme) -> std::result::Result<(), String>,
) -> std::result::Result<(), ThemeBatchFailure> {
    for (applied, (tab_id, _)) in targets.iter().enumerate() {
        if let Err(error) = apply(*tab_id, next) {
            let rollback = targets[..applied]
                .iter()
                .rev()
                .filter_map(|(rollback_id, previous)| {
                    apply(*rollback_id, previous)
                        .err()
                        .map(|error| (*rollback_id, error))
                })
                .collect();
            return Err(ThemeBatchFailure {
                tab_id: *tab_id,
                apply: error,
                rollback,
            });
        }
    }
    Ok(())
}

fn persist_theme_selection_with(
    config: &mut RoostConfig,
    path: Option<&Path>,
    name: &str,
    write: impl FnOnce(&Path, &str, &str) -> io::Result<()>,
) -> io::Result<()> {
    config.theme_name = Some(name.to_string());
    let Some(path) = path else {
        return Ok(());
    };
    write(path, "theme", name)
}

fn persist_font_size_with(
    config: &mut RoostConfig,
    path: Option<&Path>,
    size_pt: f64,
    write: impl FnOnce(&Path, &str, &str) -> io::Result<()>,
) -> io::Result<()> {
    config.font_size = Some(size_pt);
    let Some(path) = path else {
        return Ok(());
    };
    write(path, "font-size", &typography::format_font_size(size_pt))
}

fn persist_font_family_with(
    config: &mut RoostConfig,
    path: Option<&Path>,
    family: &str,
    write: impl FnOnce(&Path, &str, &str) -> io::Result<()>,
) -> io::Result<()> {
    config.font_family = Some(family.to_string());
    let Some(path) = path else {
        return Ok(());
    };
    write(path, "font-family", &typography::quote_font_family(family))
}

fn finish_theme_confirmation(
    palette: &mut Option<palette::PaletteState>,
    theme_at_open: &mut Option<String>,
    status: &mut StatusBanner,
    persistence_error: Option<String>,
    now: Instant,
) {
    *palette = None;
    *theme_at_open = None;
    if let Some(error) = persistence_error {
        status.set_at(error, now);
    }
}

fn font_palette_frame(registry: &FontRegistry, resolved: &str) -> palette::PaletteFrame {
    let names = registry.picker_names();
    let selection = names
        .iter()
        .position(|name| name.eq_ignore_ascii_case(resolved))
        .unwrap_or(0);
    let items = names
        .iter()
        .map(|name| palette::PaletteItem::new(name.clone(), name.clone()))
        .collect();
    palette::PaletteFrame::new("fonts", "Select a font…", items).with_selection(selection)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenameTarget {
    Project(i64),
    Tab(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenameCompletionKey {
    Enter,
    Escape,
}

fn consume_rename_completion_key(
    pending: &mut Option<RenameCompletionKey>,
    event: &keyboard::Event,
) -> bool {
    let Some(key) = *pending else {
        return false;
    };
    let matches = matches!(
        (key, event),
        (
            RenameCompletionKey::Enter,
            keyboard::Event::KeyPressed {
                key: Key::Named(Named::Enter),
                ..
            } | keyboard::Event::KeyReleased {
                key: Key::Named(Named::Enter),
                ..
            },
        ) | (
            RenameCompletionKey::Escape,
            keyboard::Event::KeyPressed {
                key: Key::Named(Named::Escape),
                ..
            } | keyboard::Event::KeyReleased {
                key: Key::Named(Named::Escape),
                ..
            },
        )
    );
    if matches && matches!(event, keyboard::Event::KeyReleased { .. }) {
        *pending = None;
    }
    matches
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RenameEditor {
    target: RenameTarget,
    opened_label: String,
    draft: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TabDragContext {
    project_id: i64,
    source_id: i64,
    generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TabDragPreview {
    context: TabDragContext,
    original_ids: Vec<i64>,
    ordered_ids: Vec<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TabDragCommitRequest {
    context: TabDragContext,
    original_ids: Vec<i64>,
    ordered_ids: Vec<i64>,
}

impl From<&TabDragPreview> for TabDragCommitRequest {
    fn from(preview: &TabDragPreview) -> Self {
        Self {
            context: preview.context.clone(),
            original_ids: preview.original_ids.clone(),
            ordered_ids: preview.ordered_ids.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TabDragSettlement {
    Ignored,
    Settled(Result<bool, String>),
}

fn same_stable_ids(left: &[i64], right: &[i64]) -> bool {
    if left.len() != right.len() || left.contains(&0) {
        return false;
    }
    let count = left.len();
    let left = left.iter().copied().collect::<HashSet<_>>();
    let right = right.iter().copied().collect::<HashSet<_>>();
    left.len() == count && right.len() == count && left == right
}

fn dispatch_tab_drag_commit_with(
    preview: Option<&TabDragPreview>,
    authoritative_ids: &[i64],
    context: &TabDragContext,
    original_ids: &[i64],
    ordered_ids: Vec<i64>,
    apply: impl FnOnce(i64, Vec<i64>) -> Result<(), String>,
) -> Result<bool, String> {
    let valid = preview.is_some_and(|preview| {
        preview.context == *context
            && preview.original_ids == original_ids
            && preview.ordered_ids == ordered_ids
            && authoritative_ids == original_ids
            && same_stable_ids(&ordered_ids, original_ids)
    });
    if !valid || ordered_ids == original_ids {
        return Ok(false);
    }
    apply(context.project_id, ordered_ids)?;
    Ok(true)
}

fn settle_tab_drag_commit_with(
    preview: &mut Option<TabDragPreview>,
    authoritative_ids: &[i64],
    request: TabDragCommitRequest,
    apply: impl FnOnce(i64, Vec<i64>) -> Result<(), String>,
) -> TabDragSettlement {
    if preview
        .as_ref()
        .is_none_or(|preview| preview.context != request.context)
    {
        return TabDragSettlement::Ignored;
    }

    let result = dispatch_tab_drag_commit_with(
        preview.as_ref(),
        authoritative_ids,
        &request.context,
        &request.original_ids,
        request.ordered_ids,
        apply,
    );
    *preview = None;
    TabDragSettlement::Settled(result)
}

fn end_tab_drag_preview_if_owned(
    preview: &mut Option<TabDragPreview>,
    authoritative_ids: &[i64],
    context: &TabDragContext,
    original_ids: &[i64],
) -> bool {
    let owned = preview.as_ref().is_some_and(|preview| {
        preview.context == *context
            && preview.original_ids == original_ids
            && preview.ordered_ids == original_ids
            && authoritative_ids == original_ids
    });
    if owned {
        *preview = None;
    }
    owned
}

fn rename_target_label(projects: &[Project], target: RenameTarget) -> Option<&str> {
    match target {
        RenameTarget::Project(project_id) => projects
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.name.as_str()),
        RenameTarget::Tab(tab_id) => projects
            .iter()
            .flat_map(|project| &project.tabs)
            .find(|tab| tab.id == tab_id)
            .map(|tab| tab.title.as_str()),
    }
}

fn begin_rename_editor(projects: &[Project], target: RenameTarget) -> Result<RenameEditor, String> {
    let id = match target {
        RenameTarget::Project(id) | RenameTarget::Tab(id) => id,
    };
    if id == 0 {
        return Err("no active project or tab to rename".into());
    }
    let label = rename_target_label(projects, target)
        .ok_or_else(|| format!("rename target {target:?} is no longer available"))?;
    Ok(RenameEditor {
        target,
        opened_label: label.to_string(),
        draft: label.to_string(),
    })
}

fn rename_editor_is_renderable(
    editor: &RenameEditor,
    projects: &[Project],
    active_project: i64,
    sidebar_collapsed: bool,
) -> bool {
    match editor.target {
        RenameTarget::Project(project_id) => {
            !sidebar_collapsed && projects.iter().any(|project| project.id == project_id)
        }
        RenameTarget::Tab(tab_id) => projects
            .iter()
            .find(|project| project.id == active_project)
            .is_some_and(|project| project.tabs.iter().any(|tab| tab.id == tab_id)),
    }
}

fn submit_rename_editor_with(
    editor: &mut Option<RenameEditor>,
    apply: impl FnOnce(RenameTarget, &str) -> Result<(), String>,
) -> Result<bool, String> {
    let Some(current) = editor.as_ref() else {
        return Ok(false);
    };
    let Some(label) = roost_ui_model::rename::committed_label(&current.draft) else {
        *editor = None;
        return Ok(false);
    };
    let target = current.target;
    apply(target, &label)?;
    *editor = None;
    Ok(true)
}

fn submit_rename_editor_once_with(
    editor: &mut Option<RenameEditor>,
    pending: &mut Option<RenameCompletionKey>,
    apply: impl FnOnce(RenameTarget, &str) -> Result<(), String>,
) -> Result<bool, String> {
    if editor.is_none() || *pending == Some(RenameCompletionKey::Enter) {
        return Ok(false);
    }
    *pending = Some(RenameCompletionKey::Enter);
    submit_rename_editor_with(editor, apply)
}

fn retain_palette_focus_after_back<T>(
    requested: &mut bool,
    palette_open: bool,
    result: Result<T, String>,
) -> Result<T, String> {
    *requested = palette_open;
    result
}

fn arm_rename_completion_for_open_editor(
    pending: &mut Option<RenameCompletionKey>,
    editor_open: bool,
) {
    if editor_open {
        *pending = Some(RenameCompletionKey::Enter);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyboardRoute {
    None,
    Editor,
    Palette,
    Terminal(i64),
}

fn resolve_keyboard_route(
    editor_open: bool,
    palette_open: bool,
    active_tab: i64,
    active_terminal_live: bool,
) -> KeyboardRoute {
    if editor_open {
        KeyboardRoute::Editor
    } else if palette_open {
        KeyboardRoute::Palette
    } else if active_terminal_live {
        KeyboardRoute::Terminal(active_tab)
    } else {
        KeyboardRoute::None
    }
}

fn take_palette_focus_request(requested: &mut bool, input_id: &Id) -> UiTask {
    if std::mem::take(requested) {
        UiTask::FocusWidget(input_id.clone())
    } else {
        UiTask::None
    }
}

fn take_rename_focus_request(requested: &mut bool, editor_open: bool, input_id: &Id) -> UiTask {
    if std::mem::take(requested) && editor_open {
        UiTask::FocusWidget(input_id.clone()).then(UiTask::SelectAllWidget(input_id.clone()))
    } else {
        UiTask::None
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PaletteVisibilityRequest {
    #[default]
    None,
    Measure,
    Reveal,
}

impl PaletteVisibilityRequest {
    fn merge(self, next: Self) -> Self {
        match (self, next) {
            (Self::Reveal, _) | (_, Self::Reveal) => Self::Reveal,
            (Self::Measure, _) | (_, Self::Measure) => Self::Measure,
            _ => Self::None,
        }
    }
}

fn queue_visibility_request(
    current: PaletteVisibilityRequest,
    next: PaletteVisibilityRequest,
    replace: bool,
) -> PaletteVisibilityRequest {
    if replace {
        next
    } else {
        current.merge(next)
    }
}

fn queue_scroll_measurement(
    selected_in_view: &mut Option<bool>,
    retries: &mut u8,
    request: &mut PaletteVisibilityRequest,
    measurement_generation: &mut u64,
    reveal_required: bool,
) {
    *selected_in_view = None;
    *measurement_generation = measurement_generation.wrapping_add(1).max(1);
    // Scroll offset changes do not change widget identity or row layout. Keep
    // the current revision/IDs and preserve a structural reveal until it has
    // succeeded; later viewport changes need only a fresh measurement.
    *request = if reveal_required {
        PaletteVisibilityRequest::Reveal
    } else {
        *retries = 0;
        PaletteVisibilityRequest::Measure
    };
}

fn schedule_reveal_attempt(
    attempts: &mut u8,
    _selected_in_view: &mut Option<bool>,
    reveal_required: &mut bool,
) -> bool {
    if *attempts >= PALETTE_GEOMETRY_RETRY_LIMIT {
        *reveal_required = false;
        false
    } else {
        *attempts += 1;
        true
    }
}

fn visibility_retry(retries: u8, reveal: bool) -> Option<(u8, PaletteVisibilityRequest)> {
    (retries < PALETTE_GEOMETRY_RETRY_LIMIT).then(|| {
        (
            retries + 1,
            if reveal {
                PaletteVisibilityRequest::Reveal
            } else {
                PaletteVisibilityRequest::Measure
            },
        )
    })
}

fn queue_layout_visibility_request(
    current: PaletteVisibilityRequest,
    next: PaletteVisibilityRequest,
    replace: bool,
    reveal_required: bool,
) -> PaletteVisibilityRequest {
    if reveal_required {
        PaletteVisibilityRequest::Reveal
    } else {
        queue_visibility_request(current, next, replace)
    }
}

fn apply_visible_result(
    selected_in_view: &mut Option<bool>,
    retries: &mut u8,
    request: &mut PaletteVisibilityRequest,
    reveal_required: &mut bool,
    reveal: bool,
    visible: bool,
) -> bool {
    *selected_in_view = Some(visible);
    if !reveal || visible {
        *retries = 0;
        if reveal && visible {
            *reveal_required = false;
        }
        return false;
    }
    if !*reveal_required {
        *retries = 0;
        return false;
    }
    if let Some((next_retries, retry)) = visibility_retry(*retries, true) {
        *retries = next_retries;
        *request = request.merge(retry);
        false
    } else {
        *reveal_required = false;
        true
    }
}

fn palette_row_id(session: u64, revision: u64, index: usize) -> Id {
    Id::from(format!("palette-row:{session}:{revision}:{index}"))
}

fn visibility_result_is_current(
    current_session: u64,
    current_revision: u64,
    current_measurement_generation: u64,
    result_session: u64,
    result_revision: u64,
    result_measurement_generation: u64,
) -> bool {
    current_session == result_session
        && current_revision == result_revision
        && current_measurement_generation == result_measurement_generation
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PaletteLayoutRow {
    id: String,
    title: String,
    subtitle: Option<String>,
    trailing_text: Option<String>,
    has_agent_layout: bool,
}

fn dynamic_refresh_request(
    layout_changed: bool,
    rendered_content_changed: bool,
) -> PaletteVisibilityRequest {
    if layout_changed {
        PaletteVisibilityRequest::Reveal
    } else if rendered_content_changed {
        PaletteVisibilityRequest::Measure
    } else {
        PaletteVisibilityRequest::None
    }
}

pub enum UiTask {
    None,
    Then(Box<UiTask>, Box<UiTask>),
    Focus(window::Id),
    FocusWidget(Id),
    SelectAllWidget(Id),
    Resize(window::Id, Size),
    Screenshot(window::Id),
    ClipboardRead {
        request_id: u64,
        target: ClipboardOp,
    },
    ClipboardWrite {
        request_id: u64,
        target: ClipboardOp,
        text: String,
    },
    OpenUrl {
        url: String,
    },
    PaletteVisibility {
        scroll_id: Id,
        row_id: Id,
        session: u64,
        revision: u64,
        measurement_generation: u64,
        reveal: bool,
    },
}

impl UiTask {
    fn then(self, next: Self) -> Self {
        match (self, next) {
            (Self::None, next) => next,
            (task, Self::None) => task,
            (task, next) => Self::Then(Box::new(task), Box::new(next)),
        }
    }
}

type ScreenshotReply = tokio::sync::oneshot::Sender<Result<(Vec<u8>, u32, u32), String>>;
type ClipboardReply = tokio::sync::oneshot::Sender<Result<Option<String>, String>>;

enum ClipboardReadDestination {
    Ipc(ClipboardReply),
    Paste { tab_id: i64 },
}

enum ClipboardReadCompletion {
    Ipc {
        reply: ClipboardReply,
        value: Option<String>,
    },
    Paste {
        tab_id: i64,
        value: Option<String>,
    },
}

#[derive(Debug)]
enum ClipboardEffect {
    Read {
        request_id: u64,
        target: ClipboardOp,
    },
    Write {
        request_id: u64,
        target: ClipboardOp,
        text: String,
    },
}

impl ClipboardEffect {
    fn request_id(&self) -> u64 {
        match self {
            Self::Read { request_id, .. } | Self::Write { request_id, .. } => *request_id,
        }
    }

    fn into_task(self) -> UiTask {
        match self {
            Self::Read { request_id, target } => UiTask::ClipboardRead { request_id, target },
            Self::Write {
                request_id,
                target,
                text,
            } => UiTask::ClipboardWrite {
                request_id,
                target,
                text,
            },
        }
    }
}

#[derive(Default)]
struct ClipboardQueue {
    next_request_id: u64,
    queued: VecDeque<ClipboardEffect>,
    active_request_id: Option<u64>,
    pending_reads: HashMap<u64, ClipboardReadDestination>,
}

impl ClipboardQueue {
    fn allocate_request_id(&mut self) -> u64 {
        loop {
            self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
            let request_id = self.next_request_id;
            let queued = self
                .queued
                .iter()
                .any(|effect| effect.request_id() == request_id);
            if self.active_request_id != Some(request_id)
                && !self.pending_reads.contains_key(&request_id)
                && !queued
            {
                return request_id;
            }
        }
    }

    fn enqueue_ipc_read(&mut self, target: ClipboardOp, reply: ClipboardReply) -> u64 {
        let request_id = self.allocate_request_id();
        self.pending_reads
            .insert(request_id, ClipboardReadDestination::Ipc(reply));
        self.queued
            .push_back(ClipboardEffect::Read { request_id, target });
        request_id
    }

    fn enqueue_paste_read(&mut self, target: ClipboardOp, tab_id: i64) -> u64 {
        let request_id = self.allocate_request_id();
        self.pending_reads
            .insert(request_id, ClipboardReadDestination::Paste { tab_id });
        self.queued
            .push_back(ClipboardEffect::Read { request_id, target });
        request_id
    }

    fn enqueue_write(&mut self, target: ClipboardOp, text: String) -> u64 {
        let request_id = self.allocate_request_id();
        self.queued.push_back(ClipboardEffect::Write {
            request_id,
            target,
            text,
        });
        request_id
    }

    fn start_next(&mut self) -> UiTask {
        if self.active_request_id.is_some() {
            return UiTask::None;
        }
        let Some(effect) = self.queued.pop_front() else {
            return UiTask::None;
        };
        self.active_request_id = Some(effect.request_id());
        effect.into_task()
    }

    fn complete_read(
        &mut self,
        request_id: u64,
        value: Option<String>,
    ) -> Option<ClipboardReadCompletion> {
        if self.active_request_id != Some(request_id) {
            return None;
        }
        let destination = self.pending_reads.remove(&request_id)?;
        self.active_request_id = None;
        Some(match destination {
            ClipboardReadDestination::Ipc(reply) => ClipboardReadCompletion::Ipc { reply, value },
            ClipboardReadDestination::Paste { tab_id } => {
                ClipboardReadCompletion::Paste { tab_id, value }
            }
        })
    }

    fn complete_write(&mut self, request_id: u64) -> bool {
        if self.active_request_id != Some(request_id) {
            return false;
        }
        self.active_request_id = None;
        true
    }
}

fn enqueue_osc_clipboard_write(
    clipboard: &mut ClipboardQueue,
    policy: config::ClipboardWrite,
    target: ClipboardTarget,
    text: String,
) -> bool {
    if policy == config::ClipboardWrite::Deny {
        return false;
    }
    let target = match target {
        ClipboardTarget::System => ClipboardOp::System,
        ClipboardTarget::Selection => ClipboardOp::Selection,
    };
    clipboard.enqueue_write(target, text);
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyKind {
    Explicit,
    OnSelect(config::CopyOnSelect),
}

fn enqueue_selection_copy(
    clipboard: &mut ClipboardQueue,
    kind: CopyKind,
    text: Option<String>,
) -> usize {
    let Some(text) = text.filter(|text| !text.is_empty()) else {
        return 0;
    };
    let targets: &[ClipboardOp] = match kind {
        CopyKind::Explicit => &[ClipboardOp::System, ClipboardOp::Selection],
        CopyKind::OnSelect(config::CopyOnSelect::Off) => &[],
        CopyKind::OnSelect(config::CopyOnSelect::True) => &[ClipboardOp::Selection],
        CopyKind::OnSelect(config::CopyOnSelect::Clipboard) => {
            &[ClipboardOp::System, ClipboardOp::Selection]
        }
    };
    for target in targets {
        clipboard.enqueue_write(*target, text.clone());
    }
    targets.len()
}

struct ScreenshotRequest {
    scale: u32,
    reply: ScreenshotReply,
}

#[derive(Default)]
struct ScreenshotQueue {
    pending: VecDeque<ScreenshotRequest>,
    in_flight: Option<ScreenshotRequest>,
}

impl ScreenshotQueue {
    fn enqueue(&mut self, scale: u32, reply: ScreenshotReply) {
        self.pending.push_back(ScreenshotRequest { scale, reply });
    }

    fn start_next(&mut self, window_id: Option<window::Id>) -> UiTask {
        if self.in_flight.is_some() {
            return UiTask::None;
        }
        let Some(window_id) = window_id else {
            return UiTask::None;
        };
        let Some(request) = self.pending.pop_front() else {
            return UiTask::None;
        };
        self.in_flight = Some(request);
        UiTask::Screenshot(window_id)
    }

    fn complete(&mut self) -> Option<ScreenshotRequest> {
        self.in_flight.take()
    }
}

struct WindowOpenResult {
    task: UiTask,
    retained_resize_scheduled: bool,
}

fn prepare_window_opened(
    window_id: &mut Option<window::Id>,
    pending_window_resize: &mut Option<Size>,
    screenshots: &mut ScreenshotQueue,
    id: window::Id,
) -> WindowOpenResult {
    *window_id = Some(id);
    let resize = pending_window_resize
        .take()
        .map(|size| UiTask::Resize(id, size));
    let retained_resize_scheduled = resize.is_some();
    let task = resize
        .unwrap_or(UiTask::None)
        .then(screenshots.start_next(*window_id));
    WindowOpenResult {
        task,
        retained_resize_scheduled,
    }
}

struct AgentMetricsResult {
    session: u64,
    claimed: Vec<String>,
    outcomes: Result<Vec<git_metrics::ProbeOutcome>, String>,
}

struct ProviderRunResult {
    palette_session: u64,
    request: u64,
    origin_frame: String,
    provider: provider::Provider,
    phase: provider::Phase,
    outcome: Result<provider::ProviderOutput, String>,
}

fn provider_result_is_current(
    palette_present: bool,
    palette_session: u64,
    provider_request: u64,
    current_frame: Option<&str>,
    result: &ProviderRunResult,
) -> bool {
    palette_present
        && result.palette_session == palette_session
        && result.request == provider_request
        && current_frame == Some(result.origin_frame.as_str())
}

fn report_palette_query_result(
    status: &mut StatusBanner,
    result: Result<(), String>,
    now: Instant,
) {
    if let Err(error) = result {
        status.set_at(error, now);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct NativePointerOutcome {
    selection_completed: bool,
    paste_selection: bool,
    open_url: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct NativePointerDispatch {
    action: PointerAction,
    button: Option<PointerButton>,
    col: u32,
    row: u32,
    mods: u16,
    click_count: u8,
    inside: bool,
    link_modifier_held: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalPointerGesture {
    Selection,
    MultiClick,
    Url,
}

fn pointer_origin_tab<V>(tabs: &mut HashMap<i64, V>, tab_id: i64) -> Option<&mut V> {
    tabs.get_mut(&tab_id)
}

fn paste_bytes(terminal: &Terminal, text: Option<&str>) -> Vec<u8> {
    let Some(text) = text.filter(|text| !text.is_empty()) else {
        return Vec::new();
    };
    if !terminal.mode_get(2004) {
        return text.as_bytes().to_vec();
    }
    let mut bytes = Vec::with_capacity(text.len() + 12);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct TerminalGeometry {
    cols: u16,
    rows: u16,
    metrics: TerminalMetrics,
    metric_generation: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct GeometryChange {
    previous: Option<TerminalGeometry>,
    current: TerminalGeometry,
    grid_changed: bool,
    metrics_changed: bool,
    deferred_replies: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
enum GeometryBatchOperation {
    Apply {
        tab_id: i64,
        cols: u16,
        rows: u16,
        metrics: TerminalMetrics,
        metric_generation: u64,
    },
    Rollback {
        tab_id: i64,
        previous: TerminalGeometry,
    },
}

#[derive(Debug, PartialEq, Eq)]
struct GeometryBatchFailure {
    tab_id: i64,
    apply: String,
    rollback: Vec<(i64, String)>,
}

fn apply_geometry_batch(
    tab_ids: &[i64],
    cols: u16,
    rows: u16,
    metrics: TerminalMetrics,
    metric_generation: u64,
    mut transition: impl FnMut(
        GeometryBatchOperation,
    ) -> std::result::Result<Option<GeometryChange>, String>,
) -> std::result::Result<Vec<(i64, GeometryChange)>, GeometryBatchFailure> {
    let mut applied = Vec::with_capacity(tab_ids.len());
    for tab_id in tab_ids {
        let operation = GeometryBatchOperation::Apply {
            tab_id: *tab_id,
            cols,
            rows,
            metrics,
            metric_generation,
        };
        match transition(operation) {
            Ok(Some(change)) => applied.push((*tab_id, change)),
            Ok(None) => {}
            Err(error) => {
                let rollback = applied
                    .iter()
                    .rev()
                    .filter_map(|(rollback_id, change): &(i64, GeometryChange)| {
                        let previous = change.previous?;
                        transition(GeometryBatchOperation::Rollback {
                            tab_id: *rollback_id,
                            previous,
                        })
                        .err()
                        .map(|error| (*rollback_id, error))
                    })
                    .collect();
                return Err(GeometryBatchFailure {
                    tab_id: *tab_id,
                    apply: error,
                    rollback,
                });
            }
        }
    }
    Ok(applied)
}

#[derive(Debug, Clone, Copy)]
enum FontSizeTransition {
    Adjust(f64),
    Reset,
}

fn font_size_candidate(
    current: &TerminalTypography,
    font: Font,
    transition: FontSizeTransition,
) -> Result<Option<(TerminalTypography, TerminalMetrics)>, String> {
    let mut candidate = current.clone();
    let changed = match transition {
        FontSizeTransition::Adjust(delta) => candidate.adjust_size(delta).is_some(),
        FontSizeTransition::Reset => candidate.reset_size().is_some(),
    };
    if !changed {
        return Ok(None);
    }
    let metrics = TerminalMetrics::measure_with_font(candidate.current_size_pt(), font)?;
    Ok(Some((candidate, metrics)))
}

fn terminal_grid(size: Size, sidebar_collapsed: bool, metrics: TerminalMetrics) -> (u16, u16) {
    let width = (size.width - sidebar_width(sidebar_collapsed) - 2.0 * TERMINAL_PADDING)
        .max(metrics.cell_width * 2.0);
    let height =
        (size.height - chrome::BAND_HEIGHT - 2.0 * TERMINAL_PADDING).max(metrics.cell_height * 2.0);
    (
        ((width / metrics.cell_width).floor() as u16).max(2),
        ((height / metrics.cell_height).floor() as u16).max(2),
    )
}

struct TerminalTab {
    terminal: Terminal,
    render_state: RenderState,
    encoder: KeyEncoder,
    mouse_encoder: MouseEncoder,
    scroll: TerminalScroll,
    motion_emitter: MotionEmitter,
    tracking_pointer: Option<PointerButton>,
    local_pointer_gesture: Option<LocalPointerGesture>,
    last_pointer_cell: Option<(u16, u16)>,
    link_modifier_held: bool,
    hover_url: Option<HoverUrl>,
    selection: TerminalSelection,
    word_break_chars: String,
    input_started_at: Instant,
    session: TabSession,
    output_rx: tokio::sync::mpsc::UnboundedReceiver<TabOutput>,
    reply_buffer: Arc<Mutex<Vec<u8>>>,
    input_capture: Option<InputCapture>,
    osc_router: OscRouter,
    pointer_shape: String,
    theme: Theme,
    snapshot: TerminalSnapshot,
    cols: u16,
    rows: u16,
    applied_metrics: Option<TerminalMetrics>,
    metric_generation: u64,
}

impl TerminalTab {
    fn attach(
        supervisor: Arc<PtySupervisor>,
        tab_id: i64,
        test_mode: bool,
        theme: Theme,
        word_break_chars: String,
    ) -> Result<Self> {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            max_scrollback: 2_000,
        })?;
        terminal.set_color_foreground(theme.foreground)?;
        terminal.set_color_background(theme.background)?;
        terminal.set_color_cursor(theme.cursor)?;
        terminal.set_color_palette(&theme.palette)?;
        let reply_buffer = Arc::new(Mutex::new(Vec::new()));
        terminal
            .set_write_pty_buffer(Arc::clone(&reply_buffer))
            .context("install libghostty PTY reply buffer")?;
        let render_state = RenderState::new()?;
        let encoder = KeyEncoder::new()?;
        let mouse_encoder = MouseEncoder::new()?;
        let input_capture = test_mode.then(|| Arc::new(Mutex::new(Vec::new())));
        let (output_tx, output_rx) = tokio::sync::mpsc::unbounded_channel();
        let session = TabSession::attach(supervisor, tab_id, output_tx, input_capture.clone())?;
        Ok(Self {
            terminal,
            render_state,
            encoder,
            mouse_encoder,
            scroll: TerminalScroll::new(),
            motion_emitter: MotionEmitter::new(),
            tracking_pointer: None,
            local_pointer_gesture: None,
            last_pointer_cell: None,
            link_modifier_held: false,
            hover_url: None,
            selection: TerminalSelection::new(),
            word_break_chars,
            input_started_at: Instant::now(),
            session,
            output_rx,
            reply_buffer,
            input_capture,
            osc_router: OscRouter::new(),
            pointer_shape: "default".into(),
            theme,
            snapshot: TerminalSnapshot::blank(DEFAULT_COLS, DEFAULT_ROWS),
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            applied_metrics: None,
            metric_generation: 0,
        })
    }

    fn write_vt(&mut self, bytes: &[u8]) -> Vec<OscAction> {
        let colors = self.osc_colors();
        let actions = self.osc_router.feed(bytes, &colors);
        self.terminal.vt_write(bytes);
        self.drain_terminal_replies();
        actions
    }

    fn osc_colors(&self) -> OscColorSnapshot {
        let fallback_foreground = self.theme.foreground;
        let fallback_background = self.theme.background;
        let (foreground, background, cursor) = self
            .terminal
            .live_colors()
            .map(|colors| {
                (
                    colors.foreground,
                    colors.background,
                    colors.cursor.unwrap_or(self.theme.cursor),
                )
            })
            .unwrap_or((fallback_foreground, fallback_background, self.theme.cursor));
        let palette = self.terminal.live_palette().unwrap_or(self.theme.palette);
        let rgb = |color: roost_vt::ColorRgb| (color.r, color.g, color.b);
        OscColorSnapshot::new(
            rgb(foreground),
            rgb(background),
            rgb(cursor),
            palette.map(rgb),
        )
    }

    fn drain_terminal_replies(&self) {
        self.session.send_input(self.take_terminal_replies());
    }

    fn take_terminal_replies(&self) -> Vec<u8> {
        self.reply_buffer
            .lock()
            .map(|mut buffer| std::mem::take(&mut *buffer))
            .unwrap_or_default()
    }

    fn apply_geometry(
        &mut self,
        cols: u16,
        rows: u16,
        metrics: TerminalMetrics,
        metric_generation: u64,
    ) -> Result<Option<GeometryChange>> {
        let grid_changed = self.cols != cols || self.rows != rows;
        let metrics_changed = self.applied_metrics != Some(metrics);
        if !grid_changed && !metrics_changed {
            return Ok(None);
        }
        let previous = self.applied_metrics.map(|metrics| TerminalGeometry {
            cols: self.cols,
            rows: self.rows,
            metrics,
            metric_generation: self.metric_generation,
        });
        let resize = self.terminal.resize(
            cols,
            rows,
            metrics.cell_width.round().max(1.0) as u32,
            metrics.cell_height.round().max(1.0) as u32,
        );
        let deferred_replies = self.take_terminal_replies();
        resize?;
        self.cols = cols;
        self.rows = rows;
        self.applied_metrics = Some(metrics);
        self.metric_generation = metric_generation;
        self.hover_url = None;
        self.last_pointer_cell = self.last_pointer_cell.map(|(col, row)| {
            (
                col.min(cols.saturating_sub(1)),
                row.min(rows.saturating_sub(1)),
            )
        });
        Ok(Some(GeometryChange {
            previous,
            current: TerminalGeometry {
                cols,
                rows,
                metrics,
                metric_generation,
            },
            grid_changed,
            metrics_changed,
            deferred_replies,
        }))
    }

    fn rollback_geometry(&mut self, previous: TerminalGeometry) -> Result<()> {
        let resize = self.terminal.resize(
            previous.cols,
            previous.rows,
            previous.metrics.cell_width.round().max(1.0) as u32,
            previous.metrics.cell_height.round().max(1.0) as u32,
        );
        // The candidate report was staged in `GeometryChange`; the rollback
        // report describes an internal transition the PTY never observed.
        // Neither may escape a failed all-tab transaction.
        let _ = self.take_terminal_replies();
        resize?;
        self.cols = previous.cols;
        self.rows = previous.rows;
        self.applied_metrics = Some(previous.metrics);
        self.metric_generation = previous.metric_generation;
        self.hover_url = None;
        self.last_pointer_cell = self.last_pointer_cell.map(|(col, row)| {
            (
                col.min(previous.cols.saturating_sub(1)),
                row.min(previous.rows.saturating_sub(1)),
            )
        });
        Ok(())
    }

    fn commit_geometry(&self, change: GeometryChange) {
        self.session.send_input(change.deferred_replies);
        if change.grid_changed {
            self.session
                .send_resize(change.current.cols, change.current.rows);
        }
    }

    fn prepare_pointer_cancel(&mut self) -> Result<Vec<u8>> {
        if let Some(button) = self.tracking_pointer {
            let (col, row) = self.last_pointer_cell.unwrap_or_default();
            self.encode_pointer(
                PointerAction::Release,
                Some(button),
                u32::from(col),
                u32::from(row),
                0,
            )
        } else {
            Ok(Vec::new())
        }
    }

    fn commit_pointer_cancel(&mut self, release: Vec<u8>) {
        self.session.send_input(release);
        self.tracking_pointer = None;
        self.local_pointer_gesture = None;
        self.last_pointer_cell = None;
        self.hover_url = None;
    }

    fn dispatch_pointer(
        &mut self,
        action: PointerAction,
        button: Option<PointerButton>,
        col: u32,
        row: u32,
        mods: u16,
    ) -> Result<()> {
        let col = col.min(u32::from(self.cols.saturating_sub(1)));
        let row = row.min(u32::from(self.rows.saturating_sub(1)));
        let motion_without_button = action == PointerAction::Motion && button.is_none();
        let now = self.input_started_at.elapsed().as_secs_f64();
        if motion_without_button && !self.motion_emitter.would_emit(col, row, now) {
            return Ok(());
        }

        let bytes = self.encode_pointer(action, button, col, row, mods)?;
        if bytes.is_empty() {
            return Ok(());
        }
        if motion_without_button {
            self.motion_emitter.commit(col, row, now);
        }
        self.session.send_input(bytes);
        Ok(())
    }

    fn encode_pointer(
        &mut self,
        action: PointerAction,
        button: Option<PointerButton>,
        col: u32,
        row: u32,
        mods: u16,
    ) -> Result<Vec<u8>> {
        let Some(metrics) = self.applied_metrics else {
            return Ok(Vec::new());
        };
        let cell_width = metrics.cell_width.round().max(1.0) as u32;
        let cell_height = metrics.cell_height.round().max(1.0) as u32;
        self.mouse_encoder.sync_from_terminal(&self.terminal);
        self.mouse_encoder.set_size(
            u32::from(self.cols) * cell_width,
            u32::from(self.rows) * cell_height,
            cell_width,
            cell_height,
        );
        let mut event = MouseEvent::new().context("allocate terminal mouse event")?;
        event
            .set_action(pointer_action(action))
            .set_mods(mods)
            .set_position(
                (col * cell_width) as f32 + cell_width as f32 / 2.0,
                (row * cell_height) as f32 + cell_height as f32 / 2.0,
            );
        if let Some(button) = button {
            event.set_button(pointer_button(button));
        } else {
            event.clear_button();
        }
        self.mouse_encoder
            .encode(&event)
            .context("encode terminal mouse event")
    }

    fn handle_wheel(&mut self, history_rows: f64, col: u32, row: u32, mods: u16) -> Result<()> {
        let route = self.scroll.route(&mut self.terminal, history_rows);
        match route {
            Some(ScrollRoute::MouseReport { direction, rows }) => {
                let button = match direction {
                    ScrollDirection::History => PointerButton::Four,
                    ScrollDirection::Bottom => PointerButton::Five,
                };
                for _ in 0..rows {
                    self.dispatch_pointer(PointerAction::Press, Some(button), col, row, mods)?;
                }
            }
            Some(ScrollRoute::AlternateScreenKey { direction, rows }) => {
                let key = match direction {
                    ScrollDirection::History => roost_vt::ffi::GhosttyKey_GHOSTTY_KEY_ARROW_UP,
                    ScrollDirection::Bottom => roost_vt::ffi::GhosttyKey_GHOSTTY_KEY_ARROW_DOWN,
                };
                let mut event = KeyEvent::new().context("allocate terminal wheel key event")?;
                event.set_action(key_action::PRESS);
                event.set_key(key);
                event.set_mods(0);
                self.encoder.sync_from_terminal(&self.terminal);
                let mut bytes = Vec::new();
                for _ in 0..rows {
                    bytes.extend(
                        self.encoder
                            .encode(&event)
                            .context("encode alternate-screen wheel key")?,
                    );
                }
                self.session.send_input(bytes);
            }
            Some(ScrollRoute::LocalViewport { .. }) | None => {}
        }
        Ok(())
    }

    fn snap_to_bottom_for_input(&mut self) -> Result<bool> {
        let snapped = self.scroll.snap_to_bottom(&mut self.terminal);
        if snapped {
            self.refresh_snapshot()?;
        }
        Ok(snapped)
    }

    /// Route a native pointer gesture with terminal mouse reporting taking
    /// precedence over local selection for the lifetime of the press.
    fn handle_native_pointer(
        &mut self,
        event: NativePointerDispatch,
    ) -> Result<NativePointerOutcome> {
        let NativePointerDispatch {
            action,
            button,
            col,
            row,
            mods,
            click_count,
            inside,
            link_modifier_held,
        } = event;
        let col = col.min(u32::from(self.cols.saturating_sub(1)));
        let row = row.min(u32::from(self.rows.saturating_sub(1)));
        let cell = (col as u16, row as u16);
        if inside {
            self.last_pointer_cell = Some(cell);
        } else {
            self.last_pointer_cell = None;
        }
        self.set_link_modifier_held(link_modifier_held)?;
        match action {
            PointerAction::Press if button == Some(PointerButton::Left) && link_modifier_held => {
                if let Some(hover) = self.compute_hover_url(cell.0, cell.1)? {
                    let url = hover.url.clone();
                    self.hover_url = Some(hover);
                    self.local_pointer_gesture = Some(LocalPointerGesture::Url);
                    return Ok(NativePointerOutcome {
                        open_url: Some(url),
                        ..NativePointerOutcome::default()
                    });
                }
                self.route_press_without_link(button, col, row, mods, click_count)
            }
            PointerAction::Motion if self.tracking_pointer.is_some() => {
                self.dispatch_pointer(action, self.tracking_pointer, col, row, mods)?;
                Ok(NativePointerOutcome::default())
            }
            PointerAction::Release if self.tracking_pointer.is_some() => {
                let captured = self.tracking_pointer.take();
                self.dispatch_pointer(action, captured, col, row, mods)?;
                Ok(NativePointerOutcome::default())
            }
            PointerAction::Motion => match self.local_pointer_gesture {
                Some(LocalPointerGesture::Selection) => {
                    self.selection.update(&self.terminal, cell.0, cell.1);
                    Ok(NativePointerOutcome::default())
                }
                Some(LocalPointerGesture::MultiClick | LocalPointerGesture::Url) => {
                    Ok(NativePointerOutcome::default())
                }
                None if self.terminal.mouse_tracking() => {
                    self.dispatch_pointer(action, button, col, row, mods)?;
                    Ok(NativePointerOutcome::default())
                }
                None => Ok(NativePointerOutcome::default()),
            },
            PointerAction::Release => match self.local_pointer_gesture.take() {
                Some(LocalPointerGesture::Selection) => {
                    self.selection.update(&self.terminal, cell.0, cell.1);
                    Ok(NativePointerOutcome {
                        selection_completed: true,
                        ..NativePointerOutcome::default()
                    })
                }
                Some(LocalPointerGesture::MultiClick | LocalPointerGesture::Url) | None => {
                    Ok(NativePointerOutcome::default())
                }
            },
            PointerAction::Press => {
                self.route_press_without_link(button, col, row, mods, click_count)
            }
        }
    }

    fn route_press_without_link(
        &mut self,
        button: Option<PointerButton>,
        col: u32,
        row: u32,
        mods: u16,
        click_count: u8,
    ) -> Result<NativePointerOutcome> {
        let cell = (col as u16, row as u16);
        if self.terminal.mouse_tracking() {
            self.local_pointer_gesture = None;
            if matches!(
                button,
                Some(PointerButton::Left | PointerButton::Right | PointerButton::Middle)
            ) {
                self.tracking_pointer = button;
            }
            self.dispatch_pointer(PointerAction::Press, button, col, row, mods)?;
            return Ok(NativePointerOutcome::default());
        }
        if button == Some(PointerButton::Left)
            && click_count >= 2
            && self
                .expand_selection_at(cell.0, cell.1, click_count)?
                .is_some()
        {
            self.local_pointer_gesture = Some(LocalPointerGesture::MultiClick);
            return Ok(NativePointerOutcome {
                selection_completed: true,
                ..NativePointerOutcome::default()
            });
        }
        if button == Some(PointerButton::Left) {
            self.local_pointer_gesture = self
                .selection
                .begin(&self.terminal, cell.0, cell.1)
                .then_some(LocalPointerGesture::Selection);
            return Ok(NativePointerOutcome::default());
        }
        if button == Some(PointerButton::Middle) {
            return Ok(NativePointerOutcome {
                paste_selection: true,
                ..NativePointerOutcome::default()
            });
        }
        Ok(NativePointerOutcome::default())
    }

    fn pointer_leave(&mut self) {
        self.last_pointer_cell = None;
        self.hover_url = None;
    }

    fn reset_pointer_state(&mut self) -> bool {
        let gesture = self.local_pointer_gesture.take();
        let tracking = self.tracking_pointer.take();
        let cell = self.last_pointer_cell.take();
        let hover = self.hover_url.take();
        let modifier = std::mem::take(&mut self.link_modifier_held);
        gesture.is_some() || tracking.is_some() || cell.is_some() || hover.is_some() || modifier
    }

    fn effective_pointer_shape(&self) -> &str {
        if self.hover_url.is_some() {
            "pointer"
        } else {
            &self.pointer_shape
        }
    }

    fn set_link_modifier_held(&mut self, held: bool) -> Result<()> {
        self.link_modifier_held = held;
        self.recompute_hover()
    }

    fn recompute_hover(&mut self) -> Result<()> {
        self.hover_url = match (self.link_modifier_held, self.last_pointer_cell) {
            (true, Some((col, row))) => self.compute_hover_url(col, row)?,
            _ => None,
        };
        Ok(())
    }

    fn compute_hover_url(&mut self, col: u16, row: u16) -> Result<Option<HoverUrl>> {
        if let Some(url) = self.terminal.hyperlink_at(col, u32::from(row)) {
            let (col0, col1) = roost_url::contiguous_hyperlink_span(
                col,
                self.cols.saturating_sub(1),
                &url,
                |candidate| self.terminal.hyperlink_at(candidate, u32::from(row)),
            );
            return Ok(Some(HoverUrl {
                col0,
                col1,
                row,
                url,
            }));
        }
        let projection = TerminalSelection::row_text_projection(
            &self.terminal,
            &mut self.render_state,
            row,
            self.cols,
        )?;
        let Some(char_col) = projection
            .char_index_at_cell(col)
            .and_then(|index| u16::try_from(index).ok())
        else {
            return Ok(None);
        };
        let Some(span) = roost_url::find_url_at(projection.text(), char_col) else {
            return Ok(None);
        };
        let Some((col0, col1)) =
            projection.cell_span_for_chars(usize::from(span.col0), usize::from(span.col1))
        else {
            return Ok(None);
        };
        Ok(Some(HoverUrl {
            col0,
            col1,
            row,
            url: span.url,
        }))
    }

    fn selected_text(&mut self) -> Result<Option<String>> {
        Ok(self.selection.selected_text(
            &self.terminal,
            &mut self.render_state,
            self.cols,
            self.rows,
        )?)
    }

    fn paste(&self, text: Option<&str>) {
        let bytes = paste_bytes(&self.terminal, text);
        if !bytes.is_empty() {
            self.session.send_input(bytes);
        }
    }

    fn selection_dump(&mut self) -> Result<Option<SelectionData>> {
        Ok(self
            .selection
            .snapshot(&self.terminal, &mut self.render_state, self.cols, self.rows)?
            .map(|snapshot| SelectionData {
                text: snapshot.text,
                anchor_visible: snapshot.anchor_visible,
                cursor_visible: snapshot.cursor_visible,
            }))
    }

    fn expand_selection_at(
        &mut self,
        col: u16,
        row: u16,
        click_count: u8,
    ) -> Result<Option<ExpandSelectionData>> {
        let row_text = TerminalSelection::row_text(&self.terminal, &mut self.render_state, row)?;
        let span = match click_count {
            2 => {
                roost_ui_model::word_selection::expand_word(&row_text, col, &self.word_break_chars)
            }
            _ => Some(roost_ui_model::word_selection::expand_line(&row_text)),
        };
        let Some(span) = span else {
            return Ok(None);
        };
        if !self
            .selection
            .set(&self.terminal, (span.col0, row), (span.col1, row))
        {
            return Ok(None);
        }
        let text = self.selection.selected_text(
            &self.terminal,
            &mut self.render_state,
            self.cols,
            self.rows,
        )?;
        Ok(Some(ExpandSelectionData {
            col0: span.col0,
            col1: span.col1,
            text,
        }))
    }

    fn set_window_focus(&self, focused: bool) {
        let bytes = self.terminal.encode_focus(focused);
        if !bytes.is_empty() {
            self.session.send_input(bytes);
        }
    }

    fn set_theme(&mut self, theme: &Theme) -> Result<()> {
        let previous = self.theme.clone();
        if let Err(failure) = apply_with_rollback(&previous, theme, |candidate| {
            self.apply_theme_candidate(candidate)
        }) {
            return Err(match failure.rollback {
                Some(rollback) => anyhow::anyhow!(
                    "theme apply failed: {}; rollback failed: {}",
                    failure.apply,
                    rollback
                ),
                None => anyhow::anyhow!("theme apply failed: {}", failure.apply),
            });
        }
        if self.terminal.mode_get(2031) {
            self.session.send_input(if theme.background.is_light() {
                b"\x1b[?997;2n".to_vec()
            } else {
                b"\x1b[?997;1n".to_vec()
            });
        }
        Ok(())
    }

    fn apply_theme_candidate(&mut self, theme: &Theme) -> Result<()> {
        self.theme = theme.clone();
        self.terminal.set_color_foreground(theme.foreground)?;
        self.terminal.set_color_background(theme.background)?;
        self.terminal.set_color_cursor(theme.cursor)?;
        self.terminal.set_color_palette(&theme.palette)?;
        self.refresh_snapshot()
    }

    fn refresh_snapshot(&mut self) -> Result<()> {
        self.recompute_hover()?;
        self.render_state.update(&self.terminal)?;
        let colors = self.render_state.colors()?;
        let cursor = self.render_state.cursor();
        let mut cells = Vec::new();
        let mut rows = vec![vec![String::new(); usize::from(self.cols)]; usize::from(self.rows)];
        self.render_state.walk(&self.terminal, |row, cell| {
            if row >= u32::from(self.rows) || cell.col >= self.cols {
                return;
            }
            let text = if cell.text.is_empty() {
                " ".to_string()
            } else {
                cell.text
            };
            rows[row as usize][usize::from(cell.col)] = text.clone();
            let (foreground, background) = resolve_colors(
                cell.fg,
                cell.bg,
                (colors.foreground, colors.background),
                cell.style.inverse,
            );
            if text != " " || cell.bg.is_some() || cell.style.inverse {
                cells.push(DrawCell {
                    row,
                    col: cell.col,
                    text,
                    foreground,
                    background,
                    explicit_background: cell.bg.is_some() || cell.style.inverse,
                    bold: cell.style.bold,
                    italic: cell.style.italic,
                    inverse: cell.style.inverse,
                });
            }
        })?;
        let rows_text = rows
            .into_iter()
            .map(|row| row.concat().trim_end().to_string())
            .collect();
        self.snapshot = TerminalSnapshot {
            cols: self.cols,
            rows: self.rows,
            foreground: colors.foreground,
            background: colors.background,
            cursor,
            cells,
            rows_text,
            selection_background: self.theme.selection_background,
            selection_spans: self
                .selection
                .visible_spans(&self.terminal, self.cols, self.rows),
            link_hover: self
                .hover_url
                .as_ref()
                .map(|hover| roost_vt::SelectionSpan {
                    row: hover.row,
                    col0: hover.col0,
                    col1: hover.col1.saturating_add(1),
                }),
            pointer_shape: self.effective_pointer_shape().into(),
        };
        Ok(())
    }

    fn dump(&self) -> DumpData {
        DumpData {
            cols: u32::from(self.snapshot.cols),
            rows: u32::from(self.snapshot.rows),
            cursor: self
                .snapshot
                .cursor
                .filter(|cursor| cursor.visible)
                .map(|cursor| (cursor.row, cursor.col, cursor.visible)),
            rows_text: self.snapshot.rows_text.clone(),
        }
    }

    fn resolved_cells(&self) -> ResolvedCellsData {
        let mut by_position: HashMap<(u32, u16), &DrawCell> = self
            .snapshot
            .cells
            .iter()
            .map(|cell| ((cell.row, cell.col), cell))
            .collect();
        let mut cells = Vec::with_capacity(usize::from(self.cols) * usize::from(self.rows));
        for row in 0..u32::from(self.rows) {
            for col in 0..self.cols {
                let cell = by_position.remove(&(row, col));
                let foreground = cell.map_or(self.snapshot.foreground, |cell| cell.foreground);
                let background = cell.map_or(self.snapshot.background, |cell| cell.background);
                cells.push(ResolvedCellData {
                    row,
                    col,
                    text: cell.map_or_else(|| " ".into(), |cell| cell.text.clone()),
                    fg: (foreground.r, foreground.g, foreground.b),
                    bg: (background.r, background.g, background.b),
                    has_explicit_bg: cell.is_some_and(|cell| cell.explicit_background),
                    bold: cell.is_some_and(|cell| cell.bold),
                    italic: cell.is_some_and(|cell| cell.italic),
                    inverse: cell.is_some_and(|cell| cell.inverse),
                });
            }
        }
        ResolvedCellsData {
            cols: self.cols,
            rows: self.rows,
            cells,
        }
    }
}

pub struct App {
    workspace: Arc<Workspace>,
    supervisor: Arc<PtySupervisor>,
    client: LocalClient,
    tabs: HashMap<i64, TerminalTab>,
    projects: Vec<Project>,
    sidebar_agents: HashMap<i64, Vec<SidebarDumpAgentRow>>,
    notification_inbox: notification_inbox::NotificationInbox,
    workspace_events: tokio::sync::broadcast::Receiver<WorkspaceEvent>,
    ui_rx: tokio::sync::mpsc::UnboundedReceiver<UiRequest>,
    window_id: Option<window::Id>,
    pending_window_resize: Option<Size>,
    screenshots: ScreenshotQueue,
    window_size: Size,
    modifiers: keyboard::Modifiers,
    test_mode: bool,
    status: StatusBanner,
    rename_editor: Option<RenameEditor>,
    rename_input_id: Id,
    rename_focus_requested: bool,
    rename_completion_key: Option<RenameCompletionKey>,
    tab_drag_preview: Option<TabDragPreview>,
    tab_strip_generation: u64,
    config: RoostConfig,
    typography: TerminalTypography,
    font_registry: &'static FontRegistry,
    terminal_metrics: TerminalMetrics,
    metric_generation: u64,
    keybindings: HashMap<Accel, KeybindAction>,
    active_theme_name: String,
    palette: Option<palette::PaletteState>,
    palette_session: u64,
    palette_theme_at_open: Option<String>,
    palette_family_at_open: Option<Option<String>>,
    palette_resolved_family_at_open: Option<String>,
    palette_input_id: Id,
    palette_scroll_id: Id,
    palette_focus_requested: bool,
    palette_layout_revision: u64,
    palette_measurement_generation: u64,
    palette_reveal_required: bool,
    palette_reveal_attempts: u8,
    palette_selected_in_view: Option<bool>,
    palette_visibility_request: PaletteVisibilityRequest,
    palette_visibility_retries: u8,
    git_probe: Arc<git_metrics::GitProbe>,
    metrics_cache: git_metrics::MetricsCache,
    metrics_tx: tokio::sync::mpsc::UnboundedSender<AgentMetricsResult>,
    metrics_rx: tokio::sync::mpsc::UnboundedReceiver<AgentMetricsResult>,
    provider_request: u64,
    provider_tx: tokio::sync::mpsc::UnboundedSender<ProviderRunResult>,
    provider_rx: tokio::sync::mpsc::UnboundedReceiver<ProviderRunResult>,
    provider_frames: HashMap<String, provider::Provider>,
    palette_present_reply:
        Option<tokio::sync::oneshot::Sender<Result<PalettePresentResult, String>>>,
    clipboard: ClipboardQueue,
    // Field order is intentional: terminal sessions and IPC receivers are
    // dropped before the runtime; the lock is held until every runtime task
    // has been cancelled and joined by Runtime::drop.
    runtime: tokio::runtime::Runtime,
    _lock: InstanceLock,
}

impl App {
    pub fn bootstrap(profile: &BundleProfile, lock: InstanceLock) -> Result<Self> {
        let config = RoostConfig::load_default();
        let font_registry = system_font_registry();
        let configured_typography =
            TerminalTypography::new(config.font_family.clone(), config.font_size);
        let configured_font = font_registry.resolve(configured_typography.effective_family());
        let (typography, terminal_metrics) = match TerminalMetrics::measure_with_font(
            configured_typography.current_size_pt(),
            configured_font.font,
        ) {
            Ok(metrics) => (configured_typography, metrics),
            Err(error) => {
                tracing::warn!(
                    configured_size = configured_typography.current_size_pt(),
                    %error,
                    "configured font size cannot be rendered by Iced; using Rust UI default"
                );
                let fallback = TerminalTypography::new(None, None);
                let fallback_font = font_registry.resolve(fallback.effective_family());
                let metrics = TerminalMetrics::measure_with_font(
                    fallback.current_size_pt(),
                    fallback_font.font,
                )
                .map_err(anyhow::Error::msg)
                .context("measure default Iced terminal font")?;
                (fallback, metrics)
            }
        };
        let keybindings = keybind::canonicalize_bindings(
            keybind::default_bindings(),
            config.keybinds.clone(),
            |warning| tracing::warn!("keybind: {warning}"),
        );
        let active_theme_name = config
            .theme_name
            .clone()
            .unwrap_or_else(|| "roost-dark".into());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("build Iced engine runtime")?;
        let workspace = Arc::new(Workspace::open(profile.state_json_path()));
        let workspace_events = workspace.subscribe();
        let supervisor = Arc::new(PtySupervisor::new());
        let client = LocalClient::new(
            Arc::clone(&workspace),
            Arc::clone(&supervisor),
            profile.socket_path.clone(),
        );

        hydrate_workspace(&runtime, &client)?;

        let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let (metrics_tx, metrics_rx) = tokio::sync::mpsc::unbounded_channel();
        let (provider_tx, provider_rx) = tokio::sync::mpsc::unbounded_channel();
        let handler = IpcHandler::new(
            Arc::clone(&workspace),
            Arc::clone(&supervisor),
            profile.socket_path.clone(),
            profile.app_label,
            profile.app_id,
        )
        .with_ui(ui_tx);
        let server = runtime
            .block_on(IpcServer::bind(&profile.socket_path, handler))
            .context("bind Iced IPC server")?;
        runtime.spawn(async move {
            if let Err(error) = server.run().await {
                tracing::warn!(?error, "Iced IPC server stopped");
            }
        });

        let mut app = Self {
            workspace,
            supervisor,
            client,
            tabs: HashMap::new(),
            projects: Vec::new(),
            sidebar_agents: HashMap::new(),
            notification_inbox: notification_inbox::NotificationInbox::new(),
            workspace_events,
            ui_rx,
            window_id: None,
            pending_window_resize: None,
            screenshots: ScreenshotQueue::default(),
            window_size: Size::new(1100.0, 720.0),
            modifiers: keyboard::Modifiers::default(),
            test_mode: std::env::var("ROOST_TEST_MODE").as_deref() == Ok("1"),
            status: StatusBanner::default(),
            rename_editor: None,
            rename_input_id: Id::unique(),
            rename_focus_requested: false,
            rename_completion_key: None,
            tab_drag_preview: None,
            tab_strip_generation: 1,
            config,
            typography,
            font_registry,
            terminal_metrics,
            metric_generation: 1,
            keybindings,
            active_theme_name,
            palette: None,
            palette_session: 0,
            palette_theme_at_open: None,
            palette_family_at_open: None,
            palette_resolved_family_at_open: None,
            palette_input_id: Id::unique(),
            palette_scroll_id: Id::unique(),
            palette_focus_requested: false,
            palette_layout_revision: 0,
            palette_measurement_generation: 0,
            palette_reveal_required: false,
            palette_reveal_attempts: 0,
            palette_selected_in_view: None,
            palette_visibility_request: PaletteVisibilityRequest::None,
            palette_visibility_retries: 0,
            git_probe: Arc::new(git_metrics::GitProbe::new()),
            metrics_cache: git_metrics::MetricsCache::default(),
            metrics_tx,
            metrics_rx,
            provider_request: 0,
            provider_tx,
            provider_rx,
            provider_frames: HashMap::new(),
            palette_present_reply: None,
            clipboard: ClipboardQueue::default(),
            runtime,
            _lock: lock,
        };
        app.reconcile();
        app.resize(app.window_size);
        tracing::info!(socket = %profile.socket_path.display(), "Iced walking skeleton ready");
        Ok(app)
    }

    pub fn window_opened(&mut self, id: window::Id) -> UiTask {
        prepare_window_opened(
            &mut self.window_id,
            &mut self.pending_window_resize,
            &mut self.screenshots,
            id,
        )
        .task
    }

    pub fn window_resized(&mut self, id: window::Id, size: Size) -> UiTask {
        let opened = prepare_window_opened(
            &mut self.window_id,
            &mut self.pending_window_resize,
            &mut self.screenshots,
            id,
        );
        if !opened.retained_resize_scheduled {
            self.resize(size);
        }
        // A native resize event confirms that Iced has rebuilt the widget
        // viewport. The test IPC path may already have stored the same logical
        // size, so invalidate even when `resize` sees no numeric change.
        if self.palette.is_some() {
            self.invalidate_palette_geometry(PaletteVisibilityRequest::Reveal);
        }
        opened.task
    }

    pub fn tick(&mut self) -> UiTask {
        self.status.expire_at(Instant::now());
        let mut task = self.service_ui_requests();
        self.service_agent_metrics();
        self.service_provider_results();
        self.service_workspace_events();
        self.reconcile();
        let mut exited = Vec::new();
        let mut osc_actions = Vec::new();
        let mut output_error = None;
        for (tab_id, tab) in &mut self.tabs {
            while let Ok(output) = tab.output_rx.try_recv() {
                match output {
                    TabOutput::Bytes(bytes) => {
                        osc_actions.push((*tab_id, tab.write_vt(&bytes)));
                    }
                    TabOutput::Exit { status, reason } => {
                        tracing::info!(tab_id, status, %reason, "PTY exited");
                        exited.push(*tab_id);
                    }
                    TabOutput::Error(error) => {
                        // Broadcast lag cannot be reconstructed. Surface it and
                        // keep the workspace alive so IPC/UI state still resyncs.
                        output_error = Some(format!("tab {tab_id}: {error}"));
                        tracing::error!(tab_id, %error, "PTY output stream lost bytes");
                    }
                }
            }
            if let Err(error) = tab.refresh_snapshot() {
                tracing::warn!(tab_id, ?error, "terminal snapshot refresh failed");
            }
        }
        if let Some(error) = output_error {
            self.set_status(error);
        }
        for (tab_id, actions) in osc_actions {
            task = task.then(self.apply_osc_actions(tab_id, actions));
        }
        for tab_id in exited {
            let _ = self.workspace.close_tab(tab_id);
            self.tabs.remove(&tab_id);
        }
        task = task.then(self.take_rename_focus_task());
        task = task.then(self.take_palette_visibility_task());
        task = task.then(self.screenshots.start_next(self.window_id));
        task
    }

    pub fn clipboard_read_completed(&mut self, request_id: u64, value: Option<String>) -> UiTask {
        let Some(completion) = self.clipboard.complete_read(request_id, value) else {
            tracing::warn!(request_id, "ignored stale native clipboard read result");
            return UiTask::None;
        };
        match completion {
            ClipboardReadCompletion::Ipc { reply, value } => {
                let _ = reply.send(Ok(value));
            }
            ClipboardReadCompletion::Paste { tab_id, value } => {
                if let Some(tab) = self.tabs.get(&tab_id) {
                    tab.paste(value.as_deref());
                } else {
                    tracing::debug!(tab_id, request_id, "discarded paste for a closed tab");
                }
            }
        }
        self.clipboard.start_next()
    }

    fn set_status(&mut self, message: impl Into<String>) {
        self.status.set_at(message, Instant::now());
    }

    pub fn clipboard_write_completed(&mut self, request_id: u64) -> UiTask {
        if !self.clipboard.complete_write(request_id) {
            tracing::warn!(request_id, "ignored stale native clipboard write result");
            return UiTask::None;
        }
        self.clipboard.start_next()
    }

    pub fn screenshot_captured(&mut self, capture: &window::Screenshot) -> UiTask {
        if let Some(request) = self.screenshots.complete() {
            let result = crate::screenshot::encode(capture, request.scale);
            let _ = request.reply.send(result);
        } else {
            tracing::warn!("received an Iced screenshot with no request in flight");
        }
        self.screenshots.start_next(self.window_id)
    }

    pub fn resize(&mut self, size: Size) {
        let changed = self.window_size != size;
        self.window_size = size;
        let (cols, rows) = terminal_grid(
            size,
            self.workspace.sidebar_collapsed(),
            self.terminal_metrics,
        );
        for (tab_id, tab) in &mut self.tabs {
            match tab.apply_geometry(cols, rows, self.terminal_metrics, self.metric_generation) {
                Ok(Some(change)) => tab.commit_geometry(change),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(?error, tab_id, cols, rows, "terminal resize failed")
                }
            }
        }
        if changed && self.palette.is_some() {
            self.invalidate_palette_geometry(PaletteVisibilityRequest::Reveal);
        }
    }

    pub fn keyboard(&mut self, event: keyboard::Event) -> UiTask {
        if consume_rename_completion_key(&mut self.rename_completion_key, &event) {
            return UiTask::None;
        }
        if let keyboard::Event::ModifiersChanged(modifiers) = &event {
            self.modifiers = *modifiers;
            let held = self.link_modifier_held();
            let tab_id = self.workspace.active().1;
            if let Some(tab) = self.tabs.get_mut(&tab_id) {
                if let Err(error) = tab
                    .set_link_modifier_held(held)
                    .and_then(|()| tab.refresh_snapshot())
                {
                    tracing::warn!(?error, tab_id, "terminal link hover refresh failed");
                }
            }
        }
        if matches!(self.keyboard_route(), KeyboardRoute::Editor) {
            if let keyboard::Event::KeyPressed {
                key: Key::Named(Named::Escape),
                repeat: false,
                ..
            } = &event
            {
                self.rename_completion_key = Some(RenameCompletionKey::Escape);
                self.cancel_rename_editor();
            }
            // TextInput owns printable input and on_submit owns Enter. The
            // global listener consumes every editor event so no accelerator or
            // terminal encoder can observe the same key.
            return UiTask::None;
        }
        if let keyboard::Event::KeyPressed { key, .. } = &event {
            if matches!(self.keyboard_route(), KeyboardRoute::Palette) {
                match key.as_ref() {
                    Key::Named(Named::Escape) => self.palette_back_or_dismiss(),
                    Key::Named(Named::ArrowUp) => self.move_palette_selection(-1),
                    Key::Named(Named::ArrowDown) => self.move_palette_selection(1),
                    Key::Named(Named::Enter) => self.palette_confirm(),
                    _ => {}
                }
                // The text input widget consumes printable events. Never let
                // a palette keystroke leak through to the active PTY.
                return self.take_palette_focus_task();
            }
        } else if matches!(self.keyboard_route(), KeyboardRoute::Palette) {
            return UiTask::None;
        }

        if let Some(action) = input::accelerator(&event)
            .and_then(|accelerator| self.keybindings.get(&accelerator).copied())
        {
            let repeat = matches!(&event, keyboard::Event::KeyPressed { repeat: true, .. });
            return self.dispatch_keybind_action(action, repeat);
        }

        let KeyboardRoute::Terminal(active_tab) = self.keyboard_route() else {
            return UiTask::None;
        };
        let Some(tab) = self.tabs.get_mut(&active_tab) else {
            return UiTask::None;
        };
        if input::should_snap_for_terminal_input(&event) {
            if let Err(error) = tab.snap_to_bottom_for_input() {
                tracing::warn!(?error, active_tab, "terminal snap-to-bottom failed");
            }
        }
        let bytes = input::encode_press(&mut tab.encoder, &tab.terminal, event);
        tab.session.send_input(bytes);
        UiTask::None
    }

    /// Handle the one captured text-input key that belongs to application
    /// surface state. Never route a captured Escape into the terminal if the
    /// editor or palette disappeared before this queued message is applied.
    pub fn captured_escape(&mut self) -> UiTask {
        match self.keyboard_route() {
            KeyboardRoute::Editor => {
                self.rename_completion_key = Some(RenameCompletionKey::Escape);
                self.cancel_rename_editor();
                UiTask::None
            }
            KeyboardRoute::Palette => {
                self.palette_back_or_dismiss();
                self.take_palette_focus_task()
            }
            KeyboardRoute::None | KeyboardRoute::Terminal(_) => UiTask::None,
        }
    }

    pub fn captured_enter_release(&mut self) {
        if self.rename_completion_key == Some(RenameCompletionKey::Enter) {
            self.rename_completion_key = None;
        }
    }

    fn dispatch_keybind_action(&mut self, action: KeybindAction, repeat: bool) -> UiTask {
        // Once an accelerator matches, it belongs to Roost and must never be
        // encoded into the PTY. Suppress repeats for all application actions:
        // this prevents held destructive shortcuts from cascading while still
        // consuming every repeated event.
        let Some(result) = dispatch_keybind_once_unless_repeat(repeat, || {
            self.status.clear();
            self.dispatch_keybind_action_once(action)
        }) else {
            return UiTask::None;
        };
        match result {
            Ok(task) => task,
            Err(error) => {
                self.set_status(error);
                UiTask::None
            }
        }
    }

    fn dispatch_keybind_action_once(&mut self, action: KeybindAction) -> Result<UiTask, String> {
        match action {
            KeybindAction::NewTab => {
                self.new_tab_result()?;
                Ok(UiTask::None)
            }
            KeybindAction::CloseTab => {
                let tab_id = self.workspace.active().1;
                if tab_id != 0 {
                    self.close_tab_result(tab_id)?;
                }
                Ok(UiTask::None)
            }
            KeybindAction::NewProject => Err("New Project is not available in Iced yet".into()),
            KeybindAction::RenameProject => {
                self.begin_rename_target(RenameTarget::Project(self.workspace.active().0))?;
                Ok(UiTask::None)
            }
            KeybindAction::RenameTab => {
                self.begin_rename_target(RenameTarget::Tab(self.workspace.active().1))?;
                Ok(UiTask::None)
            }
            KeybindAction::CloseProject => Err("Close Project is not available in Iced yet".into()),
            KeybindAction::JumpToUnread => {
                let active_project_id = self.workspace.active().0;
                if let Some(tab_id) =
                    notification_inbox::next_unread(&self.notification_inbox, active_project_id)
                {
                    self.focus_tab_and_clear(tab_id, true)?;
                }
                Ok(UiTask::None)
            }
            KeybindAction::CycleTabPrev => {
                self.cycle_tab(-1)?;
                Ok(UiTask::None)
            }
            KeybindAction::CycleTabNext => {
                self.cycle_tab(1)?;
                Ok(UiTask::None)
            }
            KeybindAction::Copy => Ok(self.copy_active_selection()),
            KeybindAction::Paste => Ok(self.paste_into_active(ClipboardOp::System)),
            KeybindAction::ToggleSidebar => {
                self.toggle_sidebar();
                Ok(UiTask::None)
            }
            KeybindAction::ToggleSidebarAgents => {
                self.toggle_sidebar_agents();
                Ok(UiTask::None)
            }
            KeybindAction::FontIncrease => {
                self.apply_font_size_transition(FontSizeTransition::Adjust(1.0))?;
                Ok(UiTask::None)
            }
            KeybindAction::FontDecrease => {
                self.apply_font_size_transition(FontSizeTransition::Adjust(-1.0))?;
                Ok(UiTask::None)
            }
            KeybindAction::FontReset => {
                self.apply_font_size_transition(FontSizeTransition::Reset)?;
                Ok(UiTask::None)
            }
            KeybindAction::CommandPalette => self.open_bound_palette_result("commands"),
            KeybindAction::CommandLauncher => self.open_bound_palette_result("launcher"),
            KeybindAction::CustomPalette => self.open_bound_palette_result("custom"),
            KeybindAction::AgentPalette => self.open_bound_palette_result("agents"),
            KeybindAction::Unbind => Ok(UiTask::None),
            KeybindAction::SwitchProject(index) => {
                self.switch_project_by_index(index)?;
                Ok(UiTask::None)
            }
            KeybindAction::SwitchTab(index) => {
                self.switch_tab_by_index(index)?;
                Ok(UiTask::None)
            }
        }
    }

    fn open_bound_palette_result(&mut self, kind: &str) -> Result<UiTask, String> {
        self.open_palette(kind)?;
        Ok(self.take_palette_focus_task())
    }

    fn apply_font_size_transition(&mut self, transition: FontSizeTransition) -> Result<(), String> {
        let Some((candidate, metrics)) =
            font_size_candidate(&self.typography, self.terminal_metrics.font, transition)?
        else {
            return Ok(());
        };
        self.apply_typography_candidate(candidate, metrics, "font size")?;

        let size_pt = self.typography.current_size_pt();
        let path = config::config_path();
        persist_font_size_with(&mut self.config, path.as_deref(), size_pt, config::set_key)
            .map_err(|error| format!("persist font size: {error}"))
    }

    fn apply_typography_candidate(
        &mut self,
        candidate: TerminalTypography,
        metrics: TerminalMetrics,
        operation: &str,
    ) -> Result<(), String> {
        let metric_generation = self.metric_generation.wrapping_add(1).max(1);
        let (cols, rows) = terminal_grid(
            self.window_size,
            self.workspace.sidebar_collapsed(),
            metrics,
        );
        let mut tab_ids = self.tabs.keys().copied().collect::<Vec<_>>();
        tab_ids.sort_unstable();
        let applied = apply_geometry_batch(
            &tab_ids,
            cols,
            rows,
            metrics,
            metric_generation,
            |batch_operation| match batch_operation {
                GeometryBatchOperation::Apply {
                    tab_id,
                    cols,
                    rows,
                    metrics,
                    metric_generation,
                } => self
                    .tabs
                    .get_mut(&tab_id)
                    .ok_or_else(|| {
                        format!("tab {tab_id} disappeared during {operation} application")
                    })?
                    .apply_geometry(cols, rows, metrics, metric_generation)
                    .map_err(|error| error.to_string()),
                GeometryBatchOperation::Rollback { tab_id, previous } => self
                    .tabs
                    .get_mut(&tab_id)
                    .ok_or_else(|| format!("tab {tab_id} disappeared during {operation} rollback"))?
                    .rollback_geometry(previous)
                    .map(|()| None)
                    .map_err(|error| error.to_string()),
            },
        )
        .map_err(|failure| {
            let mut message = format!(
                "apply {operation} to tab {}: {}",
                failure.tab_id, failure.apply
            );
            for (tab_id, error) in failure.rollback {
                message.push_str(&format!("; rollback tab {tab_id}: {error}"));
            }
            message
        })?;

        let mut pointer_releases = Vec::new();
        for (tab_id, change) in &applied {
            if !change.metrics_changed {
                continue;
            }
            let release = match self.tabs.get_mut(tab_id) {
                Some(tab) => tab.prepare_pointer_cancel(),
                None => Err(anyhow::anyhow!(
                    "tab {tab_id} disappeared while staging pointer release"
                )),
            };
            match release {
                Ok(release) => pointer_releases.push((*tab_id, release)),
                Err(error) => {
                    let mut message = format!(
                        "stage pointer release for tab {tab_id} before {operation} commit: {error}"
                    );
                    for (rollback_id, applied_change) in applied.iter().rev() {
                        let Some(previous) = applied_change.previous else {
                            continue;
                        };
                        if let Err(rollback_error) = self
                            .tabs
                            .get_mut(rollback_id)
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "tab {rollback_id} disappeared during {operation} rollback"
                                )
                            })
                            .and_then(|tab| tab.rollback_geometry(previous))
                        {
                            message.push_str(&format!(
                                "; rollback tab {rollback_id}: {rollback_error}"
                            ));
                        }
                    }
                    return Err(message);
                }
            }
        }

        self.typography = candidate;
        self.terminal_metrics = metrics;
        self.metric_generation = metric_generation;
        for (tab_id, release) in pointer_releases {
            if let Some(tab) = self.tabs.get_mut(&tab_id) {
                tab.commit_pointer_cancel(release);
            }
        }
        for (tab_id, change) in applied {
            if let Some(tab) = self.tabs.get(&tab_id) {
                tab.commit_geometry(change);
            }
        }
        Ok(())
    }

    fn apply_font_family(&mut self, family: Option<String>) -> Result<(), String> {
        let mut candidate = self.typography.clone();
        let changed = candidate.set_family(family);
        let resolved = self.font_registry.resolve(candidate.effective_family());
        let metrics =
            TerminalMetrics::measure_with_font(candidate.current_size_pt(), resolved.font)?;
        if !changed && metrics == self.terminal_metrics {
            return Ok(());
        }
        self.apply_typography_candidate(candidate, metrics, "font family")
    }

    fn preview_font_family(&mut self, name: &str) -> Result<(), String> {
        self.apply_font_family(Some(name.to_string()))
    }

    fn commit_font_family(&mut self, name: &str) -> Result<Option<String>, String> {
        let opened = self
            .palette_family_at_open
            .clone()
            .ok_or_else(|| "font palette has no at-open family snapshot".to_string())?;
        let resolved_opened = self
            .palette_resolved_family_at_open
            .clone()
            .ok_or_else(|| "font palette has no resolved at-open family".to_string())?;
        let live = self.typography.family().map(str::to_owned);
        let confirmation =
            typography::confirm_family(opened.as_deref(), &resolved_opened, live.as_deref(), name);
        match confirmation.apply {
            FamilyApply::Keep => {}
            FamilyApply::Set(family) => self.apply_font_family(family)?,
        }
        let Some(persist) = confirmation.persist else {
            return Ok(None);
        };
        let path = config::config_path();
        Ok(
            persist_font_family_with(&mut self.config, path.as_deref(), &persist, config::set_key)
                .err()
                .map(|error| format!("persist font family: {error}")),
        )
    }

    fn copy_active_selection(&mut self) -> UiTask {
        let tab_id = self.workspace.active().1;
        let text = match self.tabs.get_mut(&tab_id) {
            Some(tab) => match tab.selected_text() {
                Ok(text) => text,
                Err(error) => {
                    self.set_status(format!("copy selection from tab {tab_id}: {error}"));
                    return UiTask::None;
                }
            },
            None => return UiTask::None,
        };
        enqueue_selection_copy(&mut self.clipboard, CopyKind::Explicit, text);
        self.clipboard.start_next()
    }

    fn paste_into_active(&mut self, target: ClipboardOp) -> UiTask {
        let tab_id = self.workspace.active().1;
        if !self.tabs.contains_key(&tab_id) {
            return UiTask::None;
        }
        self.clipboard.enqueue_paste_read(target, tab_id);
        self.clipboard.start_next()
    }

    fn keyboard_route(&self) -> KeyboardRoute {
        let active_tab = self.workspace.active().1;
        resolve_keyboard_route(
            self.rename_editor.is_some(),
            self.palette.is_some(),
            active_tab,
            self.tabs.contains_key(&active_tab),
        )
    }

    pub fn pointer(&mut self, event: TerminalPointerEvent) -> UiTask {
        let TerminalPointerEvent {
            tab_id,
            action,
            button,
            col,
            row,
            click_count,
            inside,
        } = event;
        if action == PointerAction::Press {
            self.cancel_editor_for_interaction();
        }
        let link_modifier_held = self.link_modifier_held();
        let Some(tab) = pointer_origin_tab(&mut self.tabs, tab_id) else {
            tracing::debug!(tab_id, "ignored terminal pointer event for a closed tab");
            return UiTask::None;
        };
        let outcome = match tab.handle_native_pointer(NativePointerDispatch {
            action,
            button,
            col,
            row,
            mods: input::ghostty_modifiers(self.modifiers),
            click_count,
            inside,
            link_modifier_held,
        }) {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::warn!(?error, tab_id, "terminal pointer dispatch failed");
                return UiTask::None;
            }
        };
        let selected_text = if outcome.selection_completed {
            match tab.selected_text() {
                Ok(text) => text,
                Err(error) => {
                    tracing::warn!(?error, tab_id, "terminal selection extraction failed");
                    None
                }
            }
        } else {
            None
        };
        if let Err(error) = tab.refresh_snapshot() {
            tracing::warn!(?error, tab_id, "terminal selection refresh failed");
        }
        enqueue_selection_copy(
            &mut self.clipboard,
            CopyKind::OnSelect(self.config.copy_on_select),
            selected_text,
        );
        #[cfg(target_os = "linux")]
        if outcome.paste_selection {
            self.clipboard
                .enqueue_paste_read(ClipboardOp::Selection, tab_id);
        }
        let clipboard = self.clipboard.start_next();
        match outcome.open_url {
            Some(url) => UiTask::OpenUrl { url }.then(clipboard),
            None => clipboard,
        }
    }

    pub fn wheel(&mut self, event: TerminalWheelEvent) -> UiTask {
        let TerminalWheelEvent {
            tab_id,
            history_rows,
            col,
            row,
        } = event;
        let Some(tab) = pointer_origin_tab(&mut self.tabs, tab_id) else {
            tracing::debug!(tab_id, "ignored terminal wheel event for a closed tab");
            return UiTask::None;
        };
        if let Err(error) = tab
            .handle_wheel(
                history_rows,
                col,
                row,
                input::ghostty_modifiers(self.modifiers),
            )
            .and_then(|()| tab.refresh_snapshot())
        {
            tracing::warn!(?error, tab_id, "terminal wheel dispatch failed");
        }
        UiTask::None
    }

    pub fn pointer_leave(&mut self, tab_id: i64) {
        if let Some(tab) = self.tabs.get_mut(&tab_id) {
            tab.pointer_leave();
            if let Err(error) = tab.refresh_snapshot() {
                tracing::warn!(?error, tab_id, "terminal hover refresh failed after leave");
            }
        }
    }

    fn link_modifier_held(&self) -> bool {
        let effective = keybind::resolve_link_modifier(self.config.link_modifier);
        input::accelerator_modifiers(self.modifiers).intersects(effective)
    }

    pub fn url_open_completed(&mut self, result: std::result::Result<(), String>) {
        match result {
            Ok(()) => {}
            Err(error) => {
                tracing::warn!(%error, "URL launcher failed");
                self.set_status(error);
            }
        }
    }

    pub fn set_window_focus(&mut self, focused: bool) {
        if !focused {
            self.rename_completion_key = None;
            self.cancel_tab_drag();
        }
        self.workspace.set_window_focused(focused);
        if let Some(tab) = self.tabs.get(&self.workspace.active().1) {
            tab.set_window_focus(focused);
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let (active_project, active_tab) = self.workspace.active();
        let mut sidebar_body = column![].spacing(2).padding([4, 0]);
        for project in &self.projects {
            let rollup = project_rollup(
                project
                    .tabs
                    .iter()
                    .map(|tab| agent::effective_lifecycle(&tab.agent_state())),
            );
            let stripe = container(
                iced::widget::Space::new()
                    .width(chrome::PROJECT_STRIPE_WIDTH)
                    .height(chrome::ROW_HEIGHT),
            )
            .style(move |_| {
                let color = if rollup == roost_ipc::agent::AgentLifecycle::Inactive {
                    Color::TRANSPARENT
                } else {
                    agent_color(rollup)
                };
                iced::widget::container::Style::default()
                    .background(color)
                    .border(iced::border::rounded(2))
            });
            let project_label: Element<'_, Message> = match self.rename_editor.as_ref() {
                Some(editor) if editor.target == RenameTarget::Project(project.id) => {
                    text_input("Project name", &editor.draft)
                        .id(self.rename_input_id.clone())
                        .on_input(Message::RenameDraftChanged)
                        .on_submit(Message::RenameSubmit)
                        .size(13)
                        .padding([1, 3])
                        .style(chrome::inline_rename_input)
                        .into()
                }
                _ => text(&project.name).size(13).into(),
            };
            let project_pill = container(project_label)
                .width(Fill)
                .height(chrome::ROW_HEIGHT)
                .padding([3.0, chrome::PROJECT_LABEL_INSET])
                .style(chrome::project_pill(project.id == active_project));
            let project_row = mouse_area(
                container(
                    row![
                        stripe,
                        iced::widget::Space::new().width(chrome::PROJECT_STRIPE_GAP),
                        project_pill,
                        iced::widget::Space::new().width(chrome::PROJECT_RIGHT_INSET)
                    ]
                    .align_y(Alignment::Center),
                )
                .width(Fill)
                .height(chrome::ROW_HEIGHT),
            )
            .on_press(Message::ProjectSelected(project.id))
            .on_double_click(Message::BeginRenameProject(project.id));
            let mut project_group = column![project_row].spacing(2);
            if self.config.show_sidebar_agents {
                for agent in self.sidebar_agents.get(&project.id).into_iter().flatten() {
                    let name = agent.name.clone();
                    let detail = format!("{} · {}", agent.status_text, agent.time_text);
                    let dot_color = agent_color(agent.lifecycle);
                    let dot =
                        container(iced::widget::Space::new().width(8).height(8)).style(move |_| {
                            iced::widget::container::Style::default()
                                .background(dot_color)
                                .border(iced::border::rounded(3))
                        });
                    let agent_row = row![
                        dot,
                        text(name).size(11),
                        text(detail).size(9).color(chrome::MUTED_TEXT)
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center);
                    project_group = project_group.push(
                        button(agent_row)
                            .width(Fill)
                            .height(chrome::ROW_HEIGHT)
                            .padding(iced::Padding {
                                top: 3.0,
                                right: 8.0,
                                bottom: 3.0,
                                left: chrome::AGENT_DOT_INSET,
                            })
                            .style(chrome::agent_button(agent.is_active))
                            .on_press(Message::AgentSelected(agent.tab_id)),
                    );
                }
            }
            sidebar_body = sidebar_body.push(project_group);
        }
        let sidebar_header = container(text("PROJECTS").size(11).color(chrome::MUTED_TEXT))
            .height(chrome::BAND_HEIGHT)
            .width(Fill)
            .padding([10, 12])
            .style(chrome::surface);
        let sidebar_footer = container(
            button(text("Hide Sidebar").size(11))
                .width(Fill)
                .height(chrome::PILL_HEIGHT)
                .padding([2, 8])
                .style(chrome::transparent_button)
                .on_press(Message::ToggleSidebar),
        )
        .height(chrome::BAND_HEIGHT)
        .width(Fill)
        .padding([5, 8])
        .style(chrome::surface);
        let sidebar = container(column![
            sidebar_header,
            scrollable(sidebar_body).height(Fill),
            sidebar_footer
        ])
        .width(SIDEBAR_WIDTH)
        .height(Fill)
        .style(chrome::surface);

        let active_project_model = self
            .projects
            .iter()
            .find(|project| project.id == active_project);
        let authoritative_tab_ids = active_project_model
            .map(|project| project.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>())
            .unwrap_or_default();
        let visual_tab_ids = self
            .tab_drag_preview
            .as_ref()
            .filter(|preview| {
                preview.context.project_id == active_project
                    && preview.original_ids == authoritative_tab_ids
                    && same_stable_ids(&preview.ordered_ids, &authoritative_tab_ids)
            })
            .map(|preview| preview.ordered_ids.clone())
            .unwrap_or_else(|| authoritative_tab_ids.clone());
        let active_project_tabs = visual_tab_ids
            .iter()
            .filter_map(|tab_id| {
                active_project_model
                    .and_then(|project| project.tabs.iter().find(|tab| tab.id == *tab_id))
            })
            .collect::<Vec<_>>();
        let collapsed = self.workspace.sidebar_collapsed();
        let mut tab_pills = row![].spacing(6);
        for tab in active_project_tabs {
            let title = if tab.title.is_empty() {
                "shell"
            } else {
                &tab.title
            };
            let active = tab.id == active_tab;
            let lifecycle = agent::effective_lifecycle(&tab.agent_state());
            let status_color = tab_status_color(lifecycle);
            let dot = container(
                iced::widget::Space::new()
                    .width(chrome::TAB_STATUS_SIZE)
                    .height(chrome::TAB_STATUS_SIZE),
            )
            .style(move |_| {
                iced::widget::container::Style::default()
                    .background(status_color)
                    .border(iced::border::rounded(4))
            });
            let select: Element<'_, Message> = if let Some(editor) = self
                .rename_editor
                .as_ref()
                .filter(|editor| editor.target == RenameTarget::Tab(tab.id))
            {
                container(
                    row![
                        dot,
                        text_input("Tab name", &editor.draft)
                            .id(self.rename_input_id.clone())
                            .on_input(Message::RenameDraftChanged)
                            .on_submit(Message::RenameSubmit)
                            .width(140)
                            .size(12)
                            .padding([1, 3])
                            .style(chrome::inline_rename_input)
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                )
                .height(chrome::PILL_HEIGHT)
                .padding([2, 7])
                .into()
            } else {
                container(
                    row![
                        dot,
                        text(title).size(12).color(if active {
                            chrome::TEXT
                        } else {
                            chrome::MUTED_TEXT
                        })
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                )
                .height(chrome::PILL_HEIGHT)
                .padding([2, 7])
                .into()
            };
            let mut pill = row![select].align_y(Alignment::Center);
            if tab.has_notification && !active {
                pill = pill.push(
                    container(
                        iced::widget::Space::new()
                            .width(chrome::NOTIFICATION_DOT_SIZE)
                            .height(chrome::NOTIFICATION_DOT_SIZE),
                    )
                    .style(chrome::badge),
                );
            }
            if active {
                pill = pill.push(
                    button(text("×").size(13))
                        .width(chrome::PILL_HEIGHT)
                        .height(chrome::PILL_HEIGHT)
                        .padding(2)
                        .style(chrome::transparent_button)
                        .on_press(Message::CloseTab(tab.id)),
                );
            }
            tab_pills = tab_pills.push(
                container(pill)
                    .height(chrome::PILL_HEIGHT)
                    .padding([0, 2])
                    .style(chrome::tab_pill(
                        active,
                        self.tab_drag_preview
                            .as_ref()
                            .is_some_and(|preview| preview.context.source_id == tab.id),
                    )),
            );
        }
        let tab_strip = TabStrip::new(
            tab_pills,
            active_project,
            visual_tab_ids,
            self.tab_strip_generation,
            self.rename_editor.is_none(),
        );
        let tab_scroller = scrollable(tab_strip)
            .horizontal()
            .width(Fill)
            .height(chrome::PILL_HEIGHT);
        let mut tabs = row![].spacing(5).align_y(Alignment::Center);
        if collapsed {
            tabs = tabs.push(
                button(text("☰").size(13))
                    .width(chrome::PILL_HEIGHT)
                    .height(chrome::PILL_HEIGHT)
                    .padding(2)
                    .style(chrome::transparent_button)
                    .on_press(Message::ToggleSidebar),
            );
        }
        tabs = tabs.push(tab_scroller).push(
            button(text("+").size(15))
                .width(chrome::PILL_HEIGHT)
                .height(chrome::PILL_HEIGHT)
                .padding(1)
                .style(chrome::transparent_button)
                .on_press(Message::NewTab),
        );
        let notification_count = self.notification_inbox.count();
        let notification_label = if notification_count == 0 {
            "○".to_string()
        } else {
            format!("•{}", notification_count.min(99))
        };
        tabs = tabs.push(
            button(
                text(notification_label)
                    .size(11)
                    .color(if notification_count == 0 {
                        chrome::MUTED_TEXT
                    } else {
                        chrome::NOTIFICATION
                    }),
            )
            .width(chrome::PILL_HEIGHT)
            .height(chrome::PILL_HEIGHT)
            .padding(2)
            .style(chrome::transparent_button)
            .on_press(Message::OpenNotifications),
        );
        let tab_bar = container(tabs)
            .height(chrome::BAND_HEIGHT)
            .width(Fill)
            .padding([5, 8])
            .style(chrome::dark_surface);

        let terminal: Element<'_, Message> = match self.tabs.get(&active_tab) {
            Some(tab) if tab.applied_metrics.is_some() => TerminalWidget {
                tab_id: active_tab,
                snapshot: tab.snapshot.clone(),
                metrics: tab.applied_metrics.unwrap_or(self.terminal_metrics),
                metric_generation: tab.metric_generation,
            }
            .into(),
            _ => container(text("Starting terminal…"))
                .center(Fill)
                .width(Fill)
                .height(Fill)
                .into(),
        };
        let main = column![tab_bar, terminal].width(Fill).height(Fill);
        let content: Element<'_, Message> = if collapsed {
            main.width(Fill).height(Fill).into()
        } else {
            row![sidebar, main].width(Fill).height(Fill).into()
        };
        let content: Element<'_, Message> = if let Some(status) = self.status.message() {
            let toast = container(text(status).size(12).color(chrome::ERROR_TEXT))
                .max_width(520)
                .padding([8, 12])
                .style(chrome::status_toast);
            let overlay = container(toast)
                .width(Fill)
                .height(Fill)
                .padding(16)
                .align_x(Alignment::End)
                .align_y(Alignment::End);
            stack![content, overlay].width(Fill).height(Fill).into()
        } else {
            content
        };
        let Some(palette) = &self.palette else {
            return if self.rename_editor.is_some() {
                mouse_area(content)
                    .on_press(Message::RenamePointerDismiss)
                    .into()
            } else {
                content
            };
        };

        let frame = palette.current();
        let input = text_input(&frame.placeholder, &frame.query)
            .id(self.palette_input_id.clone())
            .on_input(Message::PaletteQueryChanged)
            .on_submit(Message::PaletteConfirm)
            .padding([4, 8])
            .size(17)
            .style(chrome::palette_input);
        let mut items = column![].spacing(0);
        for (index, matched) in palette.matches().into_iter().enumerate() {
            let selected = index == frame.selection;
            let item = matched.item;
            let actionable = item.actionable;
            let primary_color = if actionable {
                chrome::TEXT
            } else {
                chrome::PALETTE_PLACEHOLDER.scale_alpha(0.6)
            };
            let label: Element<'_, Message> = if let Some(agent) = item.agent {
                let lifecycle_color = if actionable {
                    agent_color(agent.effective_lifecycle)
                } else {
                    chrome::PALETTE_PLACEHOLDER.scale_alpha(0.6)
                };
                let dot =
                    container(iced::widget::Space::new().width(8).height(8)).style(move |_| {
                        iced::widget::container::Style::default()
                            .background(lifecycle_color)
                            .border(iced::border::rounded(4))
                    });
                let metrics = agent.metrics_text.unwrap_or_default();
                let project =
                    ellipsize_palette_text(&agent.project, PALETTE_AGENT_PROJECT_MAX_COLUMNS);
                let (name, status) = palette_agent_left_text(&agent.name, &agent.status_text);
                row![
                    dot,
                    container(
                        text(project)
                            .size(14)
                            .font(Font {
                                weight: font::Weight::Bold,
                                ..Font::default()
                            })
                            .color(primary_color)
                            .wrapping(iced::widget::text::Wrapping::None)
                    )
                    .max_width(140),
                    container(
                        text(name)
                            .size(14)
                            .color(primary_color)
                            .wrapping(iced::widget::text::Wrapping::None)
                    ),
                    text(status)
                        .size(13)
                        .color(lifecycle_color)
                        .wrapping(iced::widget::text::Wrapping::None),
                    iced::widget::Space::new().width(Fill),
                    text(metrics)
                        .size(12)
                        .font(Font::MONOSPACE)
                        .color(chrome::MUTED_TEXT),
                    text(agent.time_text)
                        .size(12)
                        .font(Font::MONOSPACE)
                        .color(chrome::MUTED_TEXT)
                ]
                .spacing(8)
                .align_y(Alignment::Center)
                .into()
            } else {
                let spans = palette_title_runs(&item.title, &matched.ranges)
                    .into_iter()
                    .map(|run| {
                        let mut span = iced::widget::span::<(), Font>(run.text).color(
                            if run.matched && actionable {
                                chrome::PALETTE_MATCH
                            } else {
                                primary_color
                            },
                        );
                        if run.matched && actionable {
                            span = span.font(Font {
                                weight: font::Weight::Semibold,
                                ..Font::default()
                            });
                        }
                        span
                    })
                    .collect::<Vec<_>>();
                let title = iced::widget::rich_text(spans)
                    .size(14)
                    .width(Fill)
                    .wrapping(iced::widget::text::Wrapping::None);
                let mut leading = column![title];
                if let Some(subtitle) = item.subtitle {
                    leading = leading.push(
                        text(subtitle)
                            .size(12)
                            .color(chrome::PALETTE_PLACEHOLDER)
                            .wrapping(iced::widget::text::Wrapping::None),
                    );
                }
                let mut generic = row![leading.width(Fill)].align_y(Alignment::Center);
                if let Some(trailing) = item.trailing_text {
                    generic = generic.push(
                        text(trailing)
                            .size(12)
                            .color(chrome::PALETTE_PLACEHOLDER)
                            .wrapping(iced::widget::text::Wrapping::None),
                    );
                }
                generic.into()
            };
            let row = button(label)
                .width(Fill)
                .padding([6, 10])
                .style(chrome::palette_row(selected, actionable));
            let row = if actionable {
                row.on_press(Message::PaletteActivate(item.id))
            } else {
                row
            };
            items = items.push(container(row).id(palette_row_id(
                self.palette_session,
                self.palette_layout_revision,
                index,
            )));
        }
        let list = scrollable(items)
            .id(self.palette_scroll_id.clone())
            .on_scroll(|_| Message::PaletteScrolled)
            .direction(scrollable::Direction::Vertical(
                scrollable::Scrollbar::new()
                    .width(2)
                    .scroller_width(4)
                    .margin(2),
            ))
            .style(chrome::palette_scrollable)
            .height(Shrink);
        let divider = container(
            container(iced::widget::Space::new().height(1))
                .width(Fill)
                .style(chrome::palette_divider),
        )
        .padding([8, 2]);
        let panel = container(column![input, divider, list].spacing(0))
            .width(Fill)
            .max_width(chrome::PALETTE_WIDTH)
            .height(Shrink)
            .max_height(chrome::PALETTE_MAX_HEIGHT)
            .padding(10)
            .style(chrome::palette_panel);
        let overlay = container(mouse_area(panel).on_press(Message::PaletteCardPressed))
            .width(Fill)
            .height(Fill)
            .padding([60, 16])
            .center_x(Fill);
        let catcher = mouse_area(iced::widget::Space::new().width(Fill).height(Fill))
            .on_press(Message::PaletteDismiss);
        stack![content, catcher, overlay]
            .width(Fill)
            .height(Fill)
            .into()
    }

    fn begin_rename_target(&mut self, target: RenameTarget) -> Result<(), String> {
        self.cancel_tab_drag();
        if self
            .rename_editor
            .as_ref()
            .is_some_and(|editor| editor.target == target)
        {
            self.rename_focus_requested = true;
            return Ok(());
        }
        if matches!(target, RenameTarget::Project(_)) && self.workspace.sidebar_collapsed() {
            self.set_sidebar_collapsed(false);
        }
        let editor = begin_rename_editor(&self.projects, target)?;
        if !rename_editor_is_renderable(
            &editor,
            &self.projects,
            self.workspace.active().0,
            self.workspace.sidebar_collapsed(),
        ) {
            return Err(format!("rename target {target:?} is not visible"));
        }
        self.rename_editor = Some(editor);
        self.rename_focus_requested = true;
        Ok(())
    }

    pub fn begin_rename_project(&mut self, project_id: i64) {
        if let Err(error) = self.begin_rename_target(RenameTarget::Project(project_id)) {
            self.set_status(error);
        }
    }

    pub fn begin_rename_tab(&mut self, tab_id: i64) {
        if let Err(error) = self.begin_rename_target(RenameTarget::Tab(tab_id)) {
            self.set_status(error);
        }
    }

    pub fn rename_draft_changed(&mut self, draft: String) {
        if let Some(editor) = &mut self.rename_editor {
            editor.draft = draft;
        }
    }

    pub fn submit_rename_editor(&mut self) {
        let client = self.client.clone();
        let runtime = &self.runtime;
        match submit_rename_editor_once_with(
            &mut self.rename_editor,
            &mut self.rename_completion_key,
            |target, label| match target {
                RenameTarget::Project(project_id) => runtime
                    .block_on(client.rename_project(project_id, label))
                    .map_err(|error| error.to_string()),
                RenameTarget::Tab(tab_id) => runtime
                    .block_on(client.set_tab_title(tab_id, label))
                    .map_err(|error| error.to_string()),
            },
        ) {
            Ok(_) => {
                self.rename_focus_requested = false;
                self.reconcile();
            }
            Err(error) => self.set_status(error),
        }
    }

    fn cancel_rename_editor(&mut self) {
        self.rename_editor = None;
        self.rename_focus_requested = false;
    }

    pub fn rename_pointer_dismiss(&mut self) {
        self.cancel_rename_editor();
    }

    fn cancel_editor_for_interaction(&mut self) {
        self.cancel_rename_editor();
    }

    fn reconcile_rename_editor(&mut self) {
        let visible = self.rename_editor.as_ref().is_none_or(|editor| {
            rename_editor_is_renderable(
                editor,
                &self.projects,
                self.workspace.active().0,
                self.workspace.sidebar_collapsed(),
            )
        });
        if !visible {
            self.cancel_rename_editor();
        }
    }

    fn active_project_tab_ids(&self, project_id: i64) -> Vec<i64> {
        self.projects
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.tabs.iter().map(|tab| tab.id).collect())
            .unwrap_or_default()
    }

    fn cancel_tab_drag(&mut self) {
        self.tab_drag_preview = None;
        self.tab_strip_generation = self.tab_strip_generation.wrapping_add(1);
    }

    fn reconcile_tab_drag_preview(&mut self) {
        let Some(preview) = self.tab_drag_preview.as_ref() else {
            return;
        };
        let active_project = self.workspace.active().0;
        let authoritative = self.active_project_tab_ids(preview.context.project_id);
        if active_project != preview.context.project_id || authoritative != preview.original_ids {
            self.cancel_tab_drag();
        }
    }

    fn begin_tab_drag_preview(
        &mut self,
        project_id: i64,
        source_id: i64,
        context_generation: u64,
        original_ids: Vec<i64>,
    ) {
        let authoritative = self.active_project_tab_ids(project_id);
        if context_generation != self.tab_strip_generation
            || self.workspace.active().0 != project_id
            || authoritative != original_ids
            || !same_stable_ids(&original_ids, &original_ids)
            || !original_ids.contains(&source_id)
        {
            self.cancel_tab_drag();
            return;
        }
        self.tab_drag_preview = Some(TabDragPreview {
            context: TabDragContext {
                project_id,
                source_id,
                generation: context_generation,
            },
            ordered_ids: original_ids.clone(),
            original_ids,
        });
    }

    fn preview_tab_drag(
        &mut self,
        project_id: i64,
        source_id: i64,
        context_generation: u64,
        original_ids: &[i64],
        ordered_ids: Vec<i64>,
    ) {
        let context = TabDragContext {
            project_id,
            source_id,
            generation: context_generation,
        };
        let valid = self.tab_drag_preview.as_ref().is_some_and(|preview| {
            preview.context == context
                && context_generation == self.tab_strip_generation
                && preview.original_ids == original_ids
                && self.active_project_tab_ids(project_id) == original_ids
                && same_stable_ids(&ordered_ids, original_ids)
        });
        if valid {
            if let Some(preview) = &mut self.tab_drag_preview {
                preview.ordered_ids = ordered_ids;
            }
        } else {
            self.cancel_tab_drag();
        }
    }

    fn end_tab_drag_preview(
        &mut self,
        project_id: i64,
        source_id: i64,
        context_generation: u64,
        original_ids: &[i64],
    ) {
        let context = TabDragContext {
            project_id,
            source_id,
            generation: context_generation,
        };
        let authoritative = self.active_project_tab_ids(project_id);
        end_tab_drag_preview_if_owned(
            &mut self.tab_drag_preview,
            &authoritative,
            &context,
            original_ids,
        );
    }

    fn commit_tab_drag(
        &mut self,
        project_id: i64,
        source_id: i64,
        context_generation: u64,
        original_ids: &[i64],
        ordered_ids: Vec<i64>,
    ) {
        let authoritative = self.active_project_tab_ids(project_id);
        let request = TabDragCommitRequest {
            context: TabDragContext {
                project_id,
                source_id,
                generation: context_generation,
            },
            original_ids: original_ids.to_vec(),
            ordered_ids,
        };
        let runtime = &self.runtime;
        let client = &self.client;
        let settlement = settle_tab_drag_commit_with(
            &mut self.tab_drag_preview,
            &authoritative,
            request,
            |project_id, ordered_ids| {
                runtime
                    .block_on(client.reorder_tabs(project_id, ordered_ids))
                    .map_err(|error| error.to_string())
            },
        );
        tracing::debug!(
            ?settlement,
            project_id,
            source_id,
            context_generation,
            "Iced tab drag settlement"
        );
        let TabDragSettlement::Settled(result) = settlement else {
            return;
        };
        self.tab_strip_generation = self.tab_strip_generation.wrapping_add(1);
        if let Err(error) = result {
            tracing::warn!(?error, project_id, source_id, "Iced tab reorder failed");
            self.set_status(format!("reorder tabs: {error}"));
        }
        self.reconcile();
    }

    pub(crate) fn tab_pointer_released(&mut self) {
        let Some(preview) = self.tab_drag_preview.as_ref() else {
            tracing::debug!("Iced root release had no tab drag preview");
            return;
        };
        tracing::debug!(
            project_id = preview.context.project_id,
            source_id = preview.context.source_id,
            generation = preview.context.generation,
            ordered_ids = ?preview.ordered_ids,
            "Iced root release settling tab drag preview"
        );
        let request = TabDragCommitRequest::from(preview);
        self.commit_tab_drag(
            request.context.project_id,
            request.context.source_id,
            request.context.generation,
            &request.original_ids,
            request.ordered_ids,
        );
    }

    pub(crate) fn has_tab_drag_preview(&self) -> bool {
        self.tab_drag_preview.is_some()
    }

    pub(crate) fn tab_strip_event(&mut self, event: TabStripEvent) {
        match event {
            TabStripEvent::Started {
                project_id,
                source_id,
                context_generation,
                original_ids,
            } => {
                self.begin_tab_drag_preview(project_id, source_id, context_generation, original_ids)
            }
            TabStripEvent::Preview {
                project_id,
                source_id,
                context_generation,
                original_ids,
                ordered_ids,
            } => self.preview_tab_drag(
                project_id,
                source_id,
                context_generation,
                &original_ids,
                ordered_ids,
            ),
            TabStripEvent::Commit {
                project_id,
                source_id,
                context_generation,
                original_ids,
                ordered_ids,
            } => self.commit_tab_drag(
                project_id,
                source_id,
                context_generation,
                &original_ids,
                ordered_ids,
            ),
            TabStripEvent::Ended {
                project_id,
                source_id,
                context_generation,
                original_ids,
            } => {
                self.end_tab_drag_preview(project_id, source_id, context_generation, &original_ids)
            }
            TabStripEvent::Cancel { context_generation } => {
                if context_generation == self.tab_strip_generation {
                    self.cancel_tab_drag();
                    self.reconcile();
                }
            }
        }
    }

    fn take_rename_focus_task(&mut self) -> UiTask {
        take_rename_focus_request(
            &mut self.rename_focus_requested,
            self.rename_editor.is_some(),
            &self.rename_input_id,
        )
    }

    pub fn select_project(&mut self, project_id: i64) {
        self.cancel_tab_drag();
        self.cancel_editor_for_interaction();
        if let Some(tab_id) = self.workspace.preferred_tab(project_id) {
            let _ = self.focus_tab_and_clear(tab_id, false);
        }
    }

    pub fn select_tab(&mut self, tab_id: i64) {
        self.cancel_editor_for_interaction();
        let _ = self.focus_tab_and_clear(tab_id, false);
    }

    pub fn select_agent(&mut self, tab_id: i64) {
        self.cancel_tab_drag();
        self.cancel_editor_for_interaction();
        let _ = self.focus_tab_and_clear(tab_id, true);
    }

    pub fn open_notifications(&mut self) -> UiTask {
        if let Err(error) = self.open_palette("notifications") {
            self.set_status(error);
        }
        self.take_palette_focus_task()
    }

    pub fn toggle_sidebar(&mut self) {
        self.cancel_tab_drag();
        self.cancel_editor_for_interaction();
        self.set_sidebar_collapsed(!self.workspace.sidebar_collapsed());
    }

    fn set_sidebar_collapsed(&mut self, collapsed: bool) {
        self.workspace.set_sidebar_collapsed(collapsed);
        self.resize(self.window_size);
    }

    fn toggle_sidebar_agents(&mut self) {
        self.config.show_sidebar_agents = !self.config.show_sidebar_agents;
        let value = if self.config.show_sidebar_agents {
            "true"
        } else {
            "false"
        };
        if let Some(path) = config::config_path() {
            if let Err(error) = config::set_key(&path, "show-sidebar-agents", value) {
                self.set_status(format!("persist show-sidebar-agents: {error}"));
            }
        }
    }

    pub fn new_tab(&mut self) {
        self.cancel_tab_drag();
        self.cancel_editor_for_interaction();
        if let Err(error) = self.new_tab_result() {
            self.set_status(error);
        }
    }

    fn new_tab_result(&mut self) -> Result<(), String> {
        let (project_id, _) = self.workspace.active();
        if project_id == 0 {
            return Ok(());
        }
        let cwd = self.launch_cwd(project_id);
        self.runtime
            .block_on(self.client.open_tab(
                project_id,
                &cwd,
                "",
                &[],
                u32::from(DEFAULT_COLS),
                u32::from(DEFAULT_ROWS),
            ))
            .map_err(|error| error.to_string())?;
        self.reconcile();
        Ok(())
    }

    pub fn close_tab(&mut self, tab_id: i64) {
        self.cancel_tab_drag();
        self.cancel_editor_for_interaction();
        if let Err(error) = self.close_tab_result(tab_id) {
            self.set_status(error);
        }
    }

    fn close_tab_result(&mut self, tab_id: i64) -> Result<(), String> {
        match close_tab_by_id(&self.runtime, &self.client, tab_id) {
            Ok(CloseTabOutcome::Closed) => {}
            Ok(CloseTabOutcome::AlreadyGone) => {
                tracing::debug!(tab_id, "close_tab: rendered tab already gone");
            }
            Err(error) => {
                tracing::warn!(?error, tab_id, "close_tab failed");
                return Err(error.to_string());
            }
        }
        self.reconcile();
        Ok(())
    }

    pub fn palette_query_changed(&mut self, query: &str) {
        let result = self.query_palette(query);
        report_palette_query_result(&mut self.status, result.map(drop), Instant::now());
    }

    pub fn palette_activate(&mut self, id: &str) {
        if let Err(error) = self.activate_palette(id) {
            self.set_status(error);
        }
    }

    pub fn palette_confirm(&mut self) {
        if let Err(error) = self.confirm_palette_selection() {
            self.set_status(error);
        }
        arm_rename_completion_for_open_editor(
            &mut self.rename_completion_key,
            self.rename_editor.is_some(),
        );
    }

    pub fn palette_pointer_dismiss(&mut self) -> UiTask {
        self.dismiss_palette_with_focus_recovery();
        self.take_palette_focus_task()
    }

    fn open_palette(&mut self, kind: &str) -> Result<(), String> {
        self.cancel_tab_drag();
        self.cancel_editor_for_interaction();
        let frame = match kind {
            "" | "commands" => command_palette_frame(
                self.notification_inbox.count(),
                &self.config.providers,
                &self.keybindings,
            ),
            "launcher" => launcher_palette_frame(&self.config),
            "agents" => {
                agent_palette::agent_frame(&self.workspace.snapshot(), agent_palette::now_unix())
            }
            "notifications" => notification_inbox::frame(&self.notification_inbox),
            "custom" => provider_palette_frame(&self.config.providers),
            _ => return Err(format!("unknown palette kind {kind:?}")),
        };
        self.try_dismiss_palette()?;
        self.palette_session = self.palette_session.wrapping_add(1).max(1);
        self.palette_theme_at_open = Some(self.active_theme_name.clone());
        self.palette_family_at_open = Some(self.typography.family().map(str::to_owned));
        self.palette_resolved_family_at_open = Some(
            self.font_registry
                .resolve(self.typography.effective_family())
                .name
                .to_string(),
        );
        self.palette = Some(palette::PaletteState::new(frame));
        self.palette_focus_requested = true;
        self.refresh_agent_palette();
        self.invalidate_palette_geometry(PaletteVisibilityRequest::Reveal);
        Ok(())
    }

    fn present_palette(
        &mut self,
        title: String,
        placeholder: String,
        items: Vec<(String, String, Option<String>)>,
        reply: tokio::sync::oneshot::Sender<Result<PalettePresentResult, String>>,
    ) {
        self.cancel_tab_drag();
        self.cancel_editor_for_interaction();
        if let Err(error) = self.try_dismiss_palette() {
            let _ = reply.send(Err(error));
            return;
        }
        self.palette_session = self.palette_session.wrapping_add(1).max(1);
        let placeholder = if !placeholder.is_empty() {
            placeholder
        } else if !title.is_empty() {
            title
        } else {
            "Select…".to_string()
        };
        let items = items
            .into_iter()
            .map(|(id, title, subtitle)| {
                palette::PaletteItem::new(id, title).with_subtitle(subtitle)
            })
            .collect();
        self.palette = Some(palette::PaletteState::new(palette::PaletteFrame::new(
            "present",
            placeholder,
            items,
        )));
        self.palette_present_reply = Some(reply);
        self.palette_focus_requested = true;
        self.invalidate_palette_geometry(PaletteVisibilityRequest::Reveal);
    }

    fn take_palette_focus_task(&mut self) -> UiTask {
        take_palette_focus_request(&mut self.palette_focus_requested, &self.palette_input_id)
    }

    fn invalidate_palette_geometry(&mut self, request: PaletteVisibilityRequest) {
        self.reset_palette_geometry(request, true);
    }

    fn reset_palette_geometry(&mut self, request: PaletteVisibilityRequest, merge: bool) {
        self.palette_layout_revision = self.palette_layout_revision.wrapping_add(1).max(1);
        self.palette_selected_in_view = None;
        self.palette_visibility_retries = 0;
        let has_selection = self.palette.as_ref().is_some_and(|state| {
            let frame = state.current();
            state.matches().get(frame.selection).is_some()
        });
        if has_selection && request == PaletteVisibilityRequest::Reveal {
            self.palette_reveal_required = true;
            self.palette_reveal_attempts = 0;
        } else if !has_selection {
            self.palette_reveal_required = false;
            self.palette_reveal_attempts = 0;
        }
        self.palette_visibility_request = if !has_selection {
            PaletteVisibilityRequest::None
        } else {
            queue_layout_visibility_request(
                self.palette_visibility_request,
                request,
                !merge,
                self.palette_reveal_required,
            )
        };
    }

    fn clear_palette_geometry(&mut self) {
        self.palette_layout_revision = self.palette_layout_revision.wrapping_add(1).max(1);
        self.palette_selected_in_view = None;
        self.palette_visibility_request = PaletteVisibilityRequest::None;
        self.palette_visibility_retries = 0;
        self.palette_reveal_required = false;
        self.palette_reveal_attempts = 0;
    }

    fn take_palette_visibility_task(&mut self) -> UiTask {
        if self.window_id.is_none() {
            return UiTask::None;
        }
        let request = self.palette_visibility_request;
        if request == PaletteVisibilityRequest::None {
            return UiTask::None;
        }
        let Some(state) = &self.palette else {
            self.clear_palette_geometry();
            return UiTask::None;
        };
        let selection = state.current().selection;
        if state.matches().get(selection).is_none() {
            self.palette_visibility_request = PaletteVisibilityRequest::None;
            return UiTask::None;
        }
        self.palette_visibility_request = PaletteVisibilityRequest::None;
        if request == PaletteVisibilityRequest::Reveal
            && self.palette_reveal_required
            && !schedule_reveal_attempt(
                &mut self.palette_reveal_attempts,
                &mut self.palette_selected_in_view,
                &mut self.palette_reveal_required,
            )
        {
            tracing::warn!(
                session = self.palette_session,
                revision = self.palette_layout_revision,
                attempts = self.palette_reveal_attempts,
                "palette reveal exhausted its bounded scheduling budget"
            );
            return UiTask::None;
        }
        tracing::debug!(
            session = self.palette_session,
            revision = self.palette_layout_revision,
            measurement_generation = self.palette_measurement_generation,
            selection,
            reveal = request == PaletteVisibilityRequest::Reveal,
            "schedule palette visibility operation"
        );
        UiTask::PaletteVisibility {
            scroll_id: self.palette_scroll_id.clone(),
            row_id: palette_row_id(
                self.palette_session,
                self.palette_layout_revision,
                selection,
            ),
            session: self.palette_session,
            revision: self.palette_layout_revision,
            measurement_generation: self.palette_measurement_generation,
            reveal: request == PaletteVisibilityRequest::Reveal,
        }
    }

    pub fn palette_scrolled(&mut self) {
        if self.palette.is_some() {
            // Iced emits `on_scroll` for programmatic and layout-driven
            // viewport changes too. Those events must not advance row-ID
            // revisions or they can perpetually stale the reveal result.
            queue_scroll_measurement(
                &mut self.palette_selected_in_view,
                &mut self.palette_visibility_retries,
                &mut self.palette_visibility_request,
                &mut self.palette_measurement_generation,
                self.palette_reveal_required,
            );
        }
    }

    pub fn palette_visibility_measured(
        &mut self,
        session: u64,
        revision: u64,
        measurement_generation: u64,
        reveal: bool,
        visibility: Visibility,
    ) {
        if !visibility_result_is_current(
            self.palette_session,
            self.palette_layout_revision,
            self.palette_measurement_generation,
            session,
            revision,
            measurement_generation,
        ) || self.palette.is_none()
        {
            tracing::debug!(
                session,
                revision,
                measurement_generation,
                ?visibility,
                "discard stale palette visibility"
            );
            return;
        }
        tracing::debug!(
            session,
            revision,
            measurement_generation,
            ?visibility,
            "palette visibility measured"
        );
        match visibility {
            Visibility::Visible(visible) => {
                if apply_visible_result(
                    &mut self.palette_selected_in_view,
                    &mut self.palette_visibility_retries,
                    &mut self.palette_visibility_request,
                    &mut self.palette_reveal_required,
                    reveal,
                    visible,
                ) {
                    tracing::warn!(
                        session,
                        revision,
                        retries = self.palette_visibility_retries,
                        "palette row remained clipped after bounded reveal retries"
                    );
                }
                if reveal && visible {
                    self.palette_reveal_attempts = 0;
                }
            }
            Visibility::Missing => {
                if let Some((retries, retry)) =
                    visibility_retry(self.palette_visibility_retries, reveal)
                {
                    self.palette_visibility_retries = retries;
                    self.palette_visibility_request = self.palette_visibility_request.merge(retry);
                } else {
                    tracing::warn!(
                        session,
                        revision,
                        retries = self.palette_visibility_retries,
                        "palette geometry unavailable after bounded retries"
                    );
                }
            }
        }
    }

    fn palette_render_signature(&self) -> Option<(String, usize, Vec<palette::PaletteMatch>)> {
        let state = self.palette.as_ref()?;
        let frame = state.current();
        Some((frame.id.clone(), frame.selection, state.matches()))
    }

    fn palette_layout_signature(&self) -> Option<(String, usize, Vec<PaletteLayoutRow>)> {
        let state = self.palette.as_ref()?;
        let frame = state.current();
        let rows = state
            .matches()
            .into_iter()
            .map(|matched| PaletteLayoutRow {
                id: matched.item.id,
                title: matched.item.title,
                subtitle: matched.item.subtitle,
                trailing_text: matched.item.trailing_text,
                has_agent_layout: matched.item.agent.is_some(),
            })
            .collect();
        Some((frame.id.clone(), frame.selection, rows))
    }

    fn palette_state_result(&self) -> PaletteStateResult {
        let Some(state) = &self.palette else {
            return PaletteStateResult::default();
        };
        let frame = state.current();
        PaletteStateResult {
            open: true,
            frame: Some(frame.id.clone()),
            query: frame.query.clone(),
            selection: u32::try_from(frame.selection).unwrap_or(u32::MAX),
            items: state
                .matches()
                .into_iter()
                .map(|matched| PaletteItemView {
                    id: matched.item.id,
                    title: matched.item.title,
                    subtitle: matched.item.subtitle,
                    agent: matched.item.agent,
                })
                .collect(),
            // Geometry is populated asynchronously after Iced lays out the
            // current revision; `None` honestly means pending/unavailable.
            selected_in_view: self.palette_selected_in_view,
        }
    }

    fn query_palette(&mut self, query: &str) -> Result<PaletteStateResult, String> {
        let state = self
            .palette
            .as_mut()
            .ok_or_else(|| "no palette open".to_string())?;
        state.set_query(query);
        self.invalidate_palette_geometry(PaletteVisibilityRequest::Reveal);
        self.preview_selected_palette_item()?;
        Ok(self.palette_state_result())
    }

    fn move_palette_selection(&mut self, delta: isize) {
        if let Some(state) = &mut self.palette {
            state.move_selection(delta);
        }
        if let Err(error) = self.preview_selected_palette_item() {
            self.set_status(error);
        }
        self.invalidate_palette_geometry(PaletteVisibilityRequest::Reveal);
    }

    fn confirm_palette_selection(&mut self) -> Result<PaletteStateResult, String> {
        let id = self
            .palette
            .as_ref()
            .and_then(palette::PaletteState::selected_item)
            .ok_or_else(|| "no actionable palette row selected".to_string())?
            .id;
        self.activate_palette(&id)
    }

    fn activate_palette(&mut self, id: &str) -> Result<PaletteStateResult, String> {
        let (frame_id, item) = {
            let state = self
                .palette
                .as_mut()
                .ok_or_else(|| "no palette open".to_string())?;
            let matches = state.matches();
            let index = matches
                .iter()
                .position(|matched| matched.item.id == id)
                .ok_or_else(|| format!("no palette row with id {id:?}"))?;
            state.set_selection(index);
            let item = matches[index].item.clone();
            (state.current().id.clone(), item)
        };
        self.invalidate_palette_geometry(PaletteVisibilityRequest::Reveal);

        if !item.actionable {
            return Ok(self.palette_state_result());
        }

        match frame_id.as_str() {
            "commands" => match item.id.as_str() {
                palette::PaletteCommands::SELECT_THEME_ID => {
                    if let Some(state) = &mut self.palette {
                        state.push(theme_palette_frame(&self.active_theme_name));
                    }
                }
                palette::PaletteCommands::SELECT_FONT_ID => {
                    if let Some(state) = &mut self.palette {
                        let resolved = self
                            .palette_resolved_family_at_open
                            .as_deref()
                            .unwrap_or("Monospace");
                        state.push(font_palette_frame(self.font_registry, resolved));
                    }
                }
                palette::PaletteCommands::VIEW_AGENTS_ID => {
                    if let Some(state) = &mut self.palette {
                        state.push(agent_palette::agent_frame(
                            &self.workspace.snapshot(),
                            agent_palette::now_unix(),
                        ));
                    }
                    self.refresh_agent_palette();
                }
                palette::PaletteCommands::VIEW_NOTIFICATIONS_ID => {
                    if let Some(state) = &mut self.palette {
                        state.push(notification_inbox::frame(&self.notification_inbox));
                    }
                }
                palette::PaletteCommands::CLEAR_NOTIFICATIONS_ID => {
                    let tab_ids = self.notification_inbox.tab_ids();
                    let mut first_error = None;
                    for tab_id in tab_ids {
                        if let Err(error) = self.workspace.set_tab_has_notification(tab_id, false) {
                            first_error.get_or_insert_with(|| error.to_string());
                        }
                    }
                    self.clear_palette_state();
                    if let Some(error) = first_error {
                        return Err(error);
                    }
                }
                "new_tab" => {
                    self.clear_palette_state();
                    self.new_tab();
                }
                "close_tab" => {
                    let tab_id = self.workspace.active().1;
                    self.clear_palette_state();
                    self.runtime
                        .block_on(self.client.close_tab(tab_id))
                        .map_err(|error| error.to_string())?;
                    self.reconcile();
                }
                "cycle_tab_next" => {
                    self.cycle_tab(1)?;
                    self.clear_palette_state();
                }
                "cycle_tab_prev" => {
                    self.cycle_tab(-1)?;
                    self.clear_palette_state();
                }
                "toggle_sidebar" => {
                    self.clear_palette_state();
                    self.toggle_sidebar();
                }
                "toggle_sidebar_agents" => {
                    self.clear_palette_state();
                    self.toggle_sidebar_agents();
                }
                "font_increase" => {
                    self.clear_palette_state();
                    self.apply_font_size_transition(FontSizeTransition::Adjust(1.0))?;
                }
                "font_decrease" => {
                    self.clear_palette_state();
                    self.apply_font_size_transition(FontSizeTransition::Adjust(-1.0))?;
                }
                "font_reset" => {
                    self.clear_palette_state();
                    self.apply_font_size_transition(FontSizeTransition::Reset)?;
                }
                "jump_to_unread" => {
                    let active_project_id = self.workspace.active().0;
                    let target = notification_inbox::next_unread(
                        &self.notification_inbox,
                        active_project_id,
                    );
                    if let Some(tab_id) = target {
                        self.focus_tab_and_clear(tab_id, true)?;
                    }
                    self.clear_palette_state();
                }
                "rename_project" => {
                    let project_id = self.workspace.active().0;
                    self.clear_palette_state();
                    self.begin_rename_target(RenameTarget::Project(project_id))?;
                }
                "rename_tab" => {
                    let tab_id = self.workspace.active().1;
                    self.clear_palette_state();
                    self.begin_rename_target(RenameTarget::Tab(tab_id))?;
                }
                "custom_commands" => {
                    if let Some(state) = &mut self.palette {
                        state.push(provider_palette_frame(&self.config.providers));
                    }
                }
                command => {
                    return Err(format!(
                        "palette command {command:?} is not implemented by Iced"
                    ));
                }
            },
            "launcher" => {
                let index = custom_command::launch_index(&item.id)
                    .filter(|index| *index < self.config.commands.len())
                    .ok_or_else(|| format!("launcher row {:?} cannot be run", item.id))?;
                let command = self.config.commands[index].clone();
                let (project_id, _) = self.workspace.active();
                let cwd = self.launch_cwd(project_id);
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
                let argv = custom_command::launch_argv(&shell, &command);
                self.clear_palette_state();
                self.runtime
                    .block_on(self.client.open_tab(
                        project_id,
                        &cwd,
                        &command.title,
                        &argv,
                        u32::from(DEFAULT_COLS),
                        u32::from(DEFAULT_ROWS),
                    ))
                    .map_err(|error| error.to_string())?;
                self.reconcile();
            }
            "themes" => {
                let persistence_error = self.commit_theme_name(&item.id)?;
                finish_theme_confirmation(
                    &mut self.palette,
                    &mut self.palette_theme_at_open,
                    &mut self.status,
                    persistence_error,
                    Instant::now(),
                );
                self.palette_family_at_open = None;
                self.palette_resolved_family_at_open = None;
            }
            "fonts" => {
                let persistence_error = self.commit_font_family(&item.id)?;
                self.clear_palette_state();
                if let Some(error) = persistence_error {
                    self.set_status(error);
                }
            }
            agent_palette::FRAME_ID => {
                let tab_id = agent_palette::agent_tab_id(&item.id)
                    .ok_or_else(|| format!("agent row {:?} cannot be activated", item.id))?;
                self.focus_tab_and_clear(tab_id, true)?;
                self.clear_palette_state();
            }
            "notifications" => {
                let tab_id = notification_inbox::tab_id(&item.id)
                    .ok_or_else(|| format!("notification row {:?} cannot be activated", item.id))?;
                self.focus_tab_and_clear(tab_id, true)?;
                self.clear_palette_state();
            }
            "custom" => {
                let index = provider::provider_index(&item.id)
                    .filter(|index| *index < self.config.providers.len())
                    .ok_or_else(|| format!("provider row {:?} cannot be activated", item.id))?;
                self.spawn_provider(
                    self.config.providers[index].clone(),
                    provider::Phase::List,
                    None,
                );
            }
            "present" => {
                if let Some(reply) = self.palette_present_reply.take() {
                    let _ = reply.send(Ok(PalettePresentResult {
                        selected_id: Some(item.id),
                        dismissed: false,
                    }));
                }
                self.clear_palette_state();
            }
            frame if frame.starts_with("provider:items:") => {
                let provider = self
                    .provider_frames
                    .get(frame)
                    .cloned()
                    .ok_or_else(|| format!("provider frame {frame:?} is stale"))?;
                self.spawn_provider(provider, provider::Phase::Activate, Some(item.id));
            }
            frame => {
                return Err(format!(
                    "palette frame {frame:?} is not implemented by Iced"
                ))
            }
        }
        if self.palette.is_some() {
            self.invalidate_palette_geometry(PaletteVisibilityRequest::Reveal);
        } else {
            self.clear_palette_geometry();
        }
        Ok(self.palette_state_result())
    }

    fn preview_selected_palette_item(&mut self) -> Result<(), String> {
        let selected = self.palette.as_ref().and_then(|state| {
            state
                .selected_item()
                .map(|item| (state.current().id.clone(), item.id))
        });
        match selected {
            Some((frame, name)) if frame == "themes" => self.apply_theme_name(&name),
            Some((frame, name)) if frame == "fonts" => self.preview_font_family(&name),
            _ => Ok(()),
        }
    }

    fn apply_theme_name(&mut self, name: &str) -> Result<(), String> {
        if self.active_theme_name == name {
            return Ok(());
        }
        let theme = Theme::load_bundled(name);
        let mut targets = self
            .tabs
            .iter()
            .map(|(tab_id, tab)| (*tab_id, tab.theme.clone()))
            .collect::<Vec<_>>();
        targets.sort_by_key(|(tab_id, _)| *tab_id);
        if let Err(failure) = apply_theme_batch(&targets, &theme, |tab_id, candidate| {
            self.tabs
                .get_mut(&tab_id)
                .ok_or_else(|| format!("tab {tab_id} disappeared during theme application"))?
                .set_theme(candidate)
                .map_err(|error| error.to_string())
        }) {
            let mut message = format!("apply theme to tab {}: {}", failure.tab_id, failure.apply);
            for (tab_id, error) in failure.rollback {
                message.push_str(&format!("; rollback tab {tab_id}: {error}"));
            }
            return Err(message);
        }
        self.active_theme_name = name.to_string();
        Ok(())
    }

    fn commit_theme_name(&mut self, name: &str) -> Result<Option<String>, String> {
        self.apply_theme_name(name)?;
        let path = config::config_path();
        Ok(
            persist_theme_selection_with(&mut self.config, path.as_deref(), name, config::set_key)
                .err()
                .map(|error| format!("persist theme: {error}")),
        )
    }

    fn palette_back_or_dismiss(&mut self) {
        let is_root = self
            .palette
            .as_ref()
            .is_none_or(palette::PaletteState::is_root);
        if is_root {
            self.dismiss_palette_with_focus_recovery();
            return;
        }
        let frame = self
            .palette
            .as_ref()
            .map(|state| state.current().id.clone())
            .unwrap_or_default();
        let restored = match frame.as_str() {
            "themes" => self.restore_palette_theme(),
            "fonts" => self.restore_palette_family(),
            _ => Ok(()),
        };
        let restored = retain_palette_focus_after_back(
            &mut self.palette_focus_requested,
            self.palette.is_some(),
            restored,
        );
        if let Err(error) = restored {
            self.set_status(error);
            return;
        }
        if let Some(state) = &mut self.palette {
            let _ = state.pop();
        }
        self.provider_request = self.provider_request.wrapping_add(1).max(1);
        self.invalidate_palette_geometry(PaletteVisibilityRequest::Reveal);
    }

    fn dismiss_palette_with_focus_recovery(&mut self) {
        let result = self.try_dismiss_palette();
        let result = retain_palette_focus_after_back(
            &mut self.palette_focus_requested,
            self.palette.is_some(),
            result,
        );
        if let Err(error) = result {
            self.set_status(error);
        }
    }

    fn try_dismiss_palette(&mut self) -> Result<(), String> {
        self.restore_palette_theme()?;
        self.restore_palette_family()?;
        self.clear_palette_state();
        Ok(())
    }

    fn clear_palette_state(&mut self) {
        self.palette = None;
        self.palette_focus_requested = false;
        self.palette_theme_at_open = None;
        self.palette_family_at_open = None;
        self.palette_resolved_family_at_open = None;
        self.clear_palette_geometry();
        self.provider_request = self.provider_request.wrapping_add(1).max(1);
        self.provider_frames.clear();
        if let Some(reply) = self.palette_present_reply.take() {
            let _ = reply.send(Ok(PalettePresentResult {
                selected_id: None,
                dismissed: true,
            }));
        }
    }

    fn restore_palette_theme(&mut self) -> Result<(), String> {
        if let Some(name) = self.palette_theme_at_open.clone() {
            self.apply_theme_name(&name)?;
        }
        Ok(())
    }

    fn restore_palette_family(&mut self) -> Result<(), String> {
        if let Some(family) = self.palette_family_at_open.clone() {
            self.apply_font_family(family)?;
        }
        Ok(())
    }

    fn cycle_tab(&mut self, delta: isize) -> Result<(), String> {
        let (project_id, active_tab) = self.workspace.active();
        let projects = self.workspace.snapshot();
        let tabs = projects
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.tabs.as_slice())
            .unwrap_or_default();
        if tabs.is_empty() {
            return Ok(());
        }
        let current = tabs
            .iter()
            .position(|tab| tab.id == active_tab)
            .unwrap_or(0);
        let Some(next) = clamped_tab_index(current, tabs.len(), delta) else {
            return Ok(());
        };
        if next == current {
            return Ok(());
        }
        self.focus_tab_and_clear(tabs[next].id, false)?;
        Ok(())
    }

    fn switch_project_by_index(&mut self, index: u8) -> Result<(), String> {
        let projects = self.workspace.snapshot();
        let Some(project_id) = project_id_at_index(&projects, index) else {
            return Ok(());
        };
        let Some(tab_id) = self.workspace.preferred_tab(project_id) else {
            return Ok(());
        };
        self.focus_tab_and_clear(tab_id, false)
    }

    fn switch_tab_by_index(&mut self, index: u8) -> Result<(), String> {
        let project_id = self.workspace.active().0;
        let projects = self.workspace.snapshot();
        let Some(tab_id) = active_project_tab_at_index(&projects, project_id, index) else {
            return Ok(());
        };
        self.focus_tab_and_clear(tab_id, false)
    }

    fn focus_tab_and_clear(&mut self, tab_id: i64, reveal_sidebar: bool) -> Result<(), String> {
        self.workspace
            .focus_tab(tab_id)
            .map_err(|error| error.to_string())?;
        self.workspace
            .set_tab_has_notification(tab_id, false)
            .map_err(|error| error.to_string())?;
        if reveal_sidebar {
            self.set_sidebar_collapsed(false);
        }
        Ok(())
    }

    fn launch_cwd(&self, project_id: i64) -> String {
        let active_tab = self.workspace.active().1;
        if let Some(native) = self.supervisor.foreground_cwd(active_tab) {
            if !native.is_empty() {
                return native;
            }
        }
        self.projects
            .iter()
            .find(|project| project.id == project_id)
            .and_then(|project| project.tabs.iter().find(|tab| tab.id == active_tab))
            .map(|tab| tab.cwd.clone())
            .unwrap_or_default()
    }

    fn reconcile(&mut self) {
        // Full authoritative snapshot on every UI tick is the recovery path
        // for a slow consumer: deltas are an optimization, never UI truth.
        self.projects = self.workspace.snapshot();
        self.reconcile_tab_drag_preview();
        self.reconcile_rename_editor();
        self.reconcile_notification_inbox();
        self.refresh_notification_palette();
        self.refresh_sidebar_agents();
        self.refresh_agent_palette();
        let live_ids: HashSet<i64> = self
            .projects
            .iter()
            .flat_map(|project| project.tabs.iter().map(|tab| tab.id))
            .collect();
        self.tabs.retain(|tab_id, _| live_ids.contains(tab_id));
        let active_tab_id = self.workspace.active().1;
        for (tab_id, tab) in &mut self.tabs {
            if *tab_id != active_tab_id && tab.reset_pointer_state() {
                if let Err(error) = tab.refresh_snapshot() {
                    tracing::warn!(
                        ?error,
                        tab_id,
                        "terminal pointer reset failed after active tab changed"
                    );
                }
            }
        }
        for tab_id in live_ids {
            if self.tabs.contains_key(&tab_id) {
                continue;
            }
            let attached = {
                let _guard = self.runtime.enter();
                TerminalTab::attach(
                    Arc::clone(&self.supervisor),
                    tab_id,
                    self.test_mode,
                    Theme::load_bundled(&self.active_theme_name),
                    self.config.word_break_chars.clone(),
                )
            };
            match attached {
                Ok(mut tab) => {
                    let (cols, rows) = terminal_grid(
                        self.window_size,
                        self.workspace.sidebar_collapsed(),
                        self.terminal_metrics,
                    );
                    match tab.apply_geometry(
                        cols,
                        rows,
                        self.terminal_metrics,
                        self.metric_generation,
                    ) {
                        Ok(Some(change)) => {
                            tab.commit_geometry(change);
                            self.tabs.insert(tab_id, tab);
                        }
                        Ok(None) => {
                            tracing::warn!(tab_id, "new terminal did not install renderer metrics")
                        }
                        Err(error) => tracing::warn!(
                            tab_id,
                            ?error,
                            "new terminal renderer geometry installation failed"
                        ),
                    }
                }
                Err(error) => tracing::debug!(tab_id, ?error, "PTY not ready for UI attach"),
            }
        }
    }

    fn service_workspace_events(&mut self) {
        loop {
            match self.workspace_events.try_recv() {
                Ok(WorkspaceEvent::NotificationFired {
                    tab_id,
                    title: _,
                    body,
                }) => {
                    if let Some((project_id, title)) = self.notification_title(tab_id) {
                        self.notification_inbox.upsert(
                            notification_inbox::NotificationRecord::new(
                                tab_id, project_id, title, body,
                            ),
                        );
                    }
                }
                Ok(WorkspaceEvent::TabNotification {
                    tab_id,
                    has_pending: false,
                })
                | Ok(WorkspaceEvent::TabClosed { tab_id }) => {
                    self.notification_inbox.remove(tab_id);
                }
                Ok(WorkspaceEvent::ProjectDeleted { project_id }) => {
                    let stale: Vec<i64> = self
                        .notification_inbox
                        .snapshot()
                        .iter()
                        .filter(|record| record.project_id == project_id)
                        .map(|record| record.tab_id)
                        .collect();
                    for tab_id in stale {
                        self.notification_inbox.remove(tab_id);
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(dropped)) => {
                    tracing::warn!(dropped, "Iced workspace event consumer lagged; resyncing");
                    break;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                | Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
    }

    fn notification_title(&self, tab_id: i64) -> Option<(i64, String)> {
        self.workspace.snapshot().into_iter().find_map(|project| {
            project.tabs.into_iter().find_map(|tab| {
                (tab.id == tab_id).then(|| {
                    let tab_title = if !tab.title.is_empty() {
                        tab.title
                    } else if !tab.cwd.is_empty() {
                        tab.cwd
                    } else {
                        "Tab".to_string()
                    };
                    (
                        project.id,
                        notification_inbox::compose_title(&project.name, &tab_title),
                    )
                })
            })
        })
    }

    fn reconcile_notification_inbox(&mut self) {
        let pending_rows: Vec<(i64, i64, String)> = self
            .projects
            .iter()
            .flat_map(|project| {
                project
                    .tabs
                    .iter()
                    .filter(|tab| tab.has_notification)
                    .map(move |tab| {
                        let tab_title = if !tab.title.is_empty() {
                            tab.title.clone()
                        } else if !tab.cwd.is_empty() {
                            tab.cwd.clone()
                        } else {
                            "Tab".to_string()
                        };
                        (
                            tab.id,
                            project.id,
                            notification_inbox::compose_title(&project.name, &tab_title),
                        )
                    })
            })
            .collect();
        let pending_ids: HashSet<i64> = pending_rows.iter().map(|row| row.0).collect();
        let stale: Vec<i64> = self
            .notification_inbox
            .tab_ids()
            .into_iter()
            .filter(|tab_id| !pending_ids.contains(tab_id))
            .collect();
        for tab_id in stale {
            self.notification_inbox.remove(tab_id);
        }

        let existing: HashSet<i64> = self.notification_inbox.tab_ids().into_iter().collect();
        let slots = notification_inbox::CAP.saturating_sub(self.notification_inbox.count());
        let mut additions = Vec::with_capacity(slots);
        for row in pending_rows
            .into_iter()
            .filter(|row| !existing.contains(&row.0))
            .take(slots)
        {
            additions.push(row);
        }
        // Insert in reverse snapshot order so the first deterministic
        // project/tab fallback remains at the front after repeated prepends.
        while let Some((tab_id, project_id, title)) = additions.pop() {
            self.notification_inbox
                .upsert(notification_inbox::NotificationRecord::new(
                    tab_id, project_id, title, "",
                ));
        }
    }

    fn refresh_notification_palette(&mut self) {
        let has_notifications = self.palette.as_ref().is_some_and(|state| {
            state
                .frames()
                .iter()
                .any(|frame| frame.id == "notifications")
        });
        if !has_notifications {
            return;
        }
        let before_layout = self.palette_layout_signature();
        let before_render = self.palette_render_signature();
        let items = notification_inbox::frame(&self.notification_inbox).items;
        if let Some(state) = &mut self.palette {
            state.update_items("notifications", items);
        }
        let request = dynamic_refresh_request(
            before_layout != self.palette_layout_signature(),
            before_render != self.palette_render_signature(),
        );
        if request != PaletteVisibilityRequest::None {
            self.invalidate_palette_geometry(request);
        }
    }

    fn refresh_sidebar_agents(&mut self) {
        let active_tab = self.workspace.active().1;
        let now = agent_palette::now_unix();
        self.sidebar_agents = self
            .projects
            .iter()
            .map(|project| {
                let rows = agent_palette::sidebar_agents(project, now)
                    .into_iter()
                    .map(|row| SidebarDumpAgentRow {
                        tab_id: row.tab_id,
                        name: row.name,
                        lifecycle: row.lifecycle,
                        status_text: row.status_text,
                        time_text: row.time_text,
                        is_active: row.tab_id == active_tab,
                    })
                    .collect();
                (project.id, rows)
            })
            .collect();
    }

    fn sidebar_dump(&self) -> SidebarDumpResult {
        let projects = self
            .workspace
            .snapshot()
            .into_iter()
            .map(|project| SidebarDumpProject {
                project_id: project.id,
                agents: self
                    .sidebar_agents
                    .get(&project.id)
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect();
        SidebarDumpResult {
            agents_visible: self.config.show_sidebar_agents,
            projects,
        }
    }

    fn refresh_agent_palette(&mut self) {
        let has_agents = self.palette.as_ref().is_some_and(|state| {
            state
                .frames()
                .iter()
                .any(|frame| frame.id == agent_palette::FRAME_ID)
        });
        if !has_agents {
            return;
        }
        let before_layout = self.palette_layout_signature();
        let before_render = self.palette_render_signature();
        // Do not rebuild from `self.projects`: an IPC workspace mutation and
        // `palette.open` can be serviced in the same event-loop turn, before
        // the adapter's next general reconcile. The engine snapshot is the
        // authoritative resync source for this live frame.
        let projects = self.workspace.snapshot();
        let cwds = agent_palette::agent_tab_cwds(&projects);
        let mut items = agent_palette::agent_items(&projects, agent_palette::now_unix());
        self.apply_metrics_cache(&cwds, &mut items);
        if let Some(state) = &mut self.palette {
            state.update_items(agent_palette::FRAME_ID, items);
        }
        let request = dynamic_refresh_request(
            before_layout != self.palette_layout_signature(),
            before_render != self.palette_render_signature(),
        );
        if request != PaletteVisibilityRequest::None {
            self.invalidate_palette_geometry(request);
        }
        self.spawn_agent_metrics(&cwds);
    }

    fn apply_metrics_cache(&self, cwds: &HashMap<i64, String>, items: &mut [palette::PaletteItem]) {
        for item in items {
            let Some(tab_id) = agent_palette::agent_tab_id(&item.id) else {
                continue;
            };
            let (Some(agent), Some(cwd)) = (item.agent.as_mut(), cwds.get(&tab_id)) else {
                continue;
            };
            agent.metrics_text = self
                .metrics_cache
                .text_for_session(self.palette_session, cwd)
                .map(str::to_string);
        }
    }

    fn spawn_agent_metrics(&mut self, cwds: &HashMap<i64, String>) {
        self.metrics_cache.begin_session(self.palette_session);
        let claimed = self.metrics_cache.claim_unprobed(cwds.values().cloned());
        if claimed.is_empty() {
            return;
        }
        let known = self.metrics_cache.known_roots();
        let probe = Arc::clone(&self.git_probe);
        let tx = self.metrics_tx.clone();
        let session = self.palette_session;
        let failed_claims = claimed.clone();
        let task = self
            .runtime
            .spawn(git_metrics::probe_batch(probe, claimed, known));
        self.runtime.spawn(async move {
            let outcomes = task.await.map_err(|error| error.to_string());
            let _ = tx.send(AgentMetricsResult {
                session,
                claimed: failed_claims,
                outcomes,
            });
        });
    }

    fn service_agent_metrics(&mut self) {
        while let Ok(result) = self.metrics_rx.try_recv() {
            if self.palette.is_none()
                || result.session != self.palette_session
                || self.metrics_cache.session() != result.session
            {
                continue;
            }
            match result.outcomes {
                Ok(outcomes) => {
                    for outcome in outcomes {
                        let Some(root) = outcome.root.clone() else {
                            if let git_metrics::ProbeValue::Measured(Err(error)) = &outcome.value {
                                tracing::debug!(cwd = %outcome.cwd, reason = %error, "no git metrics");
                            }
                            self.metrics_cache.store_unresolved(&outcome.cwd);
                            continue;
                        };
                        let text = match outcome.value {
                            git_metrics::ProbeValue::Reused(text) => text,
                            git_metrics::ProbeValue::Measured(Ok(metrics)) => metrics.text(),
                            git_metrics::ProbeValue::Measured(Err(error)) => {
                                tracing::debug!(cwd = %outcome.cwd, reason = %error, "no git metrics");
                                git_metrics::UNKNOWN.to_string()
                            }
                        };
                        self.metrics_cache.store_root(&outcome.cwd, &root, text);
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "git metrics task failed");
                    for cwd in result.claimed {
                        self.metrics_cache.store_unresolved(&cwd);
                    }
                }
            }
        }
    }

    fn spawn_provider(
        &mut self,
        provider: provider::Provider,
        phase: provider::Phase,
        selected_id: Option<String>,
    ) {
        self.provider_request = self.provider_request.wrapping_add(1).max(1);
        let request = self.provider_request;
        let palette_session = self.palette_session;
        let origin_frame = self
            .palette
            .as_ref()
            .map(|state| state.current().id.clone())
            .unwrap_or_default();
        let context = self.provider_context(selected_id);
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let argv =
            provider::invocation_argv(&shell, &provider.run, provider.shell_interpret, phase);
        let env = provider::invocation_env(phase, &context);
        let has_roostctl = env.iter().any(|(key, _)| key == "ROOST_ROOSTCTL");
        let process_request = ProcessRequest {
            argv,
            env,
            env_remove: (!has_roostctl)
                .then(|| "ROOST_ROOSTCTL".to_string())
                .into_iter()
                .collect(),
            stdin: provider::invocation_stdin(phase, &context).into_bytes(),
            cwd: (!context.active_cwd.is_empty()).then(|| context.active_cwd.into()),
            timeout: Duration::from_secs(provider.timeout_secs),
        };
        let tx = self.provider_tx.clone();
        let result_provider = provider;
        let task = self.runtime.spawn(async move {
            let stdout = process::run(process_request)
                .await
                .map_err(|error| error.to_string())?
                .stdout;
            match phase {
                provider::Phase::List => provider::parse_provider_output(&stdout),
                provider::Phase::Activate => provider::parse_activate_output(&stdout),
            }
        });
        self.runtime.spawn(async move {
            let outcome = task
                .await
                .map_err(|error| format!("provider task failed: {error}"))
                .and_then(|outcome| outcome);
            let _ = tx.send(ProviderRunResult {
                palette_session,
                request,
                origin_frame,
                provider: result_provider,
                phase,
                outcome,
            });
        });
    }

    fn provider_context(&self, selected_id: Option<String>) -> provider::ProviderContext {
        let (project_id, tab_id) = self.workspace.active();
        let active_title = self
            .workspace
            .snapshot()
            .into_iter()
            .flat_map(|project| project.tabs)
            .find(|tab| tab.id == tab_id)
            .map(|tab| tab.title)
            .unwrap_or_default();
        provider::ProviderContext {
            socket: self.client.socket_path.to_string_lossy().into_owned(),
            query: self
                .palette
                .as_ref()
                .map(|state| state.current().query.clone())
                .unwrap_or_default(),
            selected_id,
            active_tab_id: (tab_id != 0).then_some(tab_id),
            active_project_id: (project_id != 0).then_some(project_id),
            active_cwd: if project_id != 0 {
                self.launch_cwd(project_id)
            } else {
                String::new()
            },
            active_title,
            roostctl: process::sibling_executable("roostctl"),
        }
    }

    fn service_provider_results(&mut self) {
        while let Ok(result) = self.provider_rx.try_recv() {
            let current_frame = self
                .palette
                .as_ref()
                .map(|state| state.current().id.as_str());
            if !provider_result_is_current(
                self.palette.is_some(),
                self.palette_session,
                self.provider_request,
                current_frame,
                &result,
            ) {
                continue;
            }
            let before_layout = self.palette_layout_signature();
            let before_render = self.palette_render_signature();
            match result.outcome {
                Ok(output)
                    if result.phase == provider::Phase::Activate && output.items.is_empty() =>
                {
                    if let Err(error) = self.try_dismiss_palette() {
                        self.set_status(error);
                    }
                }
                Ok(output) => {
                    let placeholder = if output.placeholder.is_empty() {
                        format!("{}…", result.provider.title)
                    } else {
                        output.placeholder.clone()
                    };
                    let items = provider::output_palette_items(&output, result.provider.limit);
                    let frame_id = format!("provider:items:{}", result.request);
                    self.provider_frames
                        .insert(frame_id.clone(), result.provider);
                    if let Some(state) = &mut self.palette {
                        state.push(palette::PaletteFrame::new(frame_id, placeholder, items));
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "provider run failed");
                    let frame_id = format!("provider:items:{}", result.request);
                    let items =
                        vec![
                            palette::PaletteItem::new("provider:_error", "Provider failed")
                                .with_subtitle(Some(error))
                                .with_actionable(false),
                        ];
                    if let Some(state) = &mut self.palette {
                        state.push(palette::PaletteFrame::new(
                            frame_id,
                            "Provider error",
                            items,
                        ));
                    }
                }
            }
            let request = dynamic_refresh_request(
                before_layout != self.palette_layout_signature(),
                before_render != self.palette_render_signature(),
            );
            if self.palette.is_some() && request != PaletteVisibilityRequest::None {
                self.invalidate_palette_geometry(request);
            }
        }
    }

    fn service_ui_requests(&mut self) -> UiTask {
        let mut task = UiTask::None;
        while let Ok(request) = self.ui_rx.try_recv() {
            match request {
                UiRequest::Activate => {
                    if let Some(id) = self.window_id {
                        task = task.then(UiTask::Focus(id));
                    }
                }
                UiRequest::Dump { tab_id, reply } => {
                    let result = self
                        .tabs
                        .get(&tab_id)
                        .map(TerminalTab::dump)
                        .ok_or_else(|| format!("tab {tab_id} has no live terminal"));
                    let _ = reply.send(result);
                }
                UiRequest::TabFeedPtyBytes {
                    tab_id,
                    data,
                    reply,
                } => {
                    let mut actions = None;
                    let result = if !self.test_mode {
                        Err("ROOST_TEST_MODE=1 is required".into())
                    } else if let Some(tab) = self.tabs.get_mut(&tab_id) {
                        actions = Some(tab.write_vt(&data));
                        tab.refresh_snapshot().map_err(|error| error.to_string())
                    } else {
                        Err(format!("tab {tab_id} has no live terminal"))
                    };
                    if let Some(actions) = actions {
                        task = task.then(self.apply_osc_actions(tab_id, actions));
                    }
                    let _ = reply.send(result);
                }
                UiRequest::TabCapturePtyInput {
                    tab_id,
                    drain,
                    reply,
                } => {
                    let result = self
                        .tabs
                        .get(&tab_id)
                        .and_then(|tab| tab.input_capture.as_ref())
                        .ok_or_else(|| {
                            "ROOST_TEST_MODE=1 is required or tab is missing".to_string()
                        })
                        .and_then(|capture| {
                            capture
                                .lock()
                                .map(|mut bytes| {
                                    if drain {
                                        std::mem::take(&mut *bytes)
                                    } else {
                                        bytes.clone()
                                    }
                                })
                                .map_err(|_| "PTY input capture lock poisoned".to_string())
                        });
                    let _ = reply.send(result);
                }
                UiRequest::TabDumpResolved { tab_id, reply } => {
                    let result = self
                        .tabs
                        .get(&tab_id)
                        .map(TerminalTab::resolved_cells)
                        .ok_or_else(|| format!("tab {tab_id} has no live terminal"));
                    let _ = reply.send(result);
                }
                UiRequest::WindowMetrics { reply } => {
                    let collapsed = self.workspace.sidebar_collapsed();
                    let resolved_family = self
                        .font_registry
                        .resolve(self.typography.effective_family())
                        .name
                        .to_string();
                    let _ = reply.send(Ok((
                        f64::from(self.window_size.width),
                        f64::from(self.window_size.height),
                        f64::from(sidebar_width(collapsed)),
                        collapsed,
                        Some(f64::from(chrome::BAND_HEIGHT)),
                        Some(resolved_family),
                    )));
                }
                UiRequest::WindowResize {
                    width,
                    height,
                    reply,
                } => {
                    let result = if !self.test_mode {
                        Err("ROOST_TEST_MODE=1 is required".into())
                    } else {
                        let size = Size::new(width as f32, height as f32);
                        // Some Wayland compositors retain authority over the
                        // toplevel size and may ignore a client request. Apply
                        // the requested logical geometry immediately for the
                        // deterministic test port; a compositor Resized event
                        // remains authoritative if it sends one afterward.
                        self.resize(size);
                        if let Some(id) = self.window_id {
                            task = task.then(UiTask::Resize(id, size));
                        } else {
                            // IPC can become reachable just before Iced emits
                            // WindowOpened. Preserve the native resize until an
                            // ID exists instead of rejecting a ready server.
                            self.pending_window_resize = Some(size);
                        }
                        Ok(())
                    };
                    let _ = reply.send(result);
                }
                UiRequest::AppSetWindowFocus { focused, reply } => {
                    let result = if self.test_mode {
                        self.set_window_focus(focused);
                        Ok(())
                    } else {
                        Err("ROOST_TEST_MODE=1 is required".into())
                    };
                    let _ = reply.send(result);
                }
                UiRequest::AppCursorShape { reply } => {
                    let shape = self
                        .tabs
                        .get(&self.workspace.active().1)
                        .map_or("default", TerminalTab::effective_pointer_shape);
                    let _ = reply.send(Ok(shape.into()));
                }
                UiRequest::AppActiveTerminalFocused { reply } => {
                    let focused = matches!(self.keyboard_route(), KeyboardRoute::Terminal(_));
                    let _ = reply.send(Ok(focused));
                }
                UiRequest::AppSelectedTabId { reply } => {
                    let _ = reply.send(Ok(self.workspace.active().1));
                }
                UiRequest::Screenshot { scale, reply } => {
                    self.screenshots.enqueue(scale, reply);
                }
                UiRequest::PaletteOpen { kind, reply } => {
                    let result = self
                        .open_palette(&kind)
                        .map(|()| self.palette_state_result());
                    if result.is_ok() {
                        task = task.then(self.take_palette_focus_task());
                    }
                    let _ = reply.send(result);
                }
                UiRequest::PaletteState { reply } => {
                    let _ = reply.send(Ok(self.palette_state_result()));
                }
                UiRequest::PaletteQuery { query, reply } => {
                    let _ = reply.send(self.query_palette(&query));
                }
                UiRequest::PaletteActivate { id, reply } => {
                    let _ = reply.send(self.activate_palette(&id));
                }
                UiRequest::PaletteDismiss { reply } => {
                    let result = self
                        .try_dismiss_palette()
                        .map(|()| self.palette_state_result());
                    let _ = reply.send(result);
                }
                UiRequest::PalettePresent {
                    title,
                    placeholder,
                    items,
                    reply,
                } => {
                    self.present_palette(title, placeholder, items, reply);
                    task = task.then(self.take_palette_focus_task());
                }
                UiRequest::SelectionSet {
                    tab_id,
                    anchor,
                    cursor,
                    reply,
                } => {
                    let result = self
                        .tabs
                        .get_mut(&tab_id)
                        .ok_or_else(|| format!("tab {tab_id} has no live terminal"))
                        .and_then(|tab| {
                            if !tab.selection.set(&tab.terminal, anchor, cursor) {
                                return Err(format!(
                                    "selection coordinates are outside tab {tab_id}'s viewport"
                                ));
                            }
                            tab.refresh_snapshot().map_err(|error| error.to_string())
                        });
                    let _ = reply.send(result);
                }
                UiRequest::SelectionClear { tab_id, reply } => {
                    let result = self
                        .tabs
                        .get_mut(&tab_id)
                        .ok_or_else(|| format!("tab {tab_id} has no live terminal"))
                        .and_then(|tab| {
                            tab.selection.clear();
                            tab.refresh_snapshot().map_err(|error| error.to_string())
                        });
                    let _ = reply.send(result);
                }
                UiRequest::SelectionDump { tab_id, reply } => {
                    let result = self
                        .tabs
                        .get_mut(&tab_id)
                        .ok_or_else(|| format!("tab {tab_id} has no live terminal"))
                        .and_then(|tab| tab.selection_dump().map_err(|error| error.to_string()));
                    let _ = reply.send(result);
                }
                UiRequest::ClipboardDump { target, reply } => {
                    self.clipboard.enqueue_ipc_read(target, reply);
                    task = task.then(self.clipboard.start_next());
                }
                UiRequest::ClipboardWrite { target, text } => {
                    self.clipboard.enqueue_write(target, text);
                    task = task.then(self.clipboard.start_next());
                }
                UiRequest::TabExpandSelectionAt {
                    tab_id,
                    col,
                    row,
                    click_count,
                    reply,
                } => {
                    let result = if !self.test_mode {
                        Err(
                            "tab.expand_selection_at requires ROOST_TEST_MODE=1 at UI launch"
                                .into(),
                        )
                    } else {
                        self.tabs
                            .get_mut(&tab_id)
                            .ok_or_else(|| format!("tab {tab_id} has no live terminal"))
                            .and_then(|tab| {
                                tab.expand_selection_at(col, row, click_count)
                                    .map_err(|error| error.to_string())?
                                    .ok_or_else(|| {
                                        format!(
                                            "no word/line span at ({col}, {row}) on tab {tab_id} \
                                             (whitespace double-click, or row out of range)"
                                        )
                                    })
                            })
                    };
                    let _ = reply.send(result);
                }
                UiRequest::SidebarDump { reply } => {
                    let _ = reply.send(Ok(self.sidebar_dump()));
                }
                UiRequest::TabDispatchMouseEvent {
                    tab_id,
                    kind,
                    button,
                    cell_x,
                    cell_y,
                    mods,
                    reply,
                } => {
                    let result = if !self.test_mode {
                        Err("ROOST_TEST_MODE=1 is required".into())
                    } else {
                        u16::try_from(mods)
                            .map_err(|_| format!("modifier mask {mods} exceeds u16"))
                            .and_then(|mods| {
                                self.tabs
                                    .get_mut(&tab_id)
                                    .ok_or_else(|| format!("tab {tab_id} has no live terminal"))?
                                    .dispatch_pointer(kind, button, cell_x, cell_y, mods)
                                    .map_err(|error| error.to_string())
                            })
                    };
                    let _ = reply.send(result);
                }
            }
        }
        task
    }

    fn apply_osc_actions(&mut self, tab_id: i64, actions: Vec<OscAction>) -> UiTask {
        for action in actions {
            match action {
                OscAction::Workspace { command, payload } => {
                    self.client.apply_osc(tab_id, command, &payload);
                }
                OscAction::PtyInput(bytes) => {
                    if let Some(tab) = self.tabs.get(&tab_id) {
                        tab.session.send_input(bytes);
                    }
                }
                OscAction::ClipboardWrite { target, text } => {
                    if !enqueue_osc_clipboard_write(
                        &mut self.clipboard,
                        self.config.clipboard_write,
                        target,
                        text,
                    ) {
                        tracing::info!(
                            tab_id,
                            "OSC 52 clipboard write dropped — clipboard-write = deny"
                        );
                        continue;
                    }
                }
                OscAction::PointerShape(name) => {
                    if let Some(tab) = self.tabs.get_mut(&tab_id) {
                        tab.pointer_shape = canonical_pointer_shape(&name).into();
                    }
                }
            }
        }
        self.clipboard.start_next()
    }
}

fn canonical_pointer_shape(name: &str) -> &str {
    match name {
        "default" | "pointer" | "text" | "crosshair" | "grab" | "grabbing" | "not-allowed"
        | "col-resize" | "row-resize" | "n-resize" | "s-resize" | "e-resize" | "w-resize"
        | "ne-resize" | "nw-resize" | "se-resize" | "sw-resize" | "wait" | "progress" | "help"
        | "move" => name,
        _ => "default",
    }
}

fn agent_color(lifecycle: roost_ipc::agent::AgentLifecycle) -> Color {
    use roost_ipc::agent::AgentLifecycle;
    match lifecycle {
        AgentLifecycle::Working => Color::from_rgb8(0x5f, 0xa3, 0xf0),
        AgentLifecycle::Waiting => Color::from_rgb8(0xf0, 0xa0, 0x40),
        AgentLifecycle::Finished => Color::from_rgb8(0x7a, 0x7a, 0x7a),
        AgentLifecycle::Failed => Color::from_rgb8(0xe0, 0x52, 0x52),
        AgentLifecycle::Inactive => Color::from_rgba8(0x7a, 0x7a, 0x7a, 0.5),
    }
}

fn tab_status_color(lifecycle: roost_ipc::agent::AgentLifecycle) -> Color {
    if lifecycle == roost_ipc::agent::AgentLifecycle::Inactive {
        Color::TRANSPARENT
    } else {
        agent_color(lifecycle)
    }
}

fn pointer_action(action: PointerAction) -> roost_vt::MouseAction {
    match action {
        PointerAction::Press => mouse_action::PRESS,
        PointerAction::Release => mouse_action::RELEASE,
        PointerAction::Motion => mouse_action::MOTION,
    }
}

fn pointer_button(button: PointerButton) -> roost_vt::MouseButton {
    match button {
        PointerButton::Left => mouse_button::LEFT,
        PointerButton::Right => mouse_button::RIGHT,
        PointerButton::Middle => mouse_button::MIDDLE,
        PointerButton::Four => mouse_button::FOUR,
        PointerButton::Five => mouse_button::FIVE,
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Freeze and fsync the authoritative layout before PTY-exit tasks can
        // observe teardown and attempt a later persistence write.
        self.workspace.flush();
    }
}

fn hydrate_workspace(runtime: &tokio::runtime::Runtime, client: &LocalClient) -> Result<()> {
    let mut projects = runtime.block_on(client.list_projects())?;
    if projects.is_empty() {
        let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        projects.push(runtime.block_on(client.create_project("Roost", &cwd))?);
    }
    let restore = client.workspace.take_restore_layout();
    for project in &projects {
        let saved = restore
            .as_ref()
            .and_then(|layout| {
                layout
                    .projects
                    .iter()
                    .find(|item| item.project_id == project.id)
            })
            .map(|item| item.tabs.as_slice())
            .unwrap_or(&[]);
        let fallback;
        let specs = if saved.is_empty() {
            fallback = vec![RestoreTab {
                cwd: project.cwd.clone(),
                title: String::new(),
                user_titled: false,
            }];
            fallback.as_slice()
        } else {
            saved
        };
        for spec in specs {
            match runtime.block_on(client.open_tab(
                project.id,
                &spec.cwd,
                &spec.title,
                &[],
                u32::from(DEFAULT_COLS),
                u32::from(DEFAULT_ROWS),
            )) {
                Ok(tab) if spec.user_titled && !spec.title.is_empty() => {
                    client.workspace.set_tab_title(tab.id, &spec.title)?;
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(project_id = project.id, ?error, "restore tab failed"),
            }
        }
    }

    let snapshot = client.workspace.snapshot();
    let active_project = restore
        .as_ref()
        .map(|layout| layout.active_project_id)
        .filter(|id| snapshot.iter().any(|project| project.id == *id))
        .or_else(|| snapshot.first().map(|project| project.id));
    if let Some(project_id) = active_project {
        let position = restore
            .as_ref()
            .map_or(0, |layout| layout.active_tab_position.max(0) as usize);
        if let Some(tab_id) = snapshot
            .iter()
            .find(|project| project.id == project_id)
            .and_then(|project| project.tabs.get(position).or_else(|| project.tabs.first()))
            .map(|tab| tab.id)
        {
            client.workspace.focus_tab(tab_id)?;
        }
    }
    Ok(())
}

impl Message {
    pub(crate) fn apply(self, app: &mut App) -> UiTask {
        match self {
            Self::ProjectSelected(project_id) => app.select_project(project_id),
            Self::BeginRenameProject(project_id) => app.begin_rename_project(project_id),
            Self::AgentSelected(tab_id) => app.select_agent(tab_id),
            Self::TabSelected(tab_id) => app.select_tab(tab_id),
            Self::BeginRenameTab(tab_id) => app.begin_rename_tab(tab_id),
            Self::CloseTab(tab_id) => app.close_tab(tab_id),
            Self::NewTab => app.new_tab(),
            Self::ToggleSidebar => app.toggle_sidebar(),
            Self::OpenNotifications => return app.open_notifications(),
            _ => {}
        }
        UiTask::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deliver_ipc(completion: ClipboardReadCompletion) {
        let ClipboardReadCompletion::Ipc { reply, value } = completion else {
            panic!("expected IPC clipboard completion")
        };
        let _ = reply.send(Ok(value));
    }

    fn attached_test_terminal(tab_id: i64) -> (TerminalTab, Arc<PtySupervisor>) {
        let supervisor = Arc::new(PtySupervisor::new());
        let argv = vec!["/bin/sh".into(), "-c".into(), "cat".into()];
        let _early_output = supervisor
            .spawn(
                tab_id,
                "/tmp",
                &argv,
                DEFAULT_COLS,
                DEFAULT_ROWS,
                std::path::Path::new("/tmp/roost-iced-pointer-test.sock"),
            )
            .expect("spawn pointer-test PTY");
        let mut tab = TerminalTab::attach(
            Arc::clone(&supervisor),
            tab_id,
            true,
            Theme::roost_dark_fallback(),
            roost_ui_model::word_selection::DEFAULT_EXTRA_WORD_CHARS.to_string(),
        )
        .expect("attach pointer-test terminal");
        let metrics = TerminalMetrics::measure(13.0).expect("pointer-test terminal metrics");
        tab.apply_geometry(DEFAULT_COLS, DEFAULT_ROWS, metrics, 1)
            .expect("install pointer-test terminal metrics")
            .expect("new pointer-test terminal changes geometry");
        (tab, supervisor)
    }

    fn native_pointer(
        tab: &mut TerminalTab,
        action: PointerAction,
        button: Option<PointerButton>,
        cell: (u32, u32),
        click_count: u8,
        inside: bool,
        link_modifier_held: bool,
    ) -> NativePointerOutcome {
        tab.handle_native_pointer(NativePointerDispatch {
            action,
            button,
            col: cell.0,
            row: cell.1,
            mods: 0,
            click_count,
            inside,
            link_modifier_held,
        })
        .expect("native pointer dispatch")
    }

    #[test]
    fn terminal_geometry_never_produces_zero_grid() {
        let size = Size::new(1.0, 1.0);
        let metrics = TerminalMetrics::measure(13.0).expect("test metrics");
        assert_eq!(terminal_grid(size, false, metrics), (2, 2));
    }

    #[test]
    fn collapsed_sidebar_has_no_layout_width() {
        assert_eq!(sidebar_width(false), SIDEBAR_WIDTH);
        assert_eq!(sidebar_width(true), 0.0);
    }

    #[test]
    fn status_banner_replaces_clears_and_expires_deterministically() {
        let now = Instant::now();
        let mut status = StatusBanner::default();
        status.set_at("first", now);
        assert_eq!(status.message(), Some("first"));
        status.set_at("replacement", now + Duration::from_secs(1));
        assert_eq!(status.message(), Some("replacement"));
        status.expire_at(now + Duration::from_secs(1) + STATUS_BANNER_DURATION);
        assert_eq!(status.message(), None);

        status.set_at("clear me", now);
        status.clear();
        assert_eq!(status.message(), None);
    }

    #[test]
    fn typed_palette_query_errors_are_visible_without_hiding_prior_state() {
        let now = Instant::now();
        let mut status = StatusBanner::default();
        status.set_at("prior status", now - Duration::from_secs(1));
        report_palette_query_result(
            &mut status,
            Err("font preview rollback: injected failure".to_string()),
            now,
        );
        assert_eq!(
            status.message(),
            Some("font preview rollback: injected failure")
        );
        assert_eq!(status.expires_at, Some(now + STATUS_BANNER_DURATION));
    }

    #[test]
    fn provider_result_must_match_the_frame_that_spawned_it() {
        let result = ProviderRunResult {
            palette_session: 7,
            request: 11,
            origin_frame: "custom".to_string(),
            provider: provider::Provider {
                label: "Fixture".to_string(),
                run: "fixture-provider".to_string(),
                title: "Fixture".to_string(),
                timeout_secs: 1,
                limit: 10,
                shell_interpret: false,
            },
            phase: provider::Phase::List,
            outcome: Err("unused fixture outcome".to_string()),
        };
        assert!(provider_result_is_current(
            true,
            7,
            11,
            Some("custom"),
            &result
        ));
        assert!(!provider_result_is_current(
            true,
            7,
            11,
            Some("fonts"),
            &result
        ));
        assert!(!provider_result_is_current(
            true,
            7,
            12,
            Some("custom"),
            &result
        ));
        assert!(!provider_result_is_current(
            false,
            7,
            11,
            Some("custom"),
            &result
        ));
    }

    #[test]
    fn tab_cycle_clamps_at_both_ends_instead_of_wrapping() {
        assert_eq!(clamped_tab_index(0, 3, -1), Some(0));
        assert_eq!(clamped_tab_index(0, 3, 1), Some(1));
        assert_eq!(clamped_tab_index(2, 3, 1), Some(2));
        assert_eq!(clamped_tab_index(2, 3, -1), Some(1));
        assert_eq!(clamped_tab_index(0, 0, 1), None);
        assert_eq!(clamped_tab_index(3, 3, -1), None);
    }

    #[test]
    fn repeated_keybinds_are_consumed_without_dispatching_the_action() {
        let mut calls = 0;
        let repeated = dispatch_keybind_once_unless_repeat(true, || {
            calls += 1;
            KeybindAction::CloseProject
        });
        assert_eq!(repeated, None);
        assert_eq!(calls, 0);

        let pressed = dispatch_keybind_once_unless_repeat(false, || {
            calls += 1;
            KeybindAction::CloseProject
        });
        assert_eq!(pressed, Some(KeybindAction::CloseProject));
        assert_eq!(calls, 1);
    }

    #[test]
    fn numeric_switch_helpers_follow_authoritative_snapshot_order() {
        let workspace = Workspace::new();
        let first = workspace.create_project("first", "/tmp").unwrap();
        let first_tab = workspace.open_tab(first.id, "/tmp", "one").unwrap();
        let second_tab = workspace.open_tab(first.id, "/tmp", "two").unwrap();
        let second = workspace.create_project("second", "/tmp").unwrap();
        let second_project_tab = workspace.open_tab(second.id, "/tmp", "three").unwrap();

        let snapshot = workspace.snapshot();
        assert_eq!(project_id_at_index(&snapshot, 1), Some(first.id));
        assert_eq!(project_id_at_index(&snapshot, 2), Some(second.id));
        assert_eq!(project_id_at_index(&snapshot, 0), None);
        assert_eq!(project_id_at_index(&snapshot, 10), None);
        assert_eq!(
            active_project_tab_at_index(&snapshot, first.id, 2),
            Some(second_tab.id)
        );
        assert_eq!(active_project_tab_at_index(&snapshot, first.id, 0), None);
        assert_eq!(active_project_tab_at_index(&snapshot, first.id, 10), None);

        workspace.reorder_projects(&[second.id, first.id]).unwrap();
        let reordered = workspace.snapshot();
        assert_eq!(project_id_at_index(&reordered, 1), Some(second.id));
        assert_eq!(workspace.preferred_tab(first.id), Some(second_tab.id));
        assert_eq!(
            workspace.preferred_tab(second.id),
            Some(second_project_tab.id)
        );

        workspace.focus_tab(first_tab.id).unwrap();
        assert_eq!(workspace.preferred_tab(first.id), Some(first_tab.id));
    }

    #[test]
    fn tab_drag_membership_requires_unique_nonzero_stable_ids() {
        assert!(same_stable_ids(&[30, 10, 20], &[10, 20, 30]));
        assert!(!same_stable_ids(&[10, 20], &[10, 20, 30]));
        assert!(!same_stable_ids(&[10, 10], &[10, 10]));
        assert!(!same_stable_ids(&[0, 10], &[0, 10]));
        assert!(!same_stable_ids(&[10, 20, 30], &[10, 20, 40]));
    }

    #[test]
    fn tab_drag_commit_dispatches_exactly_once_and_rejects_stale_or_noop_state() {
        let preview = TabDragPreview {
            context: TabDragContext {
                project_id: 7,
                source_id: 10,
                generation: 4,
            },
            original_ids: vec![10, 20, 30],
            ordered_ids: vec![20, 30, 10],
        };
        let mut calls = Vec::new();
        let applied = dispatch_tab_drag_commit_with(
            Some(&preview),
            &[10, 20, 30],
            &preview.context,
            &[10, 20, 30],
            vec![20, 30, 10],
            |project_id, ordered_ids| {
                calls.push((project_id, ordered_ids));
                Ok(())
            },
        )
        .unwrap();
        assert!(applied);
        assert_eq!(calls, vec![(7, vec![20, 30, 10])]);

        for (authoritative, ordered) in [
            (vec![30, 20, 10], vec![20, 30, 10]),
            (vec![10, 20, 30], vec![10, 20, 30]),
            (vec![10, 20, 30], vec![20, 10, 30]),
        ] {
            let result = dispatch_tab_drag_commit_with(
                Some(&preview),
                &authoritative,
                &preview.context,
                &[10, 20, 30],
                ordered,
                |_, _| panic!("stale/no-op tab drag dispatched"),
            );
            assert_eq!(result, Ok(false));
        }

        let stale_generation = dispatch_tab_drag_commit_with(
            Some(&preview),
            &[10, 20, 30],
            &TabDragContext {
                project_id: 7,
                source_id: 10,
                generation: 5,
            },
            &[10, 20, 30],
            vec![20, 30, 10],
            |_, _| panic!("stale-generation tab drag dispatched"),
        );
        assert_eq!(stale_generation, Ok(false));
    }

    #[test]
    fn tab_drag_settlement_is_owned_once_in_either_release_order() {
        let preview = TabDragPreview {
            context: TabDragContext {
                project_id: 7,
                source_id: 10,
                generation: 4,
            },
            original_ids: vec![10, 20, 30],
            ordered_ids: vec![20, 30, 10],
        };
        let fallback = TabDragCommitRequest::from(&preview);
        let direct = fallback.clone();

        for (first, second) in [
            (fallback.clone(), direct.clone()),
            (direct.clone(), fallback.clone()),
        ] {
            let mut current = Some(preview.clone());
            let mut calls = 0;
            let first_result = settle_tab_drag_commit_with(
                &mut current,
                &[10, 20, 30],
                first,
                |project_id, ordered_ids| {
                    calls += 1;
                    assert_eq!(project_id, 7);
                    assert_eq!(ordered_ids, [20, 30, 10]);
                    Ok(())
                },
            );
            assert_eq!(first_result, TabDragSettlement::Settled(Ok(true)));
            assert!(current.is_none());

            let second_result =
                settle_tab_drag_commit_with(&mut current, &[10, 20, 30], second, |_, _| {
                    panic!("duplicate tab release dispatched")
                });
            assert_eq!(second_result, TabDragSettlement::Ignored);
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn stale_or_unowned_release_does_not_clear_a_newer_preview() {
        let newer = TabDragPreview {
            context: TabDragContext {
                project_id: 7,
                source_id: 20,
                generation: 5,
            },
            original_ids: vec![10, 20, 30],
            ordered_ids: vec![20, 10, 30],
        };
        let stale = TabDragCommitRequest {
            context: TabDragContext {
                project_id: 7,
                source_id: 10,
                generation: 4,
            },
            original_ids: vec![10, 20, 30],
            ordered_ids: vec![20, 30, 10],
        };
        let mut current = Some(newer.clone());
        assert_eq!(
            settle_tab_drag_commit_with(&mut current, &[10, 20, 30], stale, |_, _| {
                panic!("stale release dispatched")
            }),
            TabDragSettlement::Ignored
        );
        assert_eq!(current, Some(newer));

        let mut absent = None;
        assert_eq!(
            settle_tab_drag_commit_with(
                &mut absent,
                &[10, 20, 30],
                TabDragCommitRequest {
                    context: TabDragContext {
                        project_id: 7,
                        source_id: 10,
                        generation: 4,
                    },
                    original_ids: vec![10, 20, 30],
                    ordered_ids: vec![20, 30, 10],
                },
                |_, _| panic!("unowned release dispatched"),
            ),
            TabDragSettlement::Ignored
        );
    }

    #[test]
    fn exact_subthreshold_end_clears_without_accepting_stale_or_moved_state() {
        let original = vec![10, 20, 30];
        let context = TabDragContext {
            project_id: 7,
            source_id: 10,
            generation: 4,
        };
        let preview = TabDragPreview {
            context: context.clone(),
            original_ids: original.clone(),
            ordered_ids: original.clone(),
        };
        let mut exact = Some(preview.clone());
        assert!(end_tab_drag_preview_if_owned(
            &mut exact, &original, &context, &original,
        ));
        assert!(exact.is_none());

        let mut stale = Some(preview.clone());
        assert!(!end_tab_drag_preview_if_owned(
            &mut stale,
            &original,
            &TabDragContext {
                generation: 5,
                ..context.clone()
            },
            &original,
        ));
        assert_eq!(stale, Some(preview.clone()));

        let moved = TabDragPreview {
            ordered_ids: vec![20, 10, 30],
            ..preview
        };
        let mut moved_state = Some(moved.clone());
        assert!(!end_tab_drag_preview_if_owned(
            &mut moved_state,
            &original,
            &context,
            &original,
        ));
        assert_eq!(moved_state, Some(moved));
    }

    #[test]
    fn crossed_threshold_return_to_origin_is_a_settled_noop() {
        let original = vec![10, 20, 30];
        let preview = TabDragPreview {
            context: TabDragContext {
                project_id: 7,
                source_id: 10,
                generation: 4,
            },
            original_ids: original.clone(),
            ordered_ids: original.clone(),
        };
        let request = TabDragCommitRequest::from(&preview);
        let mut current = Some(preview);
        assert_eq!(
            settle_tab_drag_commit_with(&mut current, &original, request, |_, _| {
                panic!("return-to-origin commit dispatched a reorder")
            }),
            TabDragSettlement::Settled(Ok(false))
        );
        assert!(current.is_none());
    }

    #[test]
    fn tab_drag_commit_surfaces_the_authoritative_command_error_once() {
        let preview = TabDragPreview {
            context: TabDragContext {
                project_id: 7,
                source_id: 10,
                generation: 4,
            },
            original_ids: vec![10, 20],
            ordered_ids: vec![20, 10],
        };
        let mut calls = 0;
        let error = dispatch_tab_drag_commit_with(
            Some(&preview),
            &[10, 20],
            &preview.context,
            &[10, 20],
            vec![20, 10],
            |_, _| {
                calls += 1;
                Err("injected reorder failure".into())
            },
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(error, "injected reorder failure");
    }

    #[test]
    fn rendered_close_keeps_its_exact_id_and_engine_fallback_semantics() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let workspace = Arc::new(Workspace::new());
        let project = workspace.create_project("one", "/tmp").unwrap();
        let sibling = workspace.open_tab(project.id, "/tmp", "sibling").unwrap();
        let doomed = workspace.open_tab(project.id, "/tmp", "doomed").unwrap();
        workspace.focus_tab(doomed.id).unwrap();
        let client = LocalClient::new(
            Arc::clone(&workspace),
            Arc::new(PtySupervisor::new()),
            "/tmp/roost-iced-close-test.sock".into(),
        );

        assert_eq!(
            close_tab_by_id(&runtime, &client, doomed.id).unwrap(),
            CloseTabOutcome::Closed
        );
        assert_eq!(workspace.active(), (project.id, sibling.id));

        // A queued click message retains the ID rendered into it. If another
        // actor already removed that tab, replaying the stale message is an
        // expected no-op and cannot close the newly-active sibling.
        assert_eq!(
            close_tab_by_id(&runtime, &client, doomed.id).unwrap(),
            CloseTabOutcome::AlreadyGone
        );
        assert!(workspace.tab(sibling.id).is_ok());

        let last_project = workspace.create_project("last", "/tmp").unwrap();
        let last = workspace.open_tab(last_project.id, "/tmp", "last").unwrap();
        workspace.focus_tab(last.id).unwrap();
        assert_eq!(
            close_tab_by_id(&runtime, &client, last.id).unwrap(),
            CloseTabOutcome::Closed
        );
        assert!(
            workspace
                .snapshot()
                .iter()
                .all(|project| project.id != last_project.id),
            "closing a project's last tab must remove the project"
        );
        assert_eq!(workspace.active(), (project.id, sibling.id));
    }

    #[test]
    fn keyboard_route_requires_a_live_terminal_and_gives_editor_precedence() {
        assert_eq!(
            resolve_keyboard_route(false, false, 7, false),
            KeyboardRoute::None
        );
        assert_eq!(
            resolve_keyboard_route(false, false, 7, true),
            KeyboardRoute::Terminal(7)
        );
        assert_eq!(
            resolve_keyboard_route(false, true, 7, true),
            KeyboardRoute::Palette
        );
        assert_eq!(
            resolve_keyboard_route(true, true, 7, true),
            KeyboardRoute::Editor
        );
    }

    fn rename_fixture() -> (Vec<Project>, i64, i64, i64) {
        let workspace = Workspace::new();
        let first = workspace.create_project("First", "/tmp").unwrap();
        let first_tab = workspace.open_tab(first.id, "/tmp", "alpha").unwrap();
        let second = workspace.create_project("Second", "/var").unwrap();
        let second_tab = workspace.open_tab(second.id, "/var", "beta").unwrap();
        (workspace.snapshot(), first.id, first_tab.id, second_tab.id)
    }

    #[test]
    fn rename_editor_uses_typed_stable_targets_and_visibility() {
        let (projects, first_project, first_tab, second_tab) = rename_fixture();
        let project = begin_rename_editor(&projects, RenameTarget::Project(first_project)).unwrap();
        assert_eq!(project.opened_label, "First");
        assert!(rename_editor_is_renderable(
            &project,
            &projects,
            first_project,
            false
        ));
        assert!(!rename_editor_is_renderable(
            &project,
            &projects,
            first_project,
            true
        ));

        let tab = begin_rename_editor(&projects, RenameTarget::Tab(first_tab)).unwrap();
        assert!(rename_editor_is_renderable(
            &tab,
            &projects,
            first_project,
            false
        ));
        assert!(!rename_editor_is_renderable(
            &tab,
            &projects,
            projects.last().unwrap().id,
            false
        ));
        assert!(begin_rename_editor(&projects, RenameTarget::Tab(second_tab)).is_ok());
        assert!(begin_rename_editor(&projects, RenameTarget::Project(0)).is_err());
        assert!(begin_rename_editor(&projects, RenameTarget::Tab(i64::MAX)).is_err());
    }

    #[test]
    fn rename_completion_keys_are_consumed_through_release_only() {
        use iced::keyboard::key::{Code, Physical};
        use iced::keyboard::Location;

        let press = |named, code, repeat| keyboard::Event::KeyPressed {
            key: Key::Named(named),
            modified_key: Key::Named(named),
            physical_key: Physical::Code(code),
            location: Location::Standard,
            modifiers: keyboard::Modifiers::default(),
            text: None,
            repeat,
        };
        let release = |named, code| keyboard::Event::KeyReleased {
            key: Key::Named(named),
            modified_key: Key::Named(named),
            physical_key: Physical::Code(code),
            location: Location::Standard,
            modifiers: keyboard::Modifiers::default(),
        };

        let mut pending = Some(RenameCompletionKey::Enter);
        assert!(consume_rename_completion_key(
            &mut pending,
            &press(Named::Enter, Code::Enter, true)
        ));
        assert_eq!(pending, Some(RenameCompletionKey::Enter));
        assert!(!consume_rename_completion_key(
            &mut pending,
            &press(Named::ArrowDown, Code::ArrowDown, false)
        ));
        assert!(consume_rename_completion_key(
            &mut pending,
            &release(Named::Enter, Code::Enter)
        ));
        assert_eq!(pending, None);
        assert!(!consume_rename_completion_key(
            &mut pending,
            &press(Named::Enter, Code::Enter, false)
        ));

        pending = Some(RenameCompletionKey::Escape);
        assert!(consume_rename_completion_key(
            &mut pending,
            &press(Named::Escape, Code::Escape, true)
        ));
        assert!(consume_rename_completion_key(
            &mut pending,
            &release(Named::Escape, Code::Escape)
        ));
        assert_eq!(pending, None);
    }

    #[test]
    fn failed_rename_submit_dispatches_once_per_enter_press() {
        let mut editor = Some(RenameEditor {
            target: RenameTarget::Project(7),
            opened_label: "old".into(),
            draft: "recover me".into(),
        });
        let mut pending = None;
        let calls = std::cell::Cell::new(0);
        assert_eq!(
            submit_rename_editor_once_with(&mut editor, &mut pending, |_, _| {
                calls.set(calls.get() + 1);
                Err("injected failure".into())
            }),
            Err("injected failure".into())
        );
        assert_eq!(pending, Some(RenameCompletionKey::Enter));
        assert!(editor.is_some(), "failed command must retain the draft");
        assert_eq!(
            submit_rename_editor_once_with(&mut editor, &mut pending, |_, _| {
                calls.set(calls.get() + 1);
                Err("repeat must not dispatch".into())
            }),
            Ok(false)
        );
        assert_eq!(calls.get(), 1);

        // The captured key-release event clears this guard while the TextInput
        // remains focused. A later physical press may deliberately retry once.
        pending = None;
        assert!(
            submit_rename_editor_once_with(&mut editor, &mut pending, |_, _| {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .unwrap()
        );
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn held_palette_enter_cannot_submit_the_editor_it_opens() {
        let mut editor = Some(RenameEditor {
            target: RenameTarget::Tab(9),
            opened_label: "title".into(),
            draft: "title".into(),
        });
        let mut pending = None;
        arm_rename_completion_for_open_editor(&mut pending, editor.is_some());
        let calls = std::cell::Cell::new(0);
        assert_eq!(
            submit_rename_editor_once_with(&mut editor, &mut pending, |_, _| {
                calls.set(calls.get() + 1);
                Ok(())
            }),
            Ok(false)
        );
        assert_eq!(calls.get(), 0);
        assert!(editor.is_some());

        pending = None; // captured release from the palette-confirming Enter
        assert!(
            submit_rename_editor_once_with(&mut editor, &mut pending, |_, _| {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .unwrap()
        );
        assert_eq!(calls.get(), 1);
        assert!(editor.is_none());
    }

    #[test]
    fn rename_submit_trims_dispatches_exact_target_and_is_idempotent() {
        let mut editor = Some(RenameEditor {
            target: RenameTarget::Tab(42),
            opened_label: "old".into(),
            draft: "  new  title  ".into(),
        });
        let calls = std::cell::RefCell::new(Vec::new());
        assert_eq!(
            submit_rename_editor_with(&mut editor, |target, label| {
                calls.borrow_mut().push((target, label.to_string()));
                Ok(())
            }),
            Ok(true)
        );
        assert_eq!(
            submit_rename_editor_with(&mut editor, |target, label| {
                calls.borrow_mut().push((target, label.to_string()));
                Ok(())
            }),
            Ok(false),
            "a queued second on_submit must be a no-op"
        );
        assert_eq!(
            calls.into_inner(),
            [(RenameTarget::Tab(42), "new  title".to_string())]
        );
    }

    #[test]
    fn empty_rename_never_dispatches_and_failure_keeps_the_draft() {
        let mut empty = Some(RenameEditor {
            target: RenameTarget::Project(7),
            opened_label: "old".into(),
            draft: " \t ".into(),
        });
        assert_eq!(
            submit_rename_editor_with(&mut empty, |_, _| panic!("empty rename dispatched")),
            Ok(false)
        );
        assert!(empty.is_none());

        let expected = RenameEditor {
            target: RenameTarget::Project(7),
            opened_label: "old".into(),
            draft: "recover me".into(),
        };
        let mut failed = Some(expected.clone());
        assert_eq!(
            submit_rename_editor_with(&mut failed, |_, _| Err("injected failure".into())),
            Err("injected failure".into())
        );
        assert_eq!(failed, Some(expected));
    }

    #[test]
    fn concurrent_snapshot_rename_never_overwrites_the_draft() {
        let (mut projects, project_id, _, _) = rename_fixture();
        let mut editor = begin_rename_editor(&projects, RenameTarget::Project(project_id)).unwrap();
        editor.draft = "my draft".into();
        projects
            .iter_mut()
            .find(|project| project.id == project_id)
            .unwrap()
            .name = "remote name".into();
        assert!(rename_editor_is_renderable(
            &editor, &projects, project_id, false
        ));
        assert_eq!(editor.draft, "my draft");
        assert_eq!(
            rename_target_label(&projects, editor.target),
            Some("remote name")
        );
    }

    #[test]
    fn rename_focus_request_chains_focus_then_select_all_once() {
        let input_id = Id::unique();
        let mut requested = true;
        let UiTask::Then(first, second) =
            take_rename_focus_request(&mut requested, true, &input_id)
        else {
            panic!("rename begin must compose two widget operations")
        };
        assert!(matches!(*first, UiTask::FocusWidget(_)));
        assert!(matches!(*second, UiTask::SelectAllWidget(_)));
        assert!(!requested);
        assert!(matches!(
            take_rename_focus_request(&mut requested, true, &input_id),
            UiTask::None
        ));
        requested = true;
        assert!(matches!(
            take_rename_focus_request(&mut requested, false, &input_id),
            UiTask::None
        ));
        assert!(!requested, "hidden editor must clear stale focus work");
    }

    #[test]
    fn palette_focus_request_is_emitted_once_for_direct_open_actions() {
        let input_id = Id::unique();
        let mut requested = true;
        assert!(matches!(
            take_palette_focus_request(&mut requested, &input_id),
            UiTask::FocusWidget(_)
        ));
        assert!(!requested);
        assert!(matches!(
            take_palette_focus_request(&mut requested, &input_id),
            UiTask::None
        ));
    }

    #[test]
    fn failed_palette_restore_reclaims_focus_for_back_and_dismiss() {
        let mut requested = false;
        assert_eq!(
            retain_palette_focus_after_back(
                &mut requested,
                true,
                Err::<(), _>("injected restore failure".into())
            ),
            Err("injected restore failure".into())
        );
        assert!(requested);

        requested = true;
        assert_eq!(
            retain_palette_focus_after_back(&mut requested, false, Ok::<_, String>(())),
            Ok(())
        );
        assert!(!requested, "successful dismissal leaves no field to focus");
    }

    #[test]
    fn palette_title_runs_preserve_unicode_scalar_offsets_and_text() {
        let title = "Open 東京 terminal";
        let runs = palette_title_runs(title, &[5..7, 8..12]);
        assert_eq!(
            runs,
            vec![
                PaletteTextRun {
                    text: "Open ".into(),
                    matched: false,
                },
                PaletteTextRun {
                    text: "東京".into(),
                    matched: true,
                },
                PaletteTextRun {
                    text: " ".into(),
                    matched: false,
                },
                PaletteTextRun {
                    text: "term".into(),
                    matched: true,
                },
                PaletteTextRun {
                    text: "inal".into(),
                    matched: false,
                },
            ]
        );
        assert_eq!(
            runs.iter().map(|run| run.text.as_str()).collect::<String>(),
            title
        );
    }

    #[test]
    fn palette_title_runs_clamp_overlapping_and_out_of_bounds_ranges() {
        assert_eq!(
            palette_title_runs("abc", &[1..3, 2..usize::MAX]),
            vec![
                PaletteTextRun {
                    text: "a".into(),
                    matched: false,
                },
                PaletteTextRun {
                    text: "bc".into(),
                    matched: true,
                },
            ]
        );
        assert_eq!(
            palette_title_runs("", &[]),
            vec![PaletteTextRun {
                text: String::new(),
                matched: false,
            }]
        );
    }

    #[test]
    fn palette_agent_text_ellipsizes_on_unicode_scalar_boundaries() {
        assert_eq!(ellipsize_palette_text("short", 8), "short");
        assert_eq!(ellipsize_palette_text("abcdef", 4), "abc…");
        assert_eq!(ellipsize_palette_text("東京都市", 5), "東京…");
        assert_eq!(ellipsize_palette_text("anything", 0), "");
        assert!(UnicodeWidthStr::width(ellipsize_palette_text("東京都市", 5).as_str()) <= 5);
    }

    #[test]
    fn palette_agent_name_and_status_share_only_the_width_they_need() {
        let working = "Working through an intentionally long agent name";
        assert_eq!(
            palette_agent_left_text(working, "Working"),
            (working.to_string(), "Working".to_string())
        );

        let failed = "Failed · an intentionally long failure detail fo…";
        assert_eq!(
            palette_agent_left_text("Failed", failed),
            ("Failed".to_string(), failed.to_string())
        );

        let (name, status) = palette_agent_left_text(
            &"界".repeat(40),
            "Failed · an intentionally long failure detail",
        );
        assert!(name.ends_with('…'));
        assert!(status.ends_with('…'));
        assert!(
            UnicodeWidthStr::width(name.as_str()) + UnicodeWidthStr::width(status.as_str())
                <= PALETTE_AGENT_LEFT_MAX_COLUMNS
        );
    }

    #[test]
    fn ui_tasks_compose_without_overwriting_an_earlier_focus_request() {
        let task = UiTask::FocusWidget(Id::unique()).then(UiTask::Resize(
            window::Id::unique(),
            Size::new(800.0, 600.0),
        ));
        let UiTask::Then(first, second) = task else {
            panic!("both UI tasks must be retained");
        };
        assert!(matches!(*first, UiTask::FocusWidget(_)));
        assert!(matches!(*second, UiTask::Resize(_, _)));
    }

    #[test]
    fn screenshot_queue_retains_a_request_until_a_window_exists() {
        let mut queue = ScreenshotQueue::default();
        let (reply, mut result) = tokio::sync::oneshot::channel();
        queue.enqueue(1, reply);

        assert!(matches!(queue.start_next(None), UiTask::None));
        assert_eq!(queue.pending.len(), 1);
        assert!(queue.in_flight.is_none());
        assert!(matches!(
            result.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        let id = window::Id::unique();
        assert!(matches!(
            queue.start_next(Some(id)),
            UiTask::Screenshot(scheduled) if scheduled == id
        ));
        assert!(queue.pending.is_empty());
        assert!(queue.in_flight.is_some());
    }

    #[test]
    fn screenshot_queue_completes_in_fifo_order() {
        let mut queue = ScreenshotQueue::default();
        let (first_reply, mut first_result) = tokio::sync::oneshot::channel();
        let (second_reply, mut second_result) = tokio::sync::oneshot::channel();
        queue.enqueue(1, first_reply);
        queue.enqueue(2, second_reply);
        let id = window::Id::unique();

        assert!(matches!(queue.start_next(Some(id)), UiTask::Screenshot(_)));
        assert!(matches!(queue.start_next(Some(id)), UiTask::None));
        let first = queue.complete().expect("first capture must be active");
        assert_eq!(first.scale, 1);
        let _ = first.reply.send(Ok((Vec::new(), 1, 1)));
        assert!(first_result.try_recv().unwrap().is_ok());
        assert!(matches!(
            second_result.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        assert!(matches!(queue.start_next(Some(id)), UiTask::Screenshot(_)));
        let second = queue.complete().expect("second capture must be active");
        assert_eq!(second.scale, 2);
        let _ = second.reply.send(Ok((Vec::new(), 2, 2)));
        assert!(second_result.try_recv().unwrap().is_ok());
        assert!(queue.pending.is_empty());
        assert!(queue.in_flight.is_none());
    }

    #[test]
    fn screenshot_queue_drop_closes_pending_and_active_callers() {
        let (active_reply, mut active_result) = tokio::sync::oneshot::channel();
        let (pending_reply, mut pending_result) = tokio::sync::oneshot::channel();
        {
            let mut queue = ScreenshotQueue::default();
            queue.enqueue(1, active_reply);
            queue.enqueue(2, pending_reply);
            assert!(matches!(
                queue.start_next(Some(window::Id::unique())),
                UiTask::Screenshot(_)
            ));
        }
        assert!(matches!(
            active_result.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
        assert!(matches!(
            pending_result.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
    }

    #[test]
    fn window_open_tasks_keep_resize_bookkeeping_separate_from_screenshot_work() {
        let id = window::Id::unique();
        let retained_size = Size::new(820.0, 520.0);
        let mut window_id = None;
        let mut pending_resize = Some(retained_size);
        let mut screenshots = ScreenshotQueue::default();
        let (reply, _result) = tokio::sync::oneshot::channel();
        screenshots.enqueue(1, reply);

        let opened =
            prepare_window_opened(&mut window_id, &mut pending_resize, &mut screenshots, id);
        assert!(opened.retained_resize_scheduled);
        let UiTask::Then(first, second) = opened.task else {
            panic!("retained resize and screenshot must both be scheduled");
        };
        assert!(matches!(
            *first,
            UiTask::Resize(scheduled, size) if scheduled == id && size == retained_size
        ));
        assert!(matches!(*second, UiTask::Screenshot(scheduled) if scheduled == id));

        // A later open/focus/resize notification cannot duplicate the active
        // capture and has no retained resize to suppress native geometry.
        let reopened =
            prepare_window_opened(&mut window_id, &mut pending_resize, &mut screenshots, id);
        assert!(!reopened.retained_resize_scheduled);
        assert!(matches!(reopened.task, UiTask::None));

        let mut screenshot_only = ScreenshotQueue::default();
        let (reply, _result) = tokio::sync::oneshot::channel();
        screenshot_only.enqueue(1, reply);
        let opened = prepare_window_opened(
            &mut window_id,
            &mut pending_resize,
            &mut screenshot_only,
            id,
        );
        assert!(!opened.retained_resize_scheduled);
        assert!(matches!(opened.task, UiTask::Screenshot(scheduled) if scheduled == id));
    }

    #[test]
    fn clipboard_queue_serializes_two_writes_before_a_read() {
        let mut queue = ClipboardQueue::default();
        let first_id = queue.enqueue_write(ClipboardOp::System, "A".into());
        assert!(matches!(
            queue.start_next(),
            UiTask::ClipboardWrite {
                request_id,
                target: ClipboardOp::System,
                ref text,
            } if request_id == first_id && text == "A"
        ));

        // Model requests arriving on a later event-loop tick while the first
        // native write is still active. They must queue behind it rather than
        // starting concurrently.
        let second_id = queue.enqueue_write(ClipboardOp::System, "B".into());
        let (reply, mut result) = tokio::sync::oneshot::channel();
        let read_id = queue.enqueue_ipc_read(ClipboardOp::System, reply);
        assert!(matches!(queue.start_next(), UiTask::None));
        assert!(queue.complete_write(first_id));
        assert!(matches!(
            queue.start_next(),
            UiTask::ClipboardWrite {
                request_id,
                target: ClipboardOp::System,
                ref text,
            } if request_id == second_id && text == "B"
        ));
        assert!(queue.complete_write(second_id));
        assert!(matches!(
            queue.start_next(),
            UiTask::ClipboardRead {
                request_id,
                target: ClipboardOp::System,
            } if request_id == read_id
        ));
        deliver_ipc(
            queue
                .complete_read(read_id, Some("B".into()))
                .expect("active read completion"),
        );
        assert_eq!(result.try_recv().unwrap().unwrap().as_deref(), Some("B"));
    }

    #[test]
    fn clipboard_read_results_are_request_scoped_and_single_consumption() {
        let mut queue = ClipboardQueue::default();
        let (first_reply, mut first_result) = tokio::sync::oneshot::channel();
        let (second_reply, mut second_result) = tokio::sync::oneshot::channel();
        let first_id = queue.enqueue_ipc_read(ClipboardOp::System, first_reply);
        let second_id = queue.enqueue_ipc_read(ClipboardOp::Selection, second_reply);

        assert!(matches!(
            queue.start_next(),
            UiTask::ClipboardRead { request_id, .. } if request_id == first_id
        ));
        assert!(queue
            .complete_read(second_id, Some("early".into()))
            .is_none());
        assert!(queue
            .complete_read(u64::MAX, Some("unknown".into()))
            .is_none());
        assert!(matches!(
            first_result.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            second_result.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        deliver_ipc(
            queue
                .complete_read(first_id, Some("first".into()))
                .expect("first completion"),
        );
        assert_eq!(
            first_result.try_recv().unwrap().unwrap().as_deref(),
            Some("first")
        );
        assert!(matches!(
            queue.start_next(),
            UiTask::ClipboardRead { request_id, .. } if request_id == second_id
        ));
        assert!(queue
            .complete_read(first_id, Some("duplicate".into()))
            .is_none());
        deliver_ipc(
            queue
                .complete_read(second_id, Some("second".into()))
                .expect("second completion"),
        );
        assert_eq!(
            second_result.try_recv().unwrap().unwrap().as_deref(),
            Some("second")
        );
    }

    #[test]
    fn clipboard_request_ids_skip_live_entries_after_wrap() {
        let mut queue = ClipboardQueue::default();
        let (reply, _result) = tokio::sync::oneshot::channel();
        assert_eq!(queue.enqueue_ipc_read(ClipboardOp::System, reply), 1);
        queue.next_request_id = u64::MAX;
        assert_eq!(queue.enqueue_write(ClipboardOp::System, "next".into()), 2);
    }

    #[test]
    fn clipboard_queue_tolerates_caller_cancellation_and_closes_on_drop() {
        let mut queue = ClipboardQueue::default();
        let (cancelled_reply, cancelled_result) = tokio::sync::oneshot::channel();
        let cancelled_id = queue.enqueue_ipc_read(ClipboardOp::System, cancelled_reply);
        drop(cancelled_result);
        assert!(matches!(
            queue.start_next(),
            UiTask::ClipboardRead { request_id, .. } if request_id == cancelled_id
        ));
        deliver_ipc(
            queue
                .complete_read(cancelled_id, Some("ignored".into()))
                .expect("cancelled caller still has a scoped completion"),
        );

        let (pending_reply, pending_result) = tokio::sync::oneshot::channel();
        queue.enqueue_ipc_read(ClipboardOp::Selection, pending_reply);
        drop(queue);
        assert!(pending_result.blocking_recv().is_err());
    }

    #[test]
    fn paste_reads_keep_the_initiating_tab_and_reject_stale_results() {
        let mut queue = ClipboardQueue::default();
        let first_id = queue.enqueue_paste_read(ClipboardOp::System, 41);
        let second_id = queue.enqueue_paste_read(ClipboardOp::Selection, 42);
        assert!(matches!(
            queue.start_next(),
            UiTask::ClipboardRead { request_id, target: ClipboardOp::System }
                if request_id == first_id
        ));
        assert!(queue
            .complete_read(second_id, Some("wrong".into()))
            .is_none());
        let completion = queue
            .complete_read(first_id, Some("first".into()))
            .expect("active paste completion");
        assert!(matches!(
            completion,
            ClipboardReadCompletion::Paste { tab_id: 41, value: Some(ref value) }
                if value == "first"
        ));
        assert!(matches!(
            queue.start_next(),
            UiTask::ClipboardRead { request_id, target: ClipboardOp::Selection }
                if request_id == second_id
        ));
    }

    #[test]
    fn selection_copy_policy_orders_system_before_best_effort_primary() {
        let mut explicit = ClipboardQueue::default();
        assert_eq!(
            enqueue_selection_copy(&mut explicit, CopyKind::Explicit, Some("copy".into())),
            2
        );
        let first_id = match explicit.start_next() {
            UiTask::ClipboardWrite {
                request_id,
                target: ClipboardOp::System,
                ref text,
            } if text == "copy" => request_id,
            _ => panic!("explicit copy must write the system clipboard first"),
        };
        assert!(explicit.complete_write(first_id));
        assert!(matches!(
            explicit.start_next(),
            UiTask::ClipboardWrite {
                target: ClipboardOp::Selection,
                ref text,
                ..
            } if text == "copy"
        ));

        let mut primary_only = ClipboardQueue::default();
        assert_eq!(
            enqueue_selection_copy(
                &mut primary_only,
                CopyKind::OnSelect(config::CopyOnSelect::True),
                Some("selected".into()),
            ),
            1
        );
        assert!(matches!(
            primary_only.start_next(),
            UiTask::ClipboardWrite {
                target: ClipboardOp::Selection,
                ..
            }
        ));

        let mut disabled = ClipboardQueue::default();
        assert_eq!(
            enqueue_selection_copy(
                &mut disabled,
                CopyKind::OnSelect(config::CopyOnSelect::Off),
                Some("ignored".into()),
            ),
            0
        );
        assert!(matches!(disabled.start_next(), UiTask::None));
        assert_eq!(
            enqueue_selection_copy(&mut disabled, CopyKind::Explicit, Some(String::new())),
            0
        );
    }

    #[test]
    fn paste_bytes_are_empty_plain_or_bracketed_exactly_once() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 100,
        })
        .expect("terminal");
        assert!(paste_bytes(&terminal, None).is_empty());
        assert!(paste_bytes(&terminal, Some("")).is_empty());
        assert_eq!(paste_bytes(&terminal, Some("hello\n")), b"hello\n");

        terminal.vt_write(b"\x1b[?2004h");
        assert_eq!(
            paste_bytes(&terminal, Some("hello\n")),
            b"\x1b[200~hello\n\x1b[201~"
        );
    }

    #[test]
    fn pointer_origin_routing_never_substitutes_the_active_or_a_closed_tab() {
        let mut tabs = HashMap::from([(41, "origin"), (42, "active")]);
        assert_eq!(
            pointer_origin_tab(&mut tabs, 41).map(|value| *value),
            Some("origin")
        );
        tabs.remove(&41);
        assert_eq!(pointer_origin_tab(&mut tabs, 41), None);
        assert_eq!(
            pointer_origin_tab(&mut tabs, 42).map(|value| *value),
            Some("active")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tracked_left_gesture_never_completes_a_local_selection() {
        let (mut tab, supervisor) = attached_test_terminal(91);
        tab.write_vt(b"\x1b[?1002h\x1b[?1006h");

        let press = native_pointer(
            &mut tab,
            PointerAction::Press,
            Some(PointerButton::Left),
            (2, 2),
            1,
            true,
            false,
        );
        assert_eq!(press, NativePointerOutcome::default());
        assert_eq!(tab.tracking_pointer, Some(PointerButton::Left));
        assert_eq!(tab.local_pointer_gesture, None);

        let motion = native_pointer(
            &mut tab,
            PointerAction::Motion,
            Some(PointerButton::Left),
            (5, 2),
            0,
            false,
            false,
        );
        assert_eq!(motion, NativePointerOutcome::default());
        let release = native_pointer(
            &mut tab,
            PointerAction::Release,
            Some(PointerButton::Left),
            (5, 2),
            0,
            false,
            false,
        );
        assert_eq!(release, NativePointerOutcome::default());
        assert_eq!(tab.tracking_pointer, None);
        assert_eq!(tab.local_pointer_gesture, None);

        let captured = tab.input_capture.as_ref().unwrap().lock().unwrap().clone();
        assert!(captured.windows(3).any(|bytes| bytes == b"\x1b[<"));
        supervisor.close(91);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn geometry_transaction_defers_in_band_reports_until_commit() {
        let (mut tab, supervisor) = attached_test_terminal(92);
        tab.write_vt(b"\x1b[?2048h");
        tab.input_capture.as_ref().unwrap().lock().unwrap().clear();

        let larger = TerminalMetrics::measure(14.0).expect("larger metrics");
        let change = tab
            .apply_geometry(92, 29, larger, 2)
            .expect("stage candidate geometry")
            .expect("candidate changes geometry");
        assert!(
            tab.input_capture
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .is_empty(),
            "libghostty size reports must remain internal until batch commit"
        );
        tab.commit_geometry(change);
        assert!(
            !tab.input_capture
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .is_empty(),
            "successful commit delivers the staged in-band report"
        );

        tab.input_capture.as_ref().unwrap().lock().unwrap().clear();
        let smaller = TerminalMetrics::measure(13.0).expect("smaller metrics");
        let change = tab
            .apply_geometry(100, 32, smaller, 3)
            .expect("stage rollback candidate")
            .expect("rollback candidate changes geometry");
        tab.rollback_geometry(change.previous.expect("installed prior geometry"))
            .expect("rollback candidate geometry");
        assert!(
            tab.input_capture
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .is_empty(),
            "candidate and rollback reports must not escape a failed transaction"
        );
        supervisor.close(92);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn metric_change_stages_mouse_release_while_resize_and_rollback_preserve_tracking() {
        let (mut tab, supervisor) = attached_test_terminal(93);
        tab.write_vt(b"\x1b[?1002h\x1b[?1006h");
        let original = tab.applied_metrics.expect("installed metrics");
        native_pointer(
            &mut tab,
            PointerAction::Press,
            Some(PointerButton::Left),
            (2, 2),
            1,
            true,
            false,
        );
        tab.input_capture.as_ref().unwrap().lock().unwrap().clear();

        let grid_change = tab
            .apply_geometry(99, 31, original, 1)
            .expect("resize grid")
            .expect("grid changed");
        assert_eq!(tab.tracking_pointer, Some(PointerButton::Left));
        tab.commit_geometry(grid_change);
        native_pointer(
            &mut tab,
            PointerAction::Release,
            Some(PointerButton::Left),
            (2, 2),
            0,
            true,
            false,
        );
        assert_eq!(tab.tracking_pointer, None);
        assert_eq!(
            tab.input_capture.as_ref().unwrap().lock().unwrap().last(),
            Some(&b'm'),
            "the native release reaches the tracked application after grid-only resize"
        );

        native_pointer(
            &mut tab,
            PointerAction::Press,
            Some(PointerButton::Left),
            (3, 3),
            1,
            true,
            false,
        );
        tab.input_capture.as_ref().unwrap().lock().unwrap().clear();
        let larger = TerminalMetrics::measure(14.0).expect("larger metrics");
        let metric_change = tab
            .apply_geometry(92, 28, larger, 2)
            .expect("apply metric change")
            .expect("metrics changed");
        assert_eq!(tab.tracking_pointer, Some(PointerButton::Left));
        assert!(tab
            .input_capture
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .is_empty());
        let release = tab.prepare_pointer_cancel().expect("stage tracked release");
        assert_eq!(tab.tracking_pointer, Some(PointerButton::Left));
        assert!(tab
            .input_capture
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .is_empty());
        tab.commit_pointer_cancel(release);
        assert_eq!(tab.tracking_pointer, None);
        assert_eq!(
            tab.input_capture.as_ref().unwrap().lock().unwrap().last(),
            Some(&b'm'),
            "committed metric replacement sends its staged release"
        );
        tab.commit_geometry(metric_change);

        native_pointer(
            &mut tab,
            PointerAction::Press,
            Some(PointerButton::Left),
            (4, 4),
            1,
            true,
            false,
        );
        tab.input_capture.as_ref().unwrap().lock().unwrap().clear();
        let rollback = tab
            .apply_geometry(99, 31, original, 3)
            .expect("stage failed metric candidate")
            .expect("rollback candidate changes metrics");
        tab.rollback_geometry(rollback.previous.expect("prior geometry"))
            .expect("roll back failed metric candidate");
        assert_eq!(tab.tracking_pointer, Some(PointerButton::Left));
        assert!(
            tab.input_capture
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .is_empty(),
            "failed metric transition must not release or clear mouse ownership"
        );
        native_pointer(
            &mut tab,
            PointerAction::Release,
            Some(PointerButton::Left),
            (4, 4),
            0,
            true,
            false,
        );
        assert_eq!(tab.tracking_pointer, None);
        assert_eq!(
            tab.input_capture.as_ref().unwrap().lock().unwrap().last(),
            Some(&b'm')
        );
        supervisor.close(93);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wheel_routes_history_tracking_and_alternate_screen_through_shared_policy() {
        let (mut local, local_supervisor) = attached_test_terminal(191);
        for index in 0..48 {
            local.write_vt(format!("history-{index:02}\r\n").as_bytes());
        }
        local
            .handle_wheel(2.0, 3, 1, 0)
            .expect("local history wheel");
        assert!(local.scroll.is_scrolled_back());
        assert!(local
            .snap_to_bottom_for_input()
            .expect("snap local history"));
        assert!(!local.scroll.is_scrolled_back());
        local_supervisor.close(191);

        let (mut tracked, tracked_supervisor) = attached_test_terminal(192);
        tracked.write_vt(b"\x1b[?1000h\x1b[?1006h");
        tracked
            .input_capture
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .clear();
        tracked.handle_wheel(2.0, 3, 1, 0).expect("tracked wheel");
        let captured = tracked
            .input_capture
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .clone();
        assert_eq!(captured.iter().filter(|byte| **byte == b'M').count(), 2);
        assert!(captured.windows(5).any(|bytes| bytes == b"\x1b[<64"));
        tracked_supervisor.close(192);

        let (mut alternate, alternate_supervisor) = attached_test_terminal(193);
        alternate.write_vt(b"\x1b[?1049h");
        alternate
            .input_capture
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .clear();
        alternate
            .handle_wheel(2.0, 3, 1, 0)
            .expect("alternate-screen wheel");
        let captured = alternate
            .input_capture
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .clone();
        assert_eq!(captured, b"\x1b[A\x1b[A");
        alternate_supervisor.close(193);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tracked_middle_gesture_never_falls_through_to_primary_paste() {
        let (mut tab, supervisor) = attached_test_terminal(92);
        tab.write_vt(b"\x1b[?1000h\x1b[?1006h");

        let press = native_pointer(
            &mut tab,
            PointerAction::Press,
            Some(PointerButton::Middle),
            (3, 3),
            1,
            true,
            false,
        );
        assert_eq!(press, NativePointerOutcome::default());
        assert_eq!(tab.tracking_pointer, Some(PointerButton::Middle));
        let release = native_pointer(
            &mut tab,
            PointerAction::Release,
            Some(PointerButton::Middle),
            (3, 3),
            0,
            true,
            false,
        );
        assert_eq!(release, NativePointerOutcome::default());
        assert_eq!(tab.tracking_pointer, None);

        tab.write_vt(b"\x1b[?1000l");
        let local = native_pointer(
            &mut tab,
            PointerAction::Press,
            Some(PointerButton::Middle),
            (3, 3),
            1,
            true,
            false,
        );
        assert_eq!(
            local,
            NativePointerOutcome {
                selection_completed: false,
                paste_selection: true,
                open_url: None,
            }
        );
        supervisor.close(92);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_multi_click_expands_once_and_survives_outside_release() {
        let (mut tab, supervisor) = attached_test_terminal(93);
        tab.write_vt(b"\x1b[2J\x1b[Halpha/beta rest");

        let double = native_pointer(
            &mut tab,
            PointerAction::Press,
            Some(PointerButton::Left),
            (2, 0),
            2,
            true,
            false,
        );
        assert!(double.selection_completed);
        assert_eq!(
            tab.local_pointer_gesture,
            Some(LocalPointerGesture::MultiClick)
        );
        assert_eq!(tab.selected_text().unwrap().as_deref(), Some("alpha/beta"));

        assert_eq!(
            native_pointer(
                &mut tab,
                PointerAction::Motion,
                Some(PointerButton::Left),
                (40, 8),
                0,
                false,
                false,
            ),
            NativePointerOutcome::default()
        );
        assert_eq!(
            native_pointer(
                &mut tab,
                PointerAction::Release,
                Some(PointerButton::Left),
                (40, 8),
                0,
                false,
                false,
            ),
            NativePointerOutcome::default()
        );
        assert_eq!(tab.selected_text().unwrap().as_deref(), Some("alpha/beta"));

        let triple = native_pointer(
            &mut tab,
            PointerAction::Press,
            Some(PointerButton::Left),
            (0, 0),
            3,
            true,
            false,
        );
        assert!(triple.selection_completed);
        assert_eq!(
            tab.selected_text().unwrap().as_deref(),
            Some("alpha/beta rest")
        );
        supervisor.close(93);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn configured_word_characters_and_whitespace_fallback_are_native() {
        let (mut tab, supervisor) = attached_test_terminal(94);
        tab.word_break_chars = "_".into();
        tab.write_vt(b"\x1b[2J\x1b[Hone-two  next");

        let word = native_pointer(
            &mut tab,
            PointerAction::Press,
            Some(PointerButton::Left),
            (1, 0),
            2,
            true,
            false,
        );
        assert!(word.selection_completed);
        assert_eq!(tab.selected_text().unwrap().as_deref(), Some("one"));
        let _ = native_pointer(
            &mut tab,
            PointerAction::Release,
            Some(PointerButton::Left),
            (1, 0),
            0,
            true,
            false,
        );

        let whitespace = native_pointer(
            &mut tab,
            PointerAction::Press,
            Some(PointerButton::Left),
            (7, 0),
            2,
            true,
            false,
        );
        assert_eq!(whitespace, NativePointerOutcome::default());
        assert_eq!(
            tab.local_pointer_gesture,
            Some(LocalPointerGesture::Selection)
        );
        supervisor.close(94);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_selection_drag_keeps_capture_outside_until_release() {
        let (mut tab, supervisor) = attached_test_terminal(95);
        tab.write_vt(b"\x1b[2J\x1b[Houtside");
        let _ = native_pointer(
            &mut tab,
            PointerAction::Press,
            Some(PointerButton::Left),
            (0, 0),
            1,
            true,
            false,
        );
        let _ = native_pointer(
            &mut tab,
            PointerAction::Motion,
            Some(PointerButton::Left),
            (6, 0),
            0,
            false,
            false,
        );
        let release = native_pointer(
            &mut tab,
            PointerAction::Release,
            Some(PointerButton::Left),
            (6, 0),
            0,
            false,
            false,
        );
        assert!(release.selection_completed);
        assert_eq!(tab.selected_text().unwrap().as_deref(), Some("outside"));
        supervisor.close(95);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn regex_and_osc8_links_override_tracking_and_preserve_selection() {
        let (mut tab, supervisor) = attached_test_terminal(96);
        tab.write_vt(b"\x1b[2J\x1b[Hkeep https://visible.test");
        assert!(tab.selection.set(&tab.terminal, (0, 0), (3, 0)));
        tab.write_vt(b"\x1b[?1002h\x1b[?1006h");
        let opened = native_pointer(
            &mut tab,
            PointerAction::Press,
            Some(PointerButton::Left),
            (8, 0),
            1,
            true,
            true,
        );
        assert_eq!(opened.open_url.as_deref(), Some("https://visible.test"));
        assert_eq!(tab.tracking_pointer, None);
        assert_eq!(tab.selected_text().unwrap().as_deref(), Some("keep"));
        tab.set_link_modifier_held(false).unwrap();
        let release = native_pointer(
            &mut tab,
            PointerAction::Release,
            Some(PointerButton::Left),
            (8, 0),
            0,
            false,
            false,
        );
        assert_eq!(release, NativePointerOutcome::default());

        tab.write_vt(
            b"\x1b[?1002l\x1b[2J\x1b[H\x1b]8;;https://real.test\x1b\\https://shown.test\x1b]8;;\x1b\\",
        );
        let hover = native_pointer(&mut tab, PointerAction::Motion, None, (7, 0), 0, true, true);
        assert_eq!(hover, NativePointerOutcome::default());
        assert_eq!(
            tab.hover_url.as_ref().map(|hover| hover.url.as_str()),
            Some("https://real.test")
        );
        assert_eq!(
            tab.hover_url.as_ref().map(|hover| (hover.col0, hover.col1)),
            Some((0, 17))
        );

        let unicode_url = "https://wide.test/e\u{301}界";
        tab.write_vt(format!("\x1b[2J\x1b[H{unicode_url}").as_bytes());
        let _ = native_pointer(
            &mut tab,
            PointerAction::Motion,
            None,
            (20, 0),
            0,
            true,
            true,
        );
        assert_eq!(
            tab.hover_url.as_ref().map(|hover| hover.url.as_str()),
            Some(unicode_url)
        );
        assert_eq!(
            tab.hover_url.as_ref().map(|hover| (hover.col0, hover.col1)),
            Some((0, 20))
        );
        supervisor.close(96);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn modifier_only_hover_composes_with_osc_cursor_and_clears() {
        let (mut tab, supervisor) = attached_test_terminal(97);
        tab.pointer_shape = "crosshair".into();
        tab.write_vt(b"\x1b[2J\x1b[Hhttps://hover.test");
        let _ = native_pointer(
            &mut tab,
            PointerAction::Motion,
            None,
            (8, 0),
            0,
            true,
            false,
        );
        assert_eq!(tab.effective_pointer_shape(), "crosshair");
        tab.set_link_modifier_held(true).unwrap();
        assert_eq!(tab.effective_pointer_shape(), "pointer");
        tab.set_link_modifier_held(false).unwrap();
        assert_eq!(tab.effective_pointer_shape(), "crosshair");
        tab.set_link_modifier_held(true).unwrap();
        tab.pointer_leave();
        assert_eq!(tab.effective_pointer_shape(), "crosshair");

        let _ = native_pointer(&mut tab, PointerAction::Motion, None, (8, 0), 0, true, true);
        tab.write_vt(b"\x1b[2J\x1b[Hno link");
        tab.refresh_snapshot().unwrap();
        assert!(tab.hover_url.is_none());
        supervisor.close(97);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tab_replacement_clears_hover_and_captured_gestures() {
        let (mut tab, supervisor) = attached_test_terminal(98);
        tab.write_vt(b"\x1b[2J\x1b[Hhttps://hover.test");
        let _ = native_pointer(&mut tab, PointerAction::Motion, None, (8, 0), 0, true, true);
        assert!(tab.hover_url.is_some());

        let _ = native_pointer(
            &mut tab,
            PointerAction::Press,
            Some(PointerButton::Left),
            (8, 0),
            1,
            true,
            true,
        );
        assert_eq!(tab.local_pointer_gesture, Some(LocalPointerGesture::Url));
        assert!(tab.reset_pointer_state());
        assert!(tab.hover_url.is_none());
        assert_eq!(tab.local_pointer_gesture, None);
        assert_eq!(tab.tracking_pointer, None);
        assert_eq!(tab.last_pointer_cell, None);
        assert!(!tab.link_modifier_held);
        assert!(!tab.reset_pointer_state());
        supervisor.close(98);
    }

    #[test]
    fn configured_copy_can_replace_a_former_palette_trigger() {
        let bindings = keybind::canonicalize_bindings(
            keybind::default_bindings(),
            vec![
                ("alt+shift+p".into(), "copy".into()),
                ("ctrl+shift+v".into(), "unbind".into()),
                ("ctrl+alt+v".into(), "paste".into()),
            ],
            |_| {},
        );
        assert_eq!(
            bindings.get(&keybind::parse_trigger("alt+shift+p").unwrap()),
            Some(&KeybindAction::Copy)
        );
        assert!(!bindings.contains_key(&keybind::parse_trigger("ctrl+shift+v").unwrap()));
        assert_eq!(
            bindings.get(&keybind::parse_trigger("ctrl+alt+v").unwrap()),
            Some(&KeybindAction::Paste)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn command_v_resolves_through_native_queue_to_initiating_tab_bytes() {
        use iced::keyboard::key::{Code, Physical};
        use iced::keyboard::Location;

        let event = keyboard::Event::KeyPressed {
            key: Key::Character("v".into()),
            modified_key: Key::Character("v".into()),
            physical_key: Physical::Code(Code::KeyV),
            location: Location::Standard,
            modifiers: keyboard::Modifiers::LOGO,
            text: None,
            repeat: false,
        };
        let bindings =
            keybind::canonicalize_bindings(keybind::default_bindings(), Vec::new(), |_| {});
        let accelerator = input::accelerator(&event).expect("native Command-V accelerator");
        assert_eq!(bindings.get(&accelerator), Some(&KeybindAction::Paste));

        let mut queue = ClipboardQueue::default();
        let request_id = queue.enqueue_paste_read(ClipboardOp::System, 73);
        assert!(matches!(
            queue.start_next(),
            UiTask::ClipboardRead { request_id: scheduled, .. } if scheduled == request_id
        ));
        let completion = queue
            .complete_read(request_id, Some("mac paste".into()))
            .expect("native read completion");
        let ClipboardReadCompletion::Paste { tab_id, value } = completion else {
            panic!("expected initiating-tab paste completion")
        };
        assert_eq!(tab_id, 73);
        let terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 100,
        })
        .expect("terminal");
        assert_eq!(paste_bytes(&terminal, value.as_deref()), b"mac paste");
    }

    #[test]
    fn osc_clipboard_policy_maps_targets_and_drops_denied_writes() {
        let mut allowed_system = ClipboardQueue::default();
        assert!(enqueue_osc_clipboard_write(
            &mut allowed_system,
            config::ClipboardWrite::Allow,
            ClipboardTarget::System,
            "system".into(),
        ));
        assert!(matches!(
            allowed_system.start_next(),
            UiTask::ClipboardWrite {
                target: ClipboardOp::System,
                ref text,
                ..
            } if text == "system"
        ));

        let mut allowed = ClipboardQueue::default();
        assert!(enqueue_osc_clipboard_write(
            &mut allowed,
            config::ClipboardWrite::Allow,
            ClipboardTarget::Selection,
            "selection".into(),
        ));
        assert!(matches!(
            allowed.start_next(),
            UiTask::ClipboardWrite {
                target: ClipboardOp::Selection,
                ref text,
                ..
            } if text == "selection"
        ));

        let mut denied = ClipboardQueue::default();
        assert!(!enqueue_osc_clipboard_write(
            &mut denied,
            config::ClipboardWrite::Deny,
            ClipboardTarget::System,
            "denied".into(),
        ));
        assert!(matches!(denied.start_next(), UiTask::None));
    }

    #[test]
    fn palette_visibility_requests_keep_reveal_precedence() {
        assert_eq!(
            PaletteVisibilityRequest::Measure.merge(PaletteVisibilityRequest::Reveal),
            PaletteVisibilityRequest::Reveal
        );
        assert_eq!(
            PaletteVisibilityRequest::Reveal.merge(PaletteVisibilityRequest::Measure),
            PaletteVisibilityRequest::Reveal
        );
        assert_eq!(
            queue_visibility_request(
                PaletteVisibilityRequest::Reveal,
                PaletteVisibilityRequest::Measure,
                true,
            ),
            PaletteVisibilityRequest::Measure,
            "a later scroll replaces a reveal after structural reveal intent is satisfied"
        );

        let mut selected_in_view = Some(true);
        let mut retries = 7;
        let mut request = PaletteVisibilityRequest::Reveal;
        let mut measurement_generation = 12;
        queue_scroll_measurement(
            &mut selected_in_view,
            &mut retries,
            &mut request,
            &mut measurement_generation,
            false,
        );
        assert_eq!(selected_in_view, None);
        assert_eq!(retries, 0);
        assert_eq!(request, PaletteVisibilityRequest::Measure);
        assert_eq!(measurement_generation, 13);

        queue_scroll_measurement(
            &mut selected_in_view,
            &mut retries,
            &mut request,
            &mut measurement_generation,
            true,
        );
        assert_eq!(
            request,
            PaletteVisibilityRequest::Reveal,
            "layout scrolls cannot downgrade a reveal that has not succeeded"
        );
    }

    #[test]
    fn dynamic_content_only_changes_measure_without_revealing() {
        assert_eq!(
            dynamic_refresh_request(false, true),
            PaletteVisibilityRequest::Measure
        );
        assert_eq!(
            dynamic_refresh_request(true, true),
            PaletteVisibilityRequest::Reveal
        );
        assert_eq!(
            dynamic_refresh_request(false, false),
            PaletteVisibilityRequest::None
        );
    }

    #[test]
    fn palette_visibility_retries_to_the_named_limit() {
        assert_eq!(
            visibility_retry(0, true),
            Some((1, PaletteVisibilityRequest::Reveal))
        );
        assert_eq!(
            visibility_retry(PALETTE_GEOMETRY_RETRY_LIMIT - 1, false),
            Some((
                PALETTE_GEOMETRY_RETRY_LIMIT,
                PaletteVisibilityRequest::Measure
            ))
        );
        assert_eq!(visibility_retry(PALETTE_GEOMETRY_RETRY_LIMIT, true), None);
    }

    #[test]
    fn palette_visibility_results_require_the_same_identity_and_viewport() {
        assert!(visibility_result_is_current(4, 9, 2, 4, 9, 2));
        assert!(!visibility_result_is_current(4, 9, 2, 3, 9, 2));
        assert!(!visibility_result_is_current(4, 9, 2, 4, 8, 2));
        assert!(!visibility_result_is_current(4, 9, 2, 4, 9, 1));
        assert_ne!(palette_row_id(4, 9, 0), palette_row_id(4, 10, 0));
    }

    #[test]
    fn scroll_fences_in_flight_visible_and_missing_results() {
        let session = 4;
        let revision = 9;
        let issued_generation = 12;
        let mut current_generation = issued_generation;
        let mut selected_in_view = Some(true);
        let mut retries = 7;
        let mut request = PaletteVisibilityRequest::Reveal;

        queue_scroll_measurement(
            &mut selected_in_view,
            &mut retries,
            &mut request,
            &mut current_generation,
            false,
        );

        assert!(
            !visibility_result_is_current(
                session,
                revision,
                current_generation,
                session,
                revision,
                issued_generation,
            ),
            "a visible result measured before the scroll must be rejected"
        );
        assert_eq!(selected_in_view, None);

        if visibility_result_is_current(
            session,
            revision,
            current_generation,
            session,
            revision,
            issued_generation,
        ) {
            if let Some((next_retries, retry)) = visibility_retry(retries, true) {
                retries = next_retries;
                request = request.merge(retry);
            }
        }
        assert_eq!(retries, 0);
        assert_eq!(
            request,
            PaletteVisibilityRequest::Measure,
            "an older missing reveal must not supersede the post-scroll measurement"
        );
    }

    #[test]
    fn structural_reveal_survives_layout_scroll_until_geometry_is_visible() {
        let session = 4;
        let revision = 9;
        let issued_generation = 12;
        let mut current_generation = issued_generation;
        let mut selected_in_view = None;
        let mut retries = 0;
        let mut request = PaletteVisibilityRequest::None;

        queue_scroll_measurement(
            &mut selected_in_view,
            &mut retries,
            &mut request,
            &mut current_generation,
            true,
        );
        assert!(!visibility_result_is_current(
            session,
            revision,
            current_generation,
            session,
            revision,
            issued_generation,
        ));
        assert_eq!(
            request,
            PaletteVisibilityRequest::Reveal,
            "a structural reveal must remain pending after layout fences its old result"
        );

        // Once stable geometry is available, the pending request is still a
        // reveal rather than a measure that could finalize `Visible(false)`.
        let next = std::mem::take(&mut request);
        assert_eq!(next, PaletteVisibilityRequest::Reveal);
    }

    #[test]
    fn required_reveal_survives_content_only_measure_invalidation() {
        assert_eq!(
            queue_layout_visibility_request(
                PaletteVisibilityRequest::None,
                PaletteVisibilityRequest::Measure,
                false,
                true,
            ),
            PaletteVisibilityRequest::Reveal,
            "content refresh cannot downgrade an in-flight structural reveal"
        );
    }

    #[test]
    fn clipped_reveal_retries_are_bounded() {
        let mut selected_in_view = None;
        let mut retries = PALETTE_GEOMETRY_RETRY_LIMIT;
        let mut request = PaletteVisibilityRequest::None;
        let mut reveal_required = true;
        assert!(apply_visible_result(
            &mut selected_in_view,
            &mut retries,
            &mut request,
            &mut reveal_required,
            true,
            false,
        ));
        assert_eq!(selected_in_view, Some(false));
        assert_eq!(request, PaletteVisibilityRequest::None);
        assert!(!reveal_required);
    }

    #[test]
    fn scroll_fenced_reveals_cannot_bypass_the_scheduling_budget() {
        let mut attempts = 0;
        let mut selected_in_view = None;
        let mut retries = 4;
        let mut request = PaletteVisibilityRequest::Reveal;
        let mut generation = 1;
        let mut reveal_required = true;

        for _ in 0..PALETTE_GEOMETRY_RETRY_LIMIT {
            assert!(schedule_reveal_attempt(
                &mut attempts,
                &mut selected_in_view,
                &mut reveal_required,
            ));
            let issued_generation = generation;
            queue_scroll_measurement(
                &mut selected_in_view,
                &mut retries,
                &mut request,
                &mut generation,
                true,
            );
            assert!(!visibility_result_is_current(
                1,
                1,
                generation,
                1,
                1,
                issued_generation,
            ));
            assert_eq!(request, PaletteVisibilityRequest::Reveal);
            assert_eq!(retries, 4, "scrolls preserve the in-flight retry state");
        }
        assert!(!schedule_reveal_attempt(
            &mut attempts,
            &mut selected_in_view,
            &mut reveal_required,
        ));
        assert_eq!(
            selected_in_view, None,
            "missing or stale geometry cannot fabricate a clipped result"
        );
        assert!(!reveal_required);

        let mut measured_clipped = Some(false);
        let mut reveal_required = true;
        assert!(!schedule_reveal_attempt(
            &mut attempts,
            &mut measured_clipped,
            &mut reveal_required,
        ));
        assert_eq!(
            measured_clipped,
            Some(false),
            "scheduling exhaustion preserves a current clipped measurement"
        );
        assert!(!reveal_required);
    }

    #[test]
    fn agent_colors_match_the_shipped_gtk_and_appkit_palette() {
        use roost_ipc::agent::AgentLifecycle;
        assert_eq!(
            agent_color(AgentLifecycle::Working),
            Color::from_rgb8(0x5f, 0xa3, 0xf0)
        );
        assert_eq!(
            agent_color(AgentLifecycle::Waiting),
            Color::from_rgb8(0xf0, 0xa0, 0x40)
        );
        assert_eq!(
            agent_color(AgentLifecycle::Finished),
            Color::from_rgb8(0x7a, 0x7a, 0x7a)
        );
        assert_eq!(
            agent_color(AgentLifecycle::Failed),
            Color::from_rgb8(0xe0, 0x52, 0x52)
        );
        assert_eq!(
            tab_status_color(AgentLifecycle::Inactive),
            Color::TRANSPARENT,
            "inactive tabs reserve the status slot without painting a dot"
        );
        assert_eq!(
            tab_status_color(AgentLifecycle::Working),
            agent_color(AgentLifecycle::Working)
        );
    }

    #[test]
    fn command_palette_uses_shared_ids_and_ranking() {
        let config = RoostConfig::parse(r#"provider = label="Fixture" run="fixture.sh""#);
        let bindings = keybind::default_bindings().into_iter().collect();
        let mut state =
            palette::PaletteState::new(command_palette_frame(2, &config.providers, &bindings));
        let ids: Vec<String> = state
            .matches()
            .into_iter()
            .map(|matched| matched.item.id)
            .collect();
        assert!(ids.iter().any(|id| id == "new_tab"));
        let font = ids
            .iter()
            .position(|id| id == palette::PaletteCommands::SELECT_FONT_ID)
            .expect("font drill-in");
        assert_eq!(
            ids.get(font + 1).map(String::as_str),
            Some(palette::PaletteCommands::VIEW_AGENTS_ID)
        );
        assert_eq!(
            ids.get(font + 2).map(String::as_str),
            Some(palette::PaletteCommands::VIEW_NOTIFICATIONS_ID)
        );
        assert!(ids.iter().any(|id| id == "custom_commands"));
        assert!(state
            .current()
            .items
            .iter()
            .find(|item| item.id == "new_tab")
            .and_then(|item| item.trailing_text.as_deref())
            .is_some());
        state.set_query("theme");
        assert_eq!(
            state.selected_item().map(|item| item.id),
            Some(palette::PaletteCommands::SELECT_THEME_ID.to_string())
        );
    }

    #[test]
    fn theme_frame_selects_the_active_theme() {
        let frame = theme_palette_frame("roost-dark");
        assert_eq!(frame.id, "themes");
        assert!(frame.items.len() > 1);
        assert_eq!(frame.items[frame.selection].id, "roost-dark");
    }

    #[test]
    fn failed_apply_attempts_the_previous_value() {
        let mut applied = Vec::new();
        let failure = apply_with_rollback(&"previous", &"next", |value| {
            applied.push(*value);
            if *value == "next" {
                Err("injected apply failure")
            } else {
                Ok(())
            }
        })
        .expect_err("next value must fail");
        assert_eq!(applied, ["next", "previous"]);
        assert_eq!(
            failure,
            ApplyRollbackFailure {
                apply: "injected apply failure",
                rollback: None,
            }
        );
    }

    #[test]
    fn failed_theme_batch_rolls_back_already_applied_tabs() {
        let first = Theme::load_bundled("roost-dark");
        let second = Theme::load_bundled("Oxocarbon");
        let next = Theme::load_bundled("Atom");
        let targets = vec![(7, first.clone()), (11, second)];
        let mut applied = Vec::new();
        let failure = apply_theme_batch(&targets, &next, |tab_id, theme| {
            applied.push((tab_id, theme.background));
            if tab_id == 11 && theme.background == next.background {
                Err("injected tab failure".to_string())
            } else {
                Ok(())
            }
        })
        .expect_err("second tab must fail");
        assert_eq!(failure.tab_id, 11);
        assert_eq!(failure.apply, "injected tab failure");
        assert!(failure.rollback.is_empty());
        assert_eq!(
            applied,
            [
                (7, next.background),
                (11, next.background),
                (7, first.background),
            ]
        );
    }

    #[test]
    fn failed_font_geometry_batch_rolls_back_in_reverse_and_does_not_persist() {
        let original_metrics = TerminalMetrics::measure(13.0).expect("original metrics");
        let next_metrics = TerminalMetrics::measure(14.0).expect("next metrics");
        let original = TerminalGeometry {
            cols: 100,
            rows: 32,
            metrics: original_metrics,
            metric_generation: 4,
        };
        let mut states = HashMap::from([(7_i64, original), (11_i64, original)]);
        let mut operations = Vec::new();
        let mut persisted = false;

        let result =
            apply_geometry_batch(
                &[7, 11],
                92,
                29,
                next_metrics,
                5,
                |operation| match operation {
                    GeometryBatchOperation::Apply {
                        tab_id,
                        cols,
                        rows,
                        metrics,
                        metric_generation,
                    } => {
                        operations.push(format!("apply:{tab_id}"));
                        if tab_id == 11 {
                            return Err("injected tab failure".to_string());
                        }
                        let previous = states.insert(
                            tab_id,
                            TerminalGeometry {
                                cols,
                                rows,
                                metrics,
                                metric_generation,
                            },
                        );
                        Ok(Some(GeometryChange {
                            previous,
                            current: states[&tab_id],
                            grid_changed: true,
                            metrics_changed: true,
                            deferred_replies: Vec::new(),
                        }))
                    }
                    GeometryBatchOperation::Rollback { tab_id, previous } => {
                        operations.push(format!("rollback:{tab_id}"));
                        states.insert(tab_id, previous);
                        Ok(None)
                    }
                },
            );
        if result.is_ok() {
            persisted = true;
        }

        assert_eq!(
            result.expect_err("second tab must fail"),
            GeometryBatchFailure {
                tab_id: 11,
                apply: "injected tab failure".to_string(),
                rollback: Vec::new(),
            }
        );
        assert_eq!(operations, ["apply:7", "apply:11", "rollback:7"]);
        assert_eq!(states[&7], original);
        assert_eq!(states[&11], original);
        assert!(
            !persisted,
            "failed live application cannot reach persistence"
        );
    }

    #[test]
    fn font_size_candidate_is_atomic_when_reset_cannot_be_measured() {
        let mut current = TerminalTypography::new(None, Some(f64::MAX));
        assert_eq!(current.adjust_size(-1.0), Some(72.0));
        let before = current.clone();
        assert!(font_size_candidate(&current, Font::MONOSPACE, FontSizeTransition::Reset).is_err());
        assert_eq!(
            current, before,
            "candidate measurement cannot mutate live state"
        );

        let candidate =
            font_size_candidate(&current, Font::MONOSPACE, FontSizeTransition::Adjust(-1.0))
                .expect("measurable candidate")
                .expect("changed candidate");
        assert_eq!(candidate.0.current_size_pt(), 71.0);
        assert_eq!(current, before);
    }

    #[test]
    fn font_size_persistence_handles_absence_success_and_failure() {
        let mut absent = RoostConfig::default();
        persist_font_size_with(&mut absent, None, 14.0, |_, _, _| {
            panic!("absent path must not invoke the writer")
        })
        .expect("absent path is a silent success");
        assert_eq!(absent.font_size, Some(14.0));

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.conf");
        std::fs::write(&path, "# keep me\ntheme = roost-dark\n").expect("seed config");
        let mut successful = RoostConfig::default();
        persist_font_size_with(&mut successful, Some(&path), 14.5, config::set_key)
            .expect("persist font size");
        assert_eq!(successful.font_size, Some(14.5));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read persisted config"),
            "# keep me\ntheme = roost-dark\nfont-size = 14.5\n"
        );

        let before = std::fs::read(&path).expect("read before failure");
        let mut failed = RoostConfig::default();
        let error = persist_font_size_with(&mut failed, Some(&path), 15.0, |_, _, _| {
            Err(io::Error::other("injected writer failure"))
        })
        .expect_err("writer failure must be returned to the UI boundary");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(failed.font_size, Some(15.0));
        assert_eq!(std::fs::read(&path).expect("read after failure"), before);
    }

    #[test]
    fn font_family_persistence_handles_absence_reload_and_failure() {
        let mut absent = RoostConfig::default();
        persist_font_family_with(&mut absent, None, "JetBrains Mono", |_, _, _| {
            panic!("absent path must not invoke the writer")
        })
        .expect("absent path is a silent success");
        assert_eq!(absent.font_family.as_deref(), Some("JetBrains Mono"));

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.conf");
        std::fs::write(&path, "# keep me\nfont-size = 14\n").expect("seed config");
        let mut successful = RoostConfig::default();
        persist_font_family_with(
            &mut successful,
            Some(&path),
            "JetBrains Mono",
            config::set_key,
        )
        .expect("persist font family");
        assert_eq!(successful.font_family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read persisted config"),
            "# keep me\nfont-size = 14\nfont-family = \"JetBrains Mono\"\n"
        );
        assert_eq!(
            RoostConfig::load_from(&path).font_family.as_deref(),
            Some("JetBrains Mono"),
            "the next bootstrap observes the exact committed family"
        );

        let before = std::fs::read(&path).expect("read before failure");
        let mut failed = RoostConfig::default();
        let error = persist_font_family_with(&mut failed, Some(&path), "SF Mono", |_, _, _| {
            Err(io::Error::other("injected writer failure"))
        })
        .expect_err("writer failure must be returned to the UI boundary");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(failed.font_family.as_deref(), Some("SF Mono"));
        assert_eq!(std::fs::read(&path).expect("read after failure"), before);
    }

    #[test]
    fn theme_persistence_handles_absence_success_and_failure() {
        let mut absent = RoostConfig::default();
        persist_theme_selection_with(&mut absent, None, "Oxocarbon", |_, _, _| {
            panic!("absent path must not invoke the writer")
        })
        .expect("absent path is a silent success");
        assert_eq!(absent.theme_name.as_deref(), Some("Oxocarbon"));

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.conf");
        std::fs::write(&path, "# keep me\nfont-size = 14\n").expect("seed config");
        let mut successful = RoostConfig::default();
        persist_theme_selection_with(&mut successful, Some(&path), "Oxocarbon", config::set_key)
            .expect("persist theme");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read persisted config"),
            "# keep me\nfont-size = 14\ntheme = Oxocarbon\n"
        );
        assert_eq!(
            RoostConfig::load_from(&path).theme_name.as_deref(),
            Some("Oxocarbon"),
            "the next bootstrap observes the committed theme"
        );

        let before = std::fs::read(&path).expect("read before failure");
        let mut failed = RoostConfig::default();
        let error = persist_theme_selection_with(&mut failed, Some(&path), "Atom", |_, _, _| {
            Err(io::Error::other("injected writer failure"))
        })
        .expect_err("writer failure must be returned to the UI boundary");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(failed.theme_name.as_deref(), Some("Atom"));
        assert_eq!(std::fs::read(&path).expect("read after failure"), before);
    }

    #[test]
    fn theme_persistence_error_closes_before_status_and_cannot_revert() {
        let mut palette = Some(palette::PaletteState::new(theme_palette_frame(
            "roost-dark",
        )));
        let mut theme_at_open = Some("roost-dark".to_string());
        let mut status = StatusBanner::default();
        let now = Instant::now();
        finish_theme_confirmation(
            &mut palette,
            &mut theme_at_open,
            &mut status,
            Some("persist theme: injected writer failure".to_string()),
            now,
        );
        assert!(palette.is_none());
        assert!(theme_at_open.is_none(), "dismiss cannot revert the commit");
        assert_eq!(
            status.message(),
            Some("persist theme: injected writer failure")
        );
        assert_eq!(status.expires_at, Some(now + STATUS_BANNER_DURATION));
    }

    #[test]
    fn launcher_frame_resolves_configured_command_ids() {
        let config =
            RoostConfig::parse(r#"command = label="Echo Marker" run="printf marker" hold=true"#);
        let state = palette::PaletteState::new(launcher_palette_frame(&config));
        let item = state.selected_item().expect("configured launcher row");
        assert_eq!(item.id, "launch:0");
        assert_eq!(custom_command::launch_index(&item.id), Some(0));
    }

    #[test]
    fn provider_frame_uses_shared_provider_ids() {
        let config = RoostConfig::parse(r#"provider = label="Fixture" run="fixture.sh""#);
        let state = palette::PaletteState::new(provider_palette_frame(&config.providers));
        let item = state.selected_item().expect("provider row");
        assert_eq!(item.id, "provider:0");
        assert_eq!(provider::provider_index(&item.id), Some(0));
    }
}
