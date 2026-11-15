//! A tab's process layer (#621, #519): which process its hook owner, and
//! the program-status records that decide it, are stamped with; what a
//! prompt mark parked while that process was not in front; and whether it
//! is stopped. A tab neither names has a third layer: an agent known only
//! by its process's name, found when someone looks at the tab.
//!
//! Pure apart from the reads the sampler hands in
//! ([`crate::foreground::ProcessSample`]); what the evidence decides is
//! [`roost_ipc::process`]'s. The workspace keeps one axis per tab beside
//! its hook state and records, mutates it under the same lock, and never
//! reads a process itself: marks see only the foreground group a probe
//! read before the lock, and samples run on the sampler task.
//!
//! The layers are followed alike, each with an epoch. Every time a
//! layer starts over — an owner claimed or released, records that start
//! or stop deciding the tab, an end — its epoch moves on and what it
//! parked is dropped, so a sample requested for one, or state parked
//! under it, can never land on the next.

use roost_ipc::agent::{self, AgentLifecycle, AgentTabState, ShellState};
use roost_ipc::process::{
    self, Foreground, Layer, Liveness, MarkVerdict, ProcessState, SampleVerdict, Stamp, TabProcess,
};
use roost_ipc::program_status::{app_label, PromptMark};

use crate::foreground::{Leader, ProcessSample};
use crate::program_status::{Banner, ParkedRecords, ProgramStatusRecords};

/// One tab's process layer. Not persisted, like the hook state it follows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProcessAxis {
    /// The hook owner's process; what it parks is the lifecycle a mark
    /// dropped.
    owner: Followed<AgentLifecycle>,
    /// The process behind the program-status records while they decide
    /// the tab; what it parks is what a mark took off the records.
    records: Followed<ParkedRecords>,
    /// An agent in front of a tab that no owner claims and no records
    /// decide, named by [`process::identify`] (cursor-agent with no hooks,
    /// Codex at its sign-in screen). It has nothing to park: it only
    /// shows, running or suspended, until its process is gone.
    identity: Followed<()>,
    /// Whether the identity layer is on the wire. One a look found shows
    /// at once; one handed over from records that left ([`Left`]) shows
    /// only once a read says its process stopped or runs on, so a program
    /// that cleared its status as it exited never flashes a row.
    identity_shown: bool,
    /// The records' named stamp, kept from when they stopped deciding by
    /// their program's own word until the next mark.
    left: Option<Left>,
    /// The sampler has something to read for this tab.
    due: bool,
    /// The sample due looks for an agent in front, to name: someone looked
    /// at a tab nothing on it follows.
    discover: bool,
    /// The title of an exit banner owed to the next `D`: a sample found a
    /// busy process gone after the shell took the terminal back from it,
    /// but before the shell printed the `D` that carries its exit code.
    exit_pending: Option<String>,
}

/// Records that stopped deciding by their program's own word — a
/// `clear`, or a prompt mark of its own — while stamped with a process
/// [`process::identify`] names. Codex clears its status as it stops, so a
/// `Ctrl-Z` leaves its records nothing to park; if the next mark is the
/// shell taking the terminal back, the stamp passes to the identity layer,
/// unshown, and the read that mark asks for says whether the program is
/// stopped, in the background, or gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Left {
    stamp: Stamp,
    agent: &'static str,
    incarnation: u64,
}

/// One layer's process, and what a mark set aside while it was not in
/// front: a stopped or backgrounded program is still alive.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Followed<P> {
    epoch: u64,
    follow: Follow,
    parked: Option<P>,
    suspended: bool,
    /// Unix seconds: when the stamp was taken or `suspended` last moved.
    since: i64,
    /// The spawn the stamp was taken under. A sample from another one
    /// means the PTY was replaced, and the stamp is forgotten.
    incarnation: u64,
    /// The stamped group had the terminal when last seen: by the sample
    /// that stamped it, a later sample, or a prompt mark read with the
    /// foreground known. The mark that ends that is the shell's report on
    /// the process; a later one is about some other command.
    in_front: bool,
    /// The code the shell's `D` reported as it took the terminal back
    /// from the process, until the process stops or runs again: a stop's
    /// code (128 + `SIGTSTP`) is not an exit.
    exit_code: Option<i32>,
    /// What the stamped process's name said it is, read with the stamp.
    /// Display only: nothing it follows ends by it.
    agent: Option<&'static str>,
}

impl<P> Default for Followed<P> {
    fn default() -> Self {
        Self {
            epoch: 0,
            follow: Follow::Off,
            parked: None,
            suspended: false,
            since: 0,
            incarnation: 0,
            in_front: false,
            exit_code: None,
            agent: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Follow {
    /// Nothing to follow: what a mark always did applies.
    #[default]
    Off,
    /// The layer became live; the next sample stamps the leader in front.
    /// Until it does, the layer counts as unstamped.
    Wanted,
    /// The sample after the layer became live found nothing it may stamp,
    /// so what a mark always did applies until the layer ends.
    Unstamped,
    Stamped(Stamp),
}

impl<P> Followed<P> {
    fn stamp(&self) -> Option<&Stamp> {
        match &self.follow {
            Follow::Stamped(stamp) => Some(stamp),
            Follow::Off | Follow::Wanted | Follow::Unstamped => None,
        }
    }

    fn restart(&mut self, follow: Follow) {
        self.epoch += 1;
        self.follow = follow;
        self.parked = None;
        self.suspended = false;
        self.in_front = false;
        self.exit_code = None;
        self.agent = None;
    }

    /// Take `leader`'s stamp, read in `incarnation`'s spawn at `now`.
    fn stamp_with(&mut self, leader: &Leader, incarnation: u64, now: i64) {
        self.follow = Follow::Stamped(leader.stamp);
        self.agent = leader.agent;
        self.incarnation = incarnation;
        self.since = now;
        // The leader stamped is the one in front.
        self.in_front = true;
    }

    fn has_a_sample_to_take(&self) -> bool {
        matches!(self.follow, Follow::Wanted | Follow::Stamped(_))
    }

    /// This layer's stamp as records that leave hand it on, when its
    /// process has a name.
    fn left(&self) -> Option<Left> {
        Some(Left {
            stamp: *self.stamp()?,
            agent: self.agent?,
            incarnation: self.incarnation,
        })
    }

    fn set_suspended(&mut self, suspended: bool, now: i64) {
        if std::mem::replace(&mut self.suspended, suspended) != suspended {
            self.since = now;
        }
        self.exit_code = None;
    }

    /// An `A`, `B` or `D` mark read with `fg` in front, carrying
    /// `exit_code` if a `D` reported one. The first such mark with another
    /// group in front, after the stamped one was, is the shell taking the
    /// terminal back from it, and its code is the process's.
    fn at_mark(&mut self, fg: Foreground, exit_code: Option<i32>) {
        let Some(stamp) = self.stamp() else {
            return;
        };
        match fg {
            Foreground::Group(pgid) if pgid == stamp.pgid => self.in_front = true,
            // Not knowing who has the terminal, no code can be called the
            // process's, now or at a later mark.
            Foreground::Unknown => self.in_front = false,
            Foreground::Shell | Foreground::Group(_) => {
                if std::mem::take(&mut self.in_front) {
                    self.exit_code = exit_code;
                }
            }
        }
    }

    /// What `sample`, read for the layer as of `epoch`, says of it: a
    /// wanted stamp is taken, and a stamp from another spawn forgotten, on
    /// the way. `None` when the sample has nothing to say: it was read for
    /// an earlier layer, or there was no stamp to read.
    fn verdict(&mut self, epoch: u64, sample: &ProcessSample, now: i64) -> Option<SampleVerdict> {
        if epoch != self.epoch {
            return None;
        }
        match self.follow {
            Follow::Off | Follow::Unstamped => None,
            Follow::Wanted => {
                match &sample.leader {
                    Some(leader) => self.stamp_with(leader, sample.incarnation, now),
                    None => self.follow = Follow::Unstamped,
                }
                None
            }
            Follow::Stamped(_) if sample.incarnation != self.incarnation => {
                self.follow = Follow::Unstamped;
                self.parked = None;
                self.suspended = false;
                self.in_front = false;
                self.exit_code = None;
                self.agent = None;
                None
            }
            Follow::Stamped(stamp) => {
                let liveness = sample.liveness_of(&stamp).unwrap_or(Liveness::Unknown);
                match sample.foreground {
                    Foreground::Group(pgid) => self.in_front = pgid == stamp.pgid,
                    Foreground::Shell => self.in_front = false,
                    Foreground::Unknown => {}
                }
                Some(process::on_sample(
                    Some(&stamp),
                    liveness,
                    sample.foreground,
                ))
            }
        }
    }

    /// `Tab.process` for this layer, named `agent`, else what its process's
    /// name says.
    fn wire(&self, agent: String) -> Option<TabProcess> {
        let stamp = self.stamp()?;
        let agent = if agent.is_empty() {
            self.agent.unwrap_or_default().to_owned()
        } else {
            agent
        };
        Some(TabProcess {
            state: if self.suspended {
                ProcessState::Suspended
            } else {
                ProcessState::Running
            },
            agent,
            pid: stamp.pid,
            since: self.since,
        })
    }
}

/// What the sampler reads for one tab, as of the layers it was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SampleRequest {
    pub(crate) tab_id: i64,
    owner_epoch: u64,
    records_epoch: u64,
    identity_epoch: u64,
    /// The records' report count when the sample was asked for: what an
    /// end may take is what was reported by then.
    records_seq: u64,
    /// Read the leader in front: a layer wants its stamp, or `discover`.
    pub(crate) leader_wanted: bool,
    /// Look for an agent in front to name.
    discover: bool,
    pub(crate) stamps: Vec<Stamp>,
}

/// What a sample ended, for the workspace to log once its lock is gone,
/// and the exit banner the end raises.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Ended {
    pub(crate) owner: bool,
    pub(crate) records: bool,
    pub(crate) identity: bool,
    pub(crate) banner: Option<Banner>,
    /// The agent a look found in front and named.
    pub(crate) named: Option<&'static str>,
}

/// What one sample's end knows of the process it ended, layer by layer,
/// merged: they are one process unless two agents share the tab.
#[derive(Debug, Default)]
struct Gone {
    /// Empty when nothing names it; the workspace then uses the tab's
    /// title.
    title: String,
    /// It was working or blocked: an owner `working` or `waiting`, or a
    /// live record, set aside or not.
    busy: bool,
    exit_code: Option<i32>,
    in_front: bool,
}

impl Gone {
    fn add(&mut self, title: &str, busy: bool, exit_code: Option<i32>, in_front: bool) {
        if self.title.is_empty() {
            title.clone_into(&mut self.title);
        }
        self.busy |= busy;
        self.exit_code = self.exit_code.or(exit_code);
        self.in_front |= in_front;
    }
}

fn is_busy(lifecycle: AgentLifecycle) -> bool {
    matches!(lifecycle, AgentLifecycle::Working | AgentLifecycle::Waiting)
}

/// The exit banner's text for a `D`'s exit code, `None` for success: a
/// code above 128 is the shell's report of a death by signal 128 less,
/// named as this machine numbers signals, where the shell ran.
fn exit_body(code: i32) -> Option<String> {
    match code {
        0 => None,
        129..=255 => {
            let signal = code - 128;
            Some(match signal_name(signal) {
                Some(name) => format!("killed ({name})"),
                None => format!("killed (signal {signal})"),
            })
        }
        _ => Some(format!("exited ({code})")),
    }
}

fn signal_name(signal: i32) -> Option<&'static str> {
    const NAMES: [(libc::c_int, &str); 29] = [
        (libc::SIGHUP, "SIGHUP"),
        (libc::SIGINT, "SIGINT"),
        (libc::SIGQUIT, "SIGQUIT"),
        (libc::SIGILL, "SIGILL"),
        (libc::SIGTRAP, "SIGTRAP"),
        (libc::SIGABRT, "SIGABRT"),
        (libc::SIGBUS, "SIGBUS"),
        (libc::SIGFPE, "SIGFPE"),
        (libc::SIGKILL, "SIGKILL"),
        (libc::SIGUSR1, "SIGUSR1"),
        (libc::SIGSEGV, "SIGSEGV"),
        (libc::SIGUSR2, "SIGUSR2"),
        (libc::SIGPIPE, "SIGPIPE"),
        (libc::SIGALRM, "SIGALRM"),
        (libc::SIGTERM, "SIGTERM"),
        (libc::SIGCHLD, "SIGCHLD"),
        (libc::SIGCONT, "SIGCONT"),
        (libc::SIGSTOP, "SIGSTOP"),
        (libc::SIGTSTP, "SIGTSTP"),
        (libc::SIGTTIN, "SIGTTIN"),
        (libc::SIGTTOU, "SIGTTOU"),
        (libc::SIGURG, "SIGURG"),
        (libc::SIGXCPU, "SIGXCPU"),
        (libc::SIGXFSZ, "SIGXFSZ"),
        (libc::SIGVTALRM, "SIGVTALRM"),
        (libc::SIGPROF, "SIGPROF"),
        (libc::SIGWINCH, "SIGWINCH"),
        (libc::SIGIO, "SIGIO"),
        (libc::SIGSYS, "SIGSYS"),
    ];
    NAMES
        .iter()
        .find(|&&(number, _)| number == signal)
        .map(|&(_, name)| name)
}

impl ProcessAxis {
    /// The owner changed: a claim or a release a report made, or a release
    /// this layer made. An agent's claim wants a stamp; anything else
    /// leaves the tab unstamped. Either way an agent named only by its
    /// process is no longer one: an owner names the tab now, and a release
    /// is the agent leaving.
    pub(crate) fn owner_changed(&mut self, agent: &AgentTabState) {
        if process::owner_follows_a_process(agent) {
            self.owner.restart(Follow::Wanted);
            self.exit_pending = None;
            self.due = true;
        } else {
            self.owner.restart(Follow::Off);
        }
        self.identity.restart(Follow::Off);
        self.left = None;
    }

    /// Ask for a sample, when there is a stamp to take or to read.
    /// Answers whether it asked.
    pub(crate) fn request_sample(&mut self) -> bool {
        if ![
            self.owner.has_a_sample_to_take(),
            self.records.has_a_sample_to_take(),
            self.identity.has_a_sample_to_take(),
        ]
        .contains(&true)
        {
            return false;
        }
        self.due = true;
        true
    }

    /// Someone looked at the tab — a focus moved onto it or off it, a
    /// session's viewer came or went, a command started. Ask for a sample
    /// when there is a stamp to take or to read; on a tab that nothing
    /// follows and no one owns (`agent`), look for an agent in front to
    /// name instead. Answers whether it asked.
    pub(crate) fn request_look(&mut self, agent: &AgentTabState) -> bool {
        if self.request_sample() {
            return true;
        }
        if !self.discoverable(agent) {
            return false;
        }
        self.discover = true;
        self.due = true;
        true
    }

    /// Whether an agent in front would be named by its process alone: no
    /// owner, and nothing followed.
    fn discoverable(&self, agent: &AgentTabState) -> bool {
        agent.ownership.is_none()
            && [
                &self.owner.follow,
                &self.records.follow,
                &self.identity.follow,
            ]
            .iter()
            .all(|follow| **follow == Follow::Off)
    }

    /// Whether the tick reads this tab: it holds a stamp, and with it
    /// whatever it parked. A tab that holds none is never looked at on a
    /// tick, whatever runs in front of it; nor is an unshown identity,
    /// which the mark that handed it over asked a read for.
    pub(crate) fn ticks(&self) -> bool {
        self.owner.stamp().is_some()
            || self.records.stamp().is_some()
            || (self.identity_shown && self.identity.stamp().is_some())
    }

    /// Ask for the tick's sample, when [`Self::ticks`]. Answers whether it
    /// asked.
    pub(crate) fn request_tick(&mut self) -> bool {
        self.due |= self.ticks();
        self.ticks()
    }

    pub(crate) fn is_due(&self) -> bool {
        self.due
    }

    /// Whether an `A`, `B` or `D` mark read with `fg` in front takes the
    /// records it would drop aside rather than dropping them: they follow
    /// a process, and another group is in front.
    pub(crate) fn parks_records_at(&self, mark: PromptMark, fg: Foreground) -> bool {
        mark != PromptMark::OutputStart
            && process::at_mark(Layer::Records, self.records.stamp(), fg) == MarkVerdict::Park
    }

    /// What a parking mark took off the records. A second mark while they
    /// are parked keeps the first's: what it takes is whatever ran in
    /// front meanwhile, which the mark drops as it always did.
    ///
    /// A park that took nothing holds nothing. Codex clears its own status
    /// as it stops, often in the read that carries the shell's `D`; had
    /// the clear come a read earlier, the records would have stopped
    /// deciding and the layer ended before the mark, so it ends here too,
    /// and what names a stopped Codex is its process ([`Left`]).
    pub(crate) fn park_records(&mut self, parked: ParkedRecords) {
        if parked.is_empty() {
            return;
        }
        self.records.parked.get_or_insert(parked);
        self.due = true;
    }

    /// The records after a batch, or a sample: while they decide the tab,
    /// or a mark parked some, the layer is live and wants a stamp; once
    /// neither holds, it ends.
    pub(crate) fn records_settled(&mut self, records: &ProgramStatusRecords) {
        let live = records.is_authoritative() || self.records.parked.is_some();
        match (live, self.records.follow) {
            (true, Follow::Off) => {
                self.records.restart(Follow::Wanted);
                self.identity.restart(Follow::Off);
                self.left = None;
                self.exit_pending = None;
                self.due = true;
            }
            (false, Follow::Wanted | Follow::Unstamped | Follow::Stamped(_)) => {
                if self.identity.follow == Follow::Off {
                    self.left = self.records.left();
                }
                self.records.restart(Follow::Off);
            }
            _ => {}
        }
    }

    /// An OSC 133 mark, with `before` the hook state before the mark's own
    /// effect and `agent` the state after it, read with `fg` in front.
    ///
    /// A `C` asks for a look ([`Self::request_look`]): on a stamped tab a
    /// `fg` is one. An `A`, `B` or `D` notes who has the terminal, and the
    /// code of the `D` that took it from a stamped process, then applies
    /// [`process::at_mark`]'s verdict to the owner, and asks after an agent
    /// named by its process alone once it is not in front. Answers the
    /// exit banner owed to this `D`.
    pub(crate) fn at_mark(
        &mut self,
        mark: PromptMark,
        before: &AgentTabState,
        agent: &mut AgentTabState,
        fg: Foreground,
    ) -> Option<Banner> {
        let exit_code = match mark {
            PromptMark::OutputStart => {
                self.exit_pending = None;
                // A command starts: no later mark is about the program
                // that left.
                self.left = None;
                if !self.identity_shown && self.identity.stamp().is_some() {
                    self.identity.restart(Follow::Off);
                }
                self.request_look(agent);
                return None;
            }
            PromptMark::CommandEnd { exit_code } => exit_code,
            PromptMark::PromptStart | PromptMark::InputStart => None,
        };
        let banner = match mark {
            PromptMark::CommandEnd { .. } => self.exit_pending.take().and_then(|title| {
                Some(Banner {
                    title,
                    body: exit_body(exit_code?)?,
                })
            }),
            _ => None,
        };
        self.owner.at_mark(fg, exit_code);
        self.records.at_mark(fg, exit_code);
        self.hand_over(fg, agent);
        if process::at_mark(Layer::Identity, self.identity.stamp(), fg) == MarkVerdict::Park {
            self.due = true;
        }
        if !process::owner_follows_a_process(before) {
            return banner;
        }
        match process::at_mark(Layer::Owner, self.owner.stamp(), fg) {
            MarkVerdict::Today => {}
            MarkVerdict::Release => {
                *agent = process::released(agent);
                self.owner_changed(agent);
            }
            MarkVerdict::Park => {
                if before.lifecycle != AgentLifecycle::Inactive {
                    self.owner.parked = Some(before.lifecycle);
                }
                self.due = true;
            }
        }
        banner
    }

    /// At an `A`, `B` or `D` read with `fg` in front: records that left
    /// ([`Left`]) — before this mark, or in its own read, leaving it
    /// nothing to park — pass their stamp to the identity layer, unshown,
    /// with a read asked for, once the shell or another job has the
    /// terminal and nothing else names the tab. A mark from the program's
    /// own group keeps them for the next; one with no one known in front
    /// forgets them.
    fn hand_over(&mut self, fg: Foreground, agent: &AgentTabState) {
        let left_in_this_read = self
            .records
            .stamp()
            .filter(|stamp| {
                self.records.parked.is_none()
                    && process::at_mark(Layer::Records, Some(stamp), fg) == MarkVerdict::Park
            })
            .and_then(|_| self.records.left());
        let Some(left) = left_in_this_read.or_else(|| self.left.take()) else {
            return;
        };
        match fg {
            Foreground::Group(pgid) if pgid == left.stamp.pgid => {
                self.left = Some(left);
                return;
            }
            Foreground::Unknown => return,
            Foreground::Shell | Foreground::Group(_) => {}
        }
        if agent.ownership.is_some() || self.identity.follow != Follow::Off {
            return;
        }
        self.identity.restart(Follow::Stamped(left.stamp));
        self.identity.agent = Some(left.agent);
        self.identity.incarnation = left.incarnation;
        self.identity_shown = false;
        self.due = true;
    }

    /// The sample this tab is due, if any, clearing the flag. A look asked
    /// for while nothing was followed is dropped if something is now, or
    /// the tab is owned (`agent`).
    pub(crate) fn take_request(
        &mut self,
        tab_id: i64,
        records: &ProgramStatusRecords,
        agent: &AgentTabState,
    ) -> Option<SampleRequest> {
        if !std::mem::take(&mut self.due) {
            return None;
        }
        let discover = std::mem::take(&mut self.discover) && self.discoverable(agent);
        let leader_wanted = discover
            || self.owner.follow == Follow::Wanted
            || self.records.follow == Follow::Wanted;
        let mut stamps: Vec<Stamp> = Vec::new();
        for stamp in [
            self.owner.stamp(),
            self.records.stamp(),
            self.identity.stamp(),
        ]
        .into_iter()
        .flatten()
        {
            if !stamps.contains(stamp) {
                stamps.push(*stamp);
            }
        }
        if !leader_wanted && stamps.is_empty() {
            return None;
        }
        Some(SampleRequest {
            tab_id,
            owner_epoch: self.owner.epoch,
            records_epoch: self.records.epoch,
            identity_epoch: self.identity.epoch,
            records_seq: records.seq(),
            leader_wanted,
            discover,
            stamps,
        })
    }

    /// What `sample`, read for `request`, does to the owner `agent` and to
    /// `records`. `now` is unix seconds.
    ///
    /// A layer that started over since the request changes nothing: the
    /// claim, release or new record that restarted it asked for its own.
    pub(crate) fn apply_sample(
        &mut self,
        request: &SampleRequest,
        sample: &ProcessSample,
        agent: &mut AgentTabState,
        records: &mut ProgramStatusRecords,
        now: i64,
    ) -> Ended {
        let mut ended = Ended::default();
        let mut gone = Gone::default();
        let owner_in_front = self.owner.in_front;
        match self.owner.verdict(request.owner_epoch, sample, now) {
            None | Some(SampleVerdict::Keep) => {}
            Some(SampleVerdict::Restore) => {
                self.owner.set_suspended(false, now);
                if let Some(lifecycle) = self.owner.parked.take() {
                    if agent.lifecycle == AgentLifecycle::Inactive {
                        agent.lifecycle = lifecycle;
                    }
                }
            }
            Some(SampleVerdict::Suspend) => {
                if agent.lifecycle != AgentLifecycle::Inactive {
                    self.owner.parked = Some(agent.lifecycle);
                    agent.lifecycle = AgentLifecycle::Inactive;
                }
                self.owner.set_suspended(true, now);
            }
            Some(SampleVerdict::End) => {
                let title = agent
                    .ownership
                    .as_ref()
                    .map_or("", |owner| agent::source_label(&owner.source));
                gone.add(
                    title,
                    is_busy(self.owner.parked.unwrap_or(agent.lifecycle)),
                    self.owner.exit_code,
                    owner_in_front,
                );
                *agent = process::released(agent);
                self.owner_changed(agent);
                ended.owner = true;
            }
        }
        let records_in_front = self.records.in_front;
        match self.records.verdict(request.records_epoch, sample, now) {
            None | Some(SampleVerdict::Keep) => {}
            Some(SampleVerdict::Restore) => {
                self.records.set_suspended(false, now);
                if let Some(parked) = self.records.parked.take() {
                    records.restore(parked);
                }
            }
            Some(SampleVerdict::Suspend) => {
                if self.records.parked.is_none() && records.is_authoritative() {
                    self.records.parked = Some(records.park());
                }
                self.records.set_suspended(true, now);
            }
            Some(SampleVerdict::End) => {
                // With the shell back in front, nothing else can have
                // reported; with another job there, what it reported
                // since the request is its own.
                let upto = (sample.foreground != Foreground::Shell).then_some(request.records_seq);
                let parked = self.records.parked.take();
                let app = match &parked {
                    Some(parked) => parked.app.clone(),
                    None => records.deciding_app().unwrap_or_default().to_owned(),
                };
                let took_live = records.end(upto);
                gone.add(
                    app_label(&app).unwrap_or(&app),
                    took_live || parked.as_ref().is_some_and(ParkedRecords::holds_live),
                    self.records.exit_code,
                    records_in_front,
                );
                self.records.restart(Follow::Off);
                ended.records = true;
            }
        }
        self.records_settled(records);
        if ended.owner || ended.records {
            ended.banner = self.exit_banner(gone, agent.shell, sample.foreground);
        }
        self.apply_identity(request, sample, agent, now, &mut ended);
        ended
    }

    /// What `sample` does to an agent named by its process alone: it
    /// suspends, runs again and ends as any stamped layer does — though
    /// with nothing parked, and no banner, since it was never seen to do
    /// anything — and a look finds one to name.
    fn apply_identity(
        &mut self,
        request: &SampleRequest,
        sample: &ProcessSample,
        agent: &AgentTabState,
        now: i64,
        ended: &mut Ended,
    ) {
        match self.identity.verdict(request.identity_epoch, sample, now) {
            None | Some(SampleVerdict::Keep) => {}
            Some(verdict @ (SampleVerdict::Restore | SampleVerdict::Suspend)) => {
                if !std::mem::replace(&mut self.identity_shown, true) {
                    self.identity.since = now;
                }
                self.identity
                    .set_suspended(verdict == SampleVerdict::Suspend, now);
            }
            Some(SampleVerdict::End) => {
                self.identity.restart(Follow::Off);
                ended.identity = true;
            }
        }
        // A stamp from another spawn: forgotten, and the next look may
        // name what runs there now.
        if self.identity.follow == Follow::Unstamped {
            self.identity.restart(Follow::Off);
        }
        if !request.discover
            || request.identity_epoch != self.identity.epoch
            || !self.discoverable(agent)
        {
            return;
        }
        if let Some(leader) = sample
            .leader
            .as_ref()
            .filter(|leader| leader.agent.is_some())
        {
            self.identity.stamp_with(leader, sample.incarnation, now);
            self.identity_shown = true;
            ended.named = leader.agent;
        }
    }

    /// The banner an end raises: a process that was working or blocked
    /// is gone, and the shell's `D` said it failed. Without that `D`'s
    /// code there is none — unless the sample read the process gone
    /// between the shell taking the terminal back from it and printing
    /// its `D`, which then is the next `D`, and the banner waits for it.
    fn exit_banner(&mut self, gone: Gone, shell: ShellState, fg: Foreground) -> Option<Banner> {
        if !gone.busy {
            return None;
        }
        if let Some(code) = gone.exit_code {
            return exit_body(code).map(|body| Banner {
                title: gone.title,
                body,
            });
        }
        if gone.in_front && fg == Foreground::Shell && shell != ShellState::Unknown {
            self.exit_pending = Some(gone.title);
        }
        None
    }

    /// `Tab.process` for a tab whose hook state is `agent` and whose
    /// records are `records`: the records' process while they follow one,
    /// since they decide what the tab shows, else the owner's, else an
    /// agent named by its process alone.
    pub(crate) fn wire(
        &self,
        agent: &AgentTabState,
        records: &ProgramStatusRecords,
    ) -> Option<TabProcess> {
        if self.records.stamp().is_some() {
            let app = match &self.records.parked {
                Some(parked) => parked.app.clone(),
                None => records.deciding_app().unwrap_or_default().to_owned(),
            };
            return self.records.wire(app);
        }
        if self.owner.stamp().is_some() {
            return self.owner.wire(
                agent
                    .ownership
                    .as_ref()
                    .map(|owner| owner.source.clone())
                    .unwrap_or_default(),
            );
        }
        if !self.identity_shown {
            return None;
        }
        self.identity.wire(String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ipc::agent::{Ownership, ShellState};
    use roost_ipc::program_status::{ProgramStatusReport, ProgramStatusState, StatusEvent};

    const NOW: i64 = 1_700_000_000;
    const T_MS: i64 = 1_793_158_450_000;

    fn stamp() -> Stamp {
        Stamp {
            pid: 700,
            start: (12_345, 0),
            pgid: 700,
            sid: 500,
        }
    }

    fn owned(source: &str, lifecycle: AgentLifecycle) -> AgentTabState {
        AgentTabState {
            shell: ShellState::ForegroundProcess,
            lifecycle,
            ownership: Some(Ownership {
                source: source.into(),
                session_id: "s1".into(),
                ..Ownership::default()
            }),
        }
    }

    fn sample(foreground: Foreground, leader: Option<Stamp>, liveness: Liveness) -> ProcessSample {
        ProcessSample {
            incarnation: 1,
            foreground,
            leader: leader.map(|stamp| Leader { stamp, agent: None }),
            liveness: vec![(stamp(), liveness)],
        }
    }

    /// A claim, then the sample that stamps the leader in front.
    fn stamped(agent: &AgentTabState) -> ProcessAxis {
        let mut axis = ProcessAxis::default();
        axis.owner_changed(agent);
        let request = axis
            .take_request(1, &ProgramStatusRecords::default(), agent)
            .expect("a claim asks for a stamp");
        assert!(request.leader_wanted);
        let mut agent = agent.clone();
        axis.apply_sample(
            &request,
            &sample(Foreground::Group(700), Some(stamp()), Liveness::Running),
            &mut agent,
            &mut ProgramStatusRecords::default(),
            NOW,
        );
        assert_eq!(
            axis.owner.stamp(),
            Some(&stamp()),
            "the precondition: stamped"
        );
        axis
    }

    /// The mark's own effect, as `ProgramStatusRecords::apply` makes it,
    /// then the axis's.
    fn mark(
        axis: &mut ProcessAxis,
        agent: &AgentTabState,
        body: &str,
        fg: Foreground,
    ) -> AgentTabState {
        let mark = match body {
            "C" => PromptMark::OutputStart,
            "A" => PromptMark::PromptStart,
            _ => PromptMark::CommandEnd { exit_code: None },
        };
        let mut after = roost_ipc::agent::apply_shell_mark(agent, body).unwrap();
        axis.at_mark(mark, agent, &mut after, fg);
        after
    }

    fn sampled(axis: &mut ProcessAxis, agent: &mut AgentTabState, sample: &ProcessSample) {
        let mut records = ProgramStatusRecords::default();
        let request = axis
            .take_request(1, &records, agent)
            .expect("a sample is due");
        axis.apply_sample(&request, sample, agent, &mut records, NOW + 5);
    }

    #[test]
    fn an_unstamped_owner_is_released_when_the_shell_takes_the_terminal_back() {
        let agent = owned("claude", AgentLifecycle::Working);
        let mut axis = ProcessAxis::default();
        axis.owner_changed(&agent);

        let after = mark(&mut axis, &agent, "D", Foreground::Shell);
        assert_eq!(after.ownership, None, "#621: the shell is in front");
        assert_eq!(after.lifecycle, AgentLifecycle::Inactive);
        assert_eq!(axis.wire(&after, &ProgramStatusRecords::default()), None);
    }

    #[test]
    fn an_unstamped_owner_keeps_its_label_while_another_job_is_in_front() {
        let agent = owned("claude", AgentLifecycle::Working);
        for fg in [Foreground::Group(900), Foreground::Unknown] {
            let mut axis = ProcessAxis::default();
            axis.owner_changed(&agent);
            let after = mark(&mut axis, &agent, "D", fg);
            assert_eq!(after.ownership, agent.ownership, "{fg:?}");
            assert_eq!(after.lifecycle, AgentLifecycle::Inactive, "{fg:?}");
        }
    }

    #[test]
    fn a_manual_owner_keeps_its_label_whoever_is_in_front() {
        let agent = owned("manual", AgentLifecycle::Working);
        let mut axis = ProcessAxis::default();
        axis.owner_changed(&agent);
        assert!(!axis.is_due(), "a manual claim has no process to stamp");

        let after = mark(&mut axis, &agent, "D", Foreground::Shell);
        assert_eq!(after.ownership, agent.ownership);
    }

    #[test]
    fn a_stamped_owner_parks_its_lifecycle_when_another_group_marks() {
        let mut agent = owned("claude", AgentLifecycle::Working);
        let mut axis = stamped(&agent);

        agent = mark(&mut axis, &agent, "D", Foreground::Shell);
        assert_eq!(agent.lifecycle, AgentLifecycle::Inactive, "today's effect");
        assert!(agent.ownership.is_some(), "the owner is kept");
        assert_eq!(axis.owner.parked, Some(AgentLifecycle::Working));
        assert!(axis.is_due(), "a sample is requested");
    }

    #[test]
    fn a_parked_lifecycle_comes_back_when_the_process_runs() {
        let mut agent = owned("claude", AgentLifecycle::Working);
        let mut axis = stamped(&agent);
        agent = mark(&mut axis, &agent, "D", Foreground::Shell);
        let none = ProgramStatusRecords::default();

        sampled(
            &mut axis,
            &mut agent,
            &sample(Foreground::Shell, None, Liveness::Stopped),
        );
        assert_eq!(
            axis.wire(&agent, &none).map(|process| process.state),
            Some(ProcessState::Suspended)
        );
        assert_eq!(agent.lifecycle, AgentLifecycle::Inactive);

        agent = mark(&mut axis, &agent, "C", Foreground::Group(700));
        sampled(
            &mut axis,
            &mut agent,
            &sample(Foreground::Group(700), None, Liveness::Running),
        );
        assert_eq!(agent.lifecycle, AgentLifecycle::Working, "restored");
        assert_eq!(
            axis.wire(&agent, &none)
                .map(|process| (process.state, process.since)),
            Some((ProcessState::Running, NOW + 5))
        );
    }

    #[test]
    fn a_claim_between_the_park_and_the_sample_wins() {
        let working = owned("claude", AgentLifecycle::Working);
        let mut axis = stamped(&working);
        mark(&mut axis, &working, "D", Foreground::Shell);
        let mut none = ProgramStatusRecords::default();
        let stale = axis
            .take_request(1, &none, &working)
            .expect("the park asked for a sample");

        // A new Claude's SessionStart: inactive, under another session.
        let mut agent = owned("claude", AgentLifecycle::Inactive);
        agent.ownership.as_mut().unwrap().session_id = "s2".into();
        axis.owner_changed(&agent);
        axis.apply_sample(
            &stale,
            &sample(Foreground::Group(700), None, Liveness::Gone),
            &mut agent,
            &mut none,
            NOW,
        );
        assert!(
            agent.ownership.is_some(),
            "the old owner's END misses the new one"
        );

        // The new owner's own stamp, then a sample that finds it running:
        // the old owner's parked Working must not come back on it.
        sampled(
            &mut axis,
            &mut agent,
            &sample(Foreground::Group(700), Some(stamp()), Liveness::Running),
        );
        axis.request_sample();
        sampled(
            &mut axis,
            &mut agent,
            &sample(Foreground::Group(700), None, Liveness::Running),
        );
        assert_eq!(agent.lifecycle, AgentLifecycle::Inactive);
    }

    /// A sample read for an earlier owner never lands on a later one,
    /// whatever order samples arrive in: here the same Claude, stopped
    /// when the stale sample was read, has since run again and moved to
    /// another session, and its new stamp is the same process.
    #[test]
    fn a_sample_read_for_an_earlier_owner_never_lands_on_a_later_one() {
        let mut agent = owned("claude", AgentLifecycle::Working);
        let mut axis = stamped(&agent);
        let mut none = ProgramStatusRecords::default();
        axis.request_sample();
        let stale = axis
            .take_request(1, &none, &agent)
            .expect("a sample is due");

        agent.ownership.as_mut().unwrap().session_id = "s2".into();
        axis.owner_changed(&agent);
        sampled(
            &mut axis,
            &mut agent,
            &sample(Foreground::Group(700), Some(stamp()), Liveness::Running),
        );
        axis.apply_sample(
            &stale,
            &sample(Foreground::Shell, None, Liveness::Stopped),
            &mut agent,
            &mut none,
            NOW,
        );
        assert_eq!(agent.lifecycle, AgentLifecycle::Working);
        assert_eq!(
            axis.wire(&agent, &none).map(|process| process.state),
            Some(ProcessState::Running)
        );
    }

    #[test]
    fn a_gone_process_ends_the_owner_only_once_its_group_left_the_front() {
        let mut agent = owned("claude", AgentLifecycle::Working);
        let mut axis = stamped(&agent);

        axis.request_sample();
        sampled(
            &mut axis,
            &mut agent,
            &sample(Foreground::Group(700), None, Liveness::Gone),
        );
        assert!(
            agent.ownership.is_some(),
            "a pipeline mate still has the terminal"
        );

        agent = mark(&mut axis, &agent, "D", Foreground::Shell);
        sampled(
            &mut axis,
            &mut agent,
            &sample(Foreground::Shell, None, Liveness::Gone),
        );
        assert_eq!(agent.ownership, None);
        assert_eq!(axis.wire(&agent, &ProgramStatusRecords::default()), None);
        assert!(!axis.is_due(), "nothing left to sample");
    }

    #[test]
    fn a_mark_from_the_stamped_group_does_what_it_always_did() {
        let mut agent = owned("claude", AgentLifecycle::Working);
        let mut axis = stamped(&agent);

        agent = mark(&mut axis, &agent, "A", Foreground::Group(700));
        assert_eq!(agent.lifecycle, AgentLifecycle::Inactive);
        assert_eq!(axis.owner.parked, None, "Claude's own Screen Reader mark");
        assert!(!axis.is_due());
    }

    #[test]
    fn a_sample_from_another_spawn_forgets_the_stamp() {
        let mut agent = owned("claude", AgentLifecycle::Working);
        let mut axis = stamped(&agent);
        axis.request_sample();
        let mut replaced = sample(Foreground::Shell, None, Liveness::Gone);
        replaced.incarnation = 2;
        sampled(&mut axis, &mut agent, &replaced);
        assert!(agent.ownership.is_some());
        assert_eq!(axis.wire(&agent, &ProgramStatusRecords::default()), None);
    }

    // The records layer (#519).

    fn report(id: &str, app: &str, state: ProgramStatusState) -> StatusEvent {
        StatusEvent::Report(ProgramStatusReport {
            state,
            kind: None,
            progress: None,
            id: id.into(),
            app: app.into(),
            title: String::new(),
            msg: String::new(),
        })
    }

    /// A tab's records, hook state and process layer, moved the way the
    /// workspace moves them.
    struct Tab {
        agent: AgentTabState,
        records: ProgramStatusRecords,
        axis: ProcessAxis,
        /// Every banner raised, from a batch or a sample's end.
        banners: Vec<Banner>,
    }

    impl Tab {
        fn new() -> Self {
            Self {
                agent: AgentTabState {
                    shell: ShellState::ForegroundProcess,
                    ..AgentTabState::default()
                },
                records: ProgramStatusRecords::default(),
                axis: ProcessAxis::default(),
                banners: Vec::new(),
            }
        }

        fn feed(&mut self, events: impl IntoIterator<Item = StatusEvent>, fg: Foreground) {
            let applied =
                self.records
                    .apply_with_process(&mut self.agent, &mut self.axis, fg, events, T_MS);
            self.banners.extend(applied.banners);
        }

        fn mark(&mut self, mark: PromptMark, fg: Foreground) {
            self.feed([StatusEvent::Prompt(mark)], fg);
        }

        fn request(&mut self) -> SampleRequest {
            self.axis
                .take_request(1, &self.records, &self.agent)
                .expect("a sample is due")
        }

        fn apply(&mut self, request: &SampleRequest, sample: &ProcessSample) -> Ended {
            let ended = self.axis.apply_sample(
                request,
                sample,
                &mut self.agent,
                &mut self.records,
                NOW + 5,
            );
            self.banners.extend(ended.banner.clone());
            ended
        }

        fn sample(&mut self, sample: &ProcessSample) -> Ended {
            let request = self.request();
            self.apply(&request, sample)
        }

        fn shows(&self) -> AgentLifecycle {
            crate::program_status::compose(&self.agent, &self.records).lifecycle
        }

        fn process(&self) -> Option<TabProcess> {
            self.axis.wire(&self.agent, &self.records)
        }
    }

    /// A Codex working in front of the shell, its records stamped.
    fn stamped_codex() -> Tab {
        let mut tab = Tab::new();
        tab.feed(
            [report("", "codex", ProgramStatusState::Working)],
            Foreground::Unknown,
        );
        assert!(tab.axis.is_due(), "records that decide want a stamp");
        tab.sample(&sample(
            Foreground::Group(700),
            Some(stamp()),
            Liveness::Running,
        ));
        let process = tab.process().expect("the records' process");
        assert_eq!(
            (process.agent.as_str(), process.pid, process.state),
            ("codex", 700, ProcessState::Running)
        );
        tab
    }

    #[test]
    fn records_that_decide_are_stamped_and_stop_being_followed_when_they_stop_deciding() {
        let mut tab = stamped_codex();
        assert!(tab.axis.ticks());

        tab.mark(
            PromptMark::CommandEnd { exit_code: None },
            Foreground::Group(700),
        );
        assert_eq!(tab.shows(), AgentLifecycle::Inactive, "its own mark drops");
        assert_eq!(tab.process(), None);
        assert!(!tab.axis.ticks());
    }

    #[test]
    fn records_with_nothing_to_stamp_are_not_asked_about_again() {
        let mut tab = Tab::new();
        tab.feed(
            [report("", "codex", ProgramStatusState::Working)],
            Foreground::Unknown,
        );
        tab.sample(&sample(Foreground::Shell, None, Liveness::Running));
        assert_eq!(tab.process(), None);

        tab.feed(
            [report("", "codex", ProgramStatusState::Blocked)],
            Foreground::Unknown,
        );
        assert!(!tab.axis.is_due(), "one try per run of deciding records");
        assert!(!tab.axis.request_sample());
    }

    /// Ctrl-Z: the shell's `D` takes the live records aside rather than
    /// dropping them, the sample shows the program suspended under its
    /// app, and once it runs again they come back, raising nothing.
    #[test]
    fn parked_records_come_back_when_their_process_runs() {
        let mut tab = stamped_codex();
        tab.mark(
            PromptMark::CommandEnd { exit_code: None },
            Foreground::Shell,
        );
        assert_eq!(tab.shows(), AgentLifecycle::Inactive, "today's D");
        assert!(!tab.records.is_authoritative(), "nothing mutes the prompt");

        tab.sample(&sample(Foreground::Shell, None, Liveness::Stopped));
        let process = tab.process().expect("still stamped");
        assert_eq!(
            (process.state, process.agent.as_str()),
            (ProcessState::Suspended, "codex")
        );

        tab.mark(PromptMark::OutputStart, Foreground::Group(700));
        assert!(tab.axis.is_due(), "a C on a stamped tab asks");
        tab.sample(&sample(Foreground::Group(700), None, Liveness::Running));
        assert_eq!(tab.shows(), AgentLifecycle::Working, "restored");
        assert_eq!(
            tab.process().map(|process| process.state),
            Some(ProcessState::Running)
        );
    }

    /// An agent idle at its own prompt still decides the tab (its root has
    /// had no prompt mark), and that is what a Ctrl-Z most often stops:
    /// the root's say comes back with it.
    #[test]
    fn an_idle_roots_say_is_parked_and_comes_back() {
        let mut tab = Tab::new();
        tab.mark(PromptMark::OutputStart, Foreground::Unknown);
        tab.feed(
            [report("", "claude-code", ProgramStatusState::Idle)],
            Foreground::Unknown,
        );
        assert!(tab.records.is_authoritative());
        tab.sample(&sample(
            Foreground::Group(700),
            Some(stamp()),
            Liveness::Running,
        ));

        tab.mark(
            PromptMark::CommandEnd { exit_code: None },
            Foreground::Shell,
        );
        assert!(!tab.records.is_authoritative());
        tab.sample(&sample(Foreground::Shell, None, Liveness::Stopped));
        assert_eq!(
            tab.process().map(|process| (process.state, process.agent)),
            Some((ProcessState::Suspended, "claude-code".to_owned()))
        );

        // `ls` at the prompt, then `fg`.
        tab.mark(PromptMark::OutputStart, Foreground::Group(900));
        tab.mark(
            PromptMark::CommandEnd { exit_code: None },
            Foreground::Shell,
        );
        tab.mark(PromptMark::OutputStart, Foreground::Group(700));
        tab.sample(&sample(Foreground::Group(700), None, Liveness::Running));
        assert!(tab.records.is_authoritative(), "the root decides again");
    }

    /// Codex clears its own status as it suspends, in the read that also
    /// carries the shell's `D`. Nothing is set aside, and the layer ends
    /// exactly as it does when the clear comes a read earlier: the result
    /// never depends on how the bytes were split (DL-29).
    #[test]
    fn a_program_that_clears_as_it_stops_parks_nothing_however_the_read_splits() {
        for split in [false, true] {
            let mut tab = stamped_codex();
            let clear = report("", "codex", ProgramStatusState::Clear);
            let end = StatusEvent::Prompt(PromptMark::CommandEnd { exit_code: None });
            if split {
                tab.feed([clear], Foreground::Unknown);
                tab.feed([end], Foreground::Shell);
            } else {
                tab.feed([clear, end], Foreground::Shell);
            }
            assert_eq!(tab.process(), None, "split: {split}");
            assert!(!tab.axis.ticks(), "split: {split}");
        }
    }

    /// A root another program reported while the stamped one was stopped
    /// says the tab moved on: nothing parked comes back, neither a live
    /// child nor the root's say, and pi's finished root stays as its exit
    /// left it.
    #[test]
    fn parked_records_stay_away_once_the_root_was_reported_since() {
        let mut tab = stamped_codex();
        tab.feed(
            [report("worker", "", ProgramStatusState::Working)],
            Foreground::Unknown,
        );
        tab.mark(
            PromptMark::CommandEnd { exit_code: None },
            Foreground::Shell,
        );
        tab.mark(PromptMark::OutputStart, Foreground::Group(900));
        tab.feed(
            [report("", "pi", ProgramStatusState::Done)],
            Foreground::Unknown,
        );
        tab.mark(
            PromptMark::CommandEnd { exit_code: None },
            Foreground::Shell,
        );
        tab.sample(&sample(Foreground::Group(700), None, Liveness::Running));
        assert!(!tab.records.is_authoritative(), "nothing came back");
        let composed = crate::program_status::compose(&tab.agent, &tab.records);
        assert_eq!(
            (composed.lifecycle, composed.decided_by),
            (
                AgentLifecycle::Finished,
                crate::program_status::DecidedBy::KeptRecord
            ),
            "pi's root, kept as its exit left it"
        );
    }

    /// In a shell with no prompt marks, a Ctrl-Z prints nothing: the
    /// sample that finds the program stopped takes its records aside, so a
    /// suspended agent mutes nothing; `fg` prints nothing either, and the
    /// sample that finds it running brings them back.
    #[test]
    fn without_marks_a_stopped_programs_records_are_parked_by_the_sample() {
        let mut tab = stamped_codex();
        tab.axis.request_tick();
        tab.sample(&sample(Foreground::Shell, None, Liveness::Stopped));
        assert!(!tab.records.is_authoritative());
        assert_eq!(
            tab.process().map(|process| process.state),
            Some(ProcessState::Suspended)
        );

        tab.axis.request_tick();
        tab.sample(&sample(Foreground::Group(700), None, Liveness::Running));
        assert_eq!(tab.shows(), AgentLifecycle::Working);
    }

    /// U10: a gone program's records end like a prompt. With the shell
    /// back in front every live record is its; with another job in front
    /// (a `while true; do codex; done` restart) a record reported after
    /// the sample was asked for belongs to the job in front, which is
    /// stamped next.
    #[test]
    fn an_end_takes_every_live_record_with_the_shell_in_front_and_only_older_ones_otherwise() {
        let mut tab = stamped_codex();
        tab.feed(
            [report("sub", "", ProgramStatusState::Blocked)],
            Foreground::Unknown,
        );
        tab.axis.request_tick();
        let ended = tab.sample(&sample(Foreground::Shell, None, Liveness::Gone));
        assert!(ended.records);
        assert!(tab.records.is_empty(), "every live record was its");
        assert_eq!(tab.process(), None);
        assert!(!tab.axis.is_due());

        let mut tab = stamped_codex();
        tab.axis.request_tick();
        let request = tab.request();
        tab.feed(
            [report("", "codex", ProgramStatusState::Working)],
            Foreground::Unknown,
        );
        tab.apply(
            &request,
            &sample(Foreground::Group(800), None, Liveness::Gone),
        );
        assert_eq!(tab.shows(), AgentLifecycle::Working, "the new Codex's");
        assert_eq!(tab.process(), None, "not the gone one's stamp");
        assert!(tab.axis.is_due(), "the new one is stamped next");
    }

    /// U11: a kept root `done` survives its program's end (the 7501 spec:
    /// `done` and `error` outlive the program), and loses its say over the
    /// tab, as a prompt mark leaves it.
    #[test]
    fn a_kept_root_done_survives_an_end() {
        let mut tab = Tab::new();
        tab.feed(
            [
                report("", "codex", ProgramStatusState::Done),
                report("worker", "", ProgramStatusState::Working),
            ],
            Foreground::Unknown,
        );
        tab.sample(&sample(
            Foreground::Group(700),
            Some(stamp()),
            Liveness::Running,
        ));
        tab.axis.request_tick();
        tab.sample(&sample(Foreground::Shell, None, Liveness::Gone));
        assert!(!tab.records.is_authoritative(), "root_prompted is set");
        let composed = crate::program_status::compose(&tab.agent, &tab.records);
        assert_eq!(
            (composed.lifecycle, composed.decided_by),
            (
                AgentLifecycle::Finished,
                crate::program_status::DecidedBy::KeptRecord
            )
        );
    }

    /// An unknown liveness, or the stamped group still in front, keeps
    /// the records: only gone *and* out of front ends them.
    #[test]
    fn an_uncertain_sample_keeps_the_records() {
        for (fg, liveness) in [
            (Foreground::Shell, Liveness::Unknown),
            (Foreground::Group(700), Liveness::Gone),
            (Foreground::Unknown, Liveness::Gone),
        ] {
            let mut tab = stamped_codex();
            tab.axis.request_tick();
            tab.sample(&sample(fg, None, liveness));
            assert_eq!(tab.shows(), AgentLifecycle::Working, "{fg:?} {liveness:?}");
        }
    }

    /// U13: the tick reads only what holds a stamp.
    #[test]
    fn only_a_stamped_layer_ticks() {
        let mut tab = Tab::new();
        assert!(!tab.axis.request_tick());
        tab.feed(
            [report("", "codex", ProgramStatusState::Working)],
            Foreground::Unknown,
        );
        assert!(!tab.axis.ticks(), "wanted is not stamped");
        tab.sample(&sample(Foreground::Shell, None, Liveness::Running));
        assert!(
            !tab.axis.request_tick(),
            "nothing to stamp, nothing to tick"
        );

        let mut tab = stamped_codex();
        assert!(tab.axis.request_tick());
        assert!(tab.axis.is_due());
    }

    /// A sample read for records that have since stopped deciding and
    /// started over never lands on the new run, even one stamped with the
    /// same process: here the stale read found it gone.
    #[test]
    fn a_sample_read_for_earlier_records_never_lands_on_later_ones() {
        let mut tab = stamped_codex();
        tab.axis.request_tick();
        let stale = tab.request();
        tab.mark(
            PromptMark::CommandEnd { exit_code: None },
            Foreground::Group(700),
        );
        tab.mark(PromptMark::OutputStart, Foreground::Unknown);
        tab.feed(
            [report("", "codex", ProgramStatusState::Working)],
            Foreground::Unknown,
        );
        tab.sample(&sample(
            Foreground::Group(700),
            Some(stamp()),
            Liveness::Running,
        ));
        assert!(tab.process().is_some(), "the precondition: stamped again");

        tab.apply(&stale, &sample(Foreground::Shell, None, Liveness::Gone));
        assert_eq!(tab.shows(), AgentLifecycle::Working);
        assert!(tab.process().is_some());
    }

    // Exit codes and the exit banner (commit 3).

    fn end(exit_code: i32) -> PromptMark {
        PromptMark::CommandEnd {
            exit_code: Some(exit_code),
        }
    }

    fn exit_banner(title: &str, body: &str) -> Banner {
        Banner {
            title: title.into(),
            body: body.into(),
        }
    }

    /// U18: a `D`'s code reads as an exit up to 128, and above it as the
    /// death by signal `code - 128` the shell reports, named as this
    /// machine numbers its signals.
    #[test]
    fn an_exit_code_reads_as_an_exit_or_a_death_by_signal() {
        assert_eq!(exit_body(0), None);
        for (code, body) in [
            (1, "exited (1)".to_owned()),
            (3, "exited (3)".to_owned()),
            (128, "exited (128)".to_owned()),
            (137, "killed (SIGKILL)".to_owned()),
            (143, "killed (SIGTERM)".to_owned()),
            (128 + libc::SIGBUS, "killed (SIGBUS)".to_owned()),
            (128 + libc::SIGTSTP, "killed (SIGTSTP)".to_owned()),
            (200, "killed (signal 72)".to_owned()),
            (-1, "exited (-1)".to_owned()),
            (256, "exited (256)".to_owned()),
        ] {
            assert_eq!(exit_body(code), Some(body), "{code}");
        }
        let mut numbers: Vec<i32> = (1..=127).filter(|&n| signal_name(n).is_some()).collect();
        let named = numbers.len();
        numbers.dedup();
        assert_eq!(numbers.len(), named, "one name per number");
    }

    /// U19: the shell takes the terminal back from a killed agent before
    /// it prints its `D`, so a sample can find the process gone on either
    /// side of that `D`. Either way the end raises the one banner the
    /// `D`'s code calls for.
    #[test]
    fn the_d_before_or_after_the_end_raises_the_same_banner() {
        let mut d_first = stamped_codex();
        d_first.mark(end(137), Foreground::Shell);
        assert!(d_first.banners.is_empty(), "the D alone ends nothing");
        let ended = d_first.sample(&sample(Foreground::Shell, None, Liveness::Gone));
        assert!(ended.records);

        let mut sample_first = stamped_codex();
        sample_first.axis.request_tick();
        let ended = sample_first.sample(&sample(Foreground::Shell, None, Liveness::Gone));
        assert!(ended.records);
        assert!(sample_first.banners.is_empty(), "the code is the D's");
        sample_first.mark(end(137), Foreground::Shell);

        for tab in [&d_first, &sample_first] {
            assert_eq!(tab.banners, [exit_banner("Codex", "killed (SIGKILL)")]);
        }
        sample_first.mark(end(1), Foreground::Shell);
        assert_eq!(sample_first.banners.len(), 1, "once per end");
    }

    #[test]
    fn a_success_or_an_idle_program_raises_no_banner() {
        let mut success = stamped_codex();
        success.mark(end(0), Foreground::Shell);
        success.sample(&sample(Foreground::Shell, None, Liveness::Gone));
        assert!(success.banners.is_empty());

        // An idle 7501 agent at its own prompt is stamped too: its root
        // decides the tab, but nothing was running.
        let mut idle = Tab::new();
        idle.feed(
            [report("", "claude-code", ProgramStatusState::Idle)],
            Foreground::Unknown,
        );
        idle.sample(&sample(
            Foreground::Group(700),
            Some(stamp()),
            Liveness::Running,
        ));
        idle.mark(end(137), Foreground::Shell);
        let ended = idle.sample(&sample(Foreground::Shell, None, Liveness::Gone));
        assert!(ended.records);
        assert!(idle.banners.is_empty());
    }

    /// The banner owed to the next `D` is that `D`'s alone: a `C` in
    /// between starts another command, whose `D` says nothing about the
    /// gone process, and a shell that has printed no mark yet may never
    /// print one.
    #[test]
    fn an_owed_banner_is_dropped_by_a_c_or_never_owed_without_marks() {
        let mut tab = stamped_codex();
        tab.axis.request_tick();
        tab.sample(&sample(Foreground::Shell, None, Liveness::Gone));
        tab.mark(PromptMark::OutputStart, Foreground::Unknown);
        tab.mark(end(137), Foreground::Shell);
        assert!(tab.banners.is_empty());

        let mut no_marks = stamped_codex();
        no_marks.agent.shell = ShellState::Unknown;
        no_marks.axis.request_tick();
        no_marks.sample(&sample(Foreground::Shell, None, Liveness::Gone));
        assert_eq!(no_marks.axis.exit_pending, None);
    }

    /// A suspend's `D` carries 128 + `SIGTSTP`, which is not an exit: the
    /// sample that finds the program stopped drops it, so killing the
    /// stopped program later (`kill -9 %1`) raises nothing, whatever
    /// code the `kill` command's own `D` has.
    #[test]
    fn a_suspend_never_banners() {
        let mut tab = stamped_codex();
        tab.mark(end(128 + libc::SIGTSTP), Foreground::Shell);
        tab.sample(&sample(Foreground::Shell, None, Liveness::Stopped));
        assert_eq!(
            tab.process().map(|process| process.state),
            Some(ProcessState::Suspended)
        );

        tab.mark(PromptMark::OutputStart, Foreground::Unknown);
        let ended = tab.sample(&sample(Foreground::Shell, None, Liveness::Gone));
        assert!(ended.records);
        tab.mark(end(0), Foreground::Shell);
        assert!(tab.banners.is_empty(), "{:?}", tab.banners);
    }

    /// A program running in the background (`bg`) never had the terminal
    /// taken from it by the shell's next `D`, so that `D`'s code is the
    /// command's at the prompt, not the program's: an end then raises
    /// nothing, whether its sample comes after that `D` or before it.
    #[test]
    fn a_backgrounded_programs_end_takes_no_later_commands_code() {
        let backgrounded = || {
            let mut tab = stamped_codex();
            tab.mark(end(128 + libc::SIGTSTP), Foreground::Shell);
            tab.sample(&sample(Foreground::Shell, None, Liveness::Stopped));
            // `bg`, then its own `D`.
            tab.mark(PromptMark::OutputStart, Foreground::Unknown);
            tab.sample(&sample(Foreground::Shell, None, Liveness::Running));
            tab.mark(end(0), Foreground::Shell);
            tab.sample(&sample(Foreground::Shell, None, Liveness::Running));
            assert_eq!(tab.shows(), AgentLifecycle::Working, "the precondition");
            tab
        };

        // `false` at the prompt, as the program dies.
        let mut d_first = backgrounded();
        d_first.mark(PromptMark::OutputStart, Foreground::Unknown);
        d_first.mark(end(1), Foreground::Shell);
        let ended = d_first.sample(&sample(Foreground::Shell, None, Liveness::Gone));
        assert!(ended.records);

        // The program dies at the prompt, then `false`, in a bash older
        // than 4.4 (macOS's `/bin/bash`), which prints no `C` before it.
        let mut sample_first = backgrounded();
        sample_first.axis.request_tick();
        let ended = sample_first.sample(&sample(Foreground::Shell, None, Liveness::Gone));
        assert!(ended.records);
        sample_first.mark(end(1), Foreground::Shell);

        for tab in [&d_first, &sample_first] {
            assert!(tab.banners.is_empty(), "{:?}", tab.banners);
        }
    }

    // The identity layer: an agent named by its process alone.

    /// A sample whose leader in front is `agent`, by its process's name.
    fn named(
        foreground: Foreground,
        agent: Option<&'static str>,
        liveness: Liveness,
    ) -> ProcessSample {
        ProcessSample {
            incarnation: 1,
            foreground,
            leader: Some(Leader {
                stamp: stamp(),
                agent,
            }),
            liveness: vec![(stamp(), liveness)],
        }
    }

    impl Tab {
        fn look(&mut self) -> bool {
            self.axis.request_look(&self.agent)
        }
    }

    /// A tab nobody owns, with cursor-agent in front: looked at, named.
    fn named_cursor() -> Tab {
        let mut tab = Tab::new();
        assert!(tab.look());
        tab.sample(&named(
            Foreground::Group(700),
            Some("cursor"),
            Liveness::Running,
        ));
        let process = tab.process().expect("named");
        assert_eq!(
            (process.agent.as_str(), process.pid, process.state),
            ("cursor", 700, ProcessState::Running)
        );
        tab
    }

    /// U22, in the axis: nothing reads a tab no layer follows until
    /// someone looks at it — not the tick — and a look reads the leader
    /// in front, to name.
    #[test]
    fn an_agent_in_front_is_named_when_someone_looks_and_not_before() {
        let mut tab = Tab::new();
        assert!(!tab.axis.request_tick(), "the tick never looks");
        assert!(!tab.axis.is_due());

        assert!(tab.look());
        let request = tab.request();
        assert!(request.discover && request.leader_wanted);
        assert!(request.stamps.is_empty());
        tab.apply(
            &request,
            &named(Foreground::Group(700), Some("cursor"), Liveness::Running),
        );
        assert_eq!(
            tab.process().map(|process| process.agent),
            Some("cursor".to_owned())
        );
        assert!(tab.axis.ticks(), "a named agent is stamped, so ticked");
        assert_eq!(
            tab.agent.lifecycle,
            AgentLifecycle::Inactive,
            "never a lifecycle"
        );
    }

    /// A named agent stops, runs again and ends with its process, as any
    /// stamp does — and raises no banner, holding nothing it was doing.
    #[test]
    fn a_named_agent_suspends_runs_again_and_ends_with_its_process() {
        let mut tab = named_cursor();
        let state = |tab: &Tab| tab.process().map(|process| process.state);

        tab.mark(end(128 + libc::SIGTSTP), Foreground::Shell);
        assert!(tab.axis.is_due(), "a mark with the shell in front asks");
        tab.sample(&sample(Foreground::Shell, None, Liveness::Stopped));
        assert_eq!(state(&tab), Some(ProcessState::Suspended));

        tab.mark(PromptMark::OutputStart, Foreground::Unknown);
        tab.sample(&sample(Foreground::Group(700), None, Liveness::Running));
        assert_eq!(state(&tab), Some(ProcessState::Running));

        tab.mark(end(137), Foreground::Shell);
        let ended = tab.sample(&sample(Foreground::Shell, None, Liveness::Gone));
        assert!(ended.identity);
        assert_eq!(tab.process(), None);
        assert!(!tab.axis.ticks());
        assert!(tab.banners.is_empty(), "{:?}", tab.banners);
    }

    /// A mark from the named agent's own group is its own, and asks for
    /// nothing; a gone process whose group is still in front keeps.
    #[test]
    fn a_named_agent_keeps_while_its_group_has_the_terminal() {
        let mut tab = named_cursor();
        tab.mark(end(0), Foreground::Group(700));
        assert!(!tab.axis.is_due());
        tab.axis.request_tick();
        tab.sample(&sample(Foreground::Group(700), None, Liveness::Gone));
        assert!(tab.process().is_some());
    }

    /// An owner, records that decide, or anything already followed name
    /// the tab: a look there names nothing, and the owner's own name
    /// stands over its process's.
    #[test]
    fn a_look_names_nothing_on_a_tab_something_else_names() {
        let mut claimed = Tab::new();
        claimed.agent = owned("claude", AgentLifecycle::Working);
        claimed.axis.owner_changed(&claimed.agent);
        assert!(claimed.look(), "the owner's wanted stamp");
        let request = claimed.request();
        assert!(!request.discover);
        claimed.apply(
            &request,
            &named(Foreground::Group(700), Some("codex"), Liveness::Running),
        );
        assert_eq!(
            claimed.process().map(|process| process.agent),
            Some("claude".to_owned()),
            "the owner's source, not its process's name"
        );

        let mut manual = Tab::new();
        manual.agent = owned("manual", AgentLifecycle::Working);
        manual.axis.owner_changed(&manual.agent);
        assert!(!manual.look(), "an owner of any kind names the tab");

        let mut reporting = Tab::new();
        reporting.feed(
            [report("", "codex", ProgramStatusState::Working)],
            Foreground::Unknown,
        );
        reporting.look();
        assert!(!reporting.request().discover);
    }

    /// A claim, or records that start deciding, take over from a named
    /// agent: it is no longer the one naming the tab, whichever lands
    /// first — before the look's sample, or after it.
    #[test]
    fn a_claim_or_a_record_takes_over_from_a_named_agent() {
        let mut tab = named_cursor();
        tab.agent = owned("cursor", AgentLifecycle::Working);
        tab.axis.owner_changed(&tab.agent);
        assert_eq!(tab.process(), None, "the owner waits for its own stamp");
        tab.sample(&sample(
            Foreground::Group(700),
            Some(stamp()),
            Liveness::Running,
        ));
        assert_eq!(
            tab.process().map(|process| process.agent),
            Some("cursor".to_owned())
        );
        assert!(tab.axis.identity.stamp().is_none());

        let mut tab = named_cursor();
        tab.feed(
            [report("", "codex", ProgramStatusState::Working)],
            Foreground::Unknown,
        );
        assert!(tab.axis.identity.stamp().is_none());

        let mut tab = Tab::new();
        tab.look();
        let stale = tab.request();
        tab.agent = owned("claude", AgentLifecycle::Inactive);
        tab.axis.owner_changed(&tab.agent);
        tab.apply(
            &stale,
            &named(Foreground::Group(700), Some("claude"), Liveness::Running),
        );
        assert!(
            tab.axis.identity.stamp().is_none(),
            "a claim between the look and its sample wins"
        );
    }

    /// A look that finds no agent in front — the shell, or a program the
    /// table does not name — stamps nothing, and the next look reads
    /// again.
    #[test]
    fn a_look_that_names_nothing_stamps_nothing() {
        let mut tab = Tab::new();
        for found in [
            sample(Foreground::Shell, None, Liveness::Running),
            named(Foreground::Group(700), None, Liveness::Running),
        ] {
            assert!(tab.look());
            tab.sample(&found);
            assert_eq!(tab.process(), None);
            assert!(!tab.axis.ticks());
        }
    }

    /// A command starting is someone looking: the `C` asks.
    #[test]
    fn a_c_mark_looks_for_an_agent() {
        let mut tab = Tab::new();
        tab.mark(PromptMark::OutputStart, Foreground::Unknown);
        assert!(tab.request().discover);
    }

    /// A stamped layer the records' `app` or the owner's `source` leave
    /// unnamed takes its process's name.
    #[test]
    fn an_unnamed_layer_takes_its_processs_name() {
        for (app, want) in [("", "codex"), ("claude-code", "claude-code")] {
            let mut tab = Tab::new();
            tab.feed(
                [report("", app, ProgramStatusState::Working)],
                Foreground::Unknown,
            );
            tab.sample(&named(
                Foreground::Group(700),
                Some("codex"),
                Liveness::Running,
            ));
            assert_eq!(
                tab.process().map(|process| process.agent),
                Some(want.to_owned()),
                "app {app:?}"
            );
        }
    }

    /// U21: no end reads what a process is called. An owner and records
    /// stamped with no name, the right one, or a wrong one go through the
    /// same suspend, resume and end.
    #[test]
    fn an_end_never_reads_what_the_process_is_called() {
        let trace = |name: Option<&'static str>| {
            let mut tab = Tab::new();
            tab.agent = owned("claude", AgentLifecycle::Working);
            tab.axis.owner_changed(&tab.agent);
            tab.feed(
                [report("", "claude-code", ProgramStatusState::Working)],
                Foreground::Group(700),
            );
            tab.sample(&named(Foreground::Group(700), name, Liveness::Running));
            let mut seen = Vec::new();
            let mut step = |tab: &mut Tab, ended: Option<Ended>| {
                seen.push((
                    tab.shows(),
                    tab.agent.ownership.is_some(),
                    tab.process().map(|process| process.state),
                    ended.map(|ended| (ended.owner, ended.records, ended.banner)),
                ));
            };
            step(&mut tab, None);
            tab.mark(end(128 + libc::SIGTSTP), Foreground::Shell);
            let ended = tab.sample(&sample(Foreground::Shell, None, Liveness::Stopped));
            step(&mut tab, Some(ended));
            tab.mark(PromptMark::OutputStart, Foreground::Group(700));
            let ended = tab.sample(&sample(Foreground::Group(700), None, Liveness::Running));
            step(&mut tab, Some(ended));
            tab.mark(end(137), Foreground::Shell);
            let ended = tab.sample(&sample(Foreground::Shell, None, Liveness::Gone));
            step(&mut tab, Some(ended));
            seen
        };
        let unnamed = trace(None);
        assert!(
            unnamed
                .last()
                .is_some_and(|(_, owned, process, ended)| !owned
                    && process.is_none()
                    && ended
                        .as_ref()
                        .is_some_and(|(owner, records, _)| *owner && *records)),
            "the precondition: it ends: {unnamed:?}"
        );
        assert_eq!(trace(Some("claude")), unnamed);
        assert_eq!(trace(Some("codex")), unnamed);
    }

    /// A Codex working in front, its records stamped with a process its
    /// name says is Codex.
    fn named_codex() -> Tab {
        let mut tab = Tab::new();
        tab.feed(
            [report("", "codex", ProgramStatusState::Working)],
            Foreground::Unknown,
        );
        tab.sample(&named(
            Foreground::Group(700),
            Some("codex"),
            Liveness::Running,
        ));
        assert_eq!(
            tab.process().map(|process| process.agent),
            Some("codex".to_owned())
        );
        tab
    }

    /// Codex clears its own status, in the read before the shell's `D` or
    /// in the `D`'s own; the `D` is the shell taking the terminal back.
    fn clear_then_d(tab: &mut Tab, code: i32, split: bool) {
        let clear = report("", "codex", ProgramStatusState::Clear);
        let d = StatusEvent::Prompt(end(code));
        if split {
            tab.feed([clear], Foreground::Unknown);
            tab.feed([d], Foreground::Shell);
        } else {
            tab.feed([clear, d], Foreground::Shell);
        }
    }

    /// Codex clears its status as it stops, so its `Ctrl-Z` leaves its
    /// records nothing to park, but its process's name still names it:
    /// the stamp passes to the identity layer, and once the read the `D`
    /// asks for finds it stopped, the tab reads `codex` suspended, however
    /// the bytes were split. `fg` brings it back running, and its next
    /// report hands the tab back to its records.
    #[test]
    fn a_named_program_that_clears_as_it_stops_reads_suspended_however_the_read_splits() {
        for split in [false, true] {
            let mut tab = named_codex();
            clear_then_d(&mut tab, 128 + libc::SIGTSTP, split);
            assert_eq!(tab.process(), None, "unshown until read; split: {split}");
            assert!(tab.axis.is_due(), "split: {split}");
            tab.sample(&sample(Foreground::Shell, None, Liveness::Stopped));
            assert_eq!(
                tab.process()
                    .map(|process| (process.agent, process.state, process.since)),
                Some(("codex".to_owned(), ProcessState::Suspended, NOW + 5)),
                "split: {split}"
            );
            assert_eq!(tab.shows(), AgentLifecycle::Inactive);
            assert!(tab.axis.ticks());

            tab.mark(PromptMark::OutputStart, Foreground::Group(700));
            tab.sample(&sample(Foreground::Group(700), None, Liveness::Running));
            assert_eq!(
                tab.process().map(|process| process.state),
                Some(ProcessState::Running)
            );
            tab.feed(
                [report("", "codex", ProgramStatusState::Working)],
                Foreground::Unknown,
            );
            assert!(tab.axis.identity.stamp().is_none(), "its records again");
            assert_eq!(tab.shows(), AgentLifecycle::Working);
        }
    }

    /// The same clear as Codex exits: the read the `D` asks for finds it
    /// gone, and the tab never showed a row for it in between.
    #[test]
    fn a_named_program_that_clears_as_it_exits_never_shows() {
        for split in [false, true] {
            let mut tab = named_codex();
            clear_then_d(&mut tab, 0, split);
            assert_eq!(tab.process(), None, "split: {split}");
            assert!(!tab.axis.ticks(), "an unshown identity is not ticked");
            let ended = tab.sample(&sample(Foreground::Shell, None, Liveness::Gone));
            assert!(ended.identity, "split: {split}");
            assert_eq!(tab.process(), None);
            assert!(tab.axis.identity.stamp().is_none());
            assert!(tab.banners.is_empty(), "{:?}", tab.banners);
        }
    }

    /// Records that left pass their stamp on only at the shell's own mark:
    /// a mark from the program's group keeps them for the next, a command
    /// starting forgets them, and an owner names the tab instead.
    #[test]
    fn records_that_left_pass_on_only_at_the_shells_mark() {
        let clear = || report("", "codex", ProgramStatusState::Clear);

        let mut own = named_codex();
        own.feed([clear()], Foreground::Unknown);
        own.mark(PromptMark::PromptStart, Foreground::Group(700));
        assert!(own.axis.identity.stamp().is_none());
        own.mark(end(128 + libc::SIGTSTP), Foreground::Shell);
        assert!(own.axis.identity.stamp().is_some());

        let mut started = named_codex();
        started.feed([clear()], Foreground::Unknown);
        started.mark(PromptMark::OutputStart, Foreground::Unknown);
        started.mark(end(0), Foreground::Shell);
        assert!(started.axis.identity.stamp().is_none());

        let mut owned_tab = named_codex();
        owned_tab.agent = owned("codex", AgentLifecycle::Working);
        owned_tab.axis.owner_changed(&owned_tab.agent);
        owned_tab.sample(&named(
            Foreground::Group(700),
            Some("codex"),
            Liveness::Running,
        ));
        clear_then_d(&mut owned_tab, 128 + libc::SIGTSTP, false);
        assert!(
            owned_tab.agent.ownership.is_some(),
            "the precondition: a stamped owner parks, and stays"
        );
        assert!(owned_tab.axis.identity.stamp().is_none());
    }
}
