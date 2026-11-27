import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/scheduler.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../app/display_sheet.dart';
import '../app/providers.dart';
import '../app/theme.dart';
import '../app/width_class.dart';
import '../app/workspace_actions.dart';
import '../core/roost_core.dart';
import '../hosts/host_widgets.dart';
import '../input/compose_box.dart';
import '../input/extra_keys.dart';
import '../input/quick_replies.dart';
import '../input/soft_keyboard.dart';
import 'attached_tab.dart';
import 'copy_sheet.dart';
import 'terminal_view.dart';

/// A hardware key event within this long hides the extra-keys bar.
const hardwareKeyboardWindow = Duration(minutes: 2);

/// A local scroll has settled this long after its last step.
const scrollSettle = Duration(milliseconds: 500);

/// How long a "copied" notice stays after the last copy.
const noticeDuration = Duration(milliseconds: 1500);

/// The tab toolbar's height at the default text scale.
const terminalToolbarHeight = 48.0;

/// How long the pinch's size readout stays after the pinch's last change.
const fontSizeHudDuration = Duration(milliseconds: 900);

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

  /// The user changed the grid while no tab was attached: the resize that
  /// follows the attach carries it, or it would not take the size.
  bool _deliberatePending = false;
  int _attachSeq = 0;
  bool _attaching = false;
  String? _attachError;
  bool _composeOpen = false;
  bool _ctrl = false;
  bool _alt = false;
  DateTime? _hardwareKeyAt;
  Timer? _scrollSettle;
  String? _notice;
  Timer? _noticeTimer;
  bool _hud = false;
  Timer? _hudTimer;
  Size? _gridArea;
  late final TerminalPaneSizeNotifier _paneSize;
  late final int _paneToken;

  RoostCore get _core => ref.read(coreProvider);

  @override
  void initState() {
    super.initState();
    _paneSize = ref.read(terminalPaneSizeProvider.notifier);
    _paneToken = _paneSize.claim();
    _focus.addListener(_onFocusChange);
  }

  @override
  void dispose() {
    _attachSeq++;
    _scrollSettle?.cancel();
    _noticeTimer?.cancel();
    _hudTimer?.cancel();
    // Not now: providers may not change while the tree is torn down.
    final paneSize = _paneSize;
    final token = _paneToken;
    SchedulerBinding.instance.addPostFrameCallback(
      (_) => paneSize.release(token),
    );
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
    if (!mounted) return;
    setState(() {});
    _publishPaneSize();
  }

  void _onGridArea(Size area) {
    _gridArea = area;
    _publishPaneSize();
  }

  void _publishPaneSize() {
    final area = _gridArea;
    if (area == null) return;
    final tab = _tab;
    final sizedBy = tab != null && tab.sizedElsewhere
        ? tab.owner!.label ?? 'another client'
        : null;
    _paneSize.report(_paneToken, (gridArea: area, sizedBy: sizedBy));
  }

  void _onPinchFontSize(double size, WidthClass widthClass) {
    ref
        .read(settingsProvider.notifier)
        .edit(
          (s) => s.withTerminalFontSize(widthClass, size),
          continuous: true,
        );
    _hudTimer?.cancel();
    if (!_hud) setState(() => _hud = true);
    _hudTimer = Timer(fontSizeHudDuration, () {
      if (mounted) setState(() => _hud = false);
    });
  }

  void _detach() {
    _scrollSettle?.cancel();
    final tab = _tab;
    _tab = null;
    if (tab == null) return;
    tab.removeListener(_onTabChanged);
    unawaited(tab.close());
    // The view still holds it until the next frame rebuilds without it.
    SchedulerBinding.instance.addPostFrameCallback((_) => tab.dispose());
  }

  void _onGeometry(Geometry geometry, {required bool deliberate}) {
    _geometry = geometry;
    final tab = _tab;
    if (tab != null) {
      _fire(tab.session.resize(geometry, focused: deliberate));
      return;
    }
    _deliberatePending |= deliberate;
    if (!_attaching && _attachError == null) unawaited(_attach());
  }

  Future<void> _attach() async {
    final geometry = _geometry;
    if (geometry == null) return;
    final seq = ++_attachSeq;
    // The attach takes the size of the grid it starts with.
    _deliberatePending = false;
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
      final tab = AttachedTab(session)
        ..addListener(_onTabChanged)
        ..onClipboardWrite = _onClipboardWrite;
      setState(() {
        _tab = tab;
        _attaching = false;
      });
      _publishPaneSize();
      final latest = _geometry;
      final deliberate = _deliberatePending;
      _deliberatePending = false;
      if (latest != null && latest != geometry) {
        _fire(session.resize(latest, focused: deliberate || _focus.hasFocus));
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

  /// A notice over the terminal rather than a snackbar: a snackbar would sit
  /// on the extra-keys or quick-reply bar and take its taps, and a program
  /// that copies on every edit would queue one per copy. A new notice
  /// replaces the one showing.
  void _showNotice(String text) {
    _noticeTimer?.cancel();
    setState(() => _notice = text);
    _noticeTimer = Timer(noticeDuration, () {
      if (mounted) setState(() => _notice = null);
    });
  }

  Future<void> _onClipboardWrite(String text) async {
    await Clipboard.setData(ClipboardData(text: text));
    if (mounted) _showNotice('Copied from the terminal');
  }

  void _openCopy() {
    final tab = _tab;
    if (tab == null || !tab.model.hasFrame) return;
    unawaited(
      showCopySheet(
        context,
        tab.model,
        onCopied: (what) {
          if (mounted) _showNotice('$what copied');
        },
      ),
    );
  }

  void _scroll(int rows, int col, int row) {
    final tab = _tab;
    if (tab == null) return;
    _fire(tab.session.scroll(rows, col: col, row: row));
    _scrollSettle?.cancel();
    _scrollSettle = Timer(scrollSettle, _scrollSettled);
  }

  /// Reading this tab's scrollback is using the tab here: once the scroll
  /// settles, take its size back from whoever holds it (ipc.md, size
  /// ownership: "TAKE — once a local scroll has settled").
  void _scrollSettled() {
    final tab = _tab;
    if (tab != null && tab.sizedElsewhere) _fire(tab.session.takeSize());
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
    final widthClass = WidthClassScope.of(context);
    final fontSize = ref.watch(
      settingsProvider.select((s) => s.desired.terminalFontSizeFor(widthClass)),
    );
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
          onCopy: _openCopy,
          onRename: view == null
              ? null
              : () => renameTabFlow(context, ref, widget.hostId, view),
          onClose: view == null
              ? null
              : () => closeTabFlow(context, ref, widget.hostId, view),
          onDisplay: () => unawaited(showDisplaySheet(context)),
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
                    fontSizeSetting: widthClass,
                    focused: focused,
                    onGeometry: _onGeometry,
                    onGridArea: _onGridArea,
                    onFontSize: (size) => _onPinchFontSize(size, widthClass),
                    onPinchEnd: ref.read(settingsProvider.notifier).flush,
                    onScroll: _scroll,
                    onTap: () => _input.currentState?.showKeyboard(),
                    onLongPress: _openCopy,
                  ),
                ),
                if (_notice != null) _Notice(text: _notice!),
                if (_hud && _gridArea != null)
                  _FontSizeHud(
                    fontSize: fontSize,
                    geometry: terminalGeometry(
                      fontSize,
                      MediaQuery.devicePixelRatioOf(context),
                      _gridArea!,
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
    required this.onCopy,
    required this.onRename,
    required this.onClose,
    required this.onDisplay,
  });

  final Widget? leading;
  final TabView? view;
  final int tabId;
  final String? subtitle;
  final bool composeOpen;
  final VoidCallback onKeyboard;
  final VoidCallback onCompose;
  final VoidCallback onCopy;
  final VoidCallback? onRename;
  final VoidCallback? onClose;
  final VoidCallback onDisplay;

  @override
  Widget build(BuildContext context) {
    final text = Theme.of(context).textTheme;
    final sub = subtitle ?? view?.statusText;
    return Container(
      key: const ValueKey('terminal-toolbar'),
      constraints: const BoxConstraints(minHeight: terminalToolbarHeight),
      decoration: BoxDecoration(color: context.palette.surfaceLow),
      foregroundDecoration: BoxDecoration(
        border: Border(bottom: BorderSide(color: context.palette.divider)),
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
                    style: text.bodySmall?.copyWith(
                      color: context.palette.muted,
                    ),
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
                case 'copy':
                  onCopy();
                case 'rename':
                  onRename?.call();
                case 'close':
                  onClose?.call();
                case 'display':
                  onDisplay();
              }
            },
            itemBuilder: (context) => const [
              PopupMenuItem(
                key: ValueKey('menu-copy-text'),
                value: 'copy',
                child: Text('Copy text…'),
              ),
              PopupMenuItem(value: 'rename', child: Text('Rename tab')),
              PopupMenuItem(value: 'close', child: Text('Close tab')),
              PopupMenuItem(
                key: ValueKey('menu-display'),
                value: 'display',
                child: Text('Display…'),
              ),
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
      color: context.palette.surfaceHighest,
      padding: const EdgeInsets.only(left: 12),
      child: Row(
        children: [
          Icon(Icons.aspect_ratio, size: 16, color: context.palette.muted),
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
          color: context.palette.surfaceHigh,
          shape: RoundedRectangleBorder(
            side: BorderSide(color: context.palette.border),
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

/// A card floating over the terminal that never takes a touch.
class _OverlayCard extends StatelessWidget {
  const _OverlayCard({
    required this.cardKey,
    required this.alignment,
    required this.margin,
    required this.padding,
    required this.color,
    required this.borderColor,
    required this.child,
  });

  final Key cardKey;
  final Alignment alignment;
  final EdgeInsets margin;
  final EdgeInsets padding;
  final Color color;
  final Color borderColor;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    return IgnorePointer(
      child: Align(
        alignment: alignment,
        child: Padding(
          padding: margin,
          child: Material(
            key: cardKey,
            color: color,
            shape: RoundedRectangleBorder(
              side: BorderSide(color: borderColor),
              borderRadius: BorderRadius.circular(12),
            ),
            child: Padding(padding: padding, child: child),
          ),
        ),
      ),
    );
  }
}

class _Notice extends StatelessWidget {
  const _Notice({required this.text});

  final String text;

  @override
  Widget build(BuildContext context) {
    return _OverlayCard(
      cardKey: const ValueKey('terminal-notice'),
      alignment: Alignment.bottomCenter,
      margin: const EdgeInsets.all(16),
      padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 8),
      color: context.palette.surfaceHighest,
      borderColor: context.palette.border,
      child: Text(text, style: Theme.of(context).textTheme.bodyMedium),
    );
  }
}

class _FontSizeHud extends StatelessWidget {
  const _FontSizeHud({required this.fontSize, required this.geometry});

  final double fontSize;
  final Geometry geometry;

  @override
  Widget build(BuildContext context) {
    final palette = context.palette;
    final text = Theme.of(context).textTheme;
    return _OverlayCard(
      cardKey: const ValueKey('font-size-hud'),
      alignment: Alignment.center,
      margin: EdgeInsets.zero,
      padding: const EdgeInsets.symmetric(horizontal: 18, vertical: 10),
      color: palette.surfaceHighest,
      borderColor: palette.border,
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          Text('${formatPoints(fontSize)} pt', style: text.titleMedium),
          Text(
            '${geometry.cols} × ${geometry.rows}',
            style: text.bodySmall?.copyWith(color: palette.muted),
          ),
        ],
      ),
    );
  }
}
