//! The sidebar Agents panel's model (plan 080 D3/D4).
//!
//! One neutral row per agent-owned tab across every workspace, the three
//! orders the panel offers, and the label rules its text follows. The
//! agents palette ([`crate::agent_palette::agent_items_across`]) and the
//! per-project rows ([`crate::agent_palette::sidebar_agents_by_project`])
//! are built from the same rows, so no two agent surfaces can disagree
//! about who is an agent or what its row says.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::iter;

use roost_ipc::agent::{self, AgentLifecycle};

use crate::agent_palette::{
    agent_display_name, agent_owner, elapsed_text, normalize_line, row_name, status_text,
    AgentSource,
};
use crate::host_sidebar;
use crate::keys::{HostId, ProjectKey, TabKey};

/// Between a row's host and its project: `LOCAL › roost`.
const HOST_SEPARATOR: &str = " \u{203a} ";

/// A local row's host, as a row's location line spells it. Its group head
/// spells it [`host_sidebar::LOCAL_LABEL`] — the same word, uppercased the
/// way every band label is.
const LOCAL_HOST: &str = "local";

/// How the panel orders its rows: `agent-panel-sort`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum AgentPanelSort {
    /// Sidebar order, one head per project.
    #[default]
    Grouped,
    /// Most urgent first: the agents palette's order.
    Priority,
    /// Most recent event first.
    Recent,
}

impl AgentPanelSort {
    /// Parse a config value, case-insensitively. Any other value returns
    /// `None` so the caller can warn and keep the default.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "grouped" => Some(Self::Grouped),
            "priority" => Some(Self::Priority),
            "recent" => Some(Self::Recent),
            _ => None,
        }
    }

    /// The config value, which is also the word the panel header shows.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Grouped => "grouped",
            Self::Priority => "priority",
            Self::Recent => "recent",
        }
    }

    /// The sort `cycle_agent_panel_sort` moves to.
    pub fn next(self) -> Self {
        match self {
            Self::Grouped => Self::Priority,
            Self::Priority => Self::Recent,
            Self::Recent => Self::Grouped,
        }
    }
}

/// One agent-owned tab, carrying every key any agent surface sorts on and
/// every string one renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelAgentRow {
    pub tab: TabKey,
    pub project: ProjectKey,
    /// [`AgentSource::panel_label`], one line.
    pub panel_label: Option<String>,
    /// [`AgentSource::label`], verbatim: the palette's context suffix.
    pub palette_label: Option<String>,
    pub project_name: String,
    pub project_position: i32,
    /// Which [`AgentSource`] the row came from, in the caller's order.
    pub source_index: usize,
    pub tab_position: i32,
    pub tab_id: i64,
    /// The agent's display name, as the palette shows it.
    pub agent: String,
    pub lifecycle: AgentLifecycle,
    pub name: String,
    pub status_text: String,
    pub time_text: String,
    pub last_event_at: i64,
    pub dimmed: bool,
}

/// One line of the panel.
// Nearly every entry is an `Agent`, so boxing it would cost an allocation
// per row to save space only on the few heads.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelEntry {
    /// A (host, project) head in [`AgentPanelSort::Grouped`], with how
    /// many rows follow it.
    Group {
        label: String,
        count: usize,
    },
    Agent(PanelAgentRow),
}

/// Every agent-owned tab in `sources`, in source → project → tab input
/// order. Sorting is the caller's: [`panel_entries`], the palette, or the
/// sidebar.
pub fn panel_rows(sources: &[AgentSource<'_>], now: i64) -> Vec<PanelAgentRow> {
    let mut rows = Vec::new();
    for (source_index, source) in sources.iter().enumerate() {
        let panel_label = source.panel_label.map(normalize_line);
        let palette_label = source.label.map(str::to_string);
        for project in source.projects {
            for tab in &project.tabs {
                let axes = tab.agent_state();
                let Some(owner) = agent_owner(&axes) else {
                    continue;
                };
                let lifecycle = agent::effective_lifecycle(&axes);
                rows.push(PanelAgentRow {
                    tab: TabKey::new(source.host, tab.id),
                    project: ProjectKey::new(source.host, project.id),
                    panel_label: panel_label.clone(),
                    palette_label: palette_label.clone(),
                    project_name: normalize_line(&project.name),
                    project_position: project.position,
                    source_index,
                    tab_position: tab.position,
                    tab_id: tab.id,
                    agent: agent_display_name(&owner.source).to_string(),
                    lifecycle,
                    name: row_name(tab, owner),
                    status_text: status_text(lifecycle, tab.agent_lifecycle, &owner.detail),
                    time_text: elapsed_text(now, owner.last_event_at),
                    last_event_at: owner.last_event_at,
                    dimmed: source.dimmed,
                });
            }
        }
    }
    rows
}

/// The panel's lines for `sort`.
///
/// `band_rank` is each host's position among the sidebar's bands
/// (`host_sections`), which is **not** source order: under
/// `local-backend = session` the slot leads the bands wherever it sits in
/// the host registry. `multi_host` is whether the sidebar draws more than
/// one band, which is when the text names hosts at all (D3).
pub fn panel_entries(
    rows: &[PanelAgentRow],
    sort: AgentPanelSort,
    band_rank: &HashMap<HostId, usize>,
    multi_host: bool,
) -> Vec<PanelEntry> {
    let mut sorted: Vec<&PanelAgentRow> = rows.iter().collect();
    let agent = |row: &&PanelAgentRow| PanelEntry::Agent((*row).clone());
    match sort {
        AgentPanelSort::Priority => sorted.sort_by(|a, b| priority_order(a, b)),
        AgentPanelSort::Recent => sorted.sort_by(|a, b| recent_order(a, b)),
        AgentPanelSort::Grouped => {
            sorted.sort_by(|a, b| grouped_order(a, b, band_rank));
            return sorted
                .chunk_by(|a, b| a.project == b.project)
                .flat_map(|group| {
                    let head = PanelEntry::Group {
                        label: group_label(
                            group[0].panel_label.as_deref(),
                            &group[0].project_name,
                            multi_host,
                        ),
                        count: group.len(),
                    };
                    iter::once(head).chain(group.iter().map(agent))
                })
                .collect();
        }
    }
    sorted.iter().map(agent).collect()
}

/// A group head for a row's [`PanelAgentRow::panel_label`] and project:
/// `LOCAL › roost`, `POPOS › craze`, or the bare project name when the
/// sidebar draws one band. The host part is uppercased like its band; the
/// project name is kept as written.
pub fn group_label(panel_label: Option<&str>, project_name: &str, multi_host: bool) -> String {
    if !multi_host {
        return project_name.to_string();
    }
    let host = host_sidebar::section_label(panel_label.unwrap_or(LOCAL_HOST));
    format!("{host}{HOST_SEPARATOR}{project_name}")
}

/// A row's location line outside Grouped: `local › roost`, `popos ›
/// craze` (the saved label as written), or the bare project name when the
/// sidebar draws one band.
pub fn location_text(panel_label: Option<&str>, project_name: &str, multi_host: bool) -> String {
    if !multi_host {
        return project_name.to_string();
    }
    let host = panel_label.unwrap_or(LOCAL_HOST);
    format!("{host}{HOST_SEPARATOR}{project_name}")
}

fn rank(row: &PanelAgentRow) -> u8 {
    agent::rank(row.lifecycle)
}

/// Live rows first, then urgency, then recency, then the workspace's own
/// deterministic layout order — for live rows, exactly the agents
/// palette's order (plan 005 §3.5), which sorts with this. Whole-second
/// `last_event_at` ties are common, so the positional tiebreaks are
/// load-bearing, and `source_index` is one of them since two hosts'
/// projects can sit at the same position.
pub(crate) fn priority_order(a: &PanelAgentRow, b: &PanelAgentRow) -> Ordering {
    a.dimmed
        .cmp(&b.dimmed)
        .then(rank(b).cmp(&rank(a)))
        .then(b.last_event_at.cmp(&a.last_event_at))
        .then(a.source_index.cmp(&b.source_index))
        .then(a.project_position.cmp(&b.project_position))
        .then(a.tab_position.cmp(&b.tab_position))
        .then(a.tab_id.cmp(&b.tab_id))
}

fn recent_order(a: &PanelAgentRow, b: &PanelAgentRow) -> Ordering {
    a.dimmed
        .cmp(&b.dimmed)
        .then(b.last_event_at.cmp(&a.last_event_at))
        .then_with(|| priority_order(a, b))
}

/// The order inside one project:
/// [`crate::agent_palette::sidebar_agents_by_project`], and Grouped under
/// each head.
pub(crate) fn within_project_order(a: &PanelAgentRow, b: &PanelAgentRow) -> Ordering {
    rank(b)
        .cmp(&rank(a))
        .then(b.last_event_at.cmp(&a.last_event_at))
        .then(a.tab_position.cmp(&b.tab_position))
        .then(a.tab_id.cmp(&b.tab_id))
}

/// Sidebar order. Dimmed rows keep their place: a disconnected host's
/// band does not move either. A host with no band (none in `band_rank`)
/// sorts after every banded one, by source order; the source index and
/// project key also keep each project's rows contiguous should two
/// projects share a position, so a project never gets two heads.
fn grouped_order(
    a: &PanelAgentRow,
    b: &PanelAgentRow,
    band_rank: &HashMap<HostId, usize>,
) -> Ordering {
    let band = |row: &PanelAgentRow| {
        band_rank
            .get(&row.project.host)
            .copied()
            .unwrap_or(usize::MAX)
    };
    band(a)
        .cmp(&band(b))
        .then(a.source_index.cmp(&b.source_index))
        .then(a.project_position.cmp(&b.project_position))
        .then(a.project.cmp(&b.project))
        .then_with(|| within_project_order(a, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ipc::agent::{Ownership, ShellState};
    use roost_ipc::messages::{Project, Tab, TabState};
    use std::collections::BTreeMap;

    use crate::agent_palette::{agent_items_across, agent_tab_key};

    const NOW: i64 = 1_700_000_000;
    const SLOT: HostId = HostId::new(5);
    const POPOS: HostId = HostId::new(2);

    fn agent(id: i64, position: i32, lifecycle: AgentLifecycle, at: i64) -> Tab {
        Tab {
            id,
            project_id: 0,
            title: format!("agent-{id}"),
            cwd: "/tmp".to_string(),
            state: TabState::None,
            has_notification: false,
            is_active: false,
            user_titled: false,
            position,
            created_at: 0,
            last_active: 0,
            hook_active: false,
            shell_state: ShellState::AtPrompt,
            agent_lifecycle: lifecycle,
            ownership: Some(Ownership {
                source: "claude".to_string(),
                session_id: "s1".to_string(),
                last_event_at: at,
                detail: String::new(),
                metadata: BTreeMap::new(),
            }),
            password_input: false,
            dropped_reports: None,
        }
    }

    fn project(id: i64, position: i32, name: &str, tabs: Vec<Tab>) -> Project {
        Project {
            id,
            name: name.to_string(),
            cwd: "/tmp".to_string(),
            position,
            created_at: 0,
            tabs,
        }
    }

    fn source<'a>(
        projects: &'a [Project],
        host: HostId,
        panel_label: Option<&'a str>,
        dimmed: bool,
    ) -> AgentSource<'a> {
        AgentSource {
            projects,
            host,
            label: panel_label,
            dimmed,
            panel_label,
        }
    }

    fn bands(order: &[HostId]) -> HashMap<HostId, usize> {
        order.iter().enumerate().map(|(i, h)| (*h, i)).collect()
    }

    /// Agent rows as `tab` wire keys; group heads as `[label (count)]`.
    fn spelled(entries: &[PanelEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                PanelEntry::Group { label, count } => format!("[{label} ({count})]"),
                PanelEntry::Agent(row) => row.tab.to_wire(),
            })
            .collect()
    }

    fn sorted(
        rows: &[PanelAgentRow],
        sort: AgentPanelSort,
        band_rank: &HashMap<HostId, usize>,
    ) -> Vec<String> {
        panel_entries(rows, sort, band_rank, true)
            .iter()
            .filter_map(|entry| match entry {
                PanelEntry::Agent(row) => Some(row.tab.to_wire()),
                PanelEntry::Group { .. } => None,
            })
            .collect()
    }

    /// One project, `name`, holding one working agent.
    fn solo(name: &str) -> [Project; 1] {
        [project(
            1,
            0,
            name,
            vec![agent(1, 0, AgentLifecycle::Working, NOW)],
        )]
    }

    /// Two projects on one host, mixed lifecycles and ages: the three
    /// sorts each put the same five rows in a different order.
    fn one_host_rows() -> Vec<PanelAgentRow> {
        let projects = [
            project(
                1,
                0,
                "roost",
                vec![
                    agent(1, 0, AgentLifecycle::Working, NOW - 10),
                    agent(2, 1, AgentLifecycle::Finished, NOW),
                ],
            ),
            project(
                2,
                1,
                "craze",
                vec![
                    agent(3, 0, AgentLifecycle::Failed, NOW - 300),
                    agent(4, 1, AgentLifecycle::Waiting, NOW - 40),
                    agent(5, 2, AgentLifecycle::Working, NOW - 5),
                ],
            ),
        ];
        panel_rows(&[source(&projects, HostId::LOCAL, None, false)], NOW)
    }

    #[test]
    fn priority_is_rank_then_recency() {
        let rows = one_host_rows();
        assert_eq!(
            sorted(&rows, AgentPanelSort::Priority, &HashMap::new()),
            vec!["3", "4", "5", "1", "2"]
        );
    }

    #[test]
    fn recent_is_recency_first() {
        let rows = one_host_rows();
        assert_eq!(
            sorted(&rows, AgentPanelSort::Recent, &HashMap::new()),
            vec!["2", "5", "1", "4", "3"]
        );
    }

    #[test]
    fn grouped_is_project_order_with_heads_then_rank_inside() {
        let rows = one_host_rows();
        assert_eq!(
            spelled(&panel_entries(
                &rows,
                AgentPanelSort::Grouped,
                &bands(&[HostId::LOCAL]),
                false
            )),
            vec!["[roost (2)]", "1", "2", "[craze (3)]", "3", "4", "5"]
        );
    }

    /// Whole-second stamps tie often. Recent falls through to the
    /// Priority keys (rank first), Priority to source order.
    #[test]
    fn a_same_second_tie_falls_through_to_the_priority_keys() {
        let local = [project(
            1,
            0,
            "roost",
            vec![
                agent(1, 0, AgentLifecycle::Working, NOW),
                agent(2, 1, AgentLifecycle::Waiting, NOW),
            ],
        )];
        let remote = [project(
            1,
            0,
            "craze",
            vec![agent(1, 0, AgentLifecycle::Waiting, NOW)],
        )];
        let rows = panel_rows(
            &[
                source(&local, HostId::LOCAL, None, false),
                source(&remote, POPOS, Some("popos"), false),
            ],
            NOW,
        );
        let none = HashMap::new();
        assert_eq!(
            sorted(&rows, AgentPanelSort::Recent, &none),
            vec!["2", "h2.1", "1"],
            "rank breaks the tie, then source order"
        );
        assert_eq!(
            sorted(&rows, AgentPanelSort::Priority, &none),
            vec!["2", "h2.1", "1"]
        );
    }

    #[test]
    fn grouped_puts_the_session_slot_first_though_it_is_the_last_source() {
        let popos = solo("craze");
        let slot = solo("roost");
        let rows = panel_rows(
            &[
                source(&popos, POPOS, Some("popos"), false),
                source(&slot, SLOT, None, false),
            ],
            NOW,
        );
        assert_eq!(
            spelled(&panel_entries(
                &rows,
                AgentPanelSort::Grouped,
                &bands(&[SLOT, POPOS]),
                true
            )),
            vec!["[LOCAL › roost (1)]", "h5.1", "[POPOS › craze (1)]", "h2.1"]
        );
        assert_eq!(
            sorted(&rows, AgentPanelSort::Priority, &bands(&[SLOT, POPOS])),
            vec!["h2.1", "h5.1"],
            "Priority keeps source order for its tiebreaks and ignores the bands"
        );
    }

    /// Under `in-process` a saved host labelled `localhost` is an ordinary
    /// host in its registry place, reading `LOCALHOST`. The local source
    /// is passed last so the order can only have come from the bands.
    #[test]
    fn grouped_puts_the_in_process_local_first_and_a_saved_localhost_in_its_band() {
        let popos = solo("craze");
        let localhost = solo("scratch");
        let local = solo("roost");
        let rows = panel_rows(
            &[
                source(&localhost, SLOT, Some("localhost"), false),
                source(&popos, POPOS, Some("popos"), false),
                source(&local, HostId::LOCAL, None, false),
            ],
            NOW,
        );
        assert_eq!(
            spelled(&panel_entries(
                &rows,
                AgentPanelSort::Grouped,
                &bands(&[HostId::LOCAL, POPOS, SLOT]),
                true
            )),
            vec![
                "[LOCAL › roost (1)]",
                "1",
                "[POPOS › craze (1)]",
                "h2.1",
                "[LOCALHOST › scratch (1)]",
                "h5.1",
            ]
        );
    }

    /// The same numeric project and tab ids on two hosts are two projects
    /// and two rows: two heads, never one head counting both.
    #[test]
    fn colliding_numeric_ids_across_hosts_stay_distinct() {
        let local = [project(
            7,
            0,
            "roost",
            vec![agent(3, 0, AgentLifecycle::Working, NOW)],
        )];
        let remote = [project(
            7,
            0,
            "roost",
            vec![agent(3, 0, AgentLifecycle::Working, NOW)],
        )];
        let rows = panel_rows(
            &[
                source(&local, HostId::LOCAL, None, false),
                source(&remote, POPOS, Some("popos"), false),
            ],
            NOW,
        );
        assert_eq!(rows[0].tab, TabKey::local(3));
        assert_eq!(rows[1].tab, TabKey::new(POPOS, 3));
        assert_eq!(rows[0].project, ProjectKey::local(7));
        assert_eq!(rows[1].project, ProjectKey::new(POPOS, 7));
        assert_eq!(
            spelled(&panel_entries(
                &rows,
                AgentPanelSort::Grouped,
                &bands(&[HostId::LOCAL, POPOS]),
                true
            )),
            vec!["[LOCAL › roost (1)]", "3", "[POPOS › roost (1)]", "h2.3"]
        );
    }

    /// A disconnected host's rows sink below every live one in Priority
    /// and Recent — however urgent or recent — but in Grouped they stay
    /// under their band, which does not move either.
    #[test]
    fn dimmed_rows_sink_in_priority_and_recent_and_hold_their_place_in_grouped() {
        let offline = [project(
            1,
            0,
            "craze",
            vec![agent(1, 0, AgentLifecycle::Failed, NOW)],
        )];
        let live = [project(
            1,
            0,
            "roost",
            vec![agent(1, 0, AgentLifecycle::Finished, NOW - 600)],
        )];
        let rows = panel_rows(
            &[
                source(&offline, POPOS, Some("popos"), true),
                source(&live, SLOT, Some("shed"), false),
            ],
            NOW,
        );
        assert!(rows[0].dimmed && !rows[1].dimmed);
        let band_rank = bands(&[POPOS, SLOT]);
        assert_eq!(
            sorted(&rows, AgentPanelSort::Priority, &band_rank),
            vec!["h5.1", "h2.1"]
        );
        assert_eq!(
            sorted(&rows, AgentPanelSort::Recent, &band_rank),
            vec!["h5.1", "h2.1"]
        );
        assert_eq!(
            sorted(&rows, AgentPanelSort::Grouped, &band_rank),
            vec!["h2.1", "h5.1"]
        );
    }

    #[test]
    fn group_counts_are_the_rows_under_each_head() {
        let projects = [
            project(
                1,
                0,
                "roost",
                (1..=3)
                    .map(|id| agent(id, id as i32, AgentLifecycle::Working, NOW))
                    .collect(),
            ),
            project(2, 1, "empty", Vec::new()),
            project(
                3,
                2,
                "craze",
                vec![agent(9, 0, AgentLifecycle::Waiting, NOW)],
            ),
        ];
        let rows = panel_rows(&[source(&projects, HostId::LOCAL, None, false)], NOW);
        let heads: Vec<(String, usize)> =
            panel_entries(&rows, AgentPanelSort::Grouped, &HashMap::new(), false)
                .into_iter()
                .filter_map(|entry| match entry {
                    PanelEntry::Group { label, count } => Some((label, count)),
                    PanelEntry::Agent(_) => None,
                })
                .collect();
        assert_eq!(
            heads,
            vec![("roost".to_string(), 3), ("craze".to_string(), 1)],
            "a project with no agent gets no head"
        );
    }

    #[test]
    fn no_agents_is_no_entries() {
        for sort in [
            AgentPanelSort::Grouped,
            AgentPanelSort::Priority,
            AgentPanelSort::Recent,
        ] {
            assert!(panel_entries(&[], sort, &HashMap::new(), true).is_empty());
        }
    }

    /// One band → no host part anywhere; more than one → `LOCAL` for this
    /// machine and the saved label for any other host, uppercased in a
    /// head and as written in a location. The project keeps its case.
    #[test]
    fn labels_name_hosts_only_when_the_sidebar_draws_several_bands() {
        let both = |label: Option<&str>, multi_host| {
            (
                group_label(label, "Roost", multi_host),
                location_text(label, "Roost", multi_host),
            )
        };
        let bare = ("Roost".to_string(), "Roost".to_string());
        assert_eq!(both(None, false), bare);
        assert_eq!(both(Some("PopOS-box"), false), bare);

        let named = |head: &str, location: &str| (head.to_string(), location.to_string());
        assert_eq!(both(None, true), named("LOCAL › Roost", "local › Roost"));
        assert_eq!(
            both(Some("PopOS-box"), true),
            named("POPOS-BOX › Roost", "PopOS-box › Roost")
        );
        assert_eq!(
            both(Some("localhost"), true),
            named("LOCALHOST › Roost", "localhost › Roost")
        );
    }

    #[test]
    fn the_local_head_is_the_reserved_band_label() {
        assert_eq!(
            host_sidebar::section_label(LOCAL_HOST),
            host_sidebar::LOCAL_LABEL
        );
    }

    /// A4: over live rows, Priority is exactly the agents palette's order.
    #[test]
    fn priority_over_live_rows_is_the_palette_order() {
        let local = [
            project(
                1,
                1,
                "roost",
                vec![
                    agent(1, 0, AgentLifecycle::Working, NOW),
                    agent(2, 1, AgentLifecycle::Waiting, NOW - 30),
                    agent(3, 2, AgentLifecycle::Finished, NOW - 5),
                ],
            ),
            project(
                2,
                0,
                "craze",
                vec![
                    agent(4, 3, AgentLifecycle::Working, NOW),
                    agent(5, 0, AgentLifecycle::Failed, NOW - 900),
                ],
            ),
        ];
        let remote = [project(
            1,
            0,
            "blogd",
            vec![
                agent(1, 0, AgentLifecycle::Working, NOW),
                agent(2, 1, AgentLifecycle::Waiting, NOW - 30),
                agent(6, 2, AgentLifecycle::Inactive, NOW),
            ],
        )];
        let sources = [
            source(&local, HostId::LOCAL, None, false),
            source(&remote, POPOS, Some("popos"), false),
        ];
        let palette: Vec<TabKey> = agent_items_across(&sources, NOW)
            .iter()
            .map(|item| agent_tab_key(&item.id).expect("an agent row"))
            .collect();
        let panel: Vec<TabKey> = panel_entries(
            &panel_rows(&sources, NOW),
            AgentPanelSort::Priority,
            &bands(&[POPOS, HostId::LOCAL]),
            true,
        )
        .into_iter()
        .map(|entry| match entry {
            PanelEntry::Agent(row) => row.tab,
            PanelEntry::Group { .. } => panic!("Priority draws no heads"),
        })
        .collect();
        let local_tab = TabKey::local;
        let popos_tab = |id| TabKey::new(POPOS, id);
        let expected = vec![
            local_tab(5),
            local_tab(2),
            popos_tab(2),
            local_tab(4),
            local_tab(1),
            popos_tab(1),
            local_tab(3),
            popos_tab(6),
        ];
        assert_eq!(palette, expected);
        assert_eq!(panel, palette);
    }

    #[test]
    fn rows_carry_the_palettes_strings() {
        let projects = [project(
            1,
            0,
            "roost\n",
            vec![agent(1, 0, AgentLifecycle::Waiting, NOW - 90)],
        )];
        let row = &panel_rows(&[source(&projects, POPOS, Some("pop\tos"), true)], NOW)[0];
        assert_eq!(row.agent, "Claude Code");
        assert_eq!(row.name, "agent-1");
        assert_eq!(row.project_name, "roost");
        assert_eq!(row.panel_label.as_deref(), Some("popos"));
        assert_eq!(row.palette_label.as_deref(), Some("pop\tos"));
        assert_eq!(row.status_text, "Waiting for input");
        assert_eq!(row.time_text, "1m");
        assert!(row.dimmed);
    }

    #[test]
    fn the_sort_cycles_grouped_priority_recent() {
        let mut sort = AgentPanelSort::default();
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(sort.as_str());
            sort = sort.next();
        }
        assert_eq!(seen, vec!["grouped", "priority", "recent", "grouped"]);
    }

    #[test]
    fn every_sort_parses_back_from_its_config_value() {
        for sort in [
            AgentPanelSort::Grouped,
            AgentPanelSort::Priority,
            AgentPanelSort::Recent,
        ] {
            assert_eq!(AgentPanelSort::parse(sort.as_str()), Some(sort));
        }
        assert_eq!(
            AgentPanelSort::parse(" Priority "),
            Some(AgentPanelSort::Priority)
        );
        assert_eq!(
            AgentPanelSort::parse("RECENT"),
            Some(AgentPanelSort::Recent)
        );
        assert_eq!(AgentPanelSort::parse("state"), None);
        assert_eq!(AgentPanelSort::parse(""), None);
    }
}
