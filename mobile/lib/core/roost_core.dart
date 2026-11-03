import 'dart:typed_data';

import 'models.dart';

export 'models.dart';

/// The app's whole view of Roost: hosts, their workspaces, and attached tabs.
///
/// Mirrors the Rust `RoostApp` (plan 089 C5) one method to one method, so the
/// flutter_rust_bridge build is an adapter class: `FakeCore` serves the UI and
/// the tests until then. Every host-scoped call takes the host's id from
/// [hosts]; tab ids are the session's.
abstract class RoostCore {
  /// The host list, re-emitted whole on every change. A new listener gets the
  /// current list first.
  Stream<List<HostView>> hosts();

  /// Adds a host and returns its id. [target] is `[user@]host[:port]` (or
  /// `unix:/path` for a dev session).
  Future<String> addHost({required String label, required String target});

  Future<void> removeHost(String hostId);

  Future<void> connect(String hostId);

  Future<void> disconnect(String hostId);

  /// Pins [fingerprint] for a host in [HostNeedsTrust] and reconnects.
  Future<void> trustHostKey(String hostId, String fingerprint);

  /// The host's projects and tabs, re-emitted whole on every change. A new
  /// listener gets the current workspace first ([WorkspaceView.empty] while
  /// the host has never connected).
  Stream<WorkspaceView> workspace(String hostId);

  /// A control-plane op (`tab.open`, `tab.close`, `tab.set_title`,
  /// `project.create`, ...). [params] are the wire params, ids string-wrapped
  /// as the session expects; the result is the decoded response.
  Future<Object?> op(
    String hostId,
    String op, [
    Map<String, Object?> params = const {},
  ]);

  /// `tab.write`: never takes the tab's size. [paste] wraps [text] in a
  /// bracketed paste when the program enabled it.
  Future<void> sendText(
    String hostId,
    int tabId,
    String text, {
    required bool paste,
  });

  Future<void> clearNotification(String hostId, int tabId);

  /// Attaches the tab's data plane at [geometry]. [take] makes this device
  /// the size owner, as switching to a tab does.
  Future<TerminalSession> attach(
    String hostId,
    int tabId,
    Geometry geometry, {
    required bool take,
  });

  /// The app went to the background: release the size of, and detach, every
  /// attached tab. Host mirrors stay.
  Future<void> appPaused();

  /// Back in the foreground: reconnect hosts now. The UI re-attaches the
  /// visible tab itself.
  Future<void> appResumed();
}

/// One attached tab (`TerminalHandle` on the Rust side).
abstract class TerminalSession {
  /// Single-subscription, like an FRB `StreamSink`.
  Stream<TerminalEvent> events();

  /// The next C6 frame payload; empty when nothing changed since the last.
  Future<Uint8List> takeFrame();

  /// A key press. Takes the size first (protocol 7 `TAKE_BEFORE`).
  Future<void> key(KeyInput key);

  /// Text committed by the soft keyboard. Takes the size first.
  Future<void> text(String text);

  /// Bracketed when the program enabled mode 2004. Takes the size first.
  Future<void> paste(String text);

  /// Scrolls local scrollback, or sends wheel reports aimed at cell
  /// ([col], [row]) when the program tracks the mouse. Positive is up, into
  /// history.
  Future<void> scroll(int rows, {required int col, required int row});

  Future<void> resize(Geometry geometry, {required bool focused});

  Future<void> takeSize();

  /// Detaches. Idempotent.
  Future<void> close();
}

/// An error the core reports for a call (a refused op, an unknown host, ...).
class CoreError implements Exception {
  const CoreError(this.message);

  final String message;

  @override
  String toString() => message;
}
