//! The disaster-recovery archive: one solid, compressed copy of everything.
//!
//! Replication keeps R copies of each chunk and erasure shards survive the loss
//! of any m nodes, but both are spread across the fleet. If enough machines stay
//! off, the last line of defence is a single self-contained file that any one
//! survivor can read: all chunks once (so identical files cost nothing extra),
//! concatenated and deflated as one stream, with the blueprint inside it.
//!
//! It is built by the idle class only. A 300 MB release typically lands around
//! 90 MB, which is small enough that even a low-disk machine can keep a slice of
//! it and useful enough to rebuild every site from nothing but this file.

use crate::blueprint::Blueprint;
use crate::chunks::Store;
use crate::gzip;
use std::fs;
use std::io;
use std::path::Path;

pub const MAGIC: &str = "CARTARC 1";

pub struct ArchiveReport {
    pub path: std::path::PathBuf,
    pub raw: u64,
    pub packed: u64,
    pub chunks: usize,
}

fn read_line<'a>(buf: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let rest = &buf[*pos..];
    let end = rest.iter().position(|b| *b == b'\n')?;
    let line = &rest[..end];
    *pos += end + 1;
    Some(line)
}

pub fn build(bp: &Blueprint, store: &Store, path: &Path) -> io::Result<ArchiveReport> {
    let mut raw = Vec::new();
    for chunk in &bp.chunks {
        let bytes = store.get(&chunk.hash)?;
        raw.extend_from_slice(&bytes);
    }
    let packed = gzip::gzip(&raw);
    let text = bp.text();

    let mut out = Vec::with_capacity(packed.len() + text.len() + 96);
    out.extend_from_slice(MAGIC.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(format!("epoch {}\nroot {}\n", bp.epoch, bp.root).as_bytes());
    out.extend_from_slice(format!("blueprint {}\n", text.len()).as_bytes());
    out.extend_from_slice(format!("data {}\n", packed.len()).as_bytes());
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(&packed);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, &out)?;
    Ok(ArchiveReport {
        path: path.to_path_buf(),
        raw: raw.len() as u64,
        packed: packed.len() as u64,
        chunks: bp.chunks.len(),
    })
}

/// Read an archive back into a blueprint and verified chunk bytes. Nothing is
/// written to disk here, so a caller can restore into a fresh store or stream
/// the chunks to peers.
pub fn restore(path: &Path) -> io::Result<(Blueprint, Vec<(String, Vec<u8>)>)> {
    let buf = fs::read(path)?;
    let mut pos = 0usize;
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());

    let magic = read_line(&buf, &mut pos).ok_or_else(|| bad("archive is empty"))?;
    if magic != MAGIC.as_bytes() {
        return Err(bad("not a cartridge archive"));
    }
    let epoch_line = read_line(&buf, &mut pos).ok_or_else(|| bad("missing epoch"))?;
    let epoch: u64 = std::str::from_utf8(epoch_line)
        .ok()
        .and_then(|s| s.strip_prefix("epoch "))
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| bad("bad epoch"))?;
    let root_line = read_line(&buf, &mut pos).ok_or_else(|| bad("missing root"))?;
    let root = std::str::from_utf8(root_line)
        .ok()
        .and_then(|s| s.strip_prefix("root "))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| bad("bad root"))?;
    let bp_line = read_line(&buf, &mut pos).ok_or_else(|| bad("missing blueprint size"))?;
    let text_len: usize = std::str::from_utf8(bp_line)
        .ok()
        .and_then(|s| s.strip_prefix("blueprint "))
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| bad("bad blueprint size"))?;
    let data_line = read_line(&buf, &mut pos).ok_or_else(|| bad("missing data size"))?;
    let data_len: usize = std::str::from_utf8(data_line)
        .ok()
        .and_then(|s| s.strip_prefix("data "))
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| bad("bad data size"))?;
    if pos + text_len + data_len > buf.len() {
        return Err(bad("archive is truncated"));
    }
    let text = std::str::from_utf8(&buf[pos..pos + text_len]).map_err(|_| bad("blueprint is not text"))?;
    let bp = Blueprint::parse(text).ok_or_else(|| bad("blueprint is corrupt"))?;
    if bp.epoch != epoch || bp.root != root {
        return Err(bad("archive header disagrees with its blueprint"));
    }
    pos += text_len;
    let plain = gzip::inflate_bytes(&buf[pos..pos + data_len])
        .map_err(|e| bad(&format!("cannot decompress: {}", e)))?;

    let mut chunks = Vec::with_capacity(bp.chunks.len());
    let mut at = 0usize;
    for piece in &bp.chunks {
        let end = at + piece.len as usize;
        if end > plain.len() {
            return Err(bad("archive data is shorter than its blueprint"));
        }
        let bytes = plain[at..end].to_vec();
        let digest = crate::hash::bytes(&bytes);
        if digest != piece.hash {
            return Err(bad("chunk hash mismatch while restoring"));
        }
        chunks.push((piece.hash.clone(), bytes));
        at = end;
    }
    Ok((bp, chunks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunks;

    #[test]
    fn a_release_survives_a_full_round_trip() {
        let base = std::env::temp_dir().join(format!(
            "cartridge-arc-{}-{}",
            std::process::id(),
            crate::ident::random_u128() & 0xffff
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("rel/3000")).unwrap();
        let page = b"<h1>archive me</h1>".repeat(400);
        fs::write(base.join("rel/3000/index.html"), &page).unwrap();
        fs::write(base.join("rel/3000/empty.txt"), b"").unwrap();
        let store = Store::at(base.join("store"));
        let (bp, _) = chunks::ingest(&base.join("rel"), &store, None, 3).unwrap();
        let path = base.join("archive.cart");
        let report = build(&bp, &store, &path).unwrap();
        assert_eq!(report.chunks, bp.chunks.len());
        assert!(report.packed < report.raw, "archive should actually compress");

        let (back, bytes) = restore(&path).unwrap();
        assert_eq!(back, bp);
        for (digest, data) in &bytes {
            assert_eq!(&crate::hash::bytes(data), digest);
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn truncated_or_foreign_files_are_rejected() {
        let base = std::env::temp_dir().join(format!(
            "cartridge-arc-bad-{}-{}",
            std::process::id(),
            crate::ident::random_u128() & 0xffff
        ));
        let _ = fs::create_dir_all(&base);
        let path = base.join("x.cart");
        fs::write(&path, b"not an archive").unwrap();
        assert!(restore(&path).is_err());
        fs::write(&path, b"CARTARC 1\nepoch 1\nroot abc\nblueprint 99\ndata 1\n").unwrap();
        assert!(restore(&path).is_err());
        let _ = fs::remove_dir_all(&base);
    }
}
