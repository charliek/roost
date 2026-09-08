# IPC golden vectors

Canonical wire-format exemplars for the JSON IPC protocol defined in
[`docs/reference/ipc.md`](../../docs/reference/ipc.md).

This corpus is the compatibility contract: an existing vector is never
semantically edited to bless a wire change, and additive changes add new
vectors instead. The policy — what "additive" means, the four-direction
compatibility matrix, and what enforces this — lives in
[`docs/reference/ipc-compatibility.md`](../../docs/reference/ipc-compatibility.md).

Each file is one JSON object — either a request envelope, a response
envelope, or an event envelope. The naming convention is:

- `<op>.request.json` — request envelope for the op.
- `<op>.response.json` — success response envelope for the op.
- `<op>.error.json` — error response envelope variant.
- `<op>.<variant>.request.json` — a second exemplar of the same op,
  when one shape cannot carry both (an optional field set vs. omitted,
  a mode the flat form can't show). Additive: the plain vector stays.
- `<op>.<variant>.response.json` — the answering half, when a variant
  request is answered in a shape the plain response can't show (a
  negotiated result that differs from the default). Additive too.
- `<event-name>.event.json` — server-push event envelope.
- `<op>.response.v<N>.json` — a response that embeds a protocol
  integer, one file per generation it has carried (`session.identify`
  today). Tests build `<N>` from the constant; older generations stay.

Both the Rust side (`cargo test -p roost-ipc`) and the Swift side
(`swift test --package-path mac`, post-M4) load these files: every
vector is round-tripped schema-agnostically, and selected vectors are
decoded into the typed structs on each side, which pins field names and
shapes and guards against schema drift between the two languages. (The
byte-exact assertions live in `wire_types_test.rs` against in-file
literal exemplars; the on-disk files are compared semantically, not
byte-for-byte.)

When you add a new op or event, drop a new vector file here. The
loader is intentionally schema-agnostic — it round-trips raw
`serde_json::Value` / Swift `Any` JSON — so adding a vector doesn't
require touching the test code.

Whitespace policy: vectors should be formatted with two-space
indentation and a trailing newline. The loader normalizes whitespace
before the byte-equal comparison, so this is just for human
readability.
