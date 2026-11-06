import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart'
    show AnyhowException;

import '../src/rust/api/app.dart';
import '../src/rust/api/terminal.dart';
import '../src/rust/api/types.dart';
import 'replay_feed.dart';
import 'roost_core.dart';
import 'serial_queue.dart';

/// [RoostCore] over the Rust `RoostApp` (crates/roost-mobile, through
/// flutter_rust_bridge). Converts the bridge's flat values to the app's
/// models and its errors to [CoreError].
///
/// The Rust side keeps one sink per stream (a second `hosts()` would
/// silence the first), so each stream is opened once here and replayed to
/// every listener.
class FrbCore implements RoostCore {
  FrbCore(this._app);

  /// Call `RustLib.init()` first.
  static Future<FrbCore> start({
    required String dataDir,
    required String deviceLabel,
  }) async => FrbCore(
    await _guard(() => roostInit(dataDir: dataDir, deviceLabel: deviceLabel)),
  );

  final BridgeApp _app;
  ReplayFeed<List<HostView>>? _hosts;
  final Map<String, ReplayFeed<WorkspaceView>> _workspaces = {};
  final _writes = SerialQueue();

  BridgeLeakCounts leakCounts() => _app.leakCounts();

  @override
  Stream<List<HostView>> hosts() {
    var feed = _hosts;
    if (feed == null) {
      feed = _hosts = ReplayFeed();
      _app.hosts().listen(
        (hosts) => feed!.add([for (final h in hosts) _hostView(h)]),
        onError: feed.addError,
      );
    }
    return feed.stream;
  }

  @override
  Future<String> addHost({required String label, required String target}) =>
      _guard(() => _app.addHost(label: label, target: target));

  @override
  Future<void> removeHost(String hostId) =>
      _guard(() => _app.removeHost(hostId: hostId));

  @override
  Future<void> connect(String hostId) =>
      _guard(() => _app.connect(hostId: hostId));

  @override
  Future<void> disconnect(String hostId) =>
      _guard(() => _app.disconnect(hostId: hostId));

  @override
  Future<void> trustHostKey(String hostId, String fingerprint) =>
      _guard(() => _app.trustHostKey(hostId: hostId, fingerprint: fingerprint));

  @override
  Stream<WorkspaceView> workspace(String hostId) {
    final existing = _workspaces[hostId];
    if (existing != null) return existing.stream;
    final feed = _workspaces[hostId] = ReplayFeed();
    _app
        .workspace(hostId: hostId)
        .listen(
          (w) => feed.add(_workspaceView(w)),
          onError: (Object e) => feed.addError(_coreError(e)),
        );
    return feed.stream;
  }

  @override
  Future<Object?> op(
    String hostId,
    String op, [
    Map<String, Object?> params = const {},
  ]) async {
    final result = await _guard(
      () => _app.op(hostId: hostId, op: op, paramsJson: jsonEncode(params)),
    );
    return result.isEmpty ? null : jsonDecode(result);
  }

  /// One host's writes cross the bridge one at a time: two async bridge
  /// calls can reach the host in either order, and a quick reply's `1` must
  /// land before its Enter.
  @override
  Future<void> sendText(
    String hostId,
    int tabId,
    String text, {
    required bool paste,
  }) => _writes.run(
    hostId,
    () => _guard(
      () =>
          _app.sendText(hostId: hostId, tabId: tabId, text: text, paste: paste),
    ),
  );

  @override
  Future<void> clearNotification(String hostId, int tabId) =>
      _guard(() => _app.clearNotification(hostId: hostId, tabId: tabId));

  @override
  Future<TerminalSession> attach(
    String hostId,
    int tabId,
    Geometry geometry, {
    required bool take,
  }) async {
    final terminal = await _guard(
      () => _app.attach(
        hostId: hostId,
        tabId: tabId,
        geometry: _geometry(geometry),
        take: take,
      ),
    );
    return FrbTerminalSession(terminal);
  }

  @override
  Future<void> appPaused() => _app.appPaused();

  @override
  Future<void> appResumed() => _app.appResumed();
}

/// One attached tab over the bridge. The input calls are synchronous on
/// the Rust side (channel sends, run in call order); after [close] every
/// call is a no-op, so a frame pull still in flight cannot touch a
/// disposed handle.
class FrbTerminalSession implements TerminalSession {
  FrbTerminalSession(this._terminal);

  final BridgeTerminal _terminal;
  bool _closed = false;

  @override
  Stream<TerminalEvent> events() => _terminal.events().map(_terminalEvent);

  @override
  Future<Uint8List> takeFrame() async {
    if (_closed) return Uint8List(0);
    return _terminal.takeFrame();
  }

  /// The drawn viewport as text, one trimmed line per row (tests, logs).
  Future<List<String>> viewportText() async =>
      _closed ? const [] : _terminal.viewportText();

  @override
  Future<void> key(KeyInput key) async {
    if (_closed) return;
    _terminal.key(
      key: BridgeKeyInput(
        key: key.key,
        text: key.text,
        ctrl: key.ctrl,
        alt: key.alt,
        shift: key.shift,
        meta: key.meta,
        repeat: key.repeat,
      ),
    );
  }

  @override
  Future<void> text(String text) async {
    if (!_closed) _terminal.text(text: text);
  }

  @override
  Future<void> paste(String text) async {
    if (!_closed) _terminal.paste(text: text);
  }

  @override
  Future<void> scroll(int rows, {required int col, required int row}) async {
    if (!_closed) _terminal.scroll(rows: rows, col: col, row: row);
  }

  @override
  Future<void> resize(Geometry geometry, {required bool focused}) async {
    if (!_closed) {
      _terminal.resize(geometry: _geometry(geometry), focused: focused);
    }
  }

  @override
  Future<void> takeSize() async {
    if (!_closed) _terminal.takeSize();
  }

  @override
  Future<void> close() async {
    if (_closed) return;
    _closed = true;
    _terminal.close();
    _terminal.dispose();
  }
}

Future<T> _guard<T>(Future<T> Function() call) async {
  try {
    return await call();
  } on AnyhowException catch (e) {
    throw CoreError(e.message);
  }
}

Object _coreError(Object e) => e is AnyhowException ? CoreError(e.message) : e;

BridgeGeometry _geometry(Geometry g) => BridgeGeometry(
  cols: g.cols,
  rows: g.rows,
  cellWPx: g.cellWidthPx,
  cellHPx: g.cellHeightPx,
);

HostView _hostView(BridgeHostView h) => HostView(
  id: h.id,
  label: h.label,
  target: h.target,
  state: _hostState(h.state),
  waitingTabs: h.waitingTabs,
  workingTabs: h.workingTabs,
);

HostStateView _hostState(BridgeHostState s) => switch (s.kind) {
  BridgeHostStateKind.disconnected => const HostDisconnected(),
  BridgeHostStateKind.connecting => const HostConnecting(),
  BridgeHostStateKind.connected => const HostConnected(),
  BridgeHostStateKind.retrying => HostRetrying(
    inMs: s.retryInMs,
    reason: s.reason ?? '',
  ),
  BridgeHostStateKind.needsTrust => HostNeedsTrust(
    fingerprint: s.fingerprint ?? '',
  ),
  BridgeHostStateKind.failed => HostFailed(reason: s.reason ?? ''),
};

WorkspaceView _workspaceView(BridgeWorkspaceView w) => WorkspaceView(
  projects: [
    for (final p in w.projects)
      ProjectView(
        id: p.id,
        name: p.name,
        cwd: p.cwd,
        tabs: [
          for (final t in p.tabs)
            TabView(
              id: t.id,
              title: t.title,
              cwd: t.cwd,
              agent: _badge(t.agent),
              hasNotification: t.hasNotification,
              statusText: t.statusText,
            ),
        ],
      ),
  ],
  activeProjectId: w.activeProjectId,
  activeTabId: w.activeTabId,
);

AgentBadge _badge(BridgeAgentBadge b) => switch (b) {
  BridgeAgentBadge.none => AgentBadge.none,
  BridgeAgentBadge.idle => AgentBadge.idle,
  BridgeAgentBadge.working => AgentBadge.working,
  BridgeAgentBadge.waiting => AgentBadge.waiting,
};

AttachState _attachState(BridgeAttachState s) => switch (s) {
  BridgeAttachState.attaching => AttachState.attaching,
  BridgeAttachState.live => AttachState.live,
  BridgeAttachState.reconnecting => AttachState.reconnecting,
  BridgeAttachState.ended => AttachState.ended,
};

TerminalEvent _terminalEvent(BridgeTerminalEvent e) => switch (e.kind) {
  BridgeTerminalEventKind.frameReady => const FrameReady(),
  BridgeTerminalEventKind.status => StatusChanged(
    _attachState(e.state),
    e.message,
  ),
  BridgeTerminalEventKind.owner => OwnerChanged(
    label: e.ownerLabel,
    isMe: e.isMe,
    cols: e.cols,
    rows: e.rows,
  ),
  BridgeTerminalEventKind.exited => Exited(e.exitCode),
};
