//! Installing FerroType for the current Windows account, from its own exe: a copy in
//! `%LOCALAPPDATA%\Programs\FerroType`, a Start menu shortcut, and an entry in Settings → Apps
//! (so it can be uninstalled from there too). It all goes in the account's own folders and
//! registry, so no administrator rights are needed.
//!
//! Settings and history aren't part of it: they live in `%APPDATA%\FerroType` wherever FerroType
//! runs from, and uninstalling keeps them.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, GENERIC_READ, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_DELETE_ON_CLOSE, FILE_SHARE_READ, OPEN_EXISTING};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_DWORD, REG_SZ, RRF_RT_REG_SZ, RegDeleteTreeW, RegGetValueW, RegSetKeyValueW,
};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_Programs, FOLDERID_UserProgramFiles, KF_FLAG_CREATE, SHGetKnownFolderPath,
};
use windows_sys::core::GUID;

use crate::system::wide;
use crate::{Error, Result};

const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\FerroType";
const REPOSITORY: &str = "https://github.com/Freeflite/FerroType";
/// The argument the uninstall helper (see `uninstall`) is started with.
pub const FINISH_UNINSTALL: &str = "--finish-uninstall";

/// What uninstalling did with the installed copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removed {
    /// It's gone.
    Now,
    /// It's this very process, which Windows won't delete while it runs: it goes once this
    /// process exits, so exit promptly.
    AfterExit,
}

/// Where the installed copy goes (or is): `%LOCALAPPDATA%\Programs\FerroType\ferrotype.exe`.
pub fn installed_exe() -> Result<PathBuf> {
    Ok(known_folder(&FOLDERID_UserProgramFiles)?.join("FerroType").join("ferrotype.exe"))
}

/// The installed copy's version, from its entry in Settings → Apps.
pub fn installed_version() -> Option<String> {
    string_value("DisplayVersion")
}

/// Whether `a` and `b` are the same file.
pub fn same_file(a: &Path, b: &Path) -> bool {
    matches!((std::fs::canonicalize(a), std::fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

/// Whether `a` and `b` are the same build: the same file, or the same bytes.
pub fn same_build(a: &Path, b: &Path) -> bool {
    if same_file(a, b) {
        return true;
    }
    let size = |path: &Path| std::fs::metadata(path).map(|m| m.len()).ok();
    size(a).is_some() && size(a) == size(b) && std::fs::read(a).ok() == std::fs::read(b).ok()
}

/// Installs `exe` (or updates the installed copy to it): copies it into place, then adds the
/// Start menu shortcut and the Apps entry. Returns the installed copy's path.
pub fn install(exe: &Path) -> Result<PathBuf> {
    let copy = installed_exe()?;
    let folder = copy.parent().ok_or(Error::Other { message: "the install folder has no parent" })?;
    std::fs::create_dir_all(folder).map_err(io("CreateDirectoryW"))?;
    // Written next to it, then renamed into place: never half-written, and if the installed copy
    // is running the rename fails and it's left as it was.
    let bytes = std::fs::read(exe).map_err(io("ReadFile"))?;
    let partial = copy.with_extension("exe.partial");
    std::fs::write(&partial, bytes).map_err(io("WriteFile"))?;
    if let Err(error) = std::fs::rename(&partial, &copy) {
        let _ = std::fs::remove_file(&partial);
        return Err(io("MoveFileExW")(error));
    }
    crate::shortcut::create(&shortcut_path()?, &copy, "FerroType, the clipboard typing utility")?;

    let size_kb = std::fs::metadata(&copy).map_or(0, |m| m.len() / 1024);
    set_string("DisplayName", "FerroType")?;
    set_string("DisplayVersion", env!("CARGO_PKG_VERSION"))?;
    set_string("DisplayIcon", &format!("{},0", copy.display()))?;
    set_string("InstallLocation", &folder.to_string_lossy())?;
    set_string("UninstallString", &format!("\"{}\" --uninstall", copy.display()))?;
    set_string("URLInfoAbout", REPOSITORY)?;
    set_dword("EstimatedSize", u32::try_from(size_kb).unwrap_or(u32::MAX))?;
    set_dword("NoModify", 1)?;
    set_dword("NoRepair", 1)?;
    Ok(copy)
}

/// Removes the installed copy, the Start menu shortcut and the Apps entry. The copy goes first,
/// so if it can't (it's running in another session, say), everything stays installed.
pub fn uninstall() -> Result<Removed> {
    let copy = installed_exe()?;
    let own = std::env::current_exe().map_err(io("GetModuleFileNameW"))?;
    let removed = if same_file(&own, &copy) {
        delete_after_exit(&own)?;
        Removed::AfterExit
    } else {
        remove_copy(&copy)?;
        Removed::Now
    };
    if let Ok(link) = shortcut_path() {
        let _ = std::fs::remove_file(link);
    }
    let key = wide(UNINSTALL_KEY);
    // SAFETY: the key name is NUL-terminated.
    let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key.as_ptr()) };
    if !matches!(status, ERROR_SUCCESS | ERROR_FILE_NOT_FOUND) {
        return Err(Error::Win32 { call: "RegDeleteTreeW", code: status });
    }
    Ok(removed)
}

/// The uninstall helper's part (started with `FINISH_UNINSTALL` and the id of the FerroType that
/// started it): waits for that FerroType to exit, then deletes the installed copy and its folder.
pub fn finish_uninstall(uninstalling: u32) {
    crate::instance::wait_for_exit(uninstalling, Duration::from_secs(60));
    let Ok(copy) = installed_exe() else { return };
    // Windows can take a moment to let go of an exe after its process has ended.
    let deadline = Instant::now() + Duration::from_secs(10);
    while remove_copy(&copy).is_err() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Deletes the installed copy, and its folder if that's then empty.
fn remove_copy(copy: &Path) -> Result<()> {
    match std::fs::remove_file(copy) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io("DeleteFileW")(error)),
    }
    if let Some(folder) = copy.parent() {
        let _ = std::fs::remove_dir(folder);
    }
    Ok(())
}

/// Starts a helper that deletes the installed copy (this process's own exe) once this process
/// has exited: a copy of the exe in the temp folder, run with `FINISH_UNINSTALL`. The helper's
/// file is opened here with `FILE_FLAG_DELETE_ON_CLOSE` through a handle the helper inherits, so
/// Windows deletes it as well when the helper exits (the way rustup removes itself).
fn delete_after_exit(own: &Path) -> Result<()> {
    let helper = std::env::temp_dir().join(format!("ferrotype-uninstall-{}.exe", std::process::id()));
    let bytes = std::fs::read(own).map_err(io("ReadFile"))?;
    std::fs::write(&helper, bytes).map_err(io("WriteFile"))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let name = wide(&helper.to_string_lossy());
    // SAFETY: the name is NUL-terminated and `attributes` is fully initialised; no template file.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            &attributes,
            OPEN_EXISTING,
            FILE_FLAG_DELETE_ON_CLOSE,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = Error::last_win32("CreateFileW");
        let _ = std::fs::remove_file(&helper);
        return Err(error);
    }
    // SAFETY: a fresh handle this function owns. Closing it after the helper has started leaves
    // the helper's inherited copy, so the file goes when the helper ends (or now, if it didn't
    // start).
    let _delete_on_close = unsafe { OwnedHandle::from_raw_handle(handle) };
    Command::new(&helper)
        .arg(FINISH_UNINSTALL)
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(io("CreateProcessW"))?;
    Ok(())
}

/// The current user's Start menu shortcut.
fn shortcut_path() -> Result<PathBuf> {
    Ok(known_folder(&FOLDERID_Programs)?.join("FerroType.lnk"))
}

/// A known folder of the current user's, created if it doesn't exist yet.
fn known_folder(id: &GUID) -> Result<PathBuf> {
    let mut path: *mut u16 = std::ptr::null_mut();
    // SAFETY: `id` is a valid folder GUID and `path` a valid out pointer; a null token means the
    // current user.
    let result = unsafe { SHGetKnownFolderPath(id, KF_FLAG_CREATE as u32, std::ptr::null_mut(), &mut path) };
    let folder = if result >= 0 && !path.is_null() {
        // SAFETY: on success the shell returns a NUL-terminated path.
        Ok(PathBuf::from(unsafe { from_wide_ptr(path) }))
    } else {
        Err(Error::HResult { call: "SHGetKnownFolderPath", hresult: result })
    };
    // SAFETY: frees the string the shell allocated (freeing null is allowed).
    unsafe { CoTaskMemFree(path.cast_const().cast()) };
    folder
}

/// Copies a NUL-terminated UTF-16 string.
///
/// # Safety
/// `ptr` must point at a readable, NUL-terminated UTF-16 string.
unsafe fn from_wide_ptr(ptr: *const u16) -> OsString {
    let mut len = 0usize;
    loop {
        // SAFETY: every unit up to the terminator is readable, so `len` is in bounds.
        let at = unsafe { ptr.add(len) };
        // SAFETY: as above.
        if unsafe { *at } == 0 {
            break;
        }
        len += 1;
    }
    // SAFETY: the `len` units before the terminator are readable.
    OsString::from_wide(unsafe { std::slice::from_raw_parts(ptr, len) })
}

fn string_value(name: &str) -> Option<String> {
    let (key, name) = (wide(UNINSTALL_KEY), wide(name));
    let mut data = vec![0u16; 256];
    let mut size = (data.len() * 2) as u32;
    // SAFETY: both strings are NUL-terminated, `data` is writable for `size` bytes, and a null
    // type pointer is permitted. RRF_RT_REG_SZ guarantees a NUL-terminated result.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let units = (size as usize / 2).min(data.len());
    let end = data[..units].iter().position(|&unit| unit == 0).unwrap_or(units);
    Some(String::from_utf16_lossy(&data[..end]))
}

fn set_string(name: &str, value: &str) -> Result<()> {
    let (key, name, data) = (wide(UNINSTALL_KEY), wide(name), wide(value));
    let bytes = u32::try_from(data.len() * 2).map_err(|_| Error::Other { message: "registry value too long" })?;
    // SAFETY: the key and value names are NUL-terminated and `data` is a NUL-terminated REG_SZ of
    // `bytes` bytes. The key is created if it isn't there.
    let status =
        unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), REG_SZ, data.as_ptr().cast(), bytes) };
    if status == ERROR_SUCCESS { Ok(()) } else { Err(Error::Win32 { call: "RegSetKeyValueW", code: status }) }
}

fn set_dword(name: &str, value: u32) -> Result<()> {
    let (key, name) = (wide(UNINSTALL_KEY), wide(name));
    // SAFETY: as above, with a 4-byte DWORD.
    let status = unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), REG_DWORD, (&raw const value).cast(), 4)
    };
    if status == ERROR_SUCCESS { Ok(()) } else { Err(Error::Win32 { call: "RegSetKeyValueW", code: status }) }
}

/// Turns an I/O error into a Win32 one, naming the call.
fn io(call: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| Error::Win32 { call, code: error.raw_os_error().map_or(0, |code| code as u32) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_into_the_users_programs_folder() {
        let exe = installed_exe().expect("known folder");
        assert!(exe.ends_with(r"Programs\FerroType\ferrotype.exe"), "{}", exe.display());
        assert!(shortcut_path().expect("known folder").ends_with(r"Start Menu\Programs\FerroType.lnk"));
    }

    #[test]
    fn tells_builds_apart() {
        let dir = std::env::temp_dir().join(format!("ferrotype-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let (a, b, c) = (dir.join("a.exe"), dir.join("b.exe"), dir.join("c.exe"));
        std::fs::write(&a, b"one build").expect("write");
        std::fs::write(&b, b"one build").expect("write");
        std::fs::write(&c, b"another!!").expect("write");
        assert!(same_build(&a, &a) && same_file(&a, &a));
        assert!(same_build(&a, &b) && !same_file(&a, &b));
        assert!(!same_build(&a, &c));
        assert!(!same_build(&a, &dir.join("missing.exe")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
