import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

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

  @override
  void initState() {
    super.initState();
    _lifecycle = AppLifecycleListener(onStateChange: _onLifecycle);
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
        unawaited(core.appPaused());
      case AppLifecycleState.resumed:
        if (!_backgrounded) return;
        _backgrounded = false;
        unawaited(_resume());
      case AppLifecycleState.inactive || AppLifecycleState.detached:
        break;
    }
  }

  Future<void> _resume() async {
    await ref.read(coreProvider).appResumed();
    if (mounted) ref.read(resumeEpochProvider.notifier).bump();
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
