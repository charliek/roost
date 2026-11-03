import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import 'app/app.dart';
import 'app/providers.dart';
import 'core/fake/fake_core.dart';
import 'core/roost_core.dart';

/// `--dart-define=ROOST_FAKE=0` asks for the Rust core, which lands with the
/// flutter_rust_bridge milestone; until then the scripted core is the only
/// one there is.
const _useFakeCore = bool.fromEnvironment('ROOST_FAKE', defaultValue: true);

void main() {
  WidgetsFlutterBinding.ensureInitialized();
  runApp(
    ProviderScope(
      overrides: [coreProvider.overrideWithValue(_createCore())],
      child: const RoostMobileApp(),
    ),
  );
}

RoostCore _createCore() {
  if (_useFakeCore) return FakeCore.demo();
  throw UnsupportedError(
    'ROOST_FAKE=0: the Rust core is not bridged into this build yet',
  );
}
