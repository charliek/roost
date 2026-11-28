import 'dart:async';

import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter/scheduler.dart';

import '../app/width_class.dart';
import '../core/models.dart';
import 'attached_tab.dart';
import 'cell_metrics.dart';
import 'contrast.dart';
import 'terminal_model.dart';
import 'terminal_painter.dart';

const minTerminalFontSize = 7.0;
const terminalInset = 4.0;
const maxTerminalFontSize = 28.0;

/// Grid changes settle this long before they reach the session, the same
/// 150 ms the desktop's background resize wave waits.
const regridDebounce = Duration(milliseconds: 150);

/// The area a terminal's grid fits into on a canvas of [canvas]: inside the
/// inset on every side.
Size terminalGridArea(Size canvas) =>
    Size(canvas.width - 2 * terminalInset, canvas.height - 2 * terminalInset);

CellMetrics? _lastMeasured;

/// The grid [TerminalView] offers for [fontSize] in [gridArea]. Remembers the
/// last measurement: the Display sheet and the pinch readout ask on every
/// rebuild, and a paragraph layout is not free.
Geometry terminalGeometry(
  double fontSize,
  double devicePixelRatio,
  Size gridArea,
) {
  var metrics = _lastMeasured;
  if (metrics == null ||
      metrics.fontSize != fontSize ||
      metrics.devicePixelRatio != devicePixelRatio) {
    metrics = _lastMeasured = CellMetrics.measure(fontSize, devicePixelRatio);
  }
  return metrics.geometryFor(gridArea);
}

/// Draws an attached tab and turns the space it is given into a grid.
///
/// The first grid is reported as soon as it is known (the attach needs it);
/// later ones are debounced by [regridDebounce]. Frames are pulled by a
/// ticker, at most one per vsync, and only after the tab said one is ready.
class TerminalView extends StatefulWidget {
  const TerminalView({
    super.key,
    required this.tab,
    required this.fontSize,
    this.fontSizeSetting,
    this.fontEdits = 0,
    this.editedFontSize,
    this.focused = false,
    this.readability = const TextReadability(),
    this.onGeometry,
    this.onGridArea,
    this.onFontSize,
    this.onPinchEnd,
    this.onScroll,
    this.onTap,
    this.onLongPress,
  });

  final AttachedTab? tab;
  final double fontSize;

  /// Which setting [fontSize] comes from (the layout's width class). Only the
  /// user changes a setting, so a new size from the same one is deliberate; a
  /// new size because another setting now applies (a rotation into another
  /// width class) is not.
  final WidthClass? fontSizeSetting;

  /// Counts the user's edits to the text size. A new [fontSize] is
  /// deliberate only when this moved too: a size put back after a failed
  /// save is not the user's change.
  final int fontEdits;

  /// The size the last of those edits set for this width class: a new
  /// [fontSize] other than it (put back after a failed save in the same
  /// frame as an edit) is not the user's.
  final double? editedFontSize;
  final bool focused;
  final TextReadability readability;

  /// The grid that fits. [deliberate] when the user changed it (the font
  /// size, see [fontSizeSetting]), not when the space around it moved (a
  /// banner, a bar, the keyboard, a width class): only a deliberate regrid
  /// may take the tab's size.
  final void Function(Geometry geometry, {required bool deliberate})?
  onGeometry;

  /// The [terminalGridArea] laid out, after the frame, each time it changes.
  final ValueChanged<Size>? onGridArea;

  /// A pinch changed the text size.
  final ValueChanged<double>? onFontSize;

  /// A pinch ended: the size it left is final.
  final VoidCallback? onPinchEnd;

  /// Rows to scroll, positive up into history, and the cell (col, row)
  /// under the pointer, for a program that reads the wheel.
  final void Function(int rows, int col, int row)? onScroll;
  final VoidCallback? onTap;
  final VoidCallback? onLongPress;

  @override
  State<TerminalView> createState() => _TerminalViewState();
}

class _TerminalViewState extends State<TerminalView>
    with SingleTickerProviderStateMixin {
  late final Ticker _ticker = createTicker(_onTick);
  final RowTextCache _textCache = RowTextCache();
  final TerminalModel _blank = TerminalModel();
  CellMetrics? _metrics;
  Geometry? _reported;
  Geometry? _pending;
  bool _pendingDeliberate = false;

  /// The font size, width class and edit count of the last offer, reported
  /// or not.
  double? _seenFontSize;
  WidthClass? _seenSetting;
  int _seenFontEdits = 0;
  Timer? _debounce;
  bool _pulling = false;
  double _pinchStartFont = 0;

  /// The size this pinch last asked for, which a rebuild may not show yet.
  double _pinchFont = 0;

  /// A trackpad's pan/zoom counts as two pointers, but its pan is a
  /// two-finger scroll, not a pinch.
  bool _trackpad = false;

  /// Where the pointer sat when the trackpad gesture began: the focal point
  /// moves with the pan, the pointer does not.
  Offset? _panZoomAt;
  double _scrollCarry = 0;
  Size _size = Size.zero;
  Size? _gridArea;

  @override
  void initState() {
    super.initState();
    _listen(widget.tab);
  }

  @override
  void didUpdateWidget(TerminalView oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.tab != widget.tab) {
      oldWidget.tab?.frameSignal.removeListener(_onFrameReady);
      _textCache.clear();
      _listen(widget.tab);
    }
  }

  @override
  void dispose() {
    widget.tab?.frameSignal.removeListener(_onFrameReady);
    _debounce?.cancel();
    _ticker.dispose();
    _textCache.clear();
    _blank.dispose();
    super.dispose();
  }

  void _listen(AttachedTab? tab) {
    tab?.frameSignal.addListener(_onFrameReady);
    if (tab?.framePending ?? false) _onFrameReady();
  }

  void _onFrameReady() {
    if (!_ticker.isActive) unawaited(_ticker.start());
  }

  void _onTick(Duration _) {
    final tab = widget.tab;
    if (_pulling) return;
    if (tab == null || !tab.framePending) {
      _ticker.stop();
      return;
    }
    unawaited(_pull(tab));
  }

  Future<void> _pull(AttachedTab tab) async {
    _pulling = true;
    try {
      await tab.pullFrame();
    } finally {
      _pulling = false;
    }
  }

  CellMetrics _metricsFor(double fontSize, double dpr) {
    final m = _metrics;
    if (m != null && m.fontSize == fontSize && m.devicePixelRatio == dpr) {
      return m;
    }
    return _metrics = CellMetrics.measure(fontSize, dpr);
  }

  void _offer(
    Geometry local,
    double fontSize,
    WidthClass? setting,
    int fontEdits,
  ) {
    final first = _reported == null;
    // Deliberate: a font edit changed this width class's size since the last
    // offer. A later offer in the same class at that size (the keyboard, a
    // rotation) still carries the pending edit; a class switch or a size put
    // back without an edit does not.
    final userChanged =
        !first &&
        ((fontEdits != _seenFontEdits &&
                setting == _seenSetting &&
                fontSize != _seenFontSize &&
                fontSize == widget.editedFontSize) ||
            (_pending != null &&
                _pendingDeliberate &&
                setting == _seenSetting &&
                fontSize == _seenFontSize));
    final unchanged =
        fontSize == _seenFontSize &&
        setting == _seenSetting &&
        fontEdits == _seenFontEdits;
    _seenFontSize = fontSize;
    _seenSetting = setting;
    _seenFontEdits = fontEdits;
    if (first) {
      _reported = local;
      SchedulerBinding.instance.addPostFrameCallback((_) {
        if (mounted) widget.onGeometry?.call(local, deliberate: false);
      });
      return;
    }
    // A deliberate change is offered even when its grid is the one already
    // reported (two sizes can round to the same cells): it is how the user
    // takes the tab's size back.
    if (local == _reported && !userChanged) {
      _debounce?.cancel();
      _pending = null;
      return;
    }
    if (local == _pending && unchanged) return;
    _pending = local;
    _pendingDeliberate = userChanged;
    _debounce?.cancel();
    _debounce = Timer(regridDebounce, () {
      _pending = null;
      if (!mounted) return;
      _reported = local;
      widget.onGeometry?.call(local, deliberate: userChanged);
    });
  }

  void _reportGridArea(Size area) {
    if (area == _gridArea) return;
    _gridArea = area;
    SchedulerBinding.instance.addPostFrameCallback((_) {
      if (mounted && area == _gridArea) widget.onGridArea?.call(area);
    });
  }

  void _onScaleStart(ScaleStartDetails d) {
    _pinchStartFont = widget.fontSize;
    _pinchFont = widget.fontSize;
    _scrollCarry = 0;
    _trackpad = d.kind == PointerDeviceKind.trackpad;
  }

  void _onScaleUpdate(ScaleUpdateDetails d) {
    if (d.pointerCount >= 2) {
      final next = (_pinchStartFont * d.scale).clamp(
        minTerminalFontSize,
        maxTerminalFontSize,
      );
      final rounded = (next * 2).roundToDouble() / 2;
      if (rounded != _pinchFont) {
        _pinchFont = rounded;
        widget.onFontSize?.call(rounded);
      }
      if (!_trackpad) return;
      _scrollBy(d.focalPointDelta.dy, _panZoomAt ?? d.localFocalPoint);
      return;
    }
    _scrollBy(d.focalPointDelta.dy, d.localFocalPoint);
  }

  /// Dragging down pulls older lines into view: a positive scroll.
  void _scrollBy(double dy, Offset at) {
    final m = _metrics;
    if (m == null || m.cellHeight <= 0) return;
    _scrollCarry += dy;
    final rows = (_scrollCarry / m.cellHeight).truncate();
    if (rows == 0) return;
    _scrollCarry -= rows * m.cellHeight;
    final (col, row) = _cellAt(m, at);
    widget.onScroll?.call(rows, col, row);
  }

  /// The cell of the drawn grid under [at], through the inset and any
  /// shrink to fit.
  (int, int) _cellAt(CellMetrics m, Offset at) {
    final model = widget.tab?.model;
    if (model == null || model.cols == 0 || model.rows == 0) return (0, 0);
    final scale = m.scaleToFit(model.cols, model.rows, terminalGridArea(_size));
    final col = ((at.dx - terminalInset) / (m.cellWidth * scale)).floor();
    final row = ((at.dy - terminalInset) / (m.cellHeight * scale)).floor();
    return (col.clamp(0, model.cols - 1), row.clamp(0, model.rows - 1));
  }

  void _onPointerSignal(PointerSignalEvent event) {
    if (event is PointerScrollEvent) {
      _scrollBy(-event.scrollDelta.dy, event.localPosition);
    }
  }

  @override
  Widget build(BuildContext context) {
    final dpr = MediaQuery.devicePixelRatioOf(context);
    final metrics = _metricsFor(widget.fontSize, dpr);
    return LayoutBuilder(
      builder: (context, constraints) {
        final size = constraints.biggest;
        _size = size;
        final area = terminalGridArea(size);
        final local = metrics.geometryFor(area);
        _offer(
          local,
          metrics.fontSize,
          widget.fontSizeSetting,
          widget.fontEdits,
        );
        _reportGridArea(area);
        return Listener(
          onPointerSignal: _onPointerSignal,
          onPointerPanZoomStart: (e) => _panZoomAt = e.localPosition,
          child: GestureDetector(
            behavior: HitTestBehavior.opaque,
            onTap: widget.onTap,
            onLongPress: widget.onLongPress,
            onScaleStart: _onScaleStart,
            onScaleUpdate: _onScaleUpdate,
            onScaleEnd: (_) => widget.onPinchEnd?.call(),
            child: CustomPaint(
              key: const ValueKey('terminal-canvas'),
              size: size,
              isComplex: true,
              willChange: true,
              painter: TerminalPainter(
                model: widget.tab?.model ?? _blank,
                metrics: metrics,
                textCache: _textCache,
                localCols: local.cols,
                localRows: local.rows,
                focused: widget.focused,
                inset: terminalInset,
                readability: widget.readability,
              ),
            ),
          ),
        );
      },
    );
  }
}
