// Pure-logic tests for the agents palette frame (plan 005 §3.2–§3.6).
//
// Mirrors `crates/roost-ui-model/src/agent_palette.rs`'s (shared by
// iced) `mod tests` case for case: the same status vocabulary, time buckets, ordering
// tiebreaks, population filter, name fallback, and normalization rules,
// asserted on the same strings. A divergence between the two UIs is a
// red test here rather than a user noticing the palettes disagree.

import AppKit
import Testing

@testable import Roost

private let NOW: Int64 = 1_700_000_000

private func tab(_ id: Int64, _ title: String, projectID: Int64 = 1) -> Workspace.Tab {
    Workspace.Tab(
        id: id,
        projectId: projectID,
        title: title,
        cwd: "/tmp",
        agent: AgentTabState(shell: .atPrompt, lifecycle: .inactive, ownership: nil),
        hasNotification: false,
        userTitled: false,
        position: 0,
        createdAt: 0,
        lastActive: 0
    )
}

private func owned(
    _ tab: Workspace.Tab, _ source: String, _ lifecycle: AgentLifecycle, _ at: Int64
) -> Workspace.Tab {
    var t = tab
    t.agent.lifecycle = lifecycle
    t.agent.ownership = Ownership(source: source, sessionID: "s1", lastEventAt: at)
    return t
}

/// Mutate an owned tab's `Ownership`. Fails loudly rather than silently
/// skipping if the tab isn't owned, so a mis-built fixture can't test
/// the wrong thing.
private func withOwner(_ tab: Workspace.Tab, _ edit: (inout Ownership) -> Void) -> Workspace.Tab {
    var t = tab
    guard var owner = t.agent.ownership else {
        Issue.record("fixture tab is not owned")
        return t
    }
    edit(&owner)
    t.agent.ownership = owner
    return t
}

private func project(_ id: Int64, _ name: String, position: Int32 = 0) -> Workspace.Project {
    Workspace.Project(id: id, name: name, cwd: "/tmp", position: position, createdAt: 0)
}

private func items(_ projects: [Workspace.Project], _ tabs: [Workspace.Tab], now: Int64 = NOW)
    -> [PaletteItem]
{
    AgentPalette.agentItems(projects: projects, tabs: tabs, now: now)
}

private func agentOf(_ item: PaletteItem) -> AgentRowData {
    guard let agent = item.agent else {
        Issue.record("expected an agent row payload on \(item.id)")
        return AgentRowData(
            effectiveLifecycle: .inactive, project: "", name: "", statusText: "", timeText: "",
            metricsText: nil)
    }
    return agent
}

// MARK: - Status text

@Test
func agentStatusTextCoversEveryLifecycle() {
    #expect(AgentPalette.statusText(effective: .working, raw: .working, detail: "") == "Working")
    #expect(
        AgentPalette.statusText(effective: .waiting, raw: .waiting, detail: "")
            == "Waiting for input")
    #expect(AgentPalette.statusText(effective: .finished, raw: .finished, detail: "") == "Finished")
    #expect(AgentPalette.statusText(effective: .failed, raw: .failed, detail: "") == "Failed")
    #expect(AgentPalette.statusText(effective: .inactive, raw: .inactive, detail: "") == "Idle")
}

@Test
func agentFailedAppendsItsDetail() {
    #expect(
        AgentPalette.statusText(effective: .failed, raw: .failed, detail: "rate_limit")
            == "Failed · rate_limit")
}

@Test
func agentFailedDetailIsCappedAtFortyChars() {
    let long = String(repeating: "x", count: 60)
    let text = AgentPalette.statusText(effective: .failed, raw: .failed, detail: long)
    let detail = String(text.dropFirst("Failed · ".count))
    #expect(detail.count == 40)
    #expect(detail.hasSuffix("…"))
    // Exactly 40 is left alone.
    let exact = String(repeating: "y", count: 40)
    #expect(
        AgentPalette.statusText(effective: .failed, raw: .failed, detail: exact)
            == "Failed · \(exact)")
}

@Test
func agentBackgroundTasksDetailRendersSingularAndPlural() {
    #expect(
        AgentPalette.statusText(effective: .working, raw: .working, detail: "background_tasks:1")
            == "Working · 1 bg task")
    #expect(
        AgentPalette.statusText(effective: .working, raw: .working, detail: "background_tasks:2")
            == "Working · 2 bg tasks")
}

@Test
func agentMalformedBackgroundTasksFallsBackToPlainWorking() {
    for detail in [
        "background_tasks:",
        "background_tasks:abc",
        "background_tasks:0",
        "background_tasks:-3",
        "background_tasks:1.5",
        "background tasks:2",
    ] {
        #expect(
            AgentPalette.statusText(effective: .working, raw: .working, detail: detail) == "Working",
            "detail \(detail) must not render a count")
    }
}

@Test
func agentForeignDetailOnAWorkingRowIsIgnored() {
    // `applyReport` preserves prior detail on empty-detail reports, so a
    // non-Claude adapter can leave stale detail behind.
    #expect(
        AgentPalette.statusText(effective: .working, raw: .working, detail: "permission_prompt")
            == "Working")
}

@Test
func agentShellDerivedWorkingNeverShowsABackgroundCount() {
    // Effective is working (foreground process) while the agent axis is
    // inactive — the count belongs to the agent, not the shell.
    #expect(
        AgentPalette.statusText(effective: .working, raw: .inactive, detail: "background_tasks:2")
            == "Working")
}

@Test
func agentStatusTextNormalizesControlCharacters() {
    #expect(
        AgentPalette.statusText(
            effective: .failed, raw: .failed, detail: "rate\nlimit\r\u{1b}[31m")
            == "Failed · ratelimit[31m")
}

// MARK: - Elapsed time

@Test
func agentElapsedBucketEdges() {
    #expect(AgentPalette.elapsedText(now: NOW, lastEventAt: NOW) == "0s")
    #expect(AgentPalette.elapsedText(now: NOW, lastEventAt: NOW - 59) == "59s")
    #expect(AgentPalette.elapsedText(now: NOW, lastEventAt: NOW - 60) == "1m")
    #expect(AgentPalette.elapsedText(now: NOW, lastEventAt: NOW - 3_599) == "59m")
    #expect(AgentPalette.elapsedText(now: NOW, lastEventAt: NOW - 3_600) == "1h")
    #expect(AgentPalette.elapsedText(now: NOW, lastEventAt: NOW - 86_399) == "23h")
    #expect(AgentPalette.elapsedText(now: NOW, lastEventAt: NOW - 86_400) == "1d")
    #expect(AgentPalette.elapsedText(now: NOW, lastEventAt: NOW - 172_800) == "2d")
}

@Test
func agentElapsedClampsAFutureStamp() {
    #expect(AgentPalette.elapsedText(now: NOW, lastEventAt: NOW + 5) == "0s")
    #expect(AgentPalette.elapsedText(now: NOW, lastEventAt: Int64.max) == "0s")
}

// MARK: - metrics segmentation

/// Every segmentation case in this section, so the concat-identity
/// property is checked against the same inputs the role assertions use.
private let metricsCases: [String] = [
    "4f +86 -12",
    GitMetrics.unknown,
    "1f +0 -0",
    "12f +4021 -998",
    "",
    "+",
    "-",
    "+abc",
    "-abc",
    "<3f & +2",
]

private func expectSegments(
    _ text: String, _ expected: [(String, AgentPalette.MetricsRole)],
    sourceLocation: SourceLocation = #_sourceLocation
) {
    let actual = AgentPalette.metricsSegments(text)
    #expect(
        actual.count == expected.count, "\(text) segment count",
        sourceLocation: sourceLocation)
    for (a, e) in zip(actual, expected) {
        #expect(a.0 == e.0, "\(text) segment text", sourceLocation: sourceLocation)
        #expect(a.1 == e.1, "\(text) segment role", sourceLocation: sourceLocation)
    }
}

@Test
func agentCanonicalMetricsSplitIntoFiveSegments() {
    expectSegments(
        "4f +86 -12",
        [("4f", .muted), (" ", .muted), ("+86", .adds), (" ", .muted), ("-12", .dels)])
}

@Test
func agentTheUnknownDashIsOneMutedSegment() {
    expectSegments(GitMetrics.unknown, [(GitMetrics.unknown, .muted)])
}

@Test
func agentZeroCountsKeepTheirRoles() {
    expectSegments(
        "1f +0 -0",
        [("1f", .muted), (" ", .muted), ("+0", .adds), (" ", .muted), ("-0", .dels)])
}

@Test
func agentLargeCountsKeepTheirRoles() {
    expectSegments(
        "12f +4021 -998",
        [("12f", .muted), (" ", .muted), ("+4021", .adds), (" ", .muted), ("-998", .dels)])
}

@Test
func agentDegenerateMetricsTokensStayMuted() {
    #expect(AgentPalette.metricsSegments("").isEmpty)
    for text in ["+", "-", "+abc", "-abc"] {
        expectSegments(text, [(text, .muted)])
    }
}

@Test
func agentSegmentsConcatenateBackToTheInput() {
    for text in metricsCases {
        let joined = AgentPalette.metricsSegments(text).map(\.0).joined()
        #expect(joined == text, "segments must reproduce \(text) exactly")
    }
}

@Test
func agentMetricsRoleColorsArePinned() {
    func expectRGB(
        _ role: AgentPalette.MetricsRole, _ r: Int, _ g: Int, _ b: Int,
        sourceLocation: SourceLocation = #_sourceLocation
    ) {
        let color = AgentPalette.metricsColor(role)
        #expect(
            abs(color.redComponent - CGFloat(r) / 255.0) < 0.001,
            sourceLocation: sourceLocation)
        #expect(
            abs(color.greenComponent - CGFloat(g) / 255.0) < 0.001,
            sourceLocation: sourceLocation)
        #expect(
            abs(color.blueComponent - CGFloat(b) / 255.0) < 0.001,
            sourceLocation: sourceLocation)
        #expect(color.alphaComponent == 1.0, sourceLocation: sourceLocation)
    }
    // Matches `crates/roost-ui-model/src/agent_palette.rs`'s
    // `metrics_role_hex` (Mac-only today — iced paints these strings
    // in a single flat `chrome::MUTED_TEXT` instead).
    expectRGB(.muted, 0x7a, 0x7a, 0x7a)
    expectRGB(.adds, 0x7f, 0xbf, 0x7f)
    expectRGB(.dels, 0xe0, 0x52, 0x52)
}

// The now-removed GTK UI also pinned markup escaping
// (`metrics_markup_escapes_segment_text`); `NSAttributedString` carries
// no markup, so that pair has no Swift counterpart.

@Test
func agentMetricsAttributedColorsEachSegment() {
    let font = NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
    let text = "4f +86 -12"
    let attributed = AgentPalette.metricsAttributed(text, font: font)
    #expect(attributed.string == text)
    #expect(attributed.attribute(.font, at: 0, effectiveRange: nil) as? NSFont == font)
    for (token, role) in [
        ("4f", AgentPalette.MetricsRole.muted), ("+86", .adds), ("-12", .dels),
    ] {
        // Per index rather than per attribute run: adjacent muted
        // segments (a token and the space after it) coalesce into one
        // run, so a run's extent says nothing about a segment's.
        let range = (text as NSString).range(of: token)
        for offset in 0..<range.length {
            let color =
                attributed.attribute(
                    .foregroundColor, at: range.location + offset, effectiveRange: nil) as? NSColor
            #expect(color == AgentPalette.metricsColor(role), "\(token) color at +\(offset)")
        }
    }
}

// MARK: - Population

@Test
func agentOnlyAgentOwnedTabsAreListed() {
    let tabs = [
        tab(1, "plain shell"),
        owned(tab(2, "claude"), "claude", .working, NOW),
        owned(tab(3, "manual"), Workspace.manualSource, .working, NOW),
        owned(tab(4, "legacy"), Workspace.legacySource, .working, NOW),
        owned(tab(5, "codex"), "codex", .waiting, NOW),
    ]
    #expect(items([project(1, "roost")], tabs).map(\.id) == ["agent:5", "agent:2"])
}

@Test
func agentAnEmptySourceIsNotLive() {
    let ghost = withOwner(owned(tab(1, "ghost"), "claude", .working, NOW)) { $0.source = "" }
    #expect(items([project(1, "roost")], [ghost])[0].id == AgentPalette.emptyRowID)
}

@Test
func agentATabWithNoProjectIsSkipped() {
    // The Mac workspace keeps projects and tabs in separate maps; a tab
    // whose project vanished mid-snapshot has no bold cell to render.
    let orphan = owned(tab(1, "claude", projectID: 99), "claude", .working, NOW)
    #expect(items([project(1, "roost")], [orphan])[0].id == AgentPalette.emptyRowID)
}

@Test
func agentAFreshlyClaimedAndAFailsafedTabStillAppear() {
    // SessionStart claims with raw `inactive`; the dead-agent failsafe
    // forces raw `inactive` while keeping ownership. Both fall through
    // to the shell axis.
    var fresh = owned(tab(1, "claude"), "claude", .inactive, NOW)
    fresh.agent.shell = .atPrompt
    var busy = owned(tab(2, "claude"), "claude", .inactive, NOW)
    busy.agent.shell = .foregroundProcess
    let rows = items([project(1, "roost")], [fresh, busy])
    #expect(rows.count == 2)
    // Shell-derived: the busy one ranks above the idle one.
    #expect(rows[0].id == "agent:2")
    #expect(agentOf(rows[0]).statusText == "Working")
    #expect(agentOf(rows[1]).statusText == "Idle")
}

@Test
func agentEmptyPopulationYieldsOneNonActionableRow() {
    let rows = items([project(1, "roost")], [tab(1, "shell")])
    #expect(rows.count == 1)
    #expect(rows[0].id == AgentPalette.emptyRowID)
    #expect(rows[0].title == AgentPalette.emptyRowTitle)
    #expect(rows[0].actionable == false)
    #expect(rows[0].agent == nil)
    // The sentinel must not parse as a jump target.
    #expect(AgentPalette.tabID(fromRowID: AgentPalette.emptyRowID) == nil)
}

@Test
func agentRowIDRoundTripsToATabID() {
    #expect(AgentPalette.tabID(fromRowID: "agent:42") == 42)
    #expect(AgentPalette.tabID(fromRowID: "agent:") == nil)
    #expect(AgentPalette.tabID(fromRowID: "agent:x") == nil)
    #expect(AgentPalette.tabID(fromRowID: "notif:7") == nil)
}

// MARK: - Name + title

@Test
func agentNamePrefersSessionTitleThenTabTitleThenPlaceholder() {
    let withMeta = withOwner(owned(tab(1, "zsh"), "claude", .working, NOW)) {
        $0.metadata[AgentPalette.sessionTitleKey] = "slauth-refactor"
    }
    let withTitle = owned(tab(2, "zsh"), "claude", .working, NOW)
    let untitled = owned(tab(3, ""), "claude", .working, NOW)

    let rows = items([project(1, "roost")], [withMeta, withTitle, untitled])
    func byID(_ id: String) -> PaletteItem {
        guard let row = rows.first(where: { $0.id == id }) else {
            Issue.record("row \(id) missing")
            return PaletteItem(id: id, title: "")
        }
        return row
    }
    #expect(agentOf(byID("agent:1")).name == "slauth-refactor")
    #expect(agentOf(byID("agent:2")).name == "zsh")
    #expect(agentOf(byID("agent:3")).name == "Tab 3")
    // Title composition is the filter input, verbatim.
    #expect(byID("agent:1").title == "roost · slauth-refactor")
}

@Test
func agentABlankSessionTitleFallsThroughToTheTabTitle() {
    let t = withOwner(owned(tab(1, "zsh"), "claude", .working, NOW)) {
        $0.metadata[AgentPalette.sessionTitleKey] = "  \n "
    }
    #expect(agentOf(items([project(1, "roost")], [t])[0]).name == "zsh")
}

@Test
func agentNamesAreNormalizedToOneLine() {
    let t = withOwner(owned(tab(1, "zsh"), "claude", .working, NOW)) {
        $0.metadata[AgentPalette.sessionTitleKey] = "line one\nline two\r\t x"
    }
    let rows = items([project(1, "roost\nnope")], [t])
    #expect(agentOf(rows[0]).name == "line oneline two x")
    #expect(agentOf(rows[0]).project == "roostnope")
    #expect(rows[0].title == "roostnope · line oneline two x")
}

@Test
func agentComposeTitleWithoutAProjectIsJustTheName() {
    #expect(AgentPalette.composeTitle(project: "", name: "claude") == "claude")
    #expect(AgentPalette.composeTitle(project: "roost", name: "claude") == "roost · claude")
}

// MARK: - Ordering

@Test
func agentRowsOrderByRankThenRecency() {
    let tabs = [
        owned(tab(1, "finished"), "claude", .finished, NOW),
        owned(tab(2, "working"), "claude", .working, NOW),
        owned(tab(3, "failed"), "claude", .failed, NOW),
        owned(tab(4, "waiting-old"), "claude", .waiting, NOW - 500),
        owned(tab(5, "waiting-new"), "claude", .waiting, NOW),
    ]
    #expect(
        items([project(1, "roost")], tabs).map(\.id)
            == ["agent:3", "agent:5", "agent:4", "agent:2", "agent:1"])
}

@Test
func agentSameSecondTiesBreakByProjectThenTabPositionThenID() {
    // Whole-second stamps make ties the common case, so the positional
    // tiebreaks decide the visible order.
    var laterTab = owned(tab(20, "b"), "claude", .working, NOW)
    laterTab.position = 5
    var samePositionHighID = owned(tab(30, "c"), "claude", .working, NOW)
    samePositionHighID.position = 0
    var samePositionLowID = owned(tab(3, "d"), "claude", .working, NOW)
    samePositionLowID.position = 0
    let secondProjectTab = owned(tab(10, "a", projectID: 2), "claude", .working, NOW)

    let projects = [project(2, "shed", position: 1), project(1, "roost", position: 0)]
    let tabs = [secondProjectTab, laterTab, samePositionHighID, samePositionLowID]
    // Project position 0 first (despite its tabs coming later in the
    // input), then tab position, then tab id within a position.
    #expect(items(projects, tabs).map(\.id) == ["agent:3", "agent:30", "agent:20", "agent:10"])
}

// MARK: - sidebarAgents

private func sidebarRows(
    _ project: Workspace.Project, _ tabs: [Workspace.Tab], now: Int64 = NOW
) -> [AgentPalette.SidebarAgentRow] {
    AgentPalette.sidebarAgents(project: project, tabs: tabs, now: now)
}

@Test
func sidebarAgentsExcludesManualLegacyAndDeadTabs() {
    let tabs = [
        tab(1, "plain shell"),
        owned(tab(2, "claude"), "claude", .working, NOW),
        owned(tab(3, "manual"), Workspace.manualSource, .working, NOW),
        owned(tab(4, "legacy"), Workspace.legacySource, .working, NOW),
        owned(tab(5, "codex"), "codex", .waiting, NOW),
    ]
    #expect(sidebarRows(project(1, "roost"), tabs).map(\.tabID) == [5, 2])
}

@Test
func sidebarAgentsOnlyIncludesItsOwnProjectsTabs() {
    let tabs = [
        owned(tab(1, "claude", projectID: 1), "claude", .working, NOW),
        owned(tab(2, "claude", projectID: 2), "claude", .working, NOW),
    ]
    #expect(sidebarRows(project(1, "roost"), tabs).map(\.tabID) == [1])
}

// The older row is given the lower position AND the lower id, so the
// assertion fails if recency is dropped from the comparator and a later
// key decides instead.
@Test
func sidebarAgentsOrdersSameLifecycleByRecencyFirst() {
    var newer = owned(tab(9, "newer"), "claude", .waiting, NOW)
    newer.position = 7
    var older = owned(tab(2, "older"), "claude", .waiting, NOW - 500)
    older.position = 0

    #expect(sidebarRows(project(1, "roost"), [older, newer]).map(\.tabID) == [9, 2])
}

@Test
func sidebarAgentsOrdersByRankThenRecencyThenTabPositionThenID() {
    var laterTab = owned(tab(20, "b"), "claude", .working, NOW)
    laterTab.position = 5
    var samePositionHighID = owned(tab(30, "c"), "claude", .working, NOW)
    samePositionHighID.position = 0
    var samePositionLowID = owned(tab(3, "d"), "claude", .working, NOW)
    samePositionLowID.position = 0
    let failed = owned(tab(1, "failed"), "claude", .failed, NOW)
    let waitingOld = owned(tab(4, "waiting-old"), "claude", .waiting, NOW - 500)

    let tabs = [laterTab, samePositionHighID, samePositionLowID, failed, waitingOld]
    // rank: failed > waiting > working; then within working, position 0
    // before position 5, then id 3 before id 30.
    #expect(sidebarRows(project(1, "roost"), tabs).map(\.tabID) == [1, 4, 3, 30, 20])
}

@Test
func aLeadingAgentMarkerIsStrippedFromTheName() {
    // Claude Code's own window-title prefix, U+2733 + space.
    #expect(AgentPalette.stripLeadingMarker("\u{2733} slaudio-refactor") == "slaudio-refactor")
    #expect(AgentPalette.stripLeadingMarker("\u{2733}\u{FE0F} Claude Code") == "Claude Code")
    #expect(AgentPalette.stripLeadingMarker("\u{1F7E2} \u{1F47B} two") == "two")
}

@Test
func strippingLeavesASCIIAndNonLatinTitlesAlone() {
    for keep in ["/tmp", "~/src/roost", "[wip] refactor", "-n", "café", "日本語", "1password"] {
        #expect(AgentPalette.stripLeadingMarker(keep) == keep, "must not strip \(keep)")
    }
}

@Test
func anAllMarkerTitleSurvivesRatherThanEmptying() {
    #expect(AgentPalette.stripLeadingMarker("\u{2733}") == "\u{2733}")
}

@Test
func sidebarAgentsNameFallbackChain() {
    let withMeta = withOwner(owned(tab(1, "zsh"), "claude", .working, NOW)) {
        $0.metadata[AgentPalette.sessionTitleKey] = "slauth-refactor"
    }
    let withTitle = owned(tab(2, "zsh"), "claude", .working, NOW)
    let untitled = owned(tab(3, ""), "claude", .working, NOW)

    let rows = sidebarRows(project(1, "roost"), [withMeta, withTitle, untitled])
    func byID(_ id: Int64) -> AgentPalette.SidebarAgentRow {
        guard let row = rows.first(where: { $0.tabID == id }) else {
            Issue.record("row \(id) missing")
            return AgentPalette.SidebarAgentRow(
                tabID: id, name: "", lifecycle: .inactive, statusText: "", timeText: "")
        }
        return row
    }
    #expect(byID(1).name == "slauth-refactor")
    #expect(byID(2).name == "zsh")
    #expect(byID(3).name == "Tab 3")
}

@Test
func sidebarAgentsNormalizesAndTruncatesLikeThePalette() {
    let long = String(repeating: "x", count: 60)
    let t = withOwner(owned(tab(1, "zsh\nx"), "claude", .failed, NOW)) { $0.detail = long }
    let rows = sidebarRows(project(1, "roost"), [t])
    #expect(rows[0].name == "zshx")
    let detail = String(rows[0].statusText.dropFirst("Failed · ".count))
    #expect(detail.count == 40)
    #expect(detail.hasSuffix("…"))
}

@Test
func sidebarAgentsOnAnEmptyProjectIsEmpty() {
    #expect(sidebarRows(project(1, "roost"), []).isEmpty)
    #expect(sidebarRows(project(1, "roost"), [tab(1, "shell")]).isEmpty)
}
