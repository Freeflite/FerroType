//! Process hardening and a few facts about the system.

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories};
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows_sys::Win32::System::Threading::{
    ProcessExtensionPointDisablePolicy, ProcessImageLoadPolicy, SetProcessMitigationPolicy,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MessageBoxW,
};

/// A NUL-terminated UTF-16 copy of `text`, for Win32 string parameters.
pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Locks down how this process loads code, before anything else loads a DLL: libraries come from
/// System32 only, never from a network share or a low-integrity file, and legacy injection points
/// (AppInit DLLs, Winsock layered providers and the like) are refused. Returns how many of the
/// three measures took; older Windows versions may not support every one.
pub fn harden_process() -> usize {
    const IMAGE_LOAD_NO_REMOTE: u32 = 1 << 0;
    const IMAGE_LOAD_NO_LOW_LABEL: u32 = 1 << 1;
    const IMAGE_LOAD_PREFER_SYSTEM32: u32 = 1 << 2;
    const DISABLE_EXTENSION_POINTS: u32 = 1 << 0;
    let mut applied = 0;
    // SAFETY: no pointers; the flag is a documented LOAD_LIBRARY_SEARCH value.
    if unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) } != 0 {
        applied += 1;
    }
    for (policy, flags) in [
        (ProcessImageLoadPolicy, IMAGE_LOAD_NO_REMOTE | IMAGE_LOAD_NO_LOW_LABEL | IMAGE_LOAD_PREFER_SYSTEM32),
        (ProcessExtensionPointDisablePolicy, DISABLE_EXTENSION_POINTS),
    ] {
        // SAFETY: each policy structure is a single DWORD of flags, passed by pointer with its size.
        if unsafe { SetProcessMitigationPolicy(policy, (&raw const flags).cast(), size_of::<u32>()) } != 0 {
            applied += 1;
        }
    }
    applied
}

/// Whether Windows' app mode is light (Settings > Personalization > Colors); `None` if unknown.
pub fn apps_use_light_theme() -> Option<bool> {
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let value = wide("AppsUseLightTheme");
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: both strings are NUL-terminated, `data` is a writable DWORD whose size is in `size`,
    // and a null type pointer is permitted.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut data).cast(),
            &mut size,
        )
    };
    (status == ERROR_SUCCESS).then_some(data != 0)
}

/// Shows a message box with an OK button, for when there's no window (uninstalling from
/// Settings → Apps).
pub fn message_box(title: &str, text: &str, error: bool) {
    let (title, text) = (wide(title), wide(text));
    let icon = if error { MB_ICONERROR } else { MB_ICONINFORMATION };
    // SAFETY: both strings are NUL-terminated; a null owner is allowed.
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | icon | MB_SETFOREGROUND) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_is_nul_terminated() {
        assert_eq!(wide("ab"), [0x61, 0x62, 0]);
    }

    #[test]
    fn hardening_applies() {
        assert!(harden_process() >= 1);
    }
}
