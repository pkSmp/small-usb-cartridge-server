//! Colours taken from the boot art. The console only has sixteen colours, so
//! each role picks the palette entry that is closest to what the role means
//! (green for success, red for refusal, ...) and then the nearest console
//! colour to that entry.

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
    accent: (u8, u8, u8),
}

/// Hue in degrees and saturation, or (-1, 0) for greys.
fn hue_sat(c: (u8, u8, u8)) -> (f32, f32) {
    let r = c.0 as f32 / 255.0;
    let g = c.1 as f32 / 255.0;
    let b = c.2 as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    if max <= 0.001 || d / max < 0.08 {
        return (-1.0, 0.0);
    }
    let h = if max == r {
        ((g - b) / d) % 6.0
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (((h * 60.0) + 360.0) % 360.0, d / max)
}

fn dist(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let dr = a.0 as i32 - b.0 as i32;
    let dg = a.1 as i32 - b.1 as i32;
    let db = a.2 as i32 - b.2 as i32;
    (dr * dr + dg * dg + db * db) as u32
}

impl Theme {
    pub fn load() -> Theme {
        let mut palette = ANSI;
        let mut ok = false;
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
                                ok = true;
                            }
                        }
                    }
                }
            }
        }
        let _ = ok;
        // The accent is the most saturated colour in the art, which is what the
        // eye already associates with "this machine".
        let mut accent = (192, 192, 192);
        let mut best = 0i32;
        for c in palette.iter() {
            let max = c.0.max(c.1).max(c.2) as i32;
            let min = c.0.min(c.1).min(c.2) as i32;
            let chroma = max - min + max / 8;
            if chroma > best {
                best = chroma;
                accent = *c;
            }
        }
        Theme { palette, accent }
    }

    fn nearest_entry(&self, target: (u8, u8, u8)) -> (u8, u8, u8) {
        let mut best = self.palette[0];
        let mut bestd = u32::MAX;
        for c in self.palette.iter() {
            let d = dist(*c, target);
            if d < bestd {
                bestd = d;
                best = *c;
            }
        }
        best
    }

    fn ansi_of(&self, colour: (u8, u8, u8)) -> u8 {
        let mut best = 7u8;
        let mut bestd = u32::MAX;
        for (i, c) in ANSI.iter().enumerate() {
            let d = dist(*c, colour);
            if d < bestd {
                bestd = d;
                best = i as u8;
            }
        }
        best
    }

    pub fn colour(&self, role: Role) -> (u8, u8, u8) {
        match role {
            Role::Title | Role::Accent => self.accent,
            Role::Ok => self.nearest_entry((0, 170, 60)),
            Role::Warn => self.nearest_entry((235, 165, 0)),
            Role::Err => self.nearest_entry((210, 40, 40)),
            Role::Dim => self.nearest_entry((120, 120, 130)),
            Role::Text => (192, 192, 192),
        }
    }

    /// Pick the art colour that shares the role's hue, so "ok" only turns green
    /// if the art actually contains a green. Falls back to the plain console
    /// colour when the art is all reds and greys.
    fn hued(&self, degrees: f32, fallback: u8) -> u8 {
        let mut best: Option<(u8, u8, u8)> = None;
        let mut best_score = 0.0f32;
        for c in self.palette.iter() {
            let (h, s) = hue_sat(*c);
            if h < 0.0 || s < 0.25 {
                continue;
            }
            let mut d = (h - degrees).abs();
            if d > 180.0 {
                d = 360.0 - d;
            }
            if d < 32.0 {
                let score = s * (1.0 - d / 64.0);
                if score > best_score {
                    best_score = score;
                    best = Some(*c);
                }
            }
        }
        match best {
            Some(c) => self.ansi_of(c),
            None => fallback,
        }
    }

    pub fn sgr(&self, role: Role) -> String {
        let (idx, bold) = match role {
            Role::Title | Role::Accent => (self.ansi_of(self.accent), true),
            Role::Ok => (self.hued(125.0, 10), true),
            Role::Warn => (self.hued(45.0, 11), true),
            Role::Err => (self.hued(0.0, 9), true),
            Role::Dim => (8, false),
            Role::Text => (7, false),
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
