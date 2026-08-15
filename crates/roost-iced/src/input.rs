use iced::keyboard::{self, key::Named, Key};
use roost_ui_model::keybind::{Accel, AccelMods};
use roost_vt::ffi as ghostty;
use roost_vt::{key_action, mods, KeyEncoder, KeyEvent, PageDirection, Terminal};

/// Translate an Iced key press into the toolkit-neutral accelerator grammar.
/// Physical Latin lookup keeps configured letter bindings usable under a
/// non-Latin layout, matching the terminal encoder's existing behavior.
pub(crate) fn accelerator(event: &keyboard::Event) -> Option<Accel> {
    let keyboard::Event::KeyPressed {
        key,
        modified_key,
        physical_key,
        modifiers,
        ..
    } = event
    else {
        return None;
    };
    let logical = key.as_ref();
    // A toolkit that reports the control transform as the logical key would
    // otherwise bind `\u{1}` — the ASCII arm below accepts it — and no
    // configured ctrl+letter or ctrl+punctuation keybind could ever match.
    // The press's `text` is deliberately not consulted here, and the chord's
    // unshifted identity is what names the binding: the accelerator grammar
    // names the key, not the bytes it produced, so `ctrl+shift+[` stays
    // `bracketleft` on either event shape rather than becoming `braceleft`.
    let mut recovered = [0u8; 4];
    let key = match control_chord(&logical, &modified_key.as_ref(), *modifiers, None) {
        Some(chord) => canonical_character(chord.key.encode_utf8(&mut recovered)),
        None => match logical {
            // Prefer the logical value for ASCII so shifted punctuation keeps its
            // shared GTK-style name (`+` -> `plus`, `{` -> `braceleft`). Physical
            // Latin is only a layout fallback; taking it first collapses `+` to
            // `equal` and makes configured punctuation aliases unreachable.
            Key::Character(value) if value.is_ascii() => canonical_character(value),
            _ => key
                .to_latin(*physical_key)
                .map(|value| canonical_character(&value.to_string()))
                .or_else(|| accelerator_key(&logical))?,
        },
    };
    Some(Accel {
        modifiers: accelerator_modifiers(*modifiers),
        key,
    })
}

pub(crate) fn accelerator_modifiers(value: keyboard::Modifiers) -> AccelMods {
    let mut result = AccelMods::empty();
    if value.shift() {
        result |= AccelMods::SHIFT;
    }
    if value.control() {
        result |= AccelMods::CTRL;
    }
    if value.alt() {
        result |= AccelMods::ALT;
    }
    if value.logo() {
        result |= AccelMods::SUPER;
    }
    result
}

/// A key press that is not a bare modifier — the events a live IME
/// composition owns, and the real terminal presses that snap local
/// scrollback first. Releases, modifier-state events, and modifier-only
/// presses must not disturb scrollback or selection before a later copy
/// chord.
pub(crate) fn non_modifier_press(event: &keyboard::Event) -> bool {
    let keyboard::Event::KeyPressed { key, .. } = event else {
        return false;
    };
    !matches!(
        key,
        Key::Named(
            Named::Alt
                | Named::AltGraph
                | Named::Control
                | Named::Fn
                | Named::FnLock
                | Named::Shift
                | Named::Symbol
                | Named::SymbolLock
                | Named::Meta
                | Named::Hyper
                | Named::Super
        )
    )
}

/// The page direction of an unmodified Page Up / Page Down press, or `None`
/// for anything else. Any modifier — including Logo — hands the key back to
/// the application, so only the bare key can reach the local viewport. The
/// tracked window modifiers are checked alongside the event's own bits so a
/// held modifier disqualifies the press from either source. Repeats are
/// deliberately included: holding the key pages repeatedly.
pub(crate) fn bare_page_direction(
    event: &keyboard::Event,
    tracked: keyboard::Modifiers,
) -> Option<PageDirection> {
    let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
        return None;
    };
    if !tracked.is_empty() || !modifiers.is_empty() {
        return None;
    }
    match key {
        Key::Named(Named::PageUp) => Some(PageDirection::Up),
        Key::Named(Named::PageDown) => Some(PageDirection::Down),
        _ => None,
    }
}

fn accelerator_key(key: &Key<&str>) -> Option<String> {
    let name = match key {
        Key::Character(value) => return Some(canonical_character(value)),
        Key::Named(Named::Enter) => "return",
        Key::Named(Named::Tab) => "tab",
        Key::Named(Named::Backspace) => "backspace",
        Key::Named(Named::Escape) => "escape",
        Key::Named(Named::Space) => "space",
        Key::Named(Named::Delete) => "delete",
        Key::Named(Named::Insert) => "insert",
        Key::Named(Named::Home) => "home",
        Key::Named(Named::End) => "end",
        Key::Named(Named::PageUp) => "page_up",
        Key::Named(Named::PageDown) => "page_down",
        Key::Named(Named::ArrowUp) => "up",
        Key::Named(Named::ArrowDown) => "down",
        Key::Named(Named::ArrowLeft) => "left",
        Key::Named(Named::ArrowRight) => "right",
        Key::Named(Named::F1) => "f1",
        Key::Named(Named::F2) => "f2",
        Key::Named(Named::F3) => "f3",
        Key::Named(Named::F4) => "f4",
        Key::Named(Named::F5) => "f5",
        Key::Named(Named::F6) => "f6",
        Key::Named(Named::F7) => "f7",
        Key::Named(Named::F8) => "f8",
        Key::Named(Named::F9) => "f9",
        Key::Named(Named::F10) => "f10",
        Key::Named(Named::F11) => "f11",
        Key::Named(Named::F12) => "f12",
        _ => return None,
    };
    Some(name.to_string())
}

/// A control-transformed press, split into the two characters it stands
/// for: the text it would have typed and the key that typed it.
struct ControlChord {
    /// What libghostty receives as the press's utf8 — shift included, since
    /// the transform's own table is keyed on the typed character
    /// (`ctrl+shift+-` has to arrive as `_` to fold into 0x1f).
    text: char,
    /// What identifies the key: the unshifted character, so the key enum,
    /// the unshifted codepoint and the accelerator name agree with the
    /// press shape where the layout character arrives intact.
    key: char,
}

/// The printable characters behind a control-transformed press, or `None`
/// when the press is not a chord whose C0 byte inverts unambiguously.
///
/// The platform applies the control transform for us and hands the C0 back
/// in a field that varies: macOS puts it in `text` (`NSEvent.characters`)
/// while the logical key keeps the layout's character, and xkb does the same
/// through `text_with_all_modifiers`; a toolkit that instead reports the C0
/// as the logical key inverts the same way. libghostty keys its
/// control-sequence table on the *printable* byte, so forwarding the C0 as
/// utf8 misses the table and falls through to CSI-u — ctrl+a reaches the PTY
/// as `\x1b[1;5u` instead of 0x01. The Swift and GTK encoders never forward
/// C0 text for the same reason (`KeyEncoder.swift::printableUTF8`,
/// `key_encoder.rs`'s `to_unicode` filter).
///
/// Only the unambiguous chords invert: the letters and `[ \ ] _`. NUL
/// (ctrl+shift+2), DEL, RS (ctrl+shift+6) and every named key — ctrl+Enter's
/// `\r`, ctrl+space's NUL — keep today's encoding. `modified_key`, the press
/// with every modifier except ctrl applied, wins whenever it agrees with the
/// C0 so a shifted chord keeps the character the layout typed (ctrl+shift+-
/// → `_`, ctrl+shift+a → `A`); the canonical inverse is the layout-blind
/// fallback that still recovers non-Latin layouts (ctrl+ф → `a`).
fn control_chord(
    key: &Key<&str>,
    modified_key: &Key<&str>,
    modifiers: keyboard::Modifiers,
    text: Option<&str>,
) -> Option<ControlChord> {
    if !modifiers.control() || !matches!(key, Key::Character(_)) {
        return None;
    }
    let c0 = [text.and_then(sole_char), sole_key_char(key)]
        .into_iter()
        .flatten()
        .find(|value| value.is_control())?;
    let canonical = match c0 {
        '\u{1}'..='\u{1a}' => char::from(b'a' + c0 as u8 - 1),
        '\u{1b}' => '[',
        '\u{1c}' => '\\',
        '\u{1d}' => ']',
        '\u{1f}' => '_',
        _ => return None,
    };
    Some(ControlChord {
        text: sole_key_char(modified_key)
            .filter(|value| control_transform(*value) == Some(c0))
            .unwrap_or(canonical),
        key: unshifted_character(canonical),
    })
}

/// The unshifted character that types `value`: `_` is shift plus the minus
/// key, and `{ } |` are the shifted brackets and backslash. Letters
/// lowercase. Keeping the key identity unshifted is what makes both event
/// shapes name the same key — `ctrl+shift+-` reports the minus key on the
/// shape that hands us `-` and on the one that hands us the C0.
fn unshifted_character(value: char) -> char {
    match value {
        '_' => '-',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        _ => value.to_ascii_lowercase(),
    }
}

/// The C0 byte a platform produces for ctrl plus this character, for the
/// chords [`control_chord`] can invert.
fn control_transform(value: char) -> Option<char> {
    let byte = u8::try_from(value).ok()?;
    Some(char::from(match byte {
        b'a'..=b'z' => byte - b'a' + 1,
        b'A'..=b'Z' => byte - b'A' + 1,
        b'[' | b'{' => 0x1b,
        b'\\' | b'|' => 0x1c,
        b']' | b'}' => 0x1d,
        b'_' => 0x1f,
        _ => return None,
    }))
}

fn sole_char(value: &str) -> Option<char> {
    let mut chars = value.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

fn sole_key_char(key: &Key<&str>) -> Option<char> {
    match key {
        Key::Character(value) => sole_char(value),
        _ => None,
    }
}

fn canonical_character(value: &str) -> String {
    let name = match value {
        "[" => "bracketleft",
        "{" => "braceleft",
        "]" => "bracketright",
        "}" => "braceright",
        "=" => "equal",
        "+" => "plus",
        "-" => "minus",
        "_" => "underscore",
        " " => "space",
        _ => return value.to_ascii_lowercase(),
    };
    name.to_string()
}

/// Allocate a libghostty key event. A failure here is an allocation
/// failure inside the FFI, not a key we can encode differently, so it
/// warns and drops the keystroke.
fn new_key_event() -> Option<KeyEvent> {
    KeyEvent::new()
        .inspect_err(|error| tracing::warn!(?error, "failed to allocate libghostty key event"))
        .ok()
}

pub fn encode_press(
    encoder: &mut KeyEncoder,
    terminal: &Terminal,
    event: keyboard::Event,
    composing: bool,
) -> Vec<u8> {
    let keyboard::Event::KeyPressed {
        key,
        modified_key,
        physical_key,
        modifiers,
        text,
        repeat,
        ..
    } = event
    else {
        return Vec::new();
    };

    let chord = control_chord(
        &key.as_ref(),
        &modified_key.as_ref(),
        modifiers,
        text.as_deref(),
    );
    let latin = key.to_latin(physical_key);
    let key = key.as_ref();
    // A logical key that is the C0 itself carries no libghostty key enum and
    // no usable unshifted codepoint; the chord's unshifted character supplies
    // both, so this shape reports the same key as the shape that keeps the
    // layout character in `key`.
    let recovered_key = chord
        .as_ref()
        .map(|chord| chord.key)
        .filter(|_| sole_key_char(&key).is_some_and(char::is_control));
    let Some(keycode) = recovered_key
        .map(character_key_char)
        .or_else(|| ghostty_key(&key))
    else {
        return Vec::new();
    };
    let Some(mut event) = new_key_event() else {
        return Vec::new();
    };
    // The control transform goes back to the character it came from — the
    // byte libghostty's control-sequence table is keyed on.
    let mut control_buf = [0u8; 4];
    let utf8 = chord
        .map(|chord| &*chord.text.encode_utf8(&mut control_buf))
        .or(text.as_deref())
        .unwrap_or("")
        .as_bytes();
    let unshifted = match recovered_key {
        Some(value) => u32::from(value),
        None => latin.map_or_else(
            || match key {
                Key::Character(value) => value.chars().next().map_or(0, u32::from),
                Key::Named(Named::Space) => u32::from(' '),
                _ => 0,
            },
            u32::from,
        ),
    };
    event
        .set_action(if repeat {
            key_action::REPEAT
        } else {
            key_action::PRESS
        })
        .set_key(keycode)
        .set_mods(ghostty_modifiers(modifiers))
        .set_consumed_mods(0)
        .set_composing(composing)
        .set_unshifted_codepoint(unshifted)
        .set_utf8(utf8);
    encoder.sync_from_terminal(terminal);
    encoder.encode(&event).unwrap_or_else(|error| {
        tracing::warn!(?error, "libghostty key encoding failed");
        Vec::new()
    })
}

/// Encode text the platform input method committed.
///
/// A commit has no originating key — only the resulting text — so this
/// takes libghostty's documented path for a printable codepoint with no
/// physical-key enum: `UNIDENTIFIED` plus the utf8 payload. `composing`
/// is false because the composition is over; the bytes are meant to
/// reach the PTY.
pub fn encode_ime_commit(encoder: &mut KeyEncoder, terminal: &Terminal, text: &str) -> Vec<u8> {
    if text.is_empty() {
        return Vec::new();
    }
    let Some(mut event) = new_key_event() else {
        return Vec::new();
    };
    event
        .set_action(key_action::PRESS)
        .set_key(ghostty::GhosttyKey_GHOSTTY_KEY_UNIDENTIFIED)
        .set_mods(0)
        .set_consumed_mods(0)
        .set_composing(false)
        .set_unshifted_codepoint(0)
        .set_utf8(text.as_bytes());
    encoder.sync_from_terminal(terminal);
    encoder.encode(&event).unwrap_or_else(|error| {
        tracing::warn!(?error, "libghostty IME commit encoding failed");
        Vec::new()
    })
}

pub(crate) fn ghostty_modifiers(value: keyboard::Modifiers) -> u16 {
    let mut result = 0;
    if value.shift() {
        result |= mods::SHIFT;
    }
    if value.control() {
        result |= mods::CTRL;
    }
    if value.alt() {
        result |= mods::ALT;
    }
    if value.logo() {
        result |= mods::SUPER;
    }
    result
}

fn ghostty_key(key: &Key<&str>) -> Option<u32> {
    use ghostty::*;
    Some(match key {
        Key::Named(Named::Enter) => GhosttyKey_GHOSTTY_KEY_ENTER,
        Key::Named(Named::Tab) => GhosttyKey_GHOSTTY_KEY_TAB,
        Key::Named(Named::Backspace) => GhosttyKey_GHOSTTY_KEY_BACKSPACE,
        Key::Named(Named::Escape) => GhosttyKey_GHOSTTY_KEY_ESCAPE,
        Key::Named(Named::Space) => GhosttyKey_GHOSTTY_KEY_SPACE,
        Key::Named(Named::Delete) => GhosttyKey_GHOSTTY_KEY_DELETE,
        Key::Named(Named::Insert) => GhosttyKey_GHOSTTY_KEY_INSERT,
        Key::Named(Named::Home) => GhosttyKey_GHOSTTY_KEY_HOME,
        Key::Named(Named::End) => GhosttyKey_GHOSTTY_KEY_END,
        Key::Named(Named::PageUp) => GhosttyKey_GHOSTTY_KEY_PAGE_UP,
        Key::Named(Named::PageDown) => GhosttyKey_GHOSTTY_KEY_PAGE_DOWN,
        Key::Named(Named::ArrowUp) => GhosttyKey_GHOSTTY_KEY_ARROW_UP,
        Key::Named(Named::ArrowDown) => GhosttyKey_GHOSTTY_KEY_ARROW_DOWN,
        Key::Named(Named::ArrowLeft) => GhosttyKey_GHOSTTY_KEY_ARROW_LEFT,
        Key::Named(Named::ArrowRight) => GhosttyKey_GHOSTTY_KEY_ARROW_RIGHT,
        Key::Named(Named::F1) => GhosttyKey_GHOSTTY_KEY_F1,
        Key::Named(Named::F2) => GhosttyKey_GHOSTTY_KEY_F2,
        Key::Named(Named::F3) => GhosttyKey_GHOSTTY_KEY_F3,
        Key::Named(Named::F4) => GhosttyKey_GHOSTTY_KEY_F4,
        Key::Named(Named::F5) => GhosttyKey_GHOSTTY_KEY_F5,
        Key::Named(Named::F6) => GhosttyKey_GHOSTTY_KEY_F6,
        Key::Named(Named::F7) => GhosttyKey_GHOSTTY_KEY_F7,
        Key::Named(Named::F8) => GhosttyKey_GHOSTTY_KEY_F8,
        Key::Named(Named::F9) => GhosttyKey_GHOSTTY_KEY_F9,
        Key::Named(Named::F10) => GhosttyKey_GHOSTTY_KEY_F10,
        Key::Named(Named::F11) => GhosttyKey_GHOSTTY_KEY_F11,
        Key::Named(Named::F12) => GhosttyKey_GHOSTTY_KEY_F12,
        Key::Character(value) => character_key(value)?,
        _ => return None,
    })
}

fn character_key(value: &str) -> Option<u32> {
    Some(character_key_char(value.chars().next()?))
}

fn character_key_char(value: char) -> u32 {
    use ghostty::*;
    match value.to_ascii_lowercase() {
        'a' => GhosttyKey_GHOSTTY_KEY_A,
        'b' => GhosttyKey_GHOSTTY_KEY_B,
        'c' => GhosttyKey_GHOSTTY_KEY_C,
        'd' => GhosttyKey_GHOSTTY_KEY_D,
        'e' => GhosttyKey_GHOSTTY_KEY_E,
        'f' => GhosttyKey_GHOSTTY_KEY_F,
        'g' => GhosttyKey_GHOSTTY_KEY_G,
        'h' => GhosttyKey_GHOSTTY_KEY_H,
        'i' => GhosttyKey_GHOSTTY_KEY_I,
        'j' => GhosttyKey_GHOSTTY_KEY_J,
        'k' => GhosttyKey_GHOSTTY_KEY_K,
        'l' => GhosttyKey_GHOSTTY_KEY_L,
        'm' => GhosttyKey_GHOSTTY_KEY_M,
        'n' => GhosttyKey_GHOSTTY_KEY_N,
        'o' => GhosttyKey_GHOSTTY_KEY_O,
        'p' => GhosttyKey_GHOSTTY_KEY_P,
        'q' => GhosttyKey_GHOSTTY_KEY_Q,
        'r' => GhosttyKey_GHOSTTY_KEY_R,
        's' => GhosttyKey_GHOSTTY_KEY_S,
        't' => GhosttyKey_GHOSTTY_KEY_T,
        'u' => GhosttyKey_GHOSTTY_KEY_U,
        'v' => GhosttyKey_GHOSTTY_KEY_V,
        'w' => GhosttyKey_GHOSTTY_KEY_W,
        'x' => GhosttyKey_GHOSTTY_KEY_X,
        'y' => GhosttyKey_GHOSTTY_KEY_Y,
        'z' => GhosttyKey_GHOSTTY_KEY_Z,
        '0' => GhosttyKey_GHOSTTY_KEY_DIGIT_0,
        '1' => GhosttyKey_GHOSTTY_KEY_DIGIT_1,
        '2' => GhosttyKey_GHOSTTY_KEY_DIGIT_2,
        '3' => GhosttyKey_GHOSTTY_KEY_DIGIT_3,
        '4' => GhosttyKey_GHOSTTY_KEY_DIGIT_4,
        '5' => GhosttyKey_GHOSTTY_KEY_DIGIT_5,
        '6' => GhosttyKey_GHOSTTY_KEY_DIGIT_6,
        '7' => GhosttyKey_GHOSTTY_KEY_DIGIT_7,
        '8' => GhosttyKey_GHOSTTY_KEY_DIGIT_8,
        '9' => GhosttyKey_GHOSTTY_KEY_DIGIT_9,
        '`' | '~' => GhosttyKey_GHOSTTY_KEY_BACKQUOTE,
        '\\' | '|' => GhosttyKey_GHOSTTY_KEY_BACKSLASH,
        '[' | '{' => GhosttyKey_GHOSTTY_KEY_BRACKET_LEFT,
        ']' | '}' => GhosttyKey_GHOSTTY_KEY_BRACKET_RIGHT,
        ',' | '<' => GhosttyKey_GHOSTTY_KEY_COMMA,
        '=' | '+' => GhosttyKey_GHOSTTY_KEY_EQUAL,
        '-' | '_' => GhosttyKey_GHOSTTY_KEY_MINUS,
        '.' | '>' => GhosttyKey_GHOSTTY_KEY_PERIOD,
        '\'' | '"' => GhosttyKey_GHOSTTY_KEY_QUOTE,
        ';' | ':' => GhosttyKey_GHOSTTY_KEY_SEMICOLON,
        '/' | '?' => GhosttyKey_GHOSTTY_KEY_SLASH,
        ' ' => GhosttyKey_GHOSTTY_KEY_SPACE,
        // libghostty uses key=UNIDENTIFIED + utf8 for printable codepoints
        // without a physical-key enum (CJK, emoji, composed input, etc.).
        // Dropping them here would make the terminal ASCII-only.
        _ => GhosttyKey_GHOSTTY_KEY_UNIDENTIFIED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::key::{Code, Physical};
    use iced::keyboard::Location;
    use roost_vt::TerminalOptions;

    fn encoder_pair() -> (KeyEncoder, Terminal) {
        let terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 0,
        })
        .expect("test terminal");
        (KeyEncoder::new().expect("test key encoder"), terminal)
    }

    fn key_press(
        key: Key,
        physical_key: Physical,
        modifiers: keyboard::Modifiers,
    ) -> keyboard::Event {
        press(key.clone(), key, physical_key, modifiers, None)
    }

    /// A press with every field the encoder reads set independently, so a
    /// platform's real event shape can be replayed on any host.
    fn press(
        key: Key,
        modified_key: Key,
        physical_key: Physical,
        modifiers: keyboard::Modifiers,
        text: Option<&str>,
    ) -> keyboard::Event {
        keyboard::Event::KeyPressed {
            key,
            modified_key,
            physical_key,
            location: Location::Standard,
            modifiers,
            text: text.map(Into::into),
            repeat: false,
        }
    }

    /// A character press that carries `text`, the field a platform hands the
    /// control transform back in.
    fn chord(
        key: &str,
        modified_key: &str,
        physical: Code,
        modifiers: keyboard::Modifiers,
        text: &str,
    ) -> keyboard::Event {
        press(
            character(key),
            character(modified_key),
            Physical::Code(physical),
            modifiers,
            Some(text),
        )
    }

    fn character(value: &str) -> Key {
        Key::Character(value.into())
    }

    /// The chord's `(text, key)` pair — the character libghostty is handed
    /// and the unshifted character that identifies the key.
    fn recovered(
        key: &str,
        modified_key: &str,
        modifiers: keyboard::Modifiers,
        text: &str,
    ) -> Option<(char, char)> {
        control_chord(
            &character(key).as_ref(),
            &character(modified_key).as_ref(),
            modifiers,
            Some(text),
        )
        .map(|chord| (chord.text, chord.key))
    }

    fn kitty_encoder_pair() -> (KeyEncoder, Terminal) {
        let (mut encoder, mut terminal) = encoder_pair();
        // CSI > 1 u — the flags a Kitty-protocol app pushes; production
        // re-syncs the encoder from the terminal on every keystroke.
        terminal.vt_write(b"\x1b[>1u");
        encoder.sync_from_terminal(&terminal);
        (encoder, terminal)
    }

    #[test]
    fn maps_toolkit_keys_without_gtk() {
        assert_eq!(
            ghostty_key(&Key::Named(Named::Enter)),
            Some(ghostty::GhosttyKey_GHOSTTY_KEY_ENTER)
        );
        assert_eq!(character_key("A"), Some(ghostty::GhosttyKey_GHOSTTY_KEY_A));
        assert_eq!(
            character_key("?"),
            Some(ghostty::GhosttyKey_GHOSTTY_KEY_SLASH)
        );
        assert_eq!(
            character_key("λ"),
            Some(ghostty::GhosttyKey_GHOSTTY_KEY_UNIDENTIFIED)
        );
    }

    #[test]
    fn maps_native_pointer_modifiers_to_ghostty_bits() {
        let all = keyboard::Modifiers::SHIFT
            | keyboard::Modifiers::CTRL
            | keyboard::Modifiers::ALT
            | keyboard::Modifiers::LOGO;
        assert_eq!(
            ghostty_modifiers(all),
            mods::SHIFT | mods::CTRL | mods::ALT | mods::SUPER
        );
    }

    #[test]
    fn only_non_modifier_key_presses_snap_terminal_scrollback() {
        let character = key_press(
            Key::Character("x".into()),
            Physical::Code(Code::KeyX),
            keyboard::Modifiers::empty(),
        );
        assert!(non_modifier_press(&character));

        let modifier = key_press(
            Key::Named(Named::Shift),
            Physical::Code(Code::ShiftLeft),
            keyboard::Modifiers::SHIFT,
        );
        assert!(!non_modifier_press(&modifier));
        assert!(!non_modifier_press(&keyboard::Event::ModifiersChanged(
            keyboard::Modifiers::SHIFT
        )));
        assert!(!non_modifier_press(&keyboard::Event::KeyReleased {
            key: Key::Character("x".into()),
            modified_key: Key::Character("x".into()),
            physical_key: Physical::Code(Code::KeyX),
            location: Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
        }));
    }

    #[test]
    fn only_bare_page_keys_claim_the_local_viewport() {
        let empty = keyboard::Modifiers::empty();
        let page_up = key_press(
            Key::Named(Named::PageUp),
            Physical::Code(Code::PageUp),
            empty,
        );
        let page_down = key_press(
            Key::Named(Named::PageDown),
            Physical::Code(Code::PageDown),
            empty,
        );
        assert_eq!(
            bare_page_direction(&page_up, empty),
            Some(PageDirection::Up)
        );
        assert_eq!(
            bare_page_direction(&page_down, empty),
            Some(PageDirection::Down)
        );
        assert_eq!(
            bare_page_direction(
                &key_press(
                    Key::Named(Named::ArrowUp),
                    Physical::Code(Code::ArrowUp),
                    empty
                ),
                empty
            ),
            None
        );

        for modifier in [
            keyboard::Modifiers::SHIFT,
            keyboard::Modifiers::CTRL,
            keyboard::Modifiers::ALT,
            keyboard::Modifiers::LOGO,
        ] {
            assert_eq!(
                bare_page_direction(&page_up, modifier),
                None,
                "tracked modifier {modifier:?} must not page locally"
            );
            assert_eq!(
                bare_page_direction(
                    &key_press(
                        Key::Named(Named::PageUp),
                        Physical::Code(Code::PageUp),
                        modifier
                    ),
                    empty
                ),
                None,
                "event modifier {modifier:?} must not page locally"
            );
        }

        assert_eq!(
            bare_page_direction(
                &keyboard::Event::KeyReleased {
                    key: Key::Named(Named::PageUp),
                    modified_key: Key::Named(Named::PageUp),
                    physical_key: Physical::Code(Code::PageUp),
                    location: Location::Standard,
                    modifiers: empty,
                },
                empty
            ),
            None
        );
    }

    #[test]
    fn maps_iced_press_to_shared_accelerator_with_exact_modifiers() {
        let event = key_press(
            Key::Character("C".into()),
            Physical::Code(Code::KeyC),
            keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
        );
        assert_eq!(
            accelerator(&event),
            Some(Accel {
                modifiers: AccelMods::CTRL | AccelMods::SHIFT,
                key: "c".into(),
            })
        );
        let with_alt = key_press(
            Key::Character("c".into()),
            Physical::Code(Code::KeyC),
            keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT | keyboard::Modifiers::ALT,
        );
        assert_ne!(accelerator(&event), accelerator(&with_alt));
    }

    #[test]
    fn accelerator_uses_physical_latin_and_named_key_vocabulary() {
        let non_latin = key_press(
            Key::Character("с".into()),
            Physical::Code(Code::KeyC),
            keyboard::Modifiers::LOGO,
        );
        assert_eq!(accelerator(&non_latin).unwrap().key, "c");

        let insert = key_press(
            Key::Named(Named::Insert),
            Physical::Code(Code::Insert),
            keyboard::Modifiers::CTRL,
        );
        assert_eq!(accelerator(&insert).unwrap().key, "insert");
    }

    #[test]
    fn accelerator_canonicalizes_logical_punctuation_before_physical_fallback() {
        let bracket = key_press(
            Key::Character("[".into()),
            Physical::Code(Code::BracketLeft),
            keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
        );
        assert_eq!(accelerator(&bracket).unwrap().key, "bracketleft");

        let plus = key_press(
            Key::Character("+".into()),
            Physical::Code(Code::Equal),
            keyboard::Modifiers::CTRL,
        );
        assert_eq!(accelerator(&plus).unwrap().key, "plus");

        let space = key_press(
            Key::Named(Named::Space),
            Physical::Code(Code::Space),
            keyboard::Modifiers::CTRL,
        );
        assert_eq!(accelerator(&space).unwrap().key, "space");
    }

    /// Plain typing must be byte-identical to the hardcoded
    /// `set_composing(false)` this parameter replaced. The vectors are the
    /// bytes that build produced.
    #[test]
    fn ordinary_presses_encode_the_same_bytes_when_not_composing() {
        let (mut encoder, terminal) = encoder_pair();
        let letter = keyboard::Event::KeyPressed {
            key: Key::Character("a".into()),
            modified_key: Key::Character("a".into()),
            physical_key: Physical::Code(Code::KeyA),
            location: Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: Some("a".into()),
            repeat: false,
        };
        assert_eq!(
            encode_press(&mut encoder, &terminal, letter, false),
            b"a".to_vec()
        );
        assert_eq!(
            encode_press(
                &mut encoder,
                &terminal,
                key_press(
                    Key::Named(Named::Enter),
                    Physical::Code(Code::Enter),
                    keyboard::Modifiers::empty(),
                ),
                false,
            ),
            b"\r".to_vec()
        );
        assert_eq!(
            encode_press(
                &mut encoder,
                &terminal,
                key_press(
                    Key::Character("c".into()),
                    Physical::Code(Code::KeyC),
                    keyboard::Modifiers::CTRL,
                ),
                false,
            ),
            b"\x03".to_vec()
        );
    }

    /// The keys an IME declines are forwarded to us as ordinary presses
    /// mid-composition (winit's `doCommandBySelector` path). Marking them
    /// composing is what keeps them out of the PTY.
    #[test]
    fn composing_presses_are_swallowed_by_the_encoder() {
        let (mut encoder, terminal) = encoder_pair();
        for key in [
            (Key::Named(Named::Enter), Physical::Code(Code::Enter)),
            (Key::Named(Named::Escape), Physical::Code(Code::Escape)),
            (
                Key::Named(Named::ArrowDown),
                Physical::Code(Code::ArrowDown),
            ),
            (Key::Character("n".into()), Physical::Code(Code::KeyN)),
        ] {
            let event = key_press(key.0.clone(), key.1, keyboard::Modifiers::empty());
            assert!(
                encode_press(&mut encoder, &terminal, event, true).is_empty(),
                "{:?} must not reach the PTY while composing",
                key.0
            );
        }
    }

    #[test]
    fn ime_commits_encode_their_exact_utf8() {
        let (mut encoder, terminal) = encoder_pair();
        for text in ["é", "e\u{0301}", "你好", "👍", "ｱ"] {
            assert_eq!(
                encode_ime_commit(&mut encoder, &terminal, text),
                text.as_bytes().to_vec(),
                "commit {text:?}"
            );
        }
        assert!(encode_ime_commit(&mut encoder, &terminal, "").is_empty());
    }

    /// The event shape macOS really delivers, captured from a live
    /// `roost-iced` (winit 0.30 / iced 0.14): `key` is winit's
    /// `key_without_modifiers` so it stays the layout's unshifted
    /// character, `modified_key` is the press with shift applied, and
    /// `text` (`NSEvent.characters`) carries the control transform. Passing
    /// that C0 through as utf8 is what made ctrl+a reach the PTY as
    /// `\x1b[1;5u`.
    #[test]
    fn mac_control_chords_encode_the_legacy_control_bytes() {
        let (mut encoder, terminal) = encoder_pair();
        let ctrl = keyboard::Modifiers::CTRL;
        let ctrl_shift = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
        for (key, modified, physical, modifiers, text, expected) in [
            ("a", "a", Code::KeyA, ctrl, "\u{1}", b"\x01".as_slice()),
            ("k", "k", Code::KeyK, ctrl, "\u{b}", b"\x0b"),
            ("c", "c", Code::KeyC, ctrl, "\u{3}", b"\x03"),
            ("\\", "\\", Code::Backslash, ctrl, "\u{1c}", b"\x1c"),
            ("]", "]", Code::BracketRight, ctrl, "\u{1d}", b"\x1d"),
            // macOS types US on both the bare and the shifted minus.
            ("-", "_", Code::Minus, ctrl_shift, "\u{1f}", b"\x1f"),
            ("-", "-", Code::Minus, ctrl, "\u{1f}", b"\x1f"),
        ] {
            let event = chord(key, modified, physical, modifiers, text);
            assert_eq!(
                encode_press(&mut encoder, &terminal, event, false),
                expected.to_vec(),
                "ctrl chord on {key:?}"
            );
        }
    }

    /// ctrl+[ is the one chord in D3's set libghostty deliberately refuses
    /// to fold into a C0: `[`, `i` and `m` are commented out of its
    /// `ctrlSeq` table per fixterms so applications can tell ctrl+[ from
    /// Escape, ctrl+i from Tab and ctrl+m from Enter. Recovery still fixes
    /// the byte we were sending — the CSI-u entry now reports the key that
    /// was actually pressed (91 = `[`) instead of the transform's codepoint
    /// (27 = ESC). The Swift app sends NOTHING here (verified live: it
    /// strips the C0 and libghostty has no sequence left to build), so this
    /// is a strict improvement over both prior behaviors.
    #[test]
    fn control_bracket_left_reports_the_bracket_not_the_escape_codepoint() {
        let (mut encoder, terminal) = encoder_pair();
        let ctrl = keyboard::Modifiers::CTRL;
        let event = chord("[", "[", Code::BracketLeft, ctrl, "\u{1b}");
        assert_eq!(
            encode_press(&mut encoder, &terminal, event, false),
            b"\x1b[91;5u".to_vec()
        );
    }

    /// The shape the plan assumed and a toolkit change could still produce:
    /// the C0 arrives as the logical key itself. It has no libghostty key
    /// enum (`character_key` answers UNIDENTIFIED) and no usable unshifted
    /// codepoint, so recovery has to supply both — and the bytes must match
    /// the captured macOS shape above exactly.
    #[test]
    fn control_transformed_logical_keys_encode_the_same_bytes() {
        let (mut encoder, terminal) = encoder_pair();
        let ctrl = keyboard::Modifiers::CTRL;
        let ctrl_shift = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
        for (c0, modified, physical, modifiers, expected) in [
            ("\u{1}", "a", Code::KeyA, ctrl, b"\x01".as_slice()),
            ("\u{b}", "k", Code::KeyK, ctrl, b"\x0b"),
            ("\u{1c}", "\\", Code::Backslash, ctrl, b"\x1c"),
            ("\u{1d}", "]", Code::BracketRight, ctrl, b"\x1d"),
            ("\u{1f}", "_", Code::Minus, ctrl_shift, b"\x1f"),
            ("\u{1b}", "[", Code::BracketLeft, ctrl, b"\x1b[91;5u"),
        ] {
            let event = chord(c0, modified, physical, modifiers, c0);
            assert_eq!(
                encode_press(&mut encoder, &terminal, event, false),
                expected.to_vec(),
                "control-transformed logical key {c0:?}"
            );
        }
    }

    /// A non-Latin layout types its own character while macOS still applies
    /// the Latin control transform; `modified_key` cannot confirm the C0, so
    /// the canonical inverse carries the chord.
    #[test]
    fn non_latin_layouts_recover_the_latin_control_chord() {
        let (mut encoder, terminal) = encoder_pair();
        let event = chord("ф", "ф", Code::KeyA, keyboard::Modifiers::CTRL, "\u{1}");
        assert_eq!(
            encode_press(&mut encoder, &terminal, event, false),
            b"\x01".to_vec()
        );
    }

    /// The shapes that must not move: a toolkit that reports the printable
    /// character as `text` (GTK's keyval path, and winit wherever the
    /// platform declines to control-transform), plain typing, option-
    /// transformed text, and the C0 forms outside the invertible set —
    /// NUL, RS, and the named keys that own their own encoding.
    #[test]
    fn presses_outside_the_recoverable_chords_encode_unchanged() {
        let (mut encoder, terminal) = encoder_pair();
        let ctrl = keyboard::Modifiers::CTRL;
        let ctrl_shift = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
        let empty = keyboard::Modifiers::empty();
        let alt = keyboard::Modifiers::ALT;
        for (key, modified, physical, modifiers, text, expected) in [
            // Printable text alongside ctrl — the byte is unchanged either
            // way, which is what makes the fix safe for the platforms that
            // never transform.
            ("a", "a", Code::KeyA, ctrl, "a", b"\x01".as_slice()),
            ("a", "a", Code::KeyA, empty, "a", b"a"),
            // Option-transformed text: no ctrl, so nothing is recovered.
            ("b", "∫", Code::KeyB, alt, "∫", "∫".as_bytes()),
            // NUL (ctrl+shift+2) and RS (ctrl+shift+6) stay where they were.
            ("2", "@", Code::Digit2, ctrl_shift, "\u{0}", b"\x1b[0;5u"),
            ("6", "^", Code::Digit6, ctrl_shift, "\u{1e}", b"\x1b[30;5u"),
        ] {
            let event = chord(key, modified, physical, modifiers, text);
            assert_eq!(
                encode_press(&mut encoder, &terminal, event, false),
                expected.to_vec(),
                "{key:?} + {modifiers:?}"
            );
        }

        // Named keys own their encoding, C0 text and all.
        for (named, physical, text, expected) in [
            (Named::Enter, Code::Enter, "\r", b"\x1b[27;5;13~".as_slice()),
            (Named::Space, Code::Space, "\u{0}", b"\x1b[0;5u"),
        ] {
            let key = Key::Named(named);
            let event = press(key.clone(), key, Physical::Code(physical), ctrl, Some(text));
            assert_eq!(
                encode_press(&mut encoder, &terminal, event, false),
                expected.to_vec(),
                "{named:?}"
            );
        }
    }

    /// Kitty-ON canonical shape, pinned: the CSI-u entry reports the
    /// letter's codepoint (97) because recovery feeds libghostty a real
    /// unshifted codepoint — the GTK shape, and what
    /// `roost-vt/tests/key_encoder_test.rs::ctrl_a_under_kitty_exact_bytes`
    /// pins from the other side. The Swift app zeroes the codepoint for a
    /// C0 press instead; plan 026 D3 accepts that divergence because the
    /// legacy bytes agree.
    #[test]
    fn mac_control_chords_under_kitty_report_the_letter_codepoint() {
        let (mut encoder, terminal) = kitty_encoder_pair();
        let event = chord("a", "a", Code::KeyA, keyboard::Modifiers::CTRL, "\u{1}");
        assert_eq!(
            encode_press(&mut encoder, &terminal, event, false),
            b"\x1b[97;5u".to_vec()
        );
    }

    /// The CSI-u entry names the key, so a shifted chord must report the
    /// unshifted codepoint (97 for `a`, 45 for the minus key) on BOTH event
    /// shapes — the identity cannot drift to the shifted character (65, 95)
    /// just because the platform handed us the C0 as the logical key.
    #[test]
    fn shifted_control_chords_report_one_identity_under_kitty() {
        let (mut encoder, terminal) = kitty_encoder_pair();
        let ctrl_shift = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
        for (mac, transformed, modified, physical, expected) in [
            ("a", "\u{1}", "A", Code::KeyA, b"\x1b[97;6u".as_slice()),
            ("-", "\u{1f}", "_", Code::Minus, b"\x1b[45;6u"),
        ] {
            let from_mac = encode_press(
                &mut encoder,
                &terminal,
                chord(mac, modified, physical, ctrl_shift, transformed),
                false,
            );
            let from_transformed = encode_press(
                &mut encoder,
                &terminal,
                chord(transformed, modified, physical, ctrl_shift, transformed),
                false,
            );
            assert_eq!(from_mac, expected.to_vec(), "ctrl+shift chord on {mac:?}");
            assert_eq!(
                from_transformed, from_mac,
                "both event shapes must encode {mac:?} identically"
            );
        }
    }

    #[test]
    fn control_chord_recovery_is_limited_to_invertible_chords() {
        let ctrl = keyboard::Modifiers::CTRL;
        let ctrl_shift = ctrl | keyboard::Modifiers::SHIFT;
        assert_eq!(recovered("a", "a", ctrl, "\u{1}"), Some(('a', 'a')));
        assert_eq!(
            recovered("a", "a", keyboard::Modifiers::empty(), "\u{1}"),
            None,
            "a C0 without ctrl is not ours to invert"
        );
        for text in ["\u{0}", "\u{1e}", "\u{7f}", "a"] {
            assert_eq!(
                recovered("a", "a", ctrl, text),
                None,
                "{text:?} must keep today's encoding"
            );
        }
        // A shifted chord types the shifted character but still identifies
        // the unshifted key.
        assert_eq!(recovered("-", "_", ctrl_shift, "\u{1f}"), Some(('_', '-')));
        assert_eq!(recovered("a", "A", ctrl_shift, "\u{1}"), Some(('A', 'a')));
        assert_eq!(recovered("[", "{", ctrl_shift, "\u{1b}"), Some(('{', '[')));
        // The C0-as-logical shape resolves to the same pair.
        assert_eq!(
            recovered("\u{1f}", "_", ctrl_shift, "\u{1f}"),
            Some(('_', '-'))
        );
        let named = Key::Named(Named::Enter);
        assert_eq!(
            control_chord(&named.as_ref(), &named.as_ref(), ctrl, Some("\r"))
                .map(|chord| chord.key),
            None,
            "named keys own their encoding"
        );
    }

    /// Configured ctrl+letter and ctrl+punctuation keybinds must resolve on
    /// both event shapes — the captured macOS one (where the logical key is
    /// already the letter) and the control-transformed logical key — and to
    /// the SAME name, which is why the accelerator takes the chord's
    /// unshifted key rather than the shifted character it typed.
    #[test]
    fn accelerators_resolve_control_transformed_presses() {
        let ctrl = keyboard::Modifiers::CTRL;
        let ctrl_shift = ctrl | keyboard::Modifiers::SHIFT;
        for (key, modified, physical, modifiers, text, expected) in [
            ("a", "a", Code::KeyA, ctrl, "\u{1}", "a"),
            ("[", "[", Code::BracketLeft, ctrl, "\u{1b}", "bracketleft"),
            ("\u{1}", "a", Code::KeyA, ctrl, "\u{1}", "a"),
            (
                "\u{1b}",
                "[",
                Code::BracketLeft,
                ctrl,
                "\u{1b}",
                "bracketleft",
            ),
            ("\u{1c}", "\\", Code::Backslash, ctrl, "\u{1c}", "\\"),
            // Shifted chords, both shapes: the shifted character the layout
            // typed must never rename the binding.
            (
                "[",
                "{",
                Code::BracketLeft,
                ctrl_shift,
                "\u{1b}",
                "bracketleft",
            ),
            (
                "\u{1b}",
                "{",
                Code::BracketLeft,
                ctrl_shift,
                "\u{1b}",
                "bracketleft",
            ),
            ("-", "_", Code::Minus, ctrl_shift, "\u{1f}", "minus"),
            ("\u{1f}", "_", Code::Minus, ctrl_shift, "\u{1f}", "minus"),
            ("a", "A", Code::KeyA, ctrl_shift, "\u{1}", "a"),
            ("\u{1}", "A", Code::KeyA, ctrl_shift, "\u{1}", "a"),
        ] {
            let event = chord(key, modified, physical, modifiers, text);
            assert_eq!(
                accelerator(&event),
                Some(Accel {
                    modifiers: accelerator_modifiers(modifiers),
                    key: expected.into(),
                }),
                "accelerator for {key:?} + {modifiers:?}"
            );
        }
    }
}
