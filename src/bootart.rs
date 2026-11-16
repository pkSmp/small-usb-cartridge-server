//! Boot art service: wait for tty1, load the half-block font, draw the mosaic,
//! check it against the framebuffer and leave the markers the status service
//! and importer expect.

use crate::art;
use crate::sys;
use std::fs;
use std::path::Path;
use std::process::Command;

const ART_DIR: &str = "/var/lib/cartridge/art";
const STATE_PNG: &str = "/var/lib/cartridge/boot.png";
const FONT: &str = "/usr/local/share/cartridge/cartridge.psf.gz";
const LOG: &str = "/var/log/cartridge/art.log";
const SCRATCH_ROW: usize = 67;

fn log(text: &str) {
    sys::append(LOG, &format!("{} {}\n", sys::fmt_time("%F %T"), text));
}

fn probe() -> bool {
    if !Path::new(art::TTY).exists() {
        return false;
    }
    let marker = format!("ba{}{}", std::process::id(), sys::now_secs() % 1000);
    art::write_tty(format!("\x1b[{};1H{}", SCRATCH_ROW, marker).as_bytes()).ok();
    sys::sleep_ms(300);
    if let Ok(data) = fs::read("/dev/vcs1") {
        let text = String::from_utf8_lossy(&data);
        if text.contains(&marker) {
            art::write_tty(format!("\x1b[{};1H\x1b[K", SCRATCH_ROW).as_bytes()).ok();
            return true;
        }
    }
    false
}

fn cell_char(x: usize, y: usize) -> Option<u8> {
    let data = fs::read("/dev/vcs1").ok()?;
    if data.len() < (y * 240 + x) * 2 + 1 {
        return None;
    }
    Some(data[(y * 240 + x) * 2])
}

fn ensure_font() -> bool {
    if !Path::new(FONT).is_file() {
        return false;
    }
    for attempt in 1..=3 {
        crate::font::activate_tty1();
        // The kernel refuses KDFONTOP's unicode-table ops on this vt (EINVAL),
        // and glyphs without the table map the half blocks to the wrong cells,
        // so the font still has to come from kbd's setfont.
        let ok = Command::new("setfont")
            .args(["-C", art::TTY, FONT])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        art::write_tty("\x1b[66;200H\x1b[0m\u{2580}".as_bytes()).ok();
        sys::sleep_ms(300);
        let cell = cell_char(199, 65);
        art::write_tty("\x1b[66;200H ".as_bytes()).ok();
        if ok && cell == Some(0xDF) {
            log(&format!("console font active (attempt {})", attempt));
            return true;
        }
        sys::sleep_ms(1000);
    }
    false
}

pub fn run() -> Result<i32, String> {
    log(&format!("start tty={} wait=60s", art::TTY));

    let art_path = format!("{}/art.bin", ART_DIR);
    let meta_path = format!("{}/art.meta", ART_DIR);
    if Path::new(STATE_PNG).is_file() {
        let newer = match (fs::metadata(STATE_PNG), fs::metadata(&art_path)) {
            (Ok(p), Ok(a)) => p.modified().ok() > a.modified().ok(),
            (Ok(_), Err(_)) => true,
            _ => false,
        };
        if newer {
            match art::cmd_build(STATE_PNG, ART_DIR) {
                Ok(0) => log(&format!("art rebuilt from {}", STATE_PNG)),
                _ => log(&format!("warning: could not build art from {}", STATE_PNG)),
            }
        }
    }
    if !Path::new(&art_path).is_file() {
        log("no art file, nothing to draw");
        return Ok(0);
    }

    let mut ready = false;
    for _ in 0..60 {
        if probe() {
            ready = true;
            break;
        }
        sys::sleep_ms(1000);
    }
    if !ready {
        log("gave up: the VT never became writable");
        return Ok(0);
    }

    let _ = fs::write("/proc/sys/kernel/printk", "1\n");

    if !ensure_font() {
        log("note: font probe inconclusive, drawing anyway");
    }

    match art::cmd_draw(&art_path) {
        Ok(0) => {}
        _ => log("warning: draw failed"),
    }

    match art::cmd_verify(&format!("{}/map.json", ART_DIR)) {
        Ok(0) => log("framebuffer check: PASS"),
        _ => log("framebuffer check: FAIL (see above)"),
    }

    let rows = fs::read_to_string(&meta_path)
        .ok()
        .and_then(|t| t.split_whitespace().nth(1).and_then(|v| v.parse::<usize>().ok()))
        .unwrap_or(32);
    let _ = fs::write("/run/cartridge-art.rows", format!("{}\n", rows));
    let _ = fs::write("/run/cartridge-art.done", "");
    log(&format!("art drawn ({} rows)", rows));
    Ok(0)
}
