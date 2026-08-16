//! The native menu bar — parity port of `mac/Sources/Roost/App.swift`'s
//! `installMainMenu()` (:3935-4204).
//!
//! Two halves, deliberately separated so the interesting one is testable
//! without AppKit (the [`super::dock_badge::label`] precedent):
//!
//! * **Pure**: [`accel_to_key_equivalent`] turns the accel
//!   `roost_ui_model::keybind::menu_accel_for_action` picked into the
//!   `(keyEquivalent, modifierMask)` pair AppKit wants. Unit-tested below;
//!   the inversion policy itself lives beside the table it inverts.
//! * **AppKit**: [`install`] builds `NSApp.mainMenu` once and
//!   [`sync_gating`] mutates enabled-state / key equivalents as the app's
//!   keyboard route changes.
//!
//! Menu items never carry business logic: activation puts a [`MenuEvent`]
//! on the engine feed — the app's one inbound channel, whose receiver the
//! App owns — and the update loop routes it through the same
//! `dispatch_keybind_action` path a keystroke takes (plan 028 § 3.2/§ 3.3).
//!
//! Installing `NSApp.mainMenu` replaces winit's own default menu
//! (`winit-0.30.13/src/platform_impl/macos/menu.rs`, built when
//! `default_menu` is on) wholesale — including its `terminate:`-backed
//! Quit, which is exactly the item [`MenuEvent::Quit`] exists to
//! supersede.

use std::cell::RefCell;
use std::collections::HashMap;

use objc2::rc::Retained;
use objc2::runtime::{NSObject, Sel};
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_foundation::NSString;
use roost_ui_model::keybind::{menu_accel_for_action, Accel, AccelMods, KeybindAction};

use crate::engine_feed::{EngineFeed, EngineFeedSender};

/// What a menu activation asks the app to do. Plain data — the action
/// method runs inside AppKit's `sendAction:`, nowhere near `App`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuEvent {
    /// Routed through `dispatch_keybind_action`, exactly as the bound
    /// keystroke would be.
    Action(KeybindAction),
    /// The App menu's Quit. Deliberately NOT `NSApplication::terminate:`:
    /// terminate would tear the process down behind iced's back and skip
    /// `Workspace::flush()`'s clean-exit fsync, so Quit takes the same
    /// graceful exit path exit-on-empty uses (plan 028 § 3.2).
    Quit,
}

/// Which surface currently owns the keyboard, as far as the menu bar
/// needs to care. Plain value so the App can compare it against the last
/// one it pushed and call the seam only on a change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MenuGating {
    /// A palette is open. Mirrors `App.swift:2203-2217`'s
    /// `validateMenuItem` gate: every command item goes dead except the
    /// four palette toggles.
    pub(crate) palette_open: bool,
    /// A text surface (inline rename, confirm modal) owns the keyboard,
    /// or the terminal is mid-IME-composition. Those routes swallow every
    /// key today (`app.rs`'s `KeyboardRoute::Editor`/`Confirm` arms), so
    /// no command may fire from the menu behind them either.
    pub(crate) text_capture: bool,
}

// AppKit's modifier bits, as `NSEventModifierFlags` raw values. Named here
// so [`accel_to_key_equivalent`] stays free of AppKit types and can be
// unit-tested on its own; `modifier_bits_match_appkit` pins them.
const MOD_SHIFT: usize = 1 << 17;
const MOD_CONTROL: usize = 1 << 18;
const MOD_OPTION: usize = 1 << 19;
const MOD_COMMAND: usize = 1 << 20;

/// The four picker toggles that stay live while a palette is open — a
/// re-press is a harmless no-op, and locking the user out of switching
/// pickers mid-palette is the worse failure (`App.swift:2205-2216`).
pub(crate) fn is_palette_toggle(action: KeybindAction) -> bool {
    matches!(
        action,
        KeybindAction::CommandPalette
            | KeybindAction::CommandLauncher
            | KeybindAction::CustomPalette
            | KeybindAction::AgentPalette
    )
}

/// The `(keyEquivalent, modifierMask)` pair for an accel, or `None` when
/// the key segment names nothing AppKit can express — a user's exotic
/// binding then renders as a bare title rather than a wrong shortcut.
///
/// Ports `keyEquivalentForToken` (`mac/Sources/Roost/Keybind.swift:182`).
pub(crate) fn accel_to_key_equivalent(accel: &Accel) -> Option<(String, usize)> {
    let key = key_equivalent(&accel.key)?;
    let mut mask = 0;
    for (flag, bit) in [
        (AccelMods::SHIFT, MOD_SHIFT),
        (AccelMods::CTRL, MOD_CONTROL),
        (AccelMods::ALT, MOD_OPTION),
        (AccelMods::SUPER, MOD_COMMAND),
    ] {
        if accel.modifiers.contains(flag) {
            mask |= bit;
        }
    }
    Some((key, mask))
}

fn key_equivalent(key: &str) -> Option<String> {
    if key.chars().count() == 1 {
        return Some(key.to_lowercase());
    }
    let mapped = match key {
        "plus" => "+",
        "equal" => "=",
        "minus" => "-",
        "bracketleft" => "[",
        "bracketright" => "]",
        "braceleft" => "{",
        "braceright" => "}",
        "comma" => ",",
        "period" => ".",
        "slash" => "/",
        "backslash" => "\\",
        "semicolon" => ";",
        "apostrophe" => "'",
        "grave" => "`",
        "space" => " ",
        "return" | "enter" => "\r",
        "tab" => "\t",
        "escape" => "\u{1b}",
        "backspace" => "\u{7f}",
        _ => return None,
    };
    Some(mapped.to_string())
}

// ---------------------------------------------------------------------
// AppKit
// ---------------------------------------------------------------------

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - The class does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RoostMenuTarget"]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(roostMenuAction:))]
        fn menu_action(&self, sender: &NSMenuItem) {
            dispatch_tag(sender.tag());
        }
    }
);

/// One menu item the gate can mutate.
struct GatedItem {
    item: Retained<NSMenuItem>,
    /// Exempt from the palette-open gate.
    palette_toggle: bool,
    /// Set for Copy/Paste: the equivalent to restore when text capture
    /// ends. `None` for every other item.
    clipboard_equivalent: Option<(String, usize)>,
}

struct MainMenu {
    /// `NSMenuItem.target` is a **weak** property, so the only thing
    /// keeping the action target alive is this field.
    target: Retained<MenuTarget>,
    root: Retained<NSMenu>,
    /// Indexed by the item's tag.
    events: Vec<MenuEvent>,
    gated: Vec<GatedItem>,
    /// The app's one inbound channel. A menu activation is just another
    /// item on it, so it lands in the same FIFO — and the same reconcile
    /// bookkeeping — as an IPC request or a workspace event.
    feed: EngineFeedSender,
}

thread_local! {
    /// Main-thread-only, and never handed out: `Retained<_>` does not
    /// cross this module's boundary (`super`'s rules).
    static MENU: RefCell<Option<MainMenu>> = const { RefCell::new(None) };
}

/// Resolve the fired item's tag and put its event on the feed.
///
/// Runs inside AppKit's `sendAction:`, which a seam function may itself
/// have entered (`performActionForItemAtIndex:`), so the table is read
/// through `try_borrow` — a panic here would unwind through ObjC frames.
fn dispatch_tag(tag: isize) {
    MENU.with(|cell| {
        let Ok(menu) = cell.try_borrow() else {
            tracing::error!(
                tag,
                "menu action fired while the menu table was being rebuilt"
            );
            return;
        };
        let Some(menu) = menu.as_ref() else {
            return;
        };
        let Some(event) = usize::try_from(tag)
            .ok()
            .and_then(|index| menu.events.get(index).copied())
        else {
            tracing::warn!(tag, "menu item fired with no bound event");
            return;
        };
        // `false` means the drain side is gone: the app is already
        // tearing down and there is nobody left to act on the click.
        menu.feed.send(EngineFeed::Menu(event));
    });
}

/// Build and install `NSApp.mainMenu`, once. Returns whether *this* call
/// built it — the caller uses that to seed the gate against a menu it
/// knows is freshly all-enabled.
///
/// Idempotent: `window_opened` runs again on every focus regain, and a
/// second install would drop the live menu on the floor mid-tracking.
pub(crate) fn install(
    mtm: MainThreadMarker,
    app_name: &str,
    keybindings: &HashMap<Accel, KeybindAction>,
    feed: EngineFeedSender,
) -> bool {
    if MENU.with(|cell| cell.borrow().is_some()) {
        return false;
    }
    let menu = build(mtm, app_name, keybindings, feed);
    NSApplication::sharedApplication(mtm).setMainMenu(Some(&menu.root));
    MENU.with(|cell| *cell.borrow_mut() = Some(menu));
    tracing::info!(app_name, "installed the native menu bar");
    true
}

/// Push the app's keyboard route onto the live menu.
///
/// Two different mechanisms, because they answer two different questions:
///
/// * Palette gating **disables** items — Swift parity, whatever AppKit
///   does with a disabled item's chord (a greyed item is exactly what the
///   Swift app shows today).
/// * Text capture disables **every** command item — an editor or confirm
///   modal already swallows every key today — and additionally **blanks**
///   Copy/Paste's key equivalents. Those two are the only chords that
///   must physically reach iced's `text_input`, and a disabled item is
///   documented neither to absorb nor to forward its chord; a blank
///   equivalent cannot match in `performKeyEquivalent:` at all, so the
///   event provably gets through under either behavior (plan 028 § 3.5).
///   Copy/Paste blank while a palette is open too: its search field is an
///   iced `text_input` as well, and Swift keeps ⌘C/⌘V working there
///   because its Edit items target `NSText`, not the app delegate.
pub(crate) fn sync_gating(gating: MenuGating, _mtm: MainThreadMarker) {
    MENU.with(|cell| {
        let slot = cell.borrow();
        let Some(menu) = slot.as_ref() else {
            return;
        };
        let blank_clipboard = gating.text_capture || gating.palette_open;
        for entry in &menu.gated {
            let enabled = !gating.text_capture && (!gating.palette_open || entry.palette_toggle);
            entry.item.setEnabled(enabled);
            if let Some((key, mask)) = entry.clipboard_equivalent.as_ref() {
                let (key, mask) = if blank_clipboard {
                    ("", 0)
                } else {
                    (key.as_str(), *mask)
                };
                entry.item.setKeyEquivalent(&NSString::from_str(key));
                entry
                    .item
                    .setKeyEquivalentModifierMask(NSEventModifierFlags::from_bits_retain(mask));
            }
        }
    });
}

fn build(
    mtm: MainThreadMarker,
    app_name: &str,
    keybindings: &HashMap<Accel, KeybindAction>,
    feed: EngineFeedSender,
) -> MainMenu {
    // SAFETY: `init` on a freshly allocated instance of our own class.
    let target: Retained<MenuTarget> = unsafe { msg_send![MenuTarget::alloc(mtm), init] };
    let mut menu = MainMenu {
        target,
        root: submenu(mtm, ""),
        events: Vec::new(),
        gated: Vec::new(),
        feed,
    };

    // AppKit substitutes `CFBundleName` for the first menu's rendered
    // title; the title set here is what programmatic lookup walks.
    let app_menu = submenu(mtm, app_name);
    app_menu.addItem(&standard_item(
        mtm,
        &format!("About {app_name}"),
        sel!(orderFrontStandardAboutPanel:),
        None,
    ));
    // Present but inert: 6c wires it to Sparkle. A disabled item with no
    // action is exactly what Swift renders when its updater failed to
    // start (`App.swift:3950-3956`, nil target).
    app_menu.addItem(&disabled_item(mtm, "Check for Updates\u{2026}"));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&standard_item(
        mtm,
        &format!("Hide {app_name}"),
        sel!(hide:),
        Some(("h", MOD_COMMAND)),
    ));
    app_menu.addItem(&standard_item(
        mtm,
        "Hide Others",
        sel!(hideOtherApplications:),
        Some(("h", MOD_COMMAND | MOD_OPTION)),
    ));
    app_menu.addItem(&standard_item(
        mtm,
        "Show All",
        sel!(unhideAllApplications:),
        None,
    ));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let quit = plain_item(mtm, &format!("Quit {app_name}"));
    bind_event(&mut menu, &quit, MenuEvent::Quit);
    set_key_equivalent(&quit, Some(("q".to_string(), MOD_COMMAND)));
    app_menu.addItem(&quit);
    attach(mtm, &menu.root, &app_menu);

    let file_menu = submenu(mtm, "File");
    for spec in [
        Some(("New Project", KeybindAction::NewProject)),
        Some(("New Tab", KeybindAction::NewTab)),
        Some(("Close Tab", KeybindAction::CloseTab)),
        None,
        Some(("Rename Tab\u{2026}", KeybindAction::RenameTab)),
        Some(("Rename Project\u{2026}", KeybindAction::RenameProject)),
        Some(("Close Project", KeybindAction::CloseProject)),
        None,
        Some(("Previous Tab", KeybindAction::CycleTabPrev)),
        Some(("Next Tab", KeybindAction::CycleTabNext)),
    ] {
        add_action_item(mtm, &mut menu, &file_menu, spec, keybindings);
    }
    attach(mtm, &menu.root, &file_menu);

    let view_menu = submenu(mtm, "View");
    for spec in [
        Some(("Command Palette\u{2026}", KeybindAction::CommandPalette)),
        Some(("Command Launcher\u{2026}", KeybindAction::CommandLauncher)),
        Some(("Agent Palette\u{2026}", KeybindAction::AgentPalette)),
        Some(("Custom Commands\u{2026}", KeybindAction::CustomPalette)),
        None,
        Some(("Zoom In", KeybindAction::FontIncrease)),
        Some(("Zoom Out", KeybindAction::FontDecrease)),
        Some(("Actual Size", KeybindAction::FontReset)),
        None,
        Some(("Toggle Sidebar", KeybindAction::ToggleSidebar)),
        Some(("Toggle Sidebar Agents", KeybindAction::ToggleSidebarAgents)),
        None,
        Some(("Jump to Unread", KeybindAction::JumpToUnread)),
    ] {
        add_action_item(mtm, &mut menu, &view_menu, spec, keybindings);
    }
    attach(mtm, &menu.root, &view_menu);

    // Cut and Select All exist for menu-shape parity only: neither the
    // Swift app nor this one has a responder that implements them. They
    // ship with NO key equivalent — a disabled ⌘X/⌘A that absorbed its
    // chord would swallow keystrokes the terminal encoder sees today
    // (plan 028 § 3.6).
    let edit_menu = submenu(mtm, "Edit");
    edit_menu.addItem(&disabled_item(mtm, "Cut"));
    for spec in [
        Some(("Copy", KeybindAction::Copy)),
        Some(("Paste", KeybindAction::Paste)),
    ] {
        add_action_item(mtm, &mut menu, &edit_menu, spec, keybindings);
    }
    edit_menu.addItem(&disabled_item(mtm, "Select All"));
    attach(mtm, &menu.root, &edit_menu);

    menu
}

/// `None` adds a separator; `Some` adds a keybind-table-bound item.
fn add_action_item(
    mtm: MainThreadMarker,
    menu: &mut MainMenu,
    parent: &NSMenu,
    spec: Option<(&str, KeybindAction)>,
    keybindings: &HashMap<Accel, KeybindAction>,
) {
    let Some((title, action)) = spec else {
        parent.addItem(&NSMenuItem::separatorItem(mtm));
        return;
    };
    let item = plain_item(mtm, title);
    bind_event(menu, &item, MenuEvent::Action(action));
    let accel = menu_accel_for_action(action, keybindings)
        .and_then(|accel| accel_to_key_equivalent(&accel));
    set_key_equivalent(&item, accel.clone());
    parent.addItem(&item);
    // Copy/Paste are the two [`sync_gating`] blanks, so they are the two
    // that have to remember what to restore.
    let clipboard_equivalent = matches!(action, KeybindAction::Copy | KeybindAction::Paste)
        .then(|| accel.unwrap_or_default());
    menu.gated.push(GatedItem {
        item,
        palette_toggle: is_palette_toggle(action),
        clipboard_equivalent,
    });
}

/// A submenu with `autoenablesItems` off — every enabled-state in here is
/// written by [`sync_gating`], never inferred by AppKit from a responder
/// chain iced does not participate in.
fn submenu(mtm: MainThreadMarker, title: &str) -> Retained<NSMenu> {
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
    menu.setAutoenablesItems(false);
    menu
}

fn attach(mtm: MainThreadMarker, root: &NSMenu, child: &NSMenu) {
    let holder = NSMenuItem::new(mtm);
    holder.setSubmenu(Some(child));
    root.addItem(&holder);
}

/// An item with no action yet — [`bind_event`] or [`standard_item`] gives
/// it one, or it stays inert.
fn plain_item(mtm: MainThreadMarker, title: &str) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    item
}

/// An item that is permanently dead: no action, no key equivalent, and no
/// entry in `gated`, so nothing ever re-enables it (Cut, Select All,
/// Check for Updates…).
fn disabled_item(mtm: MainThreadMarker, title: &str) -> Retained<NSMenuItem> {
    let item = plain_item(mtm, title);
    item.setEnabled(false);
    item
}

/// An AppKit-implemented item: nil target, so the selector walks the
/// responder chain up to `NSApplication` the way Swift's does.
fn standard_item(
    mtm: MainThreadMarker,
    title: &str,
    selector: Sel,
    key: Option<(&str, usize)>,
) -> Retained<NSMenuItem> {
    let item = plain_item(mtm, title);
    // SAFETY: an AppKit selector taking one `id` sender, matching the
    // `action` signature AppKit invokes it with.
    unsafe { item.setAction(Some(selector)) };
    set_key_equivalent(&item, key.map(|(key, mask)| (key.to_string(), mask)));
    item
}

fn bind_event(menu: &mut MainMenu, item: &NSMenuItem, event: MenuEvent) {
    let tag = isize::try_from(menu.events.len()).unwrap_or(-1);
    menu.events.push(event);
    item.setTag(tag);
    // SAFETY: `roostMenuAction:` is declared on `MenuTarget` above with
    // the `(id sender)` signature AppKit calls actions with; the target is
    // a weak reference kept alive by `MainMenu::target`.
    unsafe {
        item.setAction(Some(sel!(roostMenuAction:)));
        item.setTarget(Some(&menu.target));
    }
}

fn set_key_equivalent(item: &NSMenuItem, key: Option<(String, usize)>) {
    let (key, mask) = key.unwrap_or_default();
    item.setKeyEquivalent(&NSString::from_str(&key));
    item.setKeyEquivalentModifierMask(NSEventModifierFlags::from_bits_retain(mask));
}

#[cfg(test)]
mod tests {
    use roost_ui_model::keybind::parse_trigger;

    use super::*;

    fn accel(trigger: &str) -> Accel {
        parse_trigger(trigger).expect("trigger parses")
    }

    /// The constants [`accel_to_key_equivalent`] emits are AppKit's, and
    /// nothing else checks that — the pure function deliberately never
    /// names `NSEventModifierFlags`.
    #[test]
    fn modifier_bits_match_appkit() {
        assert_eq!(MOD_SHIFT, NSEventModifierFlags::Shift.bits());
        assert_eq!(MOD_CONTROL, NSEventModifierFlags::Control.bits());
        assert_eq!(MOD_OPTION, NSEventModifierFlags::Option.bits());
        assert_eq!(MOD_COMMAND, NSEventModifierFlags::Command.bits());
    }

    #[test]
    fn letters_and_digits_pass_through_with_their_modifiers() {
        assert_eq!(
            accel_to_key_equivalent(&accel("super+t")),
            Some(("t".into(), MOD_COMMAND))
        );
        assert_eq!(
            accel_to_key_equivalent(&accel("super+shift+p")),
            Some(("p".into(), MOD_COMMAND | MOD_SHIFT))
        );
        assert_eq!(
            accel_to_key_equivalent(&accel("ctrl+1")),
            Some(("1".into(), MOD_CONTROL))
        );
        assert_eq!(
            accel_to_key_equivalent(&accel("ctrl+shift+alt+super+x")),
            Some((
                "x".into(),
                MOD_CONTROL | MOD_SHIFT | MOD_OPTION | MOD_COMMAND
            ))
        );
    }

    #[test]
    fn named_keys_map_to_their_characters() {
        for (trigger, expected) in [
            ("super+shift+bracketleft", "["),
            ("super+shift+braceright", "}"),
            ("super+plus", "+"),
            ("super+equal", "="),
            ("super+minus", "-"),
        ] {
            assert_eq!(
                accel_to_key_equivalent(&accel(trigger)).map(|(key, _)| key),
                Some(expected.to_string()),
                "{trigger}"
            );
        }
    }

    /// A binding AppKit cannot spell renders as a bare title rather than
    /// as a wrong shortcut.
    #[test]
    fn an_unmappable_key_has_no_equivalent() {
        assert_eq!(accel_to_key_equivalent(&accel("super+f13")), None);
    }

    /// The end-to-end shape a menu item gets: the shared inversion picks
    /// the accel, this module spells it for AppKit.
    #[test]
    fn the_shared_inversion_feeds_the_appkit_spelling() {
        let bindings = roost_ui_model::keybind::canonicalize_bindings(
            roost_ui_model::keybind::default_bindings(),
            Vec::new(),
            |_| {},
        );
        let copy = menu_accel_for_action(KeybindAction::Copy, &bindings)
            .as_ref()
            .and_then(accel_to_key_equivalent);
        assert_eq!(copy, Some(("c".into(), MOD_COMMAND)));
    }

    /// The exempt set is exactly Swift's (`App.swift:2205-2216`).
    #[test]
    fn only_the_four_pickers_survive_a_palette() {
        for action in [
            KeybindAction::CommandPalette,
            KeybindAction::CommandLauncher,
            KeybindAction::CustomPalette,
            KeybindAction::AgentPalette,
        ] {
            assert!(is_palette_toggle(action), "{action:?}");
        }
        for action in [
            KeybindAction::NewTab,
            KeybindAction::Copy,
            KeybindAction::ToggleSidebarAgents,
            KeybindAction::JumpToUnread,
        ] {
            assert!(!is_palette_toggle(action), "{action:?}");
        }
    }
}
