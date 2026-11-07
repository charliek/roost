import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/core/serial_queue.dart';

Future<void> _after(int ms, List<String> log, String entry) async {
  await Future<void>.delayed(Duration(milliseconds: ms));
  log.add(entry);
}

void main() {
  test(
    'calls for one key finish in call order, however long each takes',
    () async {
      final queue = SerialQueue();
      final log = <String>[];
      await Future.wait([
        queue.run('host', () => _after(30, log, '1')),
        queue.run('host', () => _after(0, log, 'Enter')),
        queue.run('host', () => _after(10, log, 'Esc')),
      ]);
      expect(log, ['1', 'Enter', 'Esc']);
    },
  );

  test('different keys do not wait for each other', () async {
    final queue = SerialQueue();
    final log = <String>[];
    await Future.wait([
      queue.run('slow', () => _after(30, log, 'slow')),
      queue.run('fast', () => _after(0, log, 'fast')),
    ]);
    expect(log, ['fast', 'slow']);
  });

  test('a failed call fails its caller and does not stop the next', () async {
    final queue = SerialQueue();
    final failed = queue.run<void>('host', () async => throw StateError('no'));
    final next = queue.run('host', () async => 'sent');
    await expectLater(failed, throwsStateError);
    expect(await next, 'sent');
  });
}
