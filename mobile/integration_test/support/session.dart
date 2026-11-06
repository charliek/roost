import 'dart:async';
import 'dart:convert';
import 'dart:io';

/// A `roost-session` of its own for one test run: private directories
/// under a short `/tmp/rm-…` root (socket paths must stay under SUN_LEN),
/// `ROOST_TEST_MODE=1`, and an environment built from scratch, so nothing
/// reaches a developer's own session. [stop] signals only the process this
/// started.
class IsolatedSession {
  IsolatedSession._(this.root, this._process, this._log);

  final Directory root;
  final Process _process;
  final IOSink _log;

  String get socket => '${root.path}/ss/roost.sock';
  int get pid => _process.pid;

  /// `ROOST_SESSION_BIN`, else the workspace's debug build
  /// (`cargo build -p roost-session`).
  static String get binary =>
      Platform.environment['ROOST_SESSION_BIN'] ??
      '${Directory.current.parent.path}/target/debug/roost-session';

  static Future<IsolatedSession> start() async {
    final bin = binary;
    if (!File(bin).existsSync()) {
      throw StateError(
        'no roost-session at $bin: cargo build -p roost-session',
      );
    }
    final root = await Directory('/tmp').createTemp('rm-');
    Future<String> private(String name) async {
      final dir = await Directory('${root.path}/$name').create();
      await Process.run('chmod', ['700', dir.path]);
      return dir.path;
    }

    final home = await private('home');
    final runtime = await private('rt');
    final env = {
      'PATH': '/usr/bin:/bin',
      'SHELL': '/bin/sh',
      'ROOST_SHELL_FEATURES': '',
      'ROOST_TEST_MODE': '1',
      'HOME': home,
      'ROOST_TEST_PASSWD_HOME': home,
      'ROOST_SESSION_SOCKET_DIR': await private('ss'),
      'ROOST_STATE_DIR': await private('state'),
      'XDG_RUNTIME_DIR': runtime,
      'ROOST_TEST_USER_RUNTIME_DIR': runtime,
      'XDG_DATA_HOME': '${root.path}/data',
      'XDG_STATE_HOME': '${root.path}/xstate',
      'XDG_CACHE_HOME': '${root.path}/cache',
    };
    final process = await Process.start(
      bin,
      ['start', '--foreground'],
      environment: env,
      includeParentEnvironment: false,
      workingDirectory: home,
    );
    final log = File('${root.path}/session.log').openWrite();
    final ready = Completer<String>();
    // Keep draining stdout after the verdict: a closed pipe would fail the
    // session's next write.
    process.stdout
        .transform(utf8.decoder)
        .transform(const LineSplitter())
        .listen((line) {
          log.writeln(line);
          if (!ready.isCompleted) ready.complete(line);
        });
    process.stderr.listen(log.add);
    final line = await ready.future.timeout(const Duration(seconds: 30));
    if (!line.startsWith('ready pid=')) {
      process.kill(ProcessSignal.sigkill);
      throw StateError('roost-session did not start: $line');
    }
    return IsolatedSession._(root, process, log);
  }

  /// Stops the session; [keepFiles] leaves the root (and its
  /// `session.log`) for a failed run to be read.
  Future<void> stop({bool keepFiles = false}) async {
    _process.kill(ProcessSignal.sigterm);
    final exited = await _process.exitCode
        .then<int?>((code) => code)
        .timeout(const Duration(seconds: 10), onTimeout: () => null);
    if (exited == null) {
      _process.kill(ProcessSignal.sigkill);
      await _process.exitCode;
    }
    await _log.close();
    if (!keepFiles) await root.delete(recursive: true);
  }
}
