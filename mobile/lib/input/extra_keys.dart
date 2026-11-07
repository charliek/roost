import 'package:flutter/material.dart';

import '../core/models.dart';

typedef ExtraKey = ({String id, String label, KeyInput input});

/// The keys a phone keyboard lacks, after Esc/Tab and the sticky modifiers.
const List<ExtraKey> extraKeys = [
  (id: 'left', label: '←', input: KeyInput('ArrowLeft')),
  (id: 'up', label: '↑', input: KeyInput('ArrowUp')),
  (id: 'down', label: '↓', input: KeyInput('ArrowDown')),
  (id: 'right', label: '→', input: KeyInput('ArrowRight')),
  (id: 'pipe', label: '|', input: KeyInput('|', text: '|')),
  (id: 'tilde', label: '~', input: KeyInput('~', text: '~')),
  (id: 'slash', label: '/', input: KeyInput('/', text: '/')),
  (id: 'dash', label: '-', input: KeyInput('-', text: '-')),
  (id: 'pgup', label: 'PgUp', input: KeyInput('PageUp')),
  (id: 'pgdn', label: 'PgDn', input: KeyInput('PageDown')),
];

const ExtraKey escKey = (id: 'esc', label: 'Esc', input: KeyInput('Escape'));
const ExtraKey tabKey = (id: 'tab', label: 'Tab', input: KeyInput('Tab'));

/// Sticky Ctrl/Alt turn one typed character into a modified key. Longer text
/// (an IME commit, a paste) is not a key press and passes through: null.
KeyInput? stickyKeyFor(String text, {required bool ctrl, required bool alt}) {
  if (!ctrl && !alt) return null;
  if (text.characters.length != 1) return null;
  final lower = text.toLowerCase();
  return KeyInput(
    lower,
    text: text,
    ctrl: ctrl,
    alt: alt,
    shift: lower != text,
  );
}

/// A scrolling row of virtual keys above the soft keyboard. [ExcludeFocus]
/// keeps a tap from taking focus off the terminal, so the keyboard stays up.
class ExtraKeysBar extends StatelessWidget {
  const ExtraKeysBar({
    super.key,
    required this.ctrlArmed,
    required this.altArmed,
    required this.onToggleCtrl,
    required this.onToggleAlt,
    required this.onKey,
  });

  final bool ctrlArmed;
  final bool altArmed;
  final VoidCallback onToggleCtrl;
  final VoidCallback onToggleAlt;
  final ValueChanged<KeyInput> onKey;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    Widget key(ExtraKey k) => _KeyButton(
      key: ValueKey('extra-key-${k.id}'),
      label: k.label,
      onTap: () => onKey(k.input),
    );
    return ExcludeFocus(
      child: Container(
        key: const ValueKey('extra-keys'),
        height: 48,
        decoration: BoxDecoration(
          color: scheme.surfaceContainer,
          border: Border(top: BorderSide(color: scheme.outlineVariant)),
        ),
        child: ListView(
          scrollDirection: Axis.horizontal,
          padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 6),
          children: [
            key(escKey),
            key(tabKey),
            _KeyButton(
              key: const ValueKey('extra-key-ctrl'),
              label: 'Ctrl',
              armed: ctrlArmed,
              onTap: onToggleCtrl,
            ),
            _KeyButton(
              key: const ValueKey('extra-key-alt'),
              label: 'Alt',
              armed: altArmed,
              onTap: onToggleAlt,
            ),
            for (final k in extraKeys) key(k),
          ],
        ),
      ),
    );
  }
}

class _KeyButton extends StatelessWidget {
  const _KeyButton({
    super.key,
    required this.label,
    required this.onTap,
    this.armed = false,
  });

  final String label;
  final VoidCallback onTap;
  final bool armed;

  static const _style = ButtonStyle(
    visualDensity: VisualDensity.compact,
    padding: WidgetStatePropertyAll(EdgeInsets.symmetric(horizontal: 12)),
    minimumSize: WidgetStatePropertyAll(Size(44, 36)),
    tapTargetSize: MaterialTapTargetSize.shrinkWrap,
  );

  @override
  Widget build(BuildContext context) {
    final child = Text(label);
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 2),
      child: Semantics(
        toggled: armed,
        child: armed
            ? FilledButton(onPressed: onTap, style: _style, child: child)
            : FilledButton.tonal(onPressed: onTap, style: _style, child: child),
      ),
    );
  }
}
