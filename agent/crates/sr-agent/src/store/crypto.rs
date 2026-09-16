//! Encryption for private-window data (class C4, docs/06-privacy-security.md).
//!
//! ```text
//! random 32-byte DEK --DPAPI(user scope, UI_FORBIDDEN)--> wrapped_key in crypto_keys
//! row --AES-256-GCM(DEK, random 96-bit nonce, aad = snapshot||tab_key||key_id)--> ciphertext
//! ```
//!
//! What this buys: protection against offline access to the database file - a stolen
//! laptop, a backup copy, another user on the machine. What it does not buy:
//! protection from malware running as you, because DPAPI unwraps for that process
//! exactly as it unwraps for us. Both halves of that are stated in the product UI.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, Result};
use rand::RngCore;
use zeroize::{Zeroize, ZeroizeOnDrop};

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;

/// A data encryption key. Zeroized on drop.
///
/// Reliable scrubbing is one of the reasons ADR-0005 chose Rust: in a GC'd language
/// you cannot control when a key buffer is collected or whether it was copied during
/// heap compaction.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Dek {
    key: [u8; KEY_LEN],
}

impl std::fmt::Debug for Dek {
    /// Never print key material, including via a derived Debug in a panic message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Dek(<redacted>)")
    }
}

impl Dek {
    pub fn generate() -> Self {
        let mut key = [0u8; KEY_LEN];
        rand::thread_rng().fill_bytes(&mut key);
        Dek { key }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != KEY_LEN {
            return Err(anyhow!("DEK must be {KEY_LEN} bytes, got {}", bytes.len()));
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(bytes);
        Ok(Dek { key })
    }

    pub fn expose(&self) -> &[u8; KEY_LEN] {
        &self.key
    }
}

/// Binds a ciphertext to the tab it belongs to.
///
/// Without this, someone with write access to the .db could move a blob onto a
/// different `tab_key` and we would decrypt it happily in the wrong context. Cheap to
/// add, and it turns a silent integrity failure into a loud decryption error.
///
/// **`snapshot_id` is deliberately NOT part of the binding.** It was, and that was a
/// bug: taking a snapshot copies private rows to a new `snapshot_id`, so every one of
/// them became undecryptable the moment it was copied. The failure was silent in the
/// worst way - the review window asked for private tabs, every decrypt failed, and it
/// displayed an empty list as though there were none.
///
/// Binding to the tab is what the check is actually for. The same tab's data appearing
/// under several snapshot ids is our own copying, not an attacker relocating a blob.
pub fn aad(tab_key: &str, key_id: i64) -> Vec<u8> {
    format!("{tab_key}\u{0}{key_id}").into_bytes()
}

pub struct Sealed {
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

pub fn seal(dek: &Dek, aad_bytes: &[u8], plaintext: &[u8]) -> Result<Sealed> {
    let cipher = Aes256Gcm::new_from_slice(dek.expose()).map_err(|e| anyhow!("bad key: {e}"))?;
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad: aad_bytes,
            },
        )
        .map_err(|_| anyhow!("encryption failed"))?;

    Ok(Sealed {
        nonce: nonce_bytes.to_vec(),
        ciphertext,
    })
}

pub fn open(dek: &Dek, aad_bytes: &[u8], nonce: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    if nonce.len() != NONCE_LEN {
        return Err(anyhow!("nonce must be {NONCE_LEN} bytes"));
    }
    let cipher = Aes256Gcm::new_from_slice(dek.expose()).map_err(|e| anyhow!("bad key: {e}"))?;
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad: aad_bytes,
            },
        )
        // Deliberately opaque: the caller cannot distinguish a wrong key from a
        // tampered row, and neither should an attacker.
        .map_err(|_| anyhow!("decryption failed"))
}

// ---------------------------------------------------------------------------
// Key wrapping
// ---------------------------------------------------------------------------

pub const WRAP_METHOD: &str = "dpapi-user-v1";

#[cfg(windows)]
mod platform {
    use super::*;
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    /// Extra entropy stored outside the database, so possessing sessions.db alone is
    /// insufficient even on the same account (docs/06).
    fn entropy_blob(entropy: &mut Vec<u8>) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: entropy.len() as u32,
            pbData: entropy.as_mut_ptr(),
        }
    }

    pub fn wrap(raw: &[u8], entropy: &[u8]) -> Result<Vec<u8>> {
        let mut input = raw.to_vec();
        let mut ent = entropy.to_vec();
        let mut in_blob = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
            pbData: input.as_mut_ptr(),
        };
        let mut ent_blob = entropy_blob(&mut ent);
        let mut out = CRYPT_INTEGER_BLOB::default();

        unsafe {
            CryptProtectData(
                &mut in_blob,
                windows::core::PCWSTR::null(),
                Some(&mut ent_blob),
                None,
                None,
                // The agent is a background process. A DPAPI prompt there would be an
                // invisible hang, and a background process should never be able to put
                // a credential prompt on screen anyway.
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )?;
            let slice = std::slice::from_raw_parts(out.pbData, out.cbData as usize);
            let result = slice.to_vec();
            let _ = LocalFree(HLOCAL(out.pbData as *mut _));
            input.zeroize();
            Ok(result)
        }
    }

    pub fn unwrap(wrapped: &[u8], entropy: &[u8]) -> Result<Vec<u8>> {
        let mut input = wrapped.to_vec();
        let mut ent = entropy.to_vec();
        let mut in_blob = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
            pbData: input.as_mut_ptr(),
        };
        let mut ent_blob = entropy_blob(&mut ent);
        let mut out = CRYPT_INTEGER_BLOB::default();

        unsafe {
            CryptUnprotectData(
                &mut in_blob,
                None,
                Some(&mut ent_blob),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )?;
            let slice = std::slice::from_raw_parts(out.pbData, out.cbData as usize);
            let mut result = slice.to_vec();
            // Scrub the OS-allocated plaintext before handing it back.
            std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
            let _ = LocalFree(HLOCAL(out.pbData as *mut _));
            if result.len() != KEY_LEN {
                result.zeroize();
                return Err(anyhow!("unwrapped key has wrong length"));
            }
            Ok(result)
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    /// Non-Windows builds exist only so the store and protocol layers stay testable on
    /// CI runners. This is NOT key protection and must never ship.
    pub fn wrap(raw: &[u8], _entropy: &[u8]) -> Result<Vec<u8>> {
        Ok(raw.to_vec())
    }
    pub fn unwrap(wrapped: &[u8], _entropy: &[u8]) -> Result<Vec<u8>> {
        Ok(wrapped.to_vec())
    }
}

pub fn wrap_key(dek: &Dek, entropy: &[u8]) -> Result<Vec<u8>> {
    platform::wrap(dek.expose(), entropy)
}

pub fn unwrap_key(wrapped: &[u8], entropy: &[u8]) -> Result<Dek> {
    let mut raw = platform::unwrap(wrapped, entropy)?;
    let dek = Dek::from_bytes(&raw)?;
    raw.zeroize();
    Ok(dek)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AAD: &[u8] = b"0\0w1:t1\01";

    #[test]
    fn seals_and_opens() {
        let dek = Dek::generate();
        let s = seal(&dek, AAD, b"https://example.test/private").unwrap();
        let out = open(&dek, AAD, &s.nonce, &s.ciphertext).unwrap();
        assert_eq!(out, b"https://example.test/private");
    }

    #[test]
    fn ciphertext_does_not_contain_the_plaintext() {
        let dek = Dek::generate();
        let url = b"https://verysecret.test/page";
        let s = seal(&dek, AAD, url).unwrap();
        assert!(
            !s.ciphertext.windows(url.len()).any(|w| w == url),
            "plaintext leaked into ciphertext"
        );
    }

    #[test]
    fn nonce_differs_per_seal() {
        let dek = Dek::generate();
        let a = seal(&dek, AAD, b"same").unwrap();
        let b = seal(&dek, AAD, b"same").unwrap();
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    #[test]
    fn wrong_key_fails_to_open() {
        let s = seal(&Dek::generate(), AAD, b"secret").unwrap();
        assert!(open(&Dek::generate(), AAD, &s.nonce, &s.ciphertext).is_err());
    }

    #[test]
    fn moving_a_row_breaks_decryption() {
        // The regression AAD exists to prevent: a blob relocated to another tab_key
        // must not decrypt in its new home.
        let dek = Dek::generate();
        let s = seal(&dek, &aad("w1:t1", 1), b"https://x.test/").unwrap();
        assert!(open(&dek, &aad("w1:t2", 1), &s.nonce, &s.ciphertext).is_err());
    }

    #[test]
    fn a_row_copied_into_a_snapshot_still_decrypts() {
        // The bug this replaced: binding to snapshot_id meant taking a snapshot made
        // every private tab undecryptable, and the review window showed an empty list
        // rather than an error.
        let dek = Dek::generate();
        let s = seal(&dek, &aad("w1:t1", 1), b"https://x.test/").unwrap();
        // Same tab, different snapshot - our own copy, not an attacker.
        let out = open(&dek, &aad("w1:t1", 1), &s.nonce, &s.ciphertext).unwrap();
        assert_eq!(out, b"https://x.test/");
    }

    #[test]
    fn a_row_sealed_under_another_key_does_not_decrypt() {
        let dek = Dek::generate();
        let s = seal(&dek, &aad("w1:t1", 1), b"u").unwrap();
        assert!(open(&dek, &aad("w1:t1", 2), &s.nonce, &s.ciphertext).is_err());
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let dek = Dek::generate();
        let mut s = seal(&dek, AAD, b"https://x.test/").unwrap();
        let last = s.ciphertext.len() - 1;
        s.ciphertext[last] ^= 0x01;
        assert!(open(&dek, AAD, &s.nonce, &s.ciphertext).is_err());
    }

    #[test]
    fn rejects_malformed_nonce() {
        let dek = Dek::generate();
        let s = seal(&dek, AAD, b"x").unwrap();
        assert!(open(&dek, AAD, &s.nonce[..4], &s.ciphertext).is_err());
    }

    #[test]
    fn dek_debug_never_prints_key_material() {
        let dek = Dek::generate();
        assert_eq!(format!("{dek:?}"), "Dek(<redacted>)");
    }

    #[test]
    fn rejects_wrong_length_key() {
        assert!(Dek::from_bytes(&[0u8; 16]).is_err());
        assert!(Dek::from_bytes(&[0u8; 32]).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_roundtrips_a_key() {
        let dek = Dek::generate();
        let entropy = b"session-restore-v1";
        let wrapped = wrap_key(&dek, entropy).unwrap();
        assert_ne!(&wrapped[..], &dek.expose()[..], "key stored in the clear");
        let back = unwrap_key(&wrapped, entropy).unwrap();
        assert_eq!(back.expose(), dek.expose());
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_unwrap_fails_without_the_matching_entropy() {
        let dek = Dek::generate();
        let wrapped = wrap_key(&dek, b"correct-entropy").unwrap();
        assert!(unwrap_key(&wrapped, b"wrong-entropy").is_err());
    }
}
