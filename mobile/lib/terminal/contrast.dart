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

bool isLightBackground(Color bg) => wcagLuminance(bg) > 0.5;

/// How the painter keeps text legible: the contrast it raises text to
/// (1.0 draws colors as they are), whether only on a light background, and
/// the alpha faint text is drawn at.
class TextReadability {
  const TextReadability({
    this.minimumContrast = 1.0,
    this.onlyOnLight = false,
    this.faintAlpha = 0.55,
  });

  final double minimumContrast;

  /// Decided from the background of the frame being painted, not from the
  /// theme the core acknowledged, which can change a frame before the
  /// recolored one arrives.
  final bool onlyOnLight;

  final double faintAlpha;

  /// The contrast text is raised to on a background of [bg].
  double minimumContrastOn(Color bg) =>
      onlyOnLight && !isLightBackground(bg) ? 1.0 : minimumContrast;

  @override
  bool operator ==(Object other) =>
      other is TextReadability &&
      other.minimumContrast == minimumContrast &&
      other.onlyOnLight == onlyOnLight &&
      other.faintAlpha == faintAlpha;

  @override
  int get hashCode => Object.hash(minimumContrast, onlyOnLight, faintAlpha);
}

/// [fg], moved toward black or white until it reaches [minRatio] against
/// [bg]. Text that already passes, or that is the background's own color
/// (hidden on purpose), is left alone. Some hue is lost when only the pole
/// itself passes. Every step is an 8-bit color, as it will be drawn.
Color ensureContrast(Color fg, Color bg, double minRatio) {
  fg = Color(fg.toARGB32());
  if (fg == bg || contrastRatio(fg, bg) >= minRatio) return fg;
  const black = Color(0xFF000000);
  const white = Color(0xFFFFFFFF);
  final pole = contrastRatio(black, bg) >= contrastRatio(white, bg)
      ? black
      : white;
  for (var step = 1; step <= 20; step++) {
    final c = Color(Color.lerp(fg, pole, step / 20)!.toARGB32());
    if (contrastRatio(c, bg) >= minRatio) return c;
  }
  return pole;
}
