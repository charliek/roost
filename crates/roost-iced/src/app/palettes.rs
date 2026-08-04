use super::*;

// Widget operations can observe an incomplete tree while a slower renderer or
// compositor is still materializing a newly-pushed palette frame. Retry on
// later application ticks for roughly two seconds at the 60 Hz subscription,
// while keeping the work bounded and revision-scoped.
const PALETTE_GEOMETRY_RETRY_LIMIT: u8 = 120;

pub(super) const PALETTE_AGENT_PROJECT_MAX_COLUMNS: usize = 24;

// Name and status share the width left after project and the reserved
// metrics/time column. Preserve both when they fit. Under genuine pressure,
// retain a useful status tail and let the usually-longer name ellipsize first.
// Unicode display columns, rather than scalar counts, keep wide labels honest.
const PALETTE_AGENT_LEFT_MAX_COLUMNS: usize = 58;

const PALETTE_AGENT_STATUS_FLOOR_COLUMNS: usize = 18;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PaletteTextRun {
    pub(super) text: String,
    pub(super) matched: bool,
}

pub(super) fn palette_title_runs(title: &str, ranges: &[Range<usize>]) -> Vec<PaletteTextRun> {
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

pub(super) fn ellipsize_palette_text(value: &str, max_columns: usize) -> String {
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

pub(super) fn palette_agent_left_text(name: &str, status: &str) -> (String, String) {
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
pub(super) struct ApplyRollbackFailure<E> {
    pub(super) apply: E,
    pub(super) rollback: Option<E>,
}

pub(super) fn apply_with_rollback<T, E>(
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

fn retain_palette_focus_after_back<T>(
    requested: &mut bool,
    palette_open: bool,
    result: Result<T, String>,
) -> Result<T, String> {
    *requested = palette_open;
    result
}

fn take_palette_focus_request(requested: &mut bool, input_id: &Id) -> UiTask {
    if std::mem::take(requested) {
        UiTask::FocusWidget(input_id.clone())
    } else {
        UiTask::None
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum PaletteVisibilityRequest {
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

pub(super) fn palette_row_id(session: u64, revision: u64, index: usize) -> Id {
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

pub(crate) struct ProviderRunResult {
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

#[derive(Debug, Clone, Copy)]
pub(super) enum FontSizeTransition {
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

impl App {
    pub(super) fn open_bound_palette_result(&mut self, kind: &str) -> Result<UiTask, String> {
        self.open_palette(kind)?;
        Ok(self.take_palette_focus_task())
    }

    pub(super) fn apply_font_size_transition(
        &mut self,
        transition: FontSizeTransition,
    ) -> Result<(), String> {
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
        let (cols, rows) = terminal_grid(self.window_size, self.effective_sidebar_width(), metrics);
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

    pub(super) fn open_palette(&mut self, kind: &str) -> Result<(), String> {
        self.cancel_drags();
        self.cancel_editor_for_interaction();
        self.cancel_confirm_delete();
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

    pub(super) fn present_palette(
        &mut self,
        title: String,
        placeholder: String,
        items: Vec<(String, String, Option<String>)>,
        reply: tokio::sync::oneshot::Sender<Result<PalettePresentResult, String>>,
    ) {
        self.cancel_drags();
        self.cancel_editor_for_interaction();
        self.cancel_confirm_delete();
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

    pub(super) fn take_palette_focus_task(&mut self) -> UiTask {
        take_palette_focus_request(&mut self.palette_focus_requested, &self.palette_input_id)
    }

    pub(super) fn invalidate_palette_geometry(&mut self, request: PaletteVisibilityRequest) {
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

    pub(super) fn take_palette_visibility_task(&mut self) -> UiTask {
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

    pub(super) fn palette_state_result(&self) -> PaletteStateResult {
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

    pub(super) fn query_palette(&mut self, query: &str) -> Result<PaletteStateResult, String> {
        let state = self
            .palette
            .as_mut()
            .ok_or_else(|| "no palette open".to_string())?;
        state.set_query(query);
        self.invalidate_palette_geometry(PaletteVisibilityRequest::Reveal);
        self.preview_selected_palette_item()?;
        Ok(self.palette_state_result())
    }

    pub(super) fn move_palette_selection(&mut self, delta: isize) {
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

    pub(super) fn activate_palette(&mut self, id: &str) -> Result<PaletteStateResult, String> {
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
                "new_project" => {
                    self.clear_palette_state();
                    self.new_project_result()?;
                }
                "close_project" => {
                    let project_id = self.workspace.active().0;
                    self.clear_palette_state();
                    self.confirm_close_project(project_id)?;
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

    pub(super) fn palette_back_or_dismiss(&mut self) {
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

    pub(super) fn dismiss_palette_with_focus_recovery(&mut self) {
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

    pub(super) fn try_dismiss_palette(&mut self) -> Result<(), String> {
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

    pub(super) fn refresh_notification_palette(&mut self) {
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

    pub(super) fn refresh_agent_palette(&mut self) {
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
        let feed = self.feed_tx.clone();
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
            feed.send(EngineFeed::Provider(Box::new(ProviderRunResult {
                palette_session,
                request,
                origin_frame,
                provider: result_provider,
                phase,
                outcome,
            })));
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

    pub(super) fn apply_provider_result(&mut self, result: ProviderRunResult) {
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
            return;
        }
        let before_layout = self.palette_layout_signature();
        let before_render = self.palette_render_signature();
        match result.outcome {
            Ok(output) if result.phase == provider::Phase::Activate && output.items.is_empty() => {
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
                let items = vec![
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

#[cfg(test)]
mod tests {
    use super::*;

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
