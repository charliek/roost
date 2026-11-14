import 'package:flutter/foundation.dart';

/// A terminal grid plus its cell size in device pixels (logical × DPR), the
/// shape `TerminalHandle::resize` and `attach` take on the Rust side.
@immutable
class Geometry {
  const Geometry({
    required this.cols,
    required this.rows,
    required this.cellWidthPx,
    required this.cellHeightPx,
  });

  final int cols;
  final int rows;
  final int cellWidthPx;
  final int cellHeightPx;

  @override
  bool operator ==(Object other) =>
      other is Geometry &&
      other.cols == cols &&
      other.rows == rows &&
      other.cellWidthPx == cellWidthPx &&
      other.cellHeightPx == cellHeightPx;

  @override
  int get hashCode => Object.hash(cols, rows, cellWidthPx, cellHeightPx);

  @override
  String toString() =>
      'Geometry(${cols}x$rows, ${cellWidthPx}x${cellHeightPx}px)';
}

/// One key press for the Rust `KeyEncoder`.
///
/// [key] is a W3C `KeyboardEvent.key`-style name for named keys (`Enter`,
/// `Escape`, `Backspace`, `Tab`, `ArrowUp`, `PageDown`, `F1`, ...) and the
/// unshifted character for printable keys (`a`, `1`, `/`). [text] is what the
/// key types with its modifiers applied (`A` for shift+a), or null when it
/// types nothing printable (named keys, ctrl combos).
@immutable
class KeyInput {
  const KeyInput(
    this.key, {
    this.text,
    this.ctrl = false,
    this.alt = false,
    this.shift = false,
    this.meta = false,
    this.repeat = false,
  });

  final String key;
  final String? text;
  final bool ctrl;
  final bool alt;
  final bool shift;
  final bool meta;
  final bool repeat;

  KeyInput withModifiers({bool ctrl = false, bool alt = false}) => KeyInput(
    key,
    text: text,
    ctrl: this.ctrl || ctrl,
    alt: this.alt || alt,
    shift: shift,
    meta: meta,
    repeat: repeat,
  );

  @override
  bool operator ==(Object other) =>
      other is KeyInput &&
      other.key == key &&
      other.text == text &&
      other.ctrl == ctrl &&
      other.alt == alt &&
      other.shift == shift &&
      other.meta == meta &&
      other.repeat == repeat;

  @override
  int get hashCode => Object.hash(key, text, ctrl, alt, shift, meta, repeat);

  @override
  String toString() {
    final mods = [
      if (ctrl) 'ctrl',
      if (alt) 'alt',
      if (shift) 'shift',
      if (meta) 'meta',
      if (repeat) 'repeat',
    ];
    return 'KeyInput($key${text == null ? '' : ' "$text"'}'
        '${mods.isEmpty ? '' : ' ${mods.join('+')}'})';
  }
}

enum AgentBadge { none, idle, working, waiting }

enum AttachState { attaching, live, reconnecting, ended }

/// A host's connection state (`HostStateView` on the Rust side).
sealed class HostStateView {
  const HostStateView();
}

final class HostDisconnected extends HostStateView {
  const HostDisconnected();

  @override
  bool operator ==(Object other) => other is HostDisconnected;
  @override
  int get hashCode => (HostDisconnected).hashCode;
}

final class HostConnecting extends HostStateView {
  const HostConnecting();

  @override
  bool operator ==(Object other) => other is HostConnecting;
  @override
  int get hashCode => (HostConnecting).hashCode;
}

final class HostConnected extends HostStateView {
  const HostConnected();

  @override
  bool operator ==(Object other) => other is HostConnected;
  @override
  int get hashCode => (HostConnected).hashCode;
}

final class HostRetrying extends HostStateView {
  const HostRetrying({required this.inMs, required this.reason});

  final int inMs;
  final String reason;

  @override
  bool operator ==(Object other) =>
      other is HostRetrying && other.inMs == inMs && other.reason == reason;
  @override
  int get hashCode => Object.hash(inMs, reason);
}

/// The host key is not pinned yet; the UI shows [fingerprint] (`SHA256:…`)
/// and calls `trustHostKey` with it.
final class HostNeedsTrust extends HostStateView {
  const HostNeedsTrust({required this.fingerprint});

  final String fingerprint;

  @override
  bool operator ==(Object other) =>
      other is HostNeedsTrust && other.fingerprint == fingerprint;
  @override
  int get hashCode => fingerprint.hashCode;
}

/// Terminal until `connect` is called again (a changed host key, a protocol
/// mismatch, auth refused, ...).
final class HostFailed extends HostStateView {
  const HostFailed({required this.reason});

  final String reason;

  @override
  bool operator ==(Object other) =>
      other is HostFailed && other.reason == reason;
  @override
  int get hashCode => reason.hashCode;
}

@immutable
class HostView {
  const HostView({
    required this.id,
    required this.label,
    required this.target,
    required this.state,
    this.waitingTabs = 0,
    this.workingTabs = 0,
  });

  final String id;
  final String label;

  /// `[user@]host[:port]`, or `unix:/path` for a dev session.
  final String target;
  final HostStateView state;
  final int waitingTabs;
  final int workingTabs;

  bool get isConnected => state is HostConnected;
}

@immutable
class WorkspaceView {
  const WorkspaceView({
    required this.projects,
    this.activeProjectId = 0,
    this.activeTabId = 0,
  });

  static const empty = WorkspaceView(projects: []);

  final List<ProjectView> projects;
  final int activeProjectId;
  final int activeTabId;

  TabView? tab(int tabId) {
    for (final p in projects) {
      for (final t in p.tabs) {
        if (t.id == tabId) return t;
      }
    }
    return null;
  }

  ProjectView? projectOfTab(int tabId) {
    for (final p in projects) {
      if (p.tabs.any((t) => t.id == tabId)) return p;
    }
    return null;
  }
}

@immutable
class ProjectView {
  const ProjectView({
    required this.id,
    required this.name,
    required this.cwd,
    required this.tabs,
  });

  final int id;
  final String name;
  final String cwd;
  final List<TabView> tabs;
}

@immutable
class TabView {
  const TabView({
    required this.id,
    required this.title,
    required this.cwd,
    this.agent = AgentBadge.none,
    this.hasNotification = false,
    this.statusText,
  });

  final int id;
  final String title;
  final String cwd;
  final AgentBadge agent;
  final bool hasNotification;

  /// The agent's OSC 7501 message or blocked kind, when it reported one.
  final String? statusText;

  /// Waiting on the user: show quick replies.
  bool get needsInput => agent == AgentBadge.waiting || hasNotification;

  TabView copyWith({
    String? title,
    AgentBadge? agent,
    bool? hasNotification,
    String? statusText,
  }) => TabView(
    id: id,
    title: title ?? this.title,
    cwd: cwd,
    agent: agent ?? this.agent,
    hasNotification: hasNotification ?? this.hasNotification,
    statusText: statusText ?? this.statusText,
  );
}

/// Events from one attached tab, in the order the core produced them.
sealed class TerminalEvent {
  const TerminalEvent();
}

/// New screen content is ready; pull it with `takeFrame` (once per vsync).
final class FrameReady extends TerminalEvent {
  const FrameReady();
}

final class StatusChanged extends TerminalEvent {
  const StatusChanged(this.state, [this.message]);

  final AttachState state;
  final String? message;
}

/// Who sets the tab's PTY size. [label] is the owning client's device name
/// (null when unknown); [isMe] when this device owns it.
final class OwnerChanged extends TerminalEvent {
  const OwnerChanged({
    required this.label,
    required this.isMe,
    required this.cols,
    required this.rows,
  });

  final String? label;
  final bool isMe;
  final int cols;
  final int rows;
}

final class Exited extends TerminalEvent {
  const Exited(this.code);

  final int code;
}

/// A program in the tab wrote the clipboard (OSC 52), as Claude Code's
/// "c to copy" does. Only the tab on screen gets one.
final class ClipboardWrite extends TerminalEvent {
  const ClipboardWrite(this.text);

  final String text;
}
