use super::*;

// ── rename ──

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RenameTarget {
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

/// What a submit resolves to before anything is dispatched. The engine
/// call is no longer part of the decision — it cannot answer on the UI
/// thread — so the editor's fate is split across the dispatch and the
/// completion that quotes its op id back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RenameSubmission {
    /// A rename is already in flight for this editor. The submit is a
    /// no-op down to the last field: a second Enter must not dispatch a
    /// second rename, and must not disturb the editor the first one is
    /// still waiting on.
    InFlight,
    /// Nothing to dispatch — no editor, the Enter guard already armed, or
    /// a draft that commits to nothing (which dismisses the editor, as it
    /// always has).
    Settled,
    Dispatch {
        target: RenameTarget,
        label: String,
        op: u64,
    },
}

fn plan_rename_submission(editor: &mut Option<RenameEditor>, op: u64) -> RenameSubmission {
    let Some(current) = editor.as_ref() else {
        return RenameSubmission::Settled;
    };
    let Some(label) = roost_ui_model::rename::committed_label(&current.draft) else {
        *editor = None;
        return RenameSubmission::Settled;
    };
    RenameSubmission::Dispatch {
        target: current.target,
        label,
        op,
    }
}

fn plan_rename_submission_once(
    editor: &mut Option<RenameEditor>,
    pending: &mut Option<RenameCompletionKey>,
    in_flight: Option<u64>,
    op: u64,
) -> RenameSubmission {
    if in_flight.is_some() {
        return RenameSubmission::InFlight;
    }
    if editor.is_none() || *pending == Some(RenameCompletionKey::Enter) {
        return RenameSubmission::Settled;
    }
    *pending = Some(RenameCompletionKey::Enter);
    plan_rename_submission(editor, op)
}

/// How a rename completion lands. The op id is the whole guard: the
/// editor the completion was dispatched from may have been dismissed and
/// a new one opened over the same target since.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RenameCompletion {
    /// The completion belongs to an editor that is no longer waiting for
    /// it. Nothing is said and nothing is touched.
    Stale,
    /// The rename landed — the editor closes, as it did the instant the
    /// blocking call returned.
    Closed,
    /// The rename failed — the editor stays open with the error in the
    /// banner (`interactions.rs` has behaved this way since the editor
    /// existed; a failed rename must not eat the user's draft).
    Failed,
}

fn resolve_rename_completion(
    in_flight: &mut Option<u64>,
    editor: &mut Option<RenameEditor>,
    op: u64,
    result: &Result<(), String>,
) -> RenameCompletion {
    if *in_flight != Some(op) {
        return RenameCompletion::Stale;
    }
    *in_flight = None;
    match result {
        Ok(()) => {
            *editor = None;
            RenameCompletion::Closed
        }
        Err(_) => RenameCompletion::Failed,
    }
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
        self.cancel_drags();
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

    pub fn begin_rename_project(&mut self, project_id: i64) -> UiTask {
        if let Err(error) = self.begin_rename_target(RenameTarget::Project(project_id)) {
            self.set_status(error);
        }
        self.take_rename_focus_task()
    }

    pub fn begin_rename_tab(&mut self, tab_id: i64) -> UiTask {
        if let Err(error) = self.begin_rename_target(RenameTarget::Tab(tab_id)) {
            self.set_status(error);
        }
        self.take_rename_focus_task()
    }

    pub fn rename_draft_changed(&mut self, draft: String) {
        if let Some(editor) = &mut self.rename_editor {
            editor.draft = draft;
        }
    }

    /// The editor no longer closes here on success — it closes when the
    /// engine says the rename landed. Until then it stays on screen with
    /// its draft, and a second Enter is refused rather than queued.
    pub fn submit_rename_editor(&mut self) -> UiTask {
        let op = self.take_engine_op_id();
        match plan_rename_submission_once(
            &mut self.rename_editor,
            &mut self.rename_completion_key,
            self.rename_op,
            op,
        ) {
            RenameSubmission::InFlight => {
                tracing::debug!(?self.rename_op, "ignored rename submit while one is in flight");
                UiTask::None
            }
            RenameSubmission::Settled => {
                self.rename_focus_requested = false;
                self.reconcile();
                UiTask::None
            }
            RenameSubmission::Dispatch { target, label, op } => {
                self.rename_op = Some(op);
                let client = self.client.clone();
                self.engine_op(
                    async move {
                        match target {
                            RenameTarget::Project(project_id) => {
                                client.rename_project(project_id, &label).await
                            }
                            RenameTarget::Tab(tab_id) => client.set_tab_title(tab_id, &label).await,
                        }
                        .map_err(|error| error.to_string())
                    },
                    move |result| EngineOpResult::Renamed { op, target, result },
                )
            }
        }
    }

    pub(super) fn rename_completed(
        &mut self,
        op: u64,
        target: RenameTarget,
        result: Result<(), String>,
    ) {
        match resolve_rename_completion(&mut self.rename_op, &mut self.rename_editor, op, &result) {
            RenameCompletion::Stale => {
                tracing::debug!(
                    op,
                    ?target,
                    "dropped a rename completion no editor is awaiting"
                );
            }
            RenameCompletion::Closed => {
                self.rename_focus_requested = false;
            }
            RenameCompletion::Failed => {
                let error = result.unwrap_err();
                tracing::warn!(%error, ?target, "rename failed");
                self.set_status(error);
            }
        }
    }

    /// Dismissing an editor mid-flight is allowed: dropping the op id is
    /// what turns the rename still on its way back into a completion
    /// nobody is waiting for, so it can neither reopen this editor nor
    /// close the next one.
    pub(super) fn cancel_rename_editor(&mut self) {
        self.rename_editor = None;
        self.rename_focus_requested = false;
        self.rename_op = None;
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
    /// Set once the reorder is dispatched. It marks the preview as
    /// already settled — no second release can re-commit it — and it is
    /// the id the completion must quote to be allowed to clear it.
    pub(super) pending_op: Option<u64>,
}

impl TabDragPreview {
    /// Whether this pill is the one being carried. A dispatched preview
    /// outlives the gesture only to hold the order: the pointer is up, so
    /// the drag styling comes off at the release, as it always did.
    pub(super) fn drags(&self, tab_id: i64) -> bool {
        self.pending_op.is_none() && self.context.source_id == tab_id
    }
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
    /// The gesture resolved to nothing to reorder — a drag that landed
    /// back where it started, or a preview the authoritative order has
    /// already moved out from under.
    Settled,
    /// The reorder is on its way to the engine under this op id. The
    /// preview stays on screen until the completion (or the reorder
    /// event that beats it) says otherwise.
    Dispatched {
        ordered_ids: Vec<i64>,
        op: u64,
    },
}

/// Ids a gesture can be keyed to: unique, and never the `0` placeholder a
/// not-yet-persisted row carries.
pub(super) fn stable_ids(ids: &[i64]) -> bool {
    !ids.contains(&0) && ids.iter().copied().collect::<HashSet<_>>().len() == ids.len()
}

pub(super) fn same_stable_ids(left: &[i64], right: &[i64]) -> bool {
    stable_ids(left)
        && stable_ids(right)
        && left.len() == right.len()
        && left.iter().copied().collect::<HashSet<_>>()
            == right.iter().copied().collect::<HashSet<_>>()
}

/// Clearing a preview always burns its generation: a gesture the widget is
/// still holding carries the old one and can never re-arm against the new.
/// Both strips cancel through here, which is also how one strip evicts the
/// other when it arms.
fn cancel_drag_preview<T>(preview: &mut Option<T>, generation: &mut u64) {
    *generation = generation.wrapping_add(1);
    preview.take();
}

fn tab_drag_commit_is_valid(
    preview: Option<&TabDragPreview>,
    authoritative_ids: &[i64],
    context: &TabDragContext,
    original_ids: &[i64],
    ordered_ids: &[i64],
) -> bool {
    let valid = preview.is_some_and(|preview| {
        preview.context == *context
            && preview.original_ids == original_ids
            && preview.ordered_ids == ordered_ids
            && authoritative_ids == original_ids
            && same_stable_ids(ordered_ids, original_ids)
    });
    valid && ordered_ids != original_ids
}

/// Settle a release against the preview it claims. A preview that already
/// dispatched keeps its order on screen but is no longer commitable — the
/// root release boundary publishes its own release right behind the
/// strip's commit, and both name the same gesture.
fn settle_tab_drag_commit(
    preview: &mut Option<TabDragPreview>,
    authoritative_ids: &[i64],
    request: TabDragCommitRequest,
    op: u64,
) -> TabDragSettlement {
    if preview
        .as_ref()
        .is_none_or(|preview| preview.pending_op.is_some() || preview.context != request.context)
    {
        return TabDragSettlement::Ignored;
    }

    if !tab_drag_commit_is_valid(
        preview.as_ref(),
        authoritative_ids,
        &request.context,
        &request.original_ids,
        &request.ordered_ids,
    ) {
        *preview = None;
        return TabDragSettlement::Settled;
    }

    if let Some(preview) = preview.as_mut() {
        preview.pending_op = Some(op);
    }
    TabDragSettlement::Dispatched {
        ordered_ids: request.ordered_ids,
        op,
    }
}

/// A completion clears only the preview that dispatched it. A newer drag
/// carries a different id (or none yet), so an older reorder reporting
/// back cannot pull the order out from under the gesture that replaced it.
fn clear_dispatched_preview<T>(
    preview: &mut Option<T>,
    op: u64,
    pending_op: impl Fn(&T) -> Option<u64>,
) -> bool {
    let owned = preview
        .as_ref()
        .is_some_and(|preview| pending_op(preview) == Some(op));
    if owned {
        *preview = None;
    }
    owned
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
        cancel_drag_preview(&mut self.tab_drag_preview, &mut self.tab_strip_generation);
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
        self.cancel_project_drag();
        let authoritative = self.active_project_tab_ids(project_id);
        if context_generation != self.tab_strip_generation
            || self.workspace.active().0 != project_id
            || authoritative != original_ids
            || !stable_ids(&original_ids)
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
            pending_op: None,
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
    ) -> UiTask {
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
        let op = self.take_engine_op_id();
        let settlement =
            settle_tab_drag_commit(&mut self.tab_drag_preview, &authoritative, request, op);
        tracing::debug!(
            ?settlement,
            project_id,
            source_id,
            context_generation,
            "Iced tab drag settlement"
        );
        match settlement {
            TabDragSettlement::Ignored => UiTask::None,
            // The generation burns the moment the gesture stops being the
            // widget's: whatever the strip still holds carries the old one
            // and can no longer re-arm, cancel, or preview against it.
            TabDragSettlement::Settled => {
                self.tab_strip_generation = self.tab_strip_generation.wrapping_add(1);
                self.reconcile();
                UiTask::None
            }
            TabDragSettlement::Dispatched { ordered_ids, op } => {
                self.tab_strip_generation = self.tab_strip_generation.wrapping_add(1);
                let client = self.client.clone();
                let dispatched = ordered_ids.clone();
                self.engine_op(
                    async move {
                        client
                            .reorder_tabs(project_id, dispatched)
                            .await
                            .map_err(|error| error.to_string())
                    },
                    move |result| EngineOpResult::TabsReordered {
                        op,
                        project_id,
                        ordered_ids,
                        result,
                    },
                )
            }
        }
    }

    /// The preview outlives the dispatch so a successful reorder never
    /// snaps: by the time the completion lands the authoritative order is
    /// the previewed one (often via the reorder event, which clears the
    /// preview first). A failure clears it too — that clear *is* the
    /// rollback, with the reconcile behind it restoring the real order.
    pub(super) fn tab_reorder_completed(
        &mut self,
        op: u64,
        project_id: i64,
        ordered_ids: &[i64],
        result: Result<(), String>,
    ) {
        let cleared =
            clear_dispatched_preview(&mut self.tab_drag_preview, op, |preview| preview.pending_op);
        if let Err(error) = result {
            tracing::warn!(?error, project_id, ?ordered_ids, "Iced tab reorder failed");
            self.set_status(format!("reorder tabs: {error}"));
        } else if !cleared {
            tracing::debug!(op, project_id, "tab reorder completed past its preview");
        }
    }

    /// At most one preview exists — the strips cancel each other when a
    /// gesture arms — so this settles whichever one owns the release.
    pub(crate) fn strip_pointer_released(&mut self) -> UiTask {
        if let Some(preview) = self.tab_drag_preview.as_ref() {
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
            )
        } else if let Some(preview) = self.project_drag_preview.as_ref() {
            tracing::debug!(
                source_id = preview.context.source_id,
                generation = preview.context.generation,
                ordered_ids = ?preview.ordered_ids,
                "Iced root release settling project drag preview"
            );
            let request = ProjectDragCommitRequest::from(preview);
            self.commit_project_drag(
                request.context.source_id,
                request.context.generation,
                &request.original_ids,
                request.ordered_ids,
            )
        } else {
            UiTask::None
        }
    }

    pub(crate) fn has_drag_preview(&self) -> bool {
        self.tab_drag_preview.is_some() || self.project_drag_preview.is_some()
    }

    pub(crate) fn tab_strip_event(&mut self, event: StripEvent) -> UiTask {
        match event {
            StripEvent::Started {
                scope_id: project_id,
                source_id,
                context_generation,
                original_ids,
            } => {
                self.begin_tab_drag_preview(
                    project_id,
                    source_id,
                    context_generation,
                    original_ids,
                );
                UiTask::None
            }
            // Tab visuals key off preview presence, so the threshold
            // crossing changes nothing here; the event exists for the
            // project strip's agent-row hiding.
            StripEvent::DragBegan { .. } => UiTask::None,
            StripEvent::Preview {
                scope_id: project_id,
                source_id,
                context_generation,
                original_ids,
                ordered_ids,
            } => {
                self.preview_tab_drag(
                    project_id,
                    source_id,
                    context_generation,
                    &original_ids,
                    ordered_ids,
                );
                UiTask::None
            }
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
                self.end_tab_drag_preview(project_id, source_id, context_generation, &original_ids);
                UiTask::None
            }
            StripEvent::Cancel { context_generation } => {
                if context_generation == self.tab_strip_generation {
                    self.cancel_tab_drag();
                    self.reconcile();
                }
                UiTask::None
            }
        }
    }
}

// ── project drag ──

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProjectDragContext {
    pub(super) source_id: i64,
    pub(super) generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProjectDragPreview {
    pub(super) context: ProjectDragContext,
    pub(super) original_ids: Vec<i64>,
    pub(super) ordered_ids: Vec<i64>,
    pub(super) dragging: bool,
    /// Set once the reorder is dispatched — see [`TabDragPreview::pending_op`].
    pub(super) pending_op: Option<u64>,
}

impl ProjectDragPreview {
    /// Whether the sidebar draws this preview's order instead of the
    /// authoritative one. A dispatched preview burned its generation on
    /// the way out, so the generation alone would drop it — and dropping
    /// it is the snap-back the optimistic order exists to avoid.
    pub(super) fn orders_the_strip(&self, strip_generation: u64) -> bool {
        self.pending_op.is_some() || self.context.generation == strip_generation
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProjectDragCommitRequest {
    context: ProjectDragContext,
    original_ids: Vec<i64>,
    ordered_ids: Vec<i64>,
}

impl From<&ProjectDragPreview> for ProjectDragCommitRequest {
    fn from(preview: &ProjectDragPreview) -> Self {
        Self {
            context: preview.context.clone(),
            original_ids: preview.original_ids.clone(),
            ordered_ids: preview.ordered_ids.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ProjectDragSettlement {
    Ignored,
    Settled,
    Dispatched { ordered_ids: Vec<i64>, op: u64 },
}

/// Agent sub-rows change every project group's height, so they are hidden
/// only once the gesture crosses the drag threshold — the strip publishes
/// its start on a bare press, which must leave the sidebar untouched.
pub(super) fn agent_rows_hidden(preview: Option<&ProjectDragPreview>) -> bool {
    preview.is_some_and(|preview| preview.dragging)
}

fn project_drag_commit_is_valid(
    preview: Option<&ProjectDragPreview>,
    authoritative_ids: &[i64],
    context: &ProjectDragContext,
    original_ids: &[i64],
    ordered_ids: &[i64],
) -> bool {
    let valid = preview.is_some_and(|preview| {
        preview.context == *context
            && preview.original_ids == original_ids
            && preview.ordered_ids == ordered_ids
            && authoritative_ids == original_ids
            && same_stable_ids(ordered_ids, original_ids)
    });
    valid && ordered_ids != original_ids
}

fn settle_project_drag_commit(
    preview: &mut Option<ProjectDragPreview>,
    authoritative_ids: &[i64],
    request: ProjectDragCommitRequest,
    op: u64,
) -> ProjectDragSettlement {
    if preview
        .as_ref()
        .is_none_or(|preview| preview.pending_op.is_some() || preview.context != request.context)
    {
        return ProjectDragSettlement::Ignored;
    }

    if !project_drag_commit_is_valid(
        preview.as_ref(),
        authoritative_ids,
        &request.context,
        &request.original_ids,
        &request.ordered_ids,
    ) {
        *preview = None;
        return ProjectDragSettlement::Settled;
    }

    if let Some(preview) = preview.as_mut() {
        preview.pending_op = Some(op);
        // The pointer is up: the row stops looking dragged and the agent
        // sub-rows come back now, exactly as they did when the preview
        // died at the release. Only the order is held back.
        preview.dragging = false;
    }
    ProjectDragSettlement::Dispatched {
        ordered_ids: request.ordered_ids,
        op,
    }
}

/// `None` rejects the press: the caller cancels rather than arming a gesture
/// against a list the sidebar no longer renders.
fn arm_project_drag_preview(
    authoritative_ids: &[i64],
    strip_generation: u64,
    source_id: i64,
    context_generation: u64,
    original_ids: Vec<i64>,
) -> Option<ProjectDragPreview> {
    let armable = context_generation == strip_generation
        && authoritative_ids == original_ids
        && stable_ids(&original_ids)
        && original_ids.contains(&source_id);
    armable.then(|| ProjectDragPreview {
        context: ProjectDragContext {
            source_id,
            generation: context_generation,
        },
        ordered_ids: original_ids.clone(),
        original_ids,
        dragging: false,
        pending_op: None,
    })
}

fn mark_project_drag_dragging(
    preview: &mut Option<ProjectDragPreview>,
    strip_generation: u64,
    context: &ProjectDragContext,
) -> bool {
    let Some(current) = preview.as_mut() else {
        return false;
    };
    let owned = context.generation == strip_generation && current.context == *context;
    if owned {
        current.dragging = true;
    }
    owned
}

fn preview_project_drag_if_owned(
    preview: &mut Option<ProjectDragPreview>,
    authoritative_ids: &[i64],
    strip_generation: u64,
    context: &ProjectDragContext,
    original_ids: &[i64],
    ordered_ids: Vec<i64>,
) -> bool {
    let Some(current) = preview.as_mut() else {
        return false;
    };
    let owned = context.generation == strip_generation
        && authoritative_ids == original_ids
        && same_stable_ids(&ordered_ids, original_ids)
        && current.context == *context
        && current.original_ids == original_ids;
    if owned {
        current.ordered_ids = ordered_ids;
    }
    owned
}

fn project_drag_preview_is_stale(
    preview: Option<&ProjectDragPreview>,
    authoritative_ids: &[i64],
) -> bool {
    preview.is_some_and(|preview| preview.original_ids != authoritative_ids)
}

fn end_project_drag_preview_if_owned(
    preview: &mut Option<ProjectDragPreview>,
    authoritative_ids: &[i64],
    context: &ProjectDragContext,
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
    pub(super) fn sidebar_project_ids(&self) -> Vec<i64> {
        self.projects.iter().map(|project| project.id).collect()
    }

    pub(super) fn cancel_project_drag(&mut self) {
        cancel_drag_preview(
            &mut self.project_drag_preview,
            &mut self.project_strip_generation,
        );
    }

    pub(super) fn cancel_drags(&mut self) {
        self.cancel_tab_drag();
        self.cancel_project_drag();
        // An interrupted resize keeps its width — the callers here are the
        // interaction choke points where an overlay restructures the root and
        // strands the grip's widget state mid-drag.
        self.commit_sidebar_drag();
    }

    pub(super) fn reconcile_project_drag_preview(&mut self) {
        if self.project_drag_preview.is_none() {
            return;
        }
        // Project drags span the whole sidebar, so the full project-id list
        // is the authority — unlike tabs, no active-project scope applies.
        let authoritative = self.sidebar_project_ids();
        if project_drag_preview_is_stale(self.project_drag_preview.as_ref(), &authoritative) {
            self.cancel_project_drag();
        }
    }

    fn begin_project_drag_preview(
        &mut self,
        source_id: i64,
        context_generation: u64,
        original_ids: Vec<i64>,
    ) {
        self.cancel_tab_drag();
        let authoritative = self.sidebar_project_ids();
        match arm_project_drag_preview(
            &authoritative,
            self.project_strip_generation,
            source_id,
            context_generation,
            original_ids,
        ) {
            Some(preview) => self.project_drag_preview = Some(preview),
            None => self.cancel_project_drag(),
        }
    }

    fn begin_project_drag(&mut self, source_id: i64, context_generation: u64) {
        let context = ProjectDragContext {
            source_id,
            generation: context_generation,
        };
        if !mark_project_drag_dragging(
            &mut self.project_drag_preview,
            self.project_strip_generation,
            &context,
        ) {
            self.cancel_project_drag();
        }
    }

    fn preview_project_drag(
        &mut self,
        source_id: i64,
        context_generation: u64,
        original_ids: &[i64],
        ordered_ids: Vec<i64>,
    ) {
        let context = ProjectDragContext {
            source_id,
            generation: context_generation,
        };
        let authoritative = self.sidebar_project_ids();
        if !preview_project_drag_if_owned(
            &mut self.project_drag_preview,
            &authoritative,
            self.project_strip_generation,
            &context,
            original_ids,
            ordered_ids,
        ) {
            self.cancel_project_drag();
        }
    }

    fn end_project_drag_preview(
        &mut self,
        source_id: i64,
        context_generation: u64,
        original_ids: &[i64],
    ) {
        let context = ProjectDragContext {
            source_id,
            generation: context_generation,
        };
        let authoritative = self.sidebar_project_ids();
        end_project_drag_preview_if_owned(
            &mut self.project_drag_preview,
            &authoritative,
            &context,
            original_ids,
        );
    }

    fn commit_project_drag(
        &mut self,
        source_id: i64,
        context_generation: u64,
        original_ids: &[i64],
        ordered_ids: Vec<i64>,
    ) -> UiTask {
        let authoritative = self.sidebar_project_ids();
        let request = ProjectDragCommitRequest {
            context: ProjectDragContext {
                source_id,
                generation: context_generation,
            },
            original_ids: original_ids.to_vec(),
            ordered_ids,
        };
        let op = self.take_engine_op_id();
        let settlement =
            settle_project_drag_commit(&mut self.project_drag_preview, &authoritative, request, op);
        tracing::debug!(
            ?settlement,
            source_id,
            context_generation,
            "Iced project drag settlement"
        );
        match settlement {
            ProjectDragSettlement::Ignored => UiTask::None,
            ProjectDragSettlement::Settled => {
                self.project_strip_generation = self.project_strip_generation.wrapping_add(1);
                self.reconcile();
                UiTask::None
            }
            ProjectDragSettlement::Dispatched { ordered_ids, op } => {
                self.project_strip_generation = self.project_strip_generation.wrapping_add(1);
                let client = self.client.clone();
                let dispatched = ordered_ids.clone();
                self.engine_op(
                    async move {
                        client
                            .reorder_projects(dispatched)
                            .await
                            .map_err(|error| error.to_string())
                    },
                    move |result| EngineOpResult::ProjectsReordered {
                        op,
                        ordered_ids,
                        result,
                    },
                )
            }
        }
    }

    /// See [`App::tab_reorder_completed`] — the sidebar keeps its
    /// optimistic order on exactly the same terms.
    pub(super) fn project_reorder_completed(
        &mut self,
        op: u64,
        ordered_ids: &[i64],
        result: Result<(), String>,
    ) {
        let cleared = clear_dispatched_preview(&mut self.project_drag_preview, op, |preview| {
            preview.pending_op
        });
        if let Err(error) = result {
            tracing::warn!(?error, ?ordered_ids, "Iced project reorder failed");
            self.set_status(format!("reorder projects: {error}"));
        } else if !cleared {
            tracing::debug!(op, "project reorder completed past its preview");
        }
    }

    pub(crate) fn project_strip_event(&mut self, event: StripEvent) -> UiTask {
        match event {
            StripEvent::Started {
                scope_id: _,
                source_id,
                context_generation,
                original_ids,
            } => {
                self.begin_project_drag_preview(source_id, context_generation, original_ids);
                UiTask::None
            }
            StripEvent::DragBegan {
                scope_id: _,
                source_id,
                context_generation,
            } => {
                self.begin_project_drag(source_id, context_generation);
                UiTask::None
            }
            StripEvent::Preview {
                scope_id: _,
                source_id,
                context_generation,
                original_ids,
                ordered_ids,
            } => {
                self.preview_project_drag(
                    source_id,
                    context_generation,
                    &original_ids,
                    ordered_ids,
                );
                UiTask::None
            }
            StripEvent::Commit {
                scope_id: _,
                source_id,
                context_generation,
                original_ids,
                ordered_ids,
            } => {
                self.commit_project_drag(source_id, context_generation, &original_ids, ordered_ids)
            }
            StripEvent::Ended {
                scope_id: _,
                source_id,
                context_generation,
                original_ids,
            } => {
                self.end_project_drag_preview(source_id, context_generation, &original_ids);
                UiTask::None
            }
            StripEvent::Cancel { context_generation } => {
                if context_generation == self.project_strip_generation {
                    self.cancel_project_drag();
                    self.reconcile();
                }
                UiTask::None
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
                // The dispatch moves the pointer cell and hover before it
                // can fail, so the tab is left mid-gesture. Publish what it
                // actually holds — nothing else will.
                refresh_or_warn(tab_id, tab, "failed pointer dispatch");
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
        refresh_or_warn(tab_id, tab, "pointer dispatch");
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

    /// When the pending gesture is due, for the one-shot the drop path
    /// schedules. `None` means nothing is pending and no shot is owed.
    pub(super) fn pending_deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|pending| pending.deadline)
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
    let Some(text) = roost_ui_model::drop_content::resolve(batch.paths, None, None) else {
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

    /// These tests drive the terminal directly with `write_vt`, so the
    /// feed receiver is surplus.
    fn attached_test_terminal(tab_id: i64) -> (TerminalTab, Arc<PtySupervisor>) {
        let (feed_tx, _) = engine_feed::channel();
        attach_test_terminal(tab_id, feed_tx)
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

    /// Each accepted path schedules a one-shot for the deadline it saw, so
    /// a path that extends the window leaves an earlier shot in flight.
    /// Nothing cancels it: the shot that fires early re-checks the
    /// deadline, finds it moved, and delivers nothing — the later shot
    /// carries the batch.
    #[test]
    fn a_file_drop_shot_whose_deadline_moved_delivers_nothing() {
        let start = Instant::now();
        let mut queue = FileDropQueue::default();
        assert_eq!(
            queue.push_at(Some(7), PathBuf::from("/tmp/first"), start),
            (None, true)
        );
        let first_shot = queue.pending_deadline().expect("the first path is pending");
        assert_eq!(first_shot, start + FILE_DROP_DEBOUNCE);

        assert_eq!(
            queue.push_at(
                Some(7),
                PathBuf::from("/tmp/second"),
                start + Duration::from_millis(30),
            ),
            (None, true)
        );
        let second_shot = queue
            .pending_deadline()
            .expect("the batch is still pending");
        assert!(
            second_shot > first_shot,
            "the second path extended the window"
        );

        assert!(
            queue.take_ready_at(first_shot).is_none(),
            "the stale shot finds a deadline that moved"
        );
        let batch = queue
            .take_ready_at(second_shot)
            .expect("the shot the extension scheduled delivers the whole gesture");
        assert_eq!(
            batch.paths,
            [PathBuf::from("/tmp/first"), PathBuf::from("/tmp/second")]
        );
        assert!(
            queue.pending_deadline().is_none(),
            "a delivered gesture owes no further shot"
        );
        assert!(
            queue
                .take_ready_at(second_shot + FILE_DROP_DEBOUNCE)
                .is_none(),
            "and a shot that arrives after delivery is a no-op"
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

    fn tab_drag_preview_at(generation: u64, ordered_ids: Vec<i64>) -> TabDragPreview {
        TabDragPreview {
            context: TabDragContext {
                project_id: 7,
                source_id: 10,
                generation,
            },
            original_ids: vec![10, 20, 30],
            ordered_ids,
            pending_op: None,
        }
    }

    #[test]
    fn tab_drag_commit_dispatches_exactly_once_and_rejects_stale_or_noop_state() {
        let preview = tab_drag_preview_at(4, vec![20, 30, 10]);
        assert!(tab_drag_commit_is_valid(
            Some(&preview),
            &[10, 20, 30],
            &preview.context,
            &[10, 20, 30],
            &[20, 30, 10],
        ));

        for (authoritative, ordered) in [
            (vec![30, 20, 10], vec![20, 30, 10]),
            (vec![10, 20, 30], vec![10, 20, 30]),
            (vec![10, 20, 30], vec![20, 10, 30]),
        ] {
            assert!(
                !tab_drag_commit_is_valid(
                    Some(&preview),
                    &authoritative,
                    &preview.context,
                    &[10, 20, 30],
                    &ordered,
                ),
                "{authoritative:?} → {ordered:?} is stale or a no-op"
            );
        }

        assert!(!tab_drag_commit_is_valid(
            Some(&preview),
            &[10, 20, 30],
            &TabDragContext {
                project_id: 7,
                source_id: 10,
                generation: 5,
            },
            &[10, 20, 30],
            &[20, 30, 10],
        ));
    }

    /// The strip's own commit and the root release boundary publish from
    /// the same mouse-up, in that order. The dispatched preview is what
    /// makes the second one inert now that the first no longer clears it.
    #[test]
    fn tab_drag_settlement_is_owned_once_in_either_release_order() {
        let preview = tab_drag_preview_at(4, vec![20, 30, 10]);
        let fallback = TabDragCommitRequest::from(&preview);
        let direct = fallback.clone();

        for (first, second) in [
            (fallback.clone(), direct.clone()),
            (direct.clone(), fallback.clone()),
        ] {
            let mut current = Some(preview.clone());
            assert_eq!(
                settle_tab_drag_commit(&mut current, &[10, 20, 30], first, 9),
                TabDragSettlement::Dispatched {
                    ordered_ids: vec![20, 30, 10],
                    op: 9,
                }
            );
            assert_eq!(
                current.as_ref().and_then(|preview| preview.pending_op),
                Some(9),
                "the preview stays on screen, marked as already settled"
            );

            assert_eq!(
                settle_tab_drag_commit(&mut current, &[10, 20, 30], second, 10),
                TabDragSettlement::Ignored
            );
            assert_eq!(
                current.as_ref().and_then(|preview| preview.pending_op),
                Some(9),
                "the duplicate release neither re-dispatches nor re-keys"
            );
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
            pending_op: None,
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
            settle_tab_drag_commit(&mut current, &[10, 20, 30], stale, 9),
            TabDragSettlement::Ignored
        );
        assert_eq!(current, Some(newer));

        let mut absent = None;
        assert_eq!(
            settle_tab_drag_commit(
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
                9,
            ),
            TabDragSettlement::Ignored
        );
    }

    /// The pinned concurrency case: a second drag settles before the first
    /// reorder reports back. The older completion must not pull the newer
    /// gesture's order off the screen.
    #[test]
    fn a_superseded_reorder_completion_leaves_the_newer_drags_preview_alone() {
        let mut current = Some(tab_drag_preview_at(4, vec![20, 30, 10]));
        let first = TabDragCommitRequest::from(current.as_ref().unwrap());
        assert!(matches!(
            settle_tab_drag_commit(&mut current, &[10, 20, 30], first, 1),
            TabDragSettlement::Dispatched { op: 1, .. }
        ));

        // The second gesture arms against the reordered list and settles
        // while op 1 is still in flight.
        let mut second_preview = tab_drag_preview_at(5, vec![30, 10, 20]);
        second_preview.original_ids = vec![20, 30, 10];
        current = Some(second_preview.clone());
        let second = TabDragCommitRequest::from(&second_preview);
        assert!(matches!(
            settle_tab_drag_commit(&mut current, &[20, 30, 10], second, 2),
            TabDragSettlement::Dispatched { op: 2, .. }
        ));

        assert!(
            !clear_dispatched_preview(&mut current, 1, |preview| preview.pending_op),
            "op 1 no longer owns any preview"
        );
        assert_eq!(
            current.as_ref().map(|preview| preview.ordered_ids.clone()),
            Some(vec![30, 10, 20]),
            "the newer drag keeps its optimistic order"
        );

        assert!(clear_dispatched_preview(&mut current, 2, |preview| {
            preview.pending_op
        }));
        assert!(current.is_none());
    }

    /// A preview the user dropped before its op reported back leaves the
    /// completion with nothing to clear — and it must not resurrect it.
    #[test]
    fn a_reorder_completion_with_no_preview_left_clears_nothing() {
        let mut current: Option<TabDragPreview> = None;
        assert!(!clear_dispatched_preview(&mut current, 1, |preview| {
            preview.pending_op
        }));
        assert!(current.is_none());

        let mut armed = Some(tab_drag_preview_at(4, vec![20, 30, 10]));
        assert!(
            !clear_dispatched_preview(&mut armed, 1, |preview| preview.pending_op),
            "a preview that never dispatched is owned by no completion"
        );
        assert!(armed.is_some());
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
            pending_op: None,
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

    /// A gesture that dropped where it started has nothing to send, so it
    /// clears immediately — there is no completion coming to clear it.
    #[test]
    fn crossed_threshold_return_to_origin_is_a_settled_noop() {
        let original = vec![10, 20, 30];
        let preview = tab_drag_preview_at(4, original.clone());
        let request = TabDragCommitRequest::from(&preview);
        let mut current = Some(preview);
        assert_eq!(
            settle_tab_drag_commit(&mut current, &original, request, 9),
            TabDragSettlement::Settled
        );
        assert!(current.is_none());
    }

    fn project_drag_preview(dragging: bool) -> ProjectDragPreview {
        ProjectDragPreview {
            context: ProjectDragContext {
                source_id: 10,
                generation: 4,
            },
            original_ids: vec![10, 20, 30],
            ordered_ids: if dragging {
                vec![20, 30, 10]
            } else {
                vec![10, 20, 30]
            },
            dragging,
            pending_op: None,
        }
    }

    #[test]
    fn project_drag_arms_only_for_the_current_generation_and_rendered_membership() {
        let ids = vec![10, 20, 30];
        let armed = arm_project_drag_preview(&ids, 4, 20, 4, ids.clone())
            .expect("a current-generation press on a rendered project arms");
        assert_eq!(
            armed.context,
            ProjectDragContext {
                source_id: 20,
                generation: 4,
            }
        );
        assert_eq!(armed.ordered_ids, ids);
        assert!(!armed.dragging, "a bare press is not yet a drag");

        assert!(arm_project_drag_preview(&ids, 5, 20, 4, ids.clone()).is_none());
        assert!(arm_project_drag_preview(&[10, 20], 4, 20, 4, ids.clone()).is_none());
        assert!(arm_project_drag_preview(&ids, 4, 99, 4, ids.clone()).is_none());
        assert!(arm_project_drag_preview(&[10, 0], 4, 10, 4, vec![10, 0]).is_none());
    }

    #[test]
    fn project_drag_threshold_gates_agent_row_hiding() {
        let context = ProjectDragContext {
            source_id: 10,
            generation: 4,
        };
        assert!(!agent_rows_hidden(None));

        let mut pressed = Some(project_drag_preview(false));
        assert!(
            !agent_rows_hidden(pressed.as_ref()),
            "an ordinary click must leave the agent rows in place"
        );
        assert!(mark_project_drag_dragging(&mut pressed, 4, &context));
        assert!(agent_rows_hidden(pressed.as_ref()));

        let mut stale = Some(project_drag_preview(false));
        assert!(!mark_project_drag_dragging(&mut stale, 5, &context));
        assert!(!mark_project_drag_dragging(
            &mut stale,
            4,
            &ProjectDragContext {
                source_id: 20,
                generation: 4,
            }
        ));
        assert!(!agent_rows_hidden(stale.as_ref()));

        let mut absent = None;
        assert!(!mark_project_drag_dragging(&mut absent, 4, &context));
    }

    #[test]
    fn project_drag_preview_updates_only_for_the_owning_gesture() {
        let ids = vec![10, 20, 30];
        let context = ProjectDragContext {
            source_id: 10,
            generation: 4,
        };
        let mut preview = Some(project_drag_preview(false));
        assert!(preview_project_drag_if_owned(
            &mut preview,
            &ids,
            4,
            &context,
            &ids,
            vec![20, 10, 30]
        ));
        assert_eq!(preview.as_ref().unwrap().ordered_ids, [20, 10, 30]);

        for (authoritative, strip_generation, context, ordered) in [
            (ids.clone(), 5, context.clone(), vec![30, 20, 10]),
            (vec![10, 20], 4, context.clone(), vec![30, 20, 10]),
            (ids.clone(), 4, context.clone(), vec![10, 20]),
            (
                ids.clone(),
                4,
                ProjectDragContext {
                    source_id: 20,
                    generation: 4,
                },
                vec![30, 20, 10],
            ),
        ] {
            assert!(!preview_project_drag_if_owned(
                &mut preview,
                &authoritative,
                strip_generation,
                &context,
                &ids,
                ordered
            ));
        }
        assert_eq!(preview.as_ref().unwrap().ordered_ids, [20, 10, 30]);
    }

    #[test]
    fn external_project_reorder_mid_drag_marks_the_preview_stale() {
        let preview = project_drag_preview(true);
        assert!(!project_drag_preview_is_stale(
            Some(&preview),
            &[10, 20, 30]
        ));
        assert!(project_drag_preview_is_stale(Some(&preview), &[30, 20, 10]));
        assert!(project_drag_preview_is_stale(
            Some(&preview),
            &[10, 20, 30, 40]
        ));
        assert!(project_drag_preview_is_stale(Some(&preview), &[10, 20]));
        assert!(!project_drag_preview_is_stale(None, &[10, 20]));
    }

    #[test]
    fn arming_either_strip_cancels_the_other_and_burns_its_generation() {
        let ids = vec![10, 20, 30];
        let mut tab_preview = Some(TabDragPreview {
            context: TabDragContext {
                project_id: 7,
                source_id: 10,
                generation: 4,
            },
            original_ids: vec![101, 102],
            ordered_ids: vec![102, 101],
            pending_op: None,
        });
        let mut tab_generation = 4;

        // A project press evicts the tab preview before arming its own.
        cancel_drag_preview(&mut tab_preview, &mut tab_generation);
        assert!(tab_preview.is_none());
        assert_eq!(tab_generation, 5);
        let mut project_preview = arm_project_drag_preview(&ids, 4, 10, 4, ids.clone());
        assert!(project_preview.is_some());

        // The reverse eviction burns the project generation, so the press the
        // strip already published against the old one can no longer arm.
        let mut project_generation = 4;
        cancel_drag_preview(&mut project_preview, &mut project_generation);
        assert!(project_preview.is_none());
        assert_eq!(project_generation, 5);
        assert!(arm_project_drag_preview(&ids, project_generation, 10, 4, ids.clone()).is_none());
    }

    #[test]
    fn project_drag_commit_dispatches_exactly_once_and_rejects_stale_or_noop_state() {
        let preview = project_drag_preview(true);
        assert!(project_drag_commit_is_valid(
            Some(&preview),
            &[10, 20, 30],
            &preview.context,
            &[10, 20, 30],
            &[20, 30, 10],
        ));

        for (authoritative, ordered) in [
            (vec![30, 20, 10], vec![20, 30, 10]),
            (vec![10, 20, 30], vec![10, 20, 30]),
            (vec![10, 20, 30], vec![20, 10, 30]),
        ] {
            assert!(
                !project_drag_commit_is_valid(
                    Some(&preview),
                    &authoritative,
                    &preview.context,
                    &[10, 20, 30],
                    &ordered,
                ),
                "{authoritative:?} → {ordered:?} is stale or a no-op"
            );
        }

        assert!(!project_drag_commit_is_valid(
            Some(&preview),
            &[10, 20, 30],
            &ProjectDragContext {
                source_id: 10,
                generation: 5,
            },
            &[10, 20, 30],
            &[20, 30, 10],
        ));
    }

    #[test]
    fn project_drag_settlement_is_owned_once_across_both_release_paths() {
        // The widget's own commit and the root release boundary build equal
        // requests, so whichever arrives first settles and the other is a
        // no-op regardless of order.
        let preview = project_drag_preview(true);
        let request = ProjectDragCommitRequest::from(&preview);
        let mut current = Some(preview);

        assert_eq!(
            settle_project_drag_commit(&mut current, &[10, 20, 30], request.clone(), 9),
            ProjectDragSettlement::Dispatched {
                ordered_ids: vec![20, 30, 10],
                op: 9,
            }
        );
        assert_eq!(
            current.as_ref().and_then(|preview| preview.pending_op),
            Some(9)
        );

        assert_eq!(
            settle_project_drag_commit(&mut current, &[10, 20, 30], request, 10),
            ProjectDragSettlement::Ignored
        );
        assert_eq!(
            current.as_ref().and_then(|preview| preview.pending_op),
            Some(9)
        );
    }

    /// The sidebar keeps drawing a dispatched preview even though its
    /// generation is already burned — that burn is what rejects the stale
    /// widget events, and reading it as "not current" would snap the
    /// projects back for the length of the round trip.
    #[test]
    fn a_dispatched_project_preview_still_orders_the_sidebar_after_the_generation_burn() {
        let armed = project_drag_preview(true);
        assert!(armed.orders_the_strip(4));
        assert!(!armed.orders_the_strip(5));

        let mut preview = Some(armed.clone());
        let request = ProjectDragCommitRequest::from(&armed);
        assert!(matches!(
            settle_project_drag_commit(&mut preview, &[10, 20, 30], request, 9),
            ProjectDragSettlement::Dispatched { op: 9, .. }
        ));
        let dispatched = preview.expect("the dispatched preview stays on screen");
        assert!(dispatched.orders_the_strip(5));
        // Everything else about the gesture ends at the release: the row
        // stops looking dragged and the agent sub-rows come back.
        assert!(!agent_rows_hidden(Some(&dispatched)));

        let tab = TabDragPreview {
            pending_op: Some(9),
            ..tab_drag_preview_at(4, vec![20, 30, 10])
        };
        assert!(!tab.drags(10), "the carried pill's styling ends too");
        assert!(tab_drag_preview_at(4, vec![20, 30, 10]).drags(10));
    }

    #[test]
    fn exact_subthreshold_project_end_clears_without_accepting_stale_or_moved_state() {
        let original = vec![10, 20, 30];
        let preview = project_drag_preview(false);
        let context = preview.context.clone();

        let mut exact = Some(preview.clone());
        assert!(end_project_drag_preview_if_owned(
            &mut exact, &original, &context, &original,
        ));
        assert!(exact.is_none());

        let mut stale = Some(preview.clone());
        assert!(!end_project_drag_preview_if_owned(
            &mut stale,
            &original,
            &ProjectDragContext {
                generation: 5,
                ..context.clone()
            },
            &original,
        ));
        assert_eq!(stale, Some(preview.clone()));

        let moved = project_drag_preview(true);
        let mut moved_state = Some(moved.clone());
        assert!(!end_project_drag_preview_if_owned(
            &mut moved_state,
            &original,
            &context,
            &original,
        ));
        assert_eq!(moved_state, Some(moved));
    }

    #[test]
    fn project_drag_commit_after_the_agent_row_hide_uses_the_gesture_id_list() {
        let ids = vec![10, 20, 30];
        let context = ProjectDragContext {
            source_id: 10,
            generation: 4,
        };
        let mut preview = arm_project_drag_preview(&ids, 4, 10, 4, ids.clone());
        assert!(!agent_rows_hidden(preview.as_ref()));
        assert!(mark_project_drag_dragging(&mut preview, 4, &context));
        assert!(agent_rows_hidden(preview.as_ref()));

        // Hiding the agent rows reflows every group, so the strip recomputes
        // its target index from live bounds; settlement compares ids only and
        // is unaffected by the changed geometry.
        assert!(preview_project_drag_if_owned(
            &mut preview,
            &ids,
            4,
            &context,
            &ids,
            vec![20, 30, 10]
        ));
        let request = ProjectDragCommitRequest::from(preview.as_ref().unwrap());
        assert_eq!(
            settle_project_drag_commit(&mut preview, &ids, request, 9),
            ProjectDragSettlement::Dispatched {
                ordered_ids: vec![20, 30, 10],
                op: 9,
            }
        );
        assert_eq!(preview.and_then(|preview| preview.pending_op), Some(9));
    }

    #[test]
    fn project_drag_commit_reorders_the_engine_project_list() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let workspace = Arc::new(Workspace::new());
        let mut ids = Vec::new();
        for name in ["first", "second", "third"] {
            let project = workspace.create_project(name, "/tmp").unwrap();
            workspace.open_tab(project.id, "/tmp", name).unwrap();
            ids.push(project.id);
        }
        let client = LocalClient::new(
            Arc::clone(&workspace),
            Arc::new(PtySupervisor::new()),
            "/tmp/roost-iced-project-drag-test.sock".into(),
        );
        let ordered = vec![ids[2], ids[0], ids[1]];
        let mut preview = Some(ProjectDragPreview {
            context: ProjectDragContext {
                source_id: ids[2],
                generation: 4,
            },
            original_ids: ids.clone(),
            ordered_ids: ordered.clone(),
            dragging: true,
            pending_op: None,
        });
        let request = ProjectDragCommitRequest::from(preview.as_ref().unwrap());

        let ProjectDragSettlement::Dispatched { ordered_ids, op } =
            settle_project_drag_commit(&mut preview, &ids, request, 9)
        else {
            panic!("a moved project drag settles into a dispatch");
        };
        assert_eq!(op, 9);
        runtime
            .block_on(client.reorder_projects(ordered_ids))
            .unwrap();

        // The completion clears the preview it dispatched; the sidebar has
        // been showing this order the whole time, so nothing moves.
        assert!(clear_dispatched_preview(&mut preview, 9, |preview| {
            preview.pending_op
        }));
        assert_eq!(
            workspace
                .snapshot()
                .iter()
                .map(|project| project.id)
                .collect::<Vec<_>>(),
            ordered
        );
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

    fn rename_editor_for(target: RenameTarget, draft: &str) -> Option<RenameEditor> {
        Some(RenameEditor {
            target,
            opened_label: "old".into(),
            draft: draft.into(),
        })
    }

    /// The failure semantics that predate the async dispatch: the engine
    /// said no, so the editor stays up holding the draft the user typed.
    /// Only the moment it is decided moved — from the blocking call's
    /// return to the completion.
    #[test]
    fn a_failed_rename_keeps_the_editor_open_with_its_draft() {
        let mut editor = rename_editor_for(RenameTarget::Project(7), "recover me");
        let mut pending = None;
        let mut in_flight = None;
        assert_eq!(
            plan_rename_submission_once(&mut editor, &mut pending, in_flight, 1),
            RenameSubmission::Dispatch {
                target: RenameTarget::Project(7),
                label: "recover me".into(),
                op: 1,
            }
        );
        assert_eq!(pending, Some(RenameCompletionKey::Enter));
        in_flight = Some(1);

        assert_eq!(
            resolve_rename_completion(
                &mut in_flight,
                &mut editor,
                1,
                &Err("injected failure".into())
            ),
            RenameCompletion::Failed
        );
        assert_eq!(
            editor.as_ref().map(|editor| editor.draft.clone()),
            Some("recover me".to_string()),
            "a failed rename must not eat the draft"
        );
        assert_eq!(
            in_flight, None,
            "the failure releases the guard for a retry"
        );

        // The captured key-release clears the Enter guard while the
        // TextInput keeps focus, so a deliberate retry dispatches again.
        pending = None;
        assert_eq!(
            plan_rename_submission_once(&mut editor, &mut pending, in_flight, 2),
            RenameSubmission::Dispatch {
                target: RenameTarget::Project(7),
                label: "recover me".into(),
                op: 2,
            }
        );
    }

    /// The Enter guard covers the key's own press/release pair; the op id
    /// covers everything after it, up to the completion. Without the
    /// second guard the released Enter would re-arm and submit again into
    /// an engine already renaming.
    #[test]
    fn a_rename_in_flight_refuses_a_second_submit() {
        let mut editor = rename_editor_for(RenameTarget::Tab(9), "new title");
        let mut pending = None;
        assert!(matches!(
            plan_rename_submission_once(&mut editor, &mut pending, None, 1),
            RenameSubmission::Dispatch { op: 1, .. }
        ));

        pending = None; // the captured Enter release
        assert_eq!(
            plan_rename_submission_once(&mut editor, &mut pending, Some(1), 2),
            RenameSubmission::InFlight
        );
        assert_eq!(pending, None, "a refused submit re-arms nothing");
        assert!(editor.is_some());
    }

    #[test]
    fn held_palette_enter_cannot_submit_the_editor_it_opens() {
        let mut editor = rename_editor_for(RenameTarget::Tab(9), "title");
        let mut pending = None;
        arm_rename_completion_for_open_editor(&mut pending, editor.is_some());
        assert_eq!(
            plan_rename_submission_once(&mut editor, &mut pending, None, 1),
            RenameSubmission::Settled
        );
        assert!(editor.is_some());

        pending = None; // captured release from the palette-confirming Enter
        assert_eq!(
            plan_rename_submission_once(&mut editor, &mut pending, None, 2),
            RenameSubmission::Dispatch {
                target: RenameTarget::Tab(9),
                label: "title".into(),
                op: 2,
            }
        );
    }

    #[test]
    fn rename_submit_trims_dispatches_exact_target_and_closes_on_success() {
        let mut editor = rename_editor_for(RenameTarget::Tab(42), "  new  title  ");
        assert_eq!(
            plan_rename_submission(&mut editor, 1),
            RenameSubmission::Dispatch {
                target: RenameTarget::Tab(42),
                label: "new  title".into(),
                op: 1,
            }
        );
        assert!(
            editor.is_some(),
            "the editor stays up until the engine confirms"
        );

        let mut in_flight = Some(1);
        assert_eq!(
            resolve_rename_completion(&mut in_flight, &mut editor, 1, &Ok(())),
            RenameCompletion::Closed
        );
        assert!(editor.is_none());
        assert_eq!(in_flight, None);
    }

    #[test]
    fn empty_rename_never_dispatches() {
        let mut empty = rename_editor_for(RenameTarget::Project(7), " \t ");
        assert_eq!(
            plan_rename_submission(&mut empty, 1),
            RenameSubmission::Settled
        );
        assert!(empty.is_none());

        let mut absent = None;
        assert_eq!(
            plan_rename_submission(&mut absent, 1),
            RenameSubmission::Settled
        );
    }

    /// The pinned concurrency case: the user dismisses the editor and
    /// opens another one over a different target before the first rename
    /// reports back. The stale completion must neither close the new
    /// editor nor raise its error over it.
    #[test]
    fn a_rename_completion_for_a_dismissed_editor_touches_the_reopened_one() {
        let mut editor = rename_editor_for(RenameTarget::Project(7), "first");
        let mut pending = None;
        let mut in_flight = None;
        assert!(matches!(
            plan_rename_submission_once(&mut editor, &mut pending, in_flight, 1),
            RenameSubmission::Dispatch { op: 1, .. }
        ));
        in_flight = Some(1);

        // Dismissal drops both the editor and the id it was waiting on —
        // `App::cancel_rename_editor` does exactly this pair — so the first
        // rename is already unowned before it reports back.
        assert_eq!(
            plan_rename_submission_once(&mut editor, &mut pending, in_flight, 2),
            RenameSubmission::InFlight
        );
        in_flight = None;
        editor = rename_editor_for(RenameTarget::Tab(9), "second");
        pending = None;

        let reopened = editor.clone();
        assert_eq!(
            resolve_rename_completion(&mut in_flight, &mut editor, 1, &Ok(())),
            RenameCompletion::Stale
        );
        assert_eq!(editor, reopened, "the stale success closed nothing");
        assert_eq!(
            resolve_rename_completion(
                &mut in_flight,
                &mut editor,
                1,
                &Err("first rename failed".into())
            ),
            RenameCompletion::Stale,
            "and its error belongs to no banner"
        );
        assert_eq!(editor, reopened);

        // The reopened editor submits on its own id and is answered by it.
        assert!(matches!(
            plan_rename_submission_once(&mut editor, &mut pending, in_flight, 2),
            RenameSubmission::Dispatch { op: 2, .. }
        ));
        in_flight = Some(2);
        assert_eq!(
            resolve_rename_completion(&mut in_flight, &mut editor, 2, &Ok(())),
            RenameCompletion::Closed
        );
        assert!(editor.is_none());
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

    fn page_press(named: Named, modifiers: keyboard::Modifiers) -> keyboard::Event {
        use iced::keyboard::key::{Code, Physical};
        use iced::keyboard::Location;

        let code = match named {
            Named::PageUp => Code::PageUp,
            _ => Code::PageDown,
        };
        keyboard::Event::KeyPressed {
            key: Key::Named(named),
            modified_key: Key::Named(named),
            physical_key: Physical::Code(code),
            location: Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        }
    }

    fn encode_page_press(tab: &mut TerminalTab, named: Named, modifiers: keyboard::Modifiers) {
        let bytes = input::encode_press(
            &mut tab.encoder,
            &tab.terminal,
            page_press(named, modifiers),
        );
        tab.session.send_input(bytes);
    }

    fn captured_input(tab: &TerminalTab) -> Vec<u8> {
        tab.input_capture.as_ref().unwrap().lock().unwrap().clone()
    }

    fn clear_captured_input(tab: &TerminalTab) {
        tab.input_capture.as_ref().unwrap().lock().unwrap().clear();
    }

    fn viewport_offset(tab: &TerminalTab) -> u64 {
        tab.terminal.scrollbar().expect("scrollbar").offset
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bare_pages_walk_local_history_without_reaching_the_pty() {
        let (mut tab, supervisor) = attached_test_terminal(194);
        for index in 0..200 {
            tab.write_vt(format!("history-{index:03}\r\n").as_bytes());
        }
        tab.refresh_snapshot().expect("baseline snapshot");
        clear_captured_input(&tab);
        let bottom = viewport_offset(&tab);
        assert_eq!(tab.dump().rows_text[0], format!("history-{bottom:03}"));

        let page = u64::from(DEFAULT_ROWS);
        for count in 1..=3 {
            assert_eq!(
                tab.handle_page(PageDirection::Up).expect("local page up"),
                PageRoute::LocalViewport {
                    scrolled_back: true
                }
            );
            let offset = viewport_offset(&tab);
            assert_eq!(bottom - offset, page * count, "page {count} moved one page");
            assert_eq!(
                tab.dump().rows_text[0],
                format!("history-{offset:03}"),
                "the published snapshot follows the local viewport"
            );
        }
        assert!(
            captured_input(&tab).is_empty(),
            "a local page must not send anything to the PTY"
        );

        assert_eq!(
            tab.handle_page(PageDirection::Down)
                .expect("local page down"),
            PageRoute::LocalViewport {
                scrolled_back: true
            }
        );
        assert_eq!(bottom - viewport_offset(&tab), page * 2);

        assert!(tab.scroll.is_scrolled_back());
        assert!(tab
            .snap_to_bottom_for_input()
            .expect("snap after local pages"));
        assert_eq!(viewport_offset(&tab), bottom);
        assert!(captured_input(&tab).is_empty());
        supervisor.close(194);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_page_preserves_the_active_selection() {
        let (mut tab, supervisor) = attached_test_terminal(195);
        for index in 0..200 {
            tab.write_vt(format!("history-{index:03}\r\n").as_bytes());
        }
        tab.refresh_snapshot().expect("baseline snapshot");
        assert!(tab.selection.set(&tab.terminal, (0, 0), (6, 0)));
        let selected = tab.selected_text().expect("selection text");
        assert_eq!(selected.as_deref(), Some("history"));

        assert_eq!(
            tab.handle_page(PageDirection::Up).expect("local page up"),
            PageRoute::LocalViewport {
                scrolled_back: true
            }
        );
        assert!(
            tab.selection.is_active(),
            "a local page must not drop the selection"
        );
        assert!(
            tab.snapshot.selection_spans.is_empty(),
            "the selected rows paged out of the viewport"
        );

        assert_eq!(
            tab.handle_page(PageDirection::Down)
                .expect("local page down"),
            PageRoute::LocalViewport {
                scrolled_back: false
            }
        );
        assert_eq!(
            tab.selected_text().expect("selection survives the page"),
            selected,
            "the same cells are selected once they are visible again"
        );
        supervisor.close(195);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pages_forward_to_the_application_on_tracking_and_alternate_screen() {
        let (mut tracked, tracked_supervisor) = attached_test_terminal(196);
        tracked.write_vt(b"\x1b[?1000h\x1b[?1006h");
        clear_captured_input(&tracked);
        assert_eq!(
            tracked
                .handle_page(PageDirection::Up)
                .expect("tracked page"),
            PageRoute::Forward
        );
        encode_page_press(&mut tracked, Named::PageUp, keyboard::Modifiers::empty());
        assert_eq!(captured_input(&tracked), b"\x1b[5~");
        tracked_supervisor.close(196);

        let (mut alternate, alternate_supervisor) = attached_test_terminal(197);
        alternate.write_vt(b"\x1b[?1049h");
        clear_captured_input(&alternate);
        assert_eq!(
            alternate
                .handle_page(PageDirection::Up)
                .expect("alternate-screen page up"),
            PageRoute::Forward
        );
        assert!(
            captured_input(&alternate).is_empty(),
            "forwarding is the app's encode, not the policy's"
        );
        encode_page_press(&mut alternate, Named::PageUp, keyboard::Modifiers::empty());
        assert_eq!(captured_input(&alternate), b"\x1b[5~");

        clear_captured_input(&alternate);
        assert_eq!(
            alternate
                .handle_page(PageDirection::Down)
                .expect("alternate-screen page down"),
            PageRoute::Forward
        );
        encode_page_press(
            &mut alternate,
            Named::PageDown,
            keyboard::Modifiers::empty(),
        );
        assert_eq!(captured_input(&alternate), b"\x1b[6~");
        assert!(!alternate.scroll.is_scrolled_back());
        alternate_supervisor.close(197);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn modified_page_keys_stay_on_the_encode_path() {
        let (mut tab, supervisor) = attached_test_terminal(198);
        for index in 0..200 {
            tab.write_vt(format!("history-{index:03}\r\n").as_bytes());
        }
        clear_captured_input(&tab);
        let bottom = viewport_offset(&tab);

        assert_eq!(
            input::bare_page_direction(
                &page_press(Named::PageUp, keyboard::Modifiers::SHIFT),
                keyboard::Modifiers::SHIFT
            ),
            None
        );
        encode_page_press(&mut tab, Named::PageUp, keyboard::Modifiers::SHIFT);
        assert_eq!(captured_input(&tab), b"\x1b[5;2~");
        assert_eq!(
            viewport_offset(&tab),
            bottom,
            "a modified page key belongs to the application, not the viewport"
        );
        supervisor.close(198);
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
