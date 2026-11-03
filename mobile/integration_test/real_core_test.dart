import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:roost_mobile/app/app.dart';
import 'package:roost_mobile/app/providers.dart';
import 'package:roost_mobile/core/frb_core.dart';
import 'package:roost_mobile/core/roost_core.dart';
import 'package:roost_mobile/hosts/workspace_list.dart';
import 'package:roost_mobile/src/rust/frb_generated.dart';
import 'package:roost_mobile/terminal/attached_tab.dart';
import 'package:roost_mobile/terminal/terminal_view.dart';

import 'support/session.dart';

/// The whole stack on the Linux desktop: the Flutter UI, the Rust core
/// through flutter_rust_bridge, and a real `roost-session` over its Unix
/// socket. Run with `make test-integration-linux`.
void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  late IsolatedSession session;
  var passed = false;

  setUpAll(() async {
    await RustLib.init();
    session = await IsolatedSession.start();
  });

  tearDownAll(() async {
    await session.stop(keepFiles: !passed);
    if (!passed) debugPrint('e2e: session files kept in ${session.root.path}');
  });

  testWidgets(
    'add a host, open its tab, type through the key path, pause and resume',
    (tester) async {
      final core = await FrbCore.start(
        dataDir: '${session.root.path}/app',
        deviceLabel: 'e2e-phone',
      );
      await tester.pumpWidget(
        ProviderScope(
          overrides: [coreProvider.overrideWithValue(core)],
          child: const RoostMobileApp(),
        ),
      );
      await tester.pumpAndSettle();
      expect(find.byKey(const ValueKey('layout-expanded')), findsOneWidget);

      // Add the session as a unix: host through the dialog.
      await tester.tap(find.byKey(const ValueKey('add-host')).first);
      await tester.pumpAndSettle();
      await tester.enterText(
        find.byKey(const ValueKey('add-host-label')),
        'dev',
      );
      await tester.enterText(
        find.byKey(const ValueKey('add-host-target')),
        'unix:${session.socket}',
      );
      await tester.tap(find.byKey(const ValueKey('add-host-submit')));

      // Connected, with the session's seed tab in the sidebar.
      await _until(tester, 'the host connects', () {
        return find.text('connected').evaluate().isNotEmpty &&
            find.byType(TabTile).evaluate().isNotEmpty;
      });

      await tester.tap(find.byType(TabTile).first);
      final tab = await _attached(tester, 'the seed tab attaches');

      // Focus the terminal and type through the hardware key path.
      await tester.tap(find.byKey(const ValueKey('terminal-canvas')));
      await tester.pump();
      final typed = Stopwatch()..start();
      await _type(tester, 'echo e2e-ok');
      await tester.sendKeyEvent(LogicalKeyboardKey.enter);
      await _until(
        tester,
        'the shell prints e2e-ok',
        () => _hasRow(tab, 'e2e-ok'),
      );
      debugPrint(
        'e2e: typed command echoed back in ${typed.elapsedMilliseconds} ms',
      );
      expect(
        _rows(tab).any((r) => r.contains('echo e2e-ok')),
        isTrue,
        reason: 'the command line itself is on screen',
      );

      await _measureFramePulls(tab);

      // Background: the core gives the size back and detaches the tab.
      final binding = tester.binding;
      // A paused app draws no frames, and the binding's own teardown waits
      // for one: whatever fails below, end the test in the foreground.
      addTearDown(
        () => binding.handleAppLifecycleStateChanged(AppLifecycleState.resumed),
      );
      for (final s in [
        AppLifecycleState.inactive,
        AppLifecycleState.hidden,
        AppLifecycleState.paused,
      ]) {
        binding.handleAppLifecycleStateChanged(s);
      }
      await _until(
        tester,
        'the tab detaches on pause',
        () => tab.state == AttachState.ended,
        pump: false,
      );
      await _until(
        tester,
        'no attach stream outlives the pause',
        () => core.leakCounts().attachPumps == 0,
        pump: false,
      );

      // Foreground: the pane re-attaches, taking the size again.
      for (final s in [
        AppLifecycleState.hidden,
        AppLifecycleState.inactive,
        AppLifecycleState.resumed,
      ]) {
        binding.handleAppLifecycleStateChanged(s);
      }
      final again = await _attached(
        tester,
        'the tab re-attaches on resume',
        not: tab,
      );
      expect(
        _hasRow(again, 'e2e-ok'),
        isTrue,
        reason: 'the snapshot brings the earlier output back',
      );
      await tester.tap(find.byKey(const ValueKey('terminal-canvas')));
      await tester.pump();
      await _type(tester, 'echo resumed-ok');
      await tester.sendKeyEvent(LogicalKeyboardKey.enter);
      await _until(
        tester,
        'typing works after resume',
        () => _hasRow(again, 'resumed-ok'),
      );

      // Leaving the app closes the tab; nothing is left attached.
      await tester.pumpWidget(const SizedBox());
      await _until(tester, 'teardown releases the terminal', () {
        final counts = core.leakCounts();
        return counts.attachments == 0 &&
            counts.terminals == 0 &&
            counts.attachPumps == 0;
      });
      passed = true;
    },
    timeout: const Timeout(Duration(minutes: 3)),
  );
}

const _budget = Duration(seconds: 30);

Future<void> _until(
  WidgetTester tester,
  String what,
  bool Function() done, {
  bool pump = true,
}) async {
  final deadline = DateTime.now().add(_budget);
  while (!done()) {
    if (DateTime.now().isAfter(deadline)) {
      fail('timed out waiting for: $what');
    }
    await Future<void>.delayed(const Duration(milliseconds: 50));
    // A paused app schedules no frames, so a pump would wait forever.
    if (pump) await tester.pump();
  }
}

/// The pane's attached tab once it is live and has drawn a frame.
Future<AttachedTab> _attached(
  WidgetTester tester,
  String what, {
  AttachedTab? not,
}) async {
  AttachedTab? current() {
    final views = find.byType(TerminalView).evaluate();
    if (views.isEmpty) return null;
    return (views.single.widget as TerminalView).tab;
  }

  await _until(tester, what, () {
    final tab = current();
    return tab != null &&
        tab != not &&
        tab.state == AttachState.live &&
        tab.model.hasFrame;
  });
  return current()!;
}

List<String> _rows(AttachedTab tab) => [
  for (var r = 0; r < tab.model.rows; r++) tab.model.rowText(r).trim(),
];

bool _hasRow(AttachedTab tab, String text) => _rows(tab).contains(text);

/// One hardware key event per character, as a physical keyboard sends them.
Future<void> _type(WidgetTester tester, String text) async {
  for (final ch in text.split('')) {
    final key =
        LogicalKeyboardKey.findKeyByKeyId(ch.codeUnitAt(0)) ??
        LogicalKeyboardKey(ch.codeUnitAt(0));
    await tester.sendKeyDownEvent(key, character: ch);
    await tester.sendKeyUpEvent(key);
  }
}

/// How long a frame pull takes across the bridge (thread pool, owner-thread
/// round trip). Pulls here take frames the view would have, so this runs
/// just before the pause, whose re-attach redraws from a snapshot.
Future<void> _measureFramePulls(AttachedTab tab) async {
  final times = <int>[];
  for (var i = 0; i < 30; i++) {
    final watch = Stopwatch()..start();
    await tab.session.takeFrame();
    times.add(watch.elapsedMicroseconds);
  }
  times.sort();
  debugPrint(
    'e2e: takeFrame over the bridge: min ${times.first} us, '
    'median ${times[times.length ~/ 2]} us, max ${times.last} us',
  );
  if (Platform.environment['ROOST_E2E_STRICT_TIMING'] == '1') {
    expect(times[times.length ~/ 2], lessThan(5000));
  }
}
