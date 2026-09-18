//! `docs/reference/ipc.md` lives outside this crate's `CARGO_MANIFEST_DIR`
//! and is absent from a packaged build — that case is a loud skip, not a
//! failure.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use roost_ipc::codes;

/// A code's spelling where it is not the wire's code, as (file, literal,
/// why). An entry that no longer matches anything fails the scan, so the
/// list cannot outlive its reason.
const NOT_A_WIRE_CODE: &[(&str, &str, &str)] = &[(
    "crates/roost-agent/src/opencode.rs",
    "busy",
    "OpenCode's own `session.status` type, read off its event payload",
)];

fn repo_root() -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(root.pop()); // roost-ipc
    assert!(root.pop()); // crates
    root
}

fn read_ipc_md() -> Option<String> {
    let path = repo_root().join("docs/reference/ipc.md");
    match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(err) => {
            eprintln!(
                "SKIP: docs/reference/ipc.md not found at {} ({err}) - this looks like a \
                 packaged build with no `docs/` directory, so the error-code catalogue \
                 check is skipped.",
                path.display()
            );
            None
        }
    }
}

fn spans(text: &str) -> impl Iterator<Item = &str> {
    text.split('`').skip(1).step_by(2)
}

/// The `**Errors:**` bullet's "Current set:" list, which ends at the first
/// span followed by a full stop.
fn catalogue(doc: &str) -> Vec<String> {
    let bullet = &doc[doc
        .find("* **Errors:**")
        .expect("ipc.md has an **Errors:** bullet")..];
    let list = &bullet[bullet
        .find("Current set:")
        .expect("the **Errors:** bullet has a `Current set:`")..];
    let end = list
        .find("`.")
        .expect("the current set ends with a full stop")
        + 1;
    spans(&list[..end]).map(str::to_string).collect()
}

/// The first cell of every `| Code | Meaning |` table: the attach
/// handshake's rejections and the data plane's `ERROR` frames.
fn code_tables(doc: &str) -> (usize, Vec<String>) {
    let mut tables = 0;
    let mut codes = Vec::new();
    let mut in_table = false;
    for line in doc.lines().map(str::trim) {
        if line == "| Code | Meaning |" {
            tables += 1;
            in_table = true;
        } else if !line.starts_with('|') {
            in_table = false;
        } else if in_table {
            let cell = line[1..].split('|').next().unwrap_or_default().trim();
            if let Some(code) = cell.strip_prefix('`').and_then(|c| c.strip_suffix('`')) {
                codes.push(code.to_string());
            }
        }
    }
    (tables, codes)
}

/// `events.subscribe`'s resume refusals: the ``* **`code`**`` bullets after
/// the sentence that introduces them.
fn resume_refusals(doc: &str) -> Vec<String> {
    let start = doc
        .find("### `events.subscribe`")
        .expect("ipc.md has an events.subscribe section");
    let section = &doc[start..];
    let section = &section[..section[1..]
        .find("\n### ")
        .map_or(section.len(), |end| end + 1)];
    let refusals = &section[section
        .find("refusals answer on the ack")
        .expect("events.subscribe lists the refusals its ack answers")..];
    refusals
        .lines()
        .filter_map(|line| line.strip_prefix("* **`")?.split_once("`**"))
        .map(|(code, _)| code.to_string())
        .collect()
}

#[test]
fn every_code_is_catalogued_and_every_catalogued_code_is_a_constant() {
    let Some(doc) = read_ipc_md() else { return };

    let catalogue = catalogue(&doc);
    assert!(
        catalogue.len() > 10,
        "only {} codes parsed out of the **Errors:** bullet: {catalogue:?}",
        catalogue.len()
    );
    let (tables, tabled) = code_tables(&doc);
    assert!(
        tables >= 2 && tabled.len() > 10,
        "{tables} `| Code | Meaning |` tables, {} codes: {tabled:?}",
        tabled.len()
    );
    let resume = resume_refusals(&doc);
    assert!(resume.len() > 2, "resume refusals parsed: {resume:?}");

    let documented: BTreeSet<&str> = catalogue
        .iter()
        .chain(&tabled)
        .chain(&resume)
        .map(String::as_str)
        .collect();
    let registry: BTreeSet<&str> = codes::ALL.iter().copied().collect();
    assert_eq!(
        registry.len(),
        codes::ALL.len(),
        "`codes::ALL` names a code twice"
    );

    let undocumented: Vec<_> = registry.difference(&documented).collect();
    let unregistered: Vec<_> = documented.difference(&registry).collect();
    assert!(
        undocumented.is_empty() && unregistered.is_empty(),
        "in `codes::ALL` but not in ipc.md: {undocumented:?}; \
         in ipc.md but not in `codes::ALL`: {unregistered:?}"
    );
}

fn rust_sources(dir: &Path, into: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.expect("a readable directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_sources(&path, into);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            into.push(path);
        }
    }
}

/// A file's numbered lines up to its test module: the first `#[cfg(test)]`
/// that gates an inline `mod … {`. Not the first `#[cfg(test)]` of any
/// kind, because several files gate a helper that way long before their
/// tests begin (`app.rs` near its top), and cutting there would leave most
/// of the file unscanned.
fn production_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let lines: Vec<&str> = text.lines().collect();
    let gates_a_test_module = |at: usize| {
        lines[at + 1..]
            .iter()
            .map(|line| line.trim())
            .find(|line| !line.starts_with("#["))
            .is_some_and(|item| {
                let item = item.strip_prefix("pub ").unwrap_or(item);
                let item = item.strip_prefix("pub(crate) ").unwrap_or(item);
                item.starts_with("mod ") && item.ends_with('{')
            })
    };
    let cut = (0..lines.len())
        .find(|&at| lines[at].trim() == "#[cfg(test)]" && gates_a_test_module(at))
        .unwrap_or(lines.len());
    lines
        .into_iter()
        .take(cut)
        .enumerate()
        .map(|(at, line)| (at + 1, line))
}

#[test]
fn no_source_spells_a_code_as_a_literal() {
    let root = repo_root();
    let mut crate_dirs: Vec<_> = std::fs::read_dir(root.join("crates"))
        .expect("crates/ is readable")
        .map(|entry| {
            entry
                .expect("a readable directory entry")
                .path()
                .join("src")
        })
        .filter(|src| src.is_dir())
        .collect();
    crate_dirs.sort();
    let mut files = Vec::new();
    for src in &crate_dirs {
        rust_sources(src, &mut files);
    }

    let registry = root.join("crates/roost-ipc/src/codes.rs");
    files.retain(|file| *file != registry);

    let quoted: Vec<(&str, String)> = codes::ALL
        .iter()
        .map(|&code| (code, format!("\"{code}\"")))
        .collect();
    let mut uses = 0;
    let mut strays = Vec::new();
    let mut unmatched: BTreeSet<_> = NOT_A_WIRE_CODE.iter().collect();
    for file in &files {
        let rel = file
            .strip_prefix(&root)
            .expect("under the repo")
            .to_string_lossy()
            .into_owned();
        let text = std::fs::read_to_string(file).unwrap_or_else(|e| panic!("{rel}: {e}"));
        for (number, line) in production_lines(&text) {
            uses += line.matches("codes::").count();
            for (code, literal) in quoted.iter().filter(|(_, literal)| line.contains(literal)) {
                match NOT_A_WIRE_CODE
                    .iter()
                    .find(|(file, exempt, _)| *file == rel && exempt == code)
                {
                    Some(exemption) => {
                        unmatched.remove(exemption);
                    }
                    None => strays.push(format!("{rel}:{number}: {literal}")),
                }
            }
        }
    }

    assert!(
        files.len() > 40,
        "only {} source files visited",
        files.len()
    );
    assert!(uses > 30, "only {uses} `codes::` uses found");
    assert!(
        strays.is_empty(),
        "a wire code spelt as a literal; name it from `roost_ipc::codes` instead:\n{}",
        strays.join("\n")
    );
    assert!(
        unmatched.is_empty(),
        "exemptions that match nothing: {unmatched:?}"
    );
}
