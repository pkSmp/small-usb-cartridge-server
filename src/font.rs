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
    // The file's own unicode table is deliberately ignored. One built by an
    // old generator carried two entries, and the half blocks then mapped to
    // glyph 0x04 (a star), which turned the boot art into punctuation. The
    // console's character set is fixed, so load() sends the CP437 table.
    let _ = mode;
    Ok(Psf { charcount, height, glyphs })
}

fn open_console() -> Result<fs::File, String> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty1")
        .map_err(|e| format!("tty1: {}", e))
}

/// The console's own 8-bit character set, as Unicode code points for glyphs
/// 0x00..0xFF. Glyph 0xDB is the full block, 0xDC the lower half and 0xDF the
/// upper half; those three are what the boot art draws with.
fn cp437() -> [u16; 256] {
    [
        0x0000, 0x0001, 0x0002, 0x0003, 0x0004, 0x0005, 0x0006, 0x0007, 0x0008, 0x0009, 0x000A,
        0x000B, 0x000C, 0x000D, 0x000E, 0x000F, 0x0010, 0x0011, 0x0012, 0x0013, 0x0014, 0x0015,
        0x0016, 0x0017, 0x0018, 0x0019, 0x001A, 0x001B, 0x001C, 0x001D, 0x001E, 0x001F, 0x0020,
        0x0021, 0x0022, 0x0023, 0x0024, 0x0025, 0x0026, 0x0027, 0x0028, 0x0029, 0x002A, 0x002B,
        0x002C, 0x002D, 0x002E, 0x002F, 0x0030, 0x0031, 0x0032, 0x0033, 0x0034, 0x0035, 0x0036,
        0x0037, 0x0038, 0x0039, 0x003A, 0x003B, 0x003C, 0x003D, 0x003E, 0x003F, 0x0040, 0x0041,
        0x0042, 0x0043, 0x0044, 0x0045, 0x0046, 0x0047, 0x0048, 0x0049, 0x004A, 0x004B, 0x004C,
        0x004D, 0x004E, 0x004F, 0x0050, 0x0051, 0x0052, 0x0053, 0x0054, 0x0055, 0x0056, 0x0057,
        0x0058, 0x0059, 0x005A, 0x005B, 0x005C, 0x005D, 0x005E, 0x005F, 0x0060, 0x0061, 0x0062,
        0x0063, 0x0064, 0x0065, 0x0066, 0x0067, 0x0068, 0x0069, 0x006A, 0x006B, 0x006C, 0x006D,
        0x006E, 0x006F, 0x0070, 0x0071, 0x0072, 0x0073, 0x0074, 0x0075, 0x0076, 0x0077, 0x0078,
        0x0079, 0x007A, 0x007B, 0x007C, 0x007D, 0x007E, 0x007F, 0x00C7, 0x00FC, 0x00E9, 0x00E2,
        0x00E4, 0x00E0, 0x00E5, 0x00E7, 0x00EA, 0x00EB, 0x00E8, 0x00EF, 0x00EE, 0x00EC, 0x00C4,
        0x00C5, 0x00C9, 0x00E6, 0x00C6, 0x00F4, 0x00F6, 0x00F2, 0x00FB, 0x00F9, 0x00FF, 0x00D6,
        0x00DC, 0x00A2, 0x00A3, 0x00A5, 0x20A7, 0x0192, 0x00E1, 0x00ED, 0x00F3, 0x00FA, 0x00F1,
        0x00D1, 0x00AA, 0x00BA, 0x00BF, 0x2310, 0x00AC, 0x00BD, 0x00BC, 0x00A1, 0x00AB, 0x00BB,
        0x2591, 0x2592, 0x2593, 0x2502, 0x2524, 0x2561, 0x2562, 0x2556, 0x2555, 0x2563, 0x2551,
        0x2557, 0x255D, 0x255C, 0x255B, 0x2510, 0x2514, 0x2534, 0x252C, 0x251C, 0x2500, 0x253C,
        0x255E, 0x255F, 0x255A, 0x2554, 0x2569, 0x2566, 0x2560, 0x2550, 0x256C, 0x2567, 0x2568,
        0x2564, 0x2565, 0x2559, 0x2558, 0x2552, 0x2553, 0x256B, 0x256A, 0x2518, 0x250C, 0x2588,
        0x2584, 0x258C, 0x2590, 0x2580, 0x03B1, 0x00DF, 0x0393, 0x03C0, 0x03A3, 0x03C3, 0x00B5,
        0x03C4, 0x03A6, 0x0398, 0x03A9, 0x03B4, 0x221E, 0x03C6, 0x03B5, 0x2229, 0x2261, 0x00B1,
        0x2265, 0x2264, 0x2320, 0x2321, 0x00F7, 0x2248, 0x00B0, 0x2219, 0x00B7, 0x221A, 0x207F,
        0x00B2, 0x25A0, 0x00A0,
    ]
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

    // The font file's own table cannot be trusted: one built by an old
    // generator carried a two-entry table, so every half block in the boot art
    // landed on glyph 0x04 (a star) instead of the block at 0xDF/0xDC/0xDB.
    // The console's character set is fixed, so send that instead.
    let mut table = cp437().to_vec();
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
