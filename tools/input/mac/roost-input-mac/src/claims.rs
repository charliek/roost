//! Whether a foreign app's claim to the keyboard is in front of the target
//! (plan 075 §D2), decided over a plain snapshot of the window list so it is
//! tested on any OS.
//!
//! A key goes to the key window, and every app keeps its own idea of which
//! window that is: a background GPUI or winit app reports its own window
//! focused while it is not active (#604). Such a claim matters only when the
//! window it names is ahead of the target's window: a non-activating panel
//! that takes the keyboard after the target was activated is ordered in front
//! of it, and a background app's ordinary window is behind it. The window
//! named is judged, never the app's other windows: an app's status item sits
//! at a higher layer than every normal window without ever holding the
//! keyboard.

use crate::args::Rect;

/// One on-screen window, in the window list's front-to-back order. A layer
/// or bounds the list did not give readably is `None`: the window still
/// counts for its owner, and makes any claim of that owner unlocatable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowEntry {
    pub owner: i32,
    pub layer: Option<i64>,
    pub bounds: Option<Rect>,
}

/// How far, in points, each edge of a window in the list may be from the
/// claimed frame Accessibility reports and still be that window: the list
/// rounds to whole points, Accessibility does not.
const EDGE_TOLERANCE: f64 = 1.0;

/// One entry of the window list as read: its owner pid, layer and bounds,
/// each `None` when unreadable.
pub type Listed = (Option<i32>, Option<i64>, Option<Rect>);

/// The snapshot, or why there is none: an entry whose owner cannot be read
/// could be anyone's window, in front of the target or not.
pub fn snapshot(listed: Vec<Listed>) -> Result<Vec<WindowEntry>, String> {
    listed
        .into_iter()
        .enumerate()
        .map(|(index, (owner, layer, bounds))| {
            owner
                .map(|owner| WindowEntry {
                    owner,
                    layer,
                    bounds,
                })
                .ok_or_else(|| format!("unreadable window list entry (#{index} has no owner pid)"))
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    Ahead,
    Behind,
    /// No window of the claimant has the claimed frame, more than one might,
    /// or one of its windows could not be read: taken as ahead.
    Unlocated,
}

impl Claim {
    fn at(index: usize, reference: usize) -> Claim {
        if index < reference {
            Claim::Ahead
        } else {
            Claim::Behind
        }
    }

    pub fn blocks(self) -> bool {
        self != Claim::Behind
    }

    pub fn name(self) -> &'static str {
        match self {
            Claim::Ahead => "ahead",
            Claim::Behind => "behind",
            Claim::Unlocated => "unlocated",
        }
    }
}

/// The target's first normal (layer 0) window: its own popups, tooltips and
/// IME candidates sit above every normal window and are never the reference.
pub fn reference_index(windows: &[WindowEntry], target: i32) -> Option<usize> {
    windows
        .iter()
        .position(|window| window.owner == target && window.layer == Some(0))
}

/// Where the claimant's claimed window is relative to the reference window.
/// It is located only when exactly one of the claimant's windows could be it
/// (every edge within `EDGE_TOLERANCE` of the claimed frame) and every
/// window of the claimant was read; anything else fails closed.
pub fn claim_is_ahead(
    windows: &[WindowEntry],
    reference: usize,
    claimant: i32,
    claimed: Rect,
) -> Claim {
    let mut candidates = windows
        .iter()
        .enumerate()
        .filter(|(_, window)| window.owner == claimant)
        .filter(|(_, window)| match (window.layer, window.bounds) {
            (Some(_), Some(bounds)) => near(bounds, claimed),
            _ => true,
        });
    match (candidates.next(), candidates.next()) {
        (Some((index, window)), None) if window.layer.is_some() && window.bounds.is_some() => {
            Claim::at(index, reference)
        }
        _ => Claim::Unlocated,
    }
}

/// `claim_is_ahead`, with a claimed window that has no frame unlocated.
pub fn judge(
    windows: &[WindowEntry],
    reference: usize,
    claimant: i32,
    claimed: Option<Rect>,
) -> Claim {
    claimed.map_or(Claim::Unlocated, |bounds| {
        claim_is_ahead(windows, reference, claimant, bounds)
    })
}

/// Where `owner`'s frontmost window is relative to the reference window.
pub fn first_window(windows: &[WindowEntry], reference: usize, owner: i32) -> Option<Claim> {
    windows
        .iter()
        .position(|window| window.owner == owner)
        .map(|index| Claim::at(index, reference))
}

/// Every layer `owner` has a window at, front to back (`None` unreadable).
pub fn layers(windows: &[WindowEntry], owner: i32) -> Vec<Option<i64>> {
    windows
        .iter()
        .filter(|window| window.owner == owner)
        .map(|window| window.layer)
        .collect()
}

/// Every owner with a window on screen but the target and this process, in
/// first-window order.
pub fn foreign_owners(windows: &[WindowEntry], target: i32, me: i32) -> Vec<i32> {
    let mut owners = Vec::new();
    for window in windows {
        if window.owner != target && window.owner != me && !owners.contains(&window.owner) {
            owners.push(window.owner);
        }
    }
    owners
}

fn near(listed: Rect, claimed: Rect) -> bool {
    let edges = |rect: Rect| [rect.x, rect.y, rect.x + rect.width, rect.y + rect.height];
    edges(listed)
        .into_iter()
        .zip(edges(claimed))
        .all(|(listed, claimed)| (listed - claimed).abs() <= EDGE_TOLERANCE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOST: i32 = 100;
    const OTHER: i32 = 200;
    const ME: i32 = 300;

    const TERMINAL: Rect = rect(0.0, 33.0, 1200.0, 800.0);
    const PANEL: Rect = rect(400.0, 200.0, 360.0, 160.0);
    const STATUS: Rect = rect(1300.0, 0.0, 30.0, 24.0);
    const BACKGROUND: Rect = rect(100.0, 100.0, 900.0, 700.0);

    const fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn window(owner: i32, layer: i64, bounds: Rect) -> WindowEntry {
        WindowEntry {
            owner,
            layer: Some(layer),
            bounds: Some(bounds),
        }
    }

    fn judge(windows: &[WindowEntry], claimed: Rect) -> Claim {
        let reference = reference_index(windows, ROOST).expect("a reference window");
        claim_is_ahead(windows, reference, OTHER, claimed)
    }

    #[test]
    fn a_claimed_window_ahead_of_the_target_blocks_and_one_behind_does_not() {
        let ahead = [window(OTHER, 0, PANEL), window(ROOST, 0, TERMINAL)];
        assert_eq!(judge(&ahead, PANEL), Claim::Ahead);
        assert!(Claim::Ahead.blocks());

        let behind = [window(ROOST, 0, TERMINAL), window(OTHER, 0, BACKGROUND)];
        assert_eq!(judge(&behind, BACKGROUND), Claim::Behind);
        assert!(!Claim::Behind.blocks());
    }

    #[test]
    fn no_reference_when_the_target_is_absent_or_has_only_higher_layer_windows() {
        let absent = [window(OTHER, 0, BACKGROUND)];
        assert_eq!(reference_index(&absent, ROOST), None);

        let only_popups = [window(ROOST, 101, PANEL), window(OTHER, 0, BACKGROUND)];
        assert_eq!(reference_index(&only_popups, ROOST), None);
    }

    #[test]
    fn the_first_layer_0_target_window_is_the_reference() {
        let windows = [
            window(OTHER, 0, PANEL),
            window(ROOST, 0, TERMINAL),
            window(ROOST, 0, BACKGROUND),
        ];
        assert_eq!(reference_index(&windows, ROOST), Some(1));
    }

    #[test]
    fn a_target_popup_does_not_hide_a_panel_between_it_and_the_terminal() {
        let windows = [
            window(ROOST, 101, rect(10.0, 10.0, 200.0, 300.0)),
            window(OTHER, 3, PANEL),
            window(ROOST, 0, TERMINAL),
        ];
        assert_eq!(reference_index(&windows, ROOST), Some(2));
        assert_eq!(judge(&windows, PANEL), Claim::Ahead);
    }

    #[test]
    fn a_status_item_ahead_does_not_count_when_the_focused_window_is_behind() {
        let windows = [
            window(OTHER, 25, STATUS),
            window(ROOST, 0, TERMINAL),
            window(OTHER, 0, BACKGROUND),
        ];
        assert_eq!(judge(&windows, BACKGROUND), Claim::Behind);
    }

    #[test]
    fn a_focused_panel_at_a_high_layer_ahead_blocks() {
        for layer in [25, 101] {
            let windows = [
                window(OTHER, layer, PANEL),
                window(ROOST, 0, TERMINAL),
                window(OTHER, 0, BACKGROUND),
            ];
            assert_eq!(judge(&windows, PANEL), Claim::Ahead, "layer {layer}");
        }
    }

    #[test]
    fn a_claim_that_cannot_be_located_once_blocks() {
        let windows = [window(ROOST, 0, TERMINAL), window(OTHER, 0, BACKGROUND)];
        assert_eq!(judge(&windows, PANEL), Claim::Unlocated);

        let twins = [
            window(ROOST, 0, TERMINAL),
            window(OTHER, 0, BACKGROUND),
            window(OTHER, 0, BACKGROUND),
        ];
        assert_eq!(judge(&twins, BACKGROUND), Claim::Unlocated);
        assert!(Claim::Unlocated.blocks());

        assert_eq!(super::judge(&windows, 0, OTHER, None), Claim::Unlocated);

        let another_owner = [window(OTHER + 1, 0, PANEL), window(ROOST, 0, TERMINAL)];
        assert_eq!(judge(&another_owner, PANEL), Claim::Unlocated);
    }

    #[test]
    fn an_owner_is_placed_by_its_frontmost_window() {
        let windows = [
            window(OTHER, 25, STATUS),
            window(ROOST, 0, TERMINAL),
            window(OTHER, 0, BACKGROUND),
            window(OTHER + 1, 0, BACKGROUND),
        ];
        assert_eq!(first_window(&windows, 1, OTHER), Some(Claim::Ahead));
        assert_eq!(first_window(&windows, 1, OTHER + 1), Some(Claim::Behind));
        assert_eq!(first_window(&windows, 1, ME), None);
        assert_eq!(layers(&windows, OTHER), [Some(25), Some(0)]);
    }

    #[test]
    fn bounds_match_within_a_point_on_every_edge() {
        let windows = [window(ROOST, 0, TERMINAL), window(OTHER, 0, BACKGROUND)];
        let accessibility = rect(100.4, 99.6, 900.2, 699.8);
        assert_eq!(judge(&windows, accessibility), Claim::Behind);
        assert_eq!(
            judge(&windows, rect(101.5, 100.0, 900.0, 700.0)),
            Claim::Unlocated
        );
    }

    /// Rounding the claimed frame would pick only the window behind; both
    /// are within a point of it, so the claim is not located.
    #[test]
    fn two_windows_within_a_point_of_the_claim_fail_closed() {
        let windows = [
            window(OTHER, 3, rect(100.0, 100.0, 360.0, 192.0)),
            window(ROOST, 0, TERMINAL),
            window(OTHER, 0, rect(101.0, 100.0, 360.0, 192.0)),
        ];
        assert_eq!(
            judge(&windows, rect(100.6, 100.0, 360.0, 192.0)),
            Claim::Unlocated
        );
    }

    #[test]
    fn an_unreadable_window_of_the_claimant_makes_its_claim_unlocated() {
        for unreadable in [
            WindowEntry {
                owner: OTHER,
                layer: None,
                bounds: Some(STATUS),
            },
            WindowEntry {
                owner: OTHER,
                layer: Some(25),
                bounds: None,
            },
        ] {
            let alone = [window(ROOST, 0, TERMINAL), unreadable];
            assert_eq!(judge(&alone, BACKGROUND), Claim::Unlocated);
            let beside = [
                window(ROOST, 0, TERMINAL),
                window(OTHER, 0, BACKGROUND),
                unreadable,
            ];
            assert_eq!(judge(&beside, BACKGROUND), Claim::Unlocated);
            assert_eq!(foreign_owners(&beside, ROOST, ME), [OTHER]);
        }
    }

    #[test]
    fn an_entry_with_no_owner_refuses_the_snapshot() {
        let listed = vec![
            (Some(ROOST), Some(0), Some(TERMINAL)),
            (Some(OTHER), None, None),
        ];
        assert_eq!(
            snapshot(listed).unwrap(),
            [
                window(ROOST, 0, TERMINAL),
                WindowEntry {
                    owner: OTHER,
                    layer: None,
                    bounds: None,
                },
            ]
        );
        let error = snapshot(vec![
            (Some(ROOST), Some(0), Some(TERMINAL)),
            (None, Some(0), Some(PANEL)),
        ])
        .unwrap_err();
        assert!(error.contains("unreadable window list entry"), "{error}");
    }

    #[test]
    fn the_target_and_the_helper_itself_are_never_foreign() {
        let windows = [
            window(ME, 0, PANEL),
            window(OTHER, 25, STATUS),
            window(ROOST, 0, TERMINAL),
            window(OTHER, 0, BACKGROUND),
            window(OTHER + 1, 0, BACKGROUND),
        ];
        assert_eq!(foreign_owners(&windows, ROOST, ME), [OTHER, OTHER + 1]);
    }
}
