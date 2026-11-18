//! Encryption tied to the signed-in Windows account (DPAPI), for the saved history: the file can
//! only be read back by the same user on the same Windows installation (or their roaming
//! profile), never by another account or after being copied elsewhere.

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};

use crate::{Error, Result};

/// Encrypts `data`. `entropy` is mixed in and must be passed again to decrypt.
pub fn protect(data: &[u8], entropy: &[u8]) -> Result<Vec<u8>> {
    let (input, entropy) = (blob(data), blob(entropy));
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input` and `entropy` describe live slices (the API only reads them), the
    // description, reserved and prompt pointers may be null, and `output` is written by the call.
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(Error::last_win32("CryptProtectData"));
    }
    Ok(take(output))
}

/// Decrypts what [`protect`] produced; fails for data from another user, damaged data or the
/// wrong `entropy`.
pub fn unprotect(data: &[u8], entropy: &[u8]) -> Result<Vec<u8>> {
    let (input, entropy) = (blob(data), blob(entropy));
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as in `protect`; a null description pointer means we don't want it back.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(Error::last_win32("CryptUnprotectData"));
    }
    Ok(take(output))
}

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    // The API takes a mutable pointer but only reads input blobs.
    CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr().cast_mut() }
}

/// Copies out a blob the API allocated, and frees it.
fn take(blob: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if blob.pbData.is_null() {
        return Vec::new();
    }
    // SAFETY: on success the API returns `cbData` bytes at `pbData`, valid until LocalFree.
    let bytes = unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize) }.to_vec();
    // SAFETY: the API allocated `pbData` with LocalAlloc and hands ownership to the caller.
    unsafe { LocalFree(blob.pbData.cast()) };
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_tampering() {
        let secret = "pinned text 🦀".as_bytes();
        let sealed = protect(secret, b"FerroType test").unwrap();
        assert!(!sealed.windows(secret.len()).any(|w| w == secret), "encrypted");
        assert_eq!(unprotect(&sealed, b"FerroType test").unwrap(), secret);
        assert!(unprotect(&sealed, b"other entropy").is_err());
        let mut damaged = sealed.clone();
        let last = damaged.len() - 1;
        damaged[last] ^= 0xFF;
        assert!(unprotect(&damaged, b"FerroType test").is_err());
        assert!(unprotect(b"not dpapi", b"FerroType test").is_err());
        assert_eq!(unprotect(&protect(b"", b"e").unwrap(), b"e").unwrap(), b"");
    }
}
