//! Console font loading without kbd's setfont: parse PSF1 (the format kbd
//! wrote) and hand the glyphs and the unicode table to the kernel through
//! KDFONTOP.

use crate::inflate;
use crate::sys;
use std::fs;
use std::os::raw::{c_int, c_void};

const KDFONTOP: u64 = 0x4B72;
const KD_FONT_OP_SET: u32 = 0;
const KD_FONT_OP_SET_TABLE: u32 = 3;
const KD_FONT_OP_GET_TABLE: u32 = 4;
const VT_ACTIVATE: u64 = 0x5606;

#[repr(C)]
struct ConsoleFontOp {
    op: u32,
    flags: u32,
    width: u32,
    height: u32,
    charcount: u32,
    data: *mut u8,
}

struct Psf {
    charcount: usize,
    height: usize,
    glyphs: Vec<u8>,
    table: Vec<u16>,
}

fn parse_psf(data: &[u8]) -> Result<Psf, String> {
    if data.len() < 4 || data[0] != 0x36 || data[1] != 0x04 {
        return Err("font: not a PSF1 file".into());
    }
    let mode = data[2];
    let height = data[3] as usize;
    let charcount = if mode & 0x01 != 0 { 512 } else { 256 };
    let glyph_bytes = charcount * height;
    if data.len() < 4 + glyph_bytes {
        return Err("font: truncated glyph data".into());
    }
    let glyphs = data[4..4 + glyph_bytes].to_vec();
    let mut table = vec![0u16; charcount];
    if mode & 0x02 != 0 {
        // Unicode table: one list of little-endian u16 code points per glyph,
        // each list terminated by 0xFFFF. The kernel takes the first code
        // point of each glyph, which is all a 256-glyph font needs.
        let mut pos = 4 + glyph_bytes;
        let mut glyph = 0usize;
        while glyph < charcount && pos + 1 < data.len() {
            let mut first = true;
            loop {
                if pos + 1 >= data.len() {
                    break;
                }
                let value = data[pos] as u16 | (data[pos + 1] as u16) << 8;
                pos += 2;
                if value == 0xFFFF {
                    break;
                }
                if first {
                    table[glyph] = value;
                    first = false;
                }
            }
            glyph += 1;
        }
    }
    Ok(Psf { charcount, height, glyphs, table })
}

fn open_console() -> Result<fs::File, String> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty1")
        .map_err(|e| format!("tty1: {}", e))
}

pub fn activate_tty1() {
    if let Ok(file) = fs::OpenOptions::new().read(true).write(true).open("/dev/tty0") {
        use std::os::unix::io::AsRawFd;
        let _ = sys::ioctl_ptr(file.as_raw_fd(), VT_ACTIVATE, 1usize as *mut c_void);
    }
}

/// Load the font and its unicode table. Returns Ok when the kernel accepted
/// both, which the caller then proves by looking at a drawn half block.
pub fn load(path: &str) -> Result<(), String> {
    let raw = fs::read(path).map_err(|e| format!("font: {}", e))?;
    let data = if raw.len() > 2 && raw[0] == 0x1F && raw[1] == 0x8B {
        inflate::gzip_decompress(&raw)?
    } else {
        raw
    };
    let psf = parse_psf(&data)?;
    let file = open_console()?;
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();

    let mut glyphs = psf.glyphs.clone();
    let mut op = ConsoleFontOp {
        op: KD_FONT_OP_SET,
        flags: 0,
        width: 8,
        height: psf.height as u32,
        charcount: psf.charcount as u32,
        data: glyphs.as_mut_ptr(),
    };
    sys::ioctl_ptr(fd, KDFONTOP, &mut op as *mut ConsoleFontOp as *mut c_void)
        .map_err(|e| format!("font: glyph load failed: {}", e))?;

    let mut table = psf.table.clone();
    let mut op = ConsoleFontOp {
        op: KD_FONT_OP_SET_TABLE,
        flags: 0,
        width: 8,
        height: psf.height as u32,
        charcount: psf.charcount as u32,
        data: table.as_mut_ptr() as *mut u8,
    };
    sys::ioctl_ptr(fd, KDFONTOP, &mut op as *mut ConsoleFontOp as *mut c_void)
        .map_err(|e| format!("font: unicode table load failed: {}", e))?;
    Ok(())
}

pub fn dump_table() -> Result<Vec<u16>, String> {
    let file = open_console()?;
    use std::os::unix::io::AsRawFd;
    let mut table = vec![0u16; 512];
    let mut op = ConsoleFontOp {
        op: KD_FONT_OP_GET_TABLE,
        flags: 0,
        width: 8,
        height: 16,
        charcount: 512,
        data: table.as_mut_ptr() as *mut u8,
    };
    sys::ioctl_ptr(file.as_raw_fd(), KDFONTOP, &mut op as *mut ConsoleFontOp as *mut c_void)
        .map_err(|e| format!("font: table read failed: {}", e))?;
    Ok(table)
}

#[allow(dead_code)]
fn unused(_: c_int) {}
