import 'dart:convert';
import 'dart:typed_data';

import 'frame.dart';

/// Encodes a [Frame] as a C6 v1 payload: the Dart twin of `roost-mobile`'s
/// encoder, used by the fake core and the decoder's round-trip tests.
Uint8List encodeFrame(Frame frame) {
  final out = BytesBuilder(copy: false);
  final header = ByteData(frameHeaderLength);
  var at = 0;
  void u8(int v) => header.setUint8(at++, v);
  void u16(int v) {
    header.setUint16(at, v, Endian.little);
    at += 2;
  }

  void u32(int v) {
    header.setUint32(at, v, Endian.little);
    at += 4;
  }

  u8(frameVersion);
  u8(frame.flags);
  u16(frame.cols);
  u16(frame.rows);
  u32(frame.defaultFg);
  u32(frame.defaultBg);
  u32(frame.cursorColor);
  u16(frame.cursorCol);
  u16(frame.cursorRow);
  u8(frame.cursorStyle.index);
  u32(frame.scrollTotal);
  u32(frame.scrollOffset);
  u32(frame.scrollLen);
  u16(frame.rowData.length);
  out.add(header.buffer.asUint8List());

  for (final row in frame.rowData) {
    final rowHead = ByteData(4)
      ..setUint16(0, row.index, Endian.little)
      ..setUint16(2, row.runs.length, Endian.little);
    out.add(rowHead.buffer.asUint8List());
    for (final run in row.runs) {
      final text = utf8.encode(run.text);
      final runHead = ByteData(15)
        ..setUint16(0, run.col, Endian.little)
        ..setUint16(2, run.cells, Endian.little)
        ..setUint32(4, run.fg, Endian.little)
        ..setUint32(8, run.bg, Endian.little)
        ..setUint8(12, run.attrs)
        ..setUint16(13, text.length, Endian.little);
      out
        ..add(runHead.buffer.asUint8List())
        ..add(text);
    }
  }
  return out.toBytes();
}
