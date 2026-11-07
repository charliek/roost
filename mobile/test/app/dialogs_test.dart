import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/app/app.dart';
import 'package:roost_mobile/app/dialogs.dart';

void main() {
  group('validateHostTarget', () {
    test('accepts the forms the dialog advertises', () {
      for (final t in [
        'charlie@devbox',
        'charlie@devbox.tail0a1b.ts.net',
        'charlie@devbox:2222',
        'charlie@10.0.0.7',
        'charlie@[fd7a:115c::1]:22',
        'unix:/tmp/rm/roost.sock',
        '  charlie@popos  ',
      ]) {
        expect(validateHostTarget(t), isNull, reason: t);
      }
    });

    test('refuses what cannot be dialed', () {
      for (final t in [
        '',
        '   ',
        'charlie@',
        '@devbox',
        'charlie@dev box',
        'charlie@devbox:0',
        'charlie@devbox:70000',
        'charlie@devbox:ssh',
        'devbox',
        'unix:relative.sock',
      ]) {
        expect(validateHostTarget(t), isNotNull, reason: t);
      }
    });
  });

  test('width classes follow Material 3 breakpoints', () {
    expect(widthClassFor(400), WidthClass.compact);
    expect(widthClassFor(599.9), WidthClass.compact);
    expect(widthClassFor(600), WidthClass.medium);
    expect(widthClassFor(839), WidthClass.medium);
    expect(widthClassFor(840), WidthClass.expanded);
    expect(widthClassFor(1333), WidthClass.expanded);
  });
}
