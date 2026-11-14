import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../app/theme.dart';
import 'screen_text.dart';
import 'terminal_model.dart';

/// The terminal is a canvas, so its text cannot be selected in place: this
/// sheet shows the screen as selectable text, with its links listed whole.
Future<void> showCopySheet(BuildContext context, TerminalModel model) {
  final text = screenText(model);
  final urls = screenUrls(model);
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
    required this.onCopyAll,
    required this.onCopyUrl,
  });

  final String text;
  final List<String> urls;
  final VoidCallback onCopyAll;
  final ValueChanged<String> onCopyUrl;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context).textTheme;
    return SizedBox(
      key: const ValueKey('copy-sheet'),
      height: MediaQuery.sizeOf(context).height * 0.85,
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
              constraints: const BoxConstraints(maxHeight: 200),
              child: ListView(
                shrinkWrap: true,
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
            child: SingleChildScrollView(
              padding: const EdgeInsets.all(16),
              child: SelectableText(
                text.isEmpty ? '(the screen is empty)' : text,
                key: const ValueKey('copy-text'),
                style: const TextStyle(
                  fontFamily: monoFamily,
                  fontSize: 13,
                  height: 1.3,
                  color: RoostColors.text,
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }
}
