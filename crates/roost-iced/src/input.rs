use iced::keyboard::{self, key::Named, Key};
use roost_vt::ffi as ghostty;
use roost_vt::{key_action, mods, KeyEncoder, KeyEvent, Terminal};

pub fn encode_press(
    encoder: &mut KeyEncoder,
    terminal: &Terminal,
    event: keyboard::Event,
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
    let mut event = match KeyEvent::new() {
        Ok(event) => event,
        Err(error) => {
            tracing::warn!(?error, "failed to allocate libghostty key event");
            return Vec::new();
        }
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
        .set_composing(false)
        .set_unshifted_codepoint(unshifted)
        .set_utf8(utf8);
    encoder.sync_from_terminal(terminal);
    encoder.encode(&event).unwrap_or_else(|error| {
        tracing::warn!(?error, "libghostty key encoding failed");
        Vec::new()
    })
}

fn ghostty_modifiers(value: keyboard::Modifiers) -> u16 {
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
}
