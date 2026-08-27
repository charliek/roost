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
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSControlStateValueOff, NSControlStateValueOn, NSEventModifierFlags, NSMenu,
    NSMenuItem,
};
use objc2_foundation::NSString;
use roost_ipc::messages::{AppMenuDumpResult, MenuDump, MenuItemDump, Project};
use roost_ui_model::keybind::{menu_accel_for_action, Accel, AccelMods, KeybindAction};
use roost_ui_model::keys::{HostId, ProjectKey, TabKey};

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
    /// A Window-menu project row. Carries the project's **stable id**, not
    /// its row position: the activation rides the feed and is acted on a
    /// turn later, by which time a position could name a different project
    /// (plan 028 § 3.3).
    /// Host-qualified: the row was built from one backend's snapshot, and
    /// the turn's delay is exactly when a second id-space could reinterpret
    /// a bare number.
    SelectProject(ProjectKey),
    /// A Window-menu tab row, by stable tab key — same reasoning.
    SelectTab(TabKey),
    /// The App menu's "Check for Updates…". Like [`MenuEvent::Quit`] it
    /// is deliberately outside the command gating: Swift's item targets
    /// the updater controller, not the app delegate, so its
    /// `validateMenuItem` self-targeting gate never sees it. What DOES
    /// gate it is the updater's own `canCheckForUpdates`
    /// ([`sync_update_item`]).
    CheckForUpdates,
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

/// Whether a custom-action item is live under `gating`. The one rule,
/// read by the seam when it writes an item's enabled-state and by the
/// App when it re-checks a dispatched activation against the route it
/// landed in.
pub(crate) fn command_enabled(gating: MenuGating, palette_toggle: bool) -> bool {
    !gating.text_capture && (!gating.palette_open || palette_toggle)
}

/// One dynamic Window-menu row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WindowRow<K> {
    /// The workspace key the row selects — stable, never a position, for
    /// [`MenuEvent::SelectProject`]'s reason. Generic so a project row and
    /// a tab row cannot be swapped for one another.
    pub(crate) id: K,
    pub(crate) title: String,
    /// Renders as the row's checkmark.
    pub(crate) active: bool,
}

/// The Window menu's dynamic half as plain data: what the seam renders,
/// and what the App diffs against the last model it built so a reconcile
/// that moved nothing never touches AppKit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WindowRows {
    pub(crate) projects: Vec<WindowRow<ProjectKey>>,
    /// The tabs of the ACTIVE project only — `App.swift:3852`'s
    /// `tabsForActiveProject()`.
    pub(crate) tabs: Vec<WindowRow<TabKey>>,
}

impl WindowRows {
    /// The parity port of `rebuildWindowMenu`'s two loops
    /// (`App.swift:3830-3869`): projects by name, then the active
    /// project's tabs as "Tab N" — the position the user counts, not the
    /// tab's own title.
    /// `host` is the instance `projects` was snapshotted from — the wire
    /// type carries bare ids, so the caller is the only thing that knows
    /// which id-space they belong to.
    pub(crate) fn derive(
        projects: &[Project],
        host: HostId,
        active_project: ProjectKey,
        active_tab: TabKey,
    ) -> Self {
        Self {
            projects: projects
                .iter()
                .map(|project| {
                    let key = ProjectKey::new(host, project.id);
                    WindowRow {
                        id: key,
                        title: project.name.clone(),
                        active: key == active_project,
                    }
                })
                .collect(),
            tabs: projects
                .iter()
                .find(|project| ProjectKey::new(host, project.id) == active_project)
                .into_iter()
                .flat_map(|project| project.tabs.iter())
                .enumerate()
                .map(|(index, tab)| {
                    let key = TabKey::new(host, tab.id);
                    WindowRow {
                        id: key,
                        title: format!("Tab {}", index + 1),
                        active: key == active_tab,
                    }
                })
                .collect(),
        }
    }

    /// Whether [`Self::derive`] would produce a value `PartialEq` to
    /// `self`, computed WITHOUT any of `derive`'s allocations — no
    /// `project.name.clone()`, no `format!("Tab {N}")`. `sync_window_menu`
    /// calls this first and only pays for a real `derive` on an actual
    /// mismatch — a match (a reconcile that moved nothing menu-relevant)
    /// is the common case.
    pub(crate) fn matches(
        &self,
        projects: &[Project],
        host: HostId,
        active_project: ProjectKey,
        active_tab: TabKey,
    ) -> bool {
        if self.projects.len() != projects.len() {
            return false;
        }
        let projects_match = self
            .projects
            .iter()
            .zip(projects.iter())
            .all(|(row, project)| {
                let key = ProjectKey::new(host, project.id);
                row.id == key && row.title == project.name && row.active == (key == active_project)
            });
        if !projects_match {
            return false;
        }

        let active_tabs = projects
            .iter()
            .find(|project| ProjectKey::new(host, project.id) == active_project)
            .into_iter()
            .flat_map(|project| project.tabs.iter());

        let mut stored = self.tabs.iter();
        for (index, tab) in active_tabs.enumerate() {
            let Some(row) = stored.next() else {
                return false;
            };
            let key = TabKey::new(host, tab.id);
            if row.id != key
                || row.active != (key == active_tab)
                || !title_is_tab_number(&row.title, index + 1)
            {
                return false;
            }
        }
        stored.next().is_none()
    }
}

/// Whether `title` is exactly what `format!("Tab {number}")` would produce,
/// without formatting: walks `number`'s decimal digits into a stack buffer
/// and compares bytes. Rejects lookalikes a numeric parse would wrongly
/// accept (e.g. `"Tab 01"` for `number == 1`) — the equivalence pin needs
/// exact-string semantics, not "parses to the same value".
fn title_is_tab_number(title: &str, number: usize) -> bool {
    let Some(digits) = title.strip_prefix("Tab ") else {
        return false;
    };
    // usize::MAX is 20 decimal digits.
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let mut n = number;
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    digits.as_bytes() == &buf[i..]
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
        // ASCII-only: Unicode lowercasing can expand one char into
        // several (İ → i̇), which AppKit would treat as an invalid
        // multi-char keyEquivalent. An exotic binding renders as a
        // bare title instead, same as any other unmappable key.
        if !key.is_ascii() {
            return None;
        }
        return Some(key.to_ascii_lowercase());
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
    /// The Window menu, kept because its rows are rebuilt in place
    /// whenever the workspace's projects or tabs move.
    window: Retained<NSMenu>,
    /// "Check for Updates…", kept because its enabled state tracks the
    /// Sparkle updater rather than the keyboard route — the one item
    /// [`sync_gating`] must never touch.
    updates: Retained<NSMenuItem>,
    /// Indexed by the item's tag.
    events: Vec<MenuEvent>,
    /// Tags below this belong to the static menus and never move. A
    /// Window rebuild truncates back to here and re-pushes, so the whole
    /// reassignment happens under one `borrow_mut` — a tag is never
    /// briefly stale.
    static_events: usize,
    gated: Vec<GatedItem>,
    /// The Window menu's dynamic rows. Separate from `gated` only so a
    /// rebuild can drop them wholesale; [`sync_gating`] treats both the
    /// same.
    window_gated: Vec<GatedItem>,
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
        for entry in menu.gated.iter().chain(&menu.window_gated) {
            entry
                .item
                .setEnabled(command_enabled(gating, entry.palette_toggle));
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

/// Push the Sparkle updater's readiness onto "Check for Updates…".
///
/// Its own axis, deliberately separate from [`sync_gating`]: the item is
/// ungated by the keyboard route (Quit's reasoning — Swift's version
/// targets the updater controller, which `validateMenuItem` never
/// gates), and gated instead by `SPUUpdater.canCheckForUpdates`, which
/// is false both when no updater started and while a check is already
/// in flight (plan 028 § 3.8).
pub(crate) fn sync_update_item(_mtm: MainThreadMarker, can_check: bool) {
    MENU.with(|cell| {
        let slot = cell.borrow();
        let Some(menu) = slot.as_ref() else {
            return;
        };
        menu.updates.setEnabled(can_check);
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
        window: submenu(mtm, "Window"),
        updates: plain_item(mtm, "Check for Updates\u{2026}"),
        events: Vec::new(),
        static_events: 0,
        gated: Vec::new(),
        window_gated: Vec::new(),
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
    // Born disabled: the Sparkle seam initializes AFTER the menu install
    // (both from `window_opened`), so nothing yet knows whether there is
    // an updater to check with. `sync_update_item` is what turns it on —
    // and in a bare-binary build, never does, which is precisely Swift's
    // "updater failed to start ⇒ greyed item" behavior
    // (`App.swift:3950-3956`, nil target).
    let updates = menu.updates.clone();
    updates.setEnabled(false);
    bind_event(&mut menu, &updates, MenuEvent::CheckForUpdates);
    app_menu.addItem(&updates);
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

    attach(mtm, &menu.root, &menu.window);
    menu.static_events = menu.events.len();
    // The rows come from the workspace, which no menu install may read:
    // `reconcile()` owns that and fills them in on the turn after this.
    populate_window(
        mtm,
        &mut menu,
        &WindowRows::default(),
        keybindings,
        MenuGating::default(),
    );

    menu
}

/// Rebuild the Window menu's dynamic rows from `rows`, in place.
///
/// The whole tag reassignment happens under this one `borrow_mut`, which
/// is what [`dispatch_tag`]'s `try_borrow` guards: a click that lands
/// mid-rebuild is dropped loudly rather than resolved against a table
/// that is half old and half new.
pub(crate) fn sync_window_menu(
    mtm: MainThreadMarker,
    rows: &WindowRows,
    keybindings: &HashMap<Accel, KeybindAction>,
    gating: MenuGating,
) {
    MENU.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(menu) = slot.as_mut() else {
            return;
        };
        populate_window(mtm, menu, rows, keybindings, gating);
    });
}

/// Projects, tabs of the active project, then Minimize/Zoom — the
/// item-for-item port of `rebuildWindowMenu` (`App.swift:3820-3886`),
/// including its rule that each dynamic group is followed by a separator
/// only when the group is non-empty.
///
/// Fresh rows are born with `gating` already applied rather than waiting
/// for the next [`sync_gating`]: the gate is edge-triggered on the App
/// side, and a rebuild that landed between two edges would otherwise
/// leave live rows behind a palette.
fn populate_window(
    mtm: MainThreadMarker,
    menu: &mut MainMenu,
    rows: &WindowRows,
    keybindings: &HashMap<Accel, KeybindAction>,
    gating: MenuGating,
) {
    let window = menu.window.clone();
    window.removeAllItems();
    menu.events.truncate(menu.static_events);
    menu.window_gated.clear();

    for (index, row) in rows.projects.iter().enumerate() {
        let key = positional_accel(index, KeybindAction::SwitchProject, keybindings);
        add_window_row(
            mtm,
            menu,
            &window,
            row,
            MenuEvent::SelectProject(row.id),
            key,
            gating,
        );
    }
    if !rows.projects.is_empty() {
        window.addItem(&NSMenuItem::separatorItem(mtm));
    }

    for (index, row) in rows.tabs.iter().enumerate() {
        let key = positional_accel(index, KeybindAction::SwitchTab, keybindings);
        add_window_row(
            mtm,
            menu,
            &window,
            row,
            MenuEvent::SelectTab(row.id),
            key,
            gating,
        );
    }
    if !rows.tabs.is_empty() {
        window.addItem(&NSMenuItem::separatorItem(mtm));
    }

    window.addItem(&standard_item(
        mtm,
        "Minimize",
        sel!(performMiniaturize:),
        Some(("m", MOD_COMMAND)),
    ));
    window.addItem(&standard_item(mtm, "Zoom", sel!(performZoom:), None));
}

/// The key equivalent for the row at `index`, from the table — so a user
/// who rebound `switch_tab_3` sees their own chord. Rows past the ninth
/// get none, exactly as Swift's `index < 9` guard does.
///
/// These stay POSITIONAL where the row's action is by id: the
/// `switch_project_N`/`switch_tab_N` bindings name a position by
/// definition, and a key press is dispatched now rather than a turn
/// later.
fn positional_accel(
    index: usize,
    action: fn(u8) -> KeybindAction,
    keybindings: &HashMap<Accel, KeybindAction>,
) -> Option<(String, usize)> {
    let position = u8::try_from(index + 1).ok().filter(|n| *n <= 9)?;
    menu_accel_for_action(action(position), keybindings)
        .and_then(|accel| accel_to_key_equivalent(&accel))
}

fn add_window_row<K>(
    mtm: MainThreadMarker,
    menu: &mut MainMenu,
    parent: &NSMenu,
    row: &WindowRow<K>,
    event: MenuEvent,
    key: Option<(String, usize)>,
    gating: MenuGating,
) {
    let item = plain_item(mtm, &row.title);
    bind_event(menu, &item, event);
    set_key_equivalent(&item, key);
    item.setState(if row.active {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    item.setEnabled(command_enabled(gating, false));
    parent.addItem(&item);
    menu.window_gated.push(GatedItem {
        item,
        palette_toggle: false,
        clipboard_equivalent: None,
    });
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
/// entry in `gated`, so nothing ever re-enables it (Cut, Select All).
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

// ---------------------------------------------------------------------
// Test-mode introspection: `app.menu_dump` / `app.menu_activate`
// ---------------------------------------------------------------------

/// The `app.menu_dump` payload: every top-level menu with its items,
/// read straight off the LIVE `NSApp.mainMenu` — never re-derived from
/// the keybind table, so the read proves what AppKit actually holds
/// (plan 028 § 3.12).
pub(crate) fn dump(_mtm: MainThreadMarker) -> Result<AppMenuDumpResult, String> {
    MENU.with(|cell| {
        let slot = cell.borrow();
        let menu = slot
            .as_ref()
            .ok_or_else(|| "the native menu bar is not installed yet".to_string())?;
        let menus = menu
            .root
            .itemArray()
            .iter()
            .map(|top| dump_menu(menu, &top))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(AppMenuDumpResult { menus })
    })
}

fn dump_menu(menu: &MainMenu, top: &NSMenuItem) -> Result<MenuDump, String> {
    let sub = top
        .submenu()
        .ok_or_else(|| "a menu bar item has no submenu — the menu bar is malformed".to_string())?;
    let items = sub
        .itemArray()
        .iter()
        .map(|item| dump_item(menu, &item))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MenuDump {
        title: sub.title().to_string(),
        items,
    })
}

fn dump_item(menu: &MainMenu, item: &NSMenuItem) -> Result<MenuItemDump, String> {
    if item.isSeparatorItem() {
        return Ok(MenuItemDump {
            title: String::new(),
            key_equivalent: String::new(),
            modifiers: Vec::new(),
            enabled: item.isEnabled(),
            state: "off".into(),
            separator: true,
            action: None,
        });
    }
    let state = if item.state() == NSControlStateValueOff {
        "off"
    } else if item.state() == NSControlStateValueOn {
        "on"
    } else {
        return Err(format!(
            "menu item {:?} has a mixed checkbox state, which nothing in this menu bar sets",
            item.title().to_string()
        ));
    };
    let key_equivalent = item.keyEquivalent().to_string();
    // AppKit's `keyEquivalentModifierMask` defaults to Command even on an
    // item that never had a key equivalent set at all (Cut, Select All and
    // Check for Updates…, none of which call `set_key_equivalent`) — a
    // phantom modifier on a nonexistent shortcut. Report no modifiers when
    // there is no key to modify, rather than leaking that AppKit default
    // into the wire contract.
    let modifiers = if key_equivalent.is_empty() {
        Vec::new()
    } else {
        modifier_names(item.keyEquivalentModifierMask())
    };
    Ok(MenuItemDump {
        title: item_display_title(item),
        key_equivalent,
        modifiers,
        enabled: item.isEnabled(),
        state: state.into(),
        separator: false,
        action: item_action_name(menu, item),
    })
}

/// A leaf item's own title, or — for a submenu-holding item — its
/// submenu's title. The one rule both [`dump_item`] and [`resolve`]
/// use, so a menu name resolves to the same string in both directions
/// (and the App-menu carrier's title is whatever [`build`] set it to —
/// the profile display name — with no separate normalization step).
fn item_display_title(item: &NSMenuItem) -> String {
    item.submenu()
        .map(|sub| sub.title().to_string())
        .unwrap_or_else(|| item.title().to_string())
}

fn modifier_names(mask: NSEventModifierFlags) -> Vec<String> {
    let mut names = Vec::new();
    for (flag, name) in [
        (NSEventModifierFlags::Shift, "shift"),
        (NSEventModifierFlags::Control, "ctrl"),
        (NSEventModifierFlags::Option, "alt"),
        (NSEventModifierFlags::Command, "super"),
    ] {
        if mask.contains(flag) {
            names.push(name.to_string());
        }
    }
    names
}

/// The wire `action` for an item: our own bound events resolve
/// through [`MainMenu::events`] by tag; a nil-target AppKit standard
/// item reports its selector; anything else (an inert item — Cut,
/// Select All) has none.
fn item_action_name(menu: &MainMenu, item: &NSMenuItem) -> Option<String> {
    if is_our_target(menu, item) {
        let index = usize::try_from(item.tag()).ok()?;
        menu.events.get(index).map(menu_event_wire_name)
    } else {
        item.action().map(|sel| format!("appkit:{sel}"))
    }
}

/// Whether `item`'s target is this module's own dispatch target — the
/// only reliable way to tell a table-bound/Window-row item (whose tag
/// indexes [`MainMenu::events`]) apart from a standard AppKit item
/// (nil target, action left on the responder chain), since either can
/// carry an incidental tag value.
fn is_our_target(menu: &MainMenu, item: &NSMenuItem) -> bool {
    let Some(target) = item.target() else {
        return false;
    };
    let ours: *const AnyObject = Retained::as_ptr(&menu.target).cast();
    let theirs: *const AnyObject = Retained::as_ptr(&target);
    core::ptr::eq(ours, theirs)
}

fn menu_event_wire_name(event: &MenuEvent) -> String {
    match event {
        MenuEvent::Action(action) => action.to_wire_name(),
        MenuEvent::Quit => "quit".into(),
        MenuEvent::CheckForUpdates => "check_for_updates".into(),
        // Wire names, so the key renders as the form a client speaks —
        // bare for the local instance, qualified for any other.
        MenuEvent::SelectProject(project) => format!("select_project:{project}"),
        MenuEvent::SelectTab(tab) => format!("select_tab:{tab}"),
    }
}

/// Resolve `path` through the live native menu bar by title and fire
/// it via `performActionForItemAtIndex:` — the same dispatch a real
/// click takes.
///
/// Never holds the [`MENU`] borrow while firing: [`resolve`] only
/// needs a clone of the root `NSMenu`, released immediately after,
/// because the action fires synchronously and re-enters this module
/// through [`dispatch_tag`]'s `try_borrow` — a held borrow here would
/// make the click silently drop.
pub(crate) fn activate(_mtm: MainThreadMarker, path: &[String]) -> Result<(), String> {
    if path.is_empty() {
        return Err("app.menu_activate path must not be empty".into());
    }
    let root = MENU.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|menu| menu.root.clone())
            .ok_or_else(|| "the native menu bar is not installed yet".to_string())
    })?;
    let (parent, index) = resolve(root, path)?;
    let item = parent
        .itemAtIndex(index)
        .ok_or_else(|| "resolved menu item vanished between lookup and dispatch".to_string())?;
    // `performActionForItemAtIndex:` runs NO validation of its own
    // (Apple's docs) — this enabled check is the only thing standing
    // between the op and silently firing a greyed-out item.
    if !item.isEnabled() {
        return Err(format!("menu item at {path:?} is disabled"));
    }
    parent.performActionForItemAtIndex(index);
    Ok(())
}

/// Walk `container` by title, one path segment per level, returning
/// the parent `NSMenu` and the resolved leaf's index. Both a missing
/// title and a duplicate title at the same level are errors — dynamic
/// Window rows (project/tab names) can collide, so callers must seed
/// unique names rather than rely on "first match wins".
fn resolve(
    mut container: Retained<NSMenu>,
    path: &[String],
) -> Result<(Retained<NSMenu>, isize), String> {
    for (depth, segment) in path.iter().enumerate() {
        let items = container.itemArray();
        let mut matching = items
            .iter()
            .enumerate()
            .filter(|(_, item)| item_display_title(item) == *segment);
        let Some((index, item)) = matching.next() else {
            return Err(format!("no menu item found for path {:?}", &path[..=depth]));
        };
        if matching.next().is_some() {
            return Err(format!(
                "ambiguous menu path {:?}: multiple items are titled {segment:?}",
                &path[..=depth]
            ));
        }
        drop(matching);
        let index = isize::try_from(index).unwrap_or(-1);
        if depth + 1 == path.len() {
            return Ok((container, index));
        }
        container = item
            .submenu()
            .ok_or_else(|| format!("{segment:?} has no submenu to descend into"))?;
    }
    unreachable!("path is checked non-empty before this loop runs")
}

#[cfg(test)]
mod tests {
    use roost_ipc::messages::{Tab, TabState};
    use roost_ui_model::keybind::parse_trigger;

    use super::*;

    fn accel(trigger: &str) -> Accel {
        parse_trigger(trigger).expect("trigger parses")
    }

    fn tab(id: i64, project_id: i64) -> Tab {
        Tab {
            id,
            project_id,
            title: format!("tab-{id}"),
            cwd: "/tmp".into(),
            state: TabState::None,
            has_notification: false,
            is_active: false,
            user_titled: false,
            position: 0,
            created_at: 0,
            last_active: 0,
            hook_active: false,
            shell_state: Default::default(),
            agent_lifecycle: Default::default(),
            ownership: None,
        }
    }

    fn project(id: i64, name: &str, tabs: Vec<Tab>) -> Project {
        Project {
            id,
            name: name.into(),
            cwd: "/tmp".into(),
            position: 0,
            created_at: 0,
            tabs,
        }
    }

    fn workspace() -> Vec<Project> {
        vec![
            project(1, "alpha", vec![tab(10, 1), tab(11, 1)]),
            project(2, "beta", vec![tab(20, 2)]),
        ]
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

    /// The one enabled-state rule, in both directions: a palette spares
    /// the four toggles, text capture spares nothing.
    #[test]
    fn text_capture_outranks_the_palette_exemption() {
        let open = MenuGating {
            palette_open: true,
            text_capture: false,
        };
        assert!(command_enabled(open, true));
        assert!(!command_enabled(open, false));

        let capture = MenuGating {
            palette_open: false,
            text_capture: true,
        };
        assert!(!command_enabled(capture, true));
        assert!(!command_enabled(capture, false));

        assert!(command_enabled(MenuGating::default(), false));
    }

    /// Projects in snapshot order; tabs of the ACTIVE project only, named
    /// by position rather than by title (`App.swift:3856`).
    #[test]
    fn the_rows_are_every_project_and_the_active_projects_tabs() {
        let rows = WindowRows::derive(
            &workspace(),
            HostId::LOCAL,
            ProjectKey::local(1),
            TabKey::local(11),
        );

        let projects: Vec<_> = rows
            .projects
            .iter()
            .map(|row| (row.id, row.title.as_str(), row.active))
            .collect();
        assert_eq!(
            projects,
            vec![
                (ProjectKey::local(1), "alpha", true),
                (ProjectKey::local(2), "beta", false)
            ]
        );

        let tabs: Vec<_> = rows
            .tabs
            .iter()
            .map(|row| (row.id, row.title.as_str(), row.active))
            .collect();
        assert_eq!(
            tabs,
            vec![
                (TabKey::local(10), "Tab 1", false),
                (TabKey::local(11), "Tab 2", true)
            ]
        );
    }

    /// A Window-menu activation rides the feed and is acted on a turn
    /// later, so its row carries the instance the snapshot came from — not
    /// a bare number to be re-read against whatever id-space is live by
    /// then. The same snapshot read as two instances must therefore
    /// produce rows that select two different tabs, and the active marks
    /// of one instance must never land on the other's rows.
    #[test]
    fn a_window_row_selects_its_own_instances_tab_not_the_number() {
        let host = HostId::new(4);
        let remote = WindowRows::derive(
            &workspace(),
            host,
            ProjectKey::new(host, 1),
            TabKey::new(host, 11),
        );
        assert_eq!(remote.tabs[1].id, TabKey::new(host, 11));
        assert!(remote.tabs[1].active);
        assert_ne!(remote.tabs[1].id, TabKey::local(11));

        // The local rows for the same numbers are a different selection.
        let local = WindowRows::derive(
            &workspace(),
            HostId::LOCAL,
            ProjectKey::local(1),
            TabKey::local(11),
        );
        assert_ne!(local, remote);

        // A local active mark cannot check a host's row, and vice versa.
        let mismatched = WindowRows::derive(
            &workspace(),
            host,
            ProjectKey::new(host, 1),
            TabKey::local(11),
        );
        assert!(
            mismatched.tabs.iter().all(|row| !row.active),
            "tab 11 on the local instance is not tab 11 on this host"
        );
        assert!(!mismatched.matches(
            &workspace(),
            host,
            ProjectKey::new(host, 1),
            TabKey::new(host, 11)
        ));

        // The wire name a client sees stays bare for the local instance
        // and is qualified for any other.
        assert_eq!(
            menu_event_wire_name(&MenuEvent::SelectTab(TabKey::local(11))),
            "select_tab:11"
        );
        assert_eq!(
            menu_event_wire_name(&MenuEvent::SelectTab(TabKey::new(host, 11))),
            "select_tab:h4.11"
        );
        assert_eq!(
            menu_event_wire_name(&MenuEvent::SelectProject(ProjectKey::local(1))),
            "select_project:1"
        );
        assert_eq!(
            menu_event_wire_name(&MenuEvent::SelectProject(ProjectKey::new(host, 1))),
            "select_project:h4.1"
        );
    }

    /// Selecting the other project re-aims the tab rows at ITS tabs.
    #[test]
    fn switching_project_switches_which_tabs_the_rows_show() {
        let rows = WindowRows::derive(
            &workspace(),
            HostId::LOCAL,
            ProjectKey::local(2),
            TabKey::local(20),
        );
        assert_eq!(
            rows.tabs.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![TabKey::local(20)]
        );
        assert!(rows.projects[1].active && !rows.projects[0].active);
    }

    /// A workspace with no project the active id names (mid-delete, or an
    /// empty workspace) renders projects with no checkmark and no tabs —
    /// never a panic and never a row pointing at a project that is gone.
    #[test]
    fn an_unknown_active_project_leaves_the_tab_rows_empty() {
        let rows = WindowRows::derive(
            &workspace(),
            HostId::LOCAL,
            ProjectKey::local(99),
            TabKey::local(10),
        );
        assert_eq!(rows.projects.len(), 2);
        assert!(rows.projects.iter().all(|row| !row.active));
        assert!(rows.tabs.is_empty());
        assert_eq!(
            WindowRows::derive(&[], HostId::LOCAL, ProjectKey::local(0), TabKey::local(0)),
            WindowRows::default()
        );
    }

    /// The change detection reconcile leans on: every field the menu
    /// renders is part of the comparison, and nothing else is.
    #[test]
    fn the_model_compares_equal_only_when_the_menu_would_look_the_same() {
        let base = WindowRows::derive(
            &workspace(),
            HostId::LOCAL,
            ProjectKey::local(1),
            TabKey::local(10),
        );
        assert_eq!(
            base,
            WindowRows::derive(
                &workspace(),
                HostId::LOCAL,
                ProjectKey::local(1),
                TabKey::local(10)
            )
        );

        // A tab's own title is not rendered, so churning it is not a
        // rebuild; its cwd and position aren't either.
        let mut untouched = workspace();
        untouched[0].tabs[0].title = "renamed by the shell".into();
        untouched[0].tabs[0].cwd = "/elsewhere".into();
        assert_eq!(
            base,
            WindowRows::derive(
                &untouched,
                HostId::LOCAL,
                ProjectKey::local(1),
                TabKey::local(10)
            )
        );

        // Everything the menu does render is.
        let mut renamed = workspace();
        renamed[0].name = "renamed".into();
        assert_ne!(
            base,
            WindowRows::derive(
                &renamed,
                HostId::LOCAL,
                ProjectKey::local(1),
                TabKey::local(10)
            )
        );

        let mut reordered = workspace();
        reordered.swap(0, 1);
        assert_ne!(
            base,
            WindowRows::derive(
                &reordered,
                HostId::LOCAL,
                ProjectKey::local(1),
                TabKey::local(10)
            )
        );

        let mut closed = workspace();
        closed[0].tabs.pop();
        assert_ne!(
            base,
            WindowRows::derive(
                &closed,
                HostId::LOCAL,
                ProjectKey::local(1),
                TabKey::local(10)
            )
        );

        assert_ne!(
            base,
            WindowRows::derive(
                &workspace(),
                HostId::LOCAL,
                ProjectKey::local(1),
                TabKey::local(11)
            )
        );
        assert_ne!(
            base,
            WindowRows::derive(
                &workspace(),
                HostId::LOCAL,
                ProjectKey::local(2),
                TabKey::local(10)
            )
        );
    }

    /// The equivalence pin `sync_window_menu` leans on for skipping
    /// `derive`: for a varied set of workspace shapes — empty titles
    /// hitting the "Tab N" fallback, a title edit, an active-selection
    /// change, and a project add/remove — `matches` must agree with
    /// `derive`-then-compare in every case, not just the happy path.
    #[test]
    fn matches_agrees_with_derive_equality() {
        fn check(
            rows: &WindowRows,
            projects: &[Project],
            active_project_id: i64,
            active_tab_id: i64,
        ) {
            let active_project = ProjectKey::local(active_project_id);
            let active_tab = TabKey::local(active_tab_id);
            let expected =
                WindowRows::derive(projects, HostId::LOCAL, active_project, active_tab) == *rows;
            assert_eq!(
                rows.matches(projects, HostId::LOCAL, active_project, active_tab),
                expected,
                "projects={projects:?} active_project_id={active_project_id} active_tab_id={active_tab_id} rows={rows:?}"
            );
        }

        let ws = workspace();
        let base = WindowRows::derive(&ws, HostId::LOCAL, ProjectKey::local(1), TabKey::local(11));

        // Exact match.
        check(&base, &ws, 1, 11);

        // Active project changed.
        check(&base, &ws, 2, 20);

        // Active tab changed (same project).
        check(&base, &ws, 1, 10);

        // A project's name changed — the "Tab N" rows are untouched by a
        // pure name edit, but the project row itself must miss.
        let mut renamed = ws.clone();
        renamed[0].name = "alpha-renamed".into();
        check(&base, &renamed, 1, 11);

        // A tab's own title changed — irrelevant to the rendered rows
        // (they're positional "Tab N"), so this must still match.
        let mut retitled = ws.clone();
        retitled[0].tabs[0].title = "renamed by the shell".into();
        check(&base, &retitled, 1, 11);

        // Project order changed.
        let mut reordered = ws.clone();
        reordered.swap(0, 1);
        check(&base, &reordered, 1, 11);

        // A project removed.
        let mut fewer = ws.clone();
        fewer.pop();
        check(&base, &fewer, 1, 11);

        // A project added.
        let mut more = ws.clone();
        more.push(project(3, "gamma", vec![tab(30, 3)]));
        check(&base, &more, 1, 11);

        // A tab removed from the active project.
        let mut fewer_tabs = ws.clone();
        fewer_tabs[0].tabs.pop();
        check(&base, &fewer_tabs, 1, 11);

        // A tab added to the active project.
        let mut more_tabs = ws.clone();
        more_tabs[0].tabs.push(tab(12, 1));
        check(&base, &more_tabs, 1, 11);

        // Empty-titled workspace and rows — the "Tab N" fallback with no
        // projects/tabs at all.
        let empty_rows = WindowRows::default();
        check(&empty_rows, &[], 0, 0);
        check(&empty_rows, &ws, 1, 11);

        // A lookalike "Tab N" string that a numeric parse (rather than an
        // exact digit-buffer compare) would wrongly accept.
        let mut lookalike = base.clone();
        if let Some(row) = lookalike.tabs.first_mut() {
            row.title = "Tab 01".into();
        }
        check(&lookalike, &ws, 1, 11);

        // An unknown active project id — no project checkmarked, tabs
        // empty.
        check(&base, &ws, 99, 10);
    }

    /// Rows are bound from the table, so a rebind reaches the menu; only
    /// the first nine of each group get a chord at all.
    #[test]
    fn row_accels_come_from_the_table_and_stop_after_nine() {
        let bindings = roost_ui_model::keybind::canonicalize_bindings(
            roost_ui_model::keybind::default_bindings(),
            Vec::new(),
            |_| {},
        );
        assert_eq!(
            positional_accel(0, KeybindAction::SwitchProject, &bindings),
            Some(("1".into(), MOD_COMMAND))
        );
        assert_eq!(
            positional_accel(8, KeybindAction::SwitchTab, &bindings),
            Some(("9".into(), MOD_CONTROL))
        );
        assert_eq!(
            positional_accel(9, KeybindAction::SwitchTab, &bindings),
            None
        );

        let rebound = roost_ui_model::keybind::canonicalize_bindings(
            roost_ui_model::keybind::default_bindings(),
            vec![("super+shift+2".into(), "switch_tab_2".into())],
            |_| {},
        );
        assert_eq!(
            positional_accel(1, KeybindAction::SwitchTab, &rebound),
            Some(("2".into(), MOD_COMMAND | MOD_SHIFT)),
            "a user's own binding reaches the row, not the default"
        );
    }
}
