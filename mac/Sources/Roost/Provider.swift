// Dynamic command providers — the pure, AppKit-free model.
//
// A *provider* is a user script Roost runs to populate a palette frame on
// demand, then runs again when the user picks a row. Where a `command =`
// entry (CustomCommand.swift) launches one fixed command in a tab, a
// `provider =` entry produces a dynamic list and then acts on the choice —
// the "open shed" pattern. Mirrors `crates/roost-linux/src/provider.rs`.
//
// This file is the pure half: parsing `provider =` lines + directory
// entries, building the subprocess invocation (argv / env / stdin), and
// parsing the script's stdout into palette rows. The spawn itself,
// off-main + with a timeout, lives in App.swift.
//
// The contract (v1): Roost runs the provider's `run` command twice,
// distinguished by an argv phase (`list`, then `activate`) and
// `ROOST_PROVIDER_PHASE`. `list` prints `{"items":[{id,title,subtitle?}],
// "placeholder?"}` (a bare array is also accepted); `activate` runs with
// `ROOST_SELECTED_ID` set and either prints nothing (close) or more items
// (drill in). Both phases also receive the active-tab context as env vars
// and as a JSON object on stdin.

import Foundation

/// Wall-clock cap on a single provider invocation when none is set.
let providerDefaultTimeoutSecs: UInt64 = 5
/// Most rows a provider's `list` may contribute when none is set.
let providerDefaultLimit = 100
private let providerMaxTimeoutSecs: UInt64 = 60
private let providerMaxLimit = 1000

/// Sentinel id for the "list was truncated" hint row.
let providerOverflowID = "provider:_overflow"

/// One `provider =` entry (or one discovered provider script).
struct Provider: Equatable {
    let label: String
    let run: String
    let title: String
    let timeoutSecs: UInt64
    let limit: Int
    /// `true` for a config `provider = run="…"` entry: `run` is a shell
    /// command (run via `sh -c`, like `command =`). `false` for a
    /// discovered script: `run` is a direct executable path, exec'd as
    /// argv[0] with no shell — no word-splitting / metacharacter / rc
    /// hazards from the path.
    let shellInterpret: Bool
}

/// Which leg of the contract a run is.
enum ProviderPhase: String {
    case list
    case activate
}

/// The active-tab context Roost injects into a provider run.
struct ProviderContext {
    var socket: String = ""
    var query: String = ""
    var selectedID: String?
    var activeTabID: Int64?
    var activeProjectID: Int64?
    var activeCwd: String = ""
    var activeTitle: String = ""
}

/// Parse one `provider =` value into a `Provider`. Returns nil when
/// `label` or `run` is missing/empty. `timeout`/`limit` fall back to the
/// defaults when absent/unparseable, clamped to a sane ceiling. Unknown
/// keys are ignored (forward-compat) — same grammar as `command =`.
func parseProviderLine(_ value: String) -> Provider? {
    var label = ""
    var run = ""
    var title: String?
    var timeoutSecs = providerDefaultTimeoutSecs
    var limit = providerDefaultLimit

    for token in tokenizeCommand(value) {
        guard let eq = token.firstIndex(of: "=") else { continue }
        let key = String(token[..<eq])
        let val = String(token[token.index(after: eq)...])
        switch key {
        case "label": label = val
        case "run": run = val
        case "title": title = val
        case "timeout":
            if let n = UInt64(val) { timeoutSecs = min(max(n, 1), providerMaxTimeoutSecs) }
        case "limit":
            if let n = Int(val) { limit = min(max(n, 1), providerMaxLimit) }
        default: break  // unknown key — forward-compat
        }
    }

    if label.isEmpty || run.isEmpty { return nil }
    let resolvedTitle = (title?.isEmpty == false) ? title! : label
    return Provider(
        label: label, run: run, title: resolvedTitle, timeoutSecs: timeoutSecs, limit: limit,
        shellInterpret: true)  // config `run =` is a shell command
}

/// Build a `Provider` from a discovered executable. `run` is the file's
/// path; the label comes from a `# @roost.label:` header comment if
/// present, else a humanized filename. `header` is the first chunk of the
/// file's text (the caller reads it; this stays I/O-free for testability).
func providerFromFile(path: String, filename: String, header: String) -> Provider {
    let (metaLabel, metaTitle) = providerMetadataFromHeader(header)
    let stem = filename.contains(".") ? String(filename[..<filename.lastIndex(of: ".")!]) : filename
    let label = metaLabel ?? humanizeProviderStem(stem)
    let title = metaTitle ?? label
    return Provider(
        path: path, label: label, title: title
    )
}

extension Provider {
    /// Convenience init for a discovered script (defaults for timeout/limit).
    /// The path is shell-quoted because it's spliced into
    /// `sh -c "<run> <phase>"`: a filename with spaces or shell
    /// metacharacters runs as one path, not word-split or interpreted.
    /// (Config `run =` strings stay shell-interpreted, like `command =`.)
    fileprivate init(path: String, label: String, title: String) {
        // Raw path, exec'd directly (shellInterpret = false) — no shell, so
        // spaces/metacharacters in the path are safe without quoting.
        self.init(
            label: label, run: path, title: title,
            timeoutSecs: providerDefaultTimeoutSecs, limit: providerDefaultLimit,
            shellInterpret: false)
    }
}

/// Pull `# @roost.label:` / `# @roost.title:` overrides out of a script's
/// leading comment lines. Stops at the first non-blank, non-comment line.
private func providerMetadataFromHeader(_ header: String) -> (label: String?, title: String?) {
    var label: String?
    var title: String?
    for line in header.split(separator: "\n", omittingEmptySubsequences: false) {
        let t = line.trimmingCharacters(in: .whitespaces)
        if t.isEmpty { continue }
        guard t.hasPrefix("#") else { break }  // first real line ends the header
        let comment = String(t.dropFirst()).trimmingCharacters(in: .whitespaces)
        if let rest = comment.dropPrefixIfPresent("@roost.label:") {
            let v = rest.trimmingCharacters(in: .whitespaces)
            if !v.isEmpty { label = v }
        } else if let rest = comment.dropPrefixIfPresent("@roost.title:") {
            let v = rest.trimmingCharacters(in: .whitespaces)
            if !v.isEmpty { title = v }
        }
    }
    return (label, title)
}

private extension String {
    func dropPrefixIfPresent(_ prefix: String) -> String? {
        hasPrefix(prefix) ? String(dropFirst(prefix.count)) : nil
    }
}

/// Turn `shed-open_logs` into `shed open logs` for a default label.
private func humanizeProviderStem(_ stem: String) -> String {
    stem.replacingOccurrences(of: "-", with: " ")
        .replacingOccurrences(of: "_", with: " ")
        .trimmingCharacters(in: .whitespaces)
}

/// Build the argv that runs a provider phase: `[shell, "-c", "<run>
/// <phase>"]`. Non-interactive (`-c`, not `-i`) so the user's rc can't
/// echo onto stdout and corrupt the JSON the script prints.
func providerInvocationArgv(shell: String, run: String, shellInterpret: Bool, phase: ProviderPhase)
    -> [String]
{
    if shellInterpret {
        return [shell, "-c", "\(run) \(phase.rawValue)"]
    }
    // Direct exec: `run` is a path (argv[0]), phase is argv[1]. No shell,
    // so no word-splitting / metacharacter interpretation, and no rc echo
    // to corrupt the JSON the script prints.
    return [run, phase.rawValue]
}

/// Build the env pairs Roost layers onto a provider run — flat, jq-free
/// access to the same context the stdin JSON carries.
func providerInvocationEnv(phase: ProviderPhase, ctx: ProviderContext) -> [(String, String)] {
    var env: [(String, String)] = [
        ("ROOST_PROVIDER_PHASE", phase.rawValue),
        ("ROOST_SOCKET", ctx.socket),
        ("ROOST_QUERY", ctx.query),
        ("ROOST_ACTIVE_CWD", ctx.activeCwd),
        ("ROOST_ACTIVE_TITLE", ctx.activeTitle),
    ]
    if let id = ctx.activeTabID { env.append(("ROOST_ACTIVE_TAB_ID", String(id))) }
    if let id = ctx.activeProjectID { env.append(("ROOST_ACTIVE_PROJECT_ID", String(id))) }
    if let sel = ctx.selectedID { env.append(("ROOST_SELECTED_ID", sel)) }
    return env
}

private struct ProviderActiveTabJSON: Encodable {
    let id: Int64?
    let project_id: Int64?
    let cwd: String
    let title: String
}

private struct ProviderInputJSON: Encodable {
    let v: Int
    let phase: String
    let selected_id: String?
    let query: String
    let active_tab: ProviderActiveTabJSON
    let socket: String
}

/// Serialize the full context as the JSON object Roost writes to the
/// provider's stdin. Ends in a newline.
func providerInvocationStdin(phase: ProviderPhase, ctx: ProviderContext) -> String {
    let input = ProviderInputJSON(
        v: 1,
        phase: phase.rawValue,
        selected_id: ctx.selectedID,
        query: ctx.query,
        active_tab: ProviderActiveTabJSON(
            id: ctx.activeTabID, project_id: ctx.activeProjectID,
            cwd: ctx.activeCwd, title: ctx.activeTitle),
        socket: ctx.socket)
    let encoder = JSONEncoder()
    guard let data = try? encoder.encode(input), let s = String(data: data, encoding: .utf8) else {
        return "{}\n"
    }
    return s + "\n"
}

/// A provider run failure (spawn error, non-zero exit, timeout, or
/// unparseable stdout), carried so the palette can show it as a row.
struct ProviderError: Error, Sendable {
    let message: String
}

/// One row a provider's stdout contributes.
struct ProviderOutputItem: Decodable, Equatable, Sendable {
    let id: String
    let title: String
    let subtitle: String?
    /// When `false`, the row renders but can't be selected (the palette
    /// stays open) — for empty/disabled states like "No results". Absent
    /// (nil) ⇒ actionable.
    let actionable: Bool?
}

/// A provider's parsed stdout: the rows plus optional palette chrome.
struct ProviderOutput: Decodable, Equatable, Sendable {
    var placeholder: String
    var items: [ProviderOutputItem]

    init(placeholder: String = "", items: [ProviderOutputItem] = []) {
        self.placeholder = placeholder
        self.items = items
    }

    enum CodingKeys: String, CodingKey { case placeholder, items }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        placeholder = (try? c.decodeIfPresent(String.self, forKey: .placeholder)) ?? ""
        // Absent `items` defaults to empty (a valid "done, close" signal);
        // present-but-malformed throws so the caller surfaces an error row
        // — matching the Rust `#[serde(default)]` + required-`id` behavior.
        if c.contains(.items) {
            items = try c.decode([ProviderOutputItem].self, forKey: .items)
        } else {
            items = []
        }
    }
}

/// Parse a provider's stdout. Empty/blank output is an empty result (a
/// valid "done, close" signal on activate). Accepts either the object
/// form (`{"items":[…]}`) or a bare array (`[…]`, dmenu-style). Throws on
/// malformed JSON so the caller can surface it as an error row.
func parseProviderOutput(_ stdout: String) throws -> ProviderOutput {
    let trimmed = stdout.trimmingCharacters(in: .whitespacesAndNewlines)
    if trimmed.isEmpty { return ProviderOutput() }
    let data = Data(trimmed.utf8)
    let decoder = JSONDecoder()
    if trimmed.hasPrefix("[") {
        let items = try decoder.decode([ProviderOutputItem].self, from: data)
        return ProviderOutput(placeholder: "", items: items)
    }
    return try decoder.decode(ProviderOutput.self, from: data)
}

/// Build the palette rows that list the configured providers (the "Custom
/// Commands" frame). Each row's id is `provider:<i>`, parsed back by
/// `providerIndex` on confirm. Empty → a single non-actionable sentinel.
func providerItems(_ providers: [Provider]) -> [PaletteItem] {
    if providers.isEmpty {
        return [
            PaletteItem(
                id: "provider:none", title: "No providers configured",
                subtitle: "Add `provider = …` to config.conf", actionable: false)
        ]
    }
    return providers.enumerated().map { i, p in
        PaletteItem(id: "provider:\(i)", title: p.label, subtitle: p.run)
    }
}

/// Parse a provider row id (`provider:<index>`) back to the index. The
/// `provider:none` sentinel and any malformed id return nil.
func providerIndex(_ id: String) -> Int? {
    guard id.hasPrefix("provider:") else { return nil }
    return Int(id.dropFirst("provider:".count))
}

/// Turn a provider's parsed output into palette rows, capped at `limit`.
/// Extras are dropped and a non-actionable hint row appended rather than
/// silently truncating.
func providerOutputPaletteItems(_ out: ProviderOutput, limit: Int) -> [PaletteItem] {
    var rows = out.items.prefix(limit).map {
        PaletteItem(
            id: $0.id, title: $0.title, subtitle: $0.subtitle, actionable: $0.actionable ?? true)
    }
    if out.items.count > limit {
        let extra = out.items.count - limit
        rows.append(
            PaletteItem(
                id: providerOverflowID, title: "… \(extra) more", subtitle: "refine your query",
                actionable: false))
    }
    return rows
}
