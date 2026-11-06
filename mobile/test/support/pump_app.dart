import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/app/app.dart';
import 'package:roost_mobile/app/providers.dart';
import 'package:roost_mobile/core/fake/fake_core.dart';

/// The whole app at a logical [width], on a fresh scripted core. Tall by
/// default: the test font is wider than JetBrains Mono or Roboto, so lists
/// wrap more and would push sidebar sections out of the lazily built view.
Future<FakeCore> pumpApp(
  WidgetTester tester, {
  required double width,
  double height = 1400,
  FakeCore? core,
}) async {
  await tester.binding.setSurfaceSize(Size(width, height));
  addTearDown(() => tester.binding.setSurfaceSize(null));
  core ??= FakeCore();
  addTearDown(core.dispose);
  await tester.pumpWidget(
    ProviderScope(
      overrides: [coreProvider.overrideWithValue(core)],
      child: const RoostMobileApp(),
    ),
  );
  await tester.pumpAndSettle();
  return core;
}

Finder byKey(String key) => find.byKey(ValueKey(key));

/// The session of the tab attached last.
FakeTerminalSession lastSession(FakeCore core) => core.sessions.last;
