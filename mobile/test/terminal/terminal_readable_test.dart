import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/terminal/cell_metrics.dart';
import 'package:roost_mobile/terminal/contrast.dart';
import 'package:roost_mobile/terminal/frame.dart';
import 'package:roost_mobile/terminal/terminal_model.dart';
import 'package:roost_mobile/terminal/terminal_painter.dart';

import '../support/fonts.dart';

const _bg = 0xeff1f5;
const _fg = 0x4c4f69;

FrameRun _run(int col, String text, {int fg = _fg, int attrs = 0}) => FrameRun(
  col: col,
  cells: text.length,
  fg: fg,
  bg: _bg,
  attrs: attrs,
  text: text,
);

void main() {
  testWidgets('a light theme with readable text on', (tester) async {
    await loadTerminalFont();
    final metrics = CellMetrics.measure(14, 1);
    final frame = Frame(
      flags: FrameFlags.full | FrameFlags.cursorVisible,
      cols: 28,
      rows: 5,
      defaultFg: _fg,
      defaultBg: _bg,
      cursorColor: 0xdc8a78,
      cursorCol: 13,
      cursorRow: 3,
      rowData: [
        FrameRow(index: 0, runs: [_run(0, 'normal text', fg: _fg)]),
        FrameRow(
          index: 1,
          runs: [
            _run(0, 'pale gray #dddddd', fg: 0xdddddd),
            _run(18, 'yellow', fg: 0xe5c07b),
          ],
        ),
        FrameRow(
          index: 2,
          runs: [
            _run(0, 'faint text', attrs: RunAttrs.faint),
            _run(11, 'faint pale', fg: 0xdddddd, attrs: RunAttrs.faint),
          ],
        ),
        FrameRow(index: 3, runs: [_run(0, 'cursor on a glyph', fg: _fg)]),
      ],
    );
    await tester.pumpWidget(
      Directionality(
        textDirection: TextDirection.ltr,
        child: Align(
          alignment: Alignment.topLeft,
          child: RepaintBoundary(
            key: const ValueKey('golden'),
            child: CustomPaint(
              size: Size(
                frame.cols * metrics.cellWidth + 8,
                frame.rows * metrics.cellHeight + 8,
              ),
              painter: TerminalPainter(
                model: TerminalModel()..apply(frame),
                metrics: metrics,
                textCache: RowTextCache(),
                localCols: frame.cols,
                localRows: frame.rows,
                focused: true,
                inset: 4,
                readability: const TextReadability(minimumContrast: 3.0),
              ),
            ),
          ),
        ),
      ),
    );
    await expectLater(
      find.byKey(const ValueKey('golden')),
      matchesGoldenFile('goldens/terminal_readable.png'),
    );
  });
}
