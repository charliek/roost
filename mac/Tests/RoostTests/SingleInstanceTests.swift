// SingleInstanceTests — covers the M4c flock-based single-instance
// enforcement. The Rust side has its own coverage in
// `crates/roost-linux/src/single_instance.rs`'s embedded #[cfg(test)]
// module; this file is the Swift mirror.

import Darwin
import Foundation
import Testing

@testable import Roost

@Suite("SingleInstance flock guard")
struct SingleInstanceTests {
    @Test func firstAcquireReturnsAcquired() throws {
        let path = uniqueLockPath()
        defer { unlink(path) }

        switch try SingleInstance.acquire(lockPath: path) {
        case .acquired(let inst):
            #expect(inst.lockPath == path)
        case .alreadyHeld(let pid):
            Issue.record("expected acquired, got alreadyHeld(\(pid))")
        case .bypassed:
            Issue.record("expected acquired, got bypassed")
        }
    }

    @Test func secondAcquireSeesAlreadyHeld() throws {
        let path = uniqueLockPath()
        defer { unlink(path) }

        let first = try SingleInstance.acquire(lockPath: path)
        guard case .acquired = first else {
            Issue.record("first acquire failed: \(first)")
            return
        }
        // The flock is held by THIS process for the lifetime of
        // `first`. flock is per-fd, so a same-process second
        // acquire on a different fd still contends — that's the
        // observable behavior we want.
        switch try SingleInstance.acquire(lockPath: path) {
        case .acquired:
            Issue.record("second acquire unexpectedly succeeded")
        case .alreadyHeld(let pid):
            // Holder PID should match our own process PID since
            // `first` wrote it.
            #expect(pid == getpid())
        case .bypassed:
            Issue.record("second acquire returned bypassed without env var")
        }
        _ = first  // keep `first` alive past the second acquire
    }

    @Test func envVarBypassReturnsBypassed() throws {
        let path = uniqueLockPath()
        defer { unlink(path) }

        let status = try SingleInstance.acquire(
            lockPath: path,
            environment: ["ROOST_ALLOW_MULTI": "1"]
        )
        guard case .bypassed = status else {
            Issue.record("expected bypassed, got \(status)")
            return
        }
        // Bypass should NOT create the lockfile — otherwise the
        // bypass would leave inert state on disk.
        #expect(!FileManager.default.fileExists(atPath: path))
    }

    @Test func releaseOnDeinitAllowsReAcquire() throws {
        let path = uniqueLockPath()
        defer { unlink(path) }

        do {
            let first = try SingleInstance.acquire(lockPath: path)
            guard case .acquired = first else {
                Issue.record("first acquire failed: \(first)")
                return
            }
            // `first` goes out of scope at the end of this block —
            // its deinit closes the fd and releases the flock.
        }

        // A fresh acquire on the same path must now succeed.
        switch try SingleInstance.acquire(lockPath: path) {
        case .acquired: break  // expected
        case .alreadyHeld(let pid):
            Issue.record("expected re-acquire, got alreadyHeld(\(pid))")
        case .bypassed:
            Issue.record("expected re-acquire, got bypassed")
        }
    }

    private func uniqueLockPath() -> String {
        let id = UUID().uuidString
        return "/tmp/roost-tests-\(id).lock"
    }
}
