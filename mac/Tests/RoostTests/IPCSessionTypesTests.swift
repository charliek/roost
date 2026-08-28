// IPCSessionTypesTests — the Swift half of the host-session wire
// types (plan 033 §D4). Loads the same
// `tests/ipc-vectors/session.identify.response.json` and
// `events.batch.json` the Rust `wire_types_test.rs` decodes, so a
// field rename on either side surfaces here rather than in HS-1's
// first attach.
//
// XCTest, not swift-testing: instantaneous value checks are the shape
// that aborts swiftpm-testing-helper on Xcode 26.x (issue #289). Same
// reasoning as `SocketLivenessTests`.

import Foundation
import XCTest

@testable import Roost

final class IPCSessionTypesTests: XCTestCase {
    func testSessionProtocolVersionIsSeparateFromTheWireVersion() {
        XCTAssertEqual(ipcSessionProtocolVersion, 1)
        XCTAssertEqual(ipcProtocolVersion, 1)
    }

    func testAttachPayloadKindEncodesAsABareString() throws {
        let data = try JSONEncoder().encode(IPCAttachPayloadKind.ghosttySnapshot)
        XCTAssertEqual(String(decoding: data, as: UTF8.self), "\"ghostty-snapshot\"")

        let decoded = try JSONDecoder().decode(
            IPCAttachPayloadKind.self, from: Data("\"vt\"".utf8))
        XCTAssertEqual(decoded, IPCAttachPayloadKind.vt)
        XCTAssertEqual(decoded.value, "vt")
    }

    func testUnknownAttachPayloadKindSurvivesARoundTrip() throws {
        let decoded = try JSONDecoder().decode(
            IPCAttachPayloadKind.self, from: Data("\"sixel-mosaic-v9\"".utf8))
        XCTAssertEqual(decoded.value, "sixel-mosaic-v9")
        let re = try JSONEncoder().encode(decoded)
        XCTAssertEqual(String(decoding: re, as: UTF8.self), "\"sixel-mosaic-v9\"")
    }

    func testSessionIdentifyVectorDecodes() throws {
        let raw = try Data(contentsOf: vectorURL("session.identify.response.json"))
        let response = try JSONDecoder().decode(IPCResponse.self, from: raw)
        XCTAssertTrue(response.ok)
        let result = try XCTUnwrap(response.result)
        let body = try JSONSerialization.data(withJSONObject: result.value)
        let identify = try JSONDecoder().decode(IPCSessionIdentify.self, from: body)

        XCTAssertEqual(identify.appVersion, "0.0.18")
        XCTAssertEqual(identify.sessionProtocol, ipcSessionProtocolVersion)
        XCTAssertEqual(
            identify.payloadKinds, [IPCAttachPayloadKind.ghosttySnapshot, IPCAttachPayloadKind.vt])
        XCTAssertEqual(identify.libghosttyBuild, "ghostty-3f6b1c9a4d2e5f80+snapshot.v1")
        XCTAssertEqual(identify.sessionID, "01K3S8TQ4F0Q9YB2K6WZ5D7XN")
        XCTAssertEqual(identify.startedAt, "2026-08-27T14:03:11Z")

        let reencoded = try JSONEncoder().encode(identify)
        XCTAssertEqual(try JSONDecoder().decode(IPCSessionIdentify.self, from: reencoded), identify)
    }

    func testSessionIdentifyToleratesUnknownFields() throws {
        let json = """
            {"app_version":"0.0.18","session_protocol":1,"payload_kinds":["vt"],
             "libghostty_build":"b","session_id":"s","started_at":"t",
             "capabilities":["mosh"],"future_field":1}
            """
        let identify = try JSONDecoder().decode(IPCSessionIdentify.self, from: Data(json.utf8))
        XCTAssertEqual(identify.payloadKinds, [IPCAttachPayloadKind.vt])
    }

    func testEventBatchVectorDecodesAndKeepsOrder() throws {
        let raw = try Data(contentsOf: vectorURL("events.batch.json"))
        let batch = try JSONDecoder().decode(IPCEventBatch.self, from: raw)
        XCTAssertEqual(batch.revision, 42)
        XCTAssertEqual(batch.events.map(\.event), ["tab.opened", "active.changed"])

        // Decode the nested tab through the real IPCTab twin, matching
        // the Rust test's typed decode — an AnyCodable-only read would
        // let a Swift coding-key drift slide past the shared vector.
        let opened = try XCTUnwrap(batch.events[0].data.value as? [String: Any])
        let tabJSON = try JSONSerialization.data(withJSONObject: XCTUnwrap(opened["tab"]))
        let tab = try JSONDecoder().decode(IPCTab.self, from: tabJSON)
        XCTAssertEqual(tab.id, 5)
        XCTAssertEqual(tab.projectID, 1)
        XCTAssertEqual(tab.shellState, .foregroundProcess)

        let active = try XCTUnwrap(batch.events[1].data.value as? [String: Any])
        XCTAssertEqual(active["project_id"] as? String, "1")
        XCTAssertEqual(active["tab_id"] as? String, "5")

        let reencoded = try JSONEncoder().encode(batch)
        let back = try JSONDecoder().decode(IPCEventBatch.self, from: reencoded)
        XCTAssertEqual(back.revision, batch.revision)
        XCTAssertEqual(back.events.map(\.event), batch.events.map(\.event))
    }

    func testEventBatchToleratesUnknownFieldsAndAnAbsentEventList() throws {
        let json = """
            {"revision":3,"events":[{"event":"tab.closed","data":{},"seq":11}],"dropped":false}
            """
        let batch = try JSONDecoder().decode(IPCEventBatch.self, from: Data(json.utf8))
        XCTAssertEqual(batch.revision, 3)
        XCTAssertEqual(batch.events.map(\.event), ["tab.closed"])

        let fence = try JSONDecoder().decode(IPCEventBatch.self, from: Data("{\"revision\":4}".utf8))
        XCTAssertEqual(fence.revision, 4)
        XCTAssertTrue(fence.events.isEmpty)
    }

    private func vectorURL(_ name: String) throws -> URL {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<4 {
            root.deleteLastPathComponent()
        }
        let url =
            root
            .appendingPathComponent("tests")
            .appendingPathComponent("ipc-vectors")
            .appendingPathComponent(name)
        guard FileManager.default.fileExists(atPath: url.path) else {
            throw NSError(
                domain: "IPCVectors",
                code: 1,
                userInfo: [
                    NSLocalizedDescriptionKey:
                        "vector not found at \(url.path); did the repo layout change?"
                ]
            )
        }
        return url
    }
}
