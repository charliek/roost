import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../app/providers.dart';
import '../app/theme.dart';
import '../app/workspace_actions.dart';
import '../core/roost_core.dart';
import 'host_widgets.dart';

/// A host's projects, each with its tabs. Not scrollable itself: the
/// sidebar, the drawer and the phone's projects screen each scroll it.
class WorkspaceList extends ConsumerWidget {
  const WorkspaceList({super.key, required this.hostId, this.onTabSelected});

  final String hostId;

  /// After a tab is selected (a drawer closes itself here).
  final VoidCallback? onTabSelected;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final workspace =
        ref.watch(workspaceProvider(hostId)).value ?? WorkspaceView.empty;
    if (workspace.projects.isEmpty) {
      return Padding(
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
        child: Row(
          children: [
            Expanded(
              child: Text(
                'No projects yet',
                style: Theme.of(context).textTheme.bodySmall,
              ),
            ),
            TextButton(
              onPressed: () => newProjectFlow(context, ref, hostId),
              child: const Text('New project'),
            ),
          ],
        ),
      );
    }
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        for (final project in workspace.projects) ...[
          _ProjectHeader(hostId: hostId, project: project),
          for (final tab in project.tabs)
            TabTile(hostId: hostId, tab: tab, onSelected: onTabSelected),
        ],
      ],
    );
  }
}

class _ProjectHeader extends ConsumerWidget {
  const _ProjectHeader({required this.hostId, required this.project});

  final String hostId;
  final ProjectView project;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final waiting = project.tabs.where((t) => t.needsInput).length;
    return Padding(
      padding: const EdgeInsets.fromLTRB(12, 8, 0, 0),
      child: Row(
        children: [
          const Icon(Icons.folder_outlined, size: 16, color: RoostColors.muted),
          const SizedBox(width: 8),
          Expanded(
            child: Text(
              project.name,
              overflow: TextOverflow.ellipsis,
              style: Theme.of(context).textTheme.titleSmall
                  ?.copyWith(fontWeight: FontWeight.w600),
            ),
          ),
          if (waiting > 0)
            Padding(
              padding: const EdgeInsets.only(right: 4),
              child: Text(
                '$waiting waiting',
                style: const TextStyle(
                  fontSize: 12,
                  color: RoostColors.waiting,
                ),
              ),
            ),
          IconButton(
            key: ValueKey('new-tab-$hostId-${project.id}'),
            tooltip: 'New tab in ${project.name}',
            visualDensity: VisualDensity.compact,
            icon: const Icon(Icons.add, size: 18),
            onPressed: () => newTabFlow(context, ref, hostId, project.id),
          ),
          PopupMenuButton<String>(
            tooltip: 'Project actions',
            icon: const Icon(Icons.more_horiz, size: 18),
            onSelected: (action) async {
              switch (action) {
                case 'rename':
                  await renameProjectFlow(context, ref, hostId, project);
                case 'new':
                  await newProjectFlow(context, ref, hostId);
              }
            },
            itemBuilder: (context) => const [
              PopupMenuItem(value: 'rename', child: Text('Rename project')),
              PopupMenuItem(value: 'new', child: Text('New project')),
            ],
          ),
        ],
      ),
    );
  }
}

class TabTile extends ConsumerWidget {
  const TabTile({
    super.key,
    required this.hostId,
    required this.tab,
    this.onSelected,
  });

  final String hostId;
  final TabView tab;
  final VoidCallback? onSelected;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final selected = ref.watch(
      selectionProvider.select((s) => s.isTab(hostId, tab.id)),
    );
    final status = tab.statusText;
    return ListTile(
      key: ValueKey('tab-$hostId-${tab.id}'),
      dense: true,
      selected: selected,
      contentPadding: const EdgeInsets.only(left: 20, right: 12),
      leading: AgentDot(badge: tab.agent),
      minLeadingWidth: 8,
      title: Text(tab.title, maxLines: 1, overflow: TextOverflow.ellipsis),
      subtitle: status == null
          ? null
          : Text(
              status,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: TextStyle(
                color: tab.agent == AgentBadge.waiting
                    ? RoostColors.waiting
                    : RoostColors.muted,
              ),
            ),
      trailing: tab.hasNotification
          ? const StatusDot(
              key: ValueKey('notification-dot'),
              color: RoostColors.waiting,
              size: 8,
            )
          : null,
      onTap: () {
        ref.read(selectionProvider.notifier).selectTab(hostId, tab.id);
        onSelected?.call();
      },
      onLongPress: () => showTabActions(context, ref, hostId, tab),
    );
  }
}
