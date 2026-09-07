//! Pure policy for moving a paste or a drop across a host boundary
//! (plan 047 §3.2).
//!
//! This module inspects nothing and touches no filesystem — it is
//! mirrored by hand into Swift, so it has to be testable with only
//! values in hand. Something else (`roost-iced`'s blocking-pool
//! inspector) turns dropped paths into [`Candidate`]s; this module only
//! decides, given a [`Target`] and a [`Source`], what happens next.

use std::ffi::OsStr;
use std::path::PathBuf;

use roost_ipc::messages::MAX_PUT_FILE_BYTES;

use crate::drop_content;

/// Where a gesture is landing, decided by the caller from tab + host
/// state before the planner ever sees a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Local,
    Host,
    Frozen,
    Unavailable,
}

/// What is being pasted or dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// `name` is the caller's minted name (today's
    /// `roost-image-<nanos>-<16hex>.png` scheme, `roost-iced`'s
    /// `paste_image.rs`) — this module is pure and cannot mint one.
    ClipboardImage {
        name: String,
        png_len: u64,
    },
    Files(Vec<Candidate>),
}

/// One dropped path, already inspected (`metadata`, deduped first-seen)
/// by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Regular {
        len: u64,
    },
    Directory,
    Missing,
    Unreadable,
    /// Anything `metadata` reports as neither a regular file nor a
    /// directory (FIFOs, devices, procfs entries whose `len()` lies).
    Other,
}

/// What the caller should do about a gesture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Local `Files`: exactly today's `drop_content::resolve` bytes.
    PasteText(String),
    /// Local `ClipboardImage`: the caller writes the temp PNG itself
    /// and pastes its bare path, as it always has.
    PasteTempPng,
    Upload {
        items: Vec<Item>,
        skipped: Vec<Skipped>,
    },
    Refuse(Refusal),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Frozen,
    Unavailable,
    Empty,
    NothingUploadable(Vec<Skipped>),
    GestureOverBudget { total: u64 },
}

/// One file to send, in gesture order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub source: ItemSource,
}

/// Where an [`Item`]'s bytes come from at upload time. The planner
/// never holds bytes itself — a path is read fresh by the execution
/// layer (§3.2: a stale `len` or a file that grew must be caught then,
/// not here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemSource {
    Path(PathBuf),
    ClipboardPng,
}

/// One candidate the plan will not upload, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    Directory,
    Missing,
    Unreadable,
    NotRegular,
    OverCap { len: u64 },
}

impl SkipReason {
    /// The stable wire string C1 documented on `SkippedFile.reason` —
    /// this is the sole source of those five literals; the wire type
    /// carries them as a bare `String` a crate away.
    pub fn as_wire_str(&self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::Missing => "missing",
            Self::Unreadable => "unreadable",
            Self::NotRegular => "not-regular",
            Self::OverCap { .. } => "over-cap",
        }
    }
}

/// Per-gesture cap on the sum of uploadable bytes — independent of the
/// per-file [`MAX_PUT_FILE_BYTES`], so one drop cannot fill half the
/// host's store.
pub const MAX_GESTURE_BYTES: u64 = 256 * 1024 * 1024;

/// Decide what a paste or drop does. Target is checked first,
/// regardless of source: a frozen or unavailable tab refuses before
/// anything about the source is even looked at.
pub fn plan(source: Source, target: Target) -> Plan {
    match target {
        Target::Frozen => Plan::Refuse(Refusal::Frozen),
        Target::Unavailable => Plan::Refuse(Refusal::Unavailable),
        Target::Local => plan_local(source),
        Target::Host => plan_host(source),
    }
}

fn plan_local(source: Source) -> Plan {
    match source {
        // Local sees every candidate path, whatever its `Kind` — today's
        // local drop does no image/directory filtering, and `resolve`
        // (not this module) owns that policy.
        Source::Files(candidates) => {
            let paths: Vec<PathBuf> = candidates.into_iter().map(|c| c.path).collect();
            match drop_content::resolve(paths, None, None) {
                Some(text) => Plan::PasteText(text),
                None => Plan::Refuse(Refusal::Empty),
            }
        }
        Source::ClipboardImage { .. } => Plan::PasteTempPng,
    }
}

fn plan_host(source: Source) -> Plan {
    match source {
        Source::Files(candidates) => plan_host_files(candidates),
        Source::ClipboardImage { name, png_len } => plan_host_clipboard(name, png_len),
    }
}

fn plan_host_files(candidates: Vec<Candidate>) -> Plan {
    if candidates.is_empty() {
        return Plan::Refuse(Refusal::Empty);
    }
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    let mut total: u64 = 0;
    for candidate in candidates {
        match candidate.kind {
            Kind::Regular { len } if len <= MAX_PUT_FILE_BYTES => {
                // Saturating: an overflow can only mean the batch is over
                // budget, and it must refuse rather than wrap into an upload.
                total = total.saturating_add(len);
                let name =
                    sanitize_name(candidate.path.file_name().unwrap_or_else(|| OsStr::new("")));
                items.push(Item {
                    name,
                    source: ItemSource::Path(candidate.path),
                });
            }
            Kind::Regular { len } => skipped.push(Skipped {
                path: candidate.path,
                reason: SkipReason::OverCap { len },
            }),
            Kind::Directory => skipped.push(Skipped {
                path: candidate.path,
                reason: SkipReason::Directory,
            }),
            Kind::Missing => skipped.push(Skipped {
                path: candidate.path,
                reason: SkipReason::Missing,
            }),
            Kind::Unreadable => skipped.push(Skipped {
                path: candidate.path,
                reason: SkipReason::Unreadable,
            }),
            Kind::Other => skipped.push(Skipped {
                path: candidate.path,
                reason: SkipReason::NotRegular,
            }),
        }
    }
    // Checked before anything else about a non-empty upload: a batch
    // over budget is refused whole, never partially uploaded.
    if total > MAX_GESTURE_BYTES {
        return Plan::Refuse(Refusal::GestureOverBudget { total });
    }
    if items.is_empty() {
        return Plan::Refuse(Refusal::NothingUploadable(skipped));
    }
    Plan::Upload { items, skipped }
}

fn plan_host_clipboard(name: String, png_len: u64) -> Plan {
    if png_len > MAX_PUT_FILE_BYTES {
        return Plan::Refuse(Refusal::NothingUploadable(vec![Skipped {
            path: PathBuf::from(&name),
            reason: SkipReason::OverCap { len: png_len },
        }]));
    }
    Plan::Upload {
        items: vec![Item {
            name,
            source: ItemSource::ClipboardPng,
        }],
        skipped: vec![],
    }
}

/// The server rule `SessionPutFileParams::name` and `TabSendFileParams`
/// paths' basenames must satisfy: 1–128 bytes, charset
/// `[A-Za-z0-9._-]`, not `.`, not `..`, not starting with `-`.
/// [`sanitize_name`] always produces a name that passes this — pinned
/// by a seeded property test rather than a property-test crate (the
/// workspace has none and this plan must not add one).
pub fn is_valid_put_file_name(name: &str) -> bool {
    let len = name.len();
    if len == 0 || len > 128 {
        return false;
    }
    if name == "." || name == ".." {
        return false;
    }
    if name.starts_with('-') {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Turn a dropped file's final path component into a name the host's
/// `session.put_file` will accept, per §3.2's pinned algorithm.
///
/// Lossy-UTF-8 first (a non-UTF-8 name becomes `U+FFFD`s, which then
/// map like any other invalid character); split at the last `.` into
/// stem and extension where the extension is 1–16 bytes of
/// `[A-Za-z0-9]`, otherwise there is no extension and the whole string
/// (dots included) is the stem; map every byte outside
/// `[A-Za-z0-9._-]` to `_` and collapse runs of `_`; a leading `-`
/// becomes `_`; an empty or all-`_` stem becomes `file`; a whole name
/// of `.` or `..` becomes `file`; truncate the stem so
/// `stem + "." + ext` is at most 128 bytes.
pub fn sanitize_name(basename: &OsStr) -> String {
    let lossy = basename.to_string_lossy();
    if lossy == "." || lossy == ".." {
        return "file".to_string();
    }
    let (stem_raw, ext) = split_extension(&lossy);
    let mut stem = sanitize_chars(stem_raw);
    if stem.starts_with('-') {
        stem.replace_range(0..1, "_");
    }
    if stem.is_empty() || stem.bytes().all(|b| b == b'_') {
        stem = "file".to_string();
    }
    let budget = match ext {
        Some(ext) => 128usize.saturating_sub(1 + ext.len()),
        None => 128,
    };
    truncate_ascii(&mut stem, budget);
    match ext {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem,
    }
}

/// Split at the last `.` when what follows is a valid extension;
/// otherwise the whole string (with any embedded dots) is the stem, so
/// `"a..b.tar.gz"` keeps `gz` and `"file.<17 alnum bytes>"` keeps
/// nothing separate at all.
fn split_extension(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(index) => {
            let candidate = &name[index + 1..];
            if is_valid_extension(candidate) {
                (&name[..index], Some(candidate))
            } else {
                (name, None)
            }
        }
        None => (name, None),
    }
}

fn is_valid_extension(candidate: &str) -> bool {
    let len = candidate.len();
    (1..=16).contains(&len) && candidate.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn sanitize_chars(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| if is_allowed(c) { c } else { '_' })
        .collect();
    collapse_underscore_runs(&mapped)
}

fn is_allowed(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

fn collapse_underscore_runs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_underscore = false;
    for c in s.chars() {
        if c == '_' {
            if !prev_underscore {
                out.push('_');
            }
            prev_underscore = true;
        } else {
            out.push(c);
            prev_underscore = false;
        }
    }
    out
}

/// Byte-safe because `sanitize_chars` already reduced `s` to ASCII.
fn truncate_ascii(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        s.truncate(max_bytes);
    }
}

/// Copy the exact status-line strings §3.3 pins, over already-decided
/// [`Plan`] outcomes. Pure formatting: no clock, no I/O.
pub mod status {
    use super::{SkipReason, Skipped};
    use std::path::Path;

    /// `Sending shot.png (4.2 MiB) to workbox…`
    pub fn sending(name: &str, bytes: u64, host: &str) -> String {
        format!(
            "Sending {} ({}) to {host}…",
            display(name),
            format_mib(bytes)
        )
    }

    /// `Sent shot.png to workbox` for one file, `Sent 3 files to
    /// workbox` for more — the *uploaded* count, skips appended once.
    pub fn sent(names: &[String], host: &str, skipped: &[Skipped]) -> String {
        let body = match names {
            [only] => format!("Sent {} to {host}", display(only)),
            _ => format!("Sent {} files to {host}", names.len()),
        };
        format!("{body}{}", skipped_suffix(skipped))
    }

    /// `Could not send shot.png to workbox: <reason>`
    pub fn failed(name: &str, host: &str, reason: &str) -> String {
        format!("Could not send {} to {host}: {reason}", display(name))
    }

    /// `Nothing to send to workbox (skipped: build/ is a directory)` —
    /// [`super::Refusal::NothingUploadable`], which has no uploaded count
    /// to report and so cannot borrow [`sent`]'s wording.
    pub fn nothing_uploadable(host: &str, skipped: &[Skipped]) -> String {
        format!("Nothing to send to {host}{}", skipped_suffix(skipped))
    }

    /// `That drop is 540 MiB, over the 256 MiB per-drop limit` —
    /// [`super::Refusal::GestureOverBudget`].
    pub fn over_budget(total: u64) -> String {
        format!(
            "That drop is {}, over the {} per-drop limit",
            format_mib(total),
            format_mib(super::MAX_GESTURE_BYTES)
        )
    }

    fn skipped_suffix(skipped: &[Skipped]) -> String {
        if skipped.is_empty() {
            return String::new();
        }
        let phrases: Vec<String> = skipped.iter().map(skip_phrase).collect();
        format!(" (skipped: {})", phrases.join(", "))
    }

    fn skip_phrase(item: &Skipped) -> String {
        let name = display(&basename(&item.path));
        match item.reason {
            SkipReason::Directory => format!("{name}/ is a directory"),
            SkipReason::Missing => format!("{name} is missing"),
            SkipReason::Unreadable => format!("{name} is unreadable"),
            SkipReason::NotRegular => format!("{name} is not a regular file"),
            SkipReason::OverCap { .. } => format!(
                "{name} is over {} MiB",
                (super::MAX_PUT_FILE_BYTES / (1024 * 1024))
            ),
        }
    }

    fn basename(path: &Path) -> String {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned())
    }

    /// Control bytes — and the Unicode line/paragraph separators, which
    /// are not `Cc` but break a status line exactly like a newline —
    /// become `?`, so an odd local basename cannot inject into status
    /// text.
    fn display(name: &str) -> String {
        name.chars()
            .map(|c| {
                if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                    '?'
                } else {
                    c
                }
            })
            .collect()
    }

    fn format_mib(bytes: u64) -> String {
        let mib = bytes as f64 / (1024.0 * 1024.0);
        let rounded = (mib * 10.0).round() / 10.0;
        if (rounded - rounded.trunc()).abs() < f64::EPSILON {
            format!("{} MiB", rounded as i64)
        } else {
            format!("{rounded:.1} MiB")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regular(path: &str, len: u64) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            kind: Kind::Regular { len },
        }
    }

    fn skip(path: &str, kind: Kind) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            kind,
        }
    }

    // -- Target ordering ----------------------------------------------

    #[test]
    fn frozen_refuses_before_looking_at_source() {
        let source = Source::Files(vec![regular("/tmp/a.png", 10)]);
        assert_eq!(plan(source, Target::Frozen), Plan::Refuse(Refusal::Frozen));
    }

    #[test]
    fn unavailable_refuses_before_looking_at_source() {
        let source = Source::Files(vec![regular("/tmp/a.png", 10)]);
        assert_eq!(
            plan(source, Target::Unavailable),
            Plan::Refuse(Refusal::Unavailable)
        );
    }

    // -- Local: byte-identical to drop_content -------------------------

    fn files(paths: &[&str]) -> Source {
        Source::Files(paths.iter().map(|p| regular(p, 1)).collect())
    }

    #[test]
    fn local_files_match_drop_content_single() {
        let expected = drop_content::resolve(["/tmp/My File.png"], None, None);
        assert_eq!(
            plan(files(&["/tmp/My File.png"]), Target::Local),
            Plan::PasteText(expected.unwrap())
        );
    }

    #[test]
    fn local_files_match_drop_content_multiple_first_seen_order() {
        let expected = drop_content::resolve(["/tmp/a b.png", "/tmp/c.png"], None, None);
        assert_eq!(
            plan(files(&["/tmp/a b.png", "/tmp/c.png"]), Target::Local),
            Plan::PasteText(expected.unwrap())
        );
    }

    #[test]
    fn local_files_match_drop_content_duplicates_collapsed() {
        let expected = drop_content::resolve(["/tmp/shot.png", "/tmp/shot.png"], None, None);
        assert_eq!(
            plan(files(&["/tmp/shot.png", "/tmp/shot.png"]), Target::Local),
            Plan::PasteText(expected.unwrap())
        );
    }

    #[test]
    fn local_files_match_drop_content_rejects_newline_paths() {
        let input = ["/tmp/ev\nil.png", "/tmp/ok.png"];
        let expected = drop_content::resolve(input, None, None);
        assert_eq!(
            plan(files(&input), Target::Local),
            Plan::PasteText(expected.unwrap())
        );
    }

    #[test]
    fn local_files_all_rejected_is_refuse_empty() {
        let input = ["/tmp/ev\nil.png"];
        assert_eq!(drop_content::resolve(input, None, None), None);
        assert_eq!(
            plan(files(&input), Target::Local),
            Plan::Refuse(Refusal::Empty)
        );
    }

    #[test]
    fn local_sees_every_candidate_kind_no_filtering() {
        // Today's local drop does no image/directory filtering — a
        // Directory candidate still becomes a plain path in the join.
        let source = Source::Files(vec![skip("/tmp/adir", Kind::Directory)]);
        let expected = drop_content::resolve(["/tmp/adir"], None, None);
        assert_eq!(
            plan(source, Target::Local),
            Plan::PasteText(expected.unwrap())
        );
    }

    #[test]
    fn local_clipboard_image_is_paste_temp_png() {
        let source = Source::ClipboardImage {
            name: "roost-image-1-aa.png".into(),
            png_len: 10,
        };
        assert_eq!(plan(source, Target::Local), Plan::PasteTempPng);
    }

    // -- Host: files ----------------------------------------------------

    #[test]
    fn host_files_preserve_order_and_upload_every_regular_under_cap() {
        let source = Source::Files(vec![regular("/tmp/b.png", 10), regular("/tmp/a.png", 20)]);
        match plan(source, Target::Host) {
            Plan::Upload { items, skipped } => {
                assert!(skipped.is_empty());
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].name, "b.png");
                assert_eq!(items[1].name, "a.png");
                assert_eq!(
                    items[0].source,
                    ItemSource::Path(PathBuf::from("/tmp/b.png"))
                );
                assert_eq!(
                    items[1].source,
                    ItemSource::Path(PathBuf::from("/tmp/a.png"))
                );
            }
            other => panic!("expected Upload, got {other:?}"),
        }
    }

    #[test]
    fn host_files_each_kind_is_skipped_with_matching_reason() {
        let source = Source::Files(vec![
            regular("/tmp/ok.png", 10),
            skip("/tmp/dir", Kind::Directory),
            skip("/tmp/gone", Kind::Missing),
            skip("/tmp/locked", Kind::Unreadable),
            skip("/tmp/fifo", Kind::Other),
        ]);
        match plan(source, Target::Host) {
            Plan::Upload { items, skipped } => {
                assert_eq!(items.len(), 1);
                assert_eq!(
                    skipped,
                    vec![
                        Skipped {
                            path: PathBuf::from("/tmp/dir"),
                            reason: SkipReason::Directory
                        },
                        Skipped {
                            path: PathBuf::from("/tmp/gone"),
                            reason: SkipReason::Missing
                        },
                        Skipped {
                            path: PathBuf::from("/tmp/locked"),
                            reason: SkipReason::Unreadable
                        },
                        Skipped {
                            path: PathBuf::from("/tmp/fifo"),
                            reason: SkipReason::NotRegular
                        },
                    ]
                );
            }
            other => panic!("expected Upload, got {other:?}"),
        }
    }

    #[test]
    fn host_files_over_cap_is_skipped_not_uploaded() {
        let source = Source::Files(vec![regular("/tmp/big.bin", MAX_PUT_FILE_BYTES + 1)]);
        assert_eq!(
            plan(source, Target::Host),
            Plan::Refuse(Refusal::NothingUploadable(vec![Skipped {
                path: PathBuf::from("/tmp/big.bin"),
                reason: SkipReason::OverCap {
                    len: MAX_PUT_FILE_BYTES + 1
                },
            }]))
        );
    }

    #[test]
    fn host_files_exactly_at_cap_uploads() {
        let source = Source::Files(vec![regular("/tmp/exact.bin", MAX_PUT_FILE_BYTES)]);
        match plan(source, Target::Host) {
            Plan::Upload { items, skipped } => {
                assert!(skipped.is_empty());
                assert_eq!(items.len(), 1);
            }
            other => panic!("expected Upload, got {other:?}"),
        }
    }

    /// `MAX_GESTURE_BYTES` (256 MiB) is a whole number of 1 MiB files, so
    /// this stays entirely under the per-file `MAX_PUT_FILE_BYTES` cap
    /// (10 MiB) while summing to exactly the gesture budget.
    fn many_one_mib_candidates(count: u64) -> Vec<Candidate> {
        (0..count)
            .map(|i| regular(&format!("/tmp/f{i}.bin"), 1024 * 1024))
            .collect()
    }

    #[test]
    fn host_files_gesture_exactly_at_budget_passes() {
        let source = Source::Files(many_one_mib_candidates(256));
        match plan(source, Target::Host) {
            Plan::Upload { items, skipped } => {
                assert!(skipped.is_empty());
                assert_eq!(items.len(), 256);
            }
            other => panic!("expected Upload, got {other:?}"),
        }
    }

    #[test]
    fn host_files_gesture_over_budget_by_one_refuses_whole_batch() {
        let mut candidates = many_one_mib_candidates(256);
        candidates.push(regular("/tmp/extra-byte.bin", 1));
        let source = Source::Files(candidates);
        assert_eq!(
            plan(source, Target::Host),
            Plan::Refuse(Refusal::GestureOverBudget {
                total: MAX_GESTURE_BYTES + 1
            })
        );
    }

    #[test]
    fn host_files_nothing_uploadable_carries_all_skips() {
        let source = Source::Files(vec![
            skip("/tmp/dir", Kind::Directory),
            skip("/tmp/gone", Kind::Missing),
        ]);
        assert_eq!(
            plan(source, Target::Host),
            Plan::Refuse(Refusal::NothingUploadable(vec![
                Skipped {
                    path: PathBuf::from("/tmp/dir"),
                    reason: SkipReason::Directory
                },
                Skipped {
                    path: PathBuf::from("/tmp/gone"),
                    reason: SkipReason::Missing
                },
            ]))
        );
    }

    #[test]
    fn host_files_empty_input_is_refuse_empty() {
        assert_eq!(
            plan(Source::Files(vec![]), Target::Host),
            Plan::Refuse(Refusal::Empty)
        );
    }

    // -- Host: clipboard image -------------------------------------------

    #[test]
    fn host_clipboard_image_under_cap_uploads() {
        let source = Source::ClipboardImage {
            name: "roost-image-1-aa.png".into(),
            png_len: MAX_PUT_FILE_BYTES,
        };
        assert_eq!(
            plan(source, Target::Host),
            Plan::Upload {
                items: vec![Item {
                    name: "roost-image-1-aa.png".into(),
                    source: ItemSource::ClipboardPng,
                }],
                skipped: vec![],
            }
        );
    }

    #[test]
    fn host_clipboard_image_over_cap_refuses_with_skip() {
        let source = Source::ClipboardImage {
            name: "roost-image-1-aa.png".into(),
            png_len: MAX_PUT_FILE_BYTES + 1,
        };
        assert_eq!(
            plan(source, Target::Host),
            Plan::Refuse(Refusal::NothingUploadable(vec![Skipped {
                path: PathBuf::from("roost-image-1-aa.png"),
                reason: SkipReason::OverCap {
                    len: MAX_PUT_FILE_BYTES + 1
                },
            }]))
        );
    }

    // -- sanitize_name: pinned edge cases ---------------------------------

    fn sanitize_str(name: &str) -> String {
        sanitize_name(OsStr::new(name))
    }

    #[test]
    fn sanitize_name_pinned_edge_cases() {
        assert_eq!(sanitize_str(""), "file");
        assert_eq!(sanitize_str("."), "file");
        assert_eq!(sanitize_str(".."), "file");
        assert_eq!(sanitize_str("-rf"), "_rf");
        assert_eq!(sanitize_str("my file.png"), "my_file.png");
        assert_eq!(sanitize_str("a..b.tar.gz"), "a..b.tar.gz");
        assert_eq!(sanitize_str("___"), "file");
    }

    #[test]
    fn sanitize_name_extension_of_seventeen_bytes_is_not_an_extension() {
        let name = format!("file.{}", "a".repeat(17));
        let sanitized = sanitize_str(&name);
        assert_eq!(sanitized, name);
        assert!(is_valid_put_file_name(&sanitized));
    }

    /// Reassembling stem + "." + ext reproduces the original bytes when
    /// both sides were already clean, so [`sanitize_name`] round-tripping
    /// unchanged does not by itself pin the 16-byte extension boundary —
    /// this exercises `split_extension`/`is_valid_extension` directly.
    #[test]
    fn extension_validity_boundary_is_sixteen_bytes() {
        assert!(is_valid_extension(&"a".repeat(16)));
        assert!(!is_valid_extension(&"a".repeat(17)));
        assert!(!is_valid_extension(""));
    }

    #[test]
    fn split_extension_rejects_a_seventeen_byte_suffix() {
        let name = format!("file.{}", "a".repeat(17));
        assert_eq!(split_extension(&name), (name.as_str(), None));
    }

    /// A construction where truncation only trims differently depending
    /// on whether the tail is treated as an extension, so the boundary
    /// is pinned through observable output too, not just the helpers.
    #[test]
    fn sanitize_name_boundary_truncation_treats_seventeen_byte_suffix_as_stem() {
        let name = format!("{}.{}", "a".repeat(120), "b".repeat(17));
        let sanitized = sanitize_str(&name);
        assert_eq!(sanitized, format!("{}.{}", "a".repeat(120), "b".repeat(7)));
    }

    #[test]
    fn sanitize_name_non_utf8_becomes_valid() {
        #[cfg(unix)]
        {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            let raw = b"bad\xffname.png";
            let sanitized = sanitize_name(OsStr::from_bytes(raw));
            assert!(is_valid_put_file_name(&sanitized));
            assert!(sanitized.ends_with(".png"));
        }
    }

    #[test]
    fn sanitize_name_long_stem_is_truncated_with_extension_kept() {
        let stem = "a".repeat(300);
        let name = format!("{stem}.png");
        let sanitized = sanitize_str(&name);
        assert!(sanitized.len() <= 128);
        assert!(sanitized.ends_with(".png"));
        assert!(is_valid_put_file_name(&sanitized));
    }

    #[test]
    fn sanitize_name_accented_basename_is_valid() {
        let sanitized = sanitize_str("résumé.pdf");
        assert!(is_valid_put_file_name(&sanitized));
        assert!(sanitized.ends_with(".pdf"));
    }

    #[test]
    fn sanitize_name_seeded_property_test_ten_thousand_cases() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;

            struct XorShift64(u64);
            impl XorShift64 {
                fn next_u64(&mut self) -> u64 {
                    let mut x = self.0;
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    self.0 = x;
                    x
                }
                fn range(&mut self, bound: u64) -> u64 {
                    self.next_u64() % bound
                }
            }

            const ALNUM: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
            let mut rng = XorShift64(0x9e3779b97f4a7c15);
            for case in 0..10_000u32 {
                let len = rng.range(301) as usize;
                let mut bytes = Vec::with_capacity(len);
                for _ in 0..len {
                    let byte = match rng.range(7) {
                        0 => b'.',
                        1 => b'-',
                        2 => b'_',
                        3 => ALNUM[rng.range(ALNUM.len() as u64) as usize],
                        4 => b' ',
                        5 => 0xff,
                        _ => (rng.next_u64() & 0xff) as u8,
                    };
                    bytes.push(byte);
                }
                let name = OsStr::from_bytes(&bytes);
                let sanitized = sanitize_name(name);
                assert!(
                    is_valid_put_file_name(&sanitized),
                    "case {case} input {bytes:?} produced invalid name {sanitized:?}"
                );
            }
        }
    }

    // -- SkipReason wire strings -------------------------------------------

    #[test]
    fn skip_reason_wire_strings_are_pinned() {
        assert_eq!(SkipReason::Directory.as_wire_str(), "directory");
        assert_eq!(SkipReason::Missing.as_wire_str(), "missing");
        assert_eq!(SkipReason::Unreadable.as_wire_str(), "unreadable");
        assert_eq!(SkipReason::NotRegular.as_wire_str(), "not-regular");
        assert_eq!(SkipReason::OverCap { len: 1 }.as_wire_str(), "over-cap");
    }

    // -- status ---------------------------------------------------------

    #[test]
    fn status_sending_line() {
        assert_eq!(
            status::sending("shot.png", 4_404_019, "workbox"),
            "Sending shot.png (4.2 MiB) to workbox…"
        );
    }

    #[test]
    fn status_sent_single_file() {
        assert_eq!(
            status::sent(&["shot.png".to_string()], "workbox", &[]),
            "Sent shot.png to workbox"
        );
    }

    #[test]
    fn status_sent_multiple_files_uses_uploaded_count() {
        let names: Vec<String> = vec!["a.png".into(), "b.png".into(), "c.png".into()];
        assert_eq!(
            status::sent(&names, "workbox", &[]),
            "Sent 3 files to workbox"
        );
    }

    #[test]
    fn status_failed_line() {
        assert_eq!(
            status::failed("shot.png", "workbox", "connection reset"),
            "Could not send shot.png to workbox: connection reset"
        );
    }

    #[test]
    fn status_mixed_example_is_pinned_byte_for_byte() {
        let names: Vec<String> = vec!["a.png".into(), "b.png".into()];
        let skipped = vec![
            Skipped {
                path: PathBuf::from("build"),
                reason: SkipReason::Directory,
            },
            Skipped {
                path: PathBuf::from("core.dump"),
                reason: SkipReason::OverCap {
                    len: 11 * 1024 * 1024,
                },
            },
        ];
        assert_eq!(
            status::sent(&names, "workbox", &skipped),
            "Sent 2 files to workbox (skipped: build/ is a directory, core.dump is over 10 MiB)"
        );
    }

    #[test]
    fn status_nothing_uploadable_names_every_skip() {
        let skipped = vec![
            Skipped {
                path: PathBuf::from("/tmp/build"),
                reason: SkipReason::Directory,
            },
            Skipped {
                path: PathBuf::from("/tmp/core.dump"),
                reason: SkipReason::OverCap {
                    len: 11 * 1024 * 1024,
                },
            },
        ];
        assert_eq!(
            status::nothing_uploadable("workbox", &skipped),
            "Nothing to send to workbox (skipped: build/ is a directory, core.dump is over 10 MiB)"
        );
        assert_eq!(
            status::nothing_uploadable("workbox", &[]),
            "Nothing to send to workbox"
        );
    }

    #[test]
    fn status_over_budget_names_both_sides_of_the_limit() {
        assert_eq!(
            status::over_budget(540 * 1024 * 1024),
            "That drop is 540 MiB, over the 256 MiB per-drop limit"
        );
    }

    #[test]
    fn status_display_name_scrubs_control_bytes() {
        assert_eq!(
            status::sending("shot\u{1b}.png", 1_048_576, "workbox"),
            "Sending shot?.png (1 MiB) to workbox…"
        );
    }

    #[test]
    fn status_display_name_scrubs_unicode_line_separators() {
        let line = status::sending("a\u{2028}b\u{2029}c", 1024 * 1024, "h");
        assert!(
            !line.contains('\u{2028}') && !line.contains('\u{2029}'),
            "{line}"
        );
        assert!(line.starts_with("Sending a?b?c ("), "{line}");
    }
}
