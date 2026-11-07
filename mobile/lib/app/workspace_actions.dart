import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../core/roost_core.dart';
import 'dialogs.dart';
import 'providers.dart';

/// Runs a core call and reports a failure in a snack bar. Null on failure.
Future<T?> runCoreAction<T>(
  BuildContext context,
  Future<T> Function() action,
) async {
  final messenger = ScaffoldMessenger.maybeOf(context);
  try {
    return await action();
  } catch (e) {
    messenger?.showSnackBar(SnackBar(content: Text('$e')));
    return null;
  }
}

Future<void> addHostFlow(BuildContext context, WidgetRef ref) async {
  final host = await showDialog<NewHost>(
    context: context,
    builder: (_) => const AddHostDialog(),
  );
  if (host == null || !context.mounted) return;
  final core = ref.read(coreProvider);
  final id = await runCoreAction(
    context,
    () => core.addHost(label: host.label, target: host.target),
  );
  if (id == null || !context.mounted) return;
  ref.read(selectionProvider.notifier).selectHost(id);
  await runCoreAction(context, () => core.connect(id));
}

Future<void> removeHostFlow(
  BuildContext context,
  WidgetRef ref,
  HostView host,
) async {
  final ok = await confirm(
    context,
    title: 'Remove ${host.label}?',
    message:
        'Forgets ${host.target} and its trusted key. '
        'Nothing on the host changes.',
    action: 'Remove',
  );
  if (!ok || !context.mounted) return;
  if (ref.read(selectionProvider).hostId == host.id) {
    ref.read(selectionProvider.notifier).clear();
  }
  await runCoreAction(
    context,
    () => ref.read(coreProvider).removeHost(host.id),
  );
}

/// Opens a tab without activating it on the host (it must not steal the
/// desktop's focus), then shows it here.
Future<void> newTabFlow(
  BuildContext context,
  WidgetRef ref,
  String hostId,
  int projectId,
) async {
  final result = await runCoreAction(
    context,
    () => ref.read(coreProvider).op(hostId, 'tab.open', {
      'project_id': '$projectId',
      'activate': false,
    }),
  );
  final id = _resultId(result, 'tab');
  if (id != null) ref.read(selectionProvider.notifier).selectTab(hostId, id);
}

Future<void> closeTabFlow(
  BuildContext context,
  WidgetRef ref,
  String hostId,
  TabView tab,
) async {
  final ok = await confirm(
    context,
    title: 'Close ${tab.title}?',
    message: 'Ends the shell and anything running in it.',
    action: 'Close tab',
  );
  if (!ok || !context.mounted) return;
  if (ref.read(selectionProvider).isTab(hostId, tab.id)) {
    ref.read(selectionProvider.notifier).clearTab();
  }
  await runCoreAction(
    context,
    () =>
        ref.read(coreProvider).op(hostId, 'tab.close', {'tab_id': '${tab.id}'}),
  );
}

Future<void> renameTabFlow(
  BuildContext context,
  WidgetRef ref,
  String hostId,
  TabView tab,
) async {
  final title = await promptText(
    context,
    title: 'Rename tab',
    label: 'Title',
    initial: tab.title,
    action: 'Rename',
  );
  if (title == null || title.isEmpty || !context.mounted) return;
  await runCoreAction(
    context,
    () => ref.read(coreProvider).op(hostId, 'tab.set_title', {
      'tab_id': '${tab.id}',
      'title': title,
    }),
  );
}

Future<void> newProjectFlow(
  BuildContext context,
  WidgetRef ref,
  String hostId,
) async {
  final name = await promptText(
    context,
    title: 'New project',
    label: 'Name',
    action: 'Create',
  );
  if (name == null || !context.mounted) return;
  await runCoreAction(
    context,
    () => ref.read(coreProvider).op(hostId, 'project.create', {'name': name}),
  );
}

Future<void> renameProjectFlow(
  BuildContext context,
  WidgetRef ref,
  String hostId,
  ProjectView project,
) async {
  final name = await promptText(
    context,
    title: 'Rename project',
    label: 'Name',
    initial: project.name,
    action: 'Rename',
  );
  if (name == null || name.isEmpty || !context.mounted) return;
  await runCoreAction(
    context,
    () => ref.read(coreProvider).op(hostId, 'project.rename', {
      'project_id': '${project.id}',
      'name': name,
    }),
  );
}

Future<void> showTabActions(
  BuildContext context,
  WidgetRef ref,
  String hostId,
  TabView tab,
) async {
  final action = await showModalBottomSheet<String>(
    context: context,
    showDragHandle: true,
    builder: (context) => SafeArea(
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          ListTile(
            key: const ValueKey('tab-action-rename'),
            leading: const Icon(Icons.edit_outlined),
            title: const Text('Rename tab'),
            onTap: () => Navigator.of(context).pop('rename'),
          ),
          ListTile(
            key: const ValueKey('tab-action-close'),
            leading: const Icon(Icons.close),
            title: const Text('Close tab'),
            onTap: () => Navigator.of(context).pop('close'),
          ),
        ],
      ),
    ),
  );
  if (!context.mounted) return;
  switch (action) {
    case 'rename':
      await renameTabFlow(context, ref, hostId, tab);
    case 'close':
      await closeTabFlow(context, ref, hostId, tab);
  }
}

/// The id of the `tab` or `project` a create op returned (ids are
/// string-wrapped on the wire).
int? _resultId(Object? result, String kind) {
  if (result is! Map) return null;
  final obj = result[kind];
  if (obj is! Map) return null;
  final id = obj['id'];
  return switch (id) {
    final int v => v,
    final String v => int.tryParse(v),
    _ => null,
  };
}
