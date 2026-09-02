// DesktopNotificationsAuthorizationTests — the two pure pieces behind
// the authorization refresh (#355): the status→bool mapping and the
// generation guard that decides which query owns the cached bit.
//
// Both are reachable without a `DesktopNotifications` instance on
// purpose: constructing one calls `UNUserNotificationCenter.current()`,
// which aborts the process outright in an unbundled one (the test
// runner) with `bundleProxyForCurrentProcess is nil`. The wiring these
// two feed — `applicationDidBecomeActive` → `refreshAuthorization()`,
// and which of the two writers claims its ticket where — is covered by
// code read and the live pass, not from here.
//
// XCTest, not swift-testing: a swarm of fast value-checks in the
// swift-testing suite reliably SIGABRTs `swiftpm-testing-helper` under
// Xcode 26.x (see `ShellEscapeTests.swift`'s header for the same note).

import Foundation
import UserNotifications
import XCTest

@testable import Roost

final class DesktopNotificationsAuthorizationTests: XCTestCase {
    /// Driven by raw value rather than by the named constants, so the
    /// arms are enumerated over what the type can hold rather than over
    /// what this SDK happens to name. `UNAuthorizationStatus` is an
    /// imported `NS_ENUM`: `init(rawValue:)` is failable in signature
    /// but hands back a value for any `Int`, which is exactly how an
    /// unknown status reaches us from the ObjC runtime.
    func testOnlyAnExplicitYesAuthorizesAndUnknownFallsClosed() throws {
        XCTAssertEqual(
            UNAuthorizationStatus.authorized.rawValue, 2,
            "the raw values below are the SDK's constants"
        )
        XCTAssertEqual(UNAuthorizationStatus.provisional.rawValue, 3)

        let expected: [Int: Bool] = [
            0: false,  // notDetermined
            1: false,  // denied
            2: true,  // authorized
            3: true,  // provisional
            // ephemeral. macOS marks the case unavailable, so it can't
            // be named in the mapping's switch — the unknown arm is
            // what answers, with the same verdict.
            4: false,
        ]
        for (raw, allowed) in expected {
            let status = try XCTUnwrap(UNAuthorizationStatus(rawValue: raw))
            XCTAssertEqual(
                DesktopNotifications.isAllowed(status), allowed,
                "status \(raw)"
            )
        }

        for raw in [5, 99, -1, Int.max, Int.min] {
            let status = try XCTUnwrap(UNAuthorizationStatus(rawValue: raw))
            XCTAssertFalse(
                DesktopNotifications.isAllowed(status),
                "a status this build does not know about is not consent: \(raw)"
            )
        }
    }

    func testOnlyTheNewestAnswerMayWriteTheCachedAuthorization() {
        let authorization = NotificationAuthorization()
        let overtaken = authorization.issueQuery()
        let newest = authorization.issueQuery()

        XCTAssertFalse(authorization.store(generation: overtaken, authorized: true))
        XCTAssertFalse(
            authorization.authorized,
            "a completion a later query overtook is dropped, not applied"
        )
        XCTAssertTrue(authorization.store(generation: newest, authorized: true))
        XCTAssertTrue(
            authorization.authorized,
            "the newest query's answer is the one that lands"
        )

        // The first-launch shape: a refresh is issued while the prompt
        // still stands, then the user answers. The answer claims its
        // ticket as it fires, so it outranks that refresh — and the
        // refresh's older snapshot cannot undo it afterwards.
        let refreshInFlight = authorization.issueQuery()
        authorization.store(generation: authorization.issueQuery(), authorized: false)
        XCTAssertFalse(
            authorization.authorized,
            "the user's answer outranks a refresh issued before they gave it"
        )
        XCTAssertFalse(authorization.store(generation: refreshInFlight, authorized: true))
        XCTAssertFalse(
            authorization.authorized,
            "and that refresh's stale snapshot cannot undo the answer"
        )
    }

    /// The reordering the completion queues can actually produce: two
    /// answers in flight, the newer one landing first. Whichever order
    /// the queues deliver them in, the older ticket must lose — which
    /// is only true because each writer claims its ticket where its
    /// answer arrives rather than after a hop, where the executor
    /// would be the one deciding the order.
    func testAnAnswerThatLandsAfterANewerOneStillLoses() {
        let authorization = NotificationAuthorization()
        let older = authorization.issueQuery()
        let newer = authorization.issueQuery()

        XCTAssertTrue(authorization.store(generation: newer, authorized: true))
        XCTAssertFalse(
            authorization.store(generation: older, authorized: false),
            "the older query's answer is refused even when it lands last"
        )
        XCTAssertTrue(
            authorization.authorized,
            "a late `denied` must not overwrite the newer `authorized`"
        )
    }

    /// The property the ticket counter gained when it moved off the
    /// main actor: UN answers on unspecified queues, so two completions
    /// can claim tickets at once and must never receive the same one.
    func testTicketsStayUniqueUnderConcurrentClaims() {
        let authorization = NotificationAuthorization()
        let claims = 500
        let collected = TicketCollector()

        DispatchQueue.concurrentPerform(iterations: claims) { _ in
            collected.add(authorization.issueQuery())
        }

        let tickets = collected.tickets
        XCTAssertEqual(tickets.count, claims)
        XCTAssertEqual(
            Set(tickets).count, claims,
            "two concurrent queries must never share a ticket"
        )
        XCTAssertEqual(
            authorization.issueQuery(), claims + 1,
            "every claim advanced the counter exactly once"
        )
    }

    /// What keeps the transition out of the log on the activations —
    /// nearly all of them — that confirm what we already believed.
    func testConfirmingTheCurrentValueIsNotATransition() {
        let authorization = NotificationAuthorization()
        XCTAssertTrue(authorization.store(generation: authorization.issueQuery(), authorized: true))
        XCTAssertFalse(
            authorization.store(generation: authorization.issueQuery(), authorized: true),
            "a refresh that confirms the cached value has no transition to report"
        )
        XCTAssertTrue(authorization.authorized)
    }
}

/// Somewhere to put tickets claimed off several queues at once, so the
/// test's own bookkeeping isn't what races.
private final class TicketCollector: @unchecked Sendable {
    private let lock = NSLock()
    private var claimed: [Int] = []

    var tickets: [Int] { lock.withLock { claimed } }

    func add(_ ticket: Int) {
        lock.withLock { claimed.append(ticket) }
    }
}
