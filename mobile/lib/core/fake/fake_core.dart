import 'dart:async';
import 'dart:typed_data';

import '../../terminal/frame.dart';
import '../../terminal/frame_encoder.dart';
import '../replay_feed.dart';
import '../roost_core.dart';
import 'fake_screen.dart';
import 'fake_shell.dart';

/// A scripted [RoostCore]: three hosts (connected, needs-trust, failed), a
/// pretend shell per tab, and a record of every call, for the tests and for
/// running the app on the Linux desktop with no Rust and no host.
class FakeCore implements RoostCore {
  FakeCore({
    this.latency = Duration.zero,
    Duration? pauseLatency,
    this.demo = false,
    bool seed = true,
    this.systemDark = true,
  }) : pauseLatency = pauseLatency ?? latency {
    if (seed) _seed();
  }

  /// The scripted hosts plus the timers that make the demo move: a test run
  /// that streams output and a desktop that takes a tab's size back.
  factory FakeCore.demo({bool systemDark = true}) => FakeCore(
    latency: const Duration(milliseconds: 600),
    demo: true,
    systemDark: systemDark,
  );

  static const desktopLabel = 'charlie-laptop';
  static const macMiniFingerprint =
      'SHA256:q3Vb8m0Xr2pL9cJk4TfYwN1zE6hG5sAa7uDiOoP0cMk';

  final Duration latency;

  /// How long `appPaused` takes before it detaches: a real pause waits for
  /// its size releases.
  final Duration pauseLatency;
  final bool demo;

  final List<FakeAttach> attaches = [];
  final List<FakeSend> sends = [];
  final List<FakeOp> ops = [];
  final List<(String, String)> trusted = [];
  final List<FakeTerminalSession> sessions = [];
  int pausedCount = 0;
  int resumedCount = 0;

  /// Every settings write, as sent.
  final List<MobileSettings> settingsWrites = [];
  final List<bool> systemDarkCalls = [];

  MobileSettings _settings = const MobileSettings();

  /// What the platform says, as `setSystemDark` last reported it.
  bool systemDark;

  /// Each fake color's stand-in in the terminal theme in effect: none under
  /// roost-dark, where the fake draws its own colors unchanged.
  Map<int, int> _colors = const {};

  final List<_FakeHost> _hosts = [];
  final _hostsFeed = ReplayFeed<List<HostView>>(const []);
  final Map<String, ReplayFeed<WorkspaceView>> _workspaceFeeds = {};
  int _nextId = 100;
  Timer? _ticker;
  bool _desktopTookBack = false;

  void dispose() {
    _ticker?.cancel();
    for (final s in List.of(sessions)) {
      s._end('core disposed');
    }
  }

  @override
  Stream<List<HostView>> hosts() => _hostsFeed.stream;

  @override
  Future<String> addHost({
    required String label,
    required String target,
  }) async {
    final trimmed = target.trim();
    if (trimmed.isEmpty) throw const CoreError('target is empty');
    final id = 'host-${_nextId++}';
    _hosts.add(
      _FakeHost(
          id: id,
          label: label.trim().isEmpty ? trimmed : label.trim(),
          target: trimmed,
          state: const HostDisconnected(),
          // A new host behaves like a first contact: its key needs trusting.
          fingerprint: 'SHA256:${_fakeFingerprint(trimmed)}',
        )
        ..projects.add(
          _FakeProject(id: _nextId++, name: 'home', cwd: '~', tabs: []),
        ),
    );
    _publishHosts();
    return id;
  }

  @override
  Future<void> removeHost(String hostId) async {
    final host = _host(hostId);
    for (final s in sessions.where((s) => s.hostId == hostId).toList()) {
      s._end('host removed');
    }
    _hosts.remove(host);
    _publishHosts();
  }

  @override
  Future<void> connect(String hostId) async {
    final host = _host(hostId);
    _setState(host, const HostConnecting());
    await _wait();
    if (!host.reachable) {
      _setState(
        host,
        const HostFailed(reason: 'Connection timed out (no route to host)'),
      );
    } else if (!host.trusted) {
      _setState(host, HostNeedsTrust(fingerprint: host.fingerprint));
    } else {
      _setState(host, const HostConnected());
    }
  }

  @override
  Future<void> disconnect(String hostId) async {
    final host = _host(hostId);
    for (final s in sessions.where((s) => s.hostId == hostId).toList()) {
      s._end('disconnected');
    }
    _setState(host, const HostDisconnected());
  }

  @override
  Future<void> trustHostKey(String hostId, String fingerprint) async {
    final host = _host(hostId);
    if (fingerprint != host.fingerprint) {
      throw CoreError('fingerprint $fingerprint is not the one offered');
    }
    trusted.add((hostId, fingerprint));
    host.trusted = true;
    await connect(hostId);
  }

  @override
  Stream<WorkspaceView> workspace(String hostId) =>
      _workspaceFeed(hostId).stream;

  @override
  Future<Object?> op(
    String hostId,
    String op, [
    Map<String, Object?> params = const {},
  ]) async {
    ops.add(FakeOp(hostId, op, params));
    final host = _connectedHost(hostId);
    switch (op) {
      case 'tab.open':
        final project = host.project(_id(params['project_id']));
        final id = _nextId++;
        final title = (params['title'] as String?) ?? '';
        final tab = _FakeTab(
          TabView(
            id: id,
            title: title.isEmpty ? 'zsh' : title,
            cwd: project.cwd,
          ),
        )..shell.prompt();
        project.tabs.add(tab);
        _publishWorkspace(host);
        return {
          'tab': {'id': '$id', 'title': tab.view.title, 'cwd': project.cwd},
        };
      case 'tab.close':
        final tabId = _id(params['tab_id']);
        final project = host.projectOfTab(tabId);
        project.tabs.removeWhere((t) => t.view.id == tabId);
        for (final s in sessions.where((s) => s.tabId == tabId).toList()) {
          s._end('tab closed');
        }
        _publishWorkspace(host);
        return const <String, Object?>{};
      case 'tab.set_title':
        final tab = host.tab(_id(params['tab_id']));
        tab.view = tab.view.copyWith(title: params['title'] as String? ?? '');
        _publishWorkspace(host);
        return const <String, Object?>{};
      case 'project.create':
        final id = _nextId++;
        final name = (params['name'] as String?) ?? '';
        final cwd = (params['cwd'] as String?) ?? '~';
        host.projects.add(
          _FakeProject(
            id: id,
            name: name.isEmpty ? 'Untitled ${host.projects.length + 1}' : name,
            cwd: cwd.isEmpty ? '~' : cwd,
            tabs: [],
          ),
        );
        _publishWorkspace(host);
        return {
          'project': {'id': '$id', 'name': name},
        };
      case 'project.rename':
        host.project(_id(params['project_id'])).name =
            params['name'] as String? ?? '';
        _publishWorkspace(host);
        return const <String, Object?>{};
      case 'project.delete':
        final project = host.project(_id(params['project_id']));
        for (final t in project.tabs) {
          for (final s
              in sessions.where((s) => s.tabId == t.view.id).toList()) {
            s._end('tab closed');
          }
        }
        host.projects.remove(project);
        _publishWorkspace(host);
        return const <String, Object?>{};
    }
    throw CoreError('unknown-op: $op');
  }

  @override
  Future<void> sendText(
    String hostId,
    int tabId,
    String text, {
    required bool paste,
  }) async {
    sends.add(FakeSend(hostId, tabId, text, paste: paste));
    final host = _connectedHost(hostId);
    final tab = host.tab(tabId);
    if (paste) {
      tab.shell.paste(text);
    } else {
      tab.shell.type(text);
    }
    if (demo && tab.view.needsInput && _answers.contains(text)) {
      _answer(host, tab, text);
    }
  }

  @override
  Future<void> clearNotification(String hostId, int tabId) async {
    final host = _connectedHost(hostId);
    final tab = host.tab(tabId);
    tab.view = tab.view.copyWith(hasNotification: false);
    _publishWorkspace(host);
  }

  @override
  Future<TerminalSession> attach(
    String hostId,
    int tabId,
    Geometry geometry, {
    required bool take,
  }) async {
    attaches.add(FakeAttach(hostId, tabId, geometry, take: take));
    final host = _connectedHost(hostId);
    final tab = host.tab(tabId);
    await _wait();
    final session = FakeTerminalSession._(this, hostId, tab, geometry);
    sessions.add(session);
    session._start(take: take);
    if (demo && tabId == _nextestTab && !_desktopTookBack) {
      _desktopTookBack = true;
      Timer(const Duration(seconds: 5), () {
        if (sessions.contains(session)) {
          takeSizeFrom(hostId, tabId, label: desktopLabel, cols: 132, rows: 40);
        }
      });
    }
    return session;
  }

  @override
  Future<void> appPaused() async {
    pausedCount++;
    if (pauseLatency != Duration.zero) await Future<void>.delayed(pauseLatency);
    for (final s in List.of(sessions)) {
      final tab = s._tab;
      if (tab.ownerIsMe) {
        tab
          ..ownerIsMe = false
          ..ownerLabel = desktopLabel;
      }
      s._end('detached: app in the background');
    }
  }

  @override
  Future<void> appResumed() async {
    resumedCount++;
  }

  @override
  SettingsState settings() => _settingsState();

  @override
  Future<SettingsState> updateSettings(MobileSettings settings) async {
    settingsWrites.add(settings);
    _changeSettings(() => _settings = sanitizeSettings(settings));
    return _settingsState();
  }

  @override
  Future<SettingsState> setSystemDark(bool dark) async {
    systemDarkCalls.add(dark);
    _changeSettings(() => systemDark = dark);
    return _settingsState();
  }

  @override
  List<ThemeSummary> terminalThemes() => fakeThemes;

  /// The roost-dark and Catppuccin Mocha values are the repo's theme files;
  /// the light two are the pinned Ghostty package's.
  static const fakeThemes = [
    ThemeSummary(
      name: 'roost-dark',
      light: false,
      background: 0x1e1e1e,
      foreground: 0xffffff,
      cursor: 0x98989d,
      ansi: [
        0x1a1a1a,
        0xcc372e,
        0x26a439,
        0xcdac08,
        0x0869cb,
        0x9647bf,
        0x479ec2,
        0x98989d,
        0x464646,
        0xff453a,
        0x32d74b,
        0xffd60a,
        0x0a84ff,
        0xbf5af2,
        0x76d6ff,
        0xffffff,
      ],
    ),
    ThemeSummary(
      name: 'Catppuccin Mocha',
      light: false,
      background: 0x1e1e2e,
      foreground: 0xcdd6f4,
      cursor: 0xf5e0dc,
      ansi: [
        0x45475a,
        0xf38ba8,
        0xa6e3a1,
        0xf9e2af,
        0x89b4fa,
        0xf5c2e7,
        0x94e2d5,
        0xa6adc8,
        0x585b70,
        0xf37799,
        0x89d88b,
        0xebd391,
        0x74a8fc,
        0xf2aede,
        0x6bd7ca,
        0xbac2de,
      ],
    ),
    ThemeSummary(
      name: 'Catppuccin Latte',
      light: true,
      background: 0xeff1f5,
      foreground: 0x4c4f69,
      cursor: 0xdc8a78,
      ansi: [
        0xbcc0cc,
        0xd20f39,
        0x40a02b,
        0xdf8e1d,
        0x1e66f5,
        0xea76cb,
        0x179299,
        0x5c5f77,
        0xacb0be,
        0xe7103f,
        0x46b02f,
        0xe49931,
        0x3878f6,
        0xef95d7,
        0x19a1a8,
        0x6c6f85,
      ],
    ),
    ThemeSummary(
      name: 'GitHub Light Default',
      light: true,
      background: 0xffffff,
      foreground: 0x1f2328,
      cursor: 0x0969da,
      ansi: [
        0x24292f,
        0xcf222e,
        0x116329,
        0x4d2d00,
        0x0969da,
        0x8250df,
        0x1b7c83,
        0x6e7781,
        0x57606a,
        0xa40e26,
        0x1a7f37,
        0x633c01,
        0x218bff,
        0xa475f9,
        0x3192aa,
        0x8c959f,
      ],
    ),
  ];

  /// The core's sanitize rules (`MobileSettings::sanitize` in
  /// `registry.rs`), over the fake's own themes.
  static MobileSettings sanitizeSettings(MobileSettings s) {
    const defaults = MobileSettings();
    double stepped(
      double value,
      double fallback,
      double min,
      double max,
      double perUnit,
    ) => value.isFinite
        ? (value.clamp(min, max) * perUnit).round() / perUnit
        : fallback;
    String theme(String name, {required bool light, required String or}) {
      final want = name.trim().toLowerCase();
      for (final t in fakeThemes) {
        if (t.light == light && t.name.toLowerCase() == want) return t.name;
      }
      return or;
    }

    return s.copyWith(
      terminalFontSizeCompact: stepped(
        s.terminalFontSizeCompact,
        defaults.terminalFontSizeCompact,
        7,
        28,
        2,
      ),
      terminalFontSizeWide: stepped(
        s.terminalFontSizeWide,
        defaults.terminalFontSizeWide,
        7,
        28,
        2,
      ),
      uiTextScale: stepped(s.uiTextScale, defaults.uiTextScale, 0.85, 1.6, 20),
      darkTheme: theme(s.darkTheme, light: false, or: defaults.darkTheme),
      lightTheme: theme(s.lightTheme, light: true, or: defaults.lightTheme),
    );
  }

  SettingsState _settingsState() {
    final dark =
        _settings.appearance == Appearance.dark ||
        (_settings.appearance == Appearance.system && systemDark);
    return SettingsState(
      settings: _settings,
      effectiveDark: dark,
      effectiveTheme: dark ? _settings.darkTheme : _settings.lightTheme,
    );
  }

  void _changeSettings(void Function() change) {
    final before = _settingsState().effectiveTheme;
    change();
    final theme = _settingsState().effectiveTheme;
    if (theme == before) return;
    _colors = theme == MobileSettings.defaultDarkTheme
        ? const {}
        : FakeColors.mappedTo(fakeThemes.firstWhere((t) => t.name == theme));
    for (final s in List.of(sessions)) {
      s._redrawAll();
    }
  }

  /// Another client takes [tabId]'s size, as a desktop does when its user
  /// types into the tab.
  void takeSizeFrom(
    String hostId,
    int tabId, {
    required String label,
    required int cols,
    required int rows,
  }) {
    final tab = _host(hostId).tab(tabId);
    tab
      ..ownerIsMe = false
      ..ownerLabel = label;
    tab.shell.screen.resize(cols, rows);
    for (final s in sessions.where((s) => s._tab == tab)) {
      s._announceOwner();
    }
    tab.shell.changed();
  }

  /// Sets a host's state directly, as a dropped connection would.
  void setHostState(String hostId, HostStateView state) =>
      _setState(_host(hostId), state);

  TabView tabView(String hostId, int tabId) => _host(hostId).tab(tabId).view;

  FakeShell shellOf(String hostId, int tabId) => _host(hostId).tab(tabId).shell;

  // --- scripted state -------------------------------------------------------

  static const _claudeTab = 11;
  static const _nextestTab = 12;
  static const _answers = {'1', '2', '3', '\r'};

  void _seed() {
    final devbox = _FakeHost(
      id: 'devbox',
      label: 'devbox',
      target: 'charlie@devbox',
      state: const HostConnected(),
      fingerprint: 'SHA256:Kp2nQ0YdV8cM3rT6wX1zB4hE7jL9sA5uF2gN8yR0vIo',
      trusted: true,
    );
    final claude = _FakeTab(
      const TabView(
        id: _claudeTab,
        title: 'claude · plan 089',
        cwd: '~/projects/roost',
        agent: AgentBadge.waiting,
        hasNotification: true,
        statusText: 'Permission: edit host_conn.rs',
      ),
      cwd: '~/projects/roost',
    );
    _scriptClaude(claude.shell);
    final nextest = _FakeTab(
      const TabView(
        id: _nextestTab,
        title: 'cargo nextest',
        cwd: '~/projects/roost',
        agent: AgentBadge.working,
        statusText: '1702/2417 passed',
      ),
      cwd: '~/projects/roost',
    );
    _scriptNextest(nextest.shell);
    final zsh = _FakeTab(
      const TabView(id: 13, title: 'zsh', cwd: '~/projects/roost'),
      cwd: '~/projects/roost',
    );
    zsh.shell
      ..write(
        '6d9c12d8 chore(appcast-iced): publish v0.0.26\n',
        fg: FakeColors.muted,
      )
      ..write('Type "help" or "colors".\n', fg: FakeColors.muted)
      ..prompt();
    devbox.projects.addAll([
      _FakeProject(
        id: 1,
        name: 'roost',
        cwd: '~/projects/roost',
        tabs: [claude, nextest, zsh],
      ),
      _FakeProject(
        id: 2,
        name: 'craze',
        cwd: '~/projects/craze',
        tabs: [
          _FakeTab(
              const TabView(
                id: 21,
                title: 'craze · acp fix',
                cwd: '~/projects/craze',
                agent: AgentBadge.working,
                statusText: 'running · 14m',
              ),
              cwd: '~/projects/craze',
            )
            ..shell.write(
              '● Reading crates/leader/src/lib.rs\n',
              fg: FakeColors.blue,
            ),
          _FakeTab(
            const TabView(
              id: 22,
              title: 'zsh',
              cwd: '~/projects/craze',
              agent: AgentBadge.idle,
            ),
            cwd: '~/projects/craze',
          )..shell.prompt(),
        ],
      ),
    ]);

    final macMini = _FakeHost(
      id: 'mac-mini',
      label: 'mac-mini',
      target: 'charlie@mac-mini',
      state: const HostNeedsTrust(fingerprint: macMiniFingerprint),
      fingerprint: macMiniFingerprint,
    );
    macMini.projects.add(
      _FakeProject(
        id: 1,
        name: 'roost (mac gates)',
        cwd: '~/projects/roost',
        tabs: [
          _FakeTab(
            const TabView(id: 31, title: 'zsh', cwd: '~/projects/roost'),
            cwd: '~/projects/roost',
          )..shell.prompt(),
        ],
      ),
    );

    final nas = _FakeHost(
      id: 'nas',
      label: 'nas',
      target: 'charlie@nas',
      state: const HostFailed(
        reason: 'Connection timed out (no route to host)',
      ),
      fingerprint: 'SHA256:nas',
      reachable: false,
    );

    _hosts.addAll([devbox, macMini, nas]);
    _publishHosts();
    if (demo) {
      _ticker = Timer.periodic(const Duration(milliseconds: 900), (_) {
        _tickNextest(devbox);
      });
    }
  }

  void _scriptClaude(FakeShell s) {
    s
      ..write(
        r'~/projects/roost (feature/mobile-mvp) $ claude'
        '\n',
        fg: FakeColors.muted,
      )
      ..write('\n')
      ..write('> Move host_conn/{state,mirror}.rs into roost-client.\n')
      ..write('\n')
      ..write(
        '● Read crates/roost-iced/src/host_conn/state.rs\n',
        fg: FakeColors.blue,
      )
      ..write(
        '● Moved host_conn/state.rs → crates/roost-client/src/state.rs\n',
        fg: FakeColors.blue,
      )
      ..write('\n')
      ..write('  Edit crates/roost-iced/src/host_conn.rs\n')
      ..write('   12  - mod state;\n', fg: FakeColors.red)
      ..write(
        '   12  + pub(crate) use roost_client::state;\n',
        fg: FakeColors.green,
      )
      ..write('\n')
      ..write(
        '  Do you want to make this edit to host_conn.rs?\n',
        fg: FakeColors.orange,
        attrs: RunAttrs.bold,
      )
      ..write('  ❯ 1. Yes\n', fg: FakeColors.orange)
      ..write("    2. Yes, and don't ask again this session\n")
      ..write('    3. No, and tell Claude what to do differently ')
      ..write('(esc)', fg: FakeColors.muted, attrs: RunAttrs.faint)
      ..write('\n');
  }

  void _scriptNextest(FakeShell s) {
    s
      ..write(
        r'~/projects/roost $ cargo nextest run'
        '\n',
        fg: FakeColors.muted,
      )
      ..write('    Finished `test` profile in 41.2s\n')
      ..write(
        '    Starting 2417 tests across 61 binaries\n',
        fg: FakeColors.green,
      );
  }

  int _tick = 0;

  void _tickNextest(_FakeHost devbox) {
    final tab = devbox.tabOrNull(_nextestTab);
    if (tab == null) return;
    _tick++;
    tab.shell
      ..write('        PASS ', fg: FakeColors.green, attrs: RunAttrs.bold)
      ..write(
        '[ ${(0.01 * (_tick % 30)).toStringAsFixed(3)}s] ',
        fg: FakeColors.muted,
      )
      ..write('roost-client state::tests::case_$_tick\n');
    tab.view = tab.view.copyWith(statusText: '${1702 + _tick}/2417 passed');
    _publishWorkspace(devbox);
  }

  void _answer(_FakeHost host, _FakeTab tab, String text) {
    tab.view = TabView(
      id: tab.view.id,
      title: tab.view.title,
      cwd: tab.view.cwd,
      agent: AgentBadge.working,
    );
    tab.shell.write('\n● Edit applied\n', fg: FakeColors.green);
    _publishWorkspace(host);
  }

  // --- plumbing -------------------------------------------------------------

  Future<void> _wait() => latency == Duration.zero
      ? Future<void>.value()
      : Future<void>.delayed(latency);

  _FakeHost _host(String id) {
    for (final h in _hosts) {
      if (h.id == id) return h;
    }
    throw CoreError('no host $id');
  }

  _FakeHost _connectedHost(String id) {
    final host = _host(id);
    if (host.state is! HostConnected) {
      throw CoreError('${host.label} is not connected');
    }
    return host;
  }

  void _setState(_FakeHost host, HostStateView state) {
    host.state = state;
    _publishWorkspace(host);
  }

  void _publishHosts() {
    _hostsFeed.add([for (final h in _hosts) h.view]);
  }

  ReplayFeed<WorkspaceView> _workspaceFeed(String hostId) =>
      _workspaceFeeds.putIfAbsent(hostId, () {
        final host = _hosts.where((h) => h.id == hostId).firstOrNull;
        return ReplayFeed(host?.workspaceView ?? WorkspaceView.empty);
      });

  void _publishWorkspace(_FakeHost host) {
    _workspaceFeed(host.id).add(host.workspaceView);
    _publishHosts();
  }

  static int _id(Object? value) => switch (value) {
    final int v => v,
    final String v when int.tryParse(v) != null => int.parse(v),
    _ => throw CoreError('invalid-param: id $value'),
  };

  static String _fakeFingerprint(String seed) {
    const alphabet =
        'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
    var h = 0x811c9dc5;
    final out = StringBuffer();
    for (var i = 0; i < 43; i++) {
      h =
          ((h ^ seed.codeUnitAt(i % seed.length) ^ i) * 0x01000193) &
          0xffffffff;
      out.write(alphabet[h % 64]);
    }
    return out.toString();
  }
}

class FakeAttach {
  FakeAttach(this.hostId, this.tabId, this.geometry, {required this.take});
  final String hostId;
  final int tabId;
  final Geometry geometry;
  final bool take;
}

class FakeSend {
  FakeSend(this.hostId, this.tabId, this.text, {required this.paste});
  final String hostId;
  final int tabId;
  final String text;
  final bool paste;

  @override
  String toString() =>
      'FakeSend($hostId, $tabId, ${text.codeUnits}, paste: $paste)';
}

class FakeOp {
  FakeOp(this.hostId, this.op, this.params);
  final String hostId;
  final String op;
  final Map<String, Object?> params;
}

/// One attached fake tab. Records what the UI sent it.
class FakeTerminalSession implements TerminalSession {
  FakeTerminalSession._(this._core, this.hostId, this._tab, this._geometry);

  final FakeCore _core;
  final String hostId;
  final _FakeTab _tab;
  Geometry _geometry;
  final _events = StreamController<TerminalEvent>();
  bool _framePending = false;
  bool _needFull = true;

  final List<KeyInput> keys = [];
  final List<String> texts = [];
  final List<String> pastes = [];
  final List<int> scrolls = [];
  final List<(Geometry, bool)> resizes = [];
  int takeSizeCount = 0;
  bool closed = false;

  int get tabId => _tab.view.id;
  FakeShell get shell => _tab.shell;

  void _start({required bool take}) {
    _events.add(const StatusChanged(AttachState.attaching));
    if (take) _takeSize();
    _tab.shell
      ..addListener(_onShellChanged)
      ..onClipboardWrite = _clipboardWrite;
    _events.add(const StatusChanged(AttachState.live));
    _announceOwner();
    _signalFrame();
  }

  void _takeSize() {
    _tab
      ..ownerIsMe = true
      ..ownerLabel = null;
    _tab.shell.screen.resize(_geometry.cols, _geometry.rows);
  }

  void _announceOwner() {
    if (closed) return;
    final screen = _tab.shell.screen;
    _events.add(
      OwnerChanged(
        label: _tab.ownerIsMe ? null : _tab.ownerLabel,
        isMe: _tab.ownerIsMe,
        cols: screen.cols,
        rows: screen.rows,
      ),
    );
  }

  void _onShellChanged() => _signalFrame();

  /// Every attachment of a tab shares its screen, so each takes its own
  /// full frame rather than the first pull clearing it for the rest.
  void _redrawAll() {
    _needFull = true;
    _signalFrame();
  }

  void _signalFrame() {
    if (closed || _framePending) return;
    _framePending = true;
    _events.add(const FrameReady());
  }

  void _end(String reason) {
    if (closed) return;
    _events.add(StatusChanged(AttachState.ended, reason));
    _shutdown();
  }

  void _clipboardWrite(String text) {
    if (!closed) _events.add(ClipboardWrite(text));
  }

  void _shutdown() {
    closed = true;
    _tab.shell.removeListener(_onShellChanged);
    if (_tab.shell.onClipboardWrite == _clipboardWrite) {
      _tab.shell.onClipboardWrite = null;
    }
    _core.sessions.remove(this);
    unawaited(_events.close());
  }

  void _input() {
    if (_tab.ownerIsMe) return;
    _takeSize();
    _announceOwner();
  }

  @override
  Stream<TerminalEvent> events() => _events.stream;

  @override
  Future<Uint8List> takeFrame() async {
    _framePending = false;
    final screen = _tab.shell.screen;
    if (_needFull) {
      _needFull = false;
      screen.markFull();
    }
    if (closed || !screen.dirty) return Uint8List(0);
    final frame = screen.takeFrame();
    final colors = _core._colors;
    return encodeFrame(colors.isEmpty ? frame : recolorFrame(frame, colors));
  }

  @override
  Future<void> key(KeyInput key) async {
    if (closed) return;
    keys.add(key);
    _input();
    _tab.shell.key(key);
  }

  @override
  Future<void> text(String text) async {
    if (closed) return;
    texts.add(text);
    _input();
    _tab.shell.type(text);
  }

  @override
  Future<void> paste(String text) async {
    if (closed) return;
    pastes.add(text);
    _input();
    _tab.shell.paste(text);
  }

  @override
  Future<void> scroll(int rows, {required int col, required int row}) async {
    if (closed) return;
    scrolls.add(rows);
    _tab.shell.screen.scroll(rows);
    _signalFrame();
  }

  @override
  Future<void> resize(Geometry geometry, {required bool focused}) async {
    if (closed) return;
    resizes.add((geometry, focused));
    _geometry = geometry;
    // A focused resize takes the size, as the session's does.
    if (!focused && !_tab.ownerIsMe) return;
    _takeSize();
    _announceOwner();
    _signalFrame();
  }

  @override
  Future<void> takeSize() async {
    if (closed) return;
    takeSizeCount++;
    _takeSize();
    _announceOwner();
    _signalFrame();
  }

  @override
  Future<void> close() async {
    if (closed) return;
    _shutdown();
  }
}

class _FakeHost {
  _FakeHost({
    required this.id,
    required this.label,
    required this.target,
    required this.state,
    required this.fingerprint,
    this.trusted = false,
    this.reachable = true,
  });

  final String id;
  final String label;
  final String target;
  HostStateView state;
  final String fingerprint;
  bool trusted;
  bool reachable;
  final List<_FakeProject> projects = [];

  bool get connected => state is HostConnected;

  WorkspaceView get workspaceView {
    if (!connected) return WorkspaceView.empty;
    return WorkspaceView(
      projects: [
        for (final p in projects)
          ProjectView(
            id: p.id,
            name: p.name,
            cwd: p.cwd,
            tabs: [for (final t in p.tabs) t.view],
          ),
      ],
      activeProjectId: projects.isEmpty ? 0 : projects.first.id,
      activeTabId: projects.isEmpty || projects.first.tabs.isEmpty
          ? 0
          : projects.first.tabs.first.view.id,
    );
  }

  HostView get view {
    var waiting = 0;
    var working = 0;
    if (connected) {
      for (final p in projects) {
        for (final t in p.tabs) {
          if (t.view.agent == AgentBadge.waiting) waiting++;
          if (t.view.agent == AgentBadge.working) working++;
        }
      }
    }
    return HostView(
      id: id,
      label: label,
      target: target,
      state: state,
      waitingTabs: waiting,
      workingTabs: working,
    );
  }

  _FakeProject project(int id) => projects.firstWhere(
    (p) => p.id == id,
    orElse: () => throw CoreError('not-found: project $id'),
  );

  _FakeProject projectOfTab(int tabId) => projects.firstWhere(
    (p) => p.tabs.any((t) => t.view.id == tabId),
    orElse: () => throw CoreError('not-found: tab $tabId'),
  );

  _FakeTab? tabOrNull(int tabId) {
    for (final p in projects) {
      for (final t in p.tabs) {
        if (t.view.id == tabId) return t;
      }
    }
    return null;
  }

  _FakeTab tab(int tabId) =>
      tabOrNull(tabId) ?? (throw CoreError('not-found: tab $tabId'));
}

class _FakeProject {
  _FakeProject({
    required this.id,
    required this.name,
    required this.cwd,
    required this.tabs,
  });

  final int id;
  String name;
  final String cwd;
  final List<_FakeTab> tabs;
}

class _FakeTab {
  _FakeTab(this.view, {String cwd = '~'}) : shell = FakeShell(cwd: cwd);

  TabView view;
  final FakeShell shell;
  bool ownerIsMe = false;
  String? ownerLabel = FakeCore.desktopLabel;
}
