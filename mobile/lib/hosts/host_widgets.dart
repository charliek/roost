import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../app/providers.dart';
import '../app/theme.dart';
import '../app/workspace_actions.dart';
import '../core/roost_core.dart';

class StatusDot extends StatelessWidget {
  const StatusDot({
    super.key,
    required this.color,
    this.size = 10,
    this.hollow = false,
  });

  final Color color;
  final double size;
  final bool hollow;

  @override
  Widget build(BuildContext context) {
    return Container(
      width: size,
      height: size,
      decoration: BoxDecoration(
        shape: BoxShape.circle,
        color: hollow ? null : color,
        border: hollow ? Border.all(color: color, width: 1.5) : null,
      ),
    );
  }
}

class HostStatusDot extends StatelessWidget {
  const HostStatusDot({super.key, required this.state});

  final HostStateView state;

  @override
  Widget build(BuildContext context) => StatusDot(
    color: hostStateColor(state),
    hollow: state is HostDisconnected || state is HostRetrying,
  );
}

class AgentDot extends StatelessWidget {
  const AgentDot({super.key, required this.badge, this.size = 8});

  final AgentBadge badge;
  final double size;

  @override
  Widget build(BuildContext context) => StatusDot(
    color: agentColor(badge),
    size: size,
    hollow: badge == AgentBadge.none,
  );
}

class HostMenuButton extends ConsumerWidget {
  const HostMenuButton({super.key, required this.host});

  final HostView host;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final core = ref.read(coreProvider);
    final live = host.state is HostConnected || host.state is HostConnecting;
    return PopupMenuButton<String>(
      key: ValueKey('host-menu-${host.id}'),
      tooltip: 'Host actions',
      icon: const Icon(Icons.more_vert, size: 20),
      onSelected: (action) async {
        switch (action) {
          case 'connect':
            await runCoreAction(context, () => core.connect(host.id));
          case 'disconnect':
            await runCoreAction(context, () => core.disconnect(host.id));
          case 'new-project':
            await newProjectFlow(context, ref, host.id);
          case 'remove':
            await removeHostFlow(context, ref, host);
        }
      },
      itemBuilder: (context) => [
        if (live)
          const PopupMenuItem(value: 'disconnect', child: Text('Disconnect'))
        else
          const PopupMenuItem(value: 'connect', child: Text('Connect')),
        if (host.state is HostConnected)
          const PopupMenuItem(value: 'new-project', child: Text('New project')),
        const PopupMenuItem(
          key: ValueKey('host-menu-remove'),
          value: 'remove',
          child: Text('Remove host'),
        ),
      ],
    );
  }
}

/// What a host that is not connected shows instead of its projects: the
/// trust prompt, the failure and a retry, or a connect button.
class HostStateCard extends ConsumerWidget {
  const HostStateCard({super.key, required this.host});

  final HostView host;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final core = ref.read(coreProvider);
    return switch (host.state) {
      HostConnected() => const SizedBox.shrink(),
      HostNeedsTrust(:final fingerprint) => TrustCard(
        host: host,
        fingerprint: fingerprint,
        onTrust: () => runCoreAction(
          context,
          () => core.trustHostKey(host.id, fingerprint),
        ),
        onCancel: () => runCoreAction(context, () => core.disconnect(host.id)),
      ),
      HostFailed(:final reason) => _ProblemCard(
        key: ValueKey('host-failed-${host.id}'),
        icon: Icons.error_outline,
        color: RoostColors.error,
        message: reason,
        action: 'Retry',
        onAction: () => runCoreAction(context, () => core.connect(host.id)),
      ),
      HostRetrying(:final reason, :final inMs) => _ProblemCard(
        key: ValueKey('host-retrying-${host.id}'),
        icon: Icons.schedule,
        color: RoostColors.muted,
        message: '$reason · retrying in ${(inMs / 1000).ceil()} s',
        action: 'Retry now',
        onAction: () => runCoreAction(context, () => core.connect(host.id)),
      ),
      HostConnecting() => const _StatusLine(
        icon: Icons.sync,
        text: 'Connecting…',
      ),
      HostDisconnected() => Align(
        alignment: Alignment.centerLeft,
        child: OutlinedButton.icon(
          key: ValueKey('host-connect-${host.id}'),
          onPressed: () => runCoreAction(context, () => core.connect(host.id)),
          icon: const Icon(Icons.power_settings_new, size: 18),
          label: const Text('Connect'),
        ),
      ),
    };
  }
}

class TrustCard extends StatelessWidget {
  const TrustCard({
    super.key,
    required this.host,
    required this.fingerprint,
    required this.onTrust,
    required this.onCancel,
  });

  final HostView host;
  final String fingerprint;
  final VoidCallback onTrust;
  final VoidCallback onCancel;

  @override
  Widget build(BuildContext context) {
    final text = Theme.of(context).textTheme;
    return Container(
      key: ValueKey('trust-card-${host.id}'),
      padding: const EdgeInsets.all(12),
      decoration: BoxDecoration(
        color: RoostColors.waitingSurface,
        border: Border.all(color: RoostColors.waitingBorder),
        borderRadius: BorderRadius.circular(12),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Text('Trust ${host.label}?', style: text.titleSmall),
          const SizedBox(height: 6),
          Text(
            'First connection to this host. Trusting pins this key; '
            'if it ever changes, Roost refuses to connect.',
            style: text.bodySmall,
          ),
          const SizedBox(height: 8),
          Container(
            padding: const EdgeInsets.all(8),
            decoration: BoxDecoration(
              color: RoostColors.background,
              borderRadius: BorderRadius.circular(8),
            ),
            child: SelectableText(
              fingerprint,
              key: ValueKey('trust-fingerprint-${host.id}'),
              style: const TextStyle(fontFamily: monoFamily, fontSize: 12),
            ),
          ),
          const SizedBox(height: 8),
          Wrap(
            alignment: WrapAlignment.end,
            spacing: 8,
            runSpacing: 4,
            children: [
              TextButton(
                key: ValueKey('trust-cancel-${host.id}'),
                onPressed: onCancel,
                child: const Text('Cancel'),
              ),
              FilledButton(
                key: ValueKey('trust-accept-${host.id}'),
                onPressed: onTrust,
                child: const Text('Trust and connect'),
              ),
            ],
          ),
        ],
      ),
    );
  }
}

class _ProblemCard extends StatelessWidget {
  const _ProblemCard({
    super.key,
    required this.icon,
    required this.color,
    required this.message,
    required this.action,
    required this.onAction,
  });

  final IconData icon;
  final Color color;
  final String message;
  final String action;
  final VoidCallback onAction;

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.fromLTRB(12, 8, 8, 8),
      decoration: BoxDecoration(
        color: color == RoostColors.error ? RoostColors.errorSurface : null,
        border: Border.all(
          color: color == RoostColors.error
              ? RoostColors.errorBorder
              : RoostColors.border,
        ),
        borderRadius: BorderRadius.circular(12),
      ),
      child: Row(
        children: [
          Icon(icon, size: 18, color: color),
          const SizedBox(width: 8),
          Expanded(
            child: Text(message, style: Theme.of(context).textTheme.bodySmall),
          ),
          TextButton(onPressed: onAction, child: Text(action)),
        ],
      ),
    );
  }
}

class _StatusLine extends StatelessWidget {
  const _StatusLine({required this.icon, required this.text});

  final IconData icon;
  final String text;

  @override
  Widget build(BuildContext context) {
    return Row(
      children: [
        Icon(icon, size: 16, color: RoostColors.muted),
        const SizedBox(width: 8),
        Text(text, style: Theme.of(context).textTheme.bodySmall),
      ],
    );
  }
}

/// "2 waiting · 1 running" style counts for a connected host.
class HostCounts extends StatelessWidget {
  const HostCounts({super.key, required this.host});

  final HostView host;

  @override
  Widget build(BuildContext context) {
    if (!host.isConnected) return const SizedBox.shrink();
    return Wrap(
      spacing: 6,
      runSpacing: 4,
      children: [
        if (host.waitingTabs > 0)
          _Pill(
            text: '${host.waitingTabs} waiting',
            color: RoostColors.waiting,
            background: const Color(0xFF3A2A17),
          ),
        if (host.workingTabs > 0)
          _Pill(
            text: '${host.workingTabs} running',
            color: RoostColors.working,
            background: const Color(0xFF172338),
          ),
        if (host.waitingTabs == 0 && host.workingTabs == 0)
          const _Pill(
            text: 'idle',
            color: RoostColors.muted,
            background: RoostColors.surfaceHighest,
          ),
      ],
    );
  }
}

class _Pill extends StatelessWidget {
  const _Pill({
    required this.text,
    required this.color,
    required this.background,
  });

  final String text;
  final Color color;
  final Color background;

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
      decoration: BoxDecoration(
        color: background,
        borderRadius: BorderRadius.circular(10),
      ),
      child: Text(text, style: TextStyle(fontSize: 12, color: color)),
    );
  }
}
