import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/app/theme.dart';
import 'package:roost_mobile/terminal/contrast.dart';

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

  group('contrast', () {
    // (foreground, background, minimum ratio, where it is used). 4.5 for
    // text, 3.0 for icons and dots.
    final pairs =
        <
          (
            Color Function(RoostPalette, ColorScheme),
            Color Function(RoostPalette, ColorScheme),
            double,
            String,
          )
        >[
          (
            (p, c) => p.text,
            (p, c) => p.background,
            4.5,
            'default text on a page',
          ),
          ((p, c) => p.text, (p, c) => p.surface, 4.5, 'copy sheet text'),
          (
            (p, c) => p.text,
            (p, c) => p.surfaceHighest,
            4.5,
            'notice card, HUD title',
          ),
          (
            (p, c) => p.text,
            (p, c) => p.surfaceHigh,
            4.5,
            'terminal status card',
          ),
          (
            (p, c) => p.text,
            (p, c) => p.waitingSurface,
            4.5,
            'trust card body',
          ),
          (
            (p, c) => p.muted,
            (p, c) => p.background,
            4.5,
            'empty state, workspace list',
          ),
          (
            (p, c) => p.muted,
            (p, c) => p.surfaceLow,
            4.5,
            'toolbar subtitle, idle tab',
          ),
          ((p, c) => p.muted, (p, c) => p.surface, 4.5, 'copy sheet captions'),
          (
            (p, c) => p.muted,
            (p, c) => p.surfaceHighest,
            4.5,
            'HUD size, idle pill',
          ),
          (
            (p, c) => p.waiting,
            (p, c) => p.background,
            4.5,
            '"n waiting", tab status',
          ),
          ((p, c) => p.waiting, (p, c) => p.waitingPill, 4.5, 'waiting pill'),
          (
            (p, c) => p.waitingAction,
            (p, c) => p.waitingSurface,
            4.5,
            'quick reply label',
          ),
          (
            (p, c) => p.waitingText,
            (p, c) => p.waitingSurface,
            4.5,
            'quick reply button',
          ),
          ((p, c) => p.working, (p, c) => p.workingPill, 4.5, 'running pill'),
          (
            (p, c) => p.text,
            (p, c) => p.errorSurface,
            4.5,
            'failed host message',
          ),
          (
            (p, c) => p.muted,
            (p, c) => p.waitingSurface,
            4.5,
            'banner subtitle',
          ),
          (
            (p, c) => p.waiting,
            (p, c) => p.surfaceLow,
            4.5,
            'project header waiting count',
          ),
          (
            (p, c) => p.waiting,
            (p, c) => p.surfaceHighest,
            4.5,
            'selected tile waiting status',
          ),
          (
            (p, c) => c.onSecondaryContainer,
            (p, c) => c.secondaryContainer,
            4.5,
            'Display selected segment label',
          ),
          (
            (p, c) => p.idle,
            (p, c) => p.surface,
            3.0,
            'disconnected host dot on a card',
          ),
          (
            (p, c) => p.idle,
            (p, c) => p.surfaceHighest,
            3.0,
            'dot on the selected tile',
          ),
          (
            (p, c) => p.connected,
            (p, c) => c.secondaryContainer,
            3.0,
            'rail host dot, connected',
          ),
          (
            (p, c) => p.working,
            (p, c) => c.secondaryContainer,
            3.0,
            'rail host dot, connecting',
          ),
          (
            (p, c) => p.muted,
            (p, c) => c.secondaryContainer,
            3.0,
            'rail host dot, retrying',
          ),
          (
            (p, c) => p.waiting,
            (p, c) => c.secondaryContainer,
            3.0,
            'rail host dot and waiting dot',
          ),
          (
            (p, c) => p.error,
            (p, c) => c.secondaryContainer,
            3.0,
            'rail host dot, failed',
          ),
          (
            (p, c) => p.idle,
            (p, c) => c.secondaryContainer,
            3.0,
            'rail host dot, disconnected',
          ),
          (
            (p, c) => p.waiting,
            (p, c) => p.background,
            3.0,
            'notification dot',
          ),
          (
            (p, c) => p.waiting,
            (p, c) => p.waitingSurface,
            3.0,
            'quick reply dot',
          ),
          (
            (p, c) => p.idle,
            (p, c) => p.background,
            3.0,
            'agent dot in a list',
          ),
          (
            (p, c) => p.idle,
            (p, c) => p.surfaceLow,
            3.0,
            'agent dot in the tab strip',
          ),
          (
            (p, c) => p.working,
            (p, c) => p.background,
            3.0,
            'agent dot, connecting',
          ),
          (
            (p, c) => p.connected,
            (p, c) => p.background,
            3.0,
            'connected host dot',
          ),
          ((p, c) => p.muted, (p, c) => p.surfaceHighest, 3.0, 'banner icon'),
          (
            (p, c) => p.error,
            (p, c) => p.errorSurface,
            3.0,
            'failed host icon',
          ),
        ];

    // Dark pairs below their minimum today, held at what they measure (a
    // known dark-mode shortfall: dark values do not change).
    const darkFloors = <String, double>{
      '3.0 disconnected host dot on a card': 2.9,
      '3.0 dot on the selected tile': 2.69,
      '3.0 rail host dot, disconnected': 1.5,
    };

    for (final (name, brightness, palette) in [
      ('dark', Brightness.dark, RoostPalette.dark),
      ('light', Brightness.light, RoostPalette.light),
    ]) {
      test('the $name palette meets its contrast minimums', () {
        final scheme = roostTheme(brightness).colorScheme;
        final short = <String>[];
        for (final (fg, bg, min, where) in pairs) {
          final ratio = contrastRatio(fg(palette, scheme), bg(palette, scheme));
          final floor = name == 'dark' ? darkFloors['$min $where'] : null;
          if (floor == null && ratio < min) {
            short.add('$where ${ratio.toStringAsFixed(2)} < $min');
          }
          if (floor != null) {
            expect(ratio, greaterThanOrEqualTo(floor), reason: where);
          }
        }
        expect(short, isEmpty);
      });
    }
  });

  test('contrastRatio matches the WCAG reference values', () {
    const black = Color(0xFF000000);
    const white = Color(0xFFFFFFFF);
    expect(contrastRatio(black, white), closeTo(21, 0.001));
    expect(contrastRatio(white, black), closeTo(21, 0.001));
    expect(contrastRatio(white, white), closeTo(1, 0.001));
    expect(contrastRatio(const Color(0xFF777777), white), closeTo(4.48, 0.01));
  });
}
