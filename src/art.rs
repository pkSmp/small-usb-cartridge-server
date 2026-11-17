//! The console mosaic: calibration against the real framebuffer, colour
//! quantisation, the escape payload, drawing and proof by sampling /dev/fb0.

use crate::json::{self, Value};
use crate::png::{self, Image};
use crate::sys;
use std::collections::HashMap;
use std::fs;
use std::os::raw::{c_int, c_void};

type Rgb = (u8, u8, u8);

pub const FB: &str = "/dev/fb0";
pub const TTY: &str = "/dev/tty1";
const PIO_CMAP: u64 = 0x4B71;
const CALIB: &str = "/var/lib/cartridge/calib.json";

/// Only genuinely near-black pixels are left blank. Grey and dark grey have
/// a channel well above this and stay available to the image.
const BLACK_MAX: u8 = 24;
/// Colours closer than this are visually the same; merge them instead of
/// spending two slots on almost-identical shades.
const MERGE_DIST: f64 = 24.0;
/// One slot is the black terminal background; the other 15 are usable colours.
const MAX_COLOURS: usize = 15;
const BLOCK: &str = "\u{2588}";
const UPPER: &str = "\u{2580}";
const LOWER: &str = "\u{2584}";

const CALIB_PALETTE: [&str; 16] = [
    "#000000", "#FF0000", "#00A000", "#0000FF", "#FFFF00", "#FF00FF", "#00FFFF", "#FFFFFF",
    "#804000", "#008040", "#400080", "#804080", "#C0C0C0", "#404040", "#FF8000", "#8000FF",
];

const FG_CODES: [u8; 16] = [30, 31, 32, 33, 34, 35, 36, 37, 90, 91, 92, 93, 94, 95, 96, 97];
const BG_CODES: [u8; 8] = [40, 41, 42, 43, 44, 45, 46, 47];

pub fn hexs(c: Rgb) -> String {
    format!("#{:02X}{:02X}{:02X}", c.0, c.1, c.2)
}

fn parse_hex(s: &str) -> Option<Rgb> {
    if s.len() != 7 || !s.starts_with('#') {
        return None;
    }
    let r = u8::from_str_radix(&s[1..3], 16).ok()?;
    let g = u8::from_str_radix(&s[3..5], 16).ok()?;
    let b = u8::from_str_radix(&s[5..7], 16).ok()?;
    Some((r, g, b))
}

fn dist(a: Rgb, b: Rgb) -> f64 {
    let dr = a.0 as f64 - b.0 as f64;
    let dg = a.1 as f64 - b.1 as f64;
    let db = a.2 as f64 - b.2 as f64;
    (dr * dr + dg * dg + db * db).sqrt()
}

fn nearest(c: Rgb, pal: &[Rgb]) -> usize {
    let mut best = 0;
    let mut bestd = f64::MAX;
    for (i, p) in pal.iter().enumerate() {
        let d = dist(c, *p);
        if d < bestd {
            bestd = d;
            best = i;
        }
    }
    best
}

fn is_black(c: Rgb) -> bool {
    c.0.max(c.1).max(c.2) <= BLACK_MAX
}

// ------------------------------------------------------------------ framebuffer

pub struct FrameBuffer {
    pub w: usize,
    pub h: usize,
    pub bpp: usize,
    pub stride: usize,
    pub data: Vec<u8>,
}

impl FrameBuffer {
    pub fn open() -> Result<FrameBuffer, String> {
        let vs = sys::read_trim("/sys/class/graphics/fb0/virtual_size")
            .ok_or("cannot read the framebuffer size")?;
        let mut parts = vs.split(',');
        let w: usize = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
        let h: usize = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
        let bpp: usize = sys::read_trim("/sys/class/graphics/fb0/bits_per_pixel")
            .and_then(|v| v.parse().ok())
            .unwrap_or(32);
        let stride: usize = sys::read_trim("/sys/class/graphics/fb0/stride")
            .and_then(|v| v.parse().ok())
            .unwrap_or(w * bpp / 8);
        let data = fs::read(FB).map_err(|e| format!("cannot read {}: {}", FB, e))?;
        Ok(FrameBuffer { w, h, bpp, stride, data })
    }

    pub fn tile(&self, x0: usize, y0: usize, tw: usize, th: usize, order: &str) -> (Rgb, bool) {
        let mut rs = 0u64;
        let mut gs = 0u64;
        let mut bs = 0u64;
        let mut first: Option<Rgb> = None;
        let mut flat = true;
        let mut n = 0u64;
        let step = self.bpp / 8;
        for y in y0..y0 + th {
            if y >= self.h {
                continue;
            }
            for x in x0..x0 + tw {
                if x >= self.w {
                    continue;
                }
                let o = y * self.stride + x * step;
                if o + 2 >= self.data.len() {
                    continue;
                }
                let (r, g, b) = if self.bpp == 32 {
                    let (a0, a1, a2) = (self.data[o], self.data[o + 1], self.data[o + 2]);
                    if order == "bgr" {
                        (a0, a1, a2)
                    } else {
                        (a2, a1, a0)
                    }
                } else if self.bpp == 16 {
                    let v = self.data[o] as u32 | (self.data[o + 1] as u32) << 8;
                    (
                        ((v >> 11) & 0x1F) as u8 * 255 / 31,
                        ((v >> 5) & 0x3F) as u8 * 255 / 63,
                        (v & 0x1F) as u8 * 255 / 31,
                    )
                } else {
                    (0, 0, 0)
                };
                match first {
                    None => first = Some((r, g, b)),
                    Some(f) if f != (r, g, b) => flat = false,
                    _ => {}
                }
                rs += r as u64;
                gs += g as u64;
                bs += b as u64;
                n += 1;
            }
        }
        if n == 0 {
            return ((0, 0, 0), false);
        }
        (((rs / n) as u8, (gs / n) as u8, (bs / n) as u8), flat)
    }
}

fn set_palette(pal: &[Rgb; 16]) -> Result<(), String> {
    let mut buf = [0u8; 48];
    for (i, c) in pal.iter().enumerate() {
        buf[i * 3] = c.0;
        buf[i * 3 + 1] = c.1;
        buf[i * 3 + 2] = c.2;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(TTY)
        .map_err(|e| format!("cannot open {}: {}", TTY, e))?;
    use std::os::unix::io::AsRawFd;
    sys::ioctl_ptr(file.as_raw_fd(), PIO_CMAP, buf.as_mut_ptr() as *mut c_void)
        .map_err(|e| format!("palette ioctl failed: {}", e))
}

/// Write the whole payload; a tty may accept only part of it in one call.
pub fn write_tty(text: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .open(TTY)
        .map_err(|e| format!("cannot write {}: {}", TTY, e))?;
    f.write_all(text).map_err(|e| format!("write failed: {}", e))?;
    f.flush().ok();
    Ok(())
}

fn sgr_fg(code: u8) -> String {
    format!("\x1b[{}m", code)
}

fn sgr_bg(code: u8) -> String {
    format!("\x1b[{}m", code)
}

fn calib_palette() -> [Rgb; 16] {
    let mut pal = [(0u8, 0u8, 0u8); 16];
    for (i, s) in CALIB_PALETTE.iter().enumerate() {
        pal[i] = parse_hex(s).unwrap_or((0, 0, 0));
    }
    pal
}

struct Calib {
    order: String,
    palette: [Rgb; 16],
    fg: HashMap<u8, u8>,
    bg: HashMap<u8, u8>,
}

fn load_calib() -> Result<Calib, String> {
    let text = fs::read_to_string(CALIB).map_err(|e| format!("cannot read {}: {}", CALIB, e))?;
    let v = json::parse(&text).map_err(|e| format!("{}: {}", CALIB, e))?;
    let order = v.get("order").and_then(|o| o.as_str()).unwrap_or("rgb").to_string();
    let mut palette = [(0u8, 0u8, 0u8); 16];
    if let Some(arr) = v.get("palette").and_then(|p| p.as_arr()) {
        for (i, item) in arr.iter().take(16).enumerate() {
            if let Some(s) = item.as_str() {
                palette[i] = parse_hex(s).unwrap_or((0, 0, 0));
            }
        }
    }
    let mut fg = HashMap::new();
    let mut bg = HashMap::new();
    if let Some(Value::Obj(items)) = v.get("fg") {
        for (k, val) in items {
            if let (Ok(slot), Some(code)) = (k.parse::<u8>(), val.as_i64()) {
                fg.insert(slot, code as u8);
            }
        }
    }
    if let Some(Value::Obj(items)) = v.get("bg") {
        for (k, val) in items {
            if let (Ok(slot), Some(code)) = (k.parse::<u8>(), val.as_i64()) {
                bg.insert(slot, code as u8);
            }
        }
    }
    Ok(Calib { order, palette, fg, bg })
}

pub fn cmd_calibrate(order_arg: Option<&str>) -> Result<i32, String> {
    let pal = calib_palette();
    set_palette(&pal)?;

    let mut payload = String::from("\x1b[0m\x1b[?25l\x1b[1;1H");
    for (k, code) in FG_CODES.iter().enumerate() {
        payload.push_str(&format!("\x1b[1;{}H\x1b[0m{}{}", 1 + k * 2, sgr_fg(*code), BLOCK));
    }
    payload.push_str("\x1b[3;1H");
    for code in BG_CODES {
        payload.push_str(&format!("\x1b[0m{}  ", sgr_bg(code)));
    }
    write_tty(payload.as_bytes())?;
    sys::sleep_ms(800);

    let score = |order: &str| -> f64 {
        let fb = match FrameBuffer::open() {
            Ok(f) => f,
            Err(_) => return f64::MAX,
        };
        let mut total = 0.0;
        for k in 0..FG_CODES.len() {
            let (got, _) = fb.tile(k * 2 * 8, 0, 8, 16, order);
            total += dist(got, pal[nearest(got, &pal)]);
        }
        for k in 0..BG_CODES.len() {
            let (got, _) = fb.tile(k * 2 * 8, 32, 8, 16, order);
            total += dist(got, pal[nearest(got, &pal)]);
        }
        total
    };

    let order = match order_arg {
        Some(o) => o.to_string(),
        None => {
            if score("bgr") <= score("rgb") {
                "bgr".into()
            } else {
                "rgb".into()
            }
        }
    };

    let fb = FrameBuffer::open()?;
    let mut lines = Vec::new();
    let mut raw_fg: Vec<(u8, u8)> = Vec::new();
    let mut raw_bg: Vec<(u8, u8)> = Vec::new();
    for (k, code) in FG_CODES.iter().enumerate() {
        let (got, _) = fb.tile(k * 2 * 8, 0, 8, 16, &order);
        let slot = nearest(got, &pal) as u8;
        raw_fg.push((*code, slot));
        let clear = if dist(got, pal[slot as usize]) < 20.0 { "" } else { "   <-- unclear" };
        lines.push(format!(
            "fg {:3} -> slot {:2}  measured {}  palette {}{}",
            code,
            slot,
            hexs(got),
            hexs(pal[slot as usize]),
            clear
        ));
    }
    for (k, code) in BG_CODES.iter().enumerate() {
        let (got, _) = fb.tile(k * 2 * 8, 32, 8, 16, &order);
        let slot = nearest(got, &pal) as u8;
        raw_bg.push((*code, slot));
        let clear = if dist(got, pal[slot as usize]) < 20.0 { "" } else { "   <-- unclear" };
        lines.push(format!(
            "bg {:3} -> slot {:2}  measured {}  palette {}{}",
            code,
            slot,
            hexs(got),
            hexs(pal[slot as usize]),
            clear
        ));
    }
    drop(fb);

    let mut fg_by_slot: HashMap<u8, u8> = HashMap::new();
    for (code, slot) in &raw_fg {
        fg_by_slot.entry(*slot).or_insert(*code);
    }
    let mut bg_by_slot: HashMap<u8, u8> = HashMap::new();
    for (code, slot) in &raw_bg {
        bg_by_slot.entry(*slot).or_insert(*code);
    }
    let mut fg_sorted: Vec<(u8, u8)> = fg_by_slot.into_iter().collect();
    fg_sorted.sort();
    let mut bg_sorted: Vec<(u8, u8)> = bg_by_slot.into_iter().collect();
    bg_sorted.sort();
    let mut raw_fg_sorted = raw_fg.clone();
    raw_fg_sorted.sort();
    let mut raw_bg_sorted = raw_bg.clone();
    raw_bg_sorted.sort();

    let mut root = vec![
        ("order".to_string(), Value::Str(order.clone())),
        (
            "palette".to_string(),
            Value::Arr(pal.iter().map(|c| Value::Str(hexs(*c))).collect()),
        ),
        (
            "fg".to_string(),
            Value::Obj(fg_sorted.iter().map(|(k, v)| (k.to_string(), Value::Num(*v as f64))).collect()),
        ),
        (
            "bg".to_string(),
            Value::Obj(bg_sorted.iter().map(|(k, v)| (k.to_string(), Value::Num(*v as f64))).collect()),
        ),
        (
            "raw_fg".to_string(),
            Value::Obj(raw_fg_sorted.iter().map(|(k, v)| (k.to_string(), Value::Num(*v as f64))).collect()),
        ),
        (
            "raw_bg".to_string(),
            Value::Obj(raw_bg_sorted.iter().map(|(k, v)| (k.to_string(), Value::Num(*v as f64))).collect()),
        ),
    ];
    root.shrink_to_fit();
    let data = json::to_pretty(&Value::Obj(root));
    fs::write(CALIB, data).map_err(|e| format!("cannot write {}: {}", CALIB, e))?;

    println!("channel order: {}", order);
    for line in &lines {
        println!("  {}", line);
    }
    println!(
        "foreground slots reachable: {:?}",
        fg_sorted.iter().map(|(k, _)| *k).collect::<Vec<u8>>()
    );
    println!(
        "background slots reachable: {:?}",
        bg_sorted.iter().map(|(k, _)| *k).collect::<Vec<u8>>()
    );
    println!("wrote {}", CALIB);
    Ok(0)
}

pub fn cmd_verify_calibrate() -> Result<i32, String> {
    let cal = load_calib()?;
    set_palette(&cal.palette)?;

    let mut slots: Vec<u8> = cal.fg.keys().copied().collect();
    slots.sort();
    let mut bslots: Vec<u8> = cal.bg.keys().copied().collect();
    bslots.sort();

    let mut payload = String::from("\x1b[0m\x1b[?25l\x1b[1;1H");
    for (k, slot) in slots.iter().enumerate() {
        payload.push_str(&format!("\x1b[1;{}H\x1b[0m", 1 + k * 2));
        payload.push_str(&sgr_fg(cal.fg[slot]));
        payload.push_str(BLOCK);
    }
    payload.push_str("\x1b[3;1H");
    for slot in &bslots {
        payload.push_str("\x1b[0m");
        payload.push_str(&sgr_bg(cal.bg[slot]));
        payload.push_str("  ");
    }
    write_tty(payload.as_bytes())?;
    sys::sleep_ms(800);

    let fb = FrameBuffer::open()?;
    let mut problems = Vec::new();
    for (k, slot) in slots.iter().enumerate() {
        let (got, flat) = fb.tile(k * 2 * 8, 0, 8, 16, &cal.order);
        let ok = dist(got, cal.palette[*slot as usize]) < 8.0;
        println!(
            "  fg slot {:2}  want {}  got {}  {}{}",
            slot,
            hexs(cal.palette[*slot as usize]),
            hexs(got),
            if ok { "ok" } else { "FAIL" },
            if flat { "" } else { "  (not flat)" }
        );
        if !ok || !flat {
            problems.push(format!("fg slot {}", slot));
        }
    }
    for (k, slot) in bslots.iter().enumerate() {
        let (got, flat) = fb.tile(k * 2 * 8, 32, 8, 16, &cal.order);
        let ok = dist(got, cal.palette[*slot as usize]) < 8.0;
        println!(
            "  bg slot {:2}  want {}  got {}  {}{}",
            slot,
            hexs(cal.palette[*slot as usize]),
            hexs(got),
            if ok { "ok" } else { "FAIL" },
            if flat { "" } else { "  (not flat)" }
        );
        if !ok || !flat {
            problems.push(format!("bg slot {}", slot));
        }
    }
    if !problems.is_empty() {
        println!(
            "FAIL: {} slot(s) do not match: {}",
            problems.len(),
            problems.join(", ")
        );
        return Ok(1);
    }
    println!(
        "PASS: all {} foreground and {} background slots verified on the framebuffer",
        slots.len(),
        bslots.len()
    );
    Ok(0)
}

// -------------------------------------------------------------------- quantise

struct Quantised {
    counts: Vec<(Rgb, u64)>,
    colours: Vec<(Rgb, u64)>,
    shown: Vec<(Rgb, Rgb)>,
    worst_after_quantise: u8,
}

fn find(d: &[(Rgb, u64)], key: Rgb) -> Option<usize> {
    d.iter().position(|(c, _)| *c == key)
}

fn closest_pair(d: &[(Rgb, u64)]) -> Option<(f64, Rgb, Rgb)> {
    let mut best: Option<(f64, Rgb, Rgb)> = None;
    for i in 0..d.len() {
        for j in i + 1..d.len() {
            let dd = dist(d[i].0, d[j].0);
            if best.map(|b| dd < b.0).unwrap_or(true) {
                best = Some((dd, d[i].0, d[j].0));
            }
        }
    }
    best
}

fn quantise(img: &Image) -> Quantised {
    let mut counts: Vec<(Rgb, u64)> = Vec::new();
    for y in 0..img.h {
        for x in 0..img.w {
            let c = img.get(x, y);
            match counts.iter_mut().find(|(k, _)| *k == c) {
                Some((_, n)) => *n += 1,
                None => counts.push((c, 1)),
            }
        }
    }
    // Flatten first, with black still in the list so its close neighbours are
    // merged normally. Only after that is near-black removed.
    let mut colours: Vec<(Rgb, u64)> = counts.clone();
    let mut shown: Vec<(Rgb, Rgb)> = counts.iter().map(|(c, _)| (*c, *c)).collect();

    loop {
        let pair = match closest_pair(&colours) {
            Some(p) => p,
            None => break,
        };
        let (dd, a, b) = pair;
        if dd > MERGE_DIST && colours.len() <= MAX_COLOURS + 1 {
            break;
        }
        let ai = find(&colours, a).unwrap();
        let bi = find(&colours, b).unwrap();
        let (na, nb) = (colours[ai].1, colours[bi].1);
        let total = na + nb;
        let avg: Rgb = (
            (((a.0 as f64 * na as f64 + b.0 as f64 * nb as f64) / total as f64).round_ties_even()) as u8,
            (((a.1 as f64 * na as f64 + b.1 as f64 * nb as f64) / total as f64).round_ties_even()) as u8,
            (((a.2 as f64 * na as f64 + b.2 as f64 * nb as f64) / total as f64).round_ties_even()) as u8,
        );
        let hi = ai.max(bi);
        let lo = ai.min(bi);
        colours.remove(hi);
        colours.remove(lo);
        match find(&colours, avg) {
            Some(i) => colours[i].1 += total,
            None => colours.push((avg, total)),
        }
        for entry in shown.iter_mut() {
            if entry.1 == a || entry.1 == b {
                entry.1 = avg;
            }
        }
    }

    // A merge of two dark shades can land back in the near-black band. Drop
    // those too; their source pixels become blank instead of a wasted slot.
    colours.retain(|(c, _)| !is_black(*c));
    for entry in shown.iter_mut() {
        if is_black(entry.1) {
            entry.1 = (0, 0, 0);
        }
    }

    let mut worst = 0u8;
    for (orig, painted) in &shown {
        worst = worst
            .max((orig.0 as i16 - painted.0 as i16).unsigned_abs() as u8)
            .max((orig.1 as i16 - painted.1 as i16).unsigned_abs() as u8)
            .max((orig.2 as i16 - painted.2 as i16).unsigned_abs() as u8);
    }
    Quantised { counts, colours, shown, worst_after_quantise: worst }
}

fn bump(counter: &mut Vec<(Rgb, u64)>, key: Rgb) {
    match counter.iter_mut().find(|(c, _)| *c == key) {
        Some((_, n)) => *n += 1,
        None => counter.push((key, 1)),
    }
}

fn get(counter: &[(Rgb, u64)], key: Rgb) -> u64 {
    counter.iter().find(|(c, _)| *c == key).map(|(_, n)| *n).unwrap_or(0)
}

fn set(counter: &mut Vec<(Rgb, u64)>, key: Rgb, value: u64) {
    match counter.iter_mut().find(|(c, _)| *c == key) {
        Some((_, n)) => *n = value,
        None => counter.push((key, value)),
    }
}

fn drop_key(counter: &mut Vec<(Rgb, u64)>, key: Rgb) {
    counter.retain(|(c, _)| *c != key);
}

pub struct BuildReport {
    pub art: Vec<u8>,
    pub expected: Image,
    pub palette: Vec<Rgb>,
    pub rows: usize,
    pub unique_in: usize,
    pub worst: u8,
}

pub fn build(img: &Image, cal: &Calib) -> BuildReport {
    let mut q = quantise(img);
    let mut weight: Vec<(Rgb, u64)> = Vec::new();
    let mut bottom: Vec<(Rgb, u64)> = Vec::new();
    for y in 0..img.h {
        for x in 0..img.w {
            let src = img.get(x, y);
            let shown = q
                .shown
                .iter()
                .find(|(o, _)| *o == src)
                .map(|(_, s)| *s)
                .unwrap_or(src);
            bump(&mut weight, shown);
            if y % 2 == 1 {
                bump(&mut bottom, shown);
            }
        }
    }

    let mut slot_of: HashMap<Rgb, u8> = HashMap::new();
    let mut palette: Vec<Rgb> = vec![(0, 0, 0)];
    let mut grid: Vec<Vec<u8>> = Vec::new();
    let mut rows = (img.h + 1) / 2;

    for _attempt in 0..60 {
        let mut order = q.colours.clone();
        order.sort_by(|a, b| {
            let ka = (get(&bottom, a.0), get(&weight, a.0));
            let kb = (get(&bottom, b.0), get(&weight, b.0));
            kb.cmp(&ka)
        });
        slot_of.clear();
        for (i, (c, _)) in order.iter().take(MAX_COLOURS).enumerate() {
            slot_of.insert(*c, i as u8 + 1);
        }
        palette = Vec::with_capacity(16);
        palette.push((0, 0, 0));
        for i in 0..MAX_COLOURS {
            palette.push(order.get(i).map(|(c, _)| *c).unwrap_or((0, 0, 0)));
        }

        grid = Vec::with_capacity(rows);
        for y in 0..img.h {
            let mut row = Vec::with_capacity(img.w);
            for x in 0..img.w {
                let src = img.get(x, y);
                let shown = q
                    .shown
                    .iter()
                    .find(|(o, _)| *o == src)
                    .map(|(_, s)| *s)
                    .unwrap_or(src);
                row.push(*slot_of.get(&shown).unwrap_or(&0));
            }
            grid.push(row);
        }
        if img.h % 2 == 1 {
            grid.push(vec![0; img.w]);
        }
        rows = grid.len() / 2;

        let mut bad = 0usize;
        let mut involved: Vec<Rgb> = Vec::new();
        for r in 0..rows {
            for x in 0..img.w {
                let a = grid[2 * r][x];
                let b = grid[2 * r + 1][x];
                if a != b && a >= 8 && b >= 8 {
                    bad += 1;
                    for slot in [a, b] {
                        let c = palette[slot as usize];
                        if !involved.contains(&c) {
                            involved.push(c);
                        }
                    }
                }
            }
        }
        if bad == 0 && q.colours.len() <= MAX_COLOURS {
            break;
        }
        if involved.is_empty() {
            break;
        }
        let mut best: Option<(f64, Rgb, Rgb)> = None;
        for i in 0..involved.len() {
            for j in i + 1..involved.len() {
                let d = dist(involved[i], involved[j]);
                if best.map(|b| d < b.0).unwrap_or(true) {
                    best = Some((d, involved[i], involved[j]));
                }
            }
        }
        let (_, a, b) = match best {
            Some(v) => v,
            None => break,
        };
        let na = if get(&weight, a) == 0 { 1 } else { get(&weight, a) };
        let nb = if get(&weight, b) == 0 { 1 } else { get(&weight, b) };
        let total = na + nb;
        let avg: Rgb = (
            (((a.0 as f64 * na as f64 + b.0 as f64 * nb as f64) / total as f64).round_ties_even()) as u8,
            (((a.1 as f64 * na as f64 + b.1 as f64 * nb as f64) / total as f64).round_ties_even()) as u8,
            (((a.2 as f64 * na as f64 + b.2 as f64 * nb as f64) / total as f64).round_ties_even()) as u8,
        );
        for entry in q.shown.iter_mut() {
            if entry.1 == a || entry.1 == b {
                entry.1 = avg;
            }
        }
        q.colours.retain(|(c, _)| *c != a && *c != b);
        match find(&q.colours, avg) {
            Some(i) => q.colours[i].1 += total,
            None => q.colours.push((avg, total)),
        }
        let wa = get(&weight, a);
        let wb = get(&weight, b);
        let ba = get(&bottom, a);
        let bb = get(&bottom, b);
        set(&mut weight, avg, wa + wb);
        set(&mut bottom, avg, ba + bb);
        drop_key(&mut weight, a);
        drop_key(&mut weight, b);
        drop_key(&mut bottom, a);
        drop_key(&mut bottom, b);
    }

    let fg_code = |slot: u8| -> u8 { *cal.fg.get(&slot).unwrap_or(&(30 + slot)) };
    let bg_code = |slot: u8| -> u8 { *cal.bg.get(&slot).unwrap_or(&(40 + slot)) };
    let fg_for = |slot: u8| if slot < 8 { sgr_fg(fg_code(slot)) } else { sgr_fg(fg_code(slot)) };
    let bg_for = |slot: u8| sgr_bg(bg_code(slot));

    let mut payload = String::from("\x1b[0m\x1b[?25l");
    let mut expected = Image::new(img.w, img.h);
    for r in 0..rows {
        payload.push_str(&format!("\x1b[{};1H", r + 1));
        for x in 0..img.w {
            let top = grid[2 * r][x];
            let bot = grid[2 * r + 1][x];
            let top_c = palette[top as usize];
            let bot_c = palette[bot as usize];
            if top == 0 && bot == 0 {
                // Reset to the terminal background and leave a plain blank
                // rather than spend a palette slot (or a black foreground)
                // on empty pixels.
                payload.push_str("\x1b[0m ");
            } else if top == bot {
                payload.push_str("\x1b[0m");
                payload.push_str(&fg_for(top));
                payload.push_str(BLOCK);
            } else if bot == 0 {
                payload.push_str("\x1b[0m");
                payload.push_str(&fg_for(top));
                payload.push_str(UPPER);
            } else if top == 0 {
                payload.push_str("\x1b[0m");
                payload.push_str(&fg_for(bot));
                payload.push_str(LOWER);
            } else if bot <= 7 {
                payload.push_str("\x1b[0m");
                payload.push_str(&fg_for(top));
                payload.push_str(&bg_for(bot));
                payload.push_str(UPPER);
            } else if top <= 7 {
                payload.push_str("\x1b[0m");
                payload.push_str(&bg_for(top));
                payload.push_str(&fg_for(bot));
                payload.push_str(LOWER);
            } else {
                payload.push_str("\x1b[0m");
                payload.push_str(&fg_for(top));
                let near = nearest(bot_c, &palette[..8]);
                payload.push_str(&bg_for(near as u8));
                payload.push_str(UPPER);
            }
            expected.put(x, 2 * r, top_c);
            if 2 * r + 1 < img.h {
                expected.put(x, 2 * r + 1, bot_c);
            }
        }
    }

    let mut worst = 0u8;
    for y in 0..img.h {
        for x in 0..img.w {
            let src = img.get(x, y);
            let dst = if is_black(src) { (0, 0, 0) } else { expected.get(x, y) };
            worst = worst
                .max((src.0 as i16 - dst.0 as i16).unsigned_abs() as u8)
                .max((src.1 as i16 - dst.1 as i16).unsigned_abs() as u8)
                .max((src.2 as i16 - dst.2 as i16).unsigned_abs() as u8);
        }
    }
    let _ = q.worst_after_quantise;

    BuildReport {
        art: payload.into_bytes(),
        expected,
        palette,
        rows,
        unique_in: q.counts.len(),
        worst,
    }
}

pub fn cmd_build(image: &str, out: &str) -> Result<i32, String> {
    let cal = load_calib()?;
    let data = fs::read(image).map_err(|e| format!("cannot read {}: {}", image, e))?;
    let img = png::decode(&data).map_err(|e| format!("{}: {}", image, e))?;
    let report = build(&img, &cal);

    fs::create_dir_all(out).map_err(|e| format!("cannot create {}: {}", out, e))?;
    fs::write(format!("{}/art.bin", out), &report.art).map_err(|e| e.to_string())?;
    fs::write(format!("{}/expected.png", out), png::encode_rgb(&report.expected))
        .map_err(|e| e.to_string())?;
    fs::write(
        format!("{}/art.meta", out),
        format!("{} {}\n", img.w, report.rows),
    )
    .map_err(|e| e.to_string())?;

    let map = Value::Obj(vec![
        ("image".into(), Value::Str(std::fs::canonicalize(image).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| image.into()))),
        ("size".into(), Value::Arr(vec![Value::Num(img.w as f64), Value::Num(img.h as f64)])),
        ("cells".into(), Value::Arr(vec![Value::Num(img.w as f64), Value::Num(report.rows as f64)])),
        ("cell_px".into(), Value::Arr(vec![Value::Num(8.0), Value::Num(16.0)])),
        ("origin".into(), Value::Arr(vec![Value::Num(0.0), Value::Num(0.0)])),
        ("order".into(), Value::Str(cal.order.clone())),
        ("palette".into(), Value::Arr(report.palette.iter().map(|c| Value::Str(hexs(*c))).collect())),
        ("colors".into(), Value::Arr(report.palette.iter().map(|c| Value::Str(hexs(*c))).collect())),
    ]);
    fs::write(format!("{}/map.json", out), json::to_pretty(&map)).map_err(|e| e.to_string())?;

    println!("image: {}  {}x{}", image, img.w, img.h);
    println!("unique_in: {}", report.unique_in);
    let used = report.palette.iter().skip(1).filter(|c| **c != (0, 0, 0)).count();
    println!("palette_colours: {}", 1 + used);
    println!("worst_error_rgb: {}", report.worst);
    println!("cells: {}x{}  (two source rows per cell)", img.w, report.rows);
    println!("palette:");
    for (i, c) in report.palette.iter().enumerate() {
        let tag = if i == 0 {
            "black (plain space)"
        } else if i < 8 {
            "background-capable"
        } else {
            "foreground only"
        };
        println!("  slot {:2}  {}  {}", i, hexs(*c), tag);
    }
    println!(
        "wrote {}/art.bin ({} bytes), expected.png, map.json, art.meta",
        out,
        report.art.len()
    );
    Ok(0)
}

pub fn cmd_draw(art_path: &str) -> Result<i32, String> {
    let path = std::path::Path::new(art_path);
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    let map_path = dir.join("map.json");
    let text = fs::read_to_string(&map_path).map_err(|e| format!("cannot read {}: {}", map_path.display(), e))?;
    let v = json::parse(&text).map_err(|e| e.to_string())?;
    let mut pal = [(0u8, 0u8, 0u8); 16];
    if let Some(arr) = v.get("palette").and_then(|p| p.as_arr()) {
        for (i, item) in arr.iter().take(16).enumerate() {
            if let Some(s) = item.as_str() {
                pal[i] = parse_hex(s).unwrap_or((0, 0, 0));
            }
        }
    }
    set_palette(&pal)?;
    let data = fs::read(art_path).map_err(|e| format!("cannot read {}: {}", art_path, e))?;
    write_tty(&data)?;
    println!(
        "draw: wrote {} bytes to {} in one write, palette applied",
        data.len(),
        TTY
    );
    Ok(0)
}

pub fn cmd_verify(map_path: &str) -> Result<i32, String> {
    let path = std::path::Path::new(map_path);
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    let text = fs::read_to_string(path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    let v = json::parse(&text).map_err(|e| e.to_string())?;
    let expected_bytes = fs::read(dir.join("expected.png")).map_err(|e| e.to_string())?;
    let expected = png::decode(&expected_bytes)?;
    let w = expected.w;
    let h = expected.h;
    let cell_px = v
        .get("cell_px")
        .and_then(|c| c.as_arr())
        .map(|a| {
            (
                a[0].as_f64().unwrap_or(8.0) as usize,
                a[1].as_f64().unwrap_or(16.0) as usize,
            )
        })
        .unwrap_or((8, 16));
    let origin = v
        .get("origin")
        .and_then(|c| c.as_arr())
        .map(|a| {
            (
                a[0].as_f64().unwrap_or(0.0) as usize,
                a[1].as_f64().unwrap_or(0.0) as usize,
            )
        })
        .unwrap_or((0, 0));
    let order = v.get("order").and_then(|o| o.as_str()).unwrap_or("bgr").to_string();

    let fb = FrameBuffer::open()?;
    let mut wrong = 0usize;
    let mut nonflat = 0usize;
    let mut worst = 0u8;
    let mut first: Vec<String> = Vec::new();
    let mut screen = Image::new(w, h);
    for y in 0..h {
        let cell_row = origin.1 + y / 2;
        let half_top = y % 2 == 0;
        for x in 0..w {
            let cx = origin.0 + x * cell_px.0;
            let cy = cell_row * cell_px.1 + if half_top { 0 } else { cell_px.1 / 2 };
            let (got, flat) = fb.tile(cx, cy, cell_px.0, cell_px.1 / 2, &order);
            let want = expected.get(x, y);
            screen.put(x, y, got);
            if !flat {
                nonflat += 1;
            }
            let d = (got.0 as i16 - want.0 as i16)
                .abs()
                .max((got.1 as i16 - want.1 as i16).abs())
                .max((got.2 as i16 - want.2 as i16).abs()) as u8;
            if d > 6 {
                wrong += 1;
                if first.len() < 8 {
                    first.push(format!(
                        "({},{}) want {} got {}",
                        x,
                        y,
                        hexs(want),
                        hexs(got)
                    ));
                }
                worst = worst.max(d);
            }
        }
    }

    let scale = 8;
    let left = expected.scaled(scale);
    let right = screen.scaled(scale);
    let mut combo = Image::new(left.w * 2 + 8, left.h);
    for y in 0..combo.h {
        for x in 0..combo.w {
            combo.put(x, y, (40, 40, 40));
        }
    }
    for y in 0..left.h {
        for x in 0..left.w {
            combo.put(x, y, left.get(x, y));
        }
    }
    for y in 0..right.h {
        for x in 0..right.w {
            combo.put(x + left.w + 8, y, right.get(x, y));
        }
    }
    let out = dir.join("fb-vs-expected.png");
    fs::write(&out, png::encode_rgb(&combo)).map_err(|e| e.to_string())?;

    println!("samples: {}  (every source pixel, read from {})", w * h, FB);
    for line in &first {
        println!("  {}", line);
    }
    println!("wrong colour: {}  non-flat dots: {}", wrong, nonflat);
    println!(
        "image: {}  (left = expected, right = real screen)",
        out.display()
    );
    if wrong == 0 && nonflat == 0 {
        println!("PASS");
        return Ok(0);
    }
    println!("FAIL: {} pixel(s) differ, worst {}/255", wrong, worst);
    Ok(1)
}

#[allow(dead_code)]
fn _unused(_: c_int) {}

#[allow(dead_code)]
fn _unused2(_: *const c_void) {}
