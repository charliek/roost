import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../core/roost_core.dart';
import 'compact_layout.dart';
import 'expanded_layout.dart';
import 'medium_layout.dart';
import 'providers.dart';
import 'text_scale.dart';
import 'theme.dart';
import 'width_class.dart';

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
        ref.read(settingsProvider.notifier).flush();
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
    final textScale = ref.watch(
      settingsProvider.select((s) => s.desired.uiTextScale),
    );
    return MaterialApp(
      title: 'Roost',
      debugShowCheckedModeBanner: false,
      scaffoldMessengerKey: ref.watch(scaffoldMessengerKeyProvider),
      theme: roostDarkTheme(),
      // Above the navigator, so a sheet or dialog sees the same width class
      // and text scale as the layout under it.
      builder: (context, child) {
        final media = MediaQuery.of(context);
        return MediaQuery(
          data: media.copyWith(
            textScaler: ScaledTextScaler.over(media.textScaler, textScale),
          ),
          child: LayoutBuilder(
            builder: (context, constraints) => WidthClassScope(
              widthClass: widthClassFor(constraints.maxWidth),
              child: child!,
            ),
          ),
        );
      },
      home: const AppShell(),
    );
  }
}

class AppShell extends StatelessWidget {
  const AppShell({super.key});

  @override
  Widget build(BuildContext context) {
    return switch (WidthClassScope.of(context)) {
      WidthClass.compact => const CompactLayout(
        key: ValueKey('layout-compact'),
      ),
      WidthClass.medium => const MediumLayout(key: ValueKey('layout-medium')),
      WidthClass.expanded => const ExpandedLayout(
        key: ValueKey('layout-expanded'),
      ),
    };
  }
}
