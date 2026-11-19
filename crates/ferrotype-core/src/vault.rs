//! Locking the saved history with security keys (a YubiKey, say).
//!
//! The history is encrypted with a random *data key* (AES-256-GCM). The data key is stored once
//! per registered security key, encrypted with a *slot key* derived (HKDF-SHA256) from that
//! security key's `hmac-secret` answer to this vault's salt: 32 bytes only that security key can
//! produce, and only with its PIN and a touch. Any registered key unlocks the data key; without
//! one, the history can't be read. Nothing secret is stored: the salt and the credential ids
//! (which security key is which) are public by design.
//!
//! The layout, all of it inside Windows' DPAPI layer (which the app adds and removes):
//!
//! ```text
//! "FTV1"
//! salt                32 bytes, asked of every security key
//! slot count          u32, then for each slot:
//!   credential        u32 length + bytes: the security key's WebAuthn credential id
//!   added             u64, when it was registered (Unix seconds)
//!   nonce             12 bytes
//!   wrapped data key  48 bytes: AES-256-GCM(slot key, data key)
//! nonce               12 bytes
//! ciphertext          the rest: AES-256-GCM(data key, encoded history), with every byte
//!                     before the nonce as associated data, so slots can't be swapped unseen
//! ```
//!
//! Integers are little-endian. Reading never panics on damaged input.

use std::fmt;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

const MAGIC: &[u8; 4] = b"FTV1";
/// The length of a security key's secret, and of the salt it's asked with.
pub const SECRET_LEN: usize = 32;
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const WRAPPED_LEN: usize = KEY_LEN + TAG_LEN;
/// Mixed into every slot key and slot, so they can't be mistaken for anything else.
const SLOT_CONTEXT: &[u8] = b"FerroType key slot v1";
/// WebAuthn credential ids are at most 1023 bytes.
const MAX_CREDENTIAL: usize = 1023;
/// More keys than anyone registers; also bounds what a damaged file can make us read.
pub const MAX_KEYS: usize = 16;

/// A security key's answer to the vault's salt.
pub type Secret = Zeroizing<[u8; SECRET_LEN]>;

/// The key the history is encrypted with; wiped from memory when dropped.
pub struct DataKey(Zeroizing<[u8; KEY_LEN]>);

impl fmt::Debug for DataKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DataKey(..)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Windows wouldn't hand out random bytes.
    Random,
    /// None of the registered keys is the one that answered.
    NotRegistered,
    /// The secret didn't open the key's slot (another credential's, or the file was changed).
    WrongSecret,
    /// The history didn't decrypt: the file was changed or damaged.
    Damaged,
    /// Already `MAX_KEYS` keys.
    TooManyKeys,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::Random => "Windows couldn't supply random numbers",
            Error::NotRegistered => "that security key isn't one registered here",
            Error::WrongSecret => "the security key's answer didn't unlock the history",
            Error::Damaged => "the locked history file is damaged",
            Error::TooManyKeys => "that's as many security keys as FerroType keeps",
        })
    }
}

impl std::error::Error for Error {}

/// One registered security key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// The WebAuthn credential id: tells Windows (and the key) which credential to use.
    pub credential: Vec<u8>,
    /// When it was registered (Unix seconds).
    pub added: u64,
    nonce: [u8; NONCE_LEN],
    wrapped: [u8; WRAPPED_LEN],
}

/// The part of a locked history file that isn't secret: the salt and the registered keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vault {
    salt: [u8; SECRET_LEN],
    slots: Vec<Slot>,
}

impl Vault {
    /// A new vault with no keys yet, and the data key it will use.
    pub fn create() -> Result<(Vault, DataKey), Error> {
        let vault = Vault { salt: random()?, slots: Vec::new() };
        Ok((vault, DataKey(Zeroizing::new(random()?))))
    }

    /// What every security key is asked, to get its secret.
    pub fn salt(&self) -> &[u8; SECRET_LEN] {
        &self.salt
    }

    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    /// The credential ids of the registered keys, for Windows' prompt.
    pub fn credentials(&self) -> Vec<Vec<u8>> {
        self.slots.iter().map(|slot| slot.credential.clone()).collect()
    }

    /// Registers a security key: stores the data key under a slot key derived from the key's
    /// `secret`. Registering the same credential again replaces its slot.
    pub fn add_key(&mut self, credential: &[u8], secret: &Secret, key: &DataKey, now: u64) -> Result<(), Error> {
        let replacing = self.slots.iter().any(|slot| slot.credential == credential);
        if credential.is_empty() || credential.len() > MAX_CREDENTIAL {
            return Err(Error::NotRegistered);
        }
        if !replacing && self.slots.len() >= MAX_KEYS {
            return Err(Error::TooManyKeys);
        }
        let nonce: [u8; NONCE_LEN] = random()?;
        let cipher =
            Aes256Gcm::new_from_slice(slot_key(&self.salt, credential, secret).as_ref()).map_err(|_| Error::Damaged)?;
        let aad = slot_aad(credential);
        let sealed = cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: key.0.as_ref(), aad: &aad })
            .map_err(|_| Error::Damaged)?;
        let wrapped: [u8; WRAPPED_LEN] = sealed.as_slice().try_into().map_err(|_| Error::Damaged)?;
        self.slots.retain(|slot| slot.credential != credential);
        self.slots.push(Slot { credential: credential.to_vec(), added: now, nonce, wrapped });
        Ok(())
    }

    /// Forgets a security key; whether it was registered.
    pub fn remove_key(&mut self, credential: &[u8]) -> bool {
        let before = self.slots.len();
        self.slots.retain(|slot| slot.credential != credential);
        self.slots.len() != before
    }

    /// The data key, from the secret of the security key that answered.
    pub fn unlock(&self, credential: &[u8], secret: &Secret) -> Result<DataKey, Error> {
        let slot = self.slots.iter().find(|slot| slot.credential == credential).ok_or(Error::NotRegistered)?;
        let cipher = Aes256Gcm::new_from_slice(slot_key(&self.salt, credential, secret).as_ref())
            .map_err(|_| Error::WrongSecret)?;
        let aad = slot_aad(credential);
        let opened = Zeroizing::new(
            cipher
                .decrypt(Nonce::from_slice(&slot.nonce), Payload { msg: &slot.wrapped, aad: &aad })
                .map_err(|_| Error::WrongSecret)?,
        );
        let key: [u8; KEY_LEN] = opened.as_slice().try_into().map_err(|_| Error::WrongSecret)?;
        Ok(DataKey(Zeroizing::new(key)))
    }

    /// The whole file: this header, then `plaintext` encrypted with the data key.
    pub fn seal(&self, key: &DataKey, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        let mut out = self.header();
        let nonce: [u8; NONCE_LEN] = random()?;
        let cipher = Aes256Gcm::new_from_slice(key.0.as_ref()).map_err(|_| Error::Damaged)?;
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad: &out })
            .map_err(|_| Error::Damaged)?;
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    fn header(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.salt);
        out.extend_from_slice(&(self.slots.len() as u32).to_le_bytes());
        for slot in &self.slots {
            out.extend_from_slice(&(slot.credential.len() as u32).to_le_bytes());
            out.extend_from_slice(&slot.credential);
            out.extend_from_slice(&slot.added.to_le_bytes());
            out.extend_from_slice(&slot.nonce);
            out.extend_from_slice(&slot.wrapped);
        }
        out
    }
}

/// A locked history file, read back: its vault, and the history still encrypted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    vault: Vault,
    header_len: usize,
    bytes: Vec<u8>,
}

impl Sealed {
    /// Reads a locked history file; `None` if it isn't one or is damaged.
    pub fn parse(bytes: &[u8]) -> Option<Sealed> {
        let mut reader = Reader { bytes, at: 0 };
        if reader.take(MAGIC.len())? != MAGIC {
            return None;
        }
        let salt: [u8; SECRET_LEN] = reader.take(SECRET_LEN)?.try_into().ok()?;
        let count = usize::try_from(reader.u32()?).ok()?;
        if count > MAX_KEYS {
            return None;
        }
        let mut slots = Vec::with_capacity(count);
        for _ in 0..count {
            let length = usize::try_from(reader.u32()?).ok()?;
            if length == 0 || length > MAX_CREDENTIAL {
                return None;
            }
            let credential = reader.take(length)?.to_vec();
            let added = u64::from_le_bytes(reader.take(8)?.try_into().ok()?);
            let nonce = reader.take(NONCE_LEN)?.try_into().ok()?;
            let wrapped = reader.take(WRAPPED_LEN)?.try_into().ok()?;
            slots.push(Slot { credential, added, nonce, wrapped });
        }
        let header_len = reader.at;
        // A nonce and at least a tag must follow.
        if bytes.len() < header_len + NONCE_LEN + TAG_LEN {
            return None;
        }
        Some(Sealed { vault: Vault { salt, slots }, header_len, bytes: bytes.to_vec() })
    }

    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    /// The history, decrypted with the data key.
    pub fn open(&self, key: &DataKey) -> Result<Zeroizing<Vec<u8>>, Error> {
        let (header, rest) = self.bytes.split_at(self.header_len);
        let (nonce, ciphertext) = rest.split_at(NONCE_LEN);
        let cipher = Aes256Gcm::new_from_slice(key.0.as_ref()).map_err(|_| Error::Damaged)?;
        cipher
            .decrypt(Nonce::from_slice(nonce), Payload { msg: ciphertext, aad: header })
            .map(Zeroizing::new)
            .map_err(|_| Error::Damaged)
    }
}

/// Whether `bytes` (DPAPI already removed) are a locked history file rather than a plain one.
pub fn is_locked_file(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

fn slot_key(salt: &[u8; SECRET_LEN], credential: &[u8], secret: &Secret) -> Zeroizing<[u8; KEY_LEN]> {
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    let info = slot_aad(credential);
    // 32 bytes is far below HKDF-SHA256's limit (255 * 32), so expanding can't fail.
    let _ = Hkdf::<Sha256>::new(Some(salt), secret.as_ref()).expand(&info, key.as_mut());
    key
}

/// `SLOT_CONTEXT` followed by the credential id: binds a slot to the key it belongs to.
fn slot_aad(credential: &[u8]) -> Vec<u8> {
    [SLOT_CONTEXT, credential].concat()
}

fn random<const N: usize>() -> Result<[u8; N], Error> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|_| Error::Random)?;
    Ok(bytes)
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(count)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(fill: u8) -> Secret {
        Zeroizing::new([fill; SECRET_LEN])
    }

    /// A vault with two keys ("main" and "backup") and a sealed history.
    fn two_keys() -> (Vault, Vec<u8>) {
        let (mut vault, key) = Vault::create().unwrap();
        vault.add_key(b"main", &secret(1), &key, 100).unwrap();
        vault.add_key(b"backup", &secret(2), &key, 200).unwrap();
        let file = vault.seal(&key, b"pinned entries").unwrap();
        (vault, file)
    }

    #[test]
    fn either_key_unlocks() {
        let (_, file) = two_keys();
        assert!(is_locked_file(&file));
        let sealed = Sealed::parse(&file).unwrap();
        assert_eq!(sealed.vault().credentials(), [b"main".to_vec(), b"backup".to_vec()]);
        assert_eq!(sealed.vault().slots()[1].added, 200);
        for (credential, fill) in [(&b"main"[..], 1), (&b"backup"[..], 2)] {
            let key = sealed.vault().unlock(credential, &secret(fill)).unwrap();
            assert_eq!(sealed.open(&key).unwrap().as_slice(), b"pinned entries");
        }
    }

    #[test]
    fn wrong_or_unknown_keys_fail() {
        let (_, file) = two_keys();
        let vault = Sealed::parse(&file).unwrap().vault().clone();
        assert_eq!(vault.unlock(b"main", &secret(2)).unwrap_err(), Error::WrongSecret, "backup's secret");
        assert_eq!(vault.unlock(b"main", &secret(9)).unwrap_err(), Error::WrongSecret);
        assert_eq!(vault.unlock(b"stranger", &secret(1)).unwrap_err(), Error::NotRegistered);
    }

    #[test]
    fn a_slot_belongs_to_its_credential() {
        // The main key's slot copied under another credential id doesn't open with main's secret.
        let (vault, _) = two_keys();
        let mut moved = vault.clone();
        moved.slots[1].nonce = moved.slots[0].nonce;
        moved.slots[1].wrapped = moved.slots[0].wrapped;
        assert_eq!(moved.unlock(b"backup", &secret(1)).unwrap_err(), Error::WrongSecret);
    }

    #[test]
    fn keys_come_and_go() {
        let (mut vault, key) = Vault::create().unwrap();
        vault.add_key(b"a", &secret(1), &key, 1).unwrap();
        vault.add_key(b"a", &secret(3), &key, 2).unwrap();
        assert_eq!(vault.slots().len(), 1, "registering again replaces");
        assert!(vault.unlock(b"a", &secret(3)).is_ok());
        assert!(vault.remove_key(b"a"));
        assert!(!vault.remove_key(b"a"));
        for i in 0..MAX_KEYS {
            vault.add_key(&[i as u8 + 1], &secret(1), &key, 0).unwrap();
        }
        assert_eq!(vault.add_key(b"one more", &secret(1), &key, 0).unwrap_err(), Error::TooManyKeys);
        assert_eq!(vault.add_key(&[], &secret(1), &key, 0).unwrap_err(), Error::NotRegistered);
    }

    #[test]
    fn seals_freshly_each_time() {
        let (vault, key) = Vault::create().unwrap();
        assert_ne!(vault.seal(&key, b"same").unwrap(), vault.seal(&key, b"same").unwrap());
        let (other, _) = Vault::create().unwrap();
        assert_ne!(vault.salt(), other.salt());
    }

    #[test]
    fn any_change_is_caught() {
        let (_, file) = two_keys();
        for at in 0..file.len() {
            let mut changed = file.clone();
            changed[at] ^= 0x01;
            let Some(sealed) = Sealed::parse(&changed) else { continue };
            let opened = sealed.vault().unlock(b"main", &secret(1)).and_then(|key| sealed.open(&key));
            assert!(opened.is_err(), "a change at byte {at} went unnoticed");
        }
    }

    #[test]
    fn damaged_files_never_panic() {
        let (_, file) = two_keys();
        for end in 0..file.len() {
            if let Some(sealed) = Sealed::parse(&file[..end]) {
                let key = sealed.vault().unlock(b"main", &secret(1));
                assert!(key.and_then(|key| sealed.open(&key)).is_err(), "truncated at {end}");
            }
        }
        assert!(Sealed::parse(b"FTH1 plain history").is_none());
        assert!(!is_locked_file(b"FTH1"));
        // A huge slot count or credential length is refused, not allocated.
        let mut huge = MAGIC.to_vec();
        huge.extend_from_slice(&[0; SECRET_LEN]);
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(Sealed::parse(&huge).is_none());
    }
}
