//! A tab's process layer (#621, #519): which process its hook owner, and
//! the program-status records that decide it, are stamped with; what a
//! prompt mark parked while that process was not in front; and whether it
//! is stopped.
//!
//! Pure apart from the reads the sampler hands in
//! ([`crate::foreground::ProcessSample`]); what the evidence decides is
//! [`roost_ipc::process`]'s. The workspace keeps one axis per tab beside
//! its hook state and records, mutates it under the same lock, and never
//! reads a process itself: marks see only the foreground group a probe
//! read before the lock, and samples run on the sampler task.
//!
//! The two layers are followed alike, each with an epoch. Every time a
//! layer starts over — an owner claimed or released, records that start
//! or stop deciding the tab, an end — its epoch moves on and what it
//! parked is dropped, so a sample requested for one, or state parked
//! under it, can never land on the next.

use roost_ipc::agent::{AgentLifecycle, AgentTabState};
use roost_ipc::process::{
    self, Foreground, Layer, Liveness, MarkVerdict, ProcessState, SampleVerdict, Stamp, TabProcess,
};
use roost_ipc::program_status::PromptMark;

use crate::foreground::ProcessSample;
use crate::program_status::{ParkedRecords, ProgramStatusRecords};

/// One tab's process layer. Not persisted, like the hook state it follows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProcessAxis {
    /// The hook owner's process; what it parks is the lifecycle a mark
    /// dropped.
    owner: Followed<AgentLifecycle>,
    /// The process behind the program-status records while they decide
    /// the tab; what it parks is what a mark took off the records.
    records: Followed<ParkedRecords>,
    /// The sampler has something to read for this tab.
    due: bool,
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
    }

    fn has_a_sample_to_take(&self) -> bool {
        matches!(self.follow, Follow::Wanted | Follow::Stamped(_))
    }

    fn set_suspended(&mut self, suspended: bool, now: i64) {
        if std::mem::replace(&mut self.suspended, suspended) != suspended {
            self.since = now;
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
                self.follow = sample.leader.map_or(Follow::Unstamped, Follow::Stamped);
                self.incarnation = sample.incarnation;
                self.since = now;
                None
            }
            Follow::Stamped(_) if sample.incarnation != self.incarnation => {
                self.follow = Follow::Unstamped;
                self.parked = None;
                self.suspended = false;
                None
            }
            Follow::Stamped(stamp) => {
                let liveness = sample.liveness_of(&stamp).unwrap_or(Liveness::Unknown);
                Some(process::on_sample(
                    Some(&stamp),
                    liveness,
                    sample.foreground,
                ))
            }
        }
    }

    fn wire(&self, agent: String) -> Option<TabProcess> {
        let stamp = self.stamp()?;
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
    /// The records' report count when the sample was asked for: what an
    /// end may take is what was reported by then.
    records_seq: u64,
    pub(crate) stamp_wanted: bool,
    pub(crate) stamps: Vec<Stamp>,
}

/// What a sample ended, for the workspace to log once its lock is gone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Ended {
    pub(crate) owner: bool,
    pub(crate) records: bool,
}

impl ProcessAxis {
    /// The owner changed: a claim or a release a report made, or a release
    /// this layer made. An agent's claim wants a stamp; anything else
    /// leaves the tab unstamped.
    pub(crate) fn owner_changed(&mut self, agent: &AgentTabState) {
        if process::owner_follows_a_process(agent) {
            self.owner.restart(Follow::Wanted);
            self.due = true;
        } else {
            self.owner.restart(Follow::Off);
        }
    }

    /// Ask for a sample, when there is a stamp to take or to read.
    /// Answers whether it asked.
    pub(crate) fn request_sample(&mut self) -> bool {
        if !self.owner.has_a_sample_to_take() && !self.records.has_a_sample_to_take() {
            return false;
        }
        self.due = true;
        true
    }

    /// Whether the tick reads this tab: it holds a stamp, and with it
    /// whatever it parked.
    pub(crate) fn ticks(&self) -> bool {
        self.owner.stamp().is_some() || self.records.stamp().is_some()
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
    /// deciding and the layer ended before the mark, so it ends here too.
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
                self.due = true;
            }
            (false, Follow::Wanted | Follow::Unstamped | Follow::Stamped(_)) => {
                self.records.restart(Follow::Off);
            }
            _ => {}
        }
    }

    /// An OSC 133 mark, with `before` the hook state before the mark's own
    /// effect and `agent` the state after it, read with `fg` in front.
    ///
    /// A `C` on a stamped tab asks for a sample: a `fg` is one. An `A`,
    /// `B` or `D` applies [`process::at_mark`]'s verdict to the owner.
    pub(crate) fn at_mark(
        &mut self,
        mark: PromptMark,
        before: &AgentTabState,
        agent: &mut AgentTabState,
        fg: Foreground,
    ) {
        if mark == PromptMark::OutputStart {
            if self.ticks() {
                self.due = true;
            }
            return;
        }
        if !process::owner_follows_a_process(before) {
            return;
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
    }

    /// The sample this tab is due, if any, clearing the flag.
    pub(crate) fn take_request(
        &mut self,
        tab_id: i64,
        records: &ProgramStatusRecords,
    ) -> Option<SampleRequest> {
        if !std::mem::take(&mut self.due) {
            return None;
        }
        let stamp_wanted =
            self.owner.follow == Follow::Wanted || self.records.follow == Follow::Wanted;
        let mut stamps: Vec<Stamp> = self.owner.stamp().into_iter().copied().collect();
        if let Some(stamp) = self.records.stamp().filter(|stamp| !stamps.contains(stamp)) {
            stamps.push(*stamp);
        }
        if !stamp_wanted && stamps.is_empty() {
            return None;
        }
        Some(SampleRequest {
            tab_id,
            owner_epoch: self.owner.epoch,
            records_epoch: self.records.epoch,
            records_seq: records.seq(),
            stamp_wanted,
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
                *agent = process::released(agent);
                self.owner_changed(agent);
                ended.owner = true;
            }
        }
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
                records.end(upto);
                self.records.restart(Follow::Off);
                ended.records = true;
            }
        }
        self.records_settled(records);
        ended
    }

    /// `Tab.process` for a tab whose hook state is `agent` and whose
    /// records are `records`: the records' process while they follow one,
    /// since they decide what the tab shows, else the owner's.
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
        self.owner.wire(
            agent
                .ownership
                .as_ref()
                .map(|owner| owner.source.clone())
                .unwrap_or_default(),
        )
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
            leader,
            liveness: vec![(stamp(), liveness)],
        }
    }

    /// A claim, then the sample that stamps the leader in front.
    fn stamped(agent: &AgentTabState) -> ProcessAxis {
        let mut axis = ProcessAxis::default();
        axis.owner_changed(agent);
        let request = axis
            .take_request(1, &ProgramStatusRecords::default())
            .expect("a claim asks for a stamp");
        assert!(request.stamp_wanted);
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
            _ => PromptMark::CommandEnd,
        };
        let mut after = roost_ipc::agent::apply_shell_mark(agent, body).unwrap();
        axis.at_mark(mark, agent, &mut after, fg);
        after
    }

    fn sampled(axis: &mut ProcessAxis, agent: &mut AgentTabState, sample: &ProcessSample) {
        let mut records = ProgramStatusRecords::default();
        let request = axis.take_request(1, &records).expect("a sample is due");
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
            .take_request(1, &none)
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
        let stale = axis.take_request(1, &none).expect("a sample is due");

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
            }
        }

        fn feed(&mut self, events: impl IntoIterator<Item = StatusEvent>, fg: Foreground) {
            self.records
                .apply_with_process(&mut self.agent, &mut self.axis, fg, events, T_MS);
        }

        fn mark(&mut self, mark: PromptMark, fg: Foreground) {
            self.feed([StatusEvent::Prompt(mark)], fg);
        }

        fn request(&mut self) -> SampleRequest {
            self.axis
                .take_request(1, &self.records)
                .expect("a sample is due")
        }

        fn apply(&mut self, request: &SampleRequest, sample: &ProcessSample) -> Ended {
            self.axis
                .apply_sample(request, sample, &mut self.agent, &mut self.records, NOW + 5)
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

        tab.mark(PromptMark::CommandEnd, Foreground::Group(700));
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
        tab.mark(PromptMark::CommandEnd, Foreground::Shell);
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

        tab.mark(PromptMark::CommandEnd, Foreground::Shell);
        assert!(!tab.records.is_authoritative());
        tab.sample(&sample(Foreground::Shell, None, Liveness::Stopped));
        assert_eq!(
            tab.process().map(|process| (process.state, process.agent)),
            Some((ProcessState::Suspended, "claude-code".to_owned()))
        );

        // `ls` at the prompt, then `fg`.
        tab.mark(PromptMark::OutputStart, Foreground::Group(900));
        tab.mark(PromptMark::CommandEnd, Foreground::Shell);
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
            let end = StatusEvent::Prompt(PromptMark::CommandEnd);
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
        tab.mark(PromptMark::CommandEnd, Foreground::Shell);
        tab.mark(PromptMark::OutputStart, Foreground::Group(900));
        tab.feed(
            [report("", "pi", ProgramStatusState::Done)],
            Foreground::Unknown,
        );
        tab.mark(PromptMark::CommandEnd, Foreground::Shell);
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
        tab.mark(PromptMark::CommandEnd, Foreground::Group(700));
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
}
