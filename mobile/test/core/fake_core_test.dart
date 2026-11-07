import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/core/fake/fake_core.dart';
import 'package:roost_mobile/core/fake/fake_screen.dart';
import 'package:roost_mobile/core/roost_core.dart';
import 'package:roost_mobile/terminal/attached_tab.dart';
import 'package:roost_mobile/terminal/frame.dart';

const _geometry = Geometry(
  cols: 40,
  rows: 10,
  cellWidthPx: 20,
  cellHeightPx: 40,
);

Future<AttachedTab> _attach(
  FakeCore core,
  int tabId, {
  String host = 'devbox',
}) async {
  final tab = AttachedTab(
    await core.attach(host, tabId, _geometry, take: true),
  );
  await pumpEventQueue();
  await tab.pullFrame();
  return tab;
}

void main() {
  late FakeCore core;

  setUp(() => core = FakeCore());
  tearDown(() => core.dispose());

  test('hosts: one connected, one needing trust, one failed', () async {
    final hosts = await core.hosts().first;
    expect(
      {for (final h in hosts) h.id: h.state.runtimeType},
      {'devbox': HostConnected, 'mac-mini': HostNeedsTrust, 'nas': HostFailed},
    );
    final devbox = hosts.first;
    expect(devbox.waitingTabs, 1);
    expect(devbox.workingTabs, 2);
  });

  test(
    'trusting the offered key connects the host and shows its workspace',
    () async {
      await core.trustHostKey('mac-mini', FakeCore.macMiniFingerprint);
      final hosts = await core.hosts().first;
      expect(hosts[1].state, const HostConnected());
      expect(core.trusted, [('mac-mini', FakeCore.macMiniFingerprint)]);
      expect(
        (await core.workspace('mac-mini').first).projects.single.tabs,
        hasLength(1),
      );
    },
  );

  test('a wrong fingerprint is refused', () async {
    expect(
      core.trustHostKey('mac-mini', 'SHA256:nope'),
      throwsA(isA<CoreError>()),
    );
  });

  test('retrying an unreachable host fails again', () async {
    await core.connect('nas');
    expect((await core.hosts().first)[2].state, isA<HostFailed>());
  });

  test('ops change the workspace', () async {
    final opened = await core.op('devbox', 'tab.open', {
      'project_id': '2',
      'activate': false,
    });
    final id = int.parse(((opened! as Map)['tab'] as Map)['id'] as String);
    var ws = await core.workspace('devbox').first;
    expect(ws.tab(id)?.title, 'zsh');
    await core.op('devbox', 'tab.set_title', {
      'tab_id': '$id',
      'title': 'logs',
    });
    ws = await core.workspace('devbox').first;
    expect(ws.tab(id)?.title, 'logs');
    await core.op('devbox', 'tab.close', {'tab_id': '$id'});
    ws = await core.workspace('devbox').first;
    expect(ws.tab(id), isNull);
    expect(core.op('devbox', 'tab.frobnicate'), throwsA(isA<CoreError>()));
  });

  test('an attach decodes into a screen showing the tab', () async {
    final tab = await _attach(core, 11);
    expect(tab.state, AttachState.live);
    expect(tab.owner?.isMe, isTrue);
    expect((tab.model.cols, tab.model.rows), (40, 10));
    final text = [for (var r = 0; r < tab.model.rows; r++) tab.model.rowText(r)]
        .join('\n');
    expect(text, contains('2. Yes'));
    expect(text, contains('(esc)'));
  });

  test('typed text echoes into the next frame', () async {
    final tab = await _attach(core, 13);
    await tab.session.text('echo hi');
    await tab.session.key(const KeyInput('Enter'));
    await pumpEventQueue();
    expect(tab.framePending, isTrue);
    await tab.pullFrame();
    final rows = [
      for (var r = 0; r < tab.model.rows; r++) tab.model.rowText(r),
    ];
    expect(rows, contains(r'~/projects/roost $ echo hi'));
    expect(rows, contains('hi'));
  });

  test(
    'another client taking the size is announced, and taking it back works',
    () async {
      final tab = await _attach(core, 12);
      core.takeSizeFrom(
        'devbox',
        12,
        label: 'charlie-laptop',
        cols: 132,
        rows: 40,
      );
      await pumpEventQueue();
      expect(tab.sizedElsewhere, isTrue);
      expect(tab.owner?.label, 'charlie-laptop');
      await tab.pullFrame();
      expect((tab.model.cols, tab.model.rows), (132, 40));
      await tab.session.takeSize();
      await pumpEventQueue();
      expect(tab.sizedElsewhere, isFalse);
      await tab.pullFrame();
      expect((tab.model.cols, tab.model.rows), (40, 10));
    },
  );

  test('typing takes the size back first (TAKE_BEFORE)', () async {
    final tab = await _attach(core, 13);
    core.takeSizeFrom(
      'devbox',
      13,
      label: 'charlie-laptop',
      cols: 100,
      rows: 30,
    );
    await pumpEventQueue();
    await tab.session.key(const KeyInput('a', text: 'a'));
    await pumpEventQueue();
    expect(tab.owner?.isMe, isTrue);
  });

  test('pausing detaches every tab and hands the size back', () async {
    final tab = await _attach(core, 11);
    await core.appPaused();
    await pumpEventQueue();
    expect(tab.state, AttachState.ended);
    expect(core.sessions, isEmpty);
    expect(core.pausedCount, 1);
  });

  test('scrollback scrolls by rows and reports a scrollbar', () async {
    final tab = await _attach(core, 13);
    for (var i = 0; i < 30; i++) {
      await tab.session.text('echo $i');
      await tab.session.key(const KeyInput('Enter'));
    }
    await tab.pullFrame();
    expect(tab.model.scrolledBack, isFalse);
    await tab.session.scroll(5, col: 0, row: 0);
    await pumpEventQueue();
    await tab.pullFrame();
    expect(tab.model.scrolledBack, isTrue);
    expect(
      tab.model.cursorVisible,
      isFalse,
      reason: 'the cursor is below the view',
    );
  });

  group('FakeScreen runs', () {
    test('follow the C6 rule: same style merges, blanks are gaps, wide stands alone', () {
      final screen = FakeScreen(cols: 20, rows: 2)
        ..write('ab', fg: 1)
        ..write('c', fg: 2)
        ..write('  ')
        ..write('漢', fg: 3)
        ..write('d', fg: 3);
      final runs = screen.takeFrame().rowData.single.runs;
      expect(runs.map((r) => (r.col, r.cells, r.text)).toList(), [
        (0, 2, 'ab'),
        (2, 1, 'c'),
        (5, 2, '漢'),
        (7, 1, 'd'),
      ]);
      expect(runs[2].has(RunAttrs.wide), isTrue);
    });
  });
}
