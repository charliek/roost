// Contention tests for `ConfigLock` / `RoostConfig.setKey`'s lock.
// Mirrors `crates/roost-ui-model/src/config.rs::tests`' `ConfigLock`
// coverage (shared by iced) so the two writers agree: both take
// `flock(LOCK_EX|LOCK_NB)` on the same resolved `config.lock` path.

import Darwin
import Foundation
import Testing

@testable import Roost

@_silgen_name("flock")
private func test_flock(_ fd: Int32, _ op: Int32) -> Int32

@Suite("ConfigLock contention")
struct ConfigLockTests {
    /// Holds `config.lock` from the test itself (not through
    /// `ConfigLock`, so a bug in `ConfigLock.acquire` can't also make
    /// the setup lie) and asserts `setKey` reports busy once a short
    /// injected deadline elapses — negative-control target: the
    /// `roost_config_flock` call inside `ConfigLock.acquire`.
    @Test func setKeyReportsBusyWhenLockIsHeld() throws {
        let tmp = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let configPath = tmp.appendingPathComponent("config.conf")
        let lockPath = tmp.appendingPathComponent("config.lock")

        let fd = open(lockPath.path, O_RDWR | O_CREAT, 0o600)
        #expect(fd >= 0)
        defer { Darwin.close(fd) }
        #expect(test_flock(fd, LOCK_EX | LOCK_NB) == 0)
        defer { _ = test_flock(fd, LOCK_UN) }

        let deadline: TimeInterval = 0.2
        let started = Date()
        let error = RoostConfig.setKey(
            "theme", value: "roost-dark", at: configPath, lockDeadline: deadline)
        let elapsed = Date().timeIntervalSince(started)

        #expect(error != nil)
        #expect(elapsed >= deadline)
        // Nothing was written while the lock was contended.
        #expect(!FileManager.default.fileExists(atPath: configPath.path))
    }

    /// Once the holder releases, a waiter that hasn't yet hit its
    /// deadline lands the write — proves the poll loop isn't just a
    /// sleep-then-fail.
    @Test func setKeySucceedsOnceTheLockIsReleased() throws {
        let tmp = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let configPath = tmp.appendingPathComponent("config.conf")
        let lockPath = tmp.appendingPathComponent("config.lock")

        let fd = open(lockPath.path, O_RDWR | O_CREAT, 0o600)
        #expect(fd >= 0)
        #expect(test_flock(fd, LOCK_EX | LOCK_NB) == 0)

        let releaseAfter: TimeInterval = 0.1
        DispatchQueue.global().asyncAfter(deadline: .now() + releaseAfter) {
            _ = test_flock(fd, LOCK_UN)
            Darwin.close(fd)
        }

        let error = RoostConfig.setKey(
            "theme", value: "roost-dark", at: configPath, lockDeadline: 5)
        #expect(error == nil)
        #expect(FileManager.default.fileExists(atPath: configPath.path))
    }

    private func makeTempDir() throws -> URL {
        let dir = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("RoostConfigLockTests-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }
}
