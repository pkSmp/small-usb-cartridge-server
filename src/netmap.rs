//! The whole network on one page, as plain text, on every box.
//!
//! Each node renders this file from inputs it already has: the beacon list, the
//! release blueprint and the ownership plan. Because those inputs are the same
//! everywhere, the text is the same everywhere too — no controller writes it,
//! nothing has to be kept in step, and a box that was switched off for a week
//! catches up by rendering it again.
//!
//! It answers, in one read, what a person actually asks when something is
//! broken: which computers are here, what are they called, what epoch are they
//! serving, how loaded are they, which files the cartridge had, and which
//! computer holds each chunk. It is uncompressed on purpose: it is the file to
//! read with `cat` and to check a restore against.
//!
//! Format (one record per line, fields separated by single spaces, no spaces
//! inside a field — names are `k7f3q-amber-fox`, content types use `_`):
//!
//! ```text
//! CARTNET 1
//! epoch 7
//! root <merkle sha256, or - before the first release>
//! node <id> <name> <ip> <epoch> <ready> <busy> <weight> <gpu> <ram_mb> <disk_free_mb> <owned_chunks> <owned_bytes> <self|peer>
//! file <len> <content_type> <path>
//! own <chunk sha256> <len> <node id>[,<node id>]
//! ```

use crate::blueprint::Blueprint;
use std::collections::BTreeMap;

pub const MAGIC: &str = "CARTNET 1";

/// One computer, as the map describes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Box {
    pub id: u128,
    pub name: String,
    pub ip: String,
    pub epoch: u64,
    pub ready: u64,
    pub busy: u32,
    pub weight: u32,
    pub gpu: u8,
    pub ram_mb: u64,
    pub disk_free_mb: u64,
    /// Chunks this box is responsible for keeping, from the ownership plan.
    pub owned_chunks: usize,
    pub owned_bytes: u64,
    pub me: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Map {
    pub epoch: u64,
    pub root: String,
    pub boxes: Vec<Box>,
    /// (length, content type, path) for every file the release carries.
    pub files: Vec<(u64, String, String)>,
    /// (chunk hash, length, the ids of the computers that should hold it).
    pub owned: Vec<(String, u64, Vec<u128>)>,
}

impl Map {
    /// How many chunks `id` is responsible for, from the `own` lines.
    pub fn owned_by(&self, id: u128) -> usize {
        self.owned.iter().filter(|(_, _, ids)| ids.contains(&id)).count()
    }
}

/// Render the map. Sorted by id, path and hash, so two boxes that agree on the
/// inputs produce byte-identical text and `cmp` is a valid cluster check.
pub fn render(
    epoch: u64,
    root: &str,
    boxes: &[Box],
    bp: &Blueprint,
    owned: &[(String, u64, Vec<u128>)],
) -> String {
    let mut boxes = boxes.to_vec();
    boxes.sort_by_key(|b| b.id);
    let mut files: Vec<(u64, String, String)> = bp
        .files
        .iter()
        .map(|f| (f.len, f.ctype.clone(), f.path.clone()))
        .collect();
    files.sort_by(|a, b| a.2.cmp(&b.2));
    let mut owned = owned.to_vec();
    owned.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::with_capacity(128 + boxes.len() * 160 + files.len() * 80 + owned.len() * 100);
    out.push_str(MAGIC);
    out.push('\n');
    out.push_str(&format!("epoch {}\n", epoch));
    out.push_str(&format!("root {}\n", if root.is_empty() { "-" } else { root }));
    for b in &boxes {
        out.push_str(&format!(
            "node {:032x} {} {} {} {} {} {} {} {} {} {} {} {}\n",
            b.id,
            b.name,
            b.ip,
            b.epoch,
            b.ready,
            b.busy,
            b.weight,
            b.gpu,
            b.ram_mb,
            b.disk_free_mb,
            b.owned_chunks,
            b.owned_bytes,
            if b.me { "self" } else { "peer" }
        ));
    }
    for (len, ctype, path) in &files {
        out.push_str(&format!("file {} {} {}\n", len, ctype.replace(' ', "_"), path));
    }
    for (hash, len, ids) in &owned {
        let list: Vec<String> = ids.iter().map(|id| format!("{:032x}", id)).collect();
        out.push_str(&format!("own {} {} {}\n", hash, len, list.join(",")));
    }
    out
}

pub fn parse(text: &str) -> Option<Map> {
    let mut lines = text.lines();
    if lines.next()? != MAGIC {
        return None;
    }
    let epoch = lines.next()?.strip_prefix("epoch ")?.trim().parse().ok()?;
    let root = lines.next()?.strip_prefix("root ")?.trim().to_string();
    let mut map = Map { epoch, root, ..Default::default() };
    for line in lines {
        if let Some(rest) = line.strip_prefix("node ") {
            let f: Vec<&str> = rest.split_whitespace().collect();
            if f.len() != 13 {
                return None;
            }
            map.boxes.push(Box {
                id: u128::from_str_radix(f[0], 16).ok()?,
                name: f[1].to_string(),
                ip: f[2].to_string(),
                epoch: f[3].parse().ok()?,
                ready: f[4].parse().ok()?,
                busy: f[5].parse().ok()?,
                weight: f[6].parse().ok()?,
                gpu: f[7].parse().ok()?,
                ram_mb: f[8].parse().ok()?,
                disk_free_mb: f[9].parse().ok()?,
                owned_chunks: f[10].parse().ok()?,
                owned_bytes: f[11].parse().ok()?,
                me: f[12] == "self",
            });
        } else if let Some(rest) = line.strip_prefix("file ") {
            let mut it = rest.splitn(3, ' ');
            let len = it.next()?.parse().ok()?;
            let ctype = it.next()?.replace('_', " ");
            let path = it.next()?.to_string();
            map.files.push((len, ctype, path));
        } else if let Some(rest) = line.strip_prefix("own ") {
            let mut it = rest.splitn(3, ' ');
            let hash = it.next()?.to_string();
            let len = it.next()?.parse().ok()?;
            let mut ids = Vec::new();
            for part in it.next()?.split(',') {
                ids.push(u128::from_str_radix(part, 16).ok()?);
            }
            map.owned.push((hash, len, ids));
        } else if line.trim().is_empty() {
            continue;
        } else {
            return None;
        }
    }
    Some(map)
}

/// The per-box totals the map prints, counted once from the ownership plan.
pub fn totals(owned: &[(String, u64, Vec<u128>)]) -> BTreeMap<u128, (usize, u64)> {
    let mut out: BTreeMap<u128, (usize, u64)> = BTreeMap::new();
    for (_, len, ids) in owned {
        for id in ids {
            let slot = out.entry(*id).or_insert((0, 0));
            slot.0 += 1;
            slot.1 += len;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blueprint::{FileEntry, Piece};

    fn blueprint() -> Blueprint {
        let chunk_a = "aa".repeat(32);
        let chunk_b = "bb".repeat(32);
        let chunk_c = "cc".repeat(32);
        Blueprint {
            epoch: 9,
            root: "ff".repeat(32),
            files: vec![
                FileEntry {
                    path: "3000/index.html".into(),
                    len: 12,
                    ctype: "text/html".into(),
                    pieces: vec![Piece { hash: chunk_a.clone(), len: 12 }],
                },
                FileEntry {
                    path: "8080/big.bin".into(),
                    len: 30,
                    ctype: "application/octet-stream".into(),
                    pieces: vec![
                        Piece { hash: chunk_b.clone(), len: 20 },
                        Piece { hash: chunk_c.clone(), len: 10 },
                    ],
                },
            ],
            chunks: vec![
                Piece { hash: chunk_a.clone(), len: 12 },
                Piece { hash: chunk_b.clone(), len: 20 },
                Piece { hash: chunk_c.clone(), len: 10 },
            ],
        }
    }

    fn boxes() -> Vec<Box> {
        vec![
            Box {
                id: 0x11,
                name: "k7f3q-amber-fox".into(),
                ip: "192.168.1.173".into(),
                epoch: 9,
                ready: 9,
                busy: 2,
                weight: 4,
                gpu: 1,
                ram_mb: 11_600,
                disk_free_mb: 22_800,
                owned_chunks: 2,
                owned_bytes: 32,
                me: true,
            },
            Box {
                id: 0x22,
                name: "b2x9m-slate-otter".into(),
                ip: "192.168.1.174".into(),
                epoch: 9,
                ready: 9,
                busy: 0,
                weight: 3,
                gpu: 0,
                ram_mb: 8_000,
                disk_free_mb: 40_000,
                owned_chunks: 1,
                owned_bytes: 12,
                me: false,
            },
        ]
    }

    #[test]
    fn the_map_round_trips() {
        let bp = blueprint();
        let owned = vec![
            ("aa".repeat(32), 12u64, vec![0x11u128]),
            ("bb".repeat(32), 20, vec![0x11, 0x22]),
            ("cc".repeat(32), 10, vec![0x22]),
        ];
        let text = render(bp.epoch, &bp.root, &boxes(), &bp, &owned);
        assert!(text.starts_with("CARTNET 1\nepoch 9\n"));
        let map = parse(&text).expect("map must parse");
        assert_eq!(map.epoch, 9);
        assert_eq!(map.boxes.len(), 2);
        assert_eq!(map.files.len(), 2);
        assert_eq!(map.owned.len(), 3);
        assert_eq!(map.files[0].2, "3000/index.html");
        assert_eq!(map.boxes[0].name, "k7f3q-amber-fox");
        assert!(map.boxes[0].me);
        assert!(!map.boxes[1].me);
        assert_eq!(map.owned_by(0x22), 2);
    }

    #[test]
    fn the_text_does_not_depend_on_member_order() {
        let bp = blueprint();
        let owned = vec![
            ("bb".repeat(32), 20u64, vec![0x11u128, 0x22]),
            ("aa".repeat(32), 12, vec![0x11]),
        ];
        let mut flipped = owned.clone();
        flipped.reverse();
        let mut members = boxes();
        members.reverse();
        let a = render(bp.epoch, &bp.root, &members, &bp, &owned);
        let b = render(bp.epoch, &bp.root, &boxes(), &bp, &flipped);
        assert_eq!(a, b, "every box must render byte-identical text");
    }

    #[test]
    fn totals_count_each_boxs_share() {
        let owned = vec![
            ("aa".repeat(32), 12u64, vec![0x11u128]),
            ("bb".repeat(32), 20, vec![0x11, 0x22]),
        ];
        let t = totals(&owned);
        assert_eq!(t[&0x11], (2, 32));
        assert_eq!(t[&0x22], (1, 20));
    }

    #[test]
    fn a_foreign_or_damaged_file_is_rejected() {
        assert!(parse("").is_none());
        assert!(parse("hello\n").is_none());
        assert!(parse("CARTNET 1\nepoch x\nroot -\n").is_none());
        // A node line with a field missing must not be half-accepted.
        assert!(parse("CARTNET 1\nepoch 1\nroot -\nnode 11 a 1.2.3.4\n").is_none());
    }
}
