import 'dart:async';

/// The latest value plus every change, replayed to each new listener. The
/// core's streams are "now and after every change", and one source feeds
/// however many widgets listen.
class ReplayFeed<T> {
  ReplayFeed([T? initial]) : _value = initial, _hasValue = initial != null;

  T? _value;
  bool _hasValue;
  Object? _error;
  final Set<MultiStreamController<T>> _listeners = {};

  Stream<T> get stream => Stream<T>.multi((c) {
    if (_hasValue) c.add(_value as T);
    if (_error != null) c.addError(_error!);
    _listeners.add(c);
    c.onCancel = () => _listeners.remove(c);
  });

  void add(T value) {
    _value = value;
    _hasValue = true;
    _error = null;
    for (final c in List.of(_listeners)) {
      c.add(value);
    }
  }

  void addError(Object error) {
    _error = error;
    for (final c in List.of(_listeners)) {
      c.addError(error);
    }
  }
}
