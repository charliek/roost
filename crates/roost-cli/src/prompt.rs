//! `roostctl tab prompt` — submit a prompt to an agent in a tab and wait
//! for the turn it starts (plan 067 §3.8).
//!
//! No new op: the verb is `tab.write` twice and then `wait`'s event
//! stream, in the one order that closes the race between them. The stream
//! is subscribed and fenced **first**, and both writes go on the request
//! connection that subscription checked the identity of, so the prompt
//! and the wait cannot straddle a restart and no turn can start and end
//! in a gap between them. That ordering is the whole verb; everything
//! else here is `wait`'s machinery reused.

use std::time::Duration;

use tokio::time::Instant;

use roost_ipc::messages::{ops, TabState, TabWriteParams};
use roost_ipc::IpcClient;

use crate::error::CliError;
use crate::events::Source;
use crate::millis;
use crate::wait::{self, Bound, Following, Subscribed, Waiting, Want};
use crate::UiSocket;

#[derive(clap::Args, Debug)]
pub(crate) struct Args {
    /// The tab: a bare id. Defaults to `$ROOST_TAB_ID`; exits 2 without
    /// either. A host tab (`h<host>.<id>`) is refused.
    #[arg(long)]
    pub tab: Option<String>,
    /// The prompt to submit. Written as-is; Enter follows as a second
    /// write.
    pub text: String,
    /// How long the tab has to reach `running` after the prompt before
    /// the verb gives up with exit 4 `stalled`. Never outlasts whatever
    /// `--timeout` has left.
    #[arg(long, default_value_t = 5.0)]
    pub activity_timeout: f64,
    /// A state the turn may settle in. Repeatable; the default is `idle`
    /// or `needs_input`.
    #[arg(long, value_parser = ["none", "running", "needs_input", "idle"])]
    pub until: Vec<String>,
    /// Give up after this many seconds, every call to the socket
    /// included. Required unless `--no-timeout`.
    #[arg(long)]
    pub timeout: Option<f64>,
    /// Wait for as long as the turn takes. A call the socket does not
    /// answer within 30 s still ends the wait.
    #[arg(long, conflicts_with = "timeout")]
    pub no_timeout: bool,
}

/// How often a local-backend switch in flight is re-checked. No
/// `--interval-ms`: nothing in this verb polls, because the stream is
/// required, and this is the only wait that is not on it.
const HOLD_OFF: Duration = Duration::from_millis(100);

/// What the prompt did, once the turn settled.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Done {
    tab_id: i64,
    /// How long after the Enter write the tab reached `running`. `None`
    /// when the fenced snapshot already said `running`, so there was no
    /// gate to pass.
    started_after: Option<Duration>,
    state: TabState,
    after: Duration,
}

impl Done {
    /// What the stream and its snapshot last showed, once the settled
    /// wait held.
    fn settled(
        tab_id: i64,
        started_after: Option<Duration>,
        after: Duration,
        following: &Following,
    ) -> Result<Self, CliError> {
        let state = following.state().ok_or_else(|| {
            CliError::Failed(format!(
                "tab {tab_id} settled in no state at all; the event stream and its snapshot disagree"
            ))
        })?;
        Ok(Done {
            tab_id,
            started_after,
            state,
            after,
        })
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "tab_id": self.tab_id.to_string(),
            "started_after_ms": self.started_after.map(millis),
            "settled": { "state": crate::format_state(self.state) },
            "after_ms": millis(self.after),
        })
    }

    fn line(&self) -> String {
        let settled = format!(
            "{} after {}ms",
            crate::format_state(self.state),
            millis(self.after)
        );
        match self.started_after {
            Some(started) => format!(
                "prompted: tab {} started after {}ms, {settled}",
                self.tab_id,
                millis(started)
            ),
            None => format!(
                "prompted: tab {} was already running, {settled}",
                self.tab_id
            ),
        }
    }
}

/// `roostctl tab prompt`. `tab_id` is already resolved, and a host tab
/// already refused: which tab a mutating verb writes to is settled before
/// anything else about the command line is.
pub(crate) async fn run(
    ui: &mut UiSocket<'_>,
    args: Args,
    tab_id: i64,
    json: bool,
) -> Result<i32, CliError> {
    let done = submit(ui, args, tab_id).await?;
    if json {
        crate::print_json(&done.json())?;
    } else {
        println!("{}", done.line());
    }
    Ok(0)
}

async fn submit(ui: &mut UiSocket<'_>, args: Args, tab_id: i64) -> Result<Done, CliError> {
    // Every refusal this verb can answer without a socket comes first,
    // and the bound is decided before the first dial (plan 067 §3.3).
    let deadline = budget(args.timeout, args.no_timeout)?;
    let bound = Bound::new(deadline.unwrap_or(0.0), deadline.is_none())?;
    let gate_span = gate_span(args.activity_timeout)?;
    let settled_want = Want {
        states: until(&args.until)?,
        ..Want::default()
    };
    let running = Want {
        states: vec![TabState::Running],
        ..Want::default()
    };
    // What a timeout names. Never `0`, which `Waiting` reads as
    // `--timeout 0`'s check-once; `budget` refuses that, and under
    // `--no-timeout` there is no deadline to print.
    let named = deadline.unwrap_or(args.activity_timeout);
    let settled = Waiting {
        tab_id,
        want: &settled_want,
        bound,
        timeout: named,
        interval: HOLD_OFF,
    };

    let identify = wait::identify(ui, bound, |op| settled.cut(op)).await?;
    let (source, mut legs) = match settled.subscribe(ui, identify).await? {
        Subscribed::Legs(source, legs) => (source, *legs),
        Subscribed::None(source) => return Err(unsupported(&source)),
    };
    // A turn already under way has no start left for the gate to catch.
    // The writes still go through: queueing a prompt behind the one
    // running is the agent's business, not this verb's.
    let already_running = legs
        .tabs
        .as_ref()
        .and_then(|tabs| wait::state_in(tabs, tab_id))
        == Some(TabState::Running);

    let conn = &mut legs.conn;
    write(&settled, conn, args.text.into_bytes()).await?;
    // Enter as its own write, so an input box that reads one write as a
    // paste does not swallow it into the text.
    write(&settled, conn, b"\r".to_vec()).await?;
    let sent = Instant::now();

    let (started_after, following) = if already_running {
        (None, settled.follow(ui, &source, legs).await?)
    } else {
        let gate = Gate::new(&settled, sent, gate_span, &running)?;
        let held = gate
            .waiting
            .follow(ui, &source, legs)
            .await
            .map_err(|error| gate.stalled(error))?;
        (
            Some(sent.elapsed()),
            settled.follow_on(ui, &source, held).await?,
        )
    };
    Done::settled(tab_id, started_after, sent.elapsed(), &following)
}

/// The activity gate: the same wait, on the same stream, for `running`
/// alone, under the earlier of the two deadlines.
///
/// **The gate is temporal, not causal.** It asks whether the tab reached
/// `running` after the fence, not whether this prompt is what took it
/// there — an unrelated turn starting in the window satisfies it.
struct Gate<'a> {
    waiting: Waiting<'a>,
    /// The gate's deadline is `--activity-timeout`'s rather than the
    /// overall `--timeout`'s, so its passing is a stall and not the
    /// caller's own budget running out. Decided with the deadline, in
    /// one place: the two answers cannot be read the same way twice.
    on_activity: bool,
}

impl<'a> Gate<'a> {
    fn new(
        settled: &Waiting<'_>,
        sent: Instant,
        span: Duration,
        running: &'a Want,
    ) -> Result<Self, CliError> {
        let activity = sent.checked_add(span).ok_or_else(|| {
            CliError::Usage(format!(
                "--activity-timeout {}s is too long to set a deadline by",
                span.as_secs_f64()
            ))
        })?;
        let global = settled
            .bound
            .deadline()
            .filter(|global| *global <= activity);
        Ok(Self {
            waiting: Waiting {
                want: running,
                bound: Bound::Deadline(global.unwrap_or(activity)),
                // A timeout names the deadline that produced it.
                timeout: global.map_or(span.as_secs_f64(), |_| settled.timeout),
                tab_id: settled.tab_id,
                interval: settled.interval,
            },
            on_activity: global.is_none(),
        })
    }

    /// The gate's deadline passing is `stalled` — the prompt went in and
    /// the tab showed no sign of a turn. Unless the overall `--timeout`
    /// is what ran out, which keeps `timeout`'s one meaning.
    fn stalled(&self, error: CliError) -> CliError {
        match error {
            CliError::Timeout(why) if self.on_activity => CliError::Stalled(format!(
                "{why}: it never reached running after the prompt — the text and the Enter \
                 were written, so the turn may yet start"
            )),
            other => other,
        }
    }
}

/// `--timeout N`, or `None` for `--no-timeout`.
///
/// One of the two is required: how long a turn takes is not something
/// this verb can guess, so a default would report a timeout for a prompt
/// still being answered. `--timeout 0` would check once, which can never
/// be true of a turn submitted a moment ago.
fn budget(timeout: Option<f64>, no_timeout: bool) -> Result<Option<f64>, CliError> {
    match timeout {
        None if no_timeout => Ok(None),
        None => Err(CliError::Usage(
            "tab prompt needs --timeout SECONDS or --no-timeout: how long a turn takes is not \
             something this verb can guess, so it has no default"
                .into(),
        )),
        Some(seconds) if seconds.is_finite() && seconds > 0.0 => Ok(Some(seconds)),
        Some(seconds) => Err(CliError::Usage(format!(
            "--timeout must be a positive, finite number of seconds, not {seconds}; use \
             --no-timeout to wait for as long as the turn takes"
        ))),
    }
}

/// `--activity-timeout` as a span. Non-positive is refused rather than
/// read as "no gate at all": the gate is what the verb is for.
fn gate_span(seconds: f64) -> Result<Duration, CliError> {
    Duration::try_from_secs_f64(seconds)
        .ok()
        .filter(|span| !span.is_zero())
        .ok_or_else(|| {
            CliError::Usage(format!(
                "--activity-timeout must be a positive, finite number of seconds, not {seconds}"
            ))
        })
}

/// `--until`, or the two states a finished turn leaves a tab in.
fn until(states: &[String]) -> Result<Vec<TabState>, CliError> {
    if states.is_empty() {
        return Ok(vec![TabState::Idle, TabState::NeedsInput]);
    }
    states
        .iter()
        .map(|state| crate::parse_state(state))
        .collect()
}

fn unsupported(source: &Source) -> CliError {
    CliError::Unsupported(format!(
        "{}: tab prompt needs the tabs' event stream (events.subscribe), which this Roost \
         does not serve — the Swift Mac app today. There is no polling fallback: a poll \
         cannot see a turn that starts and ends between two tab.list calls, so it would \
         report a stall that never happened. Send the text with `roostctl tab send` and read \
         the tab with `roostctl tab dump`.",
        source.socket.display()
    ))
}

async fn write(waiting: &Waiting<'_>, conn: &mut IpcClient, data: Vec<u8>) -> Result<(), CliError> {
    let params = TabWriteParams {
        tab_id: waiting.tab_id,
        data,
    };
    let call = conn.call::<_, serde_json::Value>(ops::TAB_WRITE, params);
    waiting.call(ops::TAB_WRITE, call).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::fake::{Fake, Instead, Phase};
    use serde_json::json;
    use std::future::Future;

    /// The tab every case prompts. `Fake` puts it in project 1.
    const TAB: i64 = 7;

    const TEXT: &str = "summarize the failures";

    /// A fake whose tab 7 is not running, so the gate has something to
    /// wait for, and whose active tab is some *other* tab — nothing here
    /// may fall back to it.
    fn quiet(tag: &str) -> Fake {
        let fake = Fake::ui(tag);
        fake.with(|world| {
            world.tabs.insert(TAB, "idle");
            world.identify["active_tab_id"] = json!("99");
        });
        fake
    }

    fn parse(socket: &str, argv: &[&str]) -> crate::Args {
        let full = ["roostctl", "--socket", socket, "tab", "prompt"]
            .into_iter()
            .chain(argv.iter().copied());
        <crate::Args as clap::Parser>::try_parse_from(full).expect("the argv parses")
    }

    /// Every case is bounded well above its own deadlines, so a verb
    /// that hangs fails as itself rather than as the suite's timeout.
    async fn bounded<T>(work: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(20), work)
            .await
            .expect("tab prompt never returned")
    }

    /// The verb's own core, for the facts it reports. Skips the tab
    /// resolution `run_on_ui` does, which has its own cases below.
    async fn done(fake: &Fake, argv: &[&str]) -> Result<Done, CliError> {
        let socket = fake.socket();
        let crate::Cmd::Tab(crate::TabCmd::Prompt(args)) = parse(&socket, argv).command else {
            panic!("{argv:?} is not a tab prompt")
        };
        let selector = fake.selector();
        let mut ui = UiSocket::new(&selector);
        bounded(submit(&mut ui, args, TAB)).await
    }

    /// The whole command line, through `main`'s dispatch — the tab
    /// policy, the argument refusals, and the exit code.
    async fn exit(fake: &Fake, argv: &[&str], tab_env: Option<&str>) -> Result<i32, CliError> {
        let args = parse(&fake.socket(), argv);
        bounded(crate::run(args, tab_env, None)).await
    }

    fn refused(error: &CliError) -> (i32, &str) {
        (error.exit_code(), error.code())
    }

    /// Tab 7 starts its turn and then settles, both after the prompt.
    fn runs_then(fake: &Fake, settles: &'static str) {
        fake.on("tab.write", 2, Phase::After, move |world| {
            world.set_state(TAB, "running");
            world.set_state(TAB, settles);
        });
    }

    // ------------------------------------------------------------------
    // The sequence: subscribe, write, write, gate, settle
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn a_turn_that_starts_and_settles_reports_both_durations() {
        let fake = quiet("prompt-idle");
        runs_then(&fake, "idle");
        let done = done(&fake, &["--timeout", "5", TEXT])
            .await
            .expect("the turn settled");
        assert_eq!(done.tab_id, TAB);
        assert_eq!(done.state, TabState::Idle);
        let started = done.started_after.expect("the gate was not skipped");
        assert!(started <= done.after, "{done:?}");
    }

    #[tokio::test]
    async fn both_writes_go_on_the_subscribed_request_connection() {
        let fake = quiet("prompt-legs");
        runs_then(&fake, "idle");
        assert!(done(&fake, &["--timeout", "5", TEXT]).await.is_ok());

        let (log, writes) = fake.with(|world| (world.log.clone(), world.writes.clone()));
        let ops: Vec<&str> = log.iter().map(|(_, op)| op.as_str()).collect();
        assert_eq!(
            ops,
            [
                "identify",
                "events.subscribe",
                "identify",
                "tab.list",
                "tab.write",
                "tab.write"
            ]
        );
        let request = log[3].0;
        assert_ne!(request, log[1].0, "the subscribe is on its own connection");
        assert_eq!(
            writes,
            [
                (request, TAB.to_string(), TEXT.as_bytes().to_vec()),
                (request, TAB.to_string(), b"\r".to_vec()),
            ],
            "the text then the Enter, both on the connection the snapshot came on"
        );
    }

    #[tokio::test]
    async fn no_turn_within_the_activity_gate_is_stalled() {
        let fake = quiet("prompt-stalled");
        let started = std::time::Instant::now();
        let error = done(
            &fake,
            &["--timeout", "10", "--activity-timeout", "0.2", TEXT],
        )
        .await
        .expect_err("nothing ever ran");
        assert_eq!(refused(&error), (4, "stalled"), "{error:?}");
        assert!(
            error.message().contains("never reached running"),
            "{error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(3), "{error:?}");
        assert_eq!(
            fake.with(|world| world.writes.len()),
            2,
            "the prompt went in"
        );
    }

    #[tokio::test]
    async fn a_tab_already_running_at_the_fence_skips_the_gate() {
        let fake = Fake::ui("prompt-running");
        fake.on("tab.write", 2, Phase::After, |world| {
            world.set_state(TAB, "idle");
        });
        let done = done(
            &fake,
            &["--timeout", "5", "--activity-timeout", "0.2", TEXT],
        )
        .await
        .expect("the turn settled");
        assert_eq!(done.started_after, None);
        assert_eq!(done.state, TabState::Idle);
        assert_eq!(fake.with(|world| world.writes.len()), 2);
    }

    #[tokio::test]
    async fn a_turn_that_stops_for_the_user_settles_on_needs_input() {
        let fake = quiet("prompt-needs-input");
        runs_then(&fake, "needs_input");
        let done = done(&fake, &["--timeout", "5", TEXT])
            .await
            .expect("the turn settled");
        assert_eq!(done.state, TabState::NeedsInput);
    }

    #[tokio::test]
    async fn until_needs_input_alone_waits_past_idle() {
        let fake = quiet("prompt-until");
        fake.on("tab.write", 2, Phase::After, |world| {
            world.set_state(TAB, "running");
            world.set_state(TAB, "idle");
            world.set_state(TAB, "needs_input");
        });
        let done = done(&fake, &["--timeout", "5", "--until", "needs_input", TEXT])
            .await
            .expect("the turn settled");
        assert_eq!(done.state, TabState::NeedsInput);
    }

    // ------------------------------------------------------------------
    // Where the two deadlines meet
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn a_timeout_shorter_than_the_gate_is_a_timeout() {
        let fake = quiet("prompt-global-first");
        let started = std::time::Instant::now();
        let error = done(
            &fake,
            &["--timeout", "0.3", "--activity-timeout", "30", TEXT],
        )
        .await
        .expect_err("nothing ever ran");
        assert_eq!(refused(&error), (4, "timeout"), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(3), "{error:?}");
    }

    #[tokio::test]
    async fn a_timeout_during_the_settled_wait_is_a_timeout() {
        let fake = quiet("prompt-never-settles");
        fake.on("tab.write", 2, Phase::After, |world| {
            world.set_state(TAB, "running");
        });
        let error = done(
            &fake,
            &["--timeout", "0.5", "--activity-timeout", "30", TEXT],
        )
        .await
        .expect_err("the turn never ended");
        assert_eq!(refused(&error), (4, "timeout"), "{error:?}");
    }

    // ------------------------------------------------------------------
    // What it refuses before it dials
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn a_command_line_with_no_budget_is_usage_before_anything_is_dialled() {
        let fake = quiet("prompt-usage");
        for argv in [
            &["--tab", "7", TEXT][..],
            &["--tab", "7", "--timeout", "0", TEXT],
            &["--tab", "7", "--timeout=-1", TEXT],
            &["--tab", "7", "--timeout=inf", TEXT],
            &["--tab", "7", "--timeout=nan", TEXT],
            &[
                "--tab",
                "7",
                "--timeout",
                "5",
                "--activity-timeout",
                "0",
                TEXT,
            ],
            &["--tab", "7", "--no-timeout", "--activity-timeout=-2", TEXT],
        ] {
            let error = exit(&fake, argv, None)
                .await
                .expect_err(&format!("{argv:?}"));
            assert_eq!(refused(&error), (2, "usage"), "{argv:?}: {error:?}");
        }
        assert!(fake.with(|world| world.log.is_empty()));
    }

    #[tokio::test]
    async fn the_missing_tab_is_refused_before_the_missing_timeout() {
        let fake = quiet("prompt-no-tab");
        let error = exit(&fake, &[TEXT], None).await.expect_err("no tab");
        assert_eq!(error, CliError::Usage(crate::NO_TAB.into()));
        assert!(fake.with(|world| world.log.is_empty()));
    }

    #[tokio::test]
    async fn a_host_tab_is_refused_before_anything_is_dialled() {
        let fake = quiet("prompt-host-tab");
        for (argv, env) in [
            (vec!["--tab", "h1.7", "--timeout", "5", TEXT], None),
            (vec!["--timeout", "5", TEXT], Some("h1.7")),
        ] {
            let error = exit(&fake, &argv, env).await.expect_err("a host tab");
            assert_eq!(refused(&error), (2, "usage"), "{error:?}");
            assert!(error.message().contains("host tab h1.7"), "{error:?}");
        }
        assert!(fake.with(|world| world.log.is_empty()));
    }

    /// `tab prompt`'s half of the mutating-verb policy (the table-driven
    /// one in `main.rs` cannot express a compound verb): `--tab` first,
    /// then `ROOST_TAB_ID`, and never the UI's active tab — which this
    /// fake reports as 99.
    #[tokio::test]
    async fn tab_prompt_writes_to_roost_tab_id_or_the_flag() {
        for (argv, env, expected) in [
            (&["--timeout", "0.2", TEXT][..], Some("7"), "7"),
            (
                &["--tab", "8", "--timeout", "0.2", TEXT][..],
                Some("7"),
                "8",
            ),
        ] {
            let fake = quiet(&format!("prompt-tab-{expected}"));
            let error = exit(&fake, argv, env).await.expect_err("nothing ever ran");
            assert_eq!(refused(&error), (4, "timeout"), "{argv:?}: {error:?}");
            let wrote: Vec<String> =
                fake.with(|world| world.writes.iter().map(|(_, tab, _)| tab.clone()).collect());
            assert_eq!(wrote, [expected, expected], "{argv:?}");
        }
    }

    #[test]
    fn no_timeout_and_timeout_cannot_both_be_given() {
        let parsed = <crate::Args as clap::Parser>::try_parse_from([
            "roostctl",
            "tab",
            "prompt",
            "hi",
            "--timeout",
            "3",
            "--no-timeout",
        ]);
        let error = parsed.expect_err("the two conflict");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    // ------------------------------------------------------------------
    // What the socket says back
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn a_refused_write_reaches_the_caller_with_the_servers_own_code() {
        for nth in [1, 2] {
            let fake = quiet(&format!("prompt-refused-{nth}"));
            fake.on("tab.write", nth, Phase::Before, |world| {
                world.instead = Some(Instead::Refuse("not-found", "no tab 7"));
            });
            let error = done(&fake, &["--timeout", "5", TEXT])
                .await
                .expect_err("the write was refused");
            assert_eq!(
                error,
                CliError::Server {
                    code: "not-found".into(),
                    message: "no tab 7".into()
                },
                "write {nth}"
            );
            assert_eq!(fake.with(|world| world.writes.len()), nth);
        }
    }

    #[tokio::test]
    async fn a_server_without_the_stream_is_unsupported() {
        let fake = Fake::without_stream("prompt-no-stream");
        let error = done(&fake, &["--timeout", "5", TEXT])
            .await
            .expect_err("no stream");
        assert_eq!(refused(&error), (1, "unsupported"), "{error:?}");
        assert!(error.message().contains("events.subscribe"), "{error:?}");
        assert!(
            fake.with(|world| world.writes.is_empty()),
            "nothing is written to a tab this verb cannot then watch"
        );
    }

    #[tokio::test]
    async fn a_stream_lost_after_the_writes_is_resolved_again_once() {
        let fake = quiet("prompt-lost");
        fake.on("tab.write", 2, Phase::After, |world| {
            world.end_streams(None);
        });
        fake.on("tab.list", 2, Phase::Before, |world| {
            world.tabs.insert(TAB, "running");
        });
        fake.on("tab.list", 2, Phase::After, |world| {
            world.set_state(TAB, "idle");
        });
        let done = done(&fake, &["--timeout", "5", TEXT])
            .await
            .expect("the turn settled on the second stream");
        assert_eq!(done.state, TabState::Idle);
        assert_eq!(fake.with(|world| world.count("events.subscribe")), 2);
        assert_eq!(
            fake.with(|world| world.writes.len()),
            2,
            "the prompt is written once, whatever happens to the stream"
        );
    }

    #[tokio::test]
    async fn a_stream_lost_twice_is_a_connection_failure() {
        let fake = quiet("prompt-lost-twice");
        fake.hook(|world, op, phase| {
            if op == "tab.list" && phase == Phase::After {
                world.end_streams(None);
            }
        });
        let error = done(&fake, &["--timeout", "5", TEXT])
            .await
            .expect_err("the stream never survived");
        assert_eq!(refused(&error), (1, "connection"), "{error:?}");
        assert!(error.message().contains("lost twice"), "{error:?}");
        assert_eq!(fake.with(|world| world.writes.len()), 2);
    }

    // ------------------------------------------------------------------
    // What it prints
    // ------------------------------------------------------------------

    #[test]
    fn the_report_names_the_tab_both_durations_and_the_state() {
        let started = Done {
            tab_id: 7,
            started_after: Some(Duration::from_millis(12)),
            state: TabState::Idle,
            after: Duration::from_millis(3400),
        };
        assert_eq!(
            started.json(),
            json!({
                "tab_id": "7",
                "started_after_ms": 12,
                "settled": { "state": "idle" },
                "after_ms": 3400,
            })
        );
        assert_eq!(
            started.line(),
            "prompted: tab 7 started after 12ms, idle after 3400ms"
        );

        let skipped = Done {
            started_after: None,
            state: TabState::NeedsInput,
            ..started
        };
        assert_eq!(skipped.json()["started_after_ms"], json!(null));
        assert_eq!(
            skipped.line(),
            "prompted: tab 7 was already running, needs_input after 3400ms"
        );
    }
}
