import 'dart:async';

/// Runs calls that share a key one after another, in the order they were
/// made. Each call's own error reaches only its caller.
class SerialQueue {
  final Map<Object, Future<void>> _tails = {};

  Future<T> run<T>(Object key, Future<T> Function() call) {
    final previous = _tails[key] ?? Future<void>.value();
    final result = previous.then((_) => call());
    final tail = result.then<void>((_) {}, onError: (Object _) {});
    _tails[key] = tail;
    unawaited(
      tail.whenComplete(() {
        if (identical(_tails[key], tail)) _tails.remove(key);
      }),
    );
    return result;
  }
}
