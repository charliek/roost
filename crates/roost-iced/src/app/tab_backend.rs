//! Where a tab's bytes come from and go.
//!
//! Two layers, because one enum value cannot serve both scopes:
//! [`TabBackend`] is app-scoped (`App` holds one) and answers the
//! questions asked before a terminal exists — attach, is this tab live,
//! what is it running in; [`TabHandle`] is per-tab (`TerminalTab` holds
//! one) and carries everything a live tab does to its backend.
//!
//! Only the in-process backend exists today. HS-2 adds a `Host` variant
//! whose attach is async and snapshot-driven, which is why attach is a
//! backend op returning a handle rather than something the tab builds
//! for itself. The policies that differ between the two modes —
//! [`ReplyPolicy`] and [`OscScanMode`] — are part of the seam from day
//! one for the same reason: rediscovering them later would mean
//! re-cutting a landed abstraction.

use roost_ui_model::keys::HostId;

use super::*;

pub(super) enum TabBackend {
    InProcess(InProcessBackend),
}

/// PTYs in this process, driven by the local supervisor.
pub(super) struct InProcessBackend {
    supervisor: Arc<PtySupervisor>,
    /// `ROOST_TEST_MODE=1`, decided once for the process — a backend is
    /// built once, so the per-tab input tap is a property of the backend
    /// rather than something every attach re-states.
    test_mode: bool,
}

/// What a backend does with replies the client-side VT generates — on
/// `vt_write`, on resize, and the synthesized mode-2031 theme
/// notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReplyPolicy {
    /// Drain them onto the tab's PTY input channel. This client owns the
    /// only VT for the tab, so its replies are the program's answers.
    DrainToPty,
    // HS-2: a host session's server runs the authoritative VT and
    // answers queries itself, so a client-side reply would double every
    // answer.
    #[expect(dead_code)]
    Discard,
}

/// Who scans a tab's byte stream for OSC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OscScanMode {
    /// This tab's own drain scans, tracks color state, and hands the
    /// non-reply actions up with the bytes (query replies are
    /// libghostty's job since the plan-032 pin).
    Scanned,
    // HS-2: the host server scans and reports effects as events, so the
    // client must not run a second router over the same bytes.
    #[expect(dead_code)]
    Unscanned,
}

impl TabBackend {
    pub(super) fn in_process(supervisor: Arc<PtySupervisor>, test_mode: bool) -> Self {
        Self::InProcess(InProcessBackend {
            supervisor,
            test_mode,
        })
    }

    /// The instance whose id-space this backend's bare ids belong to.
    ///
    /// The backend's own API stays bare i64 — it is engine-facing, and the
    /// engine is host-unaware — so this is the seam that says what those
    /// ids mean. HS-2's `Host` variant answers with its connection's
    /// minted instance instead, and every caller below is then correct
    /// without moving.
    pub(super) fn host(&self) -> HostId {
        match self {
            Self::InProcess(_) => HostId::LOCAL,
        }
    }

    /// Qualify one of this backend's bare ids. The single joint between
    /// the engine's id-space and the UI's keyed maps.
    pub(super) fn tab_key(&self, tab_id: i64) -> TabKey {
        TabKey::new(self.host(), tab_id)
    }

    /// Attach to a tab this backend already has a live session for, and
    /// start the forwarder that puts its output on `feed`.
    ///
    /// Must be called inside the app runtime (`Runtime::enter`): both the
    /// session's drain and the forwarder bind to the ambient runtime.
    pub(super) fn attach(
        &self,
        tab_id: i64,
        theme_colors: OscColorSnapshot,
        feed: EngineFeedSender,
    ) -> Result<TabHandle> {
        match self {
            Self::InProcess(backend) => backend.attach(self.tab_key(tab_id), theme_colors, feed),
        }
    }

    /// Whether the backend still has a session for this tab — asked
    /// before paying for a terminal the attach would then discard.
    pub(super) fn is_live(&self, tab_id: i64) -> bool {
        match self {
            Self::InProcess(backend) => backend.supervisor.has(tab_id),
        }
    }

    /// The working directory of the tab's foreground process, when the
    /// backend can see it.
    pub(super) fn foreground_cwd(&self, tab_id: i64) -> Option<String> {
        match self {
            Self::InProcess(backend) => backend.supervisor.foreground_cwd(tab_id),
        }
    }
}

impl InProcessBackend {
    fn attach(
        &self,
        key: TabKey,
        theme_colors: OscColorSnapshot,
        feed: EngineFeedSender,
    ) -> Result<TabHandle> {
        let capture = self.test_mode.then(|| Arc::new(Mutex::new(Vec::new())));
        let (output_tx, output_rx) = tokio::sync::mpsc::unbounded_channel();
        // The OSC opt-in: this session's forwarding task owns the sole
        // router for the tab. The UI keeps no router of its own — a
        // second one would double every scanned action.
        let session = TabSession::attach_scanned(
            Arc::clone(&self.supervisor),
            key.tab,
            output_tx,
            capture.clone(),
            Some(theme_colors),
        )?;
        // The per-tab channel stays — only its drain moves. This forwarder
        // is what puts PTY output in the same arrival order as everything
        // else the app reacts to, and what arms the feed's wake for it.
        let forwarder =
            tokio::spawn(engine_feed::pump_tab_output(key, output_rx, feed)).abort_handle();
        Ok(TabHandle {
            kind: TabHandleKind::InProcess(session),
            capture,
            forwarder,
        })
    }
}

/// One tab's live attachment to its backend.
pub(super) struct TabHandle {
    kind: TabHandleKind,
    /// Test-mode tap on the outbound input side, read back by
    /// `tab.capture_pty_input`. `None` in production.
    capture: Option<InputCapture>,
    forwarder: tokio::task::AbortHandle,
}

enum TabHandleKind {
    InProcess(TabSession),
}

impl Drop for TabHandle {
    /// The forwarder owns this tab's output receiver, so its life must
    /// end with the handle's. A tab that is built and then discarded —
    /// the failed-geometry arm of `reconcile` — would otherwise leave a
    /// live stream behind, and the retry that attaches the same session
    /// again cannot reuse the initial receiver (`TabSession::attach`
    /// falls back to a fresh subscription), so two streams would
    /// interleave into one terminal. Aborting drops the receiver, which
    /// ends the engine-side bridge on its next send.
    fn drop(&mut self) {
        self.forwarder.abort();
    }
}

impl TabHandle {
    fn reply_policy(&self) -> ReplyPolicy {
        match self.kind {
            TabHandleKind::InProcess(_) => ReplyPolicy::DrainToPty,
        }
    }

    fn osc_scan(&self) -> OscScanMode {
        match self.kind {
            TabHandleKind::InProcess(_) => OscScanMode::Scanned,
        }
    }

    pub(super) fn capture(&self) -> Option<&InputCapture> {
        self.capture.as_ref()
    }

    pub(super) fn send_input(&self, data: Vec<u8>) {
        match &self.kind {
            TabHandleKind::InProcess(session) => session.send_input(data),
        }
    }

    pub(super) fn send_resize(&self, cols: u16, rows: u16) {
        match &self.kind {
            TabHandleKind::InProcess(session) => session.send_resize(cols, rows),
        }
    }

    /// Emit replies the client-side VT produced, under this backend's
    /// [`ReplyPolicy`]. Every such reply goes through here — the ones
    /// `vt_write` leaves in the buffer, the ones a resize defers until
    /// its transaction commits, and the mode-2031 notification a theme
    /// change owes a program that asked for it.
    pub(super) fn send_replies(&self, data: Vec<u8>) {
        match self.reply_policy() {
            ReplyPolicy::DrainToPty => self.send_input(data),
            ReplyPolicy::Discard => {}
        }
    }

    /// Scan a chunk that did NOT arrive through this tab's drain — today
    /// only `tab.feed_pty_bytes`, the test-mode byte injector. Under
    /// [`OscScanMode::Scanned`] it runs the same router and the same
    /// color state the drain does, so an injected chunk is
    /// indistinguishable from a real one; the returned actions are the
    /// non-reply ones, exactly as `TabOutput::Scanned` carries them.
    pub(super) fn scan_osc(&self, bytes: &[u8]) -> Vec<OscAction> {
        match self.osc_scan() {
            OscScanMode::Scanned => match &self.kind {
                TabHandleKind::InProcess(session) => session.scan_osc(bytes),
            },
            OscScanMode::Unscanned => Vec::new(),
        }
    }

    /// Tell the backend which colors a theme change installed, so the
    /// answers it gives color queries move with the terminal's rendering.
    pub(super) fn reseed_theme(&self, colors: OscColorSnapshot) {
        match &self.kind {
            TabHandleKind::InProcess(session) => session.reseed_osc_colors(colors),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SOCKET: &str = "/tmp/roost-iced-tab-backend-test.sock";

    /// A backend with one live PTY running `argv`, plus a handle attached
    /// to it, the feed its forwarder writes to, and the supervisor the
    /// caller closes the tab with.
    fn attached(
        tab_id: i64,
        argv: &[String],
        test_mode: bool,
    ) -> (
        TabBackend,
        TabHandle,
        EngineFeedReceiver,
        Arc<PtySupervisor>,
    ) {
        let supervisor = Arc::new(PtySupervisor::new());
        let _early_output = supervisor
            .spawn(
                tab_id,
                "/tmp",
                argv,
                DEFAULT_COLS,
                DEFAULT_ROWS,
                std::path::Path::new(TEST_SOCKET),
            )
            .expect("spawn test PTY");
        let backend = TabBackend::in_process(Arc::clone(&supervisor), test_mode);
        let (feed_tx, feed_rx) = engine_feed::channel();
        let handle = backend
            .attach(
                tab_id,
                super::terminal_tab::theme_osc_colors(&Theme::roost_dark_fallback()),
                feed_tx,
            )
            .expect("attach through the backend");
        (backend, handle, feed_rx, supervisor)
    }

    fn cat(
        tab_id: i64,
    ) -> (
        TabBackend,
        TabHandle,
        EngineFeedReceiver,
        Arc<PtySupervisor>,
    ) {
        attached(tab_id, &["/bin/sh".into(), "-c".into(), "cat".into()], true)
    }

    fn captured(handle: &TabHandle) -> Vec<u8> {
        handle
            .capture()
            .expect("test-mode input capture")
            .lock()
            .expect("capture lock")
            .clone()
    }

    /// The policy surface the seam exists to carry: the in-process
    /// backend owns the tab's only VT, so its replies go to the PTY and
    /// its own drain does the OSC scan.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn in_process_replies_drain_to_the_pty_and_its_drain_scans() {
        let (_backend, handle, _feed, supervisor) = cat(8_601);
        assert_eq!(handle.reply_policy(), ReplyPolicy::DrainToPty);
        assert_eq!(handle.osc_scan(), OscScanMode::Scanned);
        drop(handle);
        supervisor.close(8_601);
    }

    /// `send_input` reaches the PTY: `cat` echoes the marker back through
    /// the forwarder the attach started.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_input_reaches_the_pty() {
        let (_backend, handle, mut feed, supervisor) = cat(8_602);
        handle.send_input(b"backend-marker\n".to_vec());
        let seen = feed_text_until(
            &mut feed,
            TabKey::local(8_602),
            "backend-marker",
            Duration::from_secs(5),
        )
        .await;
        assert!(
            seen.contains("backend-marker"),
            "input never round-tripped: {seen:?}"
        );
        assert_eq!(captured(&handle), b"backend-marker\n".to_vec());
        drop(handle);
        supervisor.close(8_602);
    }

    /// Under `DrainToPty` a reply takes the same path as a keystroke —
    /// what the write-side and resize-side drains rely on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_replies_takes_the_input_path_under_drain_to_pty() {
        let (_backend, handle, mut feed, supervisor) = cat(8_603);
        handle.send_replies(b"reply-marker\n".to_vec());
        let seen = feed_text_until(
            &mut feed,
            TabKey::local(8_603),
            "reply-marker",
            Duration::from_secs(5),
        )
        .await;
        assert!(
            seen.contains("reply-marker"),
            "reply never round-tripped: {seen:?}"
        );
        assert_eq!(captured(&handle), b"reply-marker\n".to_vec());
        drop(handle);
        supervisor.close(8_603);
    }

    /// `send_resize` reaches the PTY's window size — asked of the kernel
    /// itself, by a shell reading commands off the same tty.
    ///
    /// Resize and input share one serial channel, so the `stty` runs
    /// after the resize applied; `exit` ends the shell, which is what
    /// lets the supervisor's blocking reader see EOF instead of parking
    /// past the end of the test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_resize_reaches_the_pty() {
        let (_backend, handle, mut feed, supervisor) = attached(8_604, &["/bin/sh".into()], true);
        handle.send_resize(90, 40);
        handle.send_input(b"stty size\nexit\n".to_vec());
        let seen = feed_text_until(
            &mut feed,
            TabKey::local(8_604),
            "40 90",
            Duration::from_secs(10),
        )
        .await;
        assert!(
            seen.contains("40 90"),
            "resize never reached the PTY: {seen:?}"
        );
        drop(handle);
        supervisor.close(8_604);
    }

    /// `scan_osc` runs the drain's real router: a color query is silent
    /// (libghostty owns query replies since the plan-032 pin — the
    /// drain enqueues nothing, mirroring
    /// `osc_drain_reply_test::a_color_query_is_not_answered_by_the_drain`)
    /// while non-reply actions survive, and `reseed_theme` reaches the
    /// same shared state without disturbing the scan.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scan_osc_runs_the_router_and_reseed_theme_does_not_disturb_it() {
        let (_backend, handle, _feed, supervisor) = cat(8_605);
        let actions = handle.scan_osc(b"\x1b]11;?\x07\x1b]0;title\x07");
        assert_eq!(
            actions,
            vec![OscAction::Workspace {
                command: 0,
                payload: "title".into(),
            }],
            "the query produces nothing; the title action survives"
        );
        assert!(
            captured(&handle).is_empty(),
            "the drain must not answer a color query — libghostty does"
        );

        let mut recolored = Theme::roost_dark_fallback();
        recolored.background = roost_vt::ColorRgb {
            r: 0xfa,
            g: 0xce,
            b: 0x0d,
        };
        handle.reseed_theme(super::terminal_tab::theme_osc_colors(&recolored));
        let actions = handle.scan_osc(b"\x1b]11;?\x07\x1b]0;after-reseed\x07");
        assert_eq!(
            actions,
            vec![OscAction::Workspace {
                command: 0,
                payload: "after-reseed".into(),
            }],
            "reseed_theme shares the scan's lock without wedging it"
        );
        assert!(
            captured(&handle).is_empty(),
            "reseed must not resurrect drain-side query replies"
        );
        drop(handle);
        supervisor.close(8_605);
    }

    /// The capture is the test-mode tap and nothing else: production
    /// attaches have none.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_exists_only_in_test_mode() {
        let (_backend, handle, _feed, supervisor) =
            attached(8_606, &["/bin/sh".into(), "-c".into(), "cat".into()], false);
        assert!(handle.capture().is_none());
        drop(handle);
        supervisor.close(8_606);
    }

    /// `is_live` and `foreground_cwd` answer for the backend's own
    /// sessions and for nothing else.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn is_live_and_foreground_cwd_answer_for_the_backends_sessions() {
        let (backend, handle, _feed, supervisor) = cat(8_607);
        assert!(backend.is_live(8_607));
        assert!(!backend.is_live(8_608), "an unknown tab is not live");
        assert_eq!(backend.foreground_cwd(8_608), None);
        assert_eq!(
            backend.foreground_cwd(8_607),
            supervisor.foreground_cwd(8_607),
            "foreground_cwd is the supervisor's answer"
        );
        drop(handle);
        supervisor.close(8_607);
        assert!(!backend.is_live(8_607), "a closed tab stops being live");
    }
}
