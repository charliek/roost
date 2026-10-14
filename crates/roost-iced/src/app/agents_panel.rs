//! The sidebar's Agents panel (plan 080 D4–D7, D10, D11): its header,
//! rows, group heads and empty state, the clock that keeps its ages
//! current, and what `app.sidebar_dump` reports of it.
//!
//! Every line is derived from [`App::agent_panel_rows`] and the current
//! sort when it is drawn or dumped, so a sort change shows on the next
//! frame with no refresh in between.

use iced::widget::text::{LineHeight, Shaping, Wrapping};
use iced::widget::{column, rich_text, row, scrollable, span};
use iced::Padding;
use roost_ipc::agent::AgentLifecycle;
use roost_ipc::messages::{SidebarDumpAgentPanel, SidebarDumpPanelEntry};
use roost_ui_model::agent_panel::{self, AgentPanelSort, PanelAgentRow, PanelEntry};

use super::*;
use crate::agents_split::{AgentsSplitGrip, PanelHeight};

/// How often the panel's ages are recomputed while one is counting.
pub(crate) const AGENTS_CLOCK_INTERVAL: Duration = Duration::from_secs(15);

const AGENTS_LABEL: &str = "AGENTS";
const EMPTY_TEXT: &str = "No agents running";

/// What each line of the panel may spend at a sidebar width: whatever the
/// row's own chrome — insets, paddings, the dot, the gaps, the age and
/// the scrollbar — leaves.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TextBudgets {
    name: f32,
    detail: f32,
    group: f32,
}

fn text_budgets(sidebar_width: f32) -> TextBudgets {
    let inside = sidebar_width
        - chrome::DIVIDER_WIDTH
        - chrome::AGENT_PANEL_SCROLLBAR_WIDTH
        - 2.0 * chrome::AGENT_PANEL_INSET_X
        - chrome::AGENT_PANEL_ROW_PADDING_LEFT
        - chrome::AGENT_PANEL_ROW_PADDING_RIGHT;
    let detail = inside - chrome::AGENT_PANEL_DOT_SLOT - chrome::AGENT_PANEL_GAP;
    TextBudgets {
        name: (detail - chrome::AGENT_PANEL_GAP - chrome::AGENT_PANEL_AGE_SLOT).max(0.0),
        detail: detail.max(0.0),
        group: (inside - chrome::AGENT_PANEL_GROUP_COUNT_SLOT).max(0.0),
    }
}

/// A row's second line in full — its status, after its location when it
/// has one, joined as the palette joins them — and how many of its bytes
/// are the location's.
fn detail_line(location: Option<&str>, status: &str) -> (String, usize) {
    let full = agent_palette::compose_title(location.unwrap_or_default(), status);
    let prefix = full.len() - status.len();
    (full, prefix)
}

/// `elided`, a tail elision of a detail line whose location part is
/// `prefix`, split where that part ends: the status gets its own color,
/// and keeps it on whatever of it survived the cut.
fn split_detail(mut elided: String, prefix: &str) -> (String, String) {
    if !elided.starts_with(prefix) {
        return (elided, String::new());
    }
    let status = elided.split_off(prefix.len());
    (elided, status)
}

fn permille(fraction: f32) -> u16 {
    (fraction * 1000.0).round().clamp(0.0, 1000.0) as u16
}

/// How tall the panel is: hidden by the toggle, else split at `fraction`
/// with room for one whole agent row — under a head, when there are heads.
/// It never asks whether any agent is running (plan 080 R1): an empty
/// panel keeps that room, so nothing moves when the first agent starts or
/// the last one exits.
fn panel_height(visible: bool, sort: AgentPanelSort, fraction: f32) -> PanelHeight {
    if !visible {
        return PanelHeight::Hidden;
    }
    let head = if sort == AgentPanelSort::Grouped {
        chrome::AGENT_PANEL_GROUP_HEIGHT
    } else {
        0.0
    };
    PanelHeight::Split {
        fraction,
        floor: chrome::AGENT_PANEL_HEADER_HEIGHT
            + chrome::AGENT_PANEL_LIST_PADDING_Y
            + head
            + chrome::AGENT_PANEL_ROW_HEIGHT,
    }
}

/// Whether the divider is on screen: the panel is shown, in a sidebar that
/// is not collapsed.
fn divider_drawn(panel: PanelHeight, sidebar_collapsed: bool) -> bool {
    !sidebar_collapsed && matches!(panel, PanelHeight::Split { .. })
}

impl App {
    /// The sidebar's project list over its Agents panel (plan 080 D1).
    pub(super) fn agents_split<'a>(
        &'a self,
        projects: impl Into<Element<'a, Message>>,
    ) -> Element<'a, Message> {
        AgentsSplitGrip::new(
            &self.chrome,
            projects,
            self.agent_panel(),
            self.agent_panel_height(),
            self.agents_split_drag.epoch(),
        )
        .into()
    }

    /// The panel's lines, in the current sort.
    fn agent_panel_entries(&self) -> Vec<PanelEntry> {
        // A host's rank is its band's place in the sidebar, not its place
        // in the registry (plan 080 D4). The first band to name a host
        // keeps it: a never-connected host's band carries the local
        // placeholder id, and must not move the local band.
        let mut band_rank = HashMap::new();
        for (index, section) in self.host_sections.iter().enumerate() {
            band_rank.entry(section.host).or_insert(index);
        }
        agent_panel::panel_entries(
            &self.agent_panel_rows,
            self.config.agent_panel_sort,
            &band_rank,
            self.panel_names_hosts(),
        )
    }

    /// Whether the panel's text names hosts at all: only when the sidebar
    /// draws more than one band (plan 080 D3).
    fn panel_names_hosts(&self) -> bool {
        self.host_sections.len() > 1
    }

    /// A row's location outside `grouped`, where no head above it says it.
    fn agent_location(&self, row: &PanelAgentRow) -> Option<String> {
        (self.config.agent_panel_sort != AgentPanelSort::Grouped).then(|| {
            agent_panel::location_text(
                row.panel_label.as_deref(),
                &row.project_name,
                self.panel_names_hosts(),
            )
        })
    }

    /// The panel's share of the split: a drag's while one is in flight,
    /// else the workspace's.
    fn agents_panel_fraction(&self) -> f32 {
        self.agents_split_drag
            .fraction(self.workspace.agents_panel_fraction() as f32)
    }

    fn agent_panel_height(&self) -> PanelHeight {
        panel_height(
            self.config.show_sidebar_agents,
            self.config.agent_panel_sort,
            self.agents_panel_fraction(),
        )
    }

    fn agents_divider_drawn(&self) -> bool {
        divider_drawn(
            self.agent_panel_height(),
            self.workspace.sidebar_collapsed(),
        )
    }

    pub fn agents_split_dragged(&mut self, fraction: f32, epoch: u64) {
        let drawn = self.agents_divider_drawn();
        self.agents_split_drag.dragged(epoch, fraction, drawn);
    }

    pub fn agents_split_ended(&mut self, epoch: u64) {
        if let Some(fraction) = self.agents_split_drag.ended(epoch) {
            self.workspace
                .set_agents_panel_fraction(f64::from(fraction));
        }
    }

    /// Ends a divider drag from the app's side, writing its fraction
    /// through. The grip lets its own half of the drag go too.
    pub(super) fn commit_agents_split_drag(&mut self) {
        if let Some(fraction) = self.agents_split_drag.interrupt() {
            self.workspace
                .set_agents_panel_fraction(f64::from(fraction));
        }
    }

    /// Ends a divider drag whose divider a change just took away.
    pub(super) fn settle_agents_split_drag(&mut self) {
        let drawn = self.agents_divider_drawn();
        if let Some(fraction) = self.agents_split_drag.settle(drawn) {
            self.workspace
                .set_agents_panel_fraction(f64::from(fraction));
        }
    }

    /// The panel: its header over its rows, or over the one line saying
    /// there are none, with the list's background below it.
    fn agent_panel(&self) -> Element<'_, Message> {
        let header = self.agent_panel_header();
        let body: Element<'_, Message> = if !self.config.show_sidebar_agents {
            Space::new().into()
        } else if self.agent_panel_rows.is_empty() {
            container(
                text(EMPTY_TEXT)
                    .size(chrome::AGENT_PANEL_EMPTY_SIZE)
                    .color(self.chrome.host_rollup_text)
                    .wrapping(Wrapping::None),
            )
            .width(Fill)
            .center_y(chrome::AGENT_PANEL_EMPTY_HEIGHT)
            .padding(
                Padding::ZERO
                    .left(chrome::AGENT_PANEL_INSET_X + chrome::AGENT_PANEL_ROW_PADDING_LEFT),
            )
            .into()
        } else {
            let budgets = text_budgets(self.live_sidebar_width());
            let active = self.active_tab_key();
            let lines = self
                .agent_panel_entries()
                .into_iter()
                .map(|entry| match entry {
                    PanelEntry::Group { label, count } => {
                        self.agent_panel_group_head(&label, count, budgets.group)
                    }
                    PanelEntry::Agent(row) => {
                        let active = row.tab == active;
                        self.agent_panel_row(row, active, budgets)
                    }
                });
            scrollable(column(lines).padding([chrome::AGENT_PANEL_LIST_PADDING_Y, 0.0]))
                .id(self.agent_panel_scroll_id.clone())
                .direction(scrollable::Direction::Vertical(
                    scrollable::Scrollbar::new()
                        .width(chrome::AGENT_PANEL_SCROLLBAR_WIDTH)
                        .scroller_width(chrome::AGENT_PANEL_SCROLLBAR_WIDTH),
                ))
                .width(Fill)
                .height(Fill)
                .into()
        };
        container(column![header, body])
            .width(Fill)
            .height(Fill)
            .style(chrome::list(&self.chrome))
            .into()
    }

    /// `AGENTS` on a band like every other, and the sort word that cycles
    /// it (plan 080 D7).
    fn agent_panel_header(&self) -> Element<'_, Message> {
        let sort = button(
            text(self.config.agent_panel_sort.as_str())
                .size(chrome::AGENT_PANEL_SORT_SIZE)
                .font(chrome::chrome_font(font::Weight::Medium))
                .wrapping(Wrapping::None),
        )
        .padding(chrome::AGENT_PANEL_SORT_PADDING)
        .style(chrome::agent_panel_sort_button(&self.chrome))
        .on_press(Message::CycleAgentPanelSort);
        sidebar_band(
            &self.chrome,
            row![
                sidebar_band_label(&self.chrome, AGENTS_LABEL),
                Space::new().width(Fill),
                sort
            ]
            .align_y(Alignment::Center),
        )
    }

    fn agent_panel_group_head(
        &self,
        label: &str,
        count: usize,
        budget: f32,
    ) -> Element<'_, Message> {
        let font = chrome::chrome_font(font::Weight::Semibold);
        let color = self.chrome.agent_group_text;
        let label =
            self.sidebar_elisions
                .elide(label, font, chrome::AGENT_PANEL_GROUP_SIZE, budget);
        let line = LineHeight::Absolute(chrome::AGENT_PANEL_GROUP_LINE.into());
        container(
            row![
                text(label)
                    .size(chrome::AGENT_PANEL_GROUP_SIZE)
                    .line_height(line)
                    .font(font)
                    .color(color)
                    .width(Fill)
                    .shaping(Shaping::Advanced)
                    .wrapping(Wrapping::None),
                text(count.to_string())
                    .size(chrome::AGENT_PANEL_GROUP_SIZE)
                    .line_height(line)
                    .color(color)
                    .width(chrome::AGENT_PANEL_GROUP_COUNT_SLOT)
                    .align_x(iced::alignment::Horizontal::Right)
                    .wrapping(Wrapping::None),
            ]
            .align_y(Alignment::Center),
        )
        .width(Fill)
        .height(chrome::AGENT_PANEL_GROUP_HEIGHT)
        .padding(Padding {
            top: chrome::AGENT_PANEL_GROUP_PADDING_TOP,
            right: chrome::AGENT_PANEL_INSET_X + chrome::AGENT_PANEL_ROW_PADDING_RIGHT,
            bottom: chrome::AGENT_PANEL_GROUP_PADDING_BOTTOM,
            left: chrome::AGENT_PANEL_INSET_X + chrome::AGENT_PANEL_ROW_PADDING_LEFT,
        })
        .into()
    }

    /// One agent: dot, name and age; then its status, after its location
    /// outside `grouped`. Nothing wraps — both lines are elided to the
    /// width the sidebar leaves them.
    fn agent_panel_row(
        &self,
        agent: PanelAgentRow,
        active: bool,
        budgets: TextBudgets,
    ) -> Element<'_, Message> {
        let alpha = if agent.dimmed {
            chrome::HOST_SECTION_DIM
        } else {
            1.0
        };
        let location = self.agent_location(&agent);
        let font = chrome::chrome_font(font::Weight::Normal);
        let lifecycle_color = agent_color(&self.chrome, agent.lifecycle).scale_alpha(alpha);
        let muted = self.chrome.muted_text.scale_alpha(alpha);
        let status_color = match agent.lifecycle {
            AgentLifecycle::Waiting | AgentLifecycle::Failed => lifecycle_color,
            _ => muted,
        };
        let dot = container(
            Space::new()
                .width(chrome::AGENT_PANEL_DOT_SIZE)
                .height(chrome::AGENT_PANEL_DOT_SIZE),
        )
        .style(chrome::rounded_fill(
            lifecycle_color,
            chrome::AGENT_PANEL_DOT_RADIUS,
        ));
        let name = self.sidebar_elisions.elide(
            &agent.name,
            font,
            chrome::AGENT_PANEL_NAME_SIZE,
            budgets.name,
        );
        let name_line = LineHeight::Absolute(chrome::AGENT_PANEL_NAME_LINE.into());
        let first = row![
            container(dot).center_x(chrome::AGENT_PANEL_DOT_SLOT),
            text(name)
                .size(chrome::AGENT_PANEL_NAME_SIZE)
                .line_height(name_line)
                .font(font)
                .color(self.chrome.text.scale_alpha(alpha))
                .width(Fill)
                .shaping(Shaping::Advanced)
                .wrapping(Wrapping::None),
            text(agent.time_text)
                .size(chrome::AGENT_PANEL_DETAIL_SIZE)
                .line_height(name_line)
                .font(font)
                .color(muted)
                .width(chrome::AGENT_PANEL_AGE_SLOT)
                .align_x(iced::alignment::Horizontal::Right)
                .wrapping(Wrapping::None),
        ]
        .spacing(chrome::AGENT_PANEL_GAP)
        .align_y(Alignment::Center)
        .height(chrome::AGENT_PANEL_NAME_LINE);
        let (full, prefix) = detail_line(location.as_deref(), &agent.status_text);
        let elided = self.sidebar_elisions.elide(
            &full,
            font,
            chrome::AGENT_PANEL_DETAIL_SIZE,
            budgets.detail,
        );
        let (where_, status) = split_detail(elided, &full[..prefix]);
        let second = row![
            Space::new().width(chrome::AGENT_PANEL_DOT_SLOT),
            rich_text([
                span::<(), Font>(where_).color(muted),
                span(status).color(status_color),
            ])
            .size(chrome::AGENT_PANEL_DETAIL_SIZE)
            .line_height(LineHeight::Absolute(chrome::AGENT_PANEL_DETAIL_LINE.into()))
            .font(font)
            .wrapping(Wrapping::None),
        ]
        .spacing(chrome::AGENT_PANEL_GAP)
        .height(chrome::AGENT_PANEL_DETAIL_LINE);
        // A dimmed row keeps its `button`, so its layout is identical; with
        // no `on_press` iced draws it Disabled, which no press can act on.
        let mut row_button = button(column![first, second])
            .width(Fill)
            .height(chrome::AGENT_PANEL_ROW_HEIGHT)
            .padding(Padding {
                top: chrome::AGENT_PANEL_ROW_PADDING_Y,
                right: chrome::AGENT_PANEL_ROW_PADDING_RIGHT,
                bottom: chrome::AGENT_PANEL_ROW_PADDING_Y,
                left: chrome::AGENT_PANEL_ROW_PADDING_LEFT,
            })
            .style(chrome::agent_panel_row(&self.chrome, active));
        if !agent.dimmed {
            row_button = row_button.on_press(Message::AgentSelected(agent.tab));
        }
        container(row_button)
            .padding([0.0, chrome::AGENT_PANEL_INSET_X])
            .into()
    }

    /// `app.sidebar_dump`'s view of the panel: the same lines the panel
    /// draws, with their text in full.
    pub(super) fn agent_panel_dump(&self) -> SidebarDumpAgentPanel {
        let active = self.active_tab_key();
        let entries = self
            .agent_panel_entries()
            .into_iter()
            .map(|entry| match entry {
                PanelEntry::Group { label, count } => SidebarDumpPanelEntry::Group {
                    label,
                    count: u32::try_from(count).unwrap_or(u32::MAX),
                },
                PanelEntry::Agent(row) => SidebarDumpPanelEntry::Agent {
                    tab: row.tab.to_wire(),
                    location_text: self.agent_location(&row).unwrap_or_default(),
                    is_active: row.tab == active,
                    name: row.name,
                    agent: row.agent,
                    lifecycle: row.lifecycle,
                    status_text: row.status_text,
                    time_text: row.time_text,
                    dimmed: row.dimmed,
                },
            })
            .collect();
        SidebarDumpAgentPanel {
            visible: self.config.show_sidebar_agents,
            sort: self.config.agent_panel_sort.as_str().to_string(),
            fraction_permille: permille(self.agents_panel_fraction()),
            entries,
        }
    }

    /// Whether an age on screen is still counting: the panel is showing,
    /// and some agent in it is working or waiting (plan 080 D11). An idle
    /// app arms no timer.
    pub fn agents_clock_armed(&self) -> bool {
        self.config.show_sidebar_agents
            && !self.workspace.sidebar_collapsed()
            && self.agent_panel_rows.iter().any(|row| {
                matches!(
                    row.lifecycle,
                    AgentLifecycle::Working | AgentLifecycle::Waiting
                )
            })
    }

    pub fn agents_clock_tick(&mut self) {
        self.refresh_sidebar_agents();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents_split::AgentsSplitDrag;

    /// The fixed slots hold the text they are sized for, so an age or a
    /// count never spills into the name beside it.
    #[test]
    fn the_age_and_count_slots_hold_their_widest_text() {
        let normal = chrome::chrome_font(font::Weight::Normal);
        for age in ["59s", "59m", "23h", "99d"] {
            let width = chrome::text_width(age, normal, chrome::AGENT_PANEL_DETAIL_SIZE);
            assert!(width <= chrome::AGENT_PANEL_AGE_SLOT, "{age} is {width}px");
        }
        let width = chrome::text_width("99", normal, chrome::AGENT_PANEL_GROUP_SIZE);
        assert!(width <= chrome::AGENT_PANEL_GROUP_COUNT_SLOT, "{width}px");
        let width = chrome::text_width("99", normal, chrome::PROJECT_AGENT_COUNT_SIZE);
        assert!(width <= chrome::PROJECT_AGENT_COUNT_SLOT, "{width}px");
    }

    /// At the narrowest sidebar the engine allows, every line still has
    /// room for some text.
    #[test]
    fn the_narrowest_sidebar_still_leaves_every_line_room() {
        let budgets = text_budgets(roost_engine::SIDEBAR_MIN_WIDTH as f32);
        assert!(budgets.name >= 40.0, "{budgets:?}");
        assert!(budgets.detail > budgets.name, "{budgets:?}");
        assert!(budgets.group >= 80.0, "{budgets:?}");
        assert_eq!(text_budgets(0.0).name, 0.0, "a budget never goes negative");
    }

    #[test]
    fn a_detail_line_is_its_status_after_its_location() {
        assert_eq!(
            detail_line(Some("popos \u{203a} craze"), "Waiting for input"),
            (
                "popos \u{203a} craze \u{b7} Waiting for input".to_string(),
                "popos \u{203a} craze \u{b7} ".len()
            )
        );
        assert_eq!(detail_line(None, "Working"), ("Working".to_string(), 0));
    }

    /// The status keeps its color on whatever of it survives the cut, and
    /// a cut inside the location leaves no status to color.
    #[test]
    fn split_detail_colors_only_the_status_that_survived() {
        let (full, prefix) = detail_line(Some("local \u{203a} roost"), "Failed");
        let location = &full[..prefix];
        assert_eq!(
            split_detail(full.clone(), location),
            (
                "local \u{203a} roost \u{b7} ".to_string(),
                "Failed".to_string()
            )
        );
        let cut_in_status = format!("{location}Fai{}", chrome::ELLIPSIS);
        assert_eq!(
            split_detail(cut_in_status, location),
            (location.to_string(), format!("Fai{}", chrome::ELLIPSIS))
        );
        let cut_in_location = format!("local \u{203a} ro{}", chrome::ELLIPSIS);
        assert_eq!(
            split_detail(cut_in_location.clone(), location),
            (cut_in_location, String::new())
        );
        assert_eq!(
            split_detail(chrome::ELLIPSIS.to_string(), location),
            (chrome::ELLIPSIS.to_string(), String::new())
        );
        let (status_only, none) = detail_line(None, "Working");
        assert_eq!(
            split_detail(status_only.clone(), &status_only[..none]),
            (String::new(), "Working".to_string())
        );
    }

    #[test]
    fn the_fraction_is_reported_in_whole_thousandths() {
        assert_eq!(
            permille(roost_engine::AGENTS_PANEL_DEFAULT_FRACTION as f32),
            370
        );
        assert_eq!(permille(0.1234), 123);
        assert_eq!(permille(2.0), 1000);
        assert_eq!(permille(-1.0), 0);
    }

    // ── The divider's drag (plan 080 D8) ────────────────────────────
    //
    // roost-iced has no `App` fixture, so each `App` path is pinned on
    // the rule it calls.

    const STORED: f32 = 0.37;

    fn mid_drag() -> AgentsSplitDrag {
        let mut drag = AgentsSplitDrag::default();
        drag.dragged(drag.epoch(), 0.3, true);
        drag
    }

    #[test]
    fn a_drag_shows_its_fraction_until_its_release_commits_it() {
        let mut drag = AgentsSplitDrag::default();
        let epoch = drag.epoch();
        assert_eq!(drag.fraction(STORED), STORED);
        drag.dragged(epoch, 0.3, true);
        drag.dragged(epoch, 0.32, true);
        assert_eq!(drag.fraction(STORED), 0.32, "the panel shows the drag");
        assert_eq!(
            drag.ended(epoch),
            Some(0.32),
            "the release commits the last move"
        );
        assert_eq!(drag.fraction(STORED), STORED, "then the stored share again");
        assert_eq!(drag.ended(epoch), None, "a second release commits nothing");
    }

    #[test]
    fn a_move_for_a_divider_that_is_gone_starts_no_drag() {
        let mut drag = AgentsSplitDrag::default();
        let epoch = drag.epoch();
        drag.dragged(epoch, 0.3, false);
        assert_eq!(drag.fraction(STORED), STORED);
        assert_eq!(drag.ended(epoch), None);
    }

    #[test]
    fn a_toast_interrupt_commits_the_drag_with_the_divider_still_drawn() {
        let mut drag = mid_drag();
        assert_eq!(drag.settle(true), None);
        assert_eq!(drag.interrupt(), Some(0.3));
    }

    fn drawn(visible: bool, sidebar_collapsed: bool) -> bool {
        divider_drawn(
            panel_height(visible, AgentPanelSort::Grouped, 0.3),
            sidebar_collapsed,
        )
    }

    #[test]
    fn toggling_the_panel_off_mid_drag_commits_the_drag() {
        let mut drag = mid_drag();
        assert_eq!(drag.settle(drawn(false, false)), Some(0.3));
    }

    #[test]
    fn collapsing_the_sidebar_mid_drag_commits_the_drag() {
        let mut drag = mid_drag();
        assert_eq!(drag.settle(drawn(true, true)), Some(0.3));
    }

    /// A change that keeps the divider — an agent starting, the last one
    /// exiting, a sort change — leaves the drag to the grip.
    #[test]
    fn a_change_that_keeps_the_divider_keeps_the_drag() {
        for sort in [
            AgentPanelSort::Grouped,
            AgentPanelSort::Priority,
            AgentPanelSort::Recent,
        ] {
            let mut drag = mid_drag();
            let panel = panel_height(true, sort, drag.fraction(STORED));
            assert_eq!(drag.settle(divider_drawn(panel, false)), None, "{sort:?}");
            assert_eq!(drag.fraction(STORED), 0.3);
        }
    }

    #[test]
    fn a_shown_panel_splits_with_room_for_one_row_of_its_sort() {
        assert_eq!(
            panel_height(false, AgentPanelSort::Grouped, 0.3),
            PanelHeight::Hidden
        );
        let row = chrome::AGENT_PANEL_HEADER_HEIGHT
            + chrome::AGENT_PANEL_LIST_PADDING_Y
            + chrome::AGENT_PANEL_ROW_HEIGHT;
        for sort in [AgentPanelSort::Priority, AgentPanelSort::Recent] {
            assert_eq!(
                panel_height(true, sort, 0.3),
                PanelHeight::Split {
                    fraction: 0.3,
                    floor: row,
                },
                "{sort:?}"
            );
        }
        assert_eq!(
            panel_height(true, AgentPanelSort::Grouped, 0.3),
            PanelHeight::Split {
                fraction: 0.3,
                floor: row + chrome::AGENT_PANEL_GROUP_HEIGHT,
            },
            "under grouped, the one row has its head above it"
        );
    }
}
