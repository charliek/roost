import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../core/roost_core.dart';
import '../hosts/host_widgets.dart';
import '../hosts/workspace_list.dart';
import 'providers.dart';
import 'shared_widgets.dart';
import 'theme.dart';
import 'workspace_actions.dart';

/// 600–840 dp: a rail of hosts; the host's projects and tabs fill the rest
/// until a tab is picked, then a drawer holds them beside the terminal.
class MediumLayout extends ConsumerWidget {
  const MediumLayout({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final hosts = ref.watch(hostsProvider).value ?? const <HostView>[];
    final sel = ref.watch(selectionProvider);
    final host =
        hosts.where((h) => h.id == sel.hostId).firstOrNull ?? hosts.firstOrNull;
    final showTab = host != null && sel.hostId == host.id && sel.tabId != null;
    return Scaffold(
      drawer: host == null || !showTab
          ? null
          : Drawer(
              key: const ValueKey('projects-drawer'),
              child: SafeArea(
                child: Builder(
                  builder: (context) => ListView(
                    children: [
                      _HostHeader(host: host),
                      WorkspaceList(
                        hostId: host.id,
                        onTabSelected: () => Navigator.of(context).pop(),
                      ),
                    ],
                  ),
                ),
              ),
            ),
      body: SafeArea(
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            HostRail(hosts: hosts, selectedId: host?.id),
            const VerticalDivider(width: 1),
            Expanded(
              child: switch (host) {
                null => EmptyState(
                  icon: Icons.dns_outlined,
                  title: 'No hosts yet',
                  message: 'Add a machine you reach over Tailscale SSH.',
                  action: FilledButton(
                    onPressed: () => addHostFlow(context, ref),
                    child: const Text('Add host'),
                  ),
                ),
                _ when showTab => SelectedTabArea(
                  leading: Builder(
                    builder: (context) => IconButton(
                      key: const ValueKey('open-projects-drawer'),
                      tooltip: 'Projects',
                      icon: const Icon(Icons.menu),
                      onPressed: () => Scaffold.of(context).openDrawer(),
                    ),
                  ),
                ),
                _ => ListView(
                  key: const ValueKey('medium-projects'),
                  padding: const EdgeInsets.only(bottom: 16),
                  children: [
                    _HostHeader(host: host),
                    if (host.isConnected)
                      WorkspaceList(hostId: host.id)
                    else
                      Padding(
                        padding: const EdgeInsets.all(16),
                        child: HostStateCard(host: host),
                      ),
                  ],
                ),
              },
            ),
          ],
        ),
      ),
    );
  }
}

class _HostHeader extends StatelessWidget {
  const _HostHeader({required this.host});

  final HostView host;

  @override
  Widget build(BuildContext context) {
    final text = Theme.of(context).textTheme;
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 12, 4, 4),
      child: Row(
        children: [
          HostStatusDot(state: host.state),
          const SizedBox(width: 10),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(host.label, style: text.titleMedium),
                Text(
                  '${hostStateLabel(host.state)} · ${host.target}',
                  style: text.bodySmall?.copyWith(color: RoostColors.muted),
                ),
              ],
            ),
          ),
          HostMenuButton(host: host),
        ],
      ),
    );
  }
}

/// Hosts as a navigation rail. Hand-built rather than [NavigationRail],
/// which needs at least two destinations.
class HostRail extends ConsumerWidget {
  const HostRail({super.key, required this.hosts, required this.selectedId});

  final List<HostView> hosts;
  final String? selectedId;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    return Material(
      key: const ValueKey('host-rail'),
      color: RoostColors.surfaceLow,
      child: SizedBox(
        width: 84,
        child: ListView(
          padding: const EdgeInsets.symmetric(vertical: 8),
          children: [
            for (final host in hosts)
              _RailHost(
                host: host,
                selected: host.id == selectedId,
                onTap: () =>
                    ref.read(selectionProvider.notifier).selectHost(host.id),
              ),
            const SizedBox(height: 8),
            Center(
              child: IconButton.filledTonal(
                key: const ValueKey('add-host'),
                tooltip: 'Add host',
                icon: const Icon(Icons.add),
                onPressed: () => addHostFlow(context, ref),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _RailHost extends StatelessWidget {
  const _RailHost({
    required this.host,
    required this.selected,
    required this.onTap,
  });

  final HostView host;
  final bool selected;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final initial = host.label.isEmpty ? '?' : host.label[0].toUpperCase();
    return InkWell(
      key: ValueKey('rail-host-${host.id}'),
      onTap: onTap,
      child: Padding(
        padding: const EdgeInsets.symmetric(vertical: 6),
        child: Column(
          children: [
            Stack(
              clipBehavior: Clip.none,
              children: [
                Container(
                  width: 48,
                  height: 32,
                  alignment: Alignment.center,
                  decoration: BoxDecoration(
                    color: selected
                        ? Theme.of(context).colorScheme.secondaryContainer
                        : null,
                    borderRadius: BorderRadius.circular(16),
                  ),
                  child: Text(
                    initial,
                    style: const TextStyle(fontWeight: FontWeight.w600),
                  ),
                ),
                Positioned(
                  right: 2,
                  top: 0,
                  child: HostStatusDot(state: host.state),
                ),
                if (host.waitingTabs > 0)
                  const Positioned(
                    left: 2,
                    top: 0,
                    child: StatusDot(color: RoostColors.waiting, size: 8),
                  ),
              ],
            ),
            const SizedBox(height: 4),
            Text(
              host.label,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: TextStyle(
                fontSize: 12,
                color: selected ? RoostColors.text : RoostColors.muted,
              ),
            ),
          ],
        ),
      ),
    );
  }
}
