use std::fmt;

/// A failed OS call: which API failed and the error it returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A Win32 call failed; `code` is the `GetLastError` value.
    Win32 { call: &'static str, code: u32 },
    /// A COM-style call returned a failing HRESULT.
    HResult { call: &'static str, hresult: i32 },
    /// Something else went wrong; the message says what.
    Other { message: &'static str },
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn last_win32(call: &'static str) -> Self {
        let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        Error::Win32 { call, code: code as u32 }
    }

    /// The Win32 error code, if this is a Win32 failure.
    pub fn code(&self) -> Option<u32> {
        match self {
            Error::Win32 { code, .. } => Some(*code),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::Win32 { call, code } => {
                write!(f, "{call} failed: {}", std::io::Error::from_raw_os_error(code as i32))
            }
            Error::HResult { call, hresult } => write!(f, "{call} failed (HRESULT 0x{:08X})", hresult as u32),
            Error::Other { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}
