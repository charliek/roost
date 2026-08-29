//! The two streaming client transports, against a server that will
//! misbehave on demand (plan 037 C2).
//!
//! [`crates/roost-session/tests/attach_stream_test.rs`] runs the same
//! code against a real session and proves it decodes that session's
//! screen. This file proves the other half: what the client does when
//! the wire is wrong. The stub in [`support`] serves a script — a seq
//! gap, a duplicate, an identity that does not match, an EOF part-way
//! through a snapshot — so each of those is a written-down expectation
//! rather than something only a production incident produces.
//!
//! The split of responsibility asserted throughout: the framer enforces
//! the length cap, [`ServerFrame`] enforces the per-type widths, and
//! seq contiguity belongs to the endpoint that owns the terminal — so
//! a gap arrives here verbatim, and the assertion is that it arrives
//! *visibly*.

mod support;

use std::future::Future;
use std::time::Duration;

use roost_ipc::client::{
    ClientError, DataConnection, EventFrame, EventStream, ServerCode, ServerFrame,
};
use roost_ipc::dataframe::{
    write_data_frame, write_preamble, DataFrame, FRAME_INPUT, FRAME_RESIZE, MAX_DATA_FRAME_BYTES,
    PREAMBLE,
};
use roost_ipc::framing::write_frame;
use roost_ipc::messages::{
    ops, AttachAccepted, AttachHandshake, AttachHandshakeReply, AttachMode, AttachPayloadKind,
    EventEnvelope, ResponseError, SESSION_PROTOCOL_VERSION,
};
use roost_ipc::{Error, IpcClient};
use support::{accepted, End, Handshake, Plan, Push, Serve, Stub, Subscribe, STUB_EPOCH};

/// Every wait here is against a deterministic script, so a budget this
/// generous only ever fires on a hang — which is a failure, not a slow
/// machine.
const BUDGET: Duration = Duration::from_secs(10);

async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    // Scaled by `ROOST_TEST_TIMEOUT_SCALE`, the knob CI turns up on a
    // loaded runner — the same one the rest of the suite reads.
    let budget = BUDGET.mul_f64(roost_ipc::session_launch::timeout_scale());
    tokio::time::timeout(budget, future)
        .await
        .unwrap_or_else(|_| panic!("{what} never finished inside its budget"))
}

/// Dial the stub and run the ordinary snapshot handshake, discarding the
/// accepted reply — the shape most of the data-plane lanes below want.
async fn dial(stub: &Stub) -> DataConnection {
    within(
        "the dial",
        DataConnection::dial(stub.path(), &AttachHandshake::snapshot("t")),
    )
    .await
    .expect("accepted")
    .1
}

fn rejected(code: &str) -> ResponseError {
    ResponseError {
        code: code.into(),
        message: "…".into(),
    }
}

fn event(name: &str) -> EventEnvelope {
    EventEnvelope {
        event: name.into(),
        data: serde_json::json!({}),
    }
}

// ---------------------------------------------------------------------
// The events push reader
// ---------------------------------------------------------------------

/// The ack is a fence and the first batch is `revision + 1`. An empty
/// commit rides as an empty batch rather than being skipped, which is
/// the whole reason a gap can mean loss and nothing else.
#[tokio::test]
async fn the_ack_fences_the_stream_and_every_commit_arrives() {
    let stub = Stub::start(
        Plan::new()
            .subscribe(Subscribe::Ack(42))
            .push(Push::batch(43, vec![event(ops::EVENT_TAB_TITLE_CHANGED)]))
            .push(Push::empty(44))
            .push(Push::batch(45, vec![event(ops::EVENT_TAB_CLOSED)])),
    )
    .await;

    let mut stream = within(
        "the subscribe",
        EventStream::connect(stub.path(), "9f2c1d7a"),
    )
    .await
    .expect("the stub acks the subscribe");
    assert_eq!(
        stream.revision(),
        42,
        "the ack's revision is what a tab.list is fenced against"
    );

    let mut seen = Vec::new();
    for _ in 0..3 {
        match within("a batch", stream.next()).await.expect("a frame") {
            Some(EventFrame::Batch(batch)) => seen.push(batch),
            other => panic!("expected a batch, got {other:?}"),
        }
    }
    assert_eq!(
        seen.iter().map(|b| b.revision).collect::<Vec<_>>(),
        vec![43, 44, 45]
    );
    assert!(
        seen[1].events.is_empty(),
        "the empty commit is a real batch"
    );
    assert_eq!(seen[0].events[0].event, ops::EVENT_TAB_TITLE_CHANGED);

    // The script is done and the stub closed: a clean end, not an error.
    assert!(within("the close", stream.next())
        .await
        .expect("a clean close")
        .is_none());
    assert_eq!(stream.stopping_reason(), None, "nothing labeled this close");

    // The lease really went out — the ack is lease-gated on the wire.
    let requests = stub.recorded().requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].op, ops::EVENTS_SUBSCRIBE);
    assert_eq!(requests[0].params["lease"], "9f2c1d7a");
    assert_eq!(
        requests[0].params["tab_id_filter"], "0",
        "HS-2 subscribes unfiltered and filters client-side"
    );
}

/// The only loss signal the protocol offers. A client that missed it
/// would carry a mirror that silently diverges from the session's
/// workspace and could never tell.
#[tokio::test]
async fn a_skipped_revision_is_reported_as_loss() {
    let stub = Stub::start(
        Plan::new()
            .subscribe(Subscribe::Ack(7))
            .push(Push::empty(8))
            .push(Push::empty(10)),
    )
    .await;

    let mut stream = EventStream::connect(stub.path(), "lease")
        .await
        .expect("subscribed");
    within("the first batch", stream.next())
        .await
        .expect("a frame")
        .expect("a batch");
    match within("the gap", stream.next()).await {
        Err(ClientError::RevisionGap { expected, got }) => {
            assert_eq!((expected, got), (9, 10));
        }
        other => panic!("expected a RevisionGap, got {other:?}"),
    }
}

/// The terminal envelope is not a commit: it carries no revision, is
/// exempt from the gap check, and is the last frame before the close.
#[tokio::test]
async fn the_stopping_envelope_names_why_the_stream_ended() {
    for reason in ["stop", "taken-over"] {
        let stub = Stub::start(
            Plan::new()
                .subscribe(Subscribe::Ack(1))
                .push(Push::empty(2))
                .push(Push::Stopping(reason.into())),
        )
        .await;

        let mut stream = EventStream::connect(stub.path(), "lease")
            .await
            .expect("subscribed");
        within("the batch", stream.next())
            .await
            .expect("a frame")
            .expect("a batch");
        match within("the envelope", stream.next())
            .await
            .expect("a frame")
        {
            Some(EventFrame::Stopping(stopping)) => assert_eq!(stopping.reason, reason),
            other => panic!("expected the stopping envelope, got {other:?}"),
        }
        assert_eq!(stream.stopping_reason(), Some(reason));
        assert!(
            within("the close", stream.next())
                .await
                .expect("a clean close")
                .is_none(),
            "the envelope is always the last frame"
        );
    }
}

/// Additive control frames from a newer session must not break a client
/// that predates them — the versioning policy's whole point. The gap
/// check must not see them either: they are not commits.
#[tokio::test]
async fn an_unrecognized_control_envelope_is_ignored() {
    let stub = Stub::start(
        Plan::new()
            .subscribe(Subscribe::Ack(4))
            .push(Push::empty(5))
            .push(Push::Raw(
                serde_json::json!({"event": "session.something-new", "data": {"x": 1}}),
            ))
            .push(Push::empty(6)),
    )
    .await;

    let mut stream = EventStream::connect(stub.path(), "lease")
        .await
        .expect("subscribed");
    for want in [5, 6] {
        match within("a batch", stream.next()).await.expect("a frame") {
            Some(EventFrame::Batch(batch)) => assert_eq!(batch.revision, want),
            other => panic!("expected a batch, got {other:?}"),
        }
    }
}

/// A revision-bearing frame that is not a commit (no `events`) must not
/// advance the fence: decoding it as an empty batch would mask whatever
/// it actually was. Skipping it means a real commit at that number still
/// lands — and if the frame WAS a commit, the next batch trips the gap
/// check, which is the honest recovery.
#[tokio::test]
async fn a_revision_bearing_non_batch_does_not_advance_the_fence() {
    let stub = Stub::start(
        Plan::new()
            .subscribe(Subscribe::Ack(4))
            .push(Push::Raw(
                serde_json::json!({"revision": 5, "event": "future-control", "data": {}}),
            ))
            .push(Push::empty(5)),
    )
    .await;

    let mut stream = EventStream::connect(stub.path(), "lease")
        .await
        .expect("subscribed");
    match within("the real commit", stream.next())
        .await
        .expect("a frame")
    {
        Some(EventFrame::Batch(batch)) => assert_eq!(batch.revision, 5),
        other => panic!("expected batch 5, got {other:?}"),
    }
}

/// The terminal envelope latches the stream: a peer that keeps writing
/// after it (or just holds the socket open) must not have `next()` block
/// on — or yield — post-terminal frames.
#[tokio::test]
async fn the_stream_latches_closed_after_the_stopping_envelope() {
    let stub = Stub::start(
        Plan::new()
            .subscribe(Subscribe::Ack(4))
            .push(Push::Stopping("stop".into()))
            .push(Push::empty(5))
            .after_push(End::Hold),
    )
    .await;

    let mut stream = EventStream::connect(stub.path(), "lease")
        .await
        .expect("subscribed");
    match within("the stopping envelope", stream.next())
        .await
        .expect("a frame")
    {
        Some(EventFrame::Stopping(stopping)) => assert_eq!(stopping.reason, "stop"),
        other => panic!("expected stopping, got {other:?}"),
    }
    assert!(
        within("the latch", stream.next())
            .await
            .expect("latched, not blocked")
            .is_none(),
        "nothing after the terminal envelope is ever yielded"
    );
}

/// A `session.stopping` with no decodable reason is not a labeled
/// goodbye — the client falls through to the bare-EOF path the contract
/// prescribes for an unlabeled close.
#[tokio::test]
async fn a_reasonless_stopping_envelope_falls_back_to_bare_eof() {
    let stub = Stub::start(
        Plan::new()
            .subscribe(Subscribe::Ack(4))
            .push(Push::Raw(serde_json::json!({"event": "session.stopping"}))),
    )
    .await;

    let mut stream = EventStream::connect(stub.path(), "lease")
        .await
        .expect("subscribed");
    assert!(
        within("the close", stream.next())
            .await
            .expect("a clean close")
            .is_none(),
        "no reason means no Stopping frame — just the close"
    );
    assert_eq!(stream.stopping_reason(), None);
}

/// The two refusals instruct differently on purpose — go get a lease,
/// versus stop, somebody else drives this session now — so the client
/// state machine has to be able to tell them apart without reading
/// strings.
#[tokio::test]
async fn a_refused_subscribe_is_a_typed_refusal() {
    for (code, want) in [
        ("connect-required", ServerCode::ConnectRequired),
        ("taken-over", ServerCode::TakenOver),
    ] {
        let stub = Stub::start(Plan::new().subscribe(Subscribe::Reject(rejected(code)))).await;
        let error = EventStream::connect(stub.path(), "stale")
            .await
            .err()
            .expect("the subscribe is refused");
        assert_eq!(error.server_code(), Some(want));
    }
}

/// The same connection, before the flip: an ordinary op's refusal maps
/// through the identical seam.
#[tokio::test]
async fn a_refused_op_is_a_typed_refusal() {
    let stub = Stub::start(Plan::new().refuse(ops::TAB_ATTACH, "build-mismatch", "…")).await;
    let mut client = IpcClient::connect(stub.path()).await.expect("dial");
    let error = client
        .call_raw(ops::TAB_ATTACH, serde_json::json!({}))
        .await
        .expect_err("refused");
    assert_eq!(error.server_code(), Some(ServerCode::BuildMismatch));

    let error = client
        .call_raw("host.nothing", serde_json::json!({}))
        .await
        .expect_err("refused");
    assert_eq!(error.server_code(), Some(ServerCode::UnknownOp));
}

// ---------------------------------------------------------------------
// The data plane
// ---------------------------------------------------------------------

/// The whole handshake: one JSON line out, one back, the preamble, then
/// binary for the rest of the connection's life.
#[tokio::test]
async fn the_dial_negotiates_and_then_reads_frames() {
    let stub = Stub::start(
        Plan::new()
            .handshake(Handshake::Accept(accepted(AttachMode::Snapshot, 8813)))
            .serve(Serve::Snap(b"GHOSTSNP\x00".to_vec()))
            .serve(Serve::pty(8814, b"hello"))
            .serve(Serve::Exit {
                final_seq: 8815,
                code: 0,
            }),
    )
    .await;

    let request = AttachHandshake::snapshot("1a0be5c3");
    let (accepted, mut data) = within("the dial", DataConnection::dial(stub.path(), &request))
        .await
        .expect("the handshake is accepted");
    assert_eq!(accepted.mode, AttachMode::Snapshot);
    assert_eq!(accepted.seq, 8813, "the fence: the next PTY seq is 8814");
    assert_eq!(accepted.kind.as_str(), AttachPayloadKind::GHOSTTY_SNAPSHOT);

    let mut frames = Vec::new();
    while let Some(frame) = within("a frame", data.next_server_frame())
        .await
        .expect("frame read")
    {
        frames.push(frame);
    }
    assert_eq!(
        frames,
        vec![
            ServerFrame::Snap(b"GHOSTSNP\x00".to_vec()),
            ServerFrame::Pty {
                seq: 8814,
                bytes: b"hello".to_vec()
            },
            ServerFrame::Exit {
                final_seq: 8815,
                code: 0
            },
        ]
    );

    // What the client actually asked for, at the protocol version this
    // build speaks.
    let handshakes = stub.recorded().handshakes();
    assert_eq!(handshakes.len(), 1);
    assert_eq!(handshakes[0].attach, "1a0be5c3");
    assert_eq!(handshakes[0].protocol_version, SESSION_PROTOCOL_VERSION);
    assert_eq!(handshakes[0].resume_from_seq, None);
}

/// A refusal is the connection's last word and nothing binary follows
/// it, so a client that got one never has to guess whether the bytes
/// after it are frames. Each code maps to the variant the state machine
/// branches on.
#[tokio::test]
async fn a_refused_handshake_is_typed_and_never_turns_binary() {
    for (code, want) in [
        ("protocol-mismatch", ServerCode::ProtocolMismatch),
        ("invalid-token", ServerCode::InvalidToken),
        ("taken-over", ServerCode::TakenOver),
        ("not-found", ServerCode::NotFound),
        ("snapshot-failed", ServerCode::SnapshotFailed),
        ("shutting-down", ServerCode::ShuttingDown),
        ("not-supported", ServerCode::NotSupported),
    ] {
        let stub = Stub::start(
            Plan::new()
                .handshake(Handshake::Reject(rejected(code)))
                // Scripted but unreachable: a refusal returns before
                // the preamble, so a client that read on anyway would
                // decode these and this assertion would not hold.
                .serve(Serve::pty(1, b"never")),
        )
        .await;
        let error = within(
            "the refusal",
            DataConnection::dial(stub.path(), &AttachHandshake::snapshot("t")),
        )
        .await
        .err()
        .expect("the handshake is refused");
        assert_eq!(error.server_code(), Some(want));
    }
}

/// The load-bearing hand-off: the handshake line and the first binary
/// bytes routinely arrive in one read, so the line reader's residue has
/// to reach the frame reader. A slice reader delivers all of it in a
/// single `read`, which makes the hazard certain rather than likely.
#[tokio::test]
async fn the_line_reader_residue_carries_into_the_frame_reader() {
    let mut wire = Vec::new();
    let reply = AttachHandshakeReply::Accepted(AttachAccepted {
        kind: AttachPayloadKind::from(AttachPayloadKind::GHOSTTY_SNAPSHOT),
        mode: AttachMode::Resume,
        seq: 100,
        server_epoch: STUB_EPOCH,
        tab_generation: 3,
    });
    write_frame(&mut wire, &serde_json::to_vec(&reply).unwrap())
        .await
        .unwrap();
    write_preamble(&mut wire).await.unwrap();
    let mut payload = 101u64.to_le_bytes().to_vec();
    payload.extend_from_slice(b"resumed");
    write_data_frame(&mut wire, roost_ipc::dataframe::FRAME_PTY, &payload)
        .await
        .unwrap();

    let sent = Vec::<u8>::new();
    let (accepted, mut data) = DataConnection::handshake(
        wire.as_slice(),
        sent,
        &AttachHandshake::resume("t", 101, STUB_EPOCH, 3),
    )
    .await
    .expect("accepted");
    assert_eq!(accepted.mode, AttachMode::Resume);
    assert_eq!(
        data.next_server_frame().await.expect("frame read"),
        Some(ServerFrame::Pty {
            seq: 101,
            bytes: b"resumed".to_vec()
        }),
        "the frame that shared a read with the handshake line survived"
    );
}

/// Contiguity is the endpoint's rule, not the transport's, so the
/// library's job is to make the fault visible rather than to hide it.
/// Both shapes the wire can produce are scripted here; C5's decode loop
/// is what turns either into a re-attach.
#[tokio::test]
async fn a_seq_gap_and_a_duplicate_both_reach_the_client_verbatim() {
    for (name, seqs, want) in [
        ("a gap", vec![1u64, 2, 4], vec![1u64, 2, 4]),
        ("a duplicate", vec![1, 2, 2], vec![1, 2, 2]),
    ] {
        let mut plan = Plan::new().handshake(Handshake::Accept(accepted(AttachMode::Resume, 0)));
        for seq in seqs {
            plan = plan.serve(Serve::pty(seq, b"x"));
        }
        let stub = Stub::start(plan).await;
        let mut data = dial(&stub).await;

        let mut got = Vec::new();
        while let Some(frame) = within(name, data.next_server_frame())
            .await
            .expect("frame read")
        {
            match frame {
                ServerFrame::Pty { seq, .. } => got.push(seq),
                other => panic!("expected PTY, got {other:?}"),
            }
        }
        assert_eq!(got, want, "{name} must survive the transport unaltered");
    }
}

/// The resume identity is what makes a stale stream unresumable by
/// construction. The client hands both values up; a mismatch is a
/// comparison the endpoint makes, and this pins that both halves of it
/// are on the table.
#[tokio::test]
async fn a_mismatched_resume_identity_is_visible_on_both_sides() {
    let wrong = AttachAccepted {
        kind: AttachPayloadKind::from(AttachPayloadKind::GHOSTTY_SNAPSHOT),
        mode: AttachMode::Snapshot,
        seq: 0,
        server_epoch: STUB_EPOCH ^ 0xFFFF,
        tab_generation: 9,
    };
    let stub = Stub::start(Plan::new().handshake(Handshake::Accept(wrong.clone()))).await;

    let asked = AttachHandshake::resume("t", 512, STUB_EPOCH, 3);
    let (accepted, _data) = DataConnection::dial(stub.path(), &asked)
        .await
        .expect("accepted");
    assert_eq!(stub.recorded().handshakes()[0], asked);
    assert_ne!(accepted.server_epoch, STUB_EPOCH);
    assert_ne!(accepted.tab_generation, 3);
    assert_eq!(
        accepted.mode,
        AttachMode::Snapshot,
        "a resume the session cannot honor falls back in the same reply"
    );
}

/// The distinction the framer draws is what a client needs here: an EOF
/// at a frame boundary is "the peer finished" and an EOF part-way
/// through one is "the peer died mid-frame". Both are re-attach, but
/// only the second is a fault.
#[tokio::test]
async fn eof_before_finish_ends_the_stream_at_a_frame_boundary() {
    let stub = Stub::start(
        Plan::new()
            .serve(Serve::Snap(b"GHOSTSNP\x00".to_vec()))
            .serve(Serve::Snap(b"half a snapshot".to_vec()))
            .after_serve(End::Close),
    )
    .await;
    let mut data = dial(&stub).await;
    for _ in 0..2 {
        within("a SNAP frame", data.next_server_frame())
            .await
            .expect("frame read")
            .expect("a frame");
    }
    assert!(
        within("the EOF", data.next_server_frame())
            .await
            .expect("a clean boundary")
            .is_none(),
        "no FINISH ever arrived, but the stream ended cleanly"
    );
}

#[tokio::test]
async fn an_eof_inside_a_frame_is_a_fault() {
    // A header claiming four bytes, with two behind it.
    let stub = Stub::start(Plan::new().serve(Serve::Bytes(vec![
        4,
        0,
        0,
        0,
        roost_ipc::dataframe::FRAME_SNAP,
        1,
        2,
    ])))
    .await;
    let mut data = dial(&stub).await;
    match within("the truncated frame", data.next_server_frame()).await {
        Err(Error::UnexpectedEof) => {}
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
}

/// `ERROR` carries the stable codes a client branches on: re-attach,
/// passive detach, or the host's takeover state.
#[tokio::test]
async fn error_frames_map_to_the_codes_the_state_machine_branches_on() {
    for (code, want) in [
        ("desync", ServerCode::Desync),
        ("overflow", ServerCode::Overflow),
        ("superseded", ServerCode::Superseded),
        ("taken-over", ServerCode::TakenOver),
        ("shutting-down", ServerCode::ShuttingDown),
        ("protocol-error", ServerCode::ProtocolError),
    ] {
        let stub = Stub::start(Plan::new().serve(Serve::error(code, "…"))).await;
        let mut data = dial(&stub).await;
        match within("the ERROR frame", data.next_server_frame())
            .await
            .expect("frame read")
        {
            Some(ServerFrame::Error(error)) => {
                assert_eq!(ServerCode::from(&error), want);
            }
            other => panic!("expected an ERROR frame, got {other:?}"),
        }
    }
}

/// A payload the framer accepts but the protocol forbids is the
/// endpoint's to reject, and the error has to name what it saw.
#[tokio::test]
async fn a_malformed_payload_is_rejected_by_the_endpoint() {
    let stub = Stub::start(Plan::new().serve(Serve::Raw {
        frame_type: roost_ipc::dataframe::FRAME_EXIT,
        payload: vec![0; 11],
    }))
    .await;
    let mut data = dial(&stub).await;
    match within("the short EXIT", data.next_server_frame()).await {
        Err(Error::DataProtocol(message)) => assert!(message.contains("12 bytes"), "{message}"),
        other => panic!("expected DataProtocol, got {other:?}"),
    }
}

/// Splitting an oversized paste is the client's job, not something the
/// server will do for it — so the split has to live in the library
/// every client shares.
#[tokio::test]
async fn a_paste_past_the_cap_is_split_across_input_frames() {
    let stub = Stub::start(Plan::new().after_serve(End::Hold)).await;
    let mut data = dial(&stub).await;

    let paste = vec![b'p'; MAX_DATA_FRAME_BYTES + 17];
    data.send_input(&paste).await.expect("send the paste");
    data.send_resize(120, 40, 9, 18).await.expect("send resize");
    // Nothing is written for an empty paste: an empty INPUT frame is a
    // protocol error on this wire.
    data.send_input(b"").await.expect("send nothing");

    let frames = within("the frames to arrive", stub.frames_at_least(3)).await;
    assert_eq!(frames.len(), 3, "two INPUT frames and one RESIZE");
    assert_eq!(frames[0].frame_type, FRAME_INPUT);
    assert_eq!(frames[0].payload.len(), MAX_DATA_FRAME_BYTES);
    assert_eq!(frames[1].frame_type, FRAME_INPUT);
    assert_eq!(frames[1].payload.len(), 17);
    assert_eq!(
        frames[2],
        DataFrame {
            frame_type: FRAME_RESIZE,
            payload: vec![120, 0, 40, 0, 9, 0, 18, 0],
        },
        "cols | rows | cell_w_px | cell_h_px, u16 little-endian"
    );
}

/// The shape C4 needs: the reader drives a decode loop on its own task
/// while input keeps flowing from wherever the UI runs.
#[tokio::test]
async fn a_split_connection_reads_and_writes_independently() {
    let stub = Stub::start(
        Plan::new()
            .serve(Serve::pty(1, b"echo"))
            .after_serve(End::Hold),
    )
    .await;
    let data = dial(&stub).await;
    let (mut reader, mut writer) = data.into_split();

    let pump = tokio::spawn(async move { reader.next_frame().await });
    writer.send_input(b"typed").await.expect("send");

    let frame = within("the read half", pump)
        .await
        .expect("join")
        .expect("frame read")
        .expect("a frame");
    assert!(matches!(
        ServerFrame::decode(frame).expect("decode"),
        ServerFrame::Pty { seq: 1, .. }
    ));

    let sent = within("the write half", stub.frames_at_least(1)).await;
    assert_eq!(sent[0].payload, b"typed");
}

/// A peer that is not speaking this protocol at all must be refused
/// before anything downstream tries to parse what follows.
#[tokio::test]
async fn a_wrong_preamble_is_refused() {
    let mut wire = Vec::new();
    let reply = AttachHandshakeReply::Accepted(accepted(AttachMode::Snapshot, 0));
    write_frame(&mut wire, &serde_json::to_vec(&reply).unwrap())
        .await
        .unwrap();
    let mut wrong = PREAMBLE;
    wrong[7] = b'1';
    wire.extend_from_slice(&wrong);

    match DataConnection::handshake(
        wire.as_slice(),
        Vec::<u8>::new(),
        &AttachHandshake::snapshot("t"),
    )
    .await
    {
        Err(ClientError::Io(Error::BadPreamble)) => {}
        other => panic!("expected BadPreamble, got {:?}", other.map(|_| ())),
    }
}
