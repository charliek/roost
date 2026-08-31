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
//! The second half is the runtime built out of those pieces (C3): a
//! job-scoped `ssh` master, the bounded execs that carry these scripts
//! across it, the source ladder that decides *which* bytes to stream,
//! and the install choreography. Its tests live in
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

use std::path::PathBuf;

use crate::messages::SessionBinaryIdentity;

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

/// Remote-side prefix for the ladder's *absolute* rungs, empty in
/// production.
///
/// The hermetic `run-remote` test fixture (plan 039 §3.8) runs the real
/// scripts against a fake `$HOME` in a tempdir; without this the
/// `/usr/bin` rung would probe the developer's own machine and a
/// deb-installed box would answer differently from a clean one. Only
/// absolute rungs carry it — the `$HOME`-relative ones are already
/// jailed by the fake `$HOME` itself.
pub const FS_ROOT_ENV: &str = "ROOST_BOOTSTRAP_FS_ROOT";

/// Prefix an absolute ladder rung with the [`FS_ROOT_ENV`] seam. A
/// macro rather than a `format!` so [`CANDIDATES`] stays a `const`.
macro_rules! jailed {
    ($path:literal) => {
        concat!("${ROOST_BOOTSTRAP_FS_ROOT:-}", $path)
    };
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
    },
}

impl Candidate {
    /// The distinctive text this rung contributes to a generated
    /// script — what the ladder-equality test parses back out of both
    /// artifacts.
    pub fn marker(&self) -> String {
        match self.kind {
            CandidateKind::PathLookup => format!("command -v {SESSION_BIN_NAME}"),
            CandidateKind::Expansion { word, .. } => format!("\"{word}\""),
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
    fn guard(&self) -> String {
        match self.kind {
            CandidateKind::PathLookup => format!(
                "p=$(command -v {SESSION_BIN_NAME} 2>/dev/null) || p=; \
                 case \"$p\" in /*) [ -f \"$p\" ] && [ -x \"$p\" ] && "
            ),
            CandidateKind::Expansion { word, .. } => {
                format!("p=\"{word}\"; [ -f \"$p\" ] && [ -x \"$p\" ] && ")
            }
        }
    }

    /// This rung as one complete shell statement: the shared
    /// [`Candidate::guard`], then `action` against `"$p"`.
    ///
    /// Both ladders are generated through here, which is what makes
    /// "anything the probe can find, the transport can exec" a property
    /// of one function rather than of two lists that agree today.
    fn step(&self, action: &str) -> String {
        let body = match self.kind {
            CandidateKind::PathLookup => format!("{}{action};; esac", self.guard()),
            CandidateKind::Expansion { .. } => format!("{}{action}", self.guard()),
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
        },
    },
    Candidate {
        name: "path",
        kind: CandidateKind::PathLookup,
    },
    Candidate {
        name: "usr-bin",
        kind: CandidateKind::Expansion {
            word: jailed!("/usr/bin/roost-session"),
            requires: &[],
        },
    },
    Candidate {
        name: "linuxbrew",
        kind: CandidateKind::Expansion {
            word: jailed!("/home/linuxbrew/.linuxbrew/bin/roost-session"),
            requires: &[],
        },
    },
    Candidate {
        name: "nix-profile",
        kind: CandidateKind::Expansion {
            word: "$HOME/.nix-profile/bin/roost-session",
            requires: &["HOME"],
        },
    },
    Candidate {
        name: "nix-per-user",
        kind: CandidateKind::Expansion {
            word: jailed!("/etc/profiles/per-user/$USER/bin/roost-session"),
            requires: &["USER"],
        },
    },
    Candidate {
        name: "nix-default-profile",
        kind: CandidateKind::Expansion {
            word: jailed!("/nix/var/nix/profiles/default/bin/roost-session"),
            requires: &[],
        },
    },
    Candidate {
        name: "nixos-system",
        kind: CandidateKind::Expansion {
            word: jailed!("/run/current-system/sw/bin/roost-session"),
            requires: &[],
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

/// The read-only discovery script: what platform is this, and which
/// rungs of the ladder are actually there?
///
/// Fed to `/bin/sh -s` on stdin, it writes NUL-delimited fields —
/// `uname -s`, `uname -m`, then one field per rung that exists and is
/// executable — and exits 0 whatever it found. Deliberately **not**
/// `set -eu`: a host with no `$HOME` (or no `uname`) still has an
/// answer, and the answer is "nothing here", not a failed exec.
///
/// `[ -f ] && [ -x ]` rather than `[ -e ]` is what keeps a dangling
/// symlink — and an executable *directory* — out of the candidate list:
/// a path that exists but cannot be run is not a candidate, it is a trap
/// the identity exec would fall into. The test itself lives on
/// [`Candidate::guard`], shared with [`exec_chain_command`].
pub fn discovery_script() -> String {
    let mut out = String::new();
    out.push_str("printf '%s\\0' \"$(uname -s 2>/dev/null)\"\n");
    out.push_str("printf '%s\\0' \"$(uname -m 2>/dev/null)\"\n");
    for candidate in CANDIDATES {
        out.push_str(&candidate.step("printf '%s\\0' \"$p\""));
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
pub fn exec_chain_command() -> String {
    let action = format!("exec \"$p\" {BRIDGE_SUBCOMMAND}");
    let mut steps: Vec<String> = CANDIDATES
        .iter()
        .map(|candidate| candidate.step(&action))
        .collect();
    steps.push(format!(
        "printf '%s\\n' '{SESSION_BIN_NAME}: command not found' >&2; exit 127"
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
    out.push_str("tmp=\"${dest}.tmp.$$\"\n");
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
/// `chmod 700` first: `tee` created the temporary in the previous exec
/// under the login shell's umask, typically 022, so it is on disk
/// world-readable until something says otherwise. This is the first
/// exec that can say so.
///
/// `chmod -- <mode> <path>`, not `chmod <mode> -- <path>`. The mode is
/// `chmod`'s first *operand*, and a POSIX utility stops parsing options
/// at its first operand — so a trailing `--` is not a terminator, it is
/// a filename, and BSD `chmod` says exactly that. GNU `chmod` permutes
/// and accepts both spellings; only this one is portable.
pub fn verify_staged_script(tmp: &str) -> String {
    let quoted = shell_quote(tmp);
    format!("set -eu\nchmod -- 700 {quoted}\nchmod -- 755 {quoted}\nexec {quoted} identify\n")
}

/// Phase 4: the atomic rename that makes the staged binary the
/// installed one.
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
pub fn commit_script(tmp: &str, dest: &str) -> String {
    format!(
        "set -eu\n[ -f {tmp} ]\n[ ! -d {dest} ]\nmv -- {tmp} {dest}\n",
        tmp = shell_quote(tmp),
        dest = shell_quote(dest)
    )
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
    if fields.len() < 2 {
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
        candidates: fields[2..]
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
/// So: both absolute, `dest` ending in `/roost-session`, and `tmp`
/// exactly `dest` + `.tmp.` + the remote `$$`. No `trim` — the raw
/// field is the path, and a `$HOME` with a trailing space is a real
/// directory this side must keep agreeing with the far side about.
pub fn parse_prepare(bytes: &[u8]) -> Result<(String, String), BootstrapError> {
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
             emits (an absolute {SESSION_BIN_NAME} path and that path plus `.tmp.<pid>`)"
        ),
    };
    if !dest.starts_with('/') || !dest.ends_with(&format!("/{SESSION_BIN_NAME}")) {
        return Err(refuse());
    }
    let Some(pid) = tmp.strip_prefix(&format!("{dest}.tmp.")) else {
        return Err(refuse());
    };
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(refuse());
    }
    Ok((tmp.clone(), dest.clone()))
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
    /// What landed on the far side is not the build that was streamed.
    Verify(String),
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
                InstallPhase::Commit => format!(
                    "couldn't put the new roost-session in place on {target}: {detail}. The \
                     existing install is unchanged."
                ),
            },
            Self::Verify(detail) => format!(
                "the roost-session staged on {target} isn't the build this Roost needs: \
                 {detail}. It was removed and {target}'s existing install was left exactly as \
                 it was."
            ),
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
    /// A forced source rung, or `None` for the normal ladder.
    pub source: Option<InstallSource>,
}

impl BootstrapOptions {
    /// The injected identity plus whatever the environment overrides.
    ///
    /// [`SOURCE_ENV`] is read only under `ROOST_TEST_MODE=1`, the same
    /// double gate `roost-session`'s fake-build override uses: forcing
    /// the ladder is a lane's tool, not a shipped surface.
    pub fn from_env(expected: SessionBinaryIdentity) -> Self {
        let test_mode = std::env::var("ROOST_TEST_MODE").is_ok_and(|value| value == "1");
        Self {
            expected,
            asset_base: std::env::var(ASSET_BASE_ENV)
                .ok()
                .filter(|value| !value.trim().is_empty()),
            install_bin: std::env::var_os(INSTALL_BIN_ENV)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            source: if test_mode {
                std::env::var(SOURCE_ENV)
                    .ok()
                    .as_deref()
                    .and_then(InstallSource::parse)
            } else {
                None
            },
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
            source: None,
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
    fn enumerated_rungs(script: &str) -> Vec<&'static str> {
        let mut found: Vec<(usize, &'static str)> = CANDIDATES
            .iter()
            .filter_map(|candidate| {
                let marker = candidate.marker();
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
    #[test]
    fn the_probe_and_the_exec_chain_enumerate_the_identical_ladder() {
        let ladder: Vec<&str> = CANDIDATES.iter().map(|c| c.name).collect();
        let discovery = discovery_script();
        let exec_chain = exec_chain_command();
        assert_eq!(
            enumerated_rungs(&discovery),
            ladder,
            "the probe skips a rung"
        );
        assert_eq!(
            enumerated_rungs(&exec_chain),
            ladder,
            "the exec chain skips a rung"
        );

        for candidate in CANDIDATES {
            let guard = candidate.guard();
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
                    "{} carries a different guard for {} than the other ladder does:\n\
                     wanted {guard:?}\nin {script}",
                    which,
                    candidate.name
                );
            }
        }
    }

    #[test]
    fn the_probe_guards_unset_variables_and_never_sets_eu() {
        let script = discovery_script();
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
        for script in [discovery_script(), exec_chain_command()] {
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
        for script in [discovery_script(), exec_chain_command()] {
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
        let script = discovery_script();
        let os = script.find("uname -s").expect("uname -s");
        let arch = script.find("uname -m").expect("uname -m");
        let first_rung = script.find("$HOME/.local/bin").expect("first rung");
        assert!(os < arch && arch < first_rung, "{script}");
    }

    /// Absolute rungs are prefixable so the hermetic fixture can jail
    /// them (plan 039 §3.8); `$HOME`-relative ones are already jailed by
    /// the fake `$HOME`, and prefixing them would double up.
    #[test]
    fn absolute_rungs_carry_the_filesystem_root_seam() {
        // Spelled from the constant, so renaming the env var cannot
        // leave the scripts and this assertion agreeing with each other
        // and with nothing else.
        let fs_root_prefix = format!("${{{FS_ROOT_ENV}:-}}");

        for script in [discovery_script(), exec_chain_command()] {
            for absolute in [
                "/usr/bin/roost-session",
                "/home/linuxbrew/.linuxbrew/bin/roost-session",
                "/etc/profiles/per-user/$USER/bin/roost-session",
                "/nix/var/nix/profiles/default/bin/roost-session",
                "/run/current-system/sw/bin/roost-session",
            ] {
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

    /// A drop-in for `ssh::remote_command`: one argv element, one
    /// `sh -c`, one `client-bridge`.
    #[test]
    fn the_exec_chain_is_one_sh_c_word_that_execs_the_bridge() {
        let command = exec_chain_command();
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
        let command = exec_chain_command();
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
    /// narrow it.
    #[test]
    fn the_staged_verify_narrows_then_chmods_then_execs_identify() {
        assert_eq!(
            verify_staged_script("/home/u/x.tmp.7"),
            "set -eu\nchmod -- 700 /home/u/x.tmp.7\nchmod -- 755 /home/u/x.tmp.7\n\
             exec /home/u/x.tmp.7 identify\n"
        );
    }

    /// `chmod`'s mode is its first *operand*, and a POSIX utility stops
    /// parsing options there — so `chmod 755 -- path` puts the `--`
    /// where it is a filename, not a terminator, and BSD `chmod` says
    /// so out loud. The terminator has to come first.
    #[test]
    fn chmod_terminates_its_options_before_the_mode_not_after() {
        let script = verify_staged_script("/h/x.tmp.7");
        assert!(!script.contains("chmod 7"), "{script}");
        assert_eq!(script.matches("chmod -- 7").count(), 2, "{script}");
    }

    /// `[ ! -d ]` is the load-bearing one: POSIX `mv file dir` moves the
    /// file *inside* and exits 0, so without it a `dest` that is a
    /// directory reports a successful install that put nothing in place,
    /// and the next connect execs a directory and exits 126 — which
    /// nothing classifies as `NotFound`, so no new offer ever appears.
    #[test]
    fn commit_guards_the_temporary_and_the_destination_then_renames() {
        assert_eq!(
            commit_script("/h/x.tmp.7", "/h/x"),
            "set -eu\n[ -f /h/x.tmp.7 ]\n[ ! -d /h/x ]\nmv -- /h/x.tmp.7 /h/x\n"
        );
        assert_eq!(
            commit_script("/h/o'b.tmp.7", "/h/o'b"),
            "set -eu\n[ -f '/h/o'\\''b.tmp.7' ]\n[ ! -d '/h/o'\\''b' ]\n\
             mv -- '/h/o'\\''b.tmp.7' '/h/o'\\''b'\n"
        );
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
            (
                verify_staged_script(tmp),
                vec!["chmod -- 700 '-t'", "chmod -- 755 '-t'"],
            ),
            (
                commit_script(tmp, dest),
                vec!["mv -- '-t' '--target-directory=/x'"],
            ),
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
    fn discovery_reads_the_platform_then_the_candidates() {
        let discovery = parse_discovery(
            b"Linux\0x86_64\0/home/u/.local/bin/roost-session\0/usr/bin/roost-session\0",
        )
        .expect("parse");
        assert_eq!(discovery.os, "Linux");
        assert_eq!(discovery.arch, "x86_64");
        assert_eq!(
            discovery.candidates,
            ["/home/u/.local/bin/roost-session", "/usr/bin/roost-session"]
        );
    }

    #[test]
    fn discovery_with_no_candidates_is_a_clean_empty_answer() {
        let discovery = parse_discovery(b"Linux\0aarch64\0").expect("parse");
        assert!(discovery.candidates.is_empty());
    }

    /// The probe emits absolute paths and nothing else, so anything else
    /// is an answer it cannot have produced. A leading-`-` word matters
    /// most: it is about to become an operand of `tee`/`chmod`/`mv`, and
    /// the remote is not the party that gets to decide their options.
    #[test]
    fn discovery_keeps_only_the_absolute_paths_the_probe_can_emit() {
        let discovery = parse_discovery(
            b"Linux\0x86_64\0-t\0relative/roost-session\0\0roost-session\0/usr/bin/roost-session\0",
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
            parse_discovery(b" Linux \0 x86_64 \0/home/trailing /roost-session\0").expect("parse");
        assert_eq!(discovery.os, "Linux");
        assert_eq!(discovery.arch, "x86_64");
        assert_eq!(discovery.candidates, ["/home/trailing /roost-session"]);
    }

    #[test]
    fn discovery_refuses_a_truncated_or_uname_less_answer() {
        for bytes in [
            &b""[..],
            &b"Linux\0"[..],
            &b"\0x86_64\0"[..],
            &b"Linux\0\0"[..],
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

    #[test]
    fn prepare_output_reads_back_as_tmp_then_dest() {
        let (tmp, dest) =
            parse_prepare(b"/h/.local/bin/roost-session.tmp.9\0/h/.local/bin/roost-session\0")
                .expect("parse");
        assert_eq!(tmp, "/h/.local/bin/roost-session.tmp.9");
        assert_eq!(dest, "/h/.local/bin/roost-session");
        // A `$HOME` with a trailing space is a real directory; the raw
        // field is the path, and nothing here rewrites it.
        let (tmp, dest) =
            parse_prepare(b"/h /bin/roost-session.tmp.9\0/h /bin/roost-session\0").expect("parse");
        assert_eq!(tmp, "/h /bin/roost-session.tmp.9");
        assert_eq!(dest, "/h /bin/roost-session");

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
            // Right binary name, but relative.
            &b"h/bin/roost-session.tmp.9\0h/bin/roost-session\0"[..],
            // Absolute, but tmp is not dest + `.tmp.<pid>`.
            &b"/tmp/elsewhere.tmp.9\0/h/bin/roost-session\0"[..],
            // dest is the prefix of tmp but the suffix is not a pid.
            &b"/h/bin/roost-session.tmp.\0/h/bin/roost-session\0"[..],
            &b"/h/bin/roost-session.tmp.9a\0/h/bin/roost-session\0"[..],
            &b"/h/bin/roost-session.tmp.9/../../x\0/h/bin/roost-session\0"[..],
            // No `.tmp.` join at all.
            &b"/h/bin/roost-session\0/h/bin/roost-session\0"[..],
            // Empty fields.
            &b"\0/h/bin/roost-session\0"[..],
            &b"/h/bin/roost-session.tmp.9\0\0"[..],
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
            BootstrapError::Stop("timed out".into()),
            BootstrapError::Start("exited 1".into()),
        ];
        for family in families {
            let message = family.message("workbox");
            assert!(message.contains("workbox"), "{family:?}: {message}");
            assert!(!message.is_empty());
        }
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
}
