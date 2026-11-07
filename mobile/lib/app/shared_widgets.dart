import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../hosts/tab_strip.dart';
import '../terminal/terminal_pane.dart';
import 'providers.dart';
import 'theme.dart';

class EmptyState extends StatelessWidget {
  const EmptyState({
    super.key,
    required this.icon,
    required this.title,
    required this.message,
    this.action,
  });

  final IconData icon;
  final String title;
  final String message;
  final Widget? action;

  @override
  Widget build(BuildContext context) {
    final text = Theme.of(context).textTheme;
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(24),
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 360),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Icon(icon, size: 40, color: RoostColors.muted),
              const SizedBox(height: 12),
              Text(title, style: text.titleMedium, textAlign: TextAlign.center),
              const SizedBox(height: 6),
              Text(
                message,
                style: text.bodySmall?.copyWith(color: RoostColors.muted),
                textAlign: TextAlign.center,
              ),
              if (action != null) ...[const SizedBox(height: 16), action!],
            ],
          ),
        ),
      ),
    );
  }
}

/// The selected tab with its project's tab strip, or a prompt to pick one.
/// Shared by the tablet layouts.
class SelectedTabArea extends ConsumerWidget {
  const SelectedTabArea({super.key, this.leading});

  final Widget? leading;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final sel = ref.watch(selectionProvider);
    final hostId = sel.hostId;
    final tabId = sel.tabId;
    if (hostId == null || tabId == null) {
      return const EmptyState(
        key: ValueKey('pick-a-tab'),
        icon: Icons.terminal,
        title: 'Pick a tab',
        message:
            'Opening a tab here sizes it to this screen until you leave it '
            'or the app goes to the background.',
      );
    }
    final workspace = ref.watch(workspaceProvider(hostId)).value;
    if (workspace != null && workspace.tab(tabId) == null) {
      return const EmptyState(
        icon: Icons.tab_unselected,
        title: 'Tab closed',
        message: 'Pick another tab.',
      );
    }
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        TabStrip(hostId: hostId, tabId: tabId),
        Expanded(
          child: TerminalPane(
            key: ValueKey('pane-$hostId-$tabId'),
            hostId: hostId,
            tabId: tabId,
            leading: leading,
          ),
        ),
      ],
    );
  }
}
