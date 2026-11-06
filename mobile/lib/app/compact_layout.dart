import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../core/roost_core.dart';
import '../hosts/host_widgets.dart';
import '../hosts/workspace_list.dart';
import '../terminal/terminal_pane.dart';
import 'providers.dart';
import 'shared_widgets.dart';
import 'theme.dart';
import 'workspace_actions.dart';

/// < 600 dp: one screen at a time, hosts → projects and tabs → terminal,
/// driven by the selection so Back pops a level.
class CompactLayout extends ConsumerWidget {
  const CompactLayout({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final sel = ref.watch(selectionProvider);
    final hostId = sel.hostId;
    final tabId = sel.tabId;
    final Widget screen;
    final String level;
    if (hostId == null) {
      screen = const HostsScreen();
      level = 'hosts';
    } else if (tabId == null) {
      screen = ProjectsScreen(hostId: hostId);
      level = 'projects';
    } else {
      screen = TerminalScreen(hostId: hostId, tabId: tabId);
      level = 'terminal';
    }
    return PopScope(
      canPop: hostId == null,
      onPopInvokedWithResult: (didPop, _) {
        if (didPop) return;
        final notifier = ref.read(selectionProvider.notifier);
        if (tabId != null) {
          notifier.clearTab();
        } else {
          notifier.clear();
        }
      },
      child: AnimatedSwitcher(
        duration: const Duration(milliseconds: 160),
        child: KeyedSubtree(key: ValueKey('screen-$level'), child: screen),
      ),
    );
  }
}

class HostsScreen extends ConsumerWidget {
  const HostsScreen({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final hosts = ref.watch(hostsProvider).value ?? const <HostView>[];
    final waitingHost = hosts.where((h) => h.waitingTabs > 0).firstOrNull;
    return Scaffold(
      appBar: AppBar(
        title: const Text('Hosts'),
        actions: [
          IconButton(
            key: const ValueKey('add-host'),
            tooltip: 'Add host',
            icon: const Icon(Icons.add),
            onPressed: () => addHostFlow(context, ref),
          ),
        ],
      ),
      body: ListView(
        padding: const EdgeInsets.fromLTRB(16, 4, 16, 24),
        children: [
          if (waitingHost != null) ...[
            _WaitingBanner(host: waitingHost),
            const SizedBox(height: 12),
          ],
          for (final host in hosts) ...[
            HostCard(host: host),
            const SizedBox(height: 10),
          ],
          if (hosts.isEmpty)
            EmptyState(
              icon: Icons.dns_outlined,
              title: 'No hosts yet',
              message: 'Add a machine you reach over Tailscale SSH.',
              action: FilledButton(
                onPressed: () => addHostFlow(context, ref),
                child: const Text('Add host'),
              ),
            ),
        ],
      ),
    );
  }
}

class HostCard extends ConsumerWidget {
  const HostCard({super.key, required this.host});

  final HostView host;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final text = Theme.of(context).textTheme;
    return Card(
      key: ValueKey('host-card-${host.id}'),
      clipBehavior: Clip.antiAlias,
      child: InkWell(
        onTap: host.isConnected
            ? () => ref.read(selectionProvider.notifier).selectHost(host.id)
            : null,
        onLongPress: () => removeHostFlow(context, ref, host),
        child: Padding(
          padding: const EdgeInsets.fromLTRB(16, 8, 4, 14),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Row(
                children: [
                  HostStatusDot(state: host.state),
                  const SizedBox(width: 10),
                  Expanded(
                    child: Text(
                      host.label,
                      style: text.titleMedium?.copyWith(
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
              Text(
                host.target,
                style: const TextStyle(
                  fontFamily: monoFamily,
                  fontSize: 12,
                  color: RoostColors.muted,
                ),
              ),
              const SizedBox(height: 10),
              Padding(
                padding: const EdgeInsets.only(right: 12),
                child: host.isConnected
                    ? HostCounts(host: host)
                    : HostStateCard(host: host),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

/// Jumps to the first tab waiting on the user.
class _WaitingBanner extends ConsumerWidget {
  const _WaitingBanner({required this.host});

  final HostView host;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final workspace = ref.watch(workspaceProvider(host.id)).value;
    TabView? waiting;
    String? project;
    for (final p in workspace?.projects ?? const <ProjectView>[]) {
      for (final t in p.tabs) {
        if (waiting == null && t.agent == AgentBadge.waiting) {
          waiting = t;
          project = p.name;
        }
      }
    }
    final tab = waiting;
    final n = host.waitingTabs;
    return Material(
      key: const ValueKey('waiting-banner'),
      color: RoostColors.waitingSurface,
      shape: RoundedRectangleBorder(
        side: const BorderSide(color: RoostColors.waitingBorder),
        borderRadius: BorderRadius.circular(14),
      ),
      child: InkWell(
        borderRadius: BorderRadius.circular(14),
        onTap: tab == null
            ? null
            : () => ref
                  .read(selectionProvider.notifier)
                  .selectTab(host.id, tab.id),
        child: Padding(
          padding: const EdgeInsets.all(14),
          child: Row(
            children: [
              const StatusDot(color: RoostColors.waiting),
              const SizedBox(width: 12),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(
                      n == 1
                          ? '1 agent is waiting on you'
                          : '$n agents are waiting on you',
                      style: const TextStyle(fontWeight: FontWeight.w600),
                    ),
                    if (tab != null)
                      Text(
                        '${tab.title} on ${host.label} › $project',
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: const TextStyle(
                          fontSize: 13,
                          color: RoostColors.muted,
                        ),
                      ),
                  ],
                ),
              ),
              const Text('Open', style: TextStyle(color: RoostColors.waiting)),
            ],
          ),
        ),
      ),
    );
  }
}

class ProjectsScreen extends ConsumerWidget {
  const ProjectsScreen({super.key, required this.hostId});

  final String hostId;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final host = (ref.watch(hostsProvider).value ?? const <HostView>[])
        .where((h) => h.id == hostId)
        .firstOrNull;
    final text = Theme.of(context).textTheme;
    return Scaffold(
      appBar: AppBar(
        leading: BackButton(
          onPressed: () => ref.read(selectionProvider.notifier).clear(),
        ),
        title: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(host?.label ?? hostId),
            if (host != null)
              Text(
                hostStateLabel(host.state),
                style: text.bodySmall?.copyWith(color: RoostColors.muted),
              ),
          ],
        ),
        actions: [if (host != null) HostMenuButton(host: host)],
      ),
      body: host == null
          ? const SizedBox.shrink()
          : ListView(
              key: const ValueKey('compact-projects'),
              padding: const EdgeInsets.only(bottom: 24),
              children: [
                if (host.isConnected)
                  WorkspaceList(hostId: hostId)
                else
                  Padding(
                    padding: const EdgeInsets.all(16),
                    child: HostStateCard(host: host),
                  ),
              ],
            ),
    );
  }
}

/// The terminal full screen; a fling moves between the project's tabs.
class TerminalScreen extends ConsumerWidget {
  const TerminalScreen({super.key, required this.hostId, required this.tabId});

  final String hostId;
  final int tabId;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final workspace = ref.watch(workspaceProvider(hostId)).value;
    final project = workspace?.projectOfTab(tabId);
    final tabs = project?.tabs ?? const <TabView>[];
    final index = tabs.indexWhere((t) => t.id == tabId);
    final selection = ref.read(selectionProvider.notifier);
    return Scaffold(
      body: SafeArea(
        child: workspace != null && index < 0
            ? EmptyState(
                icon: Icons.tab_unselected,
                title: 'Tab closed',
                message: 'Go back and pick another tab.',
                action: TextButton(
                  onPressed: selection.clearTab,
                  child: const Text('Back'),
                ),
              )
            : TerminalPane(
                key: ValueKey('pane-$hostId-$tabId'),
                hostId: hostId,
                tabId: tabId,
                leading: BackButton(onPressed: selection.clearTab),
                subtitle: project == null
                    ? null
                    : '${project.name} · ${index + 1} of ${tabs.length}',
                onSwipeTab: (delta) {
                  final next = index + delta;
                  if (next >= 0 && next < tabs.length) {
                    selection.selectTab(hostId, tabs[next].id);
                  }
                },
              ),
      ),
    );
  }
}
