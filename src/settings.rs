//! Settings kept between runs, in `$XDG_STATE_HOME/piano/settings.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// How hard keys are struck, 1–127 (a laptop keyboard can't tell).
    pub velocity: u8,
    /// A little random variation on each strike, ± this much velocity.
    pub humanize: u8,
    /// The note on A (MIDI), moved an octave at a time with Z and X.
    pub base: i32,
    pub reverb: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { velocity: 80, humanize: 4, base: crate::keys::BASE, reverb: true }
    }
}

fn path() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("piano/settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        let mut s: Settings = path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        s.velocity = s.velocity.clamp(1, 127);
        s.base = s.base.clamp(*crate::keys::BASES.start(), *crate::keys::BASES.end());
        s
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(p, text);
        }
    }
}
