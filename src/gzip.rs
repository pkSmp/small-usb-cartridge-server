//! A deflate (RFC 1951) and gzip (RFC 1952) encoder, in std only.
//!
//! There is no compression library to link, so the fleet builds its own. This
//! encoder exists for one job now: the disaster-recovery archive, where the
//! smallest possible file matters more than the milliseconds it takes (it is
//! built by the idle class, never while serving).
//!
//! What it does to get there:
//!
//!   * **Lazy matching.** A match is only taken when the next byte does not
//!     start a longer one, which is the single cheapest ratio win in LZ77.
//!   * **Deep chains.** A 64K-entry hash table and up to 128 candidates per
//!     position, instead of 32K and 48.
//!   * **Dynamic Huffman.** Every block carries its own literal/length and
//!     distance tables, built with package-merge so they are optimal under
//!     deflate's 15-bit limit. Fixed tables cost 8 bits for every lowercase
//!     letter; a real table costs about 4-5 on text.
//!   * **A window-sized chain array.** Candidate links live in a ring of
//!     `WINDOW` entries rather than one entry per input byte, so a 300 MB
//!     release no longer needs gigabytes to compress. When a card is present
//!     the search over those links runs there instead, a slice at a time, and
//!     every match it returns is checked against the bytes before use.
//!
//! Blocks are flushed every 128K tokens: each one gets fresh tables, which
//! bounds memory and lets a block that is unlike its neighbours use its own
//! alphabet. Tiny blocks stay on the fixed tables, where a header would cost
//! more than it saves.
//!
//! Decoding is not duplicated here — `inflate.rs` already has a decoder that
//! handles both block types, and every test round-trips through it.

const WINDOW: usize = 32 * 1024;
const MAX_MATCH: usize = 258;
const MIN_MATCH: usize = 3;
const HASH_BITS: usize = 16;
const HASH_SIZE: usize = 1 << HASH_BITS;
const MAX_CHAIN: usize = 128;
/// How much input one card dispatch searches: a release is cut into slices of
/// this size, which bounds the link table and the match buffer at 8 bytes a
/// byte instead of holding them for a whole release.
const SLICE: usize = 4 * 1024 * 1024;
/// Below this many bytes a dispatch costs more than the search, so the CPU does
/// it; it matches the floor `offload` applies to every bulk task.
const CARD_SLICE_MIN: usize = 256 * 1024;
/// Tokens per block: bounds the token list and the cost of one bad table.
const BLOCK_TOKENS: usize = 1 << 17;
/// Deflate's limit for literal/length and distance codes.
const MAX_BITS: usize = 15;
/// Deflate's limit for the code-length alphabet.
const MAX_CL_BITS: usize = 7;

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
    131, 163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
    13, 13,
];
/// The order deflate writes code-length code lengths in.
const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

#[derive(Clone, Copy)]
enum Tok {
    Lit(u8),
    Match { len: u16, dist: u16 },
}

struct BitOut {
    out: Vec<u8>,
    acc: u32,
    bits: u32,
}

impl BitOut {
    fn new() -> BitOut {
        BitOut { out: Vec::new(), acc: 0, bits: 0 }
    }

    /// Bits go into the stream least-significant first, which is what deflate
    /// requires for values and extra bits.
    fn put(&mut self, value: u32, bits: u32) {
        if bits == 0 {
            return;
        }
        self.acc |= (value & ((1u32 << bits) - 1)) << self.bits;
        self.bits += bits;
        while self.bits >= 8 {
            self.out.push((self.acc & 0xff) as u8);
            self.acc >>= 8;
            self.bits -= 8;
        }
    }

    /// Huffman codes are packed most-significant bit first, so the value is
    /// reversed before it reaches the little-endian bit stream.
    fn put_code(&mut self, code: u32, bits: u32) {
        let mut reversed = 0u32;
        for i in 0..bits {
            reversed = (reversed << 1) | ((code >> i) & 1);
        }
        self.put(reversed, bits);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.out.push((self.acc & 0xff) as u8);
        }
        self.out
    }
}

fn fixed_lit_code(bits: &mut BitOut, byte: u8) {
    match byte {
        0..=143 => bits.put_code(0x30 + byte as u32, 8),
        _ => bits.put_code(0x190 + (byte as u32 - 144), 9),
    }
}

fn fixed_length_code(bits: &mut BitOut, symbol: u32) {
    // 257..=279 are seven bits from zero, 280..=287 eight bits from 0xC0.
    if symbol <= 279 {
        bits.put_code(symbol - 256, 7);
    } else {
        bits.put_code(0xC0 + (symbol - 280), 8);
    }
}

fn find_length(len: usize) -> (usize, u32) {
    let mut idx = 0;
    for i in 0..LEN_BASE.len() {
        if len >= LEN_BASE[i] as usize {
            idx = i;
        }
    }
    (idx, (len - LEN_BASE[idx] as usize) as u32)
}

fn find_distance(dist: usize) -> (usize, u32) {
    // Distance codes are 1-based: code 0 covers distance 1, and the extra bits
    // carry `dist - base`.
    let mut idx = 0;
    for i in 0..DIST_BASE.len() {
        if dist >= DIST_BASE[i] as usize {
            idx = i;
        }
    }
    (idx, (dist - DIST_BASE[idx] as usize) as u32)
}

fn hash3(data: &[u8], pos: usize) -> usize {
    let v = (data[pos] as u32) | ((data[pos + 1] as u32) << 8) | ((data[pos + 2] as u32) << 16);
    (v.wrapping_mul(0x1e35_a7bd) >> (32 - HASH_BITS)) as usize & (HASH_SIZE - 1)
}

/// Longest match at `pos`, searching the chains newest-first.
fn longest(data: &[u8], head: &[usize], prev: &[usize], pos: usize) -> (usize, usize) {
    if pos + MIN_MATCH > data.len() {
        return (0, 0);
    }
    let mut cand = head[hash3(data, pos)];
    let limit = MAX_MATCH.min(data.len() - pos);
    let (mut best_len, mut best_dist) = (0usize, 0usize);
    let mut tries = 0;
    while cand != usize::MAX && tries < MAX_CHAIN {
        if cand >= pos {
            break;
        }
        let dist = pos - cand;
        if dist > WINDOW {
            // The chain is newest-first, so everything further back is older.
            // The ring can alias, so keep walking rather than trusting order.
            cand = prev[cand & (WINDOW - 1)];
            tries += 1;
            continue;
        }
        let mut len = 0usize;
        while len < limit && data[cand + len] == data[pos + len] {
            len += 1;
        }
        if len > best_len {
            best_len = len;
            best_dist = dist;
            if len >= limit {
                break;
            }
        }
        cand = prev[cand & (WINDOW - 1)];
        tries += 1;
    }
    if best_len < MIN_MATCH {
        (0, 0)
    } else {
        (best_len, best_dist)
    }
}

fn insert(data: &[u8], head: &mut [usize], prev: &mut [usize], pos: usize) {
    if pos + MIN_MATCH > data.len() {
        return;
    }
    let h = hash3(data, pos);
    prev[pos & (WINDOW - 1)] = head[h];
    head[h] = pos;
}

/// The chain for every position, in one pass and with no comparisons: filling
/// the links is cheap, walking them is the expensive half of LZ77.
fn link_table(data: &[u8]) -> Vec<u32> {
    let mut head = vec![u32::MAX; HASH_SIZE];
    let mut link = vec![u32::MAX; data.len()];
    for pos in 0..data.len() {
        if pos + MIN_MATCH > data.len() {
            break;
        }
        let h = hash3(data, pos);
        link[pos] = head[h];
        head[h] = pos as u32;
    }
    link
}

/// Turn precomputed matches into tokens with the same lazy rule the CPU walk
/// uses. `None` when a match does not hold up against the bytes, which is the
/// only way the card can be wrong: the caller then does the slice itself.
fn walk(data: &[u8], matches: &[u32]) -> Option<Vec<Tok>> {
    if matches.len() != data.len() {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() / 2 + 1);
    let mut pos = 0usize;
    while pos < data.len() {
        let packed = matches[pos] as usize;
        let (len, dist) = (packed >> 16, packed & 0xffff);
        if len >= MIN_MATCH && dist >= 1 && dist <= pos && len <= data.len() - pos {
            // Verify the match against the bytes before emitting it. This costs
            // one pass over the token's own length, which is nothing beside the
            // chain search it replaces, and it means a wrong answer from the
            // card can never reach a compressed stream.
            if data[pos - dist..pos - dist + len] != data[pos..pos + len] {
                return None;
            }
            if pos + 1 < data.len() {
                let next_len = (matches[pos + 1] as usize) >> 16;
                if next_len > len {
                    out.push(Tok::Lit(data[pos]));
                    pos += 1;
                    continue;
                }
            }
            out.push(Tok::Match { len: len as u16, dist: dist as u16 });
            pos += len;
        } else {
            out.push(Tok::Lit(data[pos]));
            pos += 1;
        }
    }
    Some(out)
}

/// The LZ77 pass: the card searches the chains when there is one, the CPU
/// otherwise. Both produce a valid stream; where the CPU's ring aliases past
/// the window the card stops instead, so a few matches can differ.
fn lz77(data: &[u8]) -> Vec<Tok> {
    if let Some(toks) = lz77_card(data) {
        return toks;
    }
    lz77_cpu(data)
}

/// Search the chains on the card, one slice at a time so a release of any size
/// fits in device memory. `None` when there is no card, or it refuses a slice.
fn lz77_card(data: &[u8]) -> Option<Vec<Tok>> {
    if data.len() < CARD_SLICE_MIN || !crate::offload::has_card() {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() / 2 + 1);
    let mut at = 0usize;
    while at < data.len() {
        let mut end = (at + SLICE).min(data.len());
        // A dispatch has a floor, so a short tail joins the slice before it
        // rather than falling back to the CPU and throwing the whole pass away.
        if data.len() - end < CARD_SLICE_MIN {
            end = data.len();
        }
        let slice = &data[at..end];
        let link = link_table(slice);
        let matches =
            crate::offload::lz77_links(slice, &link, WINDOW as u32, MAX_CHAIN as u32, MAX_MATCH as u32)?;
        out.extend(walk(slice, &matches)?);
        at = end;
    }
    Some(out)
}

/// The LZ77 pass on the CPU: literals and matches, with one step of lazy
/// matching.
fn lz77_cpu(data: &[u8]) -> Vec<Tok> {
    let mut head = vec![usize::MAX; HASH_SIZE];
    let mut prev = vec![usize::MAX; WINDOW];
    let mut out = Vec::with_capacity(data.len() / 2 + 1);
    let mut pos = 0usize;
    while pos < data.len() {
        let (len, dist) = longest(data, &head, &prev, pos);
        if len >= MIN_MATCH {
            if pos + 1 < data.len() {
                // Make this position available to the search at pos + 1.
                insert(data, &mut head, &mut prev, pos);
                let (next_len, _) = longest(data, &head, &prev, pos + 1);
                if next_len > len {
                    out.push(Tok::Lit(data[pos]));
                    pos += 1;
                    continue;
                }
            } else {
                insert(data, &mut head, &mut prev, pos);
            }
            out.push(Tok::Match { len: len as u16, dist: dist as u16 });
            for k in 1..len {
                insert(data, &mut head, &mut prev, pos + k);
            }
            pos += len;
        } else {
            out.push(Tok::Lit(data[pos]));
            insert(data, &mut head, &mut prev, pos);
            pos += 1;
        }
    }
    out
}

/// Length-limited Huffman code lengths by package-merge: optimal under `limit`.
fn huff_lengths(freq: &[u32], limit: usize) -> Vec<u8> {
    let n = freq.len();
    let mut lengths = vec![0u8; n];
    let mut items: Vec<(u64, Vec<usize>)> = Vec::new();
    for (i, f) in freq.iter().enumerate() {
        if *f > 0 {
            items.push((*f as u64, vec![i]));
        }
    }
    if items.is_empty() {
        return lengths;
    }
    if items.len() == 1 {
        // One code alone has no length; deflate needs a tree, so give it a
        // partner that simply never appears in the data.
        let only = items[0].1[0];
        lengths[only] = 1;
        lengths[if only == 0 { 1 } else { 0 }] = 1;
        return lengths;
    }
    items.sort_by_key(|x| x.0);
    let mut lists: Vec<Vec<(u64, Vec<usize>)>> = Vec::with_capacity(limit);
    lists.push(items.clone());
    for _ in 1..limit {
        let prev = lists.last().unwrap().clone();
        let mut merged: Vec<(u64, Vec<usize>)> = items.clone();
        let mut i = 0;
        while i + 1 < prev.len() {
            let mut syms = prev[i].1.clone();
            syms.extend_from_slice(&prev[i + 1].1);
            merged.push((prev[i].0 + prev[i + 1].0, syms));
            i += 2;
        }
        merged.sort_by_key(|x| x.0);
        lists.push(merged);
    }
    let mut counts = vec![0u32; n];
    let take = 2 * items.len() - 2;
    for (_, syms) in lists.last().unwrap().iter().take(take) {
        for s in syms {
            counts[*s] += 1;
        }
    }
    for i in 0..n {
        lengths[i] = counts[i].min(limit as u32) as u8;
    }
    lengths
}

/// Canonical Huffman codes for the given lengths, in deflate's bit order.
fn canonical_codes(lengths: &[u8]) -> Vec<u16> {
    let mut bl_count = [0u16; 16];
    for l in lengths {
        if *l > 0 {
            bl_count[*l as usize] += 1;
        }
    }
    let mut next_code = [0u16; 16];
    let mut code = 0u16;
    for bits in 1..16 {
        code = (code + bl_count[bits - 1]) << 1;
        next_code[bits] = code;
    }
    let mut codes = vec![0u16; lengths.len()];
    for (i, l) in lengths.iter().enumerate() {
        if *l > 0 {
            codes[i] = next_code[*l as usize];
            next_code[*l as usize] += 1;
        }
    }
    codes
}

/// Run-length encode a code-length sequence with symbols 16, 17 and 18.
fn rle_lengths(seq: &[u8]) -> (Vec<(u8, u8, u16)>, [u32; 19]) {
    let mut out: Vec<(u8, u8, u16)> = Vec::new();
    let mut freq = [0u32; 19];
    let mut i = 0usize;
    while i < seq.len() {
        let v = seq[i];
        let mut run = 1usize;
        while i + run < seq.len() && seq[i + run] == v {
            run += 1;
        }
        i += run;
        if v == 0 {
            let mut left = run;
            while left >= 11 {
                let take = left.min(138);
                out.push((18, 7, (take - 11) as u16));
                freq[18] += 1;
                left -= take;
            }
            if left >= 3 {
                let take = left.min(10);
                out.push((17, 3, (take - 3) as u16));
                freq[17] += 1;
                left -= take;
            }
            while left > 0 {
                out.push((0, 0, 0));
                freq[0] += 1;
                left -= 1;
            }
        } else {
            out.push((v, 0, 0));
            freq[v as usize] += 1;
            let mut left = run - 1;
            while left >= 3 {
                let take = left.min(6);
                out.push((16, 2, (take - 3) as u16));
                freq[16] += 1;
                left -= take;
            }
            while left > 0 {
                out.push((v, 0, 0));
                freq[v as usize] += 1;
                left -= 1;
            }
        }
    }
    (out, freq)
}

/// One block with fixed Huffman tables: no header, so best for tiny blocks.
fn emit_fixed(bits: &mut BitOut, toks: &[Tok], last: bool) {
    bits.put(if last { 1 } else { 0 }, 1);
    bits.put(1, 2);
    for tok in toks {
        match *tok {
            Tok::Lit(b) => fixed_lit_code(bits, b),
            Tok::Match { len, dist } => {
                let (idx, extra) = find_length(len as usize);
                fixed_length_code(bits, 257 + idx as u32);
                if LEN_EXTRA[idx] > 0 {
                    bits.put(extra, LEN_EXTRA[idx] as u32);
                }
                let (didx, dextra) = find_distance(dist as usize);
                bits.put_code(didx as u32, 5);
                if DIST_EXTRA[didx] > 0 {
                    bits.put(dextra, DIST_EXTRA[didx] as u32);
                }
            }
        }
    }
    bits.put_code(0, 7);
}

/// One block with its own literal/length and distance tables.
fn emit_dynamic(bits: &mut BitOut, toks: &[Tok], last: bool) {
    let mut lit_freq = [0u32; 286];
    let mut dist_freq = [0u32; 30];
    for tok in toks {
        match *tok {
            Tok::Lit(b) => lit_freq[b as usize] += 1,
            Tok::Match { len, dist } => {
                let (idx, _) = find_length(len as usize);
                lit_freq[257 + idx] += 1;
                let (didx, _) = find_distance(dist as usize);
                dist_freq[didx] += 1;
            }
        }
    }
    lit_freq[256] += 1; // end of block

    let lit_len = huff_lengths(&lit_freq, MAX_BITS);
    let mut dist_len = huff_lengths(&dist_freq, MAX_BITS);
    if dist_len.iter().all(|l| *l == 0) {
        // No matches at all: deflate still wants one distance code.
        dist_len[0] = 1;
    }
    let lit_codes = canonical_codes(&lit_len);
    let dist_codes = canonical_codes(&dist_len);

    // Trim trailing zero lengths; the header names how many codes follow.
    let mut hlit = 286usize;
    while hlit > 257 && lit_len[hlit - 1] == 0 {
        hlit -= 1;
    }
    let mut hdist = 30usize;
    while hdist > 1 && dist_len[hdist - 1] == 0 {
        hdist -= 1;
    }
    let mut seq: Vec<u8> = Vec::with_capacity(hlit + hdist);
    seq.extend_from_slice(&lit_len[..hlit]);
    seq.extend_from_slice(&dist_len[..hdist]);
    let (rle, cl_freq) = rle_lengths(&seq);
    let cl_len = huff_lengths(&cl_freq, MAX_CL_BITS);
    let cl_codes = canonical_codes(&cl_len);
    let mut hclen = 19usize;
    while hclen > 4 && cl_len[CL_ORDER[hclen - 1]] == 0 {
        hclen -= 1;
    }

    bits.put(if last { 1 } else { 0 }, 1);
    bits.put(2, 2); // BTYPE = 10, dynamic Huffman
    bits.put((hlit - 257) as u32, 5);
    bits.put((hdist - 1) as u32, 5);
    bits.put((hclen - 4) as u32, 4);
    for i in 0..hclen {
        bits.put(cl_len[CL_ORDER[i]] as u32, 3);
    }
    for (sym, nbits, value) in &rle {
        bits.put_code(cl_codes[*sym as usize] as u32, cl_len[*sym as usize] as u32);
        if *nbits > 0 {
            bits.put(*value as u32, *nbits as u32);
        }
    }

    for tok in toks {
        match *tok {
            Tok::Lit(b) => bits.put_code(lit_codes[b as usize] as u32, lit_len[b as usize] as u32),
            Tok::Match { len, dist } => {
                let (idx, extra) = find_length(len as usize);
                let symbol = 257 + idx;
                bits.put_code(lit_codes[symbol] as u32, lit_len[symbol] as u32);
                if LEN_EXTRA[idx] > 0 {
                    bits.put(extra, LEN_EXTRA[idx] as u32);
                }
                let (didx, dextra) = find_distance(dist as usize);
                bits.put_code(dist_codes[didx] as u32, dist_len[didx] as u32);
                if DIST_EXTRA[didx] > 0 {
                    bits.put(dextra, DIST_EXTRA[didx] as u32);
                }
            }
        }
    }
    bits.put_code(lit_codes[256] as u32, lit_len[256] as u32);
}

/// Raw deflate: LZ77 with lazy matching, then blocks with their own tables.
pub fn deflate(data: &[u8]) -> Vec<u8> {
    let toks = lz77(data);
    let mut bits = BitOut::new();
    if toks.is_empty() {
        emit_fixed(&mut bits, &[], true);
        return bits.finish();
    }
    let mut at = 0usize;
    while at < toks.len() {
        let end = (at + BLOCK_TOKENS).min(toks.len());
        let last = end == toks.len();
        let block = &toks[at..end];
        if block.len() < 64 {
            emit_fixed(&mut bits, block, last);
        } else {
            emit_dynamic(&mut bits, block, last);
        }
        at = end;
    }
    bits.finish()
}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for i in 0..256u32 {
        let mut c = i;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 };
        }
        table[i as usize] = c;
    }
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

/// A complete gzip file. The card encodes it when there is one; a release is
/// written as one member per slice, and every reader concatenates members.
pub fn gzip(data: &[u8]) -> Vec<u8> {
    if let Some(out) = gzip_card(data) {
        return out;
    }
    gzip_cpu(data)
}

/// One member per slice, encoded on the card. `None` when there is no card, it
/// refuses a slice, or its own decoder does not give the bytes back.
fn gzip_card(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < CARD_SLICE_MIN || !crate::offload::has_card() {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    let mut at = 0usize;
    while at < data.len() {
        let mut end = (at + SLICE).min(data.len());
        if data.len() - end < CARD_SLICE_MIN {
            end = data.len();
        }
        out.extend_from_slice(&crate::offload::gzip_bulk(&data[at..end])?);
        at = end;
    }
    Some(out)
}

/// A complete gzip member on the CPU.
pub fn gzip_cpu(data: &[u8]) -> Vec<u8> {
    let body = deflate(data);
    let mut out = Vec::with_capacity(body.len() + 18);
    out.extend_from_slice(&[0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0x03]);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

/// Decode a gzip member. Kept here so callers that build archives do not have
/// to reach into `inflate` themselves.
pub fn inflate_bytes(data: &[u8]) -> Result<Vec<u8>, String> {
    crate::inflate::gzip_decompress(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inflate;

    fn roundtrip(data: &[u8]) {
        let packed = gzip(data);
        let back = inflate::gzip_decompress(&packed).expect("decoder rejected our output");
        assert_eq!(back, data, "round trip failed for {} bytes", data.len());
    }

    #[test]
    fn a_stream_that_ends_exactly_on_the_cap_is_accepted() {
        // The archive caps decompression at the size its header declares, so a
        // valid stream that ends on the cap must decode; only one byte more of
        // output is a refusal.
        let data = b"the exact size is the size".repeat(100);
        let packed = gzip(&data);
        let out = inflate::gzip_decompress_limited(&packed, data.len()).expect("exact cap");
        assert_eq!(out, data);
        assert!(inflate::gzip_decompress_limited(&packed, data.len() - 1).is_err());
    }

    #[test]
    fn round_trips_the_awkward_shapes() {
        roundtrip(b"");
        roundtrip(b"a");
        roundtrip(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        roundtrip(b"<html><body><h1>hello</h1></body></html>");
        // Long runs exercise the 258-byte match limit and its extra bits.
        roundtrip(&vec![b'x'; 100_000]);
        // Distances near the 32 KiB window edge.
        let mut far = vec![0u8; 70_000];
        for i in 0..far.len() {
            far[i] = (i % 251) as u8;
        }
        roundtrip(&far);
        // Enough distinct text to force several dynamic blocks.
        let mut big = Vec::new();
        for i in 0..40_000 {
            big.extend_from_slice(format!("<li id=\"{}\">row {} of the table</li>\n", i, i).as_bytes());
        }
        roundtrip(&big);
    }

    #[test]
    fn round_trips_binary_that_does_not_compress() {
        let mut data = Vec::new();
        let mut x = 0x1234_5678u32;
        for _ in 0..40_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            data.push((x & 0xff) as u8);
        }
        roundtrip(&data);
    }

    #[test]
    fn text_actually_gets_smaller() {
        let text = "<!doctype html>\n<html><head><link rel=\"stylesheet\" href=\"/a.css\"></head>\
                    <body><p>The same words, over and over, because a page repeats itself.</p></body></html>\n"
            .repeat(300);
        let packed = gzip(text.as_bytes());
        assert!(
            packed.len() * 3 < text.len(),
            "{} bytes -> {}",
            text.len(),
            packed.len()
        );
        roundtrip(text.as_bytes());
    }

    #[test]
    fn a_real_page_compresses_better_than_fixed_tables_would() {
        // A page of ordinary HTML: lowercase, spaces, short tags. Fixed Huffman
        // spends 8 bits on every one of those bytes, so a good table must do
        // clearly better than 55% of the original.
        let mut page = String::new();
        for i in 0..200 {
            page.push_str(&format!(
                "<section class=\"card\"><h2>Item {}</h2><p>Some ordinary sentence about it.</p></section>\n",
                i
            ));
        }
        let packed = gzip(page.as_bytes());
        let ratio = packed.len() as f64 / page.len() as f64;
        assert!(ratio < 0.30, "ratio {:.3} is not good enough", ratio);
        roundtrip(page.as_bytes());
    }

    /// A body of text with the repeats a real page has, big enough for a
    /// dispatch.
    fn sample(bytes: usize) -> Vec<u8> {
        let line = b"<p class=\"note\">The same words, over and over, because a page repeats itself.</p>\n";
        let mut data = Vec::with_capacity(bytes + line.len());
        while data.len() < bytes {
            data.extend_from_slice(line);
        }
        data
    }

    /// The kernel's loop, on the CPU: walk `link` from `pos`, newest first,
    /// stopping at the window edge, and keep the longest match.
    fn chain(data: &[u8], link: &[u32], pos: usize) -> (usize, usize) {
        if pos + MIN_MATCH > data.len() {
            return (0, 0);
        }
        let limit = MAX_MATCH.min(data.len() - pos);
        let (mut best_len, mut best_dist) = (0usize, 0usize);
        let mut cand = link[pos];
        let mut tries = 0usize;
        while cand != u32::MAX && tries < MAX_CHAIN {
            let at = cand as usize;
            if at >= pos || pos - at > WINDOW {
                break;
            }
            let mut len = 0usize;
            while len < limit && data[at + len] == data[pos + len] {
                len += 1;
            }
            if len > best_len {
                best_len = len;
                best_dist = pos - at;
                if len >= limit {
                    break;
                }
            }
            cand = link[at];
            tries += 1;
        }
        if best_len < MIN_MATCH {
            (0, 0)
        } else {
            (best_len, best_dist)
        }
    }

    #[test]
    fn the_link_table_holds_what_the_walk_has_in_its_head() {
        // The one-pass table must record, for every position, exactly the head
        // the sequential walk would have had at that moment.
        let data = sample(120_000);
        let link = link_table(&data);
        let mut head = vec![u32::MAX; HASH_SIZE];
        for pos in 0..data.len() {
            if pos + MIN_MATCH > data.len() {
                break;
            }
            let h = hash3(&data, pos);
            assert_eq!(link[pos], head[h], "position {}", pos);
            head[h] = pos as u32;
        }
    }

    #[test]
    fn precomputed_matches_walk_to_the_same_tokens() {
        // The card returns matches and `walk` turns them into tokens. Fed the
        // CPU's own matches, it must build exactly the stream `lz77_cpu` builds,
        // or a card-built archive would differ in ways nobody measured.
        let data = sample(400_000);
        let link = link_table(&data);
        let matches: Vec<u32> = (0..data.len())
            .map(|pos| {
                let (len, dist) = chain(&data, &link, pos);
                if len >= MIN_MATCH {
                    ((len << 16) | dist) as u32
                } else {
                    0
                }
            })
            .collect();
        let a = walk(&data, &matches).expect("the CPU's own matches must verify");
        let b = lz77_cpu(&data);
        assert_eq!(a.len(), b.len(), "token count differs");
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            match (x, y) {
                (Tok::Lit(p), Tok::Lit(q)) => assert_eq!(p, q, "literal {} differs", i),
                (
                    Tok::Match { len: l1, dist: d1 },
                    Tok::Match { len: l2, dist: d2 },
                ) => assert_eq!((l1, d1), (l2, d2), "match {} differs", i),
                _ => panic!("token {} is a different kind", i),
            }
        }
    }

    #[test]
    fn the_card_finds_the_same_matches_as_the_chain() {
        if !crate::offload::has_card() {
            return; // no card here: the CPU path is what this box runs
        }
        let data = sample(300 * 1024);
        let link = link_table(&data);
        let Some(matches) = crate::offload::lz77_links(
            &data,
            &link,
            WINDOW as u32,
            MAX_CHAIN as u32,
            MAX_MATCH as u32,
        ) else {
            return; // the driver refused the kernel; the CPU keeps the work
        };
        for pos in 0..data.len() {
            let got = ((matches[pos] >> 16) as usize, (matches[pos] & 0xffff) as usize);
            assert_eq!(got, chain(&data, &link, pos), "position {}", pos);
        }
    }

    #[test]
    fn lengths_are_optimal_under_the_limit() {
        // Every code must fit in 15 bits and the code must be complete.
        let mut freq = vec![1u32; 286];
        freq[0] = 100_000;
        freq[1] = 50_000;
        let lengths = huff_lengths(&freq, MAX_BITS);
        assert!(lengths.iter().all(|l| *l as usize <= MAX_BITS));
        let kraft: f64 = lengths
            .iter()
            .filter(|l| **l > 0)
            .map(|l| 2f64.powi(-(*l as i32)))
            .sum();
        assert!((kraft - 1.0).abs() < 1e-9, "kraft {}", kraft);
    }
}
