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

/// The order two dispatched blocks actually ran in, readable from a
/// third thread.
private final class RunOrder: @unchecked Sendable {
    private let lock = NSLock()
    private var values: [Int] = []

    func append(_ value: Int) {
        lock.lock()
        values.append(value)
        lock.unlock()
    }

    var snapshot: [Int] {
        lock.lock()
        defer { lock.unlock() }
        return values
    }
}

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

    /// The lock path table the Rust twin pins in
    /// `crates/roost-ui-model/src/config.rs`
    /// (`the_lock_path_table_the_swift_twin_mirrors`). A config path with
    /// no parent directory of its own is where the two resolvers can
    /// quietly pick different files — negative-control target:
    /// `configLockPath`'s root handling.
    @Test func lockPathMatchesRustOnDegeneratePaths() {
        let table: [(String, String)] = [
            ("/", "./config.lock"),
            ("//", "./config.lock"),
            ("", "./config.lock"),
            ("config.conf", "./config.lock"),
            ("./config.conf", "./config.lock"),
            ("a/", "./config.lock"),
            ("/config.conf", "/config.lock"),
            ("/a/", "/config.lock"),
            ("/a/b/config.conf", "/a/b/config.lock"),
        ]
        for (config, lock) in table {
            #expect(configLockPath(beside: config) == lock, "\(config)")
        }
    }

    /// Every config write goes through one **serial** queue, so two
    /// writes take the lock in the order they were asked for. A
    /// concurrent queue leaves that to the scheduler — negative-control
    /// target: `RoostConfig.writeQueue`'s serial attribute.
    @Test func theConfigWriteQueueIsSerial() {
        let order = RunOrder()
        let done = DispatchSemaphore(value: 0)
        RoostConfig.writeQueue.async {
            Thread.sleep(forTimeInterval: 0.2)
            order.append(1)
            done.signal()
        }
        RoostConfig.writeQueue.async {
            order.append(2)
            done.signal()
        }
        done.wait()
        done.wait()
        #expect(order.snapshot == [1, 2])
    }

    /// The same property in the user's terms: two rapid font-size taps
    /// leave the *second* size on disk.
    @Test @MainActor func rapidWritesOfOneKeyLeaveTheLastValue() async throws {
        let tmp = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let configPath = tmp.appendingPathComponent("config.conf")

        let sizes = Array(10...17)
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            for size in sizes {
                RoostConfig.setKeyAsync("font-size", value: "\(size)", at: configPath) { error in
                    #expect(error == nil)
                    if size == sizes.last { continuation.resume() }
                }
            }
        }

        let written = try String(contentsOf: configPath, encoding: .utf8)
        #expect(written == "font-size = 17\n")
    }

    private func makeTempDir() throws -> URL {
        let dir = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("RoostConfigLockTests-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }
}
