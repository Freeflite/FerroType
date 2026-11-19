//! A hidden, message-only window on its own thread that receives FerroType's hotkeys
//! (`RegisterHotKey`) and clipboard changes (`AddClipboardFormatListener`), and requests from
//! another FerroType launched while this one runs (see `instance`).
//!
//! These are Windows' intended APIs for both jobs: no keyboard hook watches what the user types,
//! and nothing polls the clipboard.

use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::{AddClipboardFormatListener, RemoveClipboardFormatListener};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    HWND_MESSAGE, MSG, MSGFLT_ALLOW, PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW,
    SendMessageW, TranslateMessage, WM_APP, WM_CLIPBOARDUPDATE, WM_CLOSE, WM_DESTROY, WM_HOTKEY, WNDCLASSEXW,
};

use crate::clipboard::{self, Clip};
use crate::system::wide;
use crate::{Error, Result};

/// Something the watcher noticed. Delivered on the watcher's own thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A registered hotkey was pressed; the id it was registered with.
    Hotkey(i32),
    /// The clipboard changed (only while watching it).
    Clipboard(Clip),
    /// FerroType was launched again: show the window and bring it to the front.
    ShowWindow,
    /// Asked to close (uninstalling from Settings → Apps): save and exit.
    Close,
}

/// A hotkey to register: the id to report it with, its modifiers and its virtual-key code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeySpec {
    pub id: i32,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub vk: u16,
}

/// The running watcher; dropping it closes the window and ends the thread. One per process.
pub struct Watcher {
    window: isize,
    thread: Option<JoinHandle<()>>,
}

/// The watcher window's class, which another FerroType looks for (see `instance`).
pub(crate) const CLASS: &str = "FerroTypeWatcher";

const WM_SET_HOTKEYS: u32 = WM_APP + 1;
const WM_WATCH_CLIPBOARD: u32 = WM_APP + 2;

static ON_EVENT: OnceLock<Box<dyn Fn(Event) + Send + Sync>> = OnceLock::new();
/// Hotkey ids registered on the watcher window (only touched on its thread).
static REGISTERED: Mutex<Vec<i32>> = Mutex::new(Vec::new());
static LISTENING: AtomicBool = AtomicBool::new(false);
/// Copies bigger than this many bytes aren't read.
static CLIPBOARD_LIMIT: AtomicUsize = AtomicUsize::new(2 * 1024 * 1024);
/// Copies marked private aren't read.
static SKIP_PRIVATE: AtomicBool = AtomicBool::new(true);

/// The message another FerroType posts to say "show your window".
pub(crate) fn show_message() -> u32 {
    static MESSAGE: OnceLock<u32> = OnceLock::new();
    *MESSAGE.get_or_init(|| registered_message("FerroType.ShowWindow"))
}

/// The message another FerroType posts to say "close" (uninstalling from Settings → Apps).
pub(crate) fn close_message() -> u32 {
    static MESSAGE: OnceLock<u32> = OnceLock::new();
    *MESSAGE.get_or_init(|| registered_message("FerroType.Close"))
}

fn registered_message(name: &str) -> u32 {
    let name = wide(name);
    // SAFETY: the name is NUL-terminated. 0 (failure) never matches a message, see below.
    unsafe { RegisterWindowMessageW(name.as_ptr()) }
}

/// A random number only this process knows, sent with `WM_SET_HOTKEYS`: other programs can find
/// the watcher window, and the message carries a pointer, so one without it is ignored.
fn cookie() -> usize {
    static COOKIE: OnceLock<usize> = OnceLock::new();
    *COOKIE.get_or_init(|| RandomState::new().hash_one(std::process::id()) as usize)
}

/// Passed by pointer with `WM_SET_HOTKEYS`.
struct HotkeyRequest {
    wanted: Vec<HotkeySpec>,
    results: Vec<Result<()>>,
}

impl Watcher {
    /// Starts the watcher thread and its window. `on_event` runs on that thread, so it should
    /// hand events on (to the UI thread, say) rather than do much itself.
    pub fn start(on_event: impl Fn(Event) + Send + Sync + 'static) -> Result<Watcher> {
        if ON_EVENT.set(Box::new(on_event)).is_err() {
            return Err(Error::Other { message: "the watcher is already running" });
        }
        let (created, window) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("ferrotype-watcher".into())
            .spawn(move || match create_window() {
                Ok(window) => {
                    let _ = created.send(Ok(window));
                    pump_messages();
                }
                Err(error) => {
                    let _ = created.send(Err(error));
                }
            })
            .map_err(|_| Error::Other { message: "couldn't start the watcher thread" })?;
        let window = window.recv().map_err(|_| Error::Other { message: "the watcher thread ended early" })??;
        Ok(Watcher { window, thread: Some(thread) })
    }

    /// The watcher window's handle: any FerroType window will do to open the clipboard, and
    /// clipboard text it writes is recognised as FerroType's own.
    pub fn window(&self) -> isize {
        self.window
    }

    /// Replaces the registered hotkeys with these; one result per hotkey, in order. A hotkey
    /// another program already registered fails with `ERROR_HOTKEY_ALREADY_REGISTERED`.
    pub fn set_hotkeys(&self, hotkeys: &[HotkeySpec]) -> Vec<Result<()>> {
        let mut request = HotkeyRequest { wanted: hotkeys.to_vec(), results: Vec::new() };
        let pointer = (&raw mut request).expose_provenance() as LPARAM;
        // SAFETY: SendMessageW doesn't return until the watcher thread has handled the message,
        // so `request` outlives every use of the pointer there.
        unsafe { SendMessageW(hwnd(self.window), WM_SET_HOTKEYS, cookie(), pointer) };
        request.results
    }

    /// Starts or stops listening for clipboard changes; copies over `limit` bytes are skipped, and
    /// so are copies marked private if `skip_private`.
    pub fn watch_clipboard(&self, on: bool, limit: usize, skip_private: bool) -> Result<()> {
        CLIPBOARD_LIMIT.store(limit, Ordering::Relaxed);
        SKIP_PRIVATE.store(skip_private, Ordering::Relaxed);
        // SAFETY: no pointers; handled synchronously on the watcher thread.
        let ok = unsafe { SendMessageW(hwnd(self.window), WM_WATCH_CLIPBOARD, usize::from(on), 0) };
        if ok != 0 { Ok(()) } else { Err(Error::Other { message: "couldn't listen for clipboard changes" }) }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // SAFETY: no pointers; the window destroys itself, which ends the thread's message loop.
        unsafe { PostMessageW(hwnd(self.window), WM_CLOSE, 0, 0) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn hwnd(window: isize) -> HWND {
    // An HWND is an opaque handle value, not a pointer to memory we own.
    std::ptr::without_provenance_mut(window as usize)
}

fn create_window() -> Result<isize> {
    let class = wide(CLASS);
    // SAFETY: a null name asks for this program's own module handle.
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let description = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_procedure),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        ..Default::default()
    };
    // SAFETY: `description` is fully initialised and its class name outlives the call.
    if unsafe { RegisterClassExW(&description) } == 0 {
        return Err(Error::last_win32("RegisterClassExW"));
    }
    // SAFETY: a message-only window (parent HWND_MESSAGE) of the class registered above; the
    // class name is NUL-terminated and every other pointer may be null.
    let window = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    };
    if window.is_null() {
        return Err(Error::last_win32("CreateWindowExW"));
    }
    // A second launch can bring this FerroType forward even if this one runs as administrator
    // and that one doesn't. Closing it stays limited to programs at its own level.
    // SAFETY: a window this thread just created; a null CHANGEFILTERSTRUCT is allowed.
    unsafe { ChangeWindowMessageFilterEx(window, show_message(), MSGFLT_ALLOW, std::ptr::null_mut()) };
    Ok(window.addr() as isize)
}

fn pump_messages() {
    let mut message = MSG::default();
    // SAFETY: `message` is a writable MSG; a null window means every message for this thread.
    while unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) } > 0 {
        // SAFETY: `message` was just filled in by GetMessageW.
        unsafe { TranslateMessage(&message) };
        // SAFETY: as above.
        unsafe { DispatchMessageW(&message) };
    }
}

fn emit(event: Event) {
    if let Some(on_event) = ON_EVENT.get() {
        on_event(event);
    }
}

unsafe extern "system" fn window_procedure(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_HOTKEY => {
            emit(Event::Hotkey(wparam as i32));
            0
        }
        WM_CLIPBOARDUPDATE => {
            emit(Event::Clipboard(clipboard::read_copied(
                window.addr() as isize,
                CLIPBOARD_LIMIT.load(Ordering::Relaxed),
                SKIP_PRIVATE.load(Ordering::Relaxed),
            )));
            0
        }
        WM_SET_HOTKEYS if wparam == cookie() => {
            let request: *mut HotkeyRequest = std::ptr::with_exposed_provenance_mut(lparam as usize);
            // SAFETY: only `Watcher::set_hotkeys` sends this message with this process's cookie,
            // with a pointer to a live HotkeyRequest that nothing else touches until SendMessageW
            // returns.
            let request = unsafe { &mut *request };
            request.results = register_hotkeys(window, &request.wanted);
            0
        }
        WM_SET_HOTKEYS => 0,
        WM_WATCH_CLIPBOARD => LRESULT::from(listen_to_clipboard(window, wparam != 0)),
        _ if message != 0 && message == show_message() => {
            emit(Event::ShowWindow);
            0
        }
        _ if message != 0 && message == close_message() => {
            emit(Event::Close);
            0
        }
        WM_CLOSE => {
            // SAFETY: destroys this thread's own window.
            unsafe { DestroyWindow(window) };
            0
        }
        WM_DESTROY => {
            let _ = register_hotkeys(window, &[]);
            listen_to_clipboard(window, false);
            // SAFETY: no pointers; ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            0
        }
        // SAFETY: every other message gets the default handling, with its parameters unchanged.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

fn register_hotkeys(window: HWND, wanted: &[HotkeySpec]) -> Vec<Result<()>> {
    let mut registered = REGISTERED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    for id in registered.drain(..) {
        // SAFETY: no pointers; unregisters an id this window registered.
        unsafe { UnregisterHotKey(window, id) };
    }
    wanted
        .iter()
        .map(|spec| {
            let modifiers = MOD_NOREPEAT
                | if spec.ctrl { MOD_CONTROL } else { 0 }
                | if spec.alt { MOD_ALT } else { 0 }
                | if spec.shift { MOD_SHIFT } else { 0 }
                | if spec.win { MOD_WIN } else { 0 };
            // SAFETY: no pointers; the window belongs to this thread, as RegisterHotKey requires.
            if unsafe { RegisterHotKey(window, spec.id, modifiers, u32::from(spec.vk)) } != 0 {
                registered.push(spec.id);
                Ok(())
            } else {
                Err(Error::last_win32("RegisterHotKey"))
            }
        })
        .collect()
}

/// Returns whether the window is now in the state asked for.
fn listen_to_clipboard(window: HWND, on: bool) -> bool {
    if LISTENING.load(Ordering::Relaxed) == on {
        return true;
    }
    let ok = if on {
        // SAFETY: no pointers; the window belongs to this thread.
        unsafe { AddClipboardFormatListener(window) != 0 }
    } else {
        // SAFETY: as above.
        unsafe { RemoveClipboardFormatListener(window) != 0 }
    };
    if ok {
        LISTENING.store(on, Ordering::Relaxed);
    }
    ok
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// One test, since there's one watcher per process: hotkeys register and come back with
    /// per-hotkey results, and the clipboard listener sees a copy and recognises it as ours.
    ///
    /// The clipboard half only runs with `FERROTYPE_CLIPBOARD_TEST=1` (CI sets it): it briefly
    /// replaces the desktop's shared clipboard, and puts back only text.
    #[test]
    fn registers_hotkeys_and_hears_its_own_copy() {
        let (sender, events) = mpsc::channel();
        let sender = Mutex::new(sender);
        let watcher = Watcher::start(move |event| {
            let _ = sender.lock().unwrap().send(event);
        })
        .unwrap();
        assert!(Watcher::start(|_| {}).is_err(), "one per process");

        // Ctrl+Alt+Shift+F23: nobody else will have that one.
        let spec = HotkeySpec { id: 7, ctrl: true, alt: true, shift: true, win: false, vk: 0x86 };
        let results = watcher.set_hotkeys(&[spec, spec]);
        assert!(results[0].is_ok(), "{:?}", results[0]);
        assert_eq!(results[1].as_ref().unwrap_err().code(), Some(1409), "the same id/combination twice fails");
        assert!(watcher.set_hotkeys(&[]).is_empty());

        if std::env::var_os("FERROTYPE_CLIPBOARD_TEST").is_none() {
            return;
        }
        // The clipboard is shared with the rest of the desktop, so leave it as it was.
        let before = clipboard::get_text(watcher.window()).ok().flatten();
        watcher.watch_clipboard(true, 1024, true).unwrap();
        clipboard::set_text(watcher.window(), "FerroType watcher test").unwrap();
        let heard = events.recv_timeout(Duration::from_secs(5)).expect("no clipboard event");
        match heard {
            Event::Clipboard(Clip::Text(copied)) => {
                assert_eq!(copied.text, "FerroType watcher test");
                assert!(copied.own, "our own copy is recognised");
            }
            other => panic!("unexpected {other:?}"),
        }
        watcher.watch_clipboard(false, 1024, true).unwrap();
        if let Some(before) = before {
            let _ = clipboard::set_text(watcher.window(), &before);
        }
        drop(watcher);
    }
}
