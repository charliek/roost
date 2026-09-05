//! Toolkit-neutral agent palette frame builder for the `agents` frame
//! (plan 005 §3.2–§3.6).
//!
//! One row per tab an agent owns: status dot + project + name + status
//! text + elapsed time (+ git metrics, filled asynchronously). Every
//! rendered value is derived here from the core workspace snapshot, so
//! each UI adapter's overlay stays a dumb renderer and the whole mapping —
//! population filter, status vocabulary, name fallback, time buckets,
//! ordering — is unit-testable without a UI toolkit.
//!
//! The lifecycle input is [`agent::effective_lifecycle`], the same value
//! the tab pill, the TabPage icon, and the sidebar rollup render, so the
//! palette can never disagree with them. Ordering reuses
//! [`agent::rank`], the shipped definition of "most urgent".
//!
//! To be mirrored by the Mac side — `mac/Sources/Roost/` has no agents
//! frame yet, so this is the reference implementation of the mapping.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use roost_ipc::agent::{self, AgentLifecycle, Ownership, SOURCE_LEGACY, SOURCE_MANUAL};
use roost_ipc::messages::{Project, Tab};

use crate::keys::{HostId, TabKey};
use crate::notification_inbox;
use crate::palette::{AgentRowData, PaletteFrame, PaletteItem};

/// Frame id — also the `palette.open` kind and what `palette.state`
/// reports.
pub const FRAME_ID: &str = "agents";
pub const PLACEHOLDER: &str = "Go to agent…";
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

/// One workspace's contribution to the agents palette — the local one,
/// or a connected host's mirror (plan 037 §3.1: agent surfaces are
/// host-blind).
#[derive(Debug, Clone, Copy)]
pub struct AgentSource<'a> {
    pub projects: &'a [Project],
    /// The instance whose id-space `projects` carries.
    pub host: HostId,
    /// The saved host's label, appended to each row's context as
    /// `project · host`. `None` for the local workspace — there is
    /// nothing to disambiguate it from, and a bare install's rows must
    /// read exactly as they do today.
    pub label: Option<&'a str>,
}

impl<'a> AgentSource<'a> {
    pub fn local(projects: &'a [Project], host: HostId) -> Self {
        Self {
            projects,
            host,
            label: None,
        }
    }
}

/// The agents frame across every source, in one merged `rank` order.
pub fn agent_frame_across(sources: &[AgentSource<'_>], now: i64) -> PaletteFrame {
    PaletteFrame::new(FRAME_ID, PLACEHOLDER, agent_items_across(sources, now))
}

/// Rows for every agent-owned tab in the snapshot, in `rank` order.
/// Empty populations yield the single non-actionable sentinel row.
pub fn agent_items(projects: &[Project], host: HostId, now: i64) -> Vec<PaletteItem> {
    agent_items_across(&[AgentSource::local(projects, host)], now)
}

/// [`agent_items`] over several workspaces at once: needs-attention
/// first regardless of which host a row came from, and rows from a host
/// carry `project · host` as their context so the fuzzy filter can find
/// them by machine.
pub fn agent_items_across(sources: &[AgentSource<'_>], now: i64) -> Vec<PaletteItem> {
    let mut rows: Vec<Row> = Vec::new();
    for (source_index, source) in sources.iter().enumerate() {
        for project in source.projects {
            for tab in &project.tabs {
                let axes = tab.agent_state();
                let Some(owner) = agent_owner(&axes) else {
                    continue;
                };
                rows.push(row_for(
                    project,
                    source,
                    source_index,
                    tab,
                    owner,
                    agent::effective_lifecycle(&axes),
                    now,
                ));
            }
        }
    }
    if rows.is_empty() {
        return vec![PaletteItem::new(EMPTY_ROW_ID, EMPTY_ROW_TITLE).with_actionable(false)];
    }
    // Total order (§3.5): urgency, then recency, then the workspace's
    // own deterministic layout order. Whole-second `last_event_at` ties
    // are common, so the positional tiebreaks are load-bearing — and
    // `source_index` is one of them now, since two hosts' projects can
    // sit at the same position.
    rows.sort_by(|a, b| {
        b.rank
            .cmp(&a.rank)
            .then(b.last_event_at.cmp(&a.last_event_at))
            .then(a.source_index.cmp(&b.source_index))
            .then(a.project_position.cmp(&b.project_position))
            .then(a.tab_position.cmp(&b.tab_position))
            .then(a.tab_id.cmp(&b.tab_id))
    });
    rows.into_iter().map(|r| r.item).collect()
}

/// The working directory of every tab that gets a row, keyed by tab id.
///
/// The row payload carries no path (it renders none), but the git-metrics
/// probe and the fill both key off the tab's cwd — so the population
/// filter is shared with [`agent_items`] rather than re-spelled in
/// `app.rs`, where it could drift.
pub fn agent_tab_cwds(projects: &[Project], host: HostId) -> HashMap<TabKey, String> {
    agent_tab_cwds_across(&[AgentSource::local(projects, host)])
}

/// [`agent_tab_cwds`] over several workspaces at once, keyed the same
/// way — a host tab's cwd is a path on that host, so the probe simply
/// finds no repo there and the column stays muted.
pub fn agent_tab_cwds_across(sources: &[AgentSource<'_>]) -> HashMap<TabKey, String> {
    let mut cwds = HashMap::new();
    for source in sources {
        for project in source.projects {
            for tab in &project.tabs {
                if agent_owner(&tab.agent_state()).is_some() {
                    cwds.insert(TabKey::new(source.host, tab.id), tab.cwd.clone());
                }
            }
        }
    }
    cwds
}

/// One row under a project in the sidebar (plan 007 §3.1) — the same
/// fields the palette renders, scoped to a single project. There is no
/// `project` field: the sidebar nests this row under its project
/// visually, so the row never needs to say which one it's in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarAgentRow {
    pub tab: TabKey,
    pub name: String,
    pub lifecycle: AgentLifecycle,
    pub status_text: String,
    pub time_text: String,
}

/// A single project's agent rows, in the palette's within-project order:
/// rank ↓, `last_event_at` ↓, tab position, tab id. `project_position`
/// drops out of the key relative to [`agent_items`]'s — every row here
/// already shares one project, so ordering across projects is the
/// caller's problem (it owns the project list order already).
pub fn sidebar_agents(project: &Project, host: HostId, now: i64) -> Vec<SidebarAgentRow> {
    struct Keyed {
        rank: u8,
        last_event_at: i64,
        tab_position: i32,
        tab_id: i64,
        row: SidebarAgentRow,
    }
    let mut rows: Vec<Keyed> = Vec::new();
    for tab in &project.tabs {
        let axes = tab.agent_state();
        let Some(owner) = agent_owner(&axes) else {
            continue;
        };
        let effective = agent::effective_lifecycle(&axes);
        rows.push(Keyed {
            rank: agent::rank(effective),
            last_event_at: owner.last_event_at,
            tab_position: tab.position,
            tab_id: tab.id,
            row: SidebarAgentRow {
                tab: TabKey::new(host, tab.id),
                name: row_name(tab, owner),
                lifecycle: effective,
                status_text: status_text(effective, tab.agent_lifecycle, &owner.detail),
                time_text: elapsed_text(now, owner.last_event_at),
            },
        });
    }
    rows.sort_by(|a, b| {
        b.rank
            .cmp(&a.rank)
            .then(b.last_event_at.cmp(&a.last_event_at))
            .then(a.tab_position.cmp(&b.tab_position))
            .then(a.tab_id.cmp(&b.tab_id))
    });
    rows.into_iter().map(|k| k.row).collect()
}

/// The ownership record of a tab that belongs in the palette: live
/// ownership from a source that isn't one of Roost's own internal
/// (non-agent) claims. `None` means "no row for this tab".
fn agent_owner(axes: &agent::AgentTabState) -> Option<&Ownership> {
    if !agent::is_live(axes) {
        return None;
    }
    let owner = axes.ownership.as_ref()?;
    (!NON_AGENT_SOURCES.contains(&owner.source.as_str())).then_some(owner)
}

/// The tab an agent row activates, or `None` for the empty sentinel
/// (and any other row id).
pub fn agent_tab_key(row_id: &str) -> Option<TabKey> {
    TabKey::from_wire(row_id.strip_prefix(ROW_ID_PREFIX)?)
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

/// A human-friendly name for an ownership `source` (plan 046 §3.7,
/// W7). Matched against the five adapters' own `SOURCE` constants in
/// `crates/roost-agent/src/*.rs` — spelled out here rather than
/// imported, since `roost-agent` parses hook JSON and has no reason to
/// depend on this crate's rendering, or vice versa.
///
/// `source` is an open string by design (AD-8): a third-party adapter
/// that isn't one of the five renders **verbatim** rather than being
/// hidden or mislabeled, so this table can never gate whether an
/// agent's row appears — only how its name is spelled. Mirrored by
/// `mac/Sources/Roost/AgentPalette.swift`'s `agentDisplayName`, tested
/// against the same fixture (`tests/agent-display-name-fixtures/`).
pub fn agent_display_name(source: &str) -> &str {
    match source {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "opencode" => "OpenCode",
        "grok" => "Grok",
        "cursor" => "Cursor",
        other => other,
    }
}

/// The colour role of one segment of the git-metrics column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricsRole {
    Muted,
    Adds,
    Dels,
}

/// Split the metrics string into rendered segments, alternating
/// non-whitespace tokens with standalone whitespace runs. Concatenating
/// every segment's text reproduces the input byte-for-byte, so the
/// column can never render text the probe didn't produce.
///
/// The grammar is `git_metrics`' own (`"{files}f +{adds} -{dels}"` or
/// `"—"`), but the rule here is deliberately shape-based rather than a
/// parse of that grammar: a sign followed by at least one digit is a
/// count, everything else — the file count, the unknown dash, a bare
/// sign, an unrecognized token — stays muted.
pub fn metrics_segments(text: &str) -> Vec<(&str, MetricsRole)> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let space = rest.starts_with(char::is_whitespace);
        let end = rest
            .find(|c: char| c.is_whitespace() != space)
            .unwrap_or(rest.len());
        let (segment, tail) = rest.split_at(end);
        let role = if space {
            MetricsRole::Muted
        } else {
            token_role(segment)
        };
        out.push((segment, role));
        rest = tail;
    }
    out
}

fn token_role(token: &str) -> MetricsRole {
    let (role, rest) = if let Some(rest) = token.strip_prefix('+') {
        (MetricsRole::Adds, rest)
    } else if let Some(rest) = token.strip_prefix('-') {
        (MetricsRole::Dels, rest)
    } else {
        return MetricsRole::Muted;
    };
    if rest.starts_with(|c: char| c.is_ascii_digit()) {
        role
    } else {
        MetricsRole::Muted
    }
}

/// The colour a role renders in — Mac-only today: `AgentPalette.swift`'s
/// `metricsColor` uses these exact hexes. iced does not read this
/// function; it paints the metrics + elapsed-time strings in a single
/// flat `chrome::MUTED_TEXT` (`#a0a4b0`), with no per-role split. The
/// now-removed GTK UI's `.palette-agent-time` CSS class happened to
/// share `#7a7a7a` with Mac's muted shade, which is why this value is
/// pinned rather than picked freely.
pub fn metrics_role_hex(role: MetricsRole) -> &'static str {
    match role {
        MetricsRole::Muted => "#7a7a7a",
        MetricsRole::Adds => "#7fbf7f",
        MetricsRole::Dels => "#e05252",
    }
}

/// A row plus its sort keys.
struct Row {
    rank: u8,
    last_event_at: i64,
    source_index: usize,
    project_position: i32,
    tab_position: i32,
    tab_id: i64,
    item: PaletteItem,
}

fn row_for(
    project: &Project,
    source: &AgentSource<'_>,
    source_index: usize,
    tab: &Tab,
    owner: &Ownership,
    effective: AgentLifecycle,
    now: i64,
) -> Row {
    let key = TabKey::new(source.host, tab.id);
    // The row's context column: the project, plus the machine it runs on
    // once there is more than one. Composed on the same `·` separator the
    // title uses, so "disc pop" fuzzy-matches a remote row.
    let project_name = normalize_line(&project.name);
    let project_name = match source.label {
        Some(label) if !label.is_empty() => compose_title(&project_name, &normalize_line(label)),
        _ => project_name,
    };
    let name = row_name(tab, owner);
    let agent_name = agent_display_name(&owner.source).to_string();
    let item = PaletteItem::new(
        format!("{ROW_ID_PREFIX}{key}"),
        compose_title(&agent_name, &compose_title(&project_name, &name)),
    )
    .with_agent(AgentRowData {
        effective_lifecycle: effective,
        agent: agent_name,
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
        source_index,
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
        .map(|t| strip_leading_marker(&normalize_line(t)))
        .unwrap_or_default();
    if !session_title.is_empty() {
        return session_title;
    }
    let title = strip_leading_marker(&normalize_line(&tab.title));
    if !title.is_empty() {
        return title;
    }
    format!("Tab {}", tab.id)
}

/// Drop a leading marker glyph from an agent's title — Claude Code
/// prefixes its window title with `✳ ` (U+2733), so a renamed session
/// arrives as `✳ my-session` and the row would render the glyph twice
/// over: once as our own lifecycle dot, once as the agent's.
///
/// Symbols are recognised structurally (non-ASCII and non-alphanumeric)
/// rather than by listing known markers, so a second adapter's glyph is
/// stripped without a code change. ASCII stays untouched, so a title
/// that is a path (`/tmp`, `~/src`) or bracketed (`[wip] name`) is
/// unharmed, and non-Latin scripts are alphanumeric so they survive.
fn strip_leading_marker(text: &str) -> String {
    let stripped = text
        .trim_start_matches(|c: char| c.is_whitespace() || (!c.is_ascii() && !c.is_alphanumeric()));
    // An all-glyph title would strip to nothing; keep it rather than
    // falling through to `Tab <id>`.
    if stripped.is_empty() {
        text.to_string()
    } else {
        stripped.to_string()
    }
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
///
/// Shared with `host_sidebar`'s band rollup: the two slots sit in the
/// same sidebar and an ellipsis that meant two different widths
/// depending on which one drew it would read as a bug.
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
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
    const UNKNOWN: &str = "—";

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

    // ----- host-blind rows (plan 037 §3.1) ---------------------------

    /// A remote row's context column names the machine; a local row's is
    /// untouched, so a bare install reads exactly as it does today.
    #[test]
    fn a_host_row_carries_project_then_host_as_its_context() {
        let local = [project(
            1,
            "roost",
            vec![owned(
                tab(1, "plan-037"),
                "claude",
                AgentLifecycle::Working,
                NOW,
            )],
        )];
        let remote = [project(
            1,
            "blogd",
            vec![owned(
                tab(1, "rss feed"),
                "claude",
                AgentLifecycle::Working,
                NOW,
            )],
        )];
        let items = agent_items_across(
            &[
                AgentSource::local(&local, HostId::LOCAL),
                AgentSource {
                    projects: &remote,
                    host: HostId::new(3),
                    label: Some("pop-os"),
                },
            ],
            NOW,
        );
        assert_eq!(items.len(), 2);
        assert_eq!(agent_of(&items[0]).project, "roost");
        assert_eq!(items[0].title, "Claude Code · roost · plan-037");
        assert_eq!(agent_of(&items[1]).project, "blogd · pop-os");
        assert_eq!(
            items[1].title, "Claude Code · blogd · pop-os · rss feed",
            "the fuzzy filter matches exactly what the row shows"
        );
        // Same numeric tab id on two instances, two distinct row ids.
        assert_eq!(items[0].id, "agent:1");
        assert_eq!(items[1].id, "agent:h3.1");
    }

    /// Needs-attention first regardless of host: a remote waiting agent
    /// outranks a local working one, and the source order only breaks
    /// ties.
    #[test]
    fn urgency_orders_across_hosts_before_source_order_does() {
        let local = [project(
            1,
            "roost",
            vec![owned(tab(1, "a"), "claude", AgentLifecycle::Working, NOW)],
        )];
        let remote = [project(
            1,
            "blogd",
            vec![
                owned(tab(1, "b"), "claude", AgentLifecycle::Waiting, NOW),
                owned(tab(2, "c"), "claude", AgentLifecycle::Working, NOW),
            ],
        )];
        let items = agent_items_across(
            &[
                AgentSource::local(&local, HostId::LOCAL),
                AgentSource {
                    projects: &remote,
                    host: HostId::new(3),
                    label: Some("pop-os"),
                },
            ],
            NOW,
        );
        assert_eq!(
            items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["agent:h3.1", "agent:1", "agent:h3.2"],
            "waiting first, then the tie broken by source order"
        );
    }

    #[test]
    fn cwds_span_every_source_and_stay_host_keyed() {
        let local = [project(
            1,
            "roost",
            vec![owned(tab(1, "a"), "claude", AgentLifecycle::Working, NOW)],
        )];
        let remote = [project(
            1,
            "blogd",
            vec![owned(tab(1, "b"), "claude", AgentLifecycle::Working, NOW)],
        )];
        let cwds = agent_tab_cwds_across(&[
            AgentSource::local(&local, HostId::LOCAL),
            AgentSource {
                projects: &remote,
                host: HostId::new(3),
                label: Some("pop-os"),
            },
        ]);
        assert_eq!(cwds.len(), 2);
        assert!(cwds.contains_key(&TabKey::local(1)));
        assert!(cwds.contains_key(&TabKey::new(HostId::new(3), 1)));
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

    // ----- metrics segmentation --------------------------------------

    /// Every segmentation case in this section, so the concat-identity
    /// property is checked against the same inputs the role assertions
    /// use.
    const METRICS_CASES: [&str; 10] = [
        "4f +86 -12",
        UNKNOWN,
        "1f +0 -0",
        "12f +4021 -998",
        "",
        "+",
        "-",
        "+abc",
        "-abc",
        "<3f & +2",
    ];

    #[test]
    fn canonical_metrics_split_into_five_segments() {
        use MetricsRole::*;
        assert_eq!(
            metrics_segments("4f +86 -12"),
            vec![
                ("4f", Muted),
                (" ", Muted),
                ("+86", Adds),
                (" ", Muted),
                ("-12", Dels),
            ]
        );
    }

    #[test]
    fn the_unknown_dash_is_one_muted_segment() {
        assert_eq!(
            metrics_segments(UNKNOWN),
            vec![(UNKNOWN, MetricsRole::Muted)]
        );
    }

    #[test]
    fn zero_counts_keep_their_roles() {
        use MetricsRole::*;
        assert_eq!(
            metrics_segments("1f +0 -0"),
            vec![
                ("1f", Muted),
                (" ", Muted),
                ("+0", Adds),
                (" ", Muted),
                ("-0", Dels),
            ]
        );
    }

    #[test]
    fn large_counts_keep_their_roles() {
        use MetricsRole::*;
        assert_eq!(
            metrics_segments("12f +4021 -998"),
            vec![
                ("12f", Muted),
                (" ", Muted),
                ("+4021", Adds),
                (" ", Muted),
                ("-998", Dels),
            ]
        );
    }

    #[test]
    fn degenerate_metrics_tokens_stay_muted() {
        assert!(metrics_segments("").is_empty());
        for text in ["+", "-", "+abc", "-abc"] {
            assert_eq!(
                metrics_segments(text),
                vec![(text, MetricsRole::Muted)],
                "{text:?} must not be colored as a count"
            );
        }
    }

    #[test]
    fn segments_concatenate_back_to_the_input() {
        for text in METRICS_CASES {
            let joined: String = metrics_segments(text)
                .into_iter()
                .map(|(segment, _)| segment)
                .collect();
            assert_eq!(joined, text, "segments must reproduce {text:?} exactly");
        }
    }

    #[test]
    fn role_colors_are_pinned() {
        assert_eq!(metrics_role_hex(MetricsRole::Muted), "#7a7a7a");
        assert_eq!(metrics_role_hex(MetricsRole::Adds), "#7fbf7f");
        assert_eq!(metrics_role_hex(MetricsRole::Dels), "#e05252");
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
        let ids: Vec<String> = agent_items(&projects, HostId::LOCAL, NOW)
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(ids, vec!["agent:5", "agent:2"]);
    }

    #[test]
    fn tab_cwds_cover_exactly_the_listed_rows() {
        // The metrics probe keys off this map, so it must match the row
        // population one-for-one — an extra entry probes git for a tab
        // with no row, a missing one leaves a row pending forever.
        let mut agent_tab = owned(tab(2, "claude"), "claude", AgentLifecycle::Working, NOW);
        agent_tab.cwd = "/w/roost".to_string();
        let projects = vec![project(
            1,
            "roost",
            vec![
                tab(1, "plain shell"),
                agent_tab,
                owned(tab(3, "manual"), "manual", AgentLifecycle::Working, NOW),
            ],
        )];
        let cwds = agent_tab_cwds(&projects, HostId::LOCAL);
        assert_eq!(cwds.len(), 1);
        assert_eq!(
            cwds.get(&TabKey::local(2)).map(String::as_str),
            Some("/w/roost")
        );

        let ids: Vec<Option<TabKey>> = agent_items(&projects, HostId::LOCAL, NOW)
            .iter()
            .map(|i| agent_tab_key(&i.id))
            .collect();
        assert_eq!(ids, vec![Some(TabKey::local(2))]);
    }

    /// The same snapshot read as two instances: rows, row ids and cwds
    /// must land in two id-spaces, so a connected host's tab 2 can never
    /// be probed, filled or activated as the local tab 2.
    #[test]
    fn two_instances_of_the_same_snapshot_produce_two_id_spaces() {
        let mut agent_tab = owned(tab(2, "claude"), "claude", AgentLifecycle::Working, NOW);
        agent_tab.cwd = "/w/roost".to_string();
        let projects = vec![project(1, "roost", vec![agent_tab])];
        let host = HostId::new(4);

        let local = agent_items(&projects, HostId::LOCAL, NOW);
        let remote = agent_items(&projects, host, NOW);
        assert_eq!(local[0].id, "agent:2");
        assert_eq!(remote[0].id, "agent:h4.2");
        assert_eq!(agent_tab_key(&remote[0].id), Some(TabKey::new(host, 2)));

        let cwds = agent_tab_cwds(&projects, host);
        assert!(cwds.contains_key(&TabKey::new(host, 2)));
        assert!(
            !cwds.contains_key(&TabKey::local(2)),
            "the local tab 2 is a different tab and gets no entry"
        );
        assert_eq!(
            sidebar_agents(&projects[0], host, NOW)[0].tab,
            TabKey::new(host, 2)
        );
    }

    #[test]
    fn an_empty_source_is_not_live() {
        let t = with_owner(
            owned(tab(1, "ghost"), "claude", AgentLifecycle::Working, NOW),
            |owner| owner.source = String::new(),
        );
        let items = agent_items(&[project(1, "roost", vec![t])], HostId::LOCAL, NOW);
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
        let items = agent_items(
            &[project(1, "roost", vec![fresh, busy])],
            HostId::LOCAL,
            NOW,
        );
        assert_eq!(items.len(), 2);
        // Shell-derived: the busy one ranks above the idle one.
        assert_eq!(items[0].id, "agent:2");
        assert_eq!(agent_of(&items[0]).status_text, "Working");
        assert_eq!(agent_of(&items[1]).status_text, "Idle");
    }

    #[test]
    fn empty_population_yields_one_non_actionable_row() {
        let items = agent_items(
            &[project(1, "roost", vec![tab(1, "shell")])],
            HostId::LOCAL,
            NOW,
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, EMPTY_ROW_ID);
        assert_eq!(items[0].title, EMPTY_ROW_TITLE);
        assert!(!items[0].actionable);
        assert!(items[0].agent.is_none());
        // The sentinel must not parse as a jump target.
        assert_eq!(agent_tab_key(EMPTY_ROW_ID), None);
    }

    #[test]
    fn row_id_round_trips_to_a_tab_id() {
        assert_eq!(agent_tab_key("agent:42"), Some(TabKey::local(42)));
        assert_eq!(agent_tab_key("agent:"), None);
        assert_eq!(agent_tab_key("agent:x"), None);
        assert_eq!(agent_tab_key("notif:7"), None);
        assert_eq!(
            agent_tab_key("agent:h2.42"),
            Some(TabKey::new(HostId::new(2), 42)),
            "a connected instance's row is its own target, not the local tab 42"
        );
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
            HostId::LOCAL,
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
        assert_eq!(
            by_id("agent:1").title,
            "Claude Code · roost · slauth-refactor"
        );
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
        let items = agent_items(&[project(1, "roost", vec![t])], HostId::LOCAL, NOW);
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
        let items = agent_items(&[project(1, "roost\nnope", vec![t])], HostId::LOCAL, NOW);
        let row = agent_of(&items[0]);
        assert_eq!(row.name, "line oneline two x");
        assert_eq!(row.project, "roostnope");
        assert_eq!(
            items[0].title,
            "Claude Code · roostnope · line oneline two x"
        );
    }

    #[test]
    fn compose_title_without_a_project_is_just_the_name() {
        assert_eq!(compose_title("", "claude"), "claude");
        assert_eq!(compose_title("roost", "claude"), "roost · claude");
    }

    #[test]
    fn agent_display_name_covers_the_five_source_constants() {
        // Exact spellings from each adapter's own `SOURCE` const
        // (`crates/roost-agent/src/{claude,codex,opencode,grok,cursor}.rs`),
        // not assumed.
        assert_eq!(agent_display_name("claude"), "Claude Code");
        assert_eq!(agent_display_name("codex"), "Codex");
        assert_eq!(agent_display_name("opencode"), "OpenCode");
        assert_eq!(agent_display_name("grok"), "Grok");
        assert_eq!(agent_display_name("cursor"), "Cursor");
    }

    #[test]
    fn agent_display_name_is_case_sensitive_to_the_source_constants() {
        // A near-miss casing must NOT match — the table keys on the
        // adapters' exact lowercase `SOURCE` spelling, so a caller that
        // typos casing gets the passthrough, not a silently wrong label.
        assert_eq!(agent_display_name("Claude"), "Claude");
        assert_eq!(agent_display_name("CODEX"), "CODEX");
        assert_eq!(agent_display_name("OpenCode"), "OpenCode");
    }

    #[test]
    fn agent_display_name_passes_through_an_unknown_source_verbatim() {
        // AD-8: a sixth, third-party adapter must render as-is rather
        // than needing this table edited to show up.
        assert_eq!(agent_display_name("aider"), "aider");
        assert_eq!(agent_display_name(""), "");
        assert_eq!(agent_display_name("manual"), "manual");
        assert_eq!(agent_display_name("legacy"), "legacy");
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
        let ids: Vec<String> = agent_items(&[project(1, "roost", tabs)], HostId::LOCAL, NOW)
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
        let ids: Vec<String> = agent_items(&[second, first], HostId::LOCAL, NOW)
            .into_iter()
            .map(|i| i.id)
            .collect();
        // Project position 0 first (despite being second in the input),
        // then tab position, then tab id within a position.
        assert_eq!(ids, vec!["agent:3", "agent:30", "agent:20", "agent:10"]);
    }

    // ----- sidebar_agents ---------------------------------------------

    #[test]
    fn sidebar_agents_excludes_manual_legacy_and_dead_tabs() {
        let p = project(
            1,
            "roost",
            vec![
                tab(1, "plain shell"),
                owned(tab(2, "claude"), "claude", AgentLifecycle::Working, NOW),
                owned(tab(3, "manual"), "manual", AgentLifecycle::Working, NOW),
                owned(tab(4, "legacy"), "legacy", AgentLifecycle::Working, NOW),
                owned(tab(5, "codex"), "codex", AgentLifecycle::Waiting, NOW),
            ],
        );
        let ids: Vec<i64> = sidebar_agents(&p, HostId::LOCAL, NOW)
            .into_iter()
            .map(|r| r.tab.tab)
            .collect();
        assert_eq!(ids, vec![5, 2]);
    }

    // The older row is given the lower position AND the lower id, so the
    // assertion fails if recency is dropped from the comparator and a later
    // key decides instead.
    #[test]
    fn sidebar_agents_orders_same_lifecycle_by_recency_first() {
        let mut newer = owned(tab(9, "newer"), "claude", AgentLifecycle::Waiting, NOW);
        newer.position = 7;
        let mut older = owned(
            tab(2, "older"),
            "claude",
            AgentLifecycle::Waiting,
            NOW - 500,
        );
        older.position = 0;

        let p = project(1, "p", vec![older, newer]);
        let ids: Vec<i64> = sidebar_agents(&p, HostId::LOCAL, NOW)
            .into_iter()
            .map(|r| r.tab.tab)
            .collect();
        assert_eq!(ids, vec![9, 2]);
    }

    #[test]
    fn sidebar_agents_orders_by_rank_then_recency_then_tab_position_then_id() {
        let mut later_tab = owned(tab(20, "b"), "claude", AgentLifecycle::Working, NOW);
        later_tab.position = 5;
        let mut same_position_high_id = owned(tab(30, "c"), "claude", AgentLifecycle::Working, NOW);
        same_position_high_id.position = 0;
        let mut same_position_low_id = owned(tab(3, "d"), "claude", AgentLifecycle::Working, NOW);
        same_position_low_id.position = 0;
        let failed = owned(tab(1, "failed"), "claude", AgentLifecycle::Failed, NOW);
        let waiting_old = owned(
            tab(4, "waiting-old"),
            "claude",
            AgentLifecycle::Waiting,
            NOW - 500,
        );

        let p = project(
            1,
            "roost",
            vec![
                later_tab,
                same_position_high_id,
                same_position_low_id,
                failed,
                waiting_old,
            ],
        );
        let ids: Vec<i64> = sidebar_agents(&p, HostId::LOCAL, NOW)
            .into_iter()
            .map(|r| r.tab.tab)
            .collect();
        // rank: Failed > Waiting > Working; then within Working, position
        // 0 before position 5, then id 3 before id 30.
        assert_eq!(ids, vec![1, 4, 3, 30, 20]);
    }

    #[test]
    fn a_leading_agent_marker_is_stripped_from_the_name() {
        // Claude Code's own window-title prefix, U+2733 + space.
        assert_eq!(
            strip_leading_marker("\u{2733} slaudio-refactor"),
            "slaudio-refactor"
        );
        assert_eq!(
            strip_leading_marker("\u{2733}\u{fe0f} Claude Code"),
            "Claude Code"
        );
        assert_eq!(strip_leading_marker("\u{1f7e2} \u{1f47b} two"), "two");
    }

    #[test]
    fn stripping_leaves_ascii_and_non_latin_titles_alone() {
        for keep in [
            "/tmp",
            "~/src/roost",
            "[wip] refactor",
            "-n",
            "café",
            "日本語",
            "1password",
        ] {
            assert_eq!(strip_leading_marker(keep), keep, "must not strip {keep}");
        }
    }

    #[test]
    fn an_all_marker_title_survives_rather_than_emptying() {
        assert_eq!(strip_leading_marker("\u{2733}"), "\u{2733}");
    }

    #[test]
    fn sidebar_agents_name_fallback_chain() {
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

        let rows = sidebar_agents(
            &project(1, "roost", vec![with_meta, with_title, untitled]),
            HostId::LOCAL,
            NOW,
        );
        let by_id = |id: i64| rows.iter().find(|r| r.tab.tab == id).expect("row present");
        assert_eq!(by_id(1).name, "slauth-refactor");
        assert_eq!(by_id(2).name, "zsh");
        assert_eq!(by_id(3).name, "Tab 3");
    }

    #[test]
    fn sidebar_agents_normalizes_and_truncates_like_the_palette() {
        let long_detail = "x".repeat(60);
        let t = with_owner(
            owned(tab(1, "zsh\nx"), "claude", AgentLifecycle::Failed, NOW),
            |owner| owner.detail = long_detail.clone(),
        );
        let rows = sidebar_agents(&project(1, "roost", vec![t]), HostId::LOCAL, NOW);
        assert_eq!(rows[0].name, "zshx");
        let detail = rows[0]
            .status_text
            .strip_prefix("Failed · ")
            .expect("detail suffix");
        assert_eq!(detail.chars().count(), 40);
        assert!(detail.ends_with('…'));
    }

    #[test]
    fn sidebar_agents_on_an_empty_project_is_empty() {
        assert_eq!(
            sidebar_agents(&project(1, "roost", vec![]), HostId::LOCAL, NOW),
            vec![]
        );
        assert_eq!(
            sidebar_agents(
                &project(1, "roost", vec![tab(1, "shell")]),
                HostId::LOCAL,
                NOW
            ),
            vec![]
        );
    }
}
