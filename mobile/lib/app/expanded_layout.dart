import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../core/roost_core.dart';
import '../hosts/host_widgets.dart';
import '../hosts/workspace_list.dart';
import 'providers.dart';
import 'shared_widgets.dart';
import 'theme.dart';
import 'workspace_actions.dart';

/// ≥ 840 dp: a persistent sidebar of hosts with their projects and tabs, and
/// the selected tab beside it.
class ExpandedLayout extends StatelessWidget {
  const ExpandedLayout({super.key});

  @override
  Widget build(BuildContext context) {
    return const Scaffold(
      body: SafeArea(
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            SizedBox(width: 300, child: HostSidebar()),
            VerticalDivider(width: 1),
            Expanded(child: SelectedTabArea()),
          ],
        ),
      ),
    );
  }
}

class HostSidebar extends ConsumerWidget {
  const HostSidebar({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final hosts = ref.watch(hostsProvider).value ?? const <HostView>[];
    return Material(
      key: const ValueKey('host-sidebar'),
      color: RoostColors.surfaceLow,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 12, 8, 4),
            child: Row(
              children: [
                Expanded(
                  child: Text(
                    'Roost',
                    style: Theme.of(context).textTheme.titleLarge,
                  ),
                ),
                IconButton(
                  key: const ValueKey('add-host'),
                  tooltip: 'Add host',
                  icon: const Icon(Icons.add),
                  onPressed: () => addHostFlow(context, ref),
                ),
              ],
            ),
          ),
          Expanded(
            child: ListView(
              padding: const EdgeInsets.only(bottom: 16),
              children: [
                for (final host in hosts) HostSection(host: host),
                if (hosts.isEmpty)
                  Padding(
                    padding: const EdgeInsets.all(16),
                    child: TextButton.icon(
                      onPressed: () => addHostFlow(context, ref),
                      icon: const Icon(Icons.add),
                      label: const Text('Add host'),
                    ),
                  ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}

class HostSection extends StatelessWidget {
  const HostSection({super.key, required this.host});

  final HostView host;

  @override
  Widget build(BuildContext context) {
    final text = Theme.of(context).textTheme;
    return Padding(
      key: ValueKey('host-section-${host.id}'),
      padding: const EdgeInsets.only(top: 8),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding: const EdgeInsets.only(left: 16),
            child: Row(
              children: [
                HostStatusDot(state: host.state),
                const SizedBox(width: 10),
                Expanded(
                  child: Text(
                    host.label,
                    overflow: TextOverflow.ellipsis,
                    style: text.titleSmall?.copyWith(
                      fontWeight: FontWeight.w600,
                    ),
                  ),
                ),
                Text(
                  hostStateLabel(host.state),
                  style: text.bodySmall?.copyWith(color: RoostColors.muted),
                ),
                HostMenuButton(host: host),
              ],
            ),
          ),
          if (host.isConnected)
            WorkspaceList(hostId: host.id)
          else
            Padding(
              padding: const EdgeInsets.fromLTRB(12, 4, 12, 4),
              child: HostStateCard(host: host),
            ),
          const Divider(height: 17, indent: 12, endIndent: 12),
        ],
      ),
    );
  }
}
