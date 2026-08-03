use super::*;

// ── rename ──

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RenameTarget {
    Project(i64),
    Tab(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RenameCompletionKey {
    Enter,
    Escape,
}

pub(super) fn consume_rename_completion_key(
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
pub(super) struct RenameEditor {
    pub(super) target: RenameTarget,
    pub(super) opened_label: String,
    pub(super) draft: String,
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

pub(super) fn arm_rename_completion_for_open_editor(
    pending: &mut Option<RenameCompletionKey>,
    editor_open: bool,
) {
    if editor_open {
        *pending = Some(RenameCompletionKey::Enter);
    }
}

fn take_rename_focus_request(requested: &mut bool, editor_open: bool, input_id: &Id) -> UiTask {
    if std::mem::take(requested) && editor_open {
        UiTask::FocusWidget(input_id.clone()).then(UiTask::SelectAllWidget(input_id.clone()))
    } else {
        UiTask::None
    }
}

impl App {
    pub(super) fn begin_rename_target(&mut self, target: RenameTarget) -> Result<(), String> {
        self.cancel_tab_drag();
        self.cancel_confirm_delete();
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

    pub(super) fn cancel_rename_editor(&mut self) {
        self.rename_editor = None;
        self.rename_focus_requested = false;
    }

    pub fn rename_pointer_dismiss(&mut self) {
        self.cancel_rename_editor();
    }

    pub(super) fn cancel_editor_for_interaction(&mut self) {
        self.cancel_rename_editor();
    }

    pub(super) fn reconcile_rename_editor(&mut self) {
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

    pub(super) fn take_rename_focus_task(&mut self) -> UiTask {
        take_rename_focus_request(
            &mut self.rename_focus_requested,
            self.rename_editor.is_some(),
            &self.rename_input_id,
        )
    }
}

// ── tab drag ──

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TabDragContext {
    pub(super) project_id: i64,
    pub(super) source_id: i64,
    pub(super) generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TabDragPreview {
    pub(super) context: TabDragContext,
    pub(super) original_ids: Vec<i64>,
    pub(super) ordered_ids: Vec<i64>,
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

pub(super) fn same_stable_ids(left: &[i64], right: &[i64]) -> bool {
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

impl App {
    fn active_project_tab_ids(&self, project_id: i64) -> Vec<i64> {
        self.projects
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.tabs.iter().map(|tab| tab.id).collect())
            .unwrap_or_default()
    }

    pub(super) fn cancel_tab_drag(&mut self) {
        self.tab_drag_preview = None;
        self.tab_strip_generation = self.tab_strip_generation.wrapping_add(1);
    }

    pub(super) fn reconcile_tab_drag_preview(&mut self) {
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

    pub(crate) fn strip_pointer_released(&mut self) {
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

    pub(crate) fn tab_strip_event(&mut self, event: StripEvent) {
        match event {
            StripEvent::Started {
                scope_id: project_id,
                source_id,
                context_generation,
                original_ids,
            } => {
                self.begin_tab_drag_preview(project_id, source_id, context_generation, original_ids)
            }
            // Tab visuals key off preview presence, so the threshold
            // crossing changes nothing here; the event exists for the
            // project strip's agent-row hiding.
            StripEvent::DragBegan { .. } => {}
            StripEvent::Preview {
                scope_id: project_id,
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
            StripEvent::Commit {
                scope_id: project_id,
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
            StripEvent::Ended {
                scope_id: project_id,
                source_id,
                context_generation,
                original_ids,
            } => {
                self.end_tab_drag_preview(project_id, source_id, context_generation, &original_ids)
            }
            StripEvent::Cancel { context_generation } => {
                if context_generation == self.tab_strip_generation {
                    self.cancel_tab_drag();
                    self.reconcile();
                }
            }
        }
    }
}

// ── pointer ──

impl App {
    pub fn pointer(&mut self, event: TerminalPointerEvent) -> UiTask {
        // The confirm overlay's catcher only owns primary presses;
        // motion, right/middle presses, and releases would otherwise
        // reach a mouse-tracking PTY (middle-press can even paste).
        // A destructive modal must leak no pointer input.
        if self.confirm_delete.is_some() {
            return UiTask::None;
        }
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

    pub(super) fn link_modifier_held(&self) -> bool {
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
}

// ── clipboard & drop ──

const FILE_DROP_DEBOUNCE: Duration = Duration::from_millis(50);

#[derive(Debug, PartialEq, Eq)]
pub(super) struct PendingFileDrop {
    tab_id: i64,
    paths: Vec<PathBuf>,
    deadline: Instant,
}

#[derive(Debug, Default)]
pub(super) struct FileDropQueue {
    pending: Option<PendingFileDrop>,
}

impl FileDropQueue {
    /// Add one native path event. Iced/winit emits one event per path and no
    /// successful-drop terminator, so a short extending deadline defines one
    /// multi-file gesture. An expired gesture is returned before the new event
    /// is installed, preventing delayed ticks from merging two gestures. With
    /// no native gesture ID, every event inside the deadline belongs to the
    /// first stable origin even if focus changes to another terminal, a
    /// palette, or an editor between per-path events.
    pub(super) fn push_at(
        &mut self,
        new_origin: Option<i64>,
        path: PathBuf,
        now: Instant,
    ) -> (Option<PendingFileDrop>, bool) {
        let flush = self
            .pending
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline);
        let ready = flush.then(|| self.pending.take()).flatten();
        match &mut self.pending {
            Some(pending) => {
                pending.paths.push(path);
                pending.deadline = now + FILE_DROP_DEBOUNCE;
                (ready, true)
            }
            None => {
                let Some(tab_id) = new_origin else {
                    return (ready, false);
                };
                self.pending = Some(PendingFileDrop {
                    tab_id,
                    paths: vec![path],
                    deadline: now + FILE_DROP_DEBOUNCE,
                });
                (ready, true)
            }
        }
    }

    pub(super) fn take_ready_at(&mut self, now: Instant) -> Option<PendingFileDrop> {
        self.pending
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline)
            .then(|| self.pending.take())
            .flatten()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileDropDisposition {
    Pasted,
    Invalid,
    ClosedOrigin,
}

fn dispatch_file_drop_batch(
    batch: PendingFileDrop,
    origin_live: bool,
    paste: impl FnOnce(&str),
) -> FileDropDisposition {
    if !origin_live {
        return FileDropDisposition::ClosedOrigin;
    }
    let Some(text) = roost_ui_model::drop_content::resolve(batch.paths, None) else {
        return FileDropDisposition::Invalid;
    };
    paste(&text);
    FileDropDisposition::Pasted
}

pub(super) fn native_file_drop_origin(
    app_window: Option<window::Id>,
    event_window: window::Id,
    route: KeyboardRoute,
) -> Option<i64> {
    (app_window == Some(event_window))
        .then_some(route)
        .and_then(|route| match route {
            KeyboardRoute::Terminal(tab_id) => Some(tab_id),
            KeyboardRoute::None
            | KeyboardRoute::Confirm
            | KeyboardRoute::Editor
            | KeyboardRoute::Palette => None,
        })
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
pub(super) struct ClipboardQueue {
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

    pub(super) fn enqueue_ipc_read(&mut self, target: ClipboardOp, reply: ClipboardReply) -> u64 {
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

    pub(super) fn enqueue_write(&mut self, target: ClipboardOp, text: String) -> u64 {
        let request_id = self.allocate_request_id();
        self.queued.push_back(ClipboardEffect::Write {
            request_id,
            target,
            text,
        });
        request_id
    }

    pub(super) fn start_next(&mut self) -> UiTask {
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

pub(super) fn enqueue_osc_clipboard_write(
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

pub(super) fn paste_bytes(terminal: &Terminal, text: Option<&str>) -> Vec<u8> {
    let Some(text) = text.filter(|text| !text.is_empty()) else {
        return Vec::new();
    };
    roost_ui_model::bracketed_paste::wrap(text, terminal.mode_get(2004))
}

impl App {
    pub(super) fn deliver_file_drop(&mut self, batch: PendingFileDrop) {
        let tab_id = batch.tab_id;
        let origin_live = self.workspace.tab(tab_id).is_ok() && self.tabs.contains_key(&tab_id);
        let disposition = dispatch_file_drop_batch(batch, origin_live, |text| {
            // The stable origin was stamped by the first window event. It
            // cannot change when another tab gains focus during debounce.
            self.tabs
                .get(&tab_id)
                .expect("live file-drop origin must have a terminal adapter")
                .paste(Some(text));
        });
        match disposition {
            FileDropDisposition::Pasted => {}
            FileDropDisposition::Invalid => {
                tracing::debug!(tab_id, "ignored file drop with no safe local paths")
            }
            FileDropDisposition::ClosedOrigin => {
                tracing::debug!(tab_id, "discarded file drop for a closed tab")
            }
        }
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

    pub fn clipboard_write_completed(&mut self, request_id: u64) -> UiTask {
        if !self.clipboard.complete_write(request_id) {
            tracing::warn!(request_id, "ignored stale native clipboard write result");
            return UiTask::None;
        }
        self.clipboard.start_next()
    }

    pub(super) fn copy_active_selection(&mut self) -> UiTask {
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

    pub(super) fn paste_into_active(&mut self, target: ClipboardOp) -> UiTask {
        let tab_id = self.workspace.active().1;
        if !self.tabs.contains_key(&tab_id) {
            return UiTask::None;
        }
        self.clipboard.enqueue_paste_read(target, tab_id);
        self.clipboard.start_next()
    }
}

// ── screenshot ──

struct ScreenshotRequest {
    scale: u32,
    reply: ScreenshotReply,
}

#[derive(Default)]
pub(super) struct ScreenshotQueue {
    pending: VecDeque<ScreenshotRequest>,
    in_flight: Option<ScreenshotRequest>,
}

impl ScreenshotQueue {
    pub(super) fn enqueue(&mut self, scale: u32, reply: ScreenshotReply) {
        self.pending.push_back(ScreenshotRequest { scale, reply });
    }

    pub(super) fn start_next(&mut self, window_id: Option<window::Id>) -> UiTask {
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

impl App {
    pub fn screenshot_captured(&mut self, capture: &window::Screenshot) -> UiTask {
        if let Some(request) = self.screenshots.complete() {
            let result = crate::screenshot::encode(capture, request.scale);
            let _ = request.reply.send(result);
        } else {
            tracing::warn!("received an Iced screenshot with no request in flight");
        }
        self.screenshots.start_next(self.window_id)
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

    fn rename_fixture() -> (Vec<Project>, i64, i64, i64) {
        let workspace = Workspace::new();
        let first = workspace.create_project("First", "/tmp").unwrap();
        let first_tab = workspace.open_tab(first.id, "/tmp", "alpha").unwrap();
        let second = workspace.create_project("Second", "/var").unwrap();
        let second_tab = workspace.open_tab(second.id, "/var", "beta").unwrap();
        (workspace.snapshot(), first.id, first_tab.id, second_tab.id)
    }

    #[test]
    fn file_drop_queue_extends_deadline_and_preserves_stable_origin() {
        let start = Instant::now();
        let mut queue = FileDropQueue::default();
        assert_eq!(
            queue.push_at(Some(7), PathBuf::from("/tmp/first"), start),
            (None, true)
        );
        assert_eq!(
            queue.push_at(
                Some(7),
                PathBuf::from("/tmp/second"),
                start + Duration::from_millis(40)
            ),
            (None, true)
        );
        assert!(queue
            .take_ready_at(start + Duration::from_millis(89))
            .is_none());
        let batch = queue
            .take_ready_at(start + Duration::from_millis(90))
            .expect("extended deadline is inclusive");
        assert_eq!(batch.tab_id, 7);
        assert_eq!(
            batch.paths,
            [PathBuf::from("/tmp/first"), PathBuf::from("/tmp/second")]
        );
    }

    #[test]
    fn file_drop_queue_flushes_expired_gestures_and_keeps_first_origin_across_focus() {
        let start = Instant::now();
        let mut queue = FileDropQueue::default();
        assert_eq!(
            queue.push_at(None, PathBuf::from("/tmp/unowned"), start),
            (None, false),
            "an unowned event cannot start a batch"
        );
        assert_eq!(
            queue.push_at(Some(7), PathBuf::from("/tmp/first"), start),
            (None, true)
        );
        let (expired, accepted) = queue.push_at(
            Some(7),
            PathBuf::from("/tmp/second"),
            start + FILE_DROP_DEBOUNCE,
        );
        let expired = expired.expect("deadline boundary flushes before accepting a path");
        assert!(accepted);
        assert_eq!(expired.tab_id, 7);
        assert_eq!(expired.paths, [PathBuf::from("/tmp/first")]);

        assert_eq!(
            queue.push_at(
                Some(9),
                PathBuf::from("/tmp/third"),
                start + FILE_DROP_DEBOUNCE + Duration::from_millis(1),
            ),
            (None, true),
            "focus change inside one native batch must not split or retarget it"
        );
        assert_eq!(
            queue.push_at(
                None,
                PathBuf::from("/tmp/fourth"),
                start + FILE_DROP_DEBOUNCE + Duration::from_millis(2),
            ),
            (None, true),
            "palette/editor route inside one native batch must not truncate it"
        );
        let current = queue
            .take_ready_at(start + 2 * FILE_DROP_DEBOUNCE + Duration::from_millis(2))
            .expect("second gesture remains independently flushable at its extended deadline");
        assert_eq!(current.tab_id, 7);
        assert_eq!(
            current.paths,
            [
                PathBuf::from("/tmp/second"),
                PathBuf::from("/tmp/third"),
                PathBuf::from("/tmp/fourth")
            ]
        );
    }

    #[test]
    fn native_file_drop_requires_the_owned_window_and_terminal_input_route() {
        let owned = window::Id::unique();
        let other = window::Id::unique();
        assert_eq!(
            native_file_drop_origin(Some(owned), owned, KeyboardRoute::Terminal(42)),
            Some(42)
        );
        assert_eq!(
            native_file_drop_origin(Some(owned), other, KeyboardRoute::Terminal(42)),
            None
        );
        assert_eq!(
            native_file_drop_origin(None, owned, KeyboardRoute::Terminal(42)),
            None
        );
        for route in [
            KeyboardRoute::None,
            KeyboardRoute::Editor,
            KeyboardRoute::Palette,
        ] {
            assert_eq!(native_file_drop_origin(Some(owned), owned, route), None);
        }
    }

    #[test]
    fn file_drop_batch_is_one_plain_or_bracketed_paste_and_never_retargets() {
        let start = Instant::now();
        let batch = || PendingFileDrop {
            tab_id: 41,
            paths: vec![
                PathBuf::from("/tmp/My File.png"),
                PathBuf::from("/tmp/My File.png"),
                PathBuf::from("/tmp/second.png"),
            ],
            deadline: start,
        };
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 10,
            rows: 2,
            max_scrollback: 10,
        })
        .unwrap();
        let mut calls = 0;
        let mut bytes = Vec::new();
        assert_eq!(
            dispatch_file_drop_batch(batch(), true, |text| {
                calls += 1;
                bytes = paste_bytes(&terminal, Some(text));
            }),
            FileDropDisposition::Pasted
        );
        assert_eq!(calls, 1);
        assert_eq!(bytes, b"/tmp/My\\ File.png\n/tmp/second.png");

        terminal.vt_write(b"\x1b[?2004h");
        assert_eq!(
            dispatch_file_drop_batch(batch(), true, |text| {
                calls += 1;
                bytes = paste_bytes(&terminal, Some(text));
            }),
            FileDropDisposition::Pasted
        );
        assert_eq!(calls, 2);
        assert_eq!(
            bytes,
            b"\x1b[200~/tmp/My\\ File.png\n/tmp/second.png\x1b[201~"
        );

        assert_eq!(
            dispatch_file_drop_batch(batch(), false, |_| panic!("closed origin retargeted")),
            FileDropDisposition::ClosedOrigin
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn file_drop_batch_reaches_terminal_session_capture_exactly_once() {
        let (mut tab, supervisor) = attached_test_terminal(9_043);
        let capture = tab.input_capture.as_ref().unwrap();
        let batch = || PendingFileDrop {
            tab_id: 9_043,
            paths: vec![
                PathBuf::from("/tmp/My File.png"),
                PathBuf::from("/tmp/second.png"),
            ],
            deadline: Instant::now(),
        };

        assert_eq!(
            dispatch_file_drop_batch(batch(), true, |text| tab.paste(Some(text))),
            FileDropDisposition::Pasted
        );
        assert_eq!(
            capture.lock().unwrap().as_slice(),
            b"/tmp/My\\ File.png\n/tmp/second.png"
        );

        capture.lock().unwrap().clear();
        tab.terminal.vt_write(b"\x1b[?2004h");
        assert_eq!(
            dispatch_file_drop_batch(batch(), true, |text| tab.paste(Some(text))),
            FileDropDisposition::Pasted
        );
        assert_eq!(
            capture.lock().unwrap().as_slice(),
            b"\x1b[200~/tmp/My\\ File.png\n/tmp/second.png\x1b[201~"
        );
        supervisor.close(9_043);
    }

    #[test]
    fn file_drop_invalid_first_path_keeps_origin_but_emits_no_empty_write() {
        let start = Instant::now();
        let invalid = PendingFileDrop {
            tab_id: 77,
            paths: vec![PathBuf::from("/tmp/unsafe\npath")],
            deadline: start,
        };
        assert_eq!(
            dispatch_file_drop_batch(invalid, true, |_| panic!("invalid path pasted")),
            FileDropDisposition::Invalid
        );

        let mut queue = FileDropQueue::default();
        assert_eq!(
            queue.push_at(Some(77), PathBuf::from("/tmp/unsafe\npath"), start),
            (None, true)
        );
        assert_eq!(
            queue.push_at(
                Some(77),
                PathBuf::from("/tmp/safe path"),
                start + Duration::from_millis(1),
            ),
            (None, true)
        );
        let batch = queue
            .take_ready_at(start + Duration::from_millis(51))
            .unwrap();
        assert_eq!(batch.tab_id, 77);
        let mut resolved = None;
        assert_eq!(
            dispatch_file_drop_batch(batch, true, |text| resolved = Some(text.to_string())),
            FileDropDisposition::Pasted
        );
        assert_eq!(resolved.as_deref(), Some("/tmp/safe\\ path"));
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
        // A clipboard carrying the end marker can't close the region early.
        assert_eq!(
            paste_bytes(&terminal, Some("\x1b[201~rm -rf /\n")),
            b"\x1b[200~rm -rf /\n\x1b[201~"
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
}
