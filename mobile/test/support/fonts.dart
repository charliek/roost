import 'dart:io';

import 'package:flutter/services.dart';
import 'package:roost_mobile/terminal/cell_metrics.dart';

bool _loaded = false;

/// Loads the bundled JetBrains Mono from `assets/fonts` so painter output
/// (and golden files) do not depend on the test font.
Future<void> loadTerminalFont() async {
  if (_loaded) return;
  final loader = FontLoader(terminalFontFamily);
  for (final face in ['Regular', 'Bold', 'Italic', 'BoldItalic']) {
    final bytes = File('assets/fonts/JetBrainsMono-$face.ttf')
        .readAsBytesSync();
    loader.addFont(Future.value(ByteData.sublistView(bytes)));
  }
  await loader.load();
  _loaded = true;
}
