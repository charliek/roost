import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/core/fake/fake_core.dart';
import 'package:roost_mobile/core/roost_core.dart';

import '../support/pump_app.dart';

void main() {
  group('compact (400 dp)', () {
    testWidgets('hosts, then projects and tabs, then the terminal', (
      tester,
    ) async {
      final core = await pumpApp(tester, width: 400);
      expect(byKey('layout-compact'), findsOneWidget);
      expect(byKey('host-card-devbox'), findsOneWidget);
      expect(byKey('host-card-mac-mini'), findsOneWidget);
      expect(byKey('host-card-nas'), findsOneWidget);
      expect(byKey('waiting-banner'), findsOneWidget);
      expect(
        find.text('Connection timed out (no route to host)'),
        findsOneWidget,
      );

      await tester.tap(byKey('host-card-devbox'));
      await tester.pumpAndSettle();
      expect(byKey('compact-projects'), findsOneWidget);
      expect(find.text('grok-build'), findsOneWidget);
      expect(
        find.text('1702/2417 passed'),
        findsOneWidget,
        reason: 'status_text shown',
      );
      expect(core.attaches, isEmpty, reason: 'browsing never takes a size');

      await tester.tap(byKey('tab-devbox-13'));
      await tester.pumpAndSettle();
      expect(byKey('terminal-canvas'), findsOneWidget);
      final attach = core.attaches.single;
      expect((attach.hostId, attach.tabId, attach.take), ('devbox', 13, true));
      expect(attach.geometry.cols, greaterThan(10));
      expect(find.text('roost · 3 of 3'), findsOneWidget);

      await tester.tap(find.byType(BackButton));
      await tester.pumpAndSettle();
      expect(byKey('compact-projects'), findsOneWidget);
      expect(core.sessions, isEmpty, reason: 'leaving the tab detaches it');
    });

    testWidgets('a fling moves to the next tab of the project', (tester) async {
      final core = await pumpApp(tester, width: 400);
      await tester.tap(byKey('host-card-devbox'));
      await tester.pumpAndSettle();
      await tester.tap(byKey('tab-devbox-12'));
      await tester.pumpAndSettle();
      await tester.fling(byKey('terminal-canvas'), const Offset(-300, 0), 1500);
      await tester.pumpAndSettle();
      expect(core.attaches.map((a) => a.tabId), [12, 13]);
      expect(find.text('roost · 3 of 3'), findsOneWidget);
    });

    testWidgets('the waiting banner opens the waiting tab', (tester) async {
      final core = await pumpApp(tester, width: 400);
      await tester.tap(byKey('waiting-banner'));
      await tester.pumpAndSettle();
      expect(core.attaches.single.tabId, 11);
      expect(byKey('quick-replies'), findsOneWidget);
    });
  });

  group('medium (700 dp)', () {
    testWidgets('a host rail, the host list, then the terminal with a drawer', (
      tester,
    ) async {
      final core = await pumpApp(tester, width: 700);
      expect(byKey('layout-medium'), findsOneWidget);
      expect(byKey('rail-host-devbox'), findsOneWidget);
      expect(byKey('rail-host-nas'), findsOneWidget);
      expect(byKey('medium-projects'), findsOneWidget);

      await tester.tap(byKey('tab-devbox-21'));
      await tester.pumpAndSettle();
      expect(core.attaches.single.tabId, 21);
      expect(byKey('tab-strip'), findsOneWidget);

      await tester.tap(byKey('open-projects-drawer'));
      await tester.pumpAndSettle();
      expect(byKey('projects-drawer'), findsOneWidget);
      await tester.tap(byKey('tab-devbox-22'));
      await tester.pumpAndSettle();
      expect(byKey('projects-drawer'), findsNothing);
      expect(core.attaches.map((a) => a.tabId), [21, 22]);
    });

    testWidgets('a host that needs trust shows its fingerprint', (
      tester,
    ) async {
      await pumpApp(tester, width: 700);
      await tester.tap(byKey('rail-host-mac-mini'));
      await tester.pumpAndSettle();
      expect(byKey('trust-card-mac-mini'), findsOneWidget);
      expect(find.text(FakeCore.macMiniFingerprint), findsOneWidget);
    });
  });

  group('expanded (1200 dp)', () {
    testWidgets('sidebar of every host, and no tab until one is picked', (
      tester,
    ) async {
      final core = await pumpApp(tester, width: 1200);
      expect(byKey('layout-expanded'), findsOneWidget);
      expect(byKey('host-sidebar'), findsOneWidget);
      for (final id in ['devbox', 'mac-mini', 'nas']) {
        expect(byKey('host-section-$id'), findsOneWidget);
      }
      expect(byKey('tab-devbox-11'), findsOneWidget);
      expect(byKey('notification-dot'), findsOneWidget);
      expect(byKey('pick-a-tab'), findsOneWidget);
      expect(byKey('tab-strip'), findsNothing);
      expect(core.attaches, isEmpty);
    });

    testWidgets('picking a tab attaches it; switching detaches the old one', (
      tester,
    ) async {
      final core = await pumpApp(tester, width: 1200);
      await tester.tap(byKey('tab-devbox-12'));
      await tester.pumpAndSettle();
      expect(byKey('tab-strip'), findsOneWidget);
      final first = lastSession(core);
      expect(core.attaches.single.take, isTrue);

      await tester.tap(byKey('strip-tab-13'));
      await tester.pumpAndSettle();
      expect(first.closed, isTrue);
      expect(core.attaches.map((a) => a.tabId), [12, 13]);
      expect(core.sessions.single.tabId, 13);
    });

    testWidgets('trusting a host connects it and lists its tabs', (
      tester,
    ) async {
      final core = await pumpApp(tester, width: 1200);
      expect(byKey('tab-mac-mini-31'), findsNothing);
      await tester.tap(byKey('trust-accept-mac-mini'));
      await tester.pumpAndSettle();
      expect(core.trusted, [('mac-mini', FakeCore.macMiniFingerprint)]);
      expect(byKey('trust-card-mac-mini'), findsNothing);
      expect(byKey('tab-mac-mini-31'), findsOneWidget);
    });

    testWidgets('cancelling trust leaves the host disconnected', (
      tester,
    ) async {
      await pumpApp(tester, width: 1200);
      await tester.tap(byKey('trust-cancel-mac-mini'));
      await tester.pumpAndSettle();
      expect(byKey('trust-card-mac-mini'), findsNothing);
      expect(byKey('host-connect-mac-mini'), findsOneWidget);
    });

    testWidgets('a failed host shows its reason and retries', (tester) async {
      await pumpApp(tester, width: 1200);
      expect(byKey('host-failed-nas'), findsOneWidget);
      await tester.tap(
        find.descendant(
          of: byKey('host-failed-nas'),
          matching: find.text('Retry'),
        ),
      );
      await tester.pumpAndSettle();
      expect(
        byKey('host-failed-nas'),
        findsOneWidget,
        reason: 'still unreachable',
      );
    });

    testWidgets(
      'add host: validates the target, adds, connects, asks for trust',
      (tester) async {
        await pumpApp(tester, width: 1200);
        await tester.tap(byKey('add-host'));
        await tester.pumpAndSettle();
        expect(find.text('Tailscale SSH: user@machine-name'), findsOneWidget);

        await tester.enterText(byKey('add-host-target'), 'charlie@pop os');
        await tester.tap(byKey('add-host-submit'));
        await tester.pumpAndSettle();
        expect(find.text('Use user@host or user@host:port'), findsOneWidget);

        await tester.enterText(byKey('add-host-label'), 'popos');
        await tester.enterText(byKey('add-host-target'), 'charlie@popos');
        await tester.tap(byKey('add-host-submit'));
        await tester.pumpAndSettle();
        expect(find.text('popos'), findsOneWidget);
        expect(find.text('Trust popos?'), findsOneWidget);
      },
    );

    testWidgets('remove host asks first', (tester) async {
      await pumpApp(tester, width: 1200);
      await tester.tap(byKey('host-menu-nas'));
      await tester.pumpAndSettle();
      await tester.tap(byKey('host-menu-remove'));
      await tester.pumpAndSettle();
      await tester.tap(byKey('confirm-action'));
      await tester.pumpAndSettle();
      expect(byKey('host-section-nas'), findsNothing);
    });
  });

  testWidgets('a layout switch keeps the selection', (tester) async {
    final core = await pumpApp(tester, width: 1200);
    await tester.tap(byKey('tab-devbox-13'));
    await tester.pumpAndSettle();
    await tester.binding.setSurfaceSize(const Size(400, 800));
    await tester.pumpAndSettle();
    expect(byKey('layout-compact'), findsOneWidget);
    expect(byKey('terminal-canvas'), findsOneWidget);
    expect(core.attaches.last.tabId, 13);
    expect(core.sessions.single.tabId, 13);
    expect(
      core.attaches.last.geometry.cols,
      lessThan(core.attaches.first.geometry.cols),
    );
  });

  testWidgets('host state labels follow the core', (tester) async {
    final core = await pumpApp(tester, width: 1200);
    core.setHostState(
      'devbox',
      const HostRetrying(inMs: 8000, reason: 'Connection reset'),
    );
    await tester.pumpAndSettle();
    expect(find.text('retrying in 8 s'), findsOneWidget);
    expect(find.text('Connection reset · retrying in 8 s'), findsOneWidget);
  });
}
