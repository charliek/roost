//! gdk → libghostty-vt key encoder bridge.
//!
//! Mirrors `mac/Sources/Roost/KeyEncoder.swift` (Phase 6 M1) in shape:
//! translate the platform key event (`gdk::Key` + `gdk::ModifierType`)
//! to the libghostty `(Key, Mods, utf8)` triple, then run it through
//! `roost_vt::KeyEncoder::encode` after syncing options from the
//! current terminal modes (DECCKM, Kitty flags, modifyOtherKeys).
//!
//! Coverage at this commit:
//!   * All ASCII letters + digits + the bracket / quote / arithmetic
//!     punctuation set.
//!   * Navigation: arrows, Home/End, Page Up/Down, Insert, Delete.
//!   * Editing: Enter, Tab (incl. Shift+Tab → CSI Z), Backspace,
//!     Escape, Space.
//!   * F1-F24 function keys.
//!   * Modifiers as a bitmask: Shift, Ctrl, Alt, Super.
//!   * Printable Unicode → UTF-8 via `gdk::Key::to_unicode`, filtered
//!     against C0/DEL so the encoder doesn't double up on a control
//!     code it should already derive from the physical key.
//!
//! Deferred (still works at the round-trip level via the UTF-8
//! fallback path, just without the rich CSI-u form): IME composition,
//! international keyboard layouts that emit non-ASCII Unicode for
//! a "physical" key the encoder doesn't recognize.

use gtk4::gdk;

use roost_vt::{key_action, mods as gmods, Key, KeyEvent, Mods, Terminal};

/// Encode a single keypress against the given terminal's current
/// modes. Returns the VT bytes to send into the PTY (empty for
/// modifier-only events).
pub fn encode_key(
    encoder: &mut roost_vt::KeyEncoder,
    terminal: &Terminal,
    key: gdk::Key,
    gdk_mods: gdk::ModifierType,
) -> Vec<u8> {
    // Sync encoder options from the terminal each keystroke — modes
    // change between keystrokes (cursor-key application, Kitty flags,
    // bracketed paste). Cheap, idempotent.
    encoder.sync_from_terminal(terminal);

    let mods = translate_mods(gdk_mods);
    let ghostty_key = translate_key(key);

    // UTF-8 only for printable characters. The encoder's own
    // formatting handles C0 / DEL / Esc derivations from the physical
    // key + modifier bits, so feeding utf8 there would double-emit.
    let utf8_bytes = key
        .to_unicode()
        .filter(|c| !c.is_control() && (*c as u32) < 0xE000)
        .map(|c| c.to_string().into_bytes())
        .unwrap_or_default();

    // Drop pure-modifier presses — they have no key. (Modifier-only
    // events arrive when the user holds Shift / Ctrl / etc; the
    // encoder would emit empty bytes anyway, but skipping avoids a
    // libghostty round-trip per keystroke.)
    if ghostty_key == 0 && utf8_bytes.is_empty() {
        return Vec::new();
    }

    let mut event = match KeyEvent::new() {
        Ok(ev) => ev,
        Err(err) => {
            tracing::warn!(?err, "KeyEvent::new failed");
            return Vec::new();
        }
    };
    event.set_action(key_action::PRESS);
    event.set_key(ghostty_key);
    event.set_mods(mods);
    event.set_composing(false);
    event.set_unshifted_codepoint(0);
    if !utf8_bytes.is_empty() {
        event.set_utf8(&utf8_bytes);
    }

    match encoder.encode(&event) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(?err, "key encoder failed");
            Vec::new()
        }
    }
}

fn translate_mods(m: gdk::ModifierType) -> Mods {
    let mut bits: u16 = 0;
    if m.contains(gdk::ModifierType::SHIFT_MASK) {
        bits |= gmods::SHIFT;
    }
    if m.contains(gdk::ModifierType::CONTROL_MASK) {
        bits |= gmods::CTRL;
    }
    if m.contains(gdk::ModifierType::ALT_MASK) {
        bits |= gmods::ALT;
    }
    if m.contains(gdk::ModifierType::SUPER_MASK) {
        bits |= gmods::SUPER;
    }
    if m.contains(gdk::ModifierType::LOCK_MASK) {
        bits |= gmods::CAPS_LOCK;
    }
    bits
}

/// Map a `gdk::Key` to libghostty's `Key` enum value. The full
/// table is mechanical; we use direct `gdk::Key::*` matches because
/// gtk4-rs's `Key` is an opaque newtype around the GDK keyval.
fn translate_key(key: gdk::Key) -> Key {
    use gdk::Key as K;
    use roost_vt::ffi as g;

    match key {
        // Letters — GDK emits the lowercase + uppercase variants; the
        // encoder ignores case (Shift comes through mods).
        K::a | K::A => g::GhosttyKey_GHOSTTY_KEY_A,
        K::b | K::B => g::GhosttyKey_GHOSTTY_KEY_B,
        K::c | K::C => g::GhosttyKey_GHOSTTY_KEY_C,
        K::d | K::D => g::GhosttyKey_GHOSTTY_KEY_D,
        K::e | K::E => g::GhosttyKey_GHOSTTY_KEY_E,
        K::f | K::F => g::GhosttyKey_GHOSTTY_KEY_F,
        K::g | K::G => g::GhosttyKey_GHOSTTY_KEY_G,
        K::h | K::H => g::GhosttyKey_GHOSTTY_KEY_H,
        K::i | K::I => g::GhosttyKey_GHOSTTY_KEY_I,
        K::j | K::J => g::GhosttyKey_GHOSTTY_KEY_J,
        K::k | K::K => g::GhosttyKey_GHOSTTY_KEY_K,
        K::l | K::L => g::GhosttyKey_GHOSTTY_KEY_L,
        K::m | K::M => g::GhosttyKey_GHOSTTY_KEY_M,
        K::n | K::N => g::GhosttyKey_GHOSTTY_KEY_N,
        K::o | K::O => g::GhosttyKey_GHOSTTY_KEY_O,
        K::p | K::P => g::GhosttyKey_GHOSTTY_KEY_P,
        K::q | K::Q => g::GhosttyKey_GHOSTTY_KEY_Q,
        K::r | K::R => g::GhosttyKey_GHOSTTY_KEY_R,
        K::s | K::S => g::GhosttyKey_GHOSTTY_KEY_S,
        K::t | K::T => g::GhosttyKey_GHOSTTY_KEY_T,
        K::u | K::U => g::GhosttyKey_GHOSTTY_KEY_U,
        K::v | K::V => g::GhosttyKey_GHOSTTY_KEY_V,
        K::w | K::W => g::GhosttyKey_GHOSTTY_KEY_W,
        K::x | K::X => g::GhosttyKey_GHOSTTY_KEY_X,
        K::y | K::Y => g::GhosttyKey_GHOSTTY_KEY_Y,
        K::z | K::Z => g::GhosttyKey_GHOSTTY_KEY_Z,

        // Digits (top row only — numpad maps separately).
        K::_0 => g::GhosttyKey_GHOSTTY_KEY_DIGIT_0,
        K::_1 => g::GhosttyKey_GHOSTTY_KEY_DIGIT_1,
        K::_2 => g::GhosttyKey_GHOSTTY_KEY_DIGIT_2,
        K::_3 => g::GhosttyKey_GHOSTTY_KEY_DIGIT_3,
        K::_4 => g::GhosttyKey_GHOSTTY_KEY_DIGIT_4,
        K::_5 => g::GhosttyKey_GHOSTTY_KEY_DIGIT_5,
        K::_6 => g::GhosttyKey_GHOSTTY_KEY_DIGIT_6,
        K::_7 => g::GhosttyKey_GHOSTTY_KEY_DIGIT_7,
        K::_8 => g::GhosttyKey_GHOSTTY_KEY_DIGIT_8,
        K::_9 => g::GhosttyKey_GHOSTTY_KEY_DIGIT_9,

        // Punctuation.
        K::grave | K::asciitilde => g::GhosttyKey_GHOSTTY_KEY_BACKQUOTE,
        K::backslash | K::bar => g::GhosttyKey_GHOSTTY_KEY_BACKSLASH,
        K::bracketleft | K::braceleft => g::GhosttyKey_GHOSTTY_KEY_BRACKET_LEFT,
        K::bracketright | K::braceright => g::GhosttyKey_GHOSTTY_KEY_BRACKET_RIGHT,
        K::comma | K::less => g::GhosttyKey_GHOSTTY_KEY_COMMA,
        K::equal | K::plus => g::GhosttyKey_GHOSTTY_KEY_EQUAL,
        K::minus | K::underscore => g::GhosttyKey_GHOSTTY_KEY_MINUS,
        K::period | K::greater => g::GhosttyKey_GHOSTTY_KEY_PERIOD,
        K::apostrophe | K::quotedbl => g::GhosttyKey_GHOSTTY_KEY_QUOTE,
        K::semicolon | K::colon => g::GhosttyKey_GHOSTTY_KEY_SEMICOLON,
        K::slash | K::question => g::GhosttyKey_GHOSTTY_KEY_SLASH,

        // Editing / navigation.
        K::Return | K::ISO_Enter | K::KP_Enter => g::GhosttyKey_GHOSTTY_KEY_ENTER,
        K::Tab | K::ISO_Left_Tab => g::GhosttyKey_GHOSTTY_KEY_TAB,
        K::BackSpace => g::GhosttyKey_GHOSTTY_KEY_BACKSPACE,
        K::Escape => g::GhosttyKey_GHOSTTY_KEY_ESCAPE,
        K::space => g::GhosttyKey_GHOSTTY_KEY_SPACE,
        K::Delete => g::GhosttyKey_GHOSTTY_KEY_DELETE,
        K::Insert => g::GhosttyKey_GHOSTTY_KEY_INSERT,
        K::Home => g::GhosttyKey_GHOSTTY_KEY_HOME,
        K::End => g::GhosttyKey_GHOSTTY_KEY_END,
        K::Page_Up => g::GhosttyKey_GHOSTTY_KEY_PAGE_UP,
        K::Page_Down => g::GhosttyKey_GHOSTTY_KEY_PAGE_DOWN,
        K::Up => g::GhosttyKey_GHOSTTY_KEY_ARROW_UP,
        K::Down => g::GhosttyKey_GHOSTTY_KEY_ARROW_DOWN,
        K::Left => g::GhosttyKey_GHOSTTY_KEY_ARROW_LEFT,
        K::Right => g::GhosttyKey_GHOSTTY_KEY_ARROW_RIGHT,

        // Function keys F1-F24.
        K::F1 => g::GhosttyKey_GHOSTTY_KEY_F1,
        K::F2 => g::GhosttyKey_GHOSTTY_KEY_F2,
        K::F3 => g::GhosttyKey_GHOSTTY_KEY_F3,
        K::F4 => g::GhosttyKey_GHOSTTY_KEY_F4,
        K::F5 => g::GhosttyKey_GHOSTTY_KEY_F5,
        K::F6 => g::GhosttyKey_GHOSTTY_KEY_F6,
        K::F7 => g::GhosttyKey_GHOSTTY_KEY_F7,
        K::F8 => g::GhosttyKey_GHOSTTY_KEY_F8,
        K::F9 => g::GhosttyKey_GHOSTTY_KEY_F9,
        K::F10 => g::GhosttyKey_GHOSTTY_KEY_F10,
        K::F11 => g::GhosttyKey_GHOSTTY_KEY_F11,
        K::F12 => g::GhosttyKey_GHOSTTY_KEY_F12,
        K::F13 => g::GhosttyKey_GHOSTTY_KEY_F13,
        K::F14 => g::GhosttyKey_GHOSTTY_KEY_F14,
        K::F15 => g::GhosttyKey_GHOSTTY_KEY_F15,
        K::F16 => g::GhosttyKey_GHOSTTY_KEY_F16,
        K::F17 => g::GhosttyKey_GHOSTTY_KEY_F17,
        K::F18 => g::GhosttyKey_GHOSTTY_KEY_F18,
        K::F19 => g::GhosttyKey_GHOSTTY_KEY_F19,
        K::F20 => g::GhosttyKey_GHOSTTY_KEY_F20,
        K::F21 => g::GhosttyKey_GHOSTTY_KEY_F21,
        K::F22 => g::GhosttyKey_GHOSTTY_KEY_F22,
        K::F23 => g::GhosttyKey_GHOSTTY_KEY_F23,
        K::F24 => g::GhosttyKey_GHOSTTY_KEY_F24,

        // Pure modifier keys — the encoder treats these as
        // UNIDENTIFIED + uses the mods bitmask. Returning 0 makes the
        // outer `encode_key` drop the event (no key, no utf8).
        K::Shift_L | K::Shift_R => g::GhosttyKey_GHOSTTY_KEY_SHIFT_LEFT,
        K::Control_L | K::Control_R => g::GhosttyKey_GHOSTTY_KEY_CONTROL_LEFT,
        K::Alt_L | K::Alt_R | K::Meta_L | K::Meta_R => g::GhosttyKey_GHOSTTY_KEY_ALT_LEFT,
        K::Super_L | K::Super_R => g::GhosttyKey_GHOSTTY_KEY_META_LEFT,
        K::Caps_Lock => g::GhosttyKey_GHOSTTY_KEY_CAPS_LOCK,

        _ => g::GhosttyKey_GHOSTTY_KEY_UNIDENTIFIED,
    }
}
