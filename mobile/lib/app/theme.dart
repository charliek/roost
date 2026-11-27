import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../core/models.dart';

/// The chrome colors, read through `context.palette`. Terminal cell colors
/// come from the frames and stay in the painter.
@immutable
class RoostPalette extends ThemeExtension<RoostPalette> {
  const RoostPalette({
    required this.background,
    required this.surfaceLow,
    required this.surface,
    required this.surfaceHigh,
    required this.surfaceHighest,
    required this.border,
    required this.divider,
    required this.outline,
    required this.text,
    required this.muted,
    required this.waiting,
    required this.waitingText,
    required this.waitingAction,
    required this.waitingSurface,
    required this.waitingBorder,
    required this.waitingPill,
    required this.working,
    required this.workingPill,
    required this.idle,
    required this.connected,
    required this.error,
    required this.errorSurface,
    required this.errorBorder,
  });

  static const dark = RoostPalette(
    background: Color(0xFF0F1115),
    surfaceLow: Color(0xFF121419),
    surface: Color(0xFF161920),
    surfaceHigh: Color(0xFF181B21),
    surfaceHighest: Color(0xFF1D2027),
    border: Color(0xFF2B2F38),
    divider: Color(0xFF23262E),
    outline: Color(0xFF2F343E),
    text: Color(0xFFE6E8EE),
    muted: Color(0xFF9AA3B2),
    waiting: Color(0xFFF2A65A),
    waitingText: Color(0xFFF6D2A8),
    waitingAction: Color(0xFFF2A65A),
    waitingSurface: Color(0xFF17130D),
    waitingBorder: Color(0xFF4A3A26),
    waitingPill: Color(0xFF3A2A17),
    working: Color(0xFF79AAFF),
    workingPill: Color(0xFF172338),
    idle: Color(0xFF5C6370),
    connected: Color(0xFF5FC38F),
    error: Color(0xFFF08A8A),
    errorSurface: Color(0xFF1A1213),
    errorBorder: Color(0xFF5A2F2F),
  );

  static const light = RoostPalette(
    background: Color(0xFFF6F7F9),
    surfaceLow: Color(0xFFFFFFFF),
    surface: Color(0xFFFFFFFF),
    surfaceHigh: Color(0xFFF0F2F5),
    surfaceHighest: Color(0xFFE9ECF1),
    border: Color(0xFFD6DAE1),
    divider: Color(0xFFE3E6EB),
    outline: Color(0xFFC8CDD6),
    text: Color(0xFF1B1E24),
    muted: Color(0xFF5B6372),
    waiting: Color(0xFF9A5407),
    waitingText: Color(0xFF8A4A08),
    waitingAction: Color(0xFF8A4A08),
    waitingSurface: Color(0xFFFFF4E6),
    waitingBorder: Color(0xFFEFCF9F),
    waitingPill: Color(0xFFFCE9D2),
    working: Color(0xFF2A62C9),
    workingPill: Color(0xFFE8F0FE),
    idle: Color(0xFF6B7280),
    connected: Color(0xFF17784A),
    error: Color(0xFFC23B3B),
    errorSurface: Color(0xFFFDECEC),
    errorBorder: Color(0xFFF2B8B8),
  );

  final Color background;
  final Color surfaceLow;
  final Color surface;
  final Color surfaceHigh;
  final Color surfaceHighest;
  final Color border;
  final Color divider;
  final Color outline;
  final Color text;
  final Color muted;
  final Color waiting;
  final Color waitingText;
  final Color waitingAction;
  final Color waitingSurface;
  final Color waitingBorder;
  final Color waitingPill;
  final Color working;
  final Color workingPill;
  final Color idle;
  final Color connected;
  final Color error;
  final Color errorSurface;
  final Color errorBorder;

  @override
  RoostPalette copyWith({
    Color? background,
    Color? surfaceLow,
    Color? surface,
    Color? surfaceHigh,
    Color? surfaceHighest,
    Color? border,
    Color? divider,
    Color? outline,
    Color? text,
    Color? muted,
    Color? waiting,
    Color? waitingText,
    Color? waitingAction,
    Color? waitingSurface,
    Color? waitingBorder,
    Color? waitingPill,
    Color? working,
    Color? workingPill,
    Color? idle,
    Color? connected,
    Color? error,
    Color? errorSurface,
    Color? errorBorder,
  }) => RoostPalette(
    background: background ?? this.background,
    surfaceLow: surfaceLow ?? this.surfaceLow,
    surface: surface ?? this.surface,
    surfaceHigh: surfaceHigh ?? this.surfaceHigh,
    surfaceHighest: surfaceHighest ?? this.surfaceHighest,
    border: border ?? this.border,
    divider: divider ?? this.divider,
    outline: outline ?? this.outline,
    text: text ?? this.text,
    muted: muted ?? this.muted,
    waiting: waiting ?? this.waiting,
    waitingText: waitingText ?? this.waitingText,
    waitingAction: waitingAction ?? this.waitingAction,
    waitingSurface: waitingSurface ?? this.waitingSurface,
    waitingBorder: waitingBorder ?? this.waitingBorder,
    waitingPill: waitingPill ?? this.waitingPill,
    working: working ?? this.working,
    workingPill: workingPill ?? this.workingPill,
    idle: idle ?? this.idle,
    connected: connected ?? this.connected,
    error: error ?? this.error,
    errorSurface: errorSurface ?? this.errorSurface,
    errorBorder: errorBorder ?? this.errorBorder,
  );

  @override
  RoostPalette lerp(RoostPalette? other, double t) {
    if (other == null) return this;
    return RoostPalette(
      background: Color.lerp(background, other.background, t)!,
      surfaceLow: Color.lerp(surfaceLow, other.surfaceLow, t)!,
      surface: Color.lerp(surface, other.surface, t)!,
      surfaceHigh: Color.lerp(surfaceHigh, other.surfaceHigh, t)!,
      surfaceHighest: Color.lerp(surfaceHighest, other.surfaceHighest, t)!,
      border: Color.lerp(border, other.border, t)!,
      divider: Color.lerp(divider, other.divider, t)!,
      outline: Color.lerp(outline, other.outline, t)!,
      text: Color.lerp(text, other.text, t)!,
      muted: Color.lerp(muted, other.muted, t)!,
      waiting: Color.lerp(waiting, other.waiting, t)!,
      waitingText: Color.lerp(waitingText, other.waitingText, t)!,
      waitingAction: Color.lerp(waitingAction, other.waitingAction, t)!,
      waitingSurface: Color.lerp(waitingSurface, other.waitingSurface, t)!,
      waitingBorder: Color.lerp(waitingBorder, other.waitingBorder, t)!,
      waitingPill: Color.lerp(waitingPill, other.waitingPill, t)!,
      working: Color.lerp(working, other.working, t)!,
      workingPill: Color.lerp(workingPill, other.workingPill, t)!,
      idle: Color.lerp(idle, other.idle, t)!,
      connected: Color.lerp(connected, other.connected, t)!,
      error: Color.lerp(error, other.error, t)!,
      errorSurface: Color.lerp(errorSurface, other.errorSurface, t)!,
      errorBorder: Color.lerp(errorBorder, other.errorBorder, t)!,
    );
  }
}

extension RoostPaletteX on BuildContext {
  /// Falls back to the dark palette so a bare `MaterialApp` still builds.
  RoostPalette get palette =>
      Theme.of(this).extension<RoostPalette>() ?? RoostPalette.dark;
}

const monoFamily = 'JetBrains Mono';

/// A bottom sheet's background, painted inside the route: the color a
/// sheet route is opened with is fixed until it closes, and this one follows
/// an appearance change made while the sheet shows. Open the sheet with a
/// transparent `backgroundColor`.
class SheetSurface extends StatelessWidget {
  const SheetSurface({super.key, required this.child});

  final Widget child;

  @override
  Widget build(BuildContext context) => Material(
    color: context.palette.surface,
    shape: const RoundedRectangleBorder(
      borderRadius: BorderRadius.vertical(top: Radius.circular(28)),
    ),
    clipBehavior: Clip.antiAlias,
    child: child,
  );
}

/// Icon brightness is the opposite of the chrome's. One style for the app
/// root and the AppBar, so a route with a bar and one without agree.
SystemUiOverlayStyle systemOverlayStyleFor(
  Brightness brightness,
  RoostPalette palette,
) {
  final icons = brightness == Brightness.dark
      ? Brightness.light
      : Brightness.dark;
  return SystemUiOverlayStyle(
    statusBarColor: Colors.transparent,
    statusBarIconBrightness: icons,
    statusBarBrightness: brightness,
    systemNavigationBarColor: palette.background,
    systemNavigationBarIconBrightness: icons,
    systemNavigationBarDividerColor: palette.background,
  );
}

ThemeData roostTheme(Brightness brightness) {
  final palette = brightness == Brightness.dark
      ? RoostPalette.dark
      : RoostPalette.light;
  final scheme =
      ColorScheme.fromSeed(
        seedColor: palette.working,
        brightness: brightness,
      ).copyWith(
        surface: palette.background,
        onSurface: palette.text,
        onSurfaceVariant: palette.muted,
        surfaceContainerLowest: palette.background,
        surfaceContainerLow: palette.surfaceLow,
        surfaceContainer: palette.surface,
        surfaceContainerHigh: palette.surfaceHigh,
        surfaceContainerHighest: palette.surfaceHighest,
        outline: palette.outline,
        outlineVariant: palette.divider,
        error: palette.error,
      );
  return ThemeData(
    colorScheme: scheme,
    scaffoldBackgroundColor: palette.background,
    extensions: [palette],
    appBarTheme: AppBarTheme(
      systemOverlayStyle: systemOverlayStyleFor(brightness, palette),
    ),
    dividerTheme: DividerThemeData(color: palette.divider, space: 1),
    cardTheme: CardThemeData(
      color: palette.surface,
      elevation: 0,
      margin: EdgeInsets.zero,
      shape: RoundedRectangleBorder(
        side: BorderSide(color: palette.border),
        borderRadius: const BorderRadius.all(Radius.circular(14)),
      ),
    ),
    listTileTheme: ListTileThemeData(
      selectedColor: palette.text,
      selectedTileColor: palette.surfaceHighest,
    ),
  );
}

Color agentColor(RoostPalette palette, AgentBadge badge) => switch (badge) {
  AgentBadge.waiting => palette.waiting,
  AgentBadge.working => palette.working,
  AgentBadge.idle => palette.idle,
  AgentBadge.none => palette.idle,
};

Color hostStateColor(RoostPalette palette, HostStateView state) =>
    switch (state) {
      HostConnected() => palette.connected,
      HostConnecting() => palette.working,
      HostRetrying() => palette.muted,
      HostNeedsTrust() => palette.waiting,
      HostFailed() => palette.error,
      HostDisconnected() => palette.idle,
    };

String hostStateLabel(HostStateView state) => switch (state) {
  HostConnected() => 'connected',
  HostConnecting() => 'connecting…',
  HostRetrying(:final inMs) => 'retrying in ${(inMs / 1000).ceil()} s',
  HostNeedsTrust() => 'needs trust',
  HostFailed() => 'failed',
  HostDisconnected() => 'disconnected',
};
