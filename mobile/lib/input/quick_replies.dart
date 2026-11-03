import 'package:flutter/material.dart';

import '../app/theme.dart';
import '../hosts/host_widgets.dart';

typedef QuickReply = ({String id, String label, String bytes});

/// Keys, not words: agent prompts are numbered menus, so a "Yes" chip would
/// type the word. Sent raw over `tab.write`, which never takes the size.
const List<QuickReply> quickReplies = [
  (id: '1', label: '1', bytes: '1'),
  (id: '2', label: '2', bytes: '2'),
  (id: '3', label: '3', bytes: '3'),
  (id: 'enter', label: 'Enter', bytes: '\r'),
  (id: 'esc', label: 'Esc', bytes: '\x1b'),
  (id: 'up', label: '↑', bytes: '\x1b[A'),
  (id: 'down', label: '↓', bytes: '\x1b[B'),
];

/// Below this width the bar drops its label for the keys.
const compactQuickRepliesWidth = 560.0;

class QuickRepliesBar extends StatelessWidget {
  const QuickRepliesBar({super.key, required this.onReply, this.statusText});

  final ValueChanged<String> onReply;
  final String? statusText;

  @override
  Widget build(BuildContext context) {
    final text = Theme.of(context).textTheme;
    return ExcludeFocus(
      child: Container(
        key: const ValueKey('quick-replies'),
        decoration: const BoxDecoration(
          color: RoostColors.waitingSurface,
          border: Border(top: BorderSide(color: RoostColors.waitingBorder)),
        ),
        padding: const EdgeInsets.fromLTRB(12, 6, 6, 6),
        child: LayoutBuilder(
          builder: (context, constraints) => Row(
            children: [
              // A phone has room for the keys or the words, and the keys are
              // what the bar is for: there, a dot says why it is showing.
              if (constraints.maxWidth >= compactQuickRepliesWidth)
                Expanded(
                  child: Text(
                    statusText ?? 'Waiting on you',
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: text.labelMedium?.copyWith(
                      color: RoostColors.waiting,
                    ),
                  ),
                )
              else
                const Padding(
                  padding: EdgeInsets.only(right: 8),
                  child: StatusDot(
                    key: ValueKey('quick-replies-dot'),
                    color: RoostColors.waiting,
                  ),
                ),
              Flexible(
                flex: 3,
                child: SingleChildScrollView(
                  scrollDirection: Axis.horizontal,
                  child: Row(
                    children: [
                      for (final r in quickReplies)
                        Padding(
                          padding: const EdgeInsets.symmetric(horizontal: 2),
                          child: OutlinedButton(
                            key: ValueKey('quick-reply-${r.id}'),
                            onPressed: () => onReply(r.bytes),
                            style: OutlinedButton.styleFrom(
                              minimumSize: const Size(44, 40),
                              padding: const EdgeInsets.symmetric(
                                horizontal: 10,
                              ),
                              foregroundColor: RoostColors.waitingText,
                              side: const BorderSide(
                                color: RoostColors.waitingBorder,
                              ),
                              tapTargetSize: MaterialTapTargetSize.shrinkWrap,
                            ),
                            child: Text(r.label),
                          ),
                        ),
                    ],
                  ),
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}
