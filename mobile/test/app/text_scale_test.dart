import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

import '../support/pump_app.dart';

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

      if (wide) {
        expect(byKey('tab-strip'), findsOneWidget);
        expect(tester.takeException(), isNull);
        await tester.longPress(byKey('strip-tab-11'));
      } else {
        await tester.pageBack();
        await tester.pumpAndSettle();
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
