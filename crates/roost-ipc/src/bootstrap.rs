//! Bootstrapping `roost-session` onto a host you can `ssh` to
//! (host-sessions HS-3, plan 039): detect what is installed over there,
//! and — with consent — install or upgrade it to this client's build.
//!
//! The module is in two halves, the same split [`crate::ssh`] is in.
//!
//! This half touches no filesystem, no network, and no subprocess —
//! quoting a word for `sh`, generating the scripts a probe and an
//! install run remotely, reading their NUL-delimited output back,
//! mapping a `uname` pair onto a build we publish, naming a release
//! asset and its URL, reading a `sha256sum` file, and turning any of
//! that going wrong into copy a user can act on. It is therefore
//! unit-tested exhaustively, right here.
//!
//! [`BootstrapOptions::from_env`] is the one thing here that reads
//! anything outside its arguments, and it reads only process
//! environment — the same seam [`crate::ssh::SshTunnelOptions`] uses to
//! keep every *other* function in this half a pure function of values.
//!
//! The second half is [`BootstrapJob`], the runtime built out of those
//! pieces: a job-scoped `ssh` master, the bounded execs that carry these
//! scripts across it, the source ladder that decides *which* bytes to
//! stream, and the install choreography. Its tests live in
//! `tests/bootstrap_test.rs`, driven by a fake `ssh`.
//!
//! Four rules run through everything here, the first two borrowed from
//! herdr's prior art:
//!
//! * **Every interpolated value goes through [`shell_quote`]**, and
//!   every expansion inside a generated script is double-quoted. A
//!   remote path is not this side's data — it came back off the far
//!   host — and it is about to be spliced into a shell script.
//! * **`set -eu` on mutating scripts only.** A probe that runs with
//!   `set -u` on a host with no `$HOME` dies instead of answering, and
//!   the answer ("nothing is installed here") is exactly what the probe
//!   exists to produce.
//! * **Every path operand is preceded by `--`.** Quoting stops the
//!   *shell* splitting a word; it does nothing about `tee`/`chmod`/
//!   `mv`/`rm` reading a word that starts with `-` as one of their own
//!   options. Note the position: a POSIX utility stops parsing options
//!   at its first operand, so it is `chmod -- 700 <path>`, before the
//!   mode, and `mv -- <a> <b>`.
//! * **Every value off the far side is validated against the shape this
//!   side generated**, not merely for non-emptiness. The remote's
//!   answers name the file that gets written, `chmod`ed, renamed and
//!   `exec`ed, so [`parse_discovery`], [`parse_identity_pairs`] and
//!   [`parse_prepare`] each refuse anything their own script could not
//!   have produced.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::time::Duration;

use anyhow::anyhow;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::messages::{ops, SessionBinaryIdentity, SessionIdentify};
use crate::session_launch::{drain_tail, reap_by, Tail, Verdict};
use crate::ssh::{
    classify_ssh_failure, exit_master, last_line, pick_socket_dir, scaled, SshFailure, SshTarget,
    SshTunnelOptions,
};

// ============================================================================
// Quoting
// ============================================================================

/// Characters that need no quoting at all in `sh`: the conservative set
/// herdr uses, which is POSIX-portable-filename plus the handful of
/// punctuation marks that are never special anywhere a word can appear.
const BARE_SAFE: &[u8] = b"@%_+=:,./-";

/// Quote `word` so `sh` reads it as exactly one argument, byte for byte.
///
/// A word that is non-empty and made only of `[A-Za-z0-9@%_+=:,./-]` is
/// emitted bare; anything else — an empty string included, since a bare
/// nothing is not an argument — is wrapped in single quotes with each
/// embedded `'` spelled `'\''`. Inside single quotes `sh` has no escape
/// characters at all, so the closing quote, a literal backslash-quote,
/// and a reopening quote is the whole trick, and it is exact for every
/// byte including newlines and `$`.
///
/// A word starting with `-` is always quoted, even though `-` is in the
/// bare-safe set. Quoting does **not** stop `tee`/`chmod`/`mv`/`rm` from
/// reading such a word as one of their own options — only a `--`
/// terminator does that, and every generated command here carries one —
/// but a quoted `'-t'` is at least visibly a value in a logged command
/// line, and the two guards are cheap together.
pub fn shell_quote(word: &str) -> String {
    let bare = !word.is_empty()
        && !word.starts_with('-')
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || BARE_SAFE.contains(&byte));
    if bare {
        return word.to_string();
    }
    let mut out = String::with_capacity(word.len() + 2);
    out.push('\'');
    for ch in word.chars() {
        if ch == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

// ============================================================================
// The one candidate ladder
// ============================================================================

/// The binary every rung of the ladder names.
pub const SESSION_BIN_NAME: &str = "roost-session";

/// The subcommand the transport execs once it has found a binary.
const BRIDGE_SUBCOMMAND: &str = "client-bridge";

/// Remote-side prefix for the ladder's *absolute* rungs, **emitted only
/// in test mode** and never by a shipped script.
///
/// The hermetic `run-remote` test fixture (plan 039 §3.8) runs the real
/// scripts against a fake `$HOME` in a tempdir; without this the
/// `/usr/bin` rung would probe the developer's own machine and a
/// deb-installed box would answer differently from a clean one. Only
/// absolute rungs would carry it — the `$HOME`-relative ones are already
/// jailed by the fake `$HOME` itself.
///
/// It is gated behind [`BootstrapOptions::jail_fs_root`] (itself gated
/// on `ROOST_TEST_MODE=1`, exactly like [`SOURCE_ENV`]) because the
/// prefix is an *expansion on the far side*: a variable a hostile remote
/// can set — `~/.ssh/environment` under `PermitUserEnvironment`, a PAM
/// env module, a login rc — would otherwise redirect which binary the
/// transport, the probe and the install each resolve. A shipped ladder
/// names `/usr/bin/roost-session` and nothing else.
pub const FS_ROOT_ENV: &str = "ROOST_BOOTSTRAP_FS_ROOT";

/// The [`FS_ROOT_ENV`] expansion, as it appears inside double quotes.
const FS_ROOT_EXPANSION: &str = "${ROOST_BOOTSTRAP_FS_ROOT:-}";

/// Whether **this** process is a test lane: `ROOST_TEST_MODE=1` in our
/// own environment, and nothing else.
///
/// The one reader of that variable in this module, shared by the two
/// options bundles that carry the flag onward:
/// [`BootstrapOptions::from_env`] — which the probe and the install run
/// off — and [`crate::ssh::SshTunnelOptions::from_env`], whose
/// `jail_fs_root` reaches [`crate::ssh::remote_command_for`]. Below
/// those two constructors the flag is a **value**, never another
/// lookup, because the two answers must never differ: a probe that
/// reports
/// `Compatible { path: <jail>/usr/bin/roost-session }` beside a
/// transport that execs a bare `/usr/bin/roost-session` disagree about
/// which binary the host has, and the connect fails `NotFound` on a rung
/// the probe just called good.
///
/// **A client-side gate on purpose.** The [`FS_ROOT_ENV`] prefix it
/// enables is an expansion evaluated on the *far* side, so the decision
/// to emit it at all has to come from state a remote cannot reach. Our
/// process environment is exactly that: `ROOST_BOOTSTRAP_FS_ROOT` set
/// over there means nothing to a build that never writes the expansion
/// into a script in the first place.
pub(crate) fn test_mode_env() -> bool {
    std::env::var("ROOST_TEST_MODE").is_ok_and(|value| value == "1")
}

/// One rung of [`CANDIDATES`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// A stable name for the rung — logs, and the test that pins the
    /// ladder's order.
    pub name: &'static str,
    kind: CandidateKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CandidateKind {
    /// A bare `roost-session` resolved off the remote's
    /// non-interactive `PATH`. Probed with `command -v` rather than
    /// `[ -x ]`, because there is no path to test until `PATH` has
    /// been searched.
    PathLookup,
    /// A shell expression that expands to a filesystem path, tried only
    /// when every variable in `requires` is set and non-empty.
    Expansion {
        /// The expression as it appears *inside* double quotes.
        word: &'static str,
        requires: &'static [&'static str],
        /// Whether the hermetic fixture may prefix this rung with
        /// [`FS_ROOT_EXPANSION`]. True for the rungs rooted at `/`;
        /// false for the `$HOME`-relative ones, which a fake `$HOME`
        /// already jails.
        jailable: bool,
    },
}

impl Candidate {
    /// The path expression this rung resolves `$p` from, or `None` for
    /// the `PATH` rung, which has none until `PATH` has been searched.
    fn word(&self, jailed: bool) -> Option<String> {
        match self.kind {
            CandidateKind::PathLookup => None,
            CandidateKind::Expansion { word, jailable, .. } => Some(if jailed && jailable {
                format!("{FS_ROOT_EXPANSION}{word}")
            } else {
                word.to_string()
            }),
        }
    }

    /// The distinctive text this rung contributes to a generated
    /// script — what the ladder-equality test parses back out of both
    /// artifacts.
    pub fn marker(&self, jailed: bool) -> String {
        match self.word(jailed) {
            None => format!("command -v {SESSION_BIN_NAME}"),
            Some(word) => format!("\"{word}\""),
        }
    }

    /// The variables this rung's expansion needs before it may be
    /// tried at all.
    fn requires(&self) -> &'static [&'static str] {
        match self.kind {
            CandidateKind::PathLookup => &[],
            CandidateKind::Expansion { requires, .. } => requires,
        }
    }

    /// Everything that resolves `$p` and decides whether this rung may
    /// be used, up to and including the `&&` that gates the action.
    ///
    /// `[ -f ] && [ -x ]`, never `[ -x ]` alone: `[ -x ]` is true for an
    /// executable *directory*. In the probe that would be a bogus
    /// candidate; in [`exec_chain_command`] it is worse, because that
    /// rung `exec`s — a stray directory on the ladder would abort the
    /// whole chain at 126, which nothing classifies as
    /// [`crate::ssh::SshFailure::NotFound`], so no install offer would
    /// ever appear.
    ///
    /// `command -v` gets the same absolute-path-and-file gate as the
    /// rest: it answers with a bare word for a builtin, a function or an
    /// alias, and only an absolute path names a file either ladder can
    /// hand to an exec.
    fn guard(&self, jailed: bool) -> String {
        match self.word(jailed) {
            None => format!(
                "p=$(command -v {SESSION_BIN_NAME} 2>/dev/null) || p=; \
                 case \"$p\" in /*) [ -f \"$p\" ] && [ -x \"$p\" ] && "
            ),
            Some(word) => format!("p=\"{word}\"; [ -f \"$p\" ] && [ -x \"$p\" ] && "),
        }
    }

    /// This rung as one complete shell statement: the shared
    /// [`Candidate::guard`], then `action` against `"$p"`.
    ///
    /// Both ladders are generated through here, which is what makes
    /// "anything the probe can find, the transport can exec" a property
    /// of one function rather than of two lists that agree today.
    fn step(&self, action: &str, jailed: bool) -> String {
        let body = match self.kind {
            CandidateKind::PathLookup => format!("{}{action};; esac", self.guard(jailed)),
            CandidateKind::Expansion { .. } => format!("{}{action}", self.guard(jailed)),
        };
        guarded(self.requires(), body)
    }
}

/// **The** candidate ladder: every place a `roost-session` can be, in
/// the order both the probe and the transport try them.
///
/// One definition with two consumers is the point (plan 039 §3.2).
/// [`discovery_script`] generates the probe from it and
/// [`exec_chain_command`] generates the connect command from it, so
/// anything the probe can *find* is something the transport can
/// *exec* — the alternative is a dialog offering to install a binary
/// that is already there, forever, because the two lists disagreed.
///
/// The order is preference, not likelihood: rung 1 is where this module
/// installs, so a fresh install always wins the next connect.
pub const CANDIDATES: &[Candidate] = &[
    Candidate {
        name: "home-local-bin",
        kind: CandidateKind::Expansion {
            word: "$HOME/.local/bin/roost-session",
            requires: &["HOME"],
            jailable: false,
        },
    },
    Candidate {
        name: "path",
        kind: CandidateKind::PathLookup,
    },
    Candidate {
        name: "usr-bin",
        kind: CandidateKind::Expansion {
            word: "/usr/bin/roost-session",
            requires: &[],
            jailable: true,
        },
    },
    Candidate {
        name: "linuxbrew",
        kind: CandidateKind::Expansion {
            word: "/home/linuxbrew/.linuxbrew/bin/roost-session",
            requires: &[],
            jailable: true,
        },
    },
    Candidate {
        name: "nix-profile",
        kind: CandidateKind::Expansion {
            word: "$HOME/.nix-profile/bin/roost-session",
            requires: &["HOME"],
            jailable: false,
        },
    },
    Candidate {
        name: "nix-per-user",
        kind: CandidateKind::Expansion {
            word: "/etc/profiles/per-user/$USER/bin/roost-session",
            requires: &["USER"],
            jailable: true,
        },
    },
    Candidate {
        name: "nix-default-profile",
        kind: CandidateKind::Expansion {
            word: "/nix/var/nix/profiles/default/bin/roost-session",
            requires: &[],
            jailable: true,
        },
    },
    Candidate {
        name: "nixos-system",
        kind: CandidateKind::Expansion {
            word: "/run/current-system/sw/bin/roost-session",
            requires: &[],
            jailable: true,
        },
    },
];

/// Wrap `body` in `if [ -n "${HOME:-}" ] && …; then …; fi` for whatever
/// a rung requires, or hand it back untouched when it requires nothing.
///
/// `${VAR:-}` rather than `$VAR` even though the probe scripts
/// deliberately run without `set -u`: the same helper builds the
/// install scripts, which do set it.
fn guarded(requires: &[&str], body: String) -> String {
    if requires.is_empty() {
        return body;
    }
    let test = requires
        .iter()
        .map(|name| format!("[ -n \"${{{name}:-}}\" ]"))
        .collect::<Vec<_>>()
        .join(" && ");
    format!("if {test}; then {body}; fi")
}

/// The read-only discovery script: what platform is this, where is its
/// `$HOME`, and which rungs of the ladder are actually there?
///
/// Fed to `/bin/sh -s` on stdin, it writes NUL-delimited fields —
/// `uname -s`, `uname -m`, `$HOME`, then one field per rung that exists
/// and is executable — and exits 0 whatever it found.
///
/// `$HOME` is emitted (possibly empty) because the install destination
/// is `$HOME` + [`INSTALL_DEST_SUFFIX`], expanded on the *far* side:
/// without the remote's own answer this side cannot tell whether the
/// rung the probe found **is** the file an install would overwrite, and
/// a consent card that cannot tell says the wrong thing about the one
/// case that matters. Deliberately **not**
/// `set -eu`: a host with no `$HOME` (or no `uname`) still has an
/// answer, and the answer is "nothing here", not a failed exec.
///
/// `[ -f ] && [ -x ]` rather than `[ -e ]` is what keeps a dangling
/// symlink — and an executable *directory* — out of the candidate list:
/// a path that exists but cannot be run is not a candidate, it is a trap
/// the identity exec would fall into. The test itself lives on
/// [`Candidate::guard`], shared with [`exec_chain_command`].
///
/// `jailed` is [`BootstrapOptions::jail_fs_root`] and is `false`
/// everywhere but a test lane; see [`FS_ROOT_ENV`].
pub fn discovery_script(jailed: bool) -> String {
    let mut out = String::new();
    out.push_str("printf '%s\\0' \"$(uname -s 2>/dev/null)\"\n");
    out.push_str("printf '%s\\0' \"$(uname -m 2>/dev/null)\"\n");
    out.push_str("printf '%s\\0' \"${HOME:-}\"\n");
    for candidate in CANDIDATES {
        out.push_str(&candidate.step("printf '%s\\0' \"$p\"", jailed));
        out.push('\n');
    }
    out.push_str("exit 0\n");
    out
}

/// The remote command every transport `ssh` invocation execs: the same
/// ladder as [`discovery_script`], first executable rung wins, `exec`ed
/// with `client-bridge`.
///
/// One argv element — the whole thing is `sh -c '<script>'` — so the far
/// end never has to source a login shell just to find the binary. It is
/// a drop-in for [`crate::ssh::remote_command`], which C3 replaces with
/// this.
///
/// Falling off the end exits **127** with `command not found` on
/// stderr, which is exactly what
/// [`crate::ssh::classify_ssh_failure`] reads as
/// [`crate::ssh::SshFailure::NotFound`] — the family whose copy becomes
/// the install offer. A silent non-zero exit here would surface as an
/// unclassified transport failure and the offer would never appear.
///
/// **No embedded single quote survives into the emitted word.** sshd
/// hands a remote command to the user's *login* shell, and `'\''` — the
/// only way a POSIX `sh` can spell a quote inside a single-quoted word —
/// is not an escape in csh/tcsh/fish. The chain is therefore written
/// with double quotes throughout, including the fall-through `printf`,
/// so [`shell_quote`] wraps it in one pair of single quotes and nothing
/// more. `the_exec_chain_carries_no_embedded_single_quote` pins that.
///
/// **Deviation from the previous bare `exec roost-session`** (recorded
/// deliberately): the `PATH` rung resolves through `command -v` and is
/// then gated on `case "$p" in /*)`, so a *relative* `PATH` entry is no
/// longer honored. Exec'ing out of a relative `PATH` component is a
/// security hazard, and a non-interactive `BatchMode` `PATH` essentially
/// never has one. There is deliberately no bare-`exec` fallback: it
/// would be a rung the probe cannot enumerate, which is exactly the
/// two-ladder drift §3.2 exists to prevent.
///
/// `jailed` is [`BootstrapOptions::jail_fs_root`]; see [`FS_ROOT_ENV`].
/// [`crate::ssh::remote_command_for`] is handed the same flag off
/// [`crate::ssh::SshTunnelOptions::jail_fs_root`] — the transport and
/// the probe have to land on one rung, not two.
pub fn exec_chain_command(jailed: bool) -> String {
    let action = format!("exec \"$p\" {BRIDGE_SUBCOMMAND}");
    let mut steps: Vec<String> = CANDIDATES
        .iter()
        .map(|candidate| candidate.step(&action, jailed))
        .collect();
    steps.push(format!(
        "printf \"%s\\n\" \"{SESSION_BIN_NAME}: command not found\" >&2; exit 127"
    ));
    format!("sh -c {}", shell_quote(&steps.join("; ")))
}

/// The remote-shell PATH probe: does *that* shell's `roost-session`
/// resolve, and to what?
///
/// Handed to `ssh` as the remote command, so sshd hands it to the login
/// shell rather than to the `/bin/sh` the stdin-fed scripts get. Used
/// for two things and nothing else: catching a `PATH` set in a shell rc
/// that the non-interactive `/bin/sh` never sees, and the post-install
/// warning that names — never edits — a dotfile problem.
///
/// Wrapped in `sh -c` for the same reason [`exec_chain_command`] is:
/// `command -v` does not exist in csh, and the login shell is the user's
/// choice, not ours. The login shell still gets to set `PATH` — that is
/// the whole point of running this one under it — it just does not get
/// to parse the probe.
pub fn path_check_command() -> String {
    format!(
        "sh -c {}",
        shell_quote(&format!("command -v {SESSION_BIN_NAME}"))
    )
}

/// One script that asks each discovered candidate who it is, in ladder
/// order.
///
/// Batched on purpose (plan 039 §3.2): one round trip, not one per
/// rung. Over a confirm-mode key each round trip is a biometric tap,
/// and a probe that costs eight of them is a probe nobody runs twice.
///
/// Output is NUL-delimited `(path, identify-stdout)` pairs. A candidate
/// that answers — exit 0, non-empty stdout — emits its answer and ends
/// the loop; one that does not emits an **empty** second field and the
/// loop continues, so a binary too old to know the `identify`
/// subcommand still shows up as "present, unidentifiable" rather than
/// vanishing. [`classify_probe`] reads the pairs back.
pub fn identity_script(candidates: &[String]) -> String {
    let mut out = String::new();
    for path in candidates {
        let quoted = shell_quote(path);
        out.push_str(&format!(
            "if out=$({quoted} identify 2>/dev/null) && [ -n \"$out\" ]; then \
             printf '%s\\0%s\\0' {quoted} \"$out\"; exit 0; fi\n"
        ));
        out.push_str(&format!("printf '%s\\0%s\\0' {quoted} ''\n"));
    }
    out.push_str("exit 0\n");
    out
}

// ============================================================================
// The install scripts
// ============================================================================

/// Where an install always lands: rung 1 of the ladder, so the very
/// next connect finds it.
///
/// Read off [`CANDIDATES`] rather than spelled a second time: the
/// destination drifting from the rung the transport tries first is the
/// same install-forever loop the one-ladder rule exists to close, and
/// a `match` in const position makes that drift a compile error rather
/// than a convention.
pub const INSTALL_DEST: &str = match CANDIDATES[0].kind {
    CandidateKind::Expansion { word, .. } => word,
    CandidateKind::PathLookup => panic!("rung 1 must be a filesystem path to install into"),
};

/// The fixed tail of [`INSTALL_DEST`] — everything after `$HOME`.
///
/// [`parse_prepare`] holds the remote's answer to this, not merely to
/// "somewhere absolute called roost-session". `$HOME` is the far side's
/// answer to give; the rest of the path is not, and
/// `/etc/cron.hourly/roost-session` or `$HOME/.config/autostart/
/// roost-session` are perfectly good places to make a hostile answer
/// land. Pinned to [`INSTALL_DEST`] by
/// `the_install_destination_is_home_plus_the_fixed_suffix`.
pub const INSTALL_DEST_SUFFIX: &str = "/.local/bin/roost-session";

/// Phase 1 of the install: decide the destination and a temporary name
/// beside it, and report both.
///
/// `set -eu` here — this one mutates (`mkdir -p`), and a mutating
/// script that keeps going after a failed step is how a install ends up
/// half-done. `$HOME` is asserted non-empty *and* absolute before
/// anything is created: a relative `$HOME` would put the install
/// somewhere that depends on the remote shell's cwd, which is not a
/// place anyone can find it again.
///
/// The temporary carries `$$` and sits in the destination's own
/// directory, which is what makes the later `mv` atomic — a rename
/// across filesystems is a copy, and a copy can be interrupted
/// half-way. It reports both paths rather than letting this side
/// reconstruct them, because `$HOME` is the far side's answer to give —
/// but [`parse_prepare`] still holds the answer to the shape generated
/// here, because "the far side's answer to give" is not the same as
/// "any answer at all".
///
/// No `umask` here. It would apply only to this exec's `mkdir -p` — the
/// staged file is created by `tee` in a *later* exec under the login
/// shell's own umask — so all it bought was `~/.local` and
/// `~/.local/bin` at an unconventional 0700 for every other tool that
/// uses them. [`verify_staged_script`] narrows the staged file itself,
/// which is the thing that actually needed narrowing.
///
/// Two things happen here that the plan's phase 1 did not have:
///
/// * **Stale staged files are swept first.** Dropping an in-flight
///   [`BootstrapJob::install`] future does not run [`cleanup_script`] —
///   `Drop` has no runtime to await a remote exec on, and the repo's own
///   prior art (`ssh::blocking_exit_master`) says a spawned teardown
///   task reliably never runs. So an interrupted attempt leaves a
///   `<dest>.tmp.<pid>` nobody will ever come back for, and the next
///   attempt reclaims it. Only names matching the shape *this* script
///   emits are swept: they are ours by construction. (A second install
///   racing the first on the same host would sweep its temporary; the
///   loser fails cleanly at its own `chmod`, and nothing is corrupted.)
/// * **The staged path is reserved, not merely named.** `set -C` plus
///   `: > "$tmp"` creates it with `O_EXCL`, and `[ ! -L "$tmp" ]` refuses
///   a symlink outright — without which a pre-planted symlink at the
///   predictable `<dest>.tmp.<pid>` would have `tee` follow it and write
///   the streamed ELF wherever it pointed.
pub fn prepare_script() -> String {
    let mut out = String::from("set -eu\n");
    out.push_str(
        "[ -n \"${HOME:-}\" ] || { printf '%s\\n' 'roost bootstrap: HOME is not set' >&2; exit 1; }\n",
    );
    out.push_str(
        "case \"${HOME:-}\" in /*) ;; *) printf '%s\\n' 'roost bootstrap: HOME is not an \
         absolute path' >&2; exit 1;; esac\n",
    );
    out.push_str(&format!("dest=\"{INSTALL_DEST}\"\n"));
    out.push_str("mkdir -p \"${dest%/*}\"\n");
    // An unmatched glob stays literal, so `stale` is then
    // `<dest>.tmp.*` and the all-digits test rejects it.
    out.push_str(
        "for stale in \"$dest\".tmp.*; do \
         suffix=\"${stale##*.tmp.}\"; \
         case \"$suffix\" in ''|*[!0-9]*) continue;; esac; \
         [ -f \"$stale\" ] || continue; \
         rm -f -- \"$stale\"; \
         done\n",
    );
    out.push_str("tmp=\"${dest}.tmp.$$\"\n");
    out.push_str(
        "[ ! -L \"$tmp\" ] || { printf '%s\\n' 'roost bootstrap: the staged path is a symlink' \
         >&2; exit 1; }\n",
    );
    out.push_str(
        "(set -C; : > \"$tmp\") || { printf '%s\\n' 'roost bootstrap: the staged path already \
         exists' >&2; exit 1; }\n",
    );
    out.push_str("printf '%s\\0%s\\0' \"$tmp\" \"$dest\"\n");
    out
}

/// Phase 2: the remote command the binary's bytes are streamed into.
///
/// `sh -c` around a single `tee`, and the `--` before the path.
/// [`shell_quote`] stops the *shell* parsing a hostile path; only `--`
/// stops `tee` itself reading one that starts with `-` as an option.
///
/// The `sh -c` wrapper is a deliberate amendment to plan 039 §3.4,
/// which pinned the bare `tee '<tmp>'`: this string goes to sshd as the
/// remote command, so without a wrapper the user's **login shell**
/// parses it, and `'\''` is not an escape in csh/tcsh/fish.
/// [`shell_quote`]'s contract is POSIX `sh`. The wrap puts this at
/// exactly the support bar the transport already sets — plan 038 §3.3
/// documents a shell that cannot run a quoted `sh -c` string as
/// unsupported — and the path stays one quoted word inside.
///
/// `> /dev/null` is a deliberate deviation from herdr's bare `tee`
/// (plan 039 §4): `tee` writes its input to stdout as well, and stdout
/// here is the ssh connection, so the bare form sends the whole binary
/// back across the wire for nobody to read.
pub fn stream_command(tmp: &str) -> String {
    let inner = format!("tee -- {} > /dev/null", shell_quote(tmp));
    format!("sh -c {}", shell_quote(&inner))
}

/// Phase 3: narrow the staged temporary, make it executable, and ask it
/// who it is.
///
/// This runs **before** anything replaces the destination, and its
/// answer is the gate (plan 039 §3.4): bytes that do not identify as
/// the client's exact build never become the installed binary, so a
/// wrong-arch override or a checksum-valid-but-wrong file is a clean
/// no-op rather than a broken host.
///
/// `chmod 700` and **only** 700: `tee` created the temporary in the
/// previous exec under the login shell's umask, typically 022, so it is
/// on disk world-readable until something says otherwise. This is the
/// first exec that can say so, and the staged file stays 0700 for its
/// whole unverified life — widening it back to 0755 here would reopen
/// the window the 700 just closed, for the exact stretch during which
/// nobody has yet established what these bytes are.
/// [`commit_script`] widens it, after the gate.
///
/// `chmod -- <mode> <path>`, not `chmod <mode> -- <path>`. The mode is
/// `chmod`'s first *operand*, and a POSIX utility stops parsing options
/// at its first operand — so a trailing `--` is not a terminator, it is
/// a filename, and BSD `chmod` says exactly that. GNU `chmod` permutes
/// and accepts both spellings; only this one is portable.
pub fn verify_staged_script(tmp: &str) -> String {
    let quoted = shell_quote(tmp);
    format!("set -eu\nchmod -- 700 {quoted}\nexec {quoted} identify\n")
}

/// Phase 4: widen the verified temporary, move any incumbent aside, and
/// rename the temporary into place.
///
/// Neither guard is belt-and-braces. Under `set -e`, `[ -f {tmp} ]` is
/// what turns "the temporary went missing between phases" into a failed
/// commit rather than a `mv` diagnostic nobody classified. And
/// `[ ! -d {dest} ]` is what stops the far more dangerous silent
/// success: POSIX `mv file dir` moves the file *inside* the directory
/// and exits 0, so a `dest` that is a directory — or a symlink to one —
/// would report an install that put nothing in place, and the next
/// connect would `exec` a directory and exit 126, which
/// [`crate::ssh::classify_ssh_failure`] does not read as
/// [`crate::ssh::SshFailure::NotFound`]. No offer, ever again. `mv -T`
/// says the same thing in one flag and is GNU-only; the explicit test is
/// the portable form.
///
/// **The incumbent is moved, not overwritten.** A single `mv` destroys
/// the previous install the instant it runs, and the post-commit
/// re-verify has not happened yet — so a dropped leg or a timeout after
/// that point would leave the host with no working `roost-session` and
/// this side with nothing to put back. Moving it to `{backup}` — same
/// directory, so still one rename — keeps the old bytes addressable
/// until [`BootstrapJob::install`] either discards them
/// ([`discard_backup_script`]) or puts them back
/// ([`rollback_script`]).
///
/// `[ -e ] || [ -L ]`, because `[ -e ]` is false for a *dangling*
/// symlink, and a dangling symlink at `dest` is still something the
/// `mv` below would replace.
pub fn commit_script(tmp: &str, dest: &str, backup: &str) -> String {
    format!(
        "set -eu\n\
         [ -f {tmp} ]\n\
         [ ! -d {dest} ]\n\
         chmod -- 755 {tmp}\n\
         if [ -e {dest} ] || [ -L {dest} ]; then mv -- {dest} {backup}; fi\n\
         mv -- {tmp} {dest}\n",
        tmp = shell_quote(tmp),
        dest = shell_quote(dest),
        backup = shell_quote(backup),
    )
}

/// Put the incumbent back, on any failure after [`commit_script`]'s
/// first `mv`.
///
/// Best-effort and silent about a backup that is not there: the same
/// script runs whether the commit failed before the incumbent moved,
/// after it, or on a host that had no incumbent at all, and only the
/// middle case has anything to undo.
///
/// `mv -f` rather than `rm` then `mv`: an unlink followed by a rename
/// has a window with no binary at all in it, and a rename over a regular
/// file has none.
///
/// It prints `restored` on stdout, and only when the rename actually
/// happened. This side cannot tell "there was no incumbent" from "the
/// incumbent was put back" by looking at `dest` afterwards — both leave
/// a regular file there — and [`BootstrapError::PostCommit`]'s copy
/// turns on exactly that difference.
pub fn rollback_script(dest: &str, backup: &str) -> String {
    format!(
        "set -u\n\
         if [ -e {backup} ] || [ -L {backup} ]; then \
         mv -f -- {backup} {dest} && printf '%s\\n' restored; \
         fi\n",
        dest = shell_quote(dest),
        backup = shell_quote(backup),
    )
}

/// Drop the incumbent's copy, once the new install has answered.
pub fn discard_backup_script(backup: &str) -> String {
    format!("rm -f -- {}\n", shell_quote(backup))
}

/// Best-effort removal of the staged temporary, run on **every**
/// failure path after [`prepare_script`].
///
/// No `set -eu` and no guard: this is the cleanup, and a cleanup that
/// can fail loudly is one more failure to classify on a path that is
/// already failing. `rm -f` on a path that is already gone succeeds.
pub fn cleanup_script(tmp: &str) -> String {
    format!("rm -f -- {}\n", shell_quote(tmp))
}

/// Start the session using the binary at `path` — the just-committed
/// destination after an install, or the rung the probe found for a
/// start-only flow.
///
/// The resolved path, never the ladder: a host whose only
/// `roost-session` is a deb at `/usr/bin` must not be started through a
/// `~/.local/bin` that does not exist. What comes back on stdout is one
/// readiness line, read by [`crate::session_launch::Verdict::parse`].
///
/// `sh -c` for [`stream_command`]'s reason: this reaches the far side as
/// the remote command, and the login shell that would otherwise parse it
/// is the user's choice.
pub fn start_script(path: &str) -> String {
    let inner = format!("exec {} start", shell_quote(path));
    format!("sh -c {}\n", shell_quote(&inner))
}

// ============================================================================
// Reading the far side's answers
// ============================================================================

/// Split NUL-delimited output into fields.
///
/// One trailing NUL is the terminator every `printf '%s\0'` in this
/// module writes, so the empty field after it is dropped — but only
/// that one, because an *interior* empty field is meaningful (it is how
/// [`identity_script`] says "this candidate did not answer").
///
/// Non-UTF-8 is an error rather than a lossy conversion: these fields
/// become paths this side is about to splice into another script, and a
/// path silently rewritten with replacement characters names a
/// different file.
///
/// Output that does not *end* in a NUL is refused for the same reason.
/// Every generator here terminates every field, so a stream without a
/// final terminator was cut off mid-field by construction — and the
/// half-field it ends with is a prefix of a real path, which is a
/// perfectly valid path to a different file.
pub fn parse_nul_fields(bytes: &[u8]) -> Result<Vec<String>, BootstrapError> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        BootstrapError::Probe(format!("its output was not valid UTF-8: {error}"))
    })?;
    if text.is_empty() {
        return Ok(Vec::new());
    }
    if !text.ends_with('\0') {
        return Err(BootstrapError::Probe(
            "its output was cut off — the last field has no terminator".to_string(),
        ));
    }
    let mut fields: Vec<String> = text.split('\0').map(str::to_string).collect();
    if fields.last().is_some_and(String::is_empty) {
        fields.pop();
    }
    Ok(fields)
}

/// What [`discovery_script`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// `uname -s`, trimmed.
    pub os: String,
    /// `uname -m`, trimmed.
    pub arch: String,
    /// The remote's `$HOME`, verbatim — empty when it has none.
    ///
    /// Not a path this side ever opens or splices: its one job is to
    /// expand [`INSTALL_DEST_SUFFIX`] into the destination an install
    /// would write, so a card can tell "the rung I found is the file I
    /// am about to overwrite" from "a different file that will be
    /// shadowed". Untrimmed for the same reason the candidates are: a
    /// `$HOME` with a trailing space is a real directory.
    pub home: String,
    /// Every ladder rung that exists and is executable, in ladder
    /// order.
    pub candidates: Vec<String>,
}

/// Read [`discovery_script`]'s output back.
///
/// Only absolute paths survive, because only absolute paths are what
/// the script can emit — every rung tests `case "$p" in /*)` or expands
/// a `/`-rooted word. Anything else is an answer the probe did not
/// produce, and these strings are about to be spliced into a command
/// line as operands. `os` and `arch` are `trim`ed because they are
/// genuine `uname` output; the paths deliberately are **not** — a
/// `$HOME` with a trailing space is a real directory, and silently
/// rewriting it here would leave this side and the far side disagreeing
/// about which file they mean.
pub fn parse_discovery(bytes: &[u8]) -> Result<Discovery, BootstrapError> {
    let fields = parse_nul_fields(bytes)?;
    // Three, not two: the script emits `$HOME` unconditionally (empty
    // when the remote has none), so an answer that stops short of it is
    // truncated rather than merely home-less.
    if fields.len() < 3 {
        return Err(BootstrapError::Probe(
            "it answered with no platform line".to_string(),
        ));
    }
    let os = fields[0].trim().to_string();
    let arch = fields[1].trim().to_string();
    if os.is_empty() || arch.is_empty() {
        return Err(BootstrapError::Probe(
            "`uname` did not answer there".to_string(),
        ));
    }
    Ok(Discovery {
        os,
        arch,
        home: fields[2].clone(),
        candidates: fields[3..]
            .iter()
            .filter(|field| field.starts_with('/'))
            .cloned()
            .collect(),
    })
}

/// Read [`identity_script`]'s output back as `(path, stdout)` pairs,
/// checked against the `candidates` the script was generated from.
///
/// An odd field count means the far side was cut off mid-pair, which is
/// a truncated answer and not a set of pairs with one missing — reading
/// it as the latter would pair a path with the *next* path's output.
///
/// Each pair's path is checked against the candidate it must answer for,
/// position by position, rather than taken on trust. The path in a pair
/// is echoed by the remote, and it is the path that ends up cloned into
/// a [`ProbeOutcome`] and later handed to [`start_script`] to `exec` —
/// so an answer naming anything but the exact word it was asked about is
/// refused, and there can never be more answers than questions.
pub fn parse_identity_pairs(
    bytes: &[u8],
    candidates: &[String],
) -> Result<Vec<(String, String)>, BootstrapError> {
    let fields = parse_nul_fields(bytes)?;
    if fields.len() % 2 != 0 {
        return Err(BootstrapError::Probe(
            "its identity answer was cut off mid-record".to_string(),
        ));
    }
    if fields.len() / 2 > candidates.len() {
        return Err(BootstrapError::Probe(
            "its identity answer covered more binaries than it was asked about".to_string(),
        ));
    }
    let mut pairs = Vec::with_capacity(fields.len() / 2);
    for (pair, candidate) in fields.chunks_exact(2).zip(candidates) {
        if pair[0] != *candidate {
            return Err(BootstrapError::Probe(format!(
                "its identity answer named {:?} where {candidate:?} was asked about",
                pair[0]
            )));
        }
        pairs.push((pair[0].clone(), pair[1].clone()));
    }
    Ok(pairs)
}

/// Read [`prepare_script`]'s output back as `(tmp, dest)`.
///
/// Validated against the shape [`prepare_script`] is *known* to emit,
/// not merely "two non-empty strings". These two paths flow straight
/// into `tee`, `chmod`, `mv` and `rm` on the far side, under a consent
/// dialog that named `~/.local/bin/roost-session` — a remote that
/// answers `dest=/home/u/.ssh/authorized_keys`, whether compromised or
/// merely mangled by a chatty shell rc, must not get the streamed ELF
/// written and renamed there.
///
/// So: both absolute, `dest` ending in the whole of
/// [`INSTALL_DEST_SUFFIX`] — not merely in `/roost-session`, which
/// `/etc/cron.hourly/roost-session` and `$HOME/.config/autostart/
/// roost-session` also do — and `tmp` exactly `dest` + `.tmp.` + the
/// remote `$$`. Everything after `$HOME` in the destination is *this*
/// side's constant, so holding the answer to it costs nothing and closes
/// the whole family. No `trim` — the raw field is the path, and a
/// `$HOME` with a trailing space is a real directory this side must keep
/// agreeing with the far side about.
///
/// The third path — the incumbent's backup, `dest` + `.bak.` + the same
/// pid — is *derived* rather than asked for: it is built from two
/// already-validated pieces, so it needs no round trip and no second
/// answer to check.
pub fn parse_prepare(bytes: &[u8]) -> Result<Staged, BootstrapError> {
    let fields = parse_nul_fields(bytes)?;
    let [tmp, dest] = fields.as_slice() else {
        return Err(BootstrapError::Install {
            phase: InstallPhase::Prepare,
            detail: format!("it reported {} paths, not 2", fields.len()),
        });
    };
    let refuse = || BootstrapError::Install {
        phase: InstallPhase::Prepare,
        detail: format!(
            "it answered with {tmp:?} and {dest:?}, which is not the pair the prepare script \
             emits (an absolute path ending in {INSTALL_DEST_SUFFIX}, and that path plus \
             `.tmp.<pid>`)"
        ),
    };
    if !dest.starts_with('/')
        || !dest.ends_with(INSTALL_DEST_SUFFIX)
        || dest.len() <= INSTALL_DEST_SUFFIX.len()
    {
        return Err(refuse());
    }
    let Some(pid) = tmp.strip_prefix(&format!("{dest}.tmp.")) else {
        return Err(refuse());
    };
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(refuse());
    }
    Ok(Staged {
        backup: format!("{dest}.bak.{pid}"),
        tmp: tmp.clone(),
        dest: dest.clone(),
    })
}

/// The three paths one install works with, all rooted at the `dest` the
/// far side reported and this side validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    /// `<dest>.tmp.<pid>` — where the streamed bytes land.
    pub tmp: String,
    /// The install destination itself.
    pub dest: String,
    /// `<dest>.bak.<pid>` — where an incumbent waits out the commit.
    pub backup: String,
}

/// One `roost-session identify` line, or `None`.
///
/// Deliberately lossy in one direction only (plan 039 §3.1): a binary
/// that printed something unparseable, or exited non-zero, degrades to
/// "no identity" rather than to an error. The overwhelmingly common
/// cause is an older `roost-session` exiting 2 on a subcommand it has
/// never heard of, and the honest reading of that is "needs an
/// upgrade", not "the probe failed".
pub fn parse_identity_line(stdout: &str) -> Option<SessionBinaryIdentity> {
    let line = stdout.lines().find(|line| !line.trim().is_empty())?;
    serde_json::from_str(line.trim()).ok()
}

// ============================================================================
// The compatibility rule
// ============================================================================

/// May this binary be installed — or left alone — as a match for the
/// client?
///
/// All three fields, exact string compare, no semver ordering.
///
/// **This is deliberately stricter than the runtime attach gate**
/// (`roost-iced`'s `check_compatibility`: protocol + payload kind +
/// `libghostty_build`, with no `app_version` at all), and the asymmetry
/// is pinned by plan 039 §3.1. The installer refuses to *install*
/// anything but the exact build; the runtime keeps accepting a
/// same-build adjacent-version session, so two releases with no ghostty
/// bump between them do not force a restart on every localhost user.
/// Changing either side to match the other is a decision, not a
/// cleanup.
pub fn identity_matches(expected: &SessionBinaryIdentity, found: &SessionBinaryIdentity) -> bool {
    expected == found
}

/// What the probe concluded about a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// A binary matching the client exactly, at `path`.
    Compatible { path: String },
    /// A `roost-session` is there but is not this build. `identity` is
    /// `None` when it is present but would not identify itself at all —
    /// a build older than the `identify` subcommand. Either way the
    /// answer is the same offer, so the distinction is for copy and
    /// logs, not for routing.
    Mismatch {
        path: String,
        identity: Option<SessionBinaryIdentity>,
    },
    /// No rung of the ladder exists.
    Missing,
}

/// Turn [`identity_script`]'s pairs into an outcome.
///
/// The verdict is **always** about the first pair, because that is the
/// rung [`exec_chain_command`] will exec. Nothing deeper in the ladder
/// can change it: a compatible binary further down is shadowed by
/// whatever is above it, and calling the host compatible on the strength
/// of a rung that never runs would offer no fix for a problem the user
/// still has (plan 039 §9).
///
/// This is exactly the case that needs care, because
/// [`identity_script`] stops at the first candidate that *answers*: a
/// rung too old to know `identify` emits an empty second field and the
/// loop continues, so a match lands past `pairs[0]` precisely when
/// `pairs[0]` is the stale binary the transport is about to exec.
/// Reporting `Compatible` there would offer no install, forever, and the
/// dialog that fixes it would never appear. Reporting `Mismatch` on the
/// preferred rung offers the install, which lands on rung 1 and shadows
/// the stale one — the loop closes itself.
pub fn classify_probe(
    expected: &SessionBinaryIdentity,
    pairs: &[(String, String)],
) -> ProbeOutcome {
    let Some((path, stdout)) = pairs.first() else {
        return ProbeOutcome::Missing;
    };
    let identity = parse_identity_line(stdout);
    match &identity {
        Some(found) if identity_matches(expected, found) => {
            ProbeOutcome::Compatible { path: path.clone() }
        }
        _ => ProbeOutcome::Mismatch {
            path: path.clone(),
            identity,
        },
    }
}

// ============================================================================
// The platform map
// ============================================================================

/// An architecture roost publishes a `roost-session` build for. The
/// spelling matches the deb naming, because the release assets and the
/// packages come off the same matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteArch {
    Amd64,
    Arm64,
}

impl RemoteArch {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Amd64 => "amd64",
            Self::Arm64 => "arm64",
        }
    }
}

impl std::fmt::Display for RemoteArch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `uname -s` → is there anything to install here at all?
///
/// Linux only this slice. A Darwin remote is a classified refusal
/// rather than an attempted install: no Mac `roost-session` is built,
/// so every later rung of the source ladder would fail anyway and the
/// honest place to say so is before the user consents to anything.
pub fn check_os(uname_s: &str) -> Result<(), BootstrapError> {
    let os = uname_s.trim();
    if os.eq_ignore_ascii_case("linux") {
        return Ok(());
    }
    Err(BootstrapError::UnsupportedOs(os.to_string()))
}

/// `uname -m` → the build we publish for it.
pub fn map_arch(uname_m: &str) -> Result<RemoteArch, BootstrapError> {
    let arch = uname_m.trim();
    match arch.to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" => Ok(RemoteArch::Amd64),
        "aarch64" | "arm64" => Ok(RemoteArch::Arm64),
        _ => Err(BootstrapError::UnsupportedArch(arch.to_string())),
    }
}

// ============================================================================
// Release assets
// ============================================================================

/// The GitHub release the default asset base points into.
const RELEASE_BASE: &str = "https://github.com/charliek/roost/releases/download";

/// `roost-session-<version>-linux-<arch>` — the raw ELF, no tarball, so
/// the published checksum covers exactly the bytes that get streamed.
/// Every character stays inside GitHub's `[A-Za-z0-9._-]` asset-name
/// sanitization, so what is uploaded is what is fetched.
pub fn asset_name(version: &str, arch: RemoteArch) -> String {
    format!("{SESSION_BIN_NAME}-{version}-linux-{arch}")
}

/// The `sha256sum` sibling of an asset.
pub fn checksum_name(asset: &str) -> String {
    format!("{asset}.sha256")
}

/// Is `version` a plain stable version — the only shape for which
/// `v<version>` is guaranteed to be the real release tag?
///
/// `release.yml`'s `version-check` compares only the *base* version, so
/// tag `v0.0.19-rc1` builds a client whose `CARGO_PKG_VERSION` is
/// `0.0.19`. Such a client must never construct
/// `.../download/v0.0.19/...`: that tag may not exist, or worse may
/// exist and hold a different build. Three numeric dot-separated
/// components and nothing else.
pub fn is_stable_version(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// The release-download base for a stable client, or the rung-4 refusal.
///
/// A prerelease client gets no URL at all (plan 039 §3.3): release.yml
/// still uploads prerelease artifacts under the tag's own spelling for
/// manual use, but no client guesses at that spelling.
pub fn default_asset_base(app_version: &str) -> Result<String, BootstrapError> {
    if !is_stable_version(app_version) {
        return Err(BootstrapError::NoSource {
            app_version: app_version.to_string(),
        });
    }
    Ok(format!("{RELEASE_BASE}/v{app_version}"))
}

/// Refuse any asset base that is not HTTPS, or loopback HTTP.
///
/// `https://` anywhere, or `http://` for `127.0.0.1` / `localhost` /
/// `[::1]` only — the loopback exception exists so the test fixture can
/// serve assets over a local HTTP server (plan 039 §3.8), and confining
/// it to loopback is what stops that seam from becoming a way to
/// downgrade a real download to plaintext. Userinfo is refused outright
/// in both schemes: `http://localhost@evil.example/` is not a loopback
/// URL, and credentials have no business in a path that logs its URLs.
///
/// A query or a fragment is refused outright too, because
/// [`BootstrapOptions::asset_plan`] joins the asset name onto this base
/// textually. For `https://host/download?token=x` that would put the
/// asset name inside the query string, and for `https://host/download#f`
/// the asset URL and the checksum URL would both fetch `/download`. No
/// release base has either, so rejecting is both simpler and safer than
/// a real URL join.
pub fn check_asset_base(base: &str) -> Result<(), BootstrapError> {
    let refuse = || {
        BootstrapError::Download(format!(
            "{base} is not a usable https:// URL (plain http:// is allowed only for \
             127.0.0.1, localhost or ::1, and a query or fragment is not allowed at all)"
        ))
    };
    if base.contains('?') || base.contains('#') {
        return Err(refuse());
    }
    let (scheme, rest) = base.split_once("://").ok_or_else(refuse)?;
    let authority = rest
        .split('/')
        .next()
        .expect("split always yields a first element");
    if authority.is_empty() || authority.contains('@') {
        return Err(refuse());
    }
    // A bracketed IPv6 authority is host-through-`]`, and only what
    // follows the bracket can be a `:port`. Splitting on the last colon
    // first would read `[::1]` as host `[:`.
    let host = if let Some(after_bracket) = authority.strip_prefix('[') {
        let (inside, tail) = after_bracket.split_once(']').ok_or_else(refuse)?;
        if !(tail.is_empty() || tail.starts_with(':')) {
            return Err(refuse());
        }
        inside
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => host,
            _ => authority,
        }
    };
    if host.is_empty() {
        return Err(refuse());
    }
    let scheme = scheme.to_ascii_lowercase();
    let loopback = matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1" | "[::1]"
    );
    if scheme == "https" || (scheme == "http" && loopback) {
        Ok(())
    } else {
        Err(refuse())
    }
}

/// Everything the download rung needs, and everything the consent
/// dialog needs to name its origin honestly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetPlan {
    pub base: String,
    pub asset_name: String,
    pub asset_url: String,
    pub checksum_url: String,
    /// Whether `base` came from `ROOST_SESSION_ASSET_BASE` rather than
    /// the release default — the dialog names the *actual* origin, and
    /// an overridden base is never rendered as github.com.
    pub overridden: bool,
}

// ============================================================================
// The checksum file
// ============================================================================

/// Read a `.sha256` sibling and return the hash it publishes for
/// `expected_name`, lowercased.
///
/// The rules are all refusals (plan 039 §3.6): **exactly one** record,
/// naming the asset that was actually fetched. A checksum file with two
/// records is a file for a different release; one naming another asset
/// is a hash for bytes nobody downloaded. Neither is a thing to guess at
/// when the answer decides what gets executed on someone's machine.
///
/// The accepted grammar is exactly one record of **64 hex characters,
/// one or more spaces or tabs, an optional `*` mode marker, then the
/// filename to end of line** — and nothing else in the file but blank
/// lines. Two things are accepted on purpose: uppercase hex (`sha256sum`
/// and every other tool compares case-blind, and the answer is
/// lowercased on the way out), and the `*name` binary-mode marker
/// `sha256sum -b` writes, which describes how the hash was *read* and
/// not which file it covers. Everything else is a refusal — a leading
/// `\` GNU escape marker, a comment, a second record, `./name`, a name
/// with interior whitespace, a BOM, unicode digits, and any control
/// character in the record.
pub fn parse_checksum_file(text: &str, expected_name: &str) -> Result<String, BootstrapError> {
    let mut records = text.lines().filter(|line| !line.trim().is_empty());
    let Some(record) = records.next() else {
        return Err(BootstrapError::Checksum(
            "the published checksum file is empty".to_string(),
        ));
    };
    if records.next().is_some() {
        return Err(BootstrapError::Checksum(
            "the published checksum file lists more than one file".to_string(),
        ));
    }
    // Tab is the one control character the separator may legitimately
    // be; anything else in the record means the line is not the record
    // it is pretending to be.
    if record
        .bytes()
        .any(|byte| byte.is_ascii_control() && byte != b'\t')
    {
        return Err(BootstrapError::Checksum(
            "the published checksum line contains control characters".to_string(),
        ));
    }

    let hex_len = 64;
    if record.len() <= hex_len
        || !record.as_bytes()[..hex_len]
            .iter()
            .all(u8::is_ascii_hexdigit)
    {
        return Err(BootstrapError::Checksum(
            "the published checksum line is not 64 hex characters followed by a filename"
                .to_string(),
        ));
    }
    // Safe to slice: the first 64 bytes were just proven ASCII.
    let rest = &record[hex_len..];
    let after_separator = rest.trim_start_matches([' ', '\t']);
    if after_separator.len() == rest.len() {
        return Err(BootstrapError::Checksum(
            "the published checksum is longer than 64 hex characters".to_string(),
        ));
    }
    let name = after_separator.strip_prefix('*').unwrap_or(after_separator);
    if name != expected_name {
        return Err(BootstrapError::Checksum(format!(
            "the published checksum covers {name:?}, not {expected_name}"
        )));
    }
    Ok(record[..hex_len].to_ascii_lowercase())
}

// ============================================================================
// Failures
// ============================================================================

/// Which install phase failed — the three remote steps between
/// "nothing has been written" and "the new binary is in place".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPhase {
    Prepare,
    Stream,
    Commit,
}

/// What kind of failure a bootstrap job hit, and the user-facing copy
/// for it.
///
/// A **sibling** of [`crate::ssh::SshFailure`], never new variants
/// inside it (plan 039 §3.5). The ssh families describe a *connection*
/// and are produced by matching an `ssh` stderr tail; these describe a
/// *job* that ran across a connection that worked. Folding them
/// together would put non-connection rows in a first-match stderr table
/// that has no way to produce them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapError {
    /// Looking at the far side failed — the exec, its output, or its
    /// shape.
    Probe(String),
    /// The remote is not Linux.
    UnsupportedOs(String),
    /// The remote's architecture has no build.
    UnsupportedArch(String),
    /// Rung 4 of the source ladder: nothing can supply this build.
    NoSource { app_version: String },
    /// The chosen local source could not be used.
    Source(String),
    /// Fetching the release asset failed.
    Download(String),
    /// The published checksum, or the bytes it covers, did not check
    /// out. Nothing was sent.
    Checksum(String),
    /// A remote install phase failed.
    Install { phase: InstallPhase, detail: String },
    /// The *staged* file is not this client's build. Nothing has
    /// replaced the destination — the staged verify runs before the
    /// commit, which is the whole point of the ordering.
    Verify(String),
    /// The commit landed and then the **installed** file would not
    /// answer.
    ///
    /// A sibling of [`Self::Verify`] rather than a detail on it,
    /// because the two differ in the one thing a user needs from a
    /// failed install: whether the host still has what it had.
    /// `restored` says whether an incumbent was put back, and the copy
    /// says so rather than asserting an unchanged install it cannot
    /// vouch for.
    PostCommit { detail: String, restored: bool },
    /// The running session would not stop.
    Stop(String),
    /// The session would not start.
    Start(String),
}

impl BootstrapError {
    /// User-facing copy, with `target` interpolated where the message
    /// tells the user what to do next.
    ///
    /// Every message that ends a job before the destination was touched
    /// says so: the single most useful thing to know about a failed
    /// install is whether the host is still the way it was.
    pub fn message(&self, target: &str) -> String {
        match self {
            Self::Probe(detail) => {
                format!("couldn't check what's installed on {target}: {detail}. Nothing was changed there.")
            }
            Self::UnsupportedOs(os) => format!(
                "{target} reports itself as {os}. roost-session is built for Linux only today, \
                 so there is nothing to install there."
            ),
            Self::UnsupportedArch(arch) => format!(
                "{target}'s architecture ({arch}) has no roost-session build — roost publishes \
                 amd64 and arm64 only."
            ),
            Self::NoSource { app_version } => format!(
                "no roost-session build matching {app_version} is available to install. Build \
                 one on {target}, or point ROOST_SESSION_INSTALL_BIN at a matching binary. \
                 {target} was left untouched."
            ),
            Self::Source(detail) => format!(
                "the roost-session to install couldn't be read: {detail}. {target} was left \
                 untouched."
            ),
            Self::Download(detail) => format!(
                "downloading roost-session for {target} failed: {detail}. {target} was left \
                 untouched."
            ),
            Self::Checksum(detail) => format!(
                "the roost-session download did not match its published checksum, so nothing was \
                 sent to {target}: {detail}."
            ),
            Self::Install { phase, detail } => match phase {
                InstallPhase::Prepare => format!(
                    "couldn't prepare {target} for the install: {detail}. Nothing was written \
                     there."
                ),
                InstallPhase::Stream => format!(
                    "sending roost-session to {target} failed: {detail}. A write that stops \
                     part-way is usually a full disk — check free space in that home directory. \
                     The staged file was removed and the existing install is unchanged."
                ),
                // Deliberately does *not* claim an unchanged install:
                // the rename may already have landed when the exec
                // failed or timed out. What is true either way is that
                // the staged file is gone and any incumbent was put
                // back.
                InstallPhase::Commit => format!(
                    "couldn't put the new roost-session in place on {target}: {detail}. The \
                     staged file was removed and any previous install there was put back."
                ),
            },
            Self::Verify(detail) => format!(
                "the roost-session staged on {target} isn't the build this Roost needs: \
                 {detail}. It was removed and {target}'s existing install was left exactly as \
                 it was."
            ),
            Self::PostCommit { detail, restored } => {
                let aftermath = if *restored {
                    format!(
                        "The previous install on {target} has been put back, so it is as it \
                             was — try again, or install roost-session there by hand."
                    )
                } else {
                    format!(
                        "There was no previous install to fall back to, so the new one is still \
                         there — check it with `roost-session identify` on {target}."
                    )
                };
                format!(
                    "roost-session was put in place on {target} but wouldn't identify itself \
                     afterwards: {detail}. {aftermath}"
                )
            }
            Self::Stop(detail) => format!(
                "couldn't stop the roost session on {target}: {detail}. Stop it there with \
                 `roostctl session stop`, then try again — nothing else was changed."
            ),
            Self::Start(detail) => format!(
                "roost-session is installed on {target} but wouldn't start: {detail}. Try \
                 `roostctl session start` on {target}."
            ),
        }
    }
}

// ============================================================================
// Options
// ============================================================================

/// Override naming a local file to install, instead of resolving a
/// source. Rung 1 of the source ladder — a dev escape hatch and the
/// test seam.
pub const INSTALL_BIN_ENV: &str = "ROOST_SESSION_INSTALL_BIN";

/// Override for the release-asset base URL. Points the download rung at
/// a local fixture server.
pub const ASSET_BASE_ENV: &str = "ROOST_SESSION_ASSET_BASE";

/// Test-mode-only override forcing one rung of the source ladder, so a
/// lane can exercise the download path on a machine where a compatible
/// sibling binary would have won.
pub const SOURCE_ENV: &str = "ROOST_BOOTSTRAP_SOURCE";

/// Which rung of the source ladder to use, when the choice is forced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallSource {
    /// The release asset, downloaded and checksum-verified.
    Asset,
    /// This client's own `roost-session`, found by
    /// [`crate::session_launch::locate_session_binary`].
    Sibling,
    /// The file [`INSTALL_BIN_ENV`] names.
    Env,
}

impl InstallSource {
    /// Parse [`SOURCE_ENV`]'s spelling. An unrecognized value is `None`
    /// — a forcing override nobody can spell is better ignored than
    /// treated as a preference.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "asset" => Some(Self::Asset),
            "sibling" => Some(Self::Sibling),
            "env" => Some(Self::Env),
            _ => None,
        }
    }
}

/// Everything a bootstrap job reads from its environment, in one
/// injectable place — the [`crate::ssh::SshTunnelOptions`] pattern.
///
/// Every field is a value rather than a lookup, so a test can point a
/// whole job at a fixture server and a fake binary without mutating
/// process-global environment that every other test in the same binary
/// also reads.
///
/// `expected` is **injected**, not computed: the client's own
/// `libghostty_build` comes from `roost_vt::libghostty_build()`, and
/// `roost-ipc` deliberately has no `roost-vt` dependency (plan 039
/// §3.1). The UI constructs the triple and hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapOptions {
    /// The identity a remote binary must equal, per
    /// [`identity_matches`].
    pub expected: SessionBinaryIdentity,
    /// `ROOST_SESSION_ASSET_BASE`, or `None` for the release default.
    pub asset_base: Option<String>,
    /// `ROOST_SESSION_INSTALL_BIN`.
    pub install_bin: Option<PathBuf>,
    /// This client's own `roost-session`, if this installation has one —
    /// rung 2 of the source ladder.
    ///
    /// Resolved here, at the environment edge, rather than inside the
    /// job: [`crate::session_launch::locate_session_binary`]'s ladder
    /// reads `ROOST_SESSION_BIN`, this executable's own directory and
    /// `PATH`, and every one of those is process-global state the rule
    /// above keeps out of the engine. Three `stat` calls at options time
    /// buys a rung a test can point anywhere. Being *found* is not being
    /// *usable* — [`resolve_source`] still runs it and compares the
    /// triple.
    pub sibling_bin: Option<PathBuf>,
    /// The `curl` the download rung execs. A value rather than a lookup
    /// for the same reason [`crate::ssh::SshTunnelOptions::ssh_bin`] is.
    pub curl_bin: PathBuf,
    /// A forced source rung, or `None` for the normal ladder.
    pub source: Option<InstallSource>,
    /// Whether the generated ladders prefix their absolute rungs with
    /// [`FS_ROOT_ENV`] — a test-lane seam, never true in a shipped
    /// build. See [`FS_ROOT_ENV`] for why this is a flag rather than an
    /// unconditional expansion.
    pub jail_fs_root: bool,
}

impl BootstrapOptions {
    /// The injected identity plus whatever the environment overrides.
    ///
    /// [`SOURCE_ENV`] is read only under `ROOST_TEST_MODE=1`, the same
    /// double gate `roost-session`'s fake-build override uses: forcing
    /// the ladder is a lane's tool, not a shipped surface. The
    /// [`FS_ROOT_ENV`] jail rides the identical gate, for a sharper
    /// reason: it decides which binary gets `exec`ed.
    pub fn from_env(expected: SessionBinaryIdentity) -> Self {
        let test_mode = test_mode_env();
        Self {
            expected,
            asset_base: std::env::var(ASSET_BASE_ENV)
                .ok()
                .filter(|value| !value.trim().is_empty()),
            install_bin: std::env::var_os(INSTALL_BIN_ENV)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            sibling_bin: crate::session_launch::locate_session_binary(
                std::env::var_os(crate::session_launch::BIN_ENV).as_deref(),
                std::env::current_exe().ok().as_deref(),
                std::env::var_os("PATH").as_deref(),
            )
            .ok()
            .map(|located| located.path),
            curl_bin: PathBuf::from("curl"),
            source: if test_mode {
                std::env::var(SOURCE_ENV)
                    .ok()
                    .as_deref()
                    .and_then(InstallSource::parse)
            } else {
                None
            },
            jail_fs_root: test_mode,
        }
    }

    /// Where the release asset for `arch` lives, and whether that
    /// answer came from an override.
    ///
    /// The base is validated before any URL is built from it, so an
    /// override that would downgrade the fetch to plaintext — or that
    /// carries a query the asset name would land inside of — fails here
    /// rather than at the first byte. An override is trimmed *before*
    /// its trailing slashes are stripped, or `"https://host "` would be
    /// stored with the space still in it and every joined URL would
    /// carry one in the middle.
    pub fn asset_plan(&self, arch: RemoteArch) -> Result<AssetPlan, BootstrapError> {
        let (base, overridden) = match &self.asset_base {
            Some(base) => (base.trim().trim_end_matches('/').to_string(), true),
            None => (default_asset_base(&self.expected.app_version)?, false),
        };
        check_asset_base(&base)?;
        let asset_name = asset_name(&self.expected.app_version, arch);
        Ok(AssetPlan {
            asset_url: format!("{base}/{asset_name}"),
            checksum_url: format!("{base}/{}", checksum_name(&asset_name)),
            asset_name,
            base,
            overridden,
        })
    }

    /// Which rung [`resolve_source`] will try first for `arch`, and what
    /// it would fall through to — **without doing any of it**.
    ///
    /// The consent dialog has to name where the bytes will come from
    /// before the user consents, and resolution deliberately does not
    /// happen until after (plan 039 §3.5: no subprocess, no network in
    /// the connect path). So this walks the same ladder on the facts
    /// already in hand — the overrides, whether a sibling was located,
    /// whether this client could even run over there — and stops short
    /// of every step that costs something: the sibling's local
    /// `identify`, the download, the hash.
    ///
    /// That last gap is why [`SourcePreview::fallback`] exists rather
    /// than this returning a bare origin. A located sibling can turn out
    /// not to be this build and fall through to the asset, and a dialog
    /// that promised "this Roost's own roost-session" and then downloaded
    /// one would have said something untrue at the only moment the user
    /// could act on it. Naming both, in order, is what this can honestly
    /// claim.
    pub fn source_preview(&self, arch: RemoteArch) -> Result<SourcePreview, BootstrapError> {
        let forced = self.source;
        let wanted = |rung: InstallSource| forced.is_none() || forced == Some(rung);
        let asset = || {
            self.asset_plan(arch).map(|plan| SourceOrigin::Asset {
                base: plan.base,
                overridden: plan.overridden,
            })
        };

        if wanted(InstallSource::Env) {
            if let Some(path) = &self.install_bin {
                return Ok(SourcePreview {
                    first: SourceOrigin::Override,
                    fallback: None,
                    override_path: Some(path.clone()),
                });
            }
        }
        if wanted(InstallSource::Sibling)
            && self.sibling_bin.is_some()
            && client_arch() == Some(arch)
        {
            return Ok(SourcePreview {
                first: SourceOrigin::Sibling,
                // A forced sibling has nowhere to fall through to —
                // `resolve_source` turns its refusal into an error
                // instead of trying the next rung.
                fallback: if forced == Some(InstallSource::Sibling) {
                    None
                } else {
                    asset().ok()
                },
                override_path: None,
            });
        }
        if wanted(InstallSource::Asset) {
            return Ok(SourcePreview {
                first: asset()?,
                fallback: None,
                override_path: None,
            });
        }
        Err(BootstrapError::NoSource {
            app_version: self.expected.app_version.clone(),
        })
    }
}

// ============================================================================
// The runtime
// ============================================================================

/// Per-exec budget for the read-only probe steps. Each is one `ssh`
/// exec over a master that is already up (or is being opened by this
/// very exec, which is why it is not tighter).
const PROBE_BUDGET: Duration = Duration::from_secs(30);

/// The install's remote steps that only *think* — prepare, verify the
/// staged file, commit. No bytes cross the wire on any of them.
const INSTALL_BUDGET: Duration = Duration::from_secs(60);

/// The stream phase, which carries the whole binary. Matched to
/// [`CURL_MAX_TIME_SECS`]: both move ~10 MiB over a link nobody
/// promised anything about.
const STREAM_BUDGET: Duration = Duration::from_secs(300);

/// One `curl`. Passed to `curl` itself as `--max-time` *and* used as
/// this side's own deadline, so a `curl` that ignores its own limit is
/// still bounded.
const CURL_MAX_TIME_SECS: u64 = 300;

/// The local `<candidate> identify` the sibling rung runs. A local
/// process that prints one line; anything slower is not answering.
const LOCAL_IDENTIFY_BUDGET: Duration = Duration::from_secs(10);

/// The lease-free wire stop.
const STOP_BUDGET: Duration = Duration::from_secs(30);

/// How long the stopped session gets to unlink its socket and release
/// its lock after the stop reply. See [`BootstrapJob::await_gone`].
const AWAIT_GONE_BUDGET: Duration = Duration::from_secs(30);

/// Interval between await-gone polls. Far coarser than
/// [`crate::session_launch::POLL_INTERVAL`], because every poll here is
/// an `ssh` exec rather than a `connect(2)`.
const AWAIT_GONE_POLL: Duration = Duration::from_millis(250);

/// `roost-session start` plus the ssh leg. The far side's forking
/// parent has its own 30s readiness wait, so this must comfortably
/// exceed it or a start that was still going to answer reads as a
/// timeout.
const START_BUDGET: Duration = Duration::from_secs(60);

/// How long `already-running` is re-tried after an await-gone said the
/// old session was gone. Short: what it covers is a loser overtaking
/// the winner of the socket-lock race.
const ALREADY_RUNNING_BUDGET: Duration = Duration::from_secs(5);

/// One `session.identify` over the exec chain.
const IDENTIFY_BUDGET: Duration = Duration::from_secs(30);

/// Cap on a probe exec's stdout (plan 039 §3.2). Discovery emits two
/// `uname` fields, the remote `$HOME`, and at most one path per ladder
/// rung.
const PROBE_STDOUT_CAP: usize = 64 * 1024;

/// Cap on the identity exec's stdout: the candidate paths it was asked
/// about plus one `identify` line. A binary that answers that question
/// with more than this is not answering it.
const IDENTITY_STDOUT_CAP: usize = 4 * 1024;

/// Cap for an exec whose output is a line or nothing — prepare, commit,
/// the readiness verdict, the `command -v` answer.
const SMALL_STDOUT_CAP: usize = 4 * 1024;

/// Cap on a downloaded release asset. `roost-session` is ~10 MiB
/// stripped; this leaves two orders of magnitude of headroom and still
/// bounds a server that answers a 404 page with a chunked infinity.
const ASSET_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Cap on the `.sha256` sibling: one 64-hex record and a filename.
const CHECKSUM_MAX_BYTES: u64 = 4 * 1024;

/// One read's worth of bytes while hashing.
const HASH_CHUNK: usize = 64 * 1024;

/// The `kind` segment of a bootstrap job's scratch-directory name.
const JOB_DIR_KIND: &str = "bootstrap";

/// The one refusal code [`BootstrapJob::stop_over_the_wire`] reads as
/// the stop it asked for. Spelled from
/// [`crate::client::ServerCode::ShuttingDown`]'s wire form, which
/// `the_stop_accepts_only_the_shutting_down_code` pins.
const SHUTTING_DOWN_CODE: &str = "shutting-down";

/// What one exec over the job master came back with.
#[derive(Debug)]
struct ExecOutcome {
    /// The exit code, or `None` when a signal took the child.
    code: Option<i32>,
    stdout: Vec<u8>,
    /// The tail of stderr, capped by [`crate::ssh`]'s reader.
    stderr: String,
}

impl ExecOutcome {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// This outcome, or [`Self::detail`] as the error — so that every
    /// phase spells "run it, and a non-zero exit is a failure of *this*
    /// phase" as one `map_err` rather than as a `map_err` and an `if`
    /// that have to keep naming the same error variant.
    fn require_ok(self) -> Result<Self, String> {
        if self.ok() {
            Ok(self)
        } else {
            Err(self.detail())
        }
    }

    /// A one-line description of how this exec failed, for a
    /// [`BootstrapError`]'s detail.
    ///
    /// Deliberately terse, and deliberately not
    /// [`SshFailure::message`]: every [`BootstrapError`] message already
    /// names the target and says what to do next, so splicing a second
    /// piece of advice copy into the middle of one produces a sentence
    /// that contradicts itself.
    fn detail(&self) -> String {
        match (self.code, last_line(&self.stderr)) {
            (Some(code), Some(line)) => format!("it exited {code}: {line}"),
            (Some(code), None) => format!("it exited {code} with nothing on stderr"),
            (None, Some(line)) => format!("it was killed by a signal: {line}"),
            (None, None) => "it was killed by a signal".to_string(),
        }
    }

    /// The first non-empty line of stdout, trimmed.
    fn first_line(&self) -> Option<String> {
        String::from_utf8_lossy(&self.stdout)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string)
    }
}

/// One spawned `ssh` exec with its three streams already in hand.
///
/// [`BootstrapJob::run_command`] and [`BootstrapJob::bridge_call`] differ
/// entirely in what they do with these — a bulk write and a capped read
/// versus one JSON exchange — and not at all in how they get them.
struct JobChild {
    child: tokio::process::Child,
    /// The draining stderr tail, capped by [`crate::ssh`]'s reader.
    tail: JoinHandle<Tail>,
    sink: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
}

/// What an exec is fed on stdin.
#[derive(Clone, Copy)]
enum ExecStdin<'a> {
    /// Nothing at all — the child sees an immediate EOF.
    Empty,
    /// A script, for a remote `/bin/sh -s`.
    Script(&'a str),
    /// A resolved source's bytes: the install's stream phase. Streamed
    /// off the verified descriptor when there is one, and only by path
    /// when there is not.
    Source(&'a ResolvedSource),
}

/// The remote command that reads a script off stdin.
///
/// Two words with nothing to quote, so the user's login shell — which
/// is what sshd hands a remote command to — parses it the same way
/// every shell would. `-s` is what makes `sh` read its program from
/// stdin, which is the whole discipline: a script that arrives as data
/// is never re-parsed by anything, however many apostrophes a path has
/// in it.
const SH_STDIN: &str = "/bin/sh -s";

/// What the probe concluded, and what the rest of the job needs from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub outcome: ProbeOutcome,
    /// The remote's architecture, for the source ladder.
    pub arch: RemoteArch,
    /// [`Discovery::home`] — the remote's `$HOME`, so a caller can
    /// expand the install destination the far side would write and
    /// compare it against [`ProbeOutcome`]'s path.
    pub home: String,
    /// Every candidate the discovery step found, in ladder order.
    /// Carried for logs and for the copy that names *where* a start-only
    /// flow would start from.
    pub candidates: Vec<String>,
}

/// Which side of the commit an `identify` answer is being judged on —
/// the only thing that differs between the two checks is which failure
/// family, and therefore which copy, a refusal produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Staging {
    /// The temporary, before anything replaced the destination.
    Staged,
    /// The installed file, after the commit.
    Committed,
}

/// How strict [`BootstrapJob::post_start_identify`] is about the session
/// that answered.
///
/// The two modes exist because the question is genuinely different, and
/// collapsing them either refuses a session this client can talk to or
/// accepts an upgrade that did not happen:
///
/// * [`IdentityGate::Installed`] — this job just wrote a binary. The
///   whole point was to make *that build* the one serving, so nothing
///   short of the full triple proves it worked. An upgrade that changes
///   only `app_version` (no ghostty bump) would otherwise land on disk,
///   leave the **old process** serving, and report success.
/// * [`IdentityGate::Existing`] — nothing was installed; the session
///   that is there is the session that was always there. The right bar
///   is the runtime attach gate — protocol plus `libghostty_build`,
///   `roost-iced`'s `check_compatibility` — because that is exactly the
///   set of sessions this client can then talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityGate {
    /// After an install: the exact triple, `app_version` included.
    Installed,
    /// Start-only: the attach gate, protocol + build.
    Existing,
}

/// Where the bytes an install streams came from — the dialog names the
/// *actual* origin, never a friendlier one (plan 039 §3.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceOrigin {
    /// The file [`INSTALL_BIN_ENV`] names.
    Override,
    /// This client's own `roost-session`.
    Sibling,
    /// A release asset, downloaded and checksum-verified.
    Asset { base: String, overridden: bool },
}

impl SourceOrigin {
    /// The "from where" phrase, in the one place both the consent
    /// dialog's *prediction* ([`SourcePreview::describe`]) and the
    /// resolved source's *fact* ([`ResolvedSource::describe`]) read it
    /// from — so a dialog cannot promise one origin in wording the log
    /// then reports differently.
    ///
    /// An overridden asset base is named as itself. Rendering it as
    /// github.com would be a lie in exactly the situation — a fixture
    /// server, a mirror — where the user most needs the truth.
    ///
    /// Every arm is a noun phrase, never a clause of its own: the only
    /// caller that assembles a sentence from it ([`SourcePreview::describe`],
    /// read into the consent dialog's "from {source}" line) supplies the
    /// preposition itself, so an arm that started with one — "downloaded
    /// from X" — used to double it into "from downloaded from X".
    pub fn describe(&self, override_path: Option<&Path>) -> String {
        match self {
            Self::Override => match override_path {
                Some(path) => format!("{} ({INSTALL_BIN_ENV})", path.display()),
                None => format!("the file {INSTALL_BIN_ENV} names"),
            },
            Self::Sibling => "this Roost's own roost-session".to_string(),
            Self::Asset { base, overridden } => {
                let where_from = if *overridden {
                    format!("{base} ({ASSET_BASE_ENV})")
                } else {
                    base.clone()
                };
                format!("the release at {where_from}, checksum-verified")
            }
        }
    }
}

/// Where an install's bytes will come from, decided as far as it can be
/// without running anything. See [`BootstrapOptions::source_preview`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePreview {
    /// The rung the ladder will try first.
    pub first: SourceOrigin,
    /// Where it falls through to if `first` turns out not to be this
    /// build — only ever a sibling that fails its local `identify`.
    pub fallback: Option<SourceOrigin>,
    /// The path [`SourceOrigin::Override`] names, carried so the copy
    /// can show it without the caller re-reading the options.
    override_path: Option<PathBuf>,
}

impl SourcePreview {
    /// The consent dialog's "from where" line.
    ///
    /// A fall-through is stated rather than hidden: "or X if that turns
    /// out not to be this build" is longer than a single claim and it is
    /// the only version of the sentence that stays true whichever way the
    /// resolution goes.
    pub fn describe(&self) -> String {
        let first = self.first.describe(self.override_path.as_deref());
        match &self.fallback {
            Some(fallback) => format!(
                "{first} — or {}, if that turns out not to be this build",
                fallback.describe(None)
            ),
            None => first,
        }
    }
}

/// The local file an install will stream, and where it came from.
///
/// **Opaque on purpose**: the only way to get one is [`resolve_source`]
/// (or [`BootstrapJob::resolve_source`]), so a
/// [`BootstrapJob::install`] cannot be handed a source that never
/// climbed the ladder — never had its override checked, its sibling
/// identified, or its download hashed.
///
/// For a downloaded asset it also carries the **open handle the hash was
/// computed over**. Hashing a path and then re-opening that path is a
/// window a local attacker can write through; streaming a `dup` of the
/// verified descriptor closes it, so the bytes that were checked are
/// provably the bytes that get sent.
#[derive(Debug)]
pub struct ResolvedSource {
    path: PathBuf,
    origin: SourceOrigin,
    /// The verified descriptor, for the rungs that verify bytes.
    ///
    /// `None` for the override and sibling rungs, and **deliberately**:
    /// no published checksum exists for a local file, so there is
    /// nothing to verify it against here. Their gate is the local
    /// `identify` (the sibling rung runs it before accepting the
    /// binary), then the remote staged verify and the post-commit
    /// re-verify — the same two gates every source passes, on the far
    /// side, where they decide what actually runs.
    verified: Option<std::fs::File>,
}

impl ResolvedSource {
    /// The local file the install streams.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Which rung produced it.
    pub fn origin(&self) -> &SourceOrigin {
        &self.origin
    }

    /// A rewound `dup` of the verified descriptor, or `None` for a rung
    /// that has none.
    ///
    /// A `dup` rather than the descriptor itself, so the source stays
    /// streamable more than once — a retried install must not silently
    /// fall back to the path it was built to avoid.
    fn reopen_verified(&self) -> Option<std::io::Result<std::fs::File>> {
        use std::io::{Seek, SeekFrom};

        let file = self.verified.as_ref()?;
        Some(file.try_clone().and_then(|mut clone| {
            clone.seek(SeekFrom::Start(0))?;
            Ok(clone)
        }))
    }

    /// Where these bytes actually came from, for the log and the
    /// completion copy. [`SourceOrigin::describe`] is the shared
    /// wording.
    pub fn describe(&self) -> String {
        self.origin.describe(Some(&self.path))
    }
}

/// A completed install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// The destination the far side reported and this side validated.
    pub dest: String,
    /// Set when a shell on the far side would not find *this*
    /// `roost-session` by name. Not a failure — Roost execs the absolute
    /// path — so it rides along on the success value for the toast to
    /// append (plan 039 §3.4). Nothing edits a dotfile over this.
    pub path_warning: Option<String>,
}

/// What the far side's bridge said to one op.
#[derive(Debug)]
enum BridgeAnswer {
    /// The session answered.
    Ok(serde_json::Value),
    /// The session answered, refusing. The wire `code` is carried
    /// separately from the rendered message because one caller routes on
    /// it: [`BootstrapJob::stop_over_the_wire`] treats *only*
    /// `shutting-down` as "the stop it asked for is already happening",
    /// and everything else — `connect-required`, `internal`, a code this
    /// build has never heard of — as the refusal it is.
    Refused { code: String, detail: String },
    /// The bridge ran and found no session to talk to.
    NoSession,
}

/// One bootstrap of one host: a private `ssh` master, and the bounded
/// execs that run the probe and the install across it.
///
/// **The master is job-scoped and private** (plan 039 §3.2), which
/// settles two things at once that pull in opposite directions. Reusing
/// the *tunnel's* master would inherit whatever state it is in — a
/// wedged `ControlPersist` master is exactly the condition a user is
/// most likely to be bootstrapping their way out of — so this opens a
/// fresh one, in a scratch directory of its own, on a path nothing else
/// knows. But running each of the job's ~nine execs outside a master
/// entirely would mean ~nine authentication handshakes, and on a
/// confirm-mode or biometric key that is nine taps for one button press.
/// One private master is both: immune to the old one, and one handshake.
///
/// Its directory is named the way a verify's is
/// ([`crate::ssh::one_shot_dir_name`]), so the tunnel's sweep can never
/// mistake a running job's master for a crashed tunnel's leftovers and
/// tear it down mid-install.
///
/// [`Self::close`] is the ordered teardown and [`Drop`] is the safety
/// net behind it; both are idempotent, and either way the master gets an
/// explicit `-O exit` *before* the directory holding its control socket
/// goes.
pub struct BootstrapJob {
    target: String,
    ssh_bin: PathBuf,
    dir: PathBuf,
    config_path: PathBuf,
    ctl_path: PathBuf,
    options: BootstrapOptions,
    /// [`OPEN`] / [`CLOSING`] / [`CLOSED`].
    ///
    /// Three states and not a `bool`, because the interesting one is the
    /// middle: a `close()` future dropped part-way through its teardown
    /// has neither closed the master nor left the flag saying so. A
    /// two-state flag set *before* the teardown would make [`Drop`] a
    /// no-op for exactly that case, leaking the scratch directory
    /// forever ([`crate::ssh::parse_scratch_dir_name`] refuses these
    /// names, so no sweep reclaims them) and leaving the master up until
    /// `ControlPersist` expires. Only completing the teardown writes
    /// [`CLOSED`].
    state: AtomicU8,
    /// Serial for this job's download scratch directories, so two
    /// resolutions never share — and never overwrite — one already
    /// verified asset.
    downloads: AtomicU64,
}

/// Nothing has been torn down.
const OPEN: u8 = 0;
/// A teardown started and has not been seen through.
const CLOSING: u8 = 1;
/// The master was exited and the directory removed.
const CLOSED: u8 = 2;

impl BootstrapJob {
    /// Claim a private scratch directory, write the generated
    /// `ssh_config` into it, and hand back a job. Connects nothing —
    /// the first exec opens the master.
    ///
    /// A failure here is reported as [`BootstrapError::Probe`] whatever
    /// the job was about to do, and that is not a shrug: a job that
    /// could not create its own directory has looked at nothing and
    /// written nothing on the far side, which is precisely what that
    /// family's copy says.
    pub async fn open(
        target: &SshTarget,
        ssh: &SshTunnelOptions,
        options: BootstrapOptions,
    ) -> Result<Self, BootstrapError> {
        let local = |detail: String| BootstrapError::Probe(detail);

        let dir = pick_socket_dir(
            &ssh.scratch_parents,
            &crate::ssh::one_shot_dir_name(JOB_DIR_KIND),
        )
        .map_err(|error| local(format!("{error:#}")))?;
        crate::ssh::create_private_dir(&dir)
            .map_err(|error| local(format!("creating {}: {error}", dir.display())))?;
        let config_path = dir.join(crate::ssh::CONFIG_FILE);
        if let Err(error) =
            crate::ssh::write_private_file(&config_path, ssh.config_paths.render().as_bytes())
        {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(local(format!(
                "writing {}: {error:#}",
                config_path.display()
            )));
        }

        Ok(Self {
            target: target.raw.clone(),
            ssh_bin: ssh.ssh_bin.clone(),
            ctl_path: dir.join(crate::ssh::CTL_FILE),
            config_path,
            dir,
            options,
            state: AtomicU8::new(OPEN),
            downloads: AtomicU64::new(0),
        })
    }

    /// The options this job resolves sources against.
    pub fn options(&self) -> &BootstrapOptions {
        &self.options
    }

    /// Close the private master and remove the scratch directory.
    /// Idempotent, and ordered: `-O exit` is the only way to address the
    /// master, and its address is a file in the directory being removed.
    ///
    /// The closed mark goes on **after** the teardown, so a `close()`
    /// future that is dropped part-way leaves [`Drop`] still owing the
    /// work — and still doing it.
    pub async fn close(&self) {
        if self.state.load(Ordering::SeqCst) == CLOSED {
            return;
        }
        self.state.store(CLOSING, Ordering::SeqCst);
        exit_master(
            &self.ssh_bin,
            &self.config_path,
            &self.ctl_path,
            &self.target,
        )
        .await;
        if let Err(error) = tokio::fs::remove_dir_all(&self.dir).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    dir = %self.dir.display(),
                    %error,
                    "bootstrap: could not remove the job scratch directory"
                );
            }
        }
        self.state.store(CLOSED, Ordering::SeqCst);
    }

    // --------------------------------------------------------------
    // The probe ladder
    // --------------------------------------------------------------

    /// Three round trips: what platform is this, what does its own shell
    /// find, and who are the binaries that turned up.
    ///
    /// Read-only from end to end — nothing here writes, starts or stops
    /// anything, which is what makes it safe to run *before* the consent
    /// dialog (plan 039 §3.5).
    pub async fn probe(&self) -> Result<Probe, BootstrapError> {
        let discovery = self
            .run_script(
                &discovery_script(self.options.jail_fs_root),
                PROBE_BUDGET,
                PROBE_STDOUT_CAP,
            )
            .await
            .and_then(ExecOutcome::require_ok)
            .map_err(BootstrapError::Probe)?;
        let found = parse_discovery(&discovery.stdout)?;
        check_os(&found.os)?;
        let arch = map_arch(&found.arch)?;

        let mut candidates = found.candidates;
        if let Some(extra) = self.remote_shell_hit(&candidates).await {
            candidates.push(extra);
        }

        let outcome = if candidates.is_empty() {
            ProbeOutcome::Missing
        } else {
            let identity = self
                .run_script(
                    &identity_script(&candidates),
                    PROBE_BUDGET,
                    IDENTITY_STDOUT_CAP,
                )
                .await
                .and_then(ExecOutcome::require_ok)
                .map_err(BootstrapError::Probe)?;
            let pairs = parse_identity_pairs(&identity.stdout, &candidates)?;
            classify_probe(&self.options.expected, &pairs)
        };

        tracing::info!(
            host = %self.target,
            os = %found.os,
            %arch,
            candidates = candidates.len(),
            outcome = ?outcome,
            "bootstrap: probed"
        );
        Ok(Probe {
            outcome,
            arch,
            home: found.home,
            candidates,
        })
    }

    /// What the *remote shell* resolves `roost-session` to, when that is
    /// something the ladder did not already find.
    ///
    /// Appended after every ladder rung, never spliced among them. The
    /// probe's verdict is about the rung [`exec_chain_command`] will
    /// actually exec, and this answer comes from a shell the exec chain
    /// is not run by — putting it at position 2 would let a path the
    /// transport cannot reach decide whether an install is offered.
    ///
    /// Every failure is silence, per plan 039 §3.2: `command -v` exits
    /// non-zero for "not found", which is an answer and not a fault, and
    /// a step that exists to catch an unusual `PATH` must not be able to
    /// fail a probe that has already succeeded without it.
    async fn remote_shell_hit(&self, known: &[String]) -> Option<String> {
        let outcome = match self
            .run_command(
                &path_check_command(),
                ExecStdin::Empty,
                PROBE_BUDGET,
                SMALL_STDOUT_CAP,
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(detail) => {
                tracing::debug!(host = %self.target, detail, "bootstrap: the remote-shell PATH probe did not run");
                return None;
            }
        };
        if !outcome.ok() {
            return None;
        }
        outcome
            .first_line()
            .filter(|path| path.starts_with('/') && !known.iter().any(|known| known == path))
    }

    // --------------------------------------------------------------
    // The install
    // --------------------------------------------------------------

    /// Prepare, stream, verify the staged file, commit, re-verify, and
    /// check the far side's `PATH`.
    ///
    /// The order is the plan's, and the load-bearing part of it is that
    /// **nothing replaces the destination until the staged bytes have
    /// identified themselves as this client's exact build** (§3.4). A
    /// wrong-arch override, a checksum-valid-but-wrong binary, a
    /// truncated stream: each one ends with the temporary removed and the
    /// previous install byte-for-byte intact.
    ///
    /// Every failing path out of here after prepare runs
    /// [`cleanup_script`] and [`rollback_script`] best-effort, because
    /// the temporary is named for the remote's pid and nothing else will
    /// ever come looking for it.
    ///
    /// **A cancelled `install()` future is the one path that does not**,
    /// and the comment is here rather than a claim that it does: `Drop`
    /// has no runtime to await a remote exec on, and this module's own
    /// prior art (`ssh::blocking_exit_master`'s doc) records that
    /// spawning the teardown instead produces a task that reliably never
    /// runs — which for a *remote* round trip would also mean blocking a
    /// runtime thread on a host that may be exactly the wedged one being
    /// bootstrapped. So cancellation leaves a `<dest>.tmp.<pid>` behind,
    /// and [`prepare_script`] sweeps it on the next attempt: the name is
    /// one this side generated, so the file is ours by construction and
    /// the leak self-heals rather than accumulating. A cancelled install
    /// never leaves a `.bak.` behind either, because the commit exec
    /// creates and consumes one within a single exec's lifetime.
    pub async fn install(&self, source: &ResolvedSource) -> Result<Installed, BootstrapError> {
        let prepared = self
            .run_script(&prepare_script(), INSTALL_BUDGET, SMALL_STDOUT_CAP)
            .await
            .and_then(ExecOutcome::require_ok)
            .map_err(|detail| BootstrapError::Install {
                phase: InstallPhase::Prepare,
                detail,
            })?;
        let staged = parse_prepare(&prepared.stdout)?;

        match self.install_staged(source, &staged).await {
            Ok(()) => self.discard_backup(&staged.backup).await,
            Err(error) => {
                let restored = self.rollback(&staged.dest, &staged.backup).await;
                self.cleanup(&staged.tmp).await;
                return Err(match error {
                    // The one family whose copy depends on what the
                    // rollback actually managed.
                    BootstrapError::PostCommit { detail, .. } => {
                        BootstrapError::PostCommit { detail, restored }
                    }
                    other => other,
                });
            }
        }

        Ok(Installed {
            dest: staged.dest.clone(),
            path_warning: self.path_warning(&staged.dest).await,
        })
    }

    /// [`Self::install`]'s middle, split out so that one `?` on any of
    /// its steps is one cleanup at the caller — rather than four
    /// hand-written cleanup paths that have to stay in agreement.
    async fn install_staged(
        &self,
        source: &ResolvedSource,
        staged: &Staged,
    ) -> Result<(), BootstrapError> {
        self.run_command(
            &stream_command(&staged.tmp),
            ExecStdin::Source(source),
            STREAM_BUDGET,
            SMALL_STDOUT_CAP,
        )
        .await
        .and_then(ExecOutcome::require_ok)
        .map_err(|detail| BootstrapError::Install {
            phase: InstallPhase::Stream,
            detail,
        })?;

        let verified = self
            .run_script(
                &verify_staged_script(&staged.tmp),
                INSTALL_BUDGET,
                SMALL_STDOUT_CAP,
            )
            .await
            .and_then(ExecOutcome::require_ok)
            .map_err(BootstrapError::Verify)?;
        self.require_identity(&String::from_utf8_lossy(&verified.stdout), Staging::Staged)?;

        self.run_script(
            &commit_script(&staged.tmp, &staged.dest, &staged.backup),
            INSTALL_BUDGET,
            SMALL_STDOUT_CAP,
        )
        .await
        .and_then(ExecOutcome::require_ok)
        .map_err(|detail| BootstrapError::Install {
            phase: InstallPhase::Commit,
            detail,
        })?;

        // The same question again, of the file that is now installed:
        // the staged verify proved the *temporary* was right, and a
        // `mv` is not the only thing that can happen to a path between
        // two execs.
        self.identify_binary(&staged.dest).await
    }

    /// Ask the binary at `path` who it is, and require this client's
    /// exact triple.
    ///
    /// Reported as [`BootstrapError::PostCommit`], not
    /// [`BootstrapError::Verify`]: by the time this runs the new file is
    /// the installed one, and the two families exist to tell those apart
    /// in the copy.
    async fn identify_binary(&self, path: &str) -> Result<(), BootstrapError> {
        let asked = vec![path.to_string()];
        let outcome = self
            .run_script(
                &identity_script(&asked),
                INSTALL_BUDGET,
                IDENTITY_STDOUT_CAP,
            )
            .await
            .and_then(ExecOutcome::require_ok)
            .map_err(|detail| BootstrapError::PostCommit {
                detail,
                restored: false,
            })?;
        let pairs = parse_identity_pairs(&outcome.stdout, &asked)?;
        let Some((_, stdout)) = pairs.first() else {
            return Err(BootstrapError::PostCommit {
                detail: format!("{path} was gone by the time it was asked to identify itself"),
                restored: false,
            });
        };
        self.require_identity(stdout, Staging::Committed)
    }

    /// The install rule, applied to one `identify` answer.
    fn require_identity(&self, stdout: &str, staging: Staging) -> Result<(), BootstrapError> {
        let expected = &self.options.expected;
        let refuse = |detail: String| match staging {
            Staging::Staged => BootstrapError::Verify(detail),
            Staging::Committed => BootstrapError::PostCommit {
                detail,
                restored: false,
            },
        };
        match parse_identity_line(stdout) {
            Some(found) if identity_matches(expected, &found) => Ok(()),
            Some(found) => Err(refuse(format!(
                "it reports {} / protocol {} / {}, and this Roost needs {} / protocol {} / {}",
                found.app_version,
                found.session_protocol,
                found.libghostty_build,
                expected.app_version,
                expected.session_protocol,
                expected.libghostty_build
            ))),
            None => Err(refuse("it would not identify itself at all".to_string())),
        }
    }

    /// Best-effort `rm -f` of the staged temporary. Never returns a
    /// failure: this runs on a path that is already failing, and a
    /// cleanup that can fail loudly is one more failure to classify.
    async fn cleanup(&self, tmp: &str) {
        if let Err(detail) = self
            .run_script(&cleanup_script(tmp), INSTALL_BUDGET, SMALL_STDOUT_CAP)
            .await
            .and_then(ExecOutcome::require_ok)
        {
            tracing::warn!(host = %self.target, tmp, detail, "bootstrap: could not remove the staged file");
        }
    }

    /// Put the incumbent back if the commit had already moved it aside.
    /// Answers whether it did — which is what
    /// [`BootstrapError::PostCommit`]'s copy turns on.
    ///
    /// Best-effort like [`Self::cleanup`], with one difference: a
    /// rollback that could not run is reported as *not* restored, so the
    /// copy never claims a restore that did not happen.
    async fn rollback(&self, dest: &str, backup: &str) -> bool {
        match self
            .run_script(
                &rollback_script(dest, backup),
                INSTALL_BUDGET,
                SMALL_STDOUT_CAP,
            )
            .await
            .and_then(ExecOutcome::require_ok)
        {
            Ok(outcome) => outcome.first_line().is_some_and(|line| line == "restored"),
            Err(detail) => {
                tracing::warn!(host = %self.target, backup, detail, "bootstrap: could not put the previous install back");
                false
            }
        }
    }

    /// Drop the incumbent's copy once the new install has answered.
    /// Best-effort: a backup left behind is a stale file, not a broken
    /// install, and failing the whole job over it would be worse.
    async fn discard_backup(&self, backup: &str) {
        if let Err(detail) = self
            .run_script(
                &discard_backup_script(backup),
                INSTALL_BUDGET,
                SMALL_STDOUT_CAP,
            )
            .await
            .and_then(ExecOutcome::require_ok)
        {
            tracing::warn!(host = %self.target, backup, detail, "bootstrap: could not remove the previous install's backup");
        }
    }

    /// Does a shell on the far side find `dest` by name?
    ///
    /// A warning and never a failure, and never a dotfile edit: Roost
    /// execs the absolute path, so a `PATH` that misses the install
    /// costs the user nothing until they type `roost-session` there
    /// themselves.
    async fn path_warning(&self, dest: &str) -> Option<String> {
        let target = &self.target;
        let dir = dest.rsplit_once('/').map_or(dest, |(dir, _)| dir);
        // A `?` and not a fall-through: a check that could not run says
        // nothing about the far side's `PATH`, and the arms below both
        // claim it does.
        let outcome = self
            .run_command(
                &path_check_command(),
                ExecStdin::Empty,
                PROBE_BUDGET,
                SMALL_STDOUT_CAP,
            )
            .await
            .ok()?;
        let resolved = outcome
            .require_ok()
            .ok()
            .and_then(|outcome| outcome.first_line());
        match resolved {
            Some(path) if path == dest => None,
            Some(other) => Some(format!(
                "a shell on {target} finds roost-session at {other}, not the {dest} that was just \
                 installed — Roost doesn't need its PATH, but that shell's own roost-session won't \
                 be this one."
            )),
            None => Some(format!(
                "{dir} isn't on {target}'s PATH — Roost doesn't need it, but roost-session won't \
                 be runnable by name in a shell there."
            )),
        }
    }

    // --------------------------------------------------------------
    // Stop, wait, start, check
    // --------------------------------------------------------------

    /// Stop the running session over the wire, lease-free.
    ///
    /// `session.stop` is dispatched *before* the lease gate, so this
    /// sends it raw: no `session.connect`, no takeover, none of the
    /// eviction side-effects a client attach would have. The in-repo
    /// precedent is `roost-session`'s own signal handler, which
    /// self-dials exactly this.
    ///
    /// **`client-bridge: no session` is success**, not a failure: the
    /// job asked for the session to be gone, and it is. So is one
    /// specific refusal — `shutting-down`, the code a session answers
    /// with while it is already on its way out — because
    /// [`Self::await_gone`] is the gate that decides whether the process
    /// actually went, and it does not care who asked it to.
    ///
    /// **Every other refusal is a failed stop.** Flattening them all into
    /// "it is shutting down" would read `connect-required`,
    /// `not-implemented`, `internal` and a code this build has never
    /// heard of as consent to carry on, and the job would then wait out
    /// the whole await-gone budget before reporting a timeout instead of
    /// the reason the session gave in the first sentence.
    pub async fn stop_over_the_wire(&self) -> Result<(), BootstrapError> {
        match self
            .bridge_call(ops::SESSION_STOP, scaled(STOP_BUDGET))
            .await
            .map_err(BootstrapError::Stop)?
        {
            BridgeAnswer::Ok(_) => Ok(()),
            BridgeAnswer::NoSession => {
                tracing::info!(host = %self.target, "bootstrap: nothing to stop over there");
                Ok(())
            }
            BridgeAnswer::Refused { code, detail } if code == SHUTTING_DOWN_CODE => {
                tracing::info!(host = %self.target, detail, "bootstrap: the session was already shutting down; waiting for it to go");
                Ok(())
            }
            BridgeAnswer::Refused { detail, .. } => Err(BootstrapError::Stop(detail)),
        }
    }

    /// Poll until the far side reports no session.
    ///
    /// Load-bearing, not belt-and-braces: `session.stop` replies from a
    /// dispatcher whose process then unlinks its socket and releases its
    /// lock *after* the reply is on its way. A start that follows the
    /// reply directly can lose that race, print `already-running
    /// pid=<the dying one>` and exit 0 — a masked failure on the happy
    /// path.
    pub async fn await_gone(&self) -> Result<(), BootstrapError> {
        self.await_gone_within(scaled(AWAIT_GONE_BUDGET)).await
    }

    /// [`Self::await_gone`] against an explicit, already-scaled budget.
    ///
    /// Exposed for the test that pins the bound — the shipped entry
    /// point takes no argument on purpose.
    ///
    /// **The budget bounds the whole loop, not each poll.** Checking the
    /// deadline only *after* a complete [`Self::bridge_call`] meant one
    /// hung poll could eat the entire budget and a second full poll
    /// would still start behind it, so the wait ran to ~2× while the
    /// error reported 1×. Each poll now gets whatever is left (capped at
    /// [`IDENTIFY_BUDGET`]) and the sleep between them is clamped the
    /// same way.
    #[doc(hidden)]
    pub async fn await_gone_within(&self, budget: Duration) -> Result<(), BootstrapError> {
        let deadline = Instant::now() + budget;
        let expired = |last: &str| {
            Err(BootstrapError::Stop(format!(
                "it was still holding its socket {}s after being asked to stop ({last})",
                budget.as_secs().max(1)
            )))
        };
        let mut last = "it never answered".to_string();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return expired(&last);
            }
            match self
                .bridge_call(
                    ops::SESSION_IDENTIFY,
                    remaining.min(scaled(IDENTIFY_BUDGET)),
                )
                .await
            {
                Ok(BridgeAnswer::NoSession) => return Ok(()),
                Ok(BridgeAnswer::Ok(_)) => last = "it is still serving".to_string(),
                Ok(BridgeAnswer::Refused { detail, .. }) => last = detail,
                // A poll that could not run at all is not evidence the
                // session is gone, and the deadline above is what stops
                // this becoming a loop.
                Err(detail) => last = detail,
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return expired(&last);
            }
            tokio::time::sleep(scaled(AWAIT_GONE_POLL).min(remaining)).await;
        }
    }

    /// Start the session using the binary at `path` — the just-committed
    /// destination after an install, or the rung the probe found for a
    /// start-only flow.
    ///
    /// The resolved path and never the ladder: a host whose only
    /// `roost-session` is a deb at `/usr/bin` must not be started
    /// through a `~/.local/bin` that does not exist.
    ///
    /// `already-running` is retried briefly and then accepted, because
    /// after an await-gone the only thing it can mean is a loser of the
    /// socket-lock race overtaking the winner. What makes accepting it
    /// safe is [`Self::post_start_identify`], which asks the session
    /// that is actually serving who it is.
    pub async fn start(&self, path: &str) -> Result<Verdict, BootstrapError> {
        let deadline = Instant::now() + scaled(ALREADY_RUNNING_BUDGET);
        loop {
            let verdict = self.start_once(path).await?;
            match verdict {
                Verdict::AlreadyRunning(_) if Instant::now() < deadline => {
                    tokio::time::sleep(scaled(AWAIT_GONE_POLL)).await;
                }
                Verdict::Error(reason) => return Err(BootstrapError::Start(reason)),
                verdict => return Ok(verdict),
            }
        }
    }

    /// One `roost-session start`, read the way the binary actually
    /// behaves.
    ///
    /// **The verdict line is read before the exit status.** A real
    /// `roost-session start` that fails writes `error: <reason>` to
    /// *stdout* and exits **1** — so requiring a zero exit first threw
    /// the reason away and replaced it with "it exited 1", and
    /// [`Verdict::Error`] became a variant nothing could ever produce.
    /// The exit status is still what speaks when there is no verdict
    /// line at all, which is the only case where it says anything the
    /// line does not.
    async fn start_once(&self, path: &str) -> Result<Verdict, BootstrapError> {
        let outcome = self
            .run_command(
                &start_script(path),
                ExecStdin::Empty,
                START_BUDGET,
                SMALL_STDOUT_CAP,
            )
            .await
            .map_err(BootstrapError::Start)?;
        if let Some(line) = outcome.first_line() {
            return Ok(Verdict::parse(&line));
        }
        outcome.require_ok().map_err(BootstrapError::Start)?;
        Err(BootstrapError::Start(
            "it printed no readiness line at all".to_string(),
        ))
    }

    /// Ask the session that is now *running* who it is, before the job
    /// reports success.
    ///
    /// The on-disk checks proved a file was right. This proves the
    /// process is — which is a different claim, and the one the user
    /// cares about. It catches a binary that started and crashed, an
    /// `already-running` that was in fact the old session all along, and
    /// the start-only flow, where nothing but the probe's on-disk read
    /// has been checked at all.
    ///
    /// `gate` is what makes those two flows different rather than
    /// merely similar — see [`IdentityGate`]. After an install the whole
    /// triple is required, because `already-running` is accepted as a
    /// success once the retry budget expires and an `app_version`-only
    /// upgrade would otherwise report success with the **old process**
    /// still serving.
    pub async fn post_start_identify(
        &self,
        gate: IdentityGate,
    ) -> Result<SessionIdentify, BootstrapError> {
        let answer = self
            .bridge_call(ops::SESSION_IDENTIFY, scaled(IDENTIFY_BUDGET))
            .await
            .map_err(BootstrapError::Start)?;
        let value = match answer {
            BridgeAnswer::Ok(value) => value,
            BridgeAnswer::NoSession => {
                return Err(BootstrapError::Start(
                    "it reported ready and then no session was there".to_string(),
                ))
            }
            BridgeAnswer::Refused { detail, .. } => return Err(BootstrapError::Start(detail)),
        };
        let identity: SessionIdentify = serde_json::from_value(value).map_err(|error| {
            BootstrapError::Start(format!("its identity did not parse: {error}"))
        })?;

        let expected = &self.options.expected;
        if identity.session_protocol != expected.session_protocol
            || identity.libghostty_build != expected.libghostty_build
        {
            return Err(BootstrapError::Start(format!(
                "the session that came up speaks protocol {} / {}, and this Roost needs {} / {}",
                identity.session_protocol,
                identity.libghostty_build,
                expected.session_protocol,
                expected.libghostty_build
            )));
        }
        if gate == IdentityGate::Installed && identity.app_version != expected.app_version {
            return Err(BootstrapError::Start(format!(
                "roost-session {} was installed on {}, but the session still serving there is \
                 {} — the old one never went away",
                expected.app_version, self.target, identity.app_version
            )));
        }
        Ok(identity)
    }

    // --------------------------------------------------------------
    // Execs
    // --------------------------------------------------------------

    /// Argv for one exec over this job's private master: the transport's
    /// own connect shape, pointed at a control socket nothing else knows
    /// about.
    fn job_argv(&self, remote_command: &str) -> Vec<String> {
        let mut argv =
            crate::ssh::mux_connect_argv(&self.config_path, &self.ctl_path, &self.target);
        argv.push(remote_command.to_string());
        argv
    }

    /// Spawn one exec over the job master with all three streams piped,
    /// and the stderr tail already draining.
    ///
    /// The tail has to be taken before anything else is awaited: an
    /// unread stderr pipe fills and blocks the very child both callers
    /// then go on to wait for.
    fn spawn_exec(&self, remote_command: &str) -> Result<JobChild, String> {
        let argv = self.job_argv(remote_command);
        let mut child = crate::ssh::spawn_ssh_command(
            &self.ssh_bin,
            &argv,
            Stdio::piped(),
            Stdio::piped(),
            Stdio::piped(),
        )
        .map_err(|error| error.to_string())?;
        let tail = crate::ssh::spawn_stderr_tail(&mut child);
        let sink = child
            .stdin
            .take()
            .ok_or_else(|| "no stdin pipe on the ssh child".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "no stdout pipe on the ssh child".to_string())?;
        Ok(JobChild {
            child,
            tail,
            sink,
            stdout,
        })
    }

    /// Feed `script` to a remote `/bin/sh -s` and collect what it says.
    async fn run_script(
        &self,
        script: &str,
        budget: Duration,
        stdout_cap: usize,
    ) -> Result<ExecOutcome, String> {
        self.run_command(SH_STDIN, ExecStdin::Script(script), budget, stdout_cap)
            .await
    }

    /// Run one remote command over the job master, bounded on every
    /// axis: a scaled deadline, a stdout cap, [`crate::ssh`]'s stderr
    /// tail cap, and a kill-and-reap on expiry.
    ///
    /// **Write-then-wait ordering** is the subtle part. Whatever is
    /// going onto the child's stdin is written by a task of its own, and
    /// its error is only ever reported when the child *also* exited
    /// cleanly. A remote script that refuses and exits before reading
    /// its input hands this side an `EPIPE` on the write and a real
    /// diagnosis on the child's stderr; reporting the former would bury
    /// the latter, and "broken pipe" is not something a user can act on.
    async fn run_command(
        &self,
        remote_command: &str,
        stdin: ExecStdin<'_>,
        budget: Duration,
        stdout_cap: usize,
    ) -> Result<ExecOutcome, String> {
        let budget = scaled(budget);
        let deadline = Instant::now() + budget;
        let JobChild {
            mut child,
            tail,
            sink,
            stdout,
        } = self.spawn_exec(remote_command)?;
        let writer = spawn_writer(sink, stdin);

        let timed_out =
            |what: &str| format!("{what} did not finish within {}s", budget.as_secs().max(1));

        let read = match tokio::time::timeout_at(deadline, read_capped(stdout, stdout_cap)).await {
            Ok(read) => read,
            Err(_elapsed) => {
                writer.abort();
                reap_by(&mut child, Instant::now()).await;
                return Err(timed_out("it"));
            }
        };
        let status = match tokio::time::timeout_at(deadline, child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(error)) => return Err(format!("waiting for ssh failed: {error}")),
            Err(_elapsed) => {
                writer.abort();
                reap_by(&mut child, Instant::now()).await;
                return Err(timed_out("it"));
            }
        };
        // The child is gone, so its end of the pipe is closed and a
        // still-blocked write fails immediately rather than hanging this.
        let write = writer.await.unwrap_or(Ok(()));

        let outcome = ExecOutcome {
            code: status.and_then(|status| status.code()),
            stdout: read?,
            stderr: drain_tail(tail, deadline).await.text,
        };
        if outcome.ok() {
            write?;
        }
        Ok(outcome)
    }

    /// Speak one no-parameter op to whatever session
    /// [`exec_chain_command`] reaches, over the child's own stdio.
    ///
    /// The same trick [`crate::ssh::verify_ssh_target`] uses: there is no
    /// socket to dial, because the remote bridge *is* the pipe. Same
    /// frames, same envelope, no transport.
    ///
    /// `budget` is already scaled — callers hold their own deadlines
    /// here ([`Self::await_gone_within`] hands each poll what is left of
    /// one), and scaling twice would silently double them.
    async fn bridge_call(&self, op: &str, budget: Duration) -> Result<BridgeAnswer, String> {
        let JobChild {
            mut child,
            tail,
            mut sink,
            stdout,
        } = self.spawn_exec(&exec_chain_command(self.options.jail_fs_root))?;

        let deadline = Instant::now() + budget;
        let exchange =
            tokio::time::timeout_at(deadline, crate::ssh::call_over(&mut sink, stdout, op)).await;
        // The far side is done being asked; letting it see EOF is what
        // lets it exit on its own rather than be killed.
        drop(sink);

        let answered = match exchange {
            Ok(inner) => inner,
            Err(_elapsed) => Err(anyhow!(
                "{op} did not answer within {}s",
                budget.as_secs().max(1)
            )),
        };
        match answered {
            Ok(response) => {
                reap_by(&mut child, deadline).await;
                Ok(if response.ok {
                    BridgeAnswer::Ok(response.result.unwrap_or(serde_json::Value::Null))
                } else {
                    BridgeAnswer::Refused {
                        code: response
                            .error
                            .as_ref()
                            .map(|error| error.code.clone())
                            .unwrap_or_default(),
                        detail: crate::ssh::render_response_error(&response),
                    }
                })
            }
            Err(error) => {
                reap_by(&mut child, Instant::now()).await;
                let tail = drain_tail(tail, deadline).await.text;
                let code = child
                    .try_wait()
                    .ok()
                    .flatten()
                    .and_then(|status| status.code());
                if matches!(classify_ssh_failure(code, &tail), SshFailure::NoSession) {
                    return Ok(BridgeAnswer::NoSession);
                }
                Err(match last_line(&tail) {
                    Some(line) => format!("{error:#} ({line})"),
                    None => format!("{error:#}"),
                })
            }
        }
    }

    // --------------------------------------------------------------
    // The source ladder
    // --------------------------------------------------------------

    /// [`resolve_source`], with this job's own options and a scratch
    /// directory that goes away when the job does.
    ///
    /// A **fresh** subdirectory per resolution. One shared `download/`
    /// let a second resolution — a retry, a concurrent one — write over
    /// an asset the first had already hashed and was about to stream,
    /// which is a checksum gate that checks one file and sends another.
    pub async fn resolve_source(&self, arch: RemoteArch) -> Result<ResolvedSource, BootstrapError> {
        let serial = self.downloads.fetch_add(1, Ordering::Relaxed);
        resolve_source(
            &self.options,
            arch,
            &self.dir.join(format!("download-{serial}")),
        )
        .await
    }
}

/// The teardown a job that was never closed still owes.
///
/// Blocking on purpose, for [`crate::ssh::SshTunnel`]'s reason: `Drop`
/// has no runtime to await on and may well be running as one shuts down,
/// and the alternative — spawning the exit — is a task that reliably
/// never runs. The wait is bounded, and what it waits for is a local
/// round trip to a control socket.
impl Drop for BootstrapJob {
    fn drop(&mut self) {
        // `CLOSING` falls through on purpose: a `close()` that was
        // cancelled mid-teardown left the work half done, and this is
        // the only thing that will ever finish it.
        if *self.state.get_mut() == CLOSED {
            return;
        }
        crate::ssh::blocking_exit_master(
            &self.ssh_bin,
            &self.config_path,
            &self.ctl_path,
            &self.target,
        );
        let _ = std::fs::remove_dir_all(&self.dir);
        *self.state.get_mut() = CLOSED;
    }
}

/// Write an [`ExecStdin`] into a child's stdin, on a task of its own.
///
/// A task rather than an inline write because the two directions have to
/// run at once: a 10 MiB binary going up a pipe the far side has not
/// drained yet would otherwise block this side before it ever read a
/// byte of the answer.
fn spawn_writer(
    mut sink: tokio::process::ChildStdin,
    stdin: ExecStdin<'_>,
) -> JoinHandle<Result<(), String>> {
    enum Owned {
        Empty,
        Bytes(Vec<u8>),
        File(PathBuf),
        /// A `dup` of the descriptor whose bytes were hashed, rewound.
        /// Streaming this rather than re-opening the path is what makes
        /// "the bytes that were verified" and "the bytes that were sent"
        /// the same claim rather than two.
        Handle(PathBuf, std::fs::File),
    }
    let owned = match stdin {
        ExecStdin::Empty => Owned::Empty,
        ExecStdin::Script(script) => Owned::Bytes(script.as_bytes().to_vec()),
        ExecStdin::Source(source) => match source.reopen_verified() {
            Some(Ok(file)) => Owned::Handle(source.path.clone(), file),
            // A `dup` that fails says nothing about the path, so the
            // by-path stream is the honest fallback rather than a
            // failure of its own.
            Some(Err(_)) | None => Owned::File(source.path.clone()),
        },
    };
    tokio::spawn(async move {
        let result = match owned {
            // Dropped, not written to: the far side's `sh -s` reads its
            // program from stdin, so an immediate EOF is what tells a
            // command with no input that there is none coming.
            Owned::Empty => Ok(()),
            Owned::Bytes(bytes) => sink
                .write_all(&bytes)
                .await
                .map_err(|error| format!("writing to it failed: {error}")),
            Owned::File(path) => {
                async {
                    let mut file = tokio::fs::File::open(&path)
                        .await
                        .map_err(|error| format!("opening {}: {error}", path.display()))?;
                    tokio::io::copy(&mut file, &mut sink)
                        .await
                        .map(|_| ())
                        .map_err(|error| format!("sending {}: {error}", path.display()))
                }
                .await
            }
            Owned::Handle(path, file) => {
                let mut file = tokio::fs::File::from_std(file);
                tokio::io::copy(&mut file, &mut sink)
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("sending {}: {error}", path.display()))
            }
        };
        let _ = sink.shutdown().await;
        drop(sink);
        result
    })
}

/// Read a child's output to EOF, holding at most `cap` bytes.
///
/// Draining continues past the cap rather than stopping: a reader that
/// walks away from a pipe blocks the writer, and the writer here is the
/// process this side is about to wait on.
async fn read_capped<R: AsyncRead + Unpin>(mut reader: R, cap: usize) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 8 * 1024];
    let mut overflowed = false;
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(read) => {
                if !overflowed {
                    if out.len() + read > cap {
                        overflowed = true;
                        out = Vec::new();
                    } else {
                        out.extend_from_slice(&buf[..read]);
                    }
                }
            }
            Err(error) => return Err(format!("reading its output failed: {error}")),
        }
    }
    if overflowed {
        return Err(format!("it printed more than {cap} bytes"));
    }
    Ok(out)
}

// ============================================================================
// Which bytes to install
// ============================================================================

/// The architecture this client's own `roost-session` would be built
/// for, or `None` when this client is not on a platform roost publishes
/// a session for at all.
///
/// Compile-time facts, not environment: what it answers is "could the
/// binary sitting next to me run over there", and only the target triple
/// this was built for can say.
pub fn client_arch() -> Option<RemoteArch> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    map_arch(std::env::consts::ARCH).ok()
}

/// Which local file gets streamed, and where it came from — plan 039
/// §3.3's ladder, resolved lazily at confirm time and never in the
/// connect path.
///
/// 1. [`BootstrapOptions::install_bin`] — an explicit local file.
/// 2. The **sibling**: this client's own `roost-session`, but only when
///    this client is a Linux build of the remote's architecture, and only
///    when running it locally produces the exact expected triple. A stale
///    `target/debug/roost-session` fails that and falls through — it is a
///    real binary, it is simply not this build, and shipping it would
///    install a session the client then refuses to attach to.
/// 3. The **release asset**, downloaded and checksum-verified.
/// 4. Nothing applies — [`BootstrapError::NoSource`], host untouched.
///
/// [`BootstrapOptions::source`] forces one rung. Forcing
/// [`InstallSource::Asset`] skips 1 and 2 *entirely* rather than merely
/// preferring 3: on a Linux runner the sibling always wins, so without
/// that the download path would never once be exercised by CI.
///
/// **Only rung 3 is sha256-verified here, and that is the design.**
/// There is no published checksum for a file on this machine, so there
/// is nothing rungs 1 and 2 could be checked against locally. What gates
/// them is the sibling rung's own local `identify` plus — for every rung
/// alike — the remote staged verify before the commit and the
/// post-commit re-verify after it, which are the two checks that decide
/// what actually runs over there.
pub async fn resolve_source(
    options: &BootstrapOptions,
    arch: RemoteArch,
    scratch: &Path,
) -> Result<ResolvedSource, BootstrapError> {
    let forced = options.source;
    let wanted = |rung: InstallSource| forced.is_none() || forced == Some(rung);

    if wanted(InstallSource::Env) {
        match &options.install_bin {
            Some(path) => return resolve_override(path),
            None if forced == Some(InstallSource::Env) => {
                return Err(BootstrapError::Source(format!(
                    "{INSTALL_BIN_ENV} was forced as the install source but is not set"
                )))
            }
            None => {}
        }
    }

    if wanted(InstallSource::Sibling) {
        match resolve_sibling(options, arch).await {
            Ok(source) => return Ok(source),
            Err(detail) if forced == Some(InstallSource::Sibling) => {
                return Err(BootstrapError::Source(detail))
            }
            Err(detail) => {
                tracing::debug!(
                    detail,
                    "bootstrap: this Roost's own roost-session can't be used"
                );
            }
        }
    }

    if wanted(InstallSource::Asset) {
        let plan = options.asset_plan(arch)?;
        return download_asset(options, &plan, scratch).await;
    }

    Err(BootstrapError::NoSource {
        app_version: options.expected.app_version.clone(),
    })
}

/// Rung 1: the file [`INSTALL_BIN_ENV`] names, exactly as it is.
///
/// A hard error rather than a fall-through, for
/// [`crate::session_launch::locate_session_binary`]'s reason: an
/// explicit override that cannot be used means the user asked for a
/// specific binary, and quietly installing a different one is worse than
/// failing. What it is *not* checked for is being the right build —
/// that is the staged verify's job, on the far side, where it is the
/// same check for every source.
fn resolve_override(path: &Path) -> Result<ResolvedSource, BootstrapError> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        BootstrapError::Source(format!(
            "{INSTALL_BIN_ENV}={} could not be read: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(BootstrapError::Source(format!(
            "{INSTALL_BIN_ENV}={} is not a regular file",
            path.display()
        )));
    }
    Ok(ResolvedSource {
        path: path.to_path_buf(),
        origin: SourceOrigin::Override,
        verified: None,
    })
}

/// Rung 2: this client's own `roost-session`, if it could run over there
/// and if it says it is this build.
async fn resolve_sibling(
    options: &BootstrapOptions,
    arch: RemoteArch,
) -> Result<ResolvedSource, String> {
    let Some(local_arch) = client_arch() else {
        return Err(
            "this Roost is not a Linux build, so its own roost-session can't run on a Linux host"
                .to_string(),
        );
    };
    if local_arch != arch {
        return Err(format!(
            "this Roost is a {local_arch} build and that host is {arch}"
        ));
    }
    let Some(path) = options.sibling_bin.as_deref() else {
        return Err("this Roost has no roost-session next to it".to_string());
    };
    let found = local_identity(path).await?;
    if !identity_matches(&options.expected, &found) {
        return Err(format!(
            "{} reports {} / {} rather than this Roost's {} / {}",
            path.display(),
            found.app_version,
            found.libghostty_build,
            options.expected.app_version,
            options.expected.libghostty_build
        ));
    }
    Ok(ResolvedSource {
        path: path.to_path_buf(),
        origin: SourceOrigin::Sibling,
        verified: None,
    })
}

/// Run `<path> identify` here, bounded, and read the one line back.
async fn local_identity(path: &Path) -> Result<SessionBinaryIdentity, String> {
    let mut child = tokio::process::Command::new(path)
        .arg("identify")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("running {} identify: {error}", path.display()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("no stdout pipe on {}", path.display()))?;

    let deadline = Instant::now() + scaled(LOCAL_IDENTIFY_BUDGET);
    let read = tokio::time::timeout_at(deadline, read_capped(stdout, IDENTITY_STDOUT_CAP)).await;
    reap_by(&mut child, deadline).await;
    let bytes = read.map_err(|_| format!("{} identify timed out", path.display()))??;

    parse_identity_line(&String::from_utf8_lossy(&bytes))
        .ok_or_else(|| format!("{} would not identify itself", path.display()))
}

/// Rung 3: fetch the release asset and its published checksum, verify
/// the bytes here, and only then hand them over as a source.
///
/// Both files land in a private 0700 directory of their own, and every
/// failure removes it — a half-downloaded asset that outlived its job
/// would be a file nothing owns and nothing checks.
///
/// The order is the point (plan 039 §3.6): **nothing is streamed to a
/// host before the hash matches**. The size is re-checked here rather
/// than trusted to `curl --max-filesize`, which can only enforce a limit
/// it was told in advance — a response with no `Content-Length` is
/// exactly the shape that slips past it.
async fn download_asset(
    options: &BootstrapOptions,
    plan: &AssetPlan,
    scratch: &Path,
) -> Result<ResolvedSource, BootstrapError> {
    match download_into(options, plan, scratch).await {
        Ok((path, verified)) => Ok(ResolvedSource {
            path,
            origin: SourceOrigin::Asset {
                base: plan.base.clone(),
                overridden: plan.overridden,
            },
            verified: Some(verified),
        }),
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(scratch).await;
            Err(error)
        }
    }
}

async fn download_into(
    options: &BootstrapOptions,
    plan: &AssetPlan,
    scratch: &Path,
) -> Result<(PathBuf, std::fs::File), BootstrapError> {
    // A hard error on `AlreadyExists` too: the caller's contract is a
    // *fresh* directory, and a pre-existing one is either another
    // resolution's or something planted, neither of which this may then
    // write an asset into.
    crate::ssh::create_private_dir(scratch).map_err(|error| {
        BootstrapError::Download(format!("creating {}: {error}", scratch.display()))
    })?;
    let asset = scratch.join(&plan.asset_name);
    let checksum = scratch.join(checksum_name(&plan.asset_name));

    tracing::info!(url = %redact_url(&plan.asset_url), "bootstrap: downloading roost-session");
    curl(options, &plan.asset_url, &asset, ASSET_MAX_BYTES).await?;
    curl(options, &plan.checksum_url, &checksum, CHECKSUM_MAX_BYTES).await?;

    let size = tokio::fs::metadata(&asset)
        .await
        .map_err(|error| {
            BootstrapError::Download(format!("{} could not be read: {error}", asset.display()))
        })?
        .len();
    if size == 0 {
        return Err(BootstrapError::Download(format!(
            "{} was served as an empty file",
            redact_url(&plan.asset_url)
        )));
    }
    // Belt and braces behind `curl`'s own `--max-filesize` and the
    // counting copy in [`curl`]: three bounds, none of which is the
    // only one.
    if size > ASSET_MAX_BYTES {
        return Err(BootstrapError::Download(format!(
            "{} is {size} bytes, past the {ASSET_MAX_BYTES}-byte limit",
            redact_url(&plan.asset_url)
        )));
    }

    let published = tokio::fs::read_to_string(&checksum)
        .await
        .map_err(|error| {
            BootstrapError::Checksum(format!("its .sha256 could not be read: {error}"))
        })?;
    let expected = parse_checksum_file(&published, &plan.asset_name)?;

    // Opened once, hashed through that handle, and handed back still
    // open — so what the checksum covers and what the install streams
    // are provably one file, not two lookups of one name.
    let opened = std::fs::File::open(&asset).map_err(|error| {
        BootstrapError::Checksum(format!("{} could not be read: {error}", asset.display()))
    })?;
    let path = asset.clone();
    let (actual, verified) = tokio::task::spawn_blocking(move || hash_open_file(opened))
        .await
        .map_err(|error| BootstrapError::Checksum(format!("hashing was interrupted: {error}")))?
        .map_err(|error| {
            BootstrapError::Checksum(format!("{} could not be read: {error}", path.display()))
        })?;
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(BootstrapError::Checksum(format!(
            "the download hashes to {actual} and the published checksum is {expected}"
        )));
    }
    Ok((asset, verified))
}

/// One `curl` into a file, bounded by size and by time on both sides.
///
/// The system `curl` rather than an HTTP crate: it is on every macOS and
/// every Linux this targets, it already knows this machine's proxy and
/// CA configuration, and the alternative is a TLS stack in a crate that
/// exists to write JSON to a Unix socket. No `Authorization` header is
/// ever set — the repo is public, so there is nothing to send and
/// nothing to leak.
async fn curl(
    options: &BootstrapOptions,
    url: &str,
    out: &Path,
    max_bytes: u64,
) -> Result<(), BootstrapError> {
    let mut child = tokio::process::Command::new(&options.curl_bin)
        .args(curl_argv(url, max_bytes))
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            BootstrapError::Download(if error.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "curl is required to download roost-session, and {} isn't there",
                    options.curl_bin.display()
                )
            } else {
                format!("running {}: {error}", options.curl_bin.display())
            })
        })?;
    let tail = crate::ssh::spawn_stderr_tail(&mut child);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| BootstrapError::Download("no stdout pipe on curl".to_string()))?;

    let budget = scaled(Duration::from_secs(CURL_MAX_TIME_SECS));
    let deadline = Instant::now() + budget;
    let timed_out = || {
        BootstrapError::Download(format!(
            "{} did not answer within {}s",
            redact_url(url),
            budget.as_secs().max(1)
        ))
    };

    let mut file = tokio::fs::File::create(out)
        .await
        .map_err(|error| BootstrapError::Download(format!("writing {}: {error}", out.display())))?;
    match tokio::time::timeout_at(deadline, copy_capped(stdout, &mut file, max_bytes)).await {
        Ok(Ok(_written)) => {}
        Ok(Err(detail)) => {
            reap_by(&mut child, Instant::now()).await;
            return Err(BootstrapError::Download(format!(
                "{} — {detail}",
                redact_url(url)
            )));
        }
        Err(_elapsed) => {
            reap_by(&mut child, Instant::now()).await;
            return Err(timed_out());
        }
    }
    if let Err(error) = file.flush().await {
        return Err(BootstrapError::Download(format!(
            "writing {}: {error}",
            out.display()
        )));
    }

    let status = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            return Err(BootstrapError::Download(format!(
                "waiting for curl failed: {error}"
            )))
        }
        Err(_elapsed) => {
            reap_by(&mut child, Instant::now()).await;
            return Err(timed_out());
        }
    };
    if status.success() {
        return Ok(());
    }
    let detail = last_line(&drain_tail(tail, deadline).await.text)
        .unwrap_or_else(|| format!("curl exited {}", status.code().unwrap_or(-1)));
    Err(BootstrapError::Download(format!(
        "{} — {detail}",
        redact_url(url)
    )))
}

/// One `curl` invocation's argv, minus the binary.
///
/// Three things here are load-bearing:
///
/// * **`--proto` / `--proto-redir`.** `-L` follows redirects, and a
///   redirect can change scheme — so an `https://` base that
///   [`check_asset_base`] passed could still be bounced to plaintext by
///   the server, which is the whole rule undone by one `302`. The
///   plaintext relaxation is confined to a URL that was *already*
///   plaintext, which [`check_asset_base`] only permits for loopback.
/// * **No `-o`.** The body comes back on stdout so this side can count
///   it while it is written; see [`copy_capped`].
/// * **`--max-filesize` stays** anyway: it refuses an oversized
///   `Content-Length` before a byte of body is transferred, which is
///   cheaper than reading one and cutting it off.
fn curl_argv(url: &str, max_bytes: u64) -> Vec<String> {
    // `check_asset_base` has already refused plain http off loopback,
    // so "the URL is http" and "the URL is loopback" are the same fact
    // by the time this runs.
    let protocols = if url.to_ascii_lowercase().starts_with("http://") {
        "=https,http"
    } else {
        "=https"
    };
    vec![
        "-fsSL".to_string(),
        "--proto".to_string(),
        protocols.to_string(),
        "--proto-redir".to_string(),
        protocols.to_string(),
        "--max-time".to_string(),
        CURL_MAX_TIME_SECS.to_string(),
        "--max-filesize".to_string(),
        max_bytes.to_string(),
        url.to_string(),
    ]
}

/// Copy `reader` into `writer`, refusing past `cap` bytes.
///
/// The cap is enforced **while writing**, not after: `--max-filesize`
/// can only bound a length the server declared in advance, so a
/// chunked — or merely `Content-Length`-less — response slips straight
/// past it and is bounded by nothing but `--max-time`. That is a disk,
/// not a download.
async fn copy_capped<R: AsyncRead + Unpin>(
    mut reader: R,
    writer: &mut tokio::fs::File,
    cap: u64,
) -> Result<u64, String> {
    let mut buf = vec![0u8; HASH_CHUNK];
    let mut written: u64 = 0;
    loop {
        let read = reader
            .read(&mut buf)
            .await
            .map_err(|error| format!("reading it failed: {error}"))?;
        if read == 0 {
            return Ok(written);
        }
        written += read as u64;
        if written > cap {
            return Err(format!("it is larger than the {cap}-byte limit"));
        }
        writer
            .write_all(&buf[..read])
            .await
            .map_err(|error| format!("writing it failed: {error}"))?;
    }
}

/// A URL with anything private taken out of it, for a log line.
///
/// [`check_asset_base`] already refuses a base carrying userinfo or a
/// query, so in production this only ever has the scheme and host to
/// hand back. It runs anyway, because "the validator upstream would have
/// caught it" is the reasoning that puts credentials in log files.
pub fn redact_url(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some(split) => split,
        None => return "<not a url>".to_string(),
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let authority_and_path = match rest.split_once('@') {
        Some((_userinfo, tail)) => format!("<redacted>@{tail}"),
        None => rest.to_string(),
    };
    format!("{scheme}://{authority_and_path}")
}

/// SHA-256 of an **open** file, streamed, handing the handle back
/// rewound.
///
/// By handle and not by path, so the caller can stream the very
/// descriptor these bytes were read through: hashing `path` and then
/// re-opening `path` is two lookups of a name, and anything with write
/// access to that directory gets to make them resolve to different
/// files.
///
/// Streamed rather than read whole because the cap this runs under is
/// 256 MiB, and a hash that has to hold its input in memory turns a
/// download limit into a memory limit. Synchronous because it is called
/// from `spawn_blocking` — a 256 MiB read is not something to do on a
/// runtime thread.
fn hash_open_file(mut file: std::fs::File) -> std::io::Result<(String, std::fs::File)> {
    use std::fmt::Write as _;
    use std::io::{Read as _, Seek, SeekFrom};

    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    file.seek(SeekFrom::Start(0))?;

    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok((hex, file))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(app_version: &str, build: &str) -> SessionBinaryIdentity {
        SessionBinaryIdentity {
            app_version: app_version.to_string(),
            session_protocol: 2,
            libghostty_build: build.to_string(),
        }
    }

    fn options() -> BootstrapOptions {
        BootstrapOptions {
            expected: identity("0.0.19", "ghostty-abc+snapshot.v1"),
            asset_base: None,
            install_bin: None,
            sibling_bin: None,
            curl_bin: PathBuf::from("curl"),
            source: None,
            jail_fs_root: false,
        }
    }

    // ------------------------------------------------------------------
    // shell_quote
    // ------------------------------------------------------------------

    #[test]
    fn safe_words_are_emitted_bare() {
        for word in [
            "roost-session",
            "/usr/bin/roost-session",
            "a.b_c-d",
            "TERM=xterm",
            "50%",
            "a,b:c@d",
        ] {
            assert_eq!(shell_quote(word), word, "{word:?} needs no quoting");
        }
    }

    /// The case the rule exists for: a home directory with an
    /// apostrophe in it. `'` closes the quote, `\'` is the literal, and
    /// `'` reopens — a single `\'` on its own would end the word.
    #[test]
    fn an_apostrophe_is_escaped_by_closing_and_reopening_the_quote() {
        assert_eq!(
            shell_quote("/home/o'brien/.local/bin/roost-session"),
            r#"'/home/o'\''brien/.local/bin/roost-session'"#
        );
        assert_eq!(shell_quote("'"), r#"''\'''"#);
        assert_eq!(shell_quote("''"), r#"''\'''\'''"#);
    }

    /// A bare nothing is not an argument — an empty word has to be
    /// quoted or the command loses a parameter.
    #[test]
    fn the_empty_word_is_quoted() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn spaces_newlines_and_metacharacters_are_quoted_verbatim() {
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("line\nbreak"), "'line\nbreak'");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_quote("a;b|c&d"), "'a;b|c&d'");
        assert_eq!(shell_quote("back\\slash"), "'back\\slash'");
        assert_eq!(shell_quote("~/roost"), "'~/roost'");
    }

    /// `-` is in the bare-safe set, so a leading-dash word used to pass
    /// through naked. Quoting it does not stop `tee`/`chmod`/`mv`/`rm`
    /// reading it as an option — that is what the `--` terminators are
    /// for — but it keeps such a word visibly a value, and the parsers
    /// refuse to hand one over in the first place.
    #[test]
    fn a_leading_dash_is_always_quoted() {
        assert_eq!(shell_quote("-t"), "'-t'");
        assert_eq!(
            shell_quote("--target-directory=/x"),
            "'--target-directory=/x'"
        );
        assert_eq!(shell_quote("-"), "'-'");
        // Only *leading*: an interior dash is still ordinary.
        assert_eq!(shell_quote("roost-session"), "roost-session");
    }

    // ------------------------------------------------------------------
    // the candidate ladder
    // ------------------------------------------------------------------

    #[test]
    fn the_ladder_is_the_pinned_order() {
        let names: Vec<&str> = CANDIDATES.iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            [
                "home-local-bin",
                "path",
                "usr-bin",
                "linuxbrew",
                "nix-profile",
                "nix-per-user",
                "nix-default-profile",
                "nixos-system",
            ]
        );
    }

    /// Read a generated script back and report which ladder rungs it
    /// mentions, in the order they appear.
    fn enumerated_rungs(script: &str, jailed: bool) -> Vec<&'static str> {
        let mut found: Vec<(usize, &'static str)> = CANDIDATES
            .iter()
            .filter_map(|candidate| {
                let marker = candidate.marker(jailed);
                let at = script.find(&marker)?;
                assert_eq!(
                    script.matches(&marker).count(),
                    1,
                    "{} appears more than once",
                    candidate.name
                );
                Some((at, candidate.name))
            })
            .collect();
        found.sort();
        found.into_iter().map(|(_, name)| name).collect()
    }

    /// **The fence.** Both artifacts are generated from [`CANDIDATES`],
    /// and this parses them back to prove it: anything the probe can
    /// find, the transport can exec. The two drifting apart is the
    /// dialog loop plan 039 §3.2 exists to close — an install offer for
    /// a binary that is already installed, forever.
    ///
    /// Enumeration alone is not the fence. Two ladders can name every
    /// rung and still *test* them differently — a probe that requires
    /// `[ -f ] && [ -x ]` beside an exec chain that takes `[ -x ]`
    /// alone, or a `command -v` rung the probe gates on an absolute path
    /// and the chain execs bare. The second half of this test compares
    /// the guards themselves, byte for byte, and it is the half that
    /// catches that.
    ///
    /// Run in **both** modes. The jail prefix is now a parameter rather
    /// than a fact of the ladder, and a parameter that reached only one
    /// of the two generators would put the probe and the transport back
    /// on different rungs — which is precisely the drift this fence
    /// exists for.
    #[test]
    fn the_probe_and_the_exec_chain_enumerate_the_identical_ladder() {
        let ladder: Vec<&str> = CANDIDATES.iter().map(|c| c.name).collect();
        for jailed in [false, true] {
            let discovery = discovery_script(jailed);
            let exec_chain = exec_chain_command(jailed);
            assert_eq!(
                enumerated_rungs(&discovery, jailed),
                ladder,
                "the probe skips a rung (jailed={jailed})"
            );
            assert_eq!(
                enumerated_rungs(&exec_chain, jailed),
                ladder,
                "the exec chain skips a rung (jailed={jailed})"
            );

            for candidate in CANDIDATES {
                let guard = candidate.guard(jailed);
                assert!(
                    guard.contains("[ -f \"$p\" ] && [ -x \"$p\" ] &&"),
                    "{}: a rung must be gated on being a *file* that is executable — \
                     `[ -x ]` alone is true for a directory: {guard}",
                    candidate.name
                );
                for (which, script) in [("probe", &discovery), ("exec chain", &exec_chain)] {
                    assert_eq!(
                        script.matches(&guard).count(),
                        1,
                        "{which} carries a different guard for {} than the other ladder does \
                         (jailed={jailed}):\nwanted {guard:?}\nin {script}",
                        candidate.name
                    );
                }
            }
        }
    }

    #[test]
    fn the_probe_guards_unset_variables_and_never_sets_eu() {
        let script = discovery_script(false);
        assert!(!script.contains("set -e"), "{script}");
        assert!(!script.contains("set -u"), "{script}");
        // Every rung that interpolates a variable is guarded on it.
        assert_eq!(
            script.matches("[ -n \"${HOME:-}\" ]").count(),
            2,
            "{script}"
        );
        assert_eq!(
            script.matches("[ -n \"${USER:-}\" ]").count(),
            1,
            "{script}"
        );
        assert!(script.ends_with("exit 0\n"), "{script}");
    }

    /// `[ -f ] && [ -x ]`, not `[ -e ]` and not `[ -x ]` alone: a
    /// dangling symlink exists and cannot be run, and an executable
    /// *directory* passes `[ -x ]` — both are traps for the identity
    /// exec behind the candidate.
    #[test]
    fn the_probe_filters_candidates_on_being_an_executable_file() {
        for script in [discovery_script(false), exec_chain_command(false)] {
            assert_eq!(
                script.matches("[ -f \"$p\" ] && [ -x \"$p\" ]").count(),
                CANDIDATES.len(),
                "{script}"
            );
            // No bare `[ -x ]` anywhere: every one is paired with its
            // `[ -f ]`.
            assert_eq!(
                script.matches("[ -x \"$p\" ]").count(),
                CANDIDATES.len(),
                "{script}"
            );
            assert!(!script.contains("[ -e "), "{script}");
        }
    }

    /// `command -v` answers with a bare word for a builtin, a function
    /// or an alias. Both ladders gate it on an absolute path that is an
    /// executable file — the exec chain especially, since a bare
    /// `command -v` success there would `exec` whatever the word named.
    #[test]
    fn the_path_lookup_rung_is_gated_the_same_way_in_both_ladders() {
        for script in [discovery_script(false), exec_chain_command(false)] {
            assert_eq!(
                script
                    .matches("case \"$p\" in /*) [ -f \"$p\" ] && [ -x \"$p\" ] &&")
                    .count(),
                1,
                "{script}"
            );
            assert!(
                !script.contains("command -v roost-session >/dev/null"),
                "an ungated `command -v` rung: {script}"
            );
        }
    }

    #[test]
    fn the_probe_reports_both_uname_fields_first() {
        let script = discovery_script(false);
        let os = script.find("uname -s").expect("uname -s");
        let arch = script.find("uname -m").expect("uname -m");
        let first_rung = script.find("$HOME/.local/bin").expect("first rung");
        assert!(os < arch && arch < first_rung, "{script}");
    }

    /// The five rungs the hermetic fixture needs to redirect (plan 039
    /// §3.8); `$HOME`-relative ones are already jailed by the fake
    /// `$HOME`, and prefixing them would double up.
    const ABSOLUTE_RUNGS: &[&str] = &[
        "/usr/bin/roost-session",
        "/home/linuxbrew/.linuxbrew/bin/roost-session",
        "/etc/profiles/per-user/$USER/bin/roost-session",
        "/nix/var/nix/profiles/default/bin/roost-session",
        "/run/current-system/sw/bin/roost-session",
    ];

    /// In **test mode** the absolute rungs carry the jail prefix, so the
    /// fixture's `/usr/bin` probe lands in a tempdir.
    #[test]
    fn a_jailed_ladder_prefixes_its_absolute_rungs_and_only_those() {
        // Spelled from the constant, so renaming the env var cannot
        // leave the scripts and this assertion agreeing with each other
        // and with nothing else.
        let fs_root_prefix = format!("${{{FS_ROOT_ENV}:-}}");

        for script in [discovery_script(true), exec_chain_command(true)] {
            for absolute in ABSOLUTE_RUNGS {
                assert!(
                    script.contains(&format!("{fs_root_prefix}{absolute}")),
                    "{absolute} is not prefixed in {script}"
                );
            }
            assert!(
                !script.contains(&format!("{fs_root_prefix}$HOME")),
                "a $HOME rung must not be prefixed: {script}"
            );
        }
    }

    /// **The shipped ladders name absolute paths and nothing else.**
    ///
    /// The prefix is an expansion evaluated on the *far* side, and a
    /// remote can set that variable — `~/.ssh/environment` under
    /// `PermitUserEnvironment`, a PAM env module, a login rc. A shipped
    /// script carrying it would let the host choose which binary the
    /// transport execs, which binary the probe identifies, and which
    /// file the install compares itself against.
    #[test]
    fn a_shipped_ladder_carries_no_filesystem_root_seam_at_all() {
        for script in [discovery_script(false), exec_chain_command(false)] {
            assert!(
                !script.contains(FS_ROOT_ENV),
                "a shipped script must never expand {FS_ROOT_ENV}: {script}"
            );
            for absolute in ABSOLUTE_RUNGS {
                assert!(
                    script.contains(&format!("p=\"{absolute}\"")),
                    "{absolute} must be named bare in {script}"
                );
            }
        }
        // The transport's shipped command is one of those two, and it is
        // spelled as a *value* — `remote_command_for(false)` — rather
        // than read back through `remote_command()`'s ambient gate,
        // because a `cargo test` run may itself inherit
        // `ROOST_TEST_MODE=1` and that would make this vacuous.
        assert_eq!(
            crate::ssh::remote_command_for(false),
            exec_chain_command(false)
        );
        assert!(!crate::ssh::remote_command_for(false).contains(FS_ROOT_ENV));
    }

    /// **The probe and the transport must never disagree about the
    /// jail.**
    ///
    /// They did: `ssh::remote_command` hardcoded `false` while
    /// `BootstrapOptions::from_env` gated the same flag on
    /// `ROOST_TEST_MODE`, so a jailed lane's probe answered
    /// `Compatible { path: <jail>/usr/bin/roost-session }` and the
    /// connect behind it — and the post-install reconnect, which goes
    /// through the same command — looked at a bare
    /// `/usr/bin/roost-session` and failed `NotFound`. Only rungs whose
    /// winning path is absolute could show it; the `$HOME`-relative ones
    /// are jailed by the fake `$HOME` and agreed by accident.
    ///
    /// **Pinned as a value, not as a gate.** Reading the flag off this
    /// process's ambient `ROOST_TEST_MODE` is exactly how the old
    /// version of this test could not fail: CI runs `cargo test` with no
    /// such variable, so `false` was the only answer exercised and the
    /// buggy `exec_chain_command(false)` satisfied it. Here the pairing
    /// is asserted for **both** answers a
    /// [`BootstrapOptions::jail_fs_root`] can hold — including the one a
    /// hardcoded `false` gets wrong — and the two ladders are asserted
    /// genuinely different, so nothing that ignores the flag survives.
    /// This is also the fence on the other half of that bug: anything
    /// that builds a `BootstrapOptions` by hand (the `jail_fs_root:
    /// true` harness in `tests/bootstrap_test.rs`) pairs with a
    /// `SshTunnelOptions` carrying the same flag, and both are checked
    /// here rather than trusted to `from_env`.
    #[test]
    fn the_transport_and_the_probe_resolve_the_same_rung_in_both_modes() {
        for jailed in [false, true] {
            let probe_options = BootstrapOptions {
                jail_fs_root: jailed,
                ..BootstrapOptions::from_env(identity("0.0.19", "ghostty-abc+snapshot.v1"))
            };
            let tunnel_options = crate::ssh::SshTunnelOptions {
                jail_fs_root: jailed,
                ..crate::ssh::SshTunnelOptions::from_env()
            };
            assert_eq!(
                probe_options.jail_fs_root, tunnel_options.jail_fs_root,
                "the job and the tunnel must be handed one flag"
            );
            assert_eq!(
                crate::ssh::remote_command_for(tunnel_options.jail_fs_root),
                exec_chain_command(probe_options.jail_fs_root),
                "the transport execs a different ladder than the probe searched \
                 (jailed={jailed})"
            );
        }
        // The anti-vacuity clause: the two ladders really do differ, so
        // the loop above cannot be satisfied by ignoring the flag.
        assert_ne!(
            crate::ssh::remote_command_for(false),
            crate::ssh::remote_command_for(true)
        );
        assert!(crate::ssh::remote_command_for(true).contains(FS_ROOT_ENV));
        assert!(!crate::ssh::remote_command_for(false).contains(FS_ROOT_ENV));
        // And the environment edge is still the gate it claims to be.
        assert_eq!(
            BootstrapOptions::from_env(identity("0.0.19", "ghostty-abc+snapshot.v1")).jail_fs_root,
            test_mode_env(),
        );
        assert_eq!(
            crate::ssh::SshTunnelOptions::from_env().jail_fs_root,
            test_mode_env(),
        );

        for jailed in [false, true] {
            let discovery = discovery_script(jailed);
            let exec_chain = exec_chain_command(jailed);
            for candidate in CANDIDATES {
                let guard = candidate.guard(jailed);
                assert!(
                    discovery.contains(&guard),
                    "probe (jailed={jailed}): {guard}"
                );
                assert!(
                    exec_chain.contains(&guard),
                    "transport (jailed={jailed}): {guard}"
                );
            }
            for absolute in ABSOLUTE_RUNGS {
                let rung = if jailed {
                    format!("p=\"{FS_ROOT_EXPANSION}{absolute}\"")
                } else {
                    format!("p=\"{absolute}\"")
                };
                assert!(discovery.contains(&rung), "probe: {rung}");
                assert!(exec_chain.contains(&rung), "transport: {rung}");
            }
        }
    }

    /// sshd hands a remote command to the user's **login** shell, and
    /// `'\''` — a POSIX `sh` idiom — is not an escape in csh/tcsh/fish.
    /// The shipped literal this replaced had no embedded quote, so a
    /// generated chain that grows one is a live regression for hosts
    /// that already worked.
    #[test]
    fn the_exec_chain_carries_no_embedded_single_quote() {
        for jailed in [false, true] {
            let command = exec_chain_command(jailed);
            assert!(
                !command.contains(r"'\''"),
                "the login shell may not be a POSIX sh (jailed={jailed}): {command}"
            );
            // One opening quote and one closing quote, and nothing in
            // between — which is the same claim, counted.
            assert_eq!(command.matches('\'').count(), 2, "{command}");
        }
        assert!(!crate::ssh::remote_command().contains(r"'\''"));
    }

    /// A drop-in for `ssh::remote_command`: one argv element, one
    /// `sh -c`, one `client-bridge`.
    #[test]
    fn the_exec_chain_is_one_sh_c_word_that_execs_the_bridge() {
        let command = exec_chain_command(false);
        assert!(command.starts_with("sh -c '"), "{command}");
        assert!(command.ends_with('\''), "{command}");
        assert!(!command.contains('\n'), "{command}");
        assert_eq!(
            command.matches("client-bridge").count(),
            CANDIDATES.len(),
            "{command}"
        );
    }

    /// Falling off the end has to look like `NotFound` to
    /// `classify_ssh_failure` — that family's copy is what becomes the
    /// install offer. A silent non-zero exit would surface as an
    /// unclassified transport failure instead.
    #[test]
    fn the_exec_chain_falls_through_to_a_classifiable_not_found() {
        let command = exec_chain_command(false);
        assert!(command.contains("command not found"), "{command}");
        assert!(command.contains("exit 127"), "{command}");
        assert_eq!(
            crate::ssh::classify_ssh_failure(Some(127), "roost-session: command not found\n"),
            crate::ssh::SshFailure::NotFound
        );
    }

    /// Wrapped in `sh -c` like the other two remote commands: the login
    /// shell sshd starts is the user's choice, and `command -v` does not
    /// exist in csh.
    #[test]
    fn the_remote_shell_path_probe_is_a_wrapped_command_v() {
        assert_eq!(path_check_command(), "sh -c 'command -v roost-session'");
    }

    // ------------------------------------------------------------------
    // identity_script
    // ------------------------------------------------------------------

    #[test]
    fn the_identity_script_quotes_every_candidate_path() {
        let script = identity_script(&[
            "/home/o'brien/.local/bin/roost-session".to_string(),
            "/usr/bin/roost-session".to_string(),
        ]);
        assert!(
            script.contains(r#"'/home/o'\''brien/.local/bin/roost-session' identify"#),
            "{script}"
        );
        assert!(
            script.contains("/usr/bin/roost-session identify"),
            "{script}"
        );
    }

    /// One round trip, not one per rung — and a candidate that will not
    /// identify still leaves a record, so an old build reads as
    /// "present, unidentifiable" rather than as missing.
    #[test]
    fn the_identity_script_stops_at_the_first_answer_but_records_the_failures() {
        let script = identity_script(&["/a".to_string(), "/b".to_string()]);
        assert_eq!(script.matches("exit 0").count(), 3, "{script}");
        assert_eq!(script.matches("printf '%s\\0%s\\0'").count(), 4, "{script}");
        assert!(script.contains("printf '%s\\0%s\\0' /a ''"), "{script}");
    }

    #[test]
    fn an_empty_candidate_list_yields_a_script_that_answers_nothing() {
        assert_eq!(identity_script(&[]), "exit 0\n");
    }

    // ------------------------------------------------------------------
    // the install scripts
    // ------------------------------------------------------------------

    /// No `umask`: it would cover only this exec's `mkdir -p`, leaving
    /// `~/.local` and `~/.local/bin` at an unconventional 0700 while the
    /// staged file — created by `tee` in a *later* exec under the login
    /// shell's own umask — stayed world-readable. Narrowing the staged
    /// file is [`verify_staged_script`]'s job.
    #[test]
    fn prepare_asserts_home_and_reports_both_paths_without_touching_umask() {
        let script = prepare_script();
        assert!(script.starts_with("set -eu\n"), "{script}");
        assert!(script.contains("[ -n \"${HOME:-}\" ]"), "{script}");
        assert!(
            script.contains("case \"${HOME:-}\" in /*) ;; *)"),
            "{script}"
        );
        assert!(!script.contains("umask"), "{script}");
        assert!(
            script.contains("dest=\"$HOME/.local/bin/roost-session\""),
            "{script}"
        );
        assert!(script.contains("mkdir -p \"${dest%/*}\""), "{script}");
        assert!(script.contains("tmp=\"${dest}.tmp.$$\""), "{script}");
        assert!(
            script.contains("printf '%s\\0%s\\0' \"$tmp\" \"$dest\""),
            "{script}"
        );
    }

    /// Cancelling an install cannot run a remote cleanup — `Drop` has no
    /// runtime for a round trip and a spawned one reliably never runs —
    /// so the next prepare reclaims what the last one left. Only names
    /// this script's own `$$` shape could have produced are swept, which
    /// is what makes them ours rather than someone else's file.
    #[test]
    fn prepare_sweeps_only_its_own_stale_temporaries() {
        let script = prepare_script();
        assert!(script.contains("for stale in \"$dest\".tmp.*"), "{script}");
        assert!(
            script.contains("case \"$suffix\" in ''|*[!0-9]*) continue;; esac"),
            "the sweep must refuse anything but <dest>.tmp.<digits>: {script}"
        );
        assert!(script.contains("rm -f -- \"$stale\""), "{script}");
        // The sweep comes before the reservation, or the reservation
        // would refuse the very file the sweep exists to clear.
        let sweep = script.find("for stale").expect("the sweep");
        let reserve = script.find("set -C").expect("the reservation");
        assert!(sweep < reserve, "{script}");
    }

    /// The staged path is predictable (`<dest>.tmp.<pid>`), so it is
    /// *reserved* rather than merely named: `O_EXCL` through `set -C`,
    /// plus an explicit symlink refusal, or a pre-planted symlink would
    /// have `tee` write the streamed ELF wherever it pointed.
    #[test]
    fn prepare_reserves_the_staged_path_with_noclobber_and_refuses_a_symlink() {
        let script = prepare_script();
        assert!(script.contains("[ ! -L \"$tmp\" ]"), "{script}");
        assert!(script.contains("(set -C; : > \"$tmp\")"), "{script}");
        // A failure to reserve is a refusal, not a shrug.
        assert!(
            script.contains("the staged path already exists"),
            "{script}"
        );
    }

    /// The redirect is the deviation from herdr (plan 039 §4): without
    /// it `tee` writes the whole binary back up the ssh connection. The
    /// `sh -c` wrapper is the amendment to plan 039 §3.4 — this string
    /// is the remote command, so without it the user's login shell
    /// parses it, and `'\''` is not an escape in csh/tcsh/fish.
    #[test]
    fn the_stream_command_is_a_wrapped_tee_to_devnull_with_the_path_as_one_word() {
        assert_eq!(
            stream_command("/home/u/.local/bin/roost-session.tmp.42"),
            "sh -c 'tee -- /home/u/.local/bin/roost-session.tmp.42 > /dev/null'"
        );
        assert_eq!(
            stream_command("/home/o'brien/x.tmp.9"),
            r#"sh -c 'tee -- '\''/home/o'\''\'\'''\''brien/x.tmp.9'\'' > /dev/null'"#
        );
    }

    /// The staged file is created by `tee` under the login shell's
    /// umask — typically 022 — so this is the first exec that can
    /// narrow it. It **stays** narrow: widening back to 0755 here would
    /// reopen the window the 700 just closed, over exactly the stretch
    /// where nothing has yet established what these bytes are.
    /// [`commit_script`] widens it, past the gate.
    #[test]
    fn the_staged_verify_narrows_to_700_and_leaves_it_there() {
        assert_eq!(
            verify_staged_script("/home/u/x.tmp.7"),
            "set -eu\nchmod -- 700 /home/u/x.tmp.7\nexec /home/u/x.tmp.7 identify\n"
        );
        assert!(
            !verify_staged_script("/home/u/x.tmp.7").contains("755"),
            "an unverified file must not be world-executable"
        );
    }

    /// `chmod`'s mode is its first *operand*, and a POSIX utility stops
    /// parsing options there — so `chmod 755 -- path` puts the `--`
    /// where it is a filename, not a terminator, and BSD `chmod` says
    /// so out loud. The terminator has to come first.
    #[test]
    fn chmod_terminates_its_options_before_the_mode_not_after() {
        let scripts = [
            verify_staged_script("/h/x.tmp.7"),
            commit_script("/h/x.tmp.7", "/h/x", "/h/x.bak.7"),
        ];
        for script in scripts {
            assert!(!script.contains("chmod 7"), "{script}");
            assert_eq!(script.matches("chmod -- 7").count(), 1, "{script}");
        }
    }

    /// `[ ! -d ]` is the load-bearing one: POSIX `mv file dir` moves the
    /// file *inside* and exits 0, so without it a `dest` that is a
    /// directory reports a successful install that put nothing in place,
    /// and the next connect execs a directory and exits 126 — which
    /// nothing classifies as `NotFound`, so no new offer ever appears.
    ///
    /// The incumbent moves aside rather than being overwritten: the
    /// post-commit re-verify has not run yet, and a single `mv` leaves
    /// nothing to put back when it fails.
    #[test]
    fn commit_backs_up_the_incumbent_then_renames_the_widened_temporary() {
        assert_eq!(
            commit_script("/h/x.tmp.7", "/h/x", "/h/x.bak.7"),
            "set -eu\n[ -f /h/x.tmp.7 ]\n[ ! -d /h/x ]\nchmod -- 755 /h/x.tmp.7\n\
             if [ -e /h/x ] || [ -L /h/x ]; then mv -- /h/x /h/x.bak.7; fi\nmv -- /h/x.tmp.7 /h/x\n"
        );
        assert_eq!(
            commit_script("/h/o'b.tmp.7", "/h/o'b", "/h/o'b.bak.7"),
            "set -eu\n[ -f '/h/o'\\''b.tmp.7' ]\n[ ! -d '/h/o'\\''b' ]\n\
             chmod -- 755 '/h/o'\\''b.tmp.7'\n\
             if [ -e '/h/o'\\''b' ] || [ -L '/h/o'\\''b' ]; then \
             mv -- '/h/o'\\''b' '/h/o'\\''b.bak.7'; fi\n\
             mv -- '/h/o'\\''b.tmp.7' '/h/o'\\''b'\n"
        );
    }

    /// The rollback says whether it *did* anything, because this side
    /// cannot tell "there was no incumbent" from "the incumbent is
    /// back" by looking at the destination afterwards — and
    /// `BootstrapError::PostCommit`'s copy turns on exactly that.
    #[test]
    fn rollback_restores_only_when_there_is_a_backup_and_reports_that_it_did() {
        assert_eq!(
            rollback_script("/h/x", "/h/x.bak.7"),
            "set -u\nif [ -e /h/x.bak.7 ] || [ -L /h/x.bak.7 ]; then \
             mv -f -- /h/x.bak.7 /h/x && printf '%s\\n' restored; fi\n"
        );
        // No `set -e`: this runs on a path that is already failing.
        assert!(!rollback_script("/h/x", "/h/b").contains("set -e"),);
        assert_eq!(discard_backup_script("/h/x.bak.7"), "rm -f -- /h/x.bak.7\n");
    }

    /// Cleanup runs on paths that are already failing; a `set -eu` here
    /// would add a failure to classify to a job that already has one.
    #[test]
    fn cleanup_is_a_bare_best_effort_rm() {
        assert_eq!(cleanup_script("/h/x.tmp.7"), "rm -f -- /h/x.tmp.7\n");
        assert!(!cleanup_script("/h/x.tmp.7").contains("set -e"));
    }

    /// `shell_quote` stops the *shell* splitting a path; only `--` stops
    /// `tee`/`chmod`/`mv`/`rm` reading one that starts with `-` as an
    /// option of their own.
    #[test]
    fn every_generated_command_terminates_its_options_before_a_path() {
        // A path a hostile — or merely rc-mangled — remote could name.
        let tmp = "-t";
        let dest = "--target-directory=/x";
        for (command, expected) in [
            // `sh -c`-wrapped, so its quoting is one layer deeper: the
            // `--` is the part that has to survive the wrap.
            (stream_command(tmp), vec!["tee -- ", r"'\''-t'\''"]),
            (verify_staged_script(tmp), vec!["chmod -- 700 '-t'"]),
            (
                commit_script(tmp, dest, "-b"),
                vec![
                    "chmod -- 755 '-t'",
                    "mv -- '--target-directory=/x' '-b'",
                    "mv -- '-t' '--target-directory=/x'",
                ],
            ),
            (
                rollback_script(dest, "-b"),
                vec!["mv -f -- '-b' '--target-directory=/x'"],
            ),
            (discard_backup_script("-b"), vec!["rm -f -- '-b'"]),
            (cleanup_script(tmp), vec!["rm -f -- '-t'"]),
        ] {
            for fragment in expected {
                assert!(
                    command.contains(fragment),
                    "wanted {fragment:?} in {command:?}"
                );
            }
        }
    }

    /// Wrapped, like the other two remote commands: the login shell that
    /// would otherwise parse this is the user's choice, not ours.
    #[test]
    fn start_execs_the_resolved_path_under_a_wrapped_sh() {
        assert_eq!(
            start_script("/usr/bin/roost-session"),
            "sh -c 'exec /usr/bin/roost-session start'\n"
        );
        assert_eq!(
            start_script("/home/o'brien/.local/bin/roost-session"),
            "sh -c 'exec '\\''/home/o'\\''\\'\\'''\\''brien/.local/bin/roost-session'\\'' start'\n"
        );
    }

    // ------------------------------------------------------------------
    // parsers
    // ------------------------------------------------------------------

    #[test]
    fn nul_fields_drop_exactly_one_trailing_terminator() {
        assert_eq!(parse_nul_fields(b"").unwrap(), Vec::<String>::new());
        assert_eq!(parse_nul_fields(b"a\0").unwrap(), ["a"]);
        assert_eq!(parse_nul_fields(b"a\0b\0").unwrap(), ["a", "b"]);
        // An interior empty field is meaningful; only the terminator's
        // own empty tail goes.
        assert_eq!(parse_nul_fields(b"a\0\0").unwrap(), ["a", ""]);
        assert_eq!(parse_nul_fields(b"a\0\0b\0").unwrap(), ["a", "", "b"]);
    }

    /// Every generator here terminates every field, so a stream that
    /// does not end in a NUL was cut off mid-field — and the half-field
    /// it ends with is a prefix of a real path, which is a perfectly
    /// valid path to some *other* file. A `dest` truncated mid-word is
    /// exactly how an `mv` lands somewhere nobody consented to.
    #[test]
    fn an_unterminated_final_field_is_a_truncated_stream() {
        for bytes in [&b"a"[..], &b"a\0b"[..], &b"/h/.local/bin/roost-sess"[..]] {
            let error = parse_nul_fields(bytes).expect_err("must be refused");
            assert!(matches!(error, BootstrapError::Probe(_)), "{error:?}");
        }
    }

    /// These fields become paths that get spliced into another script;
    /// a lossy conversion would name a different file.
    #[test]
    fn non_utf8_output_is_an_error_not_a_lossy_conversion() {
        let error = parse_nul_fields(b"/tmp/\xff\0").expect_err("invalid UTF-8 must be refused");
        assert!(matches!(error, BootstrapError::Probe(_)), "{error:?}");
    }

    #[test]
    fn discovery_reads_the_platform_then_the_home_then_the_candidates() {
        let discovery = parse_discovery(
            b"Linux\0x86_64\0/home/u\0/home/u/.local/bin/roost-session\0/usr/bin/roost-session\0",
        )
        .expect("parse");
        assert_eq!(discovery.os, "Linux");
        assert_eq!(discovery.arch, "x86_64");
        assert_eq!(discovery.home, "/home/u");
        assert_eq!(
            discovery.candidates,
            ["/home/u/.local/bin/roost-session", "/usr/bin/roost-session"]
        );
    }

    #[test]
    fn discovery_with_no_candidates_is_a_clean_empty_answer() {
        let discovery = parse_discovery(b"Linux\0aarch64\0/home/u\0").expect("parse");
        assert!(discovery.candidates.is_empty());
        assert_eq!(discovery.home, "/home/u");
    }

    /// A remote with no `$HOME` is a real host, not a truncated answer:
    /// the ladder's `$HOME` rungs are skipped over there and the field
    /// arrives empty. The trailing-NUL pop must not eat it.
    #[test]
    fn discovery_accepts_a_home_less_remote() {
        let discovery = parse_discovery(b"Linux\0x86_64\0\0").expect("parse");
        assert_eq!(discovery.home, "");
        assert!(discovery.candidates.is_empty());
    }

    /// The probe emits absolute paths and nothing else, so anything else
    /// is an answer it cannot have produced. A leading-`-` word matters
    /// most: it is about to become an operand of `tee`/`chmod`/`mv`, and
    /// the remote is not the party that gets to decide their options.
    #[test]
    fn discovery_keeps_only_the_absolute_paths_the_probe_can_emit() {
        let discovery = parse_discovery(
            b"Linux\0x86_64\0/home/u\0-t\0relative/roost-session\0\0roost-session\0\
              /usr/bin/roost-session\0",
        )
        .expect("parse");
        assert_eq!(discovery.candidates, ["/usr/bin/roost-session"]);
    }

    /// `os`/`arch` are `uname` output and get trimmed; a path is a path
    /// and does not — a `$HOME` with a trailing space is a real
    /// directory, and rewriting it here would leave this side naming a
    /// file the far side never found.
    #[test]
    fn discovery_trims_uname_but_never_a_path() {
        let discovery =
            parse_discovery(b" Linux \0 x86_64 \0/home/trailing \0/home/trailing /roost-session\0")
                .expect("parse");
        assert_eq!(discovery.os, "Linux");
        assert_eq!(discovery.arch, "x86_64");
        assert_eq!(discovery.home, "/home/trailing ");
        assert_eq!(discovery.candidates, ["/home/trailing /roost-session"]);
    }

    #[test]
    fn discovery_refuses_a_truncated_or_uname_less_answer() {
        for bytes in [
            &b""[..],
            &b"Linux\0"[..],
            // Two fields and no `$HOME` field at all: the script always
            // emits one, so this answer was cut off.
            &b"Linux\0x86_64\0"[..],
            &b"\0x86_64\0/home/u\0"[..],
            &b"Linux\0\0/home/u\0"[..],
        ] {
            let error = parse_discovery(bytes).expect_err("must be refused");
            assert!(matches!(error, BootstrapError::Probe(_)), "{error:?}");
        }
    }

    fn asked(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    #[test]
    fn identity_pairs_round_trip_including_the_unanswered_ones() {
        let pairs =
            parse_identity_pairs(b"/a\0\0/b\0{\"x\":1}\0", &asked(&["/a", "/b"])).expect("parse");
        assert_eq!(
            pairs,
            [
                ("/a".to_string(), String::new()),
                ("/b".to_string(), "{\"x\":1}".to_string())
            ]
        );
        // Stopping at the first answer is normal: fewer pairs than
        // candidates, and the ones present still line up.
        let pairs = parse_identity_pairs(b"/a\0{}\0", &asked(&["/a", "/b"])).expect("parse");
        assert_eq!(pairs.len(), 1);
        assert!(parse_identity_pairs(b"", &asked(&["/a"]))
            .expect("parse")
            .is_empty());
    }

    /// An odd field count is a cut-off answer, not a set of pairs with
    /// one field missing — reading it as the latter would pair a path
    /// with the *next* path's output.
    #[test]
    fn a_half_pair_is_refused() {
        let candidates = asked(&["/a", "/b"]);
        for bytes in [&b"/a\0\0/b\0"[..], &b"/a\0"[..]] {
            let error = parse_identity_pairs(bytes, &candidates).expect_err("must be refused");
            assert!(matches!(error, BootstrapError::Probe(_)), "{error:?}");
        }
    }

    /// The path in a pair is *echoed* by the remote and then cloned into
    /// a [`ProbeOutcome`] that [`start_script`] later execs. It is
    /// checked against the candidate it must answer for, position by
    /// position — an answer for a path nobody asked about is refused,
    /// and there can never be more answers than questions.
    #[test]
    fn an_identity_answer_must_name_the_candidate_it_was_asked_about() {
        let candidates = asked(&["/a", "/b"]);
        for bytes in [
            // A path nobody asked about, in position 0.
            &b"/evil\0{}\0"[..],
            // Right paths, wrong order.
            &b"/b\0\0/a\0{}\0"[..],
            // An empty path.
            &b"\0out\0"[..],
            // More answers than questions.
            &b"/a\0\0/b\0\0/c\0{}\0"[..],
        ] {
            let error = parse_identity_pairs(bytes, &candidates).expect_err("must be refused");
            assert!(matches!(error, BootstrapError::Probe(_)), "{error:?}");
        }
        // Nothing was asked, so nothing may be answered.
        assert!(parse_identity_pairs(b"/a\0\0", &[]).is_err());
    }

    /// The destination is `$HOME` plus a constant of *this* side's, so
    /// the parser can hold the remote's answer to the whole constant.
    #[test]
    fn the_install_destination_is_home_plus_the_fixed_suffix() {
        assert_eq!(INSTALL_DEST, format!("$HOME{INSTALL_DEST_SUFFIX}"));
    }

    #[test]
    fn prepare_output_reads_back_as_tmp_dest_and_a_derived_backup() {
        let staged =
            parse_prepare(b"/h/.local/bin/roost-session.tmp.9\0/h/.local/bin/roost-session\0")
                .expect("parse");
        assert_eq!(staged.tmp, "/h/.local/bin/roost-session.tmp.9");
        assert_eq!(staged.dest, "/h/.local/bin/roost-session");
        // Derived from two already-validated pieces, so it costs no
        // round trip and needs no second answer to check.
        assert_eq!(staged.backup, "/h/.local/bin/roost-session.bak.9");
        // A `$HOME` with a trailing space is a real directory; the raw
        // field is the path, and nothing here rewrites it.
        let staged =
            parse_prepare(b"/h /.local/bin/roost-session.tmp.9\0/h /.local/bin/roost-session\0")
                .expect("parse");
        assert_eq!(staged.tmp, "/h /.local/bin/roost-session.tmp.9");
        assert_eq!(staged.dest, "/h /.local/bin/roost-session");

        for bytes in [&b"/only\0"[..], &b"/a\0/b\0/c\0"[..]] {
            let error = parse_prepare(bytes).expect_err("must be refused");
            assert!(
                matches!(
                    error,
                    BootstrapError::Install {
                        phase: InstallPhase::Prepare,
                        ..
                    }
                ),
                "{error:?}"
            );
        }
    }

    /// These two paths flow straight into `tee`, `chmod`, `mv` and `rm`
    /// on the far side, under a consent dialog that named
    /// `~/.local/bin/roost-session`. A remote that answers with anything
    /// but the pair [`prepare_script`] emits — compromised, or merely
    /// mangled by a chatty shell rc — must not get the streamed ELF
    /// written and renamed wherever it asked.
    #[test]
    fn prepare_refuses_any_pair_the_prepare_script_could_not_have_emitted() {
        for bytes in [
            // dest somewhere else entirely.
            &b"/home/u/.ssh/authorized_keys.tmp.9\0/home/u/.ssh/authorized_keys\0"[..],
            // Right binary name, absolute, and **not** the install
            // destination: `roost-session` alone is a suffix a great
            // many useful places share.
            &b"/etc/cron.hourly/roost-session.tmp.9\0/etc/cron.hourly/roost-session\0"[..],
            &b"/root/roost-session.tmp.9\0/root/roost-session\0"[..],
            &b"/h/.config/autostart/roost-session.tmp.9\0/h/.config/autostart/roost-session\0"[..],
            // The right *tail*, with nothing in front of it — which is
            // what an empty `$HOME` would produce, and prepare refuses
            // that before it gets here.
            &b"/.local/bin/roost-session.tmp.9\0/.local/bin/roost-session\0"[..],
            // Right binary name, but relative.
            &b"h/.local/bin/roost-session.tmp.9\0h/.local/bin/roost-session\0"[..],
            // Absolute, but tmp is not dest + `.tmp.<pid>`.
            &b"/tmp/elsewhere.tmp.9\0/h/.local/bin/roost-session\0"[..],
            // dest is the prefix of tmp but the suffix is not a pid.
            &b"/h/.local/bin/roost-session.tmp.\0/h/.local/bin/roost-session\0"[..],
            &b"/h/.local/bin/roost-session.tmp.9a\0/h/.local/bin/roost-session\0"[..],
            &b"/h/.local/bin/roost-session.tmp.9/../../x\0/h/.local/bin/roost-session\0"[..],
            // No `.tmp.` join at all.
            &b"/h/.local/bin/roost-session\0/h/.local/bin/roost-session\0"[..],
            // Empty fields.
            &b"\0/h/.local/bin/roost-session\0"[..],
            &b"/h/.local/bin/roost-session.tmp.9\0\0"[..],
            // A leading dash, which `--` alone should not have to catch.
            &b"-t.tmp.9\0-t\0"[..],
        ] {
            let error = parse_prepare(bytes).expect_err("must be refused");
            assert!(
                matches!(
                    error,
                    BootstrapError::Install {
                        phase: InstallPhase::Prepare,
                        ..
                    }
                ),
                "{:?} was accepted, or refused as the wrong family: {error:?}",
                String::from_utf8_lossy(bytes)
            );
            assert!(
                error.message("workbox").contains("Nothing was written"),
                "{error:?}"
            );
        }
    }

    #[test]
    fn an_identity_line_parses_and_anything_else_degrades_to_none() {
        let line = r#"{"app_version":"0.0.19","session_protocol":2,"libghostty_build":"g+s"}"#;
        assert_eq!(parse_identity_line(line), Some(identity("0.0.19", "g+s")));
        assert_eq!(
            parse_identity_line(&format!("{line}\n")),
            Some(identity("0.0.19", "g+s"))
        );
        for stdout in ["", "\n", "not json", "error: unknown subcommand", "{}"] {
            assert_eq!(parse_identity_line(stdout), None, "{stdout:?}");
        }
    }

    /// The readiness line is `session_launch`'s format, parsed by
    /// `session_launch`'s parser — a third reading of `ready pid=N`
    /// would be one more than the contract can survive.
    #[test]
    fn the_readiness_line_is_the_session_launch_verdict() {
        use crate::session_launch::Verdict;

        assert_eq!(Verdict::parse("ready pid=41"), Verdict::Ready(41));
        assert_eq!(
            Verdict::parse("already-running pid=7"),
            Verdict::AlreadyRunning(Some(7))
        );
        assert_eq!(
            Verdict::parse("already-running"),
            Verdict::AlreadyRunning(None)
        );
        assert!(matches!(Verdict::parse("error: nope"), Verdict::Error(_)));
        assert!(matches!(Verdict::parse("garbage"), Verdict::Error(_)));
    }

    // ------------------------------------------------------------------
    // the platform map
    // ------------------------------------------------------------------

    #[test]
    fn linux_is_the_only_platform_with_a_build() {
        check_os("Linux").expect("Linux");
        check_os(" linux \n").expect("trimmed and case-blind");
        for os in ["Darwin", "FreeBSD", "SunOS", ""] {
            let error = check_os(os).expect_err("{os} has no build");
            assert!(
                matches!(error, BootstrapError::UnsupportedOs(_)),
                "{error:?}"
            );
        }
        assert!(check_os("Darwin")
            .unwrap_err()
            .message("box")
            .contains("Linux only"));
    }

    #[test]
    fn the_arch_map_covers_both_spellings_of_both_builds() {
        for raw in ["x86_64", "amd64", " X86_64 "] {
            assert_eq!(map_arch(raw).expect(raw), RemoteArch::Amd64);
        }
        for raw in ["aarch64", "arm64", "ARM64"] {
            assert_eq!(map_arch(raw).expect(raw), RemoteArch::Arm64);
        }
        for raw in ["armv7l", "riscv64", "i686", ""] {
            let error = map_arch(raw).expect_err("no build");
            assert!(
                matches!(error, BootstrapError::UnsupportedArch(_)),
                "{error:?}"
            );
        }
        // The copy names the arch it refused.
        assert!(map_arch("riscv64")
            .unwrap_err()
            .message("box")
            .contains("riscv64"));
    }

    // ------------------------------------------------------------------
    // the compatibility rule
    // ------------------------------------------------------------------

    /// All three fields, exact compare. The `app_version` row is the
    /// one that makes this stricter than the runtime attach gate — see
    /// [`identity_matches`].
    #[test]
    fn the_install_rule_compares_all_three_fields_exactly() {
        let expected = identity("0.0.19", "ghostty-abc+snapshot.v1");
        assert!(identity_matches(&expected, &expected.clone()));
        assert!(!identity_matches(
            &expected,
            &identity("0.0.20", "ghostty-abc+snapshot.v1")
        ));
        assert!(!identity_matches(
            &expected,
            &identity("0.0.19", "ghostty-def+snapshot.v1")
        ));

        let mut protocol = expected.clone();
        protocol.session_protocol = 3;
        assert!(!identity_matches(&expected, &protocol));

        // No ordering: a newer build is not a match either.
        assert!(!identity_matches(
            &identity("0.0.20", "g"),
            &identity("0.0.19", "g")
        ));
    }

    // ------------------------------------------------------------------
    // classify_probe
    // ------------------------------------------------------------------

    fn line(app_version: &str, build: &str) -> String {
        serde_json::to_string(&identity(app_version, build)).expect("serialize")
    }

    #[test]
    fn no_candidates_is_missing() {
        assert_eq!(
            classify_probe(&options().expected, &[]),
            ProbeOutcome::Missing
        );
    }

    #[test]
    fn an_exact_match_is_compatible() {
        let expected = options().expected;
        let pairs = [(
            "/usr/bin/roost-session".to_string(),
            line("0.0.19", "ghostty-abc+snapshot.v1"),
        )];
        assert_eq!(
            classify_probe(&expected, &pairs),
            ProbeOutcome::Compatible {
                path: "/usr/bin/roost-session".to_string()
            }
        );
    }

    #[test]
    fn a_different_build_is_a_mismatch_carrying_what_it_said() {
        let pairs = [("/usr/bin/roost-session".to_string(), line("0.0.18", "old"))];
        assert_eq!(
            classify_probe(&options().expected, &pairs),
            ProbeOutcome::Mismatch {
                path: "/usr/bin/roost-session".to_string(),
                identity: Some(identity("0.0.18", "old")),
            }
        );
    }

    /// A build too old to know the `identify` subcommand: present, but
    /// unidentifiable. Same offer, different copy.
    #[test]
    fn a_binary_that_will_not_identify_is_a_mismatch_with_no_identity() {
        let pairs = [("/usr/bin/roost-session".to_string(), String::new())];
        assert_eq!(
            classify_probe(&options().expected, &pairs),
            ProbeOutcome::Mismatch {
                path: "/usr/bin/roost-session".to_string(),
                identity: None,
            }
        );
    }

    /// The conflicting-candidates case (plan 039 §9): a compatible
    /// binary further down the ladder is shadowed by whatever is above
    /// it, and the exec chain cannot version-rank. The verdict stays
    /// pinned to the rung that will actually be exec'd, so the install
    /// lands where it fixes the problem.
    #[test]
    fn a_mismatched_preferred_rung_shadows_a_compatible_later_one() {
        let expected = options().expected;
        let pairs = [
            (
                "/home/u/.local/bin/roost-session".to_string(),
                String::new(),
            ),
            (
                "/usr/bin/roost-session".to_string(),
                line("0.0.18", "ghostty-old+snapshot.v1"),
            ),
        ];
        assert_eq!(
            classify_probe(&expected, &pairs),
            ProbeOutcome::Mismatch {
                path: "/home/u/.local/bin/roost-session".to_string(),
                identity: None,
            }
        );

        // …and a match *further down* changes nothing, which is the
        // sharp half. `identity_script` stops at the first candidate
        // that answers, so this shape means rung 1 exists and is too old
        // to know `identify` — exactly the rung `exec_chain_command`
        // will exec. Calling the host `Compatible` here would offer no
        // install, so the attach would keep failing on the old binary
        // and the dialog that fixes it would never appear. `Mismatch` on
        // the preferred rung offers the install, which overwrites rung 1
        // and self-heals.
        let pairs = [
            (
                "/home/u/.local/bin/roost-session".to_string(),
                String::new(),
            ),
            (
                "/usr/bin/roost-session".to_string(),
                line("0.0.19", "ghostty-abc+snapshot.v1"),
            ),
        ];
        assert_eq!(
            classify_probe(&expected, &pairs),
            ProbeOutcome::Mismatch {
                path: "/home/u/.local/bin/roost-session".to_string(),
                identity: None,
            }
        );
        // Same shape, but rung 1 answered with a *wrong* build rather
        // than not answering: still the preferred rung's verdict.
        let pairs = [
            (
                "/home/u/.local/bin/roost-session".to_string(),
                line("0.0.18", "ghostty-old+snapshot.v1"),
            ),
            (
                "/usr/bin/roost-session".to_string(),
                line("0.0.19", "ghostty-abc+snapshot.v1"),
            ),
        ];
        assert_eq!(
            classify_probe(&expected, &pairs),
            ProbeOutcome::Mismatch {
                path: "/home/u/.local/bin/roost-session".to_string(),
                identity: Some(identity("0.0.18", "ghostty-old+snapshot.v1")),
            }
        );

        // The install destination is rung 1, so the offer this verdict
        // produces lands exactly where the shadow is.
        assert_eq!(INSTALL_DEST, "$HOME/.local/bin/roost-session");
    }

    // ------------------------------------------------------------------
    // release assets
    // ------------------------------------------------------------------

    // Naming pin (plan 039 §3.7, C4): linux/scripts/stage-session-artifact.sh
    // hardcodes this same "roost-session-0.0.19-linux-amd64" shape (and its
    // .sha256 sibling) in its own test,
    // linux/scripts/stage-session-artifact_test.sh — the release-side script
    // has no Rust to import this function from, so the two are kept from
    // drifting by cross-referencing comments instead. If this string moves,
    // update the shell test too (and vice versa).
    #[test]
    fn asset_names_are_versioned_per_arch_and_github_safe() {
        assert_eq!(
            asset_name("0.0.19", RemoteArch::Amd64),
            "roost-session-0.0.19-linux-amd64"
        );
        assert_eq!(
            asset_name("0.0.19", RemoteArch::Arm64),
            "roost-session-0.0.19-linux-arm64"
        );
        assert_eq!(
            checksum_name(&asset_name("0.0.19", RemoteArch::Arm64)),
            "roost-session-0.0.19-linux-arm64.sha256"
        );
        for name in [
            asset_name("0.0.19", RemoteArch::Amd64),
            checksum_name(&asset_name("0.0.19", RemoteArch::Amd64)),
        ] {
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')),
                "{name} would be sanitized by GitHub"
            );
        }
    }

    #[test]
    fn only_a_plain_three_part_version_is_stable() {
        for version in ["0.0.19", "1.2.3", "10.20.30"] {
            assert!(is_stable_version(version), "{version}");
        }
        for version in [
            "0.0.19-rc1",
            "0.0.19+build",
            "0.0",
            "0.0.19.1",
            "v0.0.19",
            "",
            "0.0.x",
        ] {
            assert!(!is_stable_version(version), "{version}");
        }
    }

    /// `version-check` compares only the *base* version, so a
    /// `v0.0.19-rc1` tag builds a client whose `CARGO_PKG_VERSION` is
    /// `0.0.19`. Such a client must never construct
    /// `.../download/v0.0.19/…` — that tag may not exist, or may hold a
    /// different build.
    #[test]
    fn a_stable_client_gets_a_release_url_and_a_prerelease_client_gets_none() {
        assert_eq!(
            default_asset_base("0.0.19").expect("stable"),
            "https://github.com/charliek/roost/releases/download/v0.0.19"
        );
        let error = default_asset_base("0.0.19-rc1").expect_err("prerelease must refuse");
        assert_eq!(
            error,
            BootstrapError::NoSource {
                app_version: "0.0.19-rc1".to_string()
            }
        );
        assert!(error.message("box").contains("ROOST_SESSION_INSTALL_BIN"));
    }

    #[test]
    fn https_is_allowed_and_plain_http_only_for_loopback() {
        for base in [
            "https://github.com/charliek/roost/releases/download/v0.0.19",
            "HTTPS://example.com/x",
            "https://localhost:8080/x",
            "http://127.0.0.1:53535",
            "http://localhost:53535/assets",
            "http://LOCALHOST/x",
            // A bracketed IPv6 authority: host is everything through
            // `]`, and only what follows it can be a port. Splitting on
            // the last colon first would read this as host `[:`.
            "http://[::1]:53535/assets",
            "http://[::1]",
            "https://[2001:db8::1]:8443/x",
        ] {
            check_asset_base(base).unwrap_or_else(|error| panic!("{base}: {error:?}"));
        }
        for base in [
            "http://example.com/x",
            "http://127.0.0.1.evil.example/x",
            "http://localhost.evil.example/x",
            "http://localhost@evil.example/x",
            "https://user:pass@example.com/x",
            "ftp://example.com/x",
            "file:///tmp/x",
            "example.com/x",
            "https://",
            "https://:8080/x",
            "http://[2001:db8::1]/x",
            "http://[::1/x",
            "http://[::1]x/y",
            "",
        ] {
            let error = check_asset_base(base).expect_err(base);
            assert!(matches!(error, BootstrapError::Download(_)), "{error:?}");
        }
    }

    /// `asset_plan` joins the asset name on textually, so a base with a
    /// query would bury the asset name inside the query string and a
    /// base with a fragment would make the asset URL and the checksum
    /// URL fetch the same path. No release base has either.
    #[test]
    fn a_base_with_a_query_or_a_fragment_is_refused_outright() {
        for base in [
            "https://host/download?token=x",
            "https://host/download#frag",
            "https://host/?",
            "https://host/download?",
            "http://127.0.0.1:9/assets?a=b",
        ] {
            let error = check_asset_base(base).expect_err(base);
            assert!(matches!(error, BootstrapError::Download(_)), "{error:?}");
        }
    }

    #[test]
    fn the_asset_plan_names_the_actual_origin() {
        let plan = options()
            .asset_plan(RemoteArch::Amd64)
            .expect("default base");
        assert!(!plan.overridden);
        assert_eq!(
            plan.asset_url,
            "https://github.com/charliek/roost/releases/download/v0.0.19/\
             roost-session-0.0.19-linux-amd64"
        );
        assert_eq!(plan.checksum_url, format!("{}.sha256", plan.asset_url));

        let mut overridden = options();
        overridden.asset_base = Some("http://127.0.0.1:9/assets/".to_string());
        let plan = overridden.asset_plan(RemoteArch::Arm64).expect("override");
        assert!(
            plan.overridden,
            "an override must never render as github.com"
        );
        assert_eq!(plan.base, "http://127.0.0.1:9/assets");
        assert_eq!(
            plan.asset_url,
            "http://127.0.0.1:9/assets/roost-session-0.0.19-linux-arm64"
        );

        // Trimmed *before* the trailing slashes come off, or the space
        // would be stored and land in the middle of every joined URL.
        let mut padded = options();
        padded.asset_base = Some("  http://127.0.0.1:9/assets/  ".to_string());
        let plan = padded.asset_plan(RemoteArch::Arm64).expect("override");
        assert_eq!(plan.base, "http://127.0.0.1:9/assets");
        assert_eq!(
            plan.asset_url,
            "http://127.0.0.1:9/assets/roost-session-0.0.19-linux-arm64"
        );

        let mut ipv6 = options();
        ipv6.asset_base = Some("http://[::1]:53535".to_string());
        assert_eq!(
            ipv6.asset_plan(RemoteArch::Amd64).expect("ipv6").asset_url,
            "http://[::1]:53535/roost-session-0.0.19-linux-amd64"
        );

        let mut query = options();
        query.asset_base = Some("https://host/download?token=x".to_string());
        assert!(matches!(
            query.asset_plan(RemoteArch::Amd64),
            Err(BootstrapError::Download(_))
        ));

        let mut downgrade = options();
        downgrade.asset_base = Some("http://example.com/assets".to_string());
        assert!(matches!(
            downgrade.asset_plan(RemoteArch::Amd64),
            Err(BootstrapError::Download(_))
        ));

        let mut prerelease = options();
        prerelease.expected.app_version = "0.0.19-rc1".to_string();
        assert!(matches!(
            prerelease.asset_plan(RemoteArch::Amd64),
            Err(BootstrapError::NoSource { .. })
        ));
    }

    // ------------------------------------------------------------------
    // the source preview (what the consent dialog may claim)
    // ------------------------------------------------------------------

    /// The rung the preview names is the rung [`resolve_source`] tries
    /// first, override before sibling before asset.
    #[test]
    fn the_preview_walks_the_same_ladder_resolution_does() {
        let arch = client_arch().unwrap_or(RemoteArch::Amd64);

        let mut env = options();
        env.install_bin = Some(PathBuf::from("/tmp/roost-session"));
        env.sibling_bin = Some(PathBuf::from("/usr/bin/roost-session"));
        let preview = env.source_preview(arch).expect("override rung");
        assert_eq!(preview.first, SourceOrigin::Override);
        assert_eq!(preview.fallback, None, "an override is taken or nothing is");
        assert!(
            preview.describe().contains("/tmp/roost-session")
                && preview.describe().contains(INSTALL_BIN_ENV),
            "{}",
            preview.describe()
        );

        // The sibling rung is only reachable where this client could run
        // over there at all, which is a compile-time fact.
        let mut sibling = options();
        sibling.sibling_bin = Some(PathBuf::from("/usr/bin/roost-session"));
        let preview = sibling.source_preview(arch).expect("some rung");
        if client_arch() == Some(arch) {
            assert_eq!(preview.first, SourceOrigin::Sibling);
            assert!(
                matches!(preview.fallback, Some(SourceOrigin::Asset { .. })),
                "a sibling that turns out to be stale falls through: {preview:?}"
            );
            let copy = preview.describe();
            assert!(copy.contains("this Roost's own roost-session"), "{copy}");
            assert!(
                copy.contains("if that turns out not to be this build"),
                "the fall-through is stated, not hidden: {copy}"
            );
        } else {
            assert!(
                matches!(preview.first, SourceOrigin::Asset { .. }),
                "a non-Linux client can never stream its own binary: {preview:?}"
            );
        }

        let bare = options().source_preview(arch).expect("asset rung");
        assert!(matches!(bare.first, SourceOrigin::Asset { .. }));
        assert_eq!(bare.fallback, None);
    }

    /// The dialog names the *actual* base. An overridden
    /// `ROOST_SESSION_ASSET_BASE` is never rendered as github.com — a
    /// fixture server or a mirror is exactly where a user needs to be
    /// told (plan 039 §3.5).
    #[test]
    fn an_overridden_asset_base_is_named_rather_than_masked() {
        let mut overridden = options();
        overridden.asset_base = Some("http://127.0.0.1:9/assets".to_string());
        overridden.source = Some(InstallSource::Asset);
        let copy = overridden
            .source_preview(RemoteArch::Amd64)
            .expect("asset rung")
            .describe();
        assert!(copy.contains("http://127.0.0.1:9/assets"), "{copy}");
        assert!(copy.contains(ASSET_BASE_ENV), "{copy}");
        assert!(!copy.contains("github.com"), "{copy}");
        assert_eq!(
            copy,
            format!(
                "the release at http://127.0.0.1:9/assets ({ASSET_BASE_ENV}), checksum-verified"
            ),
            "a noun phrase, not a clause — the consent dialog supplies the leading \
             \"from\" itself, so this must not start with a preposition of its own: {copy}"
        );

        let default = options()
            .source_preview(RemoteArch::Amd64)
            .expect("asset rung");
        let copy = default.describe();
        assert!(copy.contains("github.com"), "{copy}");
        assert!(copy.contains("checksum-verified"), "{copy}");
        assert!(!copy.contains(ASSET_BASE_ENV), "{copy}");
        assert!(
            copy.starts_with("the release at "),
            "the default rung must read the same as the overridden one: {copy}"
        );
    }

    /// A forced rung is the only rung. Forcing the sibling removes the
    /// fall-through the unforced ladder has, because `resolve_source`
    /// turns that rung's refusal into an error rather than trying the
    /// next one.
    #[test]
    fn a_forced_rung_previews_without_a_fall_through() {
        let arch = client_arch().unwrap_or(RemoteArch::Amd64);
        let mut forced = options();
        forced.sibling_bin = Some(PathBuf::from("/usr/bin/roost-session"));
        forced.source = Some(InstallSource::Sibling);
        if client_arch() == Some(arch) {
            let preview = forced.source_preview(arch).expect("forced sibling");
            assert_eq!(preview.first, SourceOrigin::Sibling);
            assert_eq!(preview.fallback, None);
        }

        // Nothing to force onto: no override set, and the asset rung is
        // excluded by the force.
        let mut nothing = options();
        nothing.source = Some(InstallSource::Env);
        assert!(matches!(
            nothing.source_preview(arch),
            Err(BootstrapError::NoSource { .. })
        ));
    }

    /// A prerelease client constructs no release URL at all, so there is
    /// nothing honest to promise and the preview refuses rather than
    /// naming a tag that does not exist.
    #[test]
    fn a_prerelease_client_has_no_asset_to_preview() {
        let mut prerelease = options();
        prerelease.expected.app_version = "0.0.19-rc1".to_string();
        assert!(matches!(
            prerelease.source_preview(RemoteArch::Amd64),
            Err(BootstrapError::NoSource { .. })
        ));
    }

    // ------------------------------------------------------------------
    // the checksum file
    // ------------------------------------------------------------------

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_well_formed_single_record_yields_its_hash() {
        let name = "roost-session-0.0.19-linux-amd64";
        assert_eq!(
            parse_checksum_file(&format!("{HASH}  {name}\n"), name).expect("parse"),
            HASH
        );
        // No trailing newline, single-space separator, and the `-b`
        // binary-mode marker all read the same.
        assert_eq!(
            parse_checksum_file(&format!("{HASH} {name}"), name).expect("parse"),
            HASH
        );
        assert_eq!(
            parse_checksum_file(&format!("{HASH}  *{name}\n"), name).expect("parse"),
            HASH
        );
        // Mixed case in, lowercase out.
        assert_eq!(
            parse_checksum_file(&format!("{}  {name}\n", HASH.to_uppercase()), name)
                .expect("parse"),
            HASH
        );
    }

    #[test]
    fn a_checksum_for_another_file_is_refused() {
        let error = parse_checksum_file(&format!("{HASH}  some-other-asset\n"), "wanted")
            .expect_err("wrong filename");
        assert!(matches!(error, BootstrapError::Checksum(_)), "{error:?}");
        assert!(error.message("box").contains("nothing was sent"));
    }

    #[test]
    fn more_than_one_record_is_refused() {
        let text = format!("{HASH}  a\n{HASH}  b\n");
        assert!(matches!(
            parse_checksum_file(&text, "a"),
            Err(BootstrapError::Checksum(_))
        ));
    }

    #[test]
    fn a_malformed_hash_or_record_is_refused() {
        for text in [
            String::new(),
            "\n\n".to_string(),
            format!("{}  a\n", &HASH[..63]),
            format!("{HASH}0  a\n"),
            format!("{}zz  a\n", &HASH[..62]),
            format!("{HASH}\n"),
            format!("{HASH}  a  b\n"),
        ] {
            assert!(
                matches!(
                    parse_checksum_file(&text, "a"),
                    Err(BootstrapError::Checksum(_))
                ),
                "{text:?} must be refused"
            );
        }
    }

    /// The grammar is exactly "64 hex, whitespace, optional `*`, the
    /// filename to end of line". Uppercase hex and the `*` binary marker
    /// are the two things accepted on purpose; everything else that
    /// looks like a checksum file but is not one is a refusal, because
    /// the answer decides what gets executed on someone's machine.
    #[test]
    fn the_accepted_checksum_grammar_is_exactly_one_hash_separator_name_record() {
        let name = "roost-session-0.0.19-linux-amd64";
        // A tab separator is whitespace and reads the same.
        assert_eq!(
            parse_checksum_file(&format!("{HASH}\t{name}\n"), name).expect("tab"),
            HASH
        );
        // Blank lines around the single record are not records.
        assert_eq!(
            parse_checksum_file(&format!("\n\n{HASH}  {name}\n\n"), name).expect("blank lines"),
            HASH
        );

        for (text, why) in [
            (
                format!("\\{HASH}  {name}\n"),
                "GNU's leading-backslash marker",
            ),
            (format!("# a comment\n{HASH}  {name}\n"), "a comment line"),
            (format!("{HASH}  ./{name}\n"), "a ./-prefixed name"),
            (format!("{HASH}  {name} \n"), "a trailing space in the name"),
            (format!("{HASH}  {name}\x07\n"), "a control character"),
            (format!("{HASH}  {name}\r"), "a lone carriage return"),
            (format!("\u{feff}{HASH}  {name}\n"), "a BOM"),
            (
                format!("{}\u{ff10}  {name}\n", &HASH[..63]),
                "a full-width unicode digit",
            ),
            (format!("{HASH}{name}\n"), "no separator at all"),
            (format!("{HASH}  **{name}\n"), "a doubled binary marker"),
            (format!("{HASH}  \n"), "an empty name"),
        ] {
            assert!(
                matches!(
                    parse_checksum_file(&text, name),
                    Err(BootstrapError::Checksum(_))
                ),
                "{why} must be refused: {text:?}"
            );
        }
    }

    // ------------------------------------------------------------------
    // failure copy
    // ------------------------------------------------------------------

    /// Every family names the host, and every family that stopped
    /// before the destination changed says so — the first thing a user
    /// wants from a failed install is whether the host still works.
    #[test]
    fn every_failure_family_names_the_target() {
        let families = [
            BootstrapError::Probe("no answer".into()),
            BootstrapError::UnsupportedOs("Darwin".into()),
            BootstrapError::UnsupportedArch("riscv64".into()),
            BootstrapError::NoSource {
                app_version: "0.0.19".into(),
            },
            BootstrapError::Source("not a file".into()),
            BootstrapError::Download("404".into()),
            BootstrapError::Checksum("mismatch".into()),
            BootstrapError::Install {
                phase: InstallPhase::Prepare,
                detail: "mkdir failed".into(),
            },
            BootstrapError::Install {
                phase: InstallPhase::Stream,
                detail: "broken pipe".into(),
            },
            BootstrapError::Install {
                phase: InstallPhase::Commit,
                detail: "mv failed".into(),
            },
            BootstrapError::Verify("wrong build".into()),
            BootstrapError::PostCommit {
                detail: "it exited 2".into(),
                restored: true,
            },
            BootstrapError::PostCommit {
                detail: "it exited 2".into(),
                restored: false,
            },
            BootstrapError::Stop("timed out".into()),
            BootstrapError::Start("exited 1".into()),
        ];
        for family in families {
            let message = family.message("workbox");
            assert!(message.contains("workbox"), "{family:?}: {message}");
            assert!(!message.is_empty());
        }
    }

    /// The copy that used to lie. A commit that may already have
    /// renamed, and a post-commit re-verify that failed, are both
    /// *after* the destination changed — so neither may claim the
    /// install is as it was, and the one that knows whether a rollback
    /// landed says which.
    #[test]
    fn no_failure_after_the_commit_claims_an_unchanged_install() {
        let commit = BootstrapError::Install {
            phase: InstallPhase::Commit,
            detail: "it did not finish within 60s".into(),
        }
        .message("workbox");
        assert!(
            !commit.contains("unchanged"),
            "the rename may already have landed: {commit}"
        );
        assert!(commit.contains("put back"), "{commit}");

        let restored = BootstrapError::PostCommit {
            detail: "it would not identify itself at all".into(),
            restored: true,
        }
        .message("workbox");
        assert!(restored.contains("put back"), "{restored}");
        assert!(!restored.contains("It was removed"), "{restored}");

        let fresh = BootstrapError::PostCommit {
            detail: "it would not identify itself at all".into(),
            restored: false,
        }
        .message("workbox");
        assert!(fresh.contains("no previous install"), "{fresh}");
        assert!(fresh.contains("still there"), "{fresh}");

        // The *staged* verify still says what it always could: it runs
        // before anything replaced the destination.
        let staged = BootstrapError::Verify("wrong build".into()).message("workbox");
        assert!(staged.contains("exactly as it was"), "{staged}");
    }

    /// A truncated stream is a full disk far more often than anything
    /// else, and the remedy is on the far host.
    #[test]
    fn a_failed_stream_hints_at_disk_space() {
        let message = BootstrapError::Install {
            phase: InstallPhase::Stream,
            detail: "broken pipe".into(),
        }
        .message("workbox");
        assert!(message.contains("disk"), "{message}");
        assert!(message.contains("unchanged"), "{message}");
    }

    // ------------------------------------------------------------------
    // options
    // ------------------------------------------------------------------

    #[test]
    fn a_forced_source_parses_its_three_spellings_and_nothing_else() {
        assert_eq!(InstallSource::parse("asset"), Some(InstallSource::Asset));
        assert_eq!(
            InstallSource::parse(" Sibling "),
            Some(InstallSource::Sibling)
        );
        assert_eq!(InstallSource::parse("ENV"), Some(InstallSource::Env));
        for raw in ["", "release", "download", "asset,sibling"] {
            assert_eq!(InstallSource::parse(raw), None, "{raw:?}");
        }
    }

    // ------------------------------------------------------------------
    // the job master
    // ------------------------------------------------------------------

    /// A scratch parent short enough for a `sun_path` on both platforms.
    /// A macOS `$TMPDIR` is deep enough that the job's own directory name
    /// plus OpenSSH's `ctl.XXXXXXXXXXXXXXXX` bind name overflows it,
    /// which is the real fallback `pick_socket_dir` exists for and not
    /// something to reproduce in every test.
    fn scratch_parent() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("roost-bs-")
            .tempdir_in("/tmp")
            .expect("a scratch parent under /tmp")
    }

    async fn job_in(parent: &std::path::Path) -> BootstrapJob {
        let target = match crate::ssh::classify("workbox").expect("classify the target") {
            crate::ssh::ResolvedTransport::Ssh(target) => target,
            other => panic!("expected an ssh target, got {other:?}"),
        };
        let ssh = SshTunnelOptions {
            config_paths: crate::ssh::SshConfigPaths {
                user: None,
                system: None,
            },
            scratch_parents: vec![parent.to_path_buf()],
            // Never spawned by these tests; the choreography that does
            // spawn it is `tests/bootstrap_test.rs`, against a fake ssh.
            ssh_bin: PathBuf::from("/nonexistent/ssh"),
            // Paired with `options()`'s own flag — the tunnel and the
            // job must never be jailed differently.
            jail_fs_root: options().jail_fs_root,
        };
        BootstrapJob::open(&target, &ssh, options())
            .await
            .expect("open a bootstrap job")
    }

    /// Every exec runs over a master of this job's own: a control socket
    /// on a path nothing else knows, opened `auto` so the first exec
    /// pays for the handshake and the rest do not.
    #[tokio::test]
    async fn every_job_exec_runs_over_a_private_persistent_master() {
        let parent = scratch_parent();
        let job = job_in(parent.path()).await;
        let argv = job.job_argv("/bin/sh -s");

        let ctl = job.dir.join("ctl");
        assert!(
            argv.windows(2)
                .any(|pair| pair[0] == "-S" && pair[1] == ctl.display().to_string()),
            "{argv:?} must name this job's own control socket"
        );
        for option in [
            "ControlMaster=auto",
            "ControlPersist=60s",
            "BatchMode=yes",
            "RequestTTY=no",
        ] {
            assert!(argv.iter().any(|arg| arg == option), "{option} in {argv:?}");
        }
        assert_eq!(argv.last().map(String::as_str), Some("/bin/sh -s"));
        assert_eq!(argv[argv.len() - 2], "workbox", "{argv:?}");
        assert!(
            argv.windows(2)
                .any(|pair| pair[0] == "-F" && pair[1] == job.config_path.display().to_string()),
            "{argv:?} must use the config this job generated"
        );
    }

    /// The job's directory must never read as a *tunnel's* leftovers, or
    /// the next `SshTunnel::open` for that host would sweep a running
    /// job's master away mid-install.
    #[tokio::test]
    async fn the_job_directory_is_never_read_as_a_tunnels_leftovers() {
        let parent = scratch_parent();
        let job = job_in(parent.path()).await;
        let name = job
            .dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a leaf name");
        assert!(
            name.starts_with("roost-ssh-bootstrap-"),
            "{name} must be recognizable in a directory listing"
        );
        assert_eq!(
            crate::ssh::parse_scratch_dir_name(name),
            None,
            "{name} must not parse as a host's scratch directory"
        );
    }

    /// The generated config is written 0600 into a 0700 directory, and
    /// the whole directory goes when the job does — including on the
    /// path where nobody remembered to close it.
    #[tokio::test]
    async fn a_dropped_job_takes_its_scratch_directory_with_it() {
        use std::os::unix::fs::PermissionsExt;

        let parent = scratch_parent();
        let dir = {
            let job = job_in(parent.path()).await;
            assert_eq!(
                std::fs::metadata(&job.dir)
                    .expect("the job directory")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert!(job.config_path.exists(), "the generated config");
            job.dir.clone()
        };
        assert!(!dir.exists(), "{} outlived its job", dir.display());
    }

    #[tokio::test]
    async fn closing_a_job_is_idempotent() {
        let parent = scratch_parent();
        let job = job_in(parent.path()).await;
        let dir = job.dir.clone();
        job.close().await;
        job.close().await;
        assert!(!dir.exists());
    }

    // ------------------------------------------------------------------
    // exec plumbing
    // ------------------------------------------------------------------

    fn outcome(code: Option<i32>, stdout: &str, stderr: &str) -> ExecOutcome {
        ExecOutcome {
            code,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.to_string(),
        }
    }

    #[test]
    fn a_failed_exec_reports_its_code_and_the_last_thing_it_said() {
        assert_eq!(
            outcome(Some(2), "", "warming up\nmv: not a directory\n").detail(),
            "it exited 2: mv: not a directory"
        );
        assert_eq!(
            outcome(Some(1), "", "  \n").detail(),
            "it exited 1 with nothing on stderr"
        );
        assert_eq!(
            outcome(None, "", "Killed\n").detail(),
            "it was killed by a signal: Killed"
        );
        assert_eq!(outcome(None, "", "").detail(), "it was killed by a signal");
    }

    #[test]
    fn the_first_line_of_output_skips_the_blank_ones_and_trims() {
        assert_eq!(
            outcome(Some(0), "\n  ready pid=42  \nnoise\n", "").first_line(),
            Some("ready pid=42".to_string())
        );
        assert_eq!(outcome(Some(0), "\n \n", "").first_line(), None);
    }

    #[tokio::test]
    async fn output_past_the_cap_is_refused_rather_than_truncated() {
        let bytes = vec![b'x'; 100];
        assert_eq!(
            read_capped(&bytes[..], 100).await.expect("exactly the cap"),
            bytes
        );
        let error = read_capped(&bytes[..], 99)
            .await
            .expect_err("one byte over the cap");
        assert!(error.contains("99"), "{error}");
    }

    /// `roost-session start`'s forking parent waits 30s of its own for
    /// the daemonized child to report. A budget under that would report
    /// a timeout for a start that was still going to answer.
    #[test]
    fn the_start_budget_outlasts_the_far_sides_own_readiness_wait() {
        assert!(START_BUDGET > Duration::from_secs(30));
        assert!(STREAM_BUDGET >= INSTALL_BUDGET);
    }

    // ------------------------------------------------------------------
    // the source ladder
    // ------------------------------------------------------------------

    fn temp_file(dir: &tempfile::TempDir, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, contents).expect("write a fixture file");
        path
    }

    #[tokio::test]
    async fn an_explicit_install_bin_wins_and_is_streamed_as_is() {
        let dir = scratch_parent();
        let bin = temp_file(&dir, "roost-session", b"\x7fELF");
        let mut opts = options();
        opts.install_bin = Some(bin.clone());

        let source = resolve_source(&opts, RemoteArch::Amd64, &dir.path().join("dl"))
            .await
            .expect("the override rung");
        assert_eq!(source.path(), bin);
        assert_eq!(*source.origin(), SourceOrigin::Override);
        assert!(source.describe().contains(INSTALL_BIN_ENV), "{source:?}");
    }

    #[tokio::test]
    async fn an_install_bin_that_is_missing_or_not_a_file_is_a_source_failure() {
        let dir = scratch_parent();
        for path in [dir.path().join("nope"), dir.path().to_path_buf()] {
            let mut opts = options();
            opts.install_bin = Some(path.clone());
            let error = resolve_source(&opts, RemoteArch::Amd64, &dir.path().join("dl"))
                .await
                .expect_err("an unusable override must not fall through");
            assert!(
                matches!(error, BootstrapError::Source(_)),
                "{path:?}: {error:?}"
            );
            // The user named a specific binary; installing a *different*
            // one instead is worse than saying so.
            assert!(error.message("workbox").contains("untouched"));
        }
    }

    /// Forcing the asset rung must skip the override entirely — on a
    /// Linux runner a usable sibling or override always wins, so
    /// without this CI would never once exercise the download path.
    #[tokio::test]
    async fn forcing_the_asset_rung_skips_an_available_override() {
        let dir = scratch_parent();
        let bin = temp_file(&dir, "roost-session", b"\x7fELF");
        let mut opts = options();
        opts.install_bin = Some(bin);
        opts.source = Some(InstallSource::Asset);
        opts.curl_bin = PathBuf::from("/nonexistent/curl");
        opts.asset_base = Some("https://127.0.0.1:1/assets".to_string());

        let error = resolve_source(&opts, RemoteArch::Amd64, &dir.path().join("dl"))
            .await
            .expect_err("the download must be attempted, not the override");
        assert!(
            matches!(&error, BootstrapError::Download(detail) if detail.contains("curl")),
            "{error:?}"
        );
    }

    /// A missing `curl` is a classified failure with copy that names it,
    /// never a panic and never an unexplained download error.
    #[tokio::test]
    async fn a_missing_curl_is_classified_and_leaves_the_host_untouched() {
        let dir = scratch_parent();
        let mut opts = options();
        opts.source = Some(InstallSource::Asset);
        opts.curl_bin = PathBuf::from("/nonexistent/curl");
        opts.asset_base = Some("https://example.invalid/assets".to_string());

        let error = resolve_source(&opts, RemoteArch::Arm64, &dir.path().join("dl"))
            .await
            .expect_err("no curl, no download");
        let message = error.message("workbox");
        assert!(message.contains("curl"), "{message}");
        assert!(message.contains("untouched"), "{message}");
    }

    #[tokio::test]
    async fn forcing_a_rung_that_cannot_be_used_says_so_instead_of_falling_through() {
        let dir = scratch_parent();
        let mut opts = options();
        opts.source = Some(InstallSource::Env);
        let error = resolve_source(&opts, RemoteArch::Amd64, &dir.path().join("dl"))
            .await
            .expect_err("forced at a rung with nothing on it");
        assert!(
            matches!(&error, BootstrapError::Source(detail) if detail.contains(INSTALL_BIN_ENV)),
            "{error:?}"
        );

        let mut opts = options();
        opts.source = Some(InstallSource::Sibling);
        opts.sibling_bin = None;
        let error = resolve_source(&opts, RemoteArch::Amd64, &dir.path().join("dl"))
            .await
            .expect_err("forced at the sibling rung with no sibling");
        assert!(matches!(error, BootstrapError::Source(_)), "{error:?}");
    }

    /// Rung 4: a prerelease client constructs no release URL (§3.3), and
    /// with no override and no usable sibling that is the end of the
    /// ladder — the host is never touched.
    #[tokio::test]
    async fn a_prerelease_client_with_no_other_source_refuses_before_touching_anything() {
        let dir = scratch_parent();
        let mut opts = options();
        opts.expected.app_version = "0.0.19-rc1".to_string();
        opts.source = Some(InstallSource::Asset);

        let error = resolve_source(&opts, RemoteArch::Amd64, &dir.path().join("dl"))
            .await
            .expect_err("no tag spelling to guess at");
        assert_eq!(
            error,
            BootstrapError::NoSource {
                app_version: "0.0.19-rc1".to_string()
            }
        );
        assert!(!dir.path().join("dl").exists(), "nothing was downloaded");
    }

    /// The sibling rung is a Linux-to-Linux affair: it streams the
    /// binary next to *this* client, which is an ELF for this client's
    /// own architecture.
    #[test]
    fn the_sibling_rung_only_applies_to_a_linux_build_of_the_right_arch() {
        let arch = client_arch();
        if cfg!(target_os = "linux") {
            assert!(arch.is_some(), "a Linux build knows its own arch");
        } else {
            assert_eq!(arch, None, "no roost-session is built for this platform");
        }
    }

    // ------------------------------------------------------------------
    // download plumbing
    // ------------------------------------------------------------------

    /// Hash a file by path, for the tests that do not care which handle
    /// the bytes came through.
    fn hash_path(path: &Path) -> String {
        let file = std::fs::File::open(path).expect("open the fixture");
        hash_open_file(file).expect("hash the file").0
    }

    /// The verified descriptor is what gets streamed, so a rewound `dup`
    /// of it must read the same bytes the hash covered — even after the
    /// *path* has been replaced underneath.
    #[test]
    fn the_verified_handle_reads_the_bytes_that_were_hashed_not_the_path() {
        let dir = scratch_parent();
        let asset = temp_file(&dir, "asset", b"the verified bytes\n");
        let (hashed, verified) =
            hash_open_file(std::fs::File::open(&asset).expect("open")).expect("hash");

        let source = ResolvedSource {
            path: asset.clone(),
            origin: SourceOrigin::Sibling,
            verified: Some(verified),
        };
        // Swap the *name* out from under it, which is what a local
        // attacker between the hash and the stream does: a rename puts
        // a different inode behind the path a re-open would resolve.
        let decoy = temp_file(&dir, "decoy", b"something else entirely\n");
        std::fs::rename(&decoy, &asset).expect("swap the path");
        assert_eq!(
            std::fs::read(&asset).expect("read the path"),
            b"something else entirely\n",
            "the path now names other bytes"
        );

        let mut reopened = source
            .reopen_verified()
            .expect("an asset carries its handle")
            .expect("dup the handle");
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut reopened, &mut bytes).expect("read the dup");
        assert_eq!(bytes, b"the verified bytes\n");
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let again: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(again, hashed);

        // …and a rung with nothing to verify streams by path, which is
        // deliberate: no published checksum exists for a local file.
        let plain = ResolvedSource {
            path: asset,
            origin: SourceOrigin::Sibling,
            verified: None,
        };
        assert!(plain.reopen_verified().is_none());
    }

    /// The two halves of the checksum gate agree: what [`hash_open_file`]
    /// computes is what [`parse_checksum_file`] reads out of the record
    /// `sha256sum` would have written for the same bytes.
    #[tokio::test]
    async fn the_hash_and_the_published_record_meet_in_the_middle() {
        let dir = scratch_parent();
        let asset = temp_file(&dir, "roost-session-0.0.19-linux-amd64", b"hello roost\n");
        let hashed = hash_path(&asset);
        // `printf 'hello roost\n' | sha256sum`
        assert_eq!(
            hashed,
            "f239ae84e5eeec18604ed45613a90fb726d172f82e35184e2174ea1c7cd1a72e"
        );
        let published = format!("{hashed}  roost-session-0.0.19-linux-amd64\n");
        assert_eq!(
            parse_checksum_file(&published, "roost-session-0.0.19-linux-amd64").expect("parse"),
            hashed
        );
        // Published uppercase, compared case-blind, exactly as
        // `download_into` compares them.
        let shouted = format!(
            "{}  roost-session-0.0.19-linux-amd64\n",
            hashed.to_ascii_uppercase()
        );
        assert!(
            parse_checksum_file(&shouted, "roost-session-0.0.19-linux-amd64")
                .expect("parse")
                .eq_ignore_ascii_case(&hashed)
        );
    }

    #[test]
    fn hashing_streams_rather_than_reading_the_file_whole() {
        let dir = scratch_parent();
        // Two chunks and a bit, so the loop runs more than once.
        let bytes = vec![b'z'; HASH_CHUNK * 2 + 7];
        let asset = temp_file(&dir, "big", &bytes);
        let streamed = hash_path(&asset);
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let at_once: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(streamed, at_once);
    }

    /// `-L` follows redirects, and a redirect can change scheme — so an
    /// `https://` base that [`check_asset_base`] passed can still be
    /// bounced to plaintext by the server, which is the whole rule
    /// undone by one `302`. The relaxation is confined to a URL that was
    /// *already* plaintext, which the base check only permits for
    /// loopback.
    #[test]
    fn curl_is_pinned_to_https_except_for_an_already_plaintext_loopback_url() {
        let secure = curl_argv("https://github.com/charliek/roost/x", ASSET_MAX_BYTES);
        for flag in ["--proto", "--proto-redir"] {
            let at = secure.iter().position(|arg| arg == flag).expect(flag);
            assert_eq!(secure[at + 1], "=https", "{flag} in {secure:?}");
        }

        let loopback = curl_argv("http://127.0.0.1:53535/assets/x", CHECKSUM_MAX_BYTES);
        for flag in ["--proto", "--proto-redir"] {
            let at = loopback.iter().position(|arg| arg == flag).expect(flag);
            assert_eq!(loopback[at + 1], "=https,http", "{flag} in {loopback:?}");
        }

        // No `-o`: the body comes back on stdout so it can be counted
        // while it is written.
        assert!(!secure.iter().any(|arg| arg == "-o"), "{secure:?}");
        // `--max-filesize` stays, as the cheap first bound.
        assert!(
            secure
                .windows(2)
                .any(|pair| pair[0] == "--max-filesize" && pair[1] == ASSET_MAX_BYTES.to_string()),
            "{secure:?}"
        );
        assert_eq!(
            secure.last().map(String::as_str),
            Some("https://github.com/charliek/roost/x")
        );
    }

    /// `--max-filesize` can only bound a length the server declared in
    /// advance, so a `Content-Length`-less response slips past it and is
    /// bounded by nothing but `--max-time`. The copy counts as it
    /// writes, which is a bound the server cannot decline to provide.
    #[tokio::test]
    async fn the_download_cap_is_enforced_while_writing_not_after() {
        let dir = scratch_parent();
        let out = dir.path().join("asset");

        let mut file = tokio::fs::File::create(&out).await.expect("create");
        let written = copy_capped(&b"exactly ten"[..], &mut file, 11)
            .await
            .expect("exactly the cap");
        assert_eq!(written, 11);

        let mut file = tokio::fs::File::create(&out).await.expect("create");
        let error = copy_capped(&b"one byte over"[..], &mut file, 12)
            .await
            .expect_err("one byte over the cap");
        assert!(error.contains("12"), "{error}");
    }

    #[test]
    fn a_logged_url_carries_no_credentials_and_no_query() {
        assert_eq!(
            redact_url("https://github.com/charliek/roost/releases/download/v0.0.19/x"),
            "https://github.com/charliek/roost/releases/download/v0.0.19/x"
        );
        assert_eq!(
            redact_url("https://user:secret@host/path?token=abc#frag"),
            "https://<redacted>@host/path"
        );
        assert_eq!(redact_url("not a url"), "<not a url>");
    }
}
