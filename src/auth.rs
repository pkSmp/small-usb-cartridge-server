//! One shared secret for the whole fleet.
//!
//! Without it, anything on the switch can answer a beacon and push a release
//! onto every node: the sync protocol trusts whoever claims the newest epoch.
//! The key is provisioned once at install (`/etc/cartridge/fleet.key`), never
//! travels over the network, and is checked with HMAC-SHA256, which costs a few
//! lines on top of the SHA-256 the release manifest already uses.
//!
//! No key file means today's open behaviour, so a single box or a lab stays
//! usable; `cartridge fleet key` writes one when the fleet is ready for it.

use crate::hash;
use std::fs;
use std::path::PathBuf;

pub const KEY_FILE: &str = "/etc/cartridge/fleet.key";

fn key_path() -> PathBuf {
    PathBuf::from(std::env::var("CARTRIDGE_KEY_FILE").unwrap_or_else(|_| KEY_FILE.to_string()))
}

fn sha(data: &[u8]) -> [u8; 32] {
    let mut h = hash::Sha256::new();
    h.update(data);
    h.finish()
}

/// HMAC-SHA256 (RFC 2104). Keys longer than the block size are hashed first.
pub fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&sha(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = [0x36u8; 64];
    let mut outer = [0x5cu8; 64];
    for i in 0..64 {
        inner[i] ^= block[i];
        outer[i] ^= block[i];
    }
    let mut h = hash::Sha256::new();
    h.update(&inner);
    h.update(message);
    let first = h.finish();
    let mut h = hash::Sha256::new();
    h.update(&outer);
    h.update(&first);
    h.finish()
}

pub fn tag(key: &[u8], message: &[u8]) -> String {
    hash::hex(&hmac(key, message))
}

/// Compare without an early exit, so a caller cannot learn the tag one byte at
/// a time. The keys here are LAN secrets, but the check costs nothing.
pub fn verify(key: &[u8], message: &[u8], expected_hex: &str) -> bool {
    let want = expected_hex.trim().to_ascii_lowercase();
    if want.len() != 64 {
        return false;
    }
    let got = tag(key, message);
    let a = got.as_bytes();
    let b = want.as_bytes();
    let mut diff = 0u8;
    for i in 0..64 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn parse_key(text: &str) -> Option<Vec<u8>> {
    let hex: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if hex.len() < 32 || hex.len() % 2 != 0 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    for pair in bytes.chunks(2) {
        let s = std::str::from_utf8(pair).ok()?;
        out.push(u8::from_str_radix(s, 16).ok()?);
    }
    Some(out)
}

/// The fleet key, if one has been provisioned.
pub fn fleet_key() -> Option<Vec<u8>> {
    let text = fs::read_to_string(key_path()).ok()?;
    parse_key(&text)
}

/// Create a key if there is none. Returns true when a new one was written.
pub fn ensure_key() -> Result<bool, String> {
    if fleet_key().is_some() {
        return Ok(false);
    }
    let path = key_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {}", parent.display(), e))?;
    }
    let key = crate::ident::random_u128().to_be_bytes();
    let mut text = String::new();
    for b in key.iter() {
        text.push_str(&format!("{:02x}", b));
    }
    text.push('\n');
    fs::write(&path, text).map_err(|e| format!("{}: {}", path.display(), e))?;
    // The key is a secret: only the owner may read it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_rfc_4231_vector() {
        let key = [0x0bu8; 20];
        assert_eq!(
            tag(&key, b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Second vector: key "Jefe", message "what do ya want for nothing?"
        assert_eq!(
            tag(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn a_long_key_is_hashed_first() {
        // RFC 4231 case 6.
        let key = [0xaau8; 131];
        let msg = b"Test Using Larger Than Block-Size Key - Hash Key First";
        assert_eq!(
            tag(&key, msg),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn verification_rejects_tampering() {
        let key = b"secret";
        let good = tag(key, b"release 7");
        assert!(verify(key, b"release 7", &good));
        assert!(!verify(key, b"release 8", &good));
        assert!(!verify(b"other", b"release 7", &good));
        assert!(!verify(key, b"release 7", "short"));
    }

    #[test]
    fn keys_parse_from_hex_and_reject_junk() {
        // A real key is 16 bytes or more; a truncated file must not look like one.
        let text = "00ff10".repeat(6);
        assert_eq!(parse_key(&text).unwrap().len(), 18);
        assert!(parse_key("00ff10\n").is_none());
        assert!(parse_key("xyz").is_none());
        assert!(parse_key("0").is_none());
    }
}
