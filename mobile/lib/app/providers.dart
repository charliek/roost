import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../core/roost_core.dart';
import '../core/serial_queue.dart';
import 'width_class.dart';

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

/// The app's one `ScaffoldMessenger`, so an error from anywhere (a failed
/// settings save after a pinch, with no sheet open) still shows.
final scaffoldMessengerKeyProvider = Provider(
  (ref) => GlobalKey<ScaffoldMessengerState>(),
);

/// A continuous change (a pinch, a slider being dragged) is written this
/// long after its last step.
const settingsWriteDelay = Duration(milliseconds: 300);

/// [desired] is what the UI shows, edits included; [acknowledged] is what
/// the core last kept, and the only source of the effective brightness and
/// terminal theme. [revision] moves with every edit.
@immutable
class SettingsModel {
  const SettingsModel({
    required this.acknowledged,
    required this.desired,
    required this.revision,
  });

  final SettingsState acknowledged;
  final MobileSettings desired;
  final int revision;
}

extension TerminalFontSizes on MobileSettings {
  /// A phone-width layout has its own terminal text size; medium and
  /// expanded share the other.
  double terminalFontSizeFor(WidthClass widthClass) =>
      widthClass == WidthClass.compact
      ? terminalFontSizeCompact
      : terminalFontSizeWide;

  MobileSettings withTerminalFontSize(WidthClass widthClass, double size) =>
      widthClass == WidthClass.compact
      ? copyWith(terminalFontSizeCompact: size)
      : copyWith(terminalFontSizeWide: size);
}

/// Edits apply to the UI at once and are written to the core behind them.
/// Every call this makes to the core waits its turn in one queue, and a
/// write reads the newest edit when its turn comes, so no older value can
/// land after a newer one. At most one write waits in the queue: it will
/// carry every edit made meanwhile. A write's answer never undoes a newer
/// edit: it is adopted only when no edit came after it, and a failure
/// reverts only then.
///
/// What survives a relaunch is the last write the core acknowledged: a
/// process killed within [settingsWriteDelay] of a pinch step, before the
/// write, loses that step.
class SettingsNotifier extends Notifier<SettingsModel> {
  final _core = SerialQueue();
  Timer? _trailing;
  bool _writeQueued = false;

  /// The revision the core last kept: a write of it again is skipped.
  int _written = 0;

  @override
  SettingsModel build() {
    ref.onDispose(() {
      _trailing?.cancel();
    });
    final initial = ref.watch(coreProvider).settings();
    return SettingsModel(
      acknowledged: initial,
      desired: initial.settings,
      revision: 0,
    );
  }

  /// [continuous]: one step of a gesture or a drag, written once the steps
  /// stop (or on [flush]); otherwise written now.
  void edit(
    MobileSettings Function(MobileSettings) change, {
    bool continuous = false,
  }) {
    state = SettingsModel(
      acknowledged: state.acknowledged,
      desired: change(state.desired),
      revision: state.revision + 1,
    );
    if (continuous) {
      _trailing?.cancel();
      _trailing = Timer(settingsWriteDelay, _write);
    } else {
      _write();
    }
  }

  /// Writes a continuous change now: its gesture ended, or the app is going
  /// to the background.
  void flush() {
    if (_trailing != null) _write();
  }

  void _write() {
    _trailing?.cancel();
    _trailing = null;
    if (_writeQueued) return;
    _writeQueued = true;
    unawaited(_core.run(this, _writeLatest));
  }

  Future<void> _writeLatest() async {
    _writeQueued = false;
    if (!ref.mounted) return;
    final revision = state.revision;
    if (revision == _written) return;
    try {
      final kept = await ref.read(coreProvider).updateSettings(state.desired);
      if (!ref.mounted) return;
      _written = revision;
      state = SettingsModel(
        acknowledged: kept,
        desired: state.revision == revision ? kept.settings : state.desired,
        revision: state.revision,
      );
    } catch (e) {
      if (!ref.mounted) return;
      if (state.revision == revision) {
        state = SettingsModel(
          acknowledged: state.acknowledged,
          desired: state.acknowledged.settings,
          revision: state.revision,
        );
      }
      _report(e);
    }
  }

  Future<void> setSystemDark(bool dark) => _core.run(this, () async {
    try {
      final kept = await ref.read(coreProvider).setSystemDark(dark);
      if (!ref.mounted) return;
      state = SettingsModel(
        acknowledged: kept,
        desired: state.desired,
        revision: state.revision,
      );
    } catch (e) {
      if (ref.mounted) _report(e);
    }
  });

  void _report(Object error) {
    final messenger = ref.read(scaffoldMessengerKeyProvider).currentState;
    if (messenger == null) {
      debugPrint('roost: settings not saved: $error');
      return;
    }
    messenger.showSnackBar(
      SnackBar(
        key: const ValueKey('settings-error'),
        content: Text('Display settings not saved: $error'),
      ),
    );
  }
}

final settingsProvider = NotifierProvider<SettingsNotifier, SettingsModel>(
  SettingsNotifier.new,
);

/// Bumped each time the app comes back to the foreground, after
/// `appResumed`: the visible terminal re-attaches on it.
class ResumeEpoch extends Notifier<int> {
  @override
  int build() => 0;

  void bump() => state++;
}

final resumeEpochProvider = NotifierProvider<ResumeEpoch, int>(ResumeEpoch.new);
