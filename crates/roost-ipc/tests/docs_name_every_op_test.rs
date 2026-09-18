//! Every `ops::` constant gets a name on the wire's own reference page.
//!
//! `docs/reference/ipc.md` is the contract an external tool reads —
//! `roostctl`, a Claude hook, a future Lua script. An op or event that
//! ships with no entry there is invisible to exactly the audience the
//! page exists for, and nothing in the Rust build catches that: a new
//! `pub const` compiles fine whether or not anyone ever wrote a
//! sentence about it. This test is the catch.
//!
//! Constants are read out of `messages.rs`'s own source text, the same
//! way `local_route.rs`'s `ops_declared_in_the_source` does, rather
//! than trusting a second, hand-maintained list to stay in sync with
//! the first.
//!
//! The doc lives outside this crate's `CARGO_MANIFEST_DIR` and is
//! absent from a packaged build (a source tree with no `docs/`
//! directory) — that case is a loud skip, not a failure.

use std::path::PathBuf;

/// (constant name, wire value) pairs, e.g. `("TAB_OPEN", "tab.open")`.
type NamedConsts = Vec<(String, String)>;

/// `pub const NAME: &str = "value";`, parsed the way
/// `local_route.rs::regex_lite_const_lines` does — no regex crate for
/// four lines of scanning.
fn const_lines(body: &str) -> NamedConsts {
    body.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("pub const ")?;
            let (name, rest) = rest.split_once(": &str = ")?;
            let value = rest.strip_prefix('"')?.strip_suffix("\";")?;
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

/// Every constant in `messages.rs`'s `pub mod ops { … }` block, split
/// into (non-event op, event) pairs by the `EVENT_` name prefix —
/// reliable because every event constant is named that way and no op
/// constant is (checked below).
fn ops_and_events_declared_in_the_source() -> (NamedConsts, NamedConsts) {
    let source = include_str!("../src/messages.rs");
    let start = source
        .find("\npub mod ops {\n")
        .expect("messages.rs declares `pub mod ops`");
    let body = &source[start..];
    let end = body.find("\n}\n").expect("the ops module closes");
    let all = const_lines(&body[..end]);

    // Guards the parser itself: a drift that stopped matching any line
    // (a reformat, a rename of `pub mod ops`) must fail loudly here
    // rather than let both tests below pass vacuously over an empty
    // list.
    assert!(
        all.len() > 70,
        "only {} constants parsed out of `messages.rs`'s ops module - \
         the parser has drifted from the source and would pass \
         vacuously",
        all.len()
    );

    let (events, ops): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|(name, _)| name.starts_with("EVENT_"));
    assert!(
        ops.len() > 50,
        "only {} non-event op constants parsed - parser drift",
        ops.len()
    );
    assert!(
        events.len() > 10,
        "only {} event constants parsed - parser drift",
        events.len()
    );
    (ops, events)
}

/// Ops still undocumented while this test was written, each with the
/// reason it isn't yet. **Must be empty** — an op landing here is a
/// TODO, not a pass.
const EXEMPT_OPS: &[(&str, &str)] = &[];

/// `docs/reference/ipc.md`, or `None` with a loud `eprintln!` when the
/// source tree has no `docs/` directory (a packaged build).
fn read_ipc_md() -> Option<String> {
    read_ipc_md_at(&ipc_md_path())
}

fn ipc_md_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(p.pop()); // pop "roost-ipc"
    assert!(p.pop()); // pop "crates"
    p.push("docs");
    p.push("reference");
    p.push("ipc.md");
    p
}

fn read_ipc_md_at(path: &PathBuf) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(err) => {
            eprintln!(
                "SKIP: docs/reference/ipc.md not found at {} ({err}) - \
                 this looks like a packaged build with no `docs/` \
                 directory, so the op/event naming check is skipped.",
                path.display()
            );
            None
        }
    }
}

/// Every `###` heading's backticked tokens, plus every Markdown table
/// row's backticked first cell — the two shapes `ipc.md` documents an
/// op in (a dedicated section, or a grouped table like the palette /
/// selection+clipboard / host families).
fn documented_op_tokens(doc: &str) -> std::collections::HashSet<String> {
    let mut tokens = std::collections::HashSet::new();
    for line in doc.lines() {
        if let Some(heading) = line.strip_prefix("### ") {
            for tok in backticked(heading) {
                tokens.insert(tok);
            }
        } else if let Some(rest) = line.trim_start().strip_prefix('|') {
            // A table row's first cell: `| \`op.name\` | ... |`.
            let first_cell = rest.split('|').next().unwrap_or("").trim();
            if let Some(tok) = first_cell
                .strip_prefix('`')
                .and_then(|s| s.strip_suffix('`'))
            {
                tokens.insert(tok.to_string());
            }
        }
    }
    tokens
}

fn backticked(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        if let Some(end) = after.find('`') {
            out.push(after[..end].to_string());
            rest = &after[end + 1..];
        } else {
            break;
        }
    }
    out
}

/// The `## Events` section's own text — from its heading to the next
/// `## ` heading (`## Versioning`) or end of file — so an event name
/// that happens to appear in prose elsewhere on the page (an op's
/// description mentioning the event it fires) doesn't count.
fn events_section(doc: &str) -> &str {
    let start = doc
        .find("\n## Events\n")
        .expect("ipc.md has an `## Events` section");
    let body = &doc[start + 1..];
    let end = body[3..].find("\n## ").map(|i| i + 3).unwrap_or(body.len());
    &body[..end]
}

#[test]
fn every_non_event_op_is_named_in_ipc_md() {
    let Some(doc) = read_ipc_md() else {
        return;
    };
    let (ops, _events) = ops_and_events_declared_in_the_source();
    let documented = documented_op_tokens(&doc);
    let exempt: std::collections::HashSet<&str> = EXEMPT_OPS.iter().map(|(op, _)| *op).collect();

    let missing: Vec<String> = ops
        .iter()
        .map(|(_, value)| value.clone())
        .filter(|value| !documented.contains(value) && !exempt.contains(value.as_str()))
        .collect();

    assert!(
        missing.is_empty(),
        "docs/reference/ipc.md has no `###` heading and no grouped-table \
         row naming: {}. Every non-event `ops::` constant needs a \
         contract section (or a row in one of the grouped tables, e.g. \
         palette.*/selection.*+clipboard.*/host.*) - add one, or add the \
         op to EXEMPT_OPS with a reason if it is deliberately not yet \
         documented.",
        missing.join(", ")
    );
}

#[test]
fn every_event_is_named_in_the_events_section() {
    let Some(doc) = read_ipc_md() else {
        return;
    };
    let (_ops, events) = ops_and_events_declared_in_the_source();
    let section = events_section(&doc);

    let missing: Vec<String> = events
        .iter()
        .map(|(_, value)| value.clone())
        .filter(|value| !section.contains(&format!("`{value}`")))
        .collect();

    assert!(
        missing.is_empty(),
        "docs/reference/ipc.md's `## Events` section names none of: {}. \
         Every `EVENT_*` constant needs an entry in that catalog (a \
         bullet or a heading naming it in backticks).",
        missing.join(", ")
    );
}

#[test]
fn no_op_constant_is_named_like_an_event() {
    // `documented_op_tokens` / `events_section` trust the `EVENT_` name
    // prefix to sort constants correctly. If a future op's *value*
    // collided with an event's dotted spelling the two tests above
    // could pass past each other's blind spot; nothing in the wire
    // format forbids it, so this pins the assumption instead.
    let (ops, events) = ops_and_events_declared_in_the_source();
    let event_values: std::collections::HashSet<&str> =
        events.iter().map(|(_, v)| v.as_str()).collect();
    let collisions: Vec<&str> = ops
        .iter()
        .map(|(_, v)| v.as_str())
        .filter(|v| event_values.contains(v))
        .collect();
    assert!(
        collisions.is_empty(),
        "op/event value collision(s), breaks the EVENT_ prefix split: {collisions:?}"
    );
}

#[test]
fn read_ipc_md_skips_loudly_over_a_missing_file() {
    // Confirms the skip path actually returns `None` (and therefore
    // both real tests above would return early) rather than panicking
    // or silently reading something else, for a path that cannot
    // exist. `eprintln!` output isn't captured by an assertion, so
    // this only pins the `None` return; the loud message itself was
    // eyeballed during development by pointing `ipc_md_path()` at a
    // nonexistent path and reading the test's stderr.
    let bogus = PathBuf::from("/nonexistent/does-not-exist/ipc.md");
    assert!(read_ipc_md_at(&bogus).is_none());
}
