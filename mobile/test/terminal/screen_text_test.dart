import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/terminal/frame.dart';
import 'package:roost_mobile/terminal/screen_text.dart';
import 'package:roost_mobile/terminal/terminal_model.dart';

TerminalModel _screen(List<String> lines, {int cols = 20}) => TerminalModel()
  ..apply(
    Frame(
      flags: FrameFlags.full,
      cols: cols,
      rows: lines.length,
      defaultFg: 1,
      defaultBg: 0,
      cursorColor: 2,
      rowData: [
        for (final (i, line) in lines.indexed)
          if (line.isNotEmpty)
            FrameRow(
              index: i,
              runs: [
                FrameRun(col: 0, cells: line.length, fg: 1, bg: 0, text: line),
              ],
            ),
      ],
    ),
  );

void main() {
  test('screen text keeps inner blank rows and drops trailing ones', () {
    final m = _screen(['one', '', 'three', '', '']);
    expect(screenText(m), 'one\n\nthree');
  });

  test('a URL that runs to the right edge continues on the next row', () {
    final m = _screen([
      'Open this link:',
      'https://claude.ai/oa',
      'uth/authorize?code=t',
      'rue&state=abc',
      r'$',
    ]);
    expect(screenUrls(m), [
      'https://claude.ai/oauth/authorize?code=true&state=abc',
    ]);
  });

  test('a wrap a padded box made, a column or two short, still joins', () {
    final m = _screen(['  https://example.c', '  om/a/b/c/d/e/f/g', '  /h']);
    expect(screenUrls(m), ['https://example.com/a/b/c/d/e/f/g/h']);
  });

  test('a short URL at the end of a short row does not take the next row', () {
    final m = _screen(['see https://a.io.', 'Next steps'], cols: 40);
    expect(screenUrls(m), ['https://a.io']);
  });

  test('URLs are listed once, in order, without trailing punctuation', () {
    final m = _screen([
      '(https://b.io/x)',
      'go https://a.io, ok',
      'https://b.io/x',
    ], cols: 40);
    expect(screenUrls(m), ['https://b.io/x', 'https://a.io']);
  });

  test('a balanced paren stays in the URL', () {
    final m = _screen(['https://en.wiki/Foo_(bar)'], cols: 40);
    expect(screenUrls(m), ['https://en.wiki/Foo_(bar)']);
  });

  test('a continuation row is not read as a URL of its own', () {
    final m = _screen(['https://a.io/?next=', 'https://b.io/done'], cols: 19);
    expect(screenUrls(m), ['https://a.io/?next=https://b.io/done']);
  });
}
