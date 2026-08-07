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
        physical_key,
        modifiers,
        ..
    } = event
    else {
        return None;
    };
    let logical = key.as_ref();
    let key = match logical {
        // Prefer the logical value for ASCII so shifted punctuation keeps its
        // shared GTK-style name (`+` -> `plus`, `{` -> `braceleft`). Physical
        // Latin is only a layout fallback; taking it first collapses `+` to
        // `equal` and makes configured punctuation aliases unreachable.
        Key::Character(value) if value.is_ascii() => canonical_character(value),
        _ => key
            .to_latin(*physical_key)
            .map(|value| canonical_character(&value.to_string()))
            .or_else(|| accelerator_key(&logical))?,
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
        physical_key,
        modifiers,
        text,
        repeat,
        ..
    } = event
    else {
        return Vec::new();
    };

    let latin = key.to_latin(physical_key);
    let key = key.as_ref();
    let Some(keycode) = ghostty_key(&key) else {
        return Vec::new();
    };
    let Some(mut event) = new_key_event() else {
        return Vec::new();
    };
    let utf8 = text.as_deref().unwrap_or("").as_bytes();
    let unshifted = latin.map_or_else(
        || match key {
            Key::Character(value) => value.chars().next().map_or(0, u32::from),
            Key::Named(Named::Space) => u32::from(' '),
            _ => 0,
        },
        u32::from,
    );
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
    use ghostty::*;
    Some(match value.chars().next()?.to_ascii_lowercase() {
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
    })
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
        keyboard::Event::KeyPressed {
            modified_key: key.clone(),
            key,
            physical_key,
            location: Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        }
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
}
