//! Who this machine is.
//!
//! Every node needs a name that survives a reboot, is unique on the switch, and
//! can be said out loud. `/etc/machine-id` is none of those on a fleet: cloned
//! and imaged PCs share it, so two boxes would merge into one peer entry. A
//! random 128-bit id written once to `/var/lib/cartridge/id` is unique by
//! construction, and the display name is derived from it so every node computes
//! the same name for the same peer without asking.

use crate::hash;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub const DIR: &str = "/var/lib/cartridge";
const ID_FILE: &str = "/var/lib/cartridge/id";

/// Two short word lists are enough to make a name memorable; the five base32
/// characters in front are what actually keeps them apart.
const WORDS_A: [&str; 16] = [
    "amber", "brisk", "cobalt", "dusty", "ember", "frost", "glass", "hollow",
    "ivory", "jade", "kiln", "lunar", "moss", "nickel", "opal", "pine",
];
const WORDS_B: [&str; 16] = [
    "fox", "heron", "otter", "lynx", "wren", "moth", "hare", "crow",
    "seal", "finch", "badger", "crane", "stoat", "raven", "vole", "kite",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub id: u128,
    pub name: String,
}

impl Identity {
    pub fn hex(&self) -> String {
        format!("{:032x}", self.id)
    }

    /// The number placement uses. Any stable 64-bit projection works as long as
    /// every node derives it the same way.
    pub fn key(&self) -> u64 {
        key_of(self.id)
    }
}

/// The placement key for an id, without needing the rest of the identity. Every
/// node derives it identically, which is what makes the maps agree.
pub fn key_of(id: u128) -> u64 {
    let mut h = hash::Sha256::new();
    h.update(&id.to_be_bytes());
    let d = h.finish();
    u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

fn id_path() -> PathBuf {
    PathBuf::from(std::env::var("CARTRIDGE_ID_FILE").unwrap_or_else(|_| ID_FILE.to_string()))
}

/// Sixteen random bytes, or a hash of everything available if `/dev/urandom`
/// cannot be opened. The fallback is only a last resort; it is not secret, and
/// it does not need to be: this is an identifier, not a key.
pub fn random_u128() -> u128 {
    let mut buf = [0u8; 16];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            let v = u128::from_be_bytes(buf);
            if v != 0 {
                return v;
            }
        }
    }
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) as u128;
    let mut h = hash::Sha256::new();
    h.update(&nanos.to_be_bytes());
    h.update(&(std::process::id() as u128).to_be_bytes());
    h.update(&seq.to_be_bytes());
    let d = h.finish();
    let mut v = [0u8; 16];
    v.copy_from_slice(&d[..16]);
    u128::from_be_bytes(v)
}

/// Base32 (lowercase, no padding) of the first bytes of the hash.
fn base32(data: &[u8], chars: usize) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::new();
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for b in data {
        acc = (acc << 8) | *b as u32;
        bits += 8;
        while bits >= 5 && out.len() < chars {
            out.push(ALPHABET[((acc >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
        if out.len() >= chars {
            break;
        }
    }
    out
}

/// `k7f3q-amber-fox`: five characters from the id, two words for the humans.
pub fn derive_name(id: u128) -> String {
    let mut h = hash::Sha256::new();
    h.update(&id.to_be_bytes());
    let d = h.finish();
    let code = base32(&d[..4], 5);
    let a = WORDS_A[(d[4] as usize) % WORDS_A.len()];
    let b = WORDS_B[(d[5] as usize) % WORDS_B.len()];
    format!("{}-{}-{}", code, a, b)
}

pub fn parse(text: &str) -> Option<Identity> {
    let mut parts = text.split_whitespace();
    let hex = parts.next()?;
    if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let id = u128::from_str_radix(hex, 16).ok()?;
    if id == 0 {
        return None;
    }
    let name = parts
        .next()
        .map(|s| s.to_string())
        .unwrap_or_else(|| derive_name(id));
    Some(Identity { id, name })
}

/// Read this machine's id, creating it on first start.
pub fn load_or_create() -> Identity {
    let path = id_path();
    if let Ok(text) = fs::read_to_string(&path) {
        if let Some(id) = parse(&text) {
            return id;
        }
    }
    let id = Identity { id: random_u128(), name: String::new() };
    let id = Identity { name: derive_name(id.id), ..id };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, format!("{} {}\n", id.hex(), id.name));
    id
}

/// Two boxes can only collide on the short prefix, and then both know it: the
/// higher id widens its name and the other keeps its own, so the fleet converges
/// without a negotiation round.
pub fn disambiguate(mine: &Identity, rival_id: u128, rival_name: &str) -> String {
    if rival_id == mine.id || rival_name != mine.name {
        return mine.name.clone();
    }
    if mine.id < rival_id {
        return mine.name.clone();
    }
    let mut h = hash::Sha256::new();
    h.update(&mine.id.to_be_bytes());
    let d = h.finish();
    format!("{}{}", mine.name, base32(&d[6..9], 3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_stable_and_unique_enough() {
        let mut seen = std::collections::BTreeSet::new();
        for i in 0..2_000u128 {
            let id = i.wrapping_mul(0x9e37_79b9_7f4a_7c15_1325_43a1_b2c3_d4e5) + 1;
            let named = derive_name(id);
            assert_eq!(named, derive_name(id), "name must be deterministic");
            let parts: Vec<&str> = named.split('-').collect();
            assert_eq!(parts.len(), 3, "{} is not code-word-word", named);
            assert_eq!(parts[0].len(), 5);
            seen.insert(named);
        }
        // 2000 names over 32^5 prefixes: collisions should be vanishingly rare.
        assert!(seen.len() > 1_995, "only {} distinct names", seen.len());
    }

    #[test]
    fn id_round_trips_through_the_file_format() {
        let id = Identity { id: 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef, name: String::new() };
        let id = Identity { name: derive_name(id.id), ..id };
        let text = format!("{} {}\n", id.hex(), id.name);
        assert_eq!(parse(&text).unwrap(), id);
        assert!(parse("").is_none());
        assert!(parse("nothex this").is_none());
        assert!(parse("00000000000000000000000000000000 zero").is_none());
        // A missing name is derived rather than rejected.
        assert_eq!(parse(&id.hex()).unwrap().name, id.name);
    }

    #[test]
    fn a_collision_widens_only_the_higher_id() {
        let low = Identity { id: 5, name: "k7f3q-amber-fox".into() };
        let high = Identity { id: 9, name: "k7f3q-amber-fox".into() };
        assert_eq!(disambiguate(&low, high.id, &high.name), low.name);
        let widened = disambiguate(&high, low.id, &low.name);
        assert_ne!(widened, high.name);
        assert!(widened.starts_with(&high.name));
        // Different names never trigger a rename.
        assert_eq!(disambiguate(&high, low.id, "other"), high.name);
    }

    #[test]
    fn the_key_is_stable_and_separates_ids() {
        let a = Identity { id: 1, name: "a".into() };
        let b = Identity { id: 2, name: "b".into() };
        assert_eq!(a.key(), a.key());
        assert_ne!(a.key(), b.key());
    }
}
