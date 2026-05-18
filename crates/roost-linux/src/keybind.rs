//! Keybind trigger parser + default action table.
//!
//! Mirrors `mac/Sources/Roost/Keybind.swift` (Phase 6 P1) in shape +
//! the Go binary's `cmd/roost/shortcuts.go`. Defaults flip Linux's
//! primary modifier to `ctrl` (Mac uses `super`/`cmd`); everything
//! else — modifier-alias rules, `unbind` semantics, action namespace —
//! is shared with the other UIs so config files port verbatim across
//! `mac/`, `linux/`, and the Go binary.

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
    /// M8: delete the active project (with an `adw::AlertDialog`
    /// confirmation, since this cascades to delete every tab in the
    /// project). No default trigger — advanced users bind via
    /// `~/.config/roost/config.conf` so a stray keypress can't
    /// dataloss.
    DeleteProject,
    CycleTabPrev,
    CycleTabNext,
    Copy,
    Paste,
    ToggleSidebar,
    /// Browser-style font sizing on the active tab's terminal.
    /// Defaults to `primary+plus`/`primary+equal` (Go matches both
    /// because `Cmd-+` on US layouts is really `Cmd-Shift-=` and
    /// users frequently hit `Cmd-=` without shift). Mirrors the
    /// Go binary's per-tab font adjusters.
    FontIncrease,
    /// Default `primary+minus`.
    FontDecrease,
    /// Default `primary+0`. Resets to the config-file default
    /// (or `cell_metrics::DEFAULT_FONT_SIZE_PT` if no config).
    FontReset,
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
            "delete_project" => Some(Self::DeleteProject),
            "cycle_tab_prev" => Some(Self::CycleTabPrev),
            "cycle_tab_next" => Some(Self::CycleTabNext),
            "copy" => Some(Self::Copy),
            "paste" => Some(Self::Paste),
            "toggle_sidebar" => Some(Self::ToggleSidebar),
            "font_increase" => Some(Self::FontIncrease),
            "font_decrease" => Some(Self::FontDecrease),
            "font_reset" => Some(Self::FontReset),
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

/// Default bindings table — host-platform aware. Matches the Go
/// binary's `cmd/roost/app.go::defaultBindings`:
///
/// * Linux: `primary = ctrl`, `projectMod = alt`, `clipboardMod = alt`.
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
        ("ctrl", "alt", "alt")
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
    // DeleteProject: no default trigger — see KeybindAction docs.

    // Cycle prev/next: Shift+[ and Shift+] map to bracketleft/right on
    // most US layouts; some layouts emit braceleft/right after Shift.
    // Bind both so the keybind fires regardless of layout — matches
    // the Go binary's pair-bind in cmd/roost/app.go::defaultBindings.
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

    // Browser-style font sizing on the active terminal. `Cmd-+` on
    // US layouts is really `Cmd-Shift-=`, and many users hit `Cmd-=`
    // without shift; bind both for FontIncrease (Go does the same).
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
        // SwitchTab stays on Ctrl on both platforms — matches the Go
        // binary, and keeps `Cmd+N` (or `Alt+N`) free for the
        // project-switching keybind above.
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
            KeybindAction::from_name("delete_project"),
            Some(KeybindAction::DeleteProject)
        );
    }

    #[test]
    fn default_bindings_m8_actions() {
        let defaults: HashMap<_, _> = default_bindings().into_iter().collect();
        // RenameTab + RenameProject default to the host's project
        // modifier — `alt+r` / `alt+shift+r` on Linux, `super+r` /
        // `super+shift+r` (Cmd+R / Cmd+Shift+R) on macOS — matching
        // the Go binary's `cmd/roost/app.go::defaultBindings`. Same
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
        // DeleteProject has no default trigger — would cascade-delete
        // every tab in the project, so users opt in via config.
        assert!(!defaults
            .values()
            .any(|a| matches!(a, KeybindAction::DeleteProject)));
    }

    #[test]
    fn default_bindings_primary_modifier_is_host_appropriate() {
        let defaults: HashMap<_, _> = default_bindings().into_iter().collect();
        // NewTab lives on `primary+t` — `ctrl+t` on Linux,
        // `super+t` (Cmd+T) on macOS. Pins the host-detect logic so
        // a refactor of `default_bindings` can't silently flip
        // platforms.
        let expected_new_tab = if cfg!(target_os = "macos") {
            "super+t"
        } else {
            "ctrl+t"
        };
        let trigger = parse_trigger(expected_new_tab).unwrap();
        assert_eq!(defaults.get(&trigger), Some(&KeybindAction::NewTab));
    }

    #[test]
    fn user_override_replaces_default() {
        // Use a trigger the host-detect defaults won't already claim.
        // On Linux `ctrl+t` is now the NewTab default; on macOS
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
            "ctrl+t"
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
            "ctrl+t"
        };
        let user = vec![(platform_default.into(), "unbind".into())];
        let map = canonicalize_bindings(defaults, user, |_| {});
        assert!(map.get(&parse_trigger(platform_default).unwrap()).is_none());
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
