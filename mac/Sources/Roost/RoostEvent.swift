// RoostEvent.swift — daemon-removal refactor M4b3b.
//
// Swift-native event enum + payload structs that App.swift's
// `handleEvent` consumes. Replaces the proto-generated
// `Roost_V1_Event.OneOf_Kind` once the gRPC stack is removed.
// The variant names + payload fields mirror the proto shapes
// exactly so the App.swift switch arms need only the
// type rename (no field access changes).

import Foundation

enum RoostEvent: Sendable {
    case projectCreated(RoostProjectCreatedEvent)
    case projectRenamed(RoostProjectRenamedEvent)
    case projectDeleted(RoostProjectDeletedEvent)
    case tabOpened(RoostTabOpenedEvent)
    case tabDeleted(RoostTabDeletedEvent)
    case tabTitle(RoostTabTitleChangedEvent)
    case tabCwd(RoostTabCwdChangedEvent)
    case tabState(RoostTabStateChangedEvent)
    case tabNotification(RoostTabNotificationEvent)
    case notification(RoostNotificationEvent)
    case hookActive(RoostHookActiveChangedEvent)
    case tabsReordered(RoostTabsReorderedEvent)
    case projectsReordered(RoostProjectsReorderedEvent)
    case active(RoostActiveChangedEvent)
}

struct RoostProjectCreatedEvent: Sendable {
    let project: ProjectSnapshot
}

struct RoostProjectRenamedEvent: Sendable {
    let projectID: Int64
    let name: String
}

struct RoostProjectDeletedEvent: Sendable {
    let projectID: Int64
}

struct RoostTabOpenedEvent: Sendable {
    let tab: RoostTabSnapshot
}

struct RoostTabDeletedEvent: Sendable {
    let tabID: Int64
}

struct RoostTabTitleChangedEvent: Sendable {
    let tabID: Int64
    let title: String
}

struct RoostTabCwdChangedEvent: Sendable {
    let tabID: Int64
    let cwd: String
}

struct RoostTabStateChangedEvent: Sendable {
    let tabID: Int64
    let state: RoostTabStateValue
}

/// Tab state surfaced through `RoostEvent.tabState`. Numeric to
/// match the legacy `Roost_V1_TabState` proto enum (the rollup
/// helper in `ProjectRollup.swift` maps integers to colors).
enum RoostTabStateValue: Int32, Sendable {
    case unspecified = 0
    case none = 1
    case running = 2
    case needsInput = 3
    case idle = 4
}

struct RoostTabNotificationEvent: Sendable {
    let tabID: Int64
    let hasPending: Bool
}

struct RoostNotificationEvent: Sendable {
    let tabID: Int64
    let title: String
    let body: String
}

struct RoostHookActiveChangedEvent: Sendable {
    let tabID: Int64
    let active: Bool
}

struct RoostTabsReorderedEvent: Sendable {
    let projectID: Int64
    let tabIds: [Int64]
}

struct RoostProjectsReorderedEvent: Sendable {
    let projectIds: [Int64]
}

struct RoostActiveChangedEvent: Sendable {
    let projectID: Int64
    let tabID: Int64
}

/// Mac-side tab snapshot — what `RoostTabOpenedEvent` carries.
/// Mirrors the legacy `Roost_V1_Tab` proto message's field
/// shape so the App.swift consumers don't change. The `state`
/// field is `RoostTabStateValue` rather than the proto's enum.
struct RoostTabSnapshot: Sendable {
    let id: Int64
    let projectID: Int64
    let title: String
    let cwd: String
    let state: RoostTabStateValue
    let hasNotification: Bool
    let isActive: Bool
    let userTitled: Bool
    let position: Int32
    let createdAt: Int64
    let lastActive: Int64
    let hookActive: Bool
}

// MARK: - Workspace.Event → RoostEvent conversion

extension Workspace.Event {
    /// Convert an in-process `Workspace.Event` to the
    /// transport-shaped `RoostEvent` that App.swift consumes.
    /// Returns nil for `Workspace.Event` variants that have no
    /// `RoostEvent` analog (none currently).
    @MainActor
    func toRoostEvent(workspace: Workspace) -> RoostEvent {
        switch self {
        case .projectCreated(let p):
            return .projectCreated(
                .init(
                    project: ProjectSnapshot(id: p.id, name: p.name, cwd: p.cwd)
                )
            )
        case .projectRenamed(let projectID, let name):
            return .projectRenamed(.init(projectID: projectID, name: name))
        case .projectDeleted(let projectID):
            return .projectDeleted(.init(projectID: projectID))
        case .tabOpened(let t):
            return .tabOpened(
                .init(
                    tab: RoostTabSnapshot(
                        id: t.id,
                        projectID: t.projectId,
                        title: t.title,
                        cwd: t.cwd,
                        state: t.state.toRoostStateValue(),
                        hasNotification: t.hasNotification,
                        isActive: workspace.activeTabID == t.id,
                        userTitled: t.userTitled,
                        position: t.position,
                        createdAt: t.createdAt,
                        lastActive: t.lastActive,
                        hookActive: t.hookActive
                    )
                )
            )
        case .tabClosed(let tabID):
            return .tabDeleted(.init(tabID: tabID))
        case .tabStateChanged(let tabID, let state):
            return .tabState(
                .init(tabID: tabID, state: state.toRoostStateValue())
            )
        case .tabTitleChanged(let tabID, let title):
            return .tabTitle(.init(tabID: tabID, title: title))
        case .tabCwdChanged(let tabID, let cwd):
            return .tabCwd(.init(tabID: tabID, cwd: cwd))
        case .tabNotification(let tabID, let hasPending):
            return .tabNotification(.init(tabID: tabID, hasPending: hasPending))
        case .activeChanged(let projectID, let tabID):
            return .active(.init(projectID: projectID, tabID: tabID))
        case .hookActiveChanged(let tabID, let active):
            return .hookActive(.init(tabID: tabID, active: active))
        case .notificationFired(let tabID, let title, let body):
            return .notification(.init(tabID: tabID, title: title, body: body))
        }
    }
}

extension Workspace.TabState {
    func toRoostStateValue() -> RoostTabStateValue {
        switch self {
        case .none: return .none
        case .running: return .running
        case .needsInput: return .needsInput
        case .idle: return .idle
        }
    }
}
