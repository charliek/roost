//! Flutter key presses, committed text and pastes, to the bytes a tab's
//! program reads — through the same libghostty `KeyEncoder` the desktop
//! uses, synced from the terminal's modes (DECCKM, Kitty flags, …).

use roost_ui_model::bracketed_paste;
use roost_vt::{ffi, key_action, mods, Key, KeyEncoder, KeyEvent, OptionAsAlt, Terminal};

/// One key press as Flutter reports it. `key` is
/// `LogicalKeyboardKey.keyLabel`-style: `"Enter"`, `"Escape"`,
/// `"Arrow Up"` (or `"ArrowUp"`), `"Page Up"`, `"F1"`, or one character
/// (`"a"`, `"A"`, `"1"`, `" "`). A letter's case does not matter — `shift`
/// does. `text` is the character the press typed, when the platform
/// reports one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyInput {
    pub key: String,
    pub text: Option<String>,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    pub repeat: bool,
}

enum Resolved {
    Named(Key),
    Char(char),
}

fn resolve(name: &str) -> Option<Resolved> {
    let mut chars = name.chars();
    if let (Some(only), None) = (chars.next(), chars.next()) {
        return Some(Resolved::Char(only));
    }
    let squashed: String = name
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '_')
        .collect::<String>()
        .to_ascii_lowercase();
    use ffi::*;
    let key = match squashed.as_str() {
        "enter" | "return" | "numpadenter" => GhosttyKey_GHOSTTY_KEY_ENTER,
        "escape" | "esc" => GhosttyKey_GHOSTTY_KEY_ESCAPE,
        "backspace" => GhosttyKey_GHOSTTY_KEY_BACKSPACE,
        "tab" => GhosttyKey_GHOSTTY_KEY_TAB,
        "space" | "spacebar" => return Some(Resolved::Char(' ')),
        "delete" | "del" => GhosttyKey_GHOSTTY_KEY_DELETE,
        "insert" => GhosttyKey_GHOSTTY_KEY_INSERT,
        "home" => GhosttyKey_GHOSTTY_KEY_HOME,
        "end" => GhosttyKey_GHOSTTY_KEY_END,
        "pageup" => GhosttyKey_GHOSTTY_KEY_PAGE_UP,
        "pagedown" => GhosttyKey_GHOSTTY_KEY_PAGE_DOWN,
        "arrowup" | "up" => GhosttyKey_GHOSTTY_KEY_ARROW_UP,
        "arrowdown" | "down" => GhosttyKey_GHOSTTY_KEY_ARROW_DOWN,
        "arrowleft" | "left" => GhosttyKey_GHOSTTY_KEY_ARROW_LEFT,
        "arrowright" | "right" => GhosttyKey_GHOSTTY_KEY_ARROW_RIGHT,
        "f1" => GhosttyKey_GHOSTTY_KEY_F1,
        "f2" => GhosttyKey_GHOSTTY_KEY_F2,
        "f3" => GhosttyKey_GHOSTTY_KEY_F3,
        "f4" => GhosttyKey_GHOSTTY_KEY_F4,
        "f5" => GhosttyKey_GHOSTTY_KEY_F5,
        "f6" => GhosttyKey_GHOSTTY_KEY_F6,
        "f7" => GhosttyKey_GHOSTTY_KEY_F7,
        "f8" => GhosttyKey_GHOSTTY_KEY_F8,
        "f9" => GhosttyKey_GHOSTTY_KEY_F9,
        "f10" => GhosttyKey_GHOSTTY_KEY_F10,
        "f11" => GhosttyKey_GHOSTTY_KEY_F11,
        "f12" => GhosttyKey_GHOSTTY_KEY_F12,
        _ => return None,
    };
    Some(Resolved::Named(key))
}

/// The physical-key enum for a character, US layout; anything off the
/// table is `UNIDENTIFIED`, which libghostty encodes from the UTF-8.
/// The same table as `roost-iced`'s `input::character_key_char`.
fn character_key(value: char) -> Key {
    use ffi::*;
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
        _ => GhosttyKey_GHOSTTY_KEY_UNIDENTIFIED,
    }
}

/// Text libghostty may take as a key's UTF-8: no control characters,
/// and none of the private-use codepoints platforms report for function
/// keys.
fn printable(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| !c.is_control() && !('\u{E000}'..='\u{F8FF}').contains(&c))
}

/// What one key event carries into libghostty, before encoding.
struct Press {
    key: Key,
    mods: u16,
    consumed: u16,
    unshifted: u32,
    utf8: String,
}

fn press(input: &KeyInput) -> Option<Press> {
    let mut mod_bits = 0;
    for (on, bit) in [
        (input.shift, mods::SHIFT),
        (input.ctrl, mods::CTRL),
        (input.alt, mods::ALT),
        (input.meta, mods::SUPER),
    ] {
        if on {
            mod_bits |= bit;
        }
    }
    match resolve(&input.key)? {
        Resolved::Named(key) => Some(Press {
            key,
            mods: mod_bits,
            consumed: 0,
            unshifted: 0,
            utf8: String::new(),
        }),
        Resolved::Char(ch) => {
            let unshifted = ch.to_ascii_lowercase();
            let shifted = if input.shift {
                ch.to_ascii_uppercase()
            } else {
                unshifted
            };
            // Under ctrl the platform's text is the C0 byte (or nothing):
            // libghostty wants the character the chord is keyed on and
            // derives the control code itself.
            let utf8 = match input.text.as_deref() {
                Some(text) if !input.ctrl && printable(text) => text.to_string(),
                _ => shifted.to_string(),
            };
            // A printable that differs from the unshifted key means shift
            // went into it (`A`, `!`), so the encoder must not report it
            // again. Ctrl never translates.
            let consumed = if !input.ctrl && printable(&utf8) && utf8 != unshifted.to_string() {
                mod_bits & mods::SHIFT
            } else {
                0
            };
            Some(Press {
                key: character_key(ch),
                mods: mod_bits,
                consumed,
                unshifted: u32::from(unshifted),
                utf8,
            })
        }
    }
}

fn encode_press(
    encoder: &mut KeyEncoder,
    terminal: Option<&Terminal>,
    press: &Press,
    repeat: bool,
) -> roost_vt::Result<Vec<u8>> {
    let mut event = KeyEvent::new()?;
    event
        .set_action(if repeat {
            key_action::REPEAT
        } else {
            key_action::PRESS
        })
        .set_key(press.key)
        .set_mods(press.mods)
        .set_consumed_mods(press.consumed)
        .set_composing(false)
        .set_unshifted_codepoint(press.unshifted)
        .set_utf8(press.utf8.as_bytes());
    if let Some(terminal) = terminal {
        sync_encoder(encoder, terminal);
    }
    encoder.encode(&event)
}

/// Encode one key press. A key the table does not know (a bare
/// modifier, Caps Lock) encodes to nothing. `terminal` is the one the
/// tab draws, whose modes pick the encoding; `None` encodes as a
/// terminal at its defaults would.
/// The encoder every mobile terminal uses. Alt is always Alt: a phone or a
/// tablet keyboard has no macOS text-modifier Option, and libghostty's macOS
/// build (the desktop dev target) would otherwise drop the ESC prefix.
pub fn new_encoder() -> roost_vt::Result<KeyEncoder> {
    let mut encoder = KeyEncoder::new()?;
    encoder.set_macos_option_as_alt(OptionAsAlt::True);
    Ok(encoder)
}

/// Sync `encoder` from `terminal`'s modes. The sync resets Option-as-Alt,
/// so [`new_encoder`]'s setting is put back after it.
pub fn sync_encoder(encoder: &mut KeyEncoder, terminal: &Terminal) {
    encoder.sync_from_terminal(terminal);
    encoder.set_macos_option_as_alt(OptionAsAlt::True);
}

pub fn encode_key(
    encoder: &mut KeyEncoder,
    terminal: Option<&Terminal>,
    input: &KeyInput,
) -> roost_vt::Result<Vec<u8>> {
    match press(input) {
        Some(press) => encode_press(encoder, terminal, &press, input.repeat),
        None => Ok(Vec::new()),
    }
}

fn named(key: Key) -> Press {
    Press {
        key,
        mods: 0,
        consumed: 0,
        unshifted: 0,
        utf8: String::new(),
    }
}

/// Encode text a soft keyboard committed. Printable text takes
/// libghostty's no-physical-key path (`UNIDENTIFIED` plus the UTF-8), as
/// the desktop's IME commit does. Some IMEs commit Enter, Tab or
/// Backspace as characters; those become the key presses they stand
/// for, and any other control character is dropped.
pub fn encode_text(
    encoder: &mut KeyEncoder,
    terminal: Option<&Terminal>,
    text: &str,
) -> roost_vt::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut printable_from = 0;
    for (at, ch) in text.char_indices() {
        if !ch.is_control() {
            continue;
        }
        out.extend(encode_commit(encoder, terminal, &text[printable_from..at])?);
        printable_from = at + ch.len_utf8();
        let key = match ch {
            '\r' | '\n' => ffi::GhosttyKey_GHOSTTY_KEY_ENTER,
            '\t' => ffi::GhosttyKey_GHOSTTY_KEY_TAB,
            '\u{8}' | '\u{7f}' => ffi::GhosttyKey_GHOSTTY_KEY_BACKSPACE,
            _ => continue,
        };
        out.extend(encode_press(encoder, terminal, &named(key), false)?);
    }
    out.extend(encode_commit(encoder, terminal, &text[printable_from..])?);
    Ok(out)
}

fn encode_commit(
    encoder: &mut KeyEncoder,
    terminal: Option<&Terminal>,
    chunk: &str,
) -> roost_vt::Result<Vec<u8>> {
    if chunk.is_empty() {
        return Ok(Vec::new());
    }
    let press = Press {
        key: ffi::GhosttyKey_GHOSTTY_KEY_UNIDENTIFIED,
        mods: 0,
        consumed: 0,
        unshifted: 0,
        utf8: chunk.to_string(),
    };
    encode_press(encoder, terminal, &press, false)
}

/// Frame pasted text for the PTY. Bracketed (DECSET 2004) it is wrapped
/// by the desktop's own framing, markers inside it neutralized.
/// Unbracketed, every line break becomes a carriage return — the key a
/// raw-mode program reads as Enter — as libghostty's own paste encoder
/// does (`paste.h`).
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        return bracketed_paste::wrap(text, true);
    }
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    bracketed_paste::wrap(&text, false)
}

#[cfg(test)]
mod tests {
    use roost_vt::TerminalOptions;

    use super::*;

    fn terminal() -> Terminal {
        Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            ..Default::default()
        })
        .expect("terminal")
    }

    fn key(name: &str) -> KeyInput {
        KeyInput {
            key: name.to_string(),
            ..Default::default()
        }
    }

    fn encode(terminal: &Terminal, input: &KeyInput) -> Vec<u8> {
        let mut encoder = new_encoder().expect("encoder");
        encode_key(&mut encoder, Some(terminal), input).expect("encode")
    }

    #[test]
    fn enter_is_carriage_return() {
        assert_eq!(encode(&terminal(), &key("Enter")), b"\r");
    }

    #[test]
    fn ctrl_c_is_etx() {
        let t = terminal();
        let mut input = key("c");
        input.ctrl = true;
        assert_eq!(encode(&t, &input), b"\x03");
        // Flutter's label is the uppercase letter, and the platform's
        // text under ctrl is the C0 byte itself.
        input.key = "C".into();
        input.text = Some("\u{3}".into());
        assert_eq!(encode(&t, &input), b"\x03");
    }

    #[test]
    fn arrow_up_follows_cursor_key_mode() {
        let mut t = terminal();
        assert_eq!(encode(&t, &key("Arrow Up")), b"\x1b[A");
        assert_eq!(encode(&t, &key("ArrowUp")), b"\x1b[A");
        t.vt_write(b"\x1b[?1h");
        assert_eq!(encode(&t, &key("Arrow Up")), b"\x1bOA");
    }

    #[test]
    fn alt_x_is_escape_prefixed() {
        let mut input = key("x");
        input.alt = true;
        assert_eq!(encode(&terminal(), &input), b"\x1bx");
    }

    #[test]
    fn backspace_is_del() {
        assert_eq!(encode(&terminal(), &key("Backspace")), b"\x7f");
    }

    #[test]
    fn tab_and_shift_tab() {
        let t = terminal();
        assert_eq!(encode(&t, &key("Tab")), b"\t");
        let mut back = key("Tab");
        back.shift = true;
        assert_eq!(encode(&t, &back), b"\x1b[Z");
    }

    #[test]
    fn f1_and_escape_and_page_up() {
        let t = terminal();
        assert_eq!(encode(&t, &key("F1")), b"\x1bOP");
        assert_eq!(encode(&t, &key("Escape")), b"\x1b");
        assert_eq!(encode(&t, &key("Page Up")), b"\x1b[5~");
    }

    #[test]
    fn letters_take_their_case_from_shift_or_text() {
        let t = terminal();
        assert_eq!(encode(&t, &key("A")), b"a");
        let mut upper = key("a");
        upper.shift = true;
        assert_eq!(encode(&t, &upper), b"A");
        let mut bang = key("1");
        bang.shift = true;
        bang.text = Some("!".into());
        assert_eq!(encode(&t, &bang), b"!");
        assert_eq!(encode(&t, &key(" ")), b" ");
    }

    /// The names the Flutter UI sends (`mobile/lib/input/key_mapping.dart`,
    /// whose test checks it sends exactly this list).
    #[test]
    fn every_named_key_the_app_sends_encodes() {
        let names: Vec<String> =
            serde_json::from_str(include_str!("../../../mobile/test/fixtures/key_names.json"))
                .expect("key_names.json");
        assert!(names.len() > 20, "{names:?}");
        let t = terminal();
        for name in names {
            assert!(
                !encode(&t, &key(&name)).is_empty(),
                "{name} encodes to nothing"
            );
        }
    }

    #[test]
    fn an_unknown_key_encodes_to_nothing() {
        assert!(encode(&terminal(), &key("Shift Left")).is_empty());
        assert!(encode(&terminal(), &key("")).is_empty());
    }

    #[test]
    fn committed_text_is_its_utf8_and_line_breaks_are_enter() {
        let t = terminal();
        let mut encoder = new_encoder().expect("encoder");
        assert_eq!(
            encode_text(&mut encoder, Some(&t), "héllo 中").expect("encode"),
            "héllo 中".as_bytes()
        );
        assert_eq!(
            encode_text(&mut encoder, Some(&t), "ls\n").expect("encode"),
            b"ls\r"
        );
        assert_eq!(
            encode_text(&mut encoder, Some(&t), "a\u{7}b\tc").expect("encode"),
            b"ab\tc"
        );
    }

    #[test]
    fn paste_is_bracketed_only_under_2004_and_newlines_normalize_outside_it() {
        assert_eq!(paste_bytes("a\nb", true), b"\x1b[200~a\nb\x1b[201~");
        assert_eq!(paste_bytes("a\r\nb\nc", false), b"a\rb\rc");
        assert!(paste_bytes("", true).is_empty());
    }
}
