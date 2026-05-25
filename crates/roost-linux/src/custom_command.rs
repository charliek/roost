//! Custom command launcher — the pure, GTK-free model.
//!
//! Parses the repeated `command =` config key into [`CustomCommand`]s,
//! builds the login-shell argv that runs one in a fresh tab, and turns a
//! command list into launcher [`PaletteItem`]s. Kept split from the
//! GTK/app layer (`app.rs`) so the tokenizer + argv builder are
//! unit-tested in isolation, mirroring
//! `mac/Sources/Roost/CustomCommand.swift`.
//!
//! Format — one record per `command =` line; the value is
//! whitespace-separated `key="value"` (or `key=value`) tokens, where a
//! `"` groups spaces and is stripped. There is no escaping inside quotes
//! in v1 (a value can't contain a literal `"`).
//!
//! ```conf
//! command = label="Lazygit" run="lazygit"
//! command = label="Logs" run="docker compose logs -f" hold=true env="RUST_LOG=debug"
//! ```

use crate::palette::PaletteItem;

/// One launcher entry. `label` + `run` are required (a line missing
/// either parses to `None`); `title` defaults to `label` when absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    pub label: String,
    pub run: String,
    pub title: String,
    pub env: Vec<(String, String)>,
    pub hold: bool,
}

/// Parse one `command =` value into a [`CustomCommand`]. Returns `None`
/// when `label` or `run` is missing/empty so the caller can skip the line
/// with a warning. Unknown keys are ignored (forward-compat).
pub fn parse_command_line(value: &str) -> Option<CustomCommand> {
    let mut label = String::new();
    let mut run = String::new();
    let mut title: Option<String> = None;
    let mut env: Vec<(String, String)> = Vec::new();
    let mut hold = false;

    for token in tokenize(value) {
        // Each token splits on its FIRST `=` into key/value. A bare
        // token with no `=` is only meaningful as `hold` (⇒ true).
        match token.split_once('=') {
            Some((key, val)) => match key {
                "label" => label = val.to_string(),
                "run" => run = val.to_string(),
                "title" => title = Some(val.to_string()),
                "hold" => hold = val.eq_ignore_ascii_case("true"),
                "env" => parse_env_into(val, &mut env),
                _ => {} // unknown key — forward-compat
            },
            None => {
                if token == "hold" {
                    hold = true;
                }
            }
        }
    }

    if label.is_empty() || run.is_empty() {
        return None;
    }
    let title = title
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| label.clone());
    Some(CustomCommand {
        label,
        run,
        title,
        env,
        hold,
    })
}

/// Build the login-shell argv that runs `cmd`: `[shell, "-i", "-c",
/// inner]`. `inner` exports each env pair (single-quoted) then runs
/// `cmd.run`; with `hold`, appends `; exec <shell> -i` so the tab drops
/// to a fresh interactive shell instead of closing when `run` exits.
/// Running through `$SHELL -i -c` sources the user's rc (so `PATH`/env
/// match a normal tab) and lets `run` use shell features.
pub fn launch_argv(shell: &str, cmd: &CustomCommand) -> Vec<String> {
    let mut parts: Vec<String> = Vec::with_capacity(cmd.env.len() + 1);
    for (k, v) in &cmd.env {
        parts.push(format!("export {}={}", k, shell_single_quote(v)));
    }
    parts.push(cmd.run.clone());
    let mut inner = parts.join("; ");
    if cmd.hold {
        inner.push_str(&format!("; exec {shell} -i"));
    }
    vec![shell.to_string(), "-i".to_string(), "-c".to_string(), inner]
}

/// Wrap `s` in single quotes for safe inclusion in a shell command,
/// escaping embedded single quotes as `'\''` (close-quote, escaped
/// literal quote, reopen-quote — the POSIX idiom).
pub fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Build the launcher palette rows from the configured commands. Each row
/// encodes its index as `launch:<i>` (parsed back by [`launch_index`] on
/// confirm), shows `run` as the subtitle, and tags `hold` commands. An
/// empty list yields a single non-actionable "No commands configured"
/// sentinel row.
pub fn launcher_items(commands: &[CustomCommand]) -> Vec<PaletteItem> {
    if commands.is_empty() {
        return vec![PaletteItem::new("launch:none", "No commands configured")
            .with_subtitle(Some("Add `command = …` to config.conf".to_string()))];
    }
    commands
        .iter()
        .enumerate()
        .map(|(i, c)| {
            PaletteItem::new(format!("launch:{i}"), c.label.clone())
                .with_subtitle(Some(c.run.clone()))
                .with_trailing(if c.hold {
                    Some("hold".to_string())
                } else {
                    None
                })
        })
        .collect()
}

/// Parse a launcher row id (`launch:<index>`) back to the command index.
/// The `launch:none` sentinel and any malformed id return `None`.
pub fn launch_index(id: &str) -> Option<usize> {
    id.strip_prefix("launch:")
        .and_then(|n| n.parse::<usize>().ok())
}

/// Quote-aware tokenizer: a `"` toggles quote mode (and is dropped),
/// unquoted whitespace ends a token, everything else accumulates. Empty
/// tokens (a stray `""`) are dropped.
fn tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for ch in s.chars() {
        match ch {
            '"' => in_quote = !in_quote,
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Split an `env` value into `K=V` pairs on whitespace. Each pair splits
/// on its first `=`; a pair whose key isn't a valid env-var identifier is
/// dropped (the value is single-quoted in `launch_argv`, but the key is
/// spliced into `export K=…` verbatim, so an arbitrary key could inject
/// shell — reject anything that isn't `[A-Za-z_][A-Za-z0-9_]*`).
fn parse_env_into(val: &str, env: &mut Vec<(String, String)>) {
    for pair in val.split_whitespace() {
        if let Some((k, v)) = pair.split_once('=') {
            if is_valid_env_key(k) {
                env.push((k.to_string(), v.to_string()));
            }
        }
    }
}

/// A POSIX-ish env-var name: non-empty, first char `[A-Za-z_]`, rest
/// `[A-Za-z0-9_]`.
fn is_valid_env_key(k: &str) -> bool {
    let mut chars = k.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(value: &str) -> CustomCommand {
        parse_command_line(value).expect("expected a valid command")
    }

    #[test]
    fn parses_simple_label_and_run() {
        let c = cmd(r#"label="Claude" run="claude --resume""#);
        assert_eq!(c.label, "Claude");
        assert_eq!(c.run, "claude --resume");
        // title defaults to label.
        assert_eq!(c.title, "Claude");
        assert!(c.env.is_empty());
        assert!(!c.hold);
    }

    #[test]
    fn quoted_values_keep_internal_spaces() {
        let c = cmd(r#"label="Logs" run="docker compose logs -f""#);
        assert_eq!(c.run, "docker compose logs -f");
    }

    #[test]
    fn unquoted_values_parse() {
        let c = cmd("label=Build run=make");
        assert_eq!(c.label, "Build");
        assert_eq!(c.run, "make");
    }

    #[test]
    fn explicit_title_overrides_label() {
        let c = cmd(r#"label="Logs" run="lazygit" title="git""#);
        assert_eq!(c.title, "git");
    }

    #[test]
    fn hold_true_and_bare_and_false() {
        assert!(cmd(r#"label="a" run="b" hold=true"#).hold);
        assert!(cmd(r#"label="a" run="b" hold"#).hold);
        assert!(!cmd(r#"label="a" run="b" hold=false"#).hold);
        // absent ⇒ false
        assert!(!cmd(r#"label="a" run="b""#).hold);
    }

    #[test]
    fn single_env_pair() {
        let c = cmd(r#"label="a" run="b" env="RUST_LOG=debug""#);
        assert_eq!(c.env, vec![("RUST_LOG".to_string(), "debug".to_string())]);
    }

    #[test]
    fn multiple_env_pairs() {
        let c = cmd(r#"label="a" run="b" env="A=1 B=2""#);
        assert_eq!(
            c.env,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2".to_string()),
            ]
        );
    }

    #[test]
    fn env_key_must_be_identifier() {
        // A key that isn't a valid identifier is dropped (it would
        // otherwise splice into `export K=…` verbatim and inject shell).
        let c = cmd(r#"label="a" run="b" env="GOOD=1 bad-key=2 A;rm=3 OK_2=4""#);
        assert_eq!(
            c.env,
            vec![
                ("GOOD".to_string(), "1".to_string()),
                ("OK_2".to_string(), "4".to_string()),
            ]
        );
    }

    #[test]
    fn unknown_key_is_ignored() {
        let c = cmd(r#"label="a" run="b" icon="star" quickkey="1""#);
        assert_eq!(c.label, "a");
        assert_eq!(c.run, "b");
    }

    #[test]
    fn missing_label_is_none() {
        assert!(parse_command_line(r#"run="claude""#).is_none());
    }

    #[test]
    fn missing_run_is_none() {
        assert!(parse_command_line(r#"label="Claude""#).is_none());
    }

    #[test]
    fn empty_run_is_none() {
        assert!(parse_command_line(r#"label="Claude" run="""#).is_none());
    }

    #[test]
    fn launch_argv_non_hold() {
        let c = cmd(r#"label="a" run="echo hi""#);
        assert_eq!(
            launch_argv("/bin/zsh", &c),
            vec![
                "/bin/zsh".to_string(),
                "-i".to_string(),
                "-c".to_string(),
                "echo hi".to_string(),
            ]
        );
    }

    #[test]
    fn launch_argv_hold_appends_exec_shell() {
        let c = cmd(r#"label="a" run="make" hold=true"#);
        let argv = launch_argv("/bin/zsh", &c);
        assert!(
            argv[3].ends_with("; exec /bin/zsh -i"),
            "inner was {:?}",
            argv[3]
        );
        assert!(argv[3].starts_with("make"));
    }

    #[test]
    fn launch_argv_env_exports_before_run() {
        let c = cmd(r#"label="a" run="echo $K" env="K=v""#);
        let argv = launch_argv("/bin/sh", &c);
        assert_eq!(argv[3], "export K='v'; echo $K");
    }

    #[test]
    fn launch_argv_env_and_hold_combine() {
        let c = cmd(r#"label="a" run="cmd" env="A=1 B=2" hold=true"#);
        let argv = launch_argv("/bin/sh", &c);
        assert_eq!(argv[3], "export A='1'; export B='2'; cmd; exec /bin/sh -i");
    }

    #[test]
    fn shell_single_quote_escapes_embedded_quote() {
        assert_eq!(shell_single_quote("plain"), "'plain'");
        // a'b ⇒ 'a'\''b'
        assert_eq!(shell_single_quote("a'b"), r#"'a'\''b'"#);
    }

    #[test]
    fn launcher_items_map_commands() {
        let commands = vec![
            cmd(r#"label="Claude" run="claude --resume""#),
            cmd(r#"label="Build" run="make" hold=true"#),
        ];
        let items = launcher_items(&commands);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "launch:0");
        assert_eq!(items[0].title, "Claude");
        assert_eq!(items[0].subtitle.as_deref(), Some("claude --resume"));
        assert_eq!(items[0].trailing_text, None);
        assert_eq!(items[1].id, "launch:1");
        assert_eq!(items[1].trailing_text.as_deref(), Some("hold"));
    }

    #[test]
    fn launcher_items_empty_shows_sentinel() {
        let items = launcher_items(&[]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "launch:none");
        assert_eq!(items[0].title, "No commands configured");
        assert!(items[0].subtitle.is_some());
    }

    #[test]
    fn launch_index_parses_and_rejects() {
        assert_eq!(launch_index("launch:0"), Some(0));
        assert_eq!(launch_index("launch:12"), Some(12));
        assert_eq!(launch_index("launch:none"), None);
        assert_eq!(launch_index("notif:3"), None);
        assert_eq!(launch_index("launch:"), None);
    }
}
