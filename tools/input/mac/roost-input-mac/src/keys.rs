//! Modifier keys as the HID system reports them: a real side keycode, the
//! generic `CGEventFlags` bit, and the device-dependent side bit
//! (`NX_DEVICE*KEYMASK` in IOKit's `IOLLEvent.h`). An app that tells left
//! Option from right Option reads the side bit, so a posted chord without it is
//! not the chord a keyboard sends.

pub const FLAG_SHIFT: u64 = 0x0002_0000;
pub const FLAG_CONTROL: u64 = 0x0004_0000;
pub const FLAG_ALTERNATE: u64 = 0x0008_0000;
pub const FLAG_COMMAND: u64 = 0x0010_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modifier {
    pub name: &'static str,
    pub keycode: u16,
    pub generic: u64,
    pub device: u64,
}

pub const MODIFIERS: [Modifier; 8] = [
    Modifier {
        name: "shift-left",
        keycode: 56,
        generic: FLAG_SHIFT,
        device: 0x0002,
    },
    Modifier {
        name: "shift-right",
        keycode: 60,
        generic: FLAG_SHIFT,
        device: 0x0004,
    },
    Modifier {
        name: "ctrl-left",
        keycode: 59,
        generic: FLAG_CONTROL,
        device: 0x0001,
    },
    Modifier {
        name: "ctrl-right",
        keycode: 62,
        generic: FLAG_CONTROL,
        device: 0x2000,
    },
    Modifier {
        name: "alt-left",
        keycode: 58,
        generic: FLAG_ALTERNATE,
        device: 0x0020,
    },
    Modifier {
        name: "alt-right",
        keycode: 61,
        generic: FLAG_ALTERNATE,
        device: 0x0040,
    },
    Modifier {
        name: "cmd-left",
        keycode: 55,
        generic: FLAG_COMMAND,
        device: 0x0008,
    },
    Modifier {
        name: "cmd-right",
        keycode: 54,
        generic: FLAG_COMMAND,
        device: 0x0010,
    },
];

/// A modifier by name. `shift`, `ctrl` and `cmd` mean the left key; Option has
/// no default side, because which side was pressed is the thing its tests pin.
pub fn modifier(name: &str) -> Result<Modifier, String> {
    let canonical = match name {
        "shift" => "shift-left",
        "ctrl" => "ctrl-left",
        "cmd" => "cmd-left",
        "alt" | "option" => return Err(format!("`{name}` needs a side: alt-left or alt-right")),
        other => other,
    };
    MODIFIERS
        .iter()
        .find(|modifier| modifier.name == canonical)
        .copied()
        .ok_or_else(|| {
            let known: Vec<&str> = MODIFIERS.iter().map(|modifier| modifier.name).collect();
            format!(
                "unknown modifier `{name}` (want shift, ctrl, cmd or one of {})",
                known.join(", ")
            )
        })
}

/// A comma-separated `--flags` value, in press order, without repeats.
pub fn parse_modifiers(list: &str) -> Result<Vec<Modifier>, String> {
    let mut out: Vec<Modifier> = Vec::new();
    for name in list
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let modifier = modifier(name)?;
        if out.contains(&modifier) {
            return Err(format!("modifier `{}` given twice", modifier.name));
        }
        out.push(modifier);
    }
    Ok(out)
}

/// The flags every event carries while `held` are down.
pub fn flags_for(held: &[Modifier]) -> u64 {
    held.iter().fold(0, |flags, modifier| {
        flags | modifier.generic | modifier.device
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_sides_carry_the_device_bit_the_probe_used() {
        let left = modifier("alt-left").unwrap();
        let right = modifier("alt-right").unwrap();
        assert_eq!((left.keycode, flags_for(&[left])), (58, 0x0008_0020));
        assert_eq!((right.keycode, flags_for(&[right])), (61, 0x0008_0040));
    }

    #[test]
    fn unsided_names_mean_the_left_key_except_option() {
        assert_eq!(modifier("shift").unwrap().keycode, 56);
        assert_eq!(modifier("ctrl").unwrap().keycode, 59);
        assert_eq!(modifier("cmd").unwrap().keycode, 55);
        assert!(modifier("alt").unwrap_err().contains("alt-left"));
        assert!(modifier("option").is_err());
        assert!(modifier("hyper").unwrap_err().contains("unknown modifier"));
    }

    #[test]
    fn both_shifts_keep_the_generic_bit_and_both_side_bits() {
        let held = parse_modifiers("shift-left,shift-right").unwrap();
        assert_eq!(flags_for(&held), FLAG_SHIFT | 0x0002 | 0x0004);
        assert_eq!(flags_for(&held[1..]), FLAG_SHIFT | 0x0004);
    }

    #[test]
    fn a_modifier_list_keeps_press_order_and_rejects_repeats() {
        let held = parse_modifiers("cmd, alt-right").unwrap();
        assert_eq!(
            held.iter().map(|m| m.name).collect::<Vec<_>>(),
            ["cmd-left", "alt-right"]
        );
        assert!(parse_modifiers("shift,shift-left").is_err());
        assert!(parse_modifiers("").unwrap().is_empty());
    }

    #[test]
    fn every_keycode_is_distinct() {
        let mut codes: Vec<u16> = MODIFIERS.iter().map(|m| m.keycode).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), MODIFIERS.len());
    }
}
