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

class _CopySheet extends StatelessWidget {
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

  /// Small enough that each terminal row stays one line, so the screen's
  /// own line breaks are the ones the text shows.
  double _fontSizeFor(double width, double devicePixelRatio) {
    if (cols <= 0) return _sheetFontSize;
    final em =
        CellMetrics.measure(_sheetFontSize, devicePixelRatio).cellWidth /
        _sheetFontSize;
    final fit = width * 0.99 / (cols * em);
    return fit.clamp(minTerminalFontSize, _sheetFontSize);
  }

  @override
  Widget build(BuildContext context) {
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
                  onPressed: text.isEmpty ? null : onCopyAll,
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
                      onTap: () => onCopyUrl(url),
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
                return SelectionArea(
                  child: SingleChildScrollView(
                    padding: EdgeInsets.fromLTRB(16, 16, 16, 16 + bottomInset),
                    child: Text(
                      text.isEmpty ? '(the screen is empty)' : text,
                      key: const ValueKey('copy-text'),
                      style: TextStyle(
                        fontFamily: monoFamily,
                        fontSize: fontSize,
                        height: 1.3,
                        color: RoostColors.text,
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
