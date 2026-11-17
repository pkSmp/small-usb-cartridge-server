//! Colours taken from the boot art. Small labels use a reserved bright red on
//! the console; if the art has no free slot, they fall back to the best visible
//! saturated red or green. Big text uses the brightest art colour closest to
//! white. The console palette is the art palette, so a colour must be emitted
//! through the slot that actually holds it.

use crate::json;
use std::fs;

const ANSI: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (128, 0, 0),
    (0, 128, 0),
    (128, 128, 0),
    (0, 0, 128),
    (128, 0, 128),
    (0, 128, 128),
    (192, 192, 192),
    (128, 128, 128),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (0, 0, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

const CALIB: &str = "/var/lib/cartridge/calib.json";
const DEFAULT_FG: [u8; 16] = [30, 31, 32, 33, 34, 35, 36, 37, 90, 91, 92, 93, 94, 95, 96, 97];
const TEXT_TARGET: (u8, u8, u8) = (235, 235, 245);
const TEXT_FALLBACK: (u8, u8, u8) = (232, 232, 248);
const RED_FALLBACK: (u8, u8, u8) = (255, 96, 96);
/// A real red for the console labels. The art often has only a dark crimson
/// (this boot's #990030), which is too dim to read against black.
const UI_RED: (u8, u8, u8) = (255, 0, 0);

#[derive(Clone, Copy)]
pub enum Role {
    Title,
    Accent,
    Ok,
    Warn,
    Err,
    Dim,
    Text,
}

pub struct Theme {
    palette: [(u8, u8, u8); 16],
    fg: [u8; 16],
    text: (u8, u8, u8),
    small: (u8, u8, u8),
}

fn luminance(c: (u8, u8, u8)) -> u32 {
    299 * c.0 as u32 + 587 * c.1 as u32 + 114 * c.2 as u32
}

fn dist(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let dr = a.0 as i32 - b.0 as i32;
    let dg = a.1 as i32 - b.1 as i32;
    let db = a.2 as i32 - b.2 as i32;
    (dr * dr + dg * dg + db * db) as u32
}

fn visible(c: (u8, u8, u8)) -> bool {
    c.0.max(c.1).max(c.2) > 24 && luminance(c) > 32
}

fn is_red(c: (u8, u8, u8)) -> bool {
    c.0 as u32 > c.1 as u32 + 8 && c.0 as u32 > c.2 as u32 + 8
}

fn is_green(c: (u8, u8, u8)) -> bool {
    c.1 as u32 > c.0 as u32 + 8 && c.1 as u32 > c.2 as u32 + 8
}

/// Beige and brown are technically reddish, but they are not the red the
/// labels need. Require a genuinely saturated colour before it can be chosen.
fn saturated(c: (u8, u8, u8)) -> bool {
    let max = c.0.max(c.1).max(c.2) as u32;
    let min = c.0.min(c.1).min(c.2) as u32;
    max >= 64 && (max - min) * 100 / max >= 55
}

/// The brightest strongly-saturated red or green in the art. Choosing by
/// saturation first avoids the pale beige/salmon that is closer in raw RGB
/// distance but reads as beige on the terminal.
fn best_small(palette: &[(u8, u8, u8); 16], keep: fn((u8, u8, u8)) -> bool) -> Option<(u8, u8, u8)> {
    palette
        .iter()
        .copied()
        .filter(|c| visible(*c) && saturated(*c) && keep(*c))
        .max_by_key(|c| luminance(*c))
}

fn nearest_light(
    palette: &[(u8, u8, u8); 16],
    target: (u8, u8, u8),
    min_luma: u32,
    keep: fn((u8, u8, u8)) -> bool,
    skip: Option<(u8, u8, u8)>,
) -> Option<(u8, u8, u8)> {
    let mut best = None;
    let mut bestd = u32::MAX;
    for c in palette.iter().copied() {
        if !visible(c) || luminance(c) < min_luma || !keep(c) || Some(c) == skip {
            continue;
        }
        let d = dist(c, target);
        if d < bestd {
            bestd = d;
            best = Some(c);
        }
    }
    best
}

fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    if s.len() != 7 || !s.starts_with('#') {
        return None;
    }
    Some((
        u8::from_str_radix(&s[1..3], 16).ok()?,
        u8::from_str_radix(&s[3..5], 16).ok()?,
        u8::from_str_radix(&s[5..7], 16).ok()?,
    ))
}

impl Theme {
    /// Load the console theme, reserving one unused art slot for a genuinely
    /// bright red. The art itself keeps its own palette; only a slot the image
    /// does not use is replaced.
    pub fn console() -> Theme {
        let mut theme = Theme::load();
        if let Some(slot) = (10..16).find(|i| !visible(theme.palette[*i])) {
            theme.palette[slot] = UI_RED;
            theme.small = UI_RED;
        }
        theme
    }

    /// Apply this palette to the console tty. The console redraws through the
    /// calibrated slot holding UI_RED, so the label colour is the real red.
    pub fn install(&self) {
        let _ = crate::art::apply_palette(&self.palette);
    }

    pub fn load() -> Theme {
        let mut palette = ANSI;
        if let Ok(text) = fs::read_to_string("/var/lib/cartridge/art/map.json") {
            if let Ok(v) = json::parse(&text) {
                if let Some(arr) = v.get("palette").and_then(|p| p.as_arr()) {
                    for (i, item) in arr.iter().take(16).enumerate() {
                        if let Some(c) = item.as_str().and_then(parse_hex) {
                            palette[i] = c;
                        }
                    }
                }
            }
        }

        let mut fg = DEFAULT_FG;
        if let Ok(text) = fs::read_to_string(CALIB) {
            if let Ok(v) = json::parse(&text) {
                if let Some(json::Value::Obj(items)) = v.get("fg") {
                    for (key, val) in items {
                        if let (Ok(slot), Some(code)) = (key.parse::<usize>(), val.as_i64()) {
                            if slot < 16 {
                                fg[slot] = code as u8;
                            }
                        }
                    }
                }
            }
        }

        // Small snippets: a genuinely saturated red or green from the image.
        // Beige/salmon is rejected even when its RGB distance is smaller.
        let small = best_small(&palette, is_red)
            .or_else(|| best_small(&palette, is_green))
            .unwrap_or(RED_FALLBACK);

        // Big text: the brightest image colour, but never the exact colour
        // already chosen for the small snippets.
        let text = nearest_light(&palette, TEXT_TARGET, 140, |_| true, Some(small))
            .unwrap_or(TEXT_FALLBACK);
        Theme { palette, fg, text, small }
    }

    /// Find the palette slot holding this colour and return the foreground
    /// code calibrated for that slot. This is essential: the terminal palette
    /// is the art palette, not the stock ANSI palette.
    fn code_for(&self, colour: (u8, u8, u8)) -> u8 {
        let mut best = None;
        let mut bestd = u32::MAX;
        for (i, c) in self.palette.iter().enumerate() {
            if !visible(*c) {
                continue;
            }
            let d = dist(*c, colour);
            if d < bestd {
                bestd = d;
                best = Some(i);
            }
        }
        match best {
            Some(i) => self.fg[i],
            None => self.fg[7],
        }
    }

    pub fn colour(&self, role: Role) -> (u8, u8, u8) {
        match role {
            Role::Title | Role::Text => self.text,
            _ => self.small,
        }
    }

    pub fn sgr(&self, role: Role) -> String {
        // No bold: bold changes which palette slot the console uses, and the
        // unused bright slots in the art palette are black. Use the calibrated
        // slot code directly so every text colour is visible.
        let code = match role {
            Role::Title | Role::Text => self.code_for(self.text),
            Role::Accent | Role::Ok | Role::Warn | Role::Err | Role::Dim => {
                self.code_for(self.small)
            }
        };
        format!("\x1b[{}m", code)
    }

    pub fn paint(&self, role: Role, text: &str) -> String {
        format!("{}{}\x1b[0m", self.sgr(role), text)
    }

    /// For a terminal that can mix colours: the exact hex from the art.
    pub fn truecolor(&self, role: Role) -> String {
        let c = self.colour(role);
        format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2)
    }
}
