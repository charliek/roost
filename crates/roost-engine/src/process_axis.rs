//! A tab's process layer (#621): which process its hook owner is stamped
//! with, what a prompt mark parked while that process was not in front,
//! and whether it is stopped.
//!
//! Pure apart from the reads the sampler hands in
//! ([`crate::foreground::ProcessSample`]); what the evidence decides is
//! [`roost_ipc::process`]'s. The workspace keeps one axis per tab beside
//! its hook state, mutates it under the same lock, and never reads a
//! process itself: marks see only the foreground group a probe read
//! before the lock, and samples run on the sampler task.
//!
//! An epoch numbers the owner. Every claim, release and end bumps it and
//! drops what was parked, so a sample requested for one owner, or a
//! lifecycle parked under it, can never land on the next.

use roost_ipc::agent::{AgentLifecycle, AgentTabState};
use roost_ipc::process::{
    self, Foreground, Layer, Liveness, MarkVerdict, ProcessState, SampleVerdict, Stamp, TabProcess,
};
use roost_ipc::program_status::PromptMark;

use crate::foreground::ProcessSample;

/// One tab's process layer. Not persisted, like the hook state it follows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProcessAxis {
    owner_epoch: u64,
    owner: OwnerStamp,
    /// The owner's lifecycle a mark dropped while its stamped process was
    /// not in front: a stopped or backgrounded agent is still alive.
    parked: Option<AgentLifecycle>,
    suspended: bool,
    /// Unix seconds: when the stamp was taken or `suspended` last moved.
    since: i64,
    /// The spawn the stamp was taken under. A sample from another one
    /// means the PTY was replaced, and the stamp is forgotten.
    incarnation: u64,
    /// The sampler has something to read for this tab.
    due: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum OwnerStamp {
    /// No agent owns the tab, or the first sample after its claim found
    /// nothing it may stamp: what a mark always did applies.
    #[default]
    None,
    /// An agent claimed the tab; the next sample stamps the leader in
    /// front. Until it does, the owner counts as unstamped.
    Wanted,
    Stamped(Stamp),
}

/// What the sampler reads for one tab, as of the owner it was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SampleRequest {
    pub(crate) tab_id: i64,
    owner_epoch: u64,
    pub(crate) stamp_wanted: bool,
    pub(crate) stamps: Vec<Stamp>,
}

impl ProcessAxis {
    /// The owner changed: a claim or a release a report made, or a release
    /// this layer made. An agent's claim wants a stamp; anything else
    /// leaves the tab unstamped.
    pub(crate) fn owner_changed(&mut self, agent: &AgentTabState) {
        self.owner_epoch += 1;
        self.parked = None;
        self.suspended = false;
        self.owner = if process::owner_follows_a_process(agent) {
            self.due = true;
            OwnerStamp::Wanted
        } else {
            OwnerStamp::None
        };
    }

    /// Ask for a sample, when there is a stamp to take or to read.
    /// Answers whether it asked.
    pub(crate) fn request_sample(&mut self) -> bool {
        if self.owner == OwnerStamp::None {
            return false;
        }
        self.due = true;
        true
    }

    pub(crate) fn is_due(&self) -> bool {
        self.due
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
            if matches!(self.owner, OwnerStamp::Stamped(_)) {
                self.due = true;
            }
            return;
        }
        if !process::owner_follows_a_process(before) {
            return;
        }
        match process::at_mark(Layer::Owner, self.stamp(), fg) {
            MarkVerdict::Today => {}
            MarkVerdict::Release => {
                *agent = process::released(agent);
                self.owner_changed(agent);
            }
            MarkVerdict::Park => {
                if before.lifecycle != AgentLifecycle::Inactive {
                    self.parked = Some(before.lifecycle);
                }
                self.due = true;
            }
        }
    }

    /// The sample this tab is due, if any, clearing the flag.
    pub(crate) fn take_request(&mut self, tab_id: i64) -> Option<SampleRequest> {
        if !std::mem::take(&mut self.due) {
            return None;
        }
        let (stamp_wanted, stamps) = match self.owner {
            OwnerStamp::None => return None,
            OwnerStamp::Wanted => (true, Vec::new()),
            OwnerStamp::Stamped(stamp) => (false, vec![stamp]),
        };
        Some(SampleRequest {
            tab_id,
            owner_epoch: self.owner_epoch,
            stamp_wanted,
            stamps,
        })
    }

    /// What `sample`, read for `request`, does to the owner `agent`.
    /// `now` is unix seconds.
    ///
    /// A request made for an earlier owner changes nothing: the claim or
    /// release that replaced it asked for its own.
    pub(crate) fn apply_sample(
        &mut self,
        request: &SampleRequest,
        sample: &ProcessSample,
        agent: &mut AgentTabState,
        now: i64,
    ) {
        if request.owner_epoch != self.owner_epoch {
            return;
        }
        match self.owner {
            OwnerStamp::None => {}
            OwnerStamp::Wanted => {
                if request.stamp_wanted {
                    self.owner = sample.leader.map_or(OwnerStamp::None, OwnerStamp::Stamped);
                    self.incarnation = sample.incarnation;
                    self.since = now;
                }
            }
            OwnerStamp::Stamped(_) if sample.incarnation != self.incarnation => {
                self.owner = OwnerStamp::None;
                self.parked = None;
                self.suspended = false;
            }
            OwnerStamp::Stamped(stamp) => {
                let liveness = sample.liveness_of(&stamp).unwrap_or(Liveness::Unknown);
                match process::on_sample(Some(&stamp), liveness, sample.foreground) {
                    SampleVerdict::Keep => {}
                    SampleVerdict::Restore => {
                        if std::mem::take(&mut self.suspended) {
                            self.since = now;
                        }
                        if let Some(lifecycle) = self.parked.take() {
                            if agent.lifecycle == AgentLifecycle::Inactive {
                                agent.lifecycle = lifecycle;
                            }
                        }
                    }
                    SampleVerdict::Suspend => {
                        if agent.lifecycle != AgentLifecycle::Inactive {
                            self.parked = Some(agent.lifecycle);
                            agent.lifecycle = AgentLifecycle::Inactive;
                        }
                        if !std::mem::replace(&mut self.suspended, true) {
                            self.since = now;
                        }
                    }
                    SampleVerdict::End => {
                        *agent = process::released(agent);
                        self.owner_changed(agent);
                    }
                }
            }
        }
    }

    /// `Tab.process` for a tab whose hook state is `agent`.
    pub(crate) fn wire(&self, agent: &AgentTabState) -> Option<TabProcess> {
        let stamp = self.stamp()?;
        Some(TabProcess {
            state: if self.suspended {
                ProcessState::Suspended
            } else {
                ProcessState::Running
            },
            agent: agent
                .ownership
                .as_ref()
                .map(|owner| owner.source.clone())
                .unwrap_or_default(),
            pid: stamp.pid,
            since: self.since,
        })
    }

    fn stamp(&self) -> Option<&Stamp> {
        match &self.owner {
            OwnerStamp::Stamped(stamp) => Some(stamp),
            OwnerStamp::None | OwnerStamp::Wanted => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ipc::agent::{Ownership, ShellState};

    const NOW: i64 = 1_700_000_000;

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
        let request = axis.take_request(1).expect("a claim asks for a stamp");
        assert!(request.stamp_wanted);
        let mut agent = agent.clone();
        axis.apply_sample(
            &request,
            &sample(Foreground::Group(700), Some(stamp()), Liveness::Running),
            &mut agent,
            NOW,
        );
        assert_eq!(axis.stamp(), Some(&stamp()), "the precondition: stamped");
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
        let request = axis.take_request(1).expect("a sample is due");
        axis.apply_sample(&request, sample, agent, NOW + 5);
    }

    #[test]
    fn an_unstamped_owner_is_released_when_the_shell_takes_the_terminal_back() {
        let agent = owned("claude", AgentLifecycle::Working);
        let mut axis = ProcessAxis::default();
        axis.owner_changed(&agent);

        let after = mark(&mut axis, &agent, "D", Foreground::Shell);
        assert_eq!(after.ownership, None, "#621: the shell is in front");
        assert_eq!(after.lifecycle, AgentLifecycle::Inactive);
        assert_eq!(axis.wire(&after), None);
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
        assert_eq!(axis.parked, Some(AgentLifecycle::Working));
        assert!(axis.is_due(), "a sample is requested");
    }

    #[test]
    fn a_parked_lifecycle_comes_back_when_the_process_runs() {
        let mut agent = owned("claude", AgentLifecycle::Working);
        let mut axis = stamped(&agent);
        agent = mark(&mut axis, &agent, "D", Foreground::Shell);

        sampled(
            &mut axis,
            &mut agent,
            &sample(Foreground::Shell, None, Liveness::Stopped),
        );
        assert_eq!(
            axis.wire(&agent).map(|process| process.state),
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
            axis.wire(&agent)
                .map(|process| (process.state, process.since)),
            Some((ProcessState::Running, NOW + 5))
        );
    }

    #[test]
    fn a_claim_between_the_park_and_the_sample_wins() {
        let working = owned("claude", AgentLifecycle::Working);
        let mut axis = stamped(&working);
        mark(&mut axis, &working, "D", Foreground::Shell);
        let stale = axis.take_request(1).expect("the park asked for a sample");

        // A new Claude's SessionStart: inactive, under another session.
        let mut agent = owned("claude", AgentLifecycle::Inactive);
        agent.ownership.as_mut().unwrap().session_id = "s2".into();
        axis.owner_changed(&agent);
        axis.apply_sample(
            &stale,
            &sample(Foreground::Group(700), None, Liveness::Gone),
            &mut agent,
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
        axis.request_sample();
        let stale = axis.take_request(1).expect("a sample is due");

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
            NOW,
        );
        assert_eq!(agent.lifecycle, AgentLifecycle::Working);
        assert_eq!(
            axis.wire(&agent).map(|process| process.state),
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
        assert_eq!(axis.wire(&agent), None);
        assert!(!axis.is_due(), "nothing left to sample");
    }

    #[test]
    fn a_mark_from_the_stamped_group_does_what_it_always_did() {
        let mut agent = owned("claude", AgentLifecycle::Working);
        let mut axis = stamped(&agent);

        agent = mark(&mut axis, &agent, "A", Foreground::Group(700));
        assert_eq!(agent.lifecycle, AgentLifecycle::Inactive);
        assert_eq!(axis.parked, None, "Claude's own Screen Reader mark");
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
        assert_eq!(axis.wire(&agent), None);
    }
}
