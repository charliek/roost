//! Toolkit-neutral ordering helpers for pointer-driven list reordering.
//!
//! UIs own gesture geometry; the authoritative workspace owns committed
//! ordering. These helpers bridge the two without embedding GTK or Iced
//! concepts in either side.

use std::collections::HashSet;
use std::fmt;

/// Compute the post-removal insertion index for a source item and a raw
/// insertion boundary measured while the source is still present.
///
/// `raw_target_idx` is a boundary in `0..=len`. Landing on either side of the
/// source is a no-op. Moving toward the tail subtracts one because removing
/// the source shifts later indices left.
#[must_use]
pub fn compute_insert_idx(source_idx: usize, raw_target_idx: usize) -> Option<usize> {
    if raw_target_idx == source_idx || raw_target_idx == source_idx + 1 {
        return None;
    }
    if raw_target_idx > source_idx {
        Some(raw_target_idx - 1)
    } else {
        Some(raw_target_idx)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReorderError {
    EmptyId,
    DuplicateId(i64),
    MissingSource(i64),
    TargetOutOfRange { target: usize, len: usize },
}

impl fmt::Display for ReorderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyId => formatter.write_str("reorder IDs must be nonzero"),
            Self::DuplicateId(id) => write!(formatter, "duplicate reorder ID {id}"),
            Self::MissingSource(id) => write!(formatter, "reorder source ID {id} is missing"),
            Self::TargetOutOfRange { target, len } => {
                write!(
                    formatter,
                    "reorder target {target} exceeds list length {len}"
                )
            }
        }
    }
}

impl std::error::Error for ReorderError {}

/// Move a stable ID to the requested raw insertion boundary.
///
/// Returns `Ok(None)` for a no-op and rejects malformed/ambiguous ID lists.
pub fn moved_ids(
    ids: &[i64],
    source_id: i64,
    raw_target_idx: usize,
) -> Result<Option<Vec<i64>>, ReorderError> {
    if raw_target_idx > ids.len() {
        return Err(ReorderError::TargetOutOfRange {
            target: raw_target_idx,
            len: ids.len(),
        });
    }
    let mut seen = HashSet::with_capacity(ids.len());
    for id in ids {
        if *id == 0 {
            return Err(ReorderError::EmptyId);
        }
        if !seen.insert(*id) {
            return Err(ReorderError::DuplicateId(*id));
        }
    }
    let source_idx = ids
        .iter()
        .position(|id| *id == source_id)
        .ok_or(ReorderError::MissingSource(source_id))?;
    let Some(insert_idx) = compute_insert_idx(source_idx, raw_target_idx) else {
        return Ok(None);
    };
    let mut reordered = ids.to_vec();
    let source = reordered.remove(source_idx);
    reordered.insert(insert_idx, source);
    Ok(Some(reordered))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insertion_index_matches_the_full_four_item_reference_table() {
        let cases: &[(usize, usize, Option<usize>)] = &[
            (0, 0, None),
            (0, 1, None),
            (0, 2, Some(1)),
            (0, 3, Some(2)),
            (0, 4, Some(3)),
            (1, 0, Some(0)),
            (1, 1, None),
            (1, 2, None),
            (1, 3, Some(2)),
            (1, 4, Some(3)),
            (2, 0, Some(0)),
            (2, 1, Some(1)),
            (2, 2, None),
            (2, 3, None),
            (2, 4, Some(3)),
            (3, 0, Some(0)),
            (3, 1, Some(1)),
            (3, 2, Some(2)),
            (3, 3, None),
            (3, 4, None),
        ];
        for &(source, raw_target, expected) in cases {
            assert_eq!(compute_insert_idx(source, raw_target), expected);
        }
    }

    #[test]
    fn stable_ids_move_in_both_directions_and_preserve_membership() {
        let ids = [11, 22, 33, 44];
        assert_eq!(moved_ids(&ids, 11, 4), Ok(Some(vec![22, 33, 44, 11])));
        assert_eq!(moved_ids(&ids, 44, 0), Ok(Some(vec![44, 11, 22, 33])));
        assert_eq!(moved_ids(&ids, 22, 2), Ok(None));
    }

    #[test]
    fn malformed_id_lists_and_targets_are_rejected() {
        assert_eq!(moved_ids(&[1, 0], 1, 0), Err(ReorderError::EmptyId));
        assert_eq!(moved_ids(&[1, 1], 1, 0), Err(ReorderError::DuplicateId(1)));
        assert_eq!(
            moved_ids(&[1, 2], 3, 0),
            Err(ReorderError::MissingSource(3))
        );
        assert_eq!(
            moved_ids(&[1, 2], 1, 3),
            Err(ReorderError::TargetOutOfRange { target: 3, len: 2 })
        );
    }
}
