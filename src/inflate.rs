//! DEFLATE (RFC 1951) and the zlib wrapper (RFC 1950). Needed because the
//! boot image is a PNG and the console font is a gzip file, and the rewrite
//! has no external crates.

pub struct BitReader<'a> {
    data: &'a [u8],
    byte: usize,
    bit: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, byte: 0, bit: 0 }
    }

    fn bits(&mut self, n: u32) -> Result<u32, String> {
        let mut v = 0u32;
        for i in 0..n {
            if self.byte >= self.data.len() {
                return Err("deflate: unexpected end of data".into());
            }
            let bit = (self.data[self.byte] >> self.bit) & 1;
            v |= (bit as u32) << i;
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.byte += 1;
            }
        }
        Ok(v)
    }

    fn align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.byte += 1;
        }
    }
}

struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn build(lengths: &[u8]) -> Result<Huffman, String> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        let mut offs = [0u16; 16];
        for i in 1..16 {
            offs[i] = offs[i - 1] + counts[i - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                symbols[offs[len as usize] as usize] = sym as u16;
                offs[len as usize] += 1;
            }
        }
        Ok(Huffman { counts, symbols })
    }

    fn decode(&self, br: &mut BitReader) -> Result<u16, String> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..=15usize {
            code |= br.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - count < first {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err("deflate: bad Huffman code".into())
    }
}

const LEN_BASE: [u32; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
    131, 163, 195, 227, 258,
];
const LEN_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u32; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
    13, 13,
];

fn inflate_into(br: &mut BitReader, out: &mut Vec<u8>, limit: usize) -> Result<(), String> {
    loop {
        let last = br.bits(1)?;
        let kind = br.bits(2)?;
        match kind {
            0 => {
                br.align();
                let len = br.bits(16)? as usize;
                let nlen = br.bits(16)? as usize;
                if len != (!nlen & 0xFFFF) {
                    return Err("deflate: stored block length mismatch".into());
                }
                if out.len() + len > limit {
                    return Err("inflate: output exceeds the limit".into());
                }
                for _ in 0..len {
                    out.push(br.bits(8)? as u8);
                }
            }
            1 => {
                let mut lit_lengths = [0u8; 288];
                for (i, l) in lit_lengths.iter_mut().enumerate() {
                    *l = if i < 144 {
                        8
                    } else if i < 256 {
                        9
                    } else if i < 280 {
                        7
                    } else {
                        8
                    };
                }
                let mut dist_lengths = [0u8; 32];
                for l in dist_lengths.iter_mut() {
                    *l = 5;
                }
                let lit = Huffman::build(&lit_lengths)?;
                let dist = Huffman::build(&dist_lengths)?;
                inflate_block(br, out, &lit, &dist, limit)?;
            }
            2 => {
                let hlit = br.bits(5)? as usize + 257;
                let hdist = br.bits(5)? as usize + 1;
                let hclen = br.bits(4)? as usize + 4;
                const ORDER: [usize; 19] = [
                    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
                ];
                let mut cl_lengths = [0u8; 19];
                for i in 0..hclen {
                    cl_lengths[ORDER[i]] = br.bits(3)? as u8;
                }
                let cl = Huffman::build(&cl_lengths)?;
                let mut lengths = vec![0u8; hlit + hdist];
                let mut i = 0;
                while i < lengths.len() {
                    let sym = cl.decode(br)?;
                    match sym {
                        0..=15 => {
                            lengths[i] = sym as u8;
                            i += 1;
                        }
                        16 => {
                            if i == 0 {
                                return Err("deflate: repeat with no previous length".into());
                            }
                            let prev = lengths[i - 1];
                            let repeat = 3 + br.bits(2)? as usize;
                            for _ in 0..repeat {
                                if i < lengths.len() {
                                    lengths[i] = prev;
                                    i += 1;
                                }
                            }
                        }
                        17 => {
                            let repeat = 3 + br.bits(3)? as usize;
                            i = (i + repeat).min(lengths.len());
                        }
                        18 => {
                            let repeat = 11 + br.bits(7)? as usize;
                            i = (i + repeat).min(lengths.len());
                        }
                        _ => return Err("deflate: bad code length symbol".into()),
                    }
                }
                let lit = Huffman::build(&lengths[..hlit])?;
                let dist = Huffman::build(&lengths[hlit..])?;
                inflate_block(br, out, &lit, &dist, limit)?;
            }
            _ => return Err("deflate: reserved block type".into()),
        }
        if last == 1 {
            return Ok(());
        }
    }
}

fn inflate_block(
    br: &mut BitReader,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
    limit: usize,
) -> Result<(), String> {
    loop {
        // The limit is checked against what a symbol is about to emit, not
        // against the output so far: a stream that ends exactly on the cap is
        // valid, and rejecting it made every exact-size decompression fail.
        let sym = lit.decode(br)? as usize;
        if sym < 256 {
            if out.len() >= limit {
                return Err("inflate: output exceeds the limit".into());
            }
            out.push(sym as u8);
        } else if sym == 256 {
            return Ok(());
        } else {
            let idx = sym - 257;
            if idx >= LEN_BASE.len() {
                return Err("deflate: bad length symbol".into());
            }
            let len = LEN_BASE[idx] as usize + br.bits(LEN_EXTRA[idx])? as usize;
            if out.len() + len > limit {
                return Err("inflate: output exceeds the limit".into());
            }
            let dsym = dist.decode(br)? as usize;
            if dsym >= DIST_BASE.len() {
                return Err("deflate: bad distance symbol".into());
            }
            let d = DIST_BASE[dsym] as usize + br.bits(DIST_EXTRA[dsym])? as usize;
            if d == 0 || d > out.len() {
                return Err("deflate: distance past start of output".into());
            }
            let start = out.len() - d;
            for k in 0..len {
                let byte = out[start + k];
                out.push(byte);
            }
        }
    }
}

pub fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    zlib_decompress_limited(data, crate::guard::MAX_INFLATE)
}

/// Decompress with an explicit output cap. The cap is checked as the output
/// grows, so a 10 KB bomb that expands to 10 GB stops at the cap.
pub fn zlib_decompress_limited(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    // The card decodes it when there is one, and checks the stream's own
    // Adler-32 before saying so. A refusal (bad stream, no card, no memory)
    // falls through to the CPU, which is also what reports the error.
    if let Some(out) = crate::offload::gunzip_bulk(data, limit, true) {
        return Ok(out);
    }
    if data.len() < 2 {
        return Err("zlib: too short".into());
    }
    let cmf = data[0];
    let flg = data[1];
    if cmf & 0x0F != 8 {
        return Err("zlib: not a deflate stream".into());
    }
    if ((cmf as u16) << 8 | flg as u16) % 31 != 0 {
        return Err("zlib: bad header checksum".into());
    }
    if flg & 0x20 != 0 {
        return Err("zlib: preset dictionaries are not supported".into());
    }
    let mut out = Vec::new();
    let mut br = BitReader::new(&data[2..]);
    inflate_into(&mut br, &mut out, limit)?;
    Ok(out)
}

pub fn gzip_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    gzip_decompress_limited(data, crate::guard::MAX_INFLATE)
}

/// Decompress a gzip stream with an explicit output cap.
pub fn gzip_decompress_limited(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    // The card decodes it when there is one, checking each member's CRC-32 and
    // length as it goes; only a refusal comes back here.
    if let Some(out) = crate::offload::gunzip_bulk(data, limit, false) {
        return Ok(out);
    }
    if data.len() < 18 || data[0] != 0x1F || data[1] != 0x8B {
        return Err("gzip: bad magic".into());
    }
    let flags = data[3];
    let mut i = 10usize;
    if flags & 0x04 != 0 {
        if i + 2 > data.len() {
            return Err("gzip: truncated header".into());
        }
        let xlen = data[i] as usize | (data[i + 1] as usize) << 8;
        i += 2 + xlen;
    }
    if flags & 0x08 != 0 {
        while i < data.len() && data[i] != 0 {
            i += 1;
        }
        i += 1;
    }
    if flags & 0x10 != 0 {
        while i < data.len() && data[i] != 0 {
            i += 1;
        }
        i += 1;
    }
    if flags & 0x02 != 0 {
        i += 2;
    }
    if i >= data.len() {
        return Err("gzip: no deflate payload".into());
    }
    let mut out = Vec::new();
    let mut br = BitReader::new(&data[i..]);
    inflate_into(&mut br, &mut out, limit)?;
    Ok(out)
}

pub fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// zlib stream made of stored blocks: no compression, no CPU cost, and every
/// decoder accepts it. Used for the PNGs this program writes.
pub fn zlib_store(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 65535 * 5 + 16);
    out.extend_from_slice(&[0x78, 0x01]);
    let mut pos = 0;
    while pos < data.len() {
        let take = (data.len() - pos).min(65535);
        let last = if pos + take >= data.len() { 1u8 } else { 0u8 };
        out.push(last);
        out.extend_from_slice(&(take as u16).to_le_bytes());
        out.extend_from_slice(&(!(take as u16)).to_le_bytes());
        out.extend_from_slice(&data[pos..pos + take]);
        pos += take;
    }
    let sum = adler32(data);
    out.extend_from_slice(&sum.to_be_bytes());
    out
}
