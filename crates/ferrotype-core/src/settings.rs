//! Preferences, saved to `%APPDATA%\FerroType\settings.ini` as `key=value` lines.
//!
//! Loading never fails: unknown keys, bad values and a missing file all fall back to defaults,
//! and numbers are clamped to sensible ranges.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crate::history::{Limits, Mode};
use crate::hotkey::{self, Hotkey, Key};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThemeMode {
    #[default]
    Light,
    Dark,
    /// Follow Windows' app light/dark setting.
    System,
}

impl ThemeMode {
    pub const ALL: [ThemeMode; 3] = [ThemeMode::Light, ThemeMode::Dark, ThemeMode::System];

    pub fn id(self) -> &'static str {
        match self {
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
            ThemeMode::System => "system",
        }
    }
}

/// Key delays are kept to 0-10 s, the countdown to 0-60 s.
pub const MAX_DELAY: f64 = 10.0;
pub const MAX_COUNTDOWN: u32 = 60;
/// History limits allowed in Settings.
pub const ENTRIES_RANGE: (usize, usize) = (1, 1000);
pub const BYTES_RANGE: (usize, usize) = (16 * 1024, 100 * 1024 * 1024);

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub theme: ThemeMode,
    /// Pause between keystrokes, picked at random between these (seconds).
    pub min_delay: f64,
    pub max_delay: f64,
    /// Seconds to click into the target window before typing starts.
    pub countdown: u32,
    pub on_top: bool,
    pub start_hotkey: Option<Hotkey>,
    pub stop_hotkey: Option<Hotkey>,
    /// Stop typing when another window gets the focus. Off by default: over Remote Desktop the
    /// window Windows reports often isn't the one really getting the keys.
    pub stop_on_focus_change: bool,
    pub history_mode: Mode,
    pub history_limits: Limits,
    /// Record text copied in other programs.
    pub watch_clipboard: bool,
    /// Don't record copies marked private (passwords from password managers). Text pulled in
    /// with From clipboard is recorded either way: the user asked for it.
    pub skip_private: bool,
    /// Record text FerroType types.
    pub remember_typed: bool,
}

pub const DEFAULT_HOTKEY: Hotkey = Hotkey::new(true, true, false, false, Key { name: "F9", vk: 0x78 });

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            theme: ThemeMode::Light,
            min_delay: 0.05,
            max_delay: 0.12,
            countdown: 3,
            on_top: false,
            // The same combination for both: one key starts and stops.
            start_hotkey: Some(DEFAULT_HOTKEY),
            stop_hotkey: Some(DEFAULT_HOTKEY),
            stop_on_focus_change: false,
            history_mode: Mode::Session,
            history_limits: Limits::default(),
            watch_clipboard: true,
            skip_private: true,
            remember_typed: true,
        }
    }
}

impl Settings {
    pub fn parse(text: &str) -> Settings {
        let mut settings = Settings::default();
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            settings.set(key.trim(), value.trim());
        }
        settings
    }

    fn set(&mut self, key: &str, value: &str) {
        let flag = |value: &str| match value {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        };
        let seconds =
            |value: &str| value.parse::<f64>().ok().filter(|v| v.is_finite()).map(|v| v.clamp(0.0, MAX_DELAY));
        match key {
            "theme" => {
                if let Some(theme) = ThemeMode::ALL.into_iter().find(|t| t.id() == value) {
                    self.theme = theme;
                }
            }
            "min_delay" => self.min_delay = seconds(value).unwrap_or(self.min_delay),
            "max_delay" => self.max_delay = seconds(value).unwrap_or(self.max_delay),
            "countdown" => self.countdown = value.parse().map_or(self.countdown, |v: u32| v.min(MAX_COUNTDOWN)),
            "on_top" => self.on_top = flag(value).unwrap_or(self.on_top),
            // A hotkey that no longer passes the checks (or never did) keeps the default.
            "start_hotkey" => self.start_hotkey = Hotkey::parse(value).unwrap_or(self.start_hotkey),
            "stop_hotkey" => self.stop_hotkey = Hotkey::parse(value).unwrap_or(self.stop_hotkey),
            "stop_on_focus_change" => self.stop_on_focus_change = flag(value).unwrap_or(self.stop_on_focus_change),
            "history" => self.history_mode = Mode::from_id(value).unwrap_or(self.history_mode),
            "history_max_entries" => {
                if let Ok(count) = value.parse::<usize>() {
                    self.history_limits.max_entries = count.clamp(ENTRIES_RANGE.0, ENTRIES_RANGE.1);
                }
            }
            "history_max_bytes" => {
                if let Ok(bytes) = value.parse::<usize>() {
                    self.history_limits.max_bytes = bytes.clamp(BYTES_RANGE.0, BYTES_RANGE.1);
                }
            }
            "watch_clipboard" => self.watch_clipboard = flag(value).unwrap_or(self.watch_clipboard),
            "skip_private" => self.skip_private = flag(value).unwrap_or(self.skip_private),
            "remember_typed" => self.remember_typed = flag(value).unwrap_or(self.remember_typed),
            _ => {}
        }
    }

    pub fn to_text(&self) -> String {
        let flag = |value: bool| if value { "true" } else { "false" };
        let lines = [
            "# FerroType settings. Edit them in FerroType (Settings).".to_owned(),
            format!("theme={}", self.theme.id()),
            format!("min_delay={}", self.min_delay),
            format!("max_delay={}", self.max_delay),
            format!("countdown={}", self.countdown),
            format!("on_top={}", flag(self.on_top)),
            format!("start_hotkey={}", hotkey::label(self.start_hotkey)),
            format!("stop_hotkey={}", hotkey::label(self.stop_hotkey)),
            format!("stop_on_focus_change={}", flag(self.stop_on_focus_change)),
            format!("history={}", self.history_mode.id()),
            format!("history_max_entries={}", self.history_limits.max_entries),
            format!("history_max_bytes={}", self.history_limits.max_bytes),
            format!("watch_clipboard={}", flag(self.watch_clipboard)),
            format!("skip_private={}", flag(self.skip_private)),
            format!("remember_typed={}", flag(self.remember_typed)),
        ];
        lines.join("\n") + "\n"
    }

    /// The key delay range, shortest first.
    pub fn delays(&self) -> (Duration, Duration) {
        let (a, b) = (Duration::from_secs_f64(self.min_delay), Duration::from_secs_f64(self.max_delay));
        if a <= b { (a, b) } else { (b, a) }
    }

    /// `%APPDATA%\FerroType`, where settings and history live.
    pub fn folder() -> Option<PathBuf> {
        std::env::var_os("APPDATA").map(|appdata| PathBuf::from(appdata).join("FerroType"))
    }

    pub fn load() -> Settings {
        Settings::folder()
            .and_then(|folder| std::fs::read_to_string(folder.join("settings.ini")).ok())
            .map_or_else(Settings::default, |text| Settings::parse(&text))
    }

    pub fn save(&self) -> io::Result<()> {
        let folder = Settings::folder().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no %APPDATA%"))?;
        write_atomically(&folder.join("settings.ini"), self.to_text().as_bytes())
    }
}

/// Writes `path` by writing a temporary file next to it and renaming it over the old one, so a
/// crash halfway never leaves a half-written file. Creates the folder if needed.
pub fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder)?;
    }
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let settings = Settings {
            theme: ThemeMode::Dark,
            min_delay: 0.01,
            max_delay: 0.25,
            countdown: 5,
            on_top: true,
            start_hotkey: Hotkey::parse("ctrl+shift+f8").unwrap(),
            stop_hotkey: None,
            stop_on_focus_change: true,
            history_mode: Mode::Saved,
            history_limits: Limits { max_entries: 100, max_bytes: 5 * 1024 * 1024 },
            watch_clipboard: false,
            skip_private: false,
            remember_typed: false,
        };
        assert_eq!(Settings::parse(&settings.to_text()), settings);
        assert_eq!(Settings::parse(&Settings::default().to_text()), Settings::default());
    }

    #[test]
    fn bad_values_fall_back() {
        let text = "theme=purple\nmin_delay=abc\nmax_delay=99\ncountdown=-3\non_top=maybe\nstart_hotkey=alt+page up\n\
                    history=forever\nhistory_max_entries=0\nhistory_max_bytes=99999999999\nnonsense\n=x\nunknown=1\n";
        let settings = Settings::parse(text);
        let default = Settings::default();
        assert_eq!(settings.theme, default.theme);
        assert_eq!(settings.min_delay, default.min_delay);
        assert_eq!(settings.max_delay, MAX_DELAY);
        assert_eq!(settings.countdown, default.countdown);
        assert_eq!(settings.on_top, default.on_top);
        assert_eq!(settings.start_hotkey, default.start_hotkey, "an RDP-reserved hotkey isn't loaded");
        assert_eq!(settings.history_mode, Mode::Session);
        assert_eq!(settings.history_limits.max_entries, 1);
        assert_eq!(settings.history_limits.max_bytes, BYTES_RANGE.1);
        assert_eq!(Settings::parse(""), default);
    }

    #[test]
    fn older_files_still_load() {
        // 0.1.0 had a list of programs to ignore; it's gone, and its line is skipped.
        let settings = Settings::parse("ignore_programs=keepass.exe; mstsc.exe\nskip_private=false\n");
        assert_eq!(settings, Settings { skip_private: false, ..Settings::default() });
        assert!(Settings::default().skip_private, "private copies are skipped unless you say otherwise");
    }

    #[test]
    fn delays_shortest_first() {
        let swapped = Settings { min_delay: 0.5, max_delay: 0.1, ..Settings::default() };
        assert_eq!(swapped.delays(), (Duration::from_millis(100), Duration::from_millis(500)));
    }
}
