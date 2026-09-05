//! Key definitions and chord parsing, shared by both engines.
//!
//! CDP and BiDi name keys in different namespaces: CDP wants
//! `KeyboardEvent.key` (`"ArrowDown"`) plus a Windows virtual key code, while
//! BiDi wants the WebDriver normalised key values (`\u{E015}` for ArrowDown).
//! Neither is derivable from the other, so a [`KeyDef`] carries both rather
//! than converting at the call site.
//!
//! Scope is the US layout: named keys, the modifiers, and printable ASCII.
//! Anything beyond that belongs in `type`, which goes through
//! `Input.insertText` and is layout-independent.

use anyhow::{bail, Result};

/// Everything both engines need to press one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyDef {
    /// `KeyboardEvent.key`.
    pub key: &'static str,
    /// `KeyboardEvent.code` (physical key).
    pub code: &'static str,
    /// Windows virtual key code, for CDP.
    pub vk: u32,
    /// Text this key inserts, or `None` when it inserts nothing.
    ///
    /// Load-bearing: Chromium synthesises `keypress` and inserts characters
    /// **from this field**, so giving `ArrowDown` a `text` types a glyph
    /// instead of moving the caret. Conversely `Enter` needs `"\r"` here or
    /// forms do not submit.
    pub text: Option<&'static str>,
    /// WebDriver normalised key value, for BiDi.
    pub bidi: &'static str,
}

/// CDP modifier bitmask values. The mask goes on **every** event in a chord,
/// including the target key's — leaving it off there is the usual reason
/// `Control+A` selects nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Alt = 1,
    Control = 2,
    Meta = 4,
    Shift = 8,
}

impl Modifier {
    pub fn def(self) -> KeyDef {
        match self {
            Modifier::Alt => KeyDef {
                key: "Alt",
                code: "AltLeft",
                vk: 0x12,
                text: None,
                bidi: "\u{E00A}",
            },
            Modifier::Control => KeyDef {
                key: "Control",
                code: "ControlLeft",
                vk: 0x11,
                text: None,
                bidi: "\u{E009}",
            },
            Modifier::Meta => KeyDef {
                key: "Meta",
                code: "MetaLeft",
                vk: 0x5B,
                text: None,
                bidi: "\u{E03D}",
            },
            Modifier::Shift => KeyDef {
                key: "Shift",
                code: "ShiftLeft",
                vk: 0x10,
                text: None,
                bidi: "\u{E008}",
            },
        }
    }

    fn parse(name: &str) -> Option<Modifier> {
        match name.to_ascii_lowercase().as_str() {
            "alt" | "option" => Some(Modifier::Alt),
            "control" | "ctrl" => Some(Modifier::Control),
            "meta" | "cmd" | "command" | "super" => Some(Modifier::Meta),
            "shift" => Some(Modifier::Shift),
            _ => None,
        }
    }
}

/// A key plus the modifiers held while it is pressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    pub modifiers: Vec<Modifier>,
    pub key: KeyDef,
}

impl Chord {
    pub fn plain(key: KeyDef) -> Chord {
        Chord {
            modifiers: Vec::new(),
            key,
        }
    }

    /// Combined CDP modifier bitmask.
    pub fn mask(&self) -> u32 {
        self.modifiers.iter().fold(0, |m, k| m | (*k as u32))
    }
}

const NAMED: &[KeyDef] = &[
    KeyDef {
        key: "Enter",
        code: "Enter",
        vk: 0x0D,
        text: Some("\r"),
        bidi: "\u{E007}",
    },
    KeyDef {
        key: "Tab",
        code: "Tab",
        vk: 0x09,
        text: Some("\t"),
        bidi: "\u{E004}",
    },
    KeyDef {
        key: "Escape",
        code: "Escape",
        vk: 0x1B,
        text: None,
        bidi: "\u{E00C}",
    },
    KeyDef {
        key: "Backspace",
        code: "Backspace",
        vk: 0x08,
        text: None,
        bidi: "\u{E003}",
    },
    KeyDef {
        key: "Delete",
        code: "Delete",
        vk: 0x2E,
        text: None,
        bidi: "\u{E017}",
    },
    KeyDef {
        key: "Insert",
        code: "Insert",
        vk: 0x2D,
        text: None,
        bidi: "\u{E016}",
    },
    KeyDef {
        key: " ",
        code: "Space",
        vk: 0x20,
        text: Some(" "),
        bidi: "\u{E00D}",
    },
    KeyDef {
        key: "Home",
        code: "Home",
        vk: 0x24,
        text: None,
        bidi: "\u{E011}",
    },
    KeyDef {
        key: "End",
        code: "End",
        vk: 0x23,
        text: None,
        bidi: "\u{E010}",
    },
    KeyDef {
        key: "PageUp",
        code: "PageUp",
        vk: 0x21,
        text: None,
        bidi: "\u{E00E}",
    },
    KeyDef {
        key: "PageDown",
        code: "PageDown",
        vk: 0x22,
        text: None,
        bidi: "\u{E00F}",
    },
    KeyDef {
        key: "ArrowUp",
        code: "ArrowUp",
        vk: 0x26,
        text: None,
        bidi: "\u{E013}",
    },
    KeyDef {
        key: "ArrowDown",
        code: "ArrowDown",
        vk: 0x28,
        text: None,
        bidi: "\u{E015}",
    },
    KeyDef {
        key: "ArrowLeft",
        code: "ArrowLeft",
        vk: 0x25,
        text: None,
        bidi: "\u{E012}",
    },
    KeyDef {
        key: "ArrowRight",
        code: "ArrowRight",
        vk: 0x27,
        text: None,
        bidi: "\u{E014}",
    },
    KeyDef {
        key: "F1",
        code: "F1",
        vk: 0x70,
        text: None,
        bidi: "\u{E031}",
    },
    KeyDef {
        key: "F2",
        code: "F2",
        vk: 0x71,
        text: None,
        bidi: "\u{E032}",
    },
    KeyDef {
        key: "F3",
        code: "F3",
        vk: 0x72,
        text: None,
        bidi: "\u{E033}",
    },
    KeyDef {
        key: "F4",
        code: "F4",
        vk: 0x73,
        text: None,
        bidi: "\u{E034}",
    },
    KeyDef {
        key: "F5",
        code: "F5",
        vk: 0x74,
        text: None,
        bidi: "\u{E035}",
    },
    KeyDef {
        key: "F6",
        code: "F6",
        vk: 0x75,
        text: None,
        bidi: "\u{E036}",
    },
    KeyDef {
        key: "F7",
        code: "F7",
        vk: 0x76,
        text: None,
        bidi: "\u{E037}",
    },
    KeyDef {
        key: "F8",
        code: "F8",
        vk: 0x77,
        text: None,
        bidi: "\u{E038}",
    },
    KeyDef {
        key: "F9",
        code: "F9",
        vk: 0x78,
        text: None,
        bidi: "\u{E039}",
    },
    KeyDef {
        key: "F10",
        code: "F10",
        vk: 0x79,
        text: None,
        bidi: "\u{E03A}",
    },
    KeyDef {
        key: "F11",
        code: "F11",
        vk: 0x7A,
        text: None,
        bidi: "\u{E03B}",
    },
    KeyDef {
        key: "F12",
        code: "F12",
        vk: 0x7B,
        text: None,
        bidi: "\u{E03C}",
    },
];

/// The `Enter` definition, so `press_enter` keeps its exact wire payload.
pub const ENTER: KeyDef = NAMED[0];

/// Aliases callers reasonably expect. `Space` is spelled `" "` in
/// `KeyboardEvent.key`, so it cannot be found by name without this.
fn alias(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "space" | "spacebar" => Some(" "),
        "esc" => Some("Escape"),
        "return" => Some("Enter"),
        "del" => Some("Delete"),
        "up" => Some("ArrowUp"),
        "down" => Some("ArrowDown"),
        "left" => Some("ArrowLeft"),
        "right" => Some("ArrowRight"),
        "pgup" => Some("PageUp"),
        "pgdn" | "pgdown" => Some("PageDown"),
        _ => None,
    }
}

/// Look up one key by name. Named keys are case-insensitive; a single
/// character is taken literally, so `"a"` and `"A"` differ.
pub fn lookup(name: &str) -> Option<KeyDef> {
    let wanted = alias(name).unwrap_or(name);
    if let Some(def) = NAMED
        .iter()
        .find(|d| d.key.eq_ignore_ascii_case(wanted) && !wanted.is_empty())
    {
        return Some(*def);
    }
    let mut chars = wanted.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_graphic() => Some(printable(c)),
        _ => None,
    }
}

/// Build a definition for a printable ASCII character.
///
/// `text` is leaked to `'static` because [`KeyDef`] is `Copy` and shared with
/// the const table; one small allocation per distinct character pressed, which
/// is bounded by the 95 printable ASCII characters.
fn printable(c: char) -> KeyDef {
    let upper = c.to_ascii_uppercase();
    let code: &'static str = match upper {
        'A'..='Z' => Box::leak(format!("Key{upper}").into_boxed_str()),
        '0'..='9' => Box::leak(format!("Digit{upper}").into_boxed_str()),
        _ => "",
    };
    KeyDef {
        key: Box::leak(c.to_string().into_boxed_str()),
        code,
        // Virtual key codes are for the *unshifted* physical key.
        vk: if upper.is_ascii_alphanumeric() {
            upper as u32
        } else {
            0
        },
        text: Some(Box::leak(c.to_string().into_boxed_str())),
        bidi: Box::leak(c.to_string().into_boxed_str()),
    }
}

/// Parse `"Control+Shift+K"`. The last segment is the key; the rest are
/// modifiers. A trailing `+` means the `+` key, as in `"Control++"`.
pub fn parse_chord(spec: &str) -> Result<Chord> {
    if spec.is_empty() {
        bail!("empty key");
    }
    // `+` is both the separator and a pressable key, so a trailing `+` is
    // peeled off before splitting rather than handled afterwards — splitting
    // first leaves empty segments that look like unnamed modifiers.
    let (mod_spec, key_name) = if spec.ends_with('+') {
        (spec[..spec.len() - 1].trim_end_matches('+'), "+")
    } else {
        match spec.rsplit_once('+') {
            Some((head, key)) => (head, key),
            None => ("", spec),
        }
    };

    let mut modifiers = Vec::new();
    for m in mod_spec.split('+').filter(|s| !s.is_empty()) {
        match Modifier::parse(m) {
            Some(modifier) => modifiers.push(modifier),
            None => bail!(
                "unknown modifier `{m}` in `{spec}`. Known: Control/Ctrl, Shift, Alt/Option, Meta/Cmd"
            ),
        }
    }

    let key = lookup(key_name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown key `{key_name}` in `{spec}`.{}",
            suggestion(key_name)
        )
    })?;
    Ok(Chord { modifiers, key })
}

/// Name the closest known key, because a silently-wrong keystroke is worse
/// than an error.
fn suggestion(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let near: Vec<&str> = NAMED
        .iter()
        .map(|d| d.key)
        .filter(|k| {
            let k = k.to_ascii_lowercase();
            k.starts_with(&lower) || lower.starts_with(&k)
        })
        .take(3)
        .collect();
    if near.is_empty() {
        " Named keys are e.g. Enter, Tab, Escape, ArrowDown, F5; \
         a single character presses that character."
            .to_string()
    } else {
        format!(" Did you mean {}?", near.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_named_key() {
        let c = parse_chord("Enter").unwrap();
        assert!(c.modifiers.is_empty());
        assert_eq!(c.key.key, "Enter");
        assert_eq!(c.mask(), 0);
    }

    #[test]
    fn named_keys_are_case_insensitive() {
        assert_eq!(parse_chord("escape").unwrap().key.key, "Escape");
        assert_eq!(parse_chord("ARROWDOWN").unwrap().key.key, "ArrowDown");
    }

    #[test]
    fn single_characters_are_case_sensitive() {
        assert_eq!(parse_chord("a").unwrap().key.key, "a");
        assert_eq!(parse_chord("A").unwrap().key.key, "A");
    }

    #[test]
    fn modifiers_combine_into_the_cdp_mask() {
        let c = parse_chord("Control+A").unwrap();
        assert_eq!(c.modifiers, vec![Modifier::Control]);
        assert_eq!(c.mask(), 2);
        assert_eq!(parse_chord("Control+Shift+K").unwrap().mask(), 2 | 8);
        assert_eq!(parse_chord("Alt+Meta+x").unwrap().mask(), 1 | 4);
    }

    #[test]
    fn modifier_aliases() {
        assert_eq!(parse_chord("Ctrl+a").unwrap().mask(), 2);
        assert_eq!(parse_chord("Cmd+a").unwrap().mask(), 4);
        assert_eq!(parse_chord("Option+a").unwrap().mask(), 1);
    }

    #[test]
    fn trailing_plus_is_the_plus_key() {
        // `+` is both the separator and a pressable key.
        let c = parse_chord("Control++").unwrap();
        assert_eq!(c.modifiers, vec![Modifier::Control]);
        assert_eq!(c.key.key, "+");

        let bare = parse_chord("+").unwrap();
        assert!(bare.modifiers.is_empty());
        assert_eq!(bare.key.key, "+");

        let two = parse_chord("Control+Shift++").unwrap();
        assert_eq!(two.modifiers, vec![Modifier::Control, Modifier::Shift]);
        assert_eq!(two.key.key, "+");
    }

    #[test]
    fn space_is_reachable_by_name() {
        // `KeyboardEvent.key` for space is " ", which no one types as a chord.
        assert_eq!(parse_chord("Space").unwrap().key.code, "Space");
    }

    #[test]
    fn named_keys_that_insert_nothing_carry_no_text() {
        // Chromium inserts from `text`; giving ArrowDown one types a glyph.
        for name in ["ArrowDown", "Escape", "F5", "Home", "Backspace"] {
            assert_eq!(lookup(name).unwrap().text, None, "{name} must not insert");
        }
    }

    #[test]
    fn enter_still_carries_the_carriage_return() {
        // This is what makes forms submit; press_enter depends on it.
        assert_eq!(ENTER.text, Some("\r"));
        assert_eq!(ENTER.vk, 13);
    }

    #[test]
    fn printable_keys_insert_themselves() {
        let a = lookup("a").unwrap();
        assert_eq!(a.text, Some("a"));
        assert_eq!(a.code, "KeyA");
        assert_eq!(a.vk, 'A' as u32);
    }

    #[test]
    fn unknown_key_names_the_closest_match() {
        let err = parse_chord("Ente").unwrap_err().to_string();
        assert!(err.contains("Enter"), "{err}");
    }

    #[test]
    fn unknown_modifier_is_rejected_rather_than_ignored() {
        let err = parse_chord("Hyper+a").unwrap_err().to_string();
        assert!(err.contains("Hyper") && err.contains("Control"), "{err}");
    }

    #[test]
    fn empty_input_is_an_error() {
        assert!(parse_chord("").is_err());
    }

    #[test]
    fn bidi_values_are_the_webdriver_namespace_not_the_key_name() {
        assert_eq!(lookup("ArrowDown").unwrap().bidi, "\u{E015}");
        assert_eq!(ENTER.bidi, "\u{E007}");
    }
}
