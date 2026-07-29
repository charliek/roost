//! Agent palette — the pure frame builder for the `agents` frame
//! (plan 005 §3.2–§3.6).
//!
//! One row per tab an agent owns: status dot + project + name + status
//! text + elapsed time (+ git metrics, filled asynchronously). Every
//! rendered value is derived here from the core workspace snapshot, so
//! the GTK overlay stays a dumb renderer and the whole mapping —
//! population filter, status vocabulary, name fallback, time buckets,
//! ordering — is unit-testable without GTK.
//!
//! The lifecycle input is [`agent::effective_lifecycle`], the same value
//! the tab pill, the TabPage icon, and the sidebar rollup render, so the
//! palette can never disagree with them. Ordering reuses
//! [`agent::rank`], the shipped definition of "most urgent".
//!
//! To be mirrored by the Mac side — `mac/Sources/Roost/` has no agents
//! frame yet, so this is the reference implementation of the mapping.

use std::time::{SystemTime, UNIX_EPOCH};

use roost_ipc::agent::{self, AgentLifecycle, Ownership, SOURCE_LEGACY, SOURCE_MANUAL};
use roost_ipc::messages::{Project, Tab};

use crate::notification_inbox;
use crate::palette::{AgentRowData, PaletteFrame, PaletteItem};

/// Frame id — also the `palette.open` kind and what `palette.state`
/// reports.
pub const FRAME_ID: &str = "agents";
pub const PLACEHOLDER: &str = "Go to agent…";
/// Muted hint bar under the list.
pub const FOOTER_HINTS: &str = "↑↓ move  ↵ go to tab  esc close";
/// The empty-state row. Deliberately not parseable as `agent:<id>`.
pub const EMPTY_ROW_ID: &str = "agents:empty";
pub const EMPTY_ROW_TITLE: &str = "No agent sessions";

const ROW_ID_PREFIX: &str = "agent:";
/// `detail` is an open string from an arbitrary adapter; cap what we
/// render so one long line can't blow out the row.
const DETAIL_MAX_CHARS: usize = 40;
/// The repo's two non-agent internal ownership sources (`tab.set_state`
/// claims as `manual`, the deprecated `tab.set_hook_active` as
/// `legacy`). Any *other* source is presumed an agent, so a third-party
/// adapter shows up without a whitelist (AD-8). Taken from the contract
/// crate rather than re-spelled, so renaming a source can't silently
/// un-filter the palette.
const NON_AGENT_SOURCES: [&str; 2] = [SOURCE_MANUAL, SOURCE_LEGACY];
/// Key under `ownership.metadata` carrying the agent's own session name.
const SESSION_TITLE_KEY: &str = "session_title";

/// Wall-clock seconds — the same scale `ownership.last_event_at` is
/// stamped in (server receipt time).
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// The root/sub frame for the agents palette.
pub fn agent_frame(projects: &[Project], now: i64) -> PaletteFrame {
    PaletteFrame::new(FRAME_ID, PLACEHOLDER, agent_items(projects, now))
        .with_footer_hints(FOOTER_HINTS)
}

/// Rows for every agent-owned tab in the snapshot, in `rank` order.
/// Empty populations yield the single non-actionable sentinel row.
pub fn agent_items(projects: &[Project], now: i64) -> Vec<PaletteItem> {
    let mut rows: Vec<Row> = Vec::new();
    for project in projects {
        for tab in &project.tabs {
            let axes = tab.agent_state();
            if !agent::is_live(&axes) {
                continue;
            }
            let Some(owner) = axes.ownership.as_ref() else {
                continue;
            };
            if NON_AGENT_SOURCES.contains(&owner.source.as_str()) {
                continue;
            }
            rows.push(row_for(
                project,
                tab,
                owner,
                agent::effective_lifecycle(&axes),
                now,
            ));
        }
    }
    if rows.is_empty() {
        return vec![PaletteItem::new(EMPTY_ROW_ID, EMPTY_ROW_TITLE).with_actionable(false)];
    }
    // Total order (§3.5): urgency, then recency, then the workspace's
    // own deterministic layout order. Whole-second `last_event_at` ties
    // are common, so the positional tiebreaks are load-bearing.
    rows.sort_by(|a, b| {
        b.rank
            .cmp(&a.rank)
            .then(b.last_event_at.cmp(&a.last_event_at))
            .then(a.project_position.cmp(&b.project_position))
            .then(a.tab_position.cmp(&b.tab_position))
            .then(a.tab_id.cmp(&b.tab_id))
    });
    rows.into_iter().map(|r| r.item).collect()
}

/// The tab id an agent row activates, or `None` for the empty sentinel
/// (and any other row id).
pub fn agent_tab_id(row_id: &str) -> Option<i64> {
    row_id.strip_prefix(ROW_ID_PREFIX)?.parse().ok()
}

/// CSS class carrying a lifecycle's colour (dot background + status
/// text). Defined in `resources/style.css` alongside the sidebar
/// stripes, which use the same palette.
pub fn lifecycle_class(lifecycle: AgentLifecycle) -> &'static str {
    match lifecycle {
        AgentLifecycle::Working => "agent-working",
        AgentLifecycle::Waiting => "agent-waiting",
        AgentLifecycle::Finished => "agent-finished",
        AgentLifecycle::Failed => "agent-failed",
        AgentLifecycle::Inactive => "agent-inactive",
    }
}

/// The row's status column (§3.3). `effective` picks the vocabulary;
/// `raw` gates the background-tasks exception so only a genuinely
/// working *agent* (not a shell-derived "Working") can report it.
pub fn status_text(effective: AgentLifecycle, raw: AgentLifecycle, detail: &str) -> String {
    match effective {
        AgentLifecycle::Working => match background_tasks(raw, detail) {
            Some(1) => "Working · 1 bg task".to_string(),
            Some(n) => format!("Working · {n} bg tasks"),
            None => "Working".to_string(),
        },
        AgentLifecycle::Waiting => "Waiting for input".to_string(),
        AgentLifecycle::Finished => "Finished".to_string(),
        AgentLifecycle::Failed => {
            let detail = display_detail(detail);
            if detail.is_empty() {
                "Failed".to_string()
            } else {
                format!("Failed · {detail}")
            }
        }
        AgentLifecycle::Inactive => "Idle".to_string(),
    }
}

/// Elapsed since `last_event_at` (§3.6). Clock skew — a stamp in the
/// future — clamps to `"0s"` rather than rendering a negative age.
///
/// Only the sub-minute bucket differs from the notification inbox's
/// label (`"Ns"` here, `"just now"` there); the m/h/d edges are shared
/// so the two lists can't drift apart.
pub fn elapsed_text(now: i64, last_event_at: i64) -> String {
    let secs = now.saturating_sub(last_event_at).max(0) as u64;
    if secs < 60 {
        return format!("{secs}s");
    }
    notification_inbox::relative_time(secs)
}

/// The fuzzy-match input and the generic-client fallback title. One
/// composition, used everywhere — the filter matches exactly what the
/// row shows.
pub fn compose_title(project: &str, name: &str) -> String {
    if project.is_empty() {
        return name.to_string();
    }
    notification_inbox::compose_title(project, name)
}

/// A row plus its sort keys.
struct Row {
    rank: u8,
    last_event_at: i64,
    project_position: i32,
    tab_position: i32,
    tab_id: i64,
    item: PaletteItem,
}

fn row_for(
    project: &Project,
    tab: &Tab,
    owner: &Ownership,
    effective: AgentLifecycle,
    now: i64,
) -> Row {
    let project_name = normalize_line(&project.name);
    let name = row_name(tab, owner);
    let item = PaletteItem::new(
        format!("{ROW_ID_PREFIX}{}", tab.id),
        compose_title(&project_name, &name),
    )
    .with_agent(AgentRowData {
        effective_lifecycle: effective,
        project: project_name,
        name,
        status_text: status_text(effective, tab.agent_lifecycle, &owner.detail),
        time_text: elapsed_text(now, owner.last_event_at),
        // Filled by the git-metrics probe; absent means pending.
        metrics_text: None,
    });
    Row {
        rank: agent::rank(effective),
        last_event_at: owner.last_event_at,
        project_position: project.position,
        tab_position: tab.position,
        tab_id: tab.id,
        item,
    }
}

/// The agent's own session name when it published one, else the tab
/// title, else a stable placeholder.
fn row_name(tab: &Tab, owner: &Ownership) -> String {
    let session_title = owner
        .metadata
        .get(SESSION_TITLE_KEY)
        .map(|t| normalize_line(t))
        .unwrap_or_default();
    if !session_title.is_empty() {
        return session_title;
    }
    let title = normalize_line(&tab.title);
    if !title.is_empty() {
        return title;
    }
    format!("Tab {}", tab.id)
}

/// `Some(n)` for a working agent reporting `background_tasks:N` with
/// `N >= 1`. Anything else — a shell-derived "Working", a foreign
/// detail an adapter left behind, a malformed / zero / negative count —
/// is `None` and renders plain "Working".
fn background_tasks(raw: AgentLifecycle, detail: &str) -> Option<i64> {
    if raw != AgentLifecycle::Working {
        return None;
    }
    let n: i64 = normalize_line(detail)
        .strip_prefix("background_tasks:")?
        .parse()
        .ok()?;
    (n >= 1).then_some(n)
}

fn display_detail(detail: &str) -> String {
    truncate_chars(&normalize_line(detail), DETAIL_MAX_CHARS)
}

/// Collapse an open string to one printable line. `metadata` and
/// `detail` come from arbitrary adapters, so a newline or an escape
/// sequence must not be able to reshape a palette row.
fn normalize_line(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

/// Tail-ellipsize to at most `max` characters *including* the ellipsis.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ipc::agent::ShellState;
    use roost_ipc::messages::TabState;
    use std::collections::BTreeMap;

    const NOW: i64 = 1_700_000_000;

    fn tab(id: i64, title: &str) -> Tab {
        Tab {
            id,
            project_id: 1,
            title: title.to_string(),
            cwd: "/tmp".to_string(),
            state: TabState::None,
            has_notification: false,
            is_active: false,
            user_titled: false,
            position: 0,
            created_at: 0,
            last_active: 0,
            hook_active: false,
            shell_state: ShellState::AtPrompt,
            agent_lifecycle: AgentLifecycle::Inactive,
            ownership: None,
        }
    }

    fn owned(mut tab: Tab, source: &str, lifecycle: AgentLifecycle, at: i64) -> Tab {
        tab.agent_lifecycle = lifecycle;
        tab.ownership = Some(Ownership {
            source: source.to_string(),
            session_id: "s1".to_string(),
            last_event_at: at,
            detail: String::new(),
            metadata: BTreeMap::new(),
        });
        tab
    }

    /// Mutate an owned tab's `Ownership`. Panics rather than silently
    /// skipping if the tab isn't owned, so a mis-built fixture fails
    /// loudly instead of testing the wrong thing.
    fn with_owner(mut tab: Tab, edit: impl FnOnce(&mut Ownership)) -> Tab {
        edit(tab.ownership.as_mut().expect("owned tab"));
        tab
    }

    fn project(id: i64, name: &str, tabs: Vec<Tab>) -> Project {
        Project {
            id,
            name: name.to_string(),
            cwd: "/tmp".to_string(),
            position: 0,
            created_at: 0,
            tabs,
        }
    }

    fn agent_of(item: &PaletteItem) -> &AgentRowData {
        item.agent.as_ref().expect("agent row payload")
    }

    // ----- status text ----------------------------------------------

    #[test]
    fn status_text_covers_every_lifecycle() {
        use AgentLifecycle::*;
        assert_eq!(status_text(Working, Working, ""), "Working");
        assert_eq!(status_text(Waiting, Waiting, ""), "Waiting for input");
        assert_eq!(status_text(Finished, Finished, ""), "Finished");
        assert_eq!(status_text(Failed, Failed, ""), "Failed");
        assert_eq!(status_text(Inactive, Inactive, ""), "Idle");
    }

    #[test]
    fn failed_appends_its_detail() {
        assert_eq!(
            status_text(AgentLifecycle::Failed, AgentLifecycle::Failed, "rate_limit"),
            "Failed · rate_limit"
        );
    }

    #[test]
    fn failed_detail_is_capped_at_forty_chars() {
        let long = "x".repeat(60);
        let text = status_text(AgentLifecycle::Failed, AgentLifecycle::Failed, &long);
        let detail = text.strip_prefix("Failed · ").expect("detail suffix");
        assert_eq!(detail.chars().count(), 40);
        assert!(detail.ends_with('…'));
        // Exactly 40 is left alone.
        let exact = "y".repeat(40);
        let text = status_text(AgentLifecycle::Failed, AgentLifecycle::Failed, &exact);
        assert_eq!(text, format!("Failed · {exact}"));
    }

    #[test]
    fn background_tasks_detail_renders_singular_and_plural() {
        use AgentLifecycle::Working;
        assert_eq!(
            status_text(Working, Working, "background_tasks:1"),
            "Working · 1 bg task"
        );
        assert_eq!(
            status_text(Working, Working, "background_tasks:2"),
            "Working · 2 bg tasks"
        );
    }

    #[test]
    fn malformed_background_tasks_falls_back_to_plain_working() {
        use AgentLifecycle::Working;
        for detail in [
            "background_tasks:",
            "background_tasks:abc",
            "background_tasks:0",
            "background_tasks:-3",
            "background_tasks:1.5",
            "background tasks:2",
        ] {
            assert_eq!(
                status_text(Working, Working, detail),
                "Working",
                "detail {detail:?} must not render a count"
            );
        }
    }

    #[test]
    fn foreign_detail_on_a_working_row_is_ignored() {
        // `apply_report` preserves prior detail on empty-detail reports,
        // so a non-Claude adapter can leave stale detail behind.
        assert_eq!(
            status_text(
                AgentLifecycle::Working,
                AgentLifecycle::Working,
                "permission_prompt"
            ),
            "Working"
        );
    }

    #[test]
    fn shell_derived_working_never_shows_a_background_count() {
        // Effective is Working (foreground process) while the agent axis
        // is Inactive — the count belongs to the agent, not the shell.
        assert_eq!(
            status_text(
                AgentLifecycle::Working,
                AgentLifecycle::Inactive,
                "background_tasks:2"
            ),
            "Working"
        );
    }

    #[test]
    fn status_text_normalizes_control_characters() {
        assert_eq!(
            status_text(
                AgentLifecycle::Failed,
                AgentLifecycle::Failed,
                "rate\nlimit\r\u{1b}[31m"
            ),
            "Failed · ratelimit[31m"
        );
    }

    // ----- elapsed time ---------------------------------------------

    #[test]
    fn elapsed_bucket_edges() {
        assert_eq!(elapsed_text(NOW, NOW), "0s");
        assert_eq!(elapsed_text(NOW, NOW - 59), "59s");
        assert_eq!(elapsed_text(NOW, NOW - 60), "1m");
        assert_eq!(elapsed_text(NOW, NOW - 3_599), "59m");
        assert_eq!(elapsed_text(NOW, NOW - 3_600), "1h");
        assert_eq!(elapsed_text(NOW, NOW - 86_399), "23h");
        assert_eq!(elapsed_text(NOW, NOW - 86_400), "1d");
        assert_eq!(elapsed_text(NOW, NOW - 172_800), "2d");
    }

    #[test]
    fn elapsed_clamps_a_future_stamp() {
        assert_eq!(elapsed_text(NOW, NOW + 5), "0s");
        assert_eq!(elapsed_text(NOW, i64::MAX), "0s");
    }

    // ----- population -----------------------------------------------

    #[test]
    fn only_agent_owned_tabs_are_listed() {
        let projects = vec![project(
            1,
            "roost",
            vec![
                tab(1, "plain shell"),
                owned(tab(2, "claude"), "claude", AgentLifecycle::Working, NOW),
                owned(tab(3, "manual"), "manual", AgentLifecycle::Working, NOW),
                owned(tab(4, "legacy"), "legacy", AgentLifecycle::Working, NOW),
                owned(tab(5, "codex"), "codex", AgentLifecycle::Waiting, NOW),
            ],
        )];
        let ids: Vec<String> = agent_items(&projects, NOW)
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(ids, vec!["agent:5", "agent:2"]);
    }

    #[test]
    fn an_empty_source_is_not_live() {
        let t = with_owner(
            owned(tab(1, "ghost"), "claude", AgentLifecycle::Working, NOW),
            |owner| owner.source = String::new(),
        );
        let items = agent_items(&[project(1, "roost", vec![t])], NOW);
        assert_eq!(items[0].id, EMPTY_ROW_ID);
    }

    #[test]
    fn a_freshly_claimed_and_a_failsafed_tab_still_appear() {
        // SessionStart claims with raw `inactive`; the dead-agent
        // failsafe forces raw `inactive` while keeping ownership. Both
        // fall through to the shell axis.
        let mut fresh = owned(tab(1, "claude"), "claude", AgentLifecycle::Inactive, NOW);
        fresh.shell_state = ShellState::AtPrompt;
        let mut busy = owned(tab(2, "claude"), "claude", AgentLifecycle::Inactive, NOW);
        busy.shell_state = ShellState::ForegroundProcess;
        let items = agent_items(&[project(1, "roost", vec![fresh, busy])], NOW);
        assert_eq!(items.len(), 2);
        // Shell-derived: the busy one ranks above the idle one.
        assert_eq!(items[0].id, "agent:2");
        assert_eq!(agent_of(&items[0]).status_text, "Working");
        assert_eq!(agent_of(&items[1]).status_text, "Idle");
    }

    #[test]
    fn empty_population_yields_one_non_actionable_row() {
        let items = agent_items(&[project(1, "roost", vec![tab(1, "shell")])], NOW);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, EMPTY_ROW_ID);
        assert_eq!(items[0].title, EMPTY_ROW_TITLE);
        assert!(!items[0].actionable);
        assert!(items[0].agent.is_none());
        // The sentinel must not parse as a jump target.
        assert_eq!(agent_tab_id(EMPTY_ROW_ID), None);
    }

    #[test]
    fn row_id_round_trips_to_a_tab_id() {
        assert_eq!(agent_tab_id("agent:42"), Some(42));
        assert_eq!(agent_tab_id("agent:"), None);
        assert_eq!(agent_tab_id("agent:x"), None);
        assert_eq!(agent_tab_id("notif:7"), None);
    }

    // ----- name + title ---------------------------------------------

    #[test]
    fn name_prefers_session_title_then_tab_title_then_placeholder() {
        let with_meta = with_owner(
            owned(tab(1, "zsh"), "claude", AgentLifecycle::Working, NOW),
            |owner| {
                owner
                    .metadata
                    .insert(SESSION_TITLE_KEY.to_string(), "slauth-refactor".to_string());
            },
        );
        let with_title = owned(tab(2, "zsh"), "claude", AgentLifecycle::Working, NOW);
        let untitled = owned(tab(3, ""), "claude", AgentLifecycle::Working, NOW);

        let items = agent_items(
            &[project(1, "roost", vec![with_meta, with_title, untitled])],
            NOW,
        );
        let by_id = |id: &str| {
            items
                .iter()
                .find(|i| i.id == id)
                .expect("row present")
                .clone()
        };
        assert_eq!(agent_of(&by_id("agent:1")).name, "slauth-refactor");
        assert_eq!(agent_of(&by_id("agent:2")).name, "zsh");
        assert_eq!(agent_of(&by_id("agent:3")).name, "Tab 3");
        // Title composition is the filter input, verbatim.
        assert_eq!(by_id("agent:1").title, "roost · slauth-refactor");
    }

    #[test]
    fn a_blank_session_title_falls_through_to_the_tab_title() {
        let t = with_owner(
            owned(tab(1, "zsh"), "claude", AgentLifecycle::Working, NOW),
            |owner| {
                owner
                    .metadata
                    .insert(SESSION_TITLE_KEY.to_string(), "  \n ".to_string());
            },
        );
        let items = agent_items(&[project(1, "roost", vec![t])], NOW);
        assert_eq!(agent_of(&items[0]).name, "zsh");
    }

    #[test]
    fn names_are_normalized_to_one_line() {
        let t = with_owner(
            owned(tab(1, "zsh"), "claude", AgentLifecycle::Working, NOW),
            |owner| {
                owner.metadata.insert(
                    SESSION_TITLE_KEY.to_string(),
                    "line one\nline two\r\t x".to_string(),
                );
            },
        );
        let items = agent_items(&[project(1, "roost\nnope", vec![t])], NOW);
        let row = agent_of(&items[0]);
        assert_eq!(row.name, "line oneline two x");
        assert_eq!(row.project, "roostnope");
        assert_eq!(items[0].title, "roostnope · line oneline two x");
    }

    #[test]
    fn compose_title_without_a_project_is_just_the_name() {
        assert_eq!(compose_title("", "claude"), "claude");
        assert_eq!(compose_title("roost", "claude"), "roost · claude");
    }

    // ----- ordering -------------------------------------------------

    #[test]
    fn rows_order_by_rank_then_recency() {
        let tabs = vec![
            owned(tab(1, "finished"), "claude", AgentLifecycle::Finished, NOW),
            owned(tab(2, "working"), "claude", AgentLifecycle::Working, NOW),
            owned(tab(3, "failed"), "claude", AgentLifecycle::Failed, NOW),
            owned(
                tab(4, "waiting-old"),
                "claude",
                AgentLifecycle::Waiting,
                NOW - 500,
            ),
            owned(
                tab(5, "waiting-new"),
                "claude",
                AgentLifecycle::Waiting,
                NOW,
            ),
        ];
        let ids: Vec<String> = agent_items(&[project(1, "roost", tabs)], NOW)
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(
            ids,
            vec!["agent:3", "agent:5", "agent:4", "agent:2", "agent:1"]
        );
    }

    #[test]
    fn same_second_ties_break_by_project_then_tab_position_then_id() {
        // Whole-second stamps make ties the common case, so the
        // positional tiebreaks decide the visible order.
        let mut second = project(
            2,
            "shed",
            vec![owned(tab(10, "a"), "claude", AgentLifecycle::Working, NOW)],
        );
        second.position = 1;

        let mut later_tab = owned(tab(20, "b"), "claude", AgentLifecycle::Working, NOW);
        later_tab.position = 5;
        let mut same_position_high_id = owned(tab(30, "c"), "claude", AgentLifecycle::Working, NOW);
        same_position_high_id.position = 0;
        let mut same_position_low_id = owned(tab(3, "d"), "claude", AgentLifecycle::Working, NOW);
        same_position_low_id.position = 0;

        let first = project(
            1,
            "roost",
            vec![later_tab, same_position_high_id, same_position_low_id],
        );
        let ids: Vec<String> = agent_items(&[second, first], NOW)
            .into_iter()
            .map(|i| i.id)
            .collect();
        // Project position 0 first (despite being second in the input),
        // then tab position, then tab id within a position.
        assert_eq!(ids, vec!["agent:3", "agent:30", "agent:20", "agent:10"]);
    }

    // ----- row payload ----------------------------------------------

    #[test]
    fn row_payload_carries_the_effective_lifecycle_and_pending_metrics() {
        let t = with_owner(
            owned(tab(7, "zsh"), "claude", AgentLifecycle::Waiting, NOW - 120),
            |owner| owner.detail = "permission_prompt".to_string(),
        );
        let items = agent_items(&[project(1, "roost", vec![t])], NOW);
        let row = agent_of(&items[0]);
        assert_eq!(row.effective_lifecycle, AgentLifecycle::Waiting);
        assert_eq!(row.project, "roost");
        assert_eq!(row.name, "zsh");
        assert_eq!(row.status_text, "Waiting for input");
        assert_eq!(row.time_text, "2m");
        assert_eq!(row.metrics_text, None, "metrics are pending until probed");
    }

    #[test]
    fn frame_carries_the_footer_hints_and_placeholder() {
        let frame = agent_frame(&[], NOW);
        assert_eq!(frame.id, FRAME_ID);
        assert_eq!(frame.placeholder, PLACEHOLDER);
        assert_eq!(frame.footer_hints.as_deref(), Some(FOOTER_HINTS));
        assert_eq!(frame.items.len(), 1);
        assert_eq!(frame.items[0].id, EMPTY_ROW_ID);
    }

    #[test]
    fn lifecycle_classes_are_distinct() {
        use AgentLifecycle::*;
        let classes = [Working, Waiting, Finished, Failed, Inactive].map(lifecycle_class);
        let unique: std::collections::HashSet<_> = classes.iter().collect();
        assert_eq!(unique.len(), classes.len());
        assert_eq!(lifecycle_class(Working), "agent-working");
        assert_eq!(lifecycle_class(Inactive), "agent-inactive");
    }
}
