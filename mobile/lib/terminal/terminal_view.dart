import 'dart:async';

import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter/scheduler.dart';

import '../core/models.dart';
import 'attached_tab.dart';
import 'cell_metrics.dart';
import 'terminal_model.dart';
import 'terminal_painter.dart';

const minTerminalFontSize = 7.0;
const terminalInset = 4.0;
const maxTerminalFontSize = 28.0;

/// Grid changes settle this long before they reach the session, the same
/// 150 ms the desktop's background resize wave waits.
const regridDebounce = Duration(milliseconds: 150);

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
    this.focused = false,
    this.onGeometry,
    this.onFontSize,
    this.onScroll,
    this.onTap,
  });

  final AttachedTab? tab;
  final double fontSize;
  final bool focused;
  final ValueChanged<Geometry>? onGeometry;
  final ValueChanged<double>? onFontSize;

  /// Rows to scroll, positive up into history, and the cell (col, row)
  /// under the pointer, for a program that reads the wheel.
  final void Function(int rows, int col, int row)? onScroll;
  final VoidCallback? onTap;

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
  Timer? _debounce;
  bool _pulling = false;
  double _pinchStartFont = 0;
  double _scrollCarry = 0;
  Size _size = Size.zero;

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

  void _offer(Geometry local) {
    if (_reported == null) {
      _reported = local;
      SchedulerBinding.instance.addPostFrameCallback((_) {
        if (mounted) widget.onGeometry?.call(local);
      });
      return;
    }
    if (local == _reported) {
      _debounce?.cancel();
      _pending = null;
      return;
    }
    if (local == _pending) return;
    _pending = local;
    _debounce?.cancel();
    _debounce = Timer(regridDebounce, () {
      _pending = null;
      if (!mounted || local == _reported) return;
      _reported = local;
      widget.onGeometry?.call(local);
    });
  }

  void _onScaleStart(ScaleStartDetails d) {
    _pinchStartFont = widget.fontSize;
    _scrollCarry = 0;
  }

  void _onScaleUpdate(ScaleUpdateDetails d) {
    if (d.pointerCount >= 2) {
      final next = (_pinchStartFont * d.scale).clamp(
        minTerminalFontSize,
        maxTerminalFontSize,
      );
      final rounded = (next * 2).roundToDouble() / 2;
      if (rounded != widget.fontSize) widget.onFontSize?.call(rounded);
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
    final inner = Size(
      _size.width - 2 * terminalInset,
      _size.height - 2 * terminalInset,
    );
    final scale = m.scaleToFit(model.cols, model.rows, inner);
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
        final local = metrics.geometryFor(
          Size(size.width - 2 * terminalInset, size.height - 2 * terminalInset),
        );
        _offer(local);
        return Listener(
          onPointerSignal: _onPointerSignal,
          child: GestureDetector(
            behavior: HitTestBehavior.opaque,
            onTap: widget.onTap,
            onScaleStart: _onScaleStart,
            onScaleUpdate: _onScaleUpdate,
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
              ),
            ),
          ),
        );
      },
    );
  }
}
