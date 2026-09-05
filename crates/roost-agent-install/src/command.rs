//! The one string Roost writes into every agent's config — and the
//! ownership rule that follows from it.
//!
//! It is the same bytes on every machine, per agent and per integration
//! version: no absolute paths, so a synced dotfile directory works
//! unchanged on the next machine and on a host. The indirection is
//! `${ROOST_AGENT_HOOK:-}`, which the UI (or `roost-session`) puts into
//! each tab's environment.
//!
//! Because the string is fixed, it doubles as the marker: an entry is
//! Roost's **iff its command is byte-equal** to one of the strings this
//! module has ever produced. That is deliberately not a substring test.
//! A user's own hook that mentions `$ROOST_AGENT_HOOK` is theirs and
//! survives an uninstall; a Roost entry the user has edited stops being
//! ours and is left exactly where it is (doctor names it instead).
//!
//! ## Who actually runs the string
//!
//! The design once assumed every agent hands the command to a shell, so
//! the `else` branch below would always get its chance. That assumption
//! was wrong for grok and cost users a red row per tool call. What each
//! agent really does:
//!
//! | Agent | Execution model | Consequence |
//! |---|---|---|
//! | Claude | shell | our `sh -c` runs; the fallback answers |
//! | codex | shell | same; codex additionally *hashes* the entry (see `codex_hash`) |
//! | cursor | shell | same |
//! | grok | **validates every `$` reference against its own environment first**, then a shell | a bare `$NAME` that is not set means the hook is **never executed** |
//! | opencode | neither — a JS plugin that reads `process.env` and spawns | no command string at all |
//!
//! grok's row is the load-bearing one, and it is measured, not
//! inferred (grok 1.0.13, isolated `GROK_HOME`, 2026-09-05):
//!
//! - a bare `$ROOST_AGENT_HOOK` or `${ROOST_AGENT_HOOK}` with the
//!   variable unset: refused — `hook not executed: required env var(s)
//!   not set: ${ROOST_AGENT_HOOK}`, drawn as a failed-hook row in the
//!   user's scrollback on every event;
//! - a bare **local** `$out`, which no environment ever holds: refused
//!   the same way (`… not set: ${out}`) — the check is over every
//!   reference in the string, not only the environment ones;
//! - `${NAME:-}` for either: accepted and handed to the shell
//!   **unchanged** — a local `out=hello; printf "%s" "${out:-}"` prints
//!   `hello`, so grok validates, it does not substitute;
//! - the hook's own stdout reaches grok byte for byte.
//!
//! It is fail-open, so nothing breaks; it is just permanent noise in any
//! terminal that is not Roost. Writing every reference as `${NAME:-}`
//! satisfies grok's check and is the same POSIX expansion for everyone
//! else, so the one spelling serves both models. Adding an adapter?
//! Establish which row it belongs in before assuming the shell will be
//! reached.

use roost_agent::Agent;

/// The environment variable every installed command reads.
pub const HOOK_ENV: &str = "ROOST_AGENT_HOOK";

/// Bumped when the written entries change shape. It is recorded per
/// agent in the state record, so `ensure` can tell "wired, but by an
/// older Roost" from "wired and current", and [`owned_commands`] keeps
/// every past spelling so an upgrade refreshes rather than duplicates.
pub const INTEGRATION_VERSION: u32 = 3;

/// The timeout Roost asks an agent for. Ten seconds is far past the
/// hook verb's own 2 s budget; it exists so a stalled machine does not
/// leave the agent waiting forever.
///
/// It is the *starting* value, not the written one everywhere:
/// `codex::hook_timeout_secs` lowers it on the two events codex caps at
/// 3 s, because codex warns on every launch about anything above its own
/// cap.
pub const HOOK_TIMEOUT_SECS: u64 = 10;

/// The command string for `agent`, at the current integration version.
///
/// The `else` branch is not decoration. `PermissionRequest` is a
/// *decision* hook on both Claude and codex: the approval dialog the
/// user is looking at blocks on this process, and a hook that exits
/// non-zero with no JSON may be read as a block. A bare `exec` of a
/// binary that has moved (a relocated bundle, a `brew upgrade`) would do
/// exactly that — 127, no output. So an unset variable, an empty one, a
/// missing binary and a crashing one all end the same way: stdin
/// drained, `{}` on stdout, exit 0.
///
/// The hook's own answer is **captured, not streamed**, which is the
/// whole difference between v2 and v1. A hook that prints part of a JSON
/// object and *then* dies would otherwise leave that fragment on stdout
/// with the fallback `{}` stapled to the end of it, and a decision hook
/// reading `{"decision":"blo{}` may take it for a block. Nothing reaches
/// the agent until the hook has exited 0.
///
/// **Every variable is spelled `${NAME:-}`**, and that is v3. grok does
/// not hand the string to a shell unexamined: it checks every `$`
/// reference against its environment first — the local `$out`
/// included — and *refuses to run the hook at all* when one is unset,
/// then draws the refusal as a red row in the user's scrollback —
/// `hook not executed: required env var(s) not set: ${ROOST_AGENT_HOOK}`,
/// on every tool call, in every terminal that isn't Roost. The
/// shell-style default passes that check, so grok runs the command and
/// our own `else` branch answers `{}` silently, which is the "inert
/// outside Roost" guarantee we actually promised. It is identical POSIX
/// shell for `sh`/`dash`/`bash` (and `set -u`-safe), so no other agent
/// notices.
pub fn installed_command(agent: Agent) -> String {
    format!(
        "sh -c 'if [ -n \"${{{HOOK_ENV}:-}}\" ] && out=$(\"${{{HOOK_ENV}:-}}\" agent-hook \
         {source} 2>/dev/null); then [ -n \"${{out:-}}\" ] || out=\"{{}}\"; printf \"%s\" \
         \"${{out:-}}\"; else cat >/dev/null; printf \"{{}}\"; fi'",
        source = agent.source()
    )
}

/// Integration version 2's spelling: the capture-then-print shape, but
/// with the bare `$ROOST_AGENT_HOOK` and `$out` references grok refuses
/// to run.
fn command_v2(agent: Agent) -> String {
    format!(
        "sh -c 'if [ -n \"${HOOK_ENV}\" ] && out=$(\"${HOOK_ENV}\" agent-hook {source} \
         2>/dev/null); then [ -n \"$out\" ] || out=\"{{}}\"; printf \"%s\" \"$out\"; \
         else cat >/dev/null; printf \"{{}}\"; fi'",
        source = agent.source()
    )
}

/// Integration version 1's spelling, kept so an entry an older Roost
/// installed is still recognised — and so still refreshed and still
/// cleanable — rather than orphaned in the user's file forever.
/// Ownership is byte equality, so a retired string has to stay on this
/// list for as long as any machine might still carry it.
fn command_v1(agent: Agent) -> String {
    format!(
        "sh -c 'if [ -n \"${HOOK_ENV}\" ] && \"${HOOK_ENV}\" agent-hook {source} 2>/dev/null; \
         then :; else cat >/dev/null; printf \"{{}}\"; fi'",
        source = agent.source()
    )
}

/// Every command string Roost has ever installed for `agent`, current
/// first. Ownership is exact equality against this list.
pub fn owned_commands(agent: Agent) -> Vec<String> {
    vec![
        installed_command(agent),
        command_v2(agent),
        command_v1(agent),
    ]
}

/// Is `command` an entry Roost wrote — at any integration version?
pub fn is_roost_command(agent: Agent, command: &str) -> bool {
    owned_commands(agent).iter().any(|owned| owned == command)
}

/// Does `command` look like a Roost entry the user has since edited?
///
/// Used **only** to produce the "modified Roost entry" fact doctor
/// renders. It never removes, rewrites, or claims anything: ownership is
/// [`is_roost_command`], and this deliberately answers `false` for
/// everything that one answers `true` for.
pub fn looks_edited(agent: Agent, command: &str) -> bool {
    !is_roost_command(agent, command)
        && command.contains(HOOK_ENV)
        && command.contains(&format!("agent-hook {}", agent.source()))
}

/// Does every `$` in `command` resolve for an agent that interpolates
/// the string before running it — i.e. is every reference `${NAME:-…}`
/// rather than a bare `$NAME`?
///
/// `$(` is command substitution, not a reference, and is allowed. The
/// rule this answers is the grok row of the table at the top of this
/// module; it is asserted both against the canonical constant and
/// against the strings each agent's writer actually puts on disk.
#[cfg(test)]
pub(crate) fn every_reference_has_a_default(command: &str) -> bool {
    let bytes = command.as_bytes();
    command.match_indices('$').all(|(at, _)| {
        bytes.get(at + 1) == Some(&b'(')
            || command[at..]
                .strip_prefix("${")
                .and_then(|rest| rest.split_once('}'))
                .is_some_and(|(name, _)| name.contains(":-"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;
    use std::process::{Command, Stdio};

    /// Frozen literal, not a re-derivation. This exact string is in
    /// users' `settings.json` files; changing it silently would orphan
    /// every entry already installed (ownership is byte equality), so a
    /// change has to land here, in `owned_commands`, and in a bumped
    /// `INTEGRATION_VERSION` together.
    #[test]
    fn the_claude_command_is_the_frozen_string() {
        assert_eq!(
            installed_command(Agent::Claude),
            "sh -c 'if [ -n \"${ROOST_AGENT_HOOK:-}\" ] && out=$(\"${ROOST_AGENT_HOOK:-}\" agent-hook claude 2>/dev/null); then [ -n \"${out:-}\" ] || out=\"{}\"; printf \"%s\" \"${out:-}\"; else cat >/dev/null; printf \"{}\"; fi'"
        );
    }

    /// Every reference in the command has to carry a shell-style
    /// default, `${NAME:-…}`, and never be a bare `$NAME`.
    ///
    /// This is not style. grok checks every `$` reference in a hook's
    /// command against its environment before it spawns a shell, and
    /// refuses to execute the hook when one is unset — drawing a red
    /// `hook not executed: required env var(s) not set:
    /// ${ROOST_AGENT_HOOK}` row on every tool call in any terminal that
    /// is not Roost. `${NAME:-}` always resolves, so grok runs the
    /// command and the `else` branch answers `{}` quietly. Tidy the
    /// braces away and every non-Roost grok session goes red again.
    #[test]
    fn every_variable_reference_carries_a_default() {
        for agent in [
            Agent::Claude,
            Agent::Grok,
            Agent::Codex,
            Agent::Cursor,
            Agent::Opencode,
        ] {
            let command = installed_command(agent);
            assert!(
                every_reference_has_a_default(&command),
                "{}: {command}",
                agent.source()
            );
        }
        // The check has teeth: both retired spellings fail it, which is
        // exactly why grok went red on them.
        assert!(!every_reference_has_a_default(&command_v1(Agent::Grok)));
        assert!(!every_reference_has_a_default(&command_v2(Agent::Grok)));
    }

    /// Versions 1 and 2 are frozen too. Both are in users' config files
    /// right now, and dropping either from the owned list would leave
    /// those entries unrecognised — not refreshed, not removable, just
    /// there.
    #[test]
    fn the_retired_commands_are_still_owned() {
        let v1 = "sh -c 'if [ -n \"$ROOST_AGENT_HOOK\" ] && \"$ROOST_AGENT_HOOK\" agent-hook claude 2>/dev/null; then :; else cat >/dev/null; printf \"{}\"; fi'";
        let v2 = "sh -c 'if [ -n \"$ROOST_AGENT_HOOK\" ] && out=$(\"$ROOST_AGENT_HOOK\" agent-hook claude 2>/dev/null); then [ -n \"$out\" ] || out=\"{}\"; printf \"%s\" \"$out\"; else cat >/dev/null; printf \"{}\"; fi'";
        assert_eq!(command_v1(Agent::Claude), v1);
        assert_eq!(command_v2(Agent::Claude), v2);
        for retired in [v1, v2] {
            assert!(is_roost_command(Agent::Claude, retired));
            assert_ne!(installed_command(Agent::Claude), retired);
        }
        // Every agent, not just Claude: a retired spelling dropped for
        // one of them orphans that agent's installed entries alone,
        // which is the kind of gap a Claude-only check would miss.
        for agent in [
            Agent::Claude,
            Agent::Grok,
            Agent::Codex,
            Agent::Cursor,
            Agent::Opencode,
        ] {
            assert!(
                is_roost_command(agent, &command_v1(agent)),
                "{}",
                agent.source()
            );
            assert!(
                is_roost_command(agent, &command_v2(agent)),
                "{}",
                agent.source()
            );
            assert!(
                !is_roost_command(agent, &command_v1(Agent::Opencode)) || agent == Agent::Opencode
            );
        }
    }

    #[test]
    fn every_agent_gets_its_own_verb_and_nothing_machine_specific() {
        for agent in [
            Agent::Claude,
            Agent::Grok,
            Agent::Codex,
            Agent::Cursor,
            Agent::Opencode,
        ] {
            let command = installed_command(agent);
            assert!(
                command.contains(&format!("agent-hook {}", agent.source())),
                "{command}"
            );
            // Host-independence is the whole design: the only path in
            // the string is `/dev/null`, so the same bytes work on this
            // machine, the next one, and every host.
            assert!(!command.replace("/dev/null", "").contains('/'), "{command}");
            assert!(!command.contains("roostctl"), "{command}");
            assert!(!command.contains("$HOME"), "{command}");
        }
    }

    #[test]
    fn the_current_command_is_always_an_owned_one() {
        for agent in [
            Agent::Claude,
            Agent::Grok,
            Agent::Codex,
            Agent::Cursor,
            Agent::Opencode,
        ] {
            assert_eq!(
                owned_commands(agent).first().unwrap(),
                &installed_command(agent)
            );
            assert!(is_roost_command(agent, &installed_command(agent)));
        }
    }

    /// The rule the whole uninstall path rests on.
    #[test]
    fn ownership_is_exact_equality_never_a_substring() {
        let ours = installed_command(Agent::Claude);
        // A user's own hook that merely mentions the variable.
        let foreign = "echo \"$ROOST_AGENT_HOOK\" >> ~/hooks.log";
        assert!(!is_roost_command(Agent::Claude, foreign));
        // Ours, plus one byte.
        assert!(!is_roost_command(Agent::Claude, &format!("{ours} ")));
        // Ours, but for a different agent.
        assert!(!is_roost_command(
            Agent::Claude,
            &installed_command(Agent::Codex)
        ));
    }

    #[test]
    fn an_edited_roost_entry_is_recognisable_but_never_owned() {
        let edited = installed_command(Agent::Claude).replace("2>/dev/null", "2>>/tmp/log");
        assert!(!is_roost_command(Agent::Claude, &edited));
        assert!(looks_edited(Agent::Claude, &edited));

        // Neither an untouched Roost entry nor a genuinely foreign one
        // is "edited".
        assert!(!looks_edited(
            Agent::Claude,
            &installed_command(Agent::Claude)
        ));
        assert!(!looks_edited(
            Agent::Claude,
            "echo \"$ROOST_AGENT_HOOK\" >> ~/hooks.log"
        ));
    }

    // ---- the shell contract (plan 046 §3.2) ---------------------------
    //
    // Run under `/bin/sh` — bash on macOS, dash on Linux CI — because
    // the difference between the two has cost this repo real time
    // before. Everything below sticks to POSIX constructs.

    /// How much stdin the child must swallow. Comfortably past a pipe
    /// buffer (64 KiB on both platforms), so a child that does *not*
    /// drain closes the pipe and our write fails with `EPIPE` instead of
    /// the test hanging.
    const STDIN_BYTES: usize = 256 * 1024;

    struct Ran {
        stdout: String,
        code: Option<i32>,
        wrote_all_stdin: bool,
    }

    /// Execute the literal installed command the way an agent does:
    /// hand it to a shell.
    fn run(agent: Agent, hook: Option<&str>, stdin: &[u8]) -> Ran {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(installed_command(agent))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        match hook {
            Some(path) => cmd.env(HOOK_ENV, path),
            None => cmd.env_remove(HOOK_ENV),
        };
        let mut child = cmd.spawn().expect("spawn /bin/sh");

        let mut pipe = child.stdin.take().expect("stdin");
        let payload = stdin.to_vec();
        let writer = std::thread::spawn(move || {
            let ok = pipe.write_all(&payload).is_ok() && pipe.flush().is_ok();
            drop(pipe);
            ok
        });

        let out = child.wait_with_output().expect("wait");
        Ran {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            code: out.status.code(),
            wrote_all_stdin: writer.join().expect("writer thread"),
        }
    }

    fn stub_hook(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("hook.sh");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn with_no_hook_binary_the_command_still_answers_and_drains() {
        let payload = vec![b'x'; STDIN_BYTES];
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nothing-here/roostctl");

        for (label, hook) in [
            ("unset", None),
            ("empty", Some("")),
            ("missing", Some(missing.to_str().unwrap())),
        ] {
            let ran = run(Agent::Claude, hook, &payload);
            assert_eq!(ran.stdout, "{}", "{label}: stdout");
            assert_eq!(ran.code, Some(0), "{label}: exit code");
            assert!(ran.wrote_all_stdin, "{label}: stdin was not drained");
        }
    }

    /// The same unset case under `set -u`, which is a second and
    /// *independent* way to catch a reintroduced bare `$VAR`.
    ///
    /// `every_variable_reference_carries_a_default` reads the string;
    /// this one runs it. A bare reference is invisible in a normal shell
    /// when the variable happens to be unset (the `[ -n "" ]` test just
    /// takes the `else` branch) but aborts a strict one before a byte of
    /// output — so under `-u` the two spellings are trivially
    /// distinguishable, and some users do run their hooks through a
    /// strict shell.
    ///
    /// The outer `sh -c` would swallow the flag — `set -u` is not
    /// inherited by a child shell, and our string spawns one — so the
    /// script inside the quotes is what gets handed to `sh -u`.
    #[test]
    fn the_unset_case_survives_a_strict_shell() {
        let command = installed_command(Agent::Claude);
        let script = command
            .strip_prefix("sh -c '")
            .and_then(|rest| rest.strip_suffix('\''))
            .expect("the command is a single-quoted sh script");
        assert!(
            !script.contains('\''),
            "the script grew a quote; this extraction is no longer exact"
        );

        let out = Command::new("/bin/sh")
            .arg("-u")
            .arg("-c")
            .arg(script)
            .env_remove(HOOK_ENV)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .expect("spawn /bin/sh -u");

        assert_eq!(String::from_utf8_lossy(&out.stdout), "{}");
        assert_eq!(out.status.code(), Some(0), "a bare $VAR aborts under -u");
    }

    #[test]
    fn a_working_hook_binary_gets_the_argv_and_the_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let argv_out = dir.path().join("argv");
        let stdin_out = dir.path().join("stdin");
        let hook = stub_hook(
            dir.path(),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat > '{}'\nprintf '{{}}'\n",
                argv_out.display(),
                stdin_out.display()
            ),
        );

        let payload = br#"{"hook_event_name":"SessionStart","session_id":"s-1"}"#;
        let ran = run(Agent::Codex, Some(hook.to_str().unwrap()), payload);

        assert_eq!(ran.stdout, "{}");
        assert_eq!(ran.code, Some(0));
        assert!(ran.wrote_all_stdin);
        assert_eq!(
            std::fs::read_to_string(&argv_out).unwrap(),
            "agent-hook\ncodex\n"
        );
        assert_eq!(
            std::fs::read_to_string(&stdin_out).unwrap(),
            String::from_utf8_lossy(payload)
        );
    }

    /// The case the `else` branch cannot be allowed to make worse: the
    /// hook writes something and *then* dies. Whatever it managed to
    /// print must not be handed to a decision hook with `{}` stapled to
    /// the end of it.
    #[test]
    fn a_hook_that_prints_then_fails_never_emits_partial_json() {
        let dir = tempfile::tempdir().unwrap();
        let hook = stub_hook(
            dir.path(),
            "#!/bin/sh\ncat >/dev/null\nprintf '{\"decision\":\"blo'\nexit 1\n",
        );
        let ran = run(Agent::Claude, Some(hook.to_str().unwrap()), b"{}");

        assert_eq!(ran.stdout, "{}", "partial output reached the agent");
        assert_eq!(ran.code, Some(0));
    }

    /// And the answer a working hook produces is still the one the agent
    /// sees, byte for byte.
    #[test]
    fn a_working_hooks_own_answer_is_what_reaches_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        let hook = stub_hook(
            dir.path(),
            "#!/bin/sh\ncat >/dev/null\nprintf '{\"decision\":\"approve\"}'\n",
        );
        let ran = run(Agent::Claude, Some(hook.to_str().unwrap()), b"{}");

        assert_eq!(ran.stdout, r#"{"decision":"approve"}"#);
        assert_eq!(ran.code, Some(0));
    }

    /// A hook binary that exists but fails is the moved-bundle case a
    /// decision hook must survive: the `else` branch has to take over.
    #[test]
    fn a_failing_hook_binary_falls_back_to_the_inert_answer() {
        let dir = tempfile::tempdir().unwrap();
        let hook = stub_hook(dir.path(), "#!/bin/sh\necho boom >&2\nexit 127\n");
        let payload = vec![b'y'; STDIN_BYTES];

        let ran = run(Agent::Cursor, Some(hook.to_str().unwrap()), &payload);
        assert_eq!(ran.stdout, "{}");
        assert_eq!(ran.code, Some(0));
        assert!(ran.wrote_all_stdin, "stdin was not drained after a failure");
    }
}
