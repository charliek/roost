import 'dart:math' as math;
import 'dart:ui' as ui;

import 'package:flutter/foundation.dart';
import 'package:flutter/painting.dart';

import '../core/models.dart';

const terminalFontFamily = 'JetBrains Mono';

/// Cells are a grid: ligatures would merge glyphs across cell boundaries.
const _terminalFontFeatures = [
  ui.FontFeature.disable('liga'),
  ui.FontFeature.disable('calt'),
];

/// Every run paragraph shares one strut, so a fallback glyph (an emoji, a
/// CJK face) cannot make its row taller than the cell.
ui.ParagraphStyle terminalParagraphStyle(double fontSize) => ui.ParagraphStyle(
  fontFamily: terminalFontFamily,
  fontSize: fontSize,
  maxLines: 1,
  textDirection: ui.TextDirection.ltr,
  strutStyle: ui.StrutStyle(
    fontFamily: terminalFontFamily,
    fontSize: fontSize,
    forceStrutHeight: true,
  ),
);

ui.TextStyle terminalTextStyle({
  required double fontSize,
  required Color color,
  bool bold = false,
  bool italic = false,
  bool underline = false,
  bool strike = false,
}) => ui.TextStyle(
  color: color,
  fontFamily: terminalFontFamily,
  fontSize: fontSize,
  fontWeight: bold ? FontWeight.w700 : FontWeight.w400,
  fontStyle: italic ? FontStyle.italic : FontStyle.normal,
  fontFeatures: _terminalFontFeatures,
  decoration: TextDecoration.combine([
    if (underline) TextDecoration.underline,
    if (strike) TextDecoration.lineThrough,
  ]),
  decorationColor: color,
);

ui.Paragraph layoutTerminalText(
  String text,
  ui.TextStyle style,
  double fontSize,
) {
  final builder = ui.ParagraphBuilder(terminalParagraphStyle(fontSize))
    ..pushStyle(style)
    ..addText(text);
  return builder.build()
    ..layout(const ui.ParagraphConstraints(width: double.infinity));
}

/// One cell's size in logical pixels for a font size, measured from the font.
@immutable
class CellMetrics {
  const CellMetrics({
    required this.fontSize,
    required this.cellWidth,
    required this.cellHeight,
    required this.devicePixelRatio,
  });

  /// Width comes from the font's advance (fractional, so long runs stay on
  /// the grid); height is rounded up to whole device pixels so rows tile
  /// without seams.
  factory CellMetrics.measure(double fontSize, double devicePixelRatio) {
    const sample = 'MMMMMMMMMM';
    final p = layoutTerminalText(
      sample,
      terminalTextStyle(fontSize: fontSize, color: const Color(0xFFFFFFFF)),
      fontSize,
    );
    final width = p.maxIntrinsicWidth / sample.length;
    final height = p.height;
    p.dispose();
    return CellMetrics(
      fontSize: fontSize,
      cellWidth: width,
      cellHeight: (height * devicePixelRatio).ceilToDouble() / devicePixelRatio,
      devicePixelRatio: devicePixelRatio,
    );
  }

  final double fontSize;
  final double cellWidth;
  final double cellHeight;
  final double devicePixelRatio;

  int get cellWidthPx => math.max(1, (cellWidth * devicePixelRatio).round());
  int get cellHeightPx => math.max(1, (cellHeight * devicePixelRatio).round());

  /// The grid that fits [size] at this font size.
  Geometry geometryFor(Size size) => Geometry(
    cols: math.max(1, (size.width / cellWidth).floor()),
    rows: math.max(1, (size.height / cellHeight).floor()),
    cellWidthPx: cellWidthPx,
    cellHeightPx: cellHeightPx,
  );

  /// How much a [cols]×[rows] grid shrinks to fit [size]: 1 when it fits,
  /// less when another client sized the tab larger than this screen.
  double scaleToFit(int cols, int rows, Size size) {
    final w = cols * cellWidth;
    final h = rows * cellHeight;
    if (w <= 0 || h <= 0) return 1;
    return math.min(1, math.min(size.width / w, size.height / h));
  }

  @override
  bool operator ==(Object other) =>
      other is CellMetrics &&
      other.fontSize == fontSize &&
      other.cellWidth == cellWidth &&
      other.cellHeight == cellHeight &&
      other.devicePixelRatio == devicePixelRatio;

  @override
  int get hashCode =>
      Object.hash(fontSize, cellWidth, cellHeight, devicePixelRatio);
}
