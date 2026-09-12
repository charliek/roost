# IPC compatibility & consumption

[`ipc.md`](ipc.md) is the protocol reference — what the ops are and what
they answer. This page is the *policy* around it: how an external
project consumes `crates/roost-ipc`, what it may rely on staying put,
which direction of skew each rule protects, and what has to happen when
a change genuinely breaks the wire.

The audience is anyone building against roost from outside this
repository — the first such consumer is shed — and anyone inside it
about to change a message shape.

## Consuming the crate

`roost-ipc` is a **git dependency on `main`**. It is not on crates.io
and there are no release tags for it yet:

```toml
[dependencies]
roost-ipc = { git = "https://github.com/charliek/roost", branch = "main" }
```

Cargo records the exact commit in the consumer's `Cargo.lock`, and that
lock entry — not the branch name — is the pin. Consumers move
deliberately with `cargo update -p roost-ipc`; nothing about a merge to
`main` reaches a consumer that has not updated its lockfile. A consumer
that wants the pin visible in the manifest rather than only in the lock
can spell it `rev = "<sha>"` instead of `branch = "main"`.

`publish = false` on the workspace ([`Cargo.toml`](https://github.com/charliek/roost/blob/main/Cargo.toml))
is deliberate, not an oversight. `roost-ipc` inherits the application's
version number, which moves for reasons that have nothing to do with the
wire; on crates.io that version would be read as a semver promise the
protocol is not yet ready to make, `0.0.x` caret semantics would treat
every bump as breaking anyway, and the golden vectors — which live at the
workspace root, outside the crate directory — would not travel through
`cargo package`. Publishing is also irreversible in a way a branch pin is
not.

Two triggers revisit that decision:

* the wire settles — R1 (the lease re-cut) through R5, and R15 (plan
  057, the lease re-cut *again*, opening `tab.write`/`tab.attach` back
  up) are the changes currently expected to move it; once they have
  landed and the shape has stopped moving, git tags per protocol
  generation are the natural next step, and cheap;
* a consumer appears outside charliek's repositories, for whom "pin a
  commit of someone else's application repo" is a worse contract than a
  published crate.

Tags cost only tag ceremony during rapid change, which is why they are
deferred rather than rejected.

[`examples/ipc-consumer`](https://github.com/charliek/roost/tree/main/examples/ipc-consumer)
is this section's compilable companion: a standalone crate, outside the
root workspace, that depends on `roost-ipc` the way an external project
would (its own `Cargo.toml`/`Cargo.lock` show the path-vs-git-dependency
split above in context). It dials a UI socket, calls `identify`, and
polls `tab.list` — leaseless ops only, so it stays correct across R1's
lease work — and CI both builds it and asserts, via `cargo metadata` on
its resolved graph, that `roost-ipc` is the only workspace crate it
pulls in.

### Module tour

| Module | What it holds |
|---|---|
| [`messages`](https://github.com/charliek/roost/blob/main/crates/roost-ipc/src/messages.rs) | Every request/response/event type for both socket families, the `ops::*` op-name constants, and `SESSION_PROTOCOL_VERSION` |
| [`framing`](https://github.com/charliek/roost/blob/main/crates/roost-ipc/src/framing.rs) | Newline-delimited JSON framing and the 16 MiB frame cap |
| [`client`](https://github.com/charliek/roost/blob/main/crates/roost-ipc/src/client.rs) | `IpcClient`, the event stream, and the typed server error codes |
| `server` (re-exported from the crate root) | `IpcServer`, `Handler`, the connection context and closers |
| [`dataframe`](https://github.com/charliek/roost/blob/main/crates/roost-ipc/src/dataframe.rs) | The binary attach data plane's frame codec |
| [`paths`](https://github.com/charliek/roost/blob/main/crates/roost-ipc/src/paths.rs) / [`target`](https://github.com/charliek/roost/blob/main/crates/roost-ipc/src/target.rs) / [`socket_state`](https://github.com/charliek/roost/blob/main/crates/roost-ipc/src/socket_state.rs) | Socket path resolution per profile, the target picker, liveness probing |
| [`ssh`](https://github.com/charliek/roost/blob/main/crates/roost-ipc/src/ssh.rs) / [`bootstrap`](https://github.com/charliek/roost/blob/main/crates/roost-ipc/src/bootstrap.rs) / [`session_launch`](https://github.com/charliek/roost/blob/main/crates/roost-ipc/src/session_launch.rs) | Reaching a session over SSH, installing `roost-session` on a host, starting one locally |
| [`agent`](https://github.com/charliek/roost/blob/main/crates/roost-ipc/src/agent.rs) | Agent-report helpers shared by the hook adapters |

The crate has **no workspace-internal dependencies** — its whole
dependency set is serde, serde_json, base64, tokio, anyhow, thiserror,
sha2, tracing, and libc. That is a property worth preserving: it is what
makes the crate consumable at all. There is no feature gating today
(`ssh` and `bootstrap` come along whether or not a consumer wants them);
splitting them behind features is a crates.io-era question, not a
git-dependency one.

## Stability domains

"Compatible" means different things for different surfaces, so they get
separate promises. A change may be additive in one domain and breaking in
another — the Rust API is the usual offender.

| Domain | Promise |
|---|---|
| **UI-socket JSON wire** | Governed by `PROTOCOL_VERSION` and the matrix below. Additive changes are free; a breaking change bumps the integer |
| **Session-socket JSON wire** | Governed by `SESSION_PROTOCOL_VERSION` and the same matrix. Conforming clients gate on it exactly, so mixed generations fail closed at the handshake rather than misbehaving later |
| **Binary data plane** (attach streams; see [Data plane](ipc.md#data-plane)) | Rides the session generation. The attach handshake carries the same integer and refuses a mismatch before it looks at the token; snapshot payloads additionally require an exact `libghostty_build` match |
| **Fixture corpus** (`tests/ipc-vectors/`) | Pinnable and append-mostly — see [Fixtures are the contract](#fixtures-are-the-contract) |
| **Rust crate API** | **No stability promise.** See below |

### There is no Rust-API stability promise

While the crate is unpublished, its Rust surface may change in any
release-to-`main` without ceremony. This is not the same axis as the
wire, and the distinction bites in a specific way: adding a variant to
`EventFrame`, or a field to a public struct, is **source-breaking Rust**
for a downstream `match` or struct literal even when the corresponding
wire change is purely additive and every JSON client is unaffected.

Consumers absorb that by pinning a rev and upgrading deliberately — the
git-dependency model above is what makes it tolerable. If API stability
is ever promised (a crates.io publish would force the question),
`#[non_exhaustive]` on the wire enums and structs is the tool that buys
it, at the cost of making exhaustive matching impossible downstream.
It is deliberately not applied today.

## The compatibility matrix

Request structs in `roost-ipc` carry `#[serde(deny_unknown_fields)]` —
the strict-server half of the policy in
[Wire format](ipc.md#wire-format). That single choice is what makes
compatibility **directional**: a server rejects a request key it does not
know, while a client quietly ignores a response key it does not know. The
four directions therefore have four different requirements, and a change
is only "additive" if it satisfies all four.

| Direction | Requirement |
|---|---|
| old client → new server | New request fields carry `#[serde(default)]`, so a request that omits them still decodes |
| new client → old server | New request keys are **omitted unless explicitly engaged** (`#[serde(skip_serializing_if = "Option::is_none")]`). An old server answers `unknown-field` to a key it does not know and drops the whole request, so a client that is not using the feature must not mention it |
| new server → old client | Clients ignore unknown response fields and **skip unknown envelope names** on an event stream, so a new event kind is invisible to an old client rather than fatal |
| old server → new client | New response and event fields must **decode when absent**, via client-side defaults — the pattern `Tab`'s optional fields already follow. New event *behavior* must likewise be optional: a client that never receives the new envelope has to remain correct |

The second row is the one that surprises people. Adding an optional
request field is *not* free unless it is omitted from the serialized
form when unset — a field that always serializes, even as `null`, makes
every new client incompatible with every old server. The sentinel need
not be `Option`: `tab.dump`'s `scrollback` (plan 053) is a `u32` skipped
when it is `0`, so an unset request stays byte-identical to what
pre-053 clients always sent, and only a client that actually asks for
history is refused by a server that predates the key.

### Closed enums

Some string enums have no fallback case on either side of the wire —
notably the Swift decoders, which are closed `String` enums that throw a
`DecodingError` on an unrecognized value. **Adding a value to any of
these is a breaking protocol change and requires a generation bump**, not
merely a new vector:

* `tab.state` — pinned as a closed four-value enum; see
  [the compatibility contract](ipc.md#tabstate-hook_active-derived-and-the-compatibility-contract)
  for why `agent_lifecycle: "failed"` projects onto an existing value
  instead of adding a fifth;
* `TabEffect` and `ClipboardEffectTarget` — the
  [`tab.effect`](ipc.md#events) envelope's kind and target;
* `AttachMode` — the [`tab.attach`](ipc.md#tabattach) mode;
* the agent enums — `AgentLifecycle` and the `ownership_action`,
  `attention`, and `severity` values on
  [`tab.agent_report`](ipc.md#tabagent_report).

### The no-bump extension channels

Two shapes exist precisely so that a consumer can extend them without
touching the protocol integer:

* **the `metadata` map on `tab.agent_report`** — an open
  `map<string, string>` and the one field a client may extend without
  coordinating with the server. Anything an adapter can express as data
  belongs here; a new *named* field on that op is a request-schema
  change, because the params struct is `deny_unknown_fields`;
* **open string lists** — `payload_kinds` on
  [`session.identify`](ipc.md#sessionidentify) is the model: a list, not
  an enum, where a client preserves values it does not recognize and
  negotiates on the ones it does. `source` on `tab.agent_report` is an
  open string for the same reason.

When a change can be expressed through one of these channels, it should
be. A protocol generation is expensive; a map key is not.

## Capability negotiation over version sniffing

A client should ask what a peer *can do*, not infer it from a version
number — with one exception that comes first.

**The generation gate is exact and it runs first.** A session client
compares `session_protocol` from
[`session.identify`](ipc.md#sessionidentify) against its own constant for
equality and refuses to proceed on a mismatch. Note honestly what that
is: a **client-side** guarantee of the shipped connection sequence. The
JSON server does not require `session.identify` before anything else, so
a client that skips the handshake is not stopped — it simply gets
undefined behavior it asked for. The attach handshake on the data plane
carries the same integer and does enforce it server-side, before it looks
at the token.

**Everything optional is capability-detected.** `payload_kinds` is the
worked example: the client reads the list, keeps the entries it does not
recognize, and picks from the intersection with its own. Nothing about
that logic mentions a version number, which is why adding a payload kind
costs nothing — plan 053 spent that budget for real, adding the `vt`
kind and its whole fallback path with no protocol bump.

**There is no intra-generation capability channel today.** `session.identify`
carried a `features` list — an open string channel for an additive
session capability that did not warrant a whole generation bump —
through protocol `4`. It answered a real question (has this build also
grown one more optional thing, short of "can it speak my generation at
all") and is not gone because the question stopped mattering; it is
gone because `4` → `5` retired it along with the lease it mostly
existed to soften, and no second real consumer has needed the channel
back since. See the `4` → `5` entry under [Version bumps](#version-bumps)
for what moved. If a capability needs feature-detecting again before
the next generation exists to carry it, that channel is the thing to
resurrect — not something to route around with version sniffing.

**Absence of a mandatory capability is a legitimate refusal.** Capability
detection governs optional features; it does not mean every negotiation
is soft. A client that can only render `ghostty-snapshot` and finds no
compatible offer — an unavailable payload kind, or a `libghostty_build`
that does not match exactly — should refuse the host connection by name
(`unsupported-kind`, `build-mismatch`) rather than connecting to a
session it cannot draw. Failing at the handshake with a nameable reason
beats failing later with a corrupt screen.

## Version bumps

Two integers, described in [ipc.md's Versioning
section](ipc.md#versioning), which is where their current values live:

* `roost_ipc::PROTOCOL_VERSION` (`crates/roost-ipc/src/lib.rs`) — the
  UI-socket wire. Reported by [`identify`](ipc.md#identify) and, today,
  **nothing compares it**: the UI socket has no handshake gate. It is
  documentation of intent that a client may read, not an enforced
  contract. Treat it as informative when reasoning about what actually
  protects a UI-socket consumer — the answer is the matrix above, not the
  integer.
* `roost_ipc::messages::SESSION_PROTOCOL_VERSION`
  (`crates/roost-ipc/src/messages.rs`) — the session-socket JSON wire and
  the binary data plane. Equality-checked by conforming clients at
  session launch, in the SSH and bootstrap identity gates, in the iced
  host connection, and in the attach handshake.

**The rule, in main-consumption form:** bump the affected integer **once
per mutually-incompatible wire generation merged to `main`**. The unit is
the generation, not the commit and not the pull request — several
breaking edits share a single bump only when they land atomically in the
same merge, because that is the only case in which no consumer can ever
observe the intermediate state. Two breaking changes that merge
separately are two generations and two bumps, even if they ship in the
same week, because a consumer's lockfile can sit on the commit between
them.

A bump is not just an integer. Every mirror of it moves in the same
commit: the Rust constant and its doc comment, the Swift test constant,
`tools/roosttest/dataplane.py`, the iced host-connection check, the value
quoted in `ipc.md`, and the versioned fixtures below.

Additive changes — new optional fields that satisfy all four directions
of the matrix, new ops, new events, new values in an open list — do not
bump anything, with one deliberate exception, now **re-qualified twice**
since it was first written:

**An additive *session-socket* op bumps the session integer when a
pre-bump peer could not refuse it meaningfully — and only as a
fallback, now that `features` exists.** Two qualifiers, both learned
after the fact. First, *session-socket only*: a UI-socket op like
`tab.send_file` correctly does not move `PROTOCOL_VERSION` — that wire
has no handshake gate for a bump to protect, so the exception never
applied there in the first place; `tab.send_file` shipped as a pure
addition, no bump, exactly as the matrix predicts. Second, *fallback,
not first resort*: [`session.identify.features`](#capability-negotiation-over-version-sniffing)
(plan 049, R1) is now the preferred channel for "this build also does
one more optional thing" — a client feature-detects an entry instead of
a whole generation being spent on one op.

Plan 047's `session.put_file` is the case that set the rule in the
first place (documented on `SESSION_PROTOCOL_VERSION` and in [ipc.md's
`session.identify`](ipc.md#sessionidentify)): a pre-047 session can only
answer `unknown-op` to a file the user just pasted, which is not a
refusal a client can act on per paste, so `2` → `3` moved rather than
carrying a per-paste special case forever. **That bump stands as
history** — `features` did not exist yet, so it was the only channel
available at the time, and nothing about the re-qualification unwinds
it. Plan 049 (R1) itself bumped `3` → `4`, but not under this exception:
R1's leaseless-classified `events.subscribe` and lease-gated
`tab.write` are genuinely breaking in both directions (an old peer's
request is refused or misunderstood, not merely a feature it lacks),
which is the ordinary rule above, not the additive-op fallback. A new
*event* a client can ignore stays additive.

**`4` → `5` retired the lease, and it is the ordinary rule again, in
both directions at once.** `session.connect` is gone (a pre-bump client
gets `unknown-op`), so is the `lease` field on every op that carried one
(`unknown-field`, because each of those params is `deny_unknown_fields`),
so is `session.driver_changed` and the `connect-required` /
`taken-over` / `already-connected` / `superseded` codes, and
`events.subscribe`'s ack gained a **required** `session_id` a pre-bump
session does not send. `session.identify.features` went with them: at
`5` the integer is the whole negotiation, and a channel for
intra-generation capabilities comes back when there is a second real
consumer for one. No shim was left behind in either direction — that is
the point of a generation.

## Fixtures are the contract

[`tests/ipc-vectors/`](https://github.com/charliek/roost/blob/main/tests/ipc-vectors/README.md)
holds a canonical JSON exemplar per op and event, pinned in three
layers. Every vector is schema-agnostically round-tripped through
`serde_json::Value` (`crates/roost-ipc/tests/vectors.rs`), which catches
a vector that stops parsing. Selected vectors are decoded into the typed
structs on both sides — Rust (`roundtrip.rs`, `wire_types_test.rs`) and
Swift (`IPCSessionTypesTests.swift`) — which is what pins field names
and shapes across two independent implementations of the same wire.
And for the session types, `wire_types_test.rs` additionally asserts
serialization byte-exactly against in-file literal exemplars. No test
byte-compares the on-disk files themselves: JSON key order and
whitespace are not wire-meaningful, and the corpus README's whitespace
rules exist for diff hygiene, not compatibility.

For a consumer, the corpus is the most useful thing in this repository:
it is the executable form of the protocol, and it can be vendored or
pinned and replayed against an implementation without linking to
`roost-ipc` at all.

Three rules govern it.

**An existing vector is never semantically edited to bless a wire
change.** Reformatting is fine; changing a value, adding a key, or
removing one is not. The point of a golden file is that it records what
the wire looked like at a moment; editing it in place erases exactly the
evidence a compatibility question needs.

*Except at a breaking generation bump.* A `SESSION_PROTOCOL_VERSION`
bump may retire request and event vectors for ops that no longer exist
and re-cut the ones whose shape changed, in the same commit as the bump.
The corpus is the **current** contract, for consumers pinning `roost-ipc`
by rev; carrying a generation-qualified copy of every retired shape would
turn it into an archive nobody replays. The evidence is not lost: the
prior generation's shapes survive at the previous tag, and its identify
response survives on disk as the frozen `session.identify.response.v<N>.json`
the rule below versions. `4` → `5` is the worked example — it deleted the
four `session.connect` / `session.driver_changed` vectors, dropped the
retired `lease` key from ten, and re-cut `events.subscribe.response.json`
for the ack's new `session_id`.

**Additive changes add vectors.** A new op, a new event, or an
interesting new optional-field combination gets its own file. The loader
is schema-agnostic — it round-trips raw JSON — so a new vector needs no
test-code change.

**Generation-bearing vectors are versioned.** A vector whose content
embeds a protocol integer (`session.identify`'s response is the one that
does today) is named for the generation it carries:

```text
session.identify.response.v<N>.json
```

Tests do not hardcode `<N>`. They build the filename from the constant —
the Rust side from `SESSION_PROTOCOL_VERSION`, the Swift side from its
mirror — so a bump that forgets to add the new generation's vector fails
loudly instead of silently testing the old shape. That is the
**current-generation lookup rule**. Older `v<N>` files stay where they
are and keep being exercised by decode-only tests, which is how "an old
client's view still parses" stops being a claim and becomes a test.

The set was backfilled at the `2` → `3` bump (plan 047):
`session.identify.response.v2.json` is the unversioned file that existed
before, renamed and carrying its original content, `v3.json` is the
current generation, and both suites build the name from their constant
(`wire_types_test.rs`'s `identify_vector_name`,
`IPCSessionTypesTests.swift`'s `testSessionIdentifyVectorDecodes`). The
Rust suite additionally decodes every prior generation, from `2` up to
the current one, so a retired generation's vector cannot quietly stop
parsing.

### What enforces this

Stated honestly, because overstating it would be worse than the gap:

* **review policy** — the "never semantically edit a vector" rule is
  enforced by whoever reads the diff. A changed vector in a pull request
  is a question that has to be answered;
* **named-vector decode tests** — the Rust and Swift suites reference
  specific vectors by name, so deleting or renaming one breaks a build.
  This is what makes individual files effectively frozen;
* **the emptied-directory guard** — the loader fails if the vector
  directory comes back empty, catching the path-resolution failure that
  would otherwise turn the whole corpus into a silent no-op.

There is deliberately **no hash manifest**. At this corpus size it would
be ceremony: a second file to update on every legitimate addition,
guarding against a failure mode (a vector edited without anyone noticing)
that a diff already surfaces. Revisit it if the corpus grows past the
point where a reviewer can hold it in their head.
