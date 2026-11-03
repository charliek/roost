import 'package:flutter/material.dart';

/// A native multi-line field for writing a whole prompt with autocorrect and
/// voice, sent as one bracketed paste.
class ComposeBox extends StatefulWidget {
  const ComposeBox({
    super.key,
    required this.onSend,
    required this.onClose,
    this.title,
  });

  /// Sends the text; [enter] adds a Return after it. False keeps the text
  /// (the send failed).
  final Future<bool> Function(String text, {required bool enter}) onSend;
  final VoidCallback onClose;
  final String? title;

  @override
  State<ComposeBox> createState() => _ComposeBoxState();
}

class _ComposeBoxState extends State<ComposeBox> {
  final _controller = TextEditingController();
  bool _enterAfter = true;
  bool _sending = false;

  @override
  void dispose() {
    _controller.dispose();
    super.dispose();
  }

  Future<void> _send() async {
    final text = _controller.text;
    if (text.trim().isEmpty || _sending) return;
    setState(() => _sending = true);
    try {
      if (await widget.onSend(text, enter: _enterAfter)) _controller.clear();
    } finally {
      if (mounted) setState(() => _sending = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Container(
      key: const ValueKey('compose-box'),
      decoration: BoxDecoration(
        color: scheme.surfaceContainerHigh,
        border: Border(top: BorderSide(color: scheme.outlineVariant)),
      ),
      padding: const EdgeInsets.fromLTRB(12, 8, 12, 8),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(
            children: [
              Expanded(
                child: Text(
                  widget.title ?? 'Compose',
                  style: Theme.of(context).textTheme.titleSmall,
                  overflow: TextOverflow.ellipsis,
                ),
              ),
              TextButton(onPressed: widget.onClose, child: const Text('Close')),
            ],
          ),
          TextField(
            key: const ValueKey('compose-field'),
            controller: _controller,
            autofocus: true,
            minLines: 2,
            maxLines: 6,
            keyboardType: TextInputType.multiline,
            textInputAction: TextInputAction.newline,
            textCapitalization: TextCapitalization.sentences,
            decoration: const InputDecoration(
              hintText: 'Write a prompt…',
              border: OutlineInputBorder(),
              isDense: true,
            ),
          ),
          const SizedBox(height: 4),
          Row(
            children: [
              Checkbox(
                key: const ValueKey('compose-enter'),
                value: _enterAfter,
                onChanged: (v) => setState(() => _enterAfter = v ?? true),
              ),
              const Expanded(child: Text('Press Enter after sending')),
              FilledButton.icon(
                key: const ValueKey('compose-send'),
                onPressed: _sending ? null : _send,
                icon: const Icon(Icons.send, size: 18),
                label: const Text('Send'),
              ),
            ],
          ),
        ],
      ),
    );
  }
}
