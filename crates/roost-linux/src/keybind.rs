//! Keybind trigger parser + default action table.
//!
//! Mirrors `mac/Sources/Roost/Keybind.swift` (Phase 6 P1) in shape.
//! Defaults use `alt` as Linux's app modifier (Mac uses `super`/`cmd`);
//! `ctrl` is left to the shell apart from `Ctrl+1‑9` (switch_tab) and
//! the `Ctrl+Shift+C/V` copy/paste alternates. Everything else —
//! modifier-alias rules, `unbind` semantics, action namespace — is
//! shared with the Mac UI so config files apply verbatim across both UIs.

use std::collections::HashMap;

/// Every recognized action. Adding a new action: extend the enum,
/// extend `Self::name`, extend `Self::from_name`, and provide a
/// handler in `App::install_keybinds`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeybindAction {
    NewTab,
    CloseTab,
    NewProject,
    /// M8: rename the active project — flips its sidebar row's
    /// `gtk::Stack` to the inline `gtk::Entry` (M9). Default
    /// `projectMod+shift+r`.
    RenameProject,
    /// M8: rename the active tab — opens a `gtk::Popover` over the
    /// pill with an inline `gtk::Entry` (M9). Default `projectMod+r`.
    RenameTab,
    /// M8: close the active project (with an `adw::AlertDialog`
    /// confirmation, since this cascades to close every tab in the
    /// project). Default `projectMod+shift+w`. Named "Close" (matching
    /// the Mac UI): it removes the project + its tabs from the
    /// workspace; nothing on disk is deleted. The wire op is still
    /// `project.delete`.
    CloseProject,
    /// Focus the next tab with a pending notification — active project
    /// first (from after the focused tab), then other projects in order.
    /// A multi-project triage shortcut; default `primary+shift+u`
    /// (parity with the Mac UI).
    JumpToUnread,
    CycleTabPrev,
    CycleTabNext,
    Copy,
    Paste,
    ToggleSidebar,
    /// Browser-style font sizing on the active tab's terminal.
    /// Defaults to `primary+plus`/`primary+equal` (both are bound
    /// because `Cmd-+` on US layouts is really `Cmd-Shift-=` and
    /// users frequently hit `Cmd-=` without shift). Per-tab font
    /// adjusters.
    FontIncrease,
    /// Default `primary+minus`.
    FontDecrease,
    /// Default `primary+0`. Resets to the config-file default
    /// (or `cell_metrics::DEFAULT_FONT_SIZE_PT` if no config).
    FontReset,
    /// Open the command palette (VS Code / Zed–style `Cmd+Shift+P`
    /// overlay). Default `projectMod+shift+p` — Cmd+Shift+P on
    /// macOS-GTK, Alt+Shift+P on Linux.
    CommandPalette,
    /// Open the custom command launcher (config-defined `command =`
    /// list) on its own picker. Default `projectMod+shift+t` —
    /// Cmd+Shift+T on macOS-GTK, Alt+Shift+T on Linux.
    CommandLauncher,
    /// Open the custom palette (config-defined `provider =` list, plus
    /// any discovered provider scripts) — the dynamic, script-backed
    /// picker. Default `projectMod+shift+e` — Cmd+Shift+E on macOS-GTK,
    /// Alt+Shift+E on Linux. (`…+shift+r` is RenameProject; users who
    /// prefer the "R for Run" mnemonic rebind via config.)
    CustomPalette,
    /// Unbind a trigger; removes any default action attached to it.
    Unbind,
    /// `switch_project_N` where N is 1..=9.
    SwitchProject(u8),
    /// `switch_tab_N` where N is 1..=9.
    SwitchTab(u8),
}

impl KeybindAction {
    /// Parse an action name from a config-file string.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "new_tab" => Some(Self::NewTab),
            "close_tab" => Some(Self::CloseTab),
            "new_project" => Some(Self::NewProject),
            "rename_project" => Some(Self::RenameProject),
            "rename_tab" => Some(Self::RenameTab),
            // `delete_project` kept as an alias so any existing config
            // binding still resolves after the rename to `close_project`.
            "close_project" | "delete_project" => Some(Self::CloseProject),
            "jump_to_unread" => Some(Self::JumpToUnread),
            "cycle_tab_prev" => Some(Self::CycleTabPrev),
            "cycle_tab_next" => Some(Self::CycleTabNext),
            "copy" => Some(Self::Copy),
            "paste" => Some(Self::Paste),
            "toggle_sidebar" => Some(Self::ToggleSidebar),
            "font_increase" => Some(Self::FontIncrease),
            "font_decrease" => Some(Self::FontDecrease),
            "font_reset" => Some(Self::FontReset),
            "command_palette" => Some(Self::CommandPalette),
            "command_launcher" => Some(Self::CommandLauncher),
            "custom_palette" => Some(Self::CustomPalette),
            "unbind" => Some(Self::Unbind),
            other => {
                if let Some(n) = other.strip_prefix("switch_project_") {
                    n.parse::<u8>()
                        .ok()
                        .filter(|n| (1..=9).contains(n))
                        .map(Self::SwitchProject)
                } else if let Some(n) = other.strip_prefix("switch_tab_") {
                    n.parse::<u8>()
                        .ok()
                        .filter(|n| (1..=9).contains(n))
                        .map(Self::SwitchTab)
                } else {
                    None
                }
            }
        }
    }
}

/// Canonical accelerator triple: modifier bitmask + a lowercased key
/// name. `key` follows GTK accelerator syntax (e.g. "t", "1",
/// "bracketleft", "Tab").
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Accel {
    pub modifiers: AccelMods,
    pub key: String,
}

bitflags::bitflags! {
    /// Modifier bitmask. Matches `gdk::ModifierType` for Shift / Ctrl /
    /// Alt / Super; carried as our own type so the parser doesn't
    /// pull in gtk4 as a dep.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct AccelMods: u8 {
        const SHIFT = 1;
        const CTRL  = 2;
        const ALT   = 4;
        const SUPER = 8;
    }
}

/// Parse a Ghostty-style trigger ("ctrl+shift+t", "alt+1") into an
/// [`Accel`]. Returns `None` for unparseable input so the canonicalizer
/// can warn + fall through to the default.
pub fn parse_trigger(trigger: &str) -> Option<Accel> {
    let trigger = trigger.trim();
    if trigger.is_empty() {
        return None;
    }
    let mut mods = AccelMods::empty();
    let mut last: Option<&str> = None;
    for part in trigger.split('+') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        match part.to_ascii_lowercase().as_str() {
            "shift" => mods |= AccelMods::SHIFT,
            "ctrl" | "control" => mods |= AccelMods::CTRL,
            "alt" | "opt" | "option" => mods |= AccelMods::ALT,
            "super" | "cmd" | "command" => mods |= AccelMods::SUPER,
            _ => last = Some(part),
        }
    }
    let key = last?.to_ascii_lowercase();
    Some(Accel {
        modifiers: mods,
        key,
    })
}

/// The modifier that — held during a mouse hover/click over a URL —
/// reveals the link (underline + hand cursor) and opens it on click.
/// Platform-aware, matching each desktop's "open link" convention and
/// the app's own keybind scheme:
///
/// * macOS: `Super` (Cmd) — parity with the Swift UI and native Mac
///   apps. (At the GDK layer the Command key arrives as Meta/Super; the
///   GTK widget maps both, see `TerminalView`'s link-modifier check.)
/// * Linux: `Alt` — the GTK app's single "primary" modifier (mirrors
///   `default_bindings`), leaving `Ctrl` to the shell/readline.
///
/// Users override via `link-modifier = ctrl|alt|super` in
/// `~/.config/roost/config.conf` (parsed by [`parse_link_modifier`]).
/// Linux users who want the traditional Ctrl+click set
/// `link-modifier = ctrl`.
pub fn default_link_modifier() -> AccelMods {
    if cfg!(target_os = "macos") {
        AccelMods::SUPER
    } else {
        AccelMods::ALT
    }
}

/// Parse a `link-modifier` config value into a single modifier flag.
/// Accepts the same spellings as [`parse_trigger`]'s modifier tokens
/// (`ctrl`/`control`, `alt`/`opt`/`option`, `super`/`cmd`/`command`/
/// `meta`). Returns `None` for anything else so the caller can warn and
/// keep [`default_link_modifier`].
pub fn parse_link_modifier(value: &str) -> Option<AccelMods> {
    match value.trim().to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(AccelMods::CTRL),
        "alt" | "opt" | "option" => Some(AccelMods::ALT),
        "super" | "cmd" | "command" | "meta" => Some(AccelMods::SUPER),
        _ => None,
    }
}

/// Resolve the effective link modifier: the config override when set,
/// else the platform default.
pub fn resolve_link_modifier(config_override: Option<AccelMods>) -> AccelMods {
    config_override.unwrap_or_else(default_link_modifier)
}

/// Default bindings table — host-platform aware:
///
/// * Linux: `primary = projectMod = clipboardMod = alt`. `Alt` is the
///   single app modifier (the role `super`/Cmd plays on macOS), leaving
///   `Ctrl` to the shell/readline. The only `Ctrl` defaults are
///   `Ctrl+1‑9` (switch_tab, hardcoded below so it stays distinct from
///   `Alt+1‑9` switch_project) and the `Ctrl+Shift+C/V` copy/paste
///   alternates.
/// * macOS: `primary = super` (Cmd), `projectMod = super`,
///   `clipboardMod = super`.
///
/// The GTK app is the Linux UI, but developers commonly run it on
/// macOS Homebrew GTK4 for cross-client testing; flipping the
/// primary modifier on Mac means the GTK app feels native there.
/// Users override anything via `~/.config/roost/config.conf` —
/// the `canonicalize_bindings` layer below preserves that.
pub fn default_bindings() -> Vec<(Accel, KeybindAction)> {
    let (primary, project_mod, clipboard_mod) = if cfg!(target_os = "macos") {
        ("super", "super", "super")
    } else {
        ("alt", "alt", "alt")
    };

    let mut out = Vec::new();
    let add = |out: &mut Vec<(Accel, KeybindAction)>, trig: &str, action: KeybindAction| {
        if let Some(accel) = parse_trigger(trig) {
            out.push((accel, action));
        }
    };

    add(&mut out, &format!("{primary}+t"), KeybindAction::NewTab);
    add(&mut out, &format!("{primary}+w"), KeybindAction::CloseTab);
    add(
        &mut out,
        &format!("{project_mod}+n"),
        KeybindAction::NewProject,
    );
    add(
        &mut out,
        &format!("{project_mod}+r"),
        KeybindAction::RenameTab,
    );
    add(
        &mut out,
        &format!("{project_mod}+shift+r"),
        KeybindAction::RenameProject,
    );
    // Round-4 R3: Mac=⌘⇧W, Linux=Alt+Shift+W. Both surfaces use
    // `project_mod+shift+w` to keep parity with the project-action
    // group (new_project=alt+n, rename_project=alt+shift+r on Linux).
    add(
        &mut out,
        &format!("{project_mod}+shift+w"),
        KeybindAction::CloseProject,
    );
    // Jump to the next unread notification. `primary+shift+u`
    // (Cmd+Shift+U on macOS-GTK, Alt+Shift+U on Linux) — parity with
    // the Mac UI's `jump_to_unread`.
    add(
        &mut out,
        &format!("{primary}+shift+u"),
        KeybindAction::JumpToUnread,
    );

    // Cycle prev/next: Shift+[ and Shift+] map to bracketleft/right on
    // most US layouts; some layouts emit braceleft/right after Shift.
    // Bind both so the keybind fires regardless of layout.
    add(
        &mut out,
        &format!("{primary}+shift+bracketleft"),
        KeybindAction::CycleTabPrev,
    );
    add(
        &mut out,
        &format!("{primary}+shift+braceleft"),
        KeybindAction::CycleTabPrev,
    );
    add(
        &mut out,
        &format!("{primary}+shift+bracketright"),
        KeybindAction::CycleTabNext,
    );
    add(
        &mut out,
        &format!("{primary}+shift+braceright"),
        KeybindAction::CycleTabNext,
    );

    // Clipboard: native modifier first, plus Ctrl+Shift+C/V on every
    // platform as a fallback for users who muscle-memory the
    // X11/terminal-emulator default.
    add(&mut out, &format!("{clipboard_mod}+c"), KeybindAction::Copy);
    add(&mut out, "ctrl+shift+c", KeybindAction::Copy);
    add(
        &mut out,
        &format!("{clipboard_mod}+v"),
        KeybindAction::Paste,
    );
    add(&mut out, "ctrl+shift+v", KeybindAction::Paste);

    add(
        &mut out,
        &format!("{project_mod}+b"),
        KeybindAction::ToggleSidebar,
    );

    // Command palette: Cmd+Shift+P on macOS-GTK, Alt+Shift+P on Linux
    // (mirrors the Swift app's Cmd+Shift+P). No existing default uses
    // `…+shift+p`, so no collision.
    add(
        &mut out,
        &format!("{project_mod}+shift+p"),
        KeybindAction::CommandPalette,
    );

    // Command launcher: Cmd+Shift+T on macOS-GTK, Alt+Shift+T on Linux
    // (mirrors the Swift app). No existing default uses `…+shift+t`
    // (NewTab is `primary+t`), so no collision.
    add(
        &mut out,
        &format!("{project_mod}+shift+t"),
        KeybindAction::CommandLauncher,
    );

    // Custom palette (script-backed providers): Cmd+Shift+E on macOS-GTK,
    // Alt+Shift+E on Linux. `…+shift+r` would be the "R for Run" mnemonic
    // but RenameProject already owns it; `…+shift+e` ("Extensions") is
    // free on both platforms. Users rebind to `…+shift+r` via config.
    add(
        &mut out,
        &format!("{project_mod}+shift+e"),
        KeybindAction::CustomPalette,
    );

    // Browser-style font sizing on the active terminal. `Cmd-+` on
    // US layouts is really `Cmd-Shift-=`, and many users hit `Cmd-=`
    // without shift; bind both for FontIncrease.
    add(
        &mut out,
        &format!("{primary}+plus"),
        KeybindAction::FontIncrease,
    );
    add(
        &mut out,
        &format!("{primary}+equal"),
        KeybindAction::FontIncrease,
    );
    add(
        &mut out,
        &format!("{primary}+minus"),
        KeybindAction::FontDecrease,
    );
    add(&mut out, &format!("{primary}+0"), KeybindAction::FontReset);

    for n in 1..=9u8 {
        add(
            &mut out,
            &format!("{project_mod}+{n}"),
            KeybindAction::SwitchProject(n),
        );
        // SwitchTab stays on Ctrl on both platforms, which keeps
        // `Cmd+N` (or `Alt+N`) free for the project-switching keybind
        // above.
        add(&mut out, &format!("ctrl+{n}"), KeybindAction::SwitchTab(n));
    }

    out
}

/// Build the final accel → action map. User-config bindings override
/// defaults; `unbind` removes a default. `warn` is called for unparseable
/// triggers or unknown actions so the UI can surface the line number
/// alongside the message.
pub fn canonicalize_bindings(
    defaults: Vec<(Accel, KeybindAction)>,
    user: Vec<(String, String)>,
    mut warn: impl FnMut(&str),
) -> HashMap<Accel, KeybindAction> {
    let mut map: HashMap<Accel, KeybindAction> = defaults.into_iter().collect();

    for (trigger, action_name) in user {
        let Some(accel) = parse_trigger(&trigger) else {
            warn(&format!("invalid keybind trigger: {trigger:?}"));
            continue;
        };
        let Some(action) = KeybindAction::from_name(&action_name) else {
            warn(&format!("unknown keybind action: {action_name:?}"));
            continue;
        };
        match action {
            KeybindAction::Unbind => {
                map.remove(&accel);
            }
            other => {
                map.insert(accel, other);
            }
        }
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_link_modifier_accepts_aliases() {
        assert_eq!(parse_link_modifier("ctrl"), Some(AccelMods::CTRL));
        assert_eq!(parse_link_modifier("control"), Some(AccelMods::CTRL));
        assert_eq!(parse_link_modifier("alt"), Some(AccelMods::ALT));
        assert_eq!(parse_link_modifier("option"), Some(AccelMods::ALT));
        assert_eq!(parse_link_modifier("super"), Some(AccelMods::SUPER));
        assert_eq!(parse_link_modifier("cmd"), Some(AccelMods::SUPER));
        assert_eq!(parse_link_modifier("command"), Some(AccelMods::SUPER));
        assert_eq!(parse_link_modifier("meta"), Some(AccelMods::SUPER));
        // Case-insensitive + surrounding whitespace tolerated.
        assert_eq!(parse_link_modifier("  CTRL "), Some(AccelMods::CTRL));
    }

    #[test]
    fn parse_link_modifier_rejects_unknown() {
        assert_eq!(parse_link_modifier("hyper"), None);
        assert_eq!(parse_link_modifier(""), None);
        assert_eq!(parse_link_modifier("ctrl+alt"), None);
    }

    #[test]
    fn default_link_modifier_is_platform_primary() {
        let expected = if cfg!(target_os = "macos") {
            AccelMods::SUPER
        } else {
            AccelMods::ALT
        };
        assert_eq!(default_link_modifier(), expected);
    }

    #[test]
    fn resolve_link_modifier_prefers_override() {
        assert_eq!(
            resolve_link_modifier(Some(AccelMods::CTRL)),
            AccelMods::CTRL
        );
        assert_eq!(resolve_link_modifier(None), default_link_modifier());
    }

    #[test]
    fn parser_handles_modifier_aliases() {
        let a = parse_trigger("CTRL+SHIFT+T").unwrap();
        let b = parse_trigger("control+shift+t").unwrap();
        assert_eq!(a, b);
        assert!(a.modifiers.contains(AccelMods::CTRL));
        assert!(a.modifiers.contains(AccelMods::SHIFT));
        assert_eq!(a.key, "t");
    }

    #[test]
    fn parser_handles_super_aliases() {
        let a = parse_trigger("super+t").unwrap();
        let b = parse_trigger("cmd+t").unwrap();
        let c = parse_trigger("command+t").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert!(a.modifiers.contains(AccelMods::SUPER));
    }

    #[test]
    fn parser_handles_alt_aliases() {
        let a = parse_trigger("alt+t").unwrap();
        let b = parse_trigger("opt+t").unwrap();
        let c = parse_trigger("option+t").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert!(a.modifiers.contains(AccelMods::ALT));
    }

    #[test]
    fn parser_rejects_empty_and_unknown() {
        assert!(parse_trigger("").is_none());
        assert!(parse_trigger("ctrl+").is_none());
    }

    #[test]
    fn m8_rename_actions_parse() {
        // M8 added three new action names; pin them so a future rename
        // (or accidental removal) of one of the strings breaks loudly.
        assert_eq!(
            KeybindAction::from_name("rename_project"),
            Some(KeybindAction::RenameProject)
        );
        assert_eq!(
            KeybindAction::from_name("rename_tab"),
            Some(KeybindAction::RenameTab)
        );
        assert_eq!(
            KeybindAction::from_name("close_project"),
            Some(KeybindAction::CloseProject)
        );
        // `delete_project` stays a back-compat alias for `close_project`.
        assert_eq!(
            KeybindAction::from_name("delete_project"),
            Some(KeybindAction::CloseProject)
        );
        assert_eq!(
            KeybindAction::from_name("jump_to_unread"),
            Some(KeybindAction::JumpToUnread)
        );
    }

    #[test]
    fn default_bindings_m8_actions() {
        let defaults: HashMap<_, _> = default_bindings().into_iter().collect();
        // RenameTab + RenameProject default to the host's project
        // modifier — `alt+r` / `alt+shift+r` on Linux, `super+r` /
        // `super+shift+r` (Cmd+R / Cmd+Shift+R) on macOS. The same
        // physical gesture maps to the same action regardless of
        // host, which is what users expect when sharing a config
        // file across machines.
        //
        // Why not `Ctrl+R` on Linux: bash/readline owns it for
        // reverse-history-search and a window-global ShortcutController
        // priority steals the keystroke from the focused terminal.
        // `Alt+R` is bash's `revert-line` — much less critical and
        // worth the trade-off; users who care override via config.
        let (project_mod_r, project_mod_shift_r) = if cfg!(target_os = "macos") {
            ("super+r", "super+shift+r")
        } else {
            ("alt+r", "alt+shift+r")
        };
        let rename_tab_trigger = parse_trigger(project_mod_r).unwrap();
        assert_eq!(
            defaults.get(&rename_tab_trigger),
            Some(&KeybindAction::RenameTab)
        );
        let rename_project_trigger = parse_trigger(project_mod_shift_r).unwrap();
        assert_eq!(
            defaults.get(&rename_project_trigger),
            Some(&KeybindAction::RenameProject)
        );
        // Round-4 R3: DeleteProject now defaults to `project_mod+shift+w`
        // (Alt+Shift+W on Linux, Cmd+Shift+W on macOS) — paired with a
        // 2+-tab confirmation dialog in the dispatcher so single-tab
        // accidental ⌘⇧W stays cheap. Pre-round-4 this had no default
        // trigger; users now opt OUT by adding
        // `keybind = alt+shift+w = unbind` to their config.
        let close_project_trigger = if cfg!(target_os = "macos") {
            parse_trigger("super+shift+w").unwrap()
        } else {
            parse_trigger("alt+shift+w").unwrap()
        };
        assert_eq!(
            defaults.get(&close_project_trigger),
            Some(&KeybindAction::CloseProject)
        );
        // JumpToUnread → primary+shift+u (Cmd/Alt+Shift+U), Mac parity.
        let jump_trigger = if cfg!(target_os = "macos") {
            parse_trigger("super+shift+u").unwrap()
        } else {
            parse_trigger("alt+shift+u").unwrap()
        };
        assert_eq!(
            defaults.get(&jump_trigger),
            Some(&KeybindAction::JumpToUnread)
        );
    }

    #[test]
    fn command_palette_action_and_default() {
        assert_eq!(
            KeybindAction::from_name("command_palette"),
            Some(KeybindAction::CommandPalette)
        );
        let defaults: HashMap<_, _> = default_bindings().into_iter().collect();
        // projectMod+shift+p: Cmd+Shift+P on macOS, Alt+Shift+P on Linux.
        let trigger = if cfg!(target_os = "macos") {
            parse_trigger("super+shift+p").unwrap()
        } else {
            parse_trigger("alt+shift+p").unwrap()
        };
        assert_eq!(defaults.get(&trigger), Some(&KeybindAction::CommandPalette));
    }

    #[test]
    fn command_launcher_action_and_default() {
        assert_eq!(
            KeybindAction::from_name("command_launcher"),
            Some(KeybindAction::CommandLauncher)
        );
        let defaults: HashMap<_, _> = default_bindings().into_iter().collect();
        // projectMod+shift+t: Cmd+Shift+T on macOS, Alt+Shift+T on Linux.
        let trigger = if cfg!(target_os = "macos") {
            parse_trigger("super+shift+t").unwrap()
        } else {
            parse_trigger("alt+shift+t").unwrap()
        };
        assert_eq!(
            defaults.get(&trigger),
            Some(&KeybindAction::CommandLauncher)
        );
    }

    #[test]
    fn custom_palette_action_and_default() {
        assert_eq!(
            KeybindAction::from_name("custom_palette"),
            Some(KeybindAction::CustomPalette)
        );
        let defaults: HashMap<_, _> = default_bindings().into_iter().collect();
        // projectMod+shift+e: Cmd+Shift+E on macOS, Alt+Shift+E on Linux.
        // Must NOT collide with RenameProject (`…+shift+r`).
        let trigger = if cfg!(target_os = "macos") {
            parse_trigger("super+shift+e").unwrap()
        } else {
            parse_trigger("alt+shift+e").unwrap()
        };
        assert_eq!(defaults.get(&trigger), Some(&KeybindAction::CustomPalette));
        let rename_project = if cfg!(target_os = "macos") {
            parse_trigger("super+shift+r").unwrap()
        } else {
            parse_trigger("alt+shift+r").unwrap()
        };
        assert_eq!(
            defaults.get(&rename_project),
            Some(&KeybindAction::RenameProject),
            "custom_palette default must not steal RenameProject's binding"
        );
    }

    #[test]
    fn default_bindings_primary_modifier_is_host_appropriate() {
        let defaults: HashMap<_, _> = default_bindings().into_iter().collect();
        // NewTab lives on `primary+t` — `alt+t` on Linux,
        // `super+t` (Cmd+T) on macOS. Pins the host-detect logic so
        // a refactor of `default_bindings` can't silently flip
        // platforms.
        let expected_new_tab = if cfg!(target_os = "macos") {
            "super+t"
        } else {
            "alt+t"
        };
        let trigger = parse_trigger(expected_new_tab).unwrap();
        assert_eq!(defaults.get(&trigger), Some(&KeybindAction::NewTab));
    }

    #[test]
    fn cycle_tab_defaults_are_host_appropriate() {
        // The tab-cycle chord moved to `primary+shift+bracket{left,right}`
        // — `Alt+Shift+[`/`]` on Linux, `Cmd+Shift+[`/`]` on macOS. Pins
        // it so a refactor can't silently send these back to Ctrl (the
        // pre-Alt-scheme Linux default) where they'd leak `Meta-{` to the
        // shell instead of switching tabs.
        let defaults: HashMap<_, _> = default_bindings().into_iter().collect();
        let (prev, next) = if cfg!(target_os = "macos") {
            ("super+shift+bracketleft", "super+shift+bracketright")
        } else {
            ("alt+shift+bracketleft", "alt+shift+bracketright")
        };
        assert_eq!(
            defaults.get(&parse_trigger(prev).unwrap()),
            Some(&KeybindAction::CycleTabPrev)
        );
        assert_eq!(
            defaults.get(&parse_trigger(next).unwrap()),
            Some(&KeybindAction::CycleTabNext)
        );
    }

    #[test]
    fn default_bindings_have_no_duplicate_accels() {
        // The Linux defaults now all share one modifier (Alt, mirroring
        // Mac's all-super), so a future edit could silently map two
        // actions onto the same Accel — `into_iter().collect()` into the
        // HashMap would drop one without warning. Guard against it.
        let defaults = default_bindings();
        let mut seen = std::collections::HashSet::new();
        for (accel, action) in &defaults {
            assert!(
                seen.insert(accel.clone()),
                "duplicate default binding for {accel:?} (action {action:?})"
            );
        }
    }

    #[test]
    fn user_override_replaces_default() {
        // Use a trigger the host-detect defaults won't already claim.
        // On Linux `alt+t` is now the NewTab default; on macOS
        // `super+t` is. `ctrl+shift+alt+t` is guaranteed unbound on
        // both, so a user adding it as `new_tab` should appear in
        // the canonicalized map alongside the platform default.
        let defaults = default_bindings();
        let user = vec![("ctrl+shift+alt+t".into(), "new_tab".into())];
        let mut warnings = Vec::new();
        let map = canonicalize_bindings(defaults, user, |w| warnings.push(w.to_string()));
        // User-added binding is present.
        assert_eq!(
            map.get(&parse_trigger("ctrl+shift+alt+t").unwrap()),
            Some(&KeybindAction::NewTab)
        );
        // Platform default is also present (not clobbered).
        let platform_default = if cfg!(target_os = "macos") {
            "super+t"
        } else {
            "alt+t"
        };
        assert_eq!(
            map.get(&parse_trigger(platform_default).unwrap()),
            Some(&KeybindAction::NewTab)
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn unbind_removes_default() {
        let defaults = default_bindings();
        let platform_default = if cfg!(target_os = "macos") {
            "super+t"
        } else {
            "alt+t"
        };
        let user = vec![(platform_default.into(), "unbind".into())];
        let map = canonicalize_bindings(defaults, user, |_| {});
        assert!(!map.contains_key(&parse_trigger(platform_default).unwrap()));
    }

    #[test]
    fn unknown_action_warns() {
        let defaults = Vec::new();
        let user = vec![("ctrl+t".into(), "do_a_thing".into())];
        let mut warnings = Vec::new();
        canonicalize_bindings(defaults, user, |w| warnings.push(w.to_string()));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("unknown"));
    }

    #[test]
    fn from_name_parses_dynamic_actions() {
        assert_eq!(
            KeybindAction::from_name("switch_project_3"),
            Some(KeybindAction::SwitchProject(3))
        );
        assert_eq!(
            KeybindAction::from_name("switch_tab_9"),
            Some(KeybindAction::SwitchTab(9))
        );
        assert!(KeybindAction::from_name("switch_project_10").is_none());
    }
}
