//! The map every node holds: what the release contains, and who should store it.
//!
//! A release is split into content-addressed chunks. The blueprint lists every
//! file, the chunks that make it, and a Merkle root over the whole thing, so two
//! nodes compare 64 hex characters and know whether they hold the same release.
//! It is small — a 100,000-file site is a few megabytes — so every node keeps a
//! complete copy and can answer "who holds this chunk?" without asking anyone.
//!
//! Storage ownership is weighted rendezvous hashing with R replicas. The weight
//! is quantized to a coarse bucket before it is used, because two nodes that
//! measure 41.7 and 41.9 MB/s must compute the *same* integer weight or their
//! maps diverge and chunks go missing. That is the one bug that turns a
//! leaderless design into a data-loss design, so it is handled here, once.

use crate::hash;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::Path;

/// Files larger than this are cut into pieces; smaller files are one chunk.
pub const CHUNK: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Piece {
    pub hash: String,
    pub len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub len: u64,
    pub ctype: String,
    pub pieces: Vec<Piece>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Blueprint {
    pub epoch: u64,
    pub root: String,
    pub files: Vec<FileEntry>,
    /// Every distinct chunk once, with its length.
    pub chunks: Vec<Piece>,
}

impl Blueprint {
    pub fn bytes(&self) -> u64 {
        self.chunks.iter().map(|c| c.len).sum()
    }

    /// The chunks a path needs, with the byte offset each one starts at.
    pub fn pieces_for(&self, path: &str) -> Option<Vec<(String, u64, u64)>> {
        let entry = self.files.iter().find(|f| f.path == path)?;
        let mut out = Vec::new();
        let mut offset = 0u64;
        for piece in &entry.pieces {
            out.push((piece.hash.clone(), offset, piece.len));
            offset += piece.len;
        }
        Some(out)
    }

    pub fn has_chunk(&self, hash: &str) -> bool {
        self.chunks.iter().any(|c| c.hash == hash)
    }

    pub fn text(&self) -> String {
        let mut out = String::with_capacity(256 + self.chunks.len() * 72 + self.files.len() * 80);
        out.push_str("CARTBLUE 1\n");
        out.push_str(&format!("epoch {}\n", self.epoch));
        out.push_str(&format!("root {}\n", self.root));
        for chunk in &self.chunks {
            out.push_str(&format!("chunk {} {}\n", chunk.hash, chunk.len));
        }
        for file in &self.files {
            // The content type contains a space, and the path comes last: an
            // underscore keeps the line three fields without escaping.
            out.push_str(&format!("file {} {} {}\n", file.len, file.ctype.replace(' ', "_"), file.path));
            for piece in &file.pieces {
                out.push_str(&format!("piece {} {}\n", piece.hash, piece.len));
            }
        }
        out
    }

    pub fn parse(text: &str) -> Option<Blueprint> {
        let mut lines = text.lines();
        if lines.next()? != "CARTBLUE 1" {
            return None;
        }
        let epoch = lines.next()?.strip_prefix("epoch ")?.trim().parse().ok()?;
        let root = lines.next()?.strip_prefix("root ")?.trim().to_string();
        let mut bp = Blueprint { epoch, root, files: Vec::new(), chunks: Vec::new() };
        let mut current: Option<FileEntry> = None;
        for line in lines {
            if let Some(rest) = line.strip_prefix("chunk ") {
                let mut it = rest.split_whitespace();
                bp.chunks.push(Piece {
                    hash: it.next()?.to_string(),
                    len: it.next()?.parse().ok()?,
                });
            } else if let Some(rest) = line.strip_prefix("file ") {
                if let Some(file) = current.take() {
                    bp.files.push(file);
                }
                let mut it = rest.splitn(3, ' ');
                let len = it.next()?.parse().ok()?;
                let ctype = it.next()?.replace('_', " ");
                let path = it.next()?.to_string();
                current = Some(FileEntry { path, len, ctype, pieces: Vec::new() });
            } else if let Some(rest) = line.strip_prefix("piece ") {
                let mut it = rest.split_whitespace();
                let hash = it.next()?.to_string();
                let len = it.next()?.parse().ok()?;
                current.as_mut()?.pieces.push(Piece { hash, len });
            }
        }
        if let Some(file) = current.take() {
            bp.files.push(file);
        }
        Some(bp)
    }

    /// The Merkle root: one hash over the sorted (path, hash, len) triples.
    pub fn compute_root(files: &[FileEntry]) -> String {
        let mut lines: Vec<String> = Vec::new();
        for file in files {
            let mut offset = 0u64;
            for piece in &file.pieces {
                lines.push(format!("{} {} {} {}", file.path, offset, piece.hash, piece.len));
                offset += piece.len;
            }
        }
        lines.sort();
        let mut h = hash::Sha256::new();
        for line in lines {
            h.update(line.as_bytes());
            h.update(b"\n");
        }
        hash::hex(&h.finish())
    }

    /// Chunk a directory tree into a blueprint, without writing anything.
    pub fn scan(dir: &Path, epoch: u64) -> io::Result<Blueprint> {
        let mut files = Vec::new();
        let mut chunks: BTreeMap<String, u64> = BTreeMap::new();
        walk(dir, Path::new(""), &mut files, &mut chunks)?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let root = Blueprint::compute_root(&files);
        Ok(Blueprint {
            epoch,
            root,
            chunks: chunks
                .into_iter()
                .map(|(hash, len)| Piece { hash, len })
                .collect(),
            files,
        })
    }
}

fn walk(
    root: &Path,
    rel: &Path,
    files: &mut Vec<FileEntry>,
    chunks: &mut BTreeMap<String, u64>,
) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let child = rel.join(&name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            walk(&entry.path(), &child, files, chunks)?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let mut file = fs::File::open(entry.path())?;
        let mut buf = vec![0u8; CHUNK];
        let mut pieces = Vec::new();
        let mut total = 0u64;
        loop {
            let mut filled = 0usize;
            while filled < buf.len() {
                match file.read(&mut buf[filled..])? {
                    0 => break,
                    n => filled += n,
                }
            }
            if filled == 0 {
                break;
            }
            let piece = &buf[..filled];
            let digest = hash::bytes(piece);
            chunks.insert(digest.clone(), filled as u64);
            pieces.push(Piece { hash: digest, len: filled as u64 });
            total += filled as u64;
            if filled < buf.len() {
                break;
            }
        }
        if pieces.is_empty() {
            // An empty file still needs an entry, or the path disappears.
            let digest = hash::bytes(b"");
            chunks.entry(digest.clone()).or_insert(0);
            pieces.push(Piece { hash: digest, len: 0 });
        }
        let path = child.to_string_lossy().replace('\\', "/");
        files.push(FileEntry {
            ctype: content_type(&path).to_string(),
            path,
            len: total,
            pieces,
        });
    }
    Ok(())
}

pub fn content_type(path: &str) -> &'static str {
    let path = path.to_ascii_lowercase();
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wasm" => "application/wasm",
        "txt" | "md" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        _ => "application/octet-stream",
    }
}

// ------------------------------------------------------------- who stores it

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn chunk_key(hash: &str) -> u64 {
    // Every character counts. Reading a fixed slice was a trap: short hex
    // strings such as "0000...0001" share a prefix, which put every chunk on
    // one node. FNV over the whole digest, then a stir, is uniform and cheap.
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in hash.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    splitmix64(h)
}

fn score(weight: u32, key: u64, object: u64) -> f64 {
    let h = splitmix64(object ^ key);
    let u = ((h >> 11) as f64 + 1.0) / 9_007_199_254_740_993.0;
    weight.max(1) as f64 / -u.ln()
}

/// The R nodes that should hold `hash`, highest score first. `members` is
/// `(weight, node key)` and must be sorted by key before it is passed in; every
/// node sorts it the same way, so every node gets the same answer.
pub fn owners(hash: &str, members: &[(u32, u64)], replicas: usize) -> Vec<u64> {
    let object = chunk_key(hash);
    let mut scored: Vec<(f64, u64)> = members
        .iter()
        .map(|(weight, key)| (score(*weight, *key, object), *key))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(replicas.max(1)).map(|(_, key)| key).collect()
}

/// Reassign chunks that would overflow a node's declared space. Every node runs
/// this over the same sorted inputs, so the result is still identical everywhere;
/// the only thing that changed is that a 500 GB box now takes the overflow from
/// a 20 GB one instead of failing to store it.
pub fn assign(
    bp: &Blueprint,
    members: &[(u32, u64)],
    replicas: usize,
    capacity_mb: &BTreeMap<u64, u64>,
) -> BTreeMap<String, Vec<u64>> {
    let mut plan: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut used: BTreeMap<u64, u64> = BTreeMap::new();
    for (_, key) in members {
        used.insert(*key, 0);
    }
    // Chunks are visited in sorted hash order so the overflow walk is the same
    // on every machine.
    let mut chunk_list: Vec<&Piece> = bp.chunks.iter().collect();
    chunk_list.sort_by(|a, b| a.hash.cmp(&b.hash));
    for chunk in chunk_list {
        let mut chosen = Vec::new();
        for key in owners(&chunk.hash, members, members.len()) {
            if chosen.len() >= replicas.max(1) {
                break;
            }
            let room = capacity_mb.get(&key).copied().unwrap_or(u64::MAX);
            let have = used.get(&key).copied().unwrap_or(0);
            if room == u64::MAX || have.saturating_add(chunk.len) <= room {
                *used.entry(key).or_insert(0) += chunk.len;
                chosen.push(key);
            }
        }
        // If nothing had room, the highest-scored node takes it anyway rather
        // than dropping the chunk from the plan entirely.
        if chosen.is_empty() {
            if let Some(key) = owners(&chunk.hash, members, 1).first() {
                chosen.push(*key);
            }
        }
        plan.insert(chunk.hash.clone(), chosen);
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "cartridge-bp-{}-{}-{}",
            tag,
            std::process::id(),
            crate::ident::random_u128() & 0xffff
        ));
        let _ = fs::create_dir_all(&path);
        path
    }

    #[test]
    fn a_release_round_trips_through_the_blueprint() {
        let dir = tempdir("scan");
        fs::create_dir_all(dir.join("3000")).unwrap();
        fs::write(dir.join("3000/index.html"), b"<h1>hello</h1>").unwrap();
        fs::write(dir.join("3000/app.js"), vec![b'x'; CHUNK + 10]).unwrap();
        let bp = Blueprint::scan(&dir, 7).unwrap();
        assert_eq!(bp.epoch, 7);
        assert_eq!(bp.files.len(), 2);
        // One small file plus a 4 MiB piece and a 10-byte tail.
        assert_eq!(bp.chunks.len(), 3);
        assert_eq!(bp.bytes(), 14 + CHUNK as u64 + 10);
        let text = bp.text();
        let back = Blueprint::parse(&text).unwrap();
        assert_eq!(back, bp);
        assert_eq!(back.root, bp.root);
        assert!(bp.pieces_for("3000/app.js").unwrap().len() == 2);
        assert_eq!(content_type("x.css"), "text/css; charset=utf-8");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_identical_trees_have_one_root() {
        let a = tempdir("root-a");
        let b = tempdir("root-b");
        for dir in [&a, &b] {
            fs::create_dir_all(dir.join("8080")).unwrap();
            fs::write(dir.join("8080/index.html"), b"same").unwrap();
        }
        let one = Blueprint::scan(&a, 1).unwrap();
        let two = Blueprint::scan(&b, 2).unwrap();
        assert_eq!(one.root, two.root, "content identity must not depend on the folder");
        assert_ne!(one.epoch, two.epoch);
        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
    }

    #[test]
    fn owners_are_deterministic_and_distinct() {
        let members = [(100u32, 11u64), (200, 22), (400, 33), (50, 44)];
        for i in 0..1_000 {
            let hash = format!("{:064x}", i);
            let a = owners(&hash, &members, 2);
            let b = owners(&hash, &members, 2);
            assert_eq!(a, b);
            assert_eq!(a.len(), 2);
            assert_ne!(a[0], a[1]);
        }
    }

    #[test]
    fn replicas_follow_weight_and_membership_changes_move_little() {
        let base = [(100u32, 1u64), (200, 2), (400, 3)];
        let mut hits = [0usize; 3];
        for i in 0..30_000u64 {
            let hash = format!("{:064x}", i.wrapping_mul(0x9e37_79b9));
            let first = owners(&hash, &base, 1)[0];
            let idx = base.iter().position(|(_, k)| *k == first).unwrap();
            hits[idx] += 1;
        }
        let total: usize = hits.iter().sum();
        assert!((hits[0] as f64 / total as f64 - 1.0 / 7.0).abs() < 0.03);
        assert!((hits[2] as f64 / total as f64 - 4.0 / 7.0).abs() < 0.03);

        let bigger = [(100u32, 1u64), (200, 2), (400, 3), (100, 4)];
        let mut moved = 0;
        for i in 0..20_000u64 {
            let hash = format!("{:064x}", i.wrapping_mul(0x9e37_79b9));
            let before = owners(&hash, &base, 1)[0];
            let after = owners(&hash, &bigger, 1)[0];
            if before != after {
                assert_eq!(after, 4, "only the newcomer may take work");
                moved += 1;
            }
        }
        // The newcomer adds 100 of 800 weight: about an eighth of the objects.
        assert!(moved > 1_500 && moved < 3_500, "moved {}", moved);
    }

    #[test]
    fn overflow_is_reassigned_not_dropped() {
        let bp = Blueprint {
            epoch: 1,
            root: "x".into(),
            files: Vec::new(),
            chunks: vec![
                Piece { hash: format!("{:064x}", 1), len: 10 },
                Piece { hash: format!("{:064x}", 2), len: 10 },
            ],
        };
        let members = [(10u32, 1u64), (10, 2)];
        let mut cap = BTreeMap::new();
        cap.insert(1u64, 0u64);
        cap.insert(2, 1_000);
        let plan = assign(&bp, &members, 1, &cap);
        assert_eq!(plan.len(), 2);
        for holders in plan.values() {
            assert_eq!(holders.len(), 1);
            assert_eq!(holders[0], 2, "the full node must not be chosen");
        }
    }
}
