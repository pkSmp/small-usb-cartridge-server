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
    build_with_map(bp, store, path, "")
}

/// As `build`, but with the fleet's network map stored in the clear beside the
/// blueprint: a restore then brings back what the network looked like — the
/// computers, their names and who held which chunk — not only its bytes.
///
pub fn build_with_map(
    bp: &Blueprint,
    store: &Store,
    path: &Path,
    map: &str,
) -> io::Result<ArchiveReport> {
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
    out.extend_from_slice(format!("netmap {}\n", map.len()).as_bytes());
    out.extend_from_slice(format!("data {}\n", packed.len()).as_bytes());
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(map.as_bytes());
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
    let (bp, chunks, _map) = restore_full(path)?;
    Ok((bp, chunks))
}

/// Everything an archive carries: the blueprint, every verified chunk, and the
/// network map as it stood when the archive was written.
pub fn restore_full(path: &Path) -> io::Result<(Blueprint, Vec<(String, Vec<u8>)>, String)> {
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
    // Archives written before the network map was carried have `data` here;
    // newer ones put `netmap` first. Both are read.
    let next_line = read_line(&buf, &mut pos).ok_or_else(|| bad("missing data size"))?;
    let next = std::str::from_utf8(next_line).map_err(|_| bad("header is not text"))?;
    let mut data_len: usize;
    let netmap_len: usize = match next.strip_prefix("netmap ") {
        Some(rest) => {
            let n: usize = rest.trim().parse().map_err(|_| bad("bad netmap size"))?;
            let data_line = read_line(&buf, &mut pos).ok_or_else(|| bad("missing data size"))?;
            let d: usize = std::str::from_utf8(data_line)
                .ok()
                .and_then(|s| s.strip_prefix("data "))
                .and_then(|s| s.trim().parse().ok())
                .ok_or_else(|| bad("bad data size"))?;
            data_len = d;
            n
        }
        None => {
            data_len = next
                .strip_prefix("data ")
                .and_then(|s| s.trim().parse().ok())
                .ok_or_else(|| bad("bad data size"))?;
            0
        }
    };
    if pos + text_len + netmap_len + data_len > buf.len() {
        return Err(bad("archive is truncated"));
    }
    let text = std::str::from_utf8(&buf[pos..pos + text_len]).map_err(|_| bad("blueprint is not text"))?;
    let bp = Blueprint::parse(text).ok_or_else(|| bad("blueprint is corrupt"))?;
    if bp.epoch != epoch || bp.root != root {
        return Err(bad("archive header disagrees with its blueprint"));
    }
    pos += text_len;
    let map = std::str::from_utf8(&buf[pos..pos + netmap_len])
        .map_err(|_| bad("network map is not text"))?
        .to_string();
    pos += netmap_len;
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
    Ok((bp, chunks, map))
}

/// A real `.zip` of the release: the network map, the blueprint and every site
/// file, stored rather than deflated. The point of this file is a backup that a
/// person can open anywhere without this program, and that restores without
/// decoding anything — the solid `.cart` archive already covers size.
///
pub fn write_zip(entries: &[(String, Vec<u8>)], path: &Path) -> io::Result<u64> {
    fn le16(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn le32(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    let mut out: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    let mut count: u16 = 0;
    for (name, data) in entries {
        if data.len() as u64 > u32::MAX as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "entry is too large for a zip"));
        }
        let crc = crate::png::crc32(data);
        let offset = out.len() as u32;
        le32(&mut out, 0x0403_4b50);
        le16(&mut out, 20); // version needed
        le16(&mut out, 0x0800); // names are UTF-8
        le16(&mut out, 0); // stored, not deflated
        le16(&mut out, 0); // time
        le16(&mut out, 0x21); // date: 1980-01-01
        le32(&mut out, crc);
        le32(&mut out, data.len() as u32);
        le32(&mut out, data.len() as u32);
        le16(&mut out, name.len() as u16);
        le16(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        le32(&mut central, 0x0201_4b50);
        le16(&mut central, 20);
        le16(&mut central, 20);
        le16(&mut central, 0x0800);
        le16(&mut central, 0);
        le16(&mut central, 0);
        le16(&mut central, 0x21);
        le32(&mut central, crc);
        le32(&mut central, data.len() as u32);
        le32(&mut central, data.len() as u32);
        le16(&mut central, name.len() as u16);
        le16(&mut central, 0); // extra
        le16(&mut central, 0); // comment
        le16(&mut central, 0); // disk
        le16(&mut central, 0); // internal attributes
        le32(&mut central, 0); // external attributes
        le32(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
        count += 1;
    }
    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);
    le32(&mut out, 0x0605_4b50);
    le16(&mut out, 0);
    le16(&mut out, 0);
    le16(&mut out, count);
    le16(&mut out, count);
    le32(&mut out, cd_size);
    le32(&mut out, cd_offset);
    le16(&mut out, 0);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, &out)?;
    Ok(out.len() as u64)
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

    fn scratch(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "cartridge-arc-{}-{}-{}",
            tag,
            std::process::id(),
            crate::ident::random_u128() & 0xffff
        ));
        let _ = fs::remove_dir_all(&base);
        let _ = fs::create_dir_all(&base);
        base
    }

    #[test]
    fn the_network_map_rides_along_and_comes_back() {
        let base = scratch("map");
        fs::create_dir_all(base.join("rel/3000")).unwrap();
        fs::write(base.join("rel/3000/index.html"), b"<h1>map me</h1>").unwrap();
        let store = Store::at(base.join("store"));
        let (bp, _) = chunks::ingest(&base.join("rel"), &store, None, 5).unwrap();
        let map = "CARTNET 1\nepoch 5\nroot x\nnode 00000000000000000000000000000011 a 1.2.3.4 5 5 0 1 0 100 200 1 17 self\n";
        let path = base.join("a.cart");
        build_with_map(&bp, &store, &path, map).unwrap();
        let (back, chunks_back, map_back) = restore_full(&path).unwrap();
        assert_eq!(back, bp);
        assert_eq!(chunks_back.len(), bp.chunks.len());
        assert_eq!(map_back, map, "the map must survive the round trip verbatim");
        // The old two-argument build still writes an archive that restores.
        let plain = base.join("b.cart");
        build(&bp, &store, &plain).unwrap();
        let (_, _, no_map) = restore_full(&plain).unwrap();
        assert!(no_map.is_empty());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn the_zip_is_a_real_zip_of_stored_entries() {
        let base = scratch("zip");
        let entries = vec![
            ("netmap.txt".to_string(), b"CARTNET 1\nepoch 1\n".to_vec()),
            ("3000/index.html".to_string(), b"<h1>zip</h1>".to_vec()),
        ];
        let path = base.join("network-1.zip");
        let bytes = write_zip(&entries, &path).unwrap();
        let raw = fs::read(&path).unwrap();
        assert_eq!(bytes, raw.len() as u64);
        assert_eq!(&raw[..4], b"PK\x03\x04", "local file header");
        // Every entry is stored, and its CRC is the one the zip format expects.
        let mut at = 0usize;
        for (name, data) in &entries {
            assert_eq!(&raw[at..at + 4], b"PK\x03\x04");
            let method = u16::from_le_bytes([raw[at + 8], raw[at + 9]]);
            assert_eq!(method, 0, "stored, not deflated");
            let crc = u32::from_le_bytes([raw[at + 14], raw[at + 15], raw[at + 16], raw[at + 17]]);
            assert_eq!(crc, crate::png::crc32(data));
            let size = u32::from_le_bytes([raw[at + 22], raw[at + 23], raw[at + 24], raw[at + 25]]) as usize;
            let name_len = u16::from_le_bytes([raw[at + 26], raw[at + 27]]) as usize;
            assert_eq!(name_len, name.len());
            let body = at + 30 + name_len;
            assert_eq!(&raw[body..body + size], &data[..]);
            at = body + size;
        }
        // Central directory, then the end-of-central-directory record.
        assert_eq!(&raw[at..at + 4], b"PK\x01\x02");
        let eocd = raw.len() - 22;
        assert_eq!(&raw[eocd..eocd + 4], b"PK\x05\x06");
        let count = u16::from_le_bytes([raw[eocd + 10], raw[eocd + 11]]);
        assert_eq!(count as usize, entries.len());
        let _ = fs::remove_dir_all(&base);
    }
}
