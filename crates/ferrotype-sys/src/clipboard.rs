//! Clipboard text: what FerroType may record after a copy, and plain reading and writing.
//!
//! Only text is ever taken; copied files, images and other data are left alone. Copies marked
//! private the way password managers mark passwords can be skipped too (a setting), going by
//! either of these marks:
//!
//! - `ExcludeClipboardContentFromMonitorProcessing`: present means "clipboard tools, don't
//!   record this";
//! - `Clipboard Viewer Ignore`: the older convention for the same thing.
//!
//! `CanIncludeInClipboardHistory = 0` is deliberately *not* treated as private. It only means
//! "keep this out of Windows' clipboard history", and Remote Desktop and Hyper-V's VM Connect put
//! it on every copy they bring out of a session, so honouring it would leave out everything copied
//! in a remote session or VM, which is most of what FerroType is for.

use std::sync::OnceLock;
use std::time::Duration;

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardOwner, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

use crate::system::wide;
use crate::{Error, Result};

const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;

/// What the clipboard held after a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clip {
    /// Text FerroType may record.
    Text(Copied),
    /// Files, an image or other non-text data.
    NotText,
    /// Marked private (by a password manager, say), and the caller asked to skip those.
    Private,
    /// Bigger than the limit asked for.
    TooBig,
    /// Only whitespace.
    Empty,
    /// Another program kept the clipboard open.
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copied {
    pub text: String,
    /// FerroType copied it itself.
    pub own: bool,
}

/// The clipboard formats that mark a copy private, registered once.
struct PrivateFormats {
    exclude: u32,
    viewer_ignore: u32,
}

fn private_formats() -> &'static PrivateFormats {
    static FORMATS: OnceLock<PrivateFormats> = OnceLock::new();
    FORMATS.get_or_init(|| {
        let register = |name: &str| {
            let name = wide(name);
            // SAFETY: `name` is NUL-terminated; registering an existing name returns its id.
            unsafe { RegisterClipboardFormatW(name.as_ptr()) }
        };
        PrivateFormats {
            exclude: register("ExcludeClipboardContentFromMonitorProcessing"),
            viewer_ignore: register("Clipboard Viewer Ignore"),
        }
    })
}

/// Whether the clipboard holds a copy marked private.
fn marked_private() -> bool {
    let formats = private_formats();
    available(formats.exclude) || available(formats.viewer_ignore)
}

fn available(format: u32) -> bool {
    // SAFETY: no pointers; works without opening the clipboard.
    format != 0 && unsafe { IsClipboardFormatAvailable(format) } != 0
}

enum Text {
    Missing,
    TooBig,
    Units(Vec<u16>),
}

/// The clipboard, opened; closed again when dropped.
struct Opened;

impl Opened {
    /// Opens the clipboard for `window` (which becomes its owner if something is written),
    /// retrying briefly: the program that just copied may still have it open.
    fn new(window: HWND) -> Option<Opened> {
        for _ in 0..20 {
            // SAFETY: opening with a window handle (or null) is always allowed; it fails while
            // another program has the clipboard open.
            if unsafe { OpenClipboard(window) } != 0 {
                return Some(Opened);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    /// The UTF-16 text in a clipboard format, up to its first NUL.
    fn text(&self, format: u32, max_units: usize) -> Text {
        // SAFETY: the clipboard is open (we hold `self`); the handle stays owned by the clipboard.
        let handle = unsafe { GetClipboardData(format) };
        if handle.is_null() {
            return Text::Missing;
        }
        let Some(locked) = Locked::new(handle) else { return Text::Missing };
        let units: Vec<u16> = locked
            .bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .take_while(|&unit| unit != 0)
            .take(max_units.saturating_add(1))
            .collect();
        if units.len() > max_units { Text::TooBig } else { Text::Units(units) }
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        // SAFETY: this thread opened the clipboard (that's what an `Opened` stands for).
        unsafe { CloseClipboard() };
    }
}

/// A locked global memory block, unlocked when dropped.
struct Locked {
    handle: HANDLE,
    data: *const u8,
    size: usize,
}

impl Locked {
    fn new(handle: HANDLE) -> Option<Locked> {
        // SAFETY: `handle` is a global memory handle the clipboard handed out; GlobalSize returns 0
        // for an invalid one.
        let size = unsafe { GlobalSize(handle) };
        // SAFETY: as above; GlobalLock returns null for an invalid or discarded handle.
        let data = unsafe { GlobalLock(handle) };
        if data.is_null() { None } else { Some(Locked { handle, data: data.cast(), size }) }
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: GlobalLock returned `data` for a block GlobalSize says is `size` bytes long, and
        // it stays locked, and so valid, until `self` is dropped.
        unsafe { std::slice::from_raw_parts(self.data, self.size) }
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        // SAFETY: undoes the GlobalLock in `new`.
        unsafe { GlobalUnlock(self.handle) };
    }
}

/// What a clipboard change left there, for the history. `window` is the caller's window (any
/// window of this process will do); `max_bytes` caps the text's UTF-8 size. With `skip_private`,
/// a copy marked private isn't read at all.
pub fn read_copied(window: isize, max_bytes: usize, skip_private: bool) -> Clip {
    if skip_private && marked_private() {
        return Clip::Private;
    }
    if available(CF_HDROP) || !available(CF_UNICODETEXT) {
        return Clip::NotText;
    }
    let process = clipboard_owner();
    let Some(opened) = Opened::new(hwnd(window)) else { return Clip::Busy };
    // Every UTF-16 unit is at least one UTF-8 byte, so more units than `max_bytes` is too big.
    let units = match opened.text(CF_UNICODETEXT, max_bytes) {
        Text::Missing => return Clip::NotText,
        Text::TooBig => return Clip::TooBig,
        Text::Units(units) => units,
    };
    drop(opened);
    let text = String::from_utf16_lossy(&units);
    if text.len() > max_bytes {
        return Clip::TooBig;
    }
    if text.trim().is_empty() {
        return Clip::Empty;
    }
    Clip::Text(Copied { text, own: process == Some(std::process::id()) })
}

/// The clipboard's text, for pasting into FerroType's editor (no privacy checks: the user asked).
/// `None` if there's no text.
pub fn get_text(window: isize) -> Result<Option<String>> {
    const LIMIT: usize = 64 * 1024 * 1024;
    if !available(CF_UNICODETEXT) {
        return Ok(None);
    }
    let opened = Opened::new(hwnd(window)).ok_or(Error::Other { message: "another program is using the clipboard" })?;
    match opened.text(CF_UNICODETEXT, LIMIT) {
        Text::Missing => Ok(None),
        Text::TooBig => Err(Error::Other { message: "the clipboard text is too big" }),
        Text::Units(units) => Ok(Some(String::from_utf16_lossy(&units))),
    }
}

/// Puts `text` on the clipboard, owned by `window` (so FerroType recognises its own copy).
pub fn set_text(window: isize, text: &str) -> Result<()> {
    let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let _opened =
        Opened::new(hwnd(window)).ok_or(Error::Other { message: "another program is using the clipboard" })?;
    // SAFETY: the clipboard is open; emptying it makes `window` the owner.
    if unsafe { EmptyClipboard() } == 0 {
        return Err(Error::last_win32("EmptyClipboard"));
    }
    // SAFETY: allocates a movable block of the given size (a null handle on failure).
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, units.len() * 2) };
    if memory.is_null() {
        return Err(Error::last_win32("GlobalAlloc"));
    }
    {
        let Some(locked) = Locked::new(memory) else {
            // SAFETY: frees the block allocated above, which nothing else knows about.
            unsafe { GlobalFree(memory) };
            return Err(Error::last_win32("GlobalLock"));
        };
        // SAFETY: the locked block is at least `units.len() * 2` bytes (GlobalAlloc may round
        // up), the source is a live slice of that many bytes, and the two can't overlap.
        unsafe { std::ptr::copy_nonoverlapping(units.as_ptr().cast::<u8>(), locked.data.cast_mut(), units.len() * 2) };
    }
    // SAFETY: the clipboard is open and emptied by us; on success it takes ownership of `memory`.
    if unsafe { SetClipboardData(CF_UNICODETEXT, memory) }.is_null() {
        let error = Error::last_win32("SetClipboardData");
        // SAFETY: the clipboard didn't take the block, so it's still ours to free.
        unsafe { GlobalFree(memory) };
        return Err(error);
    }
    Ok(())
}

/// The id of the process that owns the clipboard, if Windows will say.
fn clipboard_owner() -> Option<u32> {
    // SAFETY: no arguments; returns null when the clipboard has no owner window.
    let owner = unsafe { GetClipboardOwner() };
    if owner.is_null() {
        return None;
    }
    let mut process = 0;
    // SAFETY: `process` is a writable u32; the handle is validated by the call.
    if unsafe { GetWindowThreadProcessId(owner, &mut process) } == 0 {
        return None;
    }
    Some(process)
}

fn hwnd(window: isize) -> HWND {
    // An HWND is an opaque handle value, not a pointer to memory we own.
    std::ptr::without_provenance_mut(window as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_formats_register() {
        let formats = private_formats();
        assert!(formats.exclude >= 0xC000 && formats.viewer_ignore >= 0xC000);
    }
}
