//! Dynamic command providers — the pure, GTK-free model.
//!
//! A *provider* is a user script Roost runs to populate a palette frame
//! on demand, then runs again when the user picks a row. Where a
//! `command =` entry ([`crate::custom_command`]) launches one fixed
//! command in a tab, a `provider =` entry produces a **dynamic** list and
//! then acts on the choice — the "open shed" pattern. Mirrors
//! `mac/Sources/Roost/Provider.swift`.
//!
//! This module is the pure half: parsing `provider =` lines + directory
//! entries, building the subprocess invocation (argv / env / stdin), and
//! parsing the script's stdout into palette rows. The spawn itself,
//! off-main + with a timeout, lives in `app.rs`.
//!
//! ## The contract (v1)
//!
//! Roost runs the provider's `run` command twice, distinguished by an
//! argv phase (`list`, then `activate`) and `ROOST_PROVIDER_PHASE`:
//!
//! * **list** — stdout is `{"items":[{"id","title","subtitle?"}],"placeholder?"}`
//!   (a bare `[ … ]` array is also accepted). These become the rows.
//! * **activate** — run with `ROOST_SELECTED_ID` set; the script does its
//!   work (usually via `roostctl`/`$ROOST_SOCKET`) and either prints
//!   nothing (palette closes) or another `{"items":[…]}` to drill in —
//!   the same schema as `list`, so the contract is reused on selection.
//!
//! Both phases also receive the active-tab context as env vars and as a
//! JSON object on stdin (`{"v":1,"phase","query","selected_id?","active_tab":{…},"socket"}`).

use serde::{Deserialize, Serialize};

use crate::custom_command::tokenize;
use crate::palette::PaletteItem;

/// Wall-clock cap on a single provider invocation when none is set.
pub const DEFAULT_TIMEOUT_SECS: u64 = 5;
/// Most rows a provider's `list` may contribute when none is set; extra
/// rows are dropped (with a sentinel hint) so a runaway script can't
/// flood the palette.
pub const DEFAULT_LIMIT: usize = 100;
const MAX_TIMEOUT_SECS: u64 = 60;
const MAX_LIMIT: usize = 1000;

/// One `provider =` entry (or one discovered provider script). `label` +
/// `run` are required; `title` defaults to `label`. `timeout_secs` and
/// `limit` bound a single run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub label: String,
    pub run: String,
    pub title: String,
    pub timeout_secs: u64,
    pub limit: usize,
    /// `true` for a config `provider = run="…"` entry: `run` is a shell
    /// command (run via `sh -c`, like `command =`). `false` for a
    /// discovered script: `run` is a direct executable path, exec'd as
    /// argv[0] with no shell — no word-splitting / metacharacter / rc
    /// hazards from the path.
    pub shell_interpret: bool,
}

/// Which leg of the contract a run is: the initial population or the
/// post-selection action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    List,
    Activate,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::List => "list",
            Phase::Activate => "activate",
        }
    }
}

/// The active-tab context Roost injects into a provider run so the script
/// knows where the user is. Built fresh per invocation by `app.rs`.
#[derive(Debug, Clone, Default)]
pub struct ProviderContext {
    pub socket: String,
    pub query: String,
    pub selected_id: Option<String>,
    pub active_tab_id: Option<i64>,
    pub active_project_id: Option<i64>,
    pub active_cwd: String,
    pub active_title: String,
    /// Absolute path to Roost's own `roostctl`, exposed to the script as
    /// `ROOST_ROOSTCTL` so a provider can drive Roost without `roostctl`
    /// on `PATH` — the Mac `.app` bundles it off-`PATH`; the Linux `.deb`
    /// installs it on `PATH`; either way Roost hands the script the exact
    /// binary. `None` when Roost can't locate its sibling.
    pub roostctl: Option<String>,
}

/// Parse one `provider =` value into a [`Provider`]. Returns `None` when
/// `label` or `run` is missing/empty so the caller can skip the line with
/// a warning. `timeout`/`limit` fall back to the defaults when absent or
/// unparseable; both are clamped to a sane ceiling. Unknown keys are
/// ignored (forward-compat) — same grammar as `command =`.
pub fn parse_provider_line(value: &str) -> Option<Provider> {
    let mut label = String::new();
    let mut run = String::new();
    let mut title: Option<String> = None;
    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    let mut limit = DEFAULT_LIMIT;

    for token in tokenize(value) {
        match token.split_once('=') {
            Some((key, val)) => match key {
                "label" => label = val.to_string(),
                "run" => run = val.to_string(),
                "title" => title = Some(val.to_string()),
                "timeout" => {
                    if let Ok(n) = val.parse::<u64>() {
                        timeout_secs = n.clamp(1, MAX_TIMEOUT_SECS);
                    }
                }
                "limit" => {
                    if let Ok(n) = val.parse::<usize>() {
                        limit = n.clamp(1, MAX_LIMIT);
                    }
                }
                _ => {} // unknown key — forward-compat
            },
            None => {} // bare tokens are meaningless for providers
        }
    }

    if label.is_empty() || run.is_empty() {
        return None;
    }
    let title = title
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| label.clone());
    Some(Provider {
        label,
        run,
        title,
        timeout_secs,
        limit,
        shell_interpret: true, // config `run =` is a shell command
    })
}

/// Build a [`Provider`] from a discovered executable in the providers
/// directory. `run` is the file's absolute path; the label comes from a
/// `# @roost.label:` header comment if present, else a humanized
/// filename. `header` is the first chunk of the file's text (the caller
/// reads it; this stays I/O-free for testability).
pub fn provider_from_file(path: &str, filename: &str, header: &str) -> Provider {
    let (meta_label, meta_title) = metadata_from_header(header);
    let stem = filename
        .rsplit_once('.')
        .map(|(s, _ext)| s)
        .unwrap_or(filename);
    let label = meta_label.unwrap_or_else(|| humanize(stem));
    let title = meta_title.unwrap_or_else(|| label.clone());
    Provider {
        label,
        // The raw path — exec'd directly (not shell-interpreted), so a
        // filename with spaces or shell metacharacters is run as one path.
        run: path.to_string(),
        title,
        timeout_secs: DEFAULT_TIMEOUT_SECS,
        limit: DEFAULT_LIMIT,
        shell_interpret: false,
    }
}

/// Pull `# @roost.label:` / `# @roost.title:` overrides out of a script's
/// leading comment lines. Stops scanning at the first non-blank,
/// non-comment line so it only ever reads the header.
fn metadata_from_header(header: &str) -> (Option<String>, Option<String>) {
    let mut label = None;
    let mut title = None;
    for line in header.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Some(comment) = t.strip_prefix('#') else {
            break; // first real line ends the header
        };
        let comment = comment.trim();
        if let Some(rest) = comment.strip_prefix("@roost.label:") {
            label = Some(rest.trim().to_string()).filter(|s: &String| !s.is_empty());
        } else if let Some(rest) = comment.strip_prefix("@roost.title:") {
            title = Some(rest.trim().to_string()).filter(|s: &String| !s.is_empty());
        }
    }
    (label, title)
}

/// Turn `shed-open_logs` into `shed open logs` for a default label.
fn humanize(stem: &str) -> String {
    stem.replace(['-', '_'], " ").trim().to_string()
}

/// Build the argv that runs a provider phase. With `shell_interpret`
/// (config `run =`), wrap in `[shell, "-c", "<run> <phase>"]` —
/// non-interactive (`-c`, not `-i`) so the user's rc can't echo onto
/// stdout and corrupt the JSON. Without it (a discovered script path),
/// exec directly: `[run, "<phase>"]`. Either way the phase is `$1` in the
/// script and is also exported as `ROOST_PROVIDER_PHASE`.
pub fn invocation_argv(shell: &str, run: &str, shell_interpret: bool, phase: Phase) -> Vec<String> {
    if shell_interpret {
        vec![
            shell.to_string(),
            "-c".to_string(),
            format!("{} {}", run, phase.as_str()),
        ]
    } else {
        // Direct exec: `run` is a path (argv[0]), phase is argv[1]. No
        // shell, so no word-splitting / metacharacter interpretation, and
        // no rc echo to corrupt the JSON the script prints.
        vec![run.to_string(), phase.as_str().to_string()]
    }
}

/// Build the env pairs Roost layers onto a provider run. Flat, jq-free
/// access to the same context the stdin JSON carries — a bash provider
/// can read `$ROOST_SELECTED_ID` without parsing anything.
pub fn invocation_env(phase: Phase, ctx: &ProviderContext) -> Vec<(String, String)> {
    let mut env = vec![
        (
            "ROOST_PROVIDER_PHASE".to_string(),
            phase.as_str().to_string(),
        ),
        ("ROOST_SOCKET".to_string(), ctx.socket.clone()),
        ("ROOST_QUERY".to_string(), ctx.query.clone()),
        ("ROOST_ACTIVE_CWD".to_string(), ctx.active_cwd.clone()),
        ("ROOST_ACTIVE_TITLE".to_string(), ctx.active_title.clone()),
    ];
    if let Some(id) = ctx.active_tab_id {
        env.push(("ROOST_ACTIVE_TAB_ID".to_string(), id.to_string()));
    }
    if let Some(id) = ctx.active_project_id {
        env.push(("ROOST_ACTIVE_PROJECT_ID".to_string(), id.to_string()));
    }
    if let Some(sel) = &ctx.selected_id {
        env.push(("ROOST_SELECTED_ID".to_string(), sel.clone()));
    }
    if let Some(rc) = &ctx.roostctl {
        env.push(("ROOST_ROOSTCTL".to_string(), rc.clone()));
    }
    env
}

#[derive(Serialize)]
struct ActiveTabJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<i64>,
    cwd: String,
    title: String,
}

#[derive(Serialize)]
struct ProviderInputJson<'a> {
    v: u32,
    phase: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_id: Option<&'a str>,
    query: &'a str,
    active_tab: ActiveTabJson,
    socket: &'a str,
}

/// Serialize the full context as the JSON object Roost writes to the
/// provider's stdin. Always valid JSON ending in a newline.
pub fn invocation_stdin(phase: Phase, ctx: &ProviderContext) -> String {
    let input = ProviderInputJson {
        v: 1,
        phase: phase.as_str(),
        selected_id: ctx.selected_id.as_deref(),
        query: &ctx.query,
        active_tab: ActiveTabJson {
            id: ctx.active_tab_id,
            project_id: ctx.active_project_id,
            cwd: ctx.active_cwd.clone(),
            title: ctx.active_title.clone(),
        },
        socket: &ctx.socket,
    };
    let mut s = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
    s.push('\n');
    s
}

/// One row a provider's stdout contributes. `id` round-trips back to the
/// script as `ROOST_SELECTED_ID` on activate, so it's the stable handle.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProviderOutputItem {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    /// When `Some(false)`, the row renders but can't be selected (the
    /// palette stays open) — for empty/disabled states like "No results".
    /// Absent ⇒ actionable.
    #[serde(default)]
    pub actionable: Option<bool>,
}

/// A provider's parsed stdout: the rows plus optional palette chrome.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ProviderOutput {
    #[serde(default)]
    pub placeholder: String,
    #[serde(default)]
    pub items: Vec<ProviderOutputItem>,
}

/// Parse a provider's stdout. Empty/blank output is an empty result
/// (a valid "nothing to show / done, close" signal on activate). Accepts
/// either the object form (`{"items":[…]}`) or a bare array (`[…]`,
/// dmenu-style). Returns the parse error message on malformed JSON so the
/// caller can surface it as an error row.
pub fn parse_provider_output(stdout: &str) -> Result<ProviderOutput, String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(ProviderOutput::default());
    }
    let parsed = if trimmed.starts_with('[') {
        serde_json::from_str::<Vec<ProviderOutputItem>>(trimmed).map(|items| ProviderOutput {
            placeholder: String::new(),
            items,
        })
    } else {
        serde_json::from_str::<ProviderOutput>(trimmed)
    };
    parsed.map_err(|e| {
        format!("provider output is not a valid menu — expected a JSON `{{\"items\":[…]}}` object or `[…]` array: {e}")
    })
}

/// Parse an `activate` phase's stdout. Activate is primarily a side
/// effect; its stdout is ignored unless it *looks* like a provider payload
/// — a JSON object/array (a drill-down sub-menu). Non-JSON output (the tab
/// id `roostctl tab open` prints, log lines, …) yields an empty result, so
/// the palette just closes. Output that *does* look like JSON but fails to
/// parse is still surfaced as an error (a genuinely malformed sub-menu).
pub fn parse_activate_output(stdout: &str) -> Result<ProviderOutput, String> {
    let trimmed = stdout.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        parse_provider_output(stdout)
    } else {
        Ok(ProviderOutput::default())
    }
}

/// Build the palette rows that list the configured providers (the
/// "Custom Commands" frame). Each row's id is `provider:<i>`, parsed back
/// by [`provider_index`] on confirm. An empty list yields a single
/// non-actionable sentinel row.
pub fn provider_items(providers: &[Provider]) -> Vec<PaletteItem> {
    if providers.is_empty() {
        return vec![PaletteItem::new("provider:none", "No providers configured")
            .with_subtitle(Some("Add `provider = …` to config.conf".to_string()))
            .with_actionable(false)];
    }
    providers
        .iter()
        .enumerate()
        .map(|(i, p)| {
            PaletteItem::new(format!("provider:{i}"), p.label.clone())
                .with_subtitle(Some(p.run.clone()))
        })
        .collect()
}

/// Parse a provider row id (`provider:<index>`) back to the index. The
/// `provider:none` sentinel and any malformed id return `None`.
pub fn provider_index(id: &str) -> Option<usize> {
    id.strip_prefix("provider:")
        .and_then(|n| n.parse::<usize>().ok())
}

/// Sentinel id for the "list was truncated" hint row, so the confirm
/// handler can treat it as non-actionable.
pub const OVERFLOW_ID: &str = "provider:_overflow";

/// Turn a provider's parsed output into palette rows, capped at `limit`.
/// When the script returned more than `limit` rows, the extras are
/// dropped and a non-actionable hint row is appended rather than silently
/// truncating — refining the query (re-running the provider) is the way
/// to narrow it.
pub fn output_palette_items(out: &ProviderOutput, limit: usize) -> Vec<PaletteItem> {
    let mut rows: Vec<PaletteItem> = out
        .items
        .iter()
        .take(limit)
        .map(|it| {
            PaletteItem::new(it.id.clone(), it.title.clone())
                .with_subtitle(it.subtitle.clone())
                .with_actionable(it.actionable.unwrap_or(true))
        })
        .collect();
    if out.items.len() > limit {
        let extra = out.items.len() - limit;
        rows.push(
            PaletteItem::new(OVERFLOW_ID, format!("… {extra} more"))
                .with_subtitle(Some("refine your query".to_string()))
                .with_actionable(false),
        );
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(value: &str) -> Provider {
        parse_provider_line(value).expect("expected a valid provider")
    }

    #[test]
    fn parses_label_and_run_with_defaults() {
        let pr = p(r#"label="Open shed" run="~/.config/roost/providers/shed.sh""#);
        assert_eq!(pr.label, "Open shed");
        assert_eq!(pr.run, "~/.config/roost/providers/shed.sh");
        assert_eq!(pr.title, "Open shed"); // defaults to label
        assert_eq!(pr.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(pr.limit, DEFAULT_LIMIT);
    }

    #[test]
    fn explicit_title_timeout_limit() {
        let pr = p(r#"label="Shed" run="shed.sh" title="Pick service" timeout=8 limit=25"#);
        assert_eq!(pr.title, "Pick service");
        assert_eq!(pr.timeout_secs, 8);
        assert_eq!(pr.limit, 25);
    }

    #[test]
    fn timeout_and_limit_are_clamped() {
        let pr = p(r#"label="a" run="b" timeout=9999 limit=999999"#);
        assert_eq!(pr.timeout_secs, MAX_TIMEOUT_SECS);
        assert_eq!(pr.limit, MAX_LIMIT);
        // Unparseable values fall back to defaults, not zero.
        let pr = p(r#"label="a" run="b" timeout=soon limit=many"#);
        assert_eq!(pr.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(pr.limit, DEFAULT_LIMIT);
    }

    #[test]
    fn missing_label_or_run_is_none() {
        assert!(parse_provider_line(r#"run="shed.sh""#).is_none());
        assert!(parse_provider_line(r#"label="Shed""#).is_none());
        assert!(parse_provider_line(r#"label="Shed" run="""#).is_none());
    }

    #[test]
    fn unknown_keys_ignored() {
        let pr = p(r#"label="a" run="b" icon="star" mode="x""#);
        assert_eq!(pr.label, "a");
        assert_eq!(pr.run, "b");
    }

    #[test]
    fn provider_from_file_humanizes_filename() {
        let pr = provider_from_file("/x/shed-open_logs.sh", "shed-open_logs.sh", "");
        // Discovered providers keep the raw path and exec directly.
        assert_eq!(pr.run, "/x/shed-open_logs.sh");
        assert!(!pr.shell_interpret);
        assert_eq!(pr.label, "shed open logs");
        assert_eq!(pr.title, "shed open logs");
    }

    #[test]
    fn provider_from_file_keeps_raw_path_with_spaces() {
        let pr = provider_from_file("/x/my shed.sh", "my shed.sh", "");
        // Raw path (no quoting) — direct exec handles spaces safely.
        assert_eq!(pr.run, "/x/my shed.sh");
        assert!(!pr.shell_interpret);
    }

    #[test]
    fn provider_from_file_reads_header_metadata() {
        let header = "#!/usr/bin/env bash\n# @roost.label: Open shed\n# @roost.title: Pick a service\nset -e\n# later: ignored";
        let pr = provider_from_file("/x/shed.sh", "shed.sh", header);
        assert_eq!(pr.label, "Open shed");
        assert_eq!(pr.title, "Pick a service");
    }

    #[test]
    fn header_scan_stops_at_first_real_line() {
        // A `@roost.label` after a non-comment line must not be picked up.
        let header = "echo hi\n# @roost.label: Nope";
        let pr = provider_from_file("/x/foo.sh", "foo.sh", header);
        assert_eq!(pr.label, "foo"); // humanized filename, not "Nope"
    }

    #[test]
    fn invocation_argv_shell_vs_direct() {
        // Config provider (shell_interpret = true): wrapped in `sh -c`.
        let argv = invocation_argv("/bin/zsh", "shed.sh", true, Phase::List);
        assert_eq!(argv, vec!["/bin/zsh", "-c", "shed.sh list"]);
        let argv = invocation_argv("/bin/sh", "python3 shed.py", true, Phase::Activate);
        assert_eq!(argv[2], "python3 shed.py activate");
        // Discovered provider (shell_interpret = false): exec'd directly,
        // so a path with spaces is one argv element, not word-split.
        let argv = invocation_argv("/bin/sh", "/x/my shed.sh", false, Phase::List);
        assert_eq!(argv, vec!["/x/my shed.sh", "list"]);
    }

    #[test]
    fn invocation_env_carries_context() {
        let ctx = ProviderContext {
            socket: "/tmp/roost.sock".into(),
            query: "ap".into(),
            selected_id: Some("api".into()),
            active_tab_id: Some(7),
            active_project_id: Some(3),
            active_cwd: "/repo".into(),
            active_title: "build".into(),
            roostctl: Some("/usr/bin/roostctl".into()),
        };
        let env = invocation_env(Phase::Activate, &ctx);
        let get = |k: &str| env.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.clone());
        assert_eq!(get("ROOST_PROVIDER_PHASE").as_deref(), Some("activate"));
        assert_eq!(get("ROOST_SOCKET").as_deref(), Some("/tmp/roost.sock"));
        assert_eq!(get("ROOST_SELECTED_ID").as_deref(), Some("api"));
        assert_eq!(get("ROOST_ACTIVE_TAB_ID").as_deref(), Some("7"));
        assert_eq!(get("ROOST_ACTIVE_PROJECT_ID").as_deref(), Some("3"));
        assert_eq!(get("ROOST_ACTIVE_CWD").as_deref(), Some("/repo"));
        assert_eq!(get("ROOST_ROOSTCTL").as_deref(), Some("/usr/bin/roostctl"));
    }

    #[test]
    fn invocation_env_omits_absent_optionals() {
        let ctx = ProviderContext {
            socket: "/s".into(),
            ..Default::default()
        };
        let env = invocation_env(Phase::List, &ctx);
        assert!(!env.iter().any(|(k, _)| k == "ROOST_SELECTED_ID"));
        assert!(!env.iter().any(|(k, _)| k == "ROOST_ACTIVE_TAB_ID"));
        assert!(!env.iter().any(|(k, _)| k == "ROOST_ROOSTCTL")); // None ⇒ omitted
    }

    #[test]
    fn invocation_stdin_is_valid_json_with_context() {
        let ctx = ProviderContext {
            socket: "/s".into(),
            query: "q".into(),
            selected_id: Some("api".into()),
            active_tab_id: Some(7),
            active_project_id: Some(3),
            active_cwd: "/repo".into(),
            active_title: "build".into(),
            roostctl: None,
        };
        let json = invocation_stdin(Phase::Activate, &ctx);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["phase"], "activate");
        assert_eq!(v["selected_id"], "api");
        assert_eq!(v["active_tab"]["id"], 7);
        assert_eq!(v["active_tab"]["project_id"], 3);
        assert_eq!(v["active_tab"]["cwd"], "/repo");
        assert_eq!(v["socket"], "/s");
    }

    #[test]
    fn invocation_stdin_list_omits_selected_id() {
        let ctx = ProviderContext::default();
        let json = invocation_stdin(Phase::List, &ctx);
        assert!(!json.contains("selected_id"));
    }

    #[test]
    fn parse_output_object_form() {
        let out =
            parse_provider_output(r#"{"placeholder":"pick","items":[{"id":"web","title":"Web"}]}"#)
                .unwrap();
        assert_eq!(out.placeholder, "pick");
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].id, "web");
        assert_eq!(out.items[0].subtitle, None);
    }

    #[test]
    fn parse_output_bare_array_form() {
        let out = parse_provider_output(
            r#"[{"id":"web","title":"Web","subtitle":"../web"},{"id":"api","title":"Api"}]"#,
        )
        .unwrap();
        assert_eq!(out.items.len(), 2);
        assert_eq!(out.items[1].id, "api");
        assert_eq!(out.items[0].subtitle.as_deref(), Some("../web"));
    }

    #[test]
    fn parse_output_empty_is_ok_empty() {
        assert_eq!(
            parse_provider_output("").unwrap(),
            ProviderOutput::default()
        );
        assert_eq!(
            parse_provider_output("   \n ").unwrap(),
            ProviderOutput::default()
        );
    }

    #[test]
    fn parse_output_malformed_is_err() {
        assert!(parse_provider_output("not json").is_err());
        assert!(parse_provider_output(r#"{"items":[{"title":"no id"}]}"#).is_err());
    }

    #[test]
    fn parse_output_error_names_expected_shape() {
        // A bare tab id is valid JSON but the wrong shape; the message
        // should name the menu shape rather than a raw serde error.
        let err = parse_provider_output("8").unwrap_err();
        assert!(err.contains("not a valid menu"), "got: {err}");
        assert!(err.contains("items"), "got: {err}");
    }

    #[test]
    fn parse_activate_ignores_non_json_side_effect_output() {
        // `roostctl tab open` prints the new tab id; a path or log line is
        // likewise side-effect noise. All ⇒ empty (palette closes).
        for s in ["8", "8\n", "/some/path\n", "opened tab 8\n", "", "   \n "] {
            assert_eq!(
                parse_activate_output(s).unwrap(),
                ProviderOutput::default(),
                "expected empty for {s:?}"
            );
        }
    }

    #[test]
    fn parse_activate_parses_json_drilldown_and_errors_on_malformed() {
        // JSON-shaped output is a sub-menu: parsed when valid…
        let out = parse_activate_output(r#"{"items":[{"id":"a","title":"A"}]}"#).unwrap();
        assert_eq!(out.items.len(), 1);
        assert_eq!(
            parse_activate_output(r#"[{"id":"a","title":"A"}]"#)
                .unwrap()
                .items
                .len(),
            1
        );
        // …and still an error when it looks like JSON but isn't valid.
        assert!(parse_activate_output(r#"{"items":[{"title":"no id"}]}"#).is_err());
        assert!(parse_activate_output("{ broken").is_err());
    }

    #[test]
    fn provider_items_map_and_index_round_trip() {
        let providers = vec![
            p(r#"label="Open shed" run="shed.sh""#),
            p(r#"label="Worktrees" run="wt.sh""#),
        ];
        let items = provider_items(&providers);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "provider:0");
        assert_eq!(items[0].title, "Open shed");
        assert_eq!(items[0].subtitle.as_deref(), Some("shed.sh"));
        assert_eq!(provider_index(&items[0].id), Some(0));
        assert_eq!(provider_index(&items[1].id), Some(1));
    }

    #[test]
    fn provider_items_empty_shows_sentinel() {
        let items = provider_items(&[]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "provider:none");
        assert_eq!(provider_index(&items[0].id), None);
        assert!(!items[0].actionable); // the sentinel is non-selectable
    }

    #[test]
    fn output_palette_items_caps_and_hints() {
        let out = ProviderOutput {
            placeholder: String::new(),
            items: (0..5)
                .map(|i| ProviderOutputItem {
                    id: format!("i{i}"),
                    title: format!("Item {i}"),
                    subtitle: None,
                    actionable: None,
                })
                .collect(),
        };
        let rows = output_palette_items(&out, 3);
        // 3 real rows + 1 overflow hint.
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[2].id, "i2");
        assert!(rows[2].actionable); // absent ⇒ actionable
        assert_eq!(rows[3].id, OVERFLOW_ID);
        assert_eq!(rows[3].title, "… 2 more");
        assert!(!rows[3].actionable); // overflow hint is non-actionable
    }

    #[test]
    fn output_palette_items_under_limit_has_no_hint() {
        let out = ProviderOutput {
            placeholder: String::new(),
            items: vec![ProviderOutputItem {
                id: "a".into(),
                title: "A".into(),
                subtitle: Some("sub".into()),
                actionable: None,
            }],
        };
        let rows = output_palette_items(&out, 100);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].subtitle.as_deref(), Some("sub"));
        assert!(rows.iter().all(|r| r.id != OVERFLOW_ID));
    }

    #[test]
    fn actionable_parses_and_carries_through() {
        // Explicit `actionable:false` on an item parses and propagates to
        // the palette row; omitting it defaults to actionable.
        let out = parse_provider_output(
            r#"{"items":[{"id":"x","title":"X","actionable":false},{"id":"y","title":"Y"}]}"#,
        )
        .unwrap();
        assert_eq!(out.items[0].actionable, Some(false));
        assert_eq!(out.items[1].actionable, None);
        let rows = output_palette_items(&out, 100);
        assert!(!rows[0].actionable);
        assert!(rows[1].actionable);
    }

    #[test]
    fn palette_item_actionable_defaults_true() {
        assert!(PaletteItem::new("a", "A").actionable);
        assert!(!PaletteItem::new("a", "A").with_actionable(false).actionable);
    }
}
