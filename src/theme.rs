//! Colours taken from the boot art. Small labels use the best visible art
//! colour closest to bright red or light green; big text uses the brightest
//! art colour closest to white. Black and near-black are never selected.

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

const TEXT_TARGET: (u8, u8, u8) = (235, 235, 245);
const TEXT_FALLBACK: (u8, u8, u8) = (232, 232, 248);
const RED_TARGET: (u8, u8, u8) = (255, 64, 64);
const GREEN_TARGET: (u8, u8, u8) = (128, 255, 128);
const RED_FALLBACK: (u8, u8, u8) = (255, 96, 96);
const BRIGHT_RED: u8 = 9;
const BRIGHT_GREEN: u8 = 10;

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
    text: (u8, u8, u8),
    small: (u8, u8, u8),
    small_ansi: u8,
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

impl Theme {
    pub fn load() -> Theme {
        let mut palette = ANSI;
        if let Ok(text) = fs::read_to_string("/var/lib/cartridge/art/map.json") {
            if let Ok(v) = json::parse(&text) {
                if let Some(arr) = v.get("palette").and_then(|p| p.as_arr()) {
                    for (i, item) in arr.iter().take(16).enumerate() {
                        if let Some(s) = item.as_str() {
                            if s.len() == 7 {
                                let r = u8::from_str_radix(&s[1..3], 16).unwrap_or(0);
                                let g = u8::from_str_radix(&s[3..5], 16).unwrap_or(0);
                                let b = u8::from_str_radix(&s[5..7], 16).unwrap_or(0);
                                palette[i] = (r, g, b);
                            }
                        }
                    }
                }
            }
        }

        // Small snippets: the closest bright red or light green the image
        // actually supplied. Dark/dim shades are excluded by the luma floor.
        let red = nearest_light(&palette, RED_TARGET, 100, is_red, None);
        let green = nearest_light(&palette, GREEN_TARGET, 100, is_green, None);
        let (small, small_ansi) = match (red, green) {
            (Some(r), Some(g)) => {
                if dist(r, RED_TARGET) <= dist(g, GREEN_TARGET) {
                    (r, BRIGHT_RED)
                } else {
                    (g, BRIGHT_GREEN)
                }
            }
            (Some(r), None) => (r, BRIGHT_RED),
            (None, Some(g)) => (g, BRIGHT_GREEN),
            (None, None) => (RED_FALLBACK, BRIGHT_RED),
        };

        // Big text: the brightest image colour, but never the exact colour
        // already chosen for the small snippets.
        let text = nearest_light(&palette, TEXT_TARGET, 140, |_| true, Some(small))
            .unwrap_or(TEXT_FALLBACK);
        Theme { text, small, small_ansi }
    }

    /// Nearest console colour for big text, skipping black, dark red and dark
    /// grey so text can never disappear into the background.
    fn text_ansi(&self) -> u8 {
        let mut best = 15u8;
        let mut bestd = u32::MAX;
        for (i, c) in ANSI.iter().enumerate() {
            if matches!(i, 0 | 1 | 8) {
                continue;
            }
            let d = dist(*c, self.text);
            if d < bestd {
                bestd = d;
                best = i as u8;
            }
        }
        best
    }

    pub fn colour(&self, role: Role) -> (u8, u8, u8) {
        match role {
            Role::Title | Role::Text => self.text,
            _ => self.small,
        }
    }

    pub fn sgr(&self, role: Role) -> String {
        let (idx, bold) = match role {
            Role::Title => (self.text_ansi(), true),
            Role::Text => (self.text_ansi(), false),
            Role::Accent => (self.small_ansi, true),
            Role::Dim => (self.small_ansi, false),
            Role::Ok | Role::Warn | Role::Err => (self.small_ansi, true),
        };
        let bold = if bold { "1;" } else { "" };
        if idx < 8 {
            format!("\x1b[{}{}m", bold, 30 + idx)
        } else {
            format!("\x1b[{}{}m", bold, 90 + idx - 8)
        }
    }

    pub fn paint(&self, role: Role, text: &str) -> String {
        format!("{}{}\x1b[0m", self.sgr(role), text)
    }

    /// For a terminal that can mix colours: the exact hex from the art.
    pub fn truecolor(&self, role: Role) -> String {
        let c = self.colour(role);
        format!("\x1b[1;38;2;{};{};{}m", c.0, c.1, c.2)
    }
}
