import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/scheduler.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../app/providers.dart';
import '../app/theme.dart';
import '../app/workspace_actions.dart';
import '../core/roost_core.dart';
import '../hosts/host_widgets.dart';
import '../input/compose_box.dart';
import '../input/extra_keys.dart';
import '../input/quick_replies.dart';
import '../input/soft_keyboard.dart';
import 'attached_tab.dart';
import 'terminal_view.dart';

/// A hardware key event within this long hides the extra-keys bar.
const hardwareKeyboardWindow = Duration(minutes: 2);

/// One tab on screen: attaches it (taking its size) once the view knows its
/// grid, routes every kind of input to it, and detaches when it goes away.
class TerminalPane extends ConsumerStatefulWidget {
  const TerminalPane({
    super.key,
    required this.hostId,
    required this.tabId,
    this.leading,
    this.subtitle,
    this.onSwipeTab,
  });

  final String hostId;
  final int tabId;
  final Widget? leading;
  final String? subtitle;

  /// A horizontal fling over the terminal: -1 for the previous tab, 1 for the
  /// next.
  final ValueChanged<int>? onSwipeTab;

  @override
  ConsumerState<TerminalPane> createState() => _TerminalPaneState();
}

class _TerminalPaneState extends ConsumerState<TerminalPane> {
  final _focus = FocusNode(debugLabel: 'terminal');
  final _input = GlobalKey<TerminalInputState>();
  AttachedTab? _tab;
  Geometry? _geometry;
  int _attachSeq = 0;
  bool _attaching = false;
  String? _attachError;
  bool _composeOpen = false;
  bool _ctrl = false;
  bool _alt = false;
  DateTime? _hardwareKeyAt;

  RoostCore get _core => ref.read(coreProvider);

  @override
  void initState() {
    super.initState();
    _focus.addListener(_onFocusChange);
  }

  @override
  void dispose() {
    _attachSeq++;
    _detach();
    _focus
      ..removeListener(_onFocusChange)
      ..dispose();
    super.dispose();
  }

  void _onFocusChange() {
    if (mounted) setState(() {});
  }

  void _onTabChanged() {
    if (mounted) setState(() {});
  }

  void _detach() {
    final tab = _tab;
    _tab = null;
    if (tab == null) return;
    tab.removeListener(_onTabChanged);
    unawaited(tab.close());
    // The view still holds it until the next frame rebuilds without it.
    SchedulerBinding.instance.addPostFrameCallback((_) => tab.dispose());
  }

  void _onGeometry(Geometry geometry) {
    _geometry = geometry;
    final tab = _tab;
    if (tab != null) {
      _fire(tab.session.resize(geometry, focused: _focus.hasFocus));
    } else if (!_attaching && _attachError == null) {
      unawaited(_attach());
    }
  }

  Future<void> _attach() async {
    final geometry = _geometry;
    if (geometry == null) return;
    final seq = ++_attachSeq;
    _detach();
    setState(() {
      _attaching = true;
      _attachError = null;
    });
    try {
      final session = await _core.attach(
        widget.hostId,
        widget.tabId,
        geometry,
        take: true,
      );
      if (!mounted || seq != _attachSeq) {
        unawaited(session.close());
        return;
      }
      final tab = AttachedTab(session)..addListener(_onTabChanged);
      setState(() {
        _tab = tab;
        _attaching = false;
      });
      final latest = _geometry;
      if (latest != null && latest != geometry) {
        _fire(session.resize(latest, focused: _focus.hasFocus));
      }
      _clearNotification();
    } catch (e) {
      if (!mounted || seq != _attachSeq) return;
      setState(() {
        _attaching = false;
        _attachError = '$e';
      });
    }
  }

  void _clearNotification() {
    final view = ref
        .read(workspaceProvider(widget.hostId))
        .value
        ?.tab(widget.tabId);
    if (view != null && view.hasNotification) {
      _fire(_core.clearNotification(widget.hostId, widget.tabId));
    }
  }

  void _fire(Future<void> call) {
    unawaited(
      call.catchError((Object e) {
        debugPrint('roost: terminal call failed: $e');
      }),
    );
  }

  void _disarm() {
    if (_ctrl || _alt) {
      setState(() {
        _ctrl = false;
        _alt = false;
      });
    }
  }

  void _key(KeyInput key) {
    final k = _ctrl || _alt ? key.withModifiers(ctrl: _ctrl, alt: _alt) : key;
    _disarm();
    final tab = _tab;
    if (tab != null) _fire(tab.session.key(k));
  }

  void _text(String text) {
    final sticky = stickyKeyFor(text, ctrl: _ctrl, alt: _alt);
    _disarm();
    final tab = _tab;
    if (tab == null) return;
    _fire(sticky != null ? tab.session.key(sticky) : tab.session.text(text));
  }

  Future<void> _pasteClipboard() async {
    final data = await Clipboard.getData(Clipboard.kTextPlain);
    final text = data?.text;
    final tab = _tab;
    if (text == null || text.isEmpty || tab == null) return;
    _fire(tab.session.paste(text));
  }

  void _scroll(int rows, int col, int row) {
    final tab = _tab;
    if (tab != null) _fire(tab.session.scroll(rows, col: col, row: row));
  }

  void _onHardwareKeyboard() {
    final hidden = _hardwareRecently;
    _hardwareKeyAt = DateTime.now();
    if (!hidden) setState(() {});
  }

  void _onSoftKeyboard() {
    if (_hardwareKeyAt == null) return;
    setState(() => _hardwareKeyAt = null);
  }

  bool get _hardwareRecently {
    final at = _hardwareKeyAt;
    return at != null && DateTime.now().difference(at) < hardwareKeyboardWindow;
  }

  Future<void> _quickReply(String bytes) async {
    await runCoreAction(
      context,
      () => _core.sendText(widget.hostId, widget.tabId, bytes, paste: false),
    );
  }

  Future<bool> _composeSend(String text, {required bool enter}) async {
    final sent = await runCoreAction(context, () async {
      await _core.sendText(widget.hostId, widget.tabId, text, paste: true);
      if (enter) {
        await _core.sendText(widget.hostId, widget.tabId, '\r', paste: false);
      }
      return true;
    });
    return sent ?? false;
  }

  @override
  Widget build(BuildContext context) {
    ref.listen<int>(resumeEpochProvider, (_, _) => unawaited(_attach()));
    final view = ref
        .watch(workspaceProvider(widget.hostId))
        .value
        ?.tab(widget.tabId);
    final fontSize = ref.watch(terminalFontSizeProvider);
    final tab = _tab;
    final focused = _focus.hasFocus;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _Toolbar(
          leading: widget.leading,
          view: view,
          tabId: widget.tabId,
          subtitle: widget.subtitle,
          composeOpen: _composeOpen,
          onKeyboard: () => _input.currentState?.showKeyboard(),
          onCompose: () => setState(() => _composeOpen = !_composeOpen),
          onRename: view == null
              ? null
              : () => renameTabFlow(context, ref, widget.hostId, view),
          onClose: view == null
              ? null
              : () => closeTabFlow(context, ref, widget.hostId, view),
          onFontSize: (delta) => ref
              .read(terminalFontSizeProvider.notifier)
              .set(
                (fontSize + delta).clamp(
                  minTerminalFontSize,
                  maxTerminalFontSize,
                ),
              ),
        ),
        if (tab != null && tab.sizedElsewhere)
          _SizeOwnerBanner(
            owner: tab.owner!,
            onTake: () => _fire(tab.session.takeSize()),
          ),
        Expanded(
          child: GestureDetector(
            onHorizontalDragEnd: widget.onSwipeTab == null
                ? null
                : (d) {
                    final v = d.primaryVelocity ?? 0;
                    if (v.abs() > 400) widget.onSwipeTab!(v > 0 ? -1 : 1);
                  },
            child: Stack(
              fit: StackFit.expand,
              children: [
                TerminalInput(
                  key: _input,
                  focusNode: _focus,
                  onKey: _key,
                  onText: _text,
                  onHardwareKeyboard: _onHardwareKeyboard,
                  onSoftKeyboard: _onSoftKeyboard,
                  onPasteShortcut: () => unawaited(_pasteClipboard()),
                  child: TerminalView(
                    tab: tab,
                    fontSize: fontSize,
                    focused: focused,
                    onGeometry: _onGeometry,
                    onFontSize: (s) =>
                        ref.read(terminalFontSizeProvider.notifier).set(s),
                    onScroll: _scroll,
                    onTap: () => _input.currentState?.showKeyboard(),
                  ),
                ),
                _StatusOverlay(
                  attaching: _attaching,
                  error: _attachError,
                  tab: tab,
                  onRetry: () => unawaited(_attach()),
                ),
              ],
            ),
          ),
        ),
        if (view != null && view.needsInput)
          QuickRepliesBar(statusText: view.statusText, onReply: _quickReply),
        if (_composeOpen)
          ComposeBox(
            title: view == null ? null : 'Send to ${view.title}',
            onSend: _composeSend,
            onClose: () => setState(() => _composeOpen = false),
          )
        else if (focused && !_hardwareRecently)
          ExtraKeysBar(
            ctrlArmed: _ctrl,
            altArmed: _alt,
            onToggleCtrl: () => setState(() => _ctrl = !_ctrl),
            onToggleAlt: () => setState(() => _alt = !_alt),
            onKey: _key,
          ),
      ],
    );
  }
}

class _Toolbar extends StatelessWidget {
  const _Toolbar({
    required this.leading,
    required this.view,
    required this.tabId,
    required this.subtitle,
    required this.composeOpen,
    required this.onKeyboard,
    required this.onCompose,
    required this.onRename,
    required this.onClose,
    required this.onFontSize,
  });

  final Widget? leading;
  final TabView? view;
  final int tabId;
  final String? subtitle;
  final bool composeOpen;
  final VoidCallback onKeyboard;
  final VoidCallback onCompose;
  final VoidCallback? onRename;
  final VoidCallback? onClose;
  final ValueChanged<double> onFontSize;

  @override
  Widget build(BuildContext context) {
    final text = Theme.of(context).textTheme;
    final sub = subtitle ?? view?.statusText;
    return Container(
      key: const ValueKey('terminal-toolbar'),
      height: 48,
      decoration: const BoxDecoration(
        color: RoostColors.surfaceLow,
        border: Border(bottom: BorderSide(color: RoostColors.divider)),
      ),
      child: Row(
        children: [
          ?leading,
          const SizedBox(width: 12),
          if (view != null) ...[
            AgentDot(badge: view!.agent),
            const SizedBox(width: 8),
          ],
          Expanded(
            child: Column(
              mainAxisAlignment: MainAxisAlignment.center,
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  view?.title ?? 'Tab $tabId',
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: text.titleSmall,
                ),
                if (sub != null)
                  Text(
                    sub,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: text.bodySmall?.copyWith(color: RoostColors.muted),
                  ),
              ],
            ),
          ),
          IconButton(
            key: const ValueKey('keyboard-toggle'),
            tooltip: 'Keyboard',
            icon: const Icon(Icons.keyboard_outlined),
            onPressed: onKeyboard,
          ),
          IconButton(
            key: const ValueKey('compose-toggle'),
            tooltip: 'Compose',
            isSelected: composeOpen,
            icon: const Icon(Icons.edit_note),
            onPressed: onCompose,
          ),
          PopupMenuButton<String>(
            tooltip: 'Tab actions',
            onSelected: (action) {
              switch (action) {
                case 'rename':
                  onRename?.call();
                case 'close':
                  onClose?.call();
                case 'bigger':
                  onFontSize(1);
                case 'smaller':
                  onFontSize(-1);
              }
            },
            itemBuilder: (context) => const [
              PopupMenuItem(value: 'rename', child: Text('Rename tab')),
              PopupMenuItem(value: 'close', child: Text('Close tab')),
              PopupMenuItem(value: 'bigger', child: Text('Larger text')),
              PopupMenuItem(value: 'smaller', child: Text('Smaller text')),
            ],
          ),
        ],
      ),
    );
  }
}

class _SizeOwnerBanner extends StatelessWidget {
  const _SizeOwnerBanner({required this.owner, required this.onTake});

  final OwnerChanged owner;
  final VoidCallback onTake;

  @override
  Widget build(BuildContext context) {
    return Container(
      key: const ValueKey('size-owner-banner'),
      color: RoostColors.surfaceHighest,
      padding: const EdgeInsets.only(left: 12),
      child: Row(
        children: [
          const Icon(Icons.aspect_ratio, size: 16, color: RoostColors.muted),
          const SizedBox(width: 8),
          Expanded(
            child: Text(
              'sized by ${owner.label ?? 'another client'} · '
              '${owner.cols}×${owner.rows}',
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: Theme.of(context).textTheme.bodySmall,
            ),
          ),
          TextButton(
            key: const ValueKey('take-size'),
            onPressed: onTake,
            child: const Text('Take size'),
          ),
        ],
      ),
    );
  }
}

class _StatusOverlay extends StatelessWidget {
  const _StatusOverlay({
    required this.attaching,
    required this.error,
    required this.tab,
    required this.onRetry,
  });

  final bool attaching;
  final String? error;
  final AttachedTab? tab;
  final VoidCallback onRetry;

  @override
  Widget build(BuildContext context) {
    final tab = this.tab;
    final String? message;
    var retry = false;
    if (error != null) {
      message = error;
      retry = true;
    } else if (attaching || tab?.state == AttachState.attaching) {
      message = 'Attaching…';
    } else if (tab?.state == AttachState.reconnecting) {
      message = 'Reconnecting…';
    } else if (tab?.state == AttachState.ended) {
      final code = tab!.exitCode;
      message = code != null
          ? 'Exited with code $code'
          : 'Detached${tab.message == null ? '' : ': ${tab.message}'}';
      retry = code == null;
    } else {
      message = null;
    }
    if (message == null) return const SizedBox.shrink();
    return Align(
      alignment: Alignment.topCenter,
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Material(
          key: const ValueKey('terminal-status'),
          color: RoostColors.surfaceHigh,
          shape: RoundedRectangleBorder(
            side: const BorderSide(color: RoostColors.border),
            borderRadius: BorderRadius.circular(12),
          ),
          child: Padding(
            padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
            child: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                Flexible(
                  child: Text(
                    message,
                    style: Theme.of(context).textTheme.bodySmall,
                  ),
                ),
                if (retry)
                  TextButton(onPressed: onRetry, child: const Text('Reattach')),
              ],
            ),
          ),
        ),
      ),
    );
  }
}
