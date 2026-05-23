# IPC golden vectors

Canonical wire-format exemplars for the JSON IPC protocol defined in
[`docs/reference/ipc.md`](../../docs/reference/ipc.md).

Each file is one JSON object — either a request envelope, a response
envelope, or an event envelope. The naming convention is:

- `<op>.request.json` — request envelope for the op.
- `<op>.response.json` — success response envelope for the op.
- `<op>.error.json` — error response envelope variant.
- `<event-name>.event.json` — server-push event envelope.

Both the Rust side (`cargo test -p roost-ipc`) and the Swift side
(`swift test --package-path mac`, post-M4) load these files and assert
that decode → re-encode produces a byte-equal result. This guards
against schema drift between the two languages.

When you add a new op or event, drop a new vector file here. The
loader is intentionally schema-agnostic — it round-trips raw
`serde_json::Value` / Swift `Any` JSON — so adding a vector doesn't
require touching the test code.

Whitespace policy: vectors should be formatted with two-space
indentation and a trailing newline. The loader normalizes whitespace
before the byte-equal comparison, so this is just for human
readability.
