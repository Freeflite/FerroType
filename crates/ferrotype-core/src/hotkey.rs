//! Start/stop hotkeys: reading what the user typed or pressed, and refusing combinations that
//! can't work or would get in the way.
//!
//! FerroType registers hotkeys with Windows (`RegisterHotKey`), which hands the combination to
//! FerroType *instead of* the program that has focus. So everyday shortcuts are refused: taking
//! over Ctrl+S would break Save everywhere.

use std::fmt;

/// A key that can end a hotkey, with its Windows virtual-key code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub name: &'static str,
    pub vk: u16,
}

const LETTERS: [&str; 26] = [
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S", "T", "U", "V", "W",
    "X", "Y", "Z",
];
const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
const NUMPAD: [&str; 10] = ["Num 0", "Num 1", "Num 2", "Num 3", "Num 4", "Num 5", "Num 6", "Num 7", "Num 8", "Num 9"];
const FUNCTION: [&str; 24] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15", "F16", "F17",
    "F18", "F19", "F20", "F21", "F22", "F23", "F24",
];
/// Named keys other than letters, digits, the number pad and F-keys.
const NAMED: [(&str, u16); 16] = [
    ("Backspace", 0x08),
    ("Tab", 0x09),
    ("Enter", 0x0D),
    ("Pause", 0x13),
    ("Escape", 0x1B),
    ("Space", 0x20),
    ("Page Up", 0x21),
    ("Page Down", 0x22),
    ("End", 0x23),
    ("Home", 0x24),
    ("Left", 0x25),
    ("Up", 0x26),
    ("Right", 0x27),
    ("Down", 0x28),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
];
const SCROLL_LOCK: Key = Key { name: "Scroll Lock", vk: 0x91 };
/// Other spellings people use, already squashed (see [`squash`]).
const ALIASES: [(&str, &str); 14] = [
    ("pgup", "Page Up"),
    ("pgdn", "Page Down"),
    ("pagedn", "Page Down"),
    ("del", "Delete"),
    ("ins", "Insert"),
    ("esc", "Escape"),
    ("return", "Enter"),
    ("break", "Pause"),
    ("uparrow", "Up"),
    ("downarrow", "Down"),
    ("leftarrow", "Left"),
    ("rightarrow", "Right"),
    ("scrlk", "Scroll Lock"),
    ("bksp", "Backspace"),
];

/// Lower case with spaces, underscores and hyphens removed: "Page_Up" -> "pageup".
fn squash(name: &str) -> String {
    name.chars().filter(|c| !matches!(c, ' ' | '_' | '-')).flat_map(char::to_lowercase).collect()
}

impl Key {
    pub fn from_name(name: &str) -> Option<Key> {
        let wanted = squash(name);
        let wanted = ALIASES.iter().find(|(alias, _)| *alias == wanted).map_or(wanted, |(_, real)| squash(real));
        Key::all().find(|key| squash(key.name) == wanted)
    }

    pub fn from_vk(vk: u16) -> Option<Key> {
        Key::all().find(|key| key.vk == vk)
    }

    fn all() -> impl Iterator<Item = Key> {
        let letters = (0x41..).zip(LETTERS).map(|(vk, name)| Key { name, vk });
        let digits = (0x30..).zip(DIGITS).map(|(vk, name)| Key { name, vk });
        let numpad = (0x60..).zip(NUMPAD).map(|(vk, name)| Key { name, vk });
        let function = (0x70..).zip(FUNCTION).map(|(vk, name)| Key { name, vk });
        let named = NAMED.into_iter().map(|(name, vk)| Key { name, vk });
        function.chain(letters).chain(digits).chain(numpad).chain(named).chain([SCROLL_LOCK])
    }

    /// F-keys, Pause and Scroll Lock: keys nothing types and few programs use, so they make
    /// hotkeys on their own.
    pub fn stands_alone(self) -> bool {
        (0x70..=0x87).contains(&self.vk) || self.vk == 0x13 || self.vk == SCROLL_LOCK.vk
    }
}

/// A key plus modifiers, like Ctrl+Alt+F9.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub key: Key,
}

/// Remote Desktop keeps these for itself (Alt+Page Up is its Alt+Tab), so they never reach a
/// program running on the computer you're connected from.
const REMOTE_DESKTOP: [(bool, bool, &str); 8] = [
    // (ctrl, alt, key)
    (false, true, "Page Up"),
    (false, true, "Page Down"),
    (false, true, "Home"),
    (false, true, "Insert"),
    (false, true, "Delete"),
    (true, true, "End"),
    (true, true, "Home"),
    (true, true, "Pause"),
];
/// Windows' own shortcuts that a hotkey must never take over.
const WINDOWS: [(bool, bool, &str); 6] = [
    (false, true, "F4"),
    (false, true, "Tab"),
    (false, true, "Escape"),
    (false, true, "Space"),
    (true, false, "Escape"),
    (true, true, "Delete"),
];

impl Hotkey {
    pub const fn new(ctrl: bool, alt: bool, shift: bool, win: bool, key: Key) -> Hotkey {
        Hotkey { ctrl, alt, shift, win, key }
    }

    /// Reads "Ctrl+Alt+F9", "alt + ctrl + f9", "Win+Shift+Page Down"... An empty string means no
    /// hotkey. Errors say what's wrong in a sentence fit for the Settings page.
    pub fn parse(text: &str) -> Result<Option<Hotkey>, String> {
        let parts: Vec<&str> = text.split('+').map(str::trim).filter(|part| !part.is_empty()).collect();
        if parts.is_empty() {
            return Ok(None);
        }
        let (mut ctrl, mut alt, mut shift, mut win) = (false, false, false, false);
        let mut keys = Vec::new();
        for part in parts {
            match squash(part).as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" => alt = true,
                "shift" => shift = true,
                "win" | "windows" | "super" | "meta" => win = true,
                _ => keys.push(part),
            }
        }
        let [key] = keys[..] else {
            return Err("Use one key plus any of Ctrl, Alt, Shift and Win, like Ctrl+Alt+F9.".to_owned());
        };
        let Some(key) = Key::from_name(key) else {
            return Err(format!("FerroType doesn't know a key called \"{key}\". Try an F-key or a letter."));
        };
        Hotkey::new(ctrl, alt, shift, win, key).check().map(Some)
    }

    /// Refuses combinations that can't work or would get in the way, saying why.
    pub fn check(self) -> Result<Hotkey, String> {
        let shortcut = |(ctrl, alt, name): &(bool, bool, &str)| {
            *ctrl == self.ctrl && *alt == self.alt && !self.shift && !self.win && *name == self.key.name
        };
        if REMOTE_DESKTOP.iter().any(shortcut) {
            return Err(format!("Remote Desktop keeps {self} for itself, so it would never reach FerroType."));
        }
        if WINDOWS.iter().any(shortcut) {
            return Err(format!("{self} belongs to Windows."));
        }
        if self.key.stands_alone() {
            return Ok(self);
        }
        if !(self.ctrl || self.alt || self.win) {
            return Err(format!("Add Ctrl, Alt or Win to {self}: plain and Shift keys are what FerroType types."));
        }
        let modifiers = [self.ctrl, self.alt, self.shift, self.win].into_iter().filter(|held| *held).count();
        if modifiers < 2 {
            return Err(format!(
                "{self} is a shortcut programs use, and a hotkey takes it over everywhere. Add another \
                 modifier (like Ctrl+Alt+{}) or use an F-key.",
                self.key.name
            ));
        }
        Ok(self)
    }

    /// The same combination with Shift held too. FerroType registers both, because while it's
    /// typing a capital letter its own Shift is down, and Windows would otherwise not recognise
    /// the stop hotkey pressed at that moment.
    pub fn with_shift(self) -> Hotkey {
        Hotkey { shift: true, ..self }
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (held, name) in [(self.ctrl, "Ctrl+"), (self.alt, "Alt+"), (self.shift, "Shift+"), (self.win, "Win+")] {
            if held {
                f.write_str(name)?;
            }
        }
        f.write_str(self.key.name)
    }
}

/// Shows an optional hotkey as text; `None` is an empty string.
pub fn label(hotkey: Option<Hotkey>) -> String {
    hotkey.map(|h| h.to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<String, String> {
        Hotkey::parse(text).map(label)
    }

    #[test]
    fn reads_and_normalises() {
        assert_eq!(parse("Ctrl+Alt+F9"), Ok("Ctrl+Alt+F9".into()));
        assert_eq!(parse("  alt + ctrl + f9 "), Ok("Ctrl+Alt+F9".into()));
        assert_eq!(parse(""), Ok(String::new()));
        assert_eq!(parse("F9"), Ok("F9".into()));
        assert_eq!(parse("shift+f9"), Ok("Shift+F9".into()));
        assert_eq!(parse("ctrl+shift+pgdn"), Ok("Ctrl+Shift+Page Down".into()));
        assert_eq!(parse("Control+Win+F2"), Ok("Ctrl+Win+F2".into()));
        assert_eq!(parse("ctrl+alt+s"), Ok("Ctrl+Alt+S".into()));
        assert_eq!(parse("ctrl+alt+num 5"), Ok("Ctrl+Alt+Num 5".into()));
        assert_eq!(parse("Pause"), Ok("Pause".into()));
        assert_eq!(parse("scroll_lock"), Ok("Scroll Lock".into()));
        assert_eq!(parse("ctrl+alt+7"), Ok("Ctrl+Alt+7".into()));
    }

    #[test]
    fn refuses_what_would_not_work() {
        let error = |text: &str| parse(text).unwrap_err();
        assert!(error("alt+page up").contains("Remote Desktop"));
        assert!(error("Alt+PgDn").contains("Remote Desktop"));
        assert!(error("ctrl+alt+end").contains("Remote Desktop"));
        assert!(error("alt+f4").contains("Windows"));
        assert!(error("ctrl+alt+delete").contains("Windows"));
        assert!(error("a").contains("Add Ctrl, Alt or Win"));
        assert!(error("shift+a").contains("Add Ctrl, Alt or Win"));
        assert!(error("ctrl+s").contains("takes it over everywhere"));
        assert!(error("ctrl+a+b").contains("one key"));
        assert!(error("ctrl+alt").contains("one key"));
        assert!(error("ctrl+alt+zzz").contains("zzz"));
    }

    #[test]
    fn keys_round_trip() {
        for vk in 0..=0xFF {
            if let Some(key) = Key::from_vk(vk) {
                assert_eq!(Key::from_name(key.name), Some(key), "{}", key.name);
            }
        }
        assert_eq!(Key::from_name("f24").map(|k| k.vk), Some(0x87));
        assert_eq!(Key::from_name("Z").map(|k| k.vk), Some(0x5A));
    }

    #[test]
    fn shift_variant() {
        let hotkey = Hotkey::parse("ctrl+alt+f9").unwrap().unwrap();
        assert_eq!(hotkey.with_shift().to_string(), "Ctrl+Alt+Shift+F9");
    }
}
