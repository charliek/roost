import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../core/roost_core.dart';
import '../hosts/tab_strip.dart' show tabStripHeight;
import '../terminal/terminal_painter.dart' show terminalColor;
import '../terminal/terminal_pane.dart' show terminalToolbarHeight;
import '../terminal/terminal_view.dart';
import 'expanded_layout.dart' show hostSidebarWidth;
import 'medium_layout.dart' show hostRailWidth;
import 'providers.dart';
import 'shared_widgets.dart' show layoutDividerWidth;
import 'text_scale.dart';
import 'theme.dart';
import 'width_class.dart';

/// Light enough that the terminal behind shows each change as it is made.
const _barrier = Color(0x33000000);

/// "13", "13.5".
String formatPoints(double size) =>
    size == size.roundToDouble() ? size.toInt().toString() : '$size';

/// Opens the Display settings: a bottom sheet on a phone-width layout, a
/// dialog beside a rail or sidebar.
Future<void> showDisplaySheet(BuildContext context) {
  if (WidthClassScope.of(context) == WidthClass.compact) {
    return showModalBottomSheet<void>(
      context: context,
      isScrollControlled: true,
      useSafeArea: true,
      barrierColor: _barrier,
      backgroundColor: Colors.transparent,
      builder: (context) => DraggableScrollableSheet(
        expand: false,
        initialChildSize: 0.6,
        maxChildSize: 0.92,
        builder: (context, controller) =>
            SheetSurface(child: DisplaySettings(controller: controller)),
      ),
    );
  }
  return showDialog<void>(
    context: context,
    barrierColor: _barrier,
    builder: (context) => Dialog(
      backgroundColor: context.palette.surface,
      child: ConstrainedBox(
        constraints: BoxConstraints(
          maxWidth: 560,
          maxHeight: MediaQuery.sizeOf(context).height * 0.8,
        ),
        child: const DisplaySettings(),
      ),
    ),
  );
}

/// The gear that opens [showDisplaySheet], beside add-host in each layout.
class DisplayButton extends StatelessWidget {
  const DisplayButton({super.key});

  @override
  Widget build(BuildContext context) {
    return IconButton(
      key: const ValueKey('open-display'),
      tooltip: 'Display',
      icon: const Icon(Icons.settings_outlined),
      onPressed: () => unawaited(showDisplaySheet(context)),
    );
  }
}

/// Where a terminal's grid would go with none on screen: the layout's
/// terminal area, less the rail or sidebar, the toolbar and the tab strip.
Size estimatedGridArea(Size screen, WidthClass widthClass) {
  final (side, strip) = switch (widthClass) {
    WidthClass.compact => (0.0, 0.0),
    WidthClass.medium => (hostRailWidth + layoutDividerWidth, tabStripHeight),
    WidthClass.expanded => (
      hostSidebarWidth + layoutDividerWidth,
      tabStripHeight,
    ),
  };
  return terminalGridArea(
    Size(screen.width - side, screen.height - terminalToolbarHeight - strip),
  );
}

/// Every display setting, applied as it changes. A slider's drag is written
/// once it stops; everything else at once.
class DisplaySettings extends ConsumerWidget {
  const DisplaySettings({super.key, this.controller});

  final ScrollController? controller;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final model = ref.watch(settingsProvider);
    final settings = ref.read(settingsProvider.notifier);
    final desired = model.desired;
    final palette = context.palette;
    final text = Theme.of(context).textTheme;
    final muted = text.bodySmall?.copyWith(color: palette.muted);

    final screen = MediaQuery.sizeOf(context);
    final widthClass = WidthClassScope.of(context);
    final fontSize = desired.terminalFontSizeFor(widthClass);
    final pane = ref.watch(terminalPaneSizeProvider);
    final cols = terminalGeometry(
      fontSize,
      MediaQuery.devicePixelRatioOf(context),
      pane?.gridArea ?? estimatedGridArea(screen, widthClass),
    ).cols;
    final sizedBy = pane?.sizedBy;
    void setFont(double size, {bool continuous = false}) => settings.edit(
      (s) => s.withTerminalFontSize(widthClass, size),
      continuous: continuous,
    );
    final uiScale = desired.uiTextScale.clamp(minUiTextScale, maxUiTextScale);

    return ListView(
      key: const ValueKey('display-sheet'),
      controller: controller,
      shrinkWrap: controller == null,
      padding: EdgeInsets.fromLTRB(
        20,
        16,
        20,
        24 + MediaQuery.paddingOf(context).bottom,
      ),
      children: [
        Text('Display', style: text.titleLarge),
        const _Heading('Appearance'),
        SegmentedButton<Appearance>(
          key: const ValueKey('display-appearance'),
          showSelectedIcon: false,
          segments: const [
            ButtonSegment(value: Appearance.system, label: Text('System')),
            ButtonSegment(value: Appearance.dark, label: Text('Dark')),
            ButtonSegment(value: Appearance.light, label: Text('Light')),
          ],
          selected: {desired.appearance},
          onSelectionChanged: (picked) =>
              settings.edit((s) => s.copyWith(appearance: picked.single)),
        ),
        const _Heading('Terminal text'),
        Row(
          children: [
            Expanded(
              child: Text(
                '${formatPoints(fontSize)} pt · '
                '${pane == null ? 'about ' : ''}$cols columns',
                key: const ValueKey('terminal-text-label'),
              ),
            ),
            TextButton(
              key: const ValueKey('terminal-text-reset'),
              onPressed: () => setFont(MobileSettings.defaultTerminalFontSize),
              child: const Text('Reset'),
            ),
          ],
        ),
        if (sizedBy != null)
          Text(
            'This tab is sized by $sizedBy',
            key: const ValueKey('terminal-text-sized-by'),
            style: muted,
          ),
        Slider(
          key: const ValueKey('terminal-text-slider'),
          min: minTerminalFontSize,
          max: maxTerminalFontSize,
          divisions: ((maxTerminalFontSize - minTerminalFontSize) * 2).round(),
          value: fontSize.clamp(minTerminalFontSize, maxTerminalFontSize),
          onChanged: (v) => setFont(v, continuous: true),
          onChangeEnd: (_) => settings.flush(),
        ),
        const _Heading('App text'),
        Text(
          '${(uiScale * 100).round()}%',
          key: const ValueKey('app-text-label'),
        ),
        Slider(
          key: const ValueKey('app-text-slider'),
          min: minUiTextScale,
          max: maxUiTextScale,
          divisions: ((maxUiTextScale - minUiTextScale) / uiTextScaleStep)
              .round(),
          value: uiScale,
          onChanged: (v) => settings.edit(
            (s) => s.copyWith(uiTextScale: v),
            continuous: true,
          ),
          onChangeEnd: (_) => settings.flush(),
        ),
        const _ThemeSection(),
        const _Heading('Readability'),
        const Text('Keep text readable'),
        const SizedBox(height: 8),
        SegmentedButton<ReadableText>(
          key: const ValueKey('readable-text'),
          showSelectedIcon: false,
          segments: const [
            ButtonSegment(value: ReadableText.auto, label: Text('Auto')),
            ButtonSegment(value: ReadableText.on, label: Text('On')),
            ButtonSegment(value: ReadableText.off, label: Text('Off')),
          ],
          selected: {desired.readableText},
          onSelectionChanged: (picked) =>
              settings.edit((s) => s.copyWith(readableText: picked.single)),
        ),
        const SizedBox(height: 4),
        Text('Auto: on when the background is light', style: muted),
        SwitchListTile(
          key: const ValueKey('bright-faint'),
          contentPadding: EdgeInsets.zero,
          title: const Text('Brighter dim text'),
          value: desired.brightFaint,
          onChanged: (on) => settings.edit((s) => s.copyWith(brightFaint: on)),
        ),
        const SizedBox(height: 8),
        Text.rich(
          TextSpan(
            children: [
              const TextSpan(text: 'Claude Code: '),
              _code('/theme'),
              const TextSpan(text: ' → '),
              _code('dark-ansi'),
              const TextSpan(text: ' or '),
              _code('light-ansi'),
              const TextSpan(text: ' uses these colors.'),
            ],
          ),
          key: const ValueKey('display-help'),
          style: muted,
        ),
      ],
    );
  }

  static TextSpan _code(String text) => TextSpan(
    text: text,
    style: const TextStyle(fontFamily: monoFamily),
  );
}

class _Heading extends StatelessWidget {
  const _Heading(this.text);

  final String text;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.only(top: 20, bottom: 8),
      child: Text(text, style: Theme.of(context).textTheme.titleSmall),
    );
  }
}

/// Watches only what it draws, so a slider's ticks do not rebuild the swatches.
class _ThemeSection extends ConsumerWidget {
  const _ThemeSection();

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final (:dark, :selected) = ref.watch(
      settingsProvider.select((m) {
        final dark = m.acknowledged.effectiveDark;
        return (
          dark: dark,
          selected: dark ? m.desired.darkTheme : m.desired.lightTheme,
        );
      }),
    );
    final themes = [
      for (final t in ref.watch(coreProvider).terminalThemes())
        if (t.light != dark) t,
    ];
    final settings = ref.read(settingsProvider.notifier);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _Heading(dark ? 'Dark terminal theme' : 'Light terminal theme'),
        _ThemeSwatches(
          themes: themes,
          selected: selected,
          onPick: (name) => settings.edit(
            (s) => dark
                ? s.copyWith(darkTheme: name)
                : s.copyWith(lightTheme: name),
          ),
        ),
      ],
    );
  }
}

class _ThemeSwatches extends StatelessWidget {
  const _ThemeSwatches({
    required this.themes,
    required this.selected,
    required this.onPick,
  });

  final List<ThemeSummary> themes;
  final String selected;
  final ValueChanged<String> onPick;

  static const _gap = 8.0;

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        final perRow = (constraints.maxWidth / 150).floor().clamp(1, 4);
        final width = (constraints.maxWidth - _gap * (perRow - 1)) / perRow;
        return Wrap(
          spacing: _gap,
          runSpacing: _gap,
          children: [
            for (final theme in themes)
              SizedBox(
                width: width,
                child: _Swatch(
                  theme: theme,
                  selected: theme.name == selected,
                  onTap: () => onPick(theme.name),
                ),
              ),
          ],
        );
      },
    );
  }
}

class _Swatch extends StatelessWidget {
  const _Swatch({
    required this.theme,
    required this.selected,
    required this.onTap,
  });

  final ThemeSummary theme;
  final bool selected;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final palette = context.palette;
    final fg = terminalColor(theme.foreground);
    final radius = BorderRadius.circular(10);
    return Semantics(
      selected: selected,
      button: true,
      child: Material(
        key: ValueKey('theme-swatch-${theme.name}'),
        color: terminalColor(theme.background),
        shape: RoundedRectangleBorder(
          borderRadius: radius,
          side: selected
              ? BorderSide(color: palette.working, width: 2)
              : BorderSide(color: palette.border),
        ),
        clipBehavior: Clip.antiAlias,
        child: InkWell(
          onTap: onTap,
          child: Padding(
            padding: const EdgeInsets.all(10),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Text(
                  theme.name,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(color: fg, fontSize: 12),
                ),
                const SizedBox(height: 4),
                Text(
                  r'$ ls -la',
                  maxLines: 1,
                  overflow: TextOverflow.clip,
                  style: TextStyle(
                    color: fg,
                    fontFamily: monoFamily,
                    fontSize: 12,
                  ),
                ),
                const SizedBox(height: 8),
                Row(
                  children: [
                    for (var i = 0; i < 8; i++) ...[
                      if (i > 0) const SizedBox(width: 2),
                      Expanded(
                        child: Container(
                          height: 8,
                          color: terminalColor(theme.ansi[i]),
                        ),
                      ),
                    ],
                  ],
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}
