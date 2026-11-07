import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../core/roost_core.dart';
import 'compact_layout.dart';
import 'expanded_layout.dart';
import 'medium_layout.dart';
import 'providers.dart';
import 'theme.dart';

enum WidthClass { compact, medium, expanded }

/// Material 3 window size classes.
WidthClass widthClassFor(double width) => width >= 840
    ? WidthClass.expanded
    : width >= 600
    ? WidthClass.medium
    : WidthClass.compact;

class RoostMobileApp extends ConsumerStatefulWidget {
  const RoostMobileApp({super.key});

  @override
  ConsumerState<RoostMobileApp> createState() => _RoostMobileAppState();
}

class _RoostMobileAppState extends ConsumerState<RoostMobileApp> {
  late final AppLifecycleListener _lifecycle;
  bool _backgrounded = false;

  /// Every pause and resume, one after another: a resume never overtakes
  /// the pause before it.
  Future<void> _transitions = Future.value();
  bool _connectedSavedHosts = false;

  @override
  void initState() {
    super.initState();
    _lifecycle = AppLifecycleListener(onStateChange: _onLifecycle);
    ref.listenManual(hostsProvider, (_, next) {
      final hosts = next.value;
      if (hosts != null) _connectSavedHosts(hosts);
    }, fireImmediately: true);
  }

  /// The core dials nothing on its own: at launch, connect every saved
  /// host that is not already connecting.
  void _connectSavedHosts(List<HostView> hosts) {
    if (_connectedSavedHosts) return;
    _connectedSavedHosts = true;
    final core = ref.read(coreProvider);
    for (final host in hosts) {
      if (host.state is HostDisconnected) {
        unawaited(core.connect(host.id).then((_) {}, onError: (Object _) {}));
      }
    }
  }

  @override
  void dispose() {
    _lifecycle.dispose();
    super.dispose();
  }

  /// Background: the core gives every tab's size back and detaches. Android
  /// passes through hidden on the way to paused, so only the first counts.
  void _onLifecycle(AppLifecycleState state) {
    final core = ref.read(coreProvider);
    switch (state) {
      case AppLifecycleState.hidden || AppLifecycleState.paused:
        if (_backgrounded) return;
        _backgrounded = true;
        _enqueue(core.appPaused);
      case AppLifecycleState.resumed:
        if (!_backgrounded) return;
        _backgrounded = false;
        _enqueue(() async {
          await core.appResumed();
          // Fenced: the app may have gone back to the background meanwhile.
          if (mounted && !_backgrounded) {
            ref.read(resumeEpochProvider.notifier).bump();
          }
        });
      case AppLifecycleState.inactive || AppLifecycleState.detached:
        break;
    }
  }

  void _enqueue(Future<void> Function() transition) {
    _transitions = _transitions
        .then((_) => transition())
        .catchError((Object e) => debugPrint('roost: lifecycle: $e'));
  }

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Roost',
      debugShowCheckedModeBanner: false,
      theme: roostDarkTheme(),
      home: const AppShell(),
    );
  }
}

class AppShell extends StatelessWidget {
  const AppShell({super.key});

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) => switch (widthClassFor(
        constraints.maxWidth,
      )) {
        WidthClass.compact => const CompactLayout(
          key: ValueKey('layout-compact'),
        ),
        WidthClass.medium => const MediumLayout(key: ValueKey('layout-medium')),
        WidthClass.expanded => const ExpandedLayout(
          key: ValueKey('layout-expanded'),
        ),
      },
    );
  }
}
