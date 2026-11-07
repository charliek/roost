import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/terminal/frame.dart';
import 'package:roost_mobile/terminal/terminal_model.dart';

FrameRun _run(int col, String text, {int attrs = 0}) => FrameRun(
  col: col,
  cells: text.length,
  fg: 1,
  bg: 0,
  attrs: attrs,
  text: text,
);

Frame _frame({
  bool full = false,
  int cols = 20,
  int rows = 4,
  Map<int, List<FrameRun>> rowData = const {},
  int flags = 0,
}) => Frame(
  flags: (full ? FrameFlags.full : 0) | flags,
  cols: cols,
  rows: rows,
  defaultFg: 1,
  defaultBg: 0,
  cursorColor: 2,
  rowData: [
    for (final e in rowData.entries) FrameRow(index: e.key, runs: e.value),
  ],
);

void main() {
  test('FULL drops every row the frame does not carry', () {
    final m = TerminalModel()
      ..apply(
        _frame(
          full: true,
          rowData: {
            0: [_run(0, 'one')],
            1: [_run(0, 'two')],
          },
        ),
      )
      ..apply(
        _frame(
          full: true,
          rowData: {
            1: [_run(2, 'new')],
          },
        ),
      );
    expect(m.rowText(0), '');
    expect(m.rowText(1), '  new');
  });

  test('a delta replaces only the rows it carries', () {
    final m = TerminalModel()
      ..apply(
        _frame(
          full: true,
          rowData: {
            0: [_run(0, 'keep')],
            1: [_run(0, 'old')],
          },
        ),
      );
    final gen0 = m.rowGeneration(0);
    final gen1 = m.rowGeneration(1);
    m.apply(
      _frame(
        rowData: {
          1: [_run(0, 'new'), _run(5, 'x')],
        },
      ),
    );
    expect(m.rowText(0), 'keep');
    expect(m.rowText(1), 'new  x');
    expect(m.rowGeneration(0), gen0, reason: 'untouched row keeps its text');
    expect(m.rowGeneration(1), isNot(gen1));
  });

  test('a row sent with no runs is blank', () {
    final m = TerminalModel()
      ..apply(
        _frame(
          full: true,
          rowData: {
            2: [_run(0, 'text')],
          },
        ),
      )
      ..apply(_frame(rowData: {2: []}));
    expect(m.row(2), isNull);
  });

  test('a grid change without FULL still drops the old rows', () {
    final m = TerminalModel()
      ..apply(
        _frame(
          full: true,
          rowData: {
            0: [_run(0, 'wide row')],
          },
        ),
      )
      ..apply(_frame(cols: 10, rows: 2));
    expect((m.cols, m.rows), (10, 2));
    expect(m.row(0), isNull);
  });

  test('rows past the grid are ignored', () {
    final m = TerminalModel()
      ..apply(
        _frame(
          full: true,
          rows: 2,
          rowData: {
            5: [_run(0, 'x')],
          },
        ),
      );
    expect(m.row(5), isNull);
  });

  test('modes and scroll position come from the latest frame', () {
    final m = TerminalModel()
      ..apply(
        _frame(
          full: true,
          flags:
              FrameFlags.cursorVisible |
              FrameFlags.mouseTracking |
              FrameFlags.bracketedPaste,
        ),
      );
    expect(m.cursorVisible, isTrue);
    expect(m.mouseTracking, isTrue);
    expect(m.bracketedPaste, isTrue);
    expect(m.altScreen, isFalse);
    m.apply(
      const Frame(
        cols: 20,
        rows: 4,
        defaultFg: 1,
        defaultBg: 0,
        cursorColor: 2,
        scrollTotal: 100,
        scrollOffset: 50,
        scrollLen: 4,
      ),
    );
    expect(m.cursorVisible, isFalse);
    expect(m.scrolledBack, isTrue);
  });

  test('every apply notifies', () {
    final m = TerminalModel();
    var n = 0;
    m.addListener(() => n++);
    m
      ..apply(_frame(full: true))
      ..apply(_frame());
    expect(n, 2);
  });
}
