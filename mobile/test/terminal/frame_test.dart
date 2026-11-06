import 'dart:io';
import 'dart:typed_data';

import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/terminal/frame.dart';
import 'package:roost_mobile/terminal/frame_encoder.dart';

import '../support/frame_fixture.dart';

const _fg = 0xd7dae0;
const _bg = 0x0f1115;

Frame _sample({int flags = FrameFlags.full | FrameFlags.cursorVisible}) =>
    Frame(
      flags: flags,
      cols: 80,
      rows: 24,
      defaultFg: _fg,
      defaultBg: _bg,
      cursorColor: 0xe6e8ee,
      cursorCol: 17,
      cursorRow: 23,
      cursorStyle: CursorStyle.underline,
      scrollTotal: 1024,
      scrollOffset: 1000,
      scrollLen: 24,
      rowData: const [
        FrameRow(
          index: 0,
          runs: [
            FrameRun(
              col: 0,
              cells: 2,
              fg: 0x79aaff,
              bg: _bg,
              attrs: RunAttrs.bold,
              text: '~/',
            ),
            FrameRun(col: 3, cells: 4, fg: _fg, bg: _bg, text: 'ls -l'),
          ],
        ),
        FrameRow(
          index: 7,
          runs: [
            FrameRun(
              col: 4,
              cells: 2,
              fg: 0xf2a65a,
              bg: _bg,
              attrs: RunAttrs.wide,
              text: '漢',
            ),
            FrameRun(
              col: 6,
              cells: 6,
              fg: _bg,
              bg: _fg,
              attrs: RunAttrs.explicitBg | RunAttrs.italic | RunAttrs.underline,
              text: 'émoji❯',
            ),
          ],
        ),
        FrameRow(index: 23, runs: []),
      ],
    );

void main() {
  group('round trip through the Dart encoder', () {
    test('a full frame with every field set', () {
      final frame = _sample(
        flags:
            FrameFlags.full |
            FrameFlags.cursorVisible |
            FrameFlags.mouseTracking |
            FrameFlags.altScreen |
            FrameFlags.bracketedPaste,
      );
      expect(decodeFrame(encodeFrame(frame)), frame);
    });

    test('an empty delta: header only', () {
      const frame = Frame(
        cols: 1,
        rows: 1,
        defaultFg: 0,
        defaultBg: 0,
        cursorColor: 0,
      );
      final bytes = encodeFrame(frame);
      expect(bytes.length, frameHeaderLength);
      expect(decodeFrame(bytes), frame);
    });

    test('field maxima survive (u16 grid, u32 colors and scroll)', () {
      const frame = Frame(
        cols: 0xffff,
        rows: 0xffff,
        defaultFg: 0xffffff,
        defaultBg: 0,
        cursorColor: 0xabcdef,
        cursorCol: 0xfffe,
        cursorRow: 0xfffd,
        cursorStyle: CursorStyle.blockHollow,
        scrollTotal: 0xffffffff,
        scrollOffset: 0xfffffff0,
        scrollLen: 0xffff,
      );
      expect(decodeFrame(encodeFrame(frame)), frame);
    });

    test('run text is UTF-8 with its byte length, not its char count', () {
      const frame = Frame(
        cols: 10,
        rows: 1,
        defaultFg: _fg,
        defaultBg: _bg,
        cursorColor: 0,
        rowData: [
          FrameRow(
            index: 0,
            runs: [
              FrameRun(
                col: 0,
                cells: 2,
                fg: _fg,
                bg: _bg,
                attrs: RunAttrs.wide,
                text: '🦀',
              ),
              FrameRun(col: 2, cells: 3, fg: _fg, bg: _bg, text: 'añb'),
            ],
          ),
        ],
      );
      final bytes = encodeFrame(frame);
      // 🦀 is 4 UTF-8 bytes, ñ is 2.
      expect(bytes.length, frameHeaderLength + 4 + 2 * 15 + 4 + 4);
      expect(decodeFrame(bytes), frame);
    });
  });

  group('wire layout', () {
    test('decodes bytes assembled by hand from the C6 table', () {
      final bytes = Uint8List.fromList([
        0x01, // version
        0x13, // flags: FULL | CURSOR_VISIBLE | BRACKETED_PASTE
        0x0a, 0x00, // cols 10
        0x02, 0x00, // rows 2
        0x33, 0x22, 0x11, 0x00, // default_fg 0x112233
        0x66, 0x55, 0x44, 0x00, // default_bg 0x445566
        0x99, 0x88, 0x77, 0x00, // cursor_color 0x778899
        0x03, 0x00, // cursor_col 3
        0x01, 0x00, // cursor_row 1
        0x02, // cursor_style underline
        0x2c, 0x01, 0x00, 0x00, // scroll_total 300
        0x2a, 0x01, 0x00, 0x00, // scroll_offset 298
        0x02, 0x00, 0x00, 0x00, // scroll_len 2
        0x01, 0x00, // row_count 1
        0x01, 0x00, // row_index 1
        0x01, 0x00, // run_count 1
        0x02, 0x00, // col 2
        0x02, 0x00, // cells 2
        0x00, 0x00, 0xff, 0x00, // fg 0xff0000
        0x00, 0xff, 0x00, 0x00, // bg 0x00ff00
        0x21, // attrs bold | explicit_bg
        0x02, 0x00, // text_len 2
        0x68, 0x69, // "hi"
      ]);
      final frame = decodeFrame(bytes);
      expect(frame.flags, 0x13);
      expect(frame.isFull, isTrue);
      expect(frame.has(FrameFlags.bracketedPaste), isTrue);
      expect(frame.has(FrameFlags.mouseTracking), isFalse);
      expect((frame.cols, frame.rows), (10, 2));
      expect(frame.defaultFg, 0x112233);
      expect(frame.defaultBg, 0x445566);
      expect(frame.cursorColor, 0x778899);
      expect((frame.cursorCol, frame.cursorRow), (3, 1));
      expect(frame.cursorStyle, CursorStyle.underline);
      expect(
        (frame.scrollTotal, frame.scrollOffset, frame.scrollLen),
        (300, 298, 2),
      );
      expect(frame.rowData, const [
        FrameRow(
          index: 1,
          runs: [
            FrameRun(
              col: 2,
              cells: 2,
              fg: 0xff0000,
              bg: 0x00ff00,
              attrs: RunAttrs.bold | RunAttrs.explicitBg,
              text: 'hi',
            ),
          ],
        ),
      ]);
      // The Dart encoder writes exactly these bytes back.
      expect(encodeFrame(frame), bytes);
    });

    test('an unknown cursor style reads as block', () {
      final bytes = encodeFrame(_sample());
      bytes[22] = 9; // cursor_style
      expect(decodeFrame(bytes).cursorStyle, CursorStyle.block);
    });

    test('a different version is refused', () {
      final bytes = encodeFrame(_sample())..[0] = 2;
      expect(() => decodeFrame(bytes), throwsA(isA<FrameFormatException>()));
    });

    test('every truncation is refused, never misread', () {
      final bytes = encodeFrame(_sample());
      for (var n = 0; n < bytes.length; n++) {
        expect(
          () => decodeFrame(Uint8List.sublistView(bytes, 0, n)),
          throwsA(isA<FrameFormatException>()),
          reason: 'cut at $n of ${bytes.length}',
        );
      }
    });
  });

  group('fixtures', () {
    final fixtures = findFrameFixtures(Directory('test/fixtures'));

    test('the hand-assembled fixture is present', () {
      expect(fixtures.map((f) => f.name), contains('frame_v1_handmade.bin'));
    });

    for (final fixture in fixtures) {
      test('${fixture.name} decodes to its .json', () {
        expectFixtureDecodes(fixture);
      });
    }

    test('the comparison fails on a fixture that does not match', () {
      final handmade = fixtures.firstWhere(
        (f) => f.name == 'frame_v1_handmade.bin',
      );
      final wrong = Map<String, Object?>.of(handmade.json)..['cols'] = 21;
      expect(
        () => expectFixtureDecodes((
          name: 'wrong',
          bin: handmade.bin,
          json: wrong,
        )),
        throwsA(isA<TestFailure>()),
      );
    });

    test('the JSON reader takes names, hex strings and alternate keys', () {
      final frame = frameFromFixtureJson({
        'cols': 4,
        'rows': 1,
        'flags': ['full', 'cursor_visible'],
        'default_fg': '#d7dae0',
        'default_bg': '0x0f1115',
        'cursor': {'x': 1, 'y': 0, 'style': 'block_hollow', 'color': 1},
        'scrollbar': {'total': 1, 'offset': 0, 'length': 1},
        'row_data': [
          {
            'index': 0,
            'runs': [
              {
                'col': 0,
                'width': 2,
                'fg': 1,
                'bg': 2,
                'attrs': ['bold', 'wide'],
                'text': '漢',
              },
            ],
          },
        ],
      });
      expect(frame.flags, FrameFlags.full | FrameFlags.cursorVisible);
      expect(frame.defaultFg, 0xd7dae0);
      expect(frame.defaultBg, 0x0f1115);
      expect(frame.cursorStyle, CursorStyle.blockHollow);
      expect(frame.cursorColor, 1);
      expect(frame.scrollLen, 1);
      expect(
        frame.rowData.single.runs.single.attrs,
        RunAttrs.bold | RunAttrs.wide,
      );
      expect(frame.rowData.single.runs.single.cells, 2);
    });
  });
}
