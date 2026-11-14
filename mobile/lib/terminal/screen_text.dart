import 'terminal_model.dart';

/// The screen's rows as plain text, trailing blank rows dropped.
String screenText(TerminalModel model) {
  final lines = [for (var r = 0; r < model.rows; r++) model.rowText(r)];
  while (lines.isNotEmpty && lines.last.isEmpty) {
    lines.removeLast();
  }
  return lines.join('\n');
}

final _urlStart = RegExp(r'''(?:https?|ftp|file)://[^\s<>"'`]+''');
final _continuation = RegExp(r'''^[^\s<>"'`]+''');
final _scheme = RegExp(r'^[a-z][a-z0-9+.-]*://', caseSensitive: false);
const _trailing = '.,;:!?';

/// The URLs on screen, in order and without repeats. A URL that ends in the
/// last column continues at the start of the next row, so one that wrapped
/// comes back whole: a terminal's soft wrap and Ink's hard wrap (Claude
/// Code's login link) both fill every row of it to the edge. A row that ends
/// short of the edge, or a next row that starts with a space or a new URL,
/// ends it.
List<String> screenUrls(TerminalModel model) {
  final lines = [for (var r = 0; r < model.rows; r++) model.rowText(r)];

  final urls = <String>[];
  // Characters at the start of a row already taken as a continuation.
  final consumed = List<int>.filled(lines.length, 0);
  for (var r = 0; r < lines.length; r++) {
    for (final match in _urlStart.allMatches(lines[r])) {
      if (match.start < consumed[r]) continue;
      final url = StringBuffer(match[0]!);
      var row = r;
      var end = match.end;
      while (end == lines[row].length &&
          _endCol(model, row) >= model.cols &&
          row + 1 < lines.length) {
        final next = _continuation.firstMatch(lines[row + 1]);
        if (next == null || _scheme.hasMatch(next[0]!)) break;
        url.write(next[0]!);
        row++;
        end = next.end;
        consumed[row] = end;
      }
      final trimmed = _trimUrl(url.toString());
      if (trimmed.contains('://') && !urls.contains(trimmed)) {
        urls.add(trimmed);
      }
    }
  }
  return urls;
}

/// The column after the row's last non-blank cell. Counted in cells, not
/// characters: a wide character is one character in two cells.
int _endCol(TerminalModel model, int r) {
  final runs = model.row(r)?.runs ?? const [];
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
