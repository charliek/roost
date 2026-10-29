//! OSC 7501 program-status reports, OSC 133 prompt marks and the places of
//! OSC 9 / 777 desktop notifications, as libghostty delivers them through
//! `GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS`, `GHOSTTY_TERMINAL_OPT_SEMANTIC_PROMPT`
//! and `GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION`.
//!
//! All three callbacks push into one sink ([`crate::Terminal::set_status_sink`])
//! so they keep the byte order libghostty parsed them in, whatever the PTY
//! read boundaries were.

use std::os::raw::c_void;
use std::ptr;

use crate::sys;
use crate::terminal::{contain, CallbackCtx};

/// What a program says it is doing (`GhosttyProgramStatusState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramStatusState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    /// Not a state: remove the record with this id and its subtree, or
    /// every record when the id is empty (also what RIS reports).
    Clear,
}

/// What a blocked program needs from the user (`GhosttyProgramStatusKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramStatusKind {
    Permission,
    Question,
    Auth,
}

/// One OSC 7501 report, copied out of libghostty's callback. Text the
/// program didn't send is an empty string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatusReport {
    pub state: ProgramStatusState,
    pub kind: Option<ProgramStatusKind>,
    /// 0 through 100; `None` when the program sent none, or one libghostty
    /// rejected as out of range.
    pub progress: Option<u8>,
    pub id: String,
    pub app: String,
    pub title: String,
    pub message: String,
}

/// An OSC 133 step. `PromptStart` is reported only for a primary prompt;
/// right, continuation and secondary prompts are not a new command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMark {
    PromptStart,
    InputStart,
    OutputStart,
    CommandEnd,
}

/// What the status sink receives, in callback order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    Status(ProgramStatusReport),
    Prompt(PromptMark),
    /// An OSC 9 or OSC 777 desktop notification, where it fell in the
    /// stream. roost-osc's scan of the same bytes is what raises it; the
    /// text is here so that scan's notification can find its own place.
    /// OSC 9 has no title, so its text is the body.
    Notification {
        title: String,
        body: String,
    },
}

/// Whether a libghostty sized struct, whose leading field is its `size_t`
/// size, covers the whole version Roost was built against. A shorter one
/// is dropped; a longer one is read for the fields Roost knows.
///
/// # Safety
///
/// `raw` must be non-null and point to a struct libghostty passed to a
/// callback, readable for at least its leading `size_t` and then for
/// the `size` bytes that field states, for the duration of the call.
unsafe fn covers_v1<T>(raw: *const T) -> bool {
    // SAFETY: the caller guarantees the leading `size_t` is readable. Read
    // unaligned and by value, so no reference to the struct exists before
    // its size is known.
    let size = unsafe { ptr::read_unaligned(raw.cast::<usize>()) };
    if size < std::mem::size_of::<T>() {
        tracing::debug!(
            size,
            "dropping a libghostty callback struct shorter than v1"
        );
        return false;
    }
    true
}

/// # Safety
///
/// `s.ptr` must be null or valid for `s.len` bytes for the duration of
/// the call.
unsafe fn copy_string(s: sys::GhosttyString) -> String {
    if s.ptr.is_null() || s.len == 0 {
        return String::new();
    }
    // SAFETY: the caller guarantees `ptr`/`len` describe live bytes.
    let bytes = unsafe { std::slice::from_raw_parts(s.ptr, s.len) };
    String::from_utf8_lossy(bytes).into_owned()
}

fn state_from_sys(raw: sys::GhosttyProgramStatusState) -> Option<ProgramStatusState> {
    Some(match raw {
        sys::GhosttyProgramStatusState_GHOSTTY_PROGRAM_STATUS_STATE_IDLE => {
            ProgramStatusState::Idle
        }
        sys::GhosttyProgramStatusState_GHOSTTY_PROGRAM_STATUS_STATE_WORKING => {
            ProgramStatusState::Working
        }
        sys::GhosttyProgramStatusState_GHOSTTY_PROGRAM_STATUS_STATE_DONE => {
            ProgramStatusState::Done
        }
        sys::GhosttyProgramStatusState_GHOSTTY_PROGRAM_STATUS_STATE_BLOCKED => {
            ProgramStatusState::Blocked
        }
        sys::GhosttyProgramStatusState_GHOSTTY_PROGRAM_STATUS_STATE_ERROR => {
            ProgramStatusState::Error
        }
        sys::GhosttyProgramStatusState_GHOSTTY_PROGRAM_STATUS_STATE_CLEAR => {
            ProgramStatusState::Clear
        }
        _ => return None,
    })
}

fn kind_from_sys(raw: sys::GhosttyProgramStatusKind) -> Option<ProgramStatusKind> {
    Some(match raw {
        sys::GhosttyProgramStatusKind_GHOSTTY_PROGRAM_STATUS_KIND_PERMISSION => {
            ProgramStatusKind::Permission
        }
        sys::GhosttyProgramStatusKind_GHOSTTY_PROGRAM_STATUS_KIND_QUESTION => {
            ProgramStatusKind::Question
        }
        sys::GhosttyProgramStatusKind_GHOSTTY_PROGRAM_STATUS_KIND_AUTH => ProgramStatusKind::Auth,
        _ => return None,
    })
}

fn progress_from_sys(raw: i8) -> Option<u8> {
    u8::try_from(raw).ok().filter(|p| *p <= 100)
}

fn mark_from_sys(
    kind: sys::GhosttySemanticPromptKind,
    prompt_kind: sys::GhosttySemanticPromptPromptKind,
) -> Option<PromptMark> {
    match kind {
        sys::GhosttySemanticPromptKind_GHOSTTY_SEMANTIC_PROMPT_PROMPT_START
            if prompt_kind
                == sys::GhosttySemanticPromptPromptKind_GHOSTTY_SEMANTIC_PROMPT_PROMPT_PRIMARY =>
        {
            Some(PromptMark::PromptStart)
        }
        sys::GhosttySemanticPromptKind_GHOSTTY_SEMANTIC_PROMPT_INPUT_START => {
            Some(PromptMark::InputStart)
        }
        sys::GhosttySemanticPromptKind_GHOSTTY_SEMANTIC_PROMPT_OUTPUT_START => {
            Some(PromptMark::OutputStart)
        }
        sys::GhosttySemanticPromptKind_GHOSTTY_SEMANTIC_PROMPT_COMMAND_END => {
            Some(PromptMark::CommandEnd)
        }
        _ => None,
    }
}

/// The body every status trampoline shares: drop a null pointer or a
/// struct shorter than v1 before `read` touches it, then push what `read`
/// makes of it.
///
/// # Safety
///
/// `userdata` and `raw` must be what libghostty passed the callback: the
/// `Terminal`'s `CallbackCtx` (set from construction and freed only after
/// the terminal handle is) and a sized struct valid for the call, or null.
unsafe fn push_event<T>(
    userdata: *mut c_void,
    raw: *const T,
    read: unsafe fn(*const T) -> Option<TerminalEvent>,
) {
    if userdata.is_null() || raw.is_null() {
        return;
    }
    // SAFETY: the caller's contract, and `raw` is non-null.
    if !unsafe { covers_v1(raw) } {
        return;
    }
    // SAFETY: `raw` covers v1.
    let Some(event) = (unsafe { read(raw) }) else {
        return;
    };
    // SAFETY: the caller's contract, and `userdata` is non-null.
    let ctx = unsafe { &*userdata.cast::<CallbackCtx>() };
    ctx.push_status(event);
}

/// # Safety
///
/// `raw` must cover v1 ([`covers_v1`]) and be valid for the call.
unsafe fn read_report(raw: *const sys::GhosttyTerminalProgramStatus) -> Option<TerminalEvent> {
    // SAFETY: the struct covers v1, whose fields are integers and raw
    // pointers, valid for any bit pattern.
    let raw = unsafe { ptr::read_unaligned(raw) };
    let Some(state) = state_from_sys(raw.state) else {
        tracing::debug!(
            state = raw.state,
            "dropping an OSC 7501 report with an unknown state"
        );
        return None;
    };
    // SAFETY: the strings are valid for the duration of the callback, and
    // are copied here.
    let report = unsafe {
        ProgramStatusReport {
            state,
            kind: kind_from_sys(raw.kind),
            progress: progress_from_sys(raw.progress),
            id: copy_string(raw.id),
            app: copy_string(raw.app),
            title: copy_string(raw.title),
            message: copy_string(raw.message),
        }
    };
    Some(TerminalEvent::Status(report))
}

/// # Safety
///
/// As for [`read_report`].
unsafe fn read_mark(raw: *const sys::GhosttyTerminalSemanticPrompt) -> Option<TerminalEvent> {
    // SAFETY: the struct covers v1. Only the two integer fields are read,
    // so its `bool` is never materialized.
    let (kind, prompt_kind) = unsafe {
        (
            ptr::read_unaligned(&raw const (*raw).kind),
            ptr::read_unaligned(&raw const (*raw).prompt_kind),
        )
    };
    mark_from_sys(kind, prompt_kind).map(TerminalEvent::Prompt)
}

/// # Safety
///
/// As for [`read_report`].
unsafe fn read_notification(
    raw: *const sys::GhosttyTerminalDesktopNotification,
) -> Option<TerminalEvent> {
    // SAFETY: the struct covers v1, whose fields are integers and raw
    // pointers, valid for any bit pattern.
    let raw = unsafe { ptr::read_unaligned(raw) };
    // SAFETY: the strings are valid for the duration of the callback, and
    // are copied here.
    let (title, body) = unsafe { (copy_string(raw.title), copy_string(raw.body)) };
    Some(TerminalEvent::Notification { title, body })
}

/// Installed as `GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS`, under the
/// `write_pty` trampoline's hygiene contract.
pub(crate) unsafe extern "C" fn program_status_trampoline(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    report: *const sys::GhosttyTerminalProgramStatus,
) {
    // SAFETY: libghostty passes this callback's userdata and struct.
    contain(|| unsafe { push_event(userdata, report, read_report) });
}

/// Installed as `GHOSTTY_TERMINAL_OPT_SEMANTIC_PROMPT`, under the
/// `write_pty` trampoline's hygiene contract.
pub(crate) unsafe extern "C" fn semantic_prompt_trampoline(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    event: *const sys::GhosttyTerminalSemanticPrompt,
) {
    // SAFETY: libghostty passes this callback's userdata and struct.
    contain(|| unsafe { push_event(userdata, event, read_mark) });
}

/// Installed as `GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION`, under the
/// `write_pty` trampoline's hygiene contract.
pub(crate) unsafe extern "C" fn desktop_notification_trampoline(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    notification: *const sys::GhosttyTerminalDesktopNotification,
) {
    // SAFETY: libghostty passes this callback's userdata and struct.
    contain(|| unsafe { push_event(userdata, notification, read_notification) });
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    fn ctx_with_sink() -> (CallbackCtx, Arc<Mutex<Vec<TerminalEvent>>>) {
        let ctx = CallbackCtx::default();
        let sink = Arc::new(Mutex::new(Vec::new()));
        *ctx.status.lock().expect("lock") = Some(Arc::clone(&sink));
        (ctx, sink)
    }

    fn string(s: &'static str) -> sys::GhosttyString {
        sys::GhosttyString {
            ptr: s.as_ptr(),
            len: s.len(),
        }
    }

    fn report(size: usize) -> sys::GhosttyTerminalProgramStatus {
        sys::GhosttyTerminalProgramStatus {
            size,
            state: sys::GhosttyProgramStatusState_GHOSTTY_PROGRAM_STATUS_STATE_BLOCKED,
            kind: sys::GhosttyProgramStatusKind_GHOSTTY_PROGRAM_STATUS_KIND_QUESTION,
            progress: 7,
            id: string("build/test"),
            app: string("cargo"),
            title: string("Tests"),
            message: string("Which target?"),
        }
    }

    const V1: usize = std::mem::size_of::<sys::GhosttyTerminalProgramStatus>();

    fn call(ctx: &CallbackCtx, report: *const sys::GhosttyTerminalProgramStatus) {
        // SAFETY: `report` points at a live struct whose `size` is no
        // larger than its allocation; `ctx` outlives the call.
        unsafe {
            program_status_trampoline(
                ptr::null_mut(),
                ptr::from_ref(ctx).cast_mut().cast(),
                report,
            );
        }
    }

    fn take(sink: &Arc<Mutex<Vec<TerminalEvent>>>) -> Vec<TerminalEvent> {
        std::mem::take(&mut *sink.lock().expect("lock"))
    }

    /// What `report` reads as.
    fn reported() -> TerminalEvent {
        TerminalEvent::Status(ProgramStatusReport {
            state: ProgramStatusState::Blocked,
            kind: Some(ProgramStatusKind::Question),
            progress: Some(7),
            id: "build/test".into(),
            app: "cargo".into(),
            title: "Tests".into(),
            message: "Which target?".into(),
        })
    }

    #[test]
    fn a_report_shorter_than_v1_is_dropped() {
        let (ctx, sink) = ctx_with_sink();

        call(&ctx, &report(V1 - 1));
        assert!(take(&sink).is_empty(), "a short struct must be dropped");

        call(&ctx, &report(V1));
        assert_eq!(take(&sink), vec![reported()]);
    }

    #[test]
    fn a_report_longer_than_v1_reads_the_known_fields() {
        #[repr(C)]
        struct Newer {
            v1: sys::GhosttyTerminalProgramStatus,
            later: [u64; 4],
        }
        let (ctx, sink) = ctx_with_sink();
        let newer = Newer {
            v1: report(std::mem::size_of::<Newer>()),
            later: [u64::MAX; 4],
        };

        call(&ctx, &raw const newer.v1);
        assert_eq!(take(&sink), vec![reported()]);
    }

    #[test]
    fn an_unknown_state_is_dropped_and_an_unknown_kind_is_none() {
        let (ctx, sink) = ctx_with_sink();

        let mut unknown_state = report(V1);
        unknown_state.state = 99;
        call(&ctx, &unknown_state);
        assert!(take(&sink).is_empty(), "an unknown state must be dropped");

        let mut unknown_kind = report(V1);
        unknown_kind.kind = 99;
        call(&ctx, &unknown_kind);
        let events = take(&sink);
        assert!(
            matches!(events.as_slice(), [TerminalEvent::Status(r)] if r.kind.is_none()),
            "{events:?}"
        );
    }

    #[test]
    fn progress_outside_zero_to_a_hundred_is_none() {
        let (ctx, sink) = ctx_with_sink();
        for (raw, expected) in [
            (-1, None),
            (-5, None),
            (101, None),
            (0, Some(0)),
            (100, Some(100)),
        ] {
            let mut r = report(V1);
            r.progress = raw;
            call(&ctx, &r);
            let events = take(&sink);
            assert!(
                matches!(events.as_slice(), [TerminalEvent::Status(r)] if r.progress == expected),
                "progress {raw}: {events:?}"
            );
        }
    }

    #[test]
    fn a_semantic_prompt_shorter_than_v1_is_dropped() {
        let (ctx, sink) = ctx_with_sink();
        let v1 = std::mem::size_of::<sys::GhosttyTerminalSemanticPrompt>();
        let event = |size| sys::GhosttyTerminalSemanticPrompt {
            size,
            kind: sys::GhosttySemanticPromptKind_GHOSTTY_SEMANTIC_PROMPT_COMMAND_END,
            ..Default::default()
        };
        let send = |e: &sys::GhosttyTerminalSemanticPrompt| {
            // SAFETY: as in `call`.
            unsafe {
                semantic_prompt_trampoline(
                    ptr::null_mut(),
                    ptr::from_ref(&ctx).cast_mut().cast(),
                    e,
                );
            }
        };

        send(&event(v1 - 1));
        assert!(take(&sink).is_empty(), "a short struct must be dropped");
        send(&event(v1));
        assert_eq!(
            take(&sink),
            vec![TerminalEvent::Prompt(PromptMark::CommandEnd)]
        );
    }

    #[test]
    fn a_notification_shorter_than_v1_or_null_is_dropped() {
        let (ctx, sink) = ctx_with_sink();
        let v1 = std::mem::size_of::<sys::GhosttyTerminalDesktopNotification>();
        let notification = |size| sys::GhosttyTerminalDesktopNotification {
            size,
            title: string("Build"),
            body: string("done"),
        };
        let send = |n: *const sys::GhosttyTerminalDesktopNotification| {
            // SAFETY: as in `call`; a null struct is checked first.
            unsafe {
                desktop_notification_trampoline(
                    ptr::null_mut(),
                    ptr::from_ref(&ctx).cast_mut().cast(),
                    n,
                );
            }
        };

        send(&notification(v1 - 1));
        send(ptr::null());
        assert!(take(&sink).is_empty(), "a short or null struct is dropped");
        send(&notification(v1));
        assert_eq!(
            take(&sink),
            vec![TerminalEvent::Notification {
                title: "Build".into(),
                body: "done".into(),
            }]
        );
    }

    /// A subscriber whose `event` panics, standing in for any subscriber
    /// code that can.
    struct PanickingSubscriber(Arc<std::sync::atomic::AtomicUsize>);

    impl tracing::Subscriber for PanickingSubscriber {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("the subscriber panics");
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Without containment the panic reaches the `extern "C"` frame and
    /// aborts the test process.
    ///
    /// The body runs in a child process of its own. `tracing` caches each
    /// callsite's interest process-wide, and a parallel test that first
    /// reaches the same debug callsite with no subscriber can cache it as
    /// "never" while this one installs its own, which then sees no event.
    #[test]
    fn a_panic_in_a_debug_log_drops_the_event_without_aborting() {
        const ALONE: &str = "ROOST_VT_PANICKING_SUBSCRIBER_ALONE";
        if std::env::var_os(ALONE).is_none() {
            let name = format!(
                "{}::a_panic_in_a_debug_log_drops_the_event_without_aborting",
                module_path!().split_once("::").expect("crate::module").1
            );
            let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([name.as_str(), "--exact", "--test-threads=1"])
                .env(ALONE, "1")
                .output()
                .expect("run the test alone");
            let stdout = String::from_utf8_lossy(&child.stdout);
            assert!(
                child.status.success() && stdout.contains("1 passed"),
                "{}\n{stdout}\n{}",
                child.status,
                String::from_utf8_lossy(&child.stderr)
            );
            return;
        }
        let (ctx, sink) = ctx_with_sink();
        let events_logged = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let subscriber = PanickingSubscriber(Arc::clone(&events_logged));

        tracing::subscriber::with_default(subscriber, || {
            let mut unknown_state = report(V1);
            unknown_state.state = 99;
            call(&ctx, &unknown_state);
            call(&ctx, &report(V1 - 1));
            call(&ctx, &report(V1));
        });

        assert_eq!(
            events_logged.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "both drop paths logged"
        );
        assert_eq!(take(&sink), vec![reported()], "only the valid report");
    }

    #[test]
    fn null_userdata_or_struct_is_ignored() {
        let (ctx, sink) = ctx_with_sink();
        // SAFETY: both pointers are null, which the trampoline checks first.
        unsafe {
            program_status_trampoline(ptr::null_mut(), ptr::null_mut(), &report(V1));
            program_status_trampoline(
                ptr::null_mut(),
                ptr::from_ref(&ctx).cast_mut().cast(),
                ptr::null(),
            );
        }
        assert!(take(&sink).is_empty());
    }
}
