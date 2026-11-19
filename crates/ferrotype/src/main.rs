//! FerroType, the clipboard typing utility: types text into windows that won't take a paste.
// `deny` rather than `forbid`: the Slint-generated UI glue opts into `unsafe` internally.
// Handwritten code in this crate can't.
#![deny(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::RefCell;
use std::num::NonZeroIsize;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferrotype_core::history::{Added, Entry, History, Limits, Mode, Skip, Source};
use ferrotype_core::hotkey::{self, Hotkey, Key};
use ferrotype_core::rng::Rng;
use ferrotype_core::settings::{self, BYTES_RANGE, ENTRIES_RANGE, MAX_COUNTDOWN, MAX_DELAY, Settings, ThemeMode};
use ferrotype_core::typing::{self, Cancel, Keyboard, Outcome, Plan, Progress, Refused};
use ferrotype_core::vault::{self, DataKey, Sealed, Secret, Vault};
use ferrotype_core::{format, history, text};
use ferrotype_sys::clipboard::{self, Clip, Copied};
use ferrotype_sys::input::{self, Layout, Window};
use ferrotype_sys::install::{self, Removed};
use ferrotype_sys::instance;
use ferrotype_sys::security_key::{self, KeyError};
use ferrotype_sys::watcher::{Event, HotkeySpec, Watcher};
use ferrotype_sys::window::{Rgb, TitleBarStyle};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

slint::include_modules!();

/// Mixed into the history file's encryption, so only FerroType decrypts it as its own.
const HISTORY_ENTROPY: &[u8] = b"FerroType history v1";
const HISTORY_FILE: &str = "history.dat";

/// Said once FerroType has been uninstalled.
const UNINSTALLED: &str = "FerroType was uninstalled. Your settings and saved history were kept, in \
                           %APPDATA%\\FerroType; delete that folder to remove them too.";

/// Hotkey ids. Each hotkey is also registered with Shift added (see `Hotkey::with_shift`).
const START: i32 = 1;
const START_SHIFTED: i32 = 2;
const STOP: i32 = 3;
const STOP_SHIFTED: i32 = 4;
/// `ERROR_HOTKEY_ALREADY_REGISTERED`.
const HOTKEY_TAKEN: u32 = 1409;

thread_local! {
    /// App state, reachable from events posted to the UI thread.
    static APP: RefCell<Option<Rc<RefCell<App>>>> = const { RefCell::new(None) };
}

struct App {
    ui: slint::Weak<MainWindow>,
    settings: Settings,
    history: History,
    rows: Rc<VecModel<HistoryRow>>,
    filter: String,
    watcher: Option<Watcher>,
    /// The running typing job, if any.
    job: Option<Cancel>,
    /// What the running job types, to say where it stopped.
    job_text: String,
    save_timer: slint::Timer,
    /// The security-key lock on the saved history.
    lock: Lock,
    /// A security-key prompt is showing.
    key_busy: bool,
    /// The vault being set up while the first security key is registered.
    new_vault: Option<(Vault, DataKey)>,
    /// This was the installed copy, and it was uninstalled: say so once the window has closed.
    uninstalled: bool,
}

/// Whether the saved history is locked with security keys.
enum Lock {
    /// No: it's protected by DPAPI (the Windows account) only.
    None,
    /// Yes, and not unlocked yet this run: the saved entries aren't loaded, and saving waits.
    Locked(Sealed),
    /// Yes, and unlocked until FerroType closes.
    Open { vault: Vault, key: DataKey },
}

/// What the history file held.
enum Loaded {
    Plain(Vec<Entry>),
    Locked(Sealed),
}

impl App {
    fn new(ui: &MainWindow, settings: Settings) -> App {
        let history = History::new(settings.history_mode, settings.history_limits);
        App {
            ui: ui.as_weak(),
            settings,
            history,
            rows: Rc::new(VecModel::default()),
            filter: String::new(),
            watcher: None,
            job: None,
            job_text: String::new(),
            save_timer: slint::Timer::default(),
            lock: Lock::None,
            key_busy: false,
            new_vault: None,
            uninstalled: false,
        }
    }

    // -----------------------------------------------------------------------------------------
    // Typing
    // -----------------------------------------------------------------------------------------

    fn typing(&self) -> bool {
        self.job.is_some()
    }

    fn start_stop(&mut self, ui: &MainWindow) {
        if let Some(cancel) = &self.job {
            cancel.cancel();
            ui.set_status("Stopping...".into());
            return;
        }
        self.read_typing_fields(ui);
        let raw = ui.get_editor_text().to_string();
        let prepared = text::prepare(&raw);
        if prepared.is_empty() {
            ui.set_status("Nothing to type: put some text in the box first.".into());
            return;
        }
        if self.settings.remember_typed {
            self.history.add(&raw, Source::Typed, now());
            self.history_changed(ui);
        }
        let (min_delay, max_delay) = self.settings.delays();
        let plan = Plan {
            text: prepared.clone(),
            min_delay,
            max_delay,
            countdown: self.settings.countdown,
            stop_on_focus_change: self.settings.stop_on_focus_change,
        };
        let cancel = Cancel::new();
        self.job = Some(cancel.clone());
        self.job_text = prepared;
        ui.set_typing(true);
        ui.set_progress(0.0);
        let stop_hint = self.stop_hint();
        let started = std::thread::Builder::new().name("ferrotype-typing".into()).spawn(move || {
            let outcome = typing::run(&SystemKeyboard, &plan, &cancel, &mut Rng::seeded(), |progress| {
                let stop_hint = stop_hint.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    with_app(|app, ui| app.show_progress(ui, progress, &stop_hint));
                });
            });
            let _ = slint::invoke_from_event_loop(move || with_app(|app, ui| app.finish(ui, outcome)));
        });
        if started.is_err() {
            self.job = None;
            ui.set_typing(false);
            ui.set_status("Couldn't start typing (no thread).".into());
        }
    }

    fn show_progress(&mut self, ui: &MainWindow, progress: Progress, stop_hint: &str) {
        if self.job.is_none() {
            return;
        }
        let status = match progress {
            Progress::Countdown(seconds) => format!("Starting in {seconds}... click where the text should go"),
            Progress::WaitingForTarget => "Waiting: click into the window to type into".to_owned(),
            Progress::PausedOwnWindow => {
                "Paused while FerroType has the focus: click back where it was typing, or press Stop".to_owned()
            }
            Progress::PausedModifiers => "Paused: let go of Ctrl, Alt, Shift and Win".to_owned(),
            Progress::PausedRefused => {
                "Paused: Windows isn't taking keystrokes right now (locked screen or UAC prompt?). Retrying..."
                    .to_owned()
            }
            Progress::Typing { done, total, left } => {
                ui.set_progress(done as f32 / total.max(1) as f32);
                match left {
                    Some(left) => format!(
                        "Typing... {} of {} characters, about {} left ({stop_hint})",
                        format::count(done),
                        format::count(total),
                        format::duration(left)
                    ),
                    None => format!("Typing... ({stop_hint})"),
                }
            }
        };
        ui.set_status(status.into());
    }

    fn finish(&mut self, ui: &MainWindow, outcome: Outcome) {
        self.job = None;
        ui.set_typing(false);
        let total = self.job_text.chars().count();
        let stopped = |why: &str, typed: usize| {
            format!(
                "{why} after {} of {} characters (in line {}).",
                format::count(typed),
                format::count(total),
                typing::line_of(&self.job_text, typed)
            )
        };
        let status = match outcome {
            Outcome::Done { typed } => format!("Done: typed {} characters.", format::count(typed)),
            Outcome::Stopped { typed } => stopped("Stopped", typed),
            Outcome::FocusMoved { typed } => stopped("Stopped: another window got the focus", typed),
            Outcome::Cancelled => "Cancelled.".to_owned(),
        };
        ui.set_status(status.into());
    }

    /// Reads the key delays, countdown and keep-on-top from the main window into the settings
    /// (fixing up anything out of range) and saves them if they changed.
    fn read_typing_fields(&mut self, ui: &MainWindow) {
        let seconds = |text: SharedString, fallback: f64| {
            text.trim().parse::<f64>().ok().filter(|v| v.is_finite()).map_or(fallback, |v| v.clamp(0.0, MAX_DELAY))
        };
        let before = self.settings.clone();
        self.settings.min_delay = seconds(ui.get_min_delay(), self.settings.min_delay);
        self.settings.max_delay = seconds(ui.get_max_delay(), self.settings.max_delay);
        self.settings.countdown =
            ui.get_countdown().trim().parse::<u32>().map_or(self.settings.countdown, |v| v.min(MAX_COUNTDOWN));
        self.settings.on_top = ui.get_on_top();
        self.show_typing_fields(ui);
        if self.settings != before {
            let _ = self.settings.save();
        }
    }

    fn show_typing_fields(&self, ui: &MainWindow) {
        ui.set_min_delay(self.settings.min_delay.to_string().into());
        ui.set_max_delay(self.settings.max_delay.to_string().into());
        ui.set_countdown(self.settings.countdown.to_string().into());
        ui.set_on_top(self.settings.on_top);
    }

    // -----------------------------------------------------------------------------------------
    // Hotkeys
    // -----------------------------------------------------------------------------------------

    fn toggle_hotkey(&self) -> bool {
        self.settings.start_hotkey.is_some() && self.settings.start_hotkey == self.settings.stop_hotkey
    }

    fn stop_hint(&self) -> String {
        match self.settings.stop_hotkey {
            Some(hotkey) => format!("Stop or {hotkey} to stop"),
            None => "Stop to stop".to_owned(),
        }
    }

    /// Registers the hotkeys from the settings and says how it went under the buttons.
    fn register_hotkeys(&mut self, ui: &MainWindow) {
        let Some(watcher) = &self.watcher else {
            ui.set_hotkey_info("Hotkeys aren't available (FerroType couldn't start its hotkey listener).".into());
            ui.set_hotkey_problem(true);
            return;
        };
        let mut wanted: Vec<(Hotkey, i32, i32)> = Vec::new();
        if let Some(start) = self.settings.start_hotkey {
            wanted.push((start, START, START_SHIFTED));
        }
        if let Some(stop) = self.settings.stop_hotkey
            && !self.toggle_hotkey()
        {
            wanted.push((stop, STOP, STOP_SHIFTED));
        }
        let mut specs = Vec::new();
        for (hotkey, id, shifted_id) in &wanted {
            specs.push(spec(*hotkey, *id));
            if !hotkey.shift {
                specs.push(spec(hotkey.with_shift(), *shifted_id));
            }
        }
        let results = watcher.set_hotkeys(&specs);
        // Only the hotkeys as set count; their Shift twins are a convenience.
        let problems: Vec<String> = wanted
            .iter()
            .filter_map(|(hotkey, id, _)| {
                let index = specs.iter().position(|s| s.id == *id)?;
                let error = results.get(index)?.as_ref().err()?;
                Some(if error.code() == Some(HOTKEY_TAKEN) {
                    format!("{hotkey} is already used by another program")
                } else {
                    format!("{hotkey} couldn't be set up ({error})")
                })
            })
            .collect();
        let info = if !problems.is_empty() {
            format!("{}. Pick another in Settings.", problems.join("; "))
        } else {
            match (self.settings.start_hotkey, self.settings.stop_hotkey) {
                (Some(start), _) if self.toggle_hotkey() => {
                    format!("{start} starts and stops typing, from any program.")
                }
                (Some(start), Some(stop)) => format!("{start} starts typing and {stop} stops it, from any program."),
                (Some(start), None) => format!("{start} starts typing, from any program."),
                (None, Some(stop)) => format!("{stop} stops typing, from any program."),
                (None, None) => "No hotkeys: use Start typing and Stop. Set them up in Settings.".to_owned(),
            }
        };
        ui.set_hotkey_problem(!problems.is_empty());
        ui.set_hotkey_info(info.into());
    }

    fn hotkey_pressed(&mut self, ui: &MainWindow, id: i32) {
        if ui.get_settings_open() {
            return;
        }
        let starts = matches!(id, START | START_SHIFTED);
        let stops = matches!(id, STOP | STOP_SHIFTED) || (starts && self.toggle_hotkey());
        if (starts && !self.typing()) || (stops && self.typing()) {
            self.start_stop(ui);
        }
    }

    // -----------------------------------------------------------------------------------------
    // History
    // -----------------------------------------------------------------------------------------

    /// A copy the watcher noticed. It only listens while recording copies is on, and leaves out
    /// copies marked private when Settings says to.
    fn clipboard_changed(&mut self, ui: &MainWindow, clip: Clip) {
        // FerroType's own copies came out of the history in the first place. (The check on the
        // setting catches a copy that was on its way when recording was turned off.)
        if let Clip::Text(Copied { text, own: false }) = clip
            && self.settings.watch_clipboard
        {
            let _ = self.add_copy(ui, &text);
        }
    }

    /// Puts copied text into the history, or says why it didn't go in.
    fn add_copy(&mut self, ui: &MainWindow, text: &str) -> Result<(), String> {
        match self.history.add(text, Source::Copied, now()) {
            Added::New(_) | Added::Moved(_) => {
                self.history_changed(ui);
                Ok(())
            }
            Added::Skipped(Skip::Off) => Err("history is off in Settings".to_owned()),
            Added::Skipped(Skip::Blank) => Err("it's only blank space".to_owned()),
            Added::Skipped(Skip::TooBig) => Err(format!(
                "it's bigger than the history's size limit ({})",
                format::bytes(self.history.limits().max_bytes)
            )),
        }
    }

    /// Shows the history as it is now and saves it (shortly, so a burst of copies is one write).
    fn history_changed(&mut self, ui: &MainWindow) {
        self.refresh_history(ui);
        self.save_timer.start(slint::TimerMode::SingleShot, Duration::from_secs(1), || {
            with_app(|app, _| {
                app.save_history();
            });
        });
    }

    fn refresh_history(&self, ui: &MainWindow) {
        let now = now();
        let filter = self.filter.to_lowercase();
        let rows: Vec<HistoryRow> = self
            .history
            .ordered()
            .filter(|entry| filter.is_empty() || entry.text.to_lowercase().contains(&filter))
            .map(|entry| history_row(entry, now))
            .collect();
        sync_model(&self.rows, rows);
        let usage = self.history.usage();
        let limits = self.history.limits();
        let mode = match self.history.mode() {
            Mode::Off => "history off",
            Mode::Session => "kept until FerroType closes",
            Mode::Saved => "saved, encrypted",
        };
        let mut summary = format!(
            "{} of {} entries, {} of {} ({mode})",
            usage.entries,
            limits.max_entries,
            format::bytes(usage.bytes),
            format::bytes(limits.max_bytes)
        );
        if usage.pinned > 0 {
            summary = format!("{} pinned; {summary}", usage.pinned);
        }
        ui.set_history_summary(summary.into());
        ui.set_history_count((usage.entries + usage.pinned) as i32);
        ui.set_history_empty(
            if self.filter.is_empty() {
                "Nothing yet. Text you copy or type shows up here; pin what you want to keep."
            } else {
                "Nothing matches the search."
            }
            .into(),
        );
    }

    fn entry_text(&self, id: i32) -> Option<String> {
        self.history.get(u64::try_from(id).ok()?).map(|entry| entry.text.clone())
    }

    fn use_entry(&mut self, ui: &MainWindow, id: i32) {
        if self.typing() {
            return;
        }
        if let Some(text) = self.entry_text(id) {
            ui.set_editor_text(text.into());
            ui.set_tab(0);
            ui.set_status(
                "In the editor: change it if you like, press Start typing, then click where it should go.".into(),
            );
        }
    }

    fn type_entry(&mut self, ui: &MainWindow, id: i32) {
        if self.typing() {
            return;
        }
        if let Some(text) = self.entry_text(id) {
            ui.set_editor_text(text.into());
            self.start_stop(ui);
        }
    }

    fn copy_entry(&mut self, ui: &MainWindow, id: i32) {
        let (Some(text), Some(watcher)) = (self.entry_text(id), &self.watcher) else { return };
        let status = match clipboard::set_text(watcher.window(), &text) {
            Ok(()) => "Copied to the clipboard.".to_owned(),
            Err(error) => format!("Couldn't copy: {error}."),
        };
        ui.set_status(status.into());
    }

    fn toggle_pin(&mut self, ui: &MainWindow, id: i32) {
        let Ok(id) = u64::try_from(id) else { return };
        let Some(pinned) = self.history.get(id).map(|entry| entry.pinned) else { return };
        self.history.set_pinned(id, !pinned);
        self.history_changed(ui);
    }

    fn delete_entry(&mut self, ui: &MainWindow, id: i32) {
        if let Ok(id) = u64::try_from(id)
            && self.history.remove(id)
        {
            self.history_changed(ui);
        }
    }

    fn clear_history(&mut self, ui: &MainWindow) {
        let removed = self.history.clear();
        ui.set_status(format!("Cleared {removed} history entries (pinned entries stay).").into());
        self.history_changed(ui);
    }

    fn pin_editor(&mut self, ui: &MainWindow) {
        let text = ui.get_editor_text().to_string();
        let status = match self.history.pin_text(&text, now()) {
            Added::New(_) | Added::Moved(_) if matches!(self.lock, Lock::Locked(_)) => {
                "Pinned for now: it's saved once you unlock the history (History tab).".to_owned()
            }
            Added::New(_) | Added::Moved(_) => "Pinned: it stays in the history until you unpin it.".to_owned(),
            Added::Skipped(Skip::Blank) => "Nothing to pin.".to_owned(),
            Added::Skipped(Skip::TooBig) => format!(
                "Too big to pin: the history size limit is {} (see Settings).",
                format::bytes(self.history.limits().max_bytes)
            ),
            Added::Skipped(Skip::Off) => unreachable!("pinning works with history off"),
        };
        ui.set_status(status.into());
        self.history_changed(ui);
    }

    fn paste_clipboard(&mut self, ui: &MainWindow) {
        let window = self.watcher.as_ref().map_or(0, Watcher::window);
        let text = match clipboard::get_text(window) {
            Ok(Some(text)) => text,
            Ok(None) => return ui.set_status("There's no text on the clipboard.".into()),
            Err(error) => return ui.set_status(format!("Couldn't read the clipboard: {error}.").into()),
        };
        ui.set_editor_text(text.as_str().into());
        // Pulling text in is deliberate, so it always goes into the history (unless history is
        // off altogether): whenever it was copied, whether it's marked private, and whether
        // recording copies is on.
        let status = match self.add_copy(ui, &text) {
            Ok(()) => "Pasted from the clipboard, and added to the history.".to_owned(),
            Err(why) => format!("Pasted from the clipboard. Not added to the history: {why}."),
        };
        ui.set_status(status.into());
    }

    fn filter_edited(&mut self, ui: &MainWindow, filter: SharedString) {
        self.filter = filter.trim().to_owned();
        self.refresh_history(ui);
    }

    /// Saves the history (see `write_history`), unless the file has been locked with a security
    /// key since this FerroType read it (by one in another Windows session, say): a locked file
    /// this FerroType hasn't unlocked is never overwritten or removed. It takes the lock on
    /// instead, and saves once it's unlocked.
    fn save_history(&mut self) -> bool {
        if matches!(self.lock, Lock::None)
            && let Some(sealed) = locked_on_disk()
        {
            self.lock = Lock::Locked(sealed);
            if let Some(ui) = self.ui.upgrade() {
                self.show_lock(&ui);
                ui.set_status(
                    "The saved history is locked with a security key: unlock it (History tab) to save changes.".into(),
                );
            }
            return false;
        }
        self.write_history()
    }

    /// Writes the pinned entries (and, if history is saved, the rest) encrypted to the history
    /// file: with DPAPI, and inside that with the security-key lock if there is one. Without a
    /// lock, the file is removed when there's nothing to keep. Returns whether the file is now
    /// as it should be.
    fn write_history(&self) -> bool {
        let Some(path) = history_path() else { return false };
        let keep = self.history.to_save();
        let contents = match &self.lock {
            // Not unlocked this run: the file can't be rewritten without the key, so it stays as
            // it was, and what changed is saved once it's unlocked.
            Lock::Locked(_) => return false,
            // Locked files stay even when empty: they hold the registered keys.
            Lock::Open { vault, key } => match vault.seal(key, &history::encode(&keep)) {
                Ok(sealed) => sealed,
                Err(_) => return false,
            },
            Lock::None if keep.is_empty() => return std::fs::remove_file(&path).is_ok() || !path.exists(),
            Lock::None => history::encode(&keep),
        };
        ferrotype_sys::dpapi::protect(&contents, HISTORY_ENTROPY)
            .is_ok_and(|protected| settings::write_atomically(&path, &protected).is_ok())
    }

    /// Reads the history file back; an unreadable one is set aside rather than overwritten. A
    /// file locked with a security key is kept locked until `unlock_history`.
    fn load_history(&mut self) -> Option<String> {
        let path = history_path()?;
        let sealed = std::fs::read(&path).ok()?;
        let loaded = ferrotype_sys::dpapi::unprotect(&sealed, HISTORY_ENTROPY).ok().and_then(|plain| {
            if vault::is_locked_file(&plain) {
                Sealed::parse(&plain).map(Loaded::Locked)
            } else {
                history::decode(&plain).map(Loaded::Plain)
            }
        });
        match loaded {
            Some(Loaded::Plain(entries)) => {
                self.history.restore(entries);
                None
            }
            Some(Loaded::Locked(sealed)) => {
                self.lock = Lock::Locked(sealed);
                None
            }
            None => {
                let aside = path.with_file_name("history.unreadable.dat");
                let _ = std::fs::rename(&path, &aside);
                Some(format!(
                    "The saved history couldn't be read (another Windows account's, or damaged); it was moved \
                     aside as {}.",
                    aside.display()
                ))
            }
        }
    }

    fn apply_clipboard_watch(&mut self, ui: &MainWindow) {
        let Some(watcher) = &self.watcher else { return };
        let watch = self.settings.watch_clipboard && self.history.mode() != Mode::Off;
        let (limit, skip_private) = (self.settings.history_limits.max_bytes, self.settings.skip_private);
        if watcher.watch_clipboard(watch, limit, skip_private).is_err() {
            ui.set_status("Couldn't listen for copies, so copied text won't show up in the history.".into());
        }
    }

    // -----------------------------------------------------------------------------------------
    // The security-key lock
    // -----------------------------------------------------------------------------------------

    /// Shows the lock's state on the History tab and the Settings page.
    fn show_lock(&self, ui: &MainWindow) {
        let keys = |vault: &Vault| match vault.slots().len() {
            1 => "a security key".to_owned(),
            n => format!("{n} security keys"),
        };
        let (state, summary) = match &self.lock {
            Lock::None if !security_key::supported() => {
                (0, "This version of Windows can't get secrets from security keys (Windows 11 can).".to_owned())
            }
            Lock::None => (0, "Not locked: the saved history is protected by your Windows account only.".to_owned()),
            Lock::Locked(sealed) => (1, format!("Locked with {}; unlock it to make changes.", keys(sealed.vault()))),
            Lock::Open { vault, .. } => {
                (2, format!("Locked with {}, and unlocked until FerroType closes.", keys(vault)))
            }
        };
        ui.set_lock_state(state);
        ui.set_lock_summary(summary.into());
        ui.set_key_busy(self.key_busy);
        ui.set_keys_supported(security_key::supported());
    }

    /// Says how a security-key step went, in the status line and on the Settings page.
    fn key_message(&self, ui: &MainWindow, message: &str) {
        ui.set_key_message(message.into());
        ui.set_status(message.into());
    }

    /// Runs a security-key prompt off the UI thread (Windows' prompt blocks until it's answered),
    /// then hands its result to `done` back on the UI thread.
    fn key_prompt<T: Send + 'static>(
        &mut self,
        ui: &MainWindow,
        waiting: &str,
        prompt: impl FnOnce() -> Result<T, KeyError> + Send + 'static,
        done: fn(&mut App, &MainWindow, Result<T, KeyError>),
    ) {
        self.key_busy = true;
        self.show_lock(ui);
        self.key_message(ui, waiting);
        let started = std::thread::Builder::new().name("ferrotype-security-key".into()).spawn(move || {
            let result = prompt();
            let _ = slint::invoke_from_event_loop(move || {
                with_app(|app, ui| {
                    app.key_busy = false;
                    done(app, ui, result);
                    app.show_lock(ui);
                });
            });
        });
        if started.is_err() {
            self.key_busy = false;
            self.show_lock(ui);
            self.key_message(ui, "Couldn't start talking to the security key.");
        }
    }

    fn unlock_history(&mut self, ui: &MainWindow) {
        let Lock::Locked(sealed) = &self.lock else { return };
        let Some(window) = window_handle(ui) else { return };
        if self.key_busy {
            return;
        }
        let credentials = sealed.vault().credentials();
        let salt = *sealed.vault().salt();
        self.key_prompt(
            ui,
            "Unlocking: enter your security key's PIN in Windows' prompt, then touch the key.",
            move || security_key::secret(window, &credentials, &salt),
            App::unlock_finished,
        );
    }

    fn unlock_finished(&mut self, ui: &MainWindow, answer: Result<(Vec<u8>, Secret), KeyError>) {
        let Lock::Locked(sealed) = &self.lock else { return };
        let opened = answer.map_err(|error| error.to_string()).and_then(|(credential, secret)| {
            let key = sealed.vault().unlock(&credential, &secret).map_err(|error| error.to_string())?;
            let plain = sealed.open(&key).map_err(|error| error.to_string())?;
            let entries = history::decode(&plain).ok_or_else(|| "the locked history is damaged".to_owned())?;
            Ok((sealed.vault().clone(), key, entries))
        });
        match opened {
            Ok((vault, key, entries)) => {
                self.lock = Lock::Open { vault, key };
                // Saved entries join anything recorded while it was locked; saving then keeps both.
                self.history.restore(entries);
                self.history_changed(ui);
                self.key_message(ui, "Unlocked: your saved entries are back until FerroType closes.");
            }
            Err(why) => self.key_message(ui, &format!("Not unlocked: {why}.")),
        }
    }

    /// Registers a security key: the first one locks the saved history, later ones are backups.
    fn add_security_key(&mut self, ui: &MainWindow) {
        if self.key_busy {
            return;
        }
        if !security_key::supported() {
            return self.key_message(ui, "This version of Windows can't get secrets from security keys.");
        }
        let Some(window) = window_handle(ui) else { return };
        let (salt, exclude) = match &self.lock {
            Lock::Locked(_) => return self.key_message(ui, "Unlock the history first."),
            Lock::Open { vault, .. } => (*vault.salt(), vault.credentials()),
            Lock::None => match Vault::create() {
                Ok((vault, key)) => {
                    let salt = *vault.salt();
                    self.new_vault = Some((vault, key));
                    (salt, Vec::new())
                }
                Err(error) => return self.key_message(ui, &format!("Couldn't start: {error}.")),
            },
        };
        self.key_prompt(
            ui,
            "Registering: Windows asks for the security key's PIN and a touch, then for a touch again to check it works.",
            move || {
                let credential = security_key::register(window, &exclude)?;
                security_key::secret(window, &[credential], &salt)
            },
            App::key_added,
        );
    }

    fn key_added(&mut self, ui: &MainWindow, answer: Result<(Vec<u8>, Secret), KeyError>) {
        let new_vault = self.new_vault.take();
        let (credential, secret) = match answer {
            Ok(answer) => answer,
            Err(error) => return self.key_message(ui, &format!("Not registered: {error}.")),
        };
        let (mut vault, key) = match std::mem::replace(&mut self.lock, Lock::None) {
            Lock::Open { vault, key } => (vault, key),
            Lock::None => match new_vault {
                Some(pair) => pair,
                None => return,
            },
            locked @ Lock::Locked(_) => {
                self.lock = locked;
                return;
            }
        };
        let first = vault.slots().is_empty();
        if let Err(error) = vault.add_key(&credential, &secret, &key, now()) {
            if !first {
                self.lock = Lock::Open { vault, key };
            }
            return self.key_message(ui, &format!("Not registered: {error}."));
        }
        self.lock = Lock::Open { vault, key };
        let message = if !self.save_history() {
            "Registered, but the history file couldn't be written; it will be tried again on the next change."
        } else if first {
            "Locked with your security key. After FerroType restarts, unlocking takes the key, its PIN and a \
             touch. Register a second key as a backup: lose your only key and the saved entries are gone."
        } else {
            "Backup key registered: any of your registered keys unlocks the history."
        };
        self.key_message(ui, message);
    }

    /// Takes the security-key lock off: the saved history goes back to DPAPI only.
    fn remove_security_lock(&mut self, ui: &MainWindow) {
        if self.key_busy || !matches!(self.lock, Lock::Open { .. }) {
            return;
        }
        self.lock = Lock::None;
        self.write_history();
        self.show_lock(ui);
        self.key_message(ui, "The lock is off: the saved history is protected by your Windows account only.");
    }

    // -----------------------------------------------------------------------------------------
    // Installing
    // -----------------------------------------------------------------------------------------

    fn show_install(&self, ui: &MainWindow) {
        let (state, summary) = install_status();
        ui.set_install_state(state);
        ui.set_install_summary(summary.into());
    }

    fn install_message(&self, ui: &MainWindow, message: &str) {
        ui.set_install_message(message.into());
        ui.set_status(message.into());
    }

    fn install(&mut self, ui: &MainWindow) {
        let installed = std::env::current_exe()
            .map_err(|error| error.to_string())
            .and_then(|exe| install::install(&exe).map_err(|error| error.to_string()));
        match installed {
            Ok(_) => self.install_message(
                ui,
                "Installed: FerroType is in the Start menu and in Settings → Apps. Start it from there from now \
                 on; this copy keeps running until you close it.",
            ),
            Err(why) => self.install_message(ui, &format!("Couldn't install: {why}.")),
        }
        self.show_install(ui);
    }

    fn uninstall(&mut self, ui: &MainWindow) {
        match install::uninstall() {
            Ok(Removed::AfterExit) => {
                // This is the installed copy: it closes, and its file goes once it has.
                self.uninstalled = true;
                self.shut_down(ui);
                let _ = ui.hide();
            }
            Ok(Removed::Now) => {
                self.install_message(ui, "Uninstalled. Your settings and saved history are kept.");
                self.show_install(ui);
            }
            Err(error) => self.install_message(ui, &format!("Couldn't uninstall: {error}.")),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Settings
    // -----------------------------------------------------------------------------------------

    fn open_settings(&mut self, ui: &MainWindow) {
        if let Some(watcher) = &self.watcher {
            // So pressing a combination to record it doesn't also fire it.
            watcher.set_hotkeys(&[]);
        }
        let s = &self.settings;
        ui.set_opt_start_hotkey(hotkey::label(s.start_hotkey).into());
        ui.set_opt_stop_hotkey(hotkey::label(s.stop_hotkey).into());
        ui.set_opt_stop_on_focus(s.stop_on_focus_change);
        ui.set_opt_history_mode(Mode::ALL.iter().position(|m| *m == s.history_mode).unwrap_or(1) as i32);
        ui.set_opt_max_entries(s.history_limits.max_entries.to_string().into());
        ui.set_opt_max_mb(megabytes(s.history_limits.max_bytes).into());
        ui.set_opt_watch_clipboard(s.watch_clipboard);
        ui.set_opt_skip_private(s.skip_private);
        ui.set_opt_remember_typed(s.remember_typed);
        ui.set_opt_theme(ThemeMode::ALL.iter().position(|t| *t == s.theme).unwrap_or(0) as i32);
        ui.set_settings_error(SharedString::new());
        ui.set_key_message(SharedString::new());
        self.show_lock(ui);
        ui.set_install_message(SharedString::new());
        ui.set_install_confirm(false);
        self.show_install(ui);
        ui.set_recording(-1);
        ui.set_settings_open(true);
    }

    fn close_settings(&mut self, ui: &MainWindow) {
        ui.set_settings_open(false);
        ui.set_recording(-1);
        self.register_hotkeys(ui);
    }

    fn save_settings(&mut self, ui: &MainWindow) {
        match self.read_settings_page(ui) {
            Ok(new) => {
                let mode_changed = new.history_mode != self.settings.history_mode;
                self.settings = new;
                self.history.set_mode(self.settings.history_mode);
                self.history.set_limits(self.settings.history_limits);
                self.apply_clipboard_watch(ui);
                self.apply_theme(ui);
                let _ = self.settings.save();
                self.refresh_history(ui);
                // Leaving saved history takes it off the disk now, not on the next change.
                if mode_changed {
                    self.save_history();
                }
                self.close_settings(ui);
                ui.set_status("Settings saved.".into());
            }
            Err(problem) => ui.set_settings_error(problem.into()),
        }
    }

    fn read_settings_page(&self, ui: &MainWindow) -> Result<Settings, String> {
        let start = Hotkey::parse(&ui.get_opt_start_hotkey()).map_err(|e| format!("Start typing: {e}"))?;
        let stop = Hotkey::parse(&ui.get_opt_stop_hotkey()).map_err(|e| format!("Stop typing: {e}"))?;
        let max_entries = ui
            .get_opt_max_entries()
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| (ENTRIES_RANGE.0..=ENTRIES_RANGE.1).contains(n))
            .ok_or_else(|| {
                format!("History entries: a whole number from {} to {}.", ENTRIES_RANGE.0, ENTRIES_RANGE.1)
            })?;
        let max_bytes = ui
            .get_opt_max_mb()
            .trim()
            .replace(',', ".")
            .parse::<f64>()
            .ok()
            .filter(|mb| mb.is_finite())
            .map(|mb| (mb * 1024.0 * 1024.0).round() as usize)
            .filter(|bytes| (BYTES_RANGE.0..=BYTES_RANGE.1).contains(bytes))
            .ok_or_else(|| {
                format!("History size: from {} to {} MB.", megabytes(BYTES_RANGE.0), megabytes(BYTES_RANGE.1))
            })?;
        Ok(Settings {
            start_hotkey: start,
            stop_hotkey: stop,
            stop_on_focus_change: ui.get_opt_stop_on_focus(),
            history_mode: usize::try_from(ui.get_opt_history_mode())
                .ok()
                .and_then(|i| Mode::ALL.get(i).copied())
                .unwrap_or_default(),
            history_limits: Limits { max_entries, max_bytes },
            watch_clipboard: ui.get_opt_watch_clipboard(),
            skip_private: ui.get_opt_skip_private(),
            remember_typed: ui.get_opt_remember_typed(),
            theme: usize::try_from(ui.get_opt_theme())
                .ok()
                .and_then(|i| ThemeMode::ALL.get(i).copied())
                .unwrap_or_default(),
            ..self.settings.clone()
        })
    }

    /// A key pressed while a hotkey box is recording: fills the box, or says why it can't be a
    /// hotkey.
    fn record_key(&mut self, ui: &MainWindow, which: i32, key_text: SharedString, modifiers: [bool; 4]) {
        let [ctrl, alt, shift, win] = modifiers;
        let Some(key) = key_text.chars().next().and_then(|c| key_from_slint(c, ctrl)) else {
            ui.set_settings_error("That key can't be part of a hotkey: try an F-key or a letter.".into());
            return;
        };
        match Hotkey::new(ctrl, alt, shift, win, key).check() {
            Ok(hotkey) => {
                let label = SharedString::from(hotkey.to_string());
                if which == 0 {
                    ui.set_opt_start_hotkey(label)
                } else {
                    ui.set_opt_stop_hotkey(label)
                }
                ui.set_settings_error(SharedString::new());
            }
            Err(problem) => ui.set_settings_error(problem.into()),
        }
    }

    fn apply_theme(&self, ui: &MainWindow) {
        let dark = match self.settings.theme {
            ThemeMode::Light => false,
            ThemeMode::Dark => true,
            ThemeMode::System => ferrotype_sys::system::apps_use_light_theme() == Some(false),
        };
        ui.global::<Theme>().set_dark(dark);
        ui.set_dark_mode(dark);
        style_title_bar(ui);
    }

    /// Saves everything on the way out.
    fn shut_down(&mut self, ui: &MainWindow) {
        if let Some(cancel) = &self.job {
            cancel.cancel();
        }
        if self.key_busy {
            security_key::cancel();
        }
        self.read_typing_fields(ui);
        self.save_history();
        // Dropping the watcher unregisters the hotkeys and stops listening to the clipboard.
        self.watcher = None;
    }
}

/// Types with real keystrokes through `ferrotype-sys`.
struct SystemKeyboard;

impl Keyboard for SystemKeyboard {
    type Window = Window;
    type Layout = Layout;

    fn foreground(&self) -> Option<Window> {
        input::foreground_window()
    }

    fn is_own(&self, window: Window) -> bool {
        input::is_own_window(window)
    }

    fn layout(&self, window: Option<Window>) -> Layout {
        input::keyboard_layout(window)
    }

    fn modifiers_held(&self) -> bool {
        input::modifiers_held()
    }

    fn type_char(&self, c: char, layout: Layout, hold: Duration) -> Result<(), Refused> {
        input::type_char(c, layout, hold).map_err(|_| Refused)
    }
}

fn spec(hotkey: Hotkey, id: i32) -> HotkeySpec {
    HotkeySpec { id, ctrl: hotkey.ctrl, alt: hotkey.alt, shift: hotkey.shift, win: hotkey.win, vk: hotkey.key.vk }
}

/// The key a Slint key event names. Slint reports special keys as private-use characters (its
/// `Key` namespace); with Ctrl held, letters may arrive as control codes 1-26.
fn key_from_slint(c: char, ctrl: bool) -> Option<Key> {
    let code = u32::from(c);
    let name = match code {
        0xF704..=0xF71B => return Key::from_vk(0x70 + (code - 0xF704) as u16),
        0xF700 => "Up",
        0xF701 => "Down",
        0xF702 => "Left",
        0xF703 => "Right",
        0xF727 => "Insert",
        0x7F => "Delete",
        0xF729 => "Home",
        0xF72B => "End",
        0xF72C => "Page Up",
        0xF72D => "Page Down",
        0xF72F => "Scroll Lock",
        0xF730 => "Pause",
        0x20 => "Space",
        0x0A | 0x0D => "Enter",
        0x09 => "Tab",
        0x1B => "Escape",
        0x08 => "Backspace",
        0x01..=0x1A if ctrl => return Key::from_vk(0x40 + code as u16),
        _ if c.is_ascii_alphanumeric() => return Key::from_vk(c.to_ascii_uppercase() as u16),
        _ => return None,
    };
    Key::from_name(name)
}

fn history_row(entry: &Entry, now: u64) -> HistoryRow {
    let source = match entry.source {
        Source::Copied => "copied",
        Source::Typed => "typed",
        Source::Pinned => "pinned",
    };
    let lines = text::line_count(&entry.text);
    let lines = if lines == 1 { String::new() } else { format!(", {} lines", format::count(lines)) };
    HistoryRow {
        id: i32::try_from(entry.id).unwrap_or(i32::MAX),
        preview: text::preview(&entry.text, 160).into(),
        detail: format!(
            "{} {}{lines}, {}",
            source,
            format::age(now.saturating_sub(entry.when)),
            format::bytes(entry.text.len())
        )
        .into(),
        pinned: entry.pinned,
    }
}

/// "2", "0.5", "1.25": megabytes for the Settings page.
fn megabytes(bytes: usize) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    let text = format!("{mb:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn history_path() -> Option<PathBuf> {
    Settings::folder().map(|folder| folder.join(HISTORY_FILE))
}

/// The Settings page's Installation section: 0 not installed, 1 this is the installed copy,
/// 2 installed (this same build) somewhere else, 3 installed, another build; and what to say.
fn install_status() -> (i32, String) {
    let version = env!("CARGO_PKG_VERSION");
    let Ok(copy) = install::installed_exe() else {
        return (0, "Not installed (your Programs folder couldn't be found).".to_owned());
    };
    let folder = copy.parent().map_or_else(String::new, |folder| folder.display().to_string());
    if !copy.exists() {
        return (
            0,
            format!(
                "Not installed: this copy runs from where you put it. Installing copies it to {folder}, for your \
                 Windows account only (no administrator rights needed), and adds it to the Start menu and to \
                 Settings → Apps."
            ),
        );
    }
    let Ok(own) = std::env::current_exe() else { return (2, format!("Installed in {folder}.")) };
    let here = own.parent().map_or_else(String::new, |folder| folder.display().to_string());
    if install::same_file(&own, &copy) {
        (1, format!("Installed in {folder}, and this is the installed copy (version {version})."))
    } else if install::same_build(&own, &copy) {
        (2, format!("Installed in {folder} (this same version). This copy runs from {here}."))
    } else {
        let theirs = install::installed_version().unwrap_or_else(|| "unknown".to_owned());
        if theirs == version {
            (3, format!("Installed in {folder}: another build of version {version}. Update to install this one."))
        } else {
            (3, format!("Version {theirs} is installed in {folder}. This is version {version}; update to install it."))
        }
    }
}

/// Uninstalling from Settings → Apps: a running FerroType is asked to close first (it saves its
/// history as it does), then a message says how it went, as there's no window.
fn uninstall_from_apps() -> i32 {
    let message = ferrotype_sys::system::message_box;
    if !instance::close_running(Duration::from_secs(10)) {
        message("FerroType", "FerroType is still running. Close it, then uninstall it again.", true);
        return 1;
    }
    match install::uninstall() {
        Ok(_) => {
            message("FerroType", UNINSTALLED, false);
            0
        }
        Err(error) => {
            message("FerroType", &format!("Couldn't uninstall FerroType: {error}."), true);
            1
        }
    }
}

/// Shows the main window and brings it to the front (FerroType was launched again).
fn bring_forward(ui: &MainWindow) {
    if let Some(hwnd) = window_handle(ui).and_then(NonZeroIsize::new) {
        ferrotype_sys::window::bring_to_front(hwnd);
    }
}

/// The history file as it is on disk now, if it's locked with a security key.
fn locked_on_disk() -> Option<Sealed> {
    let file = std::fs::read(history_path()?).ok()?;
    let plain = ferrotype_sys::dpapi::unprotect(&file, HISTORY_ENTROPY).ok()?;
    if vault::is_locked_file(&plain) { Sealed::parse(&plain) } else { None }
}

/// Updates the model in place, so the list keeps its scroll position and only changed rows
/// redraw.
fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    let len = rows.len();
    for (index, row) in rows.into_iter().enumerate() {
        if index >= model.row_count() {
            model.push(row);
        } else if model.row_data(index).as_ref() != Some(&row) {
            model.set_row_data(index, row);
        }
    }
    while model.row_count() > len {
        model.remove(model.row_count() - 1);
    }
}

fn with_app(f: impl FnOnce(&mut App, &MainWindow)) {
    let Some(app) = APP.with(|cell| cell.borrow().clone()) else { return };
    let Ok(mut app) = app.try_borrow_mut() else { return };
    if let Some(ui) = app.ui.upgrade() {
        f(&mut app, &ui);
    }
}

/// Wraps an `App` method as a Slint callback with one argument.
fn handler<A: 'static>(action: impl Fn(&mut App, &MainWindow, A) + 'static) -> impl Fn(A) + 'static {
    move |arg| with_app(|app, ui| action(app, ui, arg))
}

/// Wraps an `App` method as a Slint callback without arguments.
fn action(action: impl Fn(&mut App, &MainWindow) + 'static) -> impl FnMut() + 'static {
    move || with_app(|app, ui| action(app, ui))
}

/// The main window's native handle, for Windows' security-key prompts to belong to.
fn window_handle(ui: &MainWindow) -> Option<isize> {
    let handle = ui.window().window_handle();
    match handle.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(win32)) => Some(win32.hwnd.get()),
        _ => None,
    }
}

/// Paints the native title bar in the current theme's colors.
fn style_title_bar(ui: &MainWindow) {
    let handle = ui.window().window_handle();
    let Ok(RawWindowHandle::Win32(win32)) = handle.window_handle().map(|h| h.as_raw()) else { return };
    let theme = ui.global::<Theme>();
    let rgb = |color: slint::Color| Rgb(color.red(), color.green(), color.blue());
    let style = TitleBarStyle {
        dark: theme.get_dark(),
        caption: rgb(theme.get_panel()),
        text: rgb(theme.get_text()),
        border: rgb(theme.get_border()),
    };
    // Older Windows versions without title-bar theming just keep the default look.
    let _ = ferrotype_sys::window::style_title_bar(win32.hwnd, &style);
}

/// Styles the title bar and watches repaints once the native window exists: `show()` only asks
/// for it, and it's created when the event loop gets to it.
fn when_native_window(ui: slint::Weak<MainWindow>, tries: u32) {
    let Some(window) = ui.upgrade() else { return };
    let handle = window.window().window_handle();
    if let Ok(RawWindowHandle::Win32(win32)) = handle.window_handle().map(|h| h.as_raw()) {
        style_title_bar(&window);
        let _ = ferrotype_sys::window::watch_repaints(win32.hwnd, || {
            // Toggling this marks the whole window changed (see `repaint-all` in main.slint).
            let _ = slint::invoke_from_event_loop(|| with_app(|_, ui| ui.set_repaint_all(!ui.get_repaint_all())));
        });
    } else if tries < 250 {
        slint::Timer::single_shot(Duration::from_millis(20), move || when_native_window(ui, tries + 1));
    }
}

fn main() -> Result<(), slint::PlatformError> {
    // Before anything else loads a DLL: System32-only search, no remote or low-integrity images,
    // no legacy injection points.
    let _ = ferrotype_sys::system::harden_process();

    // Uninstalling has no window.
    let arguments: Vec<String> = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    match arguments.as_slice() {
        [flag] if flag == "--uninstall" => std::process::exit(uninstall_from_apps()),
        [flag, uninstalling] if flag == install::FINISH_UNINSTALL => {
            if let Ok(uninstalling) = uninstalling.parse() {
                install::finish_uninstall(uninstalling);
            }
            return Ok(());
        }
        _ => {}
    }

    // One FerroType at a time: launched again, it brings the running one forward instead.
    let Some(_instance) = instance::claim() else {
        instance::show_running();
        return Ok(());
    };

    let ui = MainWindow::new()?;
    let app = Rc::new(RefCell::new(App::new(&ui, Settings::load())));
    APP.with(|cell| *cell.borrow_mut() = Some(Rc::clone(&app)));
    {
        let mut app = app.borrow_mut();
        ui.set_version(env!("CARGO_PKG_VERSION").into());
        ui.set_history(ModelRc::from(Rc::clone(&app.rows)));
        app.apply_theme(&ui);
        app.show_typing_fields(&ui);
        let problem = app.load_history();
        app.watcher = Watcher::start(|event| {
            let _ = slint::invoke_from_event_loop(move || {
                with_app(|app, ui| match event {
                    Event::Hotkey(id) => app.hotkey_pressed(ui, id),
                    Event::Clipboard(clip) => app.clipboard_changed(ui, clip),
                    Event::ShowWindow => bring_forward(ui),
                    Event::Close => {
                        app.shut_down(ui);
                        let _ = ui.hide();
                    }
                });
            });
        })
        .ok();
        app.register_hotkeys(&ui);
        app.apply_clipboard_watch(&ui);
        app.refresh_history(&ui);
        app.show_lock(&ui);
        if let Some(problem) = problem {
            ui.set_status(problem.into());
        }
    }

    ui.on_start_stop(action(|app, ui| app.start_stop(ui)));
    ui.on_paste_clipboard(action(|app, ui| app.paste_clipboard(ui)));
    ui.on_pin_editor(action(|app, ui| app.pin_editor(ui)));
    ui.on_clear_editor(action(|_, ui| ui.set_editor_text(SharedString::new())));
    ui.on_use_entry(handler(App::use_entry));
    ui.on_type_entry(handler(App::type_entry));
    ui.on_copy_entry(handler(App::copy_entry));
    ui.on_toggle_pin(handler(App::toggle_pin));
    ui.on_delete_entry(handler(App::delete_entry));
    ui.on_clear_history(action(|app, ui| app.clear_history(ui)));
    ui.on_filter_edited(handler(App::filter_edited));
    ui.on_unlock_history(action(|app, ui| app.unlock_history(ui)));
    ui.on_add_security_key(action(|app, ui| app.add_security_key(ui)));
    ui.on_remove_security_lock(action(|app, ui| app.remove_security_lock(ui)));
    ui.on_install(action(|app, ui| app.install(ui)));
    ui.on_uninstall(action(|app, ui| app.uninstall(ui)));
    ui.on_open_settings(action(|app, ui| app.open_settings(ui)));
    ui.on_save_settings(action(|app, ui| app.save_settings(ui)));
    ui.on_close_settings(action(|app, ui| app.close_settings(ui)));
    ui.on_record_key(|which, key, ctrl, alt, shift, win| {
        with_app(|app, ui| app.record_key(ui, which, key, [ctrl, alt, shift, win]));
    });
    ui.window().on_close_requested(|| {
        with_app(|app, ui| app.shut_down(ui));
        slint::CloseRequestResponse::HideWindow
    });

    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::ZERO, move || when_native_window(weak, 0));
    ui.run()?;
    let uninstalled = APP.with(|cell| cell.borrow_mut().take()).is_some_and(|app| app.borrow().uninstalled);
    if uninstalled {
        ferrotype_sys::system::message_box("FerroType", UNINSTALLED, false);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slint_keys_map_to_hotkey_keys() {
        let key = |code: u32, ctrl: bool| key_from_slint(char::from_u32(code).unwrap(), ctrl).map(|k| k.name);
        assert_eq!(key(0xF704, false), Some("F1"));
        assert_eq!(key(0xF70C, false), Some("F9"));
        assert_eq!(key(0xF71B, false), Some("F24"));
        assert_eq!(key(0xF72C, false), Some("Page Up"));
        assert_eq!(key(0xF730, false), Some("Pause"));
        assert_eq!(key(u32::from('s'), true), Some("S"));
        assert_eq!(key(0x13, true), Some("S"), "Ctrl+S as a control code");
        assert_eq!(key(u32::from('7'), false), Some("7"));
        assert_eq!(key(u32::from('!'), false), None);
    }

    #[test]
    fn megabytes_read_nicely() {
        assert_eq!(megabytes(2 * 1024 * 1024), "2");
        assert_eq!(megabytes(512 * 1024), "0.5");
        assert_eq!(megabytes(16 * 1024), "0.02");
    }
}
