//! When an SSH host's dropped link is worth dialing again, and for how
//! long — plan 040 §3.3 and §3.5.
//!
//! Pure: no clock, no sockets, no environment beyond one read-once test
//! seam. The delay takes its jitter as an argument and the verdict takes
//! the failure as a value, so every rule below — a changed host key is
//! never retried, evidence we could not read is never retried, the
//! ladder settles at ten attempts — is a unit test rather than a night
//! spent watching a sidebar.
//!
//! The policy is deliberately not a function of `Option<SshFailure>`: a
//! missing family means two unrelated things (§2.5), and which one it is
//! decides the verdict. [`DropInput`] is that distinction made
//! un-collapsible.

use std::sync::OnceLock;
use std::time::Duration;

use roost_ipc::ssh::SshFailure;

use super::state::Backoff;
use super::ConnectFailure;

/// First retry delay after an SSH drop.
///
/// Localhost's 250ms is right for probing a unix socket on this machine
/// and wrong for a TCP + auth handshake: at that base the first three
/// attempts land before a re-associating radio has a route.
const SSH_BACKOFF_BASE: Duration = Duration::from_secs(1);

/// Ceiling on the SSH retry delay — the same value localhost uses, for
/// the same reason: a link that has been down for half a minute is not
/// coming back within the second, and a client dialing every 30s costs
/// nothing while still noticing when it does.
const SSH_BACKOFF_CAP: Duration = Duration::from_secs(30);

/// How many attempts one outage gets before the host settles to
/// `Disconnected { retry_in: None }` and waits for ↻ Reconnect.
///
/// **Attempts, not elapsed time.** A suspended laptop spends zero
/// attempts, so it wakes into a live ladder rather than a settled host;
/// an elapsed budget would expire during the sleep. It is also the
/// honest bound on the failure being designed against — a retry storm is
/// measured in handshakes, not minutes. Ten bounds a genuinely-gone host
/// at ten SSH handshakes per outage per host, which no `sshd`, rate
/// limiter or `fail2ban` will notice.
///
/// The sleeps alone are 75–150s, but a full give-up at these values is
/// nearer **6–8 minutes**: every attempt also pays the establish's
/// `ConnectTimeout 15`, the previous tunnel's teardown, the lease probe
/// and the dial. That cost is why the two overrides below exist.
const SSH_ATTEMPT_BUDGET: u32 = 10;

/// Test seam: the attempt budget, so a lane can reach the give-up in
/// seconds instead of the 6–8 minutes production values cost.
const ATTEMPTS_ENV: &str = "ROOST_SSH_RECONNECT_ATTEMPTS";

/// Test seam: the base delay, same reason.
const BASE_MS_ENV: &str = "ROOST_SSH_RECONNECT_BASE_MS";

/// What dropped, in the one shape that keeps `family == None`'s two
/// meanings apart.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DropInput<'a> {
    /// A session-level drop (`apply_state`'s `Disconnected` arm): the
    /// family the overlay just folded in, or `None` for a bare bridge
    /// EOF — the ordinary dropped link, and the headline case this whole
    /// slice exists for.
    Session(Option<&'a SshFailure>),
    /// A failed establish (`tunnel_ready`'s `Err` arm). Here a `None`
    /// family means `SshTunnelError::Local` — *our* failure, not the far
    /// side's.
    Establish(&'a ConnectFailure),
}

/// What the ladder decided about one drop.
///
/// An enum rather than `Option<Duration>` because the band renders three
/// different lines (§3.8) and an `Option` cannot tell "we gave up after
/// ten tries" from "this was never eligible".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Dial again in `delay`. `attempt` is the 1-based human number
    /// §3.8 renders as `(3/10)`.
    Retry { delay: Duration, attempt: u32 },
    /// The budget is spent. The host settles; ↻ Reconnect never left.
    Exhausted { attempts: u32 },
    /// This failure is not one a retry can fix — or is one no retry
    /// should be spent on.
    NonRetryable,
}

/// Whether a failure is worth dialing again.
///
/// | Input | Retry? | Why |
/// |---|---|---|
/// | `Session(None)` | yes | A bare bridge EOF: the ordinary dropped link. |
/// | `Transport(_)`, either shape | yes | Classified transport death. |
/// | `Auth` | no | `BatchMode=yes` means there is no prompt to answer, and repeats risk `MaxAuthTries`. |
/// | `HostKeyUnknown` | no | Needs a person to review and accept a key. |
/// | `ChangedHostKey` | never | Retrying a possible machine-in-the-middle in a loop is a security misfeature. |
/// | `NotFound` | no | Nothing to exec, and a retry cannot install it. |
/// | `NoSession` | no | Auto-reconnect never auto-spawns. |
/// | `Establish(family: None)` | no | `SshTunnelError::Local`: no `ssh` binary, an unwritable scratch dir, a bridge that would not bind. |
///
/// `truncated` outranks all of it. A drain that expired or a byte cap
/// that ate the leading bytes leaves an empty-ish tail, which
/// `classify_ssh_failure` reads as `Transport(None)` — so without this
/// check a changed host key whose stderr never arrived becomes a
/// ten-attempt ladder against a possible MITM. *When we could not read
/// the evidence, we do not retry*: the fail-safe direction, at the cost
/// of one manual ↻ in a rare case.
///
/// `truncated` is a parameter because only one of the two shapes has a
/// carrier for it: a session drop reads it off the tunnel's
/// `RecordedFailure`, which never reaches this module, while an
/// establish's already rides on its [`ConnectFailure`]. So the two are
/// **OR'd, not trusted separately** — a caller that passes `false`
/// alongside a failure whose own flag is set must not be able to talk
/// this function into a retry. The rule fails closed or it is not a
/// security rule.
///
/// The `match` has no wildcard arm, so a new [`SshFailure`] family
/// breaks the build here as well as at `offer_for`. Deliberate: a family
/// nobody decided about must not inherit a verdict.
pub(crate) fn retryable(input: DropInput<'_>, truncated: bool) -> bool {
    let carried = match input {
        DropInput::Session(_) => false,
        DropInput::Establish(failure) => failure.truncated,
    };
    if truncated || carried {
        return false;
    }
    let family = match input {
        DropInput::Session(None) => return true,
        DropInput::Session(Some(family)) => family,
        DropInput::Establish(failure) => match failure.family.as_ref() {
            None => return false,
            Some(family) => family,
        },
    };
    match family {
        SshFailure::Transport(_) => true,
        SshFailure::Auth => false,
        SshFailure::HostKeyUnknown => false,
        SshFailure::ChangedHostKey => false,
        SshFailure::NotFound => false,
        SshFailure::NoSession => false,
    }
}

/// One host's outage: the backoff it is walking and how far along it is.
///
/// Created at the first drop and destroyed by any successful connect, any
/// explicit attempt, a disconnect, a give-up or a terminal settle — so a
/// ladder never spans two outages, and the attempt number the band shows
/// is the number of times *this* outage has dialed.
#[derive(Debug)]
pub(crate) struct ReconnectLadder {
    backoff: Backoff,
    attempts: u32,
    budget: u32,
}

impl Default for ReconnectLadder {
    fn default() -> Self {
        Self {
            backoff: Backoff::new(ssh_backoff_base(), SSH_BACKOFF_CAP),
            attempts: 0,
            budget: ssh_attempt_budget(),
        }
    }
}

impl ReconnectLadder {
    /// The verdict for one drop, and the ladder advances if it retries.
    ///
    /// Retryability is asked first, so a changed host key reads as
    /// [`Decision::NonRetryable`] even on an outage that had already
    /// spent its budget — "gave up trying" and "must not be tried" are
    /// different things to tell a user about a possible MITM.
    pub(crate) fn next(&mut self, input: DropInput<'_>, truncated: bool, jitter: f64) -> Decision {
        if !retryable(input, truncated) {
            return Decision::NonRetryable;
        }
        if self.attempts >= self.budget {
            return Decision::Exhausted {
                attempts: self.attempts,
            };
        }
        let delay = self.backoff.next_delay(jitter);
        self.attempts += 1;
        Decision::Retry {
            delay,
            attempt: self.attempts,
        }
    }

    /// Back to the base delay and attempt zero.
    pub(crate) fn reset(&mut self) {
        self.backoff.reset();
        self.attempts = 0;
    }

    /// How many attempts this outage has spent.
    pub(crate) fn attempts(&self) -> u32 {
        self.attempts
    }

    /// The budget this outage was built with — what §3.8's `(3/10)`
    /// renders as its denominator.
    pub(crate) fn budget(&self) -> u32 {
        self.budget
    }
}

/// [`SSH_ATTEMPT_BUDGET`], or the test override, read once.
fn ssh_attempt_budget() -> u32 {
    static BUDGET: OnceLock<u32> = OnceLock::new();
    *BUDGET.get_or_init(|| parse_attempt_budget(test_override(ATTEMPTS_ENV).as_deref()))
}

/// [`SSH_BACKOFF_BASE`], or the test override, read once.
fn ssh_backoff_base() -> Duration {
    static BASE: OnceLock<Duration> = OnceLock::new();
    *BASE.get_or_init(|| parse_backoff_base(test_override(BASE_MS_ENV).as_deref()))
}

/// An override's raw value, and only under `ROOST_TEST_MODE=1`.
///
/// The gate is the same one every other test seam in the tree uses: a
/// variable that shortens a security-relevant ladder must not be
/// reachable by setting one name in a shipped build's environment.
fn test_override(name: &str) -> Option<String> {
    if !std::env::var("ROOST_TEST_MODE").is_ok_and(|value| value == "1") {
        return None;
    }
    std::env::var(name).ok()
}

/// [`ssh_attempt_budget`]'s rule, over an already-read value — pure, so
/// the policy is pinned without mutating process-global env.
///
/// A zero or unparseable budget falls back to the shipped value rather
/// than being honored: a ladder that gives up before it starts is not
/// what anyone typing this variable meant.
fn parse_attempt_budget(raw: Option<&str>) -> u32 {
    raw.and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|attempts| *attempts > 0)
        .unwrap_or(SSH_ATTEMPT_BUDGET)
}

/// [`ssh_backoff_base`]'s rule, over an already-read value. A zero base
/// would spin, so it falls back like any other nonsense.
fn parse_backoff_base(raw: Option<&str>) -> Duration {
    raw.and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(SSH_BACKOFF_BASE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAMILIES: [SshFailure; 6] = [
        SshFailure::ChangedHostKey,
        SshFailure::HostKeyUnknown,
        SshFailure::Auth,
        SshFailure::NoSession,
        SshFailure::NotFound,
        SshFailure::Transport(None),
    ];

    fn establish(family: Option<SshFailure>) -> ConnectFailure {
        match family {
            Some(family) => ConnectFailure::classified("host-a", family),
            None => ConnectFailure::unclassified("no ssh binary on PATH"),
        }
    }

    /// The whole verdict table in one place (§3.3 A and B), so the rule
    /// is read as a table rather than reconstructed from a dozen tests —
    /// and so a family whose verdict is quietly flipped fails here with
    /// its own name in the message.
    #[test]
    fn every_input_gets_the_verdict_the_policy_pinned() {
        let transport_line = SshFailure::Transport(Some("connection reset".into()));
        let session_cases: [(Option<&SshFailure>, bool); 8] = [
            (None, true),
            (Some(&SshFailure::Transport(None)), true),
            (Some(&transport_line), true),
            (Some(&SshFailure::Auth), false),
            (Some(&SshFailure::HostKeyUnknown), false),
            (Some(&SshFailure::ChangedHostKey), false),
            (Some(&SshFailure::NotFound), false),
            (Some(&SshFailure::NoSession), false),
        ];
        for (family, expected) in session_cases {
            let input = DropInput::Session(family);
            assert_eq!(retryable(input, false), expected, "{input:?}");
        }

        let establish_cases: [(Option<SshFailure>, bool); 7] = [
            (Some(SshFailure::Transport(None)), true),
            (Some(SshFailure::Transport(Some("no route".into()))), true),
            (Some(SshFailure::Auth), false),
            (Some(SshFailure::HostKeyUnknown), false),
            (Some(SshFailure::ChangedHostKey), false),
            (Some(SshFailure::NotFound), false),
            (Some(SshFailure::NoSession), false),
        ];
        for (family, expected) in establish_cases {
            let failure = establish(family);
            let input = DropInput::Establish(&failure);
            assert_eq!(retryable(input, false), expected, "{input:?}");
        }

        let local = establish(None);
        let input = DropInput::Establish(&local);
        assert!(!retryable(input, false), "{input:?}");
    }

    /// The distinction the two shapes exist for: a session drop with no
    /// family is the far side's link dying, which is exactly what
    /// auto-reconnect is for; an establish with no family is *this*
    /// side's failure — no `ssh` binary, an unwritable scratch dir — and
    /// ten attempts at it are ten guaranteed-useless handshakes.
    #[test]
    fn a_bare_eof_retries_but_our_own_failure_does_not() {
        assert!(retryable(DropInput::Session(None), false));
        let ours = establish(None);
        assert!(!retryable(DropInput::Establish(&ours), false));
    }

    /// Evidence we could not fully read is never acted on, at every
    /// family and both shapes. Without this the drain fix's truncation
    /// flag is inert and a changed host key whose stderr never arrived
    /// becomes a ten-attempt ladder.
    #[test]
    fn a_truncated_tail_is_never_retryable() {
        assert!(
            !retryable(DropInput::Session(None), true),
            "a bare EOF with a truncated tail"
        );
        let ours = establish(None);
        assert!(!retryable(DropInput::Establish(&ours), true), "{ours:?}");

        for family in FAMILIES {
            let input = DropInput::Session(Some(&family));
            assert!(!retryable(input, true), "{input:?}");

            let failure = establish(Some(family.clone()));
            let input = DropInput::Establish(&failure);
            assert!(!retryable(input, true), "{input:?}");
        }
    }

    /// An establish carries its own truncation flag, so the two inputs
    /// can disagree — and the rule must fail closed when they do. A
    /// caller passing `false` beside a failure whose evidence was cut
    /// short is exactly how a `ChangedHostKey` that degraded into the
    /// `Transport` fallthrough would earn a ten-attempt ladder against a
    /// possible machine-in-the-middle. Found by the codex review of this
    /// commit.
    #[test]
    fn an_establishes_own_truncation_refuses_a_retry_the_caller_did_not_flag() {
        for family in FAMILIES {
            let mut failure = establish(Some(family.clone()));
            failure.truncated = true;
            let input = DropInput::Establish(&failure);
            assert!(
                !retryable(input, false),
                "the failure's own flag is authoritative: {input:?}"
            );
        }
    }

    /// The schedule itself: 1s doubling to a 30s ceiling, and the
    /// attempt number the band renders counts from one.
    #[test]
    fn the_ladder_grows_from_one_second_and_caps_at_thirty() {
        let mut ladder = ReconnectLadder::default();
        let delays: Vec<Duration> = (0..10)
            .map(
                |_| match ladder.next(DropInput::Session(None), false, 1.0) {
                    Decision::Retry { delay, .. } => delay,
                    other => panic!("{other:?} within budget"),
                },
            )
            .collect();
        assert_eq!(
            delays,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
            ]
        );
    }

    /// The jitter spreads a storm of clients without ever spinning: half
    /// the delay is the floor, the full delay is the ceiling, and a
    /// non-finite jitter reads as the midpoint rather than as zero.
    #[test]
    fn the_delay_jitters_within_half_the_step_and_never_reaches_zero() {
        for jitter in [0.0, 0.25, 0.5, 1.0, f64::NAN, -3.0, 7.0] {
            let mut ladder = ReconnectLadder::default();
            let Decision::Retry { delay, attempt } =
                ladder.next(DropInput::Session(None), false, jitter)
            else {
                panic!("the first drop of an outage retries");
            };
            assert_eq!(attempt, 1, "the band's number is 1-based");
            assert!(
                delay >= SSH_BACKOFF_BASE / 2 && delay <= SSH_BACKOFF_BASE,
                "{jitter} produced {delay:?}"
            );
        }
    }

    /// Ten attempts is the budget, so the tenth still dials and the
    /// eleventh is the give-up the band names — `Exhausted` carrying the
    /// count §3.8 renders as "gave up after 10 tries".
    #[test]
    fn the_tenth_attempt_retries_and_the_eleventh_gives_up() {
        let mut ladder = ReconnectLadder::default();
        for expected in 1..=10 {
            assert_eq!(
                ladder.next(DropInput::Session(None), false, 1.0),
                Decision::Retry {
                    delay: ladder_delay(expected),
                    attempt: expected,
                }
            );
        }
        assert_eq!(
            ladder.next(DropInput::Session(None), false, 1.0),
            Decision::Exhausted { attempts: 10 }
        );
        assert_eq!(
            ladder.next(DropInput::Session(None), false, 1.0),
            Decision::Exhausted { attempts: 10 },
            "a settled outage stays settled rather than spending an eleventh"
        );
    }

    /// What the growth of a full-jitter ladder is, so the give-up test
    /// can assert the delay beside the attempt number.
    fn ladder_delay(attempt: u32) -> Duration {
        (SSH_BACKOFF_BASE * 2u32.pow(attempt - 1)).min(SSH_BACKOFF_CAP)
    }

    /// A successful connect clears the outage, and the next one starts
    /// at the base rather than wherever the last left off — otherwise a
    /// host that drops once an hour reaches the ceiling by lunchtime.
    #[test]
    fn reset_returns_the_ladder_to_the_base_delay() {
        let mut ladder = ReconnectLadder::default();
        for _ in 0..6 {
            ladder.next(DropInput::Session(None), false, 1.0);
        }
        assert_eq!(ladder.attempts(), 6);

        ladder.reset();
        assert_eq!(ladder.attempts(), 0);
        assert_eq!(
            ladder.next(DropInput::Session(None), false, 1.0),
            Decision::Retry {
                delay: SSH_BACKOFF_BASE,
                attempt: 1,
            }
        );
    }

    /// A changed host key is refused at every depth — including a fresh
    /// ladder and an already-exhausted one. It must never read as "gave
    /// up trying", which invites another try; retrying a possible
    /// machine-in-the-middle is the one thing this policy will not do.
    #[test]
    fn a_changed_host_key_is_never_retryable_at_any_depth() {
        for spent in 0..=12 {
            let mut ladder = ReconnectLadder::default();
            for _ in 0..spent {
                ladder.next(DropInput::Session(None), false, 1.0);
            }
            assert_eq!(
                ladder.next(
                    DropInput::Session(Some(&SshFailure::ChangedHostKey)),
                    false,
                    1.0
                ),
                Decision::NonRetryable,
                "after {spent} attempts"
            );

            let failure = establish(Some(SshFailure::ChangedHostKey));
            assert_eq!(
                ladder.next(DropInput::Establish(&failure), false, 1.0),
                Decision::NonRetryable,
                "after {spent} attempts, on the establish path"
            );
        }
    }

    /// The overrides exist for C5's lane, not for production: unset,
    /// blank or nonsense all land on §3.5's shipped values, and a zero
    /// budget — a ladder that gives up before it starts — is nonsense
    /// too. Tested over the parsing rule rather than the process
    /// environment, which is global and shared with every other test
    /// thread.
    #[test]
    fn the_overrides_default_to_the_shipped_values_and_reject_nonsense() {
        for raw in [
            None,
            Some(""),
            Some("  "),
            Some("zero"),
            Some("-1"),
            Some("0"),
        ] {
            assert_eq!(parse_attempt_budget(raw), SSH_ATTEMPT_BUDGET, "{raw:?}");
        }
        for raw in [None, Some(""), Some("1.5"), Some("soon"), Some("0")] {
            assert_eq!(parse_backoff_base(raw), SSH_BACKOFF_BASE, "{raw:?}");
        }

        assert_eq!(parse_attempt_budget(Some(" 3 ")), 3);
        assert_eq!(parse_backoff_base(Some("25")), Duration::from_millis(25));
    }

    /// And the overrides actually reach the ladder they parameterize —
    /// a budget of three settles on the fourth drop.
    #[test]
    fn a_shortened_budget_gives_up_where_it_was_told_to() {
        let mut ladder = ReconnectLadder {
            backoff: Backoff::new(Duration::from_millis(25), SSH_BACKOFF_CAP),
            attempts: 0,
            budget: 3,
        };
        for expected in 1..=3 {
            let Decision::Retry { attempt, .. } = ladder.next(DropInput::Session(None), false, 1.0)
            else {
                panic!("attempt {expected} is within a budget of 3");
            };
            assert_eq!(attempt, expected);
        }
        assert_eq!(
            ladder.next(DropInput::Session(None), false, 1.0),
            Decision::Exhausted { attempts: 3 }
        );
        assert_eq!(ladder.budget(), 3);
    }
}
