/// This is copied from Cargokit (which is the official way to use it currently)
/// Details: https://fzyzcjy.github.io/flutter_rust_bridge/manual/integrate/builtin

import 'dart:io';

import 'package:collection/collection.dart';
import 'package:logging/logging.dart';
import 'package:path/path.dart' as path;

import 'android_environment.dart';
import 'cargo.dart';
import 'environment.dart';
import 'options.dart';
import 'rustup.dart';
import 'target.dart';
import 'util.dart';

final _log = Logger('builder');

enum BuildConfiguration {
  debug,
  release,
  profile,
}

extension on BuildConfiguration {
  bool get isDebug => this == BuildConfiguration.debug;
  String get rustName => switch (this) {
        BuildConfiguration.debug => 'debug',
        BuildConfiguration.release => 'release',
        BuildConfiguration.profile => 'release',
      };
}

class BuildException implements Exception {
  final String message;

  BuildException(this.message);

  @override
  String toString() {
    return 'BuildException: $message';
  }
}

class BuildEnvironment {
  final BuildConfiguration configuration;
  final CargokitCrateOptions crateOptions;
  final String targetTempDir;
  final String manifestDir;
  final CrateInfo crateInfo;

  final bool isAndroid;
  final String? androidSdkPath;
  final String? androidNdkVersion;
  final int? androidMinSdkVersion;
  final String? javaHome;

  final String? glibcVersion;

  BuildEnvironment({
    required this.configuration,
    required this.crateOptions,
    required this.targetTempDir,
    required this.manifestDir,
    required this.crateInfo,
    required this.isAndroid,
    this.androidSdkPath,
    this.androidNdkVersion,
    this.androidMinSdkVersion,
    this.javaHome,
    this.glibcVersion,
  });

  static BuildConfiguration parseBuildConfiguration(String value) {
    // XCode configuration adds the flavor to configuration name.
    final firstSegment = value.split('-').first;
    final buildConfiguration = BuildConfiguration.values.firstWhereOrNull(
      (e) => e.name == firstSegment,
    );
    if (buildConfiguration == null) {
      _log.warning('Unknown build configuraiton $value, will assume release');
      return BuildConfiguration.release;
    }
    return buildConfiguration;
  }

  static BuildEnvironment fromEnvironment({
    required bool isAndroid,
  }) {
    final buildConfiguration =
        parseBuildConfiguration(Environment.configuration);
    final manifestDir = Environment.manifestDir;
    final crateOptions = CargokitCrateOptions.load(
      manifestDir: manifestDir,
    );
    final crateInfo = CrateInfo.load(manifestDir);
    return BuildEnvironment(
      configuration: buildConfiguration,
      crateOptions: crateOptions,
      targetTempDir: Environment.targetTempDir,
      manifestDir: manifestDir,
      crateInfo: crateInfo,
      isAndroid: isAndroid,
      androidSdkPath: isAndroid ? Environment.sdkPath : null,
      androidNdkVersion: isAndroid ? Environment.ndkVersion : null,
      androidMinSdkVersion:
          isAndroid ? int.parse(Environment.minSdkVersion) : null,
      javaHome: isAndroid ? Environment.javaHome : null,
    );
  }
}

class RustBuilder {
  final Target target;
  final BuildEnvironment environment;

  RustBuilder({
    required this.target,
    required this.environment,
  });

  void prepare(
    Rustup rustup,
  ) {
    final toolchain = _toolchain;
    if (rustup.installedTargets(toolchain) == null) {
      rustup.installToolchain(toolchain);
    }
    if (toolchain == 'nightly') {
      rustup.installRustSrcForNightly();
    }
    if (!rustup.installedTargets(toolchain)!.contains(target.rust)) {
      rustup.installTarget(target.rust, toolchain: toolchain);
    }
    if (environment.glibcVersion != null) {
      rustup.installZigBuild(toolchain);
    }
  }

  CargoBuildOptions? get _buildOptions =>
      environment.crateOptions.cargo[environment.configuration];

  // ===========================================================================
  // LOCAL PATCH (shed-mobile) — honor the crate's rust-toolchain.toml pin.
  //
  // Upstream cargokit only knows the stable/beta/nightly channels declared in
  // cargokit.yaml (see options.dart:Toolchain), so `_toolchain` resolves to
  // 'stable' and every platform build runs `rustup run stable cargo …` —
  // meaning the exact version pinned in `<manifestDir>/rust-toolchain.toml`
  // (e.g. 1.97.1) NEVER governs the platform builds; CI/local build on whatever
  // stable the runner happens to ship. This patch makes the crate's
  // rust-toolchain.toml `channel = "…"` win, so prepare() auto-installs that
  // exact toolchain + its targets and build() runs `rustup run <version> cargo`
  // on every platform, CI and local (true dev == CI).
  //
  // rustup treats a version like "1.97.1" as a toolchain NAME, so
  // Rustup.installToolchain / installTarget / installedTargets all handle it
  // unchanged (verified against rustup.dart). The TOML is parsed minimally with
  // a regex (no TOML dependency).
  //
  // This mirrors the Gradle-9 ExecOperations local patch marked in
  // rust_builder/cargokit/gradle/plugin.gradle — it is a deliberate local
  // divergence from upstream cargokit and MUST be re-applied if cargokit is
  // ever updated/re-vendored.
  // ===========================================================================
  static final _channelRe =
      RegExp(r'^\s*channel\s*=\s*"([^"]+)"', multiLine: true);

  // LOCAL PATCH (roost) — on top of shed-mobile's: the pin is found the way
  // rustup finds it, in the manifest dir or the nearest parent. roost-mobile
  // is a workspace member, and the workspace's rust-toolchain.toml sits at
  // the repo root, two levels above the crate.
  String? _pinnedToolchainFromToml() {
    var dir = Directory(environment.manifestDir).absolute;
    while (true) {
      final file = File(path.join(dir.path, 'rust-toolchain.toml'));
      if (file.existsSync()) {
        final match = _channelRe.firstMatch(file.readAsStringSync());
        return match?.group(1);
      }
      final parent = dir.parent;
      if (parent.path == dir.path) return null;
      dir = parent;
    }
  }

  String get _toolchain =>
      _pinnedToolchainFromToml() ?? _buildOptions?.toolchain.name ?? 'stable';

  /// Returns the path of directory containing build artifacts.
  Future<String> build() async {
    final extraArgs = _buildOptions?.flags ?? [];
    final manifestPath = path.join(environment.manifestDir, 'Cargo.toml');
    runCommand(
      'rustup',
      [
        'run',
        _toolchain,
        'cargo',
        (target.android == null && environment.glibcVersion != null)
            ? 'zigbuild'
            : 'build',
        ...extraArgs,
        '--manifest-path',
        manifestPath,
        '-p',
        environment.crateInfo.packageName,
        if (!environment.configuration.isDebug) '--release',
        '--target',
        target.rust +
            ((target.android == null && environment.glibcVersion != null)
                ? '.${environment.glibcVersion!}'
                : ""),
        '--target-dir',
        environment.targetTempDir,
      ],
      environment: await _buildEnvironment(),
    );
    return path.join(
      environment.targetTempDir,
      target.rust,
      environment.configuration.rustName,
    );
  }

  Future<Map<String, String>> _buildEnvironment() async {
    if (target.android == null) {
      return {};
    } else {
      final sdkPath = environment.androidSdkPath;
      final ndkVersion = environment.androidNdkVersion;
      final minSdkVersion = environment.androidMinSdkVersion;
      if (sdkPath == null) {
        throw BuildException('androidSdkPath is not set');
      }
      if (ndkVersion == null) {
        throw BuildException('androidNdkVersion is not set');
      }
      if (minSdkVersion == null) {
        throw BuildException('androidMinSdkVersion is not set');
      }
      final env = AndroidEnvironment(
        sdkPath: sdkPath,
        ndkVersion: ndkVersion,
        minSdkVersion: minSdkVersion,
        targetTempDir: environment.targetTempDir,
        target: target,
      );
      if (!env.ndkIsInstalled() && environment.javaHome != null) {
        env.installNdk(javaHome: environment.javaHome!);
      }
      return env.buildEnvironment();
    }
  }
}
