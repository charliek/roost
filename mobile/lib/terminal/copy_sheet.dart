import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../app/theme.dart';
import 'cell_metrics.dart';
import 'screen_text.dart';
import 'terminal_model.dart';
import 'terminal_view.dart' show minTerminalFontSize;

const _sheetFontSize = 13.0;

/// The terminal is a canvas, so its text cannot be selected in place: this
/// sheet shows the screen as selectable text, with its links listed whole.
Future<void> showCopySheet(BuildContext context, TerminalModel model) {
  final text = screenText(model);
  final urls = screenUrls(model);
  final cols = model.cols;
  final messenger = ScaffoldMessenger.maybeOf(context);
  return showModalBottomSheet<void>(
    context: context,
    isScrollControlled: true,
    useSafeArea: true,
    backgroundColor: RoostColors.surface,
    builder: (sheet) {
      Future<void> copy(String value, String what) async {
        await Clipboard.setData(ClipboardData(text: value));
        if (sheet.mounted) Navigator.of(sheet).pop();
        messenger?.showSnackBar(SnackBar(content: Text('$what copied')));
      }

      return _CopySheet(
        text: text,
        urls: urls,
        cols: cols,
        onCopyAll: () => copy(text, 'Screen'),
        onCopyUrl: (url) => copy(url, 'Link'),
      );
    },
  );
}

class _CopySheet extends StatefulWidget {
  const _CopySheet({
    required this.text,
    required this.urls,
    required this.cols,
    required this.onCopyAll,
    required this.onCopyUrl,
  });

  final String text;
  final List<String> urls;
  final int cols;
  final VoidCallback onCopyAll;
  final ValueChanged<String> onCopyUrl;

  @override
  State<_CopySheet> createState() => _CopySheetState();
}

class _CopySheetState extends State<_CopySheet> {
  final _scrolling = ValueNotifier(false);
  final _viewport = GlobalKey();

  @override
  void dispose() {
    _scrolling.dispose();
    super.dispose();
  }

  /// The selection's menu, hidden while the text scrolls and while the
  /// selection is out of sight, where it would be drawn over the header.
  Widget _selectionMenu(BuildContext context, SelectableRegionState region) =>
      ValueListenableBuilder<bool>(
        valueListenable: _scrolling,
        builder: (context, scrolling, _) {
          final viewport = _viewport.currentContext?.findRenderObject();
          final regionBox = region.context.findRenderObject();
          final inSight =
              viewport is RenderBox &&
              regionBox is RenderBox &&
              (viewport.localToGlobal(Offset.zero) & viewport.size)
                  .inflate(1)
                  .contains(
                    TextSelectionToolbarAnchors.getSelectionRect(
                      regionBox,
                      region.startGlyphHeight,
                      region.endGlyphHeight,
                      region.selectionEndpoints,
                    ).topCenter,
                  );
          if (scrolling || !inSight) return const SizedBox.shrink();
          return AdaptiveTextSelectionToolbar.selectableRegion(
            selectableRegionState: region,
          );
        },
      );

  /// Small enough that each terminal row stays one line, so the screen's
  /// own line breaks are the ones the text shows; a tab too wide for that
  /// at [minTerminalFontSize] (about 90 columns on a phone) wraps.
  double _fontSizeFor(double width, double devicePixelRatio) {
    final cols = widget.cols;
    if (cols <= 0) return _sheetFontSize;
    final em =
        CellMetrics.measure(_sheetFontSize, devicePixelRatio).cellWidth /
        _sheetFontSize;
    final fit = width * 0.99 / (cols * em);
    return fit.clamp(minTerminalFontSize, _sheetFontSize);
  }

  @override
  Widget build(BuildContext context) {
    final text = widget.text;
    final urls = widget.urls;
    final theme = Theme.of(context).textTheme;
    final height = MediaQuery.sizeOf(context).height * 0.85;
    // The sheet runs under the navigation bar on an edge-to-edge screen.
    final bottomInset = MediaQuery.paddingOf(context).bottom;
    return SizedBox(
      key: const ValueKey('copy-sheet'),
      height: height,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 8, 8, 0),
            child: Row(
              children: [
                Expanded(child: Text('Copy text', style: theme.titleMedium)),
                TextButton(
                  key: const ValueKey('copy-all'),
                  onPressed: text.isEmpty ? null : widget.onCopyAll,
                  child: const Text('Copy all'),
                ),
                const CloseButton(),
              ],
            ),
          ),
          if (urls.isNotEmpty) ...[
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 4, 16, 4),
              child: Text(
                'Links',
                style: theme.labelMedium?.copyWith(color: RoostColors.muted),
              ),
            ),
            ConstrainedBox(
              constraints: BoxConstraints(
                maxHeight: math.min(200, height * 0.3),
              ),
              child: ListView(
                shrinkWrap: true,
                padding: EdgeInsets.zero,
                children: [
                  for (final (i, url) in urls.indexed)
                    ListTile(
                      key: ValueKey('copy-url-$i'),
                      dense: true,
                      leading: const Icon(Icons.link, size: 20),
                      title: Text(
                        url,
                        maxLines: 2,
                        overflow: TextOverflow.ellipsis,
                        style: theme.bodySmall?.copyWith(
                          fontFamily: monoFamily,
                        ),
                      ),
                      trailing: const Icon(Icons.copy, size: 18),
                      onTap: () => widget.onCopyUrl(url),
                    ),
                ],
              ),
            ),
          ],
          const Divider(height: 1),
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 8, 16, 0),
            child: Text(
              'Long-press to select, then drag the handles.',
              style: theme.bodySmall?.copyWith(color: RoostColors.muted),
            ),
          ),
          Expanded(
            child: LayoutBuilder(
              builder: (context, constraints) {
                final fontSize = _fontSizeFor(
                  constraints.maxWidth - 32,
                  MediaQuery.devicePixelRatioOf(context),
                );
                // SelectionArea over the scroll view, not a SelectableText
                // inside it: the scroll view then hides handles scrolled out
                // of sight and scrolls while a handle is dragged to its edge.
                // The band below the viewport, outside the selection so a
                // press there selects nothing hidden, is that edge: a dragged
                // handle aims above the finger, so a viewport that ran to the
                // bottom of the screen could never be dragged past.
                return Padding(
                  padding: EdgeInsets.only(bottom: bottomInset + 40),
                  child: SelectionArea(
                    contextMenuBuilder: _selectionMenu,
                    child: NotificationListener<ScrollNotification>(
                      onNotification: (n) {
                        if (n is ScrollStartNotification) {
                          _scrolling.value = true;
                        } else if (n is ScrollEndNotification) {
                          _scrolling.value = false;
                        }
                        return false;
                      },
                      child: SingleChildScrollView(
                        key: _viewport,
                        padding: const EdgeInsets.all(16),
                        child: Text(
                          text.isEmpty ? '(the screen is empty)' : text,
                          key: const ValueKey('copy-text'),
                          textScaler: TextScaler.noScaling,
                          style: TextStyle(
                            fontFamily: monoFamily,
                            fontSize: fontSize,
                            height: 1.3,
                            letterSpacing: 0,
                            wordSpacing: 0,
                            fontFeatures: const [
                              FontFeature.disable('liga'),
                              FontFeature.disable('calt'),
                            ],
                            color: RoostColors.text,
                          ),
                        ),
                      ),
                    ),
                  ),
                );
              },
            ),
          ),
        ],
      ),
    );
  }
}
