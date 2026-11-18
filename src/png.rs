//! Just enough PNG to read the cartridge's boot image and write the proof
//! images. 8-bit deep, non-interlaced, colour types 0/2/3/4/6.

use crate::inflate;

pub struct Image {
    pub w: usize,
    pub h: usize,
    pub rgb: Vec<u8>,
}

impl Image {
    pub fn new(w: usize, h: usize) -> Image {
        Image { w, h, rgb: vec![0; w * h * 3] }
    }

    pub fn get(&self, x: usize, y: usize) -> (u8, u8, u8) {
        let o = (y * self.w + x) * 3;
        (self.rgb[o], self.rgb[o + 1], self.rgb[o + 2])
    }

    pub fn put(&mut self, x: usize, y: usize, c: (u8, u8, u8)) {
        let o = (y * self.w + x) * 3;
        self.rgb[o] = c.0;
        self.rgb[o + 1] = c.1;
        self.rgb[o + 2] = c.2;
    }

    /// Integer nearest-neighbour upscale, the PNG equivalent of PIL's NEAREST.
    pub fn scaled(&self, factor: usize) -> Image {
        let mut out = Image::new(self.w * factor, self.h * factor);
        for y in 0..out.h {
            for x in 0..out.w {
                let c = self.get(x / factor, y / factor);
                out.put(x, y, c);
            }
        }
        out
    }
}

fn be32(b: &[u8]) -> u32 {
    ((b[0] as u32) << 24) | ((b[1] as u32) << 16) | ((b[2] as u32) << 8) | b[3] as u32
}

pub fn decode(data: &[u8]) -> Result<Image, String> {
    if data.len() < 8 || &data[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err("png: not a PNG file".into());
    }
    let mut pos = 8usize;
    let mut w = 0usize;
    let mut h = 0usize;
    let mut depth = 0u8;
    let mut colour = 0u8;
    let mut idat: Vec<u8> = Vec::new();
    let mut palette: Vec<(u8, u8, u8)> = Vec::new();
    let mut interlace = 0u8;
    while pos + 8 <= data.len() {
        let len = be32(&data[pos..]) as usize;
        let kind = &data[pos + 4..pos + 8];
        let body = &data[pos + 8..(pos + 8 + len).min(data.len())];
        match kind {
            b"IHDR" => {
                if body.len() < 13 {
                    return Err("png: short IHDR".into());
                }
                w = be32(&body[0..]) as usize;
                h = be32(&body[4..]) as usize;
                depth = body[8];
                colour = body[9];
                interlace = body[12];
            }
            b"PLTE" => {
                palette = body.chunks(3).map(|c| (c[0], c[1], c[2])).collect();
            }
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        pos += 12 + len;
    }
    if w == 0 || h == 0 {
        return Err("png: missing IHDR".into());
    }
    // Palette and grey images are allowed at 1, 2 and 4 bits per pixel, which
    // is how most logos and icons are saved; everything else is 8-bit.
    let bits = match (depth, colour) {
        (8, 0) | (8, 2) | (8, 3) | (8, 4) | (8, 6) => 8usize,
        (1 | 2 | 4, 0) | (1 | 2 | 4, 3) => depth as usize,
        _ => {
            return Err(format!(
                "png: unsupported bit depth {} for colour type {}",
                depth, colour
            ))
        }
    };
    if interlace != 0 {
        return Err("png: interlaced images are not supported".into());
    }
    let channels = match colour {
        0 => 1,
        2 => 3,
        3 => 1,
        4 => 2,
        6 => 4,
        other => return Err(format!("png: unsupported colour type {}", other)),
    };
    let raw = inflate::zlib_decompress(&idat)?;
    let stride = if bits < 8 { (w * bits).div_ceil(8) } else { w * channels };
    if raw.len() < h * (stride + 1) {
        return Err("png: image data is short".into());
    }
    let bpp = if bits < 8 { 1 } else { channels };
    let mut pixels = vec![0u8; h * stride];
    for y in 0..h {
        let src = &raw[y * (stride + 1)..];
        let filter = src[0];
        let line = &src[1..1 + stride];
        let (prev, cur) = pixels.split_at_mut(y * stride);
        let prev_line: &[u8] = if y == 0 {
            &[]
        } else {
            &prev[(y - 1) * stride..]
        };
        let out = &mut cur[..stride];
        for i in 0..stride {
            let a = if i >= bpp { out[i - bpp] } else { 0 };
            let b = if y > 0 { prev_line[i] } else { 0 };
            let c = if y > 0 && i >= bpp { prev_line[i - bpp] } else { 0 };
            let v = line[i];
            out[i] = match filter {
                0 => v,
                1 => v.wrapping_add(a),
                2 => v.wrapping_add(b),
                3 => v.wrapping_add(((a as u16 + b as u16) / 2) as u8),
                4 => {
                    let p = a as i16 + b as i16 - c as i16;
                    let pa = (p - a as i16).abs();
                    let pb = (p - b as i16).abs();
                    let pc = (p - c as i16).abs();
                    let pred = if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    };
                    v.wrapping_add(pred)
                }
                other => return Err(format!("png: bad filter {}", other)),
            };
        }
    }

    let mut img = Image::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let o = y * stride + x * channels;
            let (r, g, b) = if bits < 8 {
                // Samples are packed high bits first: 1-bit is 8 pixels a byte.
                let byte = pixels[y * stride + (x * bits) / 8];
                let shift = 8 - bits - (x * bits) % 8;
                let max = (1u16 << bits) - 1;
                let sample = ((byte >> shift) as u16) & max;
                if colour == 3 {
                    *palette.get(sample as usize).unwrap_or(&(0, 0, 0))
                } else {
                    let v = (sample * 255 / max) as u8;
                    (v, v, v)
                }
            } else {
                match colour {
                    0 => (pixels[o], pixels[o], pixels[o]),
                    2 => (pixels[o], pixels[o + 1], pixels[o + 2]),
                    3 => {
                        let idx = pixels[o] as usize;
                        *palette.get(idx).unwrap_or(&(0, 0, 0))
                    }
                    4 => (pixels[o], pixels[o], pixels[o]),
                    6 => (pixels[o], pixels[o + 1], pixels[o + 2]),
                    _ => (0, 0, 0),
                }
            };
            img.put(x, y, (r, g, b));
        }
    }
    Ok(img)
}

fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for n in 0..256u32 {
        let mut c = n;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
        }
        table[n as usize] = c;
    }
    table
}

fn crc32(data: &[u8]) -> u32 {
    let table = crc_table();
    let mut c = 0xFFFFFFFFu32;
    for &b in data {
        c = table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFFFFFF
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let mut crc_input = Vec::with_capacity(4 + body.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(body);
    out.extend_from_slice(&crc_input);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

pub fn encode_rgb(img: &Image) -> Vec<u8> {
    let mut raw = Vec::with_capacity(img.h * (img.w * 3 + 1));
    for y in 0..img.h {
        raw.push(0); // filter: none
        raw.extend_from_slice(&img.rgb[y * img.w * 3..(y + 1) * img.w * 3]);
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(img.w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(img.h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &inflate::zlib_store(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}
