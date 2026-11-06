//! Musical typing, as in GarageBand (and guitar's piano mode): the home row plays white keys
//! and the row above black ones, semitones up from the note on A.
//!
//! gpui names a key by the ASCII letter it types, or by where it sits on a US keyboard when
//! it types none, so on the Norwegian layout Ø, Æ and ¨ arrive as ";", "'" and "]". The key
//! after Æ types "'" itself, so `typed_offset` tells it from Æ by the typed character.

/// (gpui's key name, label on a US keyboard, label on a Nordic one, semitones above A's note).
pub const TYPING_KEYS: [(&str, &str, &str, i32); 19] = [
    ("a", "A", "A", 0), ("w", "W", "W", 1), ("s", "S", "S", 2), ("e", "E", "E", 3), ("d", "D", "D", 4),
    ("f", "F", "F", 5), ("t", "T", "T", 6), ("g", "G", "G", 7), ("y", "Y", "Y", 8), ("h", "H", "H", 9),
    ("u", "U", "U", 10), ("j", "J", "J", 11), ("k", "K", "K", 12), ("o", "O", "O", 13), ("l", "L", "L", 14),
    ("p", "P", "P", 15), (";", ";", "Ø", 16), ("'", "'", "Æ", 17), ("]", "]", "¨", 18),
];
/// The G above, on the key right of ' (US) or Æ (Nordic).
pub const TYPING_G: i32 = 19;
/// The highest offset a key plays.
pub const TYPING_TOP: i32 = TYPING_G;

/// The note on A: C3 to start (MIDI 48); Z and X move it an octave, C0 to C7.
pub const BASE: i32 = 48;
pub const BASES: std::ops::RangeInclusive<i32> = 12..=96;

/// Whether the keyboard layout puts letters (Ø Æ, Ö Ä) right of L and types "'" on the key
/// after them, as the Scandinavian ones do.
pub fn is_nordic(layout: &str) -> bool {
    ["Norwegian", "Danish", "Swedish", "Finnish"].iter().any(|n| layout.contains(n))
}

/// The semitones above A's note that a key plays, from gpui's name for the key and the
/// character it types.
pub fn typed_offset(key: &str, typed: Option<&str>, nordic: bool) -> Option<i32> {
    match key {
        // Nordic: Æ arrives as "'" too (where it sits on a US keyboard), but types æ; the key
        // after it types "'" itself.
        "'" if nordic && typed == Some("'") => Some(TYPING_G),
        "\\" if !nordic => Some(TYPING_G),
        _ => TYPING_KEYS.iter().find(|(k, ..)| *k == key).map(|&(.., offset)| offset),
    }
}

/// The key that plays `offset` semitones above A's note, as printed on the keyboard.
pub fn typing_label(offset: i32, nordic: bool) -> Option<&'static str> {
    match offset {
        TYPING_G => Some(if nordic { "'" } else { "\\" }),
        _ => TYPING_KEYS.iter().find(|k| k.3 == offset).map(|&(_, us, nordic_label, _)| if nordic { nordic_label } else { us }),
    }
}

/// Whether a MIDI note is a black key.
pub fn is_black(midi: i32) -> bool {
    matches!(midi.rem_euclid(12), 1 | 3 | 6 | 8 | 10)
}

pub fn octave(midi: i32) -> i32 {
    midi.div_euclid(12) - 1
}

const NAMES: [&str; 12] = ["C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B"];

/// "C4", "F♯2".
pub fn note_name(midi: i32) -> String {
    format!("{}{}", NAMES[midi.rem_euclid(12) as usize], octave(midi))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_play_like_garageband_on_both_layouts() {
        // The home row from A's note, black keys on the row above; Ø and Æ come in as ";" and "'".
        let us = |k: &str| typed_offset(k, None, false);
        let notes: Vec<Option<i32>> = ["a", "w", "s", "k", ";", "'", "]", "\\", "q", "z"].iter().map(|k| us(k)).collect();
        assert_eq!(notes, [Some(0), Some(1), Some(2), Some(12), Some(16), Some(17), Some(18), Some(19), None, None]);
        // Norwegian: Ø Æ ¨ come in by their US places; the key after Æ types "'" and is G.
        let no = |k: &str, typed: &str| typed_offset(k, Some(typed), true);
        assert_eq!((no(";", "ø"), no("'", "æ"), no("]", "¨"), no("'", "'")), (Some(16), Some(17), Some(18), Some(19)));
        assert_eq!((typing_label(17, true), typing_label(18, true), typing_label(19, true)), (Some("Æ"), Some("¨"), Some("'")));
        assert_eq!((typing_label(17, false), typing_label(19, false)), (Some("'"), Some("\\")));
        assert_eq!(typing_label(13, true), Some("O"), "C♯ above");
        assert_eq!(typing_label(-1, true), None);
        assert!(is_nordic("Norwegian (no dead keys)") && !is_nordic("English (US)"));
        assert_eq!((note_name(60), note_name(21), note_name(66)), ("C4".into(), "A0".into(), "F♯4".into()));
    }
}
