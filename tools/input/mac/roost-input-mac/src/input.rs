//! What a posting command sends, one event at a time, with its target checked
//! again immediately before every event. The frontmost app, the window a click
//! reaches, the console session or Secure Input can change between two events
//! of one gesture (a dialog, a notification, someone else's click), and an
//! event that lands in the wrong app can be anything from a stray letter to
//! Cmd-Q. Everything a sequence presses is tracked until its release — in
//! memory, and in a journal a wrapper can read after this process is gone — so
//! an aborted sequence, the deadline, a panic, a signal or a SIGKILL releases
//! it, and only it.
//!
//! The platform half is the [`Desktop`] trait; this module makes no system
//! calls, so the sequencing is tested on any OS.

use crate::args::{Button, MouseAction, Point};
use crate::keys::{flags_for, modifier, Modifier};
use crate::Failure;
use serde_json::{json, Value};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::Duration;

/// Time for the target app to take one event before the next; posting faster
/// coalesces or reorders presses in the receiving app.
pub const KEY_GAP: Duration = Duration::from_millis(30);
pub const MOUSE_GAP: Duration = Duration::from_millis(40);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    /// A flagsChanged with the modifier's side keycode.
    Modifier {
        modifier: Modifier,
        down: bool,
    },
    Key {
        code: u16,
        down: bool,
    },
    Move(Point),
    Button {
        button: Button,
        down: bool,
        at: Point,
    },
    Drag {
        button: Button,
        at: Point,
    },
}

impl Event {
    fn presses(self) -> bool {
        matches!(
            self,
            Event::Modifier { down: true, .. }
                | Event::Key { down: true, .. }
                | Event::Button { down: true, .. }
        )
    }
}

/// What must still hold immediately before an event is posted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Check {
    /// Keys go to the app whose window is key: the target must be the
    /// frontmost app, and no other app may hold a key window (a non-activating
    /// panel can take the keyboard while its owner stays in the background).
    Frontmost(i32),
    /// A move or a press goes to whatever a click at the point reaches: it
    /// must be the target.
    Topmost(i32, Point),
    /// A drag's later events and its release go to the window that took the
    /// press: the target must still be frontmost and still be what a click
    /// at the press point reaches.
    Grab(i32, Point),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    pub event: Event,
    pub check: Check,
    /// The pause after this event, before the next one is checked.
    pub gap: Duration,
}

/// The console session as a posting command must find it before every event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Session {
    pub locked: bool,
    pub on_console: bool,
    /// Whoever `ioreg` names while Secure Input is on (the frontmost app,
    /// not necessarily the holder).
    pub secure_input_pid: Option<i64>,
}

/// What a command allows beyond the defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Policy {
    /// Post while Secure Input is on: only for a caller that has confirmed
    /// through Roost itself that Roost is the holder.
    pub allow_secure_input: bool,
}

/// Where what is held is recorded, for a recovery after this process is gone.
pub trait Journal {
    fn journal(&self, held: &Value) -> Result<(), Failure>;
    /// Remove the record: none rather than a stale one.
    fn discard_journal(&self) -> Result<(), Failure>;
}

/// The desktop, which journals what this process itself holds.
pub trait Desktop: Journal {
    fn session(&self) -> Result<Session, Failure>;
    fn frontmost(&self) -> Result<Option<i32>, Failure>;
    /// The pids of apps other than `pid` whose focused window says it is key.
    fn foreign_key_windows(&self, pid: i32) -> Result<Vec<i32>, Failure>;
    /// The pid of the app a click at `at` reaches; nothing slower.
    fn click_target(&self, at: Point) -> Result<i32, Failure>;
    /// What a click at `at` would hit, for a refusal's message only.
    fn describe(&self, at: Point) -> Value;
    fn post(&self, event: Event, flags: u64) -> Result<(), Failure>;
    fn pause(&self, gap: Duration);
}

/// Everything this process has pressed and not yet released.
#[derive(Debug, Clone, Default)]
pub struct Held {
    modifiers: Vec<Modifier>,
    keys: Vec<u16>,
    buttons: Vec<(Button, Point)>,
    /// Set by the deadline or a signal: nothing more may be pressed.
    expired: bool,
}

pub static HELD: Mutex<Held> = Mutex::new(Held::new());

pub fn held() -> MutexGuard<'static, Held> {
    HELD.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl Held {
    pub const fn new() -> Held {
        Held {
            modifiers: Vec::new(),
            keys: Vec::new(),
            buttons: Vec::new(),
            expired: false,
        }
    }

    pub fn expire(&mut self) {
        self.expired = true;
    }

    pub fn is_empty(&self) -> bool {
        self.modifiers.is_empty() && self.keys.is_empty() && self.buttons.is_empty()
    }

    /// Hold again what `release` — a release that failed to post — would
    /// have let go.
    fn keep(&mut self, release: Event) {
        match release {
            Event::Modifier { modifier, .. } => self.apply(Event::Modifier {
                modifier,
                down: true,
            }),
            Event::Key { code, .. } => self.apply(Event::Key { code, down: true }),
            Event::Button { button, at, .. } => self.apply(Event::Button {
                button,
                down: true,
                at,
            }),
            Event::Drag { .. } | Event::Move(_) => {}
        }
    }

    /// The flags `event` carries: the modifiers held once it has landed.
    fn flags(&self, event: Event) -> u64 {
        match event {
            Event::Modifier { modifier, down } => {
                let mut after: Vec<Modifier> = self
                    .modifiers
                    .iter()
                    .copied()
                    .filter(|held| *held != modifier)
                    .collect();
                if down {
                    after.push(modifier);
                }
                flags_for(&after)
            }
            _ => flags_for(&self.modifiers),
        }
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Modifier { modifier, down } => {
                self.modifiers.retain(|held| *held != modifier);
                if down {
                    self.modifiers.push(modifier);
                }
            }
            Event::Key { code, down } => {
                self.keys.retain(|held| *held != code);
                if down {
                    self.keys.push(code);
                }
            }
            Event::Button { button, down, at } => {
                self.buttons.retain(|(held, _)| *held != button);
                if down {
                    self.buttons.push((button, at));
                }
            }
            Event::Drag { button, at } => {
                for (held, point) in &mut self.buttons {
                    if *held == button {
                        *point = at;
                    }
                }
            }
            Event::Move(_) => {}
        }
    }

    /// The state as `<outdir>/held.json` records it.
    pub fn to_json(&self) -> Value {
        json!({
            "modifiers": self.modifiers.iter().map(|modifier| modifier.name).collect::<Vec<_>>(),
            "keys": self.keys,
            "buttons": self.buttons.iter().map(|(button, at)| {
                json!({"button": button_name(*button), "at": point_json(*at)})
            }).collect::<Vec<_>>(),
        })
    }

    /// The state a `held.json` records.
    pub fn from_json(value: &Value) -> Result<Held, String> {
        let list = |name: &str| -> Result<Vec<Value>, String> {
            match value.get(name) {
                None => Ok(Vec::new()),
                Some(Value::Array(items)) => Ok(items.clone()),
                Some(other) => Err(format!("`{name}` is not a list: {other}")),
            }
        };
        let modifiers = list("modifiers")?
            .iter()
            .map(|name| {
                modifier(
                    name.as_str()
                        .ok_or_else(|| format!("bad modifier {name}"))?,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let keys = list("keys")?
            .iter()
            .map(|code| {
                code.as_u64()
                    .and_then(|code| u16::try_from(code).ok())
                    .filter(|code| *code < 128)
                    .ok_or_else(|| format!("bad keycode {code}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let buttons = list("buttons")?
            .iter()
            .map(|entry| {
                let button = match entry.get("button").and_then(Value::as_str) {
                    Some("left") => Button::Left,
                    Some("right") => Button::Right,
                    _ => return Err(format!("bad button {entry}")),
                };
                let at = entry
                    .get("at")
                    .and_then(Value::as_array)
                    .and_then(|point| {
                        Some(Point {
                            x: point.first()?.as_f64()?,
                            y: point.get(1)?.as_f64()?,
                        })
                    })
                    .ok_or_else(|| format!("bad button point {entry}"))?;
                Ok((button, at))
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Held {
            modifiers,
            keys,
            buttons,
            expired: false,
        })
    }

    /// The events that release everything held — buttons where they are, then
    /// keys, then modifiers last-pressed first — each with the flags still
    /// held after it. Empties the state.
    pub fn take_releases(&mut self) -> Vec<(Event, u64)> {
        let mut releases = Vec::new();
        while let Some((button, at)) = self.buttons.pop() {
            let event = Event::Button {
                button,
                down: false,
                at,
            };
            releases.push((event, flags_for(&self.modifiers)));
        }
        while let Some(code) = self.keys.pop() {
            releases.push((Event::Key { code, down: false }, flags_for(&self.modifiers)));
        }
        while let Some(modifier) = self.modifiers.pop() {
            let event = Event::Modifier {
                modifier,
                down: false,
            };
            releases.push((event, flags_for(&self.modifiers)));
        }
        releases
    }
}

/// Post `steps` in order, each only after its checks pass, then pause for its
/// gap. A press is journaled after its checks and before it is posted, a
/// release after it is posted (`journal_after_release`): the journal never
/// misses something that is down, and never lists a press a refusal stopped.
/// A failed check stops the sequence; what it had pressed stays in `held` for
/// the caller's release.
pub fn perform(
    desktop: &dyn Desktop,
    held: &Mutex<Held>,
    steps: &[Step],
    policy: Policy,
) -> Result<(), Failure> {
    for step in steps {
        {
            let mut held = held.lock().unwrap_or_else(|poison| poison.into_inner());
            if held.expired {
                return Err(Failure::Failed("the deadline passed mid-sequence".into()));
            }
            let flags = held.flags(step.event);
            verify(desktop, step.check, policy)?;
            if step.event.presses() {
                let mut after = held.clone();
                after.apply(step.event);
                desktop.journal(&after.to_json())?;
            }
            desktop.post(step.event, flags)?;
            held.apply(step.event);
            match step.event {
                // The button is still down, so an older point is still a
                // release of this process's own press.
                Event::Drag { .. } => desktop.journal(&held.to_json())?,
                Event::Move(_) => {}
                event if !event.presses() => journal_after_release(desktop, &held)?,
                _ => {}
            }
        }
        desktop.pause(step.gap);
    }
    Ok(())
}

/// Record what is still held once a release has been posted. A journal that
/// cannot be updated is removed rather than left listing the key or button
/// that just went up: a later recovery would post an unchecked release of it,
/// and by then the person at the desk may be holding it. The worst case is
/// the other way round — a press of this process's own left down, unrecorded.
fn journal_after_release(journal: &dyn Journal, held: &Held) -> Result<(), Failure> {
    let Err(write) = journal.journal(&held.to_json()) else {
        return Ok(());
    };
    let removed = match journal.discard_journal() {
        Ok(()) => "so it was removed".to_string(),
        Err(remove) => format!(
            "and removing it failed too ({}): a recovery from it may release a key that is up",
            remove.message()
        ),
    };
    Err(Failure::Failed(format!(
        "the journal could not be updated after a release ({}), {removed}",
        write.message()
    )))
}

/// The checks, cheapest and slowest first, so the pid check that decides who
/// receives the event is the last thing before it is posted.
fn verify(desktop: &dyn Desktop, check: Check, policy: Policy) -> Result<(), Failure> {
    let session = desktop.session()?;
    if session.locked {
        return Err(Failure::Refused("the console is locked".into()));
    }
    if !session.on_console {
        return Err(Failure::Refused(
            "the login session is not on the console".into(),
        ));
    }
    if let (Some(holder), false) = (session.secure_input_pid, policy.allow_secure_input) {
        return Err(Failure::Refused(format!(
            "Secure Input is on (ioreg names pid {holder})"
        )));
    }
    match check {
        Check::Frontmost(pid) => {
            let others = desktop.foreign_key_windows(pid)?;
            if !others.is_empty() {
                return Err(Failure::Refused(format!(
                    "another app holds a key window (pid {others:?}), so keys would not reach pid {pid}"
                )));
            }
            require_frontmost(desktop, pid)
        }
        Check::Topmost(pid, at) => require_click_target(desktop, pid, at),
        Check::Grab(pid, press) => {
            require_frontmost(desktop, pid)?;
            require_click_target(desktop, pid, press)
        }
    }
}

fn require_frontmost(desktop: &dyn Desktop, pid: i32) -> Result<(), Failure> {
    match desktop.frontmost()? {
        Some(front) if front == pid => Ok(()),
        Some(front) => Err(Failure::Refused(format!(
            "pid {pid} is not frontmost: pid {front} is"
        ))),
        None => Err(Failure::Refused(format!(
            "pid {pid} is not frontmost: no app is"
        ))),
    }
}

fn require_click_target(desktop: &dyn Desktop, pid: i32, at: Point) -> Result<(), Failure> {
    let owner = desktop.click_target(at)?;
    if owner == pid {
        return Ok(());
    }
    Err(Failure::Refused(format!(
        "a click at ({}, {}) would reach pid {owner}, not pid {pid}: {}",
        at.x,
        at.y,
        desktop.describe(at)
    )))
}

/// Post the release of everything `held` holds, unchecked: these are this
/// process's own presses, and leaving one down is worse than a release that
/// lands in another app. A release that fails to post stays in `held`, and so
/// in the journal, which then records what is still held.
pub fn release(desktop: &dyn Desktop, held: &mut Held) -> Value {
    let (released, journal) = release_and_journal(desktop, desktop, held);
    match journal {
        Ok(()) => released,
        Err(failure) => json!({"released": released, "journal": failure.message()}),
    }
}

fn release_and_journal(
    desktop: &dyn Desktop,
    journal: &dyn Journal,
    held: &mut Held,
) -> (Value, Result<(), Failure>) {
    let mut failed = Vec::new();
    let released = Value::Array(
        held.take_releases()
            .into_iter()
            .map(|(event, flags)| {
                let mut row = describe(event);
                match desktop.post(event, flags) {
                    Ok(()) => row["posted"] = true.into(),
                    Err(failure) => {
                        row["posted"] = false.into();
                        row["error"] = failure.message().into();
                        failed.push(event);
                    }
                }
                row
            })
            .collect(),
    );
    for release in failed.into_iter().rev() {
        held.keep(release);
    }
    (released, journal_after_release(journal, held))
}

/// `release-held`: release what another helper's journal kept, recording
/// what is still held back into that journal. A release that failed to post
/// stays there and — like a journal that could not be updated — makes this a
/// failure rather than a cleared record.
pub fn release_journaled(
    desktop: &dyn Desktop,
    journal: &dyn Journal,
    mut held: Held,
) -> Result<Value, Failure> {
    let (released, journal) = release_and_journal(desktop, journal, &mut held);
    if !held.is_empty() {
        return Err(Failure::Failed(format!(
            "releases that failed to post stay in the journal: {released}"
        )));
    }
    journal.map_err(|failure| {
        Failure::Failed(format!("{} (released: {released})", failure.message()))
    })?;
    Ok(released)
}

/// What the panic hook releases: the held state, or — when the panicking
/// thread holds its lock, a panic mid-step — the journal's copy of it
/// (`journaled`). Never whatever else the system reports down, which may be
/// the person at the desk's; with no journal, nothing.
pub fn release_after_panic(
    desktop: &dyn Desktop,
    held: &Mutex<Held>,
    journaled: impl FnOnce() -> Result<Held, String>,
) -> Value {
    match held.try_lock() {
        Ok(mut held) => release(desktop, &mut held),
        Err(TryLockError::Poisoned(poison)) => release(desktop, &mut poison.into_inner()),
        Err(TryLockError::WouldBlock) => match journaled() {
            Ok(mut held) => json!({"released_from_journal": release(desktop, &mut held)}),
            Err(error) => json!({"released": [], "journal": error}),
        },
    }
}

pub fn point_json(at: Point) -> Value {
    json!([at.x, at.y])
}

pub fn button_name(button: Button) -> &'static str {
    match button {
        Button::Left => "left",
        Button::Right => "right",
    }
}

fn describe(event: Event) -> Value {
    match event {
        Event::Modifier { modifier, .. } => json!({"modifier": modifier.name}),
        Event::Key { code, .. } => json!({"key": code}),
        Event::Button { button, at, .. } | Event::Drag { button, at } => {
            json!({"button": button_name(button), "at": point_json(at)})
        }
        Event::Move(at) => json!({"move": point_json(at)}),
    }
}

/// Modifiers down in order, each code tapped, modifiers up in reverse: every
/// event checked against the frontmost app.
pub fn chord(pid: i32, codes: &[u16], modifiers: &[Modifier]) -> Vec<Step> {
    let step = |event| Step {
        event,
        check: Check::Frontmost(pid),
        gap: KEY_GAP,
    };
    let mut steps: Vec<Step> = modifiers
        .iter()
        .map(|modifier| {
            step(Event::Modifier {
                modifier: *modifier,
                down: true,
            })
        })
        .collect();
    for code in codes {
        steps.push(step(Event::Key {
            code: *code,
            down: true,
        }));
        steps.push(step(Event::Key {
            code: *code,
            down: false,
        }));
    }
    steps.extend(modifiers.iter().rev().map(|modifier| {
        step(Event::Modifier {
            modifier: *modifier,
            down: false,
        })
    }));
    steps
}

/// The steps of a mouse action. Every move and press is checked against what
/// a click at its point reaches; a drag's continuation and release against the
/// press (`Check::Grab`); ctrl-click's Control events against the frontmost
/// app, like any key.
pub fn mouse(pid: i32, action: &MouseAction) -> Vec<Step> {
    let at_point = |event, at| Step {
        event,
        check: Check::Topmost(pid, at),
        gap: MOUSE_GAP,
    };
    let button = |button, down, at| at_point(Event::Button { button, down, at }, at);
    let press = |at, which| vec![at_point(Event::Move(at), at), button(which, true, at)];
    match action {
        MouseAction::Move(at) => vec![at_point(Event::Move(*at), *at)],
        MouseAction::Down(at, which) => press(*at, *which),
        MouseAction::Up(at, which) => vec![button(*which, false, *at)],
        MouseAction::Click(at) => [
            press(*at, Button::Left),
            vec![button(Button::Left, false, *at)],
        ]
        .concat(),
        MouseAction::RightClick(at) => [
            press(*at, Button::Right),
            vec![button(Button::Right, false, *at)],
        ]
        .concat(),
        MouseAction::CtrlClick(at) => {
            let control = modifier("ctrl").expect("ctrl is in the table");
            let ctrl = |down| Step {
                event: Event::Modifier {
                    modifier: control,
                    down,
                },
                check: Check::Frontmost(pid),
                gap: KEY_GAP,
            };
            vec![
                at_point(Event::Move(*at), *at),
                ctrl(true),
                button(Button::Left, true, *at),
                button(Button::Left, false, *at),
                ctrl(false),
            ]
        }
        MouseAction::Drag { path, hold, step } => {
            let start = path[0];
            let grab = |event| Step {
                event,
                check: Check::Grab(pid, start),
                gap: *step,
            };
            let mut steps = press(start, Button::Left);
            let mut from = start;
            for next in &path[1..] {
                for point in segment(from, *next) {
                    steps.push(grab(Event::Drag {
                        button: Button::Left,
                        at: point,
                    }));
                }
                from = *next;
            }
            if let Some(last) = steps.last_mut() {
                last.gap += *hold;
            }
            steps.push(grab(Event::Button {
                button: Button::Left,
                down: false,
                at: from,
            }));
            steps
        }
    }
}

/// The points of a drag between `from` and `to`, about every 8 points, ending
/// exactly on `to`.
fn segment(from: Point, to: Point) -> Vec<Point> {
    let distance = ((to.x - from.x).powi(2) + (to.y - from.y).powi(2)).sqrt();
    let steps = (distance / 8.0).ceil().max(1.0) as usize;
    (1..=steps)
        .map(|step| {
            let t = step as f64 / steps as f64;
            Point {
                x: from.x + (to.x - from.x) * t,
                y: from.y + (to.y - from.y) * t,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    const PID: i32 = 42;
    const OTHER: i32 = 7;
    const UNLOCKED: Session = Session {
        locked: false,
        on_console: true,
        secure_input_pid: None,
    };

    fn at(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    /// Answers each query from a script (the last answer repeats), and
    /// records every query, journal write, post and pause in order.
    struct Fake {
        frontmost: RefCell<VecDeque<i32>>,
        click: RefCell<VecDeque<i32>>,
        foreign: RefCell<VecDeque<Vec<i32>>>,
        session: Cell<Session>,
        /// Journal writes that succeed before every later one fails.
        journal_writes: Cell<u32>,
        discard_fails: Cell<bool>,
        /// Posts that succeed before every later one fails.
        posts_ok: Cell<u32>,
        log: RefCell<Vec<String>>,
        journals: RefCell<Vec<Value>>,
    }

    impl Fake {
        fn new(frontmost: &[i32], click: &[i32]) -> Fake {
            Fake {
                frontmost: RefCell::new(frontmost.iter().copied().collect()),
                click: RefCell::new(click.iter().copied().collect()),
                foreign: RefCell::new(VecDeque::from([Vec::new()])),
                session: Cell::new(UNLOCKED),
                journal_writes: Cell::new(u32::MAX),
                discard_fails: Cell::new(false),
                posts_ok: Cell::new(u32::MAX),
                log: RefCell::new(Vec::new()),
                journals: RefCell::new(Vec::new()),
            }
        }

        fn next<T: Clone>(queue: &RefCell<VecDeque<T>>) -> T {
            let mut queue = queue.borrow_mut();
            if queue.len() > 1 {
                queue.pop_front().unwrap()
            } else {
                queue.front().unwrap().clone()
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.borrow().clone()
        }

        fn posts(&self) -> Vec<String> {
            self.log()
                .into_iter()
                .filter(|entry| entry.starts_with("post"))
                .collect()
        }

        fn note(&self, entry: String) {
            self.log.borrow_mut().push(entry);
        }
    }

    impl Desktop for Fake {
        fn session(&self) -> Result<Session, Failure> {
            self.note("session?".into());
            Ok(self.session.get())
        }

        fn frontmost(&self) -> Result<Option<i32>, Failure> {
            self.note("frontmost?".into());
            Ok(Some(Fake::next(&self.frontmost)))
        }

        fn foreign_key_windows(&self, _pid: i32) -> Result<Vec<i32>, Failure> {
            self.note("foreign?".into());
            Ok(Fake::next(&self.foreign))
        }

        fn click_target(&self, at: Point) -> Result<i32, Failure> {
            self.note(format!("click? {},{}", at.x, at.y));
            Ok(Fake::next(&self.click))
        }

        fn describe(&self, at: Point) -> Value {
            self.note(format!("describe {},{}", at.x, at.y));
            json!({})
        }

        fn post(&self, event: Event, flags: u64) -> Result<(), Failure> {
            if self.posts_ok.get() == 0 {
                self.note(format!("post failed {}", describe(event)));
                return Err(Failure::Failed("CGEventCreate returned null".into()));
            }
            self.posts_ok.set(self.posts_ok.get().saturating_sub(1));
            self.note(format!("post {} {flags:#x}", describe(event)));
            Ok(())
        }

        fn pause(&self, _gap: Duration) {
            self.note("pause".into());
        }
    }

    impl Journal for Fake {
        fn journal(&self, held: &Value) -> Result<(), Failure> {
            if self.journal_writes.get() == 0 {
                return Err(Failure::Failed("disk full".into()));
            }
            self.journal_writes
                .set(self.journal_writes.get().saturating_sub(1));
            self.note(format!("journal {held}"));
            self.journals.borrow_mut().push(held.clone());
            Ok(())
        }

        fn discard_journal(&self) -> Result<(), Failure> {
            if self.discard_fails.get() {
                return Err(Failure::Failed("EIO".into()));
            }
            self.note("discard".into());
            Ok(())
        }
    }

    fn alt_left() -> Modifier {
        modifier("alt-left").unwrap()
    }

    fn run(fake: &Fake, steps: &[Step]) -> (Result<(), Failure>, Mutex<Held>) {
        let held = Mutex::new(Held::new());
        let result = perform(fake, &held, steps, Policy::default());
        (result, held)
    }

    /// The query that decides who receives the event is the last one before
    /// it is posted (only a journal write may sit between), and no diagnostic
    /// read runs on success.
    #[test]
    fn the_deciding_pid_check_is_the_last_query_before_every_post() {
        let fake = Fake::new(&[PID], &[PID]);
        let mut steps = chord(PID, &[11], &[alt_left()]);
        steps.extend(mouse(PID, &MouseAction::Click(at(10.0, 20.0))));
        run(&fake, &steps).0.unwrap();
        let queries: Vec<String> = fake
            .log()
            .into_iter()
            .filter(|entry| !entry.starts_with("journal"))
            .collect();
        for (index, entry) in queries.iter().enumerate() {
            if let Some(event) = entry.strip_prefix("post ") {
                let deciding = &queries[index - 1];
                if event.contains("modifier") || event.contains("key") {
                    assert_eq!(deciding, "frontmost?", "{queries:?}");
                    assert_eq!(&queries[index - 2], "foreign?", "{queries:?}");
                } else {
                    assert_eq!(deciding, "click? 10,20", "{queries:?}");
                }
            }
        }
        assert!(!fake.log().iter().any(|entry| entry.starts_with("describe")));
        assert_eq!(fake.posts().len(), 7);
    }

    #[test]
    fn a_refused_click_is_described_after_its_check() {
        let fake = Fake::new(&[PID], &[OTHER]);
        let (result, _) = run(&fake, &mouse(PID, &MouseAction::Click(at(10.0, 20.0))));
        assert!(matches!(result, Err(Failure::Refused(ref m)) if m.contains("would reach pid 7")));
        assert_eq!(
            fake.log()[fake.log().len() - 2..],
            ["click? 10,20".to_string(), "describe 10,20".to_string()]
        );
        assert!(fake.posts().is_empty());
    }

    #[test]
    fn a_foreign_key_window_stops_every_key() {
        let fake = Fake::new(&[PID], &[PID]);
        *fake.foreign.borrow_mut() = VecDeque::from([vec![OTHER]]);
        let (result, held) = run(&fake, &chord(PID, &[0], &[]));
        assert!(matches!(result, Err(Failure::Refused(ref m)) if m.contains("key window")));
        assert!(fake.posts().is_empty());
        assert!(held.lock().unwrap().take_releases().is_empty());
    }

    #[test]
    fn a_foreign_key_window_appearing_mid_chord_stops_the_rest() {
        let fake = Fake::new(&[PID], &[PID]);
        *fake.foreign.borrow_mut() = VecDeque::from([vec![], vec![OTHER]]);
        let (result, held) = run(&fake, &chord(PID, &[11], &[alt_left()]));
        assert!(result.is_err());
        assert_eq!(fake.posts().len(), 1, "only the modifier: {:?}", fake.log());
        assert_eq!(held.lock().unwrap().take_releases().len(), 1);
    }

    #[test]
    fn a_locked_or_off_console_session_or_secure_input_refuses_every_event() {
        for (session, reason) in [
            (
                Session {
                    locked: true,
                    ..UNLOCKED
                },
                "locked",
            ),
            (
                Session {
                    on_console: false,
                    ..UNLOCKED
                },
                "not on the console",
            ),
            (
                Session {
                    secure_input_pid: Some(449),
                    ..UNLOCKED
                },
                "Secure Input is on",
            ),
        ] {
            for steps in [
                chord(PID, &[0], &[]),
                mouse(PID, &MouseAction::Click(at(1.0, 1.0))),
            ] {
                let fake = Fake::new(&[PID], &[PID]);
                fake.session.set(session);
                let (result, _) = run(&fake, &steps);
                assert!(matches!(result, Err(Failure::Refused(ref m)) if m.contains(reason)));
                assert!(fake.posts().is_empty());
            }
        }
    }

    #[test]
    fn a_caller_that_confirmed_roost_holds_secure_input_may_post_through_it() {
        let fake = Fake::new(&[PID], &[PID]);
        fake.session.set(Session {
            secure_input_pid: Some(i64::from(PID)),
            ..UNLOCKED
        });
        let held = Mutex::new(Held::new());
        let policy = Policy {
            allow_secure_input: true,
        };
        perform(&fake, &held, &chord(PID, &[0], &[]), policy).unwrap();
        assert_eq!(fake.posts().len(), 2);
    }

    #[test]
    fn a_press_is_journaled_after_its_checks_and_before_it_is_posted() {
        let fake = Fake::new(&[PID], &[PID]);
        run(&fake, &chord(PID, &[11], &[alt_left()])).0.unwrap();
        let log = fake.log();
        let first_post = log
            .iter()
            .position(|entry| entry.starts_with("post"))
            .unwrap();
        assert!(log[first_post - 1].starts_with("journal"), "{log:?}");
        assert_eq!(log[first_post - 2], "frontmost?", "{log:?}");
        let journals = fake.journals.borrow();
        assert_eq!(journals[0]["modifiers"], json!(["alt-left"]));
        assert_eq!(journals[1]["keys"], json!([11]), "the key down");
        assert_eq!(journals[2]["keys"], json!([]), "the key up");
        assert_eq!(
            journals.last().unwrap(),
            &Held::new().to_json(),
            "nothing left held"
        );
    }

    #[test]
    fn a_refused_press_is_never_journaled() {
        let fake = Fake::new(&[OTHER], &[PID]);
        let (result, _) = run(&fake, &chord(PID, &[0], &[]));
        assert!(result.is_err());
        assert!(fake.journals.borrow().is_empty());
    }

    #[test]
    fn a_press_that_cannot_be_journaled_is_not_posted() {
        let fake = Fake::new(&[PID], &[PID]);
        fake.journal_writes.set(0);
        let (result, _) = run(&fake, &chord(PID, &[0], &[]));
        assert!(matches!(result, Err(Failure::Failed(ref m)) if m.contains("disk full")));
        assert!(fake.posts().is_empty());
    }

    #[test]
    fn a_drag_journals_where_its_button_is() {
        let fake = Fake::new(&[PID], &[PID]);
        let steps = mouse(
            PID,
            &MouseAction::Drag {
                path: vec![at(0.0, 0.0), at(0.0, -16.0)],
                hold: Duration::ZERO,
                step: Duration::ZERO,
            },
        );
        let held = Mutex::new(Held::new());
        perform(&fake, &held, &steps[..4], Policy::default()).unwrap();
        let journals = fake.journals.borrow();
        assert_eq!(
            journals.last().unwrap()["buttons"],
            json!([{"button": "left", "at": [0.0, -16.0]}])
        );
    }

    #[test]
    fn release_posts_exactly_what_was_held_and_clears_the_journal() {
        let fake = Fake::new(&[PID], &[PID]);
        let mut held = Held::from_json(&json!({
            "modifiers": ["alt-right"],
            "keys": [11],
            "buttons": [{"button": "left", "at": [5.0, 6.0]}],
        }))
        .unwrap();
        let released = release(&fake, &mut held);
        assert_eq!(released.as_array().unwrap().len(), 3);
        assert_eq!(
            fake.posts(),
            vec![
                r#"post {"at":[5.0,6.0],"button":"left"} 0x80040"#,
                r#"post {"key":11} 0x80040"#,
                r#"post {"modifier":"alt-right"} 0x0"#,
            ]
        );
        assert_eq!(
            fake.journals.borrow().last().unwrap(),
            &Held::new().to_json()
        );
    }

    fn journaled_everything() -> Held {
        Held::from_json(&json!({
            "modifiers": ["alt-left"],
            "keys": [11],
            "buttons": [{"button": "left", "at": [5.0, 6.0]}],
        }))
        .unwrap()
    }

    /// The key-up of a chord posts, then its journal cannot be written: the
    /// journal is removed, so it can never offer that key-up to a recovery.
    #[test]
    fn a_journal_that_cannot_follow_a_release_is_removed_not_left_stale() {
        let fake = Fake::new(&[PID], &[PID]);
        fake.journal_writes.set(2); // alt down, key down; the key-up's fails
        let (result, held) = run(&fake, &chord(PID, &[11], &[alt_left()]));
        assert!(
            matches!(result, Err(Failure::Failed(ref m)) if m.contains("so it was removed")),
            "{result:?}"
        );
        let log = fake.log();
        let key_up = log
            .iter()
            .rposition(|e| e == r#"post {"key":11} 0x80020"#)
            .unwrap();
        assert_eq!(log[key_up + 1..], ["discard".to_string()], "{log:?}");
        assert_eq!(fake.posts().len(), 3, "the sequence stops at the failure");
        // What the process still holds is its own, for its own release.
        assert_eq!(
            held.lock().unwrap().to_json()["modifiers"],
            json!(["alt-left"])
        );
    }

    #[test]
    fn a_journal_that_can_be_neither_updated_nor_removed_says_a_recovery_may_misfire() {
        let fake = Fake::new(&[PID], &[PID]);
        fake.journal_writes.set(0);
        fake.discard_fails.set(true);
        let mut held = journaled_alt_and_left();
        let released = release(&fake, &mut held);
        let message = released["journal"].as_str().unwrap();
        assert!(message.contains("removing it failed too"), "{message}");
        assert_eq!(fake.posts().len(), 2);
    }

    #[test]
    fn a_final_release_whose_journal_cannot_be_written_removes_it() {
        let fake = Fake::new(&[PID], &[PID]);
        fake.journal_writes.set(0);
        let mut held = journaled_alt_and_left();
        let released = release(&fake, &mut held);
        assert!(released["journal"].as_str().unwrap().contains("removed"));
        assert_eq!(fake.log().last().unwrap(), "discard");
    }

    #[test]
    fn a_release_that_fails_to_post_stays_held_and_journaled() {
        let fake = Fake::new(&[PID], &[PID]);
        fake.posts_ok.set(1); // the button-up posts; the key-up and alt-up fail
        let mut held = journaled_everything();
        let released = release(&fake, &mut held);
        let posted: Vec<bool> = released
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["posted"].as_bool().unwrap())
            .collect();
        assert_eq!(posted, [true, false, false]);
        let still = json!({"modifiers": ["alt-left"], "keys": [11], "buttons": []});
        assert_eq!(held.to_json(), still);
        assert_eq!(fake.journals.borrow().last().unwrap(), &still);
    }

    #[test]
    fn release_held_fails_and_keeps_in_the_journal_what_did_not_post() {
        let fake = Fake::new(&[PID], &[PID]);
        fake.posts_ok.set(1);
        let result = release_journaled(&fake, &fake, journaled_everything());
        assert!(
            matches!(result, Err(Failure::Failed(ref m)) if m.contains("stay in the journal")),
            "{result:?}"
        );
        assert_eq!(
            fake.journals.borrow().last().unwrap(),
            &json!({"modifiers": ["alt-left"], "keys": [11], "buttons": []})
        );
    }

    #[test]
    fn release_held_empties_the_journal_once_everything_posted() {
        let fake = Fake::new(&[PID], &[PID]);
        let released = release_journaled(&fake, &fake, journaled_everything()).unwrap();
        assert_eq!(released.as_array().unwrap().len(), 3);
        assert_eq!(
            fake.journals.borrow().last().unwrap(),
            &Held::new().to_json()
        );
    }

    fn journaled_alt_and_left() -> Held {
        Held::from_json(&json!({
            "modifiers": ["alt-left"],
            "buttons": [{"button": "left", "at": [5.0, 6.0]}],
        }))
        .unwrap()
    }

    #[test]
    fn a_panic_mid_step_releases_exactly_what_the_journal_kept() {
        let fake = Fake::new(&[PID], &[PID]);
        let held = Mutex::new(Held::new());
        let _mid_step = held.lock().unwrap();
        let released = release_after_panic(&fake, &held, || Ok(journaled_alt_and_left()));
        assert_eq!(
            fake.posts(),
            vec![
                r#"post {"at":[5.0,6.0],"button":"left"} 0x80020"#,
                r#"post {"modifier":"alt-left"} 0x0"#,
            ]
        );
        assert_eq!(
            released["released_from_journal"].as_array().unwrap().len(),
            2
        );
        assert_eq!(
            fake.journals.borrow().last().unwrap(),
            &Held::new().to_json()
        );
    }

    #[test]
    fn a_panic_mid_step_with_no_journal_releases_nothing() {
        let fake = Fake::new(&[PID], &[PID]);
        let held = Mutex::new(journaled_alt_and_left());
        let _mid_step = held.lock().unwrap();
        let released = release_after_panic(&fake, &held, || Err("no journal".into()));
        assert!(fake.posts().is_empty());
        assert_eq!(released["journal"], "no journal");
    }

    #[test]
    fn a_panic_outside_a_step_releases_the_held_state() {
        let fake = Fake::new(&[PID], &[PID]);
        let held = Mutex::new(journaled_alt_and_left());
        let released = release_after_panic(&fake, &held, || panic!("the journal is not needed"));
        assert_eq!(released.as_array().unwrap().len(), 2);
        assert_eq!(fake.posts().len(), 2);
    }

    #[test]
    fn a_journal_round_trips_and_rejects_garbage() {
        let state = json!({
            "modifiers": ["shift-right", "cmd-left"],
            "keys": [0, 36],
            "buttons": [{"button": "right", "at": [-10.5, 20.0]}],
        });
        assert_eq!(Held::from_json(&state).unwrap().to_json(), state);
        assert_eq!(
            Held::from_json(&json!({})).unwrap().to_json(),
            Held::new().to_json()
        );
        assert!(Held::from_json(&json!({"modifiers": ["alt"]})).is_err());
        assert!(Held::from_json(&json!({"keys": [500]})).is_err());
        assert!(
            Held::from_json(&json!({"buttons": [{"button": "middle", "at": [0, 0]}]})).is_err()
        );
    }

    #[test]
    fn a_frontmost_change_mid_chord_stops_before_the_next_key_and_releases_the_modifier() {
        let fake = Fake::new(&[PID, OTHER], &[PID]);
        let (result, held) = run(&fake, &chord(PID, &[11], &[alt_left()]));
        assert!(matches!(result, Err(Failure::Refused(ref m)) if m.contains("not frontmost")));
        assert_eq!(fake.posts().len(), 1, "{:?}", fake.log());
        assert_eq!(
            held.lock().unwrap().take_releases(),
            vec![(
                Event::Modifier {
                    modifier: alt_left(),
                    down: false
                },
                0
            )]
        );
    }

    #[test]
    fn the_press_is_checked_after_the_move_lands() {
        let fake = Fake::new(&[PID], &[PID, OTHER]);
        let (result, _) = run(&fake, &mouse(PID, &MouseAction::Click(at(10.0, 20.0))));
        assert!(result.is_err());
        assert_eq!(fake.posts(), vec![r#"post {"move":[10.0,20.0]} 0x0"#]);
        let log = fake.log();
        let pause = log.iter().position(|entry| entry == "pause").unwrap();
        assert_eq!(log[pause + 2], "click? 10,20", "{log:?}");
    }

    #[test]
    fn ctrl_click_checks_its_control_events_against_the_keyboard() {
        let steps = mouse(PID, &MouseAction::CtrlClick(at(5.0, 5.0)));
        let checks: Vec<Check> = steps.iter().map(|step| step.check).collect();
        assert_eq!(
            checks,
            vec![
                Check::Topmost(PID, at(5.0, 5.0)),
                Check::Frontmost(PID),
                Check::Topmost(PID, at(5.0, 5.0)),
                Check::Topmost(PID, at(5.0, 5.0)),
                Check::Frontmost(PID),
            ]
        );
        let fake = Fake::new(&[OTHER], &[PID]);
        let (result, _) = run(&fake, &steps);
        assert!(result.is_err());
        assert_eq!(fake.posts().len(), 1, "only the move: {:?}", fake.log());
    }

    #[test]
    fn a_drag_stops_and_releases_when_its_grab_changes_hands() {
        let steps = mouse(
            PID,
            &MouseAction::Drag {
                path: vec![at(10.0, 10.0), at(10.0, -30.0)],
                hold: Duration::from_millis(500),
                step: Duration::from_millis(16),
            },
        );
        assert!(steps[2..]
            .iter()
            .all(|step| step.check == Check::Grab(PID, at(10.0, 10.0))));
        let fake = Fake::new(&[OTHER], &[PID]);
        let (result, held) = run(&fake, &steps);
        assert!(result.is_err());
        assert_eq!(fake.posts().len(), 2);
        assert_eq!(
            held.lock().unwrap().take_releases(),
            vec![(
                Event::Button {
                    button: Button::Left,
                    down: false,
                    at: at(10.0, 10.0)
                },
                0
            )]
        );
    }

    #[test]
    fn a_drag_holds_at_its_end_and_releases_there() {
        let steps = mouse(
            PID,
            &MouseAction::Drag {
                path: vec![at(0.0, 0.0), at(0.0, -40.0)],
                hold: Duration::from_millis(1000),
                step: Duration::from_millis(16),
            },
        );
        assert_eq!(steps.len() - 3, 5);
        assert_eq!(steps[steps.len() - 2].gap, Duration::from_millis(1016));
        assert_eq!(
            steps.last().unwrap().event,
            Event::Button {
                button: Button::Left,
                down: false,
                at: at(0.0, -40.0)
            }
        );
    }

    #[test]
    fn a_key_down_cut_off_by_the_deadline_is_released() {
        let fake = Fake::new(&[PID], &[PID]);
        let held = Mutex::new(Held::new());
        perform(
            &fake,
            &held,
            &chord(PID, &[11], &[alt_left()])[..2],
            Policy::default(),
        )
        .unwrap();
        let mut held = held.lock().unwrap();
        held.expire();
        let events: Vec<Event> = held
            .take_releases()
            .into_iter()
            .map(|(event, _)| event)
            .collect();
        assert_eq!(
            events,
            vec![
                Event::Key {
                    code: 11,
                    down: false
                },
                Event::Modifier {
                    modifier: alt_left(),
                    down: false
                },
            ]
        );
    }

    #[test]
    fn nothing_is_posted_once_expired() {
        let fake = Fake::new(&[PID], &[PID]);
        let held = Mutex::new(Held::new());
        held.lock().unwrap().expire();
        assert!(perform(&fake, &held, &chord(PID, &[11], &[]), Policy::default()).is_err());
        assert!(fake.posts().is_empty());
    }

    #[test]
    fn modifier_flags_include_the_side_bit_while_held() {
        let fake = Fake::new(&[PID], &[PID]);
        run(&fake, &chord(PID, &[11], &[alt_left()])).0.unwrap();
        let posts = fake.posts();
        assert!(posts[0].ends_with("0x80020"), "{posts:?}");
        assert!(posts[1].ends_with("0x80020"), "{posts:?}");
        assert!(posts[3].ends_with("0x0"), "{posts:?}");
    }

    #[test]
    fn a_drag_segment_steps_about_every_eight_points_and_ends_on_target() {
        let points = segment(at(0.0, 0.0), at(0.0, -40.0));
        assert_eq!(points.len(), 5);
        assert_eq!(points.last(), Some(&at(0.0, -40.0)));
        assert_eq!(segment(at(3.0, 3.0), at(3.0, 3.0)), vec![at(3.0, 3.0)]);
    }
}
