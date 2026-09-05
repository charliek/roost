// The agents-palette row as AppKit lays it out (plan 046 §3.7, W7).
//
// `AgentRowData` carrying an agent display name is not the behavior —
// the user seeing it is. These read the cell's LIVE stack view back, so
// a column that the wire and the composed title both still carry, but
// that the renderer never adds or never fills, fails here.

import AppKit
import Testing

@testable import Roost

private func row(
    agent: String,
    project: String = "agents-demo",
    name: String = "Roost demo file",
    status: String = "Waiting for input"
) -> AgentRowData {
    AgentRowData(
        effectiveLifecycle: .waiting,
        agent: agent,
        project: project,
        name: name,
        statusText: status,
        timeText: "3m",
        metricsText: nil)
}

@MainActor
@Test
func agentRowRendersTheDisplayNameAheadOfTheProject() {
    let cell = PaletteAgentCellView()
    cell.configure(row(agent: "Claude Code"))
    #expect(
        cell.renderedLeftColumn == [
            "Claude Code", "agents-demo", "Roost demo file", "Waiting for input",
        ])
}

@MainActor
@Test
func agentRowRendersAnUnknownSourceVerbatim() {
    let cell = PaletteAgentCellView()
    cell.configure(row(agent: "aider"))
    #expect(cell.renderedLeftColumn.first == "aider")
}

@MainActor
@Test
func agentRowReconfiguresEveryColumnOnReuse() {
    // The cell comes off `NSTableView`'s reuse queue already filled, so
    // a column left un-set would show the previous row's agent.
    let cell = PaletteAgentCellView()
    cell.configure(row(agent: "Claude Code"))
    cell.configure(row(agent: "Codex", project: "roost", name: "port", status: "Working"))
    #expect(cell.renderedLeftColumn == ["Codex", "roost", "port", "Working"])
}

@MainActor
@Test
func agentRowDropsAnEmptyDisplayNameRatherThanSpacingIt() {
    let cell = PaletteAgentCellView()
    cell.configure(row(agent: ""))
    #expect(cell.renderedLeftColumn == ["agents-demo", "Roost demo file", "Waiting for input"])
}
