import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../app/providers.dart';
import '../app/theme.dart';
import '../app/workspace_actions.dart';
import '../core/roost_core.dart';
import 'host_widgets.dart';

/// The selected tab's project, as a row of tabs plus "new tab".
class TabStrip extends ConsumerWidget {
  const TabStrip({super.key, required this.hostId, required this.tabId});

  final String hostId;
  final int tabId;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final workspace = ref.watch(workspaceProvider(hostId)).value;
    final project = workspace?.projectOfTab(tabId);
    if (project == null) return const SizedBox(height: 44);
    return Container(
      key: const ValueKey('tab-strip'),
      height: 44,
      decoration: const BoxDecoration(
        color: RoostColors.surfaceLow,
        border: Border(bottom: BorderSide(color: RoostColors.divider)),
      ),
      child: ListView(
        scrollDirection: Axis.horizontal,
        padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 5),
        children: [
          for (final tab in project.tabs)
            _TabChip(hostId: hostId, tab: tab, selected: tab.id == tabId),
          IconButton(
            key: ValueKey('strip-new-tab-${project.id}'),
            tooltip: 'New tab',
            visualDensity: VisualDensity.compact,
            icon: const Icon(Icons.add, size: 18),
            onPressed: () => newTabFlow(context, ref, hostId, project.id),
          ),
        ],
      ),
    );
  }
}

class _TabChip extends ConsumerWidget {
  const _TabChip({
    required this.hostId,
    required this.tab,
    required this.selected,
  });

  final String hostId;
  final TabView tab;
  final bool selected;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    return Padding(
      padding: const EdgeInsets.only(right: 4),
      child: Material(
        color: selected ? RoostColors.background : Colors.transparent,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(8),
          side: BorderSide(
            color: selected ? RoostColors.border : Colors.transparent,
          ),
        ),
        child: InkWell(
          key: ValueKey('strip-tab-${tab.id}'),
          borderRadius: BorderRadius.circular(8),
          onTap: () =>
              ref.read(selectionProvider.notifier).selectTab(hostId, tab.id),
          onLongPress: () => showTabActions(context, ref, hostId, tab),
          child: Padding(
            padding: const EdgeInsets.only(left: 10, right: 2),
            child: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                AgentDot(badge: tab.agent),
                const SizedBox(width: 8),
                ConstrainedBox(
                  constraints: const BoxConstraints(maxWidth: 180),
                  child: Text(
                    tab.title,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: TextStyle(
                      fontSize: 13,
                      color: selected ? RoostColors.text : RoostColors.muted,
                    ),
                  ),
                ),
                IconButton(
                  key: ValueKey('strip-close-${tab.id}'),
                  tooltip: 'Close tab',
                  visualDensity: VisualDensity.compact,
                  iconSize: 14,
                  icon: const Icon(Icons.close),
                  onPressed: () => closeTabFlow(context, ref, hostId, tab),
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}
