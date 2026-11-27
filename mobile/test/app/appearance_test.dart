import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/app/providers.dart';
import 'package:roost_mobile/app/theme.dart';
import 'package:roost_mobile/core/fake/fake_core.dart';
import 'package:roost_mobile/core/models.dart';
import 'package:roost_mobile/terminal/terminal_pane.dart';

import '../support/pump_app.dart';

const _lightBackground = Color(0xFFF6F7F9);
const _darkBackground = Color(0xFF0F1115);

Future<FakeCore> _pump(
  WidgetTester tester, {
  required Appearance appearance,
  required Brightness platform,
}) async {
  tester.platformDispatcher.platformBrightnessTestValue = platform;
  addTearDown(tester.platformDispatcher.clearPlatformBrightnessTestValue);
  final core = await pumpApp(tester, width: 360);
  ProviderScope.containerOf(tester.element(find.byType(MaterialApp)))
      .read(settingsProvider.notifier)
      .edit((s) => s.copyWith(appearance: appearance));
  await tester.pumpAndSettle();
  return core;
}

Color _scaffold(WidgetTester tester) =>
    Theme.of(tester.element(find.byType(Scaffold).first))
        .scaffoldBackgroundColor;

Future<void> _flip(WidgetTester tester, Brightness platform) async {
  tester.platformDispatcher.platformBrightnessTestValue = platform;
  await tester.pumpAndSettle();
}

Future<void> _openTerminal(WidgetTester tester) async {
  await tester.tap(byKey('host-card-devbox'));
  await tester.pumpAndSettle();
  await tester.tap(byKey('tab-devbox-22'));
  await tester.pumpAndSettle();
  expect(find.byType(TerminalPane), findsOneWidget);
}

SystemUiOverlayStyle _styleAbove(WidgetTester tester, Finder of) => tester
    .widget<AnnotatedRegion<SystemUiOverlayStyle>>(
      find
          .ancestor(
            of: of,
            matching: find.byType(AnnotatedRegion<SystemUiOverlayStyle>),
          )
          .first,
    )
    .value;

void main() {
  testWidgets('Light appearance paints the light chrome', (tester) async {
    await _pump(
      tester,
      appearance: Appearance.light,
      platform: Brightness.dark,
    );
    expect(_scaffold(tester), _lightBackground);
  });

  testWidgets('System follows the platform and tells the core', (tester) async {
    final core = await _pump(
      tester,
      appearance: Appearance.system,
      platform: Brightness.light,
    );
    expect(_scaffold(tester), _lightBackground);
    expect(core.systemDarkCalls.last, isFalse);

    await _flip(tester, Brightness.dark);
    expect(_scaffold(tester), _darkBackground);
    expect(core.systemDarkCalls.last, isTrue);
    expect(core.systemDark, isTrue);

    await _flip(tester, Brightness.light);
    expect(_scaffold(tester), _lightBackground);
    expect(core.systemDark, isFalse);
  });

  testWidgets('the launch reconciles a core started with a stale brightness', (
    tester,
  ) async {
    // main told the core "dark"; the platform is light by initState.
    final core = FakeCore(systemDark: true);
    await core.updateSettings(
      core.settings().settings.copyWith(appearance: Appearance.system),
    );
    expect(core.settings().effectiveDark, isTrue);
    tester.platformDispatcher.platformBrightnessTestValue = Brightness.light;
    addTearDown(tester.platformDispatcher.clearPlatformBrightnessTestValue);
    await pumpApp(tester, width: 360, core: core);
    expect(core.systemDarkCalls, [false]);
    expect(core.settings().effectiveDark, isFalse);
    expect(_scaffold(tester), _lightBackground);
  });

  testWidgets('forced Dark ignores a platform flip', (tester) async {
    final core = await _pump(
      tester,
      appearance: Appearance.dark,
      platform: Brightness.dark,
    );
    await _flip(tester, Brightness.light);
    expect(_scaffold(tester), _darkBackground);
    expect(core.systemDark, isFalse);
  });

  for (final (appearance, brightness) in [
    (Appearance.dark, Brightness.dark),
    (Appearance.light, Brightness.light),
  ]) {
    testWidgets('the terminal route sets the ${brightness.name} overlay', (
      tester,
    ) async {
      await _pump(tester, appearance: appearance, platform: Brightness.dark);
      final palette = brightness == Brightness.dark
          ? RoostPalette.dark
          : RoostPalette.light;
      final want = systemOverlayStyleFor(brightness, palette);
      final icons = brightness == Brightness.dark
          ? Brightness.light
          : Brightness.dark;

      // The route with a bar and the route without agree.
      expect(_styleAbove(tester, find.byType(AppBar)), isNotNull);
      await _openTerminal(tester);
      final style = _styleAbove(tester, find.byType(TerminalPane));
      expect(style.systemNavigationBarIconBrightness, icons);
      expect(style.statusBarIconBrightness, icons);
      expect(style.systemNavigationBarColor, palette.background);
      expect(style, want);
      expect(SystemChrome.latestStyle, want);
    });
  }

  testWidgets('an AppBar route agrees with the root overlay', (tester) async {
    await _pump(
      tester,
      appearance: Appearance.light,
      platform: Brightness.dark,
    );
    final bar = tester
        .widget<AnnotatedRegion<SystemUiOverlayStyle>>(
          find
              .descendant(
                of: find.byType(AppBar),
                matching: find.byType(AnnotatedRegion<SystemUiOverlayStyle>),
              )
              .first,
        )
        .value;
    final want = systemOverlayStyleFor(Brightness.light, RoostPalette.light);
    expect(bar, want);
    expect(SystemChrome.latestStyle, want, reason: 'what Flutter applies');
  });
}
