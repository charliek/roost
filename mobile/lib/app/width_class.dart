import 'package:flutter/widgets.dart';

enum WidthClass { compact, medium, expanded }

/// Material 3 window size classes.
WidthClass widthClassFor(double width) => width >= 840
    ? WidthClass.expanded
    : width >= 600
    ? WidthClass.medium
    : WidthClass.compact;

/// The width class of the layout on screen, from `AppShell`.
class WidthClassScope extends InheritedWidget {
  const WidthClassScope({
    super.key,
    required this.widthClass,
    required super.child,
  });

  final WidthClass widthClass;

  /// Outside `AppShell` (a widget test of one pane), the screen's own width
  /// decides.
  static WidthClass of(BuildContext context) =>
      context
          .dependOnInheritedWidgetOfExactType<WidthClassScope>()
          ?.widthClass ??
      widthClassFor(MediaQuery.sizeOf(context).width);

  @override
  bool updateShouldNotify(WidthClassScope oldWidget) =>
      widthClass != oldWidget.widthClass;
}
