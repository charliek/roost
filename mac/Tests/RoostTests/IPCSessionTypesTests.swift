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
        // HS-1b's lease gate bumped the session protocol to 2; the
        // request/response wire version did not move with it.
        XCTAssertEqual(ipcSessionProtocolVersion, 2)
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

    // `session.stop`'s reap report is server-only — the Mac never
    // produces one, so it gets no Swift twin (same treatment as
    // `app.render_stats` and friends). What it does get is field-level
    // coverage of the shared vector, because the id encoding is the part
    // that silently breaks across languages: every tab id is a string,
    // including the ones past 2^53 that a `Double` would round.
    func testSessionStopVectorKeepsStringEncodedIDs() throws {
        let raw = try Data(contentsOf: vectorURL("session.stop.response.json"))
        let response = try JSONDecoder().decode(IPCResponse.self, from: raw)
        XCTAssertTrue(response.ok)
        let result = try XCTUnwrap(try XCTUnwrap(response.result).value as? [String: Any])
        XCTAssertEqual(Set(result.keys), ["reaped", "killed", "abandoned"])
        XCTAssertEqual(result["reaped"] as? [String], ["3", "5"])
        XCTAssertEqual(result["killed"] as? [String], ["8"])
        XCTAssertEqual(result["abandoned"] as? [String], ["9007199254740993"])
        XCTAssertEqual(Int64("9007199254740993"), 9_007_199_254_740_993)
    }

    // The revision fence (plan 035 C4). Like the reap report, these are
    // server-only results the Mac never produces — a host session does —
    // so they get field-level coverage of the shared vectors rather than
    // Swift twins. What matters across languages here is that `revision`
    // is a JSON *number* (revisions are counters, not ids, so the
    // string-int64 convention does not apply to them) and that a UI
    // socket's `tab.list` omits the key rather than sending null.
    func testEventsSubscribeAckCarriesANumericRevision() throws {
        let raw = try Data(contentsOf: vectorURL("events.subscribe.response.json"))
        let response = try JSONDecoder().decode(IPCResponse.self, from: raw)
        XCTAssertTrue(response.ok)
        let result = try XCTUnwrap(try XCTUnwrap(response.result).value as? [String: Any])
        XCTAssertEqual(Set(result.keys), ["revision"])
        XCTAssertEqual(result["revision"] as? Int64, 42)
        XCTAssertNil(result["revision"] as? String, "revision is a number, not an id string")
    }

    func testTabListFenceIsPresentOnlyOnTheSessionVector() throws {
        let session = try Data(contentsOf: vectorURL("tab.list.session.response.json"))
        let fenced = try JSONDecoder().decode(IPCResponse.self, from: session)
        let fencedResult = try XCTUnwrap(try XCTUnwrap(fenced.result).value as? [String: Any])
        XCTAssertEqual(Set(fencedResult.keys), ["projects", "revision"])
        XCTAssertEqual(fencedResult["revision"] as? Int64, 42)

        let ui = try Data(contentsOf: vectorURL("tab.list.response.json"))
        let plain = try JSONDecoder().decode(IPCResponse.self, from: ui)
        let plainResult = try XCTUnwrap(try XCTUnwrap(plain.result).value as? [String: Any])
        XCTAssertEqual(
            Set(plainResult.keys), ["projects"],
            "a UI socket's tab.list must not carry the fence at all")
    }

    func testReorderEventVectorsKeepStringEncodedIDLists() throws {
        let tabs = try Data(contentsOf: vectorURL("tabs.reordered.event.json"))
        let tabsEvent = try JSONDecoder().decode(IPCEventEnvelope.self, from: tabs)
        XCTAssertEqual(tabsEvent.event, "tabs.reordered")
        let tabsData = try XCTUnwrap(tabsEvent.data.value as? [String: Any])
        XCTAssertEqual(tabsData["project_id"] as? String, "1")
        XCTAssertEqual(tabsData["tab_ids"] as? [String], ["7", "5", "9007199254740993"])

        let projects = try Data(contentsOf: vectorURL("projects.reordered.event.json"))
        let projectsEvent = try JSONDecoder().decode(IPCEventEnvelope.self, from: projects)
        XCTAssertEqual(projectsEvent.event, "projects.reordered")
        let projectsData = try XCTUnwrap(projectsEvent.data.value as? [String: Any])
        XCTAssertEqual(projectsData["project_ids"] as? [String], ["2", "1"])
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

    // MARK: - Leases + attach (plan 036 §D11)

    func testSessionConnectVectorDecodes() throws {
        let raw = try Data(contentsOf: vectorURL("session.connect.response.json"))
        let response = try JSONDecoder().decode(IPCResponse.self, from: raw)
        XCTAssertTrue(response.ok)
        let body = try JSONSerialization.data(
            withJSONObject: try XCTUnwrap(response.result).value)
        let result = try JSONDecoder().decode(IPCSessionConnectResult.self, from: body)

        XCTAssertEqual(result.lease, "9f2c1d7a4b6e08315c0d9a72e4f16b83")
        // A counter, not an id: a bare JSON number, like the events
        // fence.
        XCTAssertEqual(result.revision, 42)

        let reencoded = try JSONEncoder().encode(result)
        XCTAssertEqual(
            try JSONDecoder().decode(IPCSessionConnectResult.self, from: reencoded), result)
    }

    func testSessionConnectRequestVectorDefaultsTakeoverToAnExplicitBool() throws {
        let raw = try Data(contentsOf: vectorURL("session.connect.request.json"))
        let request = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: raw) as? [String: Any])
        XCTAssertEqual(request["op"] as? String, "session.connect")
        let params = try XCTUnwrap(request["params"] as? [String: Any])
        XCTAssertEqual(params["takeover"] as? Bool, true)
    }

    func testTabAttachVectorsDecode() throws {
        let requestRaw = try Data(contentsOf: vectorURL("tab.attach.request.json"))
        let request = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: requestRaw) as? [String: Any])
        XCTAssertEqual(request["op"] as? String, "tab.attach")
        let params = try XCTUnwrap(request["params"] as? [String: Any])
        // The id encoding is the part that silently breaks across
        // languages: tab ids are strings, geometry is numbers.
        XCTAssertEqual(params["tab_id"] as? String, "5")
        XCTAssertEqual(params["cols"] as? Int, 120)
        XCTAssertEqual(params["rows"] as? Int, 40)
        XCTAssertEqual(params["kinds"] as? [String], ["ghostty-snapshot"])

        let raw = try Data(contentsOf: vectorURL("tab.attach.response.json"))
        let response = try JSONDecoder().decode(IPCResponse.self, from: raw)
        XCTAssertTrue(response.ok)
        let body = try JSONSerialization.data(
            withJSONObject: try XCTUnwrap(response.result).value)
        let result = try JSONDecoder().decode(IPCTabAttachResult.self, from: body)

        XCTAssertEqual(result.attachToken, "1a0be5c37d924f68b1c05e3a7f2d8496")
        XCTAssertEqual(result.kind, IPCAttachPayloadKind.ghosttySnapshot)
        // Past 2^53 and still exact: the epoch is a bare u64 number, so
        // a Swift side that routed it through a Double would drift.
        XCTAssertEqual(result.serverEpoch, 6_032_428_321_756_423_947)
        XCTAssertEqual(result.tabGeneration, 3)

        let reencoded = try JSONEncoder().encode(result)
        XCTAssertEqual(
            try JSONDecoder().decode(IPCTabAttachResult.self, from: reencoded), result)
    }

    func testSessionStoppingVectorDecodesAsAnEventEnvelope() throws {
        let raw = try Data(contentsOf: vectorURL("session.stopping.event.json"))
        let envelope = try JSONDecoder().decode(IPCEventEnvelope.self, from: raw)
        XCTAssertEqual(envelope.event, ipcSessionStoppingEvent)

        let body = try JSONSerialization.data(withJSONObject: envelope.data.value)
        let stopping = try JSONDecoder().decode(IPCSessionStoppingEvent.self, from: body)
        XCTAssertEqual(stopping.reason, "stop")

        // The envelope carries no revision — it is the one frame on an
        // events connection exempt from the gap check.
        let fields = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: raw) as? [String: Any])
        XCTAssertEqual(Set(fields.keys), ["event", "data"])
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
