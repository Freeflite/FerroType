//! Typing with real keystrokes (`SendInput`), and what typing needs to know about windows.
//!
//! Each character is pressed with the key that produces it on the target window's keyboard
//! layout (plus Shift when needed), like a person typing. Many tools send every character as a
//! "Unicode packet" instead, which some programs and remote sessions ignore. Only characters
//! with no plain or Shift key on the layout fall back to a Unicode packet; keys that need Ctrl or
//! Alt (AltGr) are never pressed, since those could fire shortcuts in the target.

use std::num::NonZeroIsize;
use std::time::Duration;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, GetKeyboardLayout, HKL, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS,
    KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, MapVirtualKeyExW, SendInput, ToUnicodeEx,
    VK_CAPITAL, VK_CONTROL, VK_LWIN, VK_MENU, VK_RETURN, VK_RWIN, VK_SHIFT, VK_SPACE, VK_TAB, VkKeyScanExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetForegroundWindow, GetWindowThreadProcessId,
};

use crate::{Error, Result};

/// A top-level window, by its handle value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Window(NonZeroIsize);

impl Window {
    pub fn from_handle(handle: isize) -> Option<Window> {
        NonZeroIsize::new(handle).map(Window)
    }

    pub fn handle(self) -> isize {
        self.0.get()
    }

    fn hwnd(self) -> HWND {
        // An HWND is an opaque handle value, not a pointer to memory we own.
        std::ptr::without_provenance_mut(self.0.get() as usize)
    }

    /// The process and thread that created the window, or `None` if it's gone.
    fn owner(self) -> Option<(u32, u32)> {
        let mut process = 0;
        // SAFETY: `process` is a writable u32; the call validates the handle and returns 0 for a
        // window that doesn't exist.
        let thread = unsafe { GetWindowThreadProcessId(self.hwnd(), &mut process) };
        (thread != 0).then_some((process, thread))
    }
}

/// A keyboard layout (an `HKL` handle value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout(isize);

impl Layout {
    fn hkl(self) -> HKL {
        // An HKL is an opaque handle value, not a pointer.
        std::ptr::without_provenance_mut(self.0 as usize)
    }
}

/// The top-level window that has the keyboard focus, if Windows will say (it won't while the
/// focus is changing, or on the secure desktop).
pub fn foreground_window() -> Option<Window> {
    // SAFETY: no arguments.
    let focused = unsafe { GetForegroundWindow() };
    if focused.is_null() {
        return None;
    }
    // SAFETY: GetAncestor validates the handle and returns null for one that's gone.
    let root = unsafe { GetAncestor(focused, GA_ROOT) };
    Window::from_handle(if root.is_null() { focused } else { root }.addr() as isize)
}

/// Whether the window belongs to this process.
pub fn is_own_window(window: Window) -> bool {
    window.owner().is_some_and(|(process, _)| process == std::process::id())
}

/// The keyboard layout of the window's thread (this thread's if there's no window).
pub fn keyboard_layout(window: Option<Window>) -> Layout {
    let thread = window.and_then(Window::owner).map_or(0, |(_, thread)| thread);
    // SAFETY: no pointers; thread 0 means the calling thread.
    Layout(unsafe { GetKeyboardLayout(thread) }.addr() as isize)
}

/// Whether Ctrl, Alt, Shift or a Windows key is physically or logically down right now.
pub fn modifiers_held() -> bool {
    [VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN].into_iter().any(|vk| {
        // SAFETY: no pointers; any virtual-key code is accepted.
        let state = unsafe { GetAsyncKeyState(i32::from(vk)) };
        state as u16 & 0x8000 != 0
    })
}

fn caps_lock_on() -> bool {
    // SAFETY: no pointers; any virtual-key code is accepted.
    let state = unsafe { GetKeyState(i32::from(VK_CAPITAL)) };
    state & 1 != 0
}

/// Types one character, holding its key down for `hold`. Newlines press Enter.
pub fn type_char(c: char, layout: Layout, hold: Duration) -> Result<()> {
    match c {
        '\n' => press(VK_RETURN, layout, hold, false),
        '\t' => press(VK_TAB, layout, hold, false),
        ' ' => press(VK_SPACE, layout, hold, false),
        _ => match find_key(c, layout) {
            Some((vk, shift)) => press(vk, layout, hold, shift),
            None => type_unicode(c),
        },
    }
}

/// The key (and whether Shift goes with it) that types `c` on `layout`, if there's one that
/// needs neither Ctrl nor Alt.
pub fn find_key(c: char, layout: Layout) -> Option<(u16, bool)> {
    let unit = u16::try_from(u32::from(c)).ok()?;
    // SAFETY: no pointers; an unknown layout handle just yields "no key" (-1).
    let found = unsafe { VkKeyScanExW(unit, layout.hkl()) };
    // -1: no key at all. 0x0200 / 0x0400: the key needs Ctrl / Alt.
    if found == -1 || found & 0x0600 != 0 {
        return None;
    }
    let vk = (found & 0xFF) as u16;
    let wants_shift = found & 0x0100 != 0;
    // Check the key really types `c` (allowing for Caps Lock, and rejecting dead keys).
    let caps = caps_lock_on();
    let wanted = c.to_string();
    [wants_shift, !wants_shift]
        .into_iter()
        .find(|&shift| key_output(vk, shift, caps, layout).as_deref() == Some(wanted.as_str()))
        .map(|shift| (vk, shift))
}

/// The text a key produces with the given Shift and Caps Lock state, or `None` for a dead key or
/// a key that types nothing.
fn key_output(vk: u16, shift: bool, caps: bool, layout: Layout) -> Option<String> {
    const NO_STATE_CHANGE: u32 = 0x4;
    let mut state = [0u8; 256];
    if shift {
        state[usize::from(VK_SHIFT)] = 0x80;
    }
    if caps {
        state[usize::from(VK_CAPITAL)] = 0x01;
    }
    let mut buffer = [0u16; 8];
    let scan = scan_code(vk, layout);
    // SAFETY: `state` is the 256-byte key state array ToUnicodeEx reads, `buffer`'s length is
    // passed, and NO_STATE_CHANGE keeps the call from disturbing the keyboard's dead-key state.
    let count = unsafe {
        ToUnicodeEx(
            u32::from(vk),
            u32::from(scan),
            state.as_ptr(),
            buffer.as_mut_ptr(),
            buffer.len() as i32,
            NO_STATE_CHANGE,
            layout.hkl(),
        )
    };
    let count = usize::try_from(count).ok().filter(|&n| n > 0)?;
    String::from_utf16(&buffer[..count.min(buffer.len())]).ok()
}

fn scan_code(vk: u16, layout: Layout) -> u16 {
    // SAFETY: no pointers; an unknown key or layout yields 0.
    unsafe { MapVirtualKeyExW(u32::from(vk), MAPVK_VK_TO_VSC, layout.hkl()) as u16 }
}

/// Presses and releases one key, with Shift held around it if asked. Shift is released even if
/// the key itself was refused.
fn press(vk: u16, layout: Layout, hold: Duration, shift: bool) -> Result<()> {
    let scan = scan_code(vk, layout);
    let shift_scan = scan_code(VK_SHIFT, layout);
    if shift {
        send(&[key(VK_SHIFT, shift_scan, 0)])?;
    }
    let pressed = send(&[key(vk, scan, 0)]).and_then(|()| {
        std::thread::sleep(hold);
        send(&[key(vk, scan, KEYEVENTF_KEYUP)])
    });
    if shift {
        let released = send(&[key(VK_SHIFT, shift_scan, KEYEVENTF_KEYUP)]);
        pressed.and(released)
    } else {
        pressed
    }
}

/// Types a character that has no plain or Shift key on the layout (an emoji, say).
fn type_unicode(c: char) -> Result<()> {
    let mut units = [0u16; 2];
    let events: Vec<INPUT> = c
        .encode_utf16(&mut units)
        .iter()
        .flat_map(|&unit| [key(0, unit, KEYEVENTF_UNICODE), key(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)])
        .collect();
    send(&events)
}

fn key(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

/// Sends the events in one call, so nothing else's input lands in between them.
fn send(events: &[INPUT]) -> Result<()> {
    // SAFETY: `events` is a live slice of INPUT structures; its length and the structure's size
    // are passed.
    let sent = unsafe { SendInput(events.len() as u32, events.as_ptr(), size_of::<INPUT>() as i32) };
    if sent as usize == events.len() { Ok(()) } else { Err(Error::last_win32("SendInput")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_found_on_the_current_layout_type_what_they_should() {
        let layout = keyboard_layout(None);
        let mut found = 0;
        for c in (0x21u8..0x7F).map(char::from) {
            if let Some((vk, shift)) = find_key(c, layout) {
                found += 1;
                assert_eq!(key_output(vk, shift, caps_lock_on(), layout).as_deref(), Some(&*c.to_string()), "{c}");
            }
        }
        // Every layout has keys for most of printable ASCII (non-Latin ones too, for digits).
        assert!(found >= 10, "only {found} ASCII characters have keys");
        assert_eq!(find_key(char::from_u32(0x1F980).unwrap(), layout), None, "no key types a crab");
    }

    #[test]
    fn own_window_checks_dont_panic() {
        assert!(Window::from_handle(0).is_none());
        let bogus = Window::from_handle(0x1234_5678).unwrap();
        assert!(!is_own_window(bogus));
        let _ = keyboard_layout(Some(bogus));
        let _ = foreground_window().map(is_own_window);
        let _ = modifiers_held();
    }
}
