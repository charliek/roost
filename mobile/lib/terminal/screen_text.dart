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
final _continuation = RegExp(r'''^\s*([^\s<>"'`]+)''');
const _trailing = '.,;:!?';

/// A row this close to the right edge may have wrapped: a program that
/// wraps itself (Ink, under a padded box) stops a column or two short.
const _wrapSlack = 4;

/// The URLs on screen, in order and without repeats. A URL that runs to the
/// right edge continues into the next row's first word, so one that wrapped
/// (a login link, say) comes back whole.
List<String> screenUrls(TerminalModel model) {
  final lines = [for (var r = 0; r < model.rows; r++) model.rowText(r)];
  bool nearEdge(String line) => line.length >= model.cols - _wrapSlack;

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
          nearEdge(lines[row]) &&
          row + 1 < lines.length) {
        final next = _continuation.firstMatch(lines[row + 1]);
        if (next == null) break;
        url.write(next[1]!);
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
