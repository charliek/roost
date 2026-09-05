// Roost agent-state plugin for opencode (plan 046 W1).
//
// opencode has no command hooks, so this plugin subscribes to its event
// bus and forwards a whitelist of events to
// `"$ROOST_AGENT_HOOK" agent-hook opencode` as stdin JSON.
//
// It carries NO POLICY. Nothing here decides what an event means, what
// it does to a tab, or whether it should notify: it forwards, and
// `crates/roost-agent/src/opencode.rs` is the single source of truth.
// Keeping the mapping on the Rust side is what makes opencode
// fixture-replayable like the other four agents; adding a decision here
// would fork it into two implementations.
//
// Roost writes this file; `roostctl agent uninstall opencode` removes
// it again.

import { spawn } from "node:child_process";

// Whitelisted so the `message.part.delta` flood — 697 of the 862
// records in the probe — never spawns a process. Must stay in step with
// `OPENCODE_HOOK_EVENTS` in opencode.rs, which has a test that reads
// this literal.
const FORWARDED = new Set([
  "session.created",
  "chat.message",
  "session.status",
  "permission.asked",
  "permission.replied",
  "question.asked",
  "question.replied",
  "session.idle",
  "session.error",
]);

// `roostctl agent-hook` budgets itself 2 s end to end, so a child still
// alive after this is wedged — a `$ROOST_AGENT_HOOK` that reads stdin
// and never exits, say. Unkilled it would leak a process and a pipe for
// every single bus event.
const HOOK_TIMEOUT_MS = 3000;

// How long `dispose` waits for the release to land before letting
// opencode quit. The child's own timeout cleans up either way.
const DISPOSE_WAIT_MS = 2000;

export const RoostAgentState = async () => {
  const hookBinary = process.env.ROOST_AGENT_HOOK;
  const tabId = process.env.ROOST_TAB_ID;
  const socket = process.env.ROOST_SOCKET;

  // Outside a Roost tab there is nothing to report to, and opencode is
  // used plenty of places that are not one. Returning no handlers is
  // cheaper than checking on every bus event.
  if (!hookBinary || !tabId || !socket) return {};

  // The latest root session. A child session's events are reported
  // against it rather than against their own id, so a subagent's
  // permission prompt lands on the session the user is actually looking
  // at. Last root wins: a second root session in the same opencode
  // process takes the tab from the first, matching what the user sees in
  // opencode itself.
  let rootSessionId = null;
  let spawnFailed = false;

  // Forwards run one at a time through this chain. Roost applies
  // reports in arrival order and the wire makes no ordering promise, so
  // two hooks in flight at once can land reversed: `permission.replied`
  // ahead of `permission.asked` leaves the tab stuck `waiting` behind a
  // stale banner, and a `chat.message` ahead of the `session.created`
  // that claims the tab is reported against nobody.
  let queue = Promise.resolve();

  const complain = (err) => {
    // Once per plugin instance: a hook binary that cannot be spawned
    // fails on every single event, and opencode's log is the user's.
    if (spawnFailed) return;
    spawnFailed = true;
    console.error(`roost: cannot run ${hookBinary}: ${err?.message ?? err}`);
  };

  const run = (stdin) =>
    new Promise((resolve) => {
      let child;
      try {
        child = spawn(hookBinary, ["agent-hook", "opencode"], {
          stdio: ["pipe", "ignore", "ignore"],
        });
      } catch (err) {
        complain(err);
        resolve();
        return;
      }
      const cap = setTimeout(() => {
        child.kill("SIGKILL");
        resolve();
      }, HOOK_TIMEOUT_MS);
      // The live child already keeps the event loop alive; this timer
      // must not extend it past the kill.
      cap.unref?.();
      const done = () => {
        clearTimeout(cap);
        resolve();
      };
      child.on("close", done);
      child.on("error", (err) => {
        complain(err);
        done();
      });
      // A broken pipe (the child died before reading) must not take
      // opencode down with an unhandled error event.
      child.stdin.on("error", () => {});
      child.stdin.end(stdin);
    });

  const forward = (name, properties) => {
    // Built now rather than when the queue reaches it: `rootSessionId`
    // moves on, and this event belongs to the root that was current when
    // the bus delivered it. Ours are written last so a bus property can
    // never shadow the event name or the session the report is scoped
    // to.
    const payload = { ...properties, hook_event_name: name };
    if (rootSessionId) payload.session_id = rootSessionId;
    let stdin;
    try {
      stdin = JSON.stringify(payload);
    } catch (err) {
      complain(err);
      return queue;
    }
    // The `catch` is what keeps the chain alive: one rejected link would
    // otherwise poison every forward behind it, silently, for the rest
    // of the process.
    queue = queue.then(() => run(stdin)).catch(() => {});
    return queue;
  };

  const text = (value) =>
    typeof value === "string" && value.length > 0 ? value : null;

  // The session a `session.created` is about, and the root it implies.
  //
  // A child's own id must never become the root: a claim supersedes any
  // live owner unconditionally, so a subagent would evict its own
  // parent. But the *first* creation this plugin sees can already be a
  // child — `opencode attach`, or the plugin loading mid-session — and
  // then its `parentID` is the only root there is. Discarding it would
  // leave every later event scoped to the child, which the adapter
  // reports against an id the tab's owner can never match.
  const created = (properties) => {
    const info = properties?.info;
    const id = text(properties?.sessionID) ?? text(info?.id);
    if (!id) return null;
    const parent = text(properties?.parentID) ?? text(info?.parentID);
    if (parent) return { id, root: rootSessionId ? null : parent };
    return { id, root: id };
  };

  return {
    event: async ({ event }) => {
      const name = event?.type;
      if (!name || !FORWARDED.has(name)) return;
      const properties = event?.properties ?? {};
      if (name === "session.created") {
        const session = created(properties);
        // Forwarding a creation whose own session cannot be read would
        // stamp the *previous* root onto it, and the adapter reads that
        // as an unconditional claim — stopping a live turn, or evicting
        // a newer owner with a stale id.
        if (!session) return;
        if (session.root) rootSessionId = session.root;
      }
      // Deliberately not awaited: the forwards are ordered among
      // themselves, but opencode's bus never waits on Roost.
      forward(name, properties);
    },

    // Declared in opencode's plugin `Hooks` type but never observed in
    // the probe. If opencode exits without calling it, ownership
    // survives as a label until Roost's OSC 133 failsafe drops the
    // lifecycle at the next shell prompt — the same way a killed Claude
    // degrades.
    dispose: async () => {
      // Unlike every other event, this one races opencode's own exit:
      // waiting is what makes the release actually land. Capped, because
      // neither a wedged hook binary nor a backlog of queued forwards
      // may hold up quit.
      await Promise.race([
        forward("dispose", {}),
        new Promise((resolve) => {
          const cap = setTimeout(resolve, DISPOSE_WAIT_MS);
          cap.unref?.();
        }),
      ]);
    },
  };
};
