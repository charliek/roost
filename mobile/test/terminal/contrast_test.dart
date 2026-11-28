import 'dart:ui';

import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/terminal/contrast.dart';

const _latte = Color(0xFFEFF1F5);

void main() {
  test('a pale gray on Latte fails, and is darkened to 3:1', () {
    const fg = Color(0xFFDDDDDD);
    expect(contrastRatio(fg, _latte), lessThan(1.3));
    final out = ensureContrast(fg, _latte, 3);
    expect(out, isNot(fg));
    expect(contrastRatio(out, _latte), greaterThanOrEqualTo(3));
  });

  test('light on dark passes and is kept; dark on dark is pushed to white', () {
    const bg = Color(0xFF1E1E2E);
    const light = Color(0xFFDDDDDD);
    expect(ensureContrast(light, bg, 3), light);
    const dim = Color(0xFF333344);
    final out = ensureContrast(dim, bg, 3);
    expect(contrastRatio(out, bg), greaterThanOrEqualTo(3));
    expect(wcagLuminance(out), greaterThan(wcagLuminance(dim)));
  });

  test('the faint blend is checked, not the opaque color', () {
    const fg = Color(0xFF202020);
    const bg = Color(0xFF181818);
    final blended = Color.alphaBlend(fg.withValues(alpha: 0.55), bg);
    expect(contrastRatio(fg, bg), lessThan(3));
    final out = ensureContrast(blended, bg, 3);
    expect(contrastRatio(out, bg), greaterThanOrEqualTo(3));
  });

  test('the 8-bit result reaches the ratio where the float one missed', () {
    const fg = Color(0xFF121212);
    const bg = Color(0xFF000000);
    final out = ensureContrast(fg, bg, 3);
    expect(contrastRatio(out, bg), greaterThanOrEqualTo(3));
  });

  test('text the color of its background stays hidden', () {
    expect(ensureContrast(_latte, _latte, 3), _latte);
  });

  test('every color on every background reaches the ratio', () {
    const steps = [0, 0x40, 0x80, 0xc0, 0xff];
    final colors = [
      for (final r in steps)
        for (final g in steps)
          for (final b in steps) Color.fromARGB(255, r, g, b),
    ];
    for (final bg in colors) {
      for (final fg in colors) {
        final out = ensureContrast(fg, bg, 3);
        // The ink is drawn as 8-bit color.
        expect(out, Color(out.toARGB32()));
        if (fg == bg) {
          expect(out, fg);
        } else {
          expect(
            contrastRatio(out, bg),
            greaterThanOrEqualTo(3),
            reason: '$fg on $bg',
          );
        }
      }
    }
  });
}
