//! What the window shows once the tab it was showing goes away.
//!
//! Two pure decisions over two sidebar frames — the one the selection was
//! last valid in, and the one this reconcile just built. [`pick`] is the
//! rule itself; [`decide`] is the surrounding policy, which only
//! sometimes reaches the rule.
//!
//! The rule (plan 069 §3.1, owner-pinned): tabs go **forward** first —
//! the nearest surviving tab to the right, else to the left, the browser
//! habit of the next tab sliding under the cursor — and projects go
//! **backward** first, the nearest surviving project above, else below.
//! The asymmetry is intended; do not make one match the other.
//!
//! "Nearest surviving" rather than "adjacent" because one batch can
//! remove several rows at once: a project delete, a host removal, a
//! lagged resync.

use crate::host_sidebar::RingSection;
use crate::keys::{ProjectKey, TabKey};

/// The row the window was showing when the frame changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub project: ProjectKey,
    pub tab: TabKey,
}

/// Where the fallback lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landing {
    /// Rule 1 — a sibling tab in the selection's own project.
    Tab(TabKey),
    /// Rule 2 — a neighbouring project, whose preferred tab the caller
    /// picks with the same semantics a click on that row would.
    Project(ProjectKey),
    /// Rule 2 landed on the in-process local band, which has no
    /// host-qualified row to select: the answer is "show the local
    /// workspace's own selection", and the caller gets there another way.
    Local,
}

/// The facts about live state that the two frames cannot answer, which
/// the caller reads off the app instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Liveness {
    /// The local workspace's active tab moved since the selection was
    /// made — something focused a local tab, and that ends the override.
    pub local_active_moved: bool,
    /// A connected host still lists the selected tab under its project.
    pub listed: bool,
    /// A stopped host is holding its last frame, and that frame still
    /// lists the selected tab.
    pub frozen_and_listed: bool,
}

/// What to do with the selection the window is holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// It still stands; change nothing.
    Keep,
    /// Show the local workspace's own selection instead.
    DropToLocal,
    Select(TabKey),
    /// Select this project's preferred tab.
    SelectProject(ProjectKey),
    /// Nothing survived. The selection goes away and the exit rule
    /// decides what that means.
    Clear,
}

/// The fallback rule, over the frame the selection had a place in
/// (`before`) and the frame it no longer does (`after`).
///
/// Order and position come from `before` — that is where the closed row
/// sat — while survival is judged against `after`, so a batch that both
/// reorders and closes still answers by the order the user was looking
/// at. A selection `before` does not list has no place to walk from, and
/// answers `None`.
///
/// Rows that cannot be acted on are skipped, judged on `after`: a band
/// that is not `navigable`, and a project with no tabs (landing there
/// would draw a selected row over a blank pane).
pub fn pick(before: &[RingSection], after: &[RingSection], was: Selection) -> Option<Landing> {
    let strip = project_tabs(before, was.project)?;
    let survives =
        |tab: &i64| project_tabs(after, was.project).is_some_and(|now| now.contains(tab));
    // The scan starts *on* the old tab rather than past it, so a
    // selection that is still there answers itself: nothing moves.
    let at = strip.iter().position(|tab| *tab == was.tab.tab)?;
    if let Some(tab) = strip[at..]
        .iter()
        .chain(strip[..at].iter().rev())
        .find(|tab| survives(tab))
    {
        return Some(Landing::Tab(TabKey::new(was.project.host, *tab)));
    }
    // The project has nothing left to show, so it is the project that
    // went away as far as the window is concerned — rule 2.
    let rows = flattened(before);
    let at = rows.iter().position(|row| *row == was.project)?;
    rows[..at]
        .iter()
        .rev()
        .chain(rows[at + 1..].iter())
        .find_map(|row| landing_on(after, *row))
}

/// `reconcile_host_selection`'s decision, lifted out of the app.
///
/// The arms, in order:
///
/// 1. a local tab took the focus → [`Decision::DropToLocal`];
/// 2. a connected host still lists the selection → [`Decision::Keep`];
/// 3. a frozen frame still lists it → [`Decision::Keep`];
/// 4. the selection's band, found by `saved_id`, is still in `after` but
///    under a different incarnation or no longer navigable → the host is
///    reconnecting or was replaced and its tabs are renumbered, so
///    nothing "closed" → [`Decision::DropToLocal`];
/// 5. the selection is not in `before` at all → there is no place to
///    walk from → [`Decision::DropToLocal`];
/// 6. otherwise its tab closed, its project closed, or its saved host
///    was removed → [`pick`].
///
/// **Arm 4 is narrower than it looks, and the difference is a bug.** A
/// remote host whose last project closes is auto-removed by a
/// schedule-then-confirm that spans several reconciles: in the one where
/// its last project disappears, its band is still listed, same
/// incarnation, still navigable — with zero projects — and the host is
/// still registered. That is arm 6. Reading "still there / still
/// registered" as "drop" would clear the selection before the removal
/// lands, after which nothing ever re-selects. This function therefore
/// reads only the two frames; it takes no registry or host-set input.
pub fn decide(
    before: &[RingSection],
    after: &[RingSection],
    was: Selection,
    live: Liveness,
) -> Decision {
    if live.local_active_moved {
        return Decision::DropToLocal;
    }
    if live.listed {
        return Decision::Keep;
    }
    if live.frozen_and_listed {
        return Decision::Keep;
    }
    // Arm 5 is resolved first only because arm 4 is asked *of* the
    // selection's old band, which is where that band is named.
    let Some(band) = band_of(before, was.project) else {
        return Decision::DropToLocal;
    };
    if let Some(now) = after
        .iter()
        .find(|section| section.saved_id == band.saved_id)
    {
        if now.host != band.host || !now.navigable {
            return Decision::DropToLocal;
        }
    }
    match pick(before, after, was) {
        Some(Landing::Tab(tab)) => Decision::Select(tab),
        Some(Landing::Project(project)) => Decision::SelectProject(project),
        Some(Landing::Local) => Decision::DropToLocal,
        None => Decision::Clear,
    }
}

fn band_of(frame: &[RingSection], project: ProjectKey) -> Option<&RingSection> {
    frame.iter().find(|section| {
        section.host == project.host && section.projects.iter().any(|row| row.id == project.project)
    })
}

fn project_tabs(frame: &[RingSection], project: ProjectKey) -> Option<&[i64]> {
    band_of(frame, project)?
        .projects
        .iter()
        .find(|row| row.id == project.project)
        .map(|row| row.tabs.as_slice())
}

/// The sidebar as one list: bands top to bottom, projects within a band
/// in order. Rule 2 crosses band boundaries — that is what makes a
/// remote host's last tab exiting land on the band above it.
fn flattened(frame: &[RingSection]) -> Vec<ProjectKey> {
    frame
        .iter()
        .flat_map(|section| {
            section
                .projects
                .iter()
                .map(|row| ProjectKey::new(section.host, row.id))
        })
        .collect()
}

/// `row` as a landing in `after`, or `None` when it is not one there.
fn landing_on(after: &[RingSection], row: ProjectKey) -> Option<Landing> {
    let band = band_of(after, row)?;
    if !band.navigable {
        return None;
    }
    if project_tabs(after, row)?.is_empty() {
        return None;
    }
    Some(match band.saved_id {
        Some(_) => Landing::Project(row),
        None => Landing::Local,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_sidebar::RingProject;
    use crate::keys::HostId;

    const SLOT: HostId = HostId::new(7);
    const MINI3: HostId = HostId::new(9);
    const BOX: HostId = HostId::new(5);

    const P1: i64 = 10;
    const P2: i64 = 20;
    const P3: i64 = 30;
    const Q1: i64 = 40;

    fn band(
        saved_id: Option<&str>,
        host: HostId,
        navigable: bool,
        projects: &[(i64, &[i64])],
    ) -> RingSection {
        RingSection {
            saved_id: saved_id.map(str::to_string),
            host,
            navigable,
            projects: projects
                .iter()
                .map(|(id, tabs)| RingProject {
                    id: *id,
                    tabs: tabs.to_vec(),
                })
                .collect(),
        }
    }

    /// The one-band frame cases 1–9 and 14b are written against.
    fn slot(projects: &[(i64, &[i64])]) -> Vec<RingSection> {
        vec![band(Some("hs-slot"), SLOT, true, projects)]
    }

    fn showing(project: i64, tab: i64) -> Selection {
        Selection {
            project: ProjectKey::new(SLOT, project),
            tab: TabKey::new(SLOT, tab),
        }
    }

    fn tab(id: i64) -> Option<Landing> {
        Some(Landing::Tab(TabKey::new(SLOT, id)))
    }

    fn project(id: i64) -> Option<Landing> {
        Some(Landing::Project(ProjectKey::new(SLOT, id)))
    }

    /// `P1[a b c]  P2[d]  P3[e f]`, the case table's layout.
    fn three_projects() -> Vec<RingSection> {
        slot(&[(P1, &[1, 2, 3]), (P2, &[4]), (P3, &[5, 6])])
    }

    #[test]
    fn case_1_a_closed_tab_lands_on_its_right_hand_neighbour() {
        let after = slot(&[(P1, &[1, 3]), (P2, &[4]), (P3, &[5, 6])]);
        assert_eq!(pick(&three_projects(), &after, showing(P1, 2)), tab(3));
    }

    #[test]
    fn case_2_the_last_tab_of_a_strip_lands_on_its_left_hand_neighbour() {
        let after = slot(&[(P1, &[1, 2]), (P2, &[4]), (P3, &[5, 6])]);
        assert_eq!(pick(&three_projects(), &after, showing(P1, 3)), tab(2));
    }

    #[test]
    fn case_3_the_first_tab_of_a_strip_lands_on_its_right_hand_neighbour() {
        let after = slot(&[(P1, &[2, 3]), (P2, &[4]), (P3, &[5, 6])]);
        assert_eq!(pick(&three_projects(), &after, showing(P1, 1)), tab(2));
    }

    #[test]
    fn case_4_a_closed_project_lands_on_the_project_above() {
        let after = slot(&[(P1, &[1, 2, 3]), (P3, &[5, 6])]);
        assert_eq!(pick(&three_projects(), &after, showing(P2, 4)), project(P1));
    }

    #[test]
    fn case_5_the_first_project_lands_below_when_there_is_nothing_above() {
        let before = slot(&[(P1, &[1]), (P2, &[4]), (P3, &[5, 6])]);
        let after = slot(&[(P2, &[4]), (P3, &[5, 6])]);
        assert_eq!(pick(&before, &after, showing(P1, 1)), project(P2));
    }

    /// Client-only: one engine commit removes one project, so only a
    /// client frame can lose two rows between two reconciles.
    #[test]
    fn case_6_a_batch_that_removes_two_projects_lands_on_the_nearest_survivor() {
        let after = slot(&[(P3, &[5, 6])]);
        assert_eq!(pick(&three_projects(), &after, showing(P2, 4)), project(P3));
    }

    #[test]
    fn case_7_the_last_tab_of_the_last_project_lands_nowhere() {
        let before = slot(&[(P2, &[4])]);
        assert_eq!(pick(&before, &slot(&[(P2, &[])]), showing(P2, 4)), None);
        assert_eq!(pick(&before, &slot(&[]), showing(P2, 4)), None);
    }

    #[test]
    fn case_8_closing_a_tab_that_is_not_the_shown_one_moves_nothing() {
        let after = slot(&[(P1, &[2, 3]), (P2, &[4]), (P3, &[5, 6])]);
        assert_eq!(pick(&three_projects(), &after, showing(P1, 2)), tab(2));
    }

    #[test]
    fn case_9_order_decides_and_ids_do_not() {
        // Tabs `[9 5 2]`: the neighbour right of 5 is 2, which is also
        // the *lowest* remaining id — the answer the old rule gave.
        let before = slot(&[(P1, &[9, 5, 2])]);
        let after = slot(&[(P1, &[9, 2])]);
        assert_eq!(pick(&before, &after, showing(P1, 5)), tab(2));

        // Projects `[P3 P1 P2]`: above P1 is P3, while the lowest
        // remaining id is P2.
        let before = slot(&[(P3, &[5]), (P1, &[1]), (P2, &[4])]);
        let after = slot(&[(P3, &[5]), (P2, &[4])]);
        assert_eq!(pick(&before, &after, showing(P1, 1)), project(P3));
    }

    /// `SLOT[P1 P2]  MINI3[Q1]`, for the cross-band cases.
    fn two_bands() -> Vec<RingSection> {
        vec![
            band(Some("hs-slot"), SLOT, true, &[(P1, &[1]), (P2, &[4])]),
            band(Some("hs-mini3"), MINI3, true, &[(Q1, &[7])]),
        ]
    }

    fn showing_q1() -> Selection {
        Selection {
            project: ProjectKey::new(MINI3, Q1),
            tab: TabKey::new(MINI3, 7),
        }
    }

    #[test]
    fn case_10_an_emptied_but_still_listed_band_lands_on_the_band_above() {
        let after = vec![
            band(Some("hs-slot"), SLOT, true, &[(P1, &[1]), (P2, &[4])]),
            band(Some("hs-mini3"), MINI3, true, &[]),
        ];
        assert_eq!(pick(&two_bands(), &after, showing_q1()), project(P2));
    }

    #[test]
    fn case_11_a_band_that_is_gone_entirely_lands_on_the_band_above() {
        let after = vec![band(Some("hs-slot"), SLOT, true, &[(P1, &[1]), (P2, &[4])])];
        assert_eq!(pick(&two_bands(), &after, showing_q1()), project(P2));
    }

    #[test]
    fn case_12_the_first_row_of_the_first_band_lands_on_the_band_below() {
        let before = vec![
            band(Some("hs-slot"), SLOT, true, &[(P1, &[1])]),
            band(Some("hs-mini3"), MINI3, true, &[(Q1, &[7])]),
        ];
        let after = vec![
            band(Some("hs-slot"), SLOT, true, &[]),
            band(Some("hs-mini3"), MINI3, true, &[(Q1, &[7])]),
        ];
        assert_eq!(
            pick(&before, &after, showing(P1, 1)),
            Some(Landing::Project(ProjectKey::new(MINI3, Q1)))
        );
    }

    #[test]
    fn case_13_a_band_that_is_no_longer_navigable_is_skipped() {
        let lit = |navigable| band(Some("hs-mini3"), MINI3, navigable, &[(Q1, &[7])]);
        let selection = Selection {
            project: ProjectKey::new(BOX, P3),
            tab: TabKey::new(BOX, 5),
        };

        let before = vec![
            band(Some("hs-slot"), SLOT, true, &[(P1, &[1])]),
            lit(true),
            band(Some("hs-box"), BOX, true, &[(P3, &[5])]),
        ];
        let after = vec![
            band(Some("hs-slot"), SLOT, true, &[(P1, &[1])]),
            lit(false),
            band(Some("hs-box"), BOX, true, &[]),
        ];
        assert_eq!(pick(&before, &after, selection), project(P1));

        // Nothing navigable above: the walk turns round and goes below.
        let before = vec![
            lit(true),
            band(Some("hs-box"), BOX, true, &[(P3, &[5])]),
            band(Some("hs-slot"), SLOT, true, &[(P1, &[1])]),
        ];
        let after = vec![
            lit(false),
            band(Some("hs-box"), BOX, true, &[]),
            band(Some("hs-slot"), SLOT, true, &[(P1, &[1])]),
        ];
        assert_eq!(pick(&before, &after, selection), project(P1));
    }

    #[test]
    fn case_14_a_project_with_no_tabs_is_skipped() {
        let before = slot(&[(P1, &[1]), (P2, &[4]), (P3, &[5])]);
        let after = slot(&[(P1, &[1]), (P2, &[])]);
        assert_eq!(pick(&before, &after, showing(P3, 5)), project(P1));
    }

    #[test]
    fn case_14b_a_batch_that_reorders_and_closes_answers_by_the_old_order() {
        // The strip now leads with 3; the neighbour right of the closed 1
        // is still 2, because that is where 1 sat when it was shown.
        let after = slot(&[(P1, &[3, 2])]);
        assert_eq!(
            pick(&slot(&[(P1, &[1, 2, 3])]), &after, showing(P1, 1)),
            tab(2)
        );
    }

    /// The in-process local band has no host-qualified row to select, so
    /// landing there is its own answer.
    #[test]
    fn landing_on_the_in_process_band_is_not_a_project_to_select() {
        let before = vec![
            band(None, HostId::LOCAL, true, &[(P1, &[1])]),
            band(Some("hs-mini3"), MINI3, true, &[(Q1, &[7])]),
        ];
        let after = vec![
            band(None, HostId::LOCAL, true, &[(P1, &[1])]),
            band(Some("hs-mini3"), MINI3, true, &[]),
        ];
        assert_eq!(pick(&before, &after, showing_q1()), Some(Landing::Local));
    }

    fn live() -> Liveness {
        Liveness {
            local_active_moved: false,
            listed: false,
            frozen_and_listed: false,
        }
    }

    #[test]
    fn case_15_a_local_tab_taking_the_focus_drops_the_selection() {
        assert_eq!(
            decide(
                &two_bands(),
                &two_bands(),
                showing_q1(),
                Liveness {
                    local_active_moved: true,
                    listed: true,
                    ..live()
                },
            ),
            Decision::DropToLocal,
            "a local focus wins even over a selection that is still listed"
        );
    }

    #[test]
    fn case_16_a_selection_a_connected_host_still_lists_is_kept() {
        assert_eq!(
            decide(
                &two_bands(),
                &two_bands(),
                showing_q1(),
                Liveness {
                    listed: true,
                    ..live()
                },
            ),
            Decision::Keep
        );
    }

    #[test]
    fn case_17_a_frozen_frame_that_still_lists_the_selection_is_kept() {
        assert_eq!(
            decide(
                &two_bands(),
                &two_bands(),
                showing_q1(),
                Liveness {
                    frozen_and_listed: true,
                    ..live()
                },
            ),
            Decision::Keep
        );
    }

    #[test]
    fn case_18_a_band_relisted_under_a_new_incarnation_drops_to_local() {
        // The band that would have been picked is right above, so a
        // `DropToLocal` here can only be arm 4 refusing to consult it.
        let after = vec![
            band(Some("hs-slot"), SLOT, true, &[(P1, &[1]), (P2, &[4])]),
            band(Some("hs-mini3"), HostId::new(11), true, &[(Q1, &[70])]),
        ];
        assert_eq!(
            decide(&two_bands(), &after, showing_q1(), live()),
            Decision::DropToLocal
        );
    }

    #[test]
    fn case_19_a_band_that_is_no_longer_navigable_drops_to_local() {
        let after = vec![
            band(Some("hs-slot"), SLOT, true, &[(P1, &[1]), (P2, &[4])]),
            band(Some("hs-mini3"), MINI3, false, &[]),
        ];
        assert_eq!(
            decide(&two_bands(), &after, showing_q1(), live()),
            Decision::DropToLocal
        );
    }

    #[test]
    fn case_20_an_emptied_but_still_listed_band_reaches_the_picker() {
        let after = vec![
            band(Some("hs-slot"), SLOT, true, &[(P1, &[1]), (P2, &[4])]),
            band(Some("hs-mini3"), MINI3, true, &[]),
        ];
        assert_eq!(
            decide(&two_bands(), &after, showing_q1(), live()),
            Decision::SelectProject(ProjectKey::new(SLOT, P2)),
            "the host is still registered and still listed, and it is still a close"
        );
    }

    #[test]
    fn case_21_a_selection_the_previous_frame_never_held_drops_to_local() {
        let before = vec![band(Some("hs-slot"), SLOT, true, &[(P1, &[1]), (P2, &[4])])];
        assert_eq!(
            decide(&before, &two_bands(), showing_q1(), live()),
            Decision::DropToLocal
        );
    }

    #[test]
    fn case_22_a_creation_resolved_earlier_in_the_same_reconcile_is_kept() {
        // Its row is younger than the frame, so it is absent from
        // `before` — arm 5's shape. Arm 2 runs first and keeps it.
        let before = vec![band(Some("hs-slot"), SLOT, true, &[(P1, &[1]), (P2, &[4])])];
        assert_eq!(
            decide(
                &before,
                &two_bands(),
                showing_q1(),
                Liveness {
                    listed: true,
                    ..live()
                },
            ),
            Decision::Keep
        );
    }

    #[test]
    fn nothing_surviving_clears_the_selection() {
        let after = vec![band(Some("hs-slot"), SLOT, true, &[])];
        assert_eq!(
            decide(&two_bands(), &after, showing_q1(), live()),
            Decision::Clear
        );
    }
}
