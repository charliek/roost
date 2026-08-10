// SocketLivenessTests — the errno rule that decides whether the
// stale-socket recovery path may unlink a socket.
//
// Lockstep with `roost_ipc::socket_state`'s `classify_connect_error`
// tests. The rule is fail-safe: only ECONNREFUSED (nothing queued for
// accept) and ENOENT (path gone) mean stale; every other errno means
// assume-live-and-refuse. The predicate this replaced was `rc == 0`,
// which collapses every non-zero errno to "stale, unlink it" — correct
// on Darwin, and wrong the moment the same rule meets Linux, where
// connect(2) to an AF_UNIX stream socket with a full accept backlog
// returns EAGAIN from a live listener.
//
// XCTest, not swift-testing, deliberately: this is a swarm of
// instantaneous value checks, which is the shape that aborts
// swiftpm-testing-helper on Xcode 26.x (issue #289, signal 6 killing the
// whole runner mid-suite). Adding these under swift-testing reproduced
// it on CI while passing locally. Do not "modernize" this file.

import Darwin
import XCTest

@testable import Roost

final class SocketLivenessTests: XCTestCase {
    func testASuccessfulConnectIsLive() {
        XCTAssertEqual(IPCServer.classifyConnect(result: 0, errnoValue: 0), .live)
    }

    func testOnlyRefusedAndAbsentAreStale() {
        XCTAssertEqual(IPCServer.classifyConnect(result: -1, errnoValue: ECONNREFUSED), .stale)
        XCTAssertEqual(IPCServer.classifyConnect(result: -1, errnoValue: ENOENT), .stale)
    }

    /// The case that forces the rule: a live listener whose accept
    /// backlog is full. Treating it as stale would unlink a running
    /// UI's socket out from under it.
    func testAFullAcceptBacklogIsLiveNotStale() {
        XCTAssertEqual(IPCServer.classifyConnect(result: -1, errnoValue: EAGAIN), .live)
        XCTAssertEqual(IPCServer.classifyConnect(result: -1, errnoValue: EWOULDBLOCK), .live)
    }

    func testUnexpectedErrnosStayOnTheSafeSide() {
        for code in [EACCES, EPERM, EINTR, ETIMEDOUT, ENOMEM, EADDRINUSE] {
            XCTAssertEqual(
                IPCServer.classifyConnect(result: -1, errnoValue: code), .live,
                "errno \(code) must not authorize an unlink")
        }
    }
}
