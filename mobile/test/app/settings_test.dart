import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/app/providers.dart';
import 'package:roost_mobile/app/width_class.dart';
import 'package:roost_mobile/core/fake/fake_core.dart';
import 'package:roost_mobile/core/roost_core.dart';
import 'package:roost_mobile/terminal/terminal_pane.dart';
import 'package:roost_mobile/terminal/terminal_view.dart';

import '../support/pump_app.dart';

/// One settings write the core has not finished: the test completes it.
class _Write {
  _Write(this.settings);

  final MobileSettings settings;
  final _done = Completer<void>();

  void complete() => _done.complete();
  void fail() => _done.completeError(const CoreError('disk full'));
}

/// FakeCore whose settings writes wait for the test, and whose
/// `setSystemDark` waits too while [systemDarkGate] is set.
class _HeldCore extends FakeCore {
  final List<_Write> held = [];
  Completer<void>? systemDarkGate;

  @override
  Future<SettingsState> updateSettings(MobileSettings settings) async {
    final write = _Write(settings);
    held.add(write);
    await write._done.future;
    return super.updateSettings(settings);
  }

  @override
  Future<SettingsState> setSystemDark(bool dark) async {
    await systemDarkGate?.future;
    return super.setSystemDark(dark);
  }
}

ProviderContainer _container(WidgetTester tester) =>
    ProviderScope.containerOf(tester.element(find.byType(MaterialApp)));

void main() {
  late _HeldCore core;
  late ProviderContainer container;

  SettingsModel model() => container.read(settingsProvider);
  SettingsNotifier settings() => container.read(settingsProvider.notifier);

  Future<void> start(WidgetTester tester) async {
    core = _HeldCore();
    await pumpApp(tester, width: 400, core: core);
    container = _container(tester);
  }

  testWidgets('continuous edits are written once, 300 ms after the last', (
    tester,
  ) async {
    await start(tester);
    for (final size in [14.0, 15.0, 16.0]) {
      settings().edit(
        (s) => s.copyWith(terminalFontSizeWide: size),
        continuous: true,
      );
      await tester.pump(const Duration(milliseconds: 100));
    }
    expect(model().desired.terminalFontSizeWide, 16, reason: 'shown at once');
    await tester.pump(const Duration(milliseconds: 150));
    expect(core.held, isEmpty, reason: '250 ms after the last step');
    await tester.pump(const Duration(milliseconds: 50));
    expect(core.held.map((w) => w.settings.terminalFontSizeWide), [16]);
    core.held.single.complete();
    await tester.pump();
    expect(model().acknowledged.settings.terminalFontSizeWide, 16);
  });

  testWidgets('a pinch end writes at once', (tester) async {
    await start(tester);
    settings().edit(
      (s) => s.copyWith(terminalFontSizeCompact: 18),
      continuous: true,
    );
    await tester.pump();
    expect(core.held, isEmpty);
    settings().flush();
    await tester.pump();
    expect(core.held.map((w) => w.settings.terminalFontSizeCompact), [18]);
    core.held.single.complete();
    await tester.pump(settingsWriteDelay * 2);
    expect(core.held, hasLength(1), reason: 'the flush took the timer');
  });

  testWidgets('an older write finishing never undoes a newer edit', (
    tester,
  ) async {
    await start(tester);
    settings().edit(
      (s) => s.copyWith(terminalFontSizeWide: 15),
      continuous: true,
    );
    await tester.pump(settingsWriteDelay);
    expect(core.held, hasLength(1), reason: 'the trailing write is out');
    settings().edit((s) => s.copyWith(appearance: Appearance.light));
    await tester.pump();
    expect(core.held, hasLength(1), reason: 'one write at a time');

    core.held.first.complete();
    await tester.pump();
    expect(model().acknowledged.settings.appearance, Appearance.dark);
    expect(model().desired.appearance, Appearance.light, reason: 'kept');
    expect(model().desired.terminalFontSizeWide, 15);
    expect(core.held, hasLength(2));
    expect(
      core.held.last.settings,
      const MobileSettings(
        terminalFontSizeWide: 15,
        appearance: Appearance.light,
      ),
      reason: 'the next write sends the newest edit',
    );
    core.held.last.complete();
    await tester.pump();
    expect(model().acknowledged.effectiveDark, isFalse);
    expect(model().desired, model().acknowledged.settings);
  });

  testWidgets('an older failure does not revert a newer edit', (tester) async {
    await start(tester);
    settings().edit((s) => s.copyWith(appearance: Appearance.light));
    await tester.pump();
    settings().edit(
      (s) => s.copyWith(terminalFontSizeWide: 20),
      continuous: true,
    );
    core.held.single.fail();
    await tester.pump();
    expect(model().desired.terminalFontSizeWide, 20);
    expect(model().desired.appearance, Appearance.light);
    expect(byKey('settings-error'), findsOneWidget, reason: 'still reported');

    await tester.pump(settingsWriteDelay);
    expect(core.held, hasLength(2));
    expect(
      core.held.last.settings,
      const MobileSettings(
        terminalFontSizeWide: 20,
        appearance: Appearance.light,
      ),
    );
    core.held.last.complete();
    await tester.pump();
    expect(model().acknowledged.settings, core.held.last.settings);
  });

  testWidgets('a failure with nothing newer reverts and says so', (
    tester,
  ) async {
    await start(tester);
    final before = model().acknowledged.settings;
    settings().edit((s) => s.copyWith(darkTheme: 'Catppuccin Mocha'));
    await tester.pump();
    expect(model().desired.darkTheme, 'Catppuccin Mocha');
    core.held.single.fail();
    await tester.pump();
    expect(model().desired, before);
    await tester.pump(const Duration(milliseconds: 500));
    expect(byKey('settings-error'), findsOneWidget);
    expect(find.textContaining('disk full'), findsOneWidget);
  });

  testWidgets('each width class keeps its own terminal size', (tester) async {
    await start(tester);
    await tester.tap(byKey('host-card-devbox'));
    await tester.pumpAndSettle();
    await tester.tap(byKey('tab-devbox-13'));
    await tester.pumpAndSettle();
    double drawn() =>
        tester.widget<TerminalView>(find.byType(TerminalView)).fontSize;

    settings().edit((s) => s.withTerminalFontSize(WidthClass.compact, 14));
    await tester.pumpAndSettle();
    expect(core.held.single.settings.terminalFontSizeCompact, 14);
    expect(core.held.single.settings.terminalFontSizeWide, 13);
    core.held.single.complete();
    await tester.pumpAndSettle();
    expect(drawn(), 14);

    await tester.binding.setSurfaceSize(const Size(1200, 1400));
    await tester.pumpAndSettle();
    expect(find.byType(TerminalPane), findsOneWidget);
    expect(drawn(), 13, reason: 'the wide size');
    await tester.binding.setSurfaceSize(const Size(400, 1400));
    await tester.pumpAndSettle();
    expect(drawn(), 14, reason: 'the compact size again');
    expect(model().acknowledged.settings.terminalFontSizeCompact, 14);
  });

  testWidgets('edits behind a platform change are sent as they are then', (
    tester,
  ) async {
    await start(tester);
    core.systemDarkGate = Completer();
    unawaited(settings().setSystemDark(false));
    await tester.pump();
    settings().edit((s) => s.copyWith(terminalFontSizeWide: 14));
    settings().edit((s) => s.copyWith(terminalFontSizeWide: 16));
    await tester.pump();
    expect(core.held, isEmpty, reason: 'queued behind the platform change');
    core.systemDarkGate!.complete();
    await tester.pump();
    expect(core.held.map((w) => w.settings.terminalFontSizeWide), [16]);
    core.held.single.complete();
    await tester.pump();
    expect(model().acknowledged.settings.terminalFontSizeWide, 16);
    expect(model().acknowledged.effectiveDark, isTrue, reason: 'Dark forced');
  });

  testWidgets('a revision is written once, whatever asks for it again', (
    tester,
  ) async {
    await start(tester);
    settings().edit((s) => s.copyWith(terminalFontSizeWide: 14));
    await tester.pump();
    expect(core.held, hasLength(1));
    settings().edit((s) => s.copyWith(terminalFontSizeWide: 15));
    settings().edit(
      (s) => s.copyWith(terminalFontSizeWide: 16),
      continuous: true,
    );
    core.held.first.complete();
    await tester.pump();
    expect(core.held.map((w) => w.settings.terminalFontSizeWide), [14, 16]);
    core.held.last.complete();
    await tester.pump();
    await tester.pump(settingsWriteDelay * 2);
    settings().flush();
    await tester.pump();
    expect(core.held.map((w) => w.settings.terminalFontSizeWide), [
      14,
      16,
    ], reason: 'neither the debounce nor a flush writes 16 again');
    expect(model().acknowledged.settings.terminalFontSizeWide, 16);
  });

  testWidgets('ending a pinch on the terminal writes at once', (tester) async {
    core = _HeldCore();
    await pumpApp(tester, width: 1200, core: core);
    container = _container(tester);
    await tester.tap(byKey('tab-devbox-13'));
    await tester.pumpAndSettle();
    final center = tester.getCenter(byKey('terminal-canvas'));
    final a = await tester.startGesture(center - const Offset(100, 0));
    final b = await tester.startGesture(center + const Offset(100, 0));
    await tester.pump();
    for (final spread in [120.0, 140.0, 160.0]) {
      await a.moveTo(center - Offset(spread, 0));
      await b.moveTo(center + Offset(spread, 0));
      await tester.pump(const Duration(milliseconds: 16));
    }
    final pinched = model().desired.terminalFontSizeWide;
    expect(pinched, greaterThan(13));
    expect(core.held, isEmpty, reason: 'mid-gesture: the write trails');
    await a.up();
    await b.up();
    await tester.pump(const Duration(milliseconds: 10));
    expect(core.held.map((w) => w.settings.terminalFontSizeWide), [
      pinched,
    ], reason: 'written when the pinch ended, not 300 ms later');
    core.held.single.complete();
    await tester.pumpAndSettle();
  });
}
