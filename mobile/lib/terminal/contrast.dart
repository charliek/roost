import 'dart:math' as math;
import 'dart:ui';

/// WCAG 2 relative luminance of an sRGB color.
double wcagLuminance(Color color) {
  double channel(double c) =>
      c <= 0.04045 ? c / 12.92 : math.pow((c + 0.055) / 1.055, 2.4).toDouble();
  return 0.2126 * channel(color.r) +
      0.7152 * channel(color.g) +
      0.0722 * channel(color.b);
}

/// WCAG 2 contrast ratio, 1.0 to 21.0, whichever way round the colors are.
double contrastRatio(Color a, Color b) {
  final la = wcagLuminance(a);
  final lb = wcagLuminance(b);
  return (math.max(la, lb) + 0.05) / (math.min(la, lb) + 0.05);
}
