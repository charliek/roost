// The agent-hooks consent sheet — the AppKit half of
// `AgentHooksCard.swift`, and the Mac's counterpart to the iced card in
// `crates/roost-iced/src/app/agent_hooks_dialog.rs`.
//
// An `NSAlert` presented as a window sheet, with an accessory view of
// five switch rows. Everything it decides — which switches start on,
// what the primary button says, what each row's status line reads — is
// the card's; this file only draws it and reports the answer back.

import AppKit

/// The accessory view's width. Wide enough for the longest of the five
/// file paths at the sheet's font without the alert growing a scroller.
private let agentHooksSheetWidth: CGFloat = 520

/// One live consent sheet.
///
/// A class rather than a function because the switches have to reach
/// something after the sheet is up: each toggle re-derives the primary
/// button's title, which names the count. It keeps itself alive through
/// the completion handler and lets go there.
@MainActor
final class AgentHooksSheetController: NSObject {
    private var card: AgentHooksCard
    private let alert = NSAlert()
    private let confirm: (String) -> Void
    private let dismissed: () -> Void
    private var retained: AgentHooksSheetController?

    init(
        card: AgentHooksCard,
        confirm: @escaping (String) -> Void,
        dismissed: @escaping () -> Void = {}
    ) {
        self.card = card
        self.confirm = confirm
        self.dismissed = dismissed
        super.init()
    }

    /// Put the sheet on `window`, or run it modally when there isn't
    /// one — the same fallback the daemon-unreachable alert uses, so a
    /// launch that has not built its window yet still asks.
    func present(in window: NSWindow?) {
        alert.alertStyle = .informational
        alert.messageText = AgentHooksCopy.title
        alert.informativeText = AgentHooksCopy.lede
        alert.accessoryView = buildAccessory()
        // `card.buttons` is already in the order AppKit wants: its
        // first button is the rightmost and the default, so the primary
        // goes on first.
        for title in card.buttons { alert.addButton(withTitle: title) }

        retained = self
        if let window {
            alert.beginSheetModal(for: window) { [weak self] response in
                self?.finish(response)
            }
        } else {
            finish(alert.runModal())
        }
    }

    private func finish(_ response: NSApplication.ModalResponse) {
        defer { retained = nil }
        if response == .alertFirstButtonReturn {
            confirm(card.setSpec)
        } else {
            dismissed()
        }
    }

    // MARK: - Drawing

    private func buildAccessory() -> NSView {
        let stack = NSStackView()
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 12
        stack.translatesAutoresizingMaskIntoConstraints = false

        for (index, row) in card.rows.enumerated() {
            stack.addArrangedSubview(rowView(row, index: index))
        }
        stack.addArrangedSubview(wrappingLabel(AgentHooksCopy.footer, size: 11, secondary: true))

        let container = NSView()
        container.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            stack.topAnchor.constraint(equalTo: container.topAnchor),
            stack.bottomAnchor.constraint(equalTo: container.bottomAnchor),
            stack.widthAnchor.constraint(equalToConstant: agentHooksSheetWidth),
        ])
        container.layoutSubtreeIfNeeded()
        // NSAlert sizes an accessory view from its frame, not from its
        // constraints, so the frame is set from the laid-out height.
        container.frame = NSRect(
            x: 0, y: 0, width: agentHooksSheetWidth, height: stack.fittingSize.height)
        return container
    }

    private func rowView(_ row: AgentHooksSheetRow, index: Int) -> NSView {
        let toggle = NSSwitch()
        toggle.state = row.on ? .on : .off
        toggle.tag = index
        toggle.target = self
        toggle.action = #selector(switchToggled(_:))
        toggle.setContentHuggingPriority(.required, for: .horizontal)
        toggle.setAccessibilityLabel(row.displayName)

        let heading = NSStackView()
        heading.orientation = .horizontal
        heading.spacing = 8
        heading.alignment = .firstBaseline
        let name = NSTextField(labelWithString: row.displayName)
        name.font = .systemFont(ofSize: 13, weight: .semibold)
        heading.addArrangedSubview(name)
        heading.addArrangedSubview(chipLabel(row.chip, found: row.found))

        let text = NSStackView()
        text.orientation = .vertical
        text.alignment = .leading
        text.spacing = 2
        text.addArrangedSubview(heading)
        for file in row.files {
            text.addArrangedSubview(wrappingLabel(file, size: 11, secondary: true, mono: true))
        }
        if let status = row.status {
            text.addArrangedSubview(wrappingLabel(status, size: 11, secondary: true))
        }
        if let note = row.note {
            text.addArrangedSubview(wrappingLabel(note, size: 11, secondary: true))
        }

        let rowStack = NSStackView()
        rowStack.orientation = .horizontal
        rowStack.alignment = .top
        rowStack.spacing = 10
        rowStack.addArrangedSubview(toggle)
        rowStack.addArrangedSubview(text)
        // The found rows are the ones Roost is proposing to touch, so
        // they are the ones the eye should land on first.
        if row.found {
            let backing = NSBox()
            backing.boxType = .custom
            backing.borderWidth = 0
            backing.cornerRadius = 6
            backing.fillColor = .controlAccentColor.withAlphaComponent(0.08)
            backing.contentView = rowStack
            backing.contentViewMargins = NSSize(width: 8, height: 6)
            return backing
        }
        return rowStack
    }

    private func chipLabel(_ text: String, found: Bool) -> NSTextField {
        let chip = NSTextField(labelWithString: text)
        chip.font = .systemFont(ofSize: 10, weight: .medium)
        chip.textColor = found ? .controlAccentColor : .tertiaryLabelColor
        return chip
    }

    private func wrappingLabel(
        _ text: String, size: CGFloat, secondary: Bool = false, mono: Bool = false
    ) -> NSTextField {
        let label = NSTextField(wrappingLabelWithString: text)
        label.font =
            mono
            ? .monospacedSystemFont(ofSize: size, weight: .regular)
            : .systemFont(ofSize: size)
        label.textColor = secondary ? .secondaryLabelColor : .labelColor
        label.isSelectable = false
        label.preferredMaxLayoutWidth = agentHooksSheetWidth - 60
        return label
    }

    @objc private func switchToggled(_ sender: NSSwitch) {
        card.set(sender.state == .on, at: sender.tag)
        // The primary names the count, so it has to be re-derived on
        // every flip — it is the last thing read before five config
        // files are edited.
        alert.buttons.first?.title = card.confirmLabel
    }
}
