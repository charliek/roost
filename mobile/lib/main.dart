import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart';
import 'package:path_provider/path_provider.dart';

import 'app/app.dart';
import 'app/providers.dart';
import 'core/fake/fake_core.dart';
import 'core/frb_core.dart';
import 'core/roost_core.dart';
import 'src/rust/frb_generated.dart';

/// `--dart-define=ROOST_FAKE=true` (or `=1`) runs the scripted core: no Rust,
/// no host. `bool.fromEnvironment` reads only `true`, hence the string.
const _fakeFlag = String.fromEnvironment('ROOST_FAKE');
const _useFakeCore = _fakeFlag == '1' || _fakeFlag == 'true';

/// `--dart-define=ROOST_DEVICE_LABEL=…` names this device to the hosts.
const _deviceLabelOverride = String.fromEnvironment('ROOST_DEVICE_LABEL');

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  final core = await _createCore();
  runApp(
    ProviderScope(
      overrides: [coreProvider.overrideWithValue(core)],
      child: const RoostMobileApp(),
    ),
  );
}

Future<RoostCore> _createCore() async {
  final systemDark =
      WidgetsBinding.instance.platformDispatcher.platformBrightness ==
      Brightness.dark;
  if (_useFakeCore) return FakeCore.demo(systemDark: systemDark);
  // On macOS cargokit links the Rust core into the roost_mobile_rust pod
  // framework, not the roost_mobile.framework FRB's default loader looks for;
  // that framework is loaded at launch, so its symbols are in the process.
  await RustLib.init(
    externalLibrary: Platform.isMacOS
        ? ExternalLibrary.process(iKnowHowToUseIt: true)
        : null,
  );
  final dir = await getApplicationSupportDirectory();
  return FrbCore.start(
    dataDir: dir.path,
    deviceLabel: deviceLabel(),
    systemDark: systemDark,
  );
}

/// How other clients see this device in "sized by …". Android and iOS
/// report `localhost` as the host name, so they fall back to the OS.
String deviceLabel() {
  if (_deviceLabelOverride.isNotEmpty) return _deviceLabelOverride;
  final host = Platform.localHostname.trim();
  if (host.isNotEmpty && host != 'localhost') return host;
  return '${Platform.operatingSystem} device';
}
