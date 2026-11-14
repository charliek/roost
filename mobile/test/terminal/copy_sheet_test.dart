import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/app/theme.dart';
import 'package:roost_mobile/terminal/copy_sheet.dart';
import 'package:roost_mobile/terminal/frame.dart';
import 'package:roost_mobile/terminal/terminal_model.dart';

import '../support/fonts.dart';

const _cols = 51;

TerminalModel _fullRows(List<String> rows) => TerminalModel()
  ..apply(
    Frame(
      flags: FrameFlags.full,
      cols: _cols,
      rows: rows.length,
      defaultFg: 1,
      defaultBg: 0,
      cursorColor: 2,
      rowData: [
        for (final (i, text) in rows.indexed)
          FrameRow(
            index: i,
            runs: [
              FrameRun(col: 0, cells: text.length, fg: 1, bg: 0, text: text),
            ],
          ),
      ],
    ),
  );

Future<void> _openSheet(WidgetTester tester, TerminalModel model) async {
  await tester.pumpWidget(
    MaterialApp(
      theme: roostDarkTheme(),
      home: Scaffold(
        body: Builder(
          builder: (context) => TextButton(
            onPressed: () => showCopySheet(context, model),
            child: const Text('open'),
          ),
        ),
      ),
    ),
  );
  await tester.tap(find.text('open'));
  await tester.pumpAndSettle();
}

/// The distinct line tops a range of the sheet's text lays out on.
Set<double> _lineTops(WidgetTester tester, int start, int end) {
  final paragraph = tester.renderObject<RenderParagraph>(
    find.descendant(
      of: find.byKey(const ValueKey('copy-text')),
      matching: find.byType(RichText),
    ),
  );
  return paragraph
      .getBoxesForSelection(TextSelection(baseOffset: start, extentOffset: end))
      .map((box) => box.top)
      .toSet();
}

void main() {
  setUp(loadTerminalFont);

  for (final scale in [1.0, 1.3]) {
    testWidgets('a full-width row is one line at text scale $scale', (
      tester,
    ) async {
      tester.view
        ..physicalSize = const Size(1080, 2400)
        ..devicePixelRatio = 2.625;
      tester.platformDispatcher.textScaleFactorTestValue = scale;
      addTearDown(tester.view.reset);
      addTearDown(tester.platformDispatcher.clearAllTestValues);

      final row = 'https://claude.com/cai/oauth/authorize?code=true&cl';
      expect(row.length, _cols);
      await _openSheet(tester, _fullRows([row, 'x' * _cols]));
      expect(_lineTops(tester, 0, _cols), hasLength(1));
      expect(_lineTops(tester, _cols + 1, 2 * _cols + 1), hasLength(1));
    });
  }

  testWidgets('the Copy menu hides while scrolling and when out of sight', (
    tester,
  ) async {
    tester.view
      ..physicalSize = const Size(1080, 2400)
      ..devicePixelRatio = 2.625;
    addTearDown(tester.view.reset);
    await _openSheet(
      tester,
      _fullRows([for (var i = 0; i < 120; i++) 'line$i word$i']),
    );
    final row = find.descendant(
      of: find.byKey(const ValueKey('copy-text')),
      matching: find.byType(RichText),
    );
    final topLeft = tester.getTopLeft(row);
    await tester.longPressAt(topLeft + const Offset(20, 60));
    await tester.pumpAndSettle();
    expect(find.text('Copy'), findsOneWidget);

    final gesture = await tester.startGesture(topLeft + const Offset(200, 400));
    await gesture.moveBy(const Offset(0, -20));
    await tester.pump();
    expect(find.text('Copy'), findsNothing, reason: 'hidden while scrolling');
    await gesture.up();
    await tester.pumpAndSettle();
    expect(find.text('Copy'), findsOneWidget, reason: 'back once it settles');

    await tester.dragFrom(
      topLeft + const Offset(200, 400),
      const Offset(0, -300),
    );
    await tester.pumpAndSettle();
    expect(
      find.text('Copy'),
      findsNothing,
      reason: 'the selection scrolled out of sight above the text',
    );
  });
}
