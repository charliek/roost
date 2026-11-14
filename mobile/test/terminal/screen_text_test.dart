import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/terminal/frame.dart';
import 'package:roost_mobile/terminal/screen_text.dart';
import 'package:roost_mobile/terminal/terminal_model.dart';

FrameRun _run(int col, String text, {int? cells, int attrs = 0}) => FrameRun(
  col: col,
  cells: cells ?? text.length,
  fg: 1,
  bg: 0,
  attrs: attrs,
  text: text,
);

/// A wide character as the core sends it: its own two-cell run.
FrameRun _wide(int col, String glyph) =>
    _run(col, glyph, cells: 2, attrs: RunAttrs.wide);

TerminalModel _model(int cols, List<List<FrameRun>> rows) =>
    TerminalModel()..apply(
      Frame(
        flags: FrameFlags.full,
        cols: cols,
        rows: rows.length,
        defaultFg: 1,
        defaultBg: 0,
        cursorColor: 2,
        rowData: [
          for (final (i, runs) in rows.indexed)
            if (runs.isNotEmpty) FrameRow(index: i, runs: runs),
        ],
      ),
    );

TerminalModel _screen(List<String> lines, {int cols = 20}) => _model(cols, [
  for (final line in lines) [if (line.isNotEmpty) _run(0, line)],
]);

/// [text] cut into rows of [cols], the way a wrap lays it out.
List<String> _wrapped(String text, int cols) => [
  for (var i = 0; i < text.length; i += cols)
    text.substring(i, i + cols > text.length ? text.length : i + cols),
];

void main() {
  test('screen text keeps inner blank rows and drops trailing ones', () {
    final m = _screen(['one', '', 'three', '', '']);
    expect(screenText(m), 'one\n\nthree');
  });

  test('a login URL wrapped over many rows comes back whole', () {
    const url =
        'https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c250a'
        '&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com'
        '%2Foauth%2Fcode%2Fcallback&scope=org%3Acreate_api_key+user%3Aprofile'
        '&code_challenge=e8tW8CD6XOg8&code_challenge_method=S256&state=iH_2u';
    final m = _screen([
      "Browser didn't open? Use the url below to sign in",
      '',
      ..._wrapped(url, 51),
      '',
      'Paste code here if prompted >',
    ], cols: 51);
    expect(screenUrls(m), [url]);
  });

  test('a short URL at the end of a short row does not take the next row', () {
    final m = _screen(['see https://a.io.', 'Next steps'], cols: 40);
    expect(screenUrls(m), ['https://a.io']);
  });

  test('a URL ending a column short of the edge leaves the prompt alone', () {
    final m = _screen([
      'https://example.com/auth?code=abc123',
      r'me@box:~$',
    ], cols: 40);
    expect(screenUrls(m), ['https://example.com/auth?code=abc123']);
  });

  test('a next row that starts with a space is not more URL', () {
    final m = _screen([
      'Open https://example.com/settings/tokens',
      '  and paste the token below.',
    ], cols: 40);
    expect(screenUrls(m), ['https://example.com/settings/tokens']);
  });

  test('a list item at the edge does not take the next bullet', () {
    final m = _screen([
      '- https://example.com/releases/v0.1.0',
      '- https://example.com/releases/v0.1.1',
    ], cols: 37);
    expect(screenUrls(m), [
      'https://example.com/releases/v0.1.0',
      'https://example.com/releases/v0.1.1',
    ]);
  });

  test('a rule drawn after a URL at the edge is not part of it', () {
    final m = _screen([
      'see https://a.io/abcdefghijklmn',
      '─' * 31,
      '❯ ',
    ], cols: 31);
    expect(screenUrls(m), ['https://a.io/abcdefghijklmn']);
  });

  test('a wrap inside the scheme still finds the URL', () {
    final m = _screen([
      'Read the guide: http',
      's://example.com/docs',
    ], cols: 20);
    expect(screenUrls(m), ['https://example.com/docs']);
  });

  test('a wrap inside a URL carried in another stays one URL', () {
    const url = 'https://web.archive.org/web/2020/https://example.com/a';
    final m = _screen(_wrapped(url, 34), cols: 34);
    expect(screenUrls(m), [url]);
  });

  test('a wide character wrapped off the last cell keeps the URL whole', () {
    final m = _model(20, [
      [
        _run(0, 'https://a'),
        _wide(9, '例'),
        _wide(11, 'え'),
        _run(13, '.jp/'),
        _wide(17, 'パ'),
      ],
      [_wide(0, 'ス'), _run(2, '/x')],
    ]);
    expect(screenUrls(m), ['https://a例え.jp/パス/x']);
  });

  test('URLs one per line at the edge stay separate', () {
    final m = _screen([
      'https://github.com/charliek/roost/pull/1',
      'https://github.com/charliek/roost/pull/2',
    ], cols: 40);
    expect(screenUrls(m), [
      'https://github.com/charliek/roost/pull/1',
      'https://github.com/charliek/roost/pull/2',
    ]);
  });

  test('wide characters count as two cells when finding the edge', () {
    final m = _model(30, [
      [
        for (final (i, glyph) in '请打开链接吧'.split('').indexed)
          _wide(i * 2, glyph),
        _run(12, ' https://a.io/abcd'),
      ],
      [_run(0, 'efgh/ijk')],
    ]);
    expect(screenUrls(m), ['https://a.io/abcdefgh/ijk']);
  });

  test('an emoji of many code units does not make a short row look full', () {
    final m = _model(40, [
      [
        _wide(0, '🧑‍💻'),
        _wide(2, '🧑‍💻'),
        _run(4, ' see https://example.com/a/b'),
      ],
      [_run(0, 'Then run the tests.')],
    ]);
    expect(screenUrls(m), ['https://example.com/a/b']);
  });

  test('a row that ends in trailing blank cells is not at the edge', () {
    final m = _model(20, [
      [_run(0, 'https://a.io/x      ')],
      [_run(0, 'tail')],
    ]);
    expect(screenUrls(m), ['https://a.io/x']);
  });

  test('a URL one cell short does not run into CJK on the next row', () {
    final m = _model(40, [
      [
        _wide(0, '詳'),
        _wide(2, '細'),
        _run(4, ': https://example.com/docs/setup/ab'),
      ],
      [_wide(0, '次'), _wide(2, 'に'), _wide(4, '設'), _wide(6, '定')],
    ]);
    expect(screenUrls(m), ['https://example.com/docs/setup/ab']);
  });

  test('Markdown around a URL is not part of it', () {
    final m = _screen([
      '[https://x.io/a](https://x.io/a)',
      'See **https://x.io/b** now',
      '[docs](https://x.io/c).',
    ], cols: 40);
    expect(screenUrls(m), [
      'https://x.io/a',
      'https://x.io/b',
      'https://x.io/c',
    ]);
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

  test('a URL later on a continuation row is found on its own', () {
    final m = _screen([
      'https://a.io/abcdefg',
      'hij see https://b.io',
    ], cols: 20);
    expect(screenUrls(m), ['https://a.io/abcdefghij', 'https://b.io']);
  });
}
