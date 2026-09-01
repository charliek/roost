//! The consent side of installing `roost-session` on a host over ssh
//! (plan 039 §3.5).
//!
//! `roost-ipc`'s [`roost_ipc::bootstrap`] owns the *doing* — the probe,
//! the source ladder, the staged install, the wire stop, the start. This
//! module owns the *deciding*: what a probe outcome plus a session state
//! means the job should do, what the card says about it before anybody
//! has agreed to anything, and who is allowed to be asked at all.
//!
//! Pure on purpose, like [`super::host_notice`] next door. The action
//! matrix and the copy are the two halves that must be right for every
//! combination, and a table test is the only way to say that once — the
//! adapter in `app.rs` renders whatever these answer and adds nothing.
//!
//! The one non-pure part is at the bottom: two `async fn`s that drive a
//! [`BootstrapJob`] on the engine runtime. They exist here rather than in
//! `app.rs` so the ladder they run and the plan the dialog promised are
//! read off the same value.

use std::collections::HashMap;

use roost_ipc::bootstrap::{
    BootstrapError, BootstrapJob, BootstrapOptions, IdentityGate, Probe, ProbeOutcome, RemoteArch,
};
use roost_ipc::messages::{SessionBinaryIdentity, SESSION_PROTOCOL_VERSION};
use roost_ipc::ssh::{SshTarget, SshTunnelOptions};

/// The triple a remote `roost-session` has to equal for this client to
/// install it, and the one the running session is judged against
/// afterwards.
///
/// Built here rather than in `roost-ipc` because `libghostty_build`
/// comes from `roost_vt`, and `roost-ipc` deliberately has no dependency
/// on it (plan 039 §3.1). `BootstrapOptions` takes it as an injected
/// value for exactly that reason.
pub(crate) fn client_identity() -> SessionBinaryIdentity {
    SessionBinaryIdentity {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        session_protocol: SESSION_PROTOCOL_VERSION,
        libghostty_build: roost_vt::libghostty_build(),
    }
}

/// Whether a session is running on the far side, as the *entry point*
/// knows it — not as the probe does.
///
/// The probe reads a disk. This is the other half of the matrix, and it
/// comes from why we are here at all: a `NotFound`/`NoSession` connect
/// failure means nothing is serving, and a build-mismatch dialog means
/// something is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionState {
    /// Nothing is serving over there.
    NoSession,
    /// A session is up — and this client cannot talk to it.
    Running,
}

/// Whether a failed connect has an offer behind it — and, when it does,
/// what the matrix's second input is.
///
/// Three rules, and all three are refusals:
///
/// * **Only a person is offered anything.** An IPC `host.connect` from
///   `roostctl` arrives as the same `ConnectMode::Dial` a click does, so
///   attendedness cannot tell them apart and
///   [`RequestOrigin`](crate::host_conn::RequestOrigin) is the only
///   place the difference survives. Raising a modal at a machine is the
///   one thing this whole flow must never do (plan 039 §3.5's
///   non-interactive refusal), and the launch-time auto-reconnect is
///   `Ipc` for the same reason — nobody is sitting in front of it.
/// * **Only two families have an answer.** `NotFound` means nothing to
///   exec over there and `NoSession` means a binary with nothing running
///   — both of which Roost can fix. A changed host key, a refused
///   authentication or a dead link are not offers, and dressing them up
///   as one would put an install button under a
///   machine-in-the-middle warning.
/// * **Only a connect that never worked.** `reached_connected` is the
///   difference between a *first* connect (the establish succeeds and
///   the per-connection exec is what returns `NotFound` — the primary
///   path into this whole flow) and a session the user has been working
///   in for hours going away underneath them. Both arrive as the same
///   `Disconnected` transition carrying the same family, and
///   `origin` cannot separate them: it is the origin of the establish,
///   still `User` long after. Without this, a `roostctl session stop`
///   on the far side would throw a consent card over whatever the user
///   was typing into an unrelated local tab.
///
/// Both families map to [`SessionState::NoSession`]: neither can be
/// true while something is serving. Which of the three cards to raise
/// is then the *probe's* answer, not this one's.
pub(crate) fn offer_for(
    origin: Option<crate::host_conn::RequestOrigin>,
    failure: Option<&roost_ipc::ssh::SshFailure>,
    reached_connected: bool,
) -> Option<SessionState> {
    if origin != Some(crate::host_conn::RequestOrigin::User) || reached_connected {
        return None;
    }
    match failure? {
        roost_ipc::ssh::SshFailure::NotFound | roost_ipc::ssh::SshFailure::NoSession => {
            Some(SessionState::NoSession)
        }
        roost_ipc::ssh::SshFailure::ChangedHostKey
        | roost_ipc::ssh::SshFailure::HostKeyUnknown
        | roost_ipc::ssh::SshFailure::Auth
        | roost_ipc::ssh::SshFailure::Transport(_) => None,
    }
}

/// What the entry point knew when it decided to ask, carried through
/// the probe and into the card so confirming can check it is all still
/// true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfferContext {
    /// The matrix's second input, as the entry point knows it.
    pub(crate) session: SessionState,
    /// The session over there is *newer* than this client — a downgrade
    /// warning, only ever known for a protocol skew.
    pub(crate) session_is_newer: bool,
    /// The classified failure the offer is answering, where a connect
    /// attempt is what produced it. `None` for the upgrade prompt's
    /// remote branch (a running session, not a failure) and for the Add
    /// Host dialog, which verified without ever dialing.
    pub(crate) failure: Option<roost_ipc::ssh::SshFailure>,
}

/// The far side at the moment a card is confirmed, reduced to what the
/// offer's honesty depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveState {
    /// A session is up that this client cannot talk to — the state the
    /// `Running` rows were planned against.
    NeedsRestart,
    /// Nothing is serving. `qualifies` is what a connect attempt on
    /// record still says about it; `None` means nothing dialed at all,
    /// which is the Add Host entry's normal shape rather than a host
    /// that moved.
    Cold { qualifies: Option<bool> },
    /// Connecting, connected, taken over, stopped — anything the card
    /// was not planned against.
    Other,
}

/// Whether the card the user is confirming still describes the far
/// side.
///
/// The card is a snapshot, deliberately: it must not rewrite itself
/// mid-read. What that costs is a window in which the far side moves,
/// and both directions of that window are dangerous rather than merely
/// stale:
///
/// * A `Running` plan **stops a session**. If the host reconnected
///   while the card was up, that session is healthy and attached, and
///   confirming would reap every shell on it for a mismatch that no
///   longer exists.
/// * A cold plan skips the stop entirely. If a session started while
///   the card was up, the install lands under a live process and the
///   job fails at `post_start_identify` — loudly, but describing the
///   wrong phase.
///
/// So the question is asked again at the moment the answer is acted on,
/// exactly as `host_restart_confirmed` does with its own prompt.
pub(crate) fn offer_still_stands(offered: SessionState, live: LiveState) -> bool {
    match (offered, live) {
        (SessionState::Running, LiveState::NeedsRestart) => true,
        (SessionState::NoSession, LiveState::Cold { qualifies }) => qualifies.unwrap_or(true),
        _ => false,
    }
}

/// Which of the three cards the user is looking at.
///
/// The variant is a fact about the *binary on disk*: nothing usable,
/// something that is not this build, or this build exactly. What that
/// then implies about steps lives in [`BootstrapPlan`], because the two
/// genuinely come apart — a compatible binary under a stale running
/// session needs a restart and no install at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootstrapVariant {
    /// Nothing usable is over there.
    Install,
    /// Something is, and it is not this build — or it is, and the
    /// session running off it is not.
    Update,
    /// This build is already installed and nothing is serving.
    Start,
}

impl BootstrapVariant {
    /// The primary button.
    pub(crate) fn confirm_label(self) -> &'static str {
        match self {
            Self::Install => "Install",
            Self::Update => "Update",
            Self::Start => "Start",
        }
    }

    /// What `app.dialog_dump` reports. Here rather than in the adapter,
    /// so the wire spelling is one more thing this module answers and
    /// `app.rs` only renders.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Update => "update",
            Self::Start => "start",
        }
    }
}

/// Where `roost-session start` execs from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StartFrom {
    /// The destination this job's own install just wrote.
    Installed,
    /// The rung the probe found, verbatim. A host whose only
    /// `roost-session` is a deb at `/usr/bin` must not be started through
    /// a `~/.local/bin` that does not exist.
    Probed(String),
}

/// What a confirmed bootstrap will do, in order.
///
/// Nothing optional is left implicit: `stop` implies the await-gone that
/// follows it, and `install` decides both which path is started and how
/// strictly the session that comes up is judged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BootstrapPlan {
    pub(crate) variant: BootstrapVariant,
    pub(crate) install: bool,
    /// Stop the running session (and wait for it to actually go) before
    /// starting. False when nothing is serving — a stop of nothing is
    /// success, but a round trip that can only succeed is one the user
    /// waits through for no reason.
    pub(crate) stop: bool,
    pub(crate) start: StartFrom,
    pub(crate) gate: IdentityGate,
    /// What the probe actually found on disk, wherever it found it —
    /// `None` when nothing usable is over there at all.
    ///
    /// Carried for the copy rather than for the steps, and carried at
    /// all because the destination and the find are routinely *different
    /// files*: a deb-installed host answers `Mismatch` about
    /// `/usr/bin/roost-session` while the install writes
    /// `~/.local/bin/roost-session`. A card that says "will replace what
    /// is at ~/.local/bin/roost-session" there is describing a file that
    /// does not exist, and hiding that the stale one survives.
    pub(crate) found: Option<String>,
}

/// Where the binary will be once this plan has run — the card's
/// "where", and the only thing that answers it.
///
/// Here rather than in the adapter because the two branches are the
/// plan's own: a flow that installs writes the fixed destination, and a
/// flow that writes nothing starts the rung the probe found. `~` rather
/// than the `$HOME` the install script uses — the card is read by a
/// person.
pub(crate) fn card_dest(plan: &BootstrapPlan) -> String {
    match &plan.start {
        StartFrom::Probed(path) => path.clone(),
        StartFrom::Installed => format!("~{}", roost_ipc::bootstrap::INSTALL_DEST_SUFFIX),
    }
}

/// [`card_dest`]'s far-side spelling: the same destination with `$HOME`
/// expanded to the answer the probe brought back
/// ([`roost_ipc::bootstrap::Probe::home`]).
///
/// The one thing [`BootstrapPlan::found`] can be *compared* against.
/// The probe reports a shell-expanded absolute path — rung 1 comes back
/// as `/home/u/.local/bin/roost-session` — so comparing it to
/// [`card_dest`]'s reader-facing `~/.local/bin/roost-session` never
/// matched, and every in-place upgrade told the user the file it was
/// about to overwrite (and back up) was "left where it is".
pub(crate) fn dest_on_disk(plan: &BootstrapPlan, home: &str) -> String {
    match &plan.start {
        StartFrom::Probed(path) => path.clone(),
        StartFrom::Installed => format!("{home}{}", roost_ipc::bootstrap::INSTALL_DEST_SUFFIX),
    }
}

/// The action matrix (plan 039 §3.5), as one function.
///
/// Two inputs, six rows, and the interesting ones are the asymmetries:
///
/// * A mismatch with **no** session running installs and starts, and
///   skips the stop — there is nothing over there to stop.
/// * A *compatible* binary under a running session installs **nothing**.
///   The disk is already right; what is stale is the process, so the job
///   is a restart.
/// * The post-start gate is the full triple only when this job wrote the
///   binary. Start-only asks the runtime attach question instead, which
///   is exactly the set of sessions this client can then talk to — see
///   [`IdentityGate`].
pub(crate) fn plan_bootstrap(outcome: &ProbeOutcome, session: SessionState) -> BootstrapPlan {
    let running = session == SessionState::Running;
    match outcome {
        // Nothing on disk to start from, whatever is or is not serving.
        // (`Missing` with a session running cannot happen through either
        // entry point — a session implies a binary — but the row is
        // spelled rather than left to a wildcard, because a matrix with
        // a hole in it is how a later entry point gets a silent default.)
        ProbeOutcome::Missing => BootstrapPlan {
            variant: if running {
                BootstrapVariant::Update
            } else {
                BootstrapVariant::Install
            },
            install: true,
            stop: running,
            start: StartFrom::Installed,
            gate: IdentityGate::Installed,
            found: None,
        },
        ProbeOutcome::Mismatch { path, .. } => BootstrapPlan {
            variant: BootstrapVariant::Update,
            install: true,
            stop: running,
            start: StartFrom::Installed,
            gate: IdentityGate::Installed,
            found: Some(path.clone()),
        },
        ProbeOutcome::Compatible { path } => BootstrapPlan {
            variant: if running {
                BootstrapVariant::Update
            } else {
                BootstrapVariant::Start
            },
            install: false,
            stop: running,
            start: StartFrom::Probed(path.clone()),
            gate: IdentityGate::Existing,
            found: Some(path.clone()),
        },
    }
}

/// The card's three lines, composed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BootstrapCopy {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) confirm: &'static str,
}

/// Everything the copy is a function of. A struct rather than seven
/// positional arguments because two of them are booleans that read
/// identically at a call site and mean opposite things.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CopyInputs<'a> {
    pub(crate) label: &'a str,
    pub(crate) identity: &'a SessionBinaryIdentity,
    /// Where the binary will be, after: the install destination, or the
    /// rung a start-only flow found. Written for a person, so `~`.
    pub(crate) dest: &'a str,
    /// The same destination as the *far side* spells it
    /// ([`dest_on_disk`]) — never shown, only compared against
    /// [`BootstrapPlan::found`], which is a shell-expanded path.
    pub(crate) dest_on_disk: &'a str,
    /// [`roost_ipc::bootstrap::SourcePreview::describe`]'s answer.
    pub(crate) source: &'a str,
    pub(crate) plan: &'a BootstrapPlan,
    /// The session over there is *newer* than this client. Only ever
    /// known for a protocol skew — two libghostty build strings that
    /// disagree are merely different (see `host_notice::vintage`).
    pub(crate) session_is_newer: bool,
}

/// What the consent card says.
///
/// Three rules it keeps, and each of them is a rule because the
/// alternative is a dialog that lies:
///
/// * It names **what** (the exact triple), **where** (the absolute path
///   on the named host) and **from where** — the last one being the
///   caller's already-honest [`CopyInputs::source`], never a friendlier
///   paraphrase of it. Where the destination and
///   [`BootstrapPlan::found`] are different files — the deb-installed
///   host, which is the common case — it says so, including that the
///   copy already there is *shadowed rather than removed* (plan 039
///   §9's known risk). "Will replace what is at X" is only written
///   where X is genuinely what gets overwritten.
/// * It warns that shells die **only when a session is actually
///   running**. Row 2 of the matrix — a mismatched binary with nothing
///   serving — installs over a cold host, and telling that user their
///   shells will end would be a warning about nothing.
/// * It says when the install is a **downgrade**, and still offers it.
///   The user may have a reason; what they may not have is the fact
///   hidden from them.
pub(crate) fn bootstrap_copy(inputs: CopyInputs<'_>) -> BootstrapCopy {
    let CopyInputs {
        label,
        identity,
        dest,
        dest_on_disk,
        source,
        plan,
        session_is_newer,
    } = inputs;
    let build = format!(
        "roost-session {} ({})",
        identity.app_version, identity.libghostty_build
    );

    let (title, mut body) = match plan.variant {
        BootstrapVariant::Install => (
            format!("Install roost-session on {label}?"),
            format!("{build} will be installed to {dest} on {label}, from {source}."),
        ),
        BootstrapVariant::Update if plan.install => (
            format!("Update roost-session on {label}?"),
            match plan.found.as_deref() {
                // The destination is what the probe found: a genuine
                // overwrite, and the only case that may say so. Compared
                // against the far side's spelling of the destination —
                // `dest` itself is `~`-folded for the reader and would
                // never equal a probe's expanded path.
                Some(found) if found == dest_on_disk => {
                    format!("{build} will replace what is at {dest} on {label}, from {source}.")
                }
                // A different rung — `/usr/bin/roost-session` from a
                // deb, most often. Nothing at the destination is
                // replaced, and the copy over there is left alone.
                Some(found) => format!(
                    "{build} will be installed to {dest} on {label}, from {source}. It goes \
                     ahead of the {found} already there; that copy is left where it is."
                ),
                // Nothing usable anywhere, and a session running off
                // something this client cannot identify.
                None => format!("{build} will be installed to {dest} on {label}, from {source}."),
            },
        ),
        // The disk is already right; only the process is stale.
        BootstrapVariant::Update => (
            format!("Update roost-session on {label}?"),
            format!(
                "{dest} on {label} is already {build}. Nothing will be installed — the session \
                 running there is the part that is out of date."
            ),
        ),
        BootstrapVariant::Start => (
            format!("Start roost-session on {label}?"),
            format!("{build} is already installed at {dest} on {label}. Nothing will be written."),
        ),
    };

    if plan.stop {
        body.push(' ');
        body.push_str(
            "Updating stops the running session — shells running in it end; tabs and layout are \
             kept.",
        );
    }
    if session_is_newer {
        body.push(' ');
        body.push_str(&format!(
            "The session on {label} was started by a newer Roost, so this would install an older \
             build; upgrading this Roost is likely the fix."
        ));
    }

    BootstrapCopy {
        title,
        body,
        confirm: plan.variant.confirm_label(),
    }
}

/// The consent dialog's contents, and everything confirming it needs.
///
/// Carried rather than re-derived at confirm time, for
/// `HostDialog::ConfirmStop`'s reason: the card describes the far side as
/// the probe found it, and a connect landing underneath must not rewrite
/// the question mid-read. What *is* re-read at confirm is the things a
/// snapshot cannot vouch for — that the host is still saved, and that no
/// job already holds its target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BootstrapDraft {
    pub(crate) saved_id: String,
    pub(crate) label: String,
    /// [`SshTarget::token`] — proof at confirm time that the saved host
    /// still names the machine the card was composed about.
    pub(crate) token: String,
    /// [`SshTarget::claim_key`] — what the job claim is keyed on.
    pub(crate) claim: String,
    pub(crate) arch: RemoteArch,
    pub(crate) plan: BootstrapPlan,
    pub(crate) copy: BootstrapCopy,
    /// What the entry point knew when it asked, for
    /// [`offer_still_stands`].
    pub(crate) offer: OfferContext,
}

/// What a probe answer is allowed to do at the moment it lands.
///
/// It is a multi-second ssh round trip, and every one of the three
/// refusals below is something the user can have done in that window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeLanding {
    /// Raise the card.
    Offer,
    /// Nothing is waiting for this answer: cancelled, superseded by a
    /// second Connect, or invalidated by a connect/disconnect that
    /// started underneath it.
    Stale,
    /// The host is gone, or now names a different machine.
    Moved,
    /// A modal is already on screen. Replacing it would take the
    /// keyboard away from a question the user is mid-answer to — and
    /// because Enter routes to whichever host dialog is visible, an
    /// Enter aimed at "Stop Session" would land on an install button.
    /// The offer waits for the band instead.
    Deferred,
}

/// The three facts a landing probe answer is judged on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Landed {
    /// The answer is the one still being waited on
    /// ([`BootstrapsInFlight::claim_probe`]).
    pub(crate) claimed: bool,
    /// The saved host is still there, and its target still classifies
    /// to the same ssh machine.
    pub(crate) same_host: bool,
    /// A host modal is on screen.
    pub(crate) dialog_open: bool,
}

impl Landed {
    /// The refusals in order: a reply nobody awaits is not about
    /// anything, a host that moved is not about *this*, and a modal
    /// already up is not this offer's to take.
    pub(crate) fn landing(self) -> ProbeLanding {
        if !self.claimed {
            ProbeLanding::Stale
        } else if !self.same_host {
            ProbeLanding::Moved
        } else if self.dialog_open {
            ProbeLanding::Deferred
        } else {
            ProbeLanding::Offer
        }
    }
}

/// The claims that keep one host from being bootstrapped twice.
///
/// Two of them, keyed differently on purpose:
///
/// * **Probes** are keyed on the saved id and hold a *generation*, so a
///   probe answer that arrives after the user cancelled, re-clicked, or
///   opened another host's dialog lands nowhere. Field equality could
///   not decide that — the same host probed twice is the same key — so
///   the generation is what proves a reply belongs to the request still
///   being waited on, exactly as `AddHostDraft::claim_verify` does.
/// * **Jobs** are keyed on [`SshTarget::claim_key`], not the saved id
///   and not the token. Two saved hosts can name one box under two
///   labels *and two spellings*, and two installs racing on one
///   `~/.local/bin/roost-session` is the failure that makes. See that
///   field for what the key can and cannot merge.
#[derive(Debug, Default)]
pub(crate) struct BootstrapsInFlight {
    probes: HashMap<String, u64>,
    jobs: HashMap<String, u64>,
}

impl BootstrapsInFlight {
    /// Arm a probe for `saved_id`, superseding any earlier one.
    ///
    /// Superseding rather than refusing: a second Connect is a fresh
    /// question about a host whose state may have changed, and the first
    /// answer is the one that no longer applies.
    pub(crate) fn begin_probe(&mut self, saved_id: &str, generation: u64) {
        self.probes.insert(saved_id.to_string(), generation);
    }

    /// Claim a probe answer, if it is the one still awaited.
    pub(crate) fn claim_probe(&mut self, saved_id: &str, generation: u64) -> bool {
        if self.probes.get(saved_id) != Some(&generation) {
            return false;
        }
        self.probes.remove(saved_id);
        true
    }

    /// Drop a probe whose question is no longer the one being asked.
    ///
    /// A new connect *replaces* `SshState.origin` and its failure, so a
    /// probe armed under the old attempt would raise a consent card
    /// describing a question the app has already moved past: user
    /// Connect → probe out → an IPC `host.connect` supersedes the
    /// attempt → the card opens anyway, at nobody. Answers whether
    /// there was one to drop, so the caller can clear the band note it
    /// left.
    pub(crate) fn cancel_probe(&mut self, saved_id: &str) -> bool {
        self.probes.remove(saved_id).is_some()
    }

    pub(crate) fn probing(&self, saved_id: &str) -> bool {
        self.probes.contains_key(saved_id)
    }

    /// Claim the job for a target. `false` when one is already running
    /// for that same box, whichever saved host asked.
    pub(crate) fn begin_job(&mut self, claim: &str, generation: u64) -> bool {
        if self.jobs.contains_key(claim) {
            return false;
        }
        self.jobs.insert(claim.to_string(), generation);
        true
    }

    /// Release the claim, if this completion is the one holding it.
    ///
    /// The generation matters even though the token alone would usually
    /// do: a completion that lost its race with a later job for the same
    /// box must not release the later job's claim.
    pub(crate) fn claim_job(&mut self, claim: &str, generation: u64) -> bool {
        if self.jobs.get(claim) != Some(&generation) {
            return false;
        }
        self.jobs.remove(claim);
        true
    }

    pub(crate) fn job_running(&self, claim: &str) -> bool {
        self.jobs.contains_key(claim)
    }
}

// ============================================================================
// The runtime half
// ============================================================================

/// A finished bootstrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BootstrapSuccess {
    /// Set when a shell on the far side would not find *this*
    /// `roost-session` by name. Not a failure — Roost execs the absolute
    /// path — so it rides along for the completion toast to append.
    pub(crate) path_warning: Option<String>,
}

/// Which request a step is answering.
///
/// Carried by both arms of [`BootstrapEvent`], because both can land
/// after the thing that asked has moved on: a probe after a cancel, a
/// job after a second one for the same box was started.
#[derive(Debug)]
pub(crate) struct BootstrapRequest {
    pub(crate) saved_id: String,
    pub(crate) generation: u64,
    pub(crate) label: String,
    /// The registry's spelling, for [`BootstrapError::message`].
    pub(crate) target: String,
    /// [`SshTarget::token`] — proof the saved host still names the same
    /// machine when the answer lands.
    pub(crate) token: String,
    /// [`SshTarget::claim_key`] — what the job claim is keyed on.
    pub(crate) claim: String,
}

/// One bootstrap step reporting back, through the engine feed.
#[derive(Debug)]
pub(crate) enum BootstrapEvent {
    Probed {
        request: BootstrapRequest,
        offer: OfferContext,
        result: Result<Probed, BootstrapError>,
    },
    Finished {
        request: BootstrapRequest,
        result: Result<BootstrapSuccess, BootstrapError>,
    },
}

/// What a read-only look answered, and the "from where" line a card
/// could honestly claim for it.
///
/// The two travel together because the second needs the first's arch
/// *and* a [`BootstrapOptions`], and building one of those walks `$PATH`
/// for a sibling binary — pure, but blocking, and the thread that would
/// otherwise do it is the one drawing frames.
#[derive(Debug)]
pub(crate) struct Probed {
    pub(crate) probe: Probe,
    /// [`roost_ipc::bootstrap::SourcePreview::describe`]'s answer, or
    /// why nothing can supply this build. Read only by a plan that
    /// installs.
    pub(crate) source: Result<String, BootstrapError>,
}

/// Look at the far side, and change nothing.
///
/// A job of its own rather than one held open across the dialog: the
/// user may take a minute to answer, and a `ControlPersist` master left
/// up for it would be an ssh connection nobody asked to keep.
pub(crate) async fn run_probe(
    target: SshTarget,
    ssh: SshTunnelOptions,
    options: BootstrapOptions,
) -> Result<Probed, BootstrapError> {
    let previewed = options.clone();
    let job = BootstrapJob::open(&target, &ssh, options).await?;
    let probe = job.probe().await;
    job.close().await;
    let probe = probe?;
    let source = previewed.source_preview(probe.arch).map(|p| p.describe());
    Ok(Probed { probe, source })
}

/// Run the plan the user agreed to, and nothing else.
///
/// The steps are read off [`BootstrapPlan`] rather than re-derived,
/// which is what makes "the dialog said what it would do" a structural
/// claim instead of a matter of two functions agreeing.
pub(crate) async fn run_bootstrap(
    target: SshTarget,
    ssh: SshTunnelOptions,
    options: BootstrapOptions,
    plan: BootstrapPlan,
    arch: RemoteArch,
) -> Result<BootstrapSuccess, BootstrapError> {
    let job = BootstrapJob::open(&target, &ssh, options).await?;
    let outcome = run_plan(&job, &plan, arch).await;
    // Ordered, and on every path: the master is addressed through a file
    // in the directory the close removes, and `Drop` behind it is
    // blocking.
    job.close().await;
    outcome
}

async fn run_plan(
    job: &BootstrapJob,
    plan: &BootstrapPlan,
    arch: RemoteArch,
) -> Result<BootstrapSuccess, BootstrapError> {
    let mut installed = None;
    if plan.install {
        // Resolution is the job's *first* phase and never the connect
        // path's (plan 039 §3.3): choosing a rung can mean a subprocess
        // and a download, and neither belongs behind a Connect the user
        // has not consented to.
        let source = job.resolve_source(arch).await?;
        tracing::info!(source = %source.describe(), "bootstrap: installing roost-session");
        installed = Some(job.install(&source).await?);
    }
    if plan.stop {
        job.stop_over_the_wire().await?;
        // `session.stop` replies before the old process unlinks its
        // socket; starting on the reply alone loses that race and reads
        // the dying holder as `already-running`.
        job.await_gone().await?;
    }
    let path = match (&installed, &plan.start) {
        (Some(done), _) => done.dest.clone(),
        (None, StartFrom::Probed(path)) => path.clone(),
        // Unreachable through `plan_bootstrap` — every plan that starts
        // from the destination also installs — and stated as a failure
        // rather than an `expect` so a later matrix row cannot panic a
        // UI thread's runtime.
        (None, StartFrom::Installed) => {
            return Err(BootstrapError::Start(
                "nothing was installed, so there is no destination to start".to_string(),
            ))
        }
    };
    job.start(&path).await?;
    job.post_start_identify(plan.gate).await?;
    Ok(BootstrapSuccess {
        path_warning: installed.and_then(|done| done.path_warning),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> SessionBinaryIdentity {
        SessionBinaryIdentity {
            app_version: "0.0.19".into(),
            session_protocol: SESSION_PROTOCOL_VERSION,
            libghostty_build: "ghostty-abc+snapshot.v1".into(),
        }
    }

    /// The common Mismatch: a deb at `/usr/bin`, which is a **different
    /// file** from where the install lands.
    fn mismatch() -> ProbeOutcome {
        ProbeOutcome::Mismatch {
            path: "/usr/bin/roost-session".into(),
            identity: None,
        }
    }

    /// The remote `$HOME` the probe reports back, as every real one
    /// arrives: shell-expanded and absolute. Spelling it `~` here — as
    /// this fixture used to — is what let the dead `found == dest` arm
    /// look covered; the probe never emits that.
    const REMOTE_HOME: &str = "/home/u";

    /// The other Mismatch: a stale copy at the very destination, which
    /// is the only shape a card may call a replacement.
    fn mismatch_at_dest() -> ProbeOutcome {
        ProbeOutcome::Mismatch {
            path: format!("{REMOTE_HOME}{}", roost_ipc::bootstrap::INSTALL_DEST_SUFFIX),
            identity: None,
        }
    }

    fn compatible() -> ProbeOutcome {
        ProbeOutcome::Compatible {
            path: "/usr/bin/roost-session".into(),
        }
    }

    /// **A modal is never raised at a machine.** Every family, both
    /// origins: an IPC-driven connect keeps plan 038's toast-and-band
    /// behavior exactly, whatever it failed on.
    ///
    /// The table form is the point. This is the rule the whole flow is
    /// gated on, and a wildcard arm added later for a family that
    /// happens to be convenient is precisely how it would be lost.
    #[test]
    fn only_a_person_is_ever_offered_a_bootstrap() {
        use crate::host_conn::RequestOrigin;
        use roost_ipc::ssh::SshFailure;

        let families = [
            SshFailure::NotFound,
            SshFailure::NoSession,
            SshFailure::ChangedHostKey,
            SshFailure::HostKeyUnknown,
            SshFailure::Auth,
            SshFailure::Transport(Some("connection reset".into())),
            SshFailure::Transport(None),
        ];
        for family in &families {
            assert_eq!(
                offer_for(Some(RequestOrigin::Ipc), Some(family), false),
                None,
                "{family:?} over IPC must keep the toast and the band, and raise nothing"
            );
            assert_eq!(
                offer_for(None, Some(family), false),
                None,
                "{family:?} for a host with no attempt on record offers nothing"
            );
        }

        // The two that have an answer, and they are the only two.
        assert_eq!(
            offer_for(
                Some(RequestOrigin::User),
                Some(&SshFailure::NotFound),
                false
            ),
            Some(SessionState::NoSession)
        );
        assert_eq!(
            offer_for(
                Some(RequestOrigin::User),
                Some(&SshFailure::NoSession),
                false
            ),
            Some(SessionState::NoSession)
        );
        for family in &families[2..] {
            assert_eq!(
                offer_for(Some(RequestOrigin::User), Some(family), false),
                None,
                "{family:?} is not something an install would fix"
            );
        }
        assert_eq!(
            offer_for(Some(RequestOrigin::User), None, false),
            None,
            "a failure with no far-side family has no remedy over there"
        );
    }

    /// A connection that *worked* and then went away is not an offer.
    ///
    /// The two arrive identically — same `Disconnected` transition, same
    /// `NotFound`/`NoSession` family, same `User` origin, because the
    /// origin is the establish's and stays `User` for as long as the
    /// connection lives. Only "did this connection ever reach
    /// `Connected`" separates a first connect (the offer's primary path)
    /// from `roostctl session stop` on the far side throwing a card over
    /// someone typing in an unrelated local tab.
    #[test]
    fn only_a_connection_that_never_worked_is_an_offer() {
        use crate::host_conn::RequestOrigin;
        use roost_ipc::ssh::SshFailure;

        for family in [SshFailure::NotFound, SshFailure::NoSession] {
            assert_eq!(
                offer_for(Some(RequestOrigin::User), Some(&family), false),
                Some(SessionState::NoSession),
                "a first connect that never came up is the offer's own path: {family:?}"
            );
            assert_eq!(
                offer_for(Some(RequestOrigin::User), Some(&family), true),
                None,
                "a live session going away is not a consent card: {family:?}"
            );
        }
    }

    /// The card is a snapshot, so confirming re-asks. Both directions
    /// are dangerous rather than merely stale: a stale `Running` plan
    /// stops a session that came back healthy, and a stale cold plan
    /// installs under one that started.
    #[test]
    fn confirming_a_card_the_far_side_has_moved_past_does_nothing() {
        use LiveState::{Cold, NeedsRestart, Other};
        use SessionState::{NoSession, Running};

        assert!(offer_still_stands(Running, NeedsRestart));
        assert!(
            !offer_still_stands(Running, Cold { qualifies: None }),
            "the session the plan would stop is gone"
        );
        assert!(
            !offer_still_stands(Running, Other),
            "a host that reconnected has a healthy attached session, and the plan reaps it"
        );

        assert!(
            offer_still_stands(NoSession, Cold { qualifies: None }),
            "the Add Host entry never dialed, so there is nothing on record to still agree with"
        );
        assert!(offer_still_stands(
            NoSession,
            Cold {
                qualifies: Some(true)
            }
        ));
        assert!(
            !offer_still_stands(
                NoSession,
                Cold {
                    qualifies: Some(false)
                }
            ),
            "the failure the offer answered is no longer what the host is saying"
        );
        assert!(
            !offer_still_stands(NoSession, NeedsRestart),
            "a session started underneath: the plan skips the stop and would install under it"
        );
        assert!(!offer_still_stands(NoSession, Other));
    }

    /// A probe answer is a multi-second round trip, and each of these
    /// is something the user can do inside it. The last one is the one
    /// with teeth: `Enter` routes to whichever host dialog is visible,
    /// so a card that displaced a Stop confirmation would take that
    /// Enter.
    #[test]
    fn a_landing_probe_never_takes_a_modal_away_from_the_user() {
        let landed = Landed {
            claimed: true,
            same_host: true,
            dialog_open: false,
        };
        assert_eq!(landed.landing(), ProbeLanding::Offer);
        assert_eq!(
            Landed {
                dialog_open: true,
                ..landed
            }
            .landing(),
            ProbeLanding::Deferred
        );
        assert_eq!(
            Landed {
                same_host: false,
                ..landed
            }
            .landing(),
            ProbeLanding::Moved
        );
        // Staleness is checked first: a reply nobody awaits is not about
        // anything, whatever else is true.
        for same_host in [true, false] {
            for dialog_open in [true, false] {
                assert_eq!(
                    Landed {
                        claimed: false,
                        same_host,
                        dialog_open,
                    }
                    .landing(),
                    ProbeLanding::Stale,
                    "{same_host} / {dialog_open}"
                );
            }
        }
    }

    /// The whole action matrix, row by row (plan 039 §3.5). A table
    /// rather than six tests because the point is that every
    /// combination has an answer and that the answers differ where the
    /// plan says they do.
    #[test]
    fn the_action_matrix_is_the_pinned_table() {
        use SessionState::{NoSession, Running};

        let missing_cold = plan_bootstrap(&ProbeOutcome::Missing, NoSession);
        assert_eq!(missing_cold.variant, BootstrapVariant::Install);
        assert!(missing_cold.install);
        assert!(!missing_cold.stop, "there is nothing over there to stop");
        assert_eq!(missing_cold.start, StartFrom::Installed);
        assert_eq!(missing_cold.gate, IdentityGate::Installed);
        assert_eq!(
            missing_cold.found, None,
            "nothing usable is over there, so the card has no other rung to name"
        );

        // Row 1b: nothing usable on disk *and* something serving. It
        // cannot happen through either entry point today, so the row is
        // pinned rather than left for a later entry point to discover:
        // the button says Update because a session ends, not Install.
        let missing_hot = plan_bootstrap(&ProbeOutcome::Missing, Running);
        assert_eq!(
            missing_hot.variant,
            BootstrapVariant::Update,
            "a row that stops a session is never labelled Install"
        );
        assert!(missing_hot.install);
        assert!(missing_hot.stop);
        assert_eq!(missing_hot.start, StartFrom::Installed);
        assert_eq!(missing_hot.gate, IdentityGate::Installed);
        assert_eq!(missing_hot.found, None);

        // Row 2: a stale binary with nothing serving. The stop is
        // skipped, and that is the row's whole point.
        let stale_cold = plan_bootstrap(&mismatch(), NoSession);
        assert_eq!(stale_cold.variant, BootstrapVariant::Update);
        assert!(stale_cold.install);
        assert!(!stale_cold.stop);
        assert_eq!(stale_cold.start, StartFrom::Installed);
        assert_eq!(stale_cold.gate, IdentityGate::Installed);
        assert_eq!(
            stale_cold.found.as_deref(),
            Some("/usr/bin/roost-session"),
            "the rung the probe found, which is not where the install lands"
        );

        // Row 3: start-only. Nothing is written, so the gate is the
        // runtime attach question rather than the exact triple.
        let ready_cold = plan_bootstrap(&compatible(), NoSession);
        assert_eq!(ready_cold.variant, BootstrapVariant::Start);
        assert!(!ready_cold.install);
        assert!(!ready_cold.stop);
        assert_eq!(
            ready_cold.start,
            StartFrom::Probed("/usr/bin/roost-session".into()),
            "a deb at /usr/bin is not started through a ~/.local/bin that does not exist"
        );
        assert_eq!(ready_cold.gate, IdentityGate::Existing);
        assert_eq!(ready_cold.found.as_deref(), Some("/usr/bin/roost-session"));

        // Row 4: the full ladder.
        let stale_hot = plan_bootstrap(&mismatch(), Running);
        assert_eq!(stale_hot.variant, BootstrapVariant::Update);
        assert!(stale_hot.install);
        assert!(stale_hot.stop);
        assert_eq!(stale_hot.start, StartFrom::Installed);
        assert_eq!(stale_hot.gate, IdentityGate::Installed);
        assert_eq!(stale_hot.found.as_deref(), Some("/usr/bin/roost-session"));

        // Row 5: the disk is already right; only the process is stale.
        let ready_hot = plan_bootstrap(&compatible(), Running);
        assert_eq!(ready_hot.variant, BootstrapVariant::Update);
        assert!(!ready_hot.install, "nothing to install — the disk is right");
        assert!(ready_hot.stop);
        assert_eq!(
            ready_hot.start,
            StartFrom::Probed("/usr/bin/roost-session".into())
        );
        assert_eq!(ready_hot.gate, IdentityGate::Existing);
    }

    /// Every plan that starts from the destination also installs one.
    /// The converse is the interesting half: a plan that installs never
    /// starts a path the probe found, because the install is what
    /// shadows it.
    #[test]
    fn a_plan_never_starts_a_destination_nothing_wrote() {
        for outcome in [ProbeOutcome::Missing, mismatch(), compatible()] {
            for session in [SessionState::NoSession, SessionState::Running] {
                let plan = plan_bootstrap(&outcome, session);
                if plan.start == StartFrom::Installed {
                    assert!(plan.install, "{outcome:?} / {session:?}");
                }
                if plan.install {
                    assert_eq!(plan.start, StartFrom::Installed, "{outcome:?}");
                    assert_eq!(plan.gate, IdentityGate::Installed, "{outcome:?}");
                }
                assert_eq!(
                    plan.stop,
                    session == SessionState::Running,
                    "a stop happens exactly when something is running: {outcome:?}"
                );
            }
        }
    }

    /// The install destination, spelled as the card spells it. Derived
    /// from the shipped constant rather than typed out, so the two
    /// cannot drift.
    fn install_dest() -> String {
        format!("~{}", roost_ipc::bootstrap::INSTALL_DEST_SUFFIX)
    }

    /// **The dest is derived, never asserted-in.** The adapter reads it
    /// off [`card_dest`], and a helper that hardcoded one string would
    /// have made every row's copy test agree with a destination its own
    /// plan contradicts — a Start card whose plan starts
    /// `/usr/bin/roost-session` asserting on `~/.local/bin`.
    fn copy(plan: &BootstrapPlan, source: &str, newer: bool) -> BootstrapCopy {
        bootstrap_copy(CopyInputs {
            label: "pop-os",
            identity: &identity(),
            dest: &card_dest(plan),
            dest_on_disk: &dest_on_disk(plan, REMOTE_HOME),
            source,
            plan,
            session_is_newer: newer,
        })
    }

    /// The two spellings of one destination, and the reason the copy
    /// needs both: a probe's path can only ever equal the expanded one.
    #[test]
    fn the_install_destination_has_a_reader_spelling_and_a_far_side_spelling() {
        let plan = plan_bootstrap(&mismatch_at_dest(), SessionState::NoSession);
        assert_eq!(card_dest(&plan), install_dest());
        assert_eq!(
            dest_on_disk(&plan, REMOTE_HOME),
            format!("{REMOTE_HOME}{}", roost_ipc::bootstrap::INSTALL_DEST_SUFFIX)
        );
        assert_ne!(card_dest(&plan), dest_on_disk(&plan, REMOTE_HOME));
        // A start-only plan writes nothing, so both spellings are the
        // rung the probe found, verbatim.
        let start = plan_bootstrap(&compatible(), SessionState::NoSession);
        assert_eq!(card_dest(&start), dest_on_disk(&start, REMOTE_HOME));
    }

    /// The mapping the adapter uses, both branches. A flow that writes
    /// nothing names the rung it will start; a flow that installs names
    /// the one fixed destination.
    #[test]
    fn the_card_names_where_the_binary_will_actually_be() {
        for outcome in [ProbeOutcome::Missing, mismatch(), mismatch_at_dest()] {
            for session in [SessionState::NoSession, SessionState::Running] {
                let plan = plan_bootstrap(&outcome, session);
                assert_eq!(
                    card_dest(&plan),
                    install_dest(),
                    "every installing row writes the one destination: {outcome:?} / {session:?}"
                );
            }
        }
        for session in [SessionState::NoSession, SessionState::Running] {
            assert_eq!(
                card_dest(&plan_bootstrap(&compatible(), session)),
                "/usr/bin/roost-session",
                "a start-only row names the rung it found, not a path nothing wrote"
            );
        }
    }

    /// The Update card must not claim to replace a file that is not
    /// there. A deb-installed host answers `Mismatch` about
    /// `/usr/bin/roost-session` while the install writes
    /// `~/.local/bin/roost-session` — two different files — and the
    /// stale one survives, shadowed in the exec chain (plan 039 §9).
    ///
    /// The `same` half below is the arm that was **dead in production**:
    /// its fixture used to spell the probe's path `~/…`, which no probe
    /// emits, so an in-place upgrade always took the "goes ahead of"
    /// branch and told the user the file it was about to overwrite was
    /// left alone. [`REMOTE_HOME`] is what a real probe answers with,
    /// and [`dest_on_disk`] is what makes the two comparable.
    #[test]
    fn an_update_says_replace_only_where_it_actually_replaces() {
        let elsewhere = copy(
            &plan_bootstrap(&mismatch(), SessionState::Running),
            "this Roost's own roost-session",
            false,
        );
        assert!(
            elsewhere.body.contains(&format!(
                "will be installed to {} on pop-os",
                install_dest()
            )),
            "{elsewhere:?}"
        );
        assert!(
            elsewhere
                .body
                .contains("ahead of the /usr/bin/roost-session already there"),
            "the card names what it shadows: {elsewhere:?}"
        );
        assert!(
            elsewhere.body.contains("that copy is left where it is"),
            "and that the stale one survives: {elsewhere:?}"
        );
        assert!(
            !elsewhere.body.contains("replace what is at"),
            "nothing at the destination is replaced: {elsewhere:?}"
        );

        let same = copy(
            &plan_bootstrap(&mismatch_at_dest(), SessionState::Running),
            "this Roost's own roost-session",
            false,
        );
        assert!(
            same.body.contains(&format!(
                "will replace what is at {} on pop-os",
                install_dest()
            )),
            "a stale copy at the destination genuinely is replaced: {same:?}"
        );
        assert!(!same.body.contains("already there"), "{same:?}");

        // Nothing usable anywhere: the destination holds nothing, so
        // there is nothing to shadow and nothing to replace.
        let nothing = copy(
            &plan_bootstrap(&ProbeOutcome::Missing, SessionState::Running),
            "this Roost's own roost-session",
            false,
        );
        assert!(
            nothing.body.contains(&format!(
                "will be installed to {} on pop-os",
                install_dest()
            )),
            "{nothing:?}"
        );
        assert!(!nothing.body.contains("already there"), "{nothing:?}");
        assert!(!nothing.body.contains("replace what is at"), "{nothing:?}");
    }

    /// Each variant names what, where and from where — and only the
    /// rows that actually stop a session warn about shells ending.
    #[test]
    fn every_variant_names_what_where_and_from_where() {
        let install = copy(
            &plan_bootstrap(&ProbeOutcome::Missing, SessionState::NoSession),
            "this Roost's own roost-session",
            false,
        );
        assert_eq!(install.confirm, "Install");
        assert!(install.title.starts_with("Install roost-session on pop-os"));
        assert!(install.body.contains("roost-session 0.0.19"), "{install:?}");
        assert!(
            install.body.contains("ghostty-abc+snapshot.v1"),
            "{install:?}"
        );
        assert!(install.body.contains(&install_dest()), "{install:?}");
        assert!(
            install.body.contains("this Roost's own roost-session"),
            "{install:?}"
        );
        assert!(
            !install.body.contains("shells running in it end"),
            "nothing is running: {install:?}"
        );

        let start = copy(
            &plan_bootstrap(&compatible(), SessionState::NoSession),
            "unused",
            false,
        );
        assert_eq!(start.confirm, "Start");
        assert!(start.title.starts_with("Start roost-session on pop-os"));
        assert!(
            start.body.contains("Nothing will be written"),
            "a start writes nothing, and says so: {start:?}"
        );
        assert!(
            start.body.contains("/usr/bin/roost-session"),
            "and names the rung it will actually start, not the install destination: {start:?}"
        );
        assert!(
            !start.body.contains(&install_dest()),
            "a deb-installed host is never told about a ~/.local/bin nothing wrote: {start:?}"
        );
        assert!(
            !start.body.contains("shells running in it end"),
            "{start:?}"
        );
    }

    /// The Update card is where the session-ending warning belongs —
    /// and it belongs there only when a session is actually running.
    #[test]
    fn the_update_card_warns_about_shells_only_when_one_is_running() {
        let hot = copy(
            &plan_bootstrap(&mismatch(), SessionState::Running),
            "the release at https://example.test, checksum-verified",
            false,
        );
        assert_eq!(hot.confirm, "Update");
        assert!(hot.title.starts_with("Update roost-session on pop-os"));
        assert!(
            hot.body.contains(
                "Updating stops the running session — shells running in it end; tabs \
                          and layout are kept."
            ),
            "{hot:?}"
        );
        assert!(
            !hot.body.contains("from downloaded from"),
            "the preposition must not double on the asset rung: {hot:?}"
        );
        assert!(
            hot.body
                .contains("from the release at https://example.test, checksum-verified"),
            "{hot:?}"
        );

        let cold = copy(
            &plan_bootstrap(&mismatch(), SessionState::NoSession),
            "the release at https://example.test, checksum-verified",
            false,
        );
        assert_eq!(cold.confirm, "Update");
        assert!(
            !cold.body.contains("shells running in it end"),
            "row 2 installs over a cold host: {cold:?}"
        );

        // Row 5: the binary is already right, so the card must not claim
        // anything will be written.
        let restart = copy(
            &plan_bootstrap(&compatible(), SessionState::Running),
            "",
            false,
        );
        assert!(
            restart.body.contains("Nothing will be installed"),
            "{restart:?}"
        );
        assert!(
            restart.body.contains("shells running in it end"),
            "{restart:?}"
        );
    }

    /// The options an override-driven preview is read off. Built
    /// literally rather than through `from_env`, so the test is a
    /// statement about the ladder and not about the machine running it.
    fn overridden_options(asset_base: Option<&str>, install_bin: Option<&str>) -> BootstrapOptions {
        BootstrapOptions {
            expected: identity(),
            asset_base: asset_base.map(str::to_string),
            install_bin: install_bin.map(std::path::PathBuf::from),
            sibling_bin: None,
            curl_bin: std::path::PathBuf::from("curl"),
            source: None,
            jail_fs_root: false,
        }
    }

    /// **The wiring, not a literal round trip.** The origin the card
    /// shows is composed by the very
    /// [`BootstrapOptions::source_preview`] the adapter feeds it, so an
    /// overridden `ROOST_SESSION_ASSET_BASE` cannot be masked as
    /// github.com by anything between the two — which is the failure
    /// this rule exists to stop, and the one a hand-written string
    /// passed straight back to its own assertion could never catch.
    #[test]
    fn the_origin_the_card_shows_is_the_origin_the_ladder_predicted() {
        let base = "http://127.0.0.1:9/assets";
        let source = overridden_options(Some(base), None)
            .source_preview(RemoteArch::Amd64)
            .expect("the asset rung previews")
            .describe();
        let card = copy(
            &plan_bootstrap(&ProbeOutcome::Missing, SessionState::NoSession),
            &source,
            false,
        );
        assert!(card.body.contains(base), "{card:?}");
        assert!(
            card.body.contains("ROOST_SESSION_ASSET_BASE"),
            "the card says the base was overridden: {card:?}"
        );
        assert!(
            !card.body.contains("github.com"),
            "a fixture server is never rendered as the release origin: {card:?}"
        );
        assert!(
            !card.body.contains("from downloaded from"),
            "the preposition must not double on the asset rung: {card:?}"
        );
        assert!(
            card.body.contains(&format!(
                "from the release at {base} (ROOST_SESSION_ASSET_BASE), checksum-verified"
            )),
            "{card:?}"
        );

        // The default ladder does name github.com — the assertion above
        // is about the override surviving, not about the word being
        // unreachable.
        let released = overridden_options(None, None)
            .source_preview(RemoteArch::Amd64)
            .expect("the asset rung previews")
            .describe();
        let card = copy(
            &plan_bootstrap(&ProbeOutcome::Missing, SessionState::NoSession),
            &released,
            false,
        );
        assert!(card.body.contains("github.com"), "{card:?}");
        assert!(!card.body.contains("ROOST_SESSION_ASSET_BASE"), "{card:?}");
        assert!(
            !card.body.contains("from downloaded from"),
            "the preposition must not double on the default asset rung either: {card:?}"
        );

        let source = overridden_options(None, Some("/tmp/roost-session"))
            .source_preview(RemoteArch::Amd64)
            .expect("the override rung previews")
            .describe();
        let card = copy(
            &plan_bootstrap(&mismatch(), SessionState::Running),
            &source,
            false,
        );
        assert!(card.body.contains("/tmp/roost-session"), "{card:?}");
        assert!(card.body.contains("ROOST_SESSION_INSTALL_BIN"), "{card:?}");
    }

    /// A downgrade is offered, and said out loud. Both halves matter:
    /// the user may have a reason, and they may not have noticed.
    #[test]
    fn installing_over_a_newer_session_says_so_and_still_offers_it() {
        let plan = plan_bootstrap(&mismatch(), SessionState::Running);
        let downgrade = copy(&plan, "this Roost's own roost-session", true);
        assert_eq!(
            downgrade.confirm, "Update",
            "the button is still there — this is a warning, not a refusal"
        );
        assert!(
            downgrade.body.contains("install an older build"),
            "{downgrade:?}"
        );
        assert!(
            downgrade
                .body
                .contains("upgrading this Roost is likely the fix"),
            "{downgrade:?}"
        );

        let plain = copy(&plan, "this Roost's own roost-session", false);
        assert!(
            !plain.body.contains("older build"),
            "direction is claimed only where it is known: {plain:?}"
        );
    }

    /// One job per box, whichever label *or spelling* asked for it. The
    /// key comes from [`roost_ipc::ssh::SshTarget::claim_key`], so two
    /// saved hosts written differently still find the box claimed.
    #[test]
    fn one_job_per_box_however_many_spellings_name_it() {
        let plain = classify_ssh("workbox").claim_key;
        let scheme = classify_ssh("ssh://WorkBox:22").claim_key;
        assert_eq!(plain, scheme, "precondition: one machine, two spellings");

        let mut flight = BootstrapsInFlight::default();
        assert!(flight.begin_job(&plain, 1));
        assert!(
            !flight.begin_job(&scheme, 2),
            "the other spelling's press finds the same box claimed"
        );
        assert!(flight.job_running(&scheme));
        assert!(flight.claim_job(&plain, 1));
        assert!(!flight.job_running(&scheme));
    }

    fn classify_ssh(raw: &str) -> roost_ipc::ssh::SshTarget {
        match roost_ipc::ssh::classify(raw).expect("classify an ssh target") {
            roost_ipc::ssh::ResolvedTransport::Ssh(target) => target,
            other => panic!("{raw:?} is not an ssh target: {other:?}"),
        }
    }

    /// The generation half of the same claim.
    #[test]
    fn one_job_per_target_token_however_many_labels_name_it() {
        let mut flight = BootstrapsInFlight::default();
        assert!(flight.begin_job("workbox-abc", 1));
        assert!(
            !flight.begin_job("workbox-abc", 2),
            "the second label's press finds the box claimed"
        );
        assert!(flight.job_running("workbox-abc"));
        assert!(
            flight.begin_job("other-def", 3),
            "another box is unaffected"
        );

        assert!(
            !flight.claim_job("workbox-abc", 2),
            "a completion that never held the claim does not release it"
        );
        assert!(flight.job_running("workbox-abc"));
        assert!(flight.claim_job("workbox-abc", 1));
        assert!(!flight.job_running("workbox-abc"));
        assert!(
            flight.begin_job("workbox-abc", 4),
            "and a finished job does not wedge the box out of ever being set up again"
        );
        assert!(flight.job_running("other-def"));
    }

    /// A probe answer has to prove it belongs to the request still being
    /// waited on: cancel, re-click, and the first probe's answer must not
    /// open a dialog for the second.
    #[test]
    fn a_probe_answer_belongs_to_the_request_that_asked_for_it() {
        let mut flight = BootstrapsInFlight::default();
        assert!(!flight.probing("h1"));
        flight.begin_probe("h1", 1);
        assert!(flight.probing("h1"));

        flight.begin_probe("h1", 2);
        assert!(
            !flight.claim_probe("h1", 1),
            "the superseded probe's answer lands nowhere"
        );
        assert!(flight.probing("h1"), "and did not clear the live one");
        assert!(flight.claim_probe("h1", 2));
        assert!(!flight.probing("h1"));
        assert!(
            !flight.claim_probe("h1", 2),
            "an answer is claimed once; a duplicate is not a second one"
        );
        assert!(!flight.claim_probe("h2", 2), "and never another host's");
    }

    /// A connect or disconnect starting underneath a probe invalidates
    /// it outright.
    ///
    /// Superseding is not enough on its own: a new attempt replaces the
    /// origin and the failure the probe's question was built on, so an
    /// IPC `host.connect` landing mid-probe would otherwise be followed
    /// by a consent card raised for a user request that is no longer the
    /// one in flight.
    #[test]
    fn a_new_attempt_invalidates_the_probe_it_lands_under() {
        let mut flight = BootstrapsInFlight::default();
        assert!(
            !flight.cancel_probe("h1"),
            "nothing in flight is nothing to clear"
        );

        flight.begin_probe("h1", 1);
        flight.begin_probe("h2", 2);
        assert!(flight.cancel_probe("h1"));
        assert!(!flight.probing("h1"));
        assert!(
            !flight.claim_probe("h1", 1),
            "the answer to the superseded question lands nowhere"
        );
        assert!(flight.probing("h2"), "and the other host is untouched");
        assert!(flight.claim_probe("h2", 2));
    }
}
