import 'dart:math' as math;

import 'package:flutter/widgets.dart';

/// The range and step of the app-text slider.
const minUiTextScale = 0.85;
const maxUiTextScale = 1.6;
const uiTextScaleStep = 0.05;

/// A bar that scrolls sideways needs a finite height, so it grows by what the
/// user's text scale adds to its label; at 1.0 it is exactly [base].
double scaledBarHeight(BuildContext context, double base, double labelSize) {
  final scaler = MediaQuery.textScalerOf(context);
  return (base + (scaler.scale(labelSize) - labelSize)).ceilToDouble();
}

/// The app's own text size on top of the system's, which may be nonlinear:
/// the system's size times [factor], and never more than twice the font size,
/// Android's own maximum (a larger system setting is capped on purpose).
@immutable
class ScaledTextScaler extends TextScaler {
  const ScaledTextScaler._(this.base, this.factor);

  /// [base] itself when the result would be the same, so default rendering
  /// is untouched.
  static TextScaler over(TextScaler base, double factor) =>
      factor == 1.0 && base.scale(14) <= 28
      ? base
      : ScaledTextScaler._(base, factor);

  final TextScaler base;
  final double factor;

  @override
  double scale(double fontSize) =>
      math.min(base.scale(fontSize) * factor, fontSize * 2.0);

  /// An estimate: a nonlinear scale has no single factor, so this is the
  /// one at 14 pt, body text.
  @override
  double get textScaleFactor => scale(14) / 14;

  @override
  bool operator ==(Object other) =>
      other is ScaledTextScaler && other.base == base && other.factor == factor;

  @override
  int get hashCode => Object.hash(base, factor);

  @override
  String toString() => 'ScaledTextScaler($base × $factor)';
}
