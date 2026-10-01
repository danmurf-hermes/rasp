//! HMAC signing for session cookie values.
//!
//! The session cookie must be unforgeable: a visitor who can guess or
//! write a valid cookie value would land inside another visitor's
//! session. RASP therefore signs the plain session id with HMAC-SHA256
//! under a per-process random key. Every live `SessionManager` mints
//! its own key, so a cookie is only valid inside the process that
//! issued it — restarting the server (or the debug/test helper that
//! resets the key) invalidates outstanding cookies, which matches the
//! in-process store they point at anyway.

#[cfg(feature = "test-util")]
use std::sync::atomic::Ordering;

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Byte length of the signing key (32 bytes = SHA-256 block-class key).
const KEY_LEN: usize = 32;

/// Length of the hex signature carried in the cookie value (64 hex
/// chars = the full HMAC-SHA256; shortening saves nothing here).
const SIGNATURE_HEX_LEN: usize = 64;

/// A process-lifetime signing key for session cookies.
///
/// Cloning shares the same key (the clone signs and verifies exactly
/// like the original) so the key can live on a clonable manager or be
/// swapped out by tests via [`signing_key_with`].
#[derive(Clone)]
pub struct SigningKey {
    bytes: std::rc::Rc<[u8; KEY_LEN]>,
}

impl SigningKey {
    /// Draw a fresh random key from the OS.
    pub fn generate() -> Self {
        Self::with_bytes(random_bytes())
    }

    /// A key from explicit bytes — the deterministic path for tests
    /// and for loading a persisted key later.
    pub fn with_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self {
            bytes: std::rc::Rc::new(bytes),
        }
    }
}

impl std::fmt::Debug for SigningKey {
    /// Never print the key material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SigningKey(<secret>)")
    }
}

/// Sign a session id and return the full cookie value:
/// `rasp<8-hex id>.<64-hex signature>`.
pub fn sign_session_cookie(key: &SigningKey, id: u32) -> String {
    let id_hex = format!("{id:08x}");
    format!("{SESSION_VALUE_PREFIX}{id_hex}.{}", mac_hex(key, &id_hex))
}

/// Verify a cookie value and return its session id. `None` when the
/// shape is wrong, the signature does not match (constant-time
/// compare), or anything about the value is unexpected — every such
/// cookie is treated as "no session".
pub fn verify_session_cookie(key: &SigningKey, value: &str) -> Option<u32> {
    let rest = value.strip_prefix(SESSION_VALUE_PREFIX)?;
    let (id_hex, sig_hex) = rest.split_once('.')?;
    if sig_hex.len() != SIGNATURE_HEX_LEN {
        return None;
    }
    let expected = mac_hex(key, id_hex);
    if !constant_time_eq(sig_hex.as_bytes(), expected.as_bytes()) {
        return None;
    }
    // The signature already covers exactly this id text; after the
    // compare passes, parsing it cannot hand out a foreign id.
    u32::from_str_radix(id_hex, 16).ok()
}

const SESSION_VALUE_PREFIX: &str = "rasp";

fn mac_hex(key: &SigningKey, id_hex: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(key.bytes.as_slice()).expect("HMAC accepts any key length");
    mac.update(id_hex.as_bytes());
    hex_lower(&mac.finalize().into_bytes())
}

/// Constant-time byte compare: total over all byte differences, so
/// timing does not leak how far into the signature a mismatch lies.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(feature = "test-util")]
pub mod test_util {
    //! Test-only knobs: deterministic keys and key resets. Never
    //! compiled into ordinary builds.

    use super::SigningKey;
    use std::sync::atomic::{AtomicBool, Ordering};

    static DETERMINISTIC: AtomicBool = AtomicBool::new(false);

    /// Make [`SigningKey::generate`] return a fixed key. Call this in
    /// test setup when a test needs one key across separately built
    /// in-process managers; flip back off to return to random keys.
    pub fn use_deterministic_key(enabled: bool) {
        DETERMINISTIC.store(enabled, Ordering::seq_cst);
    }
}

/// Draw 32 random bytes from the OS. With the `test-util` feature
/// enabled, honours the deterministic-key switch so the two-step test
/// paths (build a manager, then resolve through it) see one key.
fn random_bytes() -> [u8; KEY_LEN] {
    #[cfg(feature = "test-util")]
    if test_util::DETERMINISTIC.load(Ordering::seq_cst) {
        return std::array::from_fn(|i| (i as u8) ^ 0xa5);
    }
    let mut bytes = [0u8; KEY_LEN];
    getrandom::fill(&mut bytes).expect("no OS randomness for session signing keys");
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_value_round_trips() {
        let key = SigningKey::generate();
        let value = sign_session_cookie(&key, 0xdeadbeef);
        assert_eq!(verify_session_cookie(&key, &value), Some(0xdeadbeef));
        assert!(value.starts_with("raspdeadbeef."));
        assert_eq!(value.split_once('.').unwrap().1.len(), 64);
    }

    #[test]
    fn tampered_values_rejected() {
        let key = SigningKey::generate();
        let value = sign_session_cookie(&key, 1);
        // Flipped signature byte.
        let mut chars: Vec<char> = value.chars().collect();
        let last = chars.pop().unwrap();
        chars.push(if last == '0' { '1' } else { '0' });
        let flipped: String = chars.into_iter().collect();
        assert_eq!(verify_session_cookie(&key, &flipped), None);
        // Wrong length signature.
        assert_eq!(verify_session_cookie(&key, "rasp00000001.abc"), None);
        // Missing dot.
        assert_eq!(verify_session_cookie(&key, "rasp00000001"), None);
        // Unsigned legacy value.
        assert_eq!(verify_session_cookie(&key, "rasp00000001"), None);
        // Another process's key.
        assert_eq!(verify_session_cookie(&SigningKey::generate(), &value), None);
    }

    #[test]
    fn signatures_are_deterministic_and_key_dependent() {
        let key = SigningKey::with_bytes([7u8; KEY_LEN]);
        assert_eq!(sign_session_cookie(&key, 3), sign_session_cookie(&key, 3));
        let other = SigningKey::with_bytes([8u8; KEY_LEN]);
        assert_ne!(sign_session_cookie(&key, 3), sign_session_cookie(&other, 3));
    }

    #[test]
    fn id_zero_signs_and_verifies() {
        let key = SigningKey::generate();
        let value = sign_session_cookie(&key, 0);
        assert_eq!(verify_session_cookie(&key, &value), Some(0));
    }
}
