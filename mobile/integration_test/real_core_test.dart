import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:roost_mobile/app/app.dart';
import 'package:roost_mobile/app/compact_layout.dart';
import 'package:roost_mobile/app/providers.dart';
import 'package:roost_mobile/core/frb_core.dart';
import 'package:roost_mobile/core/roost_core.dart';
import 'package:roost_mobile/hosts/host_widgets.dart';
import 'package:roost_mobile/hosts/workspace_list.dart';
import 'package:roost_mobile/src/rust/frb_generated.dart';
import 'package:roost_mobile/terminal/attached_tab.dart';
import 'package:roost_mobile/terminal/terminal_view.dart';

import 'support/session.dart';

/// `user@host:port` of a dev host reached over SSH (an Android emulator
/// against `tools/mobile/dev-host.sh`: `dev@10.0.2.2:2299`). Unset, the test
/// starts a `roost-session` of its own and adds it as a `unix:` host.
const _sshTarget = String.fromEnvironment('ROOST_E2E_TARGET');

/// The whole stack: the Flutter UI, the Rust core through
/// flutter_rust_bridge, and a real `roost-session`. On the Linux desktop
/// (`make test-integration-linux`) over the session's Unix socket; on an
/// Android emulator (`make test-integration-android`) over SSH, with the
/// host-key prompt.
void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  final ssh = _sshTarget.isNotEmpty;
  final name = ssh ? 'android' : 'e2e';
  IsolatedSession? session;
  var passed = false;

  setUpAll(() async {
    await RustLib.init();
    if (!ssh) session = await IsolatedSession.start();
  });

  tearDownAll(() async {
    final s = session;
    if (s == null) return;
    await s.stop(keepFiles: !passed);
    if (!passed) debugPrint('e2e: session files kept in ${s.root.path}');
  });

  testWidgets('add a host, open its tab, type every way, pause and resume', (
    tester,
  ) async {
    final dataDir = ssh
        ? (await Directory.systemTemp.createTemp('roost-e2e-')).path
        : '${session!.root.path}/app';
    final core = await FrbCore.start(
      dataDir: dataDir,
      deviceLabel: 'e2e-$name',
    );
    await tester.pumpWidget(
      ProviderScope(
        overrides: [coreProvider.overrideWithValue(core)],
        child: const RoostMobileApp(),
      ),
    );
    await tester.pumpAndSettle();
    final container = ProviderScope.containerOf(
      tester.element(find.byType(AppShell)),
    );

    // Add the host through the dialog.
    await tester.tap(find.byKey(const ValueKey('add-host')).first);
    await tester.pumpAndSettle();
    await tester.enterText(find.byKey(const ValueKey('add-host-label')), 'dev');
    await tester.enterText(
      find.byKey(const ValueKey('add-host-target')),
      ssh ? _sshTarget : 'unix:${session!.socket}',
    );
    await tester.tap(find.byKey(const ValueKey('add-host-submit')));

    if (ssh) {
      // First contact: the host key is shown and trusted by hand.
      await _until(
        tester,
        'the host asks to trust its key',
        // Exactly one: on a phone, adding a host also moves to its screen,
        // and the trust card shows on both until the switch is done.
        () => find.byType(TrustCard).evaluate().length == 1,
      );
      final card = tester.widget<TrustCard>(find.byType(TrustCard).first);
      expect(card.fingerprint, startsWith('SHA256:'));
      debugPrint('e2e: trusting ${card.fingerprint}');
      await tester.tap(
        find.descendant(
          of: find.byType(TrustCard),
          matching: find.text('Trust and connect'),
        ),
      );
    }

    await _until(
      tester,
      'the host connects',
      () => find.text('connected').evaluate().isNotEmpty,
    );
    // On a phone the host's projects are a screen of their own.
    if (find.byType(HostCard).evaluate().isNotEmpty) {
      await tester.tap(find.byType(HostCard).first);
    }
    await _until(
      tester,
      'the seed tab is listed',
      () => find.byType(TabTile).evaluate().isNotEmpty,
    );
    await tester.tap(find.byType(TabTile).first);
    final tab = await _attached(tester, 'the seed tab attaches');
    final selection = container.read(selectionProvider);

    // Hardware keys.
    await _focus(tester);
    final typed = Stopwatch()..start();
    await _type(tester, 'echo $name-ok');
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    await _until(
      tester,
      'the shell prints $name-ok',
      () => _hasRow(tab, '$name-ok'),
    );
    debugPrint(
      'e2e: typed command echoed back in ${typed.elapsedMilliseconds} ms',
    );

    // The soft keyboard, as an IME without the delta model reports it:
    // each full value after a commit, a deletion, then the newline.
    void ime(String text) => tester.testTextInput.updateEditingValue(
      TextEditingValue(
        text: text,
        selection: TextSelection.collapsed(offset: text.length),
      ),
    );
    ime('  echo $name-softt');
    await tester.pump();
    ime('  echo $name-soft');
    await tester.pump();
    ime('  echo $name-soft\n');
    await _until(
      tester,
      'the soft keyboard runs echo $name-soft',
      () => _hasRow(tab, '$name-soft'),
    );

    // The compose box: a paste over tab.write, then Enter.
    await tester.tap(find.byKey(const ValueKey('compose-toggle')));
    await tester.pump(const Duration(milliseconds: 300));
    await tester.enterText(
      find.byKey(const ValueKey('compose-field')),
      'echo $name-compose',
    );
    await tester.tap(find.byKey(const ValueKey('compose-send')));
    await _until(
      tester,
      'the compose box runs echo $name-compose',
      () => _hasRow(tab, '$name-compose'),
    );
    await tester.tap(find.byKey(const ValueKey('compose-toggle')));
    await tester.pump(const Duration(milliseconds: 300));

    // A quick reply, on a tab marked waiting: the command line is typed,
    // the Enter chip runs it.
    await core.op(selection.hostId!, 'tab.set_state', {
      'tab_id': '${selection.tabId}',
      'state': 'needs_input',
    });
    await _until(
      tester,
      'quick replies for a waiting tab',
      () => find.byKey(const ValueKey('quick-replies')).evaluate().isNotEmpty,
    );
    await _focus(tester);
    await _type(tester, 'echo $name-quick');
    await tester.tap(find.byKey(const ValueKey('quick-reply-enter')));
    await _until(
      tester,
      'the Enter quick reply runs echo $name-quick',
      () => _hasRow(tab, '$name-quick'),
    );
    await core.op(selection.hostId!, 'tab.set_state', {
      'tab_id': '${selection.tabId}',
      'state': 'none',
    });

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
      _hasRow(again, '$name-quick'),
      isTrue,
      reason: 'the snapshot brings the earlier output back',
    );
    await _focus(tester);
    await _type(tester, 'echo $name-resumed');
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    await _until(
      tester,
      'typing works after resume',
      () => _hasRow(again, '$name-resumed'),
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
  }, timeout: const Timeout(Duration(minutes: 4)));
}

const _budget = Duration(seconds: 45);

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

/// Taps the terminal: focus, and the soft keyboard's input connection.
Future<void> _focus(WidgetTester tester) async {
  await tester.tap(find.byKey(const ValueKey('terminal-canvas')));
  await tester.pump();
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
}
