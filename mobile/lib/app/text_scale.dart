import 'package:flutter/widgets.dart';

/// A bar that scrolls sideways needs a finite height, so it grows by what the
/// user's text scale adds to its label; at 1.0 it is exactly [base].
double scaledBarHeight(BuildContext context, double base, double labelSize) {
  final scaler = MediaQuery.textScalerOf(context);
  return (base + (scaler.scale(labelSize) - labelSize)).ceilToDouble();
}
