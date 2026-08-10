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

import Darwin
import Testing

@testable import Roost

@Suite("Socket liveness errno rule")
struct SocketLivenessTests {
    @Test func aSuccessfulConnectIsLive() {
        #expect(IPCServer.classifyConnect(result: 0, errnoValue: 0) == .live)
    }

    @Test func onlyRefusedAndAbsentAreStale() {
        #expect(IPCServer.classifyConnect(result: -1, errnoValue: ECONNREFUSED) == .stale)
        #expect(IPCServer.classifyConnect(result: -1, errnoValue: ENOENT) == .stale)
    }

    /// The case that forces the rule: a live listener whose accept
    /// backlog is full. Treating it as stale would unlink a running
    /// UI's socket out from under it.
    @Test func aFullAcceptBacklogIsLiveNotStale() {
        #expect(IPCServer.classifyConnect(result: -1, errnoValue: EAGAIN) == .live)
        #expect(IPCServer.classifyConnect(result: -1, errnoValue: EWOULDBLOCK) == .live)
    }

    @Test func unexpectedErrnosStayOnTheSafeSide() {
        for code in [EACCES, EPERM, EINTR, ETIMEDOUT, ENOMEM, EADDRINUSE] {
            #expect(
                IPCServer.classifyConnect(result: -1, errnoValue: code) == .live,
                "errno \(code) must not authorize an unlink")
        }
    }
}
