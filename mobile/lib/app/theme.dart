import 'package:flutter/material.dart';

import '../core/models.dart';

/// The mockup's palette (dark only for the MVP: terminal colors come from
/// the frames, and the chrome is built to sit around them).
abstract final class RoostColors {
  static const background = Color(0xFF0F1115);
  static const surfaceLow = Color(0xFF121419);
  static const surface = Color(0xFF161920);
  static const surfaceHigh = Color(0xFF181B21);
  static const surfaceHighest = Color(0xFF1D2027);
  static const border = Color(0xFF2B2F38);
  static const divider = Color(0xFF23262E);
  static const text = Color(0xFFE6E8EE);
  static const muted = Color(0xFF9AA3B2);
  static const waiting = Color(0xFFF2A65A);
  static const waitingText = Color(0xFFF6D2A8);
  static const waitingSurface = Color(0xFF17130D);
  static const waitingBorder = Color(0xFF4A3A26);
  static const working = Color(0xFF79AAFF);
  static const idle = Color(0xFF5C6370);
  static const connected = Color(0xFF5FC38F);
  static const error = Color(0xFFF08A8A);
  static const errorSurface = Color(0xFF1A1213);
  static const errorBorder = Color(0xFF5A2F2F);
}

const monoFamily = 'JetBrains Mono';

ThemeData roostDarkTheme() {
  final scheme =
      ColorScheme.fromSeed(
        seedColor: RoostColors.working,
        brightness: Brightness.dark,
      ).copyWith(
        surface: RoostColors.background,
        onSurface: RoostColors.text,
        onSurfaceVariant: RoostColors.muted,
        surfaceContainerLowest: RoostColors.background,
        surfaceContainerLow: RoostColors.surfaceLow,
        surfaceContainer: RoostColors.surface,
        surfaceContainerHigh: RoostColors.surfaceHigh,
        surfaceContainerHighest: RoostColors.surfaceHighest,
        outline: const Color(0xFF2F343E),
        outlineVariant: RoostColors.divider,
        error: RoostColors.error,
      );
  return ThemeData(
    colorScheme: scheme,
    scaffoldBackgroundColor: RoostColors.background,
    dividerTheme: const DividerThemeData(color: RoostColors.divider, space: 1),
    cardTheme: const CardThemeData(
      color: RoostColors.surface,
      elevation: 0,
      margin: EdgeInsets.zero,
      shape: RoundedRectangleBorder(
        side: BorderSide(color: RoostColors.border),
        borderRadius: BorderRadius.all(Radius.circular(14)),
      ),
    ),
    listTileTheme: const ListTileThemeData(
      selectedColor: RoostColors.text,
      selectedTileColor: RoostColors.surfaceHighest,
    ),
  );
}

Color agentColor(AgentBadge badge) => switch (badge) {
  AgentBadge.waiting => RoostColors.waiting,
  AgentBadge.working => RoostColors.working,
  AgentBadge.idle => RoostColors.idle,
  AgentBadge.none => RoostColors.idle,
};

Color hostStateColor(HostStateView state) => switch (state) {
  HostConnected() => RoostColors.connected,
  HostConnecting() => RoostColors.working,
  HostRetrying() => RoostColors.muted,
  HostNeedsTrust() => RoostColors.waiting,
  HostFailed() => RoostColors.error,
  HostDisconnected() => RoostColors.idle,
};

String hostStateLabel(HostStateView state) => switch (state) {
  HostConnected() => 'connected',
  HostConnecting() => 'connecting…',
  HostRetrying(:final inMs) => 'retrying in ${(inMs / 1000).ceil()} s',
  HostNeedsTrust() => 'needs trust',
  HostFailed() => 'failed',
  HostDisconnected() => 'disconnected',
};
