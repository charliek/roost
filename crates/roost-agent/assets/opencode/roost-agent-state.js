// Roost agent-state plugin for opencode (plan 046 W1; the loopback
// listener is plan 054 R10).
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
// It does one thing besides forwarding. A bare `opencode` binds no
// socket at all — the TUI talks to its server in-process over a worker
// channel — so nothing outside that process can drive the session Roost
// reports. The plugin runs *inside* the server process and is handed a
// client whose `fetch` is the in-process app, so it puts a `Bun.serve`
// on `127.0.0.1:0` in front of that fetch and announces the address as
// `server_url` on every forward. What listens is opencode's own server,
// exposed by opencode's own plugin, on loopback only — Roost's sockets
// are untouched — but it does mean a bare `opencode` in a Roost tab is
// reachable by any same-host process, unauthenticated unless
// `OPENCODE_SERVER_PASSWORD` is set. `ROOST_OPENCODE_NO_SERVER=1` opts
// out; `docs/guides/agents.md` states the posture.
//
// Roost writes this file; `roostctl agent uninstall opencode` removes
// it again.

import { Buffer } from "node:buffer";
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

const text = (value) =>
  typeof value === "string" && value.length > 0 ? value : null;

// The address to announce for a server this plugin did not start.
//
// opencode hands `input.serverUrl` as a string in some paths and as a
// `URL` in others, and a `URL` always spells the empty path as `/`:
// `String(new URL("http://127.0.0.1:4096"))` is
// `"http://127.0.0.1:4096/"`. Roost's validator wants the bare origin,
// and a consumer concatenates `/session` onto whatever it is given, so
// the single canonical shape is settled here rather than by loosening a
// validator shared with `gx.remote` — otherwise a perfectly reachable
// external server is dropped and its session degrades to status-only.
const announced = (value) => {
  const raw = text(typeof value === "string" ? value : String(value ?? ""));
  if (!raw || !raw.endsWith("/")) return raw;
  const trimmed = raw.slice(0, -1);
  try {
    // Only a *root* slash is noise; `…/api/` is a path, left alone.
    return new URL(trimmed).pathname === "/" ? trimmed : raw;
  } catch {
    return raw;
  }
};

// What an `Authorization: Basic` header offers, decoded — or `null` for
// a header that is absent, not Basic, or not a credential pair at all.
//
// The comparison is made on the decoded credentials rather than by
// re-encoding the expected ones, because `btoa` throws
// `InvalidCharacterError` on any codepoint outside Latin-1: with a
// password like `秘密` an encoding check would throw on *every* request,
// including the correctly authenticated ones.
const offeredCredentials = (header) => {
  if (typeof header !== "string") return null;
  const space = header.indexOf(" ");
  if (space === -1) return null;
  if (header.slice(0, space).toLowerCase() !== "basic") return null;
  let decoded;
  try {
    decoded = Buffer.from(header.slice(space + 1).trim(), "base64").toString("utf8");
  } catch {
    return null;
  }
  // First colon only: a username cannot contain one, a password can.
  const colon = decoded.indexOf(":");
  if (colon === -1) return null;
  return { user: decoded.slice(0, colon), password: decoded.slice(colon + 1) };
};

// opencode gates its own server on Basic auth whenever
// `OPENCODE_SERVER_PASSWORD` is set, so a front end that did not would
// be a hole punched in it. Read per request rather than once, because
// the credentials are the environment's answer, not the listener's.
//
// The falsy test is opencode's, not a shortcut: `server/auth.ts`'s
// `required(config)` is `Option.isSome(config.password) &&
// config.password.value !== ""`, and `serve`/`web` print "server is
// unsecured" on a falsy `Flag.OPENCODE_SERVER_PASSWORD`. An empty
// password means "no auth" there, so demanding credentials here that
// opencode itself does not would refuse a config opencode accepts.
const unauthorized = (request) => {
  const password = process.env.OPENCODE_SERVER_PASSWORD;
  if (!password) return null;
  const user = process.env.OPENCODE_SERVER_USERNAME ?? "opencode";
  const offered = offeredCredentials(request.headers.get("authorization"));
  if (offered && offered.user === user && offered.password === password) {
    return null;
  }
  return new Response("Unauthorized", { status: 401 });
};

// One request, rebuilt against the in-process app.
//
// Everything the caller sent rides through untouched but the directory:
// opencode routes a request by `?directory=` or `x-opencode-directory`
// and otherwise falls back to the server's own cwd, which is whichever
// directory the TUI was started in and not necessarily the one this
// plugin instance belongs to. An unrouted request is therefore pinned to
// the latter.
const proxy = async (cfg, input, request) => {
  const denied = unauthorized(request);
  if (denied) return denied;

  const incoming = new URL(request.url);
  const target = new URL(cfg.baseUrl);
  target.pathname = `${target.pathname.replace(/\/$/, "")}${incoming.pathname}`;
  target.search = incoming.search;

  const headers = new Headers(request.headers);
  const directory = text(input?.directory);
  if (
    directory &&
    !headers.has("x-opencode-directory") &&
    !incoming.searchParams.has("directory")
  ) {
    headers.set("x-opencode-directory", directory);
  }

  const body =
    request.method === "GET" || request.method === "HEAD" ? undefined : request.body;
  return cfg.fetch(
    new Request(target, { method: request.method, headers, body, duplex: "half" }),
  );
};

export const RoostAgentState = async (input) => {
  const hookBinary = process.env.ROOST_AGENT_HOOK;
  const tabId = process.env.ROOST_TAB_ID;
  const socket = process.env.ROOST_SOCKET;

  // Outside a Roost tab there is nothing to report to, and opencode is
  // used plenty of places that are not one. Returning no handlers is
  // cheaper than checking on every bus event.
  if (!hookBinary || !tabId || !socket) return {};

  // The loopback front end, kept so `dispose` can close it.
  let server = null;
  // Its own flag rather than `spawnFailed`'s: a listener that never came
  // up must not spend the single complaint a broken hook binary gets.
  let listenerFailed = false;

  // Whether this run already listens on a socket of its own is read off
  // the client opencode hands over, not re-derived from `process.argv`:
  // `plugin/index.ts` builds `baseUrl` from `Server.url` and supplies a
  // `fetch` only in the `else`, so a client `fetch` and a real listener
  // are alternatives by construction. Its presence therefore *is*
  // "the server is in-process", straight from opencode, with no CLI
  // semantics to mirror and drift from.
  //
  // External is tested before the opt-out deliberately: `NO_SERVER`
  // promises that Roost stands up no server, and in the external case it
  // doesn't — the address reported is the listener the user asked for.
  const exposeServer = async (input) => {
    try {
      const cfg = input?.client?._client?.getConfig?.();
      // Already listening. `input.serverUrl` is the real `Server.url`
      // here; the hardcoded `http://localhost:4096` its getter falls
      // back to belongs to the in-process branch, which never reads it.
      if (typeof cfg?.fetch !== "function") return announced(input?.serverUrl);
      if (process.env.ROOST_OPENCODE_NO_SERVER) return null;
      // A runtime that cannot serve is not a failure; it just means this
      // session is status-only.
      if (typeof Bun === "undefined" || typeof Bun.serve !== "function") return null;
      server = Bun.serve({
        hostname: "127.0.0.1",
        port: 0,
        // `/event` is a long-lived SSE stream that is idle by design
        // between bus events; the default idle timeout would cut it.
        idleTimeout: 0,
        fetch: (request) => proxy(cfg, input, request),
      });
      return `http://127.0.0.1:${server.port}`;
    } catch (err) {
      if (!listenerFailed) {
        listenerFailed = true;
        console.error(`roost: cannot expose opencode's server: ${err?.message ?? err}`);
      }
      return null;
    }
  };

  const serverUrl = await exposeServer(input);

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
    // never shadow the event name, the session the report is scoped to,
    // or the address the session can be driven at.
    const payload = { ...properties, hook_event_name: name };
    if (rootSessionId) payload.session_id = rootSessionId;
    if (serverUrl) payload.server_url = serverUrl;
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
      // Closed before the release is awaited below: a front end still
      // accepting connections would hold opencode's exit open past the
      // quit that wait is already racing.
      server?.stop(true);
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
