// MotionThrottleTests — pure-state throttle for mode 1003 motion
// reports. 60 Hz cap + per-cell suppression.
//
// Tests drive the same `wouldEmit` + `commit` pair the production
// path uses (TerminalView.emitMouseTracking). The production
// contract is "peek; if the encoder succeeds, commit" so committing
// only happens for events the encoder actually emitted — this test
// matrix mirrors that pattern instead of using a single-call
// shortcut.

import Foundation
import Testing

@testable import Roost

@Suite("Motion 1003 throttle")
struct MotionThrottleTests {

    @Test("first call always emits (state is empty)")
    func firstEmit() {
        var e = MotionEmitter()
        #expect(e.wouldEmit(col: 5, row: 3, nowSeconds: 0.0) == true)
        e.commit(col: 5, row: 3, nowSeconds: 0.0)
        #expect(e.lastCell?.col == 5)
        #expect(e.lastCell?.row == 3)
        #expect(e.lastEmit == 0.0)
    }

    @Test("same cell within 16 ms → suppress")
    func sameCellWithinMinInterval() {
        var e = MotionEmitter()
        e.commit(col: 5, row: 3, nowSeconds: 0.0)
        #expect(e.wouldEmit(col: 5, row: 3, nowSeconds: 0.005) == false)
    }

    @Test("same cell after 100 ms → still suppress (per-cell dedup)")
    func sameCellAfterMinInterval() {
        var e = MotionEmitter()
        e.commit(col: 5, row: 3, nowSeconds: 0.0)
        #expect(e.wouldEmit(col: 5, row: 3, nowSeconds: 0.100) == false)
    }

    @Test("different cell within 16 ms → suppress (rate cap)")
    func differentCellWithinMinInterval() {
        var e = MotionEmitter()
        e.commit(col: 5, row: 3, nowSeconds: 0.0)
        #expect(e.wouldEmit(col: 6, row: 3, nowSeconds: 0.010) == false)
    }

    @Test("different cell after 16 ms → emit")
    func differentCellAfterMinInterval() {
        var e = MotionEmitter()
        e.commit(col: 5, row: 3, nowSeconds: 0.0)
        #expect(e.wouldEmit(col: 6, row: 3, nowSeconds: 0.020) == true)
        // The production path advances state ONLY after a successful
        // encode — so the test also commits to verify the next peek
        // updates against the new (col, row, time).
        e.commit(col: 6, row: 3, nowSeconds: 0.020)
        #expect(e.lastCell?.col == 6)
        #expect(e.lastEmit == 0.020)
    }

    @Test("60 Hz cap: ~60 emits per second of varied motion")
    func sixtyHzCap() {
        var e = MotionEmitter()
        var emits = 0
        // 1 s of motion at 1 ms intervals (1000 events). Every event
        // is a different cell so per-cell dedup never fires; only
        // the 16 ms rate cap does.
        for ms in 0..<1000 {
            let nowSeconds = Double(ms) / 1000.0
            let col = ms % 80
            if e.wouldEmit(col: col, row: 5, nowSeconds: nowSeconds) {
                e.commit(col: col, row: 5, nowSeconds: nowSeconds)
                emits += 1
            }
        }
        // Allow a wide tolerance: 60 ± 10 is fine, the exact count
        // depends on floating-point timing. Pinning the order of
        // magnitude is the regression net.
        #expect(emits >= 55 && emits <= 70, "got \(emits) emits, expected ~60")
    }

    @Test("peek without commit leaves state unchanged (encoder declined)")
    func peekWithoutCommit() {
        var e = MotionEmitter()
        e.commit(col: 5, row: 3, nowSeconds: 0.0)
        let before = e
        // Subsequent `wouldEmit` returns true (after the 16 ms gap)
        // — but the caller (production path) only `commit`s when
        // the encoder produced bytes. If we peek and DON'T commit
        // (encoder declined under the negotiated mode), state must
        // stay frozen so the next event can retry.
        #expect(e.wouldEmit(col: 6, row: 3, nowSeconds: 0.020) == true)
        #expect(e == before, "wouldEmit must not mutate state")
    }

    @Test("commit after a declined peek still advances state correctly")
    func commitAfterDeclinedPeek() {
        // Production sequence: mode 1000 only enabled. First motion
        // at cell A: peek says emit, encoder declines, no commit.
        // Mode 1003 toggles ON. Second motion at cell A: peek must
        // still say emit (state didn't advance on the prior
        // decline), encoder emits, we commit.
        var e = MotionEmitter()
        #expect(e.wouldEmit(col: 5, row: 3, nowSeconds: 0.0) == true)
        // Encoder declined — no commit.
        #expect(e.wouldEmit(col: 5, row: 3, nowSeconds: 0.050) == true)
        e.commit(col: 5, row: 3, nowSeconds: 0.050)
        #expect(e.lastCell?.col == 5)
    }
}
