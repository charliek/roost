import 'package:flutter/material.dart';

import 'theme.dart';

final _targetPattern = RegExp(
  r'^(?:[A-Za-z0-9._-]+@)?(?:[A-Za-z0-9.-]+|\[[0-9A-Fa-f:]+\])(?::(\d{1,5}))?$',
);

/// Null when [target] is a usable `[user@]host[:port]` (or a dev
/// `unix:/path`), else what is wrong with it.
String? validateHostTarget(String target) {
  final t = target.trim();
  if (t.isEmpty) return 'Enter user@host';
  if (t.startsWith('unix:')) {
    return t.length > 'unix:/'.length && t.startsWith('unix:/')
        ? null
        : 'unix: needs an absolute socket path';
  }
  final m = _targetPattern.firstMatch(t);
  if (m == null) return 'Use user@host or user@host:port';
  // A phone cannot know the host's login user, so the core requires one.
  if (!t.contains('@')) return 'Include the user: user@host';
  final port = m.group(1);
  if (port != null) {
    final n = int.parse(port);
    if (n < 1 || n > 65535) return 'Port must be 1–65535';
  }
  return null;
}

typedef NewHost = ({String label, String target});

class AddHostDialog extends StatefulWidget {
  const AddHostDialog({super.key});

  @override
  State<AddHostDialog> createState() => _AddHostDialogState();
}

class _AddHostDialogState extends State<AddHostDialog> {
  final _label = TextEditingController();
  final _target = TextEditingController();
  final _form = GlobalKey<FormState>();

  @override
  void dispose() {
    _label.dispose();
    _target.dispose();
    super.dispose();
  }

  void _submit() {
    if (!_form.currentState!.validate()) return;
    final target = _target.text.trim();
    final label = _label.text.trim();
    Navigator.of(context).pop<NewHost>((
      label: label.isEmpty ? _defaultLabel(target) : label,
      target: target,
    ));
  }

  static String _defaultLabel(String target) {
    final host = target.contains('@') ? target.split('@').last : target;
    return host.split(':').first.split('.').first;
  }

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: const Text('Add host'),
      content: Form(
        key: _form,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            TextFormField(
              key: const ValueKey('add-host-label'),
              controller: _label,
              autofocus: true,
              decoration: const InputDecoration(
                labelText: 'Name',
                hintText: 'devbox',
              ),
            ),
            const SizedBox(height: 12),
            TextFormField(
              key: const ValueKey('add-host-target'),
              controller: _target,
              autocorrect: false,
              enableSuggestions: false,
              keyboardType: TextInputType.url,
              style: const TextStyle(fontFamily: monoFamily),
              decoration: const InputDecoration(
                labelText: 'SSH target',
                hintText: 'Tailscale SSH: user@machine-name',
                // The monospace face is wide: at the field's size the hint
                // would be cut off on a phone.
                hintStyle: TextStyle(fontSize: 13),
                helperText: 'user@host or user@host:port',
              ),
              validator: (v) => validateHostTarget(v ?? ''),
              onFieldSubmitted: (_) => _submit(),
            ),
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Cancel'),
        ),
        FilledButton(
          key: const ValueKey('add-host-submit'),
          onPressed: _submit,
          child: const Text('Add and connect'),
        ),
      ],
    );
  }
}

/// A one-field prompt (rename a tab, name a project). Null when cancelled.
Future<String?> promptText(
  BuildContext context, {
  required String title,
  required String label,
  String initial = '',
  String action = 'Save',
}) {
  return showDialog<String>(
    context: context,
    builder: (context) => _TextPromptDialog(
      title: title,
      label: label,
      initial: initial,
      action: action,
    ),
  );
}

class _TextPromptDialog extends StatefulWidget {
  const _TextPromptDialog({
    required this.title,
    required this.label,
    required this.initial,
    required this.action,
  });

  final String title;
  final String label;
  final String initial;
  final String action;

  @override
  State<_TextPromptDialog> createState() => _TextPromptDialogState();
}

class _TextPromptDialogState extends State<_TextPromptDialog> {
  late final _controller = TextEditingController(text: widget.initial)
    ..selection = TextSelection(
      baseOffset: 0,
      extentOffset: widget.initial.length,
    );

  @override
  void dispose() {
    _controller.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    void submit() => Navigator.of(context).pop(_controller.text.trim());
    return AlertDialog(
      title: Text(widget.title),
      content: TextField(
        key: const ValueKey('prompt-field'),
        controller: _controller,
        autofocus: true,
        decoration: InputDecoration(labelText: widget.label),
        onSubmitted: (_) => submit(),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Cancel'),
        ),
        FilledButton(
          key: const ValueKey('prompt-submit'),
          onPressed: submit,
          child: Text(widget.action),
        ),
      ],
    );
  }
}

Future<bool> confirm(
  BuildContext context, {
  required String title,
  required String message,
  required String action,
}) async {
  final ok = await showDialog<bool>(
    context: context,
    builder: (context) => AlertDialog(
      title: Text(title),
      content: Text(message),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(false),
          child: const Text('Cancel'),
        ),
        FilledButton(
          key: const ValueKey('confirm-action'),
          onPressed: () => Navigator.of(context).pop(true),
          child: Text(action),
        ),
      ],
    ),
  );
  return ok ?? false;
}
