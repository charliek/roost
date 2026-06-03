// Custom command launcher — the pure, AppKit-free model.
//
// Parses the repeated `command =` config key into `CustomCommand`s,
// builds the login-shell argv that runs one in a fresh tab, and turns a
// command list into launcher `PaletteItem`s. Kept split from the App /
// PTY layer so the tokenizer + argv builder are unit-tested in isolation,
// mirroring `crates/roost-linux/src/custom_command.rs`.
//
// Format — one record per `command =` line; the value is
// whitespace-separated `key="value"` (or `key=value`) tokens, where a
// `"` groups spaces and is stripped. There is no escaping inside quotes
// in v1 (a value can't contain a literal `"`).
//
//   command = label="Lazygit" run="lazygit"
//   command = label="Logs" run="docker compose logs -f" hold=true env="RUST_LOG=debug"

import Foundation

/// One launcher entry. `label` + `run` are required (a line missing
/// either parses to `nil`); `title` defaults to `label` when absent.
struct CustomCommand: Equatable {
    let label: String
    let run: String
    let title: String
    let env: [(String, String)]
    let hold: Bool

    // `[(String, String)]` isn't auto-Equatable; compare element-wise.
    static func == (lhs: CustomCommand, rhs: CustomCommand) -> Bool {
        lhs.label == rhs.label && lhs.run == rhs.run && lhs.title == rhs.title
            && lhs.hold == rhs.hold
            && lhs.env.count == rhs.env.count
            && zip(lhs.env, rhs.env).allSatisfy { $0.0 == $1.0 && $0.1 == $1.1 }
    }
}

/// Parse one `command =` value into a `CustomCommand`. Returns nil when
/// `label` or `run` is missing/empty so the caller can skip the line with
/// a warning. Unknown keys are ignored (forward-compat).
func parseCommandLine(_ value: String) -> CustomCommand? {
    var label = ""
    var run = ""
    var title: String?
    var env: [(String, String)] = []
    var hold = false

    for token in tokenizeCommand(value) {
        // Each token splits on its FIRST `=` into key/value. A bare
        // token with no `=` is only meaningful as `hold` (⇒ true).
        if let eq = token.firstIndex(of: "=") {
            let key = String(token[..<eq])
            let val = String(token[token.index(after: eq)...])
            switch key {
            case "label": label = val
            case "run": run = val
            case "title": title = val
            case "hold": hold = val.lowercased() == "true"
            case "env": parseEnv(val, into: &env)
            default: break  // unknown key — forward-compat
            }
        } else if token == "hold" {
            hold = true
        }
    }

    if label.isEmpty || run.isEmpty {
        return nil
    }
    let resolvedTitle = (title?.isEmpty == false) ? title! : label
    return CustomCommand(label: label, run: run, title: resolvedTitle, env: env, hold: hold)
}

/// Build the login-shell argv that runs `command`: `[shell, "-i", "-c",
/// inner]`. `inner` exports each env pair (single-quoted) then runs
/// `command.run`; with `hold`, appends `; exec <shell> -i` so the tab
/// drops to a fresh interactive shell instead of closing when `run`
/// exits. Running through `$SHELL -i -c` sources the user's rc (so
/// `PATH`/env match a normal tab) and lets `run` use shell features.
func launchArgv(shell: String, command: CustomCommand) -> [String] {
    var parts: [String] = []
    for (k, v) in command.env {
        parts.append("export \(k)=\(shellSingleQuote(v))")
    }
    parts.append(command.run)
    var inner = parts.joined(separator: "; ")
    if command.hold {
        inner += "; exec \(shell) -i"
    }
    return [shell, "-i", "-c", inner]
}

/// Wrap `s` in single quotes for safe inclusion in a shell command,
/// escaping embedded single quotes as `'\''` (close-quote, escaped
/// literal quote, reopen-quote — the POSIX idiom).
func shellSingleQuote(_ s: String) -> String {
    "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
}

/// Build the launcher palette rows from the configured commands. Each row
/// encodes its index as `launch:<i>` (parsed back by `launchIndex` on
/// confirm), shows `run` as the subtitle, and tags `hold` commands. An
/// empty list yields a single non-actionable "No commands configured"
/// sentinel row.
func launcherItems(_ commands: [CustomCommand]) -> [PaletteItem] {
    if commands.isEmpty {
        return [
            PaletteItem(
                id: "launch:none",
                title: "No commands configured",
                subtitle: "Add `command = …` to config.conf"
            )
        ]
    }
    return commands.enumerated().map { i, c in
        PaletteItem(
            id: "launch:\(i)",
            title: c.label,
            subtitle: c.run,
            trailingText: c.hold ? "hold" : nil
        )
    }
}

/// Parse a launcher row id (`launch:<index>`) back to the command index.
/// The `launch:none` sentinel and any malformed id return nil.
func launchIndex(_ id: String) -> Int? {
    guard id.hasPrefix("launch:") else { return nil }
    return Int(id.dropFirst("launch:".count))
}

/// Quote-aware tokenizer: a `"` toggles quote mode (and is dropped),
/// unquoted whitespace ends a token, everything else accumulates. Empty
/// tokens (a stray `""`) are dropped. Shared with `Provider.swift`, which
/// parses `provider =` records in the same `key="value"` grammar.
func tokenizeCommand(_ s: String) -> [String] {
    var tokens: [String] = []
    var cur = ""
    var inQuote = false
    for ch in s {
        if ch == "\"" {
            inQuote.toggle()
        } else if ch.isWhitespace && !inQuote {
            if !cur.isEmpty {
                tokens.append(cur)
                cur = ""
            }
        } else {
            cur.append(ch)
        }
    }
    if !cur.isEmpty {
        tokens.append(cur)
    }
    return tokens
}

/// Split an `env` value into `K=V` pairs on whitespace. Each pair splits
/// on its first `=`; a pair whose key isn't a valid env-var identifier is
/// dropped (the value is single-quoted in `launchArgv`, but the key is
/// spliced into `export K=…` verbatim, so an arbitrary key could inject
/// shell — reject anything that isn't `[A-Za-z_][A-Za-z0-9_]*`).
private func parseEnv(_ val: String, into env: inout [(String, String)]) {
    for pair in val.split(whereSeparator: { $0.isWhitespace }) {
        guard let eq = pair.firstIndex(of: "=") else { continue }
        let k = String(pair[..<eq])
        let v = String(pair[pair.index(after: eq)...])
        if isValidEnvKey(k) {
            env.append((k, v))
        }
    }
}

/// A POSIX-ish env-var name: non-empty, first char `[A-Za-z_]`, rest
/// `[A-Za-z0-9_]`.
private func isValidEnvKey(_ k: String) -> Bool {
    func isStart(_ c: Character) -> Bool { c == "_" || (c.isASCII && c.isLetter) }
    func isRest(_ c: Character) -> Bool { c == "_" || (c.isASCII && (c.isLetter || c.isNumber)) }
    guard let first = k.first, isStart(first) else { return false }
    return k.dropFirst().allSatisfy(isRest)
}
