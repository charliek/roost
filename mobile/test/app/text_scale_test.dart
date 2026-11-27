import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/app/providers.dart';
import 'package:roost_mobile/app/text_scale.dart';
import 'package:roost_mobile/terminal/cell_metrics.dart';
import 'package:roost_mobile/terminal/terminal_painter.dart';
import 'package:roost_mobile/terminal/terminal_view.dart';

import '../support/pump_app.dart';

/// Android's kind of scaler: small text grows more than large text.
class _Nonlinear extends TextScaler {
  const _Nonlinear();

  @override
  double scale(double fontSize) => fontSize + 6;

  @override
  double get textScaleFactor => scale(14) / 14;
}

const _layouts = [
  (name: 'compact', width: 360.0, height: 800.0),
  (name: 'medium', width: 700.0, height: 900.0),
  (name: 'expanded', width: 1000.0, height: 900.0),
];

Future<void> _openTab(WidgetTester tester, double width, int tabId) async {
  if (width < 600) {
    await tester.tap(byKey('host-card-devbox'));
    await tester.pumpAndSettle();
  }
  await tester.tap(byKey('tab-devbox-$tabId'));
  await tester.pumpAndSettle();
}

void main() {
  group('ScaledTextScaler', () {
    test('builds on a nonlinear base', () {
      const base = _Nonlinear();
      expect(base.scale(14) / 14, isNot(base.scale(28) / 28));
      final scaler = ScaledTextScaler.over(base, 1.2);
      expect(scaler.scale(14), closeTo(20 * 1.2, 1e-9));
      expect(scaler.scale(28), closeTo(34 * 1.2, 1e-9));
      expect(
        // ignore: deprecated_member_use
        scaler.textScaleFactor,
        closeTo(20 * 1.2 / 14, 1e-9),
      );
    });

    test('never more than twice the size', () {
      final scaler = ScaledTextScaler.over(const TextScaler.linear(1.8), 1.5);
      expect(scaler.scale(14), 28);
      expect(scaler.scale(40), 80);
      expect(
        ScaledTextScaler.over(const TextScaler.linear(1.8), 1.1).scale(10),
        19.8,
      );
    });

    test('compares by base and factor', () {
      const base = TextScaler.linear(1.3);
      final a = ScaledTextScaler.over(base, 1.2);
      final b = ScaledTextScaler.over(const TextScaler.linear(1.3), 1.2);
      expect(a, b);
      expect(a.hashCode, b.hashCode);
      expect(a, isNot(ScaledTextScaler.over(base, 1.25)));
      expect(a, isNot(ScaledTextScaler.over(const _Nonlinear(), 1.2)));
    });

    test('is the base itself at 1.0 within the cap', () {
      const base = TextScaler.linear(1.3);
      expect(identical(ScaledTextScaler.over(base, 1.0), base), isTrue);
      const huge = TextScaler.linear(2.5);
      final capped = ScaledTextScaler.over(huge, 1.0);
      expect(identical(capped, huge), isFalse);
      expect(capped.scale(14), 28);
    });
  });

  testWidgets('app text at 150% grows the chrome, not the terminal grid', (
    tester,
  ) async {
    final core = await pumpApp(tester, width: 1200);
    await tester.tap(byKey('tab-devbox-13'));
    await tester.pumpAndSettle();
    final container = ProviderScope.containerOf(
      tester.element(find.byType(MaterialApp)),
    );
    final title = find
        .descendant(of: byKey('terminal-toolbar'), matching: find.byType(Text))
        .first;
    CellMetrics metrics() =>
        (tester.widget<CustomPaint>(byKey('terminal-canvas')).painter!
                as TerminalPainter)
            .metrics;
    final titleHeight = tester.getSize(title).height;
    final cells = metrics();
    final area = container.read(terminalPaneSizeProvider)!.gridArea;
    final grid = lastGeometry(core);

    container
        .read(settingsProvider.notifier)
        .edit((s) => s.copyWith(uiTextScale: 1.5));
    await tester.pumpAndSettle();
    await tester.pump(regridDebounce);
    await tester.pumpAndSettle();

    expect(core.settingsWrites.last.uiTextScale, 1.5);
    expect(tester.getSize(title).height, greaterThan(titleHeight));
    expect(metrics(), cells, reason: 'the painter ignores the text scale');
    final scaledArea = container.read(terminalPaneSizeProvider)!.gridArea;
    expect(scaledArea.width, area.width);
    expect(scaledArea.height, lessThan(area.height), reason: 'taller bars');
    final scaled = lastGeometry(core);
    expect(scaled.cols, grid.cols);
    expect(
      (scaled.cellWidthPx, scaled.cellHeightPx),
      (grid.cellWidthPx, grid.cellHeightPx),
    );
    expect(scaled.rows, lessThanOrEqualTo(grid.rows));
  });

  testWidgets('dragging the app text slider scales the app under the sheet', (
    tester,
  ) async {
    final core = await pumpApp(tester, width: 1200);
    await tester.tap(byKey('tab-devbox-13'));
    await tester.pumpAndSettle();
    final title = find
        .descendant(of: byKey('terminal-toolbar'), matching: find.byType(Text))
        .first;
    final titleHeight = tester.getSize(title).height;
    await tester.tap(byKey('open-display'));
    await tester.pumpAndSettle();
    String label() => tester.widget<Text>(byKey('app-text-label')).data!;
    expect(label(), '100%');

    final slider = tester.getRect(byKey('app-text-slider'));
    final drag = await tester.startGesture(slider.center);
    await tester.pump();
    await drag.moveBy(const Offset(40, 0));
    await tester.pump();
    final mid = label();
    expect(mid, isNot('100%'));
    await drag.moveBy(const Offset(40, 0));
    await tester.pump();
    expect(label(), isNot(mid), reason: 'the drag outlived the rebuild');
    await drag.up();
    await tester.pumpAndSettle();

    expect(byKey('display-sheet'), findsOneWidget, reason: 'still open');
    final scale = core.settingsWrites.last.uiTextScale;
    expect(scale, greaterThan(1));
    expect(label(), '${(scale * 100).round()}%');
    expect(tester.getSize(title).height, greaterThan(titleHeight));
  });

  for (final layout in _layouts) {
    final wide = layout.width >= 600;

    testWidgets('${layout.name}: nothing overflows at 2.0x text', (
      tester,
    ) async {
      tester.platformDispatcher.textScaleFactorTestValue = 2.0;
      addTearDown(tester.platformDispatcher.clearTextScaleFactorTestValue);
      await pumpApp(tester, width: layout.width, height: layout.height);
      expect(tester.takeException(), isNull);

      await tester.tap(byKey('add-host'));
      await tester.pumpAndSettle();
      expect(byKey('add-host-target'), findsOneWidget);
      expect(tester.takeException(), isNull);
      await tester.sendKeyEvent(LogicalKeyboardKey.escape);
      await tester.pumpAndSettle();

      await _openTab(tester, layout.width, 11);
      expect(byKey('terminal-toolbar'), findsOneWidget);
      expect(tester.takeException(), isNull);

      await tester.tap(byKey('terminal-canvas'));
      await tester.pumpAndSettle();
      expect(byKey('extra-keys'), findsOneWidget);
      expect(tester.takeException(), isNull);
      // A label that outgrows its bar is clipped without an exception.
      expect(tester.getSize(byKey('extra-keys')).height, greaterThan(48));

      if (wide) {
        expect(byKey('tab-strip'), findsOneWidget);
        expect(tester.takeException(), isNull);
        expect(tester.getSize(byKey('tab-strip')).height, greaterThan(44));
        await tester.longPress(byKey('strip-tab-11'));
      } else {
        await tester.pageBack();
        await tester.pumpAndSettle();
        final bar = tester.getRect(find.byType(AppBar));
        final titles = find.descendant(
          of: find.byType(AppBar),
          matching: find.byType(Text),
        );
        expect(titles, findsWidgets);
        for (final title in titles.evaluate()) {
          final rect = tester.getRect(find.byWidget(title.widget));
          expect(rect.bottom, lessThanOrEqualTo(bar.bottom));
        }
        await tester.longPress(byKey('tab-devbox-11'));
      }
      await tester.pumpAndSettle();
      await tester.tap(byKey('tab-action-rename'));
      await tester.pumpAndSettle();
      expect(byKey('prompt-field'), findsOneWidget);
      expect(tester.takeException(), isNull);
    });

    testWidgets('${layout.name}: fixed bars keep their size at 1.0x', (
      tester,
    ) async {
      await pumpApp(tester, width: layout.width, height: layout.height);
      await _openTab(tester, layout.width, 11);
      await tester.tap(byKey('terminal-canvas'));
      await tester.pumpAndSettle();
      expect(tester.getSize(byKey('terminal-toolbar')).height, 48);
      expect(tester.getSize(byKey('extra-keys')).height, 48);
      if (wide) expect(tester.getSize(byKey('tab-strip')).height, 44);
    });
  }
}
