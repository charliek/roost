// Mirror of `cmd/roost/sidebar_reorder_test.go::TestComputeInsertIdx`
// case-for-case (20 cases — 5 source positions × 4 raw targets each).
// The Linux Rust port at
// `crates/roost-linux/src/app.rs::compute_insert_idx_matches_go_table`
// covers the same set; this is the third corner of the triangle. Drift
// here will cause cross-UI reorder behavior to diverge.

import Foundation
import Testing
@testable import Roost

private struct ReorderCase {
    let name: String
    let source: Int
    let rawTarget: Int
    let wantIndex: Int
    let wantNoop: Bool
}

private let cases: [ReorderCase] = [
    // Source at 0 in a list of 4. rawTarget covers 0..4.
    .init(name: "src0 raw0 noop", source: 0, rawTarget: 0, wantIndex: 0, wantNoop: true),
    .init(name: "src0 raw1 noop", source: 0, rawTarget: 1, wantIndex: 0, wantNoop: true),
    .init(name: "src0 raw2", source: 0, rawTarget: 2, wantIndex: 1, wantNoop: false),
    .init(name: "src0 raw3", source: 0, rawTarget: 3, wantIndex: 2, wantNoop: false),
    .init(name: "src0 raw4 (tail)", source: 0, rawTarget: 4, wantIndex: 3, wantNoop: false),

    // Source at 1.
    .init(name: "src1 raw0", source: 1, rawTarget: 0, wantIndex: 0, wantNoop: false),
    .init(name: "src1 raw1 noop", source: 1, rawTarget: 1, wantIndex: 0, wantNoop: true),
    .init(name: "src1 raw2 noop", source: 1, rawTarget: 2, wantIndex: 0, wantNoop: true),
    .init(name: "src1 raw3", source: 1, rawTarget: 3, wantIndex: 2, wantNoop: false),
    .init(name: "src1 raw4 (tail)", source: 1, rawTarget: 4, wantIndex: 3, wantNoop: false),

    // Source at 2.
    .init(name: "src2 raw0", source: 2, rawTarget: 0, wantIndex: 0, wantNoop: false),
    .init(name: "src2 raw1", source: 2, rawTarget: 1, wantIndex: 1, wantNoop: false),
    .init(name: "src2 raw2 noop", source: 2, rawTarget: 2, wantIndex: 0, wantNoop: true),
    .init(name: "src2 raw3 noop", source: 2, rawTarget: 3, wantIndex: 0, wantNoop: true),
    .init(name: "src2 raw4 (tail)", source: 2, rawTarget: 4, wantIndex: 3, wantNoop: false),

    // Source at 3 (last). Tail-drop is a no-op (already last).
    .init(name: "src3 raw0", source: 3, rawTarget: 0, wantIndex: 0, wantNoop: false),
    .init(name: "src3 raw1", source: 3, rawTarget: 1, wantIndex: 1, wantNoop: false),
    .init(name: "src3 raw2", source: 3, rawTarget: 2, wantIndex: 2, wantNoop: false),
    .init(name: "src3 raw3 noop", source: 3, rawTarget: 3, wantIndex: 0, wantNoop: true),
    .init(name: "src3 raw4 noop (tail)", source: 3, rawTarget: 4, wantIndex: 0, wantNoop: true),
]

@Test
func computeInsertIdx_matchesGoTable() {
    for tc in cases {
        let got = computeInsertIdx(sourceIdx: tc.source, rawTargetIdx: tc.rawTarget)
        #expect(got.isNoop == tc.wantNoop, "noop mismatch for \(tc.name)")
        if !tc.wantNoop {
            #expect(got.index == tc.wantIndex, "index mismatch for \(tc.name)")
        }
    }
}
