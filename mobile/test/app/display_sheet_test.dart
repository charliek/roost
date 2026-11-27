import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/app/display_sheet.dart';
import 'package:roost_mobile/app/providers.dart';
import 'package:roost_mobile/app/theme.dart';
import 'package:roost_mobile/app/width_class.dart';
import 'package:roost_mobile/core/fake/fake_core.dart';
import 'package:roost_mobile/core/roost_core.dart';
import 'package:roost_mobile/terminal/cell_metrics.dart';
import 'package:roost_mobile/terminal/terminal_view.dart';

import '../support/pump_app.dart';

/// FakeCore with as many dark themes as the real core bundles.
class _AllThemesCore extends FakeCore {
  @override
  List<ThemeSummary> terminalThemes() => [
    ...FakeCore.fakeThemes,
    for (var i = 0; i < 24; i++)
      ThemeSummary(
        name: 'Dark theme $i',
        light: false,
        background: 0x101010 + i,
        foreground: 0xe0e0e0,
        cursor: 0xffffff,
        ansi: List.filled(16, 0x808080),
      ),
  ];
}

/// FakeCore whose attaches wait for [gate] while it is set.
class _HeldAttachCore extends FakeCore {
  Completer<void>? gate;

  @override
  Future<TerminalSession> attach(
    String hostId,
    int tabId,
    Geometry geometry, {
    required bool take,
  }) async {
    await gate?.future;
    return super.attach(hostId, tabId, geometry, take: take);
  }
}

ProviderContainer _container(WidgetTester tester) =>
    ProviderScope.containerOf(tester.element(find.byType(MaterialApp)));

Future<FakeCore> _openTab(
  WidgetTester tester,
  int tabId, {
  double width = 1200,
}) async {
  final core = await pumpApp(tester, width: width);
  if (width < 600) {
    await tester.tap(byKey('host-card-devbox'));
    await tester.pumpAndSettle();
  }
  await tester.tap(byKey('tab-devbox-$tabId'));
  await tester.pumpAndSettle();
  return core;
}

Future<void> _openSheet(WidgetTester tester) async {
  await tester.tap(byKey('open-display'));
  await tester.pumpAndSettle();
  expect(byKey('display-sheet'), findsOneWidget);
}

String _label(WidgetTester tester) =>
    tester.widget<Text>(byKey('terminal-text-label')).data!;

void main() {
  for (final (name, width, container) in [
    ('compact', 360.0, DraggableScrollableSheet),
    ('medium', 700.0, Dialog),
    ('expanded', 1000.0, Dialog),
  ]) {
    testWidgets('$name: the gear opens the sheet', (tester) async {
      await pumpApp(tester, width: width, height: 900);
      await _openSheet(tester);
      expect(find.byType(container), findsOneWidget);
      expect(
        _label(tester),
        contains('about'),
        reason: 'no terminal on screen: the columns are an estimate',
      );
    });
  }

  testWidgets('the tab menu opens it from a terminal', (tester) async {
    await _openTab(tester, 13, width: 360);
    expect(byKey('open-display'), findsNothing, reason: 'the hosts screen');
    await tester.tap(find.byTooltip('Tab actions'));
    await tester.pumpAndSettle();
    expect(find.text('Larger text'), findsNothing);
    await tester.tap(byKey('menu-display'));
    await tester.pumpAndSettle();
    expect(byKey('display-sheet'), findsOneWidget);
    expect(_label(tester), isNot(contains('about')));
  });

  testWidgets('the slider re-grids the tab and takes its size', (tester) async {
    final core = await _openTab(tester, 13);
    final session = lastSession(core);
    await _openSheet(tester);
    expect(_label(tester), '13 pt · ${lastGeometry(core).cols} columns');

    await tester.drag(byKey('terminal-text-slider'), const Offset(80, 0));
    await tester.pump();
    await tester.pump(regridDebounce);
    final size = _container(tester).read(settingsProvider).desired;
    expect(size.terminalFontSizeWide, greaterThan(13));
    expect(size.terminalFontSizeCompact, 13, reason: 'the wide setting');
    final (geometry, focused) = session.resizes.last;
    expect(focused, isTrue, reason: 'the user changed the size');
    expect(
      _label(tester),
      '${formatPoints(size.terminalFontSizeWide)} pt · ${geometry.cols} columns',
    );
    await tester.pumpAndSettle();
    expect(
      core.settingsWrites.last.terminalFontSizeWide,
      size.terminalFontSizeWide,
    );
  });

  testWidgets('the label follows the pane while the sheet is open', (
    tester,
  ) async {
    final core = await _openTab(tester, 13);
    await _openSheet(tester);
    final before = _label(tester);
    await tester.binding.setSurfaceSize(const Size(1000, 1400));
    await tester.pumpAndSettle();
    await tester.pump(regridDebounce);
    await tester.pumpAndSettle();
    expect(_label(tester), isNot(before));
    expect(_label(tester), '13 pt · ${lastGeometry(core).cols} columns');
  });

  testWidgets('Reset goes back to 13', (tester) async {
    await _openTab(tester, 13);
    await _openSheet(tester);
    await tester.drag(byKey('terminal-text-slider'), const Offset(80, 0));
    await tester.pumpAndSettle();
    SettingsModel model() => _container(tester).read(settingsProvider);
    expect(model().desired.terminalFontSizeWide, isNot(13));
    await tester.tap(byKey('terminal-text-reset'));
    await tester.pumpAndSettle();
    expect(model().desired.terminalFontSizeWide, 13);
    expect(model().acknowledged.settings.terminalFontSizeWide, 13);
    expect(_label(tester), startsWith('13 pt · '));
  });

  testWidgets('the pinch readout stays 900 ms after the last change', (
    tester,
  ) async {
    await _openTab(tester, 13);
    expect(byKey('font-size-hud'), findsNothing);
    final center = tester.getCenter(byKey('terminal-canvas'));
    final a = await tester.startGesture(center - const Offset(100, 0));
    final b = await tester.startGesture(center + const Offset(100, 0));
    await tester.pump();
    for (final spread in [110.0, 125.0]) {
      await a.moveTo(center - Offset(spread, 0));
      await b.moveTo(center + Offset(spread, 0));
      await tester.pump();
    }
    expect(byKey('font-size-hud'), findsOneWidget);

    await tester.pump(const Duration(milliseconds: 500));
    await a.moveTo(center - const Offset(160, 0));
    await b.moveTo(center + const Offset(160, 0));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 800));
    expect(
      byKey('font-size-hud'),
      findsOneWidget,
      reason: 'the second change restarted the timer',
    );
    await tester.pump(const Duration(milliseconds: 100));
    expect(byKey('font-size-hud'), findsNothing);
    await a.up();
    await b.up();
    await tester.pumpAndSettle();
  });

  testWidgets('a light theme swatch recolors the open tab', (tester) async {
    await _openTab(tester, 13);
    int background() => tester
        .widget<TerminalView>(find.byType(TerminalView))
        .tab!
        .model
        .defaultBg;
    await _openSheet(tester);
    expect(find.text('Dark terminal theme'), findsOneWidget);

    await tester.tap(
      find.descendant(
        of: byKey('display-appearance'),
        matching: find.text('Light'),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('Light terminal theme'), findsOneWidget);
    expect(byKey('theme-swatch-roost-dark'), findsNothing);
    expect(background(), 0xeff1f5, reason: 'Catppuccin Latte, the default');

    final github = byKey('theme-swatch-GitHub Light Default');
    await tester.ensureVisible(github);
    await tester.pumpAndSettle();
    await tester.tap(github);
    await tester.pumpAndSettle();
    expect(background(), 0xffffff);
    expect(
      _container(tester)
          .read(settingsProvider)
          .acknowledged
          .settings
          .lightTheme,
      'GitHub Light Default',
    );
  });

  testWidgets('at 360x640 and 2.0x text every control can be reached', (
    tester,
  ) async {
    tester.platformDispatcher.textScaleFactorTestValue = 2.0;
    addTearDown(tester.platformDispatcher.clearTextScaleFactorTestValue);
    final core = _AllThemesCore();
    await pumpApp(tester, width: 360, height: 640, core: core);
    await _openSheet(tester);
    expect(tester.takeException(), isNull);
    final last = byKey('bright-faint');
    await tester.scrollUntilVisible(
      last,
      200,
      scrollable: find
          .descendant(
            of: byKey('display-sheet'),
            matching: find.byType(Scrollable),
          )
          .first,
    );
    await tester.pumpAndSettle();
    await tester.tap(last);
    await tester.pumpAndSettle();
    expect(tester.takeException(), isNull);
    expect(core.settingsWrites.last.brightFaint, isTrue);
  });

  test('the pane mounted last owns the reported size', () {
    final container = ProviderContainer();
    addTearDown(container.dispose);
    final sizes = container.read(terminalPaneSizeProvider.notifier);
    Size? shown() => container.read(terminalPaneSizeProvider)?.gridArea;

    final outgoing = sizes.claim();
    sizes.report(outgoing, (gridArea: const Size(100, 100), sizedBy: null));
    final incoming = sizes.claim();
    sizes.report(incoming, (gridArea: const Size(300, 500), sizedBy: null));
    sizes.report(outgoing, (gridArea: const Size(100, 120), sizedBy: null));
    expect(shown(), const Size(300, 500), reason: 'a late outgoing report');
    sizes.release(outgoing);
    expect(shown(), const Size(300, 500), reason: 'the outgoing pane went');
    sizes.release(incoming);
    expect(shown(), isNull);
  });

  testWidgets('a quick back-and-open keeps the new pane\'s size', (
    tester,
  ) async {
    await _openTab(tester, 13, width: 360);
    await tester.tap(find.byType(BackButton));
    await tester.pump(const Duration(milliseconds: 50));
    await tester.tap(byKey('tab-devbox-12'));
    await tester.pumpAndSettle();
    expect(_container(tester).read(terminalPaneSizeProvider), isNotNull);
    await tester.tap(find.byTooltip('Tab actions'));
    await tester.pumpAndSettle();
    await tester.tap(byKey('menu-display'));
    await tester.pumpAndSettle();
    expect(_label(tester), isNot(contains('about')));
  });

  testWidgets('a slide while the tab attaches still takes its size', (
    tester,
  ) async {
    final core = _HeldAttachCore();
    await pumpApp(tester, width: 1200, core: core);
    final gate = core.gate = Completer();
    await tester.tap(byKey('tab-devbox-13'));
    await tester.pump();
    await tester.pump();
    await _openSheet(tester);
    expect(core.sessions, isEmpty, reason: 'still attaching');

    await tester.drag(byKey('terminal-text-slider'), const Offset(80, 0));
    await tester.pump();
    await tester.pump(regridDebounce);
    gate.complete();
    await tester.pumpAndSettle();

    final attached = core.attaches.single.geometry;
    final (geometry, focused) = lastSession(core).resizes.last;
    expect(geometry.cols, lessThan(attached.cols), reason: 'larger text');
    expect(focused, isTrue, reason: 'the user changed the size');
  });

  testWidgets('a size change that keeps the same grid still takes the size', (
    tester,
  ) async {
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetDevicePixelRatio);
    final core = await _openTab(tester, 13, width: 320);
    final session = lastSession(core);
    core.takeSizeFrom(
      'devbox',
      13,
      label: 'charlie-laptop',
      cols: 132,
      rows: 40,
    );
    await tester.pumpAndSettle();
    await tester.pump(regridDebounce);
    await tester.pumpAndSettle();
    final tab = tester.widget<TerminalView>(find.byType(TerminalView)).tab!;
    expect(tab.sizedElsewhere, isTrue);
    final before = lastGeometry(core);
    final area = _container(tester).read(terminalPaneSizeProvider)!.gridArea;
    expect(
      CellMetrics.measure(12.5, 1).geometryFor(area),
      before,
      reason: 'precondition: 12.5 pt rounds to the 13 pt grid here',
    );

    _container(tester)
        .read(settingsProvider.notifier)
        .edit((s) => s.withTerminalFontSize(WidthClass.compact, 12.5));
    await tester.pump();
    await tester.pump(regridDebounce);
    await tester.pumpAndSettle();
    expect(session.resizes.last, (before, true));
    expect(tab.sizedElsewhere, isFalse, reason: 'the fake took the size');
  });

  for (final (name, width) in [('sheet', 360.0), ('dialog', 1000.0)]) {
    testWidgets('the $name follows an appearance change made inside it', (
      tester,
    ) async {
      await pumpApp(tester, width: width);
      await _openSheet(tester);
      Color? surface() => tester
          .widget<Material>(
            find
                .ancestor(
                  of: byKey('display-sheet'),
                  matching: find.byType(Material),
                )
                .first,
          )
          .color;
      expect(surface(), RoostPalette.dark.surface);
      await tester.tap(
        find.descendant(
          of: byKey('display-appearance'),
          matching: find.text('Light'),
        ),
      );
      await tester.pumpAndSettle();
      expect(surface(), RoostPalette.light.surface);
    });
  }
}
