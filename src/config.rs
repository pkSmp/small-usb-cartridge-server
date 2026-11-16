//! cartridge config.json: four settings, with the same defaults and the same
//! tolerance for bad values as cartridge_config.py had.

use crate::json::{self, Value};
use std::path::Path;

pub struct Config {
    pub complete_sound: String,
    pub failed_sound: String,
    pub storage_mb: u64,
    pub ram_mb: u64,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            complete_sound: "complete.mp3".into(),
            failed_sound: "failed.mp3".into(),
            storage_mb: 2048,
            ram_mb: 512,
        }
    }
}

fn positive(value: Option<&Value>, fallback: u64) -> u64 {
    match value {
        Some(Value::Num(n)) if *n > 0.0 => *n as u64,
        Some(Value::Str(s)) => s.trim().parse::<i64>().ok().filter(|n| *n > 0).map(|n| n as u64).unwrap_or(fallback),
        _ => fallback,
    }
}

pub fn load(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let value = json::parse(&text).map_err(|e| format!("{}: {}", path.display(), e))?;
    match value {
        Value::Obj(_) => {}
        _ => return Err("config.json must contain a JSON object".into()),
    }
    let mut config = Config::default();
    if let Some(sounds) = value.get("sounds") {
        if let Some(s) = sounds.get("complete").and_then(|v| v.as_str()) {
            if !s.is_empty() {
                config.complete_sound = s.to_string();
            }
        }
        if let Some(s) = sounds.get("failed").and_then(|v| v.as_str()) {
            if !s.is_empty() {
                config.failed_sound = s.to_string();
            }
        }
    }
    config.storage_mb = positive(value.get("storage_mb"), 2048);
    config.ram_mb = positive(value.get("ram_mb"), 512);
    Ok(config)
}
