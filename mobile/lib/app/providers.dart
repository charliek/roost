import 'package:flutter/foundation.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../core/roost_core.dart';

/// The core every widget talks to. `main` overrides it (FakeCore until the
/// Rust bridge lands); so do the tests.
final coreProvider = Provider<RoostCore>(
  (ref) => throw StateError('coreProvider must be overridden'),
);

final hostsProvider = StreamProvider<List<HostView>>(
  (ref) => ref.watch(coreProvider).hosts(),
);

final workspaceProvider = StreamProvider.family<WorkspaceView, String>(
  (ref, hostId) => ref.watch(coreProvider).workspace(hostId),
);

/// What the user is looking at. A tab is only ever selected by the user:
/// selecting one attaches it and takes its size.
@immutable
class Selection {
  const Selection({this.hostId, this.tabId});

  final String? hostId;
  final int? tabId;

  bool isTab(String hostId, int tabId) =>
      this.hostId == hostId && this.tabId == tabId;
}

class SelectionNotifier extends Notifier<Selection> {
  @override
  Selection build() => const Selection();

  void selectHost(String hostId) => state = Selection(hostId: hostId);

  void selectTab(String hostId, int tabId) =>
      state = Selection(hostId: hostId, tabId: tabId);

  void clearTab() => state = Selection(hostId: state.hostId);

  void clear() => state = const Selection();
}

final selectionProvider = NotifierProvider<SelectionNotifier, Selection>(
  SelectionNotifier.new,
);

class TerminalFontSize extends Notifier<double> {
  @override
  double build() => 13;

  void set(double size) => state = size;
}

final terminalFontSizeProvider = NotifierProvider<TerminalFontSize, double>(
  TerminalFontSize.new,
);

/// Bumped each time the app comes back to the foreground, after
/// `appResumed`: the visible terminal re-attaches on it.
class ResumeEpoch extends Notifier<int> {
  @override
  int build() => 0;

  void bump() => state++;
}

final resumeEpochProvider = NotifierProvider<ResumeEpoch, int>(ResumeEpoch.new);
