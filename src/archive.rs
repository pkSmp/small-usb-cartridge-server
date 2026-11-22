//! The disaster-recovery archive: one solid, compressed copy of everything.
//!
//! Replication keeps R copies of each chunk and the fleet keeps full copies on
//! every node, but the last line of defence is a single self-contained file that
//! any one survivor can read: every distinct chunk once (so identical files cost
//! nothing extra), concatenated and deflated as one stream, with the index and
//! the network map inside it.
//!
//! It is built by the idle class only, and it is written for size:
//!
//!   * **One solid deflate stream** over all chunks, so a page that repeats
//!     across files is paid for once.
//!   * **No hashes in the index.** A text blueprint spends 64 characters of hex
//!     on every chunk and the hex is random, so it never compresses: on a
//!     300-file site it was 93% of the archive. The index stores chunk lengths
//!     and the file tree, and a restore recomputes every hash from the bytes it
//!     just inflated.
//!   * **The Merkle root instead.** Those recomputed hashes rebuild the release
//!     root, which the header carries, so a restore verifies the whole tree
//!     against 64 characters of hex. A corrupted archive is refused as a whole
//!     rather than half-restored.
//!   * **A deflated index and network map**, because paths and content types are
//!     repetitive and the hashes are gone.
//!
//! Three formats are read: `CARTARC 1` (text blueprint, per-chunk hex), `CARTARC
//! 2` (binary index with per-chunk hex) and `CARTARC 3` (what this writes).

use crate::blueprint::{Blueprint, FileEntry, Piece};
use crate::chunks::Store;
use crate::gzip;
use std::fs;
use std::io;
use std::path::Path;

pub const MAGIC: &str = "CARTARC 3";
/// Compact binary index, but still carrying every chunk hash.
pub const MAGIC_V2: &str = "CARTARC 2";
/// Text blueprint and map in the clear.
pub const MAGIC_V1: &str = "CARTARC 1";

pub struct ArchiveReport {
    pub path: std::path::PathBuf,
    pub raw: u64,
    /// The deflated data stream alone.
    pub packed: u64,
    /// The whole file: header, index, map and data.
    pub total: u64,
    pub chunks: usize,
}

fn read_line<'a>(buf: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let rest = &buf[*pos..];
    let end = rest.iter().position(|b| *b == b'\n')?;
    let line = &rest[..end];
    *pos += end + 1;
    Some(line)
}

fn hex_to_bytes(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let bytes = text.as_bytes();
    for i in 0..32 {
        let hi = (bytes[i * 2] as char).to_digit(16)?;
        let lo = (bytes[i * 2 + 1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

fn put_u16(out: &mut Vec<u8>, v: usize) -> Option<()> {
    if v > u16::MAX as usize {
        return None;
    }
    out.extend_from_slice(&(v as u16).to_le_bytes());
    Some(())
}

fn put_u32(out: &mut Vec<u8>, v: usize) -> Option<()> {
    if v > u32::MAX as usize {
        return None;
    }
    out.extend_from_slice(&(v as u32).to_le_bytes());
    Some(())
}

fn get_u16(buf: &[u8], pos: &mut usize) -> Option<usize> {
    if *pos + 2 > buf.len() {
        return None;
    }
    let v = u16::from_le_bytes([buf[*pos], buf[*pos + 1]]) as usize;
    *pos += 2;
    Some(v)
}

fn get_u32(buf: &[u8], pos: &mut usize) -> Option<usize> {
    if *pos + 4 > buf.len() {
        return None;
    }
    let v = u32::from_le_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]]) as usize;
    *pos += 4;
    Some(v)
}

/// One file as the index stores it: everything except the hashes.
struct IndexedFile {
    len: u64,
    ctype: String,
    path: String,
    pieces: Vec<usize>,
}

/// The index without hashes: chunk lengths, then the tree that references them.
fn encode_index(bp: &Blueprint) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(32 + bp.chunks.len() * 4 + bp.files.len() * 48);
    put_u32(&mut out, bp.chunks.len())?;
    let mut index = std::collections::BTreeMap::new();
    for (i, chunk) in bp.chunks.iter().enumerate() {
        put_u32(&mut out, chunk.len as usize)?;
        index.insert(chunk.hash.clone(), i);
    }
    put_u32(&mut out, bp.files.len())?;
    for file in &bp.files {
        put_u32(&mut out, file.len as usize)?;
        put_u16(&mut out, file.ctype.len())?;
        out.extend_from_slice(file.ctype.as_bytes());
        put_u16(&mut out, file.path.len())?;
        out.extend_from_slice(file.path.as_bytes());
        put_u32(&mut out, file.pieces.len())?;
        for piece in &file.pieces {
            put_u32(&mut out, *index.get(&piece.hash)?)?;
        }
    }
    Some(out)
}

fn decode_index(bytes: &[u8]) -> Option<(Vec<u64>, Vec<IndexedFile>)> {
    let mut pos = 0usize;
    let chunk_count = get_u32(bytes, &mut pos)?;
    if chunk_count > 4_000_000 {
        return None;
    }
    let mut lengths = Vec::with_capacity(chunk_count);
    for _ in 0..chunk_count {
        lengths.push(get_u32(bytes, &mut pos)? as u64);
    }
    let file_count = get_u32(bytes, &mut pos)?;
    if file_count > 1_000_000 {
        return None;
    }
    let mut files = Vec::with_capacity(file_count);
    for _ in 0..file_count {
        let len = get_u32(bytes, &mut pos)? as u64;
        let ctype_len = get_u16(bytes, &mut pos)?;
        if pos + ctype_len > bytes.len() {
            return None;
        }
        let ctype = std::str::from_utf8(&bytes[pos..pos + ctype_len]).ok()?.to_string();
        pos += ctype_len;
        let path_len = get_u16(bytes, &mut pos)?;
        if pos + path_len > bytes.len() {
            return None;
        }
        let path = std::str::from_utf8(&bytes[pos..pos + path_len]).ok()?.to_string();
        pos += path_len;
        let piece_count = get_u32(bytes, &mut pos)?;
        let mut pieces = Vec::with_capacity(piece_count);
        for _ in 0..piece_count {
            let idx = get_u32(bytes, &mut pos)?;
            if idx >= chunk_count {
                return None;
            }
            pieces.push(idx);
        }
        files.push(IndexedFile { len, ctype, path, pieces });
    }
    if pos != bytes.len() {
        // Trailing bytes mean the index is not what it claims to be.
        return None;
    }
    Some((lengths, files))
}

pub fn build(bp: &Blueprint, store: &Store, path: &Path) -> io::Result<ArchiveReport> {
    build_with_map(bp, store, path, "")
}

/// As `build`, but with the fleet's network map stored beside the index: a
/// restore then brings back what the network looked like — the computers, their
/// names and who held which chunk — not only its bytes.
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

    let index = encode_index(bp)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "blueprint cannot be indexed"))?;
    let index_gz = gzip::gzip(&index);
    let map_gz = gzip::gzip(map.as_bytes());

    let mut out = Vec::with_capacity(packed.len() + index_gz.len() + map_gz.len() + 160);
    out.extend_from_slice(MAGIC.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(format!("epoch {}\nroot {}\n", bp.epoch, bp.root).as_bytes());
    out.extend_from_slice(format!("index {} {}\n", index_gz.len(), index.len()).as_bytes());
    out.extend_from_slice(format!("netmap {} {}\n", map_gz.len(), map.len()).as_bytes());
    // The raw length lets a restore cap the decompression at exactly what the
    // archive claims: a corrupt or hostile file cannot inflate past it.
    out.extend_from_slice(format!("data {} {}\n", packed.len(), raw.len()).as_bytes());
    out.extend_from_slice(&index_gz);
    out.extend_from_slice(&map_gz);
    out.extend_from_slice(&packed);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, &out)?;
    Ok(ArchiveReport {
        path: path.to_path_buf(),
        raw: raw.len() as u64,
        packed: packed.len() as u64,
        total: out.len() as u64,
        chunks: bp.chunks.len(),
    })
}

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
    let v1 = magic == MAGIC_V1.as_bytes();
    let v2 = magic == MAGIC_V2.as_bytes();
    if magic != MAGIC.as_bytes() && !v1 && !v2 {
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

    // Sections between the header and `data`: the index, then the map.
    let mut index_gz: Option<(usize, usize)> = None;
    let mut meta_gz: Option<(usize, usize)> = None;
    let mut bp_text_len: Option<usize> = None;
    let mut map_gz: Option<(usize, usize)> = None;
    let mut map_text_len: usize = 0;
    let data_len: usize;
    /// Present in `CARTARC 3` and later; older archives leave it out.
    let mut data_raw: Option<usize> = None;
    loop {
        let line = read_line(&buf, &mut pos).ok_or_else(|| bad("missing data size"))?;
        let text = std::str::from_utf8(line).map_err(|_| bad("header is not text"))?;
        let mut fields = text.split_whitespace();
        match fields.next() {
            Some("index") => {
                let packed = fields.next().and_then(|v| v.parse().ok());
                let raw = fields.next().and_then(|v| v.parse().ok());
                index_gz = Some((
                    packed.ok_or_else(|| bad("bad index size"))?,
                    raw.ok_or_else(|| bad("bad index size"))?,
                ));
            }
            Some("meta") => {
                let packed = fields.next().and_then(|v| v.parse().ok());
                let raw = fields.next().and_then(|v| v.parse().ok());
                meta_gz = Some((
                    packed.ok_or_else(|| bad("bad meta size"))?,
                    raw.ok_or_else(|| bad("bad meta size"))?,
                ));
            }
            Some("blueprint") => {
                bp_text_len = Some(
                    fields.next().and_then(|v| v.parse().ok()).ok_or_else(|| bad("bad blueprint size"))?,
                );
            }
            Some("netmap") => {
                let first: usize =
                    fields.next().and_then(|v| v.parse().ok()).ok_or_else(|| bad("bad netmap size"))?;
                match fields.next().and_then(|v| v.parse().ok()) {
                    Some(raw) => map_gz = Some((first, raw)),
                    None => map_text_len = first,
                }
            }
            Some("data") => {
                data_len = fields
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| bad("bad data size"))?;
                data_raw = fields.next().and_then(|v| v.parse().ok());
                break;
            }
            _ => return Err(bad("unknown header line")),
        }
    }

    // Bytes the index occupies on disk: the deflated index, the deflated meta
    // block, or the text blueprint an old archive stores in the clear.
    let index_bytes = index_gz.map(|(p, _)| p).or(meta_gz.map(|(p, _)| p)).or(bp_text_len).unwrap_or(0);
    let map_bytes = map_gz.map(|(p, _)| p).unwrap_or(map_text_len);
    if pos + index_bytes + map_bytes + data_len > buf.len() {
        return Err(bad("archive is truncated"));
    }

    // The map first: it is independent of the data.
    let map_start = pos + index_bytes;
    const MAX_INDEX: usize = 64 * 1024 * 1024;
    let map = match map_gz {
        Some((packed, raw_len)) => {
            if raw_len > 8 * 1024 * 1024 {
                return Err(bad("the network map is larger than a map can be"));
            }
            let bytes = crate::inflate::gzip_decompress_limited(
                &buf[map_start..map_start + packed],
                raw_len,
            )
            .map_err(|e| bad(&format!("cannot decompress the network map: {}", e)))?;
            if bytes.len() != raw_len {
                return Err(bad("network map length disagrees with its header"));
            }
            String::from_utf8(bytes).map_err(|_| bad("network map is not text"))?
        }
        None => {
            let bytes = &buf[map_start..map_start + map_text_len];
            std::str::from_utf8(bytes).map_err(|_| bad("network map is not text"))?.to_string()
        }
    };

    let data_start = map_start + map_bytes;
    let plain = match data_raw {
        Some(raw_len) => {
            if raw_len as u64 > crate::guard::MAX_TOTAL_BYTES {
                return Err(bad("the archive declares more data than a release may hold"));
            }
            let bytes = crate::inflate::gzip_decompress_limited(
                &buf[data_start..data_start + data_len],
                raw_len,
            )
            .map_err(|e| bad(&format!("cannot decompress: {}", e)))?;
            if bytes.len() != raw_len {
                return Err(bad("archive data is not the length its header declares"));
            }
            bytes
        }
        // CARTARC 1 and 2 did not record it; those files are already on disk.
        None => gzip::inflate_bytes(&buf[data_start..data_start + data_len])
            .map_err(|e| bad(&format!("cannot decompress: {}", e)))?,
    };

    // An index that carries hashes is used as it is; one that does not gets its
    // hashes recomputed from the bytes, and the Merkle root decides.
    let (lengths, files, hashes, text_bp): (
        Vec<u64>,
        Vec<IndexedFile>,
        Option<Vec<String>>,
        Option<Blueprint>,
    ) =     if let Some((packed, raw_len)) = index_gz {
        if raw_len > MAX_INDEX {
            return Err(bad("the index is larger than an index can be"));
        }
        let bytes = crate::inflate::gzip_decompress_limited(&buf[pos..pos + packed], raw_len)
            .map_err(|e| bad(&format!("cannot decompress the index: {}", e)))?;
        if bytes.len() != raw_len {
            return Err(bad("index length disagrees with its header"));
        }
        let (lens, tree) = decode_index(&bytes).ok_or_else(|| bad("index is corrupt"))?;
        (lens, tree, None, None)
    } else if let Some((packed, raw_len)) = meta_gz {
        if raw_len > MAX_INDEX {
            return Err(bad("the index is larger than an index can be"));
        }
        let bytes = crate::inflate::gzip_decompress_limited(&buf[pos..pos + packed], raw_len)
            .map_err(|e| bad(&format!("cannot decompress the index: {}", e)))?;
        if bytes.len() != raw_len {
            return Err(bad("index length disagrees with its header"));
        }
        let (bp, list) = decode_meta_with_hashes(&bytes, epoch, &root)
            .ok_or_else(|| bad("index is corrupt"))?;
        let tree = indexed_files(&bp)
            .ok_or_else(|| bad("index references a chunk it does not carry"))?;
        (chunk_lengths(&bp), tree, Some(list), None)
    } else if let Some(len) = bp_text_len {
        let text = std::str::from_utf8(&buf[pos..pos + len]).map_err(|_| bad("blueprint is not text"))?;
        let bp = Blueprint::parse(text).ok_or_else(|| bad("blueprint is corrupt"))?;
        // The text blueprint was never checked against its own root, so a
        // damaged one could name any chunk and still restore "successfully".
        if Blueprint::compute_root(&bp.files) != bp.root {
            return Err(bad("blueprint does not match its root"));
        }
        let tree = indexed_files(&bp)
            .ok_or_else(|| bad("index references a chunk it does not carry"))?;
        let list = bp.chunks.iter().map(|c| c.hash.clone()).collect();
        (chunk_lengths(&bp), tree, Some(list), Some(bp))
    } else {
        return Err(bad("archive has no index"));
    };

    // Slice the stream and hash every chunk: that is what rebuilds the tree.
    let mut chunks: Vec<(String, Vec<u8>)> = Vec::with_capacity(lengths.len());
    let mut at = 0usize;
    for (i, len) in lengths.iter().enumerate() {
        let end = at + *len as usize;
        if end > plain.len() {
            return Err(bad("archive data is shorter than its index"));
        }
        let bytes = plain[at..end].to_vec();
        let digest = crate::hash::bytes(&bytes);
        if let Some(expected) = &hashes {
            if expected.get(i) != Some(&digest) {
                return Err(bad("chunk hash mismatch while restoring"));
            }
        }
        chunks.push((digest, bytes));
        at = end;
    }
    if at != plain.len() {
        return Err(bad("archive data is longer than its index"));
    }

    let bp = match text_bp {
        Some(bp) => bp,
        None => {
            let mut entries = Vec::with_capacity(files.len());
            for file in &files {
                let mut pieces = Vec::with_capacity(file.pieces.len());
                let mut total = 0u64;
                for idx in &file.pieces {
                    let (hash, bytes) = chunks.get(*idx).ok_or_else(|| bad("index points past the data"))?;
                    pieces.push(Piece { hash: hash.clone(), len: bytes.len() as u64 });
                    total += bytes.len() as u64;
                }
                if total != file.len {
                    return Err(bad("file length disagrees with its pieces"));
                }
                entries.push(FileEntry {
                    path: file.path.clone(),
                    len: file.len,
                    ctype: file.ctype.clone(),
                    pieces,
                });
            }
            entries.sort_by(|a, b| a.path.cmp(&b.path));
            let computed = Blueprint::compute_root(&entries);
            if computed != root {
                return Err(bad("archive does not match its root"));
            }
            Blueprint {
                epoch,
                root: root.clone(),
                files: entries,
                chunks: chunks
                    .iter()
                    .map(|(hash, bytes)| Piece { hash: hash.clone(), len: bytes.len() as u64 })
                    .collect(),
            }
        }
    };
    if bp.epoch != epoch || bp.root != root {
        return Err(bad("archive header disagrees with its index"));
    }
    Ok((bp, chunks, map))
}

fn chunk_lengths(bp: &Blueprint) -> Vec<u64> {
    bp.chunks.iter().map(|c| c.len).collect()
}

/// A blueprint's tree with its hashes replaced by chunk positions.
///
/// `None` when a file names a chunk the archive does not carry. That has to be
/// an error and never a default: falling back to position zero would hand the
/// file whatever chunk happened to come first and call the restore a success.
fn indexed_files(bp: &Blueprint) -> Option<Vec<IndexedFile>> {
    let mut position: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (i, chunk) in bp.chunks.iter().enumerate() {
        position.insert(chunk.hash.as_str(), i);
    }
    let mut out = Vec::with_capacity(bp.files.len());
    for f in &bp.files {
        let mut pieces = Vec::with_capacity(f.pieces.len());
        for p in &f.pieces {
            pieces.push(*position.get(p.hash.as_str())?);
        }
        out.push(IndexedFile {
            len: f.len,
            ctype: f.ctype.clone(),
            path: f.path.clone(),
            pieces,
        });
    }
    Some(out)
}

/// The `CARTARC 2` index: chunk lengths and the tree, with every hash.
fn decode_meta_with_hashes(bytes: &[u8], epoch: u64, root: &str) -> Option<(Blueprint, Vec<String>)> {
    let mut pos = 0usize;
    let chunk_count = get_u32(bytes, &mut pos)?;
    if chunk_count > 4_000_000 {
        return None;
    }
    let mut chunks = Vec::with_capacity(chunk_count);
    let mut hashes = Vec::with_capacity(chunk_count);
    for _ in 0..chunk_count {
        if pos + 32 > bytes.len() {
            return None;
        }
        let hash = bytes_to_hex(&bytes[pos..pos + 32]);
        pos += 32;
        let len = get_u32(bytes, &mut pos)? as u64;
        chunks.push(Piece { hash: hash.clone(), len });
        hashes.push(hash);
    }
    let file_count = get_u32(bytes, &mut pos)?;
    if file_count > 1_000_000 {
        return None;
    }
    let mut files = Vec::with_capacity(file_count);
    for _ in 0..file_count {
        let len = get_u32(bytes, &mut pos)? as u64;
        let ctype_len = get_u16(bytes, &mut pos)?;
        if pos + ctype_len > bytes.len() {
            return None;
        }
        let ctype = std::str::from_utf8(&bytes[pos..pos + ctype_len]).ok()?.to_string();
        pos += ctype_len;
        let path_len = get_u16(bytes, &mut pos)?;
        if pos + path_len > bytes.len() {
            return None;
        }
        let path = std::str::from_utf8(&bytes[pos..pos + path_len]).ok()?.to_string();
        pos += path_len;
        let piece_count = get_u32(bytes, &mut pos)?;
        let mut pieces = Vec::with_capacity(piece_count);
        for _ in 0..piece_count {
            let idx = get_u32(bytes, &mut pos)?;
            let chunk = chunks.get(idx)?;
            pieces.push(Piece { hash: chunk.hash.clone(), len: chunk.len });
        }
        files.push(FileEntry { path, len, ctype, pieces });
    }
    if pos != bytes.len() {
        return None;
    }
    Some((Blueprint { epoch, root: root.to_string(), files, chunks }, hashes))
}

/// A real `.zip` of the release: the network map, the blueprint and every site
/// file, stored rather than deflated. The point of this file is a backup that a
/// person can open anywhere without this program, and that restores without
/// decoding anything — the solid `.cart` archive already covers size.
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

    fn release(base: &std::path::PathBuf, files: usize) -> (Blueprint, Store) {
        fs::create_dir_all(base.join("rel/3000")).unwrap();
        for i in 0..files {
            fs::write(
                base.join(format!("rel/3000/p{}.html", i)),
                format!("<p>page {} of the site, with some words in it</p>", i),
            )
            .unwrap();
        }
        let store = Store::at(base.join("store"));
        let (bp, _) = chunks::ingest(&base.join("rel"), &store, None, 3).unwrap();
        (bp, store)
    }

    #[test]
    fn a_release_survives_a_full_round_trip() {
        let base = scratch("rt");
        let (bp, store) = release(&base, 4);
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
    fn the_index_carries_no_hashes_and_still_verifies() {
        // 120 files: a text blueprint would be tens of KB of random hex. The
        // index must stay small, and the Merkle root must still catch damage.
        let base = scratch("index");
        let (bp, store) = release(&base, 120);
        let index = encode_index(&bp).unwrap();
        let text = bp.text();
        assert!(
            index.len() * 3 < text.len(),
            "index {} bytes vs text blueprint {} bytes",
            index.len(),
            text.len()
        );
        let (lengths, files) = decode_index(&index).expect("index must decode");
        assert_eq!(lengths.len(), bp.chunks.len());
        assert_eq!(files.len(), bp.files.len());
        assert!(files.iter().all(|f| f.pieces.iter().all(|i| *i < lengths.len())));

        let path = base.join("a.cart");
        build(&bp, &store, &path).unwrap();
        // Flip one byte of the data stream: the archive must be refused.
        let mut raw = fs::read(&path).unwrap();
        let last = raw.len() - 40;
        raw[last] ^= 0x40;
        let broken = base.join("broken.cart");
        fs::write(&broken, &raw).unwrap();
        assert!(restore(&broken).is_err(), "damage must not restore");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_big_site_is_a_small_file() {
        // 200 small pages: the index dominates, so the whole archive must stay
        // well under the size of the text blueprint it would have written.
        let base = scratch("size");
        let (bp, store) = release(&base, 200);
        let path = base.join("a.cart");
        let report = build(&bp, &store, &path).unwrap();
        assert!(
            report.total < bp.text().len() as u64,
            "archive {} bytes, text blueprint {} bytes",
            report.total,
            bp.text().len()
        );
        let (back, _) = restore(&path).unwrap();
        assert_eq!(back, bp);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn the_network_map_rides_along_and_comes_back() {
        let base = scratch("map");
        let (bp, store) = release(&base, 2);
        let map = "CARTNET 1\nepoch 5\nroot x\nnode 00000000000000000000000000000011 a 1.2.3.4 5 5 0 1 0 100 200 1 17 self\n";
        let path = base.join("a.cart");
        build_with_map(&bp, &store, &path, map).unwrap();
        let (back, chunks_back, map_back) = restore_full(&path).unwrap();
        assert_eq!(back, bp);
        assert_eq!(chunks_back.len(), bp.chunks.len());
        assert_eq!(map_back, map, "the map must survive the round trip verbatim");
        let plain = base.join("b.cart");
        build(&bp, &store, &plain).unwrap();
        let (_, _, no_map) = restore_full(&plain).unwrap();
        assert!(no_map.is_empty());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn older_archives_still_restore() {
        let base = scratch("old");
        let (bp, store) = release(&base, 2);
        let mut raw = Vec::new();
        for chunk in &bp.chunks {
            raw.extend_from_slice(&store.get(&chunk.hash).unwrap());
        }
        let packed = gzip::gzip(&raw);
        let text = bp.text();
        let map = "CARTNET 1\nepoch 4\nroot x\n";

        // CARTARC 1: text blueprint and map in the clear.
        let mut v1 = Vec::new();
        v1.extend_from_slice(b"CARTARC 1\n");
        v1.extend_from_slice(format!("epoch {}\nroot {}\n", bp.epoch, bp.root).as_bytes());
        v1.extend_from_slice(format!("blueprint {}\n", text.len()).as_bytes());
        v1.extend_from_slice(format!("netmap {}\n", map.len()).as_bytes());
        v1.extend_from_slice(format!("data {}\n", packed.len()).as_bytes());
        v1.extend_from_slice(text.as_bytes());
        v1.extend_from_slice(map.as_bytes());
        v1.extend_from_slice(&packed);
        let p1 = base.join("v1.cart");
        fs::write(&p1, &v1).unwrap();
        let (back, chunks_back, map_back) = restore_full(&p1).unwrap();
        assert_eq!(back, bp);
        assert_eq!(chunks_back.len(), bp.chunks.len());
        assert_eq!(map_back, map);

        // CARTARC 2: binary index that still carries the hashes.
        let mut meta = Vec::new();
        put_u32(&mut meta, bp.chunks.len()).unwrap();
        for chunk in &bp.chunks {
            meta.extend_from_slice(&hex_to_bytes(&chunk.hash).unwrap());
            put_u32(&mut meta, chunk.len as usize).unwrap();
        }
        put_u32(&mut meta, bp.files.len()).unwrap();
        for file in &bp.files {
            put_u32(&mut meta, file.len as usize).unwrap();
            put_u16(&mut meta, file.ctype.len()).unwrap();
            meta.extend_from_slice(file.ctype.as_bytes());
            put_u16(&mut meta, file.path.len()).unwrap();
            meta.extend_from_slice(file.path.as_bytes());
            put_u32(&mut meta, file.pieces.len()).unwrap();
            for piece in &file.pieces {
                let idx = bp.chunks.iter().position(|c| c.hash == piece.hash).unwrap();
                put_u32(&mut meta, idx).unwrap();
            }
        }
        let meta_gz = gzip::gzip(&meta);
        let map_gz = gzip::gzip(map.as_bytes());
        let mut v2 = Vec::new();
        v2.extend_from_slice(b"CARTARC 2\n");
        v2.extend_from_slice(format!("epoch {}\nroot {}\n", bp.epoch, bp.root).as_bytes());
        v2.extend_from_slice(format!("meta {} {}\n", meta_gz.len(), meta.len()).as_bytes());
        v2.extend_from_slice(format!("netmap {} {}\n", map_gz.len(), map.len()).as_bytes());
        v2.extend_from_slice(format!("data {}\n", packed.len()).as_bytes());
        v2.extend_from_slice(&meta_gz);
        v2.extend_from_slice(&map_gz);
        v2.extend_from_slice(&packed);
        let p2 = base.join("v2.cart");
        fs::write(&p2, &v2).unwrap();
        let (back2, chunks2, map2) = restore_full(&p2).unwrap();
        assert_eq!(back2, bp);
        assert_eq!(chunks2.len(), bp.chunks.len());
        assert_eq!(map2, map);
        let _ = fs::remove_dir_all(&base);
    }

    /// A `CARTARC 1` archive: the format older builds wrote, blueprint as text.
    fn write_v1(path: &std::path::Path, bp: &Blueprint, packed: &[u8]) {
        let text = bp.text();
        let mut file = Vec::new();
        file.extend_from_slice(b"CARTARC 1\n");
        file.extend_from_slice(format!("epoch {}\nroot {}\n", bp.epoch, bp.root).as_bytes());
        file.extend_from_slice(format!("blueprint {}\n", text.len()).as_bytes());
        file.extend_from_slice(b"netmap 0\n");
        file.extend_from_slice(format!("data {}\n", packed.len()).as_bytes());
        file.extend_from_slice(text.as_bytes());
        file.extend_from_slice(packed);
        fs::write(path, &file).unwrap();
    }

    #[test]
    fn a_piece_naming_a_chunk_the_archive_lacks_is_refused() {
        // A damaged old blueprint can name a chunk the archive does not hold.
        // Restoring that must fail: handing the file chunk zero and reporting
        // success is exactly how corrupt data gets published.
        let base = scratch("chunk0");
        let (bp, store) = release(&base, 3);
        let mut raw = Vec::new();
        for chunk in &bp.chunks {
            raw.extend_from_slice(&store.get(&chunk.hash).unwrap());
        }
        let packed = gzip::gzip(&raw);

        let mut broken = bp.clone();
        broken.files[0].pieces[0].hash = "11".repeat(32);
        // Keep the root consistent with the damaged tree, so the missing chunk
        // is the only thing that gives it away.
        broken.root = Blueprint::compute_root(&broken.files);
        assert!(!bp.chunks.iter().any(|c| c.hash == broken.files[0].pieces[0].hash));

        let path = base.join("chunk0.cart");
        write_v1(&path, &broken, &packed);
        let err = restore_full(&path).expect_err("a missing chunk must be refused");
        assert!(err.to_string().contains("does not carry"), "{}", err);
        assert!(restore(&path).is_err(), "nothing may be handed back");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn an_old_blueprint_that_does_not_match_its_root_is_refused() {
        // The text format was trusted as written. A blueprint whose tree no
        // longer hashes to its root is damaged, and must not restore.
        let base = scratch("root");
        let (bp, store) = release(&base, 2);
        let mut raw = Vec::new();
        for chunk in &bp.chunks {
            raw.extend_from_slice(&store.get(&chunk.hash).unwrap());
        }
        let packed = gzip::gzip(&raw);

        let mut wrong = bp.clone();
        let first = if bp.root.starts_with('0') { '1' } else { '0' };
        wrong.root = format!("{}{}", first, &bp.root[1..]);
        assert_ne!(wrong.root, bp.root);

        let path = base.join("root.cart");
        write_v1(&path, &wrong, &packed);
        let err = restore_full(&path).expect_err("a bad root must be refused");
        assert!(err.to_string().contains("does not match its root"), "{}", err);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_bomb_beyond_the_declared_size_is_refused() {
        // The header states how many bytes the data section holds, and the
        // decompressor is capped at exactly that: a small file that expands to
        // gigabytes stops at the cap instead of taking the box with it.
        let base = scratch("bomb");
        let bp = Blueprint { epoch: 1, root: "abc".into(), files: vec![], chunks: vec![] };
        let index = encode_index(&bp).unwrap();
        let index_gz = gzip::gzip(&index);
        let map_gz = gzip::gzip(b"");
        let data = gzip::gzip(&vec![0u8; 4 * 1024 * 1024]);
        let mut file = Vec::new();
        file.extend_from_slice(b"CARTARC 3\nepoch 1\nroot abc\n");
        file.extend_from_slice(format!("index {} {}\n", index_gz.len(), index.len()).as_bytes());
        file.extend_from_slice(format!("netmap {} 0\n", map_gz.len()).as_bytes());
        file.extend_from_slice(format!("data {} 1\n", data.len()).as_bytes());
        file.extend_from_slice(&index_gz);
        file.extend_from_slice(&map_gz);
        file.extend_from_slice(&data);
        let path = base.join("bomb.cart");
        fs::write(&path, &file).unwrap();
        let err = restore_full(&path).expect_err("the bomb must be refused");
        assert!(err.to_string().contains("decompress"), "{}", err);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn truncated_or_foreign_files_are_rejected() {
        let base = scratch("bad");
        let path = base.join("x.cart");
        fs::write(&path, b"not an archive").unwrap();
        assert!(restore(&path).is_err());
        fs::write(&path, b"CARTARC 3\nepoch 1\nroot abc\nindex 99 1\ndata 1\n").unwrap();
        assert!(restore(&path).is_err());
        // An index that inflates to the wrong length must be refused.
        let bp = Blueprint { epoch: 1, root: "abc".into(), files: vec![], chunks: vec![] };
        let index = encode_index(&bp).unwrap();
        let index_gz = gzip::gzip(&index);
        let data = gzip::gzip(b"");
        let mut file = Vec::new();
        file.extend_from_slice(b"CARTARC 3\nepoch 1\nroot abc\n");
        file.extend_from_slice(format!("index {} {}\n", index_gz.len(), index.len() + 1).as_bytes());
        file.extend_from_slice(b"netmap 0 0\n");
        file.extend_from_slice(format!("data {}\n", data.len()).as_bytes());
        file.extend_from_slice(&index_gz);
        file.extend_from_slice(&data);
        fs::write(&path, &file).unwrap();
        assert!(restore(&path).is_err());
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
        assert_eq!(&raw[at..at + 4], b"PK\x01\x02");
        let eocd = raw.len() - 22;
        assert_eq!(&raw[eocd..eocd + 4], b"PK\x05\x06");
        let count = u16::from_le_bytes([raw[eocd + 10], raw[eocd + 11]]);
        assert_eq!(count as usize, entries.len());
        let _ = fs::remove_dir_all(&base);
    }
}
