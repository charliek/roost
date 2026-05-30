// MotionThrottleTests — pure-state throttle for mode 1003 motion
// reports. 60 Hz cap + per-cell suppression. Tests inject a hand-
// rolled clock; the helper has no I/O dependency.

import Foundation
import Testing

@testable import Roost

@Suite("Motion 1003 throttle")
struct MotionThrottleTests {

    @Test("first call always emits (state is empty)")
    func firstEmit() {
        var e = MotionEmitter()
        #expect(e.shouldEmit(col: 5, row: 3, nowSeconds: 0.0) == true)
        #expect(e.lastCell?.col == 5)
        #expect(e.lastCell?.row == 3)
        #expect(e.lastEmit == 0.0)
    }

    @Test("same cell within 16 ms → suppress")
    func sameCellWithinMinInterval() {
        var e = MotionEmitter()
        _ = e.shouldEmit(col: 5, row: 3, nowSeconds: 0.0)
        #expect(e.shouldEmit(col: 5, row: 3, nowSeconds: 0.005) == false)
    }

    @Test("same cell after 100 ms → still suppress (per-cell dedup)")
    func sameCellAfterMinInterval() {
        var e = MotionEmitter()
        _ = e.shouldEmit(col: 5, row: 3, nowSeconds: 0.0)
        #expect(e.shouldEmit(col: 5, row: 3, nowSeconds: 0.100) == false)
    }

    @Test("different cell within 16 ms → suppress (rate cap)")
    func differentCellWithinMinInterval() {
        var e = MotionEmitter()
        _ = e.shouldEmit(col: 5, row: 3, nowSeconds: 0.0)
        #expect(e.shouldEmit(col: 6, row: 3, nowSeconds: 0.010) == false)
    }

    @Test("different cell after 16 ms → emit")
    func differentCellAfterMinInterval() {
        var e = MotionEmitter()
        _ = e.shouldEmit(col: 5, row: 3, nowSeconds: 0.0)
        #expect(e.shouldEmit(col: 6, row: 3, nowSeconds: 0.020) == true)
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
            if e.shouldEmit(col: ms % 80, row: 5, nowSeconds: nowSeconds) {
                emits += 1
            }
        }
        // Allow a wide tolerance: 60 ± 5 is fine, the exact count
        // depends on floating-point timing. Pinning the order of
        // magnitude is the regression net.
        #expect(emits >= 55 && emits <= 70, "got \(emits) emits, expected ~60")
    }

    @Test("suppression is idempotent (state unchanged on skip)")
    func suppressionIdempotent() {
        var e = MotionEmitter()
        _ = e.shouldEmit(col: 5, row: 3, nowSeconds: 0.0)
        let before = e
        _ = e.shouldEmit(col: 5, row: 3, nowSeconds: 0.005)  // suppressed
        #expect(e == before)
    }
}
