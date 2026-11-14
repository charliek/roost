import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/app/providers.dart';
import 'package:roost_mobile/core/fake/fake_core.dart';
import 'package:roost_mobile/core/roost_core.dart';
import 'package:roost_mobile/terminal/terminal_pane.dart';

import '../support/pump_app.dart';

Future<FakeCore> _openTab(
  WidgetTester tester,
  int tabId, {
  double width = 1200,
  FakeCore? core,
}) async {
  core = await pumpApp(tester, width: width, core: core);
  if (width < 600) {
    await tester.tap(byKey('host-card-devbox'));
    await tester.pumpAndSettle();
  }
  await tester.tap(byKey('tab-devbox-$tabId'));
  await tester.pumpAndSettle();
  return core;
}

Future<void> _focusTerminal(WidgetTester tester) async {
  await tester.tap(byKey('terminal-canvas'));
  await tester.pumpAndSettle();
}

void main() {
  group('quick replies', () {
    testWidgets('appear for a waiting tab and send raw key bytes', (
      tester,
    ) async {
      final core = await _openTab(tester, 11);
      expect(byKey('quick-replies'), findsOneWidget);
      expect(
        find.descendant(
          of: byKey('quick-replies'),
          matching: find.text('Permission: edit host_conn.rs'),
        ),
        findsOneWidget,
        reason: 'a tablet has room for why',
      );
      await tester.tap(byKey('quick-reply-1'));
      await tester.tap(byKey('quick-reply-esc'));
      await tester.tap(byKey('quick-reply-up'));
      await tester.tap(byKey('quick-reply-enter'));
      await tester.pumpAndSettle();
      expect(core.sends.map((s) => (s.hostId, s.tabId, s.text, s.paste)), [
        ('devbox', 11, '1', false),
        ('devbox', 11, '\x1b', false),
        ('devbox', 11, '\x1b[A', false),
        ('devbox', 11, '\r', false),
      ]);
      expect(lastSession(core).keys, isEmpty, reason: 'never the data plane');
    });

    testWidgets('are absent for a tab that is not waiting', (tester) async {
      await _openTab(tester, 13);
      expect(byKey('quick-replies'), findsNothing);
    });

    testWidgets('opening a tab clears its notification', (tester) async {
      final core = await _openTab(tester, 11);
      expect(core.tabView('devbox', 11).hasNotification, isFalse);
      expect(byKey('quick-replies'), findsOneWidget, reason: 'still waiting');
    });
  });

  group('compose', () {
    testWidgets('sends a paste and then Enter, and clears', (tester) async {
      final core = await _openTab(tester, 13);
      await tester.tap(byKey('compose-toggle'));
      await tester.pumpAndSettle();
      await tester.enterText(
        byKey('compose-field'),
        'explain this\nin two lines',
      );
      await tester.tap(byKey('compose-send'));
      await tester.pumpAndSettle();
      expect(core.sends.map((s) => (s.tabId, s.text, s.paste)), [
        (13, 'explain this\nin two lines', true),
        (13, '\r', false),
      ]);
      final field = tester.widget<TextField>(byKey('compose-field'));
      expect(field.controller!.text, isEmpty);
    });

    testWidgets('without Enter it only pastes', (tester) async {
      final core = await _openTab(tester, 13);
      await tester.tap(byKey('compose-toggle'));
      await tester.pumpAndSettle();
      await tester.tap(byKey('compose-enter'));
      await tester.enterText(byKey('compose-field'), 'draft');
      await tester.tap(byKey('compose-send'));
      await tester.pumpAndSettle();
      expect(core.sends.map((s) => (s.text, s.paste)), [('draft', true)]);
    });
  });

  group('keyboard input', () {
    testWidgets(
      'hardware keys go to the session as keys and hide the extra keys',
      (tester) async {
        final core = await _openTab(tester, 13);
        await _focusTerminal(tester);
        expect(byKey('extra-keys'), findsOneWidget);
        await tester.sendKeyEvent(LogicalKeyboardKey.keyL);
        await tester.sendKeyEvent(LogicalKeyboardKey.enter);
        await tester.pumpAndSettle();
        expect(lastSession(core).keys, [
          const KeyInput('l', text: 'l'),
          const KeyInput('Enter'),
        ]);
        expect(byKey('extra-keys'), findsNothing);
      },
    );

    testWidgets('soft keyboard text goes to the session as text', (
      tester,
    ) async {
      final core = await _openTab(tester, 13, width: 400);
      await _focusTerminal(tester);
      tester.testTextInput.updateEditingValue(
        const TextEditingValue(
          text: '  ls',
          selection: TextSelection.collapsed(offset: 4),
        ),
      );
      tester.testTextInput.updateEditingValue(
        const TextEditingValue(
          text: '  l',
          selection: TextSelection.collapsed(offset: 3),
        ),
      );
      await tester.pumpAndSettle();
      final session = lastSession(core);
      expect(session.texts, ['ls']);
      expect(session.keys, [const KeyInput('Backspace')]);
      expect(core.shellOf('devbox', 13).line, 'l');
    });

    testWidgets('sticky Ctrl applies to the next typed character only', (
      tester,
    ) async {
      final core = await _openTab(tester, 13, width: 400);
      await _focusTerminal(tester);
      await tester.tap(byKey('extra-key-ctrl'));
      await tester.pump();
      tester.testTextInput.updateEditingValue(
        const TextEditingValue(
          text: '  c',
          selection: TextSelection.collapsed(offset: 3),
        ),
      );
      await tester.pump();
      tester.testTextInput.updateEditingValue(
        const TextEditingValue(
          text: '  cc',
          selection: TextSelection.collapsed(offset: 4),
        ),
      );
      await tester.pump();
      await tester.tap(byKey('extra-key-alt'));
      await tester.tap(byKey('extra-key-left'));
      await tester.pumpAndSettle();
      final session = lastSession(core);
      expect(session.keys, [
        const KeyInput('c', text: 'c', ctrl: true),
        const KeyInput('ArrowLeft', alt: true),
      ]);
      expect(session.texts, ['c']);
    });
  });

  group('copy text', () {
    Future<List<String>> mockClipboard(WidgetTester tester) async {
      final copied = <String>[];
      tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
        SystemChannels.platform,
        (call) async {
          if (call.method == 'Clipboard.setData') {
            copied.add((call.arguments as Map)['text'] as String);
          }
          return null;
        },
      );
      addTearDown(
        () => tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
          SystemChannels.platform,
          null,
        ),
      );
      return copied;
    }

    Future<void> echo(WidgetTester tester, String text) async {
      await _focusTerminal(tester);
      tester.testTextInput.updateEditingValue(
        TextEditingValue(
          text: '  echo $text',
          selection: TextSelection.collapsed(offset: 7 + text.length),
        ),
      );
      await tester.pump();
      await tester.sendKeyEvent(LogicalKeyboardKey.enter);
      await tester.pumpAndSettle();
    }

    testWidgets('a long press lists the links on screen to copy', (
      tester,
    ) async {
      final copied = await mockClipboard(tester);
      await _openTab(tester, 13, width: 400);
      await echo(tester, 'https://example.com/login?code=1');
      await tester.longPress(byKey('terminal-canvas'));
      await tester.pumpAndSettle();
      expect(byKey('copy-sheet'), findsOneWidget);
      await tester.tap(byKey('copy-url-0'));
      await tester.pumpAndSettle();
      expect(copied, ['https://example.com/login?code=1']);
      expect(byKey('copy-sheet'), findsNothing);
      expect(find.text('Link copied'), findsOneWidget);
    });

    testWidgets('a program in the tab copies to the clipboard (OSC 52)', (
      tester,
    ) async {
      final copied = await mockClipboard(tester);
      await _openTab(tester, 13, width: 400);
      await _focusTerminal(tester);
      tester.testTextInput.updateEditingValue(
        const TextEditingValue(
          text: '  copy https://claude.com/x?a=1',
          selection: TextSelection.collapsed(offset: 31),
        ),
      );
      await tester.pump();
      await tester.sendKeyEvent(LogicalKeyboardKey.enter);
      await tester.pumpAndSettle();
      expect(copied, ['https://claude.com/x?a=1']);
      expect(find.text('Copied from the terminal'), findsOneWidget);
    });

    testWidgets('a burst of copies shows one notice that leaves the bar free', (
      tester,
    ) async {
      final copied = await mockClipboard(tester);
      final core = await _openTab(tester, 13, width: 400);
      await _focusTerminal(tester);
      final shell = core.shellOf('devbox', 13);
      for (var i = 0; i < 10; i++) {
        shell.onClipboardWrite!('copy $i');
        await tester.pump(const Duration(milliseconds: 100));
      }
      await tester.pump();
      expect(copied, hasLength(10));
      expect(find.text('Copied from the terminal'), findsOneWidget);
      await tester.tap(byKey('extra-key-up'));
      await tester.pump();
      expect(lastSession(core).keys, [const KeyInput('ArrowUp')]);
      await tester.pump(noticeDuration);
      expect(find.text('Copied from the terminal'), findsNothing);
    });

    testWidgets('the tab menu opens it too, and copies the whole screen', (
      tester,
    ) async {
      final copied = await mockClipboard(tester);
      await _openTab(tester, 13);
      await echo(tester, 'hello there');
      await tester.tap(find.byTooltip('Tab actions'));
      await tester.pumpAndSettle();
      await tester.tap(byKey('menu-copy-text'));
      await tester.pumpAndSettle();
      await tester.tap(byKey('copy-all'));
      await tester.pumpAndSettle();
      expect(copied, hasLength(1));
      expect(copied.single, contains('hello there'));
    });
  });

  group('size ownership', () {
    testWidgets('a foreign owner is named, and Take size takes it back', (
      tester,
    ) async {
      final core = await _openTab(tester, 12);
      expect(byKey('size-owner-banner'), findsNothing);
      core.takeSizeFrom(
        'devbox',
        12,
        label: 'charlie-laptop',
        cols: 132,
        rows: 40,
      );
      await tester.pumpAndSettle();
      expect(find.text('sized by charlie-laptop · 132×40'), findsOneWidget);
      await tester.tap(byKey('take-size'));
      await tester.pumpAndSettle();
      expect(lastSession(core).takeSizeCount, 1);
      expect(byKey('size-owner-banner'), findsNothing);
    });

    testWidgets('a settled scroll takes the size back from another client', (
      tester,
    ) async {
      final core = await _openTab(tester, 12);
      final session = lastSession(core);
      core.takeSizeFrom(
        'devbox',
        12,
        label: 'charlie-laptop',
        cols: 132,
        rows: 40,
      );
      await tester.pumpAndSettle();
      await tester.drag(byKey('terminal-canvas'), const Offset(0, 120));
      await tester.pump();
      expect(session.scrolls, isNotEmpty);
      expect(session.scrolls.first, greaterThan(0), reason: 'down is history');
      await tester.pump(const Duration(milliseconds: 300));
      expect(session.takeSizeCount, 0, reason: 'still scrolling');
      await tester.drag(byKey('terminal-canvas'), const Offset(0, 60));
      await tester.pump(const Duration(milliseconds: 300));
      expect(session.takeSizeCount, 0, reason: 'the settle restarted');
      await tester.pump(const Duration(milliseconds: 300));
      expect(session.takeSizeCount, 1);
      expect(byKey('size-owner-banner'), findsNothing);
    });

    testWidgets('a settled scroll takes nothing when the size is ours', (
      tester,
    ) async {
      final core = await _openTab(tester, 13);
      final session = lastSession(core);
      await tester.drag(byKey('terminal-canvas'), const Offset(0, 120));
      await tester.pump(const Duration(seconds: 1));
      expect(session.scrolls, isNotEmpty);
      expect(session.takeSizeCount, 0);
    });

    testWidgets('a layout regrid never takes the size; a zoom does', (
      tester,
    ) async {
      final core = await _openTab(tester, 12);
      final session = lastSession(core);
      core.takeSizeFrom(
        'devbox',
        12,
        label: 'charlie-laptop',
        cols: 132,
        rows: 40,
      );
      await tester.pumpAndSettle();
      await tester.pump(const Duration(milliseconds: 200));
      expect(session.resizes, isNotEmpty, reason: 'the banner moved the grid');
      expect(
        session.resizes.map((r) => r.$2),
        everyElement(isFalse),
        reason: 'the banner is not the user resizing',
      );
      expect(find.text('sized by charlie-laptop · 132×40'), findsOneWidget);

      ProviderScope.containerOf(tester.element(find.byType(TerminalPane)))
          .read(terminalFontSizeProvider.notifier)
          .set(16);
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 200));
      expect(session.resizes.last.$2, isTrue, reason: 'a zoom is deliberate');
    });

    testWidgets('a burst of size changes settles into one resize', (
      tester,
    ) async {
      final core = await _openTab(tester, 13);
      final session = lastSession(core);
      await tester.binding.setSurfaceSize(const Size(1000, 1400));
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 100));
      expect(session.resizes, isEmpty, reason: 'still settling');
      await tester.binding.setSurfaceSize(const Size(900, 1400));
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 100));
      expect(session.resizes, isEmpty, reason: 'the settle restarted');
      await tester.pump(const Duration(milliseconds: 100));
      expect(session.resizes, hasLength(1));
      final (geometry, _) = session.resizes.single;
      expect(geometry.cols, lessThan(core.attaches.single.geometry.cols));
      await tester.binding.setSurfaceSize(const Size(1000, 1400));
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 200));
      expect(session.resizes.last.$1.cols, greaterThan(geometry.cols));
    });
  });

  void background(TestWidgetsFlutterBinding binding) {
    for (final s in [
      AppLifecycleState.inactive,
      AppLifecycleState.hidden,
      AppLifecycleState.paused,
    ]) {
      binding.handleAppLifecycleStateChanged(s);
    }
  }

  void foreground(TestWidgetsFlutterBinding binding) {
    for (final s in [
      AppLifecycleState.hidden,
      AppLifecycleState.inactive,
      AppLifecycleState.resumed,
    ]) {
      binding.handleAppLifecycleStateChanged(s);
    }
  }

  group('lifecycle', () {
    testWidgets('a resume waits for the pause before it', (tester) async {
      final core = await _openTab(
        tester,
        13,
        core: FakeCore(
          latency: const Duration(milliseconds: 10),
          pauseLatency: const Duration(milliseconds: 300),
        ),
      );
      background(tester.binding);
      foreground(tester.binding);
      await tester.pump(const Duration(milliseconds: 100));
      expect(core.attaches, hasLength(1), reason: 'the pause is still going');
      await tester.pump(const Duration(milliseconds: 300));
      await tester.pumpAndSettle();
      expect(core.attaches, hasLength(2));
      expect(
        core.sessions.single.closed,
        isFalse,
        reason: 'the late pause must not end the fresh attach',
      );
    });

    testWidgets('a resume overtaken by another pause does not re-attach', (
      tester,
    ) async {
      final core = await _openTab(
        tester,
        13,
        core: FakeCore(
          latency: const Duration(milliseconds: 10),
          pauseLatency: const Duration(milliseconds: 100),
        ),
      );
      background(tester.binding);
      foreground(tester.binding);
      background(tester.binding);
      await tester.pump(const Duration(seconds: 1));
      await tester.pumpAndSettle();
      expect((core.pausedCount, core.resumedCount), (2, 1));
      expect(core.attaches, hasLength(1));
      expect(core.sessions, isEmpty);
    });

    testWidgets(
      'background releases and detaches; resume re-attaches taking the size',
      (tester) async {
        final core = await _openTab(tester, 13);
        final binding = tester.binding;
        for (final s in [
          AppLifecycleState.inactive,
          AppLifecycleState.hidden,
          AppLifecycleState.paused,
        ]) {
          binding.handleAppLifecycleStateChanged(s);
        }
        await tester.pumpAndSettle();
        expect(core.pausedCount, 1);
        expect(core.sessions, isEmpty);

        for (final s in [
          AppLifecycleState.hidden,
          AppLifecycleState.inactive,
          AppLifecycleState.resumed,
        ]) {
          binding.handleAppLifecycleStateChanged(s);
        }
        await tester.pumpAndSettle();
        expect(core.resumedCount, 1);
        expect(core.attaches.map((a) => (a.tabId, a.take)), [
          (13, true),
          (13, true),
        ]);
        expect(core.sessions.single.tabId, 13);
      },
    );
  });

  group('workspace actions', () {
    testWidgets(
      'new tab opens without activating on the host, then shows it here',
      (tester) async {
        final core = await pumpApp(tester, width: 1200);
        await tester.tap(byKey('new-tab-devbox-2'));
        await tester.pumpAndSettle();
        final op = core.ops.single;
        expect((op.hostId, op.op), ('devbox', 'tab.open'));
        expect(op.params, {'project_id': '2', 'activate': false});
        expect(core.attaches.single.tabId, isNot(anyOf(21, 22)));
      },
    );

    testWidgets('rename and close a tab from its long-press menu', (
      tester,
    ) async {
      final core = await pumpApp(tester, width: 1200);
      await tester.longPress(byKey('tab-devbox-22'));
      await tester.pumpAndSettle();
      await tester.tap(byKey('tab-action-rename'));
      await tester.pumpAndSettle();
      await tester.enterText(byKey('prompt-field'), 'logs');
      await tester.tap(byKey('prompt-submit'));
      await tester.pumpAndSettle();
      expect(find.text('logs'), findsOneWidget);

      await tester.longPress(byKey('tab-devbox-22'));
      await tester.pumpAndSettle();
      await tester.tap(byKey('tab-action-close'));
      await tester.pumpAndSettle();
      await tester.tap(byKey('confirm-action'));
      await tester.pumpAndSettle();
      expect(byKey('tab-devbox-22'), findsNothing);
      expect(core.ops.map((o) => o.op), ['tab.set_title', 'tab.close']);
      expect(core.ops[0].params, {'tab_id': '22', 'title': 'logs'});
      expect(core.ops[1].params, {'tab_id': '22'});
    });
  });
}
