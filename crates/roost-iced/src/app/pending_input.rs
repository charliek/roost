//! Keys typed while a new tab opens (plan 072 §D2).
//!
//! A new-tab gesture whose dispatch produced an op arms a buffer bound to
//! that one creation, and the keyboard route hands it every key until the
//! new tab can take them. They are kept as events rather than bytes: the
//! new tab encodes them with its own encoder and modes, and the old tab's
//! (kitty, DECCKM) need not match.

use std::time::Instant;

use iced::keyboard::{self, key::Physical};
use roost_ui_model::keys::{HostId, TabKey};

use super::host_tab::HostAttach;
use super::servicing::ATTACH_RETRY_WINDOW;
use super::terminal_tab::TerminalTab;
use super::{type_into, EngineDispatch, PENDING_HOST_SELECTION_DEADLINE};
use crate::input;

/// The most entries one buffer keeps.
const CAP: usize = 1024;

/// What a paste answers while a new tab is opening. Its bracketing is
/// decided at delivery from mode 2004, which a fresh shell has not set
/// yet, so it is refused rather than kept.
pub(super) const NOT_READY: &str = "the new tab isn't ready yet";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PendingInput {
    Key(keyboard::Event),
    /// An input-method commit.
    Text(String),
}

#[derive(Debug)]
struct Pending {
    /// The dispatch's op id — the only thing a completion is matched by.
    op: u64,
    /// The incarnation the creation went to; the local backend's for an
    /// in-process one.
    host: HostId,
    gesture: u64,
    /// 071-D13's focus generation as the dispatch found it.
    focus_generation: u64,
    deadline: Instant,
    target: Option<TabKey>,
    entries: Vec<PendingInput>,
    /// Keys pressed since the arm. A release is kept only when its press
    /// was: the arming chord's own key-up and modifier releases would
    /// reach the new tab unmatched.
    pressed: Vec<Physical>,
    full: bool,
}

impl Pending {
    /// Presses and commits kept — what a status line counts.
    fn keys(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| match entry {
                PendingInput::Key(event) => input::non_modifier_press(event),
                PendingInput::Text(_) => true,
            })
            .count()
    }

    fn dropped(self, reason: Reason) -> Option<Dropped> {
        let keys = self.keys();
        (keys > 0).then_some(Dropped {
            keys,
            gesture: self.gesture,
            reason,
        })
    }
}

/// The keyboard's pending target: at most one buffer, for the latest
/// new-tab gesture.
#[derive(Debug, Default)]
pub(super) struct PendingKeyboard {
    /// Bumped at every arm, so each buffer is its gesture's own.
    gestures: u64,
    pending: Option<Pending>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Push {
    Kept,
    Skipped,
    /// The first entry past [`CAP`]; this and everything after it is not
    /// kept.
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Reason {
    Failed(String),
    Expired,
    Disconnected,
    Focus,
    Replaced,
}

/// A buffer dropped with keys in it — what its status line says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Dropped {
    keys: usize,
    gesture: u64,
    reason: Reason,
}

impl Dropped {
    fn sentence(&self) -> String {
        let lost = match self.keys {
            1 => "1 key was not sent".to_string(),
            keys => format!("{keys} keys were not sent"),
        };
        match &self.reason {
            Reason::Failed(error) => format!("the new tab didn't open ({error}); {lost}"),
            Reason::Expired => format!("the new tab wasn't ready in time; {lost}"),
            Reason::Disconnected => {
                format!("the connection dropped before the new tab was ready; {lost}")
            }
            Reason::Focus => format!("focus moved before the new tab was ready; {lost}"),
            Reason::Replaced => format!("another new tab was opened first; {lost}"),
        }
    }
}

/// What one settle reads off the window.
pub(super) struct Facts {
    pub(super) now: Instant,
    pub(super) focus_generation: u64,
    /// The creation's incarnation is still this window's connection.
    pub(super) connected: bool,
    pub(super) active: TabKey,
    /// The target can take keys: an attached terminal in-process, an
    /// attach live on its hydrated terminal on a host.
    pub(super) ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Step {
    Wait,
    Flush(TabKey),
    Drop(Reason),
}

impl PendingKeyboard {
    pub(super) fn armed(&self) -> bool {
        self.pending.is_some()
    }

    fn host(&self) -> Option<HostId> {
        self.pending.as_ref().map(|pending| pending.host)
    }

    pub(super) fn target(&self) -> Option<TabKey> {
        self.pending.as_ref().and_then(|pending| pending.target)
    }

    /// A new-tab gesture dispatched. `created` is the op and incarnation
    /// its dispatch produced, and `None` — a synchronous refusal — arms
    /// nothing and leaves any buffer as it was. Returns the buffer it
    /// replaced, when that one held keys.
    pub(super) fn arm(
        &mut self,
        created: Option<(u64, HostId)>,
        focus_generation: u64,
        now: Instant,
    ) -> Option<Dropped> {
        let (op, host) = created?;
        self.gestures = self.gestures.wrapping_add(1);
        let replaced = self.pending.replace(Pending {
            op,
            host,
            gesture: self.gestures,
            focus_generation,
            deadline: now + PENDING_HOST_SELECTION_DEADLINE,
            target: None,
            entries: Vec::new(),
            pressed: Vec::new(),
            full: false,
        });
        replaced.and_then(|pending| pending.dropped(Reason::Replaced))
    }

    pub(super) fn push(&mut self, input: PendingInput) -> Push {
        let Some(pending) = self.pending.as_mut() else {
            return Push::Skipped;
        };
        match &input {
            PendingInput::Key(keyboard::Event::ModifiersChanged(_)) => return Push::Skipped,
            PendingInput::Key(keyboard::Event::KeyReleased { physical_key, .. }) => {
                let Some(at) = pending.pressed.iter().position(|key| key == physical_key) else {
                    return Push::Skipped;
                };
                pending.pressed.swap_remove(at);
            }
            PendingInput::Key(keyboard::Event::KeyPressed { physical_key, .. }) => {
                if !pending.pressed.contains(physical_key) {
                    pending.pressed.push(*physical_key);
                }
            }
            PendingInput::Text(_) => {}
        }
        if pending.entries.len() >= CAP {
            return if std::mem::replace(&mut pending.full, true) {
                Push::Skipped
            } else {
                Push::Full
            };
        }
        pending.entries.push(input);
        Push::Kept
    }

    /// A creation was answered. Only the buffer's own op matches; any
    /// other completion neither names its tab nor cancels it. An
    /// in-process tab is attached within the attach budget of its reply,
    /// so that becomes the deadline.
    pub(super) fn answered(
        &mut self,
        op: u64,
        outcome: Result<TabKey, String>,
        now: Instant,
    ) -> Option<Dropped> {
        let pending = self.pending.as_mut().filter(|pending| pending.op == op)?;
        match outcome {
            Ok(tab) => {
                pending.target = Some(tab);
                if tab.is_local() {
                    pending.deadline = now + ATTACH_RETRY_WINDOW;
                }
                None
            }
            Err(error) => self.drop_for(Reason::Failed(error)),
        }
    }

    /// Where the buffer stands. Leaving is the user's: an in-process tab
    /// is left once the window shows another one, and a host tab once
    /// 071-D13's generation moved — which in-process can't use, because
    /// the new tab's own `ActiveChanged` bumps it.
    pub(super) fn step(&self, facts: &Facts) -> Option<Step> {
        let pending = self.pending.as_ref()?;
        let left = if pending.host.is_local() {
            pending
                .target
                .is_some_and(|target| target != facts.active)
                .then_some(Reason::Focus)
        } else if facts.focus_generation != pending.focus_generation {
            Some(Reason::Focus)
        } else if !facts.connected {
            Some(Reason::Disconnected)
        } else {
            None
        };
        Some(match (left, pending.target) {
            (Some(reason), _) => Step::Drop(reason),
            (None, Some(target)) if facts.ready => Step::Flush(target),
            _ if facts.now >= pending.deadline => Step::Drop(Reason::Expired),
            _ => Step::Wait,
        })
    }

    /// A focus the user asked for, and `landed` its refusal check. Only
    /// one that landed leaves an in-process creation that has not named
    /// its tab yet; a refused one — a stale palette row, a notification
    /// for a tab that is gone — moved nothing, so it keeps the buffer and
    /// says nothing of it. Once the creation has named its tab,
    /// [`Self::step`]'s active-tab check is the rule.
    fn user_focus<T>(&mut self, landed: Result<T, String>) -> Result<(T, Option<Dropped>), String> {
        let landed = landed?;
        let unnamed = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.host.is_local() && pending.target.is_none());
        let dropped = if unnamed {
            self.drop_for(Reason::Focus)
        } else {
            None
        };
        Ok((landed, dropped))
    }

    fn drop_for(&mut self, reason: Reason) -> Option<Dropped> {
        self.pending
            .take()
            .and_then(|pending| pending.dropped(reason))
    }

    /// The buffer, for delivery.
    pub(super) fn take(&mut self) -> Option<(u64, Vec<PendingInput>)> {
        self.pending
            .take()
            .map(|pending| (pending.gesture, pending.entries))
    }
}

/// Type what a buffer kept into the tab it was for, in order, with that
/// tab's own encoder.
pub(super) fn deliver(tab: &mut TerminalTab, tab_id: i64, entries: Vec<PendingInput>) {
    for entry in entries {
        match entry {
            PendingInput::Key(event) => {
                type_into(tab, tab_id, event, keyboard::Modifiers::empty(), false)
            }
            PendingInput::Text(text) => {
                if let Err(error) = tab.commit_ime(&text) {
                    tracing::warn!(?error, tab_id, "typed-ahead IME commit failed");
                }
            }
        }
    }
}

impl super::App {
    /// A new-tab gesture ran its dispatch (§D2's "Arm"). Called by the
    /// gestures alone: forwarded opens and `roostctl` never arm.
    pub(super) fn arm_pending_keyboard(&mut self, dispatch: &EngineDispatch) {
        let created = dispatch.op.zip(dispatch.created_on);
        let replaced = self
            .pending_keyboard
            .arm(created, self.focus_generation, Instant::now());
        self.report_dropped_keys(replaced);
    }

    pub(super) fn buffer_pending_input(&mut self, input: PendingInput) {
        if self.pending_keyboard.push(input) == Push::Full {
            self.set_status(format!(
                "{NOT_READY}; keys past the first {CAP} are not kept"
            ));
        }
    }

    /// A creation's completion: the buffer learns its tab, or drops.
    pub(super) fn pending_keyboard_answered(
        &mut self,
        answer: Option<(u64, Result<TabKey, String>)>,
    ) {
        let Some((op, outcome)) = answer else {
            return;
        };
        let dropped = self.pending_keyboard.answered(op, outcome, Instant::now());
        self.report_dropped_keys(dropped);
        self.settle_pending_keyboard();
    }

    /// Deliver, drop, or keep waiting — [`PendingKeyboard::step`].
    pub(super) fn settle_pending_keyboard(&mut self) {
        let Some(host) = self.pending_keyboard.host() else {
            return;
        };
        let facts = Facts {
            now: Instant::now(),
            focus_generation: self.focus_generation,
            connected: host.is_local() || self.hosts.owns(host),
            active: self.active_tab_key(),
            ready: self
                .pending_keyboard
                .target()
                .is_some_and(|tab| self.takes_typed_ahead(tab)),
        };
        match self.pending_keyboard.step(&facts) {
            None | Some(Step::Wait) => {}
            Some(Step::Flush(target)) => {
                let Some((gesture, entries)) = self.pending_keyboard.take() else {
                    return;
                };
                if let Some(tab) = self.tabs.get_mut(&target) {
                    tracing::debug!(%target, gesture, entries = entries.len(), "typed-ahead delivered");
                    deliver(tab, target.tab, entries);
                }
            }
            Some(Step::Drop(reason)) => {
                let dropped = self.pending_keyboard.drop_for(reason);
                self.report_dropped_keys(dropped);
            }
        }
    }

    /// An in-process tab once its terminal is attached; a host tab once
    /// [`HostAttach::takes_typed_ahead`].
    fn takes_typed_ahead(&self, tab: TabKey) -> bool {
        self.tabs.contains_key(&tab)
            && (tab.is_local()
                || self
                    .host_attach
                    .get(&tab)
                    .is_some_and(HostAttach::takes_typed_ahead))
    }

    /// [`PendingKeyboard::user_focus`], passing `landed` through.
    pub(super) fn pending_keyboard_user_focus<T>(
        &mut self,
        landed: Result<T, String>,
    ) -> Result<T, String> {
        let (landed, dropped) = self.pending_keyboard.user_focus(landed)?;
        self.report_dropped_keys(dropped);
        Ok(landed)
    }

    /// `incarnation`'s connection is gone.
    pub(super) fn pending_keyboard_host_gone(&mut self, incarnation: HostId) {
        if self.pending_keyboard.host() == Some(incarnation) {
            let dropped = self.pending_keyboard.drop_for(Reason::Disconnected);
            self.report_dropped_keys(dropped);
        }
    }

    fn report_dropped_keys(&mut self, dropped: Option<Dropped>) {
        let Some(dropped) = dropped else {
            return;
        };
        let sentence = dropped.sentence();
        tracing::info!(gesture = dropped.gesture, "{sentence}");
        self.set_status(sentence);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use iced::keyboard::key::{Code, Named};
    use iced::keyboard::{Key, Location, Modifiers};

    use roost_engine::Workspace;

    use super::*;
    use crate::app::focus_tab_in_core;
    use crate::app::terminal_tab::{attach_test_terminal, feed_text_until};
    use crate::engine_feed;

    const HOST: HostId = HostId::new(3);

    fn press(key: &str) -> PendingInput {
        PendingInput::Key(input::synthetic_press(key, None, &[]).expect("a key"))
    }

    fn release(key: Key, code: Code) -> PendingInput {
        PendingInput::Key(keyboard::Event::KeyReleased {
            modified_key: key.clone(),
            key,
            physical_key: Physical::Code(code),
            location: Location::Standard,
            modifiers: Modifiers::empty(),
        })
    }

    fn typed(keyboard: &mut PendingKeyboard, text: &str) {
        for value in text.chars() {
            assert_eq!(keyboard.push(press(&value.to_string())), Push::Kept);
        }
    }

    fn facts(now: Instant, active: TabKey) -> Facts {
        Facts {
            now,
            focus_generation: 4,
            connected: true,
            active,
            ready: false,
        }
    }

    fn armed(op: u64, host: HostId, now: Instant) -> PendingKeyboard {
        let mut keyboard = PendingKeyboard::default();
        assert_eq!(keyboard.arm(Some((op, host)), 4, now), None);
        keyboard
    }

    #[test]
    fn a_second_gesture_replaces_the_first_buffer_and_says_what_it_dropped() {
        let now = Instant::now();
        let mut keyboard = armed(10, HOST, now);
        let first = keyboard.pending.as_ref().map(|pending| pending.gesture);
        typed(&mut keyboard, "ls");

        let replaced = keyboard
            .arm(Some((11, HOST)), 4, now)
            .expect("it held keys");
        assert_eq!(
            replaced.sentence(),
            "another new tab was opened first; 2 keys were not sent"
        );
        let second = keyboard.pending.as_ref().map(|pending| pending.gesture);
        assert_ne!(first, second, "each arm is its own gesture");

        assert_eq!(keyboard.answered(10, Ok(TabKey::new(HOST, 5)), now), None);
        assert_eq!(
            keyboard.target(),
            None,
            "the first creation's answer names nothing"
        );
        keyboard.answered(11, Ok(TabKey::new(HOST, 6)), now);
        assert_eq!(keyboard.target(), Some(TabKey::new(HOST, 6)));

        let mut quiet = armed(12, HOST, now);
        assert_eq!(
            quiet.arm(Some((13, HOST)), 4, now),
            None,
            "an empty buffer goes without a word"
        );
    }

    #[test]
    fn an_unrelated_completion_neither_resolves_nor_cancels_the_buffer() {
        let now = Instant::now();
        let mut keyboard = armed(20, HOST, now);
        typed(&mut keyboard, "x");
        assert_eq!(
            keyboard.answered(21, Err("tab.open: refused".into()), now),
            None
        );
        assert_eq!(keyboard.answered(21, Ok(TabKey::new(HOST, 9)), now), None);
        assert!(keyboard.armed());
        assert_eq!(keyboard.target(), None);

        let failed = keyboard
            .answered(20, Err("tab.open: refused".into()), now)
            .expect("its own failure drops it");
        assert_eq!(
            failed.sentence(),
            "the new tab didn't open (tab.open: refused); 1 key was not sent"
        );
        assert!(!keyboard.armed());
    }

    #[test]
    fn a_synchronous_refusal_arms_nothing_and_keeps_a_buffer_it_found() {
        let now = Instant::now();
        let mut keyboard = PendingKeyboard::default();
        assert_eq!(keyboard.arm(None, 4, now), None);
        assert!(!keyboard.armed());

        let mut keyboard = armed(30, HOST, now);
        typed(&mut keyboard, "ab");
        assert_eq!(keyboard.arm(None, 4, now), None);
        assert_eq!(
            keyboard
                .pending
                .as_ref()
                .map(|pending| (pending.op, pending.keys())),
            Some((30, 2))
        );
    }

    #[test]
    fn the_deadline_starts_at_dispatch_and_in_process_at_the_reply() {
        let armed_at = Instant::now();
        let target = TabKey::new(HOST, 7);
        let mut host = armed(40, HOST, armed_at);
        typed(&mut host, "q");
        host.answered(40, Ok(target), armed_at);
        let late = armed_at + PENDING_HOST_SELECTION_DEADLINE;
        assert_eq!(
            host.step(&facts(late - Duration::from_millis(1), target)),
            Some(Step::Wait)
        );
        assert_eq!(
            host.step(&facts(late, target)),
            Some(Step::Drop(Reason::Expired))
        );

        let local = TabKey::local(7);
        let mut in_process = armed(41, HostId::LOCAL, armed_at);
        let replied = armed_at + Duration::from_secs(3);
        in_process.answered(41, Ok(local), replied);
        let budget = replied + ATTACH_RETRY_WINDOW;
        assert_eq!(
            in_process.step(&facts(budget - Duration::from_millis(1), local)),
            Some(Step::Wait)
        );
        assert_eq!(
            in_process.step(&facts(budget, local)),
            Some(Step::Drop(Reason::Expired))
        );
        assert_eq!(
            Dropped {
                keys: 3,
                gesture: 1,
                reason: Reason::Expired
            }
            .sentence(),
            "the new tab wasn't ready in time; 3 keys were not sent"
        );
    }

    #[test]
    fn past_the_cap_nothing_is_kept_and_the_overflow_is_said_once() {
        let mut keyboard = armed(50, HOST, Instant::now());
        for _ in 0..CAP {
            assert_eq!(keyboard.push(PendingInput::Text("a".into())), Push::Kept);
        }
        assert_eq!(keyboard.push(PendingInput::Text("b".into())), Push::Full);
        assert_eq!(keyboard.push(PendingInput::Text("c".into())), Push::Skipped);
        let pending = keyboard.pending.as_ref().expect("armed");
        assert_eq!(pending.entries.len(), CAP);
        assert_eq!(pending.keys(), CAP);
    }

    #[test]
    fn only_the_releases_of_keys_typed_after_the_arm_are_kept() {
        let mut keyboard = armed(60, HOST, Instant::now());
        assert_eq!(
            keyboard.push(release(Key::Character("t".into()), Code::KeyT)),
            Push::Skipped,
            "Alt+T's own key-up"
        );
        assert_eq!(
            keyboard.push(release(Key::Named(Named::Alt), Code::AltLeft)),
            Push::Skipped,
            "and the modifier's"
        );
        assert_eq!(
            keyboard.push(PendingInput::Key(keyboard::Event::ModifiersChanged(
                Modifiers::empty()
            ))),
            Push::Skipped
        );
        assert_eq!(keyboard.push(press("e")), Push::Kept);
        assert_eq!(
            keyboard.push(release(Key::Character("e".into()), Code::KeyE)),
            Push::Kept
        );
        assert_eq!(
            keyboard.push(release(Key::Character("e".into()), Code::KeyE)),
            Push::Skipped,
            "one release per press"
        );
        let pending = keyboard.pending.as_ref().expect("armed");
        assert_eq!(pending.entries.len(), 2);
        assert_eq!(pending.keys(), 1, "a release is not a key typed");
    }

    #[test]
    fn a_host_buffer_waits_for_its_tab_and_leaves_with_the_user_or_the_connection() {
        let now = Instant::now();
        let old = TabKey::new(HOST, 1);
        let new = TabKey::new(HOST, 2);
        let mut keyboard = armed(70, HOST, now);
        assert_eq!(keyboard.step(&facts(now, old)), Some(Step::Wait));
        keyboard.answered(70, Ok(new), now);
        assert_eq!(
            keyboard.step(&facts(now, old)),
            Some(Step::Wait),
            "the old tab is still on screen until the listing selects the new one"
        );
        let ready = Facts {
            ready: true,
            ..facts(now, new)
        };
        assert_eq!(keyboard.step(&ready), Some(Step::Flush(new)));
        let clicked = Facts {
            focus_generation: 5,
            ..facts(now, old)
        };
        assert_eq!(keyboard.step(&clicked), Some(Step::Drop(Reason::Focus)));
        let gone = Facts {
            connected: false,
            ..facts(now, new)
        };
        assert_eq!(keyboard.step(&gone), Some(Step::Drop(Reason::Disconnected)));
        assert_eq!(
            keyboard.user_focus(Ok(())),
            Ok(((), None)),
            "a host buffer leaves by the generation"
        );
        assert!(keyboard.armed());
    }

    #[test]
    fn an_in_process_buffer_leaves_when_the_window_shows_another_tab() {
        let now = Instant::now();
        let old = TabKey::local(1);
        let new = TabKey::local(2);
        let mut keyboard = armed(80, HostId::LOCAL, now);
        typed(&mut keyboard, "pwd");
        let bumped = Facts {
            focus_generation: 9,
            ..facts(now, new)
        };
        assert_eq!(
            keyboard.step(&bumped),
            Some(Step::Wait),
            "its own ActiveChanged moves the generation"
        );
        keyboard.answered(80, Ok(new), now);
        assert_eq!(keyboard.step(&facts(now, new)), Some(Step::Wait));
        assert_eq!(
            keyboard.step(&Facts {
                ready: true,
                ..facts(now, new)
            }),
            Some(Step::Flush(new))
        );
        assert_eq!(
            keyboard.step(&facts(now, old)),
            Some(Step::Drop(Reason::Focus))
        );

        let mut unnamed = armed(81, HostId::LOCAL, now);
        typed(&mut unnamed, "l");
        assert_eq!(
            unnamed
                .user_focus(Ok(()))
                .map(|((), dropped)| dropped.map(|dropped| dropped.sentence())),
            Ok(Some(
                "focus moved before the new tab was ready; 1 key was not sent".into()
            ))
        );
        assert!(!unnamed.armed());
    }

    /// A focus the workspace refuses — the tab a stale palette row or
    /// notification names is gone — kept the keys; one that lands drops
    /// them and says so.
    #[test]
    fn only_a_focus_that_lands_leaves_an_unnamed_in_process_buffer() {
        let workspace = Workspace::new();
        let project = workspace.create_project("p", "/tmp").expect("project");
        let old = workspace
            .open_tab(project.id, "/tmp", "old", true)
            .expect("tab");
        let mut keyboard = armed(100, HostId::LOCAL, Instant::now());
        typed(&mut keyboard, "ls");

        let gone = TabKey::local(old.id + 1);
        assert!(keyboard
            .user_focus(focus_tab_in_core(&workspace, gone))
            .is_err());
        assert_eq!(
            keyboard.pending.as_ref().map(Pending::keys),
            Some(2),
            "a refused focus moved nothing"
        );

        let ((), dropped) = keyboard
            .user_focus(focus_tab_in_core(&workspace, TabKey::local(old.id)))
            .expect("the tab is there");
        assert_eq!(
            dropped.map(|dropped| dropped.sentence()),
            Some("focus moved before the new tab was ready; 2 keys were not sent".into())
        );
        assert!(!keyboard.armed());
    }

    /// The whole in-process route against a real supervisor: keys kept
    /// before the new tab attaches reach its shell once it does, typed by
    /// the new tab's encoder.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kept_keys_reach_the_new_tab_once_it_attaches() {
        let (feed_tx, mut feed_rx) = engine_feed::channel();
        let target = TabKey::local(93);
        let now = Instant::now();
        let mut keyboard = armed(90, HostId::LOCAL, now);
        for key in ["k", "e", "p", "t"] {
            assert_eq!(keyboard.push(press(key)), Push::Kept);
        }
        keyboard.push(PendingInput::Text("-ok".into()));
        keyboard.answered(90, Ok(target), now);
        assert_eq!(keyboard.step(&facts(now, target)), Some(Step::Wait));

        let (mut tab, supervisor) = attach_test_terminal(93, feed_tx);
        let ready = Facts {
            ready: true,
            ..facts(now, target)
        };
        assert_eq!(keyboard.step(&ready), Some(Step::Flush(target)));
        let (_, entries) = keyboard.take().expect("armed");
        deliver(&mut tab, target.tab, entries);
        tab.session.send_input(b"\n".to_vec());

        let seen = feed_text_until(&mut feed_rx, target, "kept-ok", Duration::from_secs(5)).await;
        assert!(
            seen.contains("kept-ok"),
            "the shell never saw the kept keys: {seen:?}"
        );
        drop(tab);
        supervisor.close(93);
    }
}
