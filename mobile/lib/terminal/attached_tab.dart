import 'dart:async';

import 'package:flutter/foundation.dart';

import '../core/roost_core.dart';
import 'frame.dart';
import 'terminal_model.dart';

/// One attached tab as the widgets see it: its session, the screen decoded
/// from its frames, and the status its events report.
class AttachedTab extends ChangeNotifier {
  AttachedTab(this.session) {
    _events = session.events().listen(
      _onEvent,
      onError: (Object e) {
        state = AttachState.ended;
        message = '$e';
        notifyListeners();
      },
    );
  }

  final TerminalSession session;
  final TerminalModel model = TerminalModel();
  late final StreamSubscription<TerminalEvent> _events;

  AttachState state = AttachState.attaching;
  String? message;
  OwnerChanged? owner;
  int? exitCode;
  bool _framePending = false;
  bool _disposed = false;

  /// A program in the tab wrote the clipboard.
  ValueChanged<String>? onClipboardWrite;

  /// Bumped on every FrameReady: the view's ticker listens to it.
  final ValueNotifier<int> frameSignal = ValueNotifier(0);

  bool get framePending => _framePending;

  /// Another client sets this tab's size.
  bool get sizedElsewhere => owner != null && !owner!.isMe;

  void _onEvent(TerminalEvent event) {
    switch (event) {
      case FrameReady():
        _framePending = true;
        frameSignal.value++;
      case StatusChanged(:final state, :final message):
        this.state = state;
        this.message = message;
        notifyListeners();
      case OwnerChanged():
        owner = event;
        notifyListeners();
      case Exited(:final code):
        exitCode = code;
        state = AttachState.ended;
        notifyListeners();
      case ClipboardWrite(:final text):
        onClipboardWrite?.call(text);
    }
  }

  /// Pulls the pending frame into [model]. True when the screen changed.
  Future<bool> pullFrame() async {
    _framePending = false;
    final bytes = await session.takeFrame();
    if (_disposed || bytes.isEmpty) return false;
    try {
      model.apply(decodeFrame(bytes));
      return true;
    } on FrameFormatException catch (e) {
      message = '$e';
      notifyListeners();
      return false;
    }
  }

  /// Detaches. Does not wait for the event listener to wind down first.
  Future<void> close() {
    unawaited(_events.cancel());
    return session.close();
  }

  @override
  void dispose() {
    _disposed = true;
    unawaited(_events.cancel());
    frameSignal.dispose();
    model.dispose();
    super.dispose();
  }
}
