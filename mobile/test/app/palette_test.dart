import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/app/theme.dart';

void main() {
  test('the dark palette keeps the former constants and literals', () {
    const p = RoostPalette.dark;
    expect(p.background, const Color(0xFF0F1115));
    expect(p.surfaceLow, const Color(0xFF121419));
    expect(p.surface, const Color(0xFF161920));
    expect(p.surfaceHigh, const Color(0xFF181B21));
    expect(p.surfaceHighest, const Color(0xFF1D2027));
    expect(p.border, const Color(0xFF2B2F38));
    expect(p.divider, const Color(0xFF23262E));
    expect(p.outline, const Color(0xFF2F343E));
    expect(p.text, const Color(0xFFE6E8EE));
    expect(p.muted, const Color(0xFF9AA3B2));
    expect(p.waiting, const Color(0xFFF2A65A));
    expect(p.waitingText, const Color(0xFFF6D2A8));
    expect(p.waitingAction, const Color(0xFFF2A65A));
    expect(p.waitingSurface, const Color(0xFF17130D));
    expect(p.waitingBorder, const Color(0xFF4A3A26));
    expect(p.waitingPill, const Color(0xFF3A2A17));
    expect(p.working, const Color(0xFF79AAFF));
    expect(p.workingPill, const Color(0xFF172338));
    expect(p.idle, const Color(0xFF5C6370));
    expect(p.connected, const Color(0xFF5FC38F));
    expect(p.error, const Color(0xFFF08A8A));
    expect(p.errorSurface, const Color(0xFF1A1213));
    expect(p.errorBorder, const Color(0xFF5A2F2F));
  });

  test('roostTheme registers the palette and copyWith/lerp work', () {
    final theme = roostTheme(Brightness.dark);
    expect(theme.extension<RoostPalette>(), RoostPalette.dark);
    final changed = RoostPalette.dark.copyWith(text: const Color(0xFF000000));
    expect(changed.text, const Color(0xFF000000));
    expect(changed.muted, RoostPalette.dark.muted);
    expect(RoostPalette.dark.lerp(changed, 0).text, RoostPalette.dark.text);
    expect(RoostPalette.dark.lerp(changed, 1).text, const Color(0xFF000000));
  });
}
