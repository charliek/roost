import 'frame.dart';
import 'terminal_model.dart';

/// The screen's rows as plain text, trailing blank rows dropped.
String screenText(TerminalModel model) {
  final lines = [for (var r = 0; r < model.rows; r++) model.rowText(r)];
  while (lines.isNotEmpty && lines.last.isEmpty) {
    lines.removeLast();
  }
  return lines.join('\n');
}

/// URL characters (RFC 3986, plus letters and marks of any script): a box
/// border, a quote or a space ends a URL.
final _url = RegExp(
  r'(?:https?|ftp|file)://[\p{L}\p{N}\p{M}\-._~:/?#\[\]@!$&()*+,;=%]+',
  unicode: true,
);
final _listMarker = RegExp(r'^[-*+•] ');
const _trailing = '.,;:!?';

/// The URLs on screen, in order and without repeats, found in the lines the
/// rows were before they wrapped, so a wrapped URL (Claude Code's login
/// link, which Ink hard-wraps to fill every row) comes back whole.
List<String> screenUrls(TerminalModel model) {
  final urls = <String>[];
  for (final line in _unwrappedLines(model)) {
    for (final match in _url.allMatches(line)) {
      final url = _trimUrl(match[0]!);
      if (_url.hasMatch(url) && !urls.contains(url)) urls.add(url);
    }
  }
  return urls;
}

/// The rows joined wherever one fills to the last column and the next goes
/// on with it. Nothing on screen tells a wrap from a line that ended exactly
/// at the edge, so such a line runs into the next row; a next row that
/// starts a new URL or a list item is kept apart.
List<String> _unwrappedLines(TerminalModel model) {
  final lines = <String>[];
  final line = StringBuffer();
  for (var r = 0; r < model.rows; r++) {
    final text = model.rowText(r);
    if (line.isNotEmpty &&
        (_url.matchAsPrefix(text) != null || _listMarker.hasMatch(text))) {
      line.write(' ');
    }
    line.write(text);
    if (r + 1 < model.rows && _reachesEdge(model, r)) continue;
    lines.add(line.toString());
    line.clear();
  }
  return lines;
}

bool _reachesEdge(TerminalModel model, int r) {
  final end = _endCol(model, r);
  if (end >= model.cols) return true;
  // A wide character that does not fit in the last cell wraps, leaving a
  // blank cell behind it.
  final next = model.row(r + 1)?.runs ?? const <FrameRun>[];
  return end == model.cols - 1 &&
      next.isNotEmpty &&
      next.first.col == 0 &&
      next.first.has(RunAttrs.wide);
}

/// The column after the row's last non-blank cell. Counted in cells, not
/// characters: a wide character is one character in two cells.
int _endCol(TerminalModel model, int r) {
  final runs = model.row(r)?.runs ?? const <FrameRun>[];
  for (final run in runs.reversed) {
    final trailingSpaces = run.text.length - run.text.trimRight().length;
    if (trailingSpaces == run.text.length) continue;
    return run.col + run.cells - trailingSpaces;
  }
  return 0;
}

/// Drops sentence punctuation and a closing bracket the URL never opened.
String _trimUrl(String url) {
  var s = url;
  while (s.isNotEmpty) {
    final last = s[s.length - 1];
    if (_trailing.contains(last)) {
      s = s.substring(0, s.length - 1);
    } else if (last == ')' && !_balanced(s, '(', ')')) {
      s = s.substring(0, s.length - 1);
    } else if (last == ']' && !_balanced(s, '[', ']')) {
      s = s.substring(0, s.length - 1);
    } else {
      break;
    }
  }
  return s;
}

bool _balanced(String s, String open, String close) =>
    open.allMatches(s).length >= close.allMatches(s).length;
