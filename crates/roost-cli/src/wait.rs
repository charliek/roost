//! `roostctl wait`: block until a tab reaches a condition — on the tabs'
//! event stream where the server serves one, else by polling.

use std::time::Duration;

use serde::Deserialize;
use tokio::time::Instant;

use roost_ipc::client::EventFrame;
use roost_ipc::messages::{
    ops, EventBatch, IdentifyResult, TabClosedEvent, TabDumpParams, TabDumpResult, TabListResult,
    TabOpenedEvent, TabState, TabStateChangedEvent, WireTabRef,
};
use roost_ipc::{codes, ClientError, IpcClient};

use crate::error::CliError;
use crate::events::{self, Legs, Opened, Source};
use crate::UiSocket;

#[derive(clap::Args, Debug)]
pub(crate) struct Args {
    /// The tab: a bare id. Defaults to `$ROOST_TAB_ID`, then the UI's
    /// active tab. A host tab (`h<host>.<id>`) is refused.
    #[arg(long)]
    pub tab: Option<String>,
    /// Wait until the tab's agent state equals this.
    #[arg(long, value_parser = ["none", "running", "needs_input", "idle"])]
    pub state: Option<String>,
    /// Wait until the tab's terminal viewport (via `tab.dump`)
    /// contains this substring — e.g. a command's expected output.
    /// Note: the shell echoes the command you `tab send`, so pick a
    /// needle that appears in the OUTPUT, not in the command text
    /// itself (else it matches immediately).
    #[arg(long)]
    pub text: Option<String>,
    /// Wait until the tab no longer exists (closed).
    #[arg(long, default_value_t = false)]
    pub gone: bool,
    /// Give up after this many seconds. `0` checks once.
    #[arg(long, default_value_t = 5.0)]
    pub timeout: f64,
    /// Wait for as long as it takes.
    #[arg(long, conflicts_with = "timeout")]
    pub no_timeout: bool,
    /// Poll interval in milliseconds, and how often `--text` re-reads
    /// the viewport on the stream.
    #[arg(long, default_value_t = 100)]
    pub interval_ms: u64,
}

/// The conditions, all of which must hold.
#[derive(Debug, Default)]
struct Want {
    state: Option<TabState>,
    text: Option<String>,
    gone: bool,
}

impl Want {
    fn holds(&self, seen: &Seen) -> bool {
        if self.gone {
            return !seen.exists;
        }
        seen.exists
            && self.state.is_none_or(|want| seen.state == Some(want))
            && (self.text.is_none() || seen.text)
    }
}

/// What the wait knows about its tab.
#[derive(Debug, Default)]
struct Seen {
    exists: bool,
    state: Option<TabState>,
    /// `--text`'s needle was in the last dump.
    text: bool,
}

impl Seen {
    fn listed(list: &TabListResult, tab_id: i64) -> Self {
        let tab = list
            .projects
            .iter()
            .flat_map(|p| &p.tabs)
            .find(|t| t.id == tab_id);
        Seen {
            exists: tab.is_some(),
            state: tab.map(|t| t.state),
            text: false,
        }
    }

    /// Apply one commit. `true` when any of its events was about the tab.
    fn apply(&mut self, batch: &EventBatch, tab_id: i64) -> bool {
        let mut touched = false;
        for envelope in &batch.events {
            if !events::names_tab(envelope, tab_id) {
                continue;
            }
            touched = true;
            match envelope.event.as_str() {
                ops::EVENT_TAB_STATE_CHANGED => {
                    if let Ok(changed) = TabStateChangedEvent::deserialize(&envelope.data) {
                        self.state = Some(changed.state);
                    }
                }
                ops::EVENT_TAB_CLOSED => {
                    if TabClosedEvent::deserialize(&envelope.data).is_ok() {
                        self.exists = false;
                        self.state = None;
                    }
                }
                ops::EVENT_TAB_OPENED => {
                    if let Ok(opened) = TabOpenedEvent::deserialize(&envelope.data) {
                        self.exists = true;
                        self.state = Some(opened.tab.state);
                    }
                }
                _ => {}
            }
        }
        touched
    }
}

/// `roostctl wait`.
pub(crate) async fn run(
    ui: &mut UiSocket<'_>,
    args: Args,
    tab_env: Option<&str>,
    json: bool,
) -> Result<i32, CliError> {
    let started = Instant::now();
    if args.state.is_none() && args.text.is_none() && !args.gone {
        return Err(CliError::Usage(
            "wait needs at least one of --state, --text, or --gone".into(),
        ));
    }
    if args.gone && (args.state.is_some() || args.text.is_some()) {
        return Err(CliError::Usage(
            "--gone cannot be combined with --state or --text".into(),
        ));
    }
    let want = Want {
        state: args.state.as_deref().map(crate::parse_state).transpose()?,
        text: args.text,
        gone: args.gone,
    };
    let flag = args.tab.as_deref().map(crate::parse_tab_flag).transpose()?;
    let named = crate::named_tab(flag, tab_env)?
        .map(|tab| events::local_tab("wait", tab))
        .transpose()?;
    let identify = crate::identify(ui.client().await?).await?;
    let tab_id = match named {
        Some(tab_id) => tab_id,
        None => crate::active_tab(&identify)?,
    };
    let deadline = budget(args.timeout, args.no_timeout).map(|budget| Instant::now() + budget);
    let waiting = Waiting {
        tab_id,
        want: &want,
        deadline,
        timeout: args.timeout,
        interval: Duration::from_millis(args.interval_ms.max(10)),
    };
    let seen = waiting.run(ui, identify).await?;
    if json {
        crate::print_json(&satisfied(tab_id, &want, &seen, started.elapsed()))?;
    }
    Ok(0)
}

/// How long to wait: `--timeout` seconds (a negative one is `0`, one check),
/// or forever under `--no-timeout` or for a timeout too long to measure.
fn budget(timeout: f64, no_timeout: bool) -> Option<Duration> {
    if no_timeout {
        return None;
    }
    Duration::try_from_secs_f64(timeout.max(0.0)).ok()
}

/// `--json`'s success document. Each `satisfied` field is `null` unless
/// its flag was given: the state the tab was seen in, the needle found,
/// or `true` for a tab that is gone.
fn satisfied(tab_id: i64, want: &Want, seen: &Seen, after: Duration) -> serde_json::Value {
    serde_json::json!({
        "tab_id": tab_id.to_string(),
        "satisfied": {
            "state": want.state.and(seen.state).map(crate::format_state),
            "text": want.text.as_deref().filter(|_| seen.text),
            "gone": (want.gone && !seen.exists).then_some(true),
        },
        "after_ms": u64::try_from(after.as_millis()).unwrap_or(u64::MAX),
    })
}

struct Waiting<'a> {
    tab_id: i64,
    want: &'a Want,
    deadline: Option<Instant>,
    timeout: f64,
    interval: Duration,
}

/// How following a stream ended: the condition held, or the stream was
/// lost.
enum Watched {
    Held(Seen),
    Lost(Loss),
}

/// A lost stream: the process its subscription acked, and why.
struct Loss {
    incarnation: String,
    why: String,
}

/// Where resolving the source got to.
enum Reached {
    /// No stream there: the poll loop.
    Poll(Source),
    Stream(Source, Box<Legs>),
    /// Subscribed, and lost before the second connection was of any use.
    Lost(Source, Loss),
}

impl Waiting<'_> {
    /// Resolve the source and wait on it; after a lost stream, resolve
    /// again, once.
    ///
    /// The wait carries on only against the process it started on. Tab ids
    /// are that process's own: a local-backend switch replays the tabs onto
    /// the destination under new ids, and a restart mints new ones, so on
    /// any other process tab N is some other tab, or none — and a `--gone`
    /// read off its snapshot would be a lie.
    async fn run(&self, ui: &mut UiSocket<'_>, identify: IdentifyResult) -> Result<Seen, CliError> {
        let lost = match self.reach(ui, identify).await? {
            Reached::Poll(_) => return self.poll(ui.client().await?).await,
            Reached::Stream(source, legs) => match self.watch(&source, *legs).await? {
                Watched::Held(seen) => return Ok(seen),
                Watched::Lost(lost) => lost,
            },
            Reached::Lost(_, lost) => lost,
        };
        let reached = match self.reach_again(ui).await {
            Ok(reached) => reached,
            Err(error) => return Err(after_loss(&lost, error)),
        };
        let (source, again) = match reached {
            Reached::Poll(source) => return Err(self.changed(&source, &lost.why)),
            Reached::Stream(source, legs) => {
                if legs.stream.session_id() != lost.incarnation {
                    return Err(self.changed(&source, &lost.why));
                }
                match self.watch(&source, *legs).await? {
                    Watched::Held(seen) => return Ok(seen),
                    Watched::Lost(again) => (source, again),
                }
            }
            Reached::Lost(source, again) => {
                if again.incarnation != lost.incarnation {
                    return Err(self.changed(&source, &lost.why));
                }
                (source, again)
            }
        };
        Err(CliError::Connection(format!(
            "{}: the event stream was lost twice: {}; then {}",
            source.socket.display(),
            lost.why,
            again.why
        )))
    }

    /// Once no local-backend switch is in flight, the source `identify`
    /// names, subscribed to where it serves a stream. Until a switch
    /// settles, which socket serves the stream is not yet decided; a
    /// subscribe refused `busy` met a switch that began after that
    /// `identify`, and is held off the same way.
    async fn reach(
        &self,
        ui: &mut UiSocket<'_>,
        mut identify: IdentifyResult,
    ) -> Result<Reached, CliError> {
        let (source, opened) = loop {
            if identify.local_backend_switch.is_some() {
                identify = self.hold_off(ui).await?;
                continue;
            }
            let source = events::resolve(ui.socket_path(), &identify);
            if !source.serves_stream {
                return Ok(Reached::Poll(source));
            }
            match events::open(&source, true).await {
                Err(CliError::Server { code, .. }) if code == codes::BUSY => {
                    identify = self.hold_off(ui).await?;
                }
                opened => break (source, opened?),
            }
        };
        Ok(match opened {
            Opened::Legs(legs) => Reached::Stream(source, legs),
            Opened::Dropped { acked, error } => Reached::Lost(
                source,
                Loss {
                    incarnation: acked,
                    why: error.message().to_string(),
                },
            ),
        })
    }

    /// [`Self::reach`] from a fresh dial and a fresh `identify`.
    async fn reach_again(&self, ui: &mut UiSocket<'_>) -> Result<Reached, CliError> {
        ui.redial();
        let identify = crate::identify(ui.client().await?).await?;
        self.reach(ui, identify).await
    }

    /// The refusal for a stream that came back from a different process.
    fn changed(&self, source: &Source, why: &str) -> CliError {
        CliError::Connection(format!(
            "{}: the Roost serving tab {} changed after the event stream was lost ({why}): \
             a local-backend switch or a restart; tab ids do not carry across — re-resolve \
             the tab and wait again",
            source.socket.display(),
            self.tab_id
        ))
    }

    /// One interval, cut short by the deadline, then `identify` again.
    async fn hold_off(&self, ui: &mut UiSocket<'_>) -> Result<IdentifyResult, CliError> {
        self.check_deadline()?;
        let wake = Instant::now() + self.interval;
        tokio::time::sleep_until(self.deadline.map_or(wake, |deadline| deadline.min(wake))).await;
        self.check_deadline()?;
        crate::identify(ui.client().await?).await
    }

    fn check_deadline(&self) -> Result<(), CliError> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(CliError::Timeout(format!(
                "timed out after {}s waiting for tab {}",
                self.timeout, self.tab_id
            )));
        }
        Ok(())
    }

    /// The wait on the stream: the snapshot, then every commit after it.
    async fn watch(&self, source: &Source, legs: Legs) -> Result<Watched, CliError> {
        let Legs {
            mut stream,
            mut conn,
            tabs,
        } = legs;
        let incarnation = stream.session_id().to_string();
        let lost = |why: String| {
            Ok(Watched::Lost(Loss {
                incarnation: incarnation.clone(),
                why,
            }))
        };
        let tabs = tabs.unwrap_or_default();
        let Some(fence) = tabs.revision else {
            return Err(CliError::Failed(format!(
                "{}: serves events.subscribe but its tab.list carries no revision to fence \
                 the stream with",
                source.socket.display()
            )));
        };
        // Commits between the two would be on neither.
        if fence < stream.revision() {
            return lost(format!(
                "the tab.list snapshot (revision {fence}) is older than the subscription \
                 (revision {})",
                stream.revision()
            ));
        }
        let mut seen = Seen::listed(&tabs, self.tab_id);
        let mut tick = tokio::time::interval_at(Instant::now() + self.interval, self.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut reread = true;
        loop {
            if reread {
                if let Some(dropped) = self.dump(&mut conn, &mut seen).await? {
                    return lost(dropped);
                }
                tick.reset();
            }
            if self.want.holds(&seen) {
                return Ok(Watched::Held(seen));
            }
            self.check_deadline()?;
            // `EventStream::next` keeps a part-read frame in its own
            // buffer, so losing the race to the tick or the deadline
            // drops nothing.
            reread = tokio::select! {
                () = sleep_until(self.deadline) => false,
                frame = stream.next() => match frame {
                    Ok(Some(EventFrame::Batch(batch))) => {
                        batch.revision > fence && seen.apply(&batch, self.tab_id)
                    }
                    Ok(Some(EventFrame::Stopping(stopping))) => {
                        return Err(CliError::Connection(format!(
                            "{}: the session is stopping (reason: {})",
                            source.socket.display(),
                            stopping.reason
                        )));
                    }
                    Ok(Some(EventFrame::Ended(ended))) => {
                        return lost(format!("the stream ended ({})", ended.reason));
                    }
                    Ok(None) => return lost("the stream closed".into()),
                    Err(error) => return lost(error.to_string()),
                },
                _ = tick.tick(), if self.want.text.is_some() => true,
            };
        }
    }

    /// Re-read `--text`'s needle, when there is one. No event carries a
    /// tab's output, so this is the only way to see it. `Some` is the
    /// connection lost mid-call.
    async fn dump(
        &self,
        conn: &mut IpcClient,
        seen: &mut Seen,
    ) -> Result<Option<String>, CliError> {
        let Some(needle) = &self.want.text else {
            return Ok(None);
        };
        if !seen.exists {
            seen.text = false;
            return Ok(None);
        }
        match dump_contains(conn, self.tab_id, needle).await {
            Ok(found) => {
                seen.text = found;
                Ok(None)
            }
            Err(error) => match CliError::from(error) {
                CliError::Connection(dropped) => Ok(Some(dropped)),
                refused => Err(refused),
            },
        }
    }

    /// The poll loop, for a server with no stream.
    async fn poll(&self, client: &mut IpcClient) -> Result<Seen, CliError> {
        loop {
            let list = crate::list_tabs(client).await?;
            let mut seen = Seen::listed(&list, self.tab_id);
            if let (Some(needle), true) = (&self.want.text, seen.exists) {
                seen.text = dump_contains(client, self.tab_id, needle).await?;
            }
            if self.want.holds(&seen) {
                return Ok(seen);
            }
            self.check_deadline()?;
            tokio::time::sleep(self.interval).await;
        }
    }
}

/// Whether the tab's viewport contains `needle`. A tab that closed between
/// the list and the dump does not, yet.
async fn dump_contains(
    client: &mut IpcClient,
    tab_id: i64,
    needle: &str,
) -> Result<bool, ClientError> {
    let dumped = client
        .call::<_, TabDumpResult>(
            ops::TAB_DUMP,
            TabDumpParams {
                tab_id: WireTabRef::Local(tab_id),
                ..Default::default()
            },
        )
        .await;
    match dumped {
        Ok(dump) => Ok(dump.rows_text.join("\n").contains(needle)),
        Err(ClientError::Server { code, .. }) if code == codes::NOT_FOUND => Ok(false),
        Err(error) => Err(error),
    }
}

/// A failure to resolve again after a lost stream: whatever the resolve's
/// own code, the wait ended because its stream went away. A timeout is
/// still a timeout.
fn after_loss(lost: &Loss, error: CliError) -> CliError {
    if matches!(error, CliError::Timeout(_)) {
        return error;
    }
    CliError::Connection(format!(
        "could not resolve the Roost again after the event stream was lost: {}: {} (the \
         stream: {})",
        error.code(),
        error.message(),
        lost.why
    ))
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::fake::{Fake, Instead, Phase};
    use serde_json::json;

    async fn wait(fake: &Fake, argv: &[&str], tab_env: Option<&str>) -> Result<i32, CliError> {
        let socket = fake.socket();
        let argv = ["roostctl", "--socket", &socket, "wait"]
            .into_iter()
            .chain(argv.iter().copied());
        let args = <crate::Args as clap::Parser>::try_parse_from(argv).expect("the argv parses");
        tokio::time::timeout(Duration::from_secs(20), crate::run(args, tab_env, None))
            .await
            .expect("the wait never returned")
    }

    fn timed_out(exit: &Result<i32, CliError>) -> bool {
        matches!(exit, Err(CliError::Timeout(_)))
    }

    // ------------------------------------------------------------------
    // The two legs, and the fence between them
    // ------------------------------------------------------------------

    /// A change committed between the two legs is in the snapshot when the
    /// stream came first. Taken the other way round it lands after the
    /// snapshot and before the stream's revision, where neither can see it.
    #[tokio::test]
    async fn a_change_between_the_legs_is_seen_because_the_stream_comes_first() {
        let fake = Fake::ui("legs");
        let mut first_leg = true;
        let mut injected = false;
        fake.hook(move |world, op, phase| {
            if phase != Phase::Before || !matches!(op, "events.subscribe" | "tab.list") {
                return;
            }
            if !first_leg && !injected {
                world.set_state(7, "idle");
                injected = true;
            }
            first_leg = false;
        });
        let exit = wait(
            &fake,
            &["--tab", "7", "--state", "idle", "--timeout", "1"],
            None,
        )
        .await;
        assert_eq!(exit, Ok(0));
    }

    /// Two commits the snapshot already holds, still queued on the stream:
    /// a stale `idle` behind the `running` that followed it.
    fn stale_idle_behind_running(fake: &Fake) {
        fake.on("tab.list", 1, Phase::Before, |world| {
            world.set_state(7, "idle");
            world.set_state(7, "running");
        });
    }

    #[tokio::test]
    async fn a_batch_the_snapshot_already_holds_is_discarded() {
        let fake = Fake::ui("stale");
        stale_idle_behind_running(&fake);
        let exit = wait(
            &fake,
            &["--tab", "7", "--state", "idle", "--timeout", "0.5"],
            None,
        )
        .await;
        assert!(timed_out(&exit), "{exit:?}");
    }

    #[tokio::test]
    async fn a_batch_past_the_snapshot_is_applied() {
        let fake = Fake::ui("fresh");
        stale_idle_behind_running(&fake);
        fake.on("tab.list", 1, Phase::After, |world| {
            world.set_state(7, "idle");
        });
        let exit = wait(
            &fake,
            &["--tab", "7", "--state", "idle", "--timeout", "5"],
            None,
        )
        .await;
        assert_eq!(exit, Ok(0));
    }

    /// The stream is acked by the process that is going away and the
    /// second connection reaches its replacement, which hangs up on a
    /// snapshot asked for under the old ack. Compared first, that is a
    /// retry against the replacement, not a lost stream.
    #[tokio::test]
    async fn a_second_leg_on_another_process_is_retried_before_its_snapshot() {
        let fake = Fake::ui("replaced");
        fake.with(|world| {
            world.ack_id = "ui-0".into();
            world.tabs.insert(7, "idle");
        });
        fake.hook(|world, op, phase| {
            if op == "tab.list" && phase == Phase::Before && world.ack_id == "ui-0" {
                world.instead = Some(Instead::HangUp);
            }
        });
        fake.on("events.subscribe", 2, Phase::Before, |world| {
            world.ack_id = "ui-1".into();
        });
        let exit = wait(
            &fake,
            &["--tab", "7", "--state", "idle", "--timeout", "5"],
            None,
        )
        .await;
        assert_eq!(exit, Ok(0));
        assert_eq!(
            fake.with(|world| world.ops().join(" ")),
            "identify events.subscribe identify events.subscribe identify tab.list"
        );
    }

    /// A snapshot older than its subscription leaves the commits between
    /// the two on neither, so it fences nothing — here, a `--gone` read off
    /// a list that predates the tab.
    #[tokio::test]
    async fn a_snapshot_older_than_the_subscription_is_a_lost_stream() {
        let fake = Fake::ui("behind");
        fake.with(|world| {
            world.revision = 42;
            world.snapshot_behind = 2;
            world.tabs.clear();
        });
        let exit = wait(&fake, &["--tab", "7", "--gone", "--timeout", "5"], None).await;
        let Err(CliError::Connection(message)) = exit else {
            panic!("{exit:?}")
        };
        assert!(
            message.contains(
                "the tab.list snapshot (revision 40) is older than the subscription (revision 42)"
            ),
            "{message}"
        );
        assert!(message.contains("lost twice"), "{message}");
        assert_eq!(fake.with(|world| world.count("events.subscribe")), 2);
    }

    // ------------------------------------------------------------------
    // Losing the stream
    // ------------------------------------------------------------------

    fn changed_roost(exit: &Result<i32, CliError>) -> bool {
        matches!(exit, Err(CliError::Connection(message))
            if message.contains("the Roost serving tab 7 changed")
                && message.contains("tab ids do not carry across"))
    }

    /// A UI that, once the wait holds its snapshot, ends its stream for a
    /// backend switch and names the session its tabs moved to, where tab 7
    /// is `session`'s own tab 7 — whatever that is.
    fn switch_to(ui: &Fake, session: &Fake) {
        let path = session.socket();
        ui.on("tab.list", 1, Phase::After, move |world| {
            world.identify["local_session_socket"] = json!(path);
            world.end_streams(Some(&json!({
                "event": "stream.ended", "data": { "reason": "backend-switch" },
            })));
        });
    }

    /// The session's tab 7 is in the state asked for, and is still not the
    /// tab the wait was asked about.
    #[tokio::test]
    async fn a_stream_ended_by_a_switch_ends_the_wait_rather_than_reading_another_roosts_tab() {
        let session = Fake::session("switched-to");
        session.with(|world| world.tabs.insert(7, "idle"));
        let ui = Fake::ui("switched-from");
        switch_to(&ui, &session);
        let exit = wait(
            &ui,
            &["--tab", "7", "--state", "idle", "--timeout", "5"],
            None,
        )
        .await;
        assert!(changed_roost(&exit), "{exit:?}");
        assert_eq!(
            session.with(|world| world.ops().join(" ")),
            "events.subscribe session.identify tab.list",
            "the wait did resolve again, and stopped at the identity"
        );
    }

    /// `--gone` is the dangerous one: another process's snapshot without a
    /// tab 7 says nothing about this tab 7 — across a switch, across a
    /// restart on the same socket, or onto a server with no stream at all.
    #[tokio::test]
    async fn gone_is_never_read_off_a_different_roost() {
        let session = Fake::session("gone-switched-to");
        session.with(|world| world.tabs.clear());
        let ui = Fake::ui("gone-switched-from");
        switch_to(&ui, &session);
        let exit = wait(&ui, &["--tab", "7", "--gone", "--timeout", "5"], None).await;
        assert!(changed_roost(&exit), "{exit:?}");

        let restarted = Fake::ui("gone-restarted");
        restarted.on("tab.list", 1, Phase::After, |world| {
            world.ack_id = "ui-2".into();
            world.identify["instance_id"] = json!("ui-2");
            world.tabs.clear();
            world.end_streams(None);
        });
        let exit = wait(
            &restarted,
            &["--tab", "7", "--gone", "--timeout", "5"],
            None,
        )
        .await;
        assert!(changed_roost(&exit), "{exit:?}");
        assert_eq!(restarted.with(|world| world.count("events.subscribe")), 2);
    }

    #[tokio::test]
    async fn gone_is_never_polled_off_a_roost_that_lost_its_stream() {
        let downgraded = Fake::ui("gone-downgraded");
        downgraded.on("tab.list", 1, Phase::After, |world| {
            world.identify = crate::events::fake::identify(&[], None);
            world.tabs.clear();
            world.end_streams(None);
        });
        let exit = wait(
            &downgraded,
            &["--tab", "7", "--gone", "--timeout", "5"],
            None,
        )
        .await;
        assert!(changed_roost(&exit), "{exit:?}");
        assert_eq!(downgraded.with(|world| world.count("tab.list")), 1);
    }

    /// A subscriber that falls behind is closed with a bare EOF; so is one
    /// that skips a revision. Either way the wait resolves again, finds the
    /// same process, and carries on.
    #[tokio::test]
    async fn a_lagged_or_gapped_stream_is_resolved_again_once() {
        let lagged = Fake::ui("lag");
        lagged.on("events.subscribe", 1, Phase::After, |world| {
            world.retitle(9);
            world.end_streams(None);
        });
        on_second_list_idle(&lagged);
        let exit = wait(
            &lagged,
            &["--tab", "7", "--state", "idle", "--timeout", "5"],
            None,
        )
        .await;
        assert_eq!(exit, Ok(0));
        assert_eq!(lagged.with(|world| world.count("events.subscribe")), 2);

        let gapped = Fake::ui("gap");
        gapped.on("events.subscribe", 1, Phase::After, |world| {
            let skipped = json!({ "revision": world.revision + 2, "events": [] });
            world.push(&skipped.to_string());
        });
        on_second_list_idle(&gapped);
        let exit = wait(
            &gapped,
            &["--tab", "7", "--state", "idle", "--timeout", "5"],
            None,
        )
        .await;
        assert_eq!(exit, Ok(0));
        assert_eq!(gapped.with(|world| world.count("events.subscribe")), 2);
    }

    fn on_second_list_idle(fake: &Fake) {
        fake.on("tab.list", 2, Phase::Before, |world| {
            world.tabs.insert(7, "idle");
        });
    }

    #[tokio::test]
    async fn a_second_loss_is_a_connection_failure() {
        let fake = Fake::ui("twice");
        fake.hook(|world, op, phase| {
            if op == "events.subscribe" && phase == Phase::After {
                world.end_streams(None);
            }
        });
        let exit = wait(
            &fake,
            &["--tab", "7", "--state", "idle", "--timeout", "5"],
            None,
        )
        .await;
        let Err(CliError::Connection(message)) = exit else {
            panic!("{exit:?}")
        };
        assert!(message.contains("lost twice"), "{message}");
        assert_eq!(fake.with(|world| world.count("events.subscribe")), 2);
    }

    /// A subscription whose second connection drops before its snapshot
    /// is a lost stream like any other: resolved again once.
    #[tokio::test]
    async fn a_second_connection_dropped_while_opening_is_resolved_again_once() {
        let once = Fake::ui("open-drop");
        once.with(|world| world.tabs.insert(7, "idle"));
        once.on("tab.list", 1, Phase::Before, |world| {
            world.instead = Some(Instead::HangUp);
        });
        let argv = ["--tab", "7", "--state", "idle", "--timeout", "5"];
        assert_eq!(wait(&once, &argv, None).await, Ok(0));
        assert_eq!(
            once.with(|world| world.ops().join(" ")),
            "identify events.subscribe identify tab.list identify events.subscribe identify tab.list"
        );

        let twice = Fake::ui("open-drop-twice");
        twice.hook(|world, op, phase| {
            if op == "tab.list" && phase == Phase::Before {
                world.instead = Some(Instead::HangUp);
            }
        });
        let exit = wait(&twice, &argv, None).await;
        let Err(CliError::Connection(message)) = exit else {
            panic!("{exit:?}")
        };
        assert!(message.contains("lost twice"), "{message}");
        assert_eq!(twice.with(|world| world.count("events.subscribe")), 2);
    }

    /// Resolving again is part of the wait's own recovery, so however it
    /// fails, the wait failed on a lost stream: `connection`, carrying the
    /// resolve's own code and message.
    #[tokio::test]
    async fn a_failed_resolve_after_a_loss_is_a_connection_failure_naming_why() {
        let argv = ["--tab", "7", "--state", "idle", "--timeout", "5"];
        let refused = Fake::ui("again-refused");
        refused.on("events.subscribe", 1, Phase::After, |world| {
            world.end_streams(None);
        });
        refused.on("events.subscribe", 2, Phase::Before, |world| {
            world.instead = Some(Instead::Refuse("not-implemented", "no stream after all"));
        });
        let error = wait(&refused, &argv, None).await.unwrap_err();
        assert_eq!(error.code(), "connection", "{error:?}");
        assert!(
            error
                .message()
                .contains("after the event stream was lost: not-implemented: no stream after all"),
            "{error:?}"
        );

        let vanished = Fake::ui("again-vanished");
        let socket = vanished.socket.clone();
        vanished.on("tab.list", 1, Phase::After, move |world| {
            std::fs::remove_file(&socket).expect("unlink the fake's socket");
            world.end_streams(None);
        });
        let error = wait(&vanished, &argv, None).await.unwrap_err();
        assert_eq!(error.code(), "connection", "{error:?}");
        assert!(
            error
                .message()
                .contains("after the event stream was lost: connection: "),
            "{error:?}"
        );

        let lost = Loss {
            incarnation: "ui-1".into(),
            why: "the stream closed".into(),
        };
        let no_target = CliError::from(roost_ipc::target::TargetError::NoLiveTarget {
            tried: vec!["/nothing.sock".into()],
        });
        let error = after_loss(&lost, no_target);
        assert_eq!(error.code(), "connection", "{error:?}");
        assert!(
            error
                .message()
                .contains("after the event stream was lost: no-target: "),
            "{error:?}"
        );
        let timeout = CliError::Timeout("5s".into());
        assert_eq!(after_loss(&lost, timeout.clone()), timeout);
    }

    #[tokio::test]
    async fn a_session_that_stops_ends_the_wait_with_its_reason() {
        let fake = Fake::session("stopping");
        fake.on("events.subscribe", 1, Phase::After, |world| {
            world.end_streams(Some(&json!({
                "event": "session.stopping", "data": { "reason": "stop" },
            })));
        });
        let exit = wait(
            &fake,
            &["--tab", "7", "--state", "idle", "--timeout", "5"],
            None,
        )
        .await;
        let Err(CliError::Connection(message)) = exit else {
            panic!("{exit:?}")
        };
        assert!(
            message.contains("the session is stopping (reason: stop)"),
            "{message}"
        );
        assert_eq!(fake.with(|world| world.count("events.subscribe")), 1);
    }

    #[tokio::test]
    async fn a_wait_during_a_backend_switch_subscribes_once_it_settles() {
        let fake = Fake::ui("settle");
        fake.with(|world| world.identify["local_backend_switch"] = json!("preparing"));
        fake.on("identify", 3, Phase::Before, |world| {
            world.identify["local_backend_switch"] = serde_json::Value::Null;
            world.tabs.insert(7, "idle");
        });
        let exit = wait(
            &fake,
            &[
                "--tab",
                "7",
                "--state",
                "idle",
                "--interval-ms",
                "10",
                "--timeout",
                "5",
            ],
            None,
        )
        .await;
        assert_eq!(exit, Ok(0));
        assert_eq!(
            fake.with(|world| world.ops().join(" ")),
            "identify identify identify events.subscribe identify tab.list"
        );
    }

    #[tokio::test]
    async fn a_subscribe_refused_busy_is_held_off_until_the_switch_settles() {
        let fake = Fake::ui("busy");
        fake.with(|world| world.tabs.insert(7, "idle"));
        fake.on("identify", 1, Phase::After, |world| {
            world.identify["local_backend_switch"] = json!("preparing");
        });
        fake.on("identify", 3, Phase::Before, |world| {
            world.identify["local_backend_switch"] = serde_json::Value::Null;
        });
        let exit = wait(
            &fake,
            &[
                "--tab",
                "7",
                "--state",
                "idle",
                "--interval-ms",
                "10",
                "--timeout",
                "5",
            ],
            None,
        )
        .await;
        assert_eq!(exit, Ok(0));
        assert_eq!(
            fake.with(|world| world.ops().join(" ")),
            "identify events.subscribe identify identify events.subscribe identify tab.list"
        );
    }

    /// The hold-off sleeps no further than the deadline, and a deadline
    /// that passed in it is a timeout — even for a switch that has settled
    /// by then onto a tab already in the state asked for.
    #[tokio::test]
    async fn a_switch_that_outlasts_the_timeout_times_out_on_time() {
        let fake = Fake::ui("settle-late");
        fake.with(|world| {
            world.identify["local_backend_switch"] = json!("preparing");
            world.tabs.insert(7, "idle");
        });
        fake.on("identify", 2, Phase::Before, |world| {
            world.identify["local_backend_switch"] = serde_json::Value::Null;
        });
        let argv = [
            "--tab",
            "7",
            "--state",
            "idle",
            "--timeout",
            "1",
            "--interval-ms",
            "60000",
        ];
        let started = std::time::Instant::now();
        let exit = tokio::time::timeout(Duration::from_secs(5), wait(&fake, &argv, None))
            .await
            .expect("the hold-off slept past the deadline");
        assert!(timed_out(&exit), "{exit:?}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(fake.with(|world| world.count("events.subscribe")), 0);
    }

    // ------------------------------------------------------------------
    // Timeouts
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn timeout_0_checks_once_on_the_stream() {
        let held = Fake::ui("once-held");
        held.with(|world| world.tabs.insert(7, "idle"));
        let exit = wait(
            &held,
            &["--tab", "7", "--state", "idle", "--timeout", "0"],
            None,
        )
        .await;
        assert_eq!(exit, Ok(0));

        let unheld = Fake::ui("once-unheld");
        let started = std::time::Instant::now();
        let exit = wait(
            &unheld,
            &["--tab", "7", "--state", "idle", "--timeout", "0"],
            None,
        )
        .await;
        assert!(timed_out(&exit), "{exit:?}");
        assert_eq!(exit.unwrap_err().exit_code(), 4);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(unheld.with(|world| world.count("tab.list")), 1);
    }

    #[tokio::test]
    async fn timeout_0_checks_once_when_polling() {
        let fake = Fake::without_stream("poll-once");
        fake.with(|world| world.tabs.insert(7, "idle"));
        let exit = wait(
            &fake,
            &["--tab", "7", "--state", "idle", "--timeout", "0"],
            None,
        )
        .await;
        assert_eq!(exit, Ok(0));
        let exit = wait(
            &fake,
            &["--tab", "7", "--state", "running", "--timeout", "0"],
            None,
        )
        .await;
        assert!(timed_out(&exit), "{exit:?}");
    }

    #[test]
    fn the_budget_is_the_timeout_or_nothing_at_all() {
        assert_eq!(budget(2.5, false), Some(Duration::from_millis(2500)));
        assert_eq!(budget(0.0, false), Some(Duration::ZERO));
        assert_eq!(budget(-1.0, false), Some(Duration::ZERO));
        assert_eq!(budget(5.0, true), None);
        assert_eq!(budget(f64::INFINITY, false), None);
    }

    #[test]
    fn no_timeout_and_timeout_cannot_both_be_given() {
        let parsed = <crate::Args as clap::Parser>::try_parse_from([
            "roostctl",
            "wait",
            "--gone",
            "--timeout",
            "3",
            "--no-timeout",
        ]);
        let error = parsed.expect_err("the two conflict");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
        assert!(<crate::Args as clap::Parser>::try_parse_from([
            "roostctl",
            "wait",
            "--gone",
            "--no-timeout",
        ])
        .is_ok());
    }

    // ------------------------------------------------------------------
    // `--text`, `--gone`, and what `--json` reports
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn text_is_read_again_on_an_event_for_the_tab() {
        let fake = Fake::ui("text-event");
        fake.on("tab.dump", 1, Phase::After, |world| {
            world.dump = "BUILD OK".into();
            world.retitle(7);
        });
        let argv = ["--tab", "7", "--text", "BUILD OK", "--interval-ms", "60000"];
        let exit = wait(&fake, &[&argv[..], &["--timeout", "2"]].concat(), None).await;
        assert_eq!(exit, Ok(0));
        assert_eq!(fake.with(|world| world.count("tab.dump")), 2);

        let elsewhere = Fake::ui("text-elsewhere");
        elsewhere.on("tab.dump", 1, Phase::After, |world| {
            world.retitle(9);
            world.retitle(7);
            world.retitle(9);
        });
        let exit = wait(
            &elsewhere,
            &[&argv[..], &["--timeout", "0.5"]].concat(),
            None,
        )
        .await;
        assert!(timed_out(&exit), "{exit:?}");
        assert_eq!(
            elsewhere.with(|world| world.count("tab.dump")),
            2,
            "one dump for the snapshot and one for tab 7's event; none for tab 9's"
        );
    }

    #[tokio::test]
    async fn text_is_read_again_on_the_interval_with_no_event_at_all() {
        let fake = Fake::ui("text-tick");
        fake.on("tab.dump", 1, Phase::After, |world| {
            world.dump = "BUILD OK".into()
        });
        let exit = wait(
            &fake,
            &[
                "--tab",
                "7",
                "--text",
                "BUILD OK",
                "--interval-ms",
                "20",
                "--timeout",
                "5",
            ],
            None,
        )
        .await;
        assert_eq!(exit, Ok(0));
        assert_eq!(fake.with(|world| world.count("events.subscribe")), 1);
    }

    #[tokio::test]
    async fn gone_is_seen_on_the_stream() {
        let fake = Fake::ui("gone");
        fake.on("tab.list", 1, Phase::After, |world| {
            world.tabs.remove(&7);
            world.commit(&json!([{ "event": "tab.closed", "data": { "tab_id": "7" } }]));
        });
        let exit = wait(&fake, &["--tab", "7", "--gone", "--timeout", "5"], None).await;
        assert_eq!(exit, Ok(0));
    }

    #[test]
    fn json_names_what_held_and_nulls_what_was_not_asked() {
        let seen = Seen {
            exists: true,
            state: Some(TabState::Idle),
            text: true,
        };
        let want = Want {
            state: Some(TabState::Idle),
            text: Some("OK".into()),
            gone: false,
        };
        assert_eq!(
            satisfied(7, &want, &seen, Duration::from_millis(12)),
            json!({
                "tab_id": "7",
                "satisfied": { "state": "idle", "text": "OK", "gone": null },
                "after_ms": 12,
            })
        );
        let gone = Want {
            gone: true,
            ..Want::default()
        };
        assert_eq!(
            satisfied(7, &gone, &Seen::default(), Duration::ZERO)["satisfied"],
            json!({ "state": null, "text": null, "gone": true })
        );
    }

    // ------------------------------------------------------------------
    // Where the wait reads, and which tab
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn a_ui_that_streams_serves_both_legs() {
        let fake = Fake::ui("ui-source");
        fake.with(|world| world.tabs.insert(7, "idle"));
        assert_eq!(
            wait(&fake, &["--tab", "7", "--state", "idle"], None).await,
            Ok(0)
        );
        let log = fake.with(|world| world.log.clone());
        let ops: Vec<&str> = log.iter().map(|(_, op)| op.as_str()).collect();
        assert_eq!(
            ops,
            ["identify", "events.subscribe", "identify", "tab.list"]
        );
        assert_ne!(
            log[1].0, log[2].0,
            "the snapshot is not on the pushed connection"
        );
        assert_eq!(log[2].0, log[3].0);
    }

    #[tokio::test]
    async fn a_server_without_the_stream_is_polled() {
        let fake = Fake::without_stream("poll-source");
        fake.on("tab.list", 2, Phase::After, |world| {
            world.tabs.insert(7, "idle");
        });
        let exit = wait(
            &fake,
            &[
                "--tab",
                "7",
                "--state",
                "idle",
                "--interval-ms",
                "10",
                "--timeout",
                "5",
            ],
            None,
        )
        .await;
        assert_eq!(exit, Ok(0));
        let ops = fake.with(|world| world.ops().join(" "));
        assert!(!ops.contains("events.subscribe"), "{ops}");
        assert!(
            ops.starts_with("identify tab.list tab.list tab.list"),
            "{ops}"
        );
    }

    /// Under `local-backend = session` a bare id on the UI socket already
    /// means the session's tab, and so does the active tab `identify`
    /// reports — so the id resolved there is the one both legs use on the
    /// session.
    #[tokio::test]
    async fn under_a_session_backend_the_uis_tab_id_is_the_sessions() {
        let session = Fake::session("slot");
        session.with(|world| {
            world.tabs.insert(7, "idle");
            world.dump = "on the session".into();
        });
        let ui = Fake::ui("slot-ui");
        ui.with(|world| {
            world.identify["local_session_socket"] = json!(session.socket());
            world.tabs.clear();
        });
        let exit = wait(&ui, &["--state", "idle", "--text", "on the session"], None).await;
        assert_eq!(exit, Ok(0));
        assert_eq!(ui.with(|world| world.ops().join(" ")), "identify");
        assert_eq!(
            session.with(|world| world.ops().join(" ")),
            "events.subscribe session.identify tab.list tab.dump"
        );
    }

    #[tokio::test]
    async fn a_host_tab_is_refused_before_anything_is_dialled() {
        let fake = Fake::ui("host-tab");
        for (argv, env) in [
            (vec!["--tab", "h1.7", "--state", "idle"], None),
            (vec!["--state", "idle"], Some("h1.7")),
        ] {
            let exit = wait(&fake, &argv, env).await;
            let Err(error) = exit else {
                panic!("{argv:?} {env:?}")
            };
            assert_eq!(error.exit_code(), 2, "{error:?}");
            assert!(error.message().contains("host tab h1.7"), "{error:?}");
        }
        assert!(fake.with(|world| world.log.is_empty()));
    }
}
