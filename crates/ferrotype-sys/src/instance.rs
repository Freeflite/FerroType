//! One FerroType at a time in a Windows session. A second launch brings the running FerroType's
//! window to the front and exits, so two copies never fight over the hotkeys or overwrite each
//! other's history file. (A copy that doesn't know the history was just locked with a security
//! key would otherwise save over the locked file.)

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, HWND, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{CreateMutexW, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, FindWindowExW, GetWindowThreadProcessId, HWND_MESSAGE, PostMessageW,
};

use crate::system::wide;
use crate::watcher::{self, close_message, show_message};

/// Named per session (`Local\`): each signed-in user gets their own FerroType.
const MUTEX: &str = r"Local\FerroType.Instance";

/// Held for as long as this is the running FerroType; dropping it lets another one start.
pub struct Instance {
    _mutex: Option<OwnedHandle>,
}

/// Claims being the running FerroType: `None` if another one already is.
pub fn claim() -> Option<Instance> {
    let name = wide(MUTEX);
    // SAFETY: the name is NUL-terminated; null attributes give the default security. The mutex
    // is never acquired: that it exists is the signal.
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    let error = std::io::Error::last_os_error().raw_os_error().map(|code| code as u32);
    if mutex.is_null() {
        // A FerroType running as administrator made it, so this one may not open it. For any
        // other failure, running without the guard beats not running at all.
        return (error != Some(ERROR_ACCESS_DENIED)).then_some(Instance { _mutex: None });
    }
    // SAFETY: a fresh handle this process owns; only the OwnedHandle closes it.
    let mutex = unsafe { OwnedHandle::from_raw_handle(mutex) };
    (error != Some(ERROR_ALREADY_EXISTS)).then_some(Instance { _mutex: Some(mutex) })
}

/// Asks the running FerroType to show its window and come to the front; `false` if it couldn't
/// be found (it gets a few seconds, in case it's still starting).
pub fn show_running() -> bool {
    let Some(window) = find_running(Duration::from_secs(5)) else { return false };
    if let Some(process) = process_of(window) {
        // This launch came from the user, so it may bring a window to the front. The running
        // copy may not by itself, so pass that on, or its window could stay behind others.
        // SAFETY: no pointers; it fails harmlessly if this process may not.
        unsafe { AllowSetForegroundWindow(process) };
    }
    // SAFETY: posting a registered message to a window handle has no memory-safety preconditions.
    unsafe { PostMessageW(window, show_message(), 0, 0) != 0 }
}

/// Asks a running FerroType to close (it saves first) and waits up to `timeout` for it to exit;
/// `true` once none is running.
pub fn close_running(timeout: Duration) -> bool {
    let Some(window) = find_running(Duration::ZERO) else { return true };
    let Some(process) = process_of(window) else { return false };
    let Some(handle) = open_to_wait(process) else { return true };
    // SAFETY: posting a registered message to a window handle has no memory-safety preconditions.
    if unsafe { PostMessageW(window, close_message(), 0, 0) } == 0 {
        return false;
    }
    wait(&handle, timeout)
}

/// Waits up to `timeout` for a process to exit; `true` if it has (or was already gone).
pub(crate) fn wait_for_exit(process: u32, timeout: Duration) -> bool {
    open_to_wait(process).is_none_or(|handle| wait(&handle, timeout))
}

/// The running FerroType's watcher window, waiting up to `patience` for one to appear.
fn find_running(patience: Duration) -> Option<HWND> {
    let class = wide(watcher::CLASS);
    let deadline = Instant::now() + patience;
    loop {
        // SAFETY: the class name is NUL-terminated; HWND_MESSAGE searches message-only windows,
        // and a null title matches any.
        let window = unsafe { FindWindowExW(HWND_MESSAGE, std::ptr::null_mut(), class.as_ptr(), std::ptr::null()) };
        if !window.is_null() {
            return Some(window);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn process_of(window: HWND) -> Option<u32> {
    let mut process = 0u32;
    // SAFETY: a plain query of a window handle; `process` is a valid out pointer.
    unsafe { GetWindowThreadProcessId(window, &mut process) };
    (process != 0).then_some(process)
}

/// A handle to wait on a process with; `None` if it has already exited.
fn open_to_wait(process: u32) -> Option<OwnedHandle> {
    // SAFETY: no pointers; asks only for the right to wait on the process.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, process) };
    if handle.is_null() {
        return None;
    }
    // SAFETY: a fresh handle this function owns; only the OwnedHandle closes it.
    Some(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn wait(process: &OwnedHandle, timeout: Duration) -> bool {
    let milliseconds = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
    // SAFETY: a live process handle opened with SYNCHRONIZE.
    unsafe { WaitForSingleObject(process.as_raw_handle(), milliseconds) == WAIT_OBJECT_0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_that_has_exited_is_done_waiting() {
        let mut child = std::process::Command::new(r"C:\Windows\System32\cmd.exe")
            .args(["/d", "/c", "exit"])
            .spawn()
            .expect("cmd starts");
        let id = child.id();
        let _ = child.wait();
        assert!(wait_for_exit(id, Duration::from_secs(5)));
    }
}
