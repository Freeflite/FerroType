//! Security keys (a YubiKey, say) through Windows' own WebAuthn API, `webauthn.dll`.
//!
//! Since Windows 10 1903 a program without administrator rights can't talk to a security key
//! directly: it asks Windows, which shows its own prompts for the key's PIN and a touch. So
//! FerroType never sees the PIN, and nothing here goes over the network. The "relying party" a
//! credential belongs to is just the label `ferrotype`, and the credential isn't stored on the key
//! (it isn't "discoverable"), so registering uses none of the key's passkey slots.
//!
//! FerroType uses the keys' `hmac-secret` extension (which Windows exposes as PRF): asked with a
//! salt, a key gives back 32 bytes that only it can compute, for that credential and salt, and
//! only after its PIN and a touch. `ferrotype_core::vault` turns those into the key that unlocks
//! the saved history.
//!
//! Every call here blocks until the person finishes or cancels Windows' prompt, so call them off
//! the UI thread.

use std::fmt;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Networking::WindowsWebServices::{
    WEBAUTHN_API_VERSION_4, WEBAUTHN_ASSERTION, WEBAUTHN_ASSERTION_VERSION_4,
    WEBAUTHN_ATTESTATION_CONVEYANCE_PREFERENCE_NONE, WEBAUTHN_AUTHENTICATOR_ATTACHMENT_CROSS_PLATFORM,
    WEBAUTHN_AUTHENTICATOR_GET_ASSERTION_OPTIONS, WEBAUTHN_AUTHENTICATOR_GET_ASSERTION_OPTIONS_VERSION_6,
    WEBAUTHN_AUTHENTICATOR_MAKE_CREDENTIAL_OPTIONS, WEBAUTHN_AUTHENTICATOR_MAKE_CREDENTIAL_OPTIONS_VERSION_3,
    WEBAUTHN_CLIENT_DATA, WEBAUTHN_CLIENT_DATA_CURRENT_VERSION, WEBAUTHN_COSE_ALGORITHM_ECDSA_P256_WITH_SHA256,
    WEBAUTHN_COSE_CREDENTIAL_PARAMETER, WEBAUTHN_COSE_CREDENTIAL_PARAMETER_CURRENT_VERSION,
    WEBAUTHN_COSE_CREDENTIAL_PARAMETERS, WEBAUTHN_CREDENTIAL, WEBAUTHN_CREDENTIAL_ATTESTATION,
    WEBAUTHN_CREDENTIAL_CURRENT_VERSION, WEBAUTHN_CREDENTIAL_TYPE_PUBLIC_KEY, WEBAUTHN_CREDENTIALS, WEBAUTHN_EXTENSION,
    WEBAUTHN_EXTENSIONS, WEBAUTHN_EXTENSIONS_IDENTIFIER_HMAC_SECRET, WEBAUTHN_HASH_ALGORITHM_SHA_256,
    WEBAUTHN_HMAC_SECRET_SALT, WEBAUTHN_HMAC_SECRET_SALT_VALUES, WEBAUTHN_RP_ENTITY_INFORMATION,
    WEBAUTHN_RP_ENTITY_INFORMATION_CURRENT_VERSION, WEBAUTHN_USER_ENTITY_INFORMATION,
    WEBAUTHN_USER_ENTITY_INFORMATION_CURRENT_VERSION, WEBAUTHN_USER_VERIFICATION_REQUIREMENT_REQUIRED,
    WebAuthNAuthenticatorGetAssertion, WebAuthNAuthenticatorMakeCredential, WebAuthNCancelCurrentOperation,
    WebAuthNFreeAssertion, WebAuthNFreeCredentialAttestation, WebAuthNGetApiVersionNumber, WebAuthNGetCancellationId,
    WebAuthNGetErrorName,
};
use windows_sys::core::{BOOL, GUID, HRESULT};
use zeroize::Zeroizing;

use crate::system::wide;

/// The label FerroType's credentials belong to. Changing it orphans every registered key.
const RP_ID: &str = "ferrotype";
/// Asking for an hmac-secret in GetAssertion, and getting it back, arrived in API version 4.
const NEEDED_API: u32 = WEBAUTHN_API_VERSION_4;
/// How long Windows' prompt waits for the PIN and touch.
const TIMEOUT_MS: u32 = 120_000;
/// The length of a secret and of the salt it's computed for.
pub const SECRET_LEN: usize = 32;

/// WebAuthn signs a hash of this "client data"; FerroType never checks signatures (the secret is
/// what matters), so it's the same every time.
const CREATE_JSON: &[u8] = br#"{"type":"webauthn.create","challenge":"RmVycm9UeXBl","origin":"ferrotype"}"#;
const GET_JSON: &[u8] = br#"{"type":"webauthn.get","challenge":"RmVycm9UeXBl","origin":"ferrotype"}"#;
/// The (fixed) user a credential is made for; not stored anywhere, since it isn't discoverable.
const USER_ID: &[u8] = b"ferrotype-history";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// This Windows is too old for security-key secrets.
    Unsupported,
    /// The prompt was cancelled or timed out.
    Cancelled,
    /// That security key is already registered.
    AlreadyRegistered,
    /// The security key can't produce secrets (it has no `hmac-secret`).
    NoSecret,
    /// Anything else Windows reported.
    Failed(String),
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::Unsupported => {
                f.write_str("this version of Windows can't get secrets from security keys (Windows 11 can)")
            }
            KeyError::Cancelled => f.write_str("the security key prompt was cancelled or timed out"),
            KeyError::AlreadyRegistered => f.write_str("that security key is already registered"),
            KeyError::NoSecret => f.write_str("that security key can't do this (it has no hmac-secret support)"),
            KeyError::Failed(why) => write!(f, "Windows reported {why}"),
        }
    }
}

impl std::error::Error for KeyError {}

/// The WebAuthn API version Windows has (0 if none).
pub fn api_version() -> u32 {
    // SAFETY: no arguments.
    unsafe { WebAuthNGetApiVersionNumber() }
}

/// Whether this Windows can get secrets from security keys.
pub fn supported() -> bool {
    api_version() >= NEEDED_API
}

/// Makes a FerroType credential on a security key: Windows asks for the key's PIN and a touch.
/// `exclude` lists credentials already registered, so the same key isn't registered twice.
/// Returns the new credential's id. `window` is the window Windows' prompt belongs to.
pub fn register(window: isize, exclude: &[Vec<u8>]) -> Result<Vec<u8>, KeyError> {
    if !supported() {
        return Err(KeyError::Unsupported);
    }
    let rp_id = wide(RP_ID);
    let rp_name = wide("FerroType");
    let rp = WEBAUTHN_RP_ENTITY_INFORMATION {
        dwVersion: WEBAUTHN_RP_ENTITY_INFORMATION_CURRENT_VERSION,
        pwszId: rp_id.as_ptr(),
        pwszName: rp_name.as_ptr(),
        pwszIcon: std::ptr::null(),
    };
    let mut user_id = USER_ID.to_vec();
    let user_name = wide("FerroType history");
    let user = WEBAUTHN_USER_ENTITY_INFORMATION {
        dwVersion: WEBAUTHN_USER_ENTITY_INFORMATION_CURRENT_VERSION,
        cbId: user_id.len() as u32,
        pbId: user_id.as_mut_ptr(),
        pwszName: user_name.as_ptr(),
        pwszIcon: std::ptr::null(),
        pwszDisplayName: user_name.as_ptr(),
    };
    let mut algorithms = [WEBAUTHN_COSE_CREDENTIAL_PARAMETER {
        dwVersion: WEBAUTHN_COSE_CREDENTIAL_PARAMETER_CURRENT_VERSION,
        pwszCredentialType: WEBAUTHN_CREDENTIAL_TYPE_PUBLIC_KEY,
        lAlg: WEBAUTHN_COSE_ALGORITHM_ECDSA_P256_WITH_SHA256,
    }];
    let parameters = WEBAUTHN_COSE_CREDENTIAL_PARAMETERS {
        cCredentialParameters: algorithms.len() as u32,
        pCredentialParameters: algorithms.as_mut_ptr(),
    };
    let mut json = CREATE_JSON.to_vec();
    let client = client_data(&mut json);
    let mut ids = exclude.to_vec();
    let mut excluded = credential_list(&mut ids);
    let mut hmac_secret: BOOL = 1;
    let mut extensions = [WEBAUTHN_EXTENSION {
        pwszExtensionIdentifier: WEBAUTHN_EXTENSIONS_IDENTIFIER_HMAC_SECRET,
        cbExtension: size_of::<BOOL>() as u32,
        pvExtension: (&raw mut hmac_secret).cast(),
    }];
    let mut prompt = Prompt::begin();
    let options = WEBAUTHN_AUTHENTICATOR_MAKE_CREDENTIAL_OPTIONS {
        dwVersion: WEBAUTHN_AUTHENTICATOR_MAKE_CREDENTIAL_OPTIONS_VERSION_3,
        dwTimeoutMilliseconds: TIMEOUT_MS,
        // For MakeCredential, these are the credentials to refuse.
        CredentialList: WEBAUTHN_CREDENTIALS {
            cCredentials: excluded.len() as u32,
            pCredentials: excluded.as_mut_ptr(),
        },
        Extensions: WEBAUTHN_EXTENSIONS { cExtensions: extensions.len() as u32, pExtensions: extensions.as_mut_ptr() },
        dwAuthenticatorAttachment: WEBAUTHN_AUTHENTICATOR_ATTACHMENT_CROSS_PLATFORM,
        bRequireResidentKey: 0,
        dwUserVerificationRequirement: WEBAUTHN_USER_VERIFICATION_REQUIREMENT_REQUIRED,
        dwAttestationConveyancePreference: WEBAUTHN_ATTESTATION_CONVEYANCE_PREFERENCE_NONE,
        pCancellationId: prompt.id(),
        ..Default::default()
    };
    let mut attestation: *mut WEBAUTHN_CREDENTIAL_ATTESTATION = std::ptr::null_mut();
    // SAFETY: every pointer in these structures points at locals (strings, arrays, the BOOL, the
    // client data JSON, the credential ids) that outlive the call; `attestation` is a writable
    // out-pointer.
    let result = unsafe {
        WebAuthNAuthenticatorMakeCredential(hwnd(window), &rp, &user, &parameters, &client, &options, &mut attestation)
    };
    drop(prompt);
    if result < 0 {
        return Err(failure(result));
    }
    let attestation = Attestation(attestation);
    if attestation.0.is_null() {
        return Err(KeyError::Failed("no credential".to_owned()));
    }
    // SAFETY: a successful call returned a valid attestation, freed only when `attestation` drops.
    let length = unsafe { (*attestation.0).cbCredentialId };
    // SAFETY: as above.
    let id = unsafe { (*attestation.0).pbCredentialId };
    // SAFETY: Windows returned this id and its length together, valid until the attestation is freed.
    let credential = unsafe { copy_bytes(id, length) };
    if credential.is_empty() {
        return Err(KeyError::Failed("an empty credential id".to_owned()));
    }
    Ok(credential)
}

/// Asks a registered security key for its secret for `salt`: Windows asks for the key's PIN and a
/// touch. Returns which credential answered, and the secret.
pub fn secret(
    window: isize,
    credentials: &[Vec<u8>],
    salt: &[u8; SECRET_LEN],
) -> Result<(Vec<u8>, Zeroizing<[u8; SECRET_LEN]>), KeyError> {
    if !supported() {
        return Err(KeyError::Unsupported);
    }
    let rp_id = wide(RP_ID);
    let mut json = GET_JSON.to_vec();
    let client = client_data(&mut json);
    let mut ids = credentials.to_vec();
    let mut allowed = credential_list(&mut ids);
    let mut salt = *salt;
    let mut first = WEBAUTHN_HMAC_SECRET_SALT {
        cbFirst: SECRET_LEN as u32,
        pbFirst: salt.as_mut_ptr(),
        cbSecond: 0,
        pbSecond: std::ptr::null_mut(),
    };
    let mut salts = WEBAUTHN_HMAC_SECRET_SALT_VALUES {
        pGlobalHmacSalt: &raw mut first,
        cCredWithHmacSecretSaltList: 0,
        pCredWithHmacSecretSaltList: std::ptr::null_mut(),
    };
    let mut prompt = Prompt::begin();
    let options = WEBAUTHN_AUTHENTICATOR_GET_ASSERTION_OPTIONS {
        dwVersion: WEBAUTHN_AUTHENTICATOR_GET_ASSERTION_OPTIONS_VERSION_6,
        dwTimeoutMilliseconds: TIMEOUT_MS,
        // For GetAssertion, these are the credentials allowed to answer.
        CredentialList: WEBAUTHN_CREDENTIALS { cCredentials: allowed.len() as u32, pCredentials: allowed.as_mut_ptr() },
        Extensions: WEBAUTHN_EXTENSIONS { cExtensions: 0, pExtensions: std::ptr::null_mut() },
        dwAuthenticatorAttachment: WEBAUTHN_AUTHENTICATOR_ATTACHMENT_CROSS_PLATFORM,
        dwUserVerificationRequirement: WEBAUTHN_USER_VERIFICATION_REQUIREMENT_REQUIRED,
        pCancellationId: prompt.id(),
        pHmacSecretSaltValues: &raw mut salts,
        ..Default::default()
    };
    let mut assertion: *mut WEBAUTHN_ASSERTION = std::ptr::null_mut();
    // SAFETY: every pointer in these structures points at locals (the RP id, client data JSON,
    // credential ids, salt) that outlive the call; `assertion` is a writable out-pointer.
    let result =
        unsafe { WebAuthNAuthenticatorGetAssertion(hwnd(window), rp_id.as_ptr(), &client, &options, &mut assertion) };
    drop(prompt);
    if result < 0 {
        return Err(failure(result));
    }
    let assertion = Assertion(assertion);
    if assertion.0.is_null() {
        return Err(KeyError::Failed("no answer".to_owned()));
    }
    // SAFETY: a successful call returned a valid assertion, freed only when `assertion` drops.
    let version = unsafe { (*assertion.0).dwVersion };
    // `pHmacSecret` only exists from version 4 of the structure.
    if version < WEBAUTHN_ASSERTION_VERSION_4 {
        return Err(KeyError::NoSecret);
    }
    // SAFETY: as above; the structure is at least version 4, so the field is there.
    let hmac = unsafe { (*assertion.0).pHmacSecret };
    if hmac.is_null() {
        return Err(KeyError::NoSecret);
    }
    // SAFETY: Windows returned a non-null pointer to the secret, valid until the assertion is freed.
    let hmac = unsafe { *hmac };
    if hmac.cbFirst as usize != SECRET_LEN || hmac.pbFirst.is_null() {
        return Err(KeyError::NoSecret);
    }
    let mut secret = Zeroizing::new([0u8; SECRET_LEN]);
    // SAFETY: `pbFirst` points at `cbFirst` (checked: SECRET_LEN) bytes, valid until the assertion
    // is freed.
    secret.copy_from_slice(unsafe { std::slice::from_raw_parts(hmac.pbFirst, SECRET_LEN) });
    // SAFETY: as above.
    let used = unsafe { (*assertion.0).Credential };
    // SAFETY: Windows returned this id and its length together, valid until the assertion is freed.
    let credential = unsafe { copy_bytes(used.pbId, used.cbId) };
    Ok((credential, secret))
}

/// Cancels Windows' security-key prompt, if one of ours is showing (call from another thread).
pub fn cancel() {
    let showing = *PROMPT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(id) = showing {
        // SAFETY: `id` is a cancellation id Windows handed out; the call only reads it.
        unsafe { WebAuthNCancelCurrentOperation(&id) };
    }
}

/// The cancellation id of the prompt showing now, for `cancel`.
static PROMPT: Mutex<Option<GUID>> = Mutex::new(None);

/// A prompt in progress: its cancellation id, published in `PROMPT` until dropped.
struct Prompt {
    id: Option<GUID>,
}

impl Prompt {
    fn begin() -> Prompt {
        let mut id = GUID::from_u128(0);
        // SAFETY: `id` is a writable GUID.
        let id = (unsafe { WebAuthNGetCancellationId(&mut id) } >= 0).then_some(id);
        *PROMPT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = id;
        Prompt { id }
    }

    /// For the options' `pCancellationId` (null if Windows didn't give one).
    fn id(&mut self) -> *mut GUID {
        self.id.as_mut().map_or(std::ptr::null_mut(), std::ptr::from_mut)
    }
}

impl Drop for Prompt {
    fn drop(&mut self) {
        *PROMPT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

/// A credential attestation from Windows, freed when dropped.
struct Attestation(*mut WEBAUTHN_CREDENTIAL_ATTESTATION);

impl Drop for Attestation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: Windows allocated this attestation, and it's freed only here.
            unsafe { WebAuthNFreeCredentialAttestation(self.0) };
        }
    }
}

/// An assertion from Windows, freed when dropped.
struct Assertion(*mut WEBAUTHN_ASSERTION);

impl Drop for Assertion {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: Windows allocated this assertion, and it's freed only here.
            unsafe { WebAuthNFreeAssertion(self.0) };
        }
    }
}

fn client_data(json: &mut [u8]) -> WEBAUTHN_CLIENT_DATA {
    WEBAUTHN_CLIENT_DATA {
        dwVersion: WEBAUTHN_CLIENT_DATA_CURRENT_VERSION,
        cbClientDataJSON: json.len() as u32,
        pbClientDataJSON: json.as_mut_ptr(),
        pwszHashAlgId: WEBAUTHN_HASH_ALGORITHM_SHA_256,
    }
}

/// WebAuthn credential entries pointing into `ids`, which must outlive them.
fn credential_list(ids: &mut [Vec<u8>]) -> Vec<WEBAUTHN_CREDENTIAL> {
    ids.iter_mut()
        .map(|id| WEBAUTHN_CREDENTIAL {
            dwVersion: WEBAUTHN_CREDENTIAL_CURRENT_VERSION,
            cbId: id.len() as u32,
            pbId: id.as_mut_ptr(),
            pwszCredentialType: WEBAUTHN_CREDENTIAL_TYPE_PUBLIC_KEY,
        })
        .collect()
}

/// A copy of `length` bytes Windows handed back at `data`.
///
/// # Safety
///
/// `data` must be null or point at `length` readable bytes.
unsafe fn copy_bytes(data: *const u8, length: u32) -> Vec<u8> {
    if data.is_null() || length == 0 {
        return Vec::new();
    }
    // SAFETY: the caller guarantees `data` points at `length` readable bytes.
    unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec()
}

/// What a failing HRESULT means for the person at the keyboard.
fn failure(result: HRESULT) -> KeyError {
    const NTE_EXISTS: u32 = 0x8009_000F;
    const NTE_NOT_SUPPORTED: u32 = 0x8009_0029;
    const NTE_USER_CANCELLED: u32 = 0x8009_0036;
    const ERROR_CANCELLED: u32 = 0x8007_04C7;
    const ERROR_TIMEOUT: u32 = 0x8007_05B4;
    match result as u32 {
        NTE_USER_CANCELLED | ERROR_CANCELLED | ERROR_TIMEOUT => KeyError::Cancelled,
        NTE_EXISTS => KeyError::AlreadyRegistered,
        NTE_NOT_SUPPORTED => KeyError::NoSecret,
        code => KeyError::Failed(format!("{} (0x{code:08X})", error_name(result))),
    }
}

/// Windows' name for a WebAuthn error ("NotAllowedError" and the like).
fn error_name(result: HRESULT) -> String {
    // SAFETY: no pointers in; returns null or a constant, NUL-terminated string.
    let name = unsafe { WebAuthNGetErrorName(result) };
    let mut units = Vec::new();
    if !name.is_null() {
        for index in 0..128 {
            // SAFETY: the string is NUL-terminated and this stops at the NUL, so every unit read
            // is part of it.
            let unit = unsafe { name.wrapping_add(index).read() };
            if unit == 0 {
                break;
            }
            units.push(unit);
        }
    }
    if units.is_empty() { "an error".to_owned() } else { String::from_utf16_lossy(&units) }
}

fn hwnd(window: isize) -> HWND {
    // An HWND is an opaque handle value, not a pointer to memory we own.
    std::ptr::without_provenance_mut(window as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Registering and getting secrets need a person with a key, so only the parts that don't
    // show a prompt are tested here.

    #[test]
    fn windows_has_webauthn() {
        assert!(api_version() >= 1, "webauthn.dll answers");
    }

    #[test]
    fn errors_read_plainly() {
        assert_eq!(failure(0x8009_0036_u32 as i32), KeyError::Cancelled);
        assert_eq!(failure(0x8009_000F_u32 as i32), KeyError::AlreadyRegistered);
        match failure(0x8009_0027_u32 as i32) {
            KeyError::Failed(why) => assert!(why.contains("0x80090027"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cancelling_with_nothing_showing_is_harmless() {
        cancel();
    }
}
