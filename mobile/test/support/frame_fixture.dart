import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/terminal/frame.dart';

/// A `frame_v1_*.bin` payload and its `.json` expectation.
typedef FrameFixture = ({String name, File bin, Map<String, Object?> json});

/// Every `frame_v1_*.bin` in [dir] that has a `.json` sibling.
List<FrameFixture> findFrameFixtures(Directory dir) {
  if (!dir.existsSync()) return const [];
  final bins = dir.listSync().whereType<File>().where((f) {
    final name = f.uri.pathSegments.last;
    return name.startsWith('frame_v1_') && name.endsWith('.bin');
  }).toList()..sort((a, b) => a.path.compareTo(b.path));
  return [
    for (final bin in bins)
      if (File(_jsonPath(bin.path)).existsSync())
        (
          name: bin.uri.pathSegments.last,
          bin: bin,
          json: jsonDecode(
            File(_jsonPath(bin.path)).readAsStringSync(),
          ) as Map<String, Object?>,
        ),
  ];
}

String _jsonPath(String binPath) =>
    '${binPath.substring(0, binPath.length - '.bin'.length)}.json';

/// The one place that reads the fixture JSON the Rust test writes. It is
/// tolerant on purpose (alternate key names, colors as ints or "#rrggbb",
/// enums as ints or names) so a shape drift on the Rust side is a one-line
/// fix here, not a rewrite of the test.
Frame frameFromFixtureJson(Map<String, Object?> json) {
  Object? pick(Map<String, Object?> m, List<String> keys) {
    for (final k in keys) {
      if (m.containsKey(k)) return m[k];
    }
    return null;
  }

  int intOf(Object? v, {int fallback = 0}) => switch (v) {
    null => fallback,
    final int i => i,
    final double d => d.toInt(),
    final bool b => b ? 1 : 0,
    final String s when s.startsWith('#') => int.parse(
      s.substring(1),
      radix: 16,
    ),
    final String s when s.startsWith('0x') => int.parse(
      s.substring(2),
      radix: 16,
    ),
    final String s => int.parse(s),
    _ => throw FormatException('not an int: $v'),
  };

  Map<String, Object?> mapOf(Object? v) =>
      v is Map ? v.cast<String, Object?>() : const {};

  int bitsOf(Object? v, Map<String, int> names) {
    if (v is List) {
      return v.fold(0, (acc, n) {
        final bit = names['$n'.toLowerCase()];
        if (bit == null) throw FormatException('unknown flag $n');
        return acc | bit;
      });
    }
    return intOf(v);
  }

  const flagNames = {
    'full': FrameFlags.full,
    'cursor_visible': FrameFlags.cursorVisible,
    'mouse_tracking': FrameFlags.mouseTracking,
    'alt_screen': FrameFlags.altScreen,
    'bracketed_paste': FrameFlags.bracketedPaste,
  };
  const attrNames = {
    'bold': RunAttrs.bold,
    'italic': RunAttrs.italic,
    'underline': RunAttrs.underline,
    'strike': RunAttrs.strike,
    'strikethrough': RunAttrs.strike,
    'faint': RunAttrs.faint,
    'explicit_bg': RunAttrs.explicitBg,
    'wide': RunAttrs.wide,
  };

  CursorStyle styleOf(Object? v) {
    if (v is String && int.tryParse(v) == null) {
      final key = v.toLowerCase().replaceAll('_', '');
      return CursorStyle.values.firstWhere(
        (s) => s.name.toLowerCase() == key,
        orElse: () => throw FormatException('unknown cursor style $v'),
      );
    }
    return CursorStyle.fromWire(intOf(v));
  }

  final cursor = mapOf(json['cursor']);
  final scroll = mapOf(json['scroll'] ?? json['scrollbar']);
  final rowList =
      (pick(json, ['rows_data', 'row_data', 'dirty_rows', 'lines']) as List?) ??
      const [];
  return Frame(
    flags: bitsOf(json['flags'], flagNames),
    cols: intOf(json['cols']),
    rows: intOf(json['rows']),
    defaultFg: intOf(pick(json, ['default_fg', 'fg'])),
    defaultBg: intOf(pick(json, ['default_bg', 'bg'])),
    cursorColor: intOf(json['cursor_color'] ?? cursor['color']),
    cursorCol: intOf(pick(cursor, ['col', 'x']) ?? json['cursor_col']),
    cursorRow: intOf(pick(cursor, ['row', 'y']) ?? json['cursor_row']),
    cursorStyle: styleOf(cursor['style'] ?? json['cursor_style']),
    scrollTotal: intOf(scroll['total'] ?? json['scroll_total']),
    scrollOffset: intOf(scroll['offset'] ?? json['scroll_offset']),
    scrollLen: intOf(pick(scroll, ['len', 'length']) ?? json['scroll_len']),
    rowData: [
      for (final r in rowList.map(mapOf))
        FrameRow(
          index: intOf(pick(r, ['row', 'index', 'row_index'])),
          runs: [
            for (final run in ((r['runs'] as List?) ?? const []).map(mapOf))
              FrameRun(
                col: intOf(run['col']),
                cells: intOf(pick(run, ['cells', 'width', 'len'])),
                fg: intOf(run['fg']),
                bg: intOf(run['bg']),
                attrs: bitsOf(run['attrs'], attrNames),
                text: (run['text'] as String?) ?? '',
              ),
          ],
        ),
    ],
  );
}

/// Decodes [fixture] and compares it field by field with its JSON.
void expectFixtureDecodes(FrameFixture fixture) {
  final decoded = decodeFrame(fixture.bin.readAsBytesSync());
  final expected = frameFromFixtureJson(fixture.json);
  expect(decoded.cols, expected.cols, reason: 'cols');
  expect(decoded.rows, expected.rows, reason: 'rows');
  expect(decoded.flags, expected.flags, reason: 'flags');
  expect(decoded.defaultFg, expected.defaultFg, reason: 'default_fg');
  expect(decoded.defaultBg, expected.defaultBg, reason: 'default_bg');
  expect(decoded.cursorColor, expected.cursorColor, reason: 'cursor_color');
  expect(decoded.cursorCol, expected.cursorCol, reason: 'cursor.col');
  expect(decoded.cursorRow, expected.cursorRow, reason: 'cursor.row');
  expect(decoded.cursorStyle, expected.cursorStyle, reason: 'cursor.style');
  expect(decoded.scrollTotal, expected.scrollTotal, reason: 'scroll.total');
  expect(decoded.scrollOffset, expected.scrollOffset, reason: 'scroll.offset');
  expect(decoded.scrollLen, expected.scrollLen, reason: 'scroll.len');
  expect(decoded.rowData, expected.rowData, reason: 'rows_data');
}
