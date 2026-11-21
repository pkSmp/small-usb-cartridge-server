//! A deflate (RFC 1951) and gzip (RFC 1952) encoder, in std only.
//!
//! There is no compression library to link, so the fleet builds its own. The
//! encoder is deliberately simple: a greedy LZ77 pass with a 32 KiB window, and
//! deflate's *fixed* Huffman tables, which need no code-length header and are
//! within a few percent of a tuned encoder on the text a static site is made of.
//! It is used for two things: pre-compressed variants of every text asset, so
//! the hot path ships fewer bytes, and the disaster-recovery archive.
//!
//! Decoding is not duplicated here — `inflate.rs` already has a decoder, and the
//! tests round-trip through it.

const WINDOW: usize = 32 * 1024;
const MAX_MATCH: usize = 258;
const MIN_MATCH: usize = 3;
const HASH_BITS: usize = 15;
const MAX_CHAIN: usize = 48;

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

fn lit_code(bits: &mut BitOut, byte: u8) {
    match byte {
        0..=143 => bits.put_code(0x30 + byte as u32, 8),
        144..=255 => bits.put_code(0x190 + (byte as u32 - 144), 9),
        _ => unreachable!(),
    }
}

fn length_code(bits: &mut BitOut, symbol: u32) {
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
    // Deflate's distance codes are 1-based: code 0 covers distance 1, and the
    // extra bits carry `dist - base`. Subtracting one here underflowed at
    // distance 1 and corrupted every stream that used it.
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
    (v.wrapping_mul(0x1e35_a7bd) >> (32 - HASH_BITS)) as usize & ((1 << HASH_BITS) - 1)
}

/// Raw deflate, one final fixed-Huffman block.
pub fn deflate(data: &[u8]) -> Vec<u8> {
    let mut bits = BitOut::new();
    bits.put(1, 1); // BFINAL
    bits.put(1, 2); // BTYPE = 01, fixed Huffman

    let n = data.len();
    let mut head = vec![usize::MAX; 1 << HASH_BITS];
    let mut prev = vec![usize::MAX; n.max(1)];
    let mut pos = 0usize;
    while pos < n {
        let (mut best_len, mut best_dist) = (0usize, 0usize);
        if pos + MIN_MATCH <= n {
            let h = hash3(data, pos);
            let mut cand = head[h];
            let mut tries = 0;
            while cand != usize::MAX && tries < MAX_CHAIN {
                if pos <= cand || pos - cand > WINDOW {
                    if pos <= cand {
                        break;
                    }
                    cand = prev[cand];
                    tries += 1;
                    continue;
                }
                let limit = MAX_MATCH.min(n - pos);
                let mut len = 0usize;
                while len < limit && data[cand + len] == data[pos + len] {
                    len += 1;
                }
                if len > best_len {
                    best_len = len;
                    best_dist = pos - cand;
                    if len >= limit {
                        break;
                    }
                }
                cand = prev[cand];
                tries += 1;
            }
        }

        if best_len >= MIN_MATCH {
            let (idx, extra) = find_length(best_len);
            length_code(&mut bits, 257 + idx as u32);
            if LEN_EXTRA[idx] > 0 {
                bits.put(extra, LEN_EXTRA[idx] as u32);
            }
            let (didx, dextra) = find_distance(best_dist);
            bits.put_code(didx as u32, 5);
            if DIST_EXTRA[didx] > 0 {
                bits.put(dextra, DIST_EXTRA[didx] as u32);
            }
            // Every position inside the match joins the hash chains, or later
            // matches would miss what just went past.
            for k in 0..best_len {
                let p = pos + k;
                if p + MIN_MATCH <= n {
                    let h = hash3(data, p);
                    prev[p] = head[h];
                    head[h] = p;
                }
            }
            pos += best_len;
        } else {
            lit_code(&mut bits, data[pos]);
            if pos + MIN_MATCH <= n {
                let h = hash3(data, pos);
                prev[pos] = head[h];
                head[h] = pos;
            }
            pos += 1;
        }
    }

    // End of block.
    bits.put_code(0, 7);
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

/// A complete gzip member.
pub fn gzip(data: &[u8]) -> Vec<u8> {
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
}
